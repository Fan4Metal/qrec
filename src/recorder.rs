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
use windows::Win32::Foundation::E_ACCESSDENIED;
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
    /// Why the sound stopped, when it did; the picture goes on.
    audio_error: Mutex<Option<String>>,
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
                // Told to stop; it ends on a thread of its own, which also
                // removes what it wrote, so that no file of a recording
                // that never started is left.
                shared.stop.store(true, Relaxed);
                let _ = std::thread::Builder::new().name("abandoned".into()).spawn(move || {
                    let _ = thread.join();
                    let _ = std::fs::remove_file(&path);
                });
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

    /// Why the sound stopped, when it did: the recording goes on without
    /// it, and the file is complete all the same.
    pub fn audio_error(&self) -> Option<String> {
        self.shared.audio_error.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Stops and completes the file; blocks until the index is written.
    pub fn stop(mut self) -> Result<Info, String> {
        let asked = Instant::now();
        self.shared.stop.store(true, Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        log::debug!("stopped in {:.0} ms", asked.elapsed().as_secs_f64() * 1e3);
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
        // The file is opened with the first encoded frame, after the start
        // is reported: a folder that cannot be written is found now.
        std::fs::File::create(&config.path)
            .and_then(|_| std::fs::remove_file(&config.path))
            .map_err(|e| format!("{}: {e}", tr!("Cannot write the file", "Не удаётся записать файл")))?;
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
                    shared.audio_error.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert(e);
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
            // Behind the clock (the encoder slower than the frame rate):
            // still one poll without waiting, or the capture would never
            // be read again and the file would repeat one frame.
            let timeout = if remaining <= 0 { 0 } else { ((remaining + 9_999) / 10_000).clamp(1, 50) as u32 };
            match capturer.poll(timeout) {
                Ok(_) => {}
                Err(PollError::AccessLost) => {
                    // A mode change or the secure desktop: duplicate again,
                    // the last frame stands in meanwhile. The secure desktop
                    // (Win+L, a UAC prompt) refuses the duplication for as
                    // long as it is up, which is no reason to stop.
                    let since = *lost_since.get_or_insert_with(Instant::now);
                    match capturer.recreate() {
                        Ok(()) => lost_since = None,
                        Err(e) if e.code() == E_ACCESSDENIED || since.elapsed() <= Duration::from_secs(15) => {
                            std::thread::sleep(Duration::from_millis(50));
                        }
                        Err(e) => {
                            result = Err(format!("{}: {}", tr!("the display was lost", "экран стал недоступен"), win::describe(&e)));
                            break 'record;
                        }
                    }
                }
                Err(PollError::Other(e)) => {
                    result = Err(format!("{}: {}", tr!("screen capture", "захват экрана"), win::describe(&e)));
                    break 'record;
                }
            }
            if remaining <= 0 {
                break;
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

/// Gaps and overlaps larger than this are filled with silence or trimmed
/// at once (the sound stopped, the device was busy).
const AUDIO_TOLERANCE: i64 = 300_000;
/// Within the tolerance, a track more than this off the packets' times
/// (the device's clock drifting against the performance counter) is
/// brought back a frame per packet: a frame added or left out is not
/// heard, a jump of 30 ms is.
const AUDIO_DRIFT: i64 = 100_000;

fn run_audio(mut loopback: Loopback, encoder: &Encoder, start: i64, shared: &Shared) -> Result<(), String> {
    let describe = |e: windows::core::Error| format!("{}: {}", tr!("audio capture", "захват звука"), win::describe(&e));
    let mut write = |samples: &[i16], time: i64, duration: i64| {
        encoder.write_audio(samples, time, duration).map_err(|e| format!("{}: {}", tr!("encoder", "кодер"), win::describe(&e)))
    };
    let mut track = Track::new(i64::from(loopback.rate()));
    loopback.start().map_err(describe)?;
    let mut packets: Vec<(Vec<i16>, Option<i64>)> = Vec::new();
    let mut result = Ok(());
    let mut logged = 0;
    loop {
        loopback.wait(Duration::from_millis(10));
        packets.clear();
        if let Err(e) = loopback.drain(|p| packets.push((p.samples.to_vec(), p.qpc))) {
            result = Err(describe(e));
            break;
        }
        for (samples, qpc) in packets.drain(..) {
            track.add(&samples, qpc.map(|qpc| qpc - start), &mut write)?;
        }
        track.flush(&mut write)?;
        let now = win::qpc_100ns() - start;
        if now / 50_000_000 > logged {
            logged = now / 50_000_000;
            log::debug!(
                "audio at {:.2} s lags the clock by {} ms, {} frames added or left out",
                track.end() as f64 / 1e7,
                (now - track.end()) / 10_000,
                track.corrections
            );
        }
        // Nothing played for a while and no keep-alive: keep the track going.
        track.keep_up(now, &mut write)?;
        // Once more after the stop, for the packets of the last moment.
        if shared.stop.load(Relaxed) {
            break;
        }
    }
    loopback.stop();
    result
}

/// The sound track as it is laid down: where the next sample goes,
/// counted in frames so that rounding does not add up over a long
/// recording, and the samples not written yet.
struct Track {
    rate: i64,
    /// The frame the next sample takes; `None` before the first packet.
    position: Option<i64>,
    buffer: Vec<i16>,
    /// The frame of the first sample in `buffer`.
    buffer_start: i64,
    /// Frames added or left out against drift.
    corrections: u64,
}

impl Track {
    fn new(rate: i64) -> Track {
        Track { rate, position: None, buffer: Vec::new(), buffer_start: 0, corrections: 0 }
    }

    fn time(&self, frames: i64) -> i64 {
        frames * 10_000_000 / self.rate
    }

    fn frames(&self, time: i64) -> i64 {
        time * self.rate / 10_000_000
    }

    /// Where the track has got to, in 100 ns units.
    fn end(&self) -> i64 {
        self.time(self.position.unwrap_or(0))
    }

    /// Lays down a packet recorded at `time` (on the recording's clock;
    /// `None` when the device gave no time: it continues the track).
    fn add(&mut self, samples: &[i16], time: Option<i64>, write: &mut impl FnMut(&[i16], i64, i64) -> Result<(), String>) -> Result<(), String> {
        let mut samples = samples;
        let mut at = time.map_or(self.position.unwrap_or(0), |t| self.frames(t));
        if at < 0 {
            // Recorded before the video started: that part is dropped.
            let skip = (-at) as usize * 2;
            if skip >= samples.len() {
                return Ok(());
            }
            samples = &samples[skip..];
            at = 0;
        }
        // The track starts where the first packet is, not at 0.
        let position = *self.position.get_or_insert(at);
        let gap = at - position;
        let mut extra = None;
        if gap > self.frames(AUDIO_TOLERANCE) {
            self.flush(write)?;
            self.silence(gap, write)?;
        } else if gap < -self.frames(AUDIO_TOLERANCE) {
            let skip = (-gap) as usize * 2;
            if skip >= samples.len() {
                return Ok(());
            }
            samples = &samples[skip..];
        } else if gap > self.frames(AUDIO_DRIFT) && samples.len() >= 2 {
            // Behind the packets: the first frame twice.
            extra = Some([samples[0], samples[1]]);
            self.corrections += 1;
        } else if gap < -self.frames(AUDIO_DRIFT) && samples.len() >= 4 {
            // Ahead of them: the first frame left out.
            samples = &samples[2..];
            self.corrections += 1;
        }
        let position = self.position.unwrap_or(0);
        if self.buffer.is_empty() {
            self.buffer_start = position;
        }
        if let Some(frame) = extra {
            self.buffer.extend_from_slice(&frame);
        }
        self.buffer.extend_from_slice(samples);
        self.position = Some(self.buffer_start + self.buffer.len() as i64 / 2);
        if self.buffer.len() as i64 >= self.rate / 5 * 2 {
            self.flush(write)?;
        }
        Ok(())
    }

    /// Writes the samples waiting.
    fn flush(&mut self, write: &mut impl FnMut(&[i16], i64, i64) -> Result<(), String>) -> Result<(), String> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let end = self.buffer_start + self.buffer.len() as i64 / 2;
        let (time, duration) = (self.time(self.buffer_start), self.time(end) - self.time(self.buffer_start));
        let result = write(&self.buffer, time, duration);
        self.buffer.clear();
        self.buffer_start = end;
        result
    }

    /// `frames` of silence from the position on; the buffer is empty.
    fn silence(&mut self, frames: i64, write: &mut impl FnMut(&[i16], i64, i64) -> Result<(), String>) -> Result<(), String> {
        let mut at = self.position.unwrap_or(0);
        let end = at + frames;
        while at < end {
            let chunk = (end - at).min(self.rate);
            let silence = vec![0i16; chunk as usize * 2];
            write(&silence, self.time(at), self.time(at + chunk) - self.time(at))?;
            at += chunk;
        }
        self.position = Some(end);
        self.buffer_start = end;
        Ok(())
    }

    /// Silence up to a second before `now` when the track has fallen more
    /// than five behind: a stream that delivers nothing while nothing
    /// plays.
    fn keep_up(&mut self, now: i64, write: &mut impl FnMut(&[i16], i64, i64) -> Result<(), String>) -> Result<(), String> {
        let lag = self.frames(now) - self.position.unwrap_or(0);
        if lag > self.frames(5 * 10_000_000) {
            self.flush(write)?;
            self.position.get_or_insert(0);
            self.silence(lag - self.frames(10_000_000), write)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a track writes, as (first frame, frames, the samples' first value).
    fn lay(track: &mut Track, packets: &[(usize, Option<i64>, i16)]) -> Vec<(i64, i64, i16)> {
        let mut out = Vec::new();
        let rate = track.rate;
        let mut write = |s: &[i16], time: i64, duration: i64| {
            out.push((time * rate / 10_000_000, (duration * rate + 5_000_000) / 10_000_000, s[0]));
            Ok(())
        };
        for &(frames, time, value) in packets {
            track.add(&vec![value; frames * 2], time, &mut write).unwrap();
        }
        track.flush(&mut write).unwrap();
        out
    }

    #[test]
    fn track_starts_at_the_first_packet() {
        let mut track = Track::new(48_000);
        // 10 ms in: the track starts there, the next packet follows on.
        let out = lay(&mut track, &[(480, Some(100_000), 1), (480, Some(200_000), 2)]);
        assert_eq!(out, vec![(480, 960, 1)]);
    }

    #[test]
    fn track_fills_gaps_and_trims_overlaps() {
        let mut track = Track::new(48_000);
        // A second without packets: silence, then the packet in its place.
        let out = lay(&mut track, &[(480, Some(0), 1), (480, Some(10_100_000), 2)]);
        assert_eq!(out, vec![(0, 480, 1), (480, 48_000, 0), (48_480, 480, 2)]);
        // A packet 50 ms early: its first 50 ms are dropped.
        let mut track = Track::new(48_000);
        let out = lay(&mut track, &[(4800, Some(0), 1), (4800, Some(500_000), 2)]);
        assert_eq!(out, vec![(0, 4800 + 2400, 1)]);
    }

    #[test]
    fn track_follows_drift_a_frame_at_a_time() {
        // A device clock 0.1 % slow against the counter: packets of 10 ms
        // 10 µs late each. Without correction 30 s of them would be 30 ms
        // behind and get a jump of silence; the track adds a frame now and
        // then and stays within 10 ms.
        let mut track = Track::new(48_000);
        let packets: Vec<(usize, Option<i64>, i16)> = (0..3000).map(|i| (480, Some(i * 100_100), 1)).collect();
        let out = lay(&mut track, &packets);
        let behind = track.frames(2999 * 100_100) - (track.position.unwrap() - 480);
        assert!(track.corrections > 0);
        assert!(behind.abs() <= track.frames(AUDIO_DRIFT) + 1, "{behind} frames behind");
        assert!(out.iter().all(|&(_, _, v)| v == 1), "no silence written");
        // Packets without a time continue the track.
        let mut track = Track::new(48_000);
        let out = lay(&mut track, &[(480, Some(0), 1), (480, None, 2)]);
        assert_eq!(out, vec![(0, 960, 1)]);
    }

    #[test]
    fn track_keeps_up_with_the_clock() {
        let mut track = Track::new(48_000);
        let mut out = Vec::new();
        let mut write = |_: &[i16], time: i64, duration: i64| {
            out.push((time, duration));
            Ok(())
        };
        track.keep_up(4 * 10_000_000, &mut write).unwrap();
        assert_eq!(track.end(), 0);
        track.keep_up(6 * 10_000_000, &mut write).unwrap();
        assert_eq!(track.end(), 5 * 10_000_000);
        assert_eq!(out.first(), Some(&(0, 10_000_000)));
    }

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
