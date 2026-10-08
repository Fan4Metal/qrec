//! Thin Win32 helpers: wide strings, the interface language, message boxes,
//! COM, the clock of the recording, DPI awareness and known folders.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;

use windows::Win32::Foundation::HWND;
use windows::core::PCWSTR;

/// `s` as a NUL-terminated UTF-16 string for Win32.
pub fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

/// Whether Windows shows its interface in Russian (primary language of
/// the user's UI language, `LANG_RUSSIAN`).
pub fn ui_language_is_russian() -> bool {
    const LANG_RUSSIAN: u16 = 0x19;
    let id = unsafe { windows::Win32::Globalization::GetUserDefaultUILanguage() };
    id & 0x3FF == LANG_RUSSIAN
}

/// An error message box in front of everything.
pub fn error_box(title: &str, text: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_SETFOREGROUND, MessageBoxW};
    let (text, title) = (wide(text), wide(title));
    unsafe {
        MessageBoxW(None, PCWSTR(text.as_ptr()), PCWSTR(title.as_ptr()), MB_ICONERROR | MB_SETFOREGROUND);
    }
}

/// Per-monitor V2 DPI awareness, before any window exists: every
/// coordinate the program handles is then a physical pixel, the same
/// pixels Desktop Duplication delivers. eframe asks for the same, so this
/// only makes sure it holds for the overlay windows too.
pub fn set_dpi_aware() {
    use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
}

/// For the `--record` command line mode: writes to the console the program
/// was started from (the release build has none of its own).
pub fn attach_parent_console() {
    use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    let _ = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}

/// COM initialised in the multithreaded apartment on this thread for the
/// lifetime of the value: Media Foundation, WASAPI and DXGI objects are
/// free-threaded and shared between the worker threads.
pub struct Com {
    initialised: bool,
}

pub fn com_init_mta() -> Com {
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    Com { initialised: hr.is_ok() }
}

impl Drop for Com {
    fn drop(&mut self) {
        if self.initialised {
            unsafe { windows::Win32::System::Com::CoUninitialize() };
        }
    }
}

/// Keeps the window out of screen captures, Desktop Duplication included
/// (Windows 10 2004 and later). Returns whether Windows accepted it.
pub fn exclude_from_capture(hwnd: isize) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE};
    // QREC_NO_EXCLUDE=1 leaves the windows visible to a recording, to look
    // at them in one.
    if std::env::var_os("QREC_NO_EXCLUDE").is_some() {
        return false;
    }
    unsafe { SetWindowDisplayAffinity(HWND(hwnd as *mut _), WDA_EXCLUDEFROMCAPTURE).is_ok() }
}

/// The performance counter in 100-nanosecond units: the clock of a
/// recording. WASAPI stamps its packets with the same counter, so the
/// audio and the video share one timeline.
pub fn qpc_100ns() -> i64 {
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
    let (mut counter, mut freq) = (0i64, 0i64);
    unsafe {
        let _ = QueryPerformanceCounter(&mut counter);
        let _ = QueryPerformanceFrequency(&mut freq);
    }
    if freq <= 0 {
        return 0;
    }
    (counter as i128 * 10_000_000 / freq as i128) as i64
}

/// The local date and time as `2026-10-08_16-45-12`, for file names.
pub fn local_time_stamp() -> String {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!("{:04}-{:02}-{:02}_{:02}-{:02}-{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}

/// The user's Videos folder.
pub fn videos_dir() -> Option<PathBuf> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{FOLDERID_Videos, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
    unsafe {
        let path = SHGetKnownFolderPath(&FOLDERID_Videos, KF_FLAG_DEFAULT, None).ok()?;
        let text = path.to_string().ok();
        CoTaskMemFree(Some(path.0 as *const _));
        text.map(PathBuf::from)
    }
}

/// Opens Explorer on the folder with the file selected.
pub fn show_in_explorer(path: &std::path::Path) {
    let arg = format!("/select,{}", path.display());
    let _ = std::process::Command::new("explorer.exe").arg(arg).spawn();
}

/// Opens a folder in Explorer.
pub fn open_folder(path: &std::path::Path) {
    let _ = std::process::Command::new("explorer.exe").arg(path).spawn();
}

/// A readable form of a Windows error: its message, or the code when there
/// is none.
pub fn describe(e: &windows::core::Error) -> String {
    let msg = e.message();
    let msg = msg.trim();
    if msg.is_empty() { format!("{:#x}", e.code().0) } else { format!("{msg} ({:#x})", e.code().0) }
}

/// Hides the window, or shows it again in front of the others.
pub fn show_window(hwnd: isize, show: bool) {
    use windows::Win32::UI::WindowsAndMessaging::{SW_HIDE, SW_SHOW, SetForegroundWindow, ShowWindow};
    let hwnd = HWND(hwnd as *mut _);
    unsafe {
        let _ = ShowWindow(hwnd, if show { SW_SHOW } else { SW_HIDE });
        if show {
            let _ = SetForegroundWindow(hwnd);
        }
    }
}

/// The window's rectangle on the virtual screen, physical pixels:
/// `(left, top, right, bottom)`.
pub fn window_rect(hwnd: isize) -> (i32, i32, i32, i32) {
    let mut r = windows::Win32::Foundation::RECT::default();
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::GetWindowRect(HWND(hwnd as *mut _), &mut r);
    }
    (r.left, r.top, r.right, r.bottom)
}
