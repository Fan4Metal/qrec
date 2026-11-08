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

/// Takes the caption out of a window's style. winit keeps `WS_CAPTION` on
/// an undecorated window (its `WM_NCCALCSIZE` gives the whole window to
/// the client area instead), and once the window has a region
/// (`round_window`) DWM no longer composes its frame and Windows paints
/// its basic title bar over the top of the client area. Whether the
/// style changed.
pub fn strip_caption(hwnd: isize) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        GWL_STYLE, GetWindowLongPtrW, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SetWindowLongPtrW, SetWindowPos, WS_CAPTION,
        WS_SYSMENU,
    };
    let hwnd = HWND(hwnd as *mut _);
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
        let wanted = style & !((WS_CAPTION.0 | WS_SYSMENU.0) as isize);
        if wanted == style {
            return false;
        }
        SetWindowLongPtrW(hwnd, GWL_STYLE, wanted);
        let _ = SetWindowPos(hwnd, None, 0, 0, 0, 0, SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        true
    }
}

/// Moves a file to the Recycle Bin, without asking or showing progress.
/// `Ok(true)` when it went there, `Ok(false)` when it was deleted outright:
/// a network or removable drive has no Recycle Bin.
pub fn recycle(path: &std::path::Path) -> Result<bool, String> {
    recycle_all(&[path])
}

/// Moves `paths` to the Recycle Bin in one operation (one undo in
/// Explorer); `Ok(false)` when the drive of the first has none and they
/// were deleted.
pub fn recycle_all(paths: &[&std::path::Path]) -> Result<bool, String> {
    use windows::Win32::UI::Shell::{FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, SHFILEOPSTRUCTW, SHFileOperationW};
    let Some(first) = paths.first() else { return Ok(true) };
    // The shell wants full paths; a relative one is taken from its own
    // current folder.
    let recycled = has_recycle_bin(&std::path::absolute(first).unwrap_or_else(|_| first.to_path_buf()));
    // A list of paths, each ended by a null, the list by another.
    let mut from = Vec::new();
    for path in paths {
        from.extend(wide(std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())));
    }
    from.push(0);
    let mut op = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE,
        pFrom: PCWSTR(from.as_ptr()),
        fFlags: (FOF_ALLOWUNDO.0 | FOF_NOCONFIRMATION.0 | FOF_NOERRORUI.0 | FOF_SILENT.0) as u16,
        ..Default::default()
    };
    match unsafe { SHFileOperationW(&mut op) } {
        0 if !op.fAnyOperationsAborted.as_bool() => Ok(recycled),
        0 => Err("cancelled".into()),
        // Mostly Win32 error codes (a sharing violation for a file open
        // in a player).
        code => Err(describe(&windows::core::Error::from_hresult(windows::core::HRESULT::from_win32(code as u32)))),
    }
}

/// Whether the drive of `path` has a Recycle Bin: a fixed drive whose bin
/// the shell can query (network and removable drives have none).
fn has_recycle_bin(path: &std::path::Path) -> bool {
    use std::path::Component;
    use windows::Win32::Storage::FileSystem::GetDriveTypeW;
    use windows::Win32::UI::Shell::{SHQUERYRBINFO, SHQueryRecycleBinW};
    const DRIVE_FIXED: u32 = 3;
    let Some(Component::Prefix(prefix)) = path.components().next() else { return false };
    let root = wide(format!("{}\\", prefix.as_os_str().to_string_lossy()));
    let mut info = SHQUERYRBINFO { cbSize: std::mem::size_of::<SHQUERYRBINFO>() as u32, ..Default::default() };
    unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) == DRIVE_FIXED && SHQueryRecycleBinW(PCWSTR(root.as_ptr()), &mut info).is_ok() }
}

/// The top-level window of this thread titled `title`, hidden or not: a
/// viewport of the program's (eframe gives no handle for one), made hidden
/// (`app::OwnWindow`). Not "the thread's window that is not the main one":
/// the thread has hidden windows of its own (the IME's), and two viewports.
pub fn thread_window(title: &str) -> Option<isize> {
    use windows::Win32::Foundation::LPARAM;
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::{EnumThreadWindows, GetWindowTextLengthW, GetWindowTextW};
    use windows::core::BOOL;
    struct Search {
        title: Vec<u16>,
        found: Option<isize>,
    }
    unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: `lparam` is the `Search` passed below, alive during the call.
        let search = unsafe { &mut *(lparam.0 as *mut Search) };
        let len = unsafe { GetWindowTextLengthW(hwnd) };
        if len as usize == search.title.len() {
            let mut text = vec![0u16; len as usize + 1];
            let n = unsafe { GetWindowTextW(hwnd, &mut text) };
            if text[..n.max(0) as usize] == search.title[..] {
                search.found = Some(hwnd.0 as isize);
                return false.into();
            }
        }
        true.into()
    }
    if title.is_empty() {
        return None;
    }
    let mut search = Search { title: title.encode_utf16().collect(), found: None };
    unsafe {
        let _ = EnumThreadWindows(GetCurrentThreadId(), Some(visit), LPARAM(&raw mut search as isize));
    }
    search.found
}

/// Hides the window from the screen while it goes on working and painting
/// (`DWMWA_CLOAK`), or shows it again: a new window is cloaked until it has
/// painted, since Windows shows it white until its first frame reaches DWM.
pub fn cloak(hwnd: isize, on: bool) {
    // dwmapi is declared by hand, to spare another feature of `windows`.
    #[link(name = "dwmapi")]
    unsafe extern "system" {
        fn DwmSetWindowAttribute(hwnd: isize, attribute: u32, value: *const core::ffi::c_void, size: u32) -> i32;
    }
    const DWMWA_CLOAK: u32 = 13;
    let value = i32::from(on);
    unsafe { DwmSetWindowAttribute(hwnd, DWMWA_CLOAK, (&raw const value).cast(), 4) };
}

/// Whether the window is visible (shown, minimised or not, cloaked or not).
pub fn is_visible(hwnd: isize) -> bool {
    unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindowVisible(HWND(hwnd as *mut _)).as_bool() }
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

/// A window hidden and shown again keeps `WDA_EXCLUDEFROMCAPTURE`, yet
/// Desktop Duplication then delivers it as a black shape instead of what
/// is behind it (Windows 10 22H2; minimising and restoring does not do
/// this). Set anew, the exclusion works again: called after showing.
fn renew_exclusion(hwnd: HWND) {
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowDisplayAffinity, SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE, WDA_NONE};
    let mut affinity = 0;
    unsafe {
        if GetWindowDisplayAffinity(hwnd, &mut affinity).is_ok() && affinity == WDA_EXCLUDEFROMCAPTURE.0 {
            let _ = SetWindowDisplayAffinity(hwnd, WDA_NONE);
            let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
        }
    }
}

/// Clips the window to its client area with corners of `radius` pixels.
/// DWM draws no shadow for a window with a region: its rectangular shadow
/// would outline the transparent corners. A region does not follow the
/// window's size: it is set again when the size changes.
pub fn round_window(hwnd: isize, radius: i32) -> bool {
    use windows::Win32::Foundation::{POINT, RECT};
    use windows::Win32::Graphics::Gdi::{ClientToScreen, CreateRoundRectRgn, SetWindowRgn};
    use windows::Win32::UI::WindowsAndMessaging::{GetClientRect, GetWindowRect};
    let hwnd = HWND(hwnd as *mut _);
    let (mut window, mut client, mut origin) = (RECT::default(), RECT::default(), POINT::default());
    unsafe {
        if GetWindowRect(hwnd, &mut window).is_err() || GetClientRect(hwnd, &mut client).is_err() || !ClientToScreen(hwnd, &mut origin).as_bool() {
            return false;
        }
        // winit leaves a pixel of non-client area above an undecorated
        // window: the client area is offset in the window. The region
        // takes window coordinates and excludes its right and bottom edge.
        let (x, y) = (origin.x - window.left, origin.y - window.top);
        let region = CreateRoundRectRgn(x, y, x + client.right + 1, y + client.bottom + 1, 2 * radius, 2 * radius);
        // The system owns the region from here on.
        !region.is_invalid() && SetWindowRgn(hwnd, Some(region), true) != 0
    }
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

/// Opens Explorer on the folder with the file selected, through the shell
/// (`SHOpenFolderAndSelectItems`): `explorer /select` could leave the
/// Explorer window behind this one. On a thread of its own, in a
/// single-threaded apartment as the shell wants, so a slow folder (a
/// network drive) does not hold up the window; `explorer /select` when the
/// shell fails.
pub fn show_in_explorer(path: &std::path::Path) {
    use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize};
    use windows::Win32::UI::Shell::{SHOpenFolderAndSelectItems, SHParseDisplayName};
    let path = path.to_path_buf();
    let spawned = std::thread::Builder::new().name("show in explorer".into()).spawn(move || {
        let name = wide(&path);
        let shown = unsafe {
            let initialised = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
            let mut pidl = std::ptr::null_mut();
            let mut shown = SHParseDisplayName(PCWSTR(name.as_ptr()), None, &mut pidl, 0, None);
            if shown.is_ok() {
                shown = SHOpenFolderAndSelectItems(pidl, None, 0);
                CoTaskMemFree(Some(pidl as *const _));
            }
            if initialised {
                CoUninitialize();
            }
            shown
        };
        if let Err(e) = shown {
            log::warn!("the shell did not show {}: {}", path.display(), describe(&e));
            let _ = std::process::Command::new("explorer.exe").arg(format!("/select,{}", path.display())).spawn();
        }
    });
    if let Err(e) = spawned {
        log::warn!("no thread to show the file: {e}");
    }
}

/// Opens `target` (a web address here) as Explorer would. eframe opens
/// links only with its `links` feature (the webbrowser crate), which is off.
pub fn shell_open(target: &str) -> bool {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let (verb, file) = (wide("open"), wide(target));
    let result = unsafe { ShellExecuteW(None, PCWSTR(verb.as_ptr()), PCWSTR(file.as_ptr()), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL) };
    // Values above 32 mean success.
    result.0 as isize > 32
}

/// Opens a folder in Explorer.
pub fn open_folder(path: &std::path::Path) {
    let _ = std::process::Command::new("explorer.exe").arg(path).spawn();
}

/// HTTPS GET of `path` on `host` through WinHTTP, which brings the system's
/// TLS and proxy settings (no HTTP crate); `headers` are extra request
/// lines joined with `\r\n`. Returns the status code and the body, refused
/// past `limit` bytes. Blocks for up to `timeout_ms` per step, so call it
/// off the UI thread.
pub fn https_get(host: &str, path: &str, agent: &str, headers: &str, timeout_ms: i32, limit: usize) -> Result<(u32, Vec<u8>), String> {
    use std::ffi::c_void;
    use std::ptr::{null, null_mut};
    use windows::Win32::Networking::WinHttp::*;

    struct Handle(*mut c_void);
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                let _ = WinHttpCloseHandle(self.0);
            }
        }
    }
    fn handle(h: *mut c_void) -> Result<Handle, String> {
        if h.is_null() { Err(winhttp_error(&windows::core::Error::from_thread())) } else { Ok(Handle(h)) }
    }
    let check = |r: windows::core::Result<()>| r.map_err(|e| winhttp_error(&e));

    let (agent, host, path) = (wide(agent), wide(host), wide(path));
    // Without the terminating NUL: the crate passes the slice's length.
    let headers: Vec<u16> = headers.encode_utf16().collect();
    let verb = wide("GET");
    let t = timeout_ms;
    // Declared in this order, the handles close request first.
    unsafe {
        let session = handle(WinHttpOpen(PCWSTR(agent.as_ptr()), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, PCWSTR::null(), PCWSTR::null(), 0))?;
        check(WinHttpSetTimeouts(session.0, t, t, t, t))?;
        let connection = handle(WinHttpConnect(session.0, PCWSTR(host.as_ptr()), INTERNET_DEFAULT_HTTPS_PORT, 0))?;
        let request = handle(WinHttpOpenRequest(
            connection.0,
            PCWSTR(verb.as_ptr()),
            PCWSTR(path.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            null(),
            WINHTTP_FLAG_SECURE,
        ))?;
        check(WinHttpSendRequest(request.0, Some(&headers), None, 0, 0, 0))?;
        check(WinHttpReceiveResponse(request.0, null_mut()))?;
        let mut status = 0u32;
        let mut len = size_of::<u32>() as u32;
        check(WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some((&raw mut status).cast()),
            &mut len,
            null_mut(),
        ))?;
        let mut body = Vec::new();
        let mut buf = [0u8; 16 * 1024];
        loop {
            let mut read = 0u32;
            check(WinHttpReadData(request.0, buf.as_mut_ptr().cast(), buf.len() as u32, &mut read))?;
            if read == 0 {
                break;
            }
            body.extend_from_slice(&buf[..read as usize]);
            if body.len() > limit {
                return Err(format!("the answer is larger than {limit} bytes"));
            }
        }
        Ok((status, body))
    }
}

/// A WinHTTP error in words; the system's message table lacks them.
fn winhttp_error(e: &windows::core::Error) -> String {
    // HRESULT_FROM_WIN32: the facility 7 and the code in the low word.
    let hr = e.code().0 as u32;
    let code = if hr >> 16 == 0x8007 { hr & 0xFFFF } else { hr };
    match code {
        12002 => "the server did not answer in time".into(),
        12007 => "the server name could not be resolved".into(),
        12029 | 12030 => "the connection to the server failed".into(),
        12175 => "the secure connection failed".into(),
        _ => format!("WinHTTP error {code}"),
    }
}

/// A readable form of a Windows error: its message, or the code when there
/// is none.
pub fn describe(e: &windows::core::Error) -> String {
    let msg = e.message();
    let msg = msg.trim();
    if msg.is_empty() { format!("{:#x}", e.code().0) } else { format!("{msg} ({:#x})", e.code().0) }
}

/// Hides the window, or shows it again in front of the others (restored
/// when it was minimised).
pub fn show_window(hwnd: isize, show: bool) {
    use windows::Win32::UI::WindowsAndMessaging::{IsIconic, SW_HIDE, SW_RESTORE, SW_SHOW, SetForegroundWindow, ShowWindow};
    let hwnd = HWND(hwnd as *mut _);
    unsafe {
        let how = match show {
            false => SW_HIDE,
            true if IsIconic(hwnd).as_bool() => SW_RESTORE,
            true => SW_SHOW,
        };
        let _ = ShowWindow(hwnd, how);
        if show {
            renew_exclusion(hwnd);
            let _ = SetForegroundWindow(hwnd);
        }
    }
}

/// Whether the window is on the screen: visible and not minimised.
pub fn is_shown(hwnd: isize) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{IsIconic, IsWindowVisible};
    let hwnd = HWND(hwnd as *mut _);
    unsafe { IsWindowVisible(hwnd).as_bool() && !IsIconic(hwnd).as_bool() }
}

/// Whether the window is minimised.
pub fn is_minimised(hwnd: isize) -> bool {
    unsafe { windows::Win32::UI::WindowsAndMessaging::IsIconic(HWND(hwnd as *mut _)).as_bool() }
}

/// Puts the window's button on the taskbar or takes it off. winit marks
/// its windows for the taskbar (`WS_EX_APPWINDOW`), so without a button
/// the window is a tool window instead, which also leaves it out of
/// Alt+Tab. The taskbar notices the change when the window is shown:
/// `refresh` hides and shows a visible window; without it only the style
/// is set, which is enough before the window is shown or to set again
/// what winit rewrote when it changed the window's state.
pub fn set_taskbar_button(hwnd: isize, show: bool, refresh: bool) {
    use windows::Win32::UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GetWindowLongPtrW, IsWindowVisible, SW_HIDE, SW_SHOW, SetWindowLongPtrW, ShowWindow, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
    };
    let hwnd = HWND(hwnd as *mut _);
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        let wanted = if show {
            (style & !WS_EX_TOOLWINDOW.0) | WS_EX_APPWINDOW.0
        } else {
            (style & !WS_EX_APPWINDOW.0) | WS_EX_TOOLWINDOW.0
        };
        if wanted == style {
            return;
        }
        let visible = refresh && IsWindowVisible(hwnd).as_bool();
        if visible {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, wanted as isize);
        if visible {
            let _ = ShowWindow(hwnd, SW_SHOW);
            renew_exclusion(hwnd);
        }
    }
}

/// Moves the window's outer top-left corner to `(x, y)`, physical pixels
/// on the virtual screen.
pub fn move_window(hwnd: isize, x: i32, y: i32) {
    use windows::Win32::UI::WindowsAndMessaging::{SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SetWindowPos};
    unsafe {
        let _ = SetWindowPos(HWND(hwnd as *mut _), None, x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
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
