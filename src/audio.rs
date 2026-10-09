//! What the computer plays, through WASAPI loopback: no "Stereo Mix" and
//! no virtual cable.
//!
//! The sound of the whole system comes two ways, the first that works
//! wins:
//!
//! - *Process loopback* (Windows 10 2004 and later): the virtual loopback
//!   device, asked for everything except this process. The format is the
//!   program's choice (32-bit float stereo at 48 kHz, turned into the
//!   16-bit samples the encoder takes), the stream runs on while nothing
//!   plays, and it works where capture on the endpoint itself is refused.
//! - *Endpoint loopback*: the default output device opened for capture,
//!   asked for the same format as the process loopback: the audio engine
//!   converts the device's mix format (a rate the AAC encoder does not
//!   take, 96 kHz say; 5.1 channels, mixed down with the centre kept). The
//!   mix format itself is the fallback when that is refused. Such a stream
//!   only delivers packets while something is rendered, so a silent
//!   render stream on the same device keeps it flowing.
//!
//! The sound of one program is the process loopback asked for that
//! program's process tree, brought to full volume when it is boosted: the
//! volume of its sessions in the Volume Mixer is undone
//! ([`sessions::Volume`]); floating point keeps a quiet program's sound
//! whole until then.
//!
//! Either way the packets carry performance counter timestamps, and the
//! recorder fills any gap with silence.

use std::sync::mpsc;
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, E_FAIL, HANDLE};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
    AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation, IActivateAudioInterfaceCompletionHandler,
    IActivateAudioInterfaceCompletionHandler_Impl, IAudioCaptureClient, IAudioClient, IAudioRenderClient,
    IMMDeviceEnumerator, MMDeviceEnumerator, PROCESS_LOOPBACK_MODE, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eConsole, eRender,
};
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0};
use windows::Win32::System::Com::{BLOB, CLSCTX_ALL, CoCreateInstance, CoTaskMemFree};
use windows::Win32::System::Threading::{CreateEventW, GetCurrentProcessId, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{GUID, IUnknown, Interface, Ref, Result, implement};

use crate::sessions;

const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
const SUBTYPE_PCM: GUID = GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71);
const SUBTYPE_IEEE_FLOAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

/// The rate asked of the process loopback, and of the endpoint when the
/// engine converts for it.
const PROCESS_RATE: u32 = 48000;

/// 32-bit float stereo at `PROCESS_RATE`.
const STEREO_FLOAT: WAVEFORMATEX = WAVEFORMATEX {
    wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
    nChannels: 2,
    nSamplesPerSec: PROCESS_RATE,
    nAvgBytesPerSec: PROCESS_RATE * 8,
    nBlockAlign: 8,
    wBitsPerSample: 32,
    cbSize: 0,
};

/// Whose sound is recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// Everything the computer plays, but this program.
    System,
    /// One program, by the full path of its executable or by its file
    /// name (`firefox.exe`), with its child processes; with `boost`, at
    /// full volume whatever its volume in the Volume Mixer.
    App { program: String, boost: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Samples {
    Float32,
    Int16,
    Int32,
}

/// A packet of captured audio.
pub struct Packet<'a> {
    /// Interleaved 16-bit stereo.
    pub samples: &'a [i16],
    /// When the first frame was recorded, performance counter in 100 ns;
    /// `None` when the device gave no usable time for it.
    pub qpc: Option<i64>,
}

pub struct Loopback {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    keepalive: Option<Keepalive>,
    event: Option<HANDLE>,
    rate: u32,
    channels: usize,
    samples: Samples,
    block_align: usize,
    scratch: Vec<i16>,
    /// The volume to undo, for one program.
    volume: Option<sessions::Volume>,
    /// The gain of the last packet, where the next one starts.
    gain: f32,
    /// How the stream was opened, for the log.
    pub method: &'static str,
}

struct Keepalive {
    client: IAudioClient,
    render: IAudioRenderClient,
    frames: u32,
}

// WASAPI objects are free-threaded; the loopback is opened on one
// recording thread and read on another.
unsafe impl Send for Loopback {}

impl Loopback {
    /// Opens the system's audio for capture: the process loopback, or
    /// the default output device when that is not available.
    pub fn open() -> Result<Loopback> {
        let own = unsafe { GetCurrentProcessId() };
        match Self::open_process(own, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE) {
            Ok(loopback) => Ok(loopback),
            Err(e) => {
                log::warn!("process loopback not available ({e}); trying the endpoint");
                Self::open_endpoint()
            }
        }
    }

    /// Opens the sound of the process tree from `root` (see
    /// [`sessions::find`]); with `boost`, at full volume. There is nothing
    /// to fall back to: without the process loopback this fails.
    pub fn open_app(root: u32, boost: bool) -> Result<Loopback> {
        let mut loopback = Self::open_process(root, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE)?;
        if boost {
            let mut volume = sessions::Volume::new(root);
            loopback.gain = volume.gain();
            loopback.volume = Some(volume);
        }
        loopback.method = if boost { "process loopback of one program, boosted" } else { "process loopback of one program" };
        Ok(loopback)
    }

    fn open_process(pid: u32, mode: PROCESS_LOOPBACK_MODE) -> Result<Loopback> {
        // On the heap: when the activation times out it may still read them,
        // and they are then left to it (a few bytes, once).
        let mut params = Box::new(AUDIOCLIENT_ACTIVATION_PARAMS { ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, ..Default::default() });
        params.Anonymous.ProcessLoopbackParams =
            AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS { TargetProcessId: pid, ProcessLoopbackMode: mode };
        let blob = BLOB {
            cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
            pBlobData: (&mut *params as *mut AUDIOCLIENT_ACTIVATION_PARAMS).cast(),
        };
        // Never dropped: the crate's `PROPVARIANT` clears itself on drop, which
        // would free the blob pointer, and that is the box's.
        let activation = std::mem::ManuallyDrop::new(PROPVARIANT {
            Anonymous: PROPVARIANT_0 {
                Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                    vt: VT_BLOB,
                    wReserved1: 0,
                    wReserved2: 0,
                    wReserved3: 0,
                    Anonymous: PROPVARIANT_0_0_0 { blob },
                }),
            },
        });
        log::debug!("process loopback: activating");
        let (tx, rx) = mpsc::channel();
        let handler: IActivateAudioInterfaceCompletionHandler = Completion(tx).into();
        let _operation = unsafe {
            ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, &IAudioClient::IID, Some(&*activation), &handler)
                .map_err(|e| context("ActivateAudioInterfaceAsync", e))?
        };
        let client = match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(result) => result.map_err(|e| context("activation", e))?,
            Err(_) => {
                Box::leak(params);
                return Err(windows::core::Error::new(E_FAIL, "activation timed out"));
            }
        };
        log::debug!("process loopback: activated");
        let format = STEREO_FLOAT;
        unsafe {
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                    10_000_000,
                    0,
                    &format,
                    None,
                )
                .map_err(|e| context("Initialize (process loopback)", e))?;
        }
        let event = unsafe { CreateEventW(None, false, false, None).map_err(|e| context("CreateEvent", e))? };
        if let Err(e) = unsafe { client.SetEventHandle(event) } {
            unsafe {
                let _ = CloseHandle(event);
            }
            return Err(context("SetEventHandle", e));
        }
        log::debug!("process loopback: initialised");
        let capture: IAudioCaptureClient = unsafe { client.GetService().map_err(|e| context("GetService", e))? };
        Ok(Loopback {
            client,
            capture,
            keepalive: None,
            event: Some(event),
            rate: PROCESS_RATE,
            channels: 2,
            samples: Samples::Float32,
            block_align: 8,
            scratch: Vec::new(),
            volume: None,
            gain: 1.0,
            method: "process loopback",
        })
    }

    fn open_endpoint() -> Result<Loopback> {
        let enumerator: IMMDeviceEnumerator = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
        let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole)? };
        let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }.map_err(|e| context("Activate", e))?;
        let format = unsafe { client.GetMixFormat() }.map_err(|e| context("GetMixFormat", e))?;
        let parsed = unsafe { parse_format(format) };
        let result = (|| {
            let mix = parsed?;
            log::debug!("mix format: {} Hz, {} channels, {:?}, {} bytes per frame", mix.0, mix.1, mix.2, mix.3);
            // The engine converts to stereo at 48 kHz; the mix format as it
            // is when it will not.
            let converted = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
            let (client, (rate, channels, samples, block_align)) =
                match unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, converted, 10_000_000, 0, &STEREO_FLOAT, None) } {
                    Ok(()) => (client, (PROCESS_RATE, 2, Samples::Float32, 8)),
                    Err(e) => {
                        log::warn!("the endpoint loopback will not convert ({}); its mix format is taken", crate::win::describe(&e));
                        // A client whose Initialize failed is not tried again.
                        let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }.map_err(|e| context("Activate", e))?;
                        unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, 10_000_000, 0, format, None) }
                            .map_err(|e| context("Initialize (loopback)", e))?;
                        (client, mix)
                    }
                };
            let capture: IAudioCaptureClient = unsafe { client.GetService() }.map_err(|e| context("GetService", e))?;
            let keepalive = (|| -> Result<Keepalive> {
                let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None)? };
                unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 2_000_000, 0, format, None)? };
                let render: IAudioRenderClient = unsafe { client.GetService()? };
                let frames = unsafe { client.GetBufferSize()? };
                Ok(Keepalive { client, render, frames })
            })();
            let keepalive = match keepalive {
                Ok(k) => Some(k),
                Err(e) => {
                    log::warn!("no silent render stream to keep the loopback alive: {e}");
                    None
                }
            };
            Ok(Loopback {
                client,
                capture,
                keepalive,
                event: None,
                rate,
                channels,
                samples,
                block_align,
                scratch: Vec::new(),
                volume: None,
                gain: 1.0,
                method: "endpoint loopback",
            })
        })();
        unsafe { CoTaskMemFree(Some(format as *const _)) };
        result
    }

    /// Samples per second of the packets.
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Waits up to `timeout` for packets: for the stream's event when it
    /// has one, else the whole time.
    pub fn wait(&self, timeout: Duration) {
        match self.event {
            Some(event) => unsafe {
                WaitForSingleObject(event, timeout.as_millis() as u32);
            },
            None => std::thread::sleep(timeout),
        }
    }

    pub fn start(&self) -> Result<()> {
        if let Some(k) = &self.keepalive {
            k.fill()?;
            unsafe { k.client.Start()? };
        }
        unsafe { self.client.Start() }
    }

    pub fn stop(&self) {
        unsafe {
            let _ = self.client.Stop();
            if let Some(k) = &self.keepalive {
                let _ = k.client.Stop();
            }
        }
    }

    /// Hands every packet waiting in the buffer to `f`, in order, and
    /// tops up the silent render stream. The sound of one program is
    /// brought to full volume, the gain moving across a packet when the
    /// volume has changed since the last one.
    pub fn drain(&mut self, mut f: impl FnMut(Packet)) -> Result<()> {
        let target = self.volume.as_mut().map_or(1.0, sessions::Volume::gain);
        if target != self.gain {
            log::debug!("audio: gain {target:.2} (volume {:.0} %)", 100.0 / target);
        }
        loop {
            let packet = unsafe { self.capture.GetNextPacketSize()? };
            if packet == 0 {
                break;
            }
            let (mut data, mut frames, mut flags, mut qpc) = (std::ptr::null_mut(), 0u32, 0u32, 0u64);
            unsafe { self.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc))? };
            let frames = frames as usize;
            self.scratch.clear();
            self.scratch.resize(frames * 2, 0);
            if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 == 0 && !data.is_null() {
                let bytes = unsafe { std::slice::from_raw_parts(data, frames * self.block_align) };
                convert(bytes, self.channels, self.samples, (self.gain, target), &mut self.scratch);
            }
            self.gain = target;
            // A packet without a time (the flag, or a zero some drivers
            // give) continues the track; counted as before the start it
            // would be dropped, and a track of them would be silent.
            let qpc = (flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 == 0 && qpc != 0).then_some(qpc as i64);
            f(Packet { samples: &self.scratch, qpc });
            unsafe { self.capture.ReleaseBuffer(frames as u32)? };
        }
        if let Some(k) = &self.keepalive {
            k.fill()?;
        }
        Ok(())
    }
}

impl Keepalive {
    /// Fills the free part of the render buffer with silence.
    fn fill(&self) -> Result<()> {
        unsafe {
            let padding = self.client.GetCurrentPadding()?;
            let free = self.frames.saturating_sub(padding);
            if free > 0 {
                let _ = self.render.GetBuffer(free)?;
                self.render.ReleaseBuffer(free, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
            }
        }
        Ok(())
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        self.stop();
        if let Some(event) = self.event.take() {
            unsafe {
                let _ = CloseHandle(event);
            }
        }
    }
}

/// Receives the activated client of the process loopback.
#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Completion(mpsc::Sender<Result<IAudioClient>>);

impl IActivateAudioInterfaceCompletionHandler_Impl for Completion_Impl {
    fn ActivateCompleted(&self, operation: Ref<'_, IActivateAudioInterfaceAsyncOperation>) -> Result<()> {
        let result = (|| -> Result<IAudioClient> {
            let operation = operation.ok()?;
            let mut hr = windows::core::HRESULT(0);
            let mut unknown: Option<IUnknown> = None;
            unsafe { operation.GetActivateResult(&mut hr, &mut unknown)? };
            hr.ok()?;
            unknown.ok_or_else(|| windows::core::Error::from(E_FAIL))?.cast()
        })();
        let _ = self.0.send(result);
        Ok(())
    }
}

/// Rate, channels, sample kind and bytes per frame of a mix format.
unsafe fn parse_format(format: *const WAVEFORMATEX) -> Result<(u32, usize, Samples, usize)> {
    let f = unsafe { *format };
    let (tag, bits) = (f.wFormatTag, f.wBitsPerSample);
    let kind = if tag == WAVE_FORMAT_EXTENSIBLE {
        let ext = unsafe { *(format as *const WAVEFORMATEXTENSIBLE) };
        match ext.SubFormat {
            SUBTYPE_IEEE_FLOAT if bits == 32 => Some(Samples::Float32),
            SUBTYPE_PCM if bits == 16 => Some(Samples::Int16),
            SUBTYPE_PCM if bits == 32 => Some(Samples::Int32),
            _ => None,
        }
    } else {
        match (tag, bits) {
            (WAVE_FORMAT_IEEE_FLOAT, 32) => Some(Samples::Float32),
            (WAVE_FORMAT_PCM, 16) => Some(Samples::Int16),
            (WAVE_FORMAT_PCM, 32) => Some(Samples::Int32),
            _ => None,
        }
    };
    let Some(kind) = kind else {
        return Err(windows::core::Error::new(E_FAIL, format!("unsupported mix format: tag {tag:#x}, {bits} bits")));
    };
    if f.nChannels == 0 || f.nSamplesPerSec == 0 {
        return Err(windows::core::Error::new(E_FAIL, "empty mix format"));
    }
    // `convert` reads the frames by this layout.
    let (align, channels) = (f.nBlockAlign, f.nChannels);
    if usize::from(align) != usize::from(channels) * usize::from(bits / 8) {
        return Err(windows::core::Error::new(E_FAIL, format!("mix format with padded frames: {align} bytes for {channels} channels")));
    }
    Ok((f.nSamplesPerSec, f.nChannels as usize, kind, f.nBlockAlign as usize))
}

/// Frames of the device's format into interleaved 16-bit stereo: the
/// first two channels (a mono device fills both), multiplied by a gain
/// that goes from `gain.0` to `gain.1` across the frames. A boosted sound
/// is bent softly towards full scale over its top fifth (`soft_limit`)
/// instead of being cut off flat there; anything else past full scale is
/// clipped.
fn convert(bytes: &[u8], channels: usize, samples: Samples, gain: (f32, f32), out: &mut [i16]) {
    let frames = out.len() / 2;
    let step = (gain.1 - gain.0) / frames.max(1) as f32;
    let sample = |frame: usize, channel: usize, gain: f32| -> i16 {
        let i = frame * channels + channel;
        let v = match samples {
            Samples::Float32 => f32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap()),
            Samples::Int16 => f32::from(i16::from_le_bytes(bytes[i * 2..i * 2 + 2].try_into().unwrap())) / 32768.0,
            Samples::Int32 => i32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap()) as f32 / 2_147_483_648.0,
        };
        let v = if gain > 1.0 { soft_limit(v * gain) } else { v * gain };
        (v * 32768.0).round().clamp(-32768.0, 32767.0) as i16
    };
    for frame in 0..frames {
        let gain = gain.0 + step * (frame + 1) as f32;
        let left = sample(frame, 0, gain);
        let right = if channels > 1 { sample(frame, 1, gain) } else { left };
        out[frame * 2] = left;
        out[frame * 2 + 1] = right;
    }
}

/// `v` (full scale at 1) unchanged below 0.8, bent towards 1 above it
/// (tanh): a boosted quiet program's loud moments are rounded off rather
/// than cut flat.
fn soft_limit(v: f32) -> f32 {
    const KNEE: f32 = 0.8;
    let a = v.abs();
    if a <= KNEE {
        return v;
    }
    (KNEE + (1.0 - KNEE) * ((a - KNEE) / (1.0 - KNEE)).tanh()).copysign(v)
}

/// The error with the name of the call that failed in its message.
fn context(call: &str, e: windows::core::Error) -> windows::core::Error {
    windows::core::Error::new(e.code(), format!("{call}: {}", e.message()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_to_stereo_int16() {
        let frames: Vec<f32> = vec![0.5, -0.5, 1.5, 0.0];
        let bytes: Vec<u8> = frames.iter().flat_map(|f| f.to_le_bytes()).collect();
        let mut out = vec![0i16; 4];
        convert(&bytes, 2, Samples::Float32, (1.0, 1.0), &mut out);
        assert_eq!(out, vec![16384, -16384, 32767, 0]);
    }

    #[test]
    fn mono_is_doubled() {
        let bytes: Vec<u8> = [100i16, -100].iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut out = vec![0i16; 4];
        convert(&bytes, 1, Samples::Int16, (1.0, 1.0), &mut out);
        assert_eq!(out, vec![100, 100, -100, -100]);
    }

    #[test]
    fn gain_moves_across_the_packet() {
        let frames: Vec<f32> = vec![0.01; 8];
        let bytes: Vec<u8> = frames.iter().flat_map(|f| f.to_le_bytes()).collect();
        let mut out = vec![0i16; 8];
        convert(&bytes, 2, Samples::Float32, (1.0, 5.0), &mut out);
        assert_eq!(out, vec![655, 655, 983, 983, 1311, 1311, 1638, 1638]);
        // A quiet program at full volume: limited, not wrapped.
        convert(&bytes, 2, Samples::Float32, (400.0, 400.0), &mut out);
        assert!(out.iter().all(|&s| s > 32700));
    }

    #[test]
    fn boosted_sound_is_bent_not_cut() {
        assert_eq!(soft_limit(0.5), 0.5);
        assert_eq!(soft_limit(-0.8), -0.8);
        // Above the knee: still rising, below full scale, symmetric.
        let (a, b) = (soft_limit(0.9), soft_limit(1.5));
        assert!(0.8 < a && a < b && b < 1.0);
        assert_eq!(soft_limit(-1.5), -b);
    }
}
