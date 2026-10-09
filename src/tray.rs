//! The icon in the notification area: the app's icon, red with a white
//! dot while recording, the time recorded in its tooltip. A click shows
//! the window; its menu starts or stops the recording, shows the window,
//! sets whether the window is on the taskbar, whether closing it hides it
//! and whether a recording puts it away, or exits.

use std::cell::RefCell;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NIN_SELECT,
    NOTIFY_ICON_MESSAGE, NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconFromResourceEx, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu, DispatchMessageW,
    GetMessageW, GetSystemMetrics, HICON, LR_DEFAULTCOLOR, MF_CHECKED, MF_SEPARATOR, MF_STRING, MSG, PostMessageW, PostQuitMessage,
    RegisterClassExW, RegisterWindowMessageW, SM_CXSMICON, SetForegroundWindow, SetMenuDefaultItem, TPM_NONOTIFY, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, WINDOW_EX_STYLE, WM_APP, WM_CLOSE, WM_CONTEXTMENU, WM_DESTROY, WM_NULL,
    WNDCLASSEXW, WS_POPUP,
};
use windows::core::{PCWSTR, w};

use crate::win::wide;

/// What the icon asks of the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Start or stop the recording, as the hotkey does.
    Toggle,
    /// Show the window in front of the others.
    Show,
    /// Put the window's button on the taskbar or take it off.
    Taskbar,
    /// Whether closing the window hides it instead.
    CloseToTray,
    /// Whether the window is minimised when a recording starts.
    MinimiseOnRecord,
    /// Whether the trimming window opens when a recording ends.
    TrimAfterRecord,
    /// Show the window with About open.
    About,
    /// Close the program.
    Exit,
}

/// The icon's callback message.
const WM_TRAY: u32 = WM_APP + 1;
/// The state changed: the icon and the tooltip are set again.
const WM_STATE: u32 = WM_APP + 2;
/// The icon's id among the window's icons.
const ICON_ID: u32 = 1;
/// A selection with the keyboard (`NIN_SELECT | NINF_KEY`), missing from the crate.
const NIN_KEYSELECT: u32 = NIN_SELECT | 1;
const MENU_TOGGLE: usize = 1;
const MENU_SHOW: usize = 2;
const MENU_EXIT: usize = 3;
const MENU_TASKBAR: usize = 4;
const MENU_CLOSE_TO_TRAY: usize = 5;
const MENU_MINIMISE_ON_RECORD: usize = 6;
const MENU_ABOUT: usize = 7;
const MENU_TRIM_AFTER_RECORD: usize = 8;

/// What the icon and its menu show.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Status {
    /// The time recorded, `None` when there is no recording.
    pub clock: Option<String>,
    /// Whether the window is kept off the taskbar.
    pub tray_only: bool,
    /// Whether closing the window hides it.
    pub close_to_tray: bool,
    /// Whether a recording that starts minimises the window.
    pub minimise_on_record: bool,
    /// Whether a recording that ends opens in the trimming window.
    pub trim_after_record: bool,
}

/// The icon; removed when dropped. Commands arrive on the receiver, and
/// the egui context is woken for each.
pub struct Tray {
    hwnd: isize,
    state: Arc<Mutex<Status>>,
    thread: Option<JoinHandle<()>>,
}

/// What the window procedure needs, on the icon's thread.
struct Context {
    tx: mpsc::Sender<Command>,
    ctx: egui::Context,
    state: Arc<Mutex<Status>>,
    /// Idle and recording.
    icons: [HICON; 2],
}

thread_local! {
    static CONTEXT: RefCell<Option<Context>> = const { RefCell::new(None) };
}

/// Sent to every top-level window when Explorer (re)starts: the icon is
/// then added again.
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

impl Tray {
    pub fn new(ctx: egui::Context) -> Result<(Tray, mpsc::Receiver<Command>), String> {
        let (tx, rx) = mpsc::channel();
        let state = Arc::new(Mutex::new(Status::default()));
        let shared = Arc::clone(&state);
        let (ready_tx, ready_rx) = mpsc::channel::<Result<isize, String>>();
        let thread = std::thread::Builder::new()
            .name("tray".into())
            .spawn(move || unsafe {
                let hwnd = match create_window() {
                    Ok(hwnd) => hwnd,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                let icons = [load_icon(false), load_icon(true)];
                CONTEXT.with_borrow_mut(|c| *c = Some(Context { tx, ctx, state: shared, icons }));
                if !notify(hwnd, NIM_ADD) {
                    let _ = ready_tx.send(Err("the notification area refused the icon".into()));
                    let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd);
                } else {
                    let _ = ready_tx.send(Ok(hwnd.0 as isize));
                }
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                if let Some(c) = CONTEXT.take() {
                    for icon in c.icons {
                        let _ = DestroyIcon(icon);
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        match ready_rx.recv_timeout(std::time::Duration::from_secs(2)) {
            Ok(Ok(hwnd)) => Ok((Tray { hwnd, state, thread: Some(thread) }, rx)),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => Err("no answer from the tray thread".into()),
        }
    }

    /// What the icon and its menu show; the icon is set again only when it
    /// changes.
    pub fn set(&self, status: Status) {
        let mut state = self.state.lock().unwrap();
        if *state == status {
            return;
        }
        *state = status;
        drop(state);
        unsafe {
            let _ = PostMessageW(Some(HWND(self.hwnd as *mut _)), WM_STATE, WPARAM(0), LPARAM(0));
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        unsafe {
            let _ = PostMessageW(Some(HWND(self.hwnd as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A hidden top-level window: a message-only one would not receive the
/// `TaskbarCreated` broadcast.
unsafe fn create_window() -> Result<HWND, String> {
    unsafe {
        let instance = windows::Win32::System::LibraryLoader::GetModuleHandleW(None).map_err(|e| crate::win::describe(&e))?;
        let class = w!("qrec_tray");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(tray_proc),
            hInstance: instance.into(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        TASKBAR_CREATED.store(RegisterWindowMessageW(w!("TaskbarCreated")), Relaxed);
        CreateWindowExW(WINDOW_EX_STYLE(0), class, w!("qrec"), WS_POPUP, 0, 0, 0, 0, None, None, Some(instance.into()), None)
            .map_err(|e| crate::win::describe(&e))
    }
}

/// The app's icon at the size of the notification area (small icons:
/// 16 px at 100 %, 24 at 150 %).
fn load_icon(recording: bool) -> HICON {
    let size = unsafe { GetSystemMetrics(SM_CXSMICON) }.clamp(16, 64) as u32;
    let rgba = crate::icon::tray_rgba(size, recording);
    let entry = crate::icon::bmp_entry(size, &rgba);
    unsafe { CreateIconFromResourceEx(&entry, true, 0x0003_0000, size as i32, size as i32, LR_DEFAULTCOLOR) }.unwrap_or_default()
}

/// Adds the icon or sets it again from the state.
fn notify(hwnd: HWND, op: NOTIFY_ICON_MESSAGE) -> bool {
    CONTEXT.with_borrow(|c| {
        let Some(c) = c else { return false };
        let clock = c.state.lock().unwrap().clock.clone();
        let tip = match &clock {
            Some(clock) => format!("qrec: {} {clock}", tr!("recording", "запись")),
            None => "qrec".to_owned(),
        };
        let mut data = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: ICON_ID,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP,
            uCallbackMessage: WM_TRAY,
            hIcon: c.icons[usize::from(clock.is_some())],
            ..Default::default()
        };
        for (to, from) in data.szTip.iter_mut().zip(tip.encode_utf16().take(127)) {
            *to = from;
        }
        data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        unsafe {
            let done = Shell_NotifyIconW(op, &data).as_bool();
            if done && op == NIM_ADD {
                let _ = Shell_NotifyIconW(NIM_SETVERSION, &data);
            }
            done
        }
    })
}

fn send(command: Command) {
    CONTEXT.with_borrow(|c| {
        if let Some(c) = c {
            let _ = c.tx.send(command);
            c.ctx.request_repaint();
        }
    });
}

/// The menu at `(x, y)`; the command chosen, if any.
unsafe fn menu(hwnd: HWND, x: i32, y: i32) -> Option<Command> {
    let state = CONTEXT.with_borrow(|c| c.as_ref().map(|c| c.state.lock().unwrap().clone())).unwrap_or_default();
    let recording = state.clock.is_some();
    let toggle = wide(if recording { tr!("Stop recording", "Остановить запись") } else { tr!("Start recording", "Начать запись") });
    let show = wide(tr!("Show the window", "Показать окно"));
    let taskbar = wide(tr!("Not on the taskbar", "Не показывать на панели задач"));
    let close_to_tray = wide(tr!("Hide when closed", "Сворачивать в трей при закрытии"));
    let minimise_on_record = wide(tr!("Minimise when recording starts", "Сворачивать при начале записи"));
    let trim_after_record = wide(tr!("Trim after recording", "Обрезать после записи"));
    let about = wide(tr!("About…", "О программе…"));
    let exit = wide(tr!("Exit", "Выход"));
    unsafe {
        let menu = CreatePopupMenu().ok()?;
        let _ = AppendMenuW(menu, MF_STRING, MENU_TOGGLE, PCWSTR(toggle.as_ptr()));
        let _ = AppendMenuW(menu, MF_STRING, MENU_SHOW, PCWSTR(show.as_ptr()));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let check = |on: bool| if on { MF_STRING | MF_CHECKED } else { MF_STRING };
        let _ = AppendMenuW(menu, check(state.tray_only), MENU_TASKBAR, PCWSTR(taskbar.as_ptr()));
        let _ = AppendMenuW(menu, check(state.close_to_tray), MENU_CLOSE_TO_TRAY, PCWSTR(close_to_tray.as_ptr()));
        let _ = AppendMenuW(menu, check(state.minimise_on_record), MENU_MINIMISE_ON_RECORD, PCWSTR(minimise_on_record.as_ptr()));
        let _ = AppendMenuW(menu, check(state.trim_after_record), MENU_TRIM_AFTER_RECORD, PCWSTR(trim_after_record.as_ptr()));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, MENU_ABOUT, PCWSTR(about.as_ptr()));
        let _ = AppendMenuW(menu, MF_STRING, MENU_EXIT, PCWSTR(exit.as_ptr()));
        // In bold: what a click on the icon does.
        let _ = SetMenuDefaultItem(menu, MENU_SHOW as u32, 0);
        // Without it the menu would not close when clicking elsewhere.
        let _ = SetForegroundWindow(hwnd);
        let chosen = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY, x, y, None, hwnd, None).0 as usize;
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        match chosen {
            MENU_TOGGLE => Some(Command::Toggle),
            MENU_SHOW => Some(Command::Show),
            MENU_TASKBAR => Some(Command::Taskbar),
            MENU_CLOSE_TO_TRAY => Some(Command::CloseToTray),
            MENU_MINIMISE_ON_RECORD => Some(Command::MinimiseOnRecord),
            MENU_TRIM_AFTER_RECORD => Some(Command::TrimAfterRecord),
            MENU_ABOUT => Some(Command::About),
            MENU_EXIT => Some(Command::Exit),
            _ => None,
        }
    }
}

unsafe extern "system" fn tray_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        // With NOTIFYICON_VERSION_4 the event is in the low word of
        // lparam and the anchor of a menu in wparam.
        WM_TRAY => match (lparam.0 & 0xFFFF) as u32 {
            NIN_SELECT | NIN_KEYSELECT => send(Command::Show),
            WM_CONTEXTMENU => {
                let (x, y) = ((wparam.0 & 0xFFFF) as i16 as i32, ((wparam.0 >> 16) & 0xFFFF) as i16 as i32);
                if let Some(command) = unsafe { menu(hwnd, x, y) } {
                    send(command);
                }
            }
            _ => {}
        },
        WM_STATE => {
            notify(hwnd, NIM_MODIFY);
        }
        WM_DESTROY => {
            notify(hwnd, NIM_DELETE);
            unsafe { PostQuitMessage(0) };
        }
        _ if msg != 0 && msg == TASKBAR_CREATED.load(Relaxed) => {
            notify(hwnd, NIM_ADD);
        }
        _ => return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
    LRESULT(0)
}
