//! Procedurally drawn application icon: a dark rounded square with a thin
//! light frame inside, the area being recorded, and the red dot of a
//! record button in its middle. Dependency-free apart from `png`, so
//! that `build.rs` can include this file to produce the `.ico` embedded
//! in the executable, while the app uses the same pixels for its window
//! icon.

// build.rs and the app use different parts.
#![allow(dead_code)]

/// Sizes in the `.ico` of the executable and in the exported icon.
pub const ICO_SIZES: [u32; 8] = [16, 20, 24, 32, 40, 48, 64, 256];

/// Straight RGBA, channels 0..1.
type Rgba = [f32; 4];

fn hex(c: u32) -> Rgba {
    let ch = |s: u32| ((c >> s) & 0xff) as f32 / 255.0;
    [ch(16), ch(8), ch(0), 1.0]
}

/// Paint `top` over `under`.
fn over(under: Rgba, top: Rgba) -> Rgba {
    let a = top[3] + under[3] * (1.0 - top[3]);
    if a <= 0.0 {
        return [0.0; 4];
    }
    let mut out = [0.0, 0.0, 0.0, a];
    for i in 0..3 {
        out[i] = (top[i] * top[3] + under[i] * under[3] * (1.0 - top[3])) / a;
    }
    out
}

/// Signed distance from `(x, y)` to a box of half size `h` centred at the
/// origin, corners rounded by `r`; negative inside.
fn rounded_box(x: f32, y: f32, h: f32, r: f32) -> f32 {
    let (qx, qy) = ((x.abs() - h + r).max(0.0), (y.abs() - h + r).max(0.0));
    (qx * qx + qy * qy).sqrt() + (x.abs() - h + r).max(y.abs() - h + r).min(0.0) - r
}

/// Coverage of a shape whose signed distance is `d`, softened over one
/// pixel (`px`).
fn coverage(d: f32, px: f32) -> f32 {
    (0.5 - d / px).clamp(0.0, 1.0)
}

/// Colour at `(fx, fy)` in 0..1 of the icon, `px` the size of a pixel.
/// While recording (the icon in the notification area) the square is red
/// and the dot white.
fn sample(fx: f32, fy: f32, px: f32, recording: bool) -> Rgba {
    let (x, y) = (fx - 0.5, fy - 0.5);
    let mut c = [0.0; 4];
    // The square.
    let square = rounded_box(x, y, 0.5, 0.11);
    let mut back = hex(if recording { 0xe5_39_35 } else { 0x2a_33_40 });
    back[3] = coverage(square, px);
    c = over(c, back);
    // The frame: a light rounded outline, at least one pixel thick, with
    // its corners emphasised so it reads as a selection.
    let inset = 0.19;
    let thick = (0.045f32).max(px);
    let d = rounded_box(x, y, 0.5 - inset, 0.05).abs() - thick / 2.0;
    let mut frame = hex(0xdc_e3_ea);
    frame[3] = 0.85 * coverage(d, px);
    // Gaps in the middle of each side, when the icon is big enough.
    if px < 0.03 {
        let gap = 0.13;
        let on_side_x = x.abs() < gap && y.abs() > 0.5 - inset - thick;
        let on_side_y = y.abs() < gap && x.abs() > 0.5 - inset - thick;
        if on_side_x || on_side_y {
            frame[3] = 0.0;
        }
    }
    c = over(c, frame);
    // The dot.
    let dot = (x * x + y * y).sqrt() - 0.17;
    let mut dot_colour = hex(if recording { 0xff_ff_ff } else { 0xe5_39_35 });
    dot_colour[3] = coverage(dot, px);
    c = over(c, dot_colour);
    c
}

/// The icon as straight RGBA, `size` x `size`, supersampled.
pub fn rgba(size: u32) -> Vec<u8> {
    rgba_of(size, false)
}

/// The icon of the notification area while recording.
pub fn recording_rgba(size: u32) -> Vec<u8> {
    rgba_of(size, true)
}

fn rgba_of(size: u32, recording: bool) -> Vec<u8> {
    const SS: u32 = 5;
    let n = size as usize;
    let px = 1.0 / size as f32;
    let mut out = vec![0u8; n * n * 4];
    for y in 0..size {
        for x in 0..size {
            // Premultiplied average.
            let mut acc = [0.0f32; 4];
            for sy in 0..SS {
                for sx in 0..SS {
                    let fx = (x as f32 + (sx as f32 + 0.5) / SS as f32) * px;
                    let fy = (y as f32 + (sy as f32 + 0.5) / SS as f32) * px;
                    let c = sample(fx, fy, px, recording);
                    for i in 0..3 {
                        acc[i] += c[i] * c[3];
                    }
                    acc[3] += c[3];
                }
            }
            let i = (y as usize * n + x as usize) * 4;
            if acc[3] > 0.0 {
                let a = acc[3] / (SS * SS) as f32;
                let to_u8 = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u8;
                out[i..i + 4].copy_from_slice(&[to_u8(acc[0] / acc[3]), to_u8(acc[1] / acc[3]), to_u8(acc[2] / acc[3]), to_u8(a)]);
            }
        }
    }
    out
}

/// The icon as a multi-resolution `.ico`: 32-bit BMP entries, the 256
/// px layer as PNG, which Windows reads in icons since Vista and which
/// is a fraction of the size.
pub fn ico(sizes: &[u32]) -> Vec<u8> {
    let images: Vec<Vec<u8>> = sizes.iter().map(|&s| if s >= 256 { png(s, &rgba(s)) } else { bmp_entry(s, &rgba(s)) }).collect();
    let mut out = Vec::new();
    out.extend_from_slice(&[0, 0, 1, 0]); // reserved, type = icon
    out.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (&s, img) in sizes.iter().zip(&images) {
        let dim = if s >= 256 { 0 } else { s as u8 };
        out.extend_from_slice(&[dim, dim, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes()); // planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bpp
        out.extend_from_slice(&(img.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += img.len() as u32;
    }
    for img in images {
        out.extend_from_slice(&img);
    }
    out
}

/// A whole PNG file of straight RGBA pixels.
pub fn png(size: u32, px: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, size, size);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::High);
    let mut writer = encoder.write_header().expect("png header");
    writer.write_image_data(px).expect("png data");
    writer.finish().expect("png end");
    out
}

/// A 32-bit BMP icon entry: the header, the rows bottom-up in BGRA, and
/// an empty AND mask (the alpha channel carries the transparency). Also
/// what `CreateIconFromResourceEx` takes for the notification area.
pub fn bmp_entry(size: u32, px: &[u8]) -> Vec<u8> {
    let n = size as usize;
    let mask_stride = n.div_ceil(32) * 4;
    let mut out = Vec::with_capacity(40 + n * n * 4 + mask_stride * n);
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(size as i32).to_le_bytes());
    out.extend_from_slice(&(2 * size as i32).to_le_bytes()); // XOR + AND
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&[0u8; 24]); // compression .. colours important
    for y in (0..n).rev() {
        for x in 0..n {
            let i = (y * n + x) * 4;
            out.extend_from_slice(&[px[i + 2], px[i + 1], px[i], px[i + 3]]);
        }
    }
    out.resize(out.len() + mask_stride * n, 0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_shape() {
        let px = rgba(32);
        assert_eq!(px.len(), 32 * 32 * 4);
        // The corners are (all but) transparent, the middle is the red dot.
        assert!(px[3] < 32);
        let centre = (16 * 32 + 16) * 4;
        assert_eq!(px[centre + 3], 255);
        assert!(px[centre] > 200 && px[centre + 1] < 80);
        // While recording: a red square, the dot white.
        let rec = recording_rgba(32);
        assert!(rec[centre] > 240 && rec[centre + 1] > 240 && rec[centre + 2] > 240);
        let edge = (16 * 32 + 3) * 4;
        assert!(rec[edge] > 200 && rec[edge + 1] < 80 && rec[edge + 3] == 255);
        let ico = ico(&[16, 32]);
        assert_eq!(&ico[..6], &[0, 0, 1, 0, 2, 0]);
    }
}
