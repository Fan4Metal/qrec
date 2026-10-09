//! The windows drawn over the screen: the area selection, and the frame
//! around the area while it is recorded.
//!
//! Both are plain Win32 windows on their own threads, with their own
//! message loops: the selection covers the whole virtual screen and
//! needs per-pixel alpha (`UpdateLayeredWindow` with a DIB), the frame is
//! a colour-keyed layered window that clicks pass through. Both are kept
//! out of the capture.

use std::cell::RefCell;
use std::sync::atomic::{AtomicIsize, Ordering::Relaxed};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, ANTIALIASED_QUALITY, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, CLIP_DEFAULT_PRECIS,
    CreateCompatibleDC, CreateDIBSection, CreateFontW, CreateSolidBrush, DEFAULT_CHARSET, DEFAULT_PITCH, DIB_RGB_COLORS,
    DeleteDC, DeleteObject, FF_DONTCARE, FW_NORMAL, FillRect, GdiFlush, GetTextExtentPoint32W, HBITMAP, HBRUSH, HDC, HFONT,
    HGDIOBJ, OUT_DEFAULT_PRECIS, PAINTSTRUCT, BeginPaint, EndPaint, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
    TextOutW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetCapture, ReleaseCapture, SetCapture, VK_ESCAPE};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW,
    GetSystemMetrics, HWND_TOPMOST, IDC_CROSS, LWA_COLORKEY, LoadCursorW, MSG, PostQuitMessage, PostThreadMessageW,
    RegisterClassExW, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SW_SHOW,
    SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetForegroundWindow, SetLayeredWindowAttributes,
    SetWindowPos, ShowWindow, TranslateMessage, ULW_ALPHA, UPDATELAYEREDWINDOWINFO, UpdateLayeredWindowIndirect,
    WM_DESTROY, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_PAINT, WM_QUIT, WM_RBUTTONDOWN, WNDCLASSEXW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::{PCWSTR, w};

use crate::display::{self, Monitor};
use crate::region::{Aspect, Rect, Region};
use crate::win;

/// Alpha of the veil over the parts not selected.
const DIM: u32 = 0x66_000000;
const WHITE: u32 = 0xFF_FFFFFF;
const LABEL_BACKGROUND: u32 = 0xFF_202020;

/// Opens the selection over all monitors and reports what was chosen:
/// the area, of the proportions `aspect` and fitted to the monitor the
/// drag started on, or `None` when cancelled or too small. `done` runs on the selection's thread when it
/// closes, before the result is sent.
pub fn select(monitors: Vec<Monitor>, aspect: Aspect, done: impl FnOnce() + Send + 'static) -> mpsc::Receiver<Option<Region>> {
    let (tx, rx) = mpsc::channel();
    let done = std::sync::Arc::new(std::sync::Mutex::new(Some(Box::new(done) as Box<dyn FnOnce() + Send>)));
    let thread_done = done.clone();
    let spawned = std::thread::Builder::new().name("select".into()).spawn(move || {
        let result = run_selection(monitors, aspect);
        if let Some(done) = thread_done.lock().ok().and_then(|mut d| d.take()) {
            done();
        }
        let _ = tx.send(result);
    });
    if spawned.is_err() {
        if let Some(done) = done.lock().ok().and_then(|mut d| d.take()) {
            done();
        }
        let (tx, rx) = mpsc::channel();
        let _ = tx.send(None);
        return rx;
    }
    rx
}

struct Selection {
    monitors: Vec<Monitor>,
    aspect: Aspect,
    origin: (i32, i32),
    size: (i32, i32),
    dc: HDC,
    bitmap: HBITMAP,
    old_bitmap: HGDIOBJ,
    font: HFONT,
    pixels: *mut u32,
    start: Option<(i32, i32)>,
    current: (i32, i32),
    /// What the last paint touched, to be painted over next time.
    painted: Option<Rect>,
    result: Option<Region>,
    done: bool,
}

impl Selection {
    /// The rectangle dragged from the start to `end`, and what is recorded
    /// of it: fitted to the monitor the drag started on, or `None` when
    /// too small. `None` before the drag.
    fn dragged(&self, end: (i32, i32)) -> Option<(Region, Option<Region>)> {
        let start = self.start?;
        let free = Region::from_drag(start, end);
        let monitor = display::monitor_at(&self.monitors, start.0, start.1).or_else(|| display::monitor_of(&self.monitors, &free.rect()));
        let Some(monitor) = monitor else { return Some((free, None)) };
        let region = Region::drag(start, end, self.aspect, monitor.rect);
        Some((region, region.fit(monitor.rect)))
    }
}

thread_local! {
    static SELECTION: RefCell<Option<Selection>> = const { RefCell::new(None) };
}

fn run_selection(monitors: Vec<Monitor>, aspect: Aspect) -> Option<Region> {
    unsafe {
        let instance = windows::Win32::System::LibraryLoader::GetModuleHandleW(None).ok()?;
        let class = w!("qrec_select");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(selection_proc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_CROSS).unwrap_or_default(),
            lpszClassName: class,
            ..Default::default()
        };
        // Fails when already registered by an earlier selection: fine.
        RegisterClassExW(&wc);
        let origin = (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN));
        let size = (GetSystemMetrics(SM_CXVIRTUALSCREEN), GetSystemMetrics(SM_CYVIRTUALSCREEN));
        if size.0 <= 0 || size.1 <= 0 {
            return None;
        }
        let header = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size.0,
            biHeight: -size.1,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };
        let info = BITMAPINFO { bmiHeader: header, ..Default::default() };
        let mut pixels = std::ptr::null_mut();
        let bitmap = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut pixels, None, 0).ok()?;
        let dc = CreateCompatibleDC(None);
        let old_bitmap = SelectObject(dc, bitmap.into());
        // 12 points at the system's scale (18 pixels at 150 %, 12 at 100 %).
        let dpi = windows::Win32::UI::HiDpi::GetDpiForSystem().max(96) as i32;
        let font = CreateFontW(
            -(12 * dpi / 72),
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY,
            (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
            w!("Segoe UI"),
        );
        SelectObject(dc, font.into());
        SetBkMode(dc, TRANSPARENT);
        let mut current = POINT::default();
        let _ = GetCursorPos(&mut current);
        SELECTION.with(|s| {
            *s.borrow_mut() = Some(Selection {
                monitors,
                aspect,
                origin,
                size,
                dc,
                bitmap,
                old_bitmap,
                font,
                pixels: pixels.cast(),
                start: None,
                current: (current.x, current.y),
                painted: None,
                result: None,
                done: false,
            });
        });
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            class,
            w!("qrec"),
            WS_POPUP,
            origin.0,
            origin.1,
            size.0,
            size.1,
            None,
            None,
            Some(instance.into()),
            None,
        );
        let result = match hwnd {
            Ok(hwnd) => {
                win::exclude_from_capture(hwnd.0 as isize);
                paint_selection(hwnd, true);
                let _ = ShowWindow(hwnd, SW_SHOW);
                let _ = SetForegroundWindow(hwnd);
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                SELECTION.with(|s| s.borrow().as_ref().and_then(|s| s.result))
            }
            Err(_) => None,
        };
        SELECTION.with(|s| {
            if let Some(sel) = s.borrow_mut().take() {
                SelectObject(sel.dc, sel.old_bitmap);
                let _ = DeleteObject(sel.font.into());
                let _ = DeleteObject(sel.bitmap.into());
                let _ = DeleteDC(sel.dc);
            }
        });
        result
    }
}

unsafe extern "system" fn selection_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_LBUTTONDOWN => {
            let pos = position(hwnd, lparam);
            SELECTION.with(|s| {
                if let Some(s) = s.borrow_mut().as_mut() {
                    s.start = Some(pos);
                    s.current = pos;
                }
            });
            unsafe { SetCapture(hwnd) };
            paint_selection(hwnd, false);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let pos = position(hwnd, lparam);
            let dragging = SELECTION.with(|s| {
                s.borrow_mut().as_mut().map(|s| {
                    s.current = pos;
                    s.start.is_some()
                })
            });
            if dragging == Some(true) {
                paint_selection(hwnd, false);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let pos = position(hwnd, lparam);
            let finished = SELECTION.with(|s| {
                let mut s = s.borrow_mut();
                let Some(s) = s.as_mut() else { return false };
                let Some((_, fitted)) = s.dragged(pos) else { return false };
                s.result = fitted;
                s.done = true;
                true
            });
            if finished {
                unsafe {
                    if GetCapture() == hwnd {
                        let _ = ReleaseCapture();
                    }
                    let _ = DestroyWindow(hwnd);
                }
            }
            LRESULT(0)
        }
        WM_RBUTTONDOWN => {
            cancel(hwnd);
            LRESULT(0)
        }
        WM_KEYDOWN if wparam.0 as u16 == VK_ESCAPE.0 => {
            cancel(hwnd);
            LRESULT(0)
        }
        WM_PAINT => {
            // A layered window with UpdateLayeredWindow paints itself.
            let mut ps = PAINTSTRUCT::default();
            unsafe {
                BeginPaint(hwnd, &mut ps);
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn cancel(hwnd: HWND) {
    SELECTION.with(|s| {
        if let Some(s) = s.borrow_mut().as_mut() {
            s.result = None;
            s.done = true;
        }
    });
    unsafe {
        if GetCapture() == hwnd {
            let _ = ReleaseCapture();
        }
        let _ = DestroyWindow(hwnd);
    }
}

/// The pointer position of a mouse message, in screen coordinates: the
/// window sits at the virtual screen's origin, so the client coordinates
/// (signed: the mouse is captured) only need that offset.
fn position(_hwnd: HWND, lparam: LPARAM) -> (i32, i32) {
    let x = (lparam.0 & 0xffff) as u16 as i16 as i32;
    let y = ((lparam.0 >> 16) & 0xffff) as u16 as i16 as i32;
    SELECTION.with(|s| s.borrow().as_ref().map_or((x, y), |s| (x + s.origin.0, y + s.origin.1)))
}

/// Draws the veil, the selection and its size, and shows the result;
/// only the part that changed, unless `all`.
fn paint_selection(hwnd: HWND, all: bool) {
    SELECTION.with(|s| {
        let mut s = s.borrow_mut();
        let Some(s) = s.as_mut() else { return };
        if s.done {
            return;
        }
        let (w, h) = s.size;
        let whole = Rect { left: 0, top: 0, right: w, bottom: h };
        let dragged = s.dragged(s.current);
        let selection = dragged.map(|(region, _)| {
            let r = region.rect();
            Rect { left: r.left - s.origin.0, top: r.top - s.origin.1, right: r.right - s.origin.0, bottom: r.bottom - s.origin.1 }
        });
        // The hint at the top of the monitor the pointer is on.
        let hint_monitor = display::monitor_at(&s.monitors, s.current.0, s.current.1).map(|m| m.rect);
        let hint = tr!("Drag to select the area, Esc to cancel", "Выделите область мышью, Esc для отмены");
        let hint_rect = hint_monitor.map(|m| {
            let (tw, th) = text_size(s.dc, hint);
            let cx = (m.left + m.right) / 2 - s.origin.0;
            let top = m.top - s.origin.1 + 40;
            Rect { left: cx - tw / 2 - 12, top, right: cx + tw / 2 + 12, bottom: top + th + 12 }
        });
        let label = selection.map(|r| {
            // The size that will be recorded: fitted to the monitor, even.
            let fitted = dragged.and_then(|(_, fitted)| fitted);
            let text = match fitted {
                Some(f) => format!("{} × {}", f.width, f.height),
                None => format!("{} × {} ({})", r.width(), r.height(), tr!("too small", "слишком мало")),
            };
            let (tw, th) = text_size(s.dc, &text);
            // Kept on the monitor the drag started on: the virtual screen
            // can have parts no monitor shows.
            let bounds = s
                .start
                .and_then(|p| display::monitor_at(&s.monitors, p.0, p.1))
                .map_or(whole, |m| Rect {
                    left: m.rect.left - s.origin.0,
                    top: m.rect.top - s.origin.1,
                    right: m.rect.right - s.origin.0,
                    bottom: m.rect.bottom - s.origin.1,
                });
            let mut left = r.left.max(bounds.left);
            let mut top = r.bottom + 6;
            if top + th + 8 > bounds.bottom {
                top = (r.top - th - 14).max(bounds.top);
            }
            if left + tw + 16 > bounds.right {
                left = (bounds.right - tw - 16).max(bounds.left);
            }
            (Rect { left, top, right: left + tw + 16, bottom: top + th + 8 }, text)
        });
        // The white outline lies outside the selection: it belongs to what
        // this paint touches, or a new edge more than a pixel away from the
        // last one would be drawn but not shown.
        const OUTLINE: i32 = 2;
        let outline = selection.map(|r| Rect { left: r.left - OUTLINE, top: r.top - OUTLINE, right: r.right + OUTLINE, bottom: r.bottom + OUTLINE });
        let mut touched = union(outline, hint_rect);
        touched = union(touched, label.as_ref().map(|l| l.0));
        let dirty = if all { Some(whole) } else { union(s.painted, touched) };
        let Some(dirty) = dirty.and_then(|d| d.intersect(&whole)) else { return };
        let pixels = s.pixels;
        let stride = w as usize;
        let fill = |r: Rect, value: u32| {
            let Some(r) = r.intersect(&whole) else { return };
            for y in r.top..r.bottom {
                let row = unsafe { std::slice::from_raw_parts_mut(pixels.add(y as usize * stride + r.left as usize), r.width() as usize) };
                row.fill(value);
            }
        };
        fill(dirty, DIM);
        if let Some(r) = selection {
            fill(r, 0);
            let b = OUTLINE;
            fill(Rect { left: r.left - b, top: r.top - b, right: r.right + b, bottom: r.top }, WHITE);
            fill(Rect { left: r.left - b, top: r.bottom, right: r.right + b, bottom: r.bottom + b }, WHITE);
            fill(Rect { left: r.left - b, top: r.top, right: r.left, bottom: r.bottom }, WHITE);
            fill(Rect { left: r.right, top: r.top, right: r.right + b, bottom: r.bottom }, WHITE);
        }
        let mut boxes = Vec::new();
        if let Some(r) = hint_rect {
            boxes.push((r, hint.to_owned()));
        }
        if let Some((r, text)) = label {
            boxes.push((r, text));
        }
        for (r, text) in &boxes {
            fill(*r, LABEL_BACKGROUND);
            let wide: Vec<u16> = text.encode_utf16().collect();
            unsafe {
                SetTextColor(s.dc, COLORREF(0x00FF_FFFF));
                let _ = TextOutW(s.dc, r.left + 8, r.top + 4, &wide);
                let _ = GdiFlush();
            }
            // GDI leaves the alpha of the glyphs at zero: the box is opaque.
            if let Some(r) = r.intersect(&whole) {
                for y in r.top..r.bottom {
                    let row = unsafe { std::slice::from_raw_parts_mut(pixels.add(y as usize * stride + r.left as usize), r.width() as usize) };
                    for p in row {
                        *p |= 0xFF_000000;
                    }
                }
            }
        }
        s.painted = touched;
        let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
        let dst = POINT { x: s.origin.0, y: s.origin.1 };
        let size = SIZE { cx: w, cy: h };
        let src = POINT { x: 0, y: 0 };
        let dirty_rect = RECT { left: dirty.left, top: dirty.top, right: dirty.right, bottom: dirty.bottom };
        let info = UPDATELAYEREDWINDOWINFO {
            cbSize: std::mem::size_of::<UPDATELAYEREDWINDOWINFO>() as u32,
            hdcDst: HDC::default(),
            pptDst: &dst,
            psize: &size,
            hdcSrc: s.dc,
            pptSrc: &src,
            crKey: COLORREF(0),
            pblend: &blend,
            dwFlags: ULW_ALPHA,
            prcDirty: if all { std::ptr::null() } else { &dirty_rect },
        };
        unsafe {
            let _ = UpdateLayeredWindowIndirect(hwnd, &info);
        }
    });
}

fn union(a: Option<Rect>, b: Option<Rect>) -> Option<Rect> {
    match (a, b) {
        (Some(a), Some(b)) => {
            Some(Rect { left: a.left.min(b.left), top: a.top.min(b.top), right: a.right.max(b.right), bottom: a.bottom.max(b.bottom) })
        }
        (a, None) => a,
        (None, b) => b,
    }
}

fn text_size(dc: HDC, text: &str) -> (i32, i32) {
    let wide: Vec<u16> = text.encode_utf16().collect();
    let mut size = SIZE::default();
    unsafe {
        let _ = GetTextExtentPoint32W(dc, &wide, &mut size);
    }
    (size.cx, size.cy)
}

/// The frame around the area being recorded, shown until dropped.
pub struct Border {
    thread_id: Arc<AtomicIsize>,
    thread: Option<JoinHandle<()>>,
}

/// Width of the frame, drawn outside the area.
const BORDER: i32 = 3;
/// Colour key of the frame window: the inside shows through.
const KEY: COLORREF = COLORREF(0x00FF_00FF);
/// Red, as `COLORREF` holds it (BGR).
const FRAME_COLOUR: COLORREF = COLORREF(0x0000_00FF);

impl Border {
    pub fn show(region: Region) -> Border {
        let thread_id = Arc::new(AtomicIsize::new(0));
        let id = Arc::clone(&thread_id);
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("border".into())
            .spawn(move || {
                id.store(unsafe { windows::Win32::System::Threading::GetCurrentThreadId() } as isize, Relaxed);
                run_border(region, ready_tx);
            })
            .ok();
        // Not joined when it did not answer in time and has no thread id
        // yet: `Drop` could not tell it to end and would wait for ever.
        let answered = ready_rx.recv_timeout(std::time::Duration::from_secs(2)).is_ok();
        let thread = thread.filter(|_| answered || thread_id.load(Relaxed) != 0);
        Border { thread_id, thread }
    }
}

impl Drop for Border {
    fn drop(&mut self) {
        let id = self.thread_id.load(Relaxed);
        if id != 0 {
            unsafe {
                let _ = PostThreadMessageW(id as u32, WM_QUIT, WPARAM(0), LPARAM(0));
            }
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run_border(region: Region, ready: mpsc::Sender<()>) {
    unsafe {
        let Ok(instance) = windows::Win32::System::LibraryLoader::GetModuleHandleW(None) else { return };
        let class = w!("qrec_border");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(border_proc),
            hInstance: instance.into(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class,
            PCWSTR::null(),
            WS_POPUP,
            region.x - BORDER,
            region.y - BORDER,
            region.width as i32 + 2 * BORDER,
            region.height as i32 + 2 * BORDER,
            None,
            None,
            Some(instance.into()),
            None,
        ) else {
            let _ = ready.send(());
            return;
        };
        let _ = SetLayeredWindowAttributes(hwnd, KEY, 0, LWA_COLORKEY);
        win::exclude_from_capture(hwnd.0 as isize);
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        let _ = ready.send(());
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = DestroyWindow(hwnd);
    }
}

unsafe extern "system" fn border_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            unsafe {
                let dc = BeginPaint(hwnd, &mut ps);
                let mut rect = RECT::default();
                let _ = windows::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut rect);
                let key: HBRUSH = CreateSolidBrush(KEY);
                let frame: HBRUSH = CreateSolidBrush(FRAME_COLOUR);
                FillRect(dc, &rect, frame);
                let inner = RECT { left: rect.left + BORDER, top: rect.top + BORDER, right: rect.right - BORDER, bottom: rect.bottom - BORDER };
                FillRect(dc, &inner, key);
                let _ = DeleteObject(key.into());
                let _ = DeleteObject(frame.into());
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}
