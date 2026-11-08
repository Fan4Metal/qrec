//! A stretch of a recording as an animated GIF (WebP is left out: no
//! encoder of animated WebP in pure Rust), for a chat or an issue: its
//! frames decoded (`trim::Preview`) at no more than `FPS` a second, scaled
//! down by a whole factor so neither side exceeds `MAX_SIDE`, each with a
//! palette of its own (NeuQuant, in the `gif` crate). Only the rectangle
//! that changed since the frame before is written, over it; a frame the
//! same as the one before lengthens it instead, so a still screen costs
//! nothing. No sound.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::sync::atomic::Ordering::Relaxed;

use crate::trim::{Preview, Progress, SECOND};

/// Frames per second, at most (the recording's when it has fewer).
pub const FPS: f64 = 15.0;
/// The longest side, at most.
pub const MAX_SIDE: usize = 960;
/// NeuQuant's sampling: 1 is the best and slowest, 30 the fastest.
const SPEED: i32 = 10;

/// A part of a frame to be written over the one before.
struct Patch {
    left: usize,
    top: usize,
    width: usize,
    height: usize,
    rgba: Vec<u8>,
}

/// Writes the frames shown from `start` up to `end` into `dst`, with
/// `keys` (the key frames' times) to read on rather than seek; the frames
/// written. `progress` is told how far it is, and stops it.
pub fn export(src: &Path, dst: &Path, start: i64, end: i64, keys: Vec<i64>, progress: &Progress) -> Result<u32, String> {
    let describe = |e: windows::core::Error| crate::win::describe(&e);
    let mut preview = Preview::open(src).map_err(describe)?;
    preview.set_keys(keys);
    let fps = if preview.info.fps > 0.0 { preview.info.fps.min(FPS) } else { FPS };
    let step = (SECOND as f64 / fps) as i64;
    let count = ((end - start).max(0) as f64 / step as f64).ceil().max(1.0) as i64;
    // Where step `i` starts, in the hundredths of a second GIF counts in:
    // rounded from the start, so the delays do not drift.
    let centiseconds = |i: i64| ((i as f64) * 100.0 / fps).round() as i64;
    let write = |encoder: &mut ::gif::Encoder<BufWriter<File>>, first: i64, last: i64, mut patch: Patch| {
        let mut frame = ::gif::Frame::from_rgba_speed(patch.width as u16, patch.height as u16, &mut patch.rgba, SPEED);
        frame.left = patch.left as u16;
        frame.top = patch.top as u16;
        frame.dispose = ::gif::DisposalMethod::Keep;
        frame.delay = (centiseconds(last) - centiseconds(first)).clamp(1, u16::MAX as i64) as u16;
        encoder.write_frame(&frame).map_err(|e| e.to_string())
    };

    let mut encoder: Option<::gif::Encoder<BufWriter<File>>> = None;
    // The whole picture as it stands after the frames so far.
    let mut shown: Option<(usize, usize, Vec<u8>)> = None;
    // The patch waiting to be written, and the step it starts at.
    let mut pending: Option<(i64, Patch)> = None;
    let mut written = 0u32;
    for i in 0..count {
        if progress.cancel.load(Relaxed) {
            drop(encoder);
            let _ = std::fs::remove_file(dst);
            return Err(tr!("Cancelled", "Отменено").into());
        }
        progress.done.store(((i + 1) * 1000 / count) as u32, Relaxed);
        let Some(picture) = preview.frame(start + i * step, MAX_SIDE).map_err(describe)? else { break };
        let (width, height) = (picture.width, picture.height);
        if encoder.is_none() {
            let file = File::create(dst).map_err(|e| e.to_string())?;
            let mut e = ::gif::Encoder::new(BufWriter::new(file), width as u16, height as u16, &[]).map_err(|e| e.to_string())?;
            e.set_repeat(::gif::Repeat::Infinite).map_err(|e| e.to_string())?;
            encoder = Some(e);
        }
        let patch = match &shown {
            Some((w, h, before)) if (*w, *h) == (width, height) => match changed(before, &picture.rgba, width) {
                // The same picture: the one before lasts longer.
                None => continue,
                Some((left, top, right, bottom)) => crop(&picture.rgba, width, left, top, right, bottom),
            },
            _ => Patch { left: 0, top: 0, width, height, rgba: picture.rgba.clone() },
        };
        if let Some((first, before)) = pending.take() {
            write(encoder.as_mut().expect("made above"), first, i, before)?;
            written += 1;
        }
        pending = Some((i, patch));
        shown = Some((width, height, picture.rgba));
    }
    let Some(mut encoder) = encoder else {
        return Err(tr!("No frames in the stretch", "Во фрагменте нет кадров").into());
    };
    if let Some((first, patch)) = pending.take() {
        write(&mut encoder, first, count, patch)?;
        written += 1;
    }
    encoder.into_inner().map_err(|e| e.to_string())?;
    Ok(written)
}

/// The rectangle `(left, top, right, bottom)` holding every pixel that
/// differs between two pictures `width` wide; `None` when none does.
fn changed(a: &[u8], b: &[u8], width: usize) -> Option<(usize, usize, usize, usize)> {
    let (mut left, mut top, mut right, mut bottom) = (usize::MAX, usize::MAX, 0, 0);
    for (y, (ra, rb)) in a.chunks_exact(width * 4).zip(b.chunks_exact(width * 4)).enumerate() {
        if ra == rb {
            continue;
        }
        let (pa, pb) = (ra.as_chunks::<4>().0, rb.as_chunks::<4>().0);
        let first = pa.iter().zip(pb).position(|(p, q)| p != q).unwrap_or(0);
        let last = width - 1 - pa.iter().rev().zip(pb.iter().rev()).position(|(p, q)| p != q).unwrap_or(0);
        (left, right) = (left.min(first), right.max(last + 1));
        (top, bottom) = (top.min(y), y + 1);
    }
    (left < right).then_some((left, top, right, bottom))
}

fn crop(rgba: &[u8], width: usize, left: usize, top: usize, right: usize, bottom: usize) -> Patch {
    let mut out = Vec::with_capacity((right - left) * (bottom - top) * 4);
    for row in rgba.chunks_exact(width * 4).skip(top).take(bottom - top) {
        out.extend_from_slice(&row[left * 4..right * 4]);
    }
    Patch { left, top, width: right - left, height: bottom - top, rgba: out }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_what_changed() {
        let (w, h) = (5, 4);
        let a = vec![0u8; w * h * 4];
        assert_eq!(changed(&a, &a, w), None);
        let mut b = a.clone();
        // Pixels (1, 1) and (3, 2).
        b[(w + 1) * 4] = 9;
        b[(2 * w + 3) * 4 + 2] = 9;
        assert_eq!(changed(&a, &b, w), Some((1, 1, 4, 3)));
        let patch = crop(&b, w, 1, 1, 4, 3);
        assert_eq!((patch.left, patch.top, patch.width, patch.height), (1, 1, 3, 2));
        assert_eq!(patch.rgba[0], 9);
        assert_eq!(patch.rgba[(3 + 2) * 4 + 2], 9);
    }
}
