//! One window of qrec at a time. The first copy holds a named mutex and a
//! message-only window; a copy started after it hands its request to that
//! window with `WM_COPYDATA` and exits: the running window comes to the
//! front, or opens the file given in the trimming window, or (`--quit`,
//! for the installer) closes. The command line modes do not take part.

use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, CreateWindowExW, DefWindowProcW, DispatchMessageW, FindWindowExW, GetMessageW, GetWindowThreadProcessId,
    HWND_MESSAGE, MSG, RegisterClassExW, SMTO_ABORTIFHUNG, SendMessageTimeoutW, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_COPYDATA,
    WNDCLASSEXW,
};
use windows::core::{PCWSTR, w};

/// What a copy started later asks of the running one.
#[derive(Debug)]
pub enum Request {
    /// Bring the window to the front.
    Show,
    /// Open the file in the trimming window.
    Open(PathBuf),
    /// Close the program, as the cross would: a recording is completed,
    /// the settings saved.
    Quit,
}

const MUTEX: PCWSTR = w!("Local\\qrec_single_instance");
const CLASS: PCWSTR = w!("qrec_instance");
/// `COPYDATASTRUCT::dwData` of the requests; a file comes as UTF-16.
const SHOW: usize = 1;
const OPEN: usize = 2;
const QUIT: usize = 3;

/// The mutex that marks the running copy, held until the process ends.
pub struct Claim(#[allow(dead_code)] HANDLE);

/// The claim of the first copy; `None` when another copy runs already.
pub fn claim() -> Option<Claim> {
    let handle = unsafe { CreateMutexW(None, false, MUTEX) }.ok()?;
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(handle) };
        return None;
    }
    Some(Claim(handle))
}

/// Hands `open` (or a request to show the window) to the running copy;
/// waits a few seconds for its window when it is still starting. Whether
/// the request was delivered.
pub fn hand_over(open: Option<&Path>) -> bool {
    deliver(if open.is_some() { OPEN } else { SHOW }, open)
}

/// Asks the running copy to close and waits for it to be gone (the mutex
/// free), up to `timeout`. `None` when no copy runs; else whether it went.
pub fn quit_running(timeout: Duration) -> Option<bool> {
    if claim().is_some() {
        return None;
    }
    if !deliver(QUIT, None) {
        return Some(false);
    }
    let started = Instant::now();
    while started.elapsed() < timeout {
        if claim().is_some() {
            return Some(true);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Some(false)
}

fn deliver(request: usize, open: Option<&Path>) -> bool {
    let started = Instant::now();
    let hwnd = loop {
        if let Ok(hwnd) = unsafe { FindWindowExW(Some(HWND_MESSAGE), None, CLASS, None) } {
            break hwnd;
        }
        // A first start of the GL window can take a while.
        if started.elapsed() > Duration::from_secs(10) {
            log::warn!("no window of the running copy");
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    // The copy just started may bring a window to the front; the running
    // one may not, unless it is let.
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    let _ = unsafe { AllowSetForegroundWindow(pid) };
    let path: Vec<u16> = open.map(crate::win::wide).unwrap_or_default();
    let data = COPYDATASTRUCT {
        dwData: request,
        cbData: (path.len() * 2) as u32,
        lpData: path.as_ptr() as *mut _,
    };
    let mut result = 0usize;
    let sent = unsafe {
        SendMessageTimeoutW(
            hwnd,
            WM_COPYDATA,
            WPARAM(0),
            LPARAM(&data as *const _ as isize),
            SMTO_ABORTIFHUNG,
            5000,
            Some(&mut result),
        )
    };
    sent.0 != 0 && result == 1
}

/// Where the window's procedure puts what it receives.
static LISTENER: OnceLock<Mutex<(mpsc::Sender<Request>, egui::Context)>> = OnceLock::new();

/// Starts the window that copies started later talk to, on a thread of
/// its own; requests arrive on the receiver, with a repaint of `ctx`.
pub fn listen(ctx: egui::Context) -> Result<mpsc::Receiver<Request>, String> {
    let (tx, rx) = mpsc::channel();
    LISTENER.set(Mutex::new((tx, ctx))).map_err(|_| "already listening".to_owned())?;
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
    std::thread::Builder::new()
        .name("instance".into())
        .spawn(move || unsafe {
            let instance = match windows::Win32::System::LibraryLoader::GetModuleHandleW(None) {
                Ok(i) => i,
                Err(e) => {
                    let _ = ready_tx.send(Err(crate::win::describe(&e)));
                    return;
                }
            };
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(instance_proc),
                hInstance: instance.into(),
                lpszClassName: CLASS,
                ..Default::default()
            };
            RegisterClassExW(&wc);
            let created = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                CLASS,
                w!("qrec"),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                Some(instance.into()),
                None,
            );
            let hwnd = match created {
                Ok(hwnd) => hwnd,
                Err(e) => {
                    let _ = ready_tx.send(Err(crate::win::describe(&e)));
                    return;
                }
            };
            // A copy started without administrator rights may still talk
            // to one that has them.
            let _ = windows::Win32::UI::WindowsAndMessaging::ChangeWindowMessageFilterEx(
                hwnd,
                windows::Win32::UI::WindowsAndMessaging::WM_COPYDATA,
                windows::Win32::UI::WindowsAndMessaging::MSGFLT_ALLOW,
                None,
            );
            let _ = ready_tx.send(Ok(()));
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        })
        .map_err(|e| e.to_string())?;
    ready_rx.recv_timeout(Duration::from_secs(2)).map_err(|_| "no answer from the instance thread".to_owned())??;
    Ok(rx)
}

unsafe extern "system" fn instance_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg != WM_COPYDATA {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    let data = unsafe { &*(lparam.0 as *const COPYDATASTRUCT) };
    let request = match data.dwData {
        SHOW => Request::Show,
        QUIT => Request::Quit,
        OPEN if !data.lpData.is_null() => {
            let units = unsafe { std::slice::from_raw_parts(data.lpData as *const u16, data.cbData as usize / 2) };
            let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
            Request::Open(PathBuf::from(std::ffi::OsString::from_wide(&units[..end])))
        }
        _ => return LRESULT(0),
    };
    log::debug!("from another copy: {request:?}");
    if let Some(listener) = LISTENER.get() {
        let listener = listener.lock().unwrap_or_else(|e| e.into_inner());
        let _ = listener.0.send(request);
        listener.1.request_repaint();
    }
    LRESULT(1)
}
