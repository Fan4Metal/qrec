//! The MP4 file: H.264 video from [`crate::venc`] and AAC audio, muxed
//! by the Media Foundation sink writer, which also runs the AAC encoder.
//!
//! Video frames arrive as NV12 textures on the device the DXGI device
//! manager was given; audio as 16-bit PCM. Timestamps are in 100 ns from
//! the start of the recording. The file is opened with the first encoded
//! frame, whose media type (with the H.264 parameter sets) describes the
//! video stream; audio that comes earlier waits for it.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, IMFDXGIDeviceManager, IMFMediaType, IMFSample, IMFSinkWriter, MF_API_VERSION,
    MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, MF_MT_ALL_SAMPLES_INDEPENDENT, MF_MT_AUDIO_AVG_BYTES_PER_SECOND,
    MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_BLOCK_ALIGNMENT, MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND,
    MF_MT_MAJOR_TYPE, MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_SUBTYPE, MF_SDK_VERSION, MF_SINK_WRITER_DISABLE_THROTTLING,
    MF_TRANSCODE_CONTAINERTYPE, MFAudioFormat_AAC, MFAudioFormat_PCM, MFCreateAttributes, MFCreateDXGIDeviceManager,
    MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFCreateSinkWriterFromURL, MFMediaType_Audio, MFSTARTUP_FULL,
    MFShutdown, MFStartup, MFTranscodeContainerType_MPEG4,
};
use windows::core::{PCWSTR, Result};

use crate::venc::VideoEncoder;
use crate::win;

#[derive(Clone, Copy, Debug)]
pub struct VideoConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Bits per second.
    pub bitrate: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct AudioConfig {
    /// Samples per second of the 16-bit stereo PCM written.
    pub sample_rate: u32,
}

/// Bytes per second of the AAC stream, one of the values Microsoft's
/// encoder offers (192 kbit/s).
const AAC_BYTES_PER_SECOND: u32 = 24000;

pub struct Encoder {
    video: Mutex<VideoEncoder>,
    mux: Mutex<Mux>,
    _manager: IMFDXGIDeviceManager,
    /// Name of the H.264 encoder in use, and whether it is a hardware one.
    pub encoder_name: String,
    pub hardware: bool,
}

/// The sink writer, once the first frame is encoded, and what waits for it.
struct Mux {
    path: PathBuf,
    audio: Option<AudioConfig>,
    writer: Option<Writer>,
    /// Audio written before the file was opened: samples, time, duration.
    pending_audio: Vec<(Vec<i16>, i64, i64)>,
}

struct Writer {
    writer: IMFSinkWriter,
    video_stream: u32,
    audio_stream: Option<u32>,
}

// Media Foundation objects are free-threaded; the writer and the encoder
// are used under their locks from the recording threads.
unsafe impl Send for Encoder {}
unsafe impl Sync for Encoder {}

impl Encoder {
    /// Prepares to write `path`. With `audio`, the file gets an AAC
    /// track for 16-bit stereo PCM at that sample rate.
    pub fn new(path: &Path, device: &ID3D11Device, video: VideoConfig, audio: Option<AudioConfig>) -> Result<Encoder> {
        unsafe { MFStartup(mf_version(), MFSTARTUP_FULL)? };
        let (mut token, mut manager) = (0u32, None);
        unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager)? };
        let manager = manager.unwrap();
        unsafe { manager.ResetDevice(device, token)? };
        let encoder = VideoEncoder::new(device, &manager, video)?;
        let (encoder_name, hardware) = (encoder.name.clone(), encoder.hardware);
        Ok(Encoder {
            video: Mutex::new(encoder),
            mux: Mutex::new(Mux { path: path.to_path_buf(), audio, writer: None, pending_audio: Vec::new() }),
            _manager: manager,
            encoder_name,
            hardware,
        })
    }

    /// Encodes and writes an NV12 frame shown from `time` for `duration`
    /// (100 ns units). `Ok(false)` when the encoder skipped it.
    pub fn write_video(&self, texture: &ID3D11Texture2D, time: i64, duration: i64) -> Result<bool> {
        let mut video = self.video.lock().unwrap_or_else(|e| e.into_inner());
        let mut samples = Vec::new();
        let taken = video.encode(texture, time, duration, &mut |s| {
            samples.push(s);
            Ok(())
        })?;
        if !samples.is_empty() {
            let mut mux = self.mux.lock().unwrap_or_else(|e| e.into_inner());
            for sample in samples {
                mux.write_video(&video, &sample)?;
            }
        }
        Ok(taken)
    }

    /// Writes interleaved 16-bit stereo samples starting at `time`.
    pub fn write_audio(&self, samples: &[i16], time: i64, duration: i64) -> Result<()> {
        let mut mux = self.mux.lock().unwrap_or_else(|e| e.into_inner());
        if mux.audio.is_none() {
            return Ok(());
        }
        match &mux.writer {
            Some(writer) => writer.write_audio(samples, time, duration),
            None => {
                mux.pending_audio.push((samples.to_vec(), time, duration));
                Ok(())
            }
        }
    }

    /// Completes the file: the encoder is drained and the index written.
    pub fn finish(&self) -> Result<()> {
        let mut video = self.video.lock().unwrap_or_else(|e| e.into_inner());
        let mut mux = self.mux.lock().unwrap_or_else(|e| e.into_inner());
        let mut samples = Vec::new();
        video.finish(&mut |s| {
            samples.push(s);
            Ok(())
        })?;
        for sample in samples {
            mux.write_video(&video, &sample)?;
        }
        match mux.writer.take() {
            Some(writer) => unsafe { writer.writer.Finalize() },
            None => Ok(()),
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
        }
    }
}

impl Mux {
    fn write_video(&mut self, encoder: &VideoEncoder, sample: &IMFSample) -> Result<()> {
        if self.writer.is_none() {
            let media_type = encoder.output_type()?;
            if unsafe { media_type.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) }.is_err()
                && let Some(header) = parameter_sets(sample)?
            {
                unsafe { media_type.SetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &header)? };
            }
            let writer = Writer::new(&self.path, &media_type, self.audio)?;
            for (samples, time, duration) in self.pending_audio.drain(..) {
                writer.write_audio(&samples, time, duration)?;
            }
            self.writer = Some(writer);
        }
        let writer = self.writer.as_ref().unwrap();
        unsafe { writer.writer.WriteSample(writer.video_stream, sample) }
    }
}

impl Writer {
    /// Creates the file with a video stream of `video_type` (H.264,
    /// passed through) and the AAC stream, and starts writing.
    fn new(path: &Path, video_type: &IMFMediaType, audio: Option<AudioConfig>) -> Result<Writer> {
        let mut attributes = None;
        unsafe { MFCreateAttributes(&mut attributes, 2)? };
        let attributes: IMFAttributes = attributes.unwrap();
        unsafe {
            attributes.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)?;
            // Both streams come in real time and compressed: nothing to
            // throttle, and a blocked write would hold up the other stream.
            attributes.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;
        }
        let url = win::wide(path);
        let writer = unsafe { MFCreateSinkWriterFromURL(PCWSTR(url.as_ptr()), None, Some(&attributes))? };
        let video_stream = unsafe { writer.AddStream(video_type)? };
        unsafe { writer.SetInputMediaType(video_stream, video_type, None)? };
        let audio_stream = match audio {
            Some(a) => Some(add_audio_stream(&writer, a)?),
            None => None,
        };
        unsafe { writer.BeginWriting()? };
        Ok(Writer { writer, video_stream, audio_stream })
    }

    fn write_audio(&self, samples: &[i16], time: i64, duration: i64) -> Result<()> {
        let Some(stream) = self.audio_stream else { return Ok(()) };
        let bytes = samples.len() * 2;
        unsafe {
            let buffer = MFCreateMemoryBuffer(bytes as u32)?;
            let mut data = std::ptr::null_mut();
            buffer.Lock(&mut data, None, None)?;
            std::ptr::copy_nonoverlapping(samples.as_ptr().cast::<u8>(), data, bytes);
            buffer.Unlock()?;
            buffer.SetCurrentLength(bytes as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(time)?;
            sample.SetSampleDuration(duration)?;
            self.writer.WriteSample(stream, &sample)
        }
    }
}

fn add_audio_stream(writer: &IMFSinkWriter, audio: AudioConfig) -> Result<u32> {
    let output = unsafe { MFCreateMediaType()? };
    unsafe {
        output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        output.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
        output.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        output.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, audio.sample_rate)?;
        output.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
        output.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, AAC_BYTES_PER_SECOND)?;
        // AAC Profile, Level 2: what players expect in an MP4.
        output.SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29)?;
    }
    let stream = unsafe { writer.AddStream(&output)? };
    let input = unsafe { MFCreateMediaType()? };
    unsafe {
        input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        input.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
        input.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        input.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, audio.sample_rate)?;
        input.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
        input.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 4)?;
        input.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, audio.sample_rate * 4)?;
        input.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
        writer.SetInputMediaType(stream, &input, None)?;
    }
    Ok(stream)
}

/// The SPS and PPS NAL units of an Annex B sample (start codes kept), as
/// `MF_MT_MPEG_SEQUENCE_HEADER` wants them; `None` when the sample has
/// none.
fn parameter_sets(sample: &IMFSample) -> Result<Option<Vec<u8>>> {
    let buffer = unsafe { sample.ConvertToContiguousBuffer()? };
    let (mut data, mut length) = (std::ptr::null_mut(), 0u32);
    unsafe { buffer.Lock(&mut data, None, Some(&mut length))? };
    let bytes = unsafe { std::slice::from_raw_parts(data, length as usize) }.to_vec();
    unsafe { buffer.Unlock()? };
    Ok(extract_parameter_sets(&bytes))
}

/// SPS (type 7) and PPS (type 8) units of an Annex B stream, with their
/// 4-byte start codes.
fn extract_parameter_sets(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 < bytes.len() {
        if bytes[i] == 0 && bytes[i + 1] == 0 && bytes[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut header = Vec::new();
    for (n, &start) in starts.iter().enumerate() {
        let mut end = starts.get(n + 1).map_or(bytes.len(), |&next| next - 3);
        while end > start && bytes[end - 1] == 0 {
            end -= 1;
        }
        let kind = bytes[start] & 0x1f;
        if kind == 7 || kind == 8 {
            header.extend_from_slice(&[0, 0, 0, 1]);
            header.extend_from_slice(&bytes[start..end]);
        }
    }
    (!header.is_empty()).then_some(header)
}

fn mf_version() -> u32 {
    (MF_SDK_VERSION << 16) | MF_API_VERSION
}

#[cfg(test)]
mod tests {
    use super::extract_parameter_sets;

    #[test]
    fn finds_sps_and_pps() {
        let stream = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3, 0, 0, 1, 0x65, 9, 9];
        assert_eq!(extract_parameter_sets(&stream), Some(vec![0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3]));
        assert_eq!(extract_parameter_sets(&[0, 0, 0, 1, 0x41, 5]), None);
    }
}
