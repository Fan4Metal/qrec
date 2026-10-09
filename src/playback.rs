//! Playing a recording back in the trimming window. The sound
//! (`trim::Sound`, decoded to 32-bit float) goes to the default output
//! device through a shared WASAPI stream, which converts it to the mix
//! format; the position of what is heard there (`IAudioClock`) is the
//! clock the picture follows. Without sound (no track, no device) the
//! clock is the wall clock.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Media::Audio::{
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioClient, IAudioClock,
    IAudioRenderClient, IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX, eConsole, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::core::Result;

use crate::trim::{SECOND, Sound};
use crate::win;

const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
/// The length of the device buffer: what is written ahead of what is
/// heard.
const BUFFER: i64 = SECOND / 5;

/// Playback from one time to another, on a thread of its own.
pub struct Playback {
    shared: Arc<Shared>,
    from: i64,
    thread: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct Shared {
    /// Where in the file the sound heard now is (100 ns units).
    position: AtomicI64,
    stop: AtomicBool,
    /// The sound has been played to the end of the stretch or the file.
    finished: AtomicBool,
    /// When the clock is the wall clock: the instant playback was at
    /// `from`.
    wall: Mutex<Option<Instant>>,
}

impl Playback {
    /// Plays `path` from `from` up to `until` (100 ns units).
    pub fn start(path: &Path, from: i64, until: i64) -> Playback {
        let shared = Arc::new(Shared::default());
        shared.position.store(from, Relaxed);
        let worker = Arc::clone(&shared);
        let path: PathBuf = path.to_path_buf();
        let thread = std::thread::Builder::new()
            .name("playback".into())
            .spawn(move || {
                let _com = win::com_init_mta();
                let sound = match run(&path, from, until, &worker) {
                    Ok(sound) => sound,
                    Err(e) => {
                        log::warn!("no sound in playback: {}", win::describe(&e));
                        false
                    }
                };
                // Without sound, on by the wall clock from where it got to.
                if !sound && !worker.stop.load(Relaxed) {
                    let at = Duration::from_nanos((worker.position.load(Relaxed) - from).max(0) as u64 * 100);
                    *worker.wall.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now() - at);
                }
            })
            .ok();
        Playback { shared, from, thread }
    }

    /// Where in the file playback is.
    pub fn position(&self) -> i64 {
        match *self.shared.wall.lock().unwrap_or_else(|e| e.into_inner()) {
            Some(at) => self.from + (at.elapsed().as_nanos() / 100) as i64,
            None => self.shared.position.load(Relaxed),
        }
    }

    /// Whether the stretch has been played to its end.
    pub fn finished(&self) -> bool {
        self.shared.finished.load(Relaxed)
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        self.shared.stop.store(true, Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Plays the sound; `Ok(false)` when the file has none, or has none left
/// before `until` (the track shorter than the picture): the wall clock
/// takes over from the position reached.
fn run(path: &Path, from: i64, until: i64, shared: &Shared) -> Result<bool> {
    let Some(sound) = Sound::open(path)? else { return Ok(false) };
    sound.seek(from)?;
    let (rate, channels) = (sound.rate, sound.channels as usize);
    let enumerator: IMMDeviceEnumerator = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
    let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole)? };
    let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None)? };
    let format = WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
        nChannels: channels as u16,
        nSamplesPerSec: rate,
        nAvgBytesPerSec: rate * 4 * channels as u32,
        nBlockAlign: 4 * channels as u16,
        wBitsPerSample: 32,
        cbSize: 0,
    };
    // The audio engine converts the file's format to the device's.
    let flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, BUFFER, 0, &format, None)? };
    let render: IAudioRenderClient = unsafe { client.GetService()? };
    let clock: IAudioClock = unsafe { client.GetService()? };
    let frequency = unsafe { clock.GetFrequency()? }.max(1);
    let size = unsafe { client.GetBufferSize()? };

    let mut queue: VecDeque<f32> = VecDeque::new();
    // The time of the first frame written: `from`, or the file's first
    // sound after it.
    let mut origin: Option<i64> = None;
    let mut written = 0u64;
    let (mut ended, mut started) = (false, false);
    let mut drained_at: Option<Instant> = None;
    while !shared.stop.load(Relaxed) {
        while !ended && queue.len() < size as usize * channels * 2 {
            match sound.next()? {
                Some((time, samples)) => {
                    let mut samples = &samples[..];
                    if origin.is_none() {
                        // What comes before `from` in the block is not played.
                        let skip = ((from - time).max(0) as i128 * rate as i128 / SECOND as i128) as usize * channels;
                        if skip >= samples.len() {
                            continue;
                        }
                        samples = &samples[skip..];
                        origin = Some(time.max(from));
                    }
                    queue.extend(samples);
                }
                None => ended = true,
            }
        }
        let Some(origin) = origin else {
            // No sound after `from`.
            return Ok(false);
        };
        let limit = ((until - origin).max(0) as i128 * rate as i128 / SECOND as i128) as u64;
        let padding = unsafe { client.GetCurrentPadding()? };
        let left = limit.saturating_sub(written).min((queue.len() / channels) as u64) as u32;
        let n = (size - padding).min(left);
        if n > 0 {
            unsafe {
                let buffer = render.GetBuffer(n)?;
                let out = std::slice::from_raw_parts_mut(buffer.cast::<f32>(), n as usize * channels);
                for (o, s) in out.iter_mut().zip(queue.drain(..n as usize * channels)) {
                    *o = s;
                }
                render.ReleaseBuffer(n, 0)?;
            }
            written += n as u64;
        }
        if !started {
            unsafe { client.Start()? };
            started = true;
        }
        let mut heard = 0u64;
        unsafe { clock.GetPosition(&mut heard, None)? };
        let played = (heard as i128 * SECOND as i128 / frequency as i128) as i64;
        shared.position.store(origin + played, Relaxed);
        // The buffer is empty some 10 to 20 ms before its last frame is
        // heard: the clock says when (or a fifth of a second, should it
        // stop short).
        let heard_frames = (heard as i128 * rate as i128 / frequency as i128) as u64;
        let drained = left == 0 && padding == 0;
        let since = if drained { *drained_at.get_or_insert_with(Instant::now) } else { Instant::now() };
        if drained && (heard_frames >= written || since.elapsed() > Duration::from_millis(200)) {
            // All written has been heard: the end of the stretch, or of the
            // sound before it.
            if written < limit {
                unsafe {
                    let _ = client.Stop();
                }
                return Ok(false);
            }
            shared.finished.store(true, Relaxed);
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    unsafe {
        let _ = client.Stop();
    }
    Ok(true)
}
