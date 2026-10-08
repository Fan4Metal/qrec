//! `qrec --record SECONDS [--region X,Y,W,H] [--monitor N] [--fps N]
//! [--quality low|medium|high] [--no-audio] [--out FILE]`: a recording
//! without the window, for checking the pipeline from a console.

use std::path::PathBuf;

use crate::display;
use crate::recorder::{Config, Quality, Recorder};
use crate::region::Region;
use crate::win;

/// Runs the recording; returns the process exit code.
pub fn record(args: Vec<String>) -> i32 {
    match run(args) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("qrec: {e}");
            1
        }
    }
}

fn run(args: Vec<String>) -> Result<(), String> {
    let mut seconds: Option<f64> = None;
    let mut region: Option<Region> = None;
    let mut monitor_index = 0usize;
    let mut fps = 30u32;
    let mut quality = Quality::Medium;
    let mut audio = true;
    let mut cursor = true;
    let mut out: Option<PathBuf> = None;
    let mut border = false;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--region" => region = Some(Region::from_setting(&value()?).ok_or("--region wants X,Y,W,H")?),
            "--monitor" => monitor_index = value()?.parse::<usize>().map_err(|e| e.to_string())?.saturating_sub(1),
            "--fps" => fps = value()?.parse().map_err(|e: std::num::ParseIntError| e.to_string())?,
            "--quality" => quality = Quality::from_name(&value()?).ok_or("--quality wants low, medium or high")?,
            "--no-audio" => audio = false,
            "--no-cursor" => cursor = false,
            "--border" => border = true,
            "--out" => out = Some(PathBuf::from(value()?)),
            s if seconds.is_none() => seconds = Some(s.parse().map_err(|_| format!("unknown argument {s}"))?),
            s => return Err(format!("unknown argument {s}")),
        }
    }
    let seconds = seconds.ok_or("usage: qrec --record SECONDS [--region X,Y,W,H] [--monitor N] [--fps N] [--quality Q] [--no-audio] [--out FILE]")?;

    let monitors = display::monitors();
    let monitor = monitors.get(monitor_index).ok_or_else(|| format!("no display {}", monitor_index + 1))?.clone();
    let region = region
        .unwrap_or_else(|| Region::from_rect(monitor.rect))
        .fit(monitor.rect)
        .ok_or("the area does not fit the display or is too small")?;
    let path = out.unwrap_or_else(|| PathBuf::from(format!("qrec_{}.mp4", win::local_time_stamp())));
    let config = Config { monitor, region, fps, quality, audio, cursor, path: path.clone() };
    eprintln!("recording {}x{} at ({}, {}) for {seconds} s, {fps} fps, {} quality, audio {}", region.width, region.height, region.x, region.y, quality.name(), if audio { "on" } else { "off" });

    let recorder = Recorder::start(config)?;
    let _border = border.then(|| crate::overlay::Border::show(region));
    eprintln!("encoder: {} ({})", recorder.info.encoder, if recorder.info.hardware { "hardware" } else { "software" });
    if let Some(rate) = recorder.info.audio_rate {
        eprintln!("audio: {rate} Hz");
    }
    let started = std::time::Instant::now();
    while started.elapsed().as_secs_f64() < seconds {
        if recorder.failed() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let (frames, dropped) = (recorder.frames(), recorder.dropped());
    recorder.stop()?;
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    eprintln!("{frames} frames written, {dropped} skipped, {} bytes: {}", size, path.display());
    Ok(())
}

/// `qrec --test-select`: opens the selection overlay and drives it with
/// posted mouse messages (a drag from (100, 100) to (739, 459) on the
/// virtual screen), then prints what it reported. The screen dims for a
/// moment; the real mouse is not touched.
pub fn test_select() -> i32 {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, GetSystemMetrics, PostMessageW, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE};
    use windows::core::w;
    let monitors = display::monitors();
    let rx = crate::overlay::select(monitors, || {});
    let hwnd = (0..50).find_map(|_| {
        std::thread::sleep(std::time::Duration::from_millis(50));
        unsafe { FindWindowW(w!("qrec_select"), None) }.ok()
    });
    let Some(hwnd) = hwnd else {
        eprintln!("the selection window did not appear");
        return 1;
    };
    let origin = unsafe { (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN)) };
    let lparam = |x: i32, y: i32| LPARAM((((y - origin.1) as u16 as isize) << 16) | ((x - origin.0) as u16 as isize));
    let post = |msg: u32, x: i32, y: i32| unsafe {
        let _ = PostMessageW(Some(hwnd), msg, WPARAM(1), lparam(x, y));
    };
    std::thread::sleep(std::time::Duration::from_millis(300));
    post(WM_LBUTTONDOWN, 100, 100);
    for i in 1..=20 {
        std::thread::sleep(std::time::Duration::from_millis(30));
        post(WM_MOUSEMOVE, 100 + i * 32, 100 + i * 18);
    }
    std::thread::sleep(std::time::Duration::from_millis(500));
    post(WM_LBUTTONUP, 739, 459);
    match rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(Some(r)) => {
            eprintln!("selected {}x{} at ({}, {})", r.width, r.height, r.x, r.y);
            0
        }
        Ok(None) => {
            eprintln!("selection cancelled");
            1
        }
        Err(_) => {
            eprintln!("no result from the selection");
            1
        }
    }
}

/// `qrec --export-icon <file.ico|file.png>`: the application icon for the
/// installer, or its 256 px layer as a PNG for the documentation.
pub fn export_icon(path: Option<PathBuf>) -> i32 {
    let Some(path) = path else {
        eprintln!("usage: qrec --export-icon <file.ico|file.png>");
        return 2;
    };
    let bytes = if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("png")) {
        crate::icon::png(256, &crate::icon::rgba(256))
    } else {
        crate::icon::ico(&crate::icon::ICO_SIZES)
    };
    match std::fs::write(&path, bytes) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("cannot write {}: {e}", path.display());
            1
        }
    }
}
