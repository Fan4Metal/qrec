//! The recordings window docked to the main one: beside it, its top and
//! its height, at the right (or the left when the display has no room at
//! the right). Moving either moves the other.
//!
//! The main window is made its owner, so Windows keeps the two together
//! in the order of windows (bringing either to the front brings both),
//! hides it while the main one is minimised and shows it again with it.
//! Hiding the main window (to the notification area) does not hide an
//! owned one: that is done here, from `EVENT_OBJECT_HIDE` / `SHOW`. The
//! recordings window is drawn with the main window's pass, so it is never
//! on the screen without it.
//!
//! Each window is moved by Windows' own loop while it is dragged, during
//! which eframe paints little, so the other follows from a WinEvent hook
//! (`EVENT_OBJECT_LOCATIONCHANGE` of this process, out of context) on a
//! thread of its own, with `SetWindowPos`. A move made here is remembered,
//! so the event it causes is not taken for one by the user.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::thread::JoinHandle;

use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow};
use windows::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId};
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, EVENT_OBJECT_HIDE, EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_SHOW, GWLP_HWNDPARENT, GetMessageW, GetWindowRect, IsIconic,
    MSG, OBJID_WINDOW, PostThreadMessageW, SW_HIDE, SW_SHOWNA, SWP_NOACTIVATE, SWP_NOZORDER, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    TranslateMessage, WINEVENT_OUTOFCONTEXT, WM_QUIT,
};

/// The gap between the two windows, in points.
const GAP: i32 = 8;

/// Two windows kept together; let go of when dropped.
pub struct Dock {
    thread_id: Arc<AtomicU32>,
    thread: Option<JoinHandle<()>>,
    /// Whether the docked window is at the left of the main one.
    left: Arc<AtomicBool>,
    side: isize,
}

/// What the hook works with, on its thread.
struct State {
    main: HWND,
    side: HWND,
    left: Arc<AtomicBool>,
    /// Where this thread last put each window: the event of that move is
    /// not a move by the user.
    placed_main: Option<RECT>,
    placed_side: Option<RECT>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

impl Dock {
    /// Docks `side` to `main` (both top-level windows of this process) and
    /// keeps it there. Called on the thread of both windows.
    pub fn new(main: isize, side: isize) -> Dock {
        unsafe {
            SetWindowLongPtrW(HWND(side as *mut _), GWLP_HWNDPARENT, main);
        }
        let left = Arc::new(AtomicBool::new(false));
        let thread_id = Arc::new(AtomicU32::new(0));
        let (id, shared) = (Arc::clone(&thread_id), Arc::clone(&left));
        let thread = std::thread::Builder::new()
            .name("dock".into())
            .spawn(move || unsafe {
                id.store(GetCurrentThreadId(), Relaxed);
                let (main, side) = (HWND(main as *mut _), HWND(side as *mut _));
                shared.store(no_room_at_right(main, side), Relaxed);
                STATE.with_borrow_mut(|s| *s = Some(State { main, side, left: shared, placed_main: None, placed_side: None }));
                follow(main);
                let hook = SetWinEventHook(
                    EVENT_OBJECT_LOCATIONCHANGE,
                    EVENT_OBJECT_LOCATIONCHANGE,
                    None,
                    Some(on_event),
                    GetCurrentProcessId(),
                    0,
                    WINEVENT_OUTOFCONTEXT,
                );
                let shown = SetWinEventHook(
                    EVENT_OBJECT_SHOW,
                    EVENT_OBJECT_HIDE,
                    None,
                    Some(on_event),
                    GetCurrentProcessId(),
                    0,
                    WINEVENT_OUTOFCONTEXT,
                );
                if hook.is_invalid() || shown.is_invalid() {
                    log::warn!("no window event hook: the recordings window does not follow");
                }
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                for h in [hook, shown] {
                    if !h.is_invalid() {
                        let _ = UnhookWinEvent(h);
                    }
                }
            })
            .inspect_err(|e| log::warn!("no dock thread: {e}"))
            .ok();
        Dock { thread_id, thread, left, side }
    }

    /// Whether the docked window is at the left of the main one (its
    /// outer edge, the one that resizes it, is then its left).
    pub fn left(&self) -> bool {
        self.left.load(Relaxed)
    }
}

impl Drop for Dock {
    fn drop(&mut self) {
        // Owned no longer, should the window outlive the dock.
        unsafe {
            SetWindowLongPtrW(HWND(self.side as *mut _), GWLP_HWNDPARENT, 0);
        }
        let id = self.thread_id.load(Relaxed);
        if id != 0 {
            unsafe {
                let _ = PostThreadMessageW(id, WM_QUIT, WPARAM(0), LPARAM(0));
            }
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

unsafe extern "system" fn on_event(_hook: HWINEVENTHOOK, event: u32, hwnd: HWND, object: i32, _child: i32, _thread: u32, _time: u32) {
    if object != OBJID_WINDOW.0 {
        return;
    }
    match event {
        EVENT_OBJECT_LOCATIONCHANGE => follow(hwnd),
        EVENT_OBJECT_SHOW | EVENT_OBJECT_HIDE => mirror(hwnd, event == EVENT_OBJECT_SHOW),
        _ => {}
    }
}

/// The main window was hidden or shown: so is the docked one.
fn mirror(hwnd: HWND, shown: bool) {
    STATE.with_borrow(|state| {
        let Some(s) = state else { return };
        if hwnd != s.main {
            return;
        }
        unsafe {
            let _ = ShowWindow(s.side, if shown { SW_SHOWNA } else { SW_HIDE });
        }
    });
    if shown {
        follow(hwnd);
    }
}

/// `moved` changed its place: the other window goes beside it.
fn follow(moved: HWND) {
    STATE.with_borrow_mut(|state| {
        let Some(s) = state else { return };
        if moved != s.main && moved != s.side {
            return;
        }
        // A minimised window is parked off the screen: not followed.
        if unsafe { IsIconic(s.main).as_bool() || IsIconic(s.side).as_bool() } {
            return;
        }
        let (Some(main), Some(side)) = (rect(s.main), rect(s.side)) else { return };
        let gap = GAP * unsafe { GetDpiForWindow(s.main) }.max(96) as i32 / 96;
        let left = s.left.load(Relaxed);
        let height = main.bottom - main.top;
        if moved == s.main {
            if s.placed_main.take() == Some(main) {
                return;
            }
            let width = side.right - side.left;
            let x = if left { main.left - gap - width } else { main.right + gap };
            let want = RECT { left: x, top: main.top, right: x + width, bottom: main.top + height };
            if want != side {
                s.placed_side = Some(want);
                place(s.side, want);
            }
        } else {
            if s.placed_side.take() == Some(side) {
                return;
            }
            let width = main.right - main.left;
            let x = if left { side.right + gap } else { side.left - gap - width };
            let want = RECT { left: x, top: side.top, right: x + width, bottom: side.top + height };
            if want != main {
                s.placed_main = Some(want);
                place(s.main, want);
            }
            // Its height is the main window's.
            if side.bottom - side.top != height {
                let want = RECT { bottom: side.top + height, ..side };
                s.placed_side = Some(want);
                place(s.side, want);
            }
        }
    });
}

fn rect(hwnd: HWND) -> Option<RECT> {
    let mut r = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut r) }.ok().map(|_| r)
}

fn place(hwnd: HWND, r: RECT) {
    unsafe {
        let _ = SetWindowPos(hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

/// Whether `side` would go past the right of the work area of the
/// display `main` is on.
fn no_room_at_right(main: HWND, side: HWND) -> bool {
    let (Some(m), Some(s)) = (rect(main), rect(side)) else { return false };
    let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    unsafe {
        let monitor = MonitorFromWindow(main, MONITOR_DEFAULTTONEAREST);
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            return false;
        }
    }
    let gap = GAP * unsafe { GetDpiForWindow(main) }.max(96) as i32 / 96;
    m.right + gap + (s.right - s.left) > info.rcWork.right && m.left - gap - (s.right - s.left) >= info.rcWork.left
}
