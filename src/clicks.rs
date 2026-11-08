//! The mouse buttons, for the clicks drawn into a recording: Desktop
//! Duplication reports where the pointer is, not what its buttons do. A
//! low-level mouse hook on a thread of its own notes each press with the
//! time on the recording's clock (`win::qpc_100ns`) and where the pointer
//! is, on the virtual screen in physical pixels (the process is DPI
//! aware), and which buttons are held.

use std::cell::RefCell;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, HC_ACTION, MSG, MSLLHOOKSTRUCT, PostThreadMessageW, SetWindowsHookExW,
    TranslateMessage, UnhookWindowsHookEx, WH_MOUSE_LL, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_QUIT, WM_RBUTTONDOWN,
    WM_RBUTTONUP,
};

/// How long a press is kept: longer than its ring lasts.
const KEEP: i64 = 10_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
}

/// A press: when, where, which button.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Press {
    pub time: i64,
    pub x: i32,
    pub y: i32,
    pub button: Button,
}

/// What the hook has seen.
#[derive(Clone, Debug, Default)]
pub struct State {
    /// The presses of the last second, oldest first.
    pub presses: Vec<Press>,
    /// The buttons held now, if any (the left one when both are).
    pub held: Option<Button>,
    /// Where the pointer is.
    pub x: i32,
    pub y: i32,
}

/// The hook; removed when dropped.
pub struct Clicks {
    state: Arc<Mutex<State>>,
    thread_id: Arc<AtomicU32>,
    thread: Option<JoinHandle<()>>,
}

thread_local! {
    static STATE: RefCell<Option<Arc<Mutex<State>>>> = const { RefCell::new(None) };
}

impl Clicks {
    pub fn start() -> Clicks {
        let state = Arc::new(Mutex::new(State::default()));
        let thread_id = Arc::new(AtomicU32::new(0));
        let (shared, id) = (Arc::clone(&state), Arc::clone(&thread_id));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("clicks".into())
            .spawn(move || unsafe {
                id.store(windows::Win32::System::Threading::GetCurrentThreadId(), Relaxed);
                STATE.with_borrow_mut(|s| *s = Some(shared));
                let hook = SetWindowsHookExW(WH_MOUSE_LL, Some(hook_proc), None, 0);
                let _ = ready_tx.send(());
                let Ok(hook) = hook.inspect_err(|e| log::warn!("no mouse hook, no clicks: {}", crate::win::describe(e))) else { return };
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                let _ = UnhookWindowsHookEx(hook);
            })
            .ok();
        let _ = ready_rx.recv_timeout(std::time::Duration::from_secs(2));
        Clicks { state, thread_id, thread }
    }

    /// What has happened, the presses older than a second forgotten.
    pub fn state(&self, now: i64) -> State {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.presses.retain(|p| now - p.time < KEEP);
        state.clone()
    }
}

impl Drop for Clicks {
    fn drop(&mut self) {
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

/// Notes the event and passes it on at once: Windows takes the hook away
/// from a program that keeps the mouse waiting.
unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        let message = wparam.0 as u32;
        STATE.with_borrow(|s| {
            if let Some(state) = s
                && let Ok(mut state) = state.lock()
            {
                note(&mut state, message, info.pt.x, info.pt.y, crate::win::qpc_100ns());
            }
        });
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn note(state: &mut State, message: u32, x: i32, y: i32, time: i64) {
    (state.x, state.y) = (x, y);
    let button = match message {
        WM_LBUTTONDOWN | WM_LBUTTONUP => Button::Left,
        WM_RBUTTONDOWN | WM_RBUTTONUP => Button::Right,
        WM_MOUSEMOVE => return,
        _ => return,
    };
    match message {
        WM_LBUTTONDOWN | WM_RBUTTONDOWN => {
            state.presses.push(Press { time, x, y, button });
            // Not held for ever when a release was missed.
            state.presses.retain(|p| time - p.time < KEEP);
            if state.held.is_none() || button == Button::Left {
                state.held = Some(button);
            }
        }
        _ if state.held == Some(button) => state.held = None,
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_presses_and_held_buttons() {
        let mut state = State::default();
        note(&mut state, WM_MOUSEMOVE, 5, 6, 0);
        assert_eq!((state.x, state.y, state.held), (5, 6, None));
        note(&mut state, WM_LBUTTONDOWN, 10, 20, 100);
        assert_eq!(state.held, Some(Button::Left));
        assert_eq!(state.presses, vec![Press { time: 100, x: 10, y: 20, button: Button::Left }]);
        note(&mut state, WM_RBUTTONDOWN, 10, 20, 200);
        assert_eq!(state.held, Some(Button::Left));
        note(&mut state, WM_LBUTTONUP, 12, 20, 300);
        assert_eq!(state.held, None);
        note(&mut state, WM_LBUTTONDOWN, 0, 0, 300 + KEEP);
        assert_eq!(state.presses.len(), 1);
    }
}
