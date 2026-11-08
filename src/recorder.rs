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
    pub fn bits_per_pixel(self) -> f64 {
        match self {
            Quality::Low => 0.05,
            Quality::Medium => 0.1,
            Quality::High => 0.2,
        }
    }

    /// The quantisation parameter at a constant quality.
    pub fn qp(self) -> u32 {
        match self {
            Quality::Low => 30,
            Quality::Medium => 26,
            Quality::High => 22,
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

/// How the encoder spends bits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RateControl {
    /// The same quality in every frame: the file follows what is on the
    /// screen, a still one takes almost nothing.
    #[default]
    Quality,
    /// A target bitrate from the area, the frame rate and the quality:
    /// the size is known beforehand, motion loses detail.
    Bitrate,
}

impl RateControl {
    pub const ALL: [RateControl; 2] = [RateControl::Quality, RateControl::Bitrate];

    /// Name in the settings and on the command line.
    pub fn name(self) -> &'static str {
        match self {
            RateControl::Quality => "quality",
            RateControl::Bitrate => "bitrate",
        }
    }

    pub fn from_name(name: &str) -> Option<RateControl> {
        Self::ALL.into_iter().find(|r| r.name() == name)
    }

    pub fn label(self) -> &'static str {
        match self {
            RateControl::Quality => tr!("By quality", "По качеству"),
            RateControl::Bitrate => tr!("Fixed", "Заданный"),
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
    pub rate: RateControl,
    /// Whose sound is recorded; `None` for none.
    pub audio: Option<Source>,
    /// Whether the default microphone is recorded too, mixed in.
    pub microphone: bool,
    /// Draw the pointer into the frames.
    pub cursor: bool,
    /// Draw its clicks too (with the pointer only).
    pub clicks: bool,
    /// A window the area follows: moved with it, its size kept, within the
    /// monitor.
    pub follow: Option<isize>,
    pub path: PathBuf,
}

/// What the recording turned out to use.
#[derive(Clone, Debug, Default)]
pub struct Info {
    pub encoder: String,
    pub hardware: bool,
    /// Whether the encoder took the constant quality (else the bitrate).
    pub constant_quality: bool,
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
    /// Why the microphone could not be opened; the rest goes on.
    microphone_error: Mutex<Option<String>>,
    pauses: Mutex<Pauses>,
}

impl Shared {
    fn pauses(&self) -> std::sync::MutexGuard<'_, Pauses> {
        self.pauses.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The pauses of a recording, on the performance counter (100 ns units):
/// from when to when, the last one open while paused. The file leaves them
/// out: a time on the counter is moved back by the pauses before it, and
/// what was captured during one is dropped. The video thread encodes no
/// ticks while paused; the audio thread maps each packet by its own time,
/// so a packet of the moment before the pause that arrives after it still
/// counts.
#[derive(Clone, Default, Debug)]
struct Pauses(Vec<(i64, Option<i64>)>);

impl Pauses {
    fn paused(&self) -> bool {
        self.0.last().is_some_and(|(_, to)| to.is_none())
    }

    fn pause(&mut self, now: i64) {
        if !self.paused() {
            self.0.push((now, None));
        }
    }

    fn resume(&mut self, now: i64) {
        if let Some((from, to @ None)) = self.0.last_mut() {
            *to = Some(now.max(*from));
        }
    }

    /// The time paused before `t`; `None` when `t` falls in a pause.
    fn before(&self, t: i64) -> Option<i64> {
        let mut sum = 0;
        for &(from, to) in &self.0 {
            if t < from {
                break;
            }
            match to {
                Some(to) if t >= to => sum += to - from,
                _ => return None,
            }
        }
        Some(sum)
    }

    /// The time paused up to `now`, an open pause included.
    fn total(&self, now: i64) -> i64 {
        self.0.iter().map(|&(from, to)| to.unwrap_or(now).max(from) - from).sum()
    }
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

    /// Why the microphone could not be opened, when it could not: the
    /// recording goes on without it.
    pub fn microphone_error(&self) -> Option<String> {
        self.shared.microphone_error.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Pauses the recording, or resumes it: the time paused is left out of
    /// the file.
    pub fn set_paused(&self, paused: bool) {
        let now = win::qpc_100ns();
        let mut pauses = self.shared.pauses();
        if paused {
            pauses.pause(now);
        } else {
            pauses.resume(now);
        }
        log::debug!("paused: {paused}");
    }

    pub fn paused(&self) -> bool {
        self.shared.pauses().paused()
    }

    /// The time recorded, the pauses left out.
    pub fn elapsed(&self) -> Duration {
        let paused = self.shared.pauses().total(win::qpc_100ns());
        self.started.elapsed().saturating_sub(Duration::from_nanos(paused.max(0) as u64 * 100))
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
        let video = VideoConfig {
            width,
            height,
            fps: config.fps,
            bitrate: bitrate(width, height, config.fps, config.quality),
            qp: (config.rate == RateControl::Quality).then(|| config.quality.qp()),
        };
        // The microphone is opened by the audio thread, at the rate of the
        // other sound (its stream takes seconds to initialise here).
        let sources: Vec<Loopback> = loopback.into_iter().collect();
        let audio_rate = sources.first().map_or(48_000, Loopback::rate);
        let audio = (!sources.is_empty() || config.microphone).then_some(AudioConfig { sample_rate: audio_rate });
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
        let clicks = (drawer.is_some() && config.clicks).then(crate::clicks::Clicks::start);
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
        for l in &sources {
            log::debug!("audio: {} at {} Hz", l.method, l.rate());
        }
        if config.microphone {
            log::debug!("audio: the microphone at {audio_rate} Hz, once open");
        }
        let info = Info {
            encoder: encoder.encoder_name.clone(),
            hardware: encoder.hardware,
            constant_quality: encoder.constant_quality,
            audio_rate: audio.map(|a| a.sample_rate),
        };
        Ok((capturer, converter, Arc::new(encoder), sources, info, drawer, clicks))
    })();
    let (mut capturer, mut converter, encoder, sources, info, mut drawer, clicks) = match setup {
        Ok(s) => s,
        Err(e) => {
            let _ = ready.send(Err(e.clone()));
            return Err(e);
        }
    };
    log::debug!("recording {}x{} at {} fps to {}", config.region.width, config.region.height, config.fps, config.path.display());

    let start = win::qpc_100ns();
    let microphone = config.microphone;
    let audio_rate = info.audio_rate;
    let audio_thread = audio_rate.and_then(|rate| {
        let encoder = Arc::clone(&encoder);
        let shared = Arc::clone(shared);
        std::thread::Builder::new()
            .name("audio".into())
            .spawn(move || {
                let _com = win::com_init_mta();
                if let Err(e) = run_audio(sources, microphone, rate, &encoder, start, &shared) {
                    log::error!("audio stopped: {e}");
                    shared.audio_error.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert(e);
                }
            })
            .ok()
    });
    let _ = ready.send(Ok(info));

    let interval = 10_000_000 / i64::from(config.fps);
    let monitor_origin = (config.monitor.rect.left, config.monitor.rect.top);
    // The area moves with a window it follows, as each frame is copied.
    if let Some(window) = config.follow {
        capturer.follow(window, config.monitor.rect);
    }
    let mut tick: i64 = 0;
    let mut lost_since: Option<Instant> = None;
    let mut result = Ok(());
    'record: while !shared.stop.load(Relaxed) {
        let (paused, offset) = {
            let pauses = shared.pauses();
            (pauses.paused(), pauses.total(win::qpc_100ns()))
        };
        if paused {
            // Nothing is encoded, and the ticks wait; the desktop is still
            // read, so the first frame after the pause is the current one.
            match capturer.poll(20) {
                Ok(_) => {}
                Err(PollError::AccessLost) => {
                    if capturer.recreate().is_err() {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
                Err(PollError::Other(e)) => {
                    result = Err(format!("{}: {}", tr!("screen capture", "захват экрана"), win::describe(&e)));
                    break 'record;
                }
            }
            continue;
        }
        // The clock of the file: the counter less the time paused.
        let base = start + offset;
        let target = base + tick * interval;
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
            tick = (win::qpc_100ns() - base) / interval + 1;
            continue;
        }
        if let Some((composed, drawer)) = &mut drawer {
            unsafe { capturer.context().CopyResource(&*composed, capturer.frame()) };
            // Where the frame was taken from (it moves with a window).
            let local = capturer.region();
            if let Some(clicks) = &clicks {
                let now = win::qpc_100ns();
                let corner = (local.x + monitor_origin.0, local.y + monitor_origin.1);
                if let Err(e) = drawer.draw_clicks(&clicks.state(now), now, corner) {
                    log::warn!("clicks not drawn: {}", win::describe(&e));
                }
            }
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
        let behind = (win::qpc_100ns() - (base + tick * interval)) / interval;
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

/// The sound sources (the computer's, the microphone, or both, at `rate`),
/// each laid down on a track of its own and mixed into the file. The
/// microphone, when wanted, is opened on a thread of its own and joins
/// the others once open (its stream takes seconds to initialise on some
/// machines); until then it counts as silence. When it cannot be opened
/// the recording goes on without it (`Shared::microphone_error`).
fn run_audio(mut sources: Vec<Loopback>, microphone: bool, rate: u32, encoder: &Encoder, start: i64, shared: &Shared) -> Result<(), String> {
    let describe = |e: windows::core::Error| format!("{}: {}", tr!("audio capture", "захват звука"), win::describe(&e));
    let mut opening = microphone.then(|| {
        let (tx, rx) = mpsc::channel();
        let _ = std::thread::Builder::new().name("microphone".into()).spawn(move || {
            let _com = win::com_init_mta();
            let opened = Loopback::open_microphone(rate).map_err(|e| format!("{}: {}", tr!("microphone", "микрофон"), win::describe(&e)));
            let _ = tx.send(opened);
        });
        rx
    });
    let rate = i64::from(rate);
    let mut write = |samples: &[i16], frame: i64| {
        let (time, end) = (frame * 10_000_000 / rate, (frame + samples.len() as i64 / 2) * 10_000_000 / rate);
        encoder.write_audio(samples, time, end - time).map_err(|e| format!("{}: {}", tr!("encoder", "кодер"), win::describe(&e)))
    };
    // The microphone's slot is the last one.
    let slots = sources.len() + usize::from(microphone);
    let mut tracks: Vec<Track> = (0..slots).map(|_| Track::new(rate)).collect();
    let mut mixer = Mixer::new(slots, rate);
    for source in &sources {
        source.start().map_err(describe)?;
    }
    let mut packets: Vec<(Vec<i16>, Option<i64>)> = Vec::new();
    let mut result = Ok(());
    let mut logged = 0;
    'record: loop {
        if let Some(rx) = &opening {
            let opened = match rx.try_recv() {
                Ok(opened) => Some(opened),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => Some(Err(tr!("microphone: not opened", "микрофон: не открыт").into())),
            };
            match opened.map(|o| o.and_then(|mic| mic.start().map(|()| mic).map_err(describe))) {
                Some(Ok(mic)) => {
                    log::debug!("the microphone joins at {:.2} s", (win::qpc_100ns() - start) as f64 / 1e7);
                    sources.push(mic);
                    opening = None;
                }
                Some(Err(e)) => {
                    log::error!("{e}");
                    shared.microphone_error.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert(e);
                    mixer.retire(slots - 1);
                    opening = None;
                }
                None => {}
            }
        }
        match sources.first() {
            Some(source) => source.wait(Duration::from_millis(10)),
            None => std::thread::sleep(Duration::from_millis(10)),
        }
        // A copy: the lock is not held while the encoder writes.
        let pauses = shared.pauses().clone();
        let now = win::qpc_100ns();
        let now = now - pauses.total(now) - start;
        for (i, (source, track)) in sources.iter_mut().zip(&mut tracks).enumerate() {
            packets.clear();
            if let Err(e) = source.drain(|p| packets.push((p.samples.to_vec(), p.qpc))) {
                result = Err(describe(e));
                break 'record;
            }
            let mut push = |samples: &[i16], frame: i64| {
                mixer.push(i, samples, frame);
                Ok(())
            };
            for (samples, qpc) in packets.drain(..) {
                // On the file's clock; recorded during a pause, dropped (one
                // without a time, while paused).
                let time = match qpc {
                    Some(qpc) => match pauses.before(qpc) {
                        Some(paused) => Some(qpc - paused - start),
                        None => continue,
                    },
                    None if pauses.paused() => continue,
                    None => None,
                };
                track.add(&samples, time, &mut push)?;
            }
            track.flush(&mut push)?;
            // Nothing played for a while and no keep-alive: keep the track going.
            track.keep_up(now, &mut push)?;
        }
        mixer.mix(false, &mut write)?;
        if now / 50_000_000 > logged {
            logged = now / 50_000_000;
            for (track, source) in tracks.iter().zip(&sources) {
                log::debug!(
                    "{} at {:.2} s lags the clock by {} ms, {} frames added or left out",
                    source.method,
                    track.end() as f64 / 1e7,
                    (now - track.end()) / 10_000,
                    track.corrections
                );
            }
        }
        // Once more after the stop, for the packets of the last moment.
        if shared.stop.load(Relaxed) {
            break;
        }
    }
    let mixed = mixer.mix(true, &mut write);
    for source in &sources {
        source.stop();
    }
    result.and(mixed)
}

/// A source this far behind the furthest one is taken as silent there: a
/// stream that delivers nothing for a while does not hold the others up.
const MIX_LAG: i64 = 10_000_000;

/// The tracks of the sources summed into one. Each writes its samples at
/// their frames, contiguous from where it starts; what every source has
/// reached is summed and written. A source not started yet, or more than
/// `MIX_LAG` behind, counts as silence, and what it brings later for frames
/// already written is dropped.
struct Mixer {
    rate: i64,
    /// Per source: the frame of the first sample waiting, and the samples.
    sources: Vec<Option<(i64, Vec<i16>)>>,
    /// Sources that will bring nothing more (a microphone that would not
    /// open): not waited for.
    retired: Vec<bool>,
    /// The next frame to write: from the start of the file, silence until
    /// the first source brings something (a microphone that takes seconds
    /// to open, alone).
    next: i64,
}

impl Mixer {
    fn new(sources: usize, rate: i64) -> Mixer {
        Mixer { rate, sources: vec![None; sources], retired: vec![false; sources], next: 0 }
    }

    fn retire(&mut self, source: usize) {
        self.retired[source] = true;
    }

    fn push(&mut self, source: usize, samples: &[i16], frame: i64) {
        let pending = self.sources[source].get_or_insert_with(|| (frame, Vec::new()));
        // A track's writes follow on; a gap is silence, an overlap dropped.
        let end = pending.0 + pending.1.len() as i64 / 2;
        let mut samples = samples;
        if frame > end {
            pending.1.resize(pending.1.len() + (frame - end) as usize * 2, 0);
        } else if frame < end {
            samples = &samples[((end - frame) as usize * 2).min(samples.len())..];
        }
        pending.1.extend_from_slice(samples);
        // What lies before the frames written is too late.
        let next = self.next;
        if pending.0 < next {
            let drop = ((next - pending.0) as usize * 2).min(pending.1.len());
            pending.1.drain(..drop);
            pending.0 += drop as i64 / 2;
        }
    }

    /// Writes what every source has reached (with `all`, what any has).
    fn mix(&mut self, all: bool, write: &mut impl FnMut(&[i16], i64) -> Result<(), String>) -> Result<(), String> {
        let ends: Vec<Option<i64>> = self.sources.iter().map(|s| s.as_ref().map(|(start, samples)| start + samples.len() as i64 / 2)).collect();
        let Some(furthest) = ends.iter().flatten().max().copied() else { return Ok(()) };
        let first = self.next;
        let until = if all {
            furthest
        } else {
            let slowest = ends.iter().zip(&self.retired).filter(|(_, retired)| !**retired).map(|(e, _)| e.unwrap_or(i64::MIN)).min().unwrap_or(furthest);
            slowest.max(furthest - MIX_LAG * self.rate / 10_000_000)
        };
        if until <= first {
            return Ok(());
        }
        let mut out = vec![0i16; (until - first) as usize * 2];
        for (start, samples) in self.sources.iter_mut().flatten() {
            for frame in (*start).max(first)..(*start + samples.len() as i64 / 2).min(until) {
                let (from, to) = ((frame - *start) as usize * 2, (frame - first) as usize * 2);
                out[to] = out[to].saturating_add(samples[from]);
                out[to + 1] = out[to + 1].saturating_add(samples[from + 1]);
            }
            let used = (until - *start).clamp(0, samples.len() as i64 / 2);
            samples.drain(..used as usize * 2);
            *start += used;
            // Left behind: it starts again where the file is.
            if samples.is_empty() && *start < until {
                *start = until;
            }
        }
        self.next = until;
        write(&out, first)
    }
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
    /// `write` takes samples and the frame of the first.
    fn add(&mut self, samples: &[i16], time: Option<i64>, write: &mut impl FnMut(&[i16], i64) -> Result<(), String>) -> Result<(), String> {
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
    fn flush(&mut self, write: &mut impl FnMut(&[i16], i64) -> Result<(), String>) -> Result<(), String> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let end = self.buffer_start + self.buffer.len() as i64 / 2;
        let result = write(&self.buffer, self.buffer_start);
        self.buffer.clear();
        self.buffer_start = end;
        result
    }

    /// `frames` of silence from the position on; the buffer is empty.
    fn silence(&mut self, frames: i64, write: &mut impl FnMut(&[i16], i64) -> Result<(), String>) -> Result<(), String> {
        let mut at = self.position.unwrap_or(0);
        let end = at + frames;
        while at < end {
            let chunk = (end - at).min(self.rate);
            let silence = vec![0i16; chunk as usize * 2];
            write(&silence, at)?;
            at += chunk;
        }
        self.position = Some(end);
        self.buffer_start = end;
        Ok(())
    }

    /// Silence up to a second before `now` when the track has fallen more
    /// than five behind: a stream that delivers nothing while nothing
    /// plays.
    fn keep_up(&mut self, now: i64, write: &mut impl FnMut(&[i16], i64) -> Result<(), String>) -> Result<(), String> {
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
        let mut write = |s: &[i16], frame: i64| {
            out.push((frame, s.len() as i64 / 2, s[0]));
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
        let mut write = |s: &[i16], frame: i64| {
            out.push((frame, s.len() as i64 / 2));
            Ok(())
        };
        track.keep_up(4 * 10_000_000, &mut write).unwrap();
        assert_eq!(track.end(), 0);
        track.keep_up(6 * 10_000_000, &mut write).unwrap();
        assert_eq!(track.end(), 5 * 10_000_000);
        assert_eq!(out.first(), Some(&(0, 48_000)));
    }

    /// What a mixer writes, as (first frame, frames, the first left sample).
    fn mixed(mixer: &mut Mixer, all: bool) -> Vec<(i64, i64, i16)> {
        let mut out = Vec::new();
        mixer
            .mix(all, &mut |s: &[i16], frame: i64| {
                out.push((frame, s.len() as i64 / 2, s[0]));
                Ok(())
            })
            .unwrap();
        out
    }

    #[test]
    fn mixer_sums_what_both_sources_reached() {
        let mut mixer = Mixer::new(2, 48_000);
        mixer.push(0, &[100; 200], 1000);
        // The second source has not started: nothing yet within the lag.
        assert_eq!(mixed(&mut mixer, false), vec![]);
        // Retired, it is not waited for; the file starts with silence.
        let mut alone = Mixer::new(2, 48_000);
        alone.push(0, &[100; 200], 1000);
        alone.retire(1);
        assert_eq!(mixed(&mut alone, false), vec![(0, 1100, 0)]);
        mixer.push(1, &[10; 100], 1000);
        assert_eq!(mixed(&mut mixer, false), vec![(0, 1050, 0)]);
        // The rest of the first, the second silent for it at the end.
        assert_eq!(mixed(&mut mixer, true), vec![(1050, 50, 100)]);
        // Late samples of the second are dropped; later ones are mixed.
        mixer.push(1, &[10; 200], 1080);
        mixer.push(0, &[100; 40], 1100);
        assert_eq!(mixed(&mut mixer, false), vec![(1100, 20, 110)]);
    }

    #[test]
    fn mixer_does_not_wait_for_a_silent_source() {
        let mut mixer = Mixer::new(2, 48_000);
        mixer.push(0, &[1; 2 * 60_000], 0);
        // More than a second ahead: written up to a second before its end.
        assert_eq!(mixed(&mut mixer, false), vec![(0, 12_000, 1)]);
        // A source alone is written as it is.
        let mut alone = Mixer::new(1, 48_000);
        alone.push(0, &[7; 20], 0);
        assert_eq!(mixed(&mut alone, false), vec![(0, 10, 7)]);
        // Two loud ones are clipped, not wrapped.
        let mut loud = Mixer::new(2, 48_000);
        loud.push(0, &[30_000; 2], 0);
        loud.push(1, &[30_000; 2], 0);
        assert_eq!(mixed(&mut loud, false), vec![(0, 1, i16::MAX)]);
    }

    #[test]
    fn pauses_are_left_out() {
        let mut pauses = Pauses::default();
        assert_eq!(pauses.before(100), Some(0));
        pauses.pause(1000);
        pauses.pause(1500);
        assert!(pauses.paused());
        assert_eq!(pauses.before(999), Some(0));
        assert_eq!(pauses.before(1200), None);
        assert_eq!(pauses.total(1600), 600);
        pauses.resume(2000);
        pauses.resume(2500);
        assert!(!pauses.paused());
        assert_eq!(pauses.before(2000), Some(1000));
        assert_eq!(pauses.total(9000), 1000);
        pauses.pause(3000);
        pauses.resume(3100);
        assert_eq!(pauses.before(2999), Some(1000));
        assert_eq!(pauses.before(3050), None);
        assert_eq!(pauses.before(5000), Some(1100));
        assert_eq!(pauses.total(5000), 1100);
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
        for r in RateControl::ALL {
            assert_eq!(RateControl::from_name(r.name()), Some(r));
        }
        assert_eq!(RateControl::from_name("cbr"), None);
    }
}
