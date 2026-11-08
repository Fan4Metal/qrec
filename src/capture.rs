//! Desktop Duplication: the recorded area of one monitor as a Direct3D 11
//! texture, kept up to date frame by frame, and the pointer that goes
//! with it.
//!
//! The duplication delivers the whole monitor; only the area is copied
//! out, into a BGRA texture of the area's size that the converter and the
//! cursor drawing work on. A frame with a new desktop image is copied; one
//! that only reports a pointer move is not.

use windows::Win32::Foundation::{HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11CreateDevice,
    ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
    DXGI_OUTDUPL_POINTER_SHAPE_INFO, IDXGIFactory1, IDXGIOutput1, IDXGIOutput5, IDXGIOutputDuplication, IDXGIResource,
};
use windows::core::{Interface, Result};

use crate::display::Monitor;
use crate::hdr::ToneMapper;
use crate::region::Region;

/// The pointer as Desktop Duplication reports it.
#[derive(Default)]
pub struct Cursor {
    pub visible: bool,
    /// Position of the shape's top-left corner (the hotspot already
    /// subtracted), in the pixels of the monitor.
    pub x: i32,
    pub y: i32,
    pub shape: Option<CursorShape>,
}

/// A pointer shape: `DXGI_OUTDUPL_POINTER_SHAPE_TYPE_*` in `kind`, rows of
/// `pitch` bytes. Monochrome shapes hold an AND mask over an XOR mask, so
/// their buffer is `2 * height` rows.
pub struct CursorShape {
    pub kind: u32,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub data: Vec<u8>,
    /// Counts up with every new shape, so a drawer knows when to upload.
    pub generation: u64,
}

pub enum PollError {
    /// The duplication is gone (a mode change, the secure desktop): it has
    /// to be created again.
    AccessLost,
    Other(windows::core::Error),
}

pub struct Capturer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    output: IDXGIOutput1,
    duplication: Option<IDXGIOutputDuplication>,
    /// The area, in the pixels of the monitor.
    region: Region,
    /// The area's latest image, BGRA.
    frame: ID3D11Texture2D,
    has_frame: bool,
    pub cursor: Cursor,
    shapes: u64,
    /// The monitor's GDI name, for its SDR white level.
    device_name: String,
    /// An HDR desktop's area into the frame; `None` for an SDR one.
    tone: Option<ToneMapper>,
}

impl Capturer {
    /// Opens the duplication of `monitor`, on the adapter that drives it,
    /// for `region` given in the monitor's own pixels.
    pub fn new(monitor: &Monitor, region: Region) -> Result<Capturer> {
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
        let adapter = unsafe { factory.EnumAdapters1(monitor.adapter)? };
        let output: IDXGIOutput1 = unsafe { adapter.EnumOutputs(monitor.output)? }.cast()?;
        let (mut device, mut context) = (None, None);
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
        }
        let (device, context) = (device.unwrap(), context.unwrap());
        // Media Foundation's encoder uses the device from its own threads.
        unsafe { let _ = device.cast::<ID3D11Multithread>()?.SetMultithreadProtected(true); }
        let frame = create_texture(&device, region.width, region.height, DXGI_FORMAT_B8G8R8A8_UNORM)?;
        let mut capturer = Capturer {
            device,
            context,
            output,
            duplication: None,
            region,
            frame,
            has_frame: false,
            cursor: Cursor::default(),
            shapes: 0,
            device_name: monitor.device_name.clone(),
            tone: None,
        };
        capturer.recreate()?;
        Ok(capturer)
    }

    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    pub fn context(&self) -> &ID3D11DeviceContext {
        &self.context
    }

    /// The area's latest image; valid once `has_frame`.
    pub fn frame(&self) -> &ID3D11Texture2D {
        &self.frame
    }

    pub fn has_frame(&self) -> bool {
        self.has_frame
    }

    /// Moves the area (the same size, in the monitor's pixels): the next
    /// desktop frame is copied from there. Moving a window changes the
    /// desktop, so one comes.
    pub fn move_to(&mut self, region: Region) {
        debug_assert_eq!((region.width, region.height), (self.region.width, self.region.height));
        self.region = region;
    }

    /// Opens the duplication (again, after it was lost): 8-bit BGRA, or
    /// 16-bit float scRGB from an HDR display, which is tone-mapped into
    /// the frame (`hdr.rs`). QREC_HDR=1 asks for the float format alone, to
    /// check that path on an SDR display.
    pub fn recreate(&mut self) -> Result<()> {
        self.duplication = None;
        let formats: &[DXGI_FORMAT] =
            if std::env::var_os("QREC_HDR").is_some() { &[DXGI_FORMAT_R16G16B16A16_FLOAT] } else { &[DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT] };
        // `DuplicateOutput1` (Windows 10 1703) names the formats taken; the
        // older call gives an HDR display's float frames all the same.
        let duplication = match self.output.cast::<IDXGIOutput5>() {
            Ok(output) => match unsafe { output.DuplicateOutput1(&self.device, 0, formats) } {
                Ok(d) => d,
                Err(e) => {
                    log::debug!("DuplicateOutput1: {}; the older call", crate::win::describe(&e));
                    unsafe { self.output.DuplicateOutput(&self.device)? }
                }
            },
            Err(_) => unsafe { self.output.DuplicateOutput(&self.device)? },
        };
        // The format is the frames' (`take_frame`): the description gives
        // the display mode's, which differs when float frames were asked.
        self.tone = None;
        self.duplication = Some(duplication);
        Ok(())
    }

    /// Ready for frames of `format`: the tone mapping for float ones.
    fn prepare(&mut self, format: DXGI_FORMAT) -> Result<()> {
        match format {
            DXGI_FORMAT_B8G8R8A8_UNORM => self.tone = None,
            DXGI_FORMAT_R16G16B16A16_FLOAT if self.tone.is_none() => {
                let white = crate::hdr::sdr_white_nits(&self.device_name);
                log::debug!("HDR desktop: scRGB, SDR white at {white:.0} nits");
                let (width, height) = (self.region.width, self.region.height);
                self.tone = Some(ToneMapper::new(&self.device, &self.context, &self.frame, width, height, white)?);
            }
            DXGI_FORMAT_R16G16B16A16_FLOAT => {}
            other => {
                return Err(windows::core::Error::new(
                    windows::Win32::Foundation::E_NOTIMPL,
                    format!("{} {}", tr!("unsupported desktop format", "неподдерживаемый формат рабочего стола"), other.0),
                ));
            }
        }
        Ok(())
    }

    /// Waits up to `timeout_ms` for the next frame and takes it: the
    /// area is copied when the desktop image changed, the pointer is
    /// updated when it moved or changed shape. `Ok(true)` when a frame
    /// came, `Ok(false)` on the timeout.
    pub fn poll(&mut self, timeout_ms: u32) -> std::result::Result<bool, PollError> {
        let Some(duplication) = self.duplication.clone() else {
            return Err(PollError::AccessLost);
        };
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        match unsafe { duplication.AcquireNextFrame(timeout_ms, &mut info, &mut resource) } {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(false),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                self.duplication = None;
                return Err(PollError::AccessLost);
            }
            Err(e) => return Err(PollError::Other(e)),
        }
        let result = self.take_frame(&duplication, &info, resource);
        let _ = unsafe { duplication.ReleaseFrame() };
        match result {
            Ok(()) => Ok(true),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                self.duplication = None;
                Err(PollError::AccessLost)
            }
            Err(e) => Err(PollError::Other(e)),
        }
    }

    fn take_frame(
        &mut self,
        duplication: &IDXGIOutputDuplication,
        info: &DXGI_OUTDUPL_FRAME_INFO,
        resource: Option<IDXGIResource>,
    ) -> Result<()> {
        if info.LastPresentTime != 0
            && let Some(resource) = resource
        {
            let texture: ID3D11Texture2D = resource.cast()?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { texture.GetDesc(&mut desc) };
            self.prepare(desc.Format)?;
            let r = self.region;
            let source = D3D11_BOX {
                left: r.x as u32,
                top: r.y as u32,
                front: 0,
                right: r.x as u32 + r.width,
                bottom: r.y as u32 + r.height,
                back: 1,
            };
            match &self.tone {
                Some(tone) => unsafe {
                    self.context.CopySubresourceRegion(tone.source(), 0, 0, 0, 0, &texture, 0, Some(&source));
                    tone.run(&self.context);
                },
                None => unsafe { self.context.CopySubresourceRegion(&self.frame, 0, 0, 0, 0, &texture, 0, Some(&source)) },
            }
            self.has_frame = true;
        }
        if info.LastMouseUpdateTime != 0 {
            if !self.cursor.visible && info.PointerPosition.Visible.as_bool() {
                log::debug!("pointer visible at {}, {}", info.PointerPosition.Position.x, info.PointerPosition.Position.y);
            }
            self.cursor.visible = info.PointerPosition.Visible.as_bool();
            self.cursor.x = info.PointerPosition.Position.x;
            self.cursor.y = info.PointerPosition.Position.y;
        }
        if info.PointerShapeBufferSize > 0 {
            let mut data = vec![0u8; info.PointerShapeBufferSize as usize];
            let mut required = 0u32;
            let mut shape = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
            unsafe {
                duplication.GetFramePointerShape(data.len() as u32, data.as_mut_ptr().cast(), &mut required, &mut shape)?;
            }
            self.shapes += 1;
            log::debug!("pointer shape {}: type {}, {}x{}, pitch {}, hotspot {}, {}", self.shapes, shape.Type, shape.Width, shape.Height, shape.Pitch, shape.HotSpot.x, shape.HotSpot.y);
            self.cursor.shape = Some(CursorShape {
                kind: shape.Type,
                width: shape.Width,
                height: shape.Height,
                pitch: shape.Pitch,
                data,
                generation: self.shapes,
            });
            // The hotspot is where the pointer points; the shape's corner
            // is further up and left.
            let _ = (shape.HotSpot.x, shape.HotSpot.y);
        }
        Ok(())
    }
}

/// A GPU texture the video processor can read and render to.
pub fn create_texture(device: &ID3D11Device, width: u32, height: u32, format: DXGI_FORMAT) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture))? };
    Ok(texture.unwrap())
}

/// A rectangle in Win32's form.
pub fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
    RECT { left, top, right, bottom }
}
