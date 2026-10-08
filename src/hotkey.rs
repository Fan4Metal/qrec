//! The global hotkey that starts and stops the recording, and the key
//! combination it is.

use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;

use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey, UnregisterHotKey};
use windows::Win32::UI::WindowsAndMessaging::{DispatchMessageW, GetMessageW, MSG, PostThreadMessageW, TranslateMessage, WM_HOTKEY, WM_QUIT};

/// A key with its modifiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chord {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
    /// Virtual-key code.
    pub key: u32,
}

impl Default for Chord {
    fn default() -> Chord {
        Chord { ctrl: true, alt: true, shift: false, win: false, key: b'R' as u32 }
    }
}

impl Chord {
    /// As shown in the window and kept in the settings: `Ctrl+Alt+R`.
    pub fn label(&self) -> String {
        let mut parts = Vec::new();
        if self.ctrl {
            parts.push("Ctrl".to_owned());
        }
        if self.alt {
            parts.push("Alt".to_owned());
        }
        if self.shift {
            parts.push("Shift".to_owned());
        }
        if self.win {
            parts.push("Win".to_owned());
        }
        parts.push(key_name(self.key));
        parts.join("+")
    }

    pub fn from_label(label: &str) -> Option<Chord> {
        let mut chord = Chord { ctrl: false, alt: false, shift: false, win: false, key: 0 };
        for part in label.split('+').map(str::trim) {
            match part.to_ascii_lowercase().as_str() {
                "ctrl" => chord.ctrl = true,
                "alt" => chord.alt = true,
                "shift" => chord.shift = true,
                "win" => chord.win = true,
                other => chord.key = key_code(other)?,
            }
        }
        (chord.key != 0).then_some(chord)
    }

    /// Whether the chord is one worth registering: a function key, or a
    /// key with a modifier (a bare letter would steal typing everywhere).
    pub fn is_usable(&self) -> bool {
        self.key != 0 && (self.ctrl || self.alt || self.win || (0x70..=0x7B).contains(&self.key))
    }

    /// The chord of an egui key press, if the key is one a hotkey can be.
    pub fn from_egui(key: egui::Key, modifiers: egui::Modifiers) -> Option<Chord> {
        let code = egui_key_code(key)?;
        Some(Chord { ctrl: modifiers.ctrl, alt: modifiers.alt, shift: modifiers.shift, win: false, key: code })
    }
}

/// A letter, a digit or `F1`..`F12` as a name.
fn key_name(code: u32) -> String {
    match code {
        0x30..=0x39 | 0x41..=0x5A => char::from(code as u8).to_string(),
        0x70..=0x7B => format!("F{}", code - 0x70 + 1),
        _ => format!("#{code}"),
    }
}

fn key_code(name: &str) -> Option<u32> {
    let bytes = name.as_bytes();
    if bytes.len() == 1 && bytes[0].is_ascii_alphanumeric() {
        return Some(u32::from(bytes[0].to_ascii_uppercase()));
    }
    if let Some(n) = name.strip_prefix('f').and_then(|n| n.parse::<u32>().ok())
        && (1..=12).contains(&n)
    {
        return Some(0x70 + n - 1);
    }
    name.strip_prefix('#').and_then(|n| n.parse().ok())
}

fn egui_key_code(key: egui::Key) -> Option<u32> {
    use egui::Key::*;
    Some(match key {
        A => 0x41, B => 0x42, C => 0x43, D => 0x44, E => 0x45, F => 0x46, G => 0x47, H => 0x48, I => 0x49, J => 0x4A, K => 0x4B,
        L => 0x4C, M => 0x4D, N => 0x4E, O => 0x4F, P => 0x50, Q => 0x51, R => 0x52, S => 0x53, T => 0x54, U => 0x55, V => 0x56,
        W => 0x57, X => 0x58, Y => 0x59, Z => 0x5A,
        Num0 => 0x30, Num1 => 0x31, Num2 => 0x32, Num3 => 0x33, Num4 => 0x34, Num5 => 0x35, Num6 => 0x36, Num7 => 0x37,
        Num8 => 0x38, Num9 => 0x39,
        F1 => 0x70, F2 => 0x71, F3 => 0x72, F4 => 0x73, F5 => 0x74, F6 => 0x75, F7 => 0x76, F8 => 0x77, F9 => 0x78,
        F10 => 0x79, F11 => 0x7A, F12 => 0x7B,
        _ => return None,
    })
}

/// A registered hotkey; unregistered when dropped. Presses arrive on the
/// receiver, and the egui context is woken for each.
pub struct Hotkey {
    thread_id: Arc<AtomicU32>,
    thread: Option<JoinHandle<()>>,
}

impl Hotkey {
    /// Registers `chord` system-wide. `Err` when Windows refuses it,
    /// usually because another program holds it.
    pub fn register(chord: Chord, ctx: egui::Context) -> Result<(Hotkey, mpsc::Receiver<()>), String> {
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let thread_id = Arc::new(AtomicU32::new(0));
        let id = Arc::clone(&thread_id);
        let thread = std::thread::Builder::new()
            .name("hotkey".into())
            .spawn(move || unsafe {
                id.store(GetCurrentThreadId(), Relaxed);
                let mut modifiers = MOD_NOREPEAT;
                if chord.ctrl {
                    modifiers |= MOD_CONTROL;
                }
                if chord.alt {
                    modifiers |= MOD_ALT;
                }
                if chord.shift {
                    modifiers |= MOD_SHIFT;
                }
                if chord.win {
                    modifiers |= MOD_WIN;
                }
                if let Err(e) = RegisterHotKey(None, 1, HOT_KEY_MODIFIERS(modifiers.0), chord.key) {
                    let _ = ready_tx.send(Err(crate::win::describe(&e)));
                    return;
                }
                let _ = ready_tx.send(Ok(()));
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    if msg.message == WM_HOTKEY {
                        let _ = tx.send(());
                        ctx.request_repaint();
                    }
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                let _ = UnregisterHotKey(None, 1);
            })
            .map_err(|e| e.to_string())?;
        match ready_rx.recv_timeout(std::time::Duration::from_secs(2)) {
            Ok(Ok(())) => Ok((Hotkey { thread_id, thread: Some(thread) }, rx)),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => Err("no answer from the hotkey thread".into()),
        }
    }
}

impl Drop for Hotkey {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_round_trip() {
        for label in ["Ctrl+Alt+R", "F9", "Ctrl+Shift+F12", "Alt+Win+5"] {
            let chord = Chord::from_label(label).unwrap();
            assert_eq!(chord.label(), label);
            assert!(chord.is_usable());
        }
        assert_eq!(Chord::from_label("Ctrl+"), None);
        assert!(!Chord::from_label("R").unwrap().is_usable());
        assert!(!Chord::from_label("Shift+R").unwrap().is_usable());
        assert_eq!(Chord::default().label(), "Ctrl+Alt+R");
    }
}
