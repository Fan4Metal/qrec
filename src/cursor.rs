//! The pointer drawn into the frame, as Desktop Duplication reports it: a
//! colour shape blended over the frame, or a monochrome or masked one,
//! whose pixels may invert or XOR what is under them. A small pixel
//! shader does it over the patch of the frame under the pointer.

use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP, ID3DBlob};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BOX, D3D11_COMPARISON_NEVER, D3D11_FILTER_MIN_MAG_MIP_POINT, D3D11_SAMPLER_DESC, D3D11_TEXTURE_ADDRESS_CLAMP,
    D3D11_VIEWPORT, ID3D11Device, ID3D11DeviceContext, ID3D11PixelShader, ID3D11RenderTargetView, ID3D11SamplerState,
    ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::{
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME,
};
use windows::core::{PCSTR, Result, s};

use crate::capture::{Cursor, CursorShape, create_texture};
use crate::region::Rect;

const SHADERS: &str = r#"
Texture2D shape : register(t0);
Texture2D under : register(t1);
SamplerState point_sampler : register(s0);

struct Vertex { float4 position : SV_Position; float2 uv : TEXCOORD0; };

Vertex vs(uint id : SV_VertexID) {
    float2 uv = float2(id & 1, id >> 1);
    Vertex v;
    v.position = float4(uv.x * 2 - 1, 1 - uv.y * 2, 0, 1);
    v.uv = uv;
    return v;
}

// A colour shape: straight alpha over the frame.
float4 ps_blend(Vertex v) : SV_Target {
    float4 s = shape.Sample(point_sampler, v.uv);
    float4 d = under.Sample(point_sampler, v.uv);
    return float4(lerp(d.rgb, s.rgb, s.a), 1);
}

// A monochrome or masked shape, encoded by the program: alpha 0 leaves
// the frame, 255 paints the colour, 128 inverts the frame, 64 XORs the
// colour into it.
float4 ps_mask(Vertex v) : SV_Target {
    float4 s = shape.Sample(point_sampler, v.uv);
    float4 d = under.Sample(point_sampler, v.uv);
    uint a = (uint) round(s.a * 255);
    if (a == 255) return float4(s.rgb, 1);
    if (a == 128) return float4(1 - d.rgb, 1);
    if (a == 64) {
        uint3 x = (uint3) round(d.rgb * 255) ^ (uint3) round(s.rgb * 255);
        return float4(x / 255.0, 1);
    }
    return float4(d.rgb, 1);
}
"#;

/// Alpha values the shaders read as instructions (see `ps_mask`).
const INVERT: u8 = 128;
const XOR: u8 = 64;

pub struct CursorDrawer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    vertex: ID3D11VertexShader,
    blend: ID3D11PixelShader,
    mask: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    target: ID3D11Texture2D,
    target_view: ID3D11RenderTargetView,
    width: u32,
    height: u32,
    shape: Option<Shape>,
    /// A shape that could not be drawn (a type unknown here), so that it
    /// is not tried again each frame.
    refused: Option<u64>,
}

/// The current pointer shape on the GPU, with a patch texture of its
/// size for what lies under it.
struct Shape {
    generation: u64,
    width: u32,
    height: u32,
    masked: bool,
    view: ID3D11ShaderResourceView,
    under: ID3D11Texture2D,
    under_view: ID3D11ShaderResourceView,
}

impl CursorDrawer {
    /// Draws into `target`, a BGRA texture of `width` x `height`.
    pub fn new(device: &ID3D11Device, context: &ID3D11DeviceContext, target: &ID3D11Texture2D, width: u32, height: u32) -> Result<CursorDrawer> {
        let vs = compile(s!("vs"), s!("vs_4_0"))?;
        let blend = compile(s!("ps_blend"), s!("ps_4_0"))?;
        let mask = compile(s!("ps_mask"), s!("ps_4_0"))?;
        let (mut vertex, mut blend_shader, mut mask_shader, mut sampler, mut target_view) = (None, None, None, None, None);
        unsafe {
            device.CreateVertexShader(bytecode(&vs), None, Some(&mut vertex))?;
            device.CreatePixelShader(bytecode(&blend), None, Some(&mut blend_shader))?;
            device.CreatePixelShader(bytecode(&mask), None, Some(&mut mask_shader))?;
            let desc = D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_POINT,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                MipLODBias: 0.0,
                MaxAnisotropy: 1,
                ComparisonFunc: D3D11_COMPARISON_NEVER,
                BorderColor: [0.0; 4],
                MinLOD: 0.0,
                MaxLOD: f32::MAX,
            };
            device.CreateSamplerState(&desc, Some(&mut sampler))?;
            device.CreateRenderTargetView(target, None, Some(&mut target_view))?;
        }
        Ok(CursorDrawer {
            device: device.clone(),
            context: context.clone(),
            vertex: vertex.unwrap(),
            blend: blend_shader.unwrap(),
            mask: mask_shader.unwrap(),
            sampler: sampler.unwrap(),
            target: target.clone(),
            target_view: target_view.unwrap(),
            width,
            height,
            shape: None,
            refused: None,
        })
    }

    /// Draws the pointer, whose position is given relative to `origin`
    /// (the area's corner in the monitor's pixels).
    pub fn draw(&mut self, cursor: &Cursor, origin: (i32, i32)) -> Result<()> {
        if !cursor.visible {
            return Ok(());
        }
        let Some(shape) = &cursor.shape else { return Ok(()) };
        if self.refused == Some(shape.generation) {
            return Ok(());
        }
        if self.shape.as_ref().map(|s| s.generation) != Some(shape.generation) {
            self.shape = upload(&self.device, &self.context, shape)?;
            if self.shape.is_none() {
                self.refused = Some(shape.generation);
            }
        }
        let Some(s) = &self.shape else { return Ok(()) };
        let (x, y) = (cursor.x - origin.0, cursor.y - origin.1);
        let rect = Rect { left: x, top: y, right: x + s.width as i32, bottom: y + s.height as i32 };
        let frame = Rect { left: 0, top: 0, right: self.width as i32, bottom: self.height as i32 };
        let Some(visible) = rect.intersect(&frame) else { return Ok(()) };
        let source = D3D11_BOX {
            left: visible.left as u32,
            top: visible.top as u32,
            front: 0,
            right: visible.right as u32,
            bottom: visible.bottom as u32,
            back: 1,
        };
        let viewport = D3D11_VIEWPORT {
            TopLeftX: x as f32,
            TopLeftY: y as f32,
            Width: s.width as f32,
            Height: s.height as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        let context = &self.context;
        unsafe {
            // What the pointer covers, at the same place in the patch.
            context.CopySubresourceRegion(&s.under, 0, (visible.left - x) as u32, (visible.top - y) as u32, 0, &self.target, 0, Some(&source));
            context.OMSetRenderTargets(Some(&[Some(self.target_view.clone())]), None);
            context.RSSetViewports(Some(&[viewport]));
            context.IASetInputLayout(None);
            context.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            context.VSSetShader(&self.vertex, None);
            context.PSSetShader(if s.masked { &self.mask } else { &self.blend }, None);
            context.PSSetShaderResources(0, Some(&[Some(s.view.clone()), Some(s.under_view.clone())]));
            context.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            context.Draw(4, 0);
            context.PSSetShaderResources(0, Some(&[None, None]));
            context.OMSetRenderTargets(None, None);
        }
        Ok(())
    }
}

/// The shape as the shaders want it, on the GPU.
fn upload(device: &ID3D11Device, context: &ID3D11DeviceContext, shape: &CursorShape) -> Result<Option<Shape>> {
    let Some((width, height, pixels, masked)) = decode(shape) else { return Ok(None) };
    let texture = create_texture(device, width, height, DXGI_FORMAT_B8G8R8A8_UNORM)?;
    let under = create_texture(device, width, height, DXGI_FORMAT_B8G8R8A8_UNORM)?;
    let (mut view, mut under_view) = (None, None);
    unsafe {
        context.UpdateSubresource(&texture, 0, None, pixels.as_ptr().cast(), width * 4, 0);
        device.CreateShaderResourceView(&texture, None, Some(&mut view))?;
        device.CreateShaderResourceView(&under, None, Some(&mut under_view))?;
    }
    Ok(Some(Shape {
        generation: shape.generation,
        width,
        height,
        masked,
        view: view.unwrap(),
        under,
        under_view: under_view.unwrap(),
    }))
}

/// The shape as BGRA pixels: `(width, height, pixels, masked)`, where
/// `masked` selects the shader that reads the alpha as instructions.
fn decode(shape: &CursorShape) -> Option<(u32, u32, Vec<u8>, bool)> {
    let (width, pitch) = (shape.width as usize, shape.pitch as usize);
    if width == 0 || shape.height == 0 {
        return None;
    }
    let row = |i: usize| shape.data.get(i * pitch..(i + 1) * pitch);
    match shape.kind {
        k if k == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32 => {
            let height = shape.height as usize;
            let mut pixels = Vec::with_capacity(width * height * 4);
            for y in 0..height {
                pixels.extend_from_slice(row(y)?.get(..width * 4)?);
            }
            Some((width as u32, height as u32, pixels, false))
        }
        k if k == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32 => {
            let height = shape.height as usize;
            let mut pixels = Vec::with_capacity(width * height * 4);
            for y in 0..height {
                for p in row(y)?.get(..width * 4)?.as_chunks::<4>().0 {
                    // The alpha byte is a mask: 0 paints the colour, 0xFF XORs it.
                    let alpha = if p[3] == 0 { 255 } else { XOR };
                    pixels.extend_from_slice(&[p[0], p[1], p[2], alpha]);
                }
            }
            Some((width as u32, height as u32, pixels, true))
        }
        k if k == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32 => {
            // An AND mask over an XOR mask, one bit per pixel, MSB first.
            let height = shape.height as usize / 2;
            let mut pixels = Vec::with_capacity(width * height * 4);
            for y in 0..height {
                let (and_row, xor_row) = (row(y)?, row(height + y)?);
                for x in 0..width {
                    let bit = |r: &[u8]| (r.get(x / 8).copied().unwrap_or(0) >> (7 - x % 8)) & 1;
                    pixels.extend_from_slice(&match (bit(and_row), bit(xor_row)) {
                        (0, 0) => [0, 0, 0, 255],
                        (0, _) => [255, 255, 255, 255],
                        (_, 0) => [0, 0, 0, 0],
                        _ => [255, 255, 255, INVERT],
                    });
                }
            }
            Some((width as u32, height as u32, pixels, true))
        }
        _ => None,
    }
}

fn compile(entry: PCSTR, target: PCSTR) -> Result<ID3DBlob> {
    let (mut code, mut errors) = (None, None);
    let result = unsafe {
        D3DCompile(SHADERS.as_ptr().cast(), SHADERS.len(), s!("cursor.hlsl"), None, None, entry, target, 0, 0, &mut code, Some(&mut errors))
    };
    if let Err(e) = result {
        if let Some(errors) = errors {
            log::error!("shader: {}", String::from_utf8_lossy(bytecode(&errors)));
        }
        return Err(e);
    }
    Ok(code.unwrap())
}

fn bytecode(blob: &ID3DBlob) -> &[u8] {
    unsafe { std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_monochrome_masks() {
        // 8x1 pointer: AND row then XOR row.
        let shape = CursorShape {
            kind: DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32,
            width: 8,
            height: 2,
            pitch: 1,
            data: vec![0b1100_0000, 0b1010_0000],
            generation: 1,
        };
        let (w, h, pixels, masked) = decode(&shape).unwrap();
        assert_eq!((w, h, masked), (8, 1, true));
        assert_eq!(&pixels[0..4], &[255, 255, 255, INVERT]);
        assert_eq!(&pixels[4..8], &[0, 0, 0, 0]);
        assert_eq!(&pixels[8..12], &[255, 255, 255, 255]);
        assert_eq!(&pixels[12..16], &[0, 0, 0, 255]);
    }
}
