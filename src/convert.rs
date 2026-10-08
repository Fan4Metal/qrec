//! BGRA to NV12 on the GPU, with the Direct3D 11 video processor: the
//! encoder takes the result without a copy through system memory.
//!
//! The output textures come from a pool. The encoder may still hold one
//! after `WriteSample` returned (a hardware encoder reads it later), so a
//! texture is reused only when nothing but the pool references it, and
//! the pool grows when all are busy.

use std::mem::ManuallyDrop;
use std::ptr::null_mut;

use windows::Win32::Graphics::Direct3D11::{
    D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    ID3D11VideoContext1, ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709, DXGI_FORMAT_NV12, DXGI_RATIONAL,
};
use windows::core::{IUnknown, Interface, Result};

use crate::capture::{create_texture, rect};

struct Slot {
    texture: ID3D11Texture2D,
    view: ID3D11VideoProcessorOutputView,
    unknown: IUnknown,
    /// Reference count when nobody outside holds the texture.
    idle_refs: u32,
}

pub struct Converter {
    device: ID3D11Device,
    context: ID3D11VideoContext1,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    input: ID3D11VideoProcessorInputView,
    width: u32,
    height: u32,
    pool: Vec<Slot>,
}

impl Converter {
    /// Converts `source` (BGRA, `width` x `height`) whenever asked.
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        source: &ID3D11Texture2D,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Result<Converter> {
        let video_device: ID3D11VideoDevice = device.cast()?;
        let video_context: ID3D11VideoContext1 = context.cast()?;
        let rate = DXGI_RATIONAL { Numerator: fps, Denominator: 1 };
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: width,
            InputHeight: height,
            OutputFrameRate: rate,
            OutputWidth: width,
            OutputHeight: height,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&desc)? };
        let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0)? };
        let input_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 } },
        };
        let mut input = None;
        unsafe { video_device.CreateVideoProcessorInputView(source, &enumerator, &input_desc, Some(&mut input))? };
        let input = input.unwrap();
        unsafe {
            // No driver "enhancements" (sharpening, noise reduction) on
            // screen content, and the colour conversion the encoder expects.
            video_context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            video_context.VideoProcessorSetStreamFrameFormat(&processor, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            video_context.VideoProcessorSetStreamColorSpace1(&processor, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
            video_context.VideoProcessorSetOutputColorSpace1(&processor, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
            let full = rect(0, 0, width as i32, height as i32);
            video_context.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&full));
            video_context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&full));
        }
        let mut converter = Converter {
            device: device.clone(),
            context: video_context,
            enumerator,
            processor,
            input,
            width,
            height,
            pool: Vec::new(),
        };
        for _ in 0..4 {
            converter.add_slot()?;
        }
        Ok(converter)
    }

    fn add_slot(&mut self) -> Result<()> {
        let texture = create_texture(&self.device, self.width, self.height, DXGI_FORMAT_NV12)?;
        let video_device: ID3D11VideoDevice = self.device.cast()?;
        let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
        };
        let mut view = None;
        unsafe { video_device.CreateVideoProcessorOutputView(&texture, &self.enumerator, &desc, Some(&mut view))? };
        let unknown: IUnknown = texture.cast()?;
        let idle_refs = ref_count(&unknown);
        self.pool.push(Slot { texture, view: view.unwrap(), unknown, idle_refs });
        Ok(())
    }

    /// The source converted into an NV12 texture nobody else is using.
    pub fn convert(&mut self) -> Result<ID3D11Texture2D> {
        let free = self.pool.iter().position(|s| ref_count(&s.unknown) <= s.idle_refs);
        let index = match free {
            Some(i) => i,
            None => {
                self.add_slot()?;
                self.pool.len() - 1
            }
        };
        let slot = &self.pool[index];
        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: null_mut(),
            pInputSurface: ManuallyDrop::new(Some(self.input.clone())),
            ppFutureSurfaces: null_mut(),
            ppPastSurfacesRight: null_mut(),
            pInputSurfaceRight: ManuallyDrop::new(None),
            ppFutureSurfacesRight: null_mut(),
        };
        let result = unsafe { self.context.VideoProcessorBlt(&self.processor, &slot.view, 0, std::slice::from_ref(&stream)) };
        unsafe { ManuallyDrop::drop(&mut stream.pInputSurface) };
        result?;
        Ok(slot.texture.clone())
    }

    /// How many output textures exist, for the log.
    pub fn pool_size(&self) -> usize {
        self.pool.len()
    }
}

/// The COM reference count of an object: AddRef, then Release, which
/// reports the count.
fn ref_count(unknown: &IUnknown) -> u32 {
    unsafe {
        let vtable = unknown.vtable();
        (vtable.AddRef)(unknown.as_raw());
        (vtable.Release)(unknown.as_raw())
    }
}
