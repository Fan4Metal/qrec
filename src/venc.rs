//! The H.264 encoder, driven directly as a Media Foundation transform:
//! the hardware one of the graphics adapter when it has one (NVIDIA,
//! Intel and AMD all ship one), otherwise Microsoft's software encoder.
//!
//! The sink writer could drive the encoder itself, but then it queues
//! about two seconds of *input* frames to interleave the streams, and
//! each queued frame is a texture in video memory. Encoding here, the
//! textures are released as soon as the encoder has consumed them and
//! the sink writer only queues compressed samples.
//!
//! Hardware encoders are asynchronous transforms: they ask for input and
//! announce output through events, which [`VideoEncoder::encode`] pumps.
//! The software encoder is synchronous and takes its frames from system
//! memory, so they are read back from the GPU for it.

use std::mem::ManuallyDrop;

use windows::Win32::Foundation::E_NOTIMPL;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, ID3D11Device,
    ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFActivate, IMFAttributes, IMFDXGIDeviceManager, IMFMediaEventGenerator, IMFMediaType, IMFSample,
    IMFTransform, MF_E_NO_EVENTS_AVAILABLE, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE,
    MF_EVENT_FLAG_NO_WAIT, MF_MT_ALL_SAMPLES_INDEPENDENT, MF_MT_AVG_BITRATE, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE,
    MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MAX_KEYFRAME_SPACING, MF_MT_MPEG2_PROFILE,
    MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_SA_D3D11_AWARE, MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK,
    MFCreateDXGISurfaceBuffer, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video,
    MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG, MFT_ENUM_FLAG_ASYNCMFT, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_ENUM_FLAG_SYNCMFT, MFT_ENUM_HARDWARE_URL_Attribute, MFT_FRIENDLY_NAME_Attribute, MFT_MESSAGE_COMMAND_DRAIN,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_END_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MFTEnumEx, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFVideoInterlace_Progressive, METransformDrainComplete, METransformHaveOutput, METransformNeedInput,
    eAVEncH264VProfile_High,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::core::{GUID, Interface, PWSTR, Result};

use crate::encoder::VideoConfig;

pub struct VideoEncoder {
    transform: IMFTransform,
    events: Option<IMFMediaEventGenerator>,
    input_id: u32,
    output_id: u32,
    /// Inputs the asynchronous encoder has asked for and not received.
    wanted: u32,
    /// The encoder allocates its own output samples.
    provides_samples: bool,
    output_size: u32,
    /// For an encoder without Direct3D: the frame read back to memory.
    staging: Option<(ID3D11DeviceContext, ID3D11Texture2D)>,
    width: u32,
    height: u32,
    pub name: String,
    pub hardware: bool,
}

// Media Foundation transforms are free-threaded; the encoder is used
// from the video thread only.
unsafe impl Send for VideoEncoder {}

impl VideoEncoder {
    /// Opens the best encoder for `video`: a hardware one on the adapter
    /// of `device`, else the software one.
    pub fn new(device: &ID3D11Device, manager: &IMFDXGIDeviceManager, video: VideoConfig) -> Result<VideoEncoder> {
        let mut last_error = None;
        for (flags, hardware) in [(MFT_ENUM_FLAG_HARDWARE, true), (MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_ASYNCMFT, false)] {
            for activate in enumerate(flags | MFT_ENUM_FLAG_SORTANDFILTER)? {
                let name = allocated_string(&activate, &MFT_FRIENDLY_NAME_Attribute).unwrap_or_else(|| "H.264 encoder".into());
                match Self::open(&activate, device, manager, video, name.clone(), hardware) {
                    Ok(encoder) => return Ok(encoder),
                    Err(e) => {
                        log::warn!("encoder {name} refused: {}", crate::win::describe(&e));
                        last_error = Some(e);
                    }
                }
            }
        }
        Err(last_error.unwrap_or_else(|| windows::core::Error::new(E_NOTIMPL, "no H.264 encoder")))
    }

    fn open(
        activate: &IMFActivate,
        device: &ID3D11Device,
        manager: &IMFDXGIDeviceManager,
        video: VideoConfig,
        name: String,
        hardware: bool,
    ) -> Result<VideoEncoder> {
        let transform: IMFTransform = unsafe { activate.ActivateObject()? };
        let attributes = unsafe { transform.GetAttributes() }.ok();
        let flag = |key: &GUID| attributes.as_ref().and_then(|a| unsafe { a.GetUINT32(key) }.ok()).unwrap_or(0) == 1;
        let asynchronous = flag(&MF_TRANSFORM_ASYNC);
        if asynchronous && let Some(a) = &attributes {
            unsafe { a.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)? };
        }
        let hardware = hardware || allocated_string(activate, &MFT_ENUM_HARDWARE_URL_Attribute).is_some();
        let d3d_aware = flag(&MF_SA_D3D11_AWARE);
        if d3d_aware {
            unsafe { transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)? };
        }
        let (mut input_ids, mut output_ids) = ([0u32], [0u32]);
        match unsafe { transform.GetStreamIDs(&mut input_ids, &mut output_ids) } {
            Ok(()) => {}
            Err(e) if e.code() == E_NOTIMPL => {}
            Err(e) => return Err(e),
        }
        let (input_id, output_id) = (input_ids[0], output_ids[0]);

        let output = unsafe { MFCreateMediaType()? };
        unsafe {
            output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            output.SetUINT32(&MF_MT_AVG_BITRATE, video.bitrate)?;
            output.SetUINT64(&MF_MT_FRAME_SIZE, pack(video.width, video.height))?;
            output.SetUINT64(&MF_MT_FRAME_RATE, pack(video.fps, 1))?;
            output.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            output.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            output.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
            // A key frame every two seconds: seeking stays quick.
            output.SetUINT32(&MF_MT_MAX_KEYFRAME_SPACING, video.fps * 2)?;
            transform.SetOutputType(output_id, &output, 0)?;
        }
        let input = unsafe { MFCreateMediaType()? };
        unsafe {
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            input.SetUINT64(&MF_MT_FRAME_SIZE, pack(video.width, video.height))?;
            input.SetUINT64(&MF_MT_FRAME_RATE, pack(video.fps, 1))?;
            input.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            input.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
            input.SetUINT32(&MF_MT_DEFAULT_STRIDE, video.width)?;
            transform.SetInputType(input_id, &input, 0)?;
        }
        let info = unsafe { transform.GetOutputStreamInfo(output_id)? };
        let provides_samples = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
        let staging = if d3d_aware { None } else { Some((unsafe { device.GetImmediateContext()? }, staging_texture(device, video)?)) };
        unsafe {
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }
        let events = if asynchronous { Some(transform.cast::<IMFMediaEventGenerator>()?) } else { None };
        log::debug!("H.264 encoder: {name} (hardware {hardware}, asynchronous {asynchronous}, Direct3D {d3d_aware})");
        Ok(VideoEncoder {
            transform,
            events,
            input_id,
            output_id,
            wanted: 0,
            provides_samples,
            output_size: info.cbSize,
            staging,
            width: video.width,
            height: video.height,
            name,
            hardware,
        })
    }

    /// The H.264 type the encoder produces, for the file's video stream.
    pub fn output_type(&self) -> Result<IMFMediaType> {
        unsafe { self.transform.GetOutputCurrentType(self.output_id) }
    }

    /// Encodes the NV12 texture shown from `time` for `duration` (100 ns
    /// units), handing every compressed sample ready so far to `out`.
    /// `Ok(false)` when the encoder would not take the frame in time and
    /// it was skipped.
    pub fn encode(&mut self, texture: &ID3D11Texture2D, time: i64, duration: i64, out: &mut dyn FnMut(IMFSample) -> Result<()>) -> Result<bool> {
        let sample = self.input_sample(texture, time, duration)?;
        if self.events.is_some() {
            // Wait a little for the encoder to ask for input, up to half
            // a frame; a frame it cannot take is skipped.
            let deadline = std::time::Instant::now() + std::time::Duration::from_nanos(duration as u64 * 50);
            loop {
                self.pump(out)?;
                if self.wanted > 0 {
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    return Ok(false);
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            unsafe { self.transform.ProcessInput(self.input_id, &sample, 0)? };
            self.wanted -= 1;
            self.pump(out)?;
        } else {
            unsafe { self.transform.ProcessInput(self.input_id, &sample, 0)? };
            self.drain_output(out)?;
        }
        Ok(true)
    }

    /// Takes the frames still inside the encoder out; nothing is encoded
    /// afterwards.
    pub fn finish(&mut self, out: &mut dyn FnMut(IMFSample) -> Result<()>) -> Result<()> {
        unsafe {
            self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)?;
            self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?;
        }
        if let Some(events) = self.events.clone() {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                    Ok(event) => {
                        let kind = unsafe { event.GetType()? };
                        if kind == METransformDrainComplete.0 as u32 {
                            break;
                        }
                        self.handle(kind, out)?;
                    }
                    Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                        if std::time::Instant::now() >= deadline {
                            log::warn!("the encoder did not finish draining");
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    Err(e) => return Err(e),
                }
            }
        } else {
            self.drain_output(out)?;
        }
        unsafe { self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0)? };
        Ok(())
    }

    /// Handles every event the asynchronous encoder has queued.
    fn pump(&mut self, out: &mut dyn FnMut(IMFSample) -> Result<()>) -> Result<()> {
        let Some(events) = self.events.clone() else { return Ok(()) };
        loop {
            match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => {
                    let kind = unsafe { event.GetType()? };
                    self.handle(kind, out)?;
                }
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    }

    fn handle(&mut self, kind: u32, out: &mut dyn FnMut(IMFSample) -> Result<()>) -> Result<()> {
        if kind == METransformNeedInput.0 as u32 {
            self.wanted += 1;
        } else if kind == METransformHaveOutput.0 as u32 {
            self.output_one(out)?;
        }
        Ok(())
    }

    /// Output of a synchronous encoder until it wants more input.
    fn drain_output(&mut self, out: &mut dyn FnMut(IMFSample) -> Result<()>) -> Result<()> {
        while self.output_one(out)? {}
        Ok(())
    }

    /// One call of `ProcessOutput`: `Ok(false)` when the encoder has
    /// nothing ready.
    fn output_one(&mut self, out: &mut dyn FnMut(IMFSample) -> Result<()>) -> Result<bool> {
        let sample = if self.provides_samples {
            None
        } else {
            let sample = unsafe { MFCreateSample()? };
            unsafe { sample.AddBuffer(&MFCreateMemoryBuffer(self.output_size.max(1 << 16))?)? };
            Some(sample)
        };
        let mut buffer = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: self.output_id,
            pSample: ManuallyDrop::new(sample),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        };
        let mut status = 0u32;
        let result = unsafe { self.transform.ProcessOutput(0, std::slice::from_mut(&mut buffer), &mut status) };
        let sample = unsafe { ManuallyDrop::take(&mut buffer.pSample) };
        unsafe { ManuallyDrop::drop(&mut buffer.pEvents) };
        match result {
            Ok(()) => {
                if let Some(sample) = sample {
                    out(sample)?;
                }
                Ok(true)
            }
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(false),
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                // The encoder changed its output type (new parameters).
                let new_type = unsafe { self.transform.GetOutputAvailableType(self.output_id, 0)? };
                unsafe { self.transform.SetOutputType(self.output_id, &new_type, 0)? };
                Ok(true)
            }
            Err(e) => Err(e),
        }
    }

    /// The frame as the encoder takes it: the texture itself, or its
    /// pixels read back for an encoder without Direct3D.
    fn input_sample(&mut self, texture: &ID3D11Texture2D, time: i64, duration: i64) -> Result<IMFSample> {
        let sample = unsafe { MFCreateSample()? };
        let buffer = match &self.staging {
            None => {
                let buffer = unsafe { MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false)? };
                let length = unsafe { buffer.cast::<IMF2DBuffer>()?.GetContiguousLength()? };
                unsafe { buffer.SetCurrentLength(length)? };
                buffer
            }
            Some((context, staging)) => {
                let (width, height) = (self.width as usize, self.height as usize);
                let size = width * height * 3 / 2;
                let buffer = unsafe { MFCreateMemoryBuffer(size as u32)? };
                unsafe {
                    context.CopyResource(staging, texture);
                    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                    context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
                    let mut data = std::ptr::null_mut();
                    buffer.Lock(&mut data, None, None)?;
                    let pitch = mapped.RowPitch as usize;
                    let src = mapped.pData as *const u8;
                    // Y rows, then the interleaved UV rows, each `width` bytes.
                    for row in 0..height + height / 2 {
                        std::ptr::copy_nonoverlapping(src.add(row * pitch), data.add(row * width), width);
                    }
                    buffer.Unlock()?;
                    context.Unmap(staging, 0);
                    buffer.SetCurrentLength(size as u32)?;
                }
                buffer
            }
        };
        unsafe {
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(time)?;
            sample.SetSampleDuration(duration)?;
        }
        Ok(sample)
    }
}

/// A CPU-readable NV12 texture of the frame's size.
fn staging_texture(device: &ID3D11Device, video: VideoConfig) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: video.width,
        Height: video.height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture))? };
    Ok(texture.unwrap())
}

/// The H.264 encoders matching `flags`, best first.
fn enumerate(flags: MFT_ENUM_FLAG) -> Result<Vec<IMFActivate>> {
    let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
    let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
    let (mut list, mut count) = (std::ptr::null_mut(), 0u32);
    unsafe { MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, flags, Some(&input), Some(&output), &mut list, &mut count)? };
    let mut activates = Vec::with_capacity(count as usize);
    if !list.is_null() {
        for i in 0..count as usize {
            if let Some(activate) = unsafe { std::ptr::read(list.add(i)) } {
                activates.push(activate);
            }
        }
        unsafe { CoTaskMemFree(Some(list as *const _)) };
    }
    Ok(activates)
}

fn allocated_string(attributes: &IMFActivate, key: &GUID) -> Option<String> {
    let attributes: IMFAttributes = attributes.cast().ok()?;
    let mut text = PWSTR::null();
    let mut length = 0u32;
    unsafe {
        attributes.GetAllocatedString(key, &mut text, &mut length).ok()?;
        let value = text.to_string().ok();
        CoTaskMemFree(Some(text.0 as *const _));
        value
    }
}

/// Two 32-bit values in one 64-bit attribute, as `MFSetAttributeSize`
/// and `MFSetAttributeRatio` pack them.
pub fn pack(high: u32, low: u32) -> u64 {
    (u64::from(high) << 32) | u64::from(low)
}
