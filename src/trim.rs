//! Cutting a recording without re-encoding: the H.264 and AAC samples
//! of the chosen stretch are copied from one MP4 into another by the
//! source reader and the sink writer, with their timestamps moved back.
//! A cut starts on a key frame (the stretch's first) and ends on any
//! frame. Also what the editor needs to choose the stretch: the frames
//! of a file, any one of them decoded for the preview, and the sound
//! decoded for playing it back.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};

use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFAttributes, IMFMediaType, IMFSample, IMFSinkWriter, IMFSourceReader, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_MAJOR_TYPE, MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_SUBTYPE, MF_PD_DURATION, MF_SINK_WRITER_DISABLE_THROTTLING, MF_SOURCE_READER_ANY_STREAM,
    MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, MF_SOURCE_READER_MEDIASOURCE, MF_SOURCE_READERF_ENDOFSTREAM, MF_TRANSCODE_CONTAINERTYPE,
    MFAudioFormat_AAC, MFAudioFormat_Float, MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND, MFCreateAttributes, MFCreateMediaType, MFCreateSinkWriterFromURL, MFCreateSourceReaderFromURL, MFMediaType_Audio,
    MFMediaType_Video, MFSampleExtension_CleanPoint, MFTranscodeContainerType_MPEG4, MFVideoFormat_H264,
    MFVideoFormat_RGB32,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::core::{Error, GUID, Interface, PCWSTR, Result};

use crate::encoder::Mf;
use crate::win;

/// One second in the 100 ns units of Media Foundation.
pub const SECOND: i64 = 10_000_000;

/// One video frame of a file: when it is shown and for how long (100 ns
/// units), and whether it is a key frame, which a cut can start on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    pub time: i64,
    pub duration: i64,
    pub key: bool,
}

/// What a file holds, as its headers say.
#[derive(Clone, Debug, Default)]
pub struct Info {
    pub width: u32,
    pub height: u32,
    /// Frames per second as declared: the step between frames until they
    /// are listed.
    pub fps: f64,
    pub duration: i64,
    pub audio: bool,
    /// The file has sound, but not AAC: it is neither played nor kept in
    /// a cut.
    pub other_audio: bool,
}

/// How a cut is getting on, shared with the thread that runs it.
#[derive(Default)]
pub struct Progress {
    /// Thousandths of the stretch written.
    pub done: AtomicU32,
    pub cancel: AtomicBool,
}

/// What a cut came to.
#[derive(Clone, Copy, Debug)]
pub struct Cut {
    /// Where it starts in the source: the key frame at or before the
    /// start asked for.
    pub start: i64,
    pub frames: u32,
}

/// A file open for reading: the reader and the streams it has.
struct Source {
    reader: IMFSourceReader,
    video: u32,
    audio: Option<u32>,
    /// A sound stream in another format than AAC.
    other_audio: bool,
}

impl Source {
    /// Opens `path` with its compressed streams, or with the video
    /// decoded to 32-bit BGRA when `decode`; only the video stream is
    /// selected (`select_audio` adds the audio).
    fn open(path: &Path, decode: bool) -> Result<Source> {
        let mut attributes = None;
        unsafe { MFCreateAttributes(&mut attributes, 1)? };
        let attributes: IMFAttributes = attributes.unwrap();
        if decode {
            unsafe { attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)? };
        }
        let url = win::wide(path);
        let reader = unsafe { MFCreateSourceReaderFromURL(PCWSTR(url.as_ptr()), Some(&attributes))? };
        let (mut video, mut audio, mut other_audio) = (None, None, false);
        for stream in 0..8u32 {
            let Ok(native) = (unsafe { reader.GetNativeMediaType(stream, 0) }) else {
                break;
            };
            let major = unsafe { native.GetGUID(&MF_MT_MAJOR_TYPE)? };
            let subtype = unsafe { native.GetGUID(&MF_MT_SUBTYPE)? };
            if major == MFMediaType_Video && subtype == MFVideoFormat_H264 && video.is_none() {
                video = Some(stream);
            } else if major == MFMediaType_Audio && subtype == MFAudioFormat_AAC && audio.is_none() {
                audio = Some(stream);
            } else if major == MFMediaType_Audio {
                other_audio = true;
            }
            unsafe { reader.SetStreamSelection(stream, false)? };
        }
        let video = video.ok_or_else(|| Error::new(windows::Win32::Foundation::E_INVALIDARG, "no H.264 video stream"))?;
        unsafe { reader.SetStreamSelection(video, true)? };
        if decode {
            let rgb = unsafe { MFCreateMediaType()? };
            unsafe {
                rgb.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
                rgb.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
                reader.SetCurrentMediaType(video, None, &rgb)?;
            }
        }
        let other_audio = other_audio && audio.is_none();
        Ok(Source { reader, video, audio, other_audio })
    }

    fn select_audio(&self) -> Result<()> {
        match self.audio {
            Some(stream) => unsafe { self.reader.SetStreamSelection(stream, true) },
            None => Ok(()),
        }
    }

    fn native_video_type(&self) -> Result<IMFMediaType> {
        unsafe { self.reader.GetNativeMediaType(self.video, 0) }
    }

    fn info(&self) -> Result<Info> {
        let native = self.native_video_type()?;
        let (width, height) = unpack(unsafe { native.GetUINT64(&MF_MT_FRAME_SIZE)? });
        let fps = match unsafe { native.GetUINT64(&MF_MT_FRAME_RATE) } {
            Ok(rate) => {
                let (n, d) = unpack(rate);
                if d == 0 { 30.0 } else { n as f64 / d as f64 }
            }
            Err(_) => 30.0,
        };
        // A fragmented file has no length in its headers: 0, and the
        // trimming window takes it from the frames.
        let duration = unsafe { self.reader.GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION) }
            .ok()
            .and_then(|d| u64::try_from(&d).ok())
            .unwrap_or(0) as i64;
        Ok(Info {
            width,
            height,
            fps,
            duration,
            audio: self.audio.is_some(),
            other_audio: self.other_audio,
        })
    }

    fn seek(&self, time: i64) -> Result<()> {
        let position = PROPVARIANT::from(time.max(0));
        unsafe { self.reader.SetCurrentPosition(&GUID::zeroed(), &position) }
    }

    /// The next sample of `stream`: its stream, or `None` at the end.
    fn read(&self, stream: u32) -> Result<Option<(u32, IMFSample)>> {
        let (stream, sample) = self.read_from(stream)?;
        Ok(sample.map(|s| (stream, s)))
    }

    /// The next sample of `stream`, or of whichever selected stream has
    /// one with `MF_SOURCE_READER_ANY_STREAM`: the stream it came from,
    /// and `None` when that stream has reached its end (the others may
    /// still have samples).
    fn read_from(&self, stream: u32) -> Result<(u32, Option<IMFSample>)> {
        loop {
            let (mut actual, mut flags, mut sample) = (0u32, 0u32, None);
            unsafe {
                self.reader
                    .ReadSample(stream, 0, Some(&mut actual), Some(&mut flags), None, Some(&mut sample))?
            };
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                return Ok((actual, None));
            }
            if let Some(sample) = sample {
                return Ok((actual, Some(sample)));
            }
        }
    }
}

/// What `path` holds.
pub fn info(path: &Path) -> Result<Info> {
    let _mf = Mf::start()?;
    Source::open(path, false)?.info()
}

/// Every video frame of `path`, in order: from the file's index when it
/// can be read (`mp4`), else by reading the samples; `cancel` stops the
/// reading (what was listed so far comes back).
pub fn frames(path: &Path, cancel: &AtomicBool) -> Result<Vec<Frame>> {
    match crate::mp4::video_frames(path) {
        Some(frames) => Ok(frames),
        None => {
            log::debug!("frames of {}: not from the index, read", path.display());
            frames_by_reading(path, cancel)
        }
    }
}

/// Every video frame of `path` as the source reader delivers it, which
/// reads the whole file.
pub fn frames_by_reading(path: &Path, cancel: &AtomicBool) -> Result<Vec<Frame>> {
    let _mf = Mf::start()?;
    let source = Source::open(path, false)?;
    let mut frames = Vec::new();
    while let Some((_, sample)) = source.read(source.video)? {
        frames.push(Frame {
            time: unsafe { sample.GetSampleTime()? },
            duration: sample_duration(&sample),
            key: is_key(&sample),
        });
        if cancel.load(Relaxed) {
            break;
        }
    }
    Ok(frames)
}

/// A decoded frame: its time, its size and its pixels as straight RGBA.
pub struct Picture {
    pub time: i64,
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

/// A file open for decoding one frame at a time.
pub struct Preview {
    // Released before Media Foundation is shut down.
    source: Source,
    _mf: Mf,
    pub info: Info,
    /// Time of the next frame the reader delivers without a seek, when
    /// known.
    next: Option<i64>,
    /// Times of the key frames, once listed: a frame before the next key
    /// frame is reached by reading on, not by seeking.
    keys: Vec<i64>,
}

impl Preview {
    pub fn open(path: &Path) -> Result<Preview> {
        let mf = Mf::start()?;
        let source = Source::open(path, true)?;
        let info = source.info()?;
        Ok(Preview {
            _mf: mf,
            source,
            info,
            next: None,
            keys: Vec::new(),
        })
    }

    pub fn set_keys(&mut self, keys: Vec<i64>) {
        self.keys = keys;
    }

    /// The frame shown at `time`, scaled down by a whole factor so that
    /// neither side exceeds `max_side` pixels; `None` past the last frame.
    pub fn frame(&mut self, time: i64, max_side: usize) -> Result<Option<Picture>> {
        let forward = self.next.is_some_and(|next| {
            time >= next
                && if self.keys.is_empty() {
                    time - next < SECOND
                } else {
                    !self.keys.iter().any(|&k| next < k && k <= time)
                }
        });
        if !forward {
            self.source.seek(time)?;
            self.next = None;
        }
        loop {
            let Some((_, sample)) = self.source.read(self.source.video)? else {
                self.next = None;
                return Ok(None);
            };
            let shown = unsafe { sample.GetSampleTime()? };
            let duration = sample_duration(&sample);
            self.next = Some(shown + duration);
            if shown + duration > time || shown >= time {
                let (width, height) = (self.info.width as usize, self.info.height as usize);
                let stride = unsafe {
                    self.source
                        .reader
                        .GetCurrentMediaType(self.source.video)?
                        .GetUINT32(&MF_MT_DEFAULT_STRIDE)
                }
                .map_or(width as i32 * 4, |s| s as i32);
                let factor = width.max(height).div_ceil(max_side.max(1)).max(1);
                let rgba = pixels(&sample, width, height, stride, factor)?;
                return Ok(Some(Picture {
                    time: shown,
                    width: width / factor,
                    height: height / factor,
                    rgba,
                }));
            }
        }
    }
}

/// The sound of a file decoded to interleaved 32-bit float samples, in
/// the order of the file, for playing it back.
pub struct Sound {
    // Released before Media Foundation is shut down.
    source: Source,
    _mf: Mf,
    stream: u32,
    pub rate: u32,
    pub channels: u16,
}

impl Sound {
    /// `None` when the file has no AAC track.
    pub fn open(path: &Path) -> Result<Option<Sound>> {
        let mf = Mf::start()?;
        let source = Source::open(path, false)?;
        let Some(stream) = source.audio else { return Ok(None) };
        unsafe {
            source.reader.SetStreamSelection(source.video, false)?;
            source.reader.SetStreamSelection(stream, true)?;
            let float = MFCreateMediaType()?;
            float.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            float.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_Float)?;
            source.reader.SetCurrentMediaType(stream, None, &float)?;
        }
        let current = unsafe { source.reader.GetCurrentMediaType(stream)? };
        let rate = unsafe { current.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND)? };
        let channels = unsafe { current.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS)? } as u16;
        Ok(Some(Sound { source, _mf: mf, stream, rate, channels }))
    }

    /// The next samples come from `time` on (or a little before it: from
    /// the start of the block that holds it).
    pub fn seek(&self, time: i64) -> Result<()> {
        self.source.seek(time)
    }

    /// The next block: the time of its first frame and its samples;
    /// `None` at the end.
    pub fn next(&self) -> Result<Option<(i64, Vec<f32>)>> {
        let Some((_, sample)) = self.source.read(self.stream)? else { return Ok(None) };
        let time = unsafe { sample.GetSampleTime()? };
        let buffer = unsafe { sample.ConvertToContiguousBuffer()? };
        let (mut data, mut length) = (std::ptr::null_mut(), 0u32);
        unsafe { buffer.Lock(&mut data, None, Some(&mut length))? };
        let bytes = unsafe { std::slice::from_raw_parts(data, length as usize) };
        let floats = bytes.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
        unsafe { buffer.Unlock()? };
        Ok(Some((time, floats)))
    }
}

/// The pixels of a BGRA sample as straight RGBA, averaged over squares
/// of `factor` pixels.
fn pixels(sample: &IMFSample, width: usize, height: usize, stride: i32, factor: usize) -> Result<Vec<u8>> {
    let buffer = unsafe { sample.ConvertToContiguousBuffer()? };
    // A 2-D buffer knows its own pitch (negative for a bottom-up image);
    // a plain one has the pitch of the media type.
    let (data, pitch, locked_2d) = match buffer.cast::<IMF2DBuffer>() {
        Ok(planar) => {
            let (mut scanline0, mut pitch) = (std::ptr::null_mut(), 0i32);
            unsafe { planar.Lock2D(&mut scanline0, &mut pitch)? };
            (scanline0.cast_const(), pitch as isize, Some(planar))
        }
        Err(_) => {
            let (mut data, mut length) = (std::ptr::null_mut(), 0u32);
            unsafe { buffer.Lock(&mut data, None, Some(&mut length))? };
            // A negative stride in the type means the first row is the last
            // in memory.
            let data = if stride < 0 {
                unsafe { data.add((height - 1) * (-stride) as usize) }
            } else {
                data
            };
            (data.cast_const(), stride as isize, None)
        }
    };
    let (w, h) = (width / factor, height / factor);
    let mut rgba = vec![0u8; w * h * 4];
    let n = (factor * factor) as u32;
    let row = |y: usize| unsafe { std::slice::from_raw_parts(data.offset(pitch * y as isize), width * 4) };
    for y in 0..h {
        for x in 0..w {
            let (mut r, mut g, mut b) = (0u32, 0u32, 0u32);
            for dy in 0..factor {
                let src = row(y * factor + dy);
                for dx in 0..factor {
                    let p = &src[(x * factor + dx) * 4..][..4];
                    b += p[0] as u32;
                    g += p[1] as u32;
                    r += p[2] as u32;
                }
            }
            let out = &mut rgba[(y * w + x) * 4..][..4];
            out[0] = (r / n) as u8;
            out[1] = (g / n) as u8;
            out[2] = (b / n) as u8;
            out[3] = 255;
        }
    }
    unsafe {
        match locked_2d {
            Some(planar) => planar.Unlock2D()?,
            None => buffer.Unlock()?,
        }
    }
    Ok(rgba)
}

/// Copies the stretch from `start` to `end` (100 ns units; the end
/// exclusive) of `src` into `dst`, starting on the key frame at or before
/// `start` (the one the seek lands on), with the sound of the same
/// stretch.
pub fn cut(src: &Path, dst: &Path, start: i64, end: i64, progress: &Progress) -> Result<Cut> {
    let result = copy_stretch(src, dst, start, end, progress);
    if result.is_err() {
        // Cancelled, or failed part-way: nothing of the file is kept.
        let _ = std::fs::remove_file(dst);
    }
    result
}

fn copy_stretch(src: &Path, dst: &Path, start: i64, end: i64, progress: &Progress) -> Result<Cut> {
    let _mf = Mf::start()?;
    let source = Source::open(src, false)?;
    let video_type = source.native_video_type()?;
    let audio_type = match source.audio {
        Some(stream) => Some(unsafe { source.reader.GetNativeMediaType(stream, 0)? }),
        None => None,
    };
    // The picture lands on the key frame at or before `start`, the sound
    // on `start` itself: both are sought to that key frame, or the sound
    // between it and `start` would be missing.
    source.seek(start)?;
    let key = loop {
        match source.read(source.video)? {
            Some((_, sample)) if is_key(&sample) => break Some(unsafe { sample.GetSampleTime()? }),
            Some(_) => {}
            None => break None,
        }
    };
    source.select_audio()?;
    source.seek(key.unwrap_or(start).min(start))?;

    let mut writer: Option<Writer> = None;
    // Where the copy starts: the key frame the seek landed on.
    let mut origin: Option<i64> = None;
    // Sound heard before the first frame is known to be kept waits for it.
    let mut pending_audio: Vec<IMFSample> = Vec::new();
    let mut frames = 0u32;
    let (mut video_done, mut audio_done) = (false, source.audio.is_none());
    while !(video_done && audio_done) {
        if progress.cancel.load(Relaxed) {
            return Err(Error::new(windows::Win32::Foundation::E_ABORT, "cancelled"));
        }
        // The end of one stream (the sound shorter than the picture in a
        // file from elsewhere) is not the end of the other.
        let (stream, sample) = source.read_from(MF_SOURCE_READER_ANY_STREAM.0 as u32)?;
        let Some(sample) = sample else {
            if stream == source.video {
                video_done = true;
            } else if Some(stream) == source.audio {
                audio_done = true;
            }
            unsafe { source.reader.SetStreamSelection(stream, false)? };
            continue;
        };
        let time = unsafe { sample.GetSampleTime()? };
        if stream == source.video {
            if time >= end {
                if !video_done {
                    video_done = true;
                    unsafe { source.reader.SetStreamSelection(stream, false)? };
                }
                continue;
            }
            // The stretch starts on the first key frame delivered.
            if origin.is_none() && is_key(&sample) {
                origin = Some(time);
            }
            let Some(origin) = origin else { continue };
            let w = match &mut writer {
                Some(w) => w,
                None => {
                    let video_type = with_sequence_header(&video_type, &sample)?;
                    let w = writer.insert(Writer::new(dst, &video_type, audio_type.as_ref())?);
                    for pending in pending_audio.drain(..) {
                        // From the origin on, as the sound heard later is.
                        if unsafe { pending.GetSampleTime()? } >= origin {
                            w.write(w.audio, &pending, origin)?;
                        }
                    }
                    w
                }
            };
            w.write(Some(w.video), &sample, origin)?;
            frames += 1;
            let done = ((time - origin) as f64 / (end - origin).max(1) as f64 * 1000.0) as u32;
            progress.done.store(done.min(1000), Relaxed);
        } else if Some(stream) == source.audio {
            if time >= end {
                if !audio_done {
                    audio_done = true;
                    unsafe { source.reader.SetStreamSelection(stream, false)? };
                }
                continue;
            }
            match (&writer, origin) {
                (Some(w), Some(origin)) => {
                    if time >= origin {
                        w.write(w.audio, &sample, origin)?;
                    }
                }
                _ => pending_audio.push(sample),
            }
        }
    }
    let Some(origin) = origin else {
        return Err(Error::new(windows::Win32::Foundation::E_FAIL, "no key frame in the stretch"));
    };
    if let Some(w) = writer {
        unsafe { w.writer.Finalize()? };
    }
    progress.done.store(1000, Relaxed);
    Ok(Cut { start: origin, frames })
}

/// The MP4 being written: the two streams passed through.
struct Writer {
    writer: IMFSinkWriter,
    video: u32,
    audio: Option<u32>,
}

impl Writer {
    fn new(path: &Path, video_type: &IMFMediaType, audio_type: Option<&IMFMediaType>) -> Result<Writer> {
        let mut attributes = None;
        unsafe { MFCreateAttributes(&mut attributes, 2)? };
        let attributes: IMFAttributes = attributes.unwrap();
        unsafe {
            attributes.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)?;
            attributes.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;
        }
        let url = win::wide(path);
        let writer = unsafe { MFCreateSinkWriterFromURL(PCWSTR(url.as_ptr()), None, Some(&attributes))? };
        let video = unsafe { writer.AddStream(video_type)? };
        unsafe { writer.SetInputMediaType(video, video_type, None)? };
        let audio = match audio_type {
            Some(t) => {
                let stream = unsafe { writer.AddStream(t)? };
                unsafe { writer.SetInputMediaType(stream, t, None)? };
                Some(stream)
            }
            None => None,
        };
        unsafe { writer.BeginWriting()? };
        Ok(Writer { writer, video, audio })
    }

    /// Writes `sample` to `stream` with its time counted from `origin`.
    fn write(&self, stream: Option<u32>, sample: &IMFSample, origin: i64) -> Result<()> {
        let Some(stream) = stream else { return Ok(()) };
        unsafe {
            let time = sample.GetSampleTime()?;
            sample.SetSampleTime((time - origin).max(0))?;
            self.writer.WriteSample(stream, sample)
        }
    }
}

/// `video_type` with the H.264 parameter sets, taken from `sample` (a
/// key frame) when the file's type has none.
fn with_sequence_header(video_type: &IMFMediaType, sample: &IMFSample) -> Result<IMFMediaType> {
    if unsafe { video_type.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) }.is_ok() {
        return Ok(video_type.clone());
    }
    let copy = unsafe { MFCreateMediaType()? };
    unsafe { video_type.CopyAllItems(&copy)? };
    if let Some(header) = crate::encoder::parameter_sets(sample)? {
        unsafe { copy.SetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &header)? };
    }
    Ok(copy)
}

fn is_key(sample: &IMFSample) -> bool {
    unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }.is_ok_and(|v| v != 0)
}

fn sample_duration(sample: &IMFSample) -> i64 {
    unsafe { sample.GetSampleDuration() }.unwrap_or(0)
}

fn unpack(packed: u64) -> (u32, u32) {
    ((packed >> 32) as u32, packed as u32)
}
