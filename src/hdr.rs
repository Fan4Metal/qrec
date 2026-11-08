//! HDR displays. Desktop Duplication delivers such a desktop as 16-bit
//! float scRGB: linear light with the BT.709 primaries of sRGB, 1.0 being
//! 80 nits, SDR white at the level of the "SDR content brightness" slider,
//! highlights above it. The area is copied into a texture of that format
//! and a pixel shader brings it to the 8-bit sRGB frame the rest of the
//! pipeline takes: scaled so the SDR white is full scale, the highlights
//! bent softly towards it (on the largest channel, so a colour keeps its
//! hue) instead of cut off, negative values (colours outside sRGB)
//! clipped, and the sRGB curve applied.

use windows::Win32::Graphics::Direct3D::D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BUFFER_DESC, D3D11_USAGE_DEFAULT, D3D11_VIEWPORT, ID3D11Buffer, ID3D11Device,
    ID3D11DeviceContext, ID3D11PixelShader, ID3D11RenderTargetView, ID3D11ShaderResourceView, ID3D11Texture2D,
    ID3D11VertexShader,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R16G16B16A16_FLOAT;
use windows::core::{Result, s};

use crate::capture::create_texture;
use crate::cursor::{bytecode, compile_source};

const SHADERS: &str = r#"
Texture2D hdr : register(t0);
cbuffer Tone : register(b0) { float4 tone; };

float4 vs(uint id : SV_VertexID) : SV_Position {
    float2 uv = float2(id & 1, id >> 1);
    return float4(uv.x * 2 - 1, 1 - uv.y * 2, 0, 1);
}

float3 to_srgb(float3 c) {
    return c <= 0.0031308 ? c * 12.92 : 1.055 * pow(c, 1 / 2.4) - 0.055;
}

// tone.x: the scRGB value of SDR white, inverted.
float4 ps(float4 position : SV_Position) : SV_Target {
    float3 c = max(hdr.Load(int3(position.xy, 0)).rgb, 0) * tone.x;
    float m = max(max(c.r, c.g), c.b);
    if (m > 0.8) {
        c *= (0.8 + 0.2 * tanh((m - 0.8) / 0.2)) / m;
    }
    return float4(to_srgb(saturate(c)), 1);
}
"#;

/// scRGB's 1.0, in nits.
const SCRGB_NITS: f32 = 80.0;

pub struct ToneMapper {
    vertex: ID3D11VertexShader,
    pixel: ID3D11PixelShader,
    buffer: ID3D11Buffer,
    /// The area as the duplication gives it, 16-bit float.
    source: ID3D11Texture2D,
    source_view: ID3D11ShaderResourceView,
    target_view: ID3D11RenderTargetView,
    width: u32,
    height: u32,
}

impl ToneMapper {
    /// Maps into `target`, the BGRA frame of `width` x `height`, with SDR
    /// white at `white_nits`.
    pub fn new(device: &ID3D11Device, context: &ID3D11DeviceContext, target: &ID3D11Texture2D, width: u32, height: u32, white_nits: f32) -> Result<ToneMapper> {
        let vs = compile_source(SHADERS, s!("vs"), s!("vs_4_0"))?;
        let ps = compile_source(SHADERS, s!("ps"), s!("ps_4_0"))?;
        let source = create_texture(device, width, height, DXGI_FORMAT_R16G16B16A16_FLOAT)?;
        let (mut vertex, mut pixel, mut buffer, mut source_view, mut target_view) = (None, None, None, None, None);
        unsafe {
            device.CreateVertexShader(bytecode(&vs), None, Some(&mut vertex))?;
            device.CreatePixelShader(bytecode(&ps), None, Some(&mut pixel))?;
            let desc = D3D11_BUFFER_DESC { ByteWidth: 16, Usage: D3D11_USAGE_DEFAULT, BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32, ..Default::default() };
            device.CreateBuffer(&desc, None, Some(&mut buffer))?;
            device.CreateShaderResourceView(&source, None, Some(&mut source_view))?;
            device.CreateRenderTargetView(target, None, Some(&mut target_view))?;
        }
        let buffer = buffer.unwrap();
        let tone: [f32; 4] = [scale(white_nits), 0.0, 0.0, 0.0];
        unsafe { context.UpdateSubresource(&buffer, 0, None, tone.as_ptr().cast(), 0, 0) };
        Ok(ToneMapper {
            vertex: vertex.unwrap(),
            pixel: pixel.unwrap(),
            buffer,
            source,
            source_view: source_view.unwrap(),
            target_view: target_view.unwrap(),
            width,
            height,
        })
    }

    /// The texture the area is copied into.
    pub fn source(&self) -> &ID3D11Texture2D {
        &self.source
    }

    /// Maps the source into the target.
    pub fn run(&self, context: &ID3D11DeviceContext) {
        let viewport = D3D11_VIEWPORT { TopLeftX: 0.0, TopLeftY: 0.0, Width: self.width as f32, Height: self.height as f32, MinDepth: 0.0, MaxDepth: 1.0 };
        unsafe {
            context.OMSetRenderTargets(Some(&[Some(self.target_view.clone())]), None);
            context.RSSetViewports(Some(&[viewport]));
            context.IASetInputLayout(None);
            context.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            context.VSSetShader(&self.vertex, None);
            context.PSSetShader(&self.pixel, None);
            context.PSSetConstantBuffers(0, Some(&[Some(self.buffer.clone())]));
            context.PSSetShaderResources(0, Some(&[Some(self.source_view.clone())]));
            context.Draw(4, 0);
            context.PSSetShaderResources(0, Some(&[None]));
            context.PSSetConstantBuffers(0, Some(&[None]));
            context.OMSetRenderTargets(None, None);
        }
    }
}

/// What scRGB values are multiplied by so SDR white at `white_nits` is 1.
fn scale(white_nits: f32) -> f32 {
    SCRGB_NITS / white_nits.clamp(SCRGB_NITS, 10_000.0)
}

/// The SDR white level of the display named `device_name`
/// (`\\.\DISPLAY1`), in nits: Windows' "SDR content brightness"; 80 when
/// it cannot be read (the slider's lowest, and what SDR has).
pub fn sdr_white_nits(device_name: &str) -> f32 {
    use windows::Win32::Devices::Display::{
        DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL, DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_HEADER,
        DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SDR_WHITE_LEVEL, DISPLAYCONFIG_SOURCE_DEVICE_NAME,
        DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig,
    };
    let read = || -> Option<f32> {
        unsafe {
            let (mut paths, mut modes) = (0u32, 0u32);
            GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut paths, &mut modes).ok().ok()?;
            let mut path_info = vec![DISPLAYCONFIG_PATH_INFO::default(); paths as usize];
            let mut mode_info = vec![DISPLAYCONFIG_MODE_INFO::default(); modes as usize];
            QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, &mut paths, path_info.as_mut_ptr(), &mut modes, mode_info.as_mut_ptr(), None).ok().ok()?;
            for path in &path_info[..paths as usize] {
                let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
                    header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                        r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                        size: size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                        adapterId: path.sourceInfo.adapterId,
                        id: path.sourceInfo.id,
                    },
                    ..Default::default()
                };
                if DisplayConfigGetDeviceInfo(&mut source.header) != 0 {
                    continue;
                }
                let name = String::from_utf16_lossy(&source.viewGdiDeviceName);
                if name.trim_end_matches('\0') != device_name {
                    continue;
                }
                let mut white = DISPLAYCONFIG_SDR_WHITE_LEVEL {
                    header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                        r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
                        size: size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>() as u32,
                        adapterId: path.targetInfo.adapterId,
                        id: path.targetInfo.id,
                    },
                    ..Default::default()
                };
                if DisplayConfigGetDeviceInfo(&mut white.header) != 0 {
                    return None;
                }
                // In thousandths of scRGB's 80 nits.
                return Some(white.SDRWhiteLevel as f32 / 1000.0 * SCRGB_NITS);
            }
            None
        }
    };
    read().unwrap_or(SCRGB_NITS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdr_white_becomes_full_scale() {
        assert_eq!(scale(80.0), 1.0);
        assert_eq!(scale(240.0), 1.0 / 3.0);
        // Nonsense from the driver is kept within reason.
        assert_eq!(scale(0.0), 1.0);
    }
}
