//! `qrec --record SECONDS [--region X,Y,W,H] [--monitor N] [--fps N]
//! [--quality low|medium|high] [--no-audio | --audio-app NAME.exe
//! [--no-boost]] [--out FILE]`: a recording without the window, for checking the
//! pipeline from a console. `qrec --cut FILE --from S --to S [--out FILE]`
//! and `qrec --info FILE`: a cut and a look at a file, the same way.

use std::path::PathBuf;

use crate::audio::Source;
use crate::display;
use crate::recorder::{Config, Quality, Recorder};
use crate::region::Region;
use crate::win;

/// Runs the recording; returns the process exit code.
pub fn record(args: Vec<String>) -> i32 {
    match run(args) {
        Ok(()) => 0,
        Err(e) => {
            say!("qrec: {e}");
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
    let mut audio = Some(Source::System);
    let mut boost = true;
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
            "--no-audio" => audio = None,
            "--audio-app" => audio = Some(Source::App { program: value()?, boost: true }),
            "--no-boost" => boost = false,
            "--no-cursor" => cursor = false,
            "--border" => border = true,
            "--out" => out = Some(PathBuf::from(value()?)),
            s if seconds.is_none() => seconds = Some(s.parse().map_err(|_| format!("unknown argument {s}"))?),
            s => return Err(format!("unknown argument {s}")),
        }
    }
    let seconds = seconds.ok_or("usage: qrec --record SECONDS [--region X,Y,W,H] [--monitor N] [--fps N] [--quality Q] [--no-audio | --audio-app NAME.exe [--no-boost]] [--out FILE]")?;
    if let Some(Source::App { boost: b, .. }) = &mut audio {
        *b = boost;
    }

    let monitors = display::monitors();
    let monitor = monitors.get(monitor_index).ok_or_else(|| format!("no display {}", monitor_index + 1))?.clone();
    let region = region
        .unwrap_or_else(|| Region::from_rect(monitor.rect))
        .fit(monitor.rect)
        .ok_or("the area does not fit the display or is too small")?;
    let path = out.unwrap_or_else(|| PathBuf::from(format!("qrec_{}.mp4", win::local_time_stamp())));
    let sound = match &audio {
        None => "off".to_owned(),
        Some(Source::System) => "of the system".to_owned(),
        Some(Source::App { program, boost: true }) => format!("of {program}, boosted"),
        Some(Source::App { program, boost: false }) => format!("of {program}"),
    };
    let config = Config { monitor, region, fps, quality, audio, cursor, path: path.clone() };
    say!("recording {}x{} at ({}, {}) for {seconds} s, {fps} fps, {} quality, sound {sound}", region.width, region.height, region.x, region.y, quality.name());

    let recorder = Recorder::start(config)?;
    let _border = border.then(|| crate::overlay::Border::show(region));
    say!("encoder: {} ({})", recorder.info.encoder, if recorder.info.hardware { "hardware" } else { "software" });
    if let Some(rate) = recorder.info.audio_rate {
        say!("audio: {rate} Hz");
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
    say!("{frames} frames written, {dropped} skipped, {} bytes: {}", size, path.display());
    Ok(())
}

/// `qrec --cut FILE --from SECONDS --to SECONDS [--out FILE]`: the stretch
/// of a recording into a new file, without re-encoding; the start lands
/// on the key frame at or before it.
pub fn cut(args: Vec<String>) -> i32 {
    let mut src: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;
    let (mut from, mut to) = (0.0f64, f64::INFINITY);
    let mut args = args.into_iter();
    let result = (|| -> Result<(), String> {
        while let Some(arg) = args.next() {
            let mut value = || args.next().ok_or_else(|| format!("{arg} needs a value"));
            match arg.as_str() {
                "--from" => from = value()?.parse().map_err(|e: std::num::ParseFloatError| e.to_string())?,
                "--to" => to = value()?.parse().map_err(|e: std::num::ParseFloatError| e.to_string())?,
                "--out" => out = Some(PathBuf::from(value()?)),
                s if src.is_none() => src = Some(PathBuf::from(s)),
                s => return Err(format!("unknown argument {s}")),
            }
        }
        let src = src.ok_or("usage: qrec --cut FILE --from SECONDS --to SECONDS [--out FILE]")?;
        let seconds = |s: f64| if s.is_finite() { (s * crate::trim::SECOND as f64) as i64 } else { i64::MAX };
        let dst = out.unwrap_or_else(|| src.with_extension("cut.mp4"));
        let progress = crate::trim::Progress::default();
        let started = std::time::Instant::now();
        let cut = crate::trim::cut(&src, &dst, seconds(from), seconds(to), &progress).map_err(|e| win::describe(&e))?;
        let size = std::fs::metadata(&dst).map(|m| m.len()).unwrap_or(0);
        say!(
            "{} frames from {} in {:.2} s, {size} bytes: {}",
            cut.frames,
            crate::editor::clock(cut.start),
            started.elapsed().as_secs_f64(),
            dst.display()
        );
        Ok(())
    })();
    match result {
        Ok(()) => 0,
        Err(e) => {
            say!("qrec: {e}");
            1
        }
    }
}

/// `qrec --info FILE`: what a recording holds, and where its key frames are.
pub fn info(path: Option<PathBuf>) -> i32 {
    let Some(path) = path else {
        say!("usage: qrec --info FILE");
        return 2;
    };
    let result = (|| -> Result<(), String> {
        let info = crate::trim::info(&path).map_err(|e| win::describe(&e))?;
        say!(
            "{}x{} at {} fps, {} s, {}",
            info.width,
            info.height,
            info.fps,
            info.duration as f64 / crate::trim::SECOND as f64,
            if info.audio { "with audio" } else { "no audio" }
        );
        let started = std::time::Instant::now();
        let frames = crate::trim::frames(&path, &std::sync::atomic::AtomicBool::new(false)).map_err(|e| win::describe(&e))?;
        let keys: Vec<String> = frames.iter().filter(|f| f.key).map(|f| crate::editor::clock(f.time)).collect();
        say!("{} frames listed in {:.2} s, {} key frames: {}", frames.len(), started.elapsed().as_secs_f64(), keys.len(), keys.join(" "));
        Ok(())
    })();
    match result {
        Ok(()) => 0,
        Err(e) => {
            say!("qrec: {e}");
            1
        }
    }
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
    let rx = crate::overlay::select(monitors, crate::region::Aspect::Free, || {});
    let hwnd = (0..50).find_map(|_| {
        std::thread::sleep(std::time::Duration::from_millis(50));
        unsafe { FindWindowW(w!("qrec_select"), None) }.ok()
    });
    let Some(hwnd) = hwnd else {
        say!("the selection window did not appear");
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
            say!("selected {}x{} at ({}, {})", r.width, r.height, r.x, r.y);
            0
        }
        Ok(None) => {
            say!("selection cancelled");
            1
        }
        Err(_) => {
            say!("no result from the selection");
            1
        }
    }
}

/// `qrec --export-icon <file.ico|file.png>`: the application icon for the
/// installer, or its 256 px layer as a PNG for the documentation.
pub fn export_icon(path: Option<PathBuf>) -> i32 {
    let Some(path) = path else {
        say!("usage: qrec --export-icon <file.ico|file.png>");
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
            say!("cannot write {}: {e}", path.display());
            1
        }
    }
}
