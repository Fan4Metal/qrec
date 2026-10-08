//! A recording: the threads that capture, convert and encode, from
//! `start` to `stop`.
//!
//! The video thread owns the duplication, the converter and the encoder.
//! It encodes at a fixed frame rate on the performance counter: a tick
//! every `1/fps`, with whatever the desktop showed last. Frames come when
//! the desktop changes, so a still screen repeats the last image; when the
//! encoder falls behind, ticks are skipped so the timestamps stay on the
//! clock. The audio thread reads the loopback and writes its packets with
//! their own timestamps on the same clock, filling gaps with silence.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::audio::{Loopback, Source};
use crate::capture::{Capturer, PollError};
use crate::convert::Converter;
use crate::cursor::CursorDrawer;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use crate::display::Monitor;
use crate::encoder::{AudioConfig, Encoder, VideoConfig};
use crate::region::Region;
use crate::win;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Quality {
    Low,
    #[default]
    Medium,
    High,
}

impl Quality {
    pub const ALL: [Quality; 3] = [Quality::Low, Quality::Medium, Quality::High];

    /// Bits per pixel per frame: the bitrate follows the area and the
    /// frame rate.
    fn bits_per_pixel(self) -> f64 {
        match self {
            Quality::Low => 0.05,
            Quality::Medium => 0.1,
            Quality::High => 0.2,
        }
    }

    /// Name in the settings.
    pub fn name(self) -> &'static str {
        match self {
            Quality::Low => "low",
            Quality::Medium => "medium",
            Quality::High => "high",
        }
    }

    pub fn from_name(name: &str) -> Option<Quality> {
        Self::ALL.into_iter().find(|q| q.name() == name)
    }

    pub fn label(self) -> &'static str {
        match self {
            Quality::Low => tr!("Low", "Низкое"),
            Quality::Medium => tr!("Medium", "Среднее"),
            Quality::High => tr!("High", "Высокое"),
        }
    }
}

/// The bitrate for an area, a frame rate and a quality, bits per second.
pub fn bitrate(width: u32, height: u32, fps: u32, quality: Quality) -> u32 {
    let pixels = f64::from(width) * f64::from(height) * f64::from(fps);
    (pixels * quality.bits_per_pixel()).clamp(500_000.0, 60_000_000.0) as u32
}

#[derive(Clone, Debug)]
pub struct Config {
    pub monitor: Monitor,
    /// On the virtual screen; must lie within the monitor.
    pub region: Region,
    pub fps: u32,
    pub quality: Quality,
    /// Whose sound is recorded; `None` for none.
    pub audio: Option<Source>,
    /// Draw the pointer into the frames.
    pub cursor: bool,
    pub path: PathBuf,
}

/// What the recording turned out to use.
#[derive(Clone, Debug, Default)]
pub struct Info {
    pub encoder: String,
    pub hardware: bool,
    /// Sample rate of the audio track, when there is one.
    pub audio_rate: Option<u32>,
}

/// The counters shared with the window.
#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    frames: AtomicU64,
    /// Ticks skipped because the encoder was behind.
    dropped: AtomicU64,
    error: Mutex<Option<String>>,
}

pub struct Recorder {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    pub info: Info,
    pub path: PathBuf,
    pub started: Instant,
}

impl Recorder {
    /// Starts recording `config`; returns once the first frame can be
    /// written, or with what went wrong setting up.
    pub fn start(config: Config) -> Result<Recorder, String> {
        let shared = Arc::new(Shared::default());
        let (ready_tx, ready_rx) = mpsc::channel();
        let path = config.path.clone();
        let worker = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("video".into())
            .spawn(move || {
                let _com = win::com_init_mta();
                if let Err(e) = run_video(config, &worker, ready_tx) {
                    log::error!("recording stopped: {e}");
                    worker.error.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert(e);
                }
            })
            .map_err(|e| e.to_string())?;
        match ready_rx.recv_timeout(Duration::from_secs(20)) {
            Ok(Ok(info)) => Ok(Recorder { shared, thread: Some(thread), info, path, started: Instant::now() }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                shared.stop.store(true, Relaxed);
                Err(tr!("the recording did not start in time", "запись не началась вовремя").into())
            }
        }
    }

    pub fn frames(&self) -> u64 {
        self.shared.frames.load(Relaxed)
    }

    pub fn dropped(&self) -> u64 {
        self.shared.dropped.load(Relaxed)
    }

    /// Why the recording failed, if it did; the threads have stopped then.
    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn failed(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| t.is_finished())
    }

    /// Stops and completes the file; blocks until the index is written.
    pub fn stop(mut self) -> Result<Info, String> {
        self.shared.stop.store(true, Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        match self.error() {
            Some(e) => Err(e),
            None => Ok(self.info.clone()),
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.shared.stop.store(true, Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run_video(config: Config, shared: &Arc<Shared>, ready: mpsc::Sender<Result<Info, String>>) -> Result<(), String> {
    let setup = (|| -> Result<_, String> {
        let origin = (config.monitor.rect.left, config.monitor.rect.top);
        let local = config.region.relative_to(origin);
        let mut capturer = Capturer::new(&config.monitor, local)
            .map_err(|e| format!("{}: {}", tr!("screen capture", "захват экрана"), win::describe(&e)))?;
        let audio_error = |e: windows::core::Error| format!("{}: {}", tr!("audio capture", "захват звука"), win::describe(&e));
        let loopback = match &config.audio {
            None => None,
            Some(Source::System) => Some(Loopback::open().map_err(audio_error)?),
            Some(Source::App { program, boost }) => {
                let name = crate::sessions::stem(program);
                let root = crate::sessions::find(program)
                    .ok_or_else(|| tr!(format!("{name} is not running"), format!("Программа {name} не запущена")))?;
                log::debug!("audio: {program}, process {root}");
                Some(Loopback::open_app(root, *boost).map_err(audio_error)?)
            }
        };
        let (width, height) = (config.region.width, config.region.height);
        let video = VideoConfig { width, height, fps: config.fps, bitrate: bitrate(width, height, config.fps, config.quality) };
        let audio = loopback.as_ref().map(|l| AudioConfig { sample_rate: l.rate() });
        let encoder = Encoder::new(&config.path, capturer.device(), video, audio)
            .map_err(|e| format!("{}: {}", tr!("encoder", "кодер"), win::describe(&e)))?;
        // With the pointer, the frame is composed into a second texture, so
        // the captured one stays clean for the next tick.
        let mut drawer = None;
        if config.cursor {
            let composed = crate::capture::create_texture(capturer.device(), width, height, DXGI_FORMAT_B8G8R8A8_UNORM)
                .map_err(|e| format!("{}: {}", tr!("screen capture", "захват экрана"), win::describe(&e)))?;
            match CursorDrawer::new(capturer.device(), capturer.context(), &composed, width, height) {
                Ok(d) => drawer = Some((composed, d)),
                Err(e) => log::warn!("no pointer in the recording: {}", win::describe(&e)),
            }
        }
        let source = drawer.as_ref().map_or(capturer.frame(), |(composed, _)| composed).clone();
        let converter = Converter::new(capturer.device(), capturer.context(), &source, width, height, config.fps)
            .map_err(|e| format!("{}: {}", tr!("colour conversion", "преобразование цвета"), win::describe(&e)))?;
        // The first frame, so that the recording does not start black.
        for _ in 0..20 {
            if capturer.has_frame() {
                break;
            }
            match capturer.poll(100) {
                Ok(_) => {}
                Err(PollError::AccessLost) => {
                    let _ = capturer.recreate();
                }
                Err(PollError::Other(e)) => return Err(win::describe(&e)),
            }
        }
        if let Some(l) = &loopback {
            log::debug!("audio: {} at {} Hz", l.method, l.rate());
        }
        let info = Info { encoder: encoder.encoder_name.clone(), hardware: encoder.hardware, audio_rate: audio.map(|a| a.sample_rate) };
        Ok((capturer, converter, Arc::new(encoder), loopback, info, drawer, local))
    })();
    let (mut capturer, mut converter, encoder, loopback, info, mut drawer, local) = match setup {
        Ok(s) => s,
        Err(e) => {
            let _ = ready.send(Err(e.clone()));
            return Err(e);
        }
    };
    log::debug!("recording {}x{} at {} fps to {}", config.region.width, config.region.height, config.fps, config.path.display());

    let start = win::qpc_100ns();
    let audio_thread = loopback.and_then(|loopback| {
        let encoder = Arc::clone(&encoder);
        let shared = Arc::clone(shared);
        std::thread::Builder::new()
            .name("audio".into())
            .spawn(move || {
                let _com = win::com_init_mta();
                if let Err(e) = run_audio(loopback, &encoder, start, &shared) {
                    log::error!("audio stopped: {e}");
                    shared.error.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert(e);
                }
            })
            .ok()
    });
    let _ = ready.send(Ok(info));

    let interval = 10_000_000 / i64::from(config.fps);
    let mut tick: i64 = 0;
    let mut lost_since: Option<Instant> = None;
    let mut result = Ok(());
    'record: while !shared.stop.load(Relaxed) {
        let target = start + tick * interval;
        loop {
            let remaining = target - win::qpc_100ns();
            if remaining <= 0 {
                break;
            }
            let timeout = ((remaining + 9_999) / 10_000).clamp(1, 50) as u32;
            match capturer.poll(timeout) {
                Ok(_) => {}
                Err(PollError::AccessLost) => {
                    // A mode change or the secure desktop: duplicate again,
                    // the last frame stands in meanwhile.
                    let since = *lost_since.get_or_insert_with(Instant::now);
                    if capturer.recreate().is_ok() {
                        lost_since = None;
                    } else if since.elapsed() > Duration::from_secs(15) {
                        result = Err(tr!("the display was lost", "экран стал недоступен").into());
                        break 'record;
                    } else {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
                Err(PollError::Other(e)) => {
                    result = Err(format!("{}: {}", tr!("screen capture", "захват экрана"), win::describe(&e)));
                    break 'record;
                }
            }
        }
        if !capturer.has_frame() {
            // Nothing captured yet (the duplication was just recreated).
            tick = (win::qpc_100ns() - start) / interval + 1;
            continue;
        }
        if let Some((composed, drawer)) = &mut drawer {
            unsafe { capturer.context().CopyResource(&*composed, capturer.frame()) };
            if let Err(e) = drawer.draw(&capturer.cursor, (local.x, local.y)) {
                log::warn!("pointer not drawn: {}", win::describe(&e));
            }
        }
        let texture = match converter.convert() {
            Ok(t) => t,
            Err(e) => {
                result = Err(format!("{}: {}", tr!("colour conversion", "преобразование цвета"), win::describe(&e)));
                break;
            }
        };
        match encoder.write_video(&texture, tick * interval, interval) {
            Ok(true) => {
                shared.frames.fetch_add(1, Relaxed);
            }
            Ok(false) => {
                shared.dropped.fetch_add(1, Relaxed);
            }
            Err(e) => {
                result = Err(format!("{}: {}", tr!("encoder", "кодер"), win::describe(&e)));
                break;
            }
        }
        tick += 1;
        if tick % 300 == 0 {
            log::debug!("tick {tick}: {} output textures", converter.pool_size());
        }
        // Behind by whole frames: skip them, the timestamps stay on the clock.
        let behind = (win::qpc_100ns() - (start + tick * interval)) / interval;
        if behind > 0 {
            shared.dropped.fetch_add(behind as u64, Relaxed);
            tick += behind;
        }
    }
    shared.stop.store(true, Relaxed);
    if let Some(thread) = audio_thread {
        let _ = thread.join();
    }
    log::debug!("{} frames, {} skipped, {} output textures", shared.frames.load(Relaxed), shared.dropped.load(Relaxed), converter.pool_size());
    drop(converter);
    drop(capturer);
    if let Err(e) = encoder.finish() {
        let e = format!("{}: {}", tr!("finishing the file", "завершение файла"), win::describe(&e));
        return result.and(Err(e));
    }
    result
}

/// Gaps and overlaps smaller than this are jitter of the device clock and
/// are ignored; larger ones are filled with silence or trimmed.
const AUDIO_TOLERANCE: i64 = 300_000;

fn run_audio(mut loopback: Loopback, encoder: &Encoder, start: i64, shared: &Shared) -> Result<(), String> {
    let describe = |e: windows::core::Error| format!("{}: {}", tr!("audio capture", "захват звука"), win::describe(&e));
    let rate = i64::from(loopback.rate());
    let to_time = |frames: i64| frames * 10_000_000 / rate;
    let to_frames = |time: i64| time * rate / 10_000_000;
    loopback.start().map_err(describe)?;
    // Where the next sample goes on the timeline; `buffer` holds samples
    // from `buffer_start` not written yet.
    let mut next: i64 = 0;
    let mut buffer: Vec<i16> = Vec::new();
    let mut buffer_start: i64 = 0;
    let mut packets: Vec<(Vec<i16>, i64)> = Vec::new();
    let mut result = Ok(());
    while !shared.stop.load(Relaxed) {
        std::thread::sleep(Duration::from_millis(10));
        packets.clear();
        if let Err(e) = loopback.drain(|p| packets.push((p.samples.to_vec(), p.qpc))) {
            result = Err(describe(e));
            break;
        }
        for (samples, qpc) in packets.drain(..) {
            let mut time = qpc - start;
            let mut samples = &samples[..];
            if time < 0 {
                // Recorded before the video started: drop that part.
                let skip = to_frames(-time) as usize * 2;
                if skip >= samples.len() {
                    continue;
                }
                samples = &samples[skip..];
                time = 0;
            }
            let gap = time - next;
            if gap > AUDIO_TOLERANCE {
                flush(encoder, &mut buffer, buffer_start, to_time)?;
                write_silence(encoder, next, gap, to_frames, to_time)?;
                next = time;
            } else if gap < -AUDIO_TOLERANCE {
                let skip = to_frames(-gap) as usize * 2;
                if skip >= samples.len() {
                    continue;
                }
                samples = &samples[skip..];
            }
            if buffer.is_empty() {
                buffer_start = next;
            }
            buffer.extend_from_slice(samples);
            next += to_time(samples.len() as i64 / 2);
            if buffer.len() >= rate as usize / 5 * 2 {
                flush(encoder, &mut buffer, buffer_start, to_time)?;
            }
        }
        flush(encoder, &mut buffer, buffer_start, to_time)?;
        // Nothing played for a while and no keep-alive: keep the track going.
        let now = win::qpc_100ns() - start;
        if now / 50_000_000 != (now - 100_000) / 50_000_000 {
            log::debug!("audio at {:.2} s lags the clock by {} ms", next as f64 / 1e7, (now - next) / 10_000);
        }
        if now - next > 5_000_000 {
            let gap = now - next - 1_000_000;
            write_silence(encoder, next, gap, to_frames, to_time)?;
            next += to_time(to_frames(gap));
        }
    }
    loopback.stop();
    result
}

fn flush(encoder: &Encoder, buffer: &mut Vec<i16>, start: i64, to_time: impl Fn(i64) -> i64) -> Result<(), String> {
    if buffer.is_empty() {
        return Ok(());
    }
    let duration = to_time(buffer.len() as i64 / 2);
    let r = encoder.write_audio(buffer, start, duration);
    buffer.clear();
    r.map_err(|e| format!("{}: {}", tr!("encoder", "кодер"), win::describe(&e)))
}

fn write_silence(
    encoder: &Encoder,
    mut at: i64,
    gap: i64,
    to_frames: impl Fn(i64) -> i64,
    to_time: impl Fn(i64) -> i64,
) -> Result<(), String> {
    let mut frames = to_frames(gap);
    while frames > 0 {
        let chunk = frames.min(to_frames(10_000_000));
        let silence = vec![0i16; chunk as usize * 2];
        let duration = to_time(chunk);
        encoder.write_audio(&silence, at, duration).map_err(|e| format!("{}: {}", tr!("encoder", "кодер"), win::describe(&e)))?;
        at += duration;
        frames -= chunk;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitrate_follows_area_and_rate() {
        assert_eq!(bitrate(1920, 1080, 30, Quality::Medium), 6_220_800);
        assert_eq!(bitrate(1920, 1080, 60, Quality::High), 24_883_200);
        assert_eq!(bitrate(64, 64, 30, Quality::Low), 500_000);
        assert_eq!(bitrate(7680, 4320, 60, Quality::High), 60_000_000);
    }

    #[test]
    fn quality_names_round_trip() {
        for q in Quality::ALL {
            assert_eq!(Quality::from_name(q.name()), Some(q));
        }
        assert_eq!(Quality::from_name("ultra"), None);
    }
}
