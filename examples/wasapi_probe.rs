//! Tries loopback capture on the default render endpoint in several ways.
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::*;
use windows::core::Interface;

fn main() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).unwrap();
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).unwrap();

        // A: plain PCM16 format with auto conversion.
        {
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None).unwrap();
            let fmt = WAVEFORMATEX { wFormatTag: 1, nChannels: 2, nSamplesPerSec: 48000, nAvgBytesPerSec: 192000, nBlockAlign: 4, wBitsPerSample: 16, cbSize: 0 };
            let flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
            let r = client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 10_000_000, 0, &fmt, None);
            println!("A pcm16+autoconvert loopback: {:?}", r.map_err(|e| e.code()));
            let r2 = client.Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, 10_000_000, 0, &fmt, None);
            println!("A' pcm16+autoconvert render: {:?}", r2.map_err(|e| e.code()));
        }
        // B: IAudioClient3 shared stream.
        {
            let client: IAudioClient3 = device.Activate(CLSCTX_ALL, None).unwrap();
            let format = client.GetMixFormat().unwrap();
            let (mut def, mut fund, mut min, mut max) = (0u32, 0u32, 0u32, 0u32);
            let p = client.GetSharedModeEnginePeriod(format, &mut def, &mut fund, &mut min, &mut max);
            println!("B periods: {:?} def {def} fund {fund} min {min} max {max}", p.map_err(|e| e.code()));
            let r = client.InitializeSharedAudioStream(AUDCLNT_STREAMFLAGS_LOOPBACK, def, format, None);
            println!("B client3 loopback: {:?}", r.map_err(|e| e.code()));
            CoTaskMemFree(Some(format as *const _));
        }
        // C: client properties first.
        {
            let client: IAudioClient2 = device.Activate(CLSCTX_ALL, None).unwrap();
            let props = AudioClientProperties { cbSize: std::mem::size_of::<AudioClientProperties>() as u32, bIsOffload: false.into(), eCategory: AudioCategory_Other, Options: AUDCLNT_STREAMOPTIONS_NONE };
            let p = client.SetClientProperties(&props);
            let format = client.GetMixFormat().unwrap();
            let r = client.Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, 10_000_000, 0, format, None);
            println!("C props {:?} then loopback: {:?}", p.map_err(|e| e.code()), r.map_err(|e| e.code()));
            CoTaskMemFree(Some(format as *const _));
        }
        // D: event callback + 0 duration, mix format.
        {
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None).unwrap();
            let format = client.GetMixFormat().unwrap();
            let r = client.Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK, 0, 0, format, None);
            println!("D event loopback: {:?}", r.map_err(|e| e.code()));
            let r = client.cast::<IAudioClient>().is_ok();
            println!("   (client alive {r})");
            CoTaskMemFree(Some(format as *const _));
        }
        // E: eMultimedia role and 2 s.
        {
            let device = enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia).unwrap();
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None).unwrap();
            let format = client.GetMixFormat().unwrap();
            let r = client.Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, 20_000_000, 0, format, None);
            println!("E multimedia 2s loopback: {:?}", r.map_err(|e| e.code()));
            CoTaskMemFree(Some(format as *const _));
        }
        // F: capture endpoint, normal capture (does capture work at all?).
        {
            if let Ok(device) = enumerator.GetDefaultAudioEndpoint(eCapture, eConsole) {
                let client: IAudioClient = device.Activate(CLSCTX_ALL, None).unwrap();
                let format = client.GetMixFormat().unwrap();
                let r = client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 10_000_000, 0, format, None);
                println!("F microphone capture: {:?}", r.map_err(|e| e.code()));
                CoTaskMemFree(Some(format as *const _));
            } else {
                println!("F no capture device");
            }
        }
    }
}
