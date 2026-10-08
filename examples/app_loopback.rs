//! Process loopback of one program: whether it hears the sound before or
//! after the volume of the program's session (the Volume Mixer) and the
//! device's master volume. A child process (this example with `play`)
//! plays a quiet tone at several session volumes while this one captures
//! the child's tree and prints the level; the master volume is halved for
//! a moment and restored. The tone is audible for several seconds.
//!
//! The child is needed: a process that captures itself has its session
//! volume applied twice, to the tone and to the capture stream, which is
//! in the same session.
use std::process::{Child, Command};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0};
use windows::Win32::System::Com::*;
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{IUnknown, Interface, Result, implement};

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Handler(mpsc::Sender<Result<IAudioClient>>);

impl IActivateAudioInterfaceCompletionHandler_Impl for Handler_Impl {
    fn ActivateCompleted(&self, op: windows::core::Ref<'_, IActivateAudioInterfaceAsyncOperation>) -> Result<()> {
        let result = (|| -> Result<IAudioClient> {
            let op = op.ok()?;
            let mut hr = windows::core::HRESULT(0);
            let mut unknown: Option<IUnknown> = None;
            unsafe { op.GetActivateResult(&mut hr, &mut unknown)? };
            hr.ok()?;
            unknown.ok_or_else(|| windows::core::Error::from(E_FAIL))?.cast()
        })();
        let _ = self.0.send(result);
        Ok(())
    }
}

const AMPLITUDE: f32 = 0.1;

fn main() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("play") {
        let volume: f32 = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(0.25);
        let seconds: f64 = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(10.0);
        play(volume, seconds);
        return;
    }
    unsafe {
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).unwrap();
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).unwrap();
        let endpoint: IAudioEndpointVolume = device.Activate(CLSCTX_ALL, None).unwrap();
        let hardware = endpoint.QueryHardwareSupport().unwrap();
        println!("endpoint hardware support: {hardware:#x} (volume in hardware: {})", hardware & 2 != 0);
        let master = endpoint.GetMasterVolumeLevelScalar().unwrap();
        let master_db = endpoint.GetMasterVolumeLevel().unwrap();
        println!("master volume: {:.0} % ({master_db:.2} dB)", master * 100.0);
        println!("the tone itself: rms {:.2} dBFS", 20.0 * (f64::from(AMPLITUDE) / 2f64.sqrt()).log10());

        for v in [1.0f32, 0.5, 0.25, 0.1] {
            let linear = 20.0 * v.log10();
            let level = measure(v, || {});
            println!("session {:3.0} % (v: {linear:6.1} dB, v²: {:6.1} dB): rms {level:7.2} dBFS", v * 100.0, linear * 2.0);
        }
        let lower = master * 0.5;
        let level = measure(1.0, || endpoint.SetMasterVolumeLevelScalar(lower, std::ptr::null()).unwrap());
        let lower_db = endpoint.GetMasterVolumeLevel().unwrap();
        endpoint.SetMasterVolumeLevelScalar(master, std::ptr::null()).unwrap();
        println!("master {:.0} % ({lower_db:.2} dB), session 100 %: rms {level:7.2} dBFS", lower * 100.0);
    }
}

/// Plays the tone at a volume of this process's session; the session is
/// set back to full volume after, as the Volume Mixer keeps it between runs.
fn play(volume: f32, seconds: f64) {
    unsafe {
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).unwrap();
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).unwrap();
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None).unwrap();
        let format = client.GetMixFormat().unwrap();
        let (channels, rate, bits) = ((*format).nChannels as usize, (*format).nSamplesPerSec, (*format).wBitsPerSample);
        assert_eq!(bits, 32, "the tone wants a float mix format");
        client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 2_000_000, 0, format, None).unwrap();
        let render: IAudioRenderClient = client.GetService().unwrap();
        let session: ISimpleAudioVolume = client.GetService().unwrap();
        let frames = client.GetBufferSize().unwrap();
        session.SetMasterVolume(volume, std::ptr::null()).unwrap();
        let mut phase = 0f64;
        let mut fill = || {
            let free = frames - client.GetCurrentPadding().unwrap();
            if free == 0 {
                return;
            }
            let data = render.GetBuffer(free).unwrap() as *mut f32;
            for frame in std::slice::from_raw_parts_mut(data, free as usize * channels).chunks_mut(channels) {
                frame.fill((phase * std::f64::consts::TAU).sin() as f32 * AMPLITUDE);
                phase = (phase + 440.0 / f64::from(rate)).fract();
            }
            render.ReleaseBuffer(free, 0).unwrap();
        };
        fill();
        client.Start().unwrap();
        let start = Instant::now();
        while start.elapsed().as_secs_f64() < seconds {
            std::thread::sleep(Duration::from_millis(10));
            fill();
        }
        client.Stop().unwrap();
        session.SetMasterVolume(1.0, std::ptr::null()).unwrap();
        CoTaskMemFree(Some(format as *const _));
    }
}

/// The RMS level of a child playing at `volume`, captured through the
/// process loopback of its tree in 32-bit float; `during` runs once the
/// tone has settled, before the second that is measured.
fn measure(volume: f32, during: impl FnOnce()) -> f64 {
    let exe = std::env::current_exe().unwrap();
    let mut child: Child = Command::new(exe).args(["play", &volume.to_string(), "2.5"]).spawn().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    unsafe {
        let mut params = AUDIOCLIENT_ACTIVATION_PARAMS { ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, ..Default::default() };
        params.Anonymous.ProcessLoopbackParams = AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
            TargetProcessId: child.id(),
            ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
        };
        let blob = BLOB { cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32, pBlobData: (&mut params as *mut AUDIOCLIENT_ACTIVATION_PARAMS).cast() };
        let activate = std::mem::ManuallyDrop::new(PROPVARIANT {
            Anonymous: PROPVARIANT_0 {
                Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 { vt: VT_BLOB, wReserved1: 0, wReserved2: 0, wReserved3: 0, Anonymous: PROPVARIANT_0_0_0 { blob } }),
            },
        });
        let (tx, rx) = mpsc::channel();
        let handler: IActivateAudioInterfaceCompletionHandler = Handler(tx).into();
        let _op = ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, &IAudioClient::IID, Some(&*activate), &handler).unwrap();
        let loopback = rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
        let fmt = WAVEFORMATEX { wFormatTag: 3, nChannels: 2, nSamplesPerSec: 48000, nAvgBytesPerSec: 384000, nBlockAlign: 8, wBitsPerSample: 32, cbSize: 0 };
        loopback.Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, 10_000_000, 0, &fmt, None).unwrap();
        let capture: IAudioCaptureClient = loopback.GetService().unwrap();
        loopback.Start().unwrap();
        let read = |keep: bool, sum: &mut f64, count: &mut u64| loop {
            if capture.GetNextPacketSize().unwrap() == 0 {
                break;
            }
            let (mut data, mut n, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
            capture.GetBuffer(&mut data, &mut n, &mut flags, None, None).unwrap();
            if keep {
                *count += u64::from(n) * 2;
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 == 0 {
                    for &s in std::slice::from_raw_parts(data as *const f32, n as usize * 2) {
                        *sum += f64::from(s) * f64::from(s);
                    }
                }
            }
            capture.ReleaseBuffer(n).unwrap();
        };
        let (mut sum, mut count) = (0f64, 0u64);
        std::thread::sleep(Duration::from_millis(300));
        during();
        std::thread::sleep(Duration::from_millis(200));
        read(false, &mut sum, &mut count);
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(1) {
            std::thread::sleep(Duration::from_millis(10));
            read(true, &mut sum, &mut count);
        }
        loopback.Stop().unwrap();
        let _ = child.wait();
        20.0 * (sum / count.max(1) as f64).sqrt().max(1e-9).log10()
    }
}
