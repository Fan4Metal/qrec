//! Process loopback (Windows 10 2004+): captures everything but this
//! process through the virtual loopback device, reading a second of audio.
use std::sync::mpsc;

use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0};
use windows::Win32::System::Com::*;
use windows::Win32::System::Threading::GetCurrentProcessId;
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

fn main() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let mut params = AUDIOCLIENT_ACTIVATION_PARAMS { ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, ..Default::default() };
        params.Anonymous.ProcessLoopbackParams = AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
            TargetProcessId: GetCurrentProcessId(),
            ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
        };
        let blob = BLOB { cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32, pBlobData: (&mut params as *mut AUDIOCLIENT_ACTIVATION_PARAMS).cast() };
        let activate = PROPVARIANT {
            Anonymous: PROPVARIANT_0 {
                Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 { vt: VT_BLOB, wReserved1: 0, wReserved2: 0, wReserved3: 0, Anonymous: PROPVARIANT_0_0_0 { blob } }),
            },
        };
        let (tx, rx) = mpsc::channel();
        let handler: IActivateAudioInterfaceCompletionHandler = Handler(tx).into();
        let op = ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, &IAudioClient::IID, Some(&activate), &handler);
        println!("ActivateAudioInterfaceAsync: {:?}", op.as_ref().map(|_| ()).map_err(|e| e.code()));
        let Ok(_op) = op else { return };
        let client = match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => {
                println!("activation failed: {:?} {}", e.code(), e.message());
                return;
            }
            Err(_) => {
                println!("activation timed out");
                return;
            }
        };
        let fmt = WAVEFORMATEX { wFormatTag: 1, nChannels: 2, nSamplesPerSec: 48000, nAvgBytesPerSec: 192000, nBlockAlign: 4, wBitsPerSample: 16, cbSize: 0 };
        let r = client.Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK, 2_000_000, 0, &fmt, None);
        println!("Initialize (process loopback, event): {:?}", r.as_ref().map_err(|e| e.code()));
        if r.is_err() {
            let r = client.Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, 2_000_000, 0, &fmt, None);
            println!("Initialize (process loopback, polling): {:?}", r.as_ref().map_err(|e| e.code()));
            if r.is_err() {
                return;
            }
        }
        let event = windows::Win32::System::Threading::CreateEventW(None, false, false, None).unwrap();
        let _ = client.SetEventHandle(event);
        let capture: IAudioCaptureClient = client.GetService().unwrap();
        client.Start().unwrap();
        let (mut packets, mut frames_total, mut nonzero, mut qpc_zero) = (0u32, 0u64, 0u64, 0u32);
        let start = std::time::Instant::now();
        while start.elapsed() < std::time::Duration::from_secs(2) {
            std::thread::sleep(std::time::Duration::from_millis(10));
            loop {
                let size = capture.GetNextPacketSize().unwrap();
                if size == 0 {
                    break;
                }
                let (mut data, mut frames, mut flags, mut qpc) = (std::ptr::null_mut(), 0u32, 0u32, 0u64);
                capture.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc)).unwrap();
                packets += 1;
                frames_total += frames as u64;
                if qpc == 0 {
                    qpc_zero += 1;
                }
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 == 0 && !data.is_null() {
                    let samples = std::slice::from_raw_parts(data as *const i16, frames as usize * 2);
                    nonzero += samples.iter().filter(|&&s| s != 0).count() as u64;
                }
                capture.ReleaseBuffer(frames).unwrap();
            }
        }
        client.Stop().unwrap();
        println!("2 s: {packets} packets, {frames_total} frames ({:.2} s), {nonzero} non-zero samples, {qpc_zero} packets without QPC", frames_total as f64 / 48000.0);
    }
}
