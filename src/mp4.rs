//! The frames of an MP4's video track, read from its index (the `moov`
//! box: the sample table's `stts`, `ctts` and `stss`) instead of from the
//! samples themselves. Media Foundation's source reader has no way to
//! list the samples but reading them, which reads the whole file: an hour
//! of 1080p60 is gigabytes from the disk; the index of it is a few
//! megabytes.
//!
//! Only what Media Foundation would report the same way is read: a single
//! H.264 video track, not fragmented, with no edit list that moves its
//! times. Anything else is `None`, and the caller reads the samples.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::trim::Frame;

/// A larger index is not read into memory.
const MAX_MOOV: u64 = 256 << 20;

/// The frames of the file's H.264 track in the order they are stored
/// (decode order), with their times as Media Foundation gives them (100 ns
/// units); `None` when the file is not one this reads.
pub fn video_frames(path: &Path) -> Option<Vec<Frame>> {
    let moov = read_moov(path)?;
    // Fragments (`mvex`, `moof`) keep their samples outside the index.
    if children(&moov).any(|(kind, _)| &kind == b"mvex") {
        return None;
    }
    children(&moov).filter(|(kind, _)| kind == b"trak").find_map(|(_, trak)| video_track(trak))?
}

/// The `moov` box's contents.
fn read_moov(path: &Path) -> Option<Vec<u8>> {
    let mut file = std::fs::File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    let mut at = 0u64;
    while at + 8 <= length {
        file.seek(SeekFrom::Start(at)).ok()?;
        let mut header = [0u8; 16];
        file.read_exact(&mut header[..8]).ok()?;
        let mut size = u64::from(u32::from_be_bytes(header[..4].try_into().ok()?));
        let kind: [u8; 4] = header[4..8].try_into().ok()?;
        let mut header_size = 8;
        if size == 1 {
            file.read_exact(&mut header[8..16]).ok()?;
            size = u64::from_be_bytes(header[8..16].try_into().ok()?);
            header_size = 16;
        } else if size == 0 {
            size = length - at;
        }
        if size < header_size || at + size > length {
            return None;
        }
        if &kind == b"moov" {
            let body = size - header_size;
            if body > MAX_MOOV {
                return None;
            }
            let mut moov = vec![0u8; body as usize];
            file.read_exact(&mut moov).ok()?;
            return Some(moov);
        }
        at += size;
    }
    None
}

/// The boxes inside `data`: their type and contents.
fn children(data: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
    let mut at = 0usize;
    std::iter::from_fn(move || {
        let header = data.get(at..at + 8)?;
        let mut size = u32::from_be_bytes(header[..4].try_into().ok()?) as usize;
        let kind: [u8; 4] = header[4..8].try_into().ok()?;
        let mut header_size = 8;
        if size == 1 {
            size = usize::try_from(u64::from_be_bytes(data.get(at + 8..at + 16)?.try_into().ok()?)).ok()?;
            header_size = 16;
        } else if size == 0 {
            size = data.len() - at;
        }
        if size < header_size || at.checked_add(size)? > data.len() {
            return None;
        }
        let body = &data[at + header_size..at + size];
        at += size;
        Some((kind, body))
    })
}

/// The first box of type `kind` in `data`.
fn child<'a>(data: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    children(data).find(|(k, _)| k == kind).map(|(_, body)| body)
}

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(data: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(data.get(at..at + 8)?.try_into().ok()?))
}

/// The pairs of a table box (`stts`, `ctts`): after the version, the flags
/// and the count, `count` entries of two 32-bit values.
fn pairs(table: &[u8]) -> Option<Vec<(u32, u32)>> {
    let count = u32_at(table, 4)? as usize;
    let entries = table.get(8..8 + count.checked_mul(8)?)?;
    Some(entries.as_chunks::<8>().0.iter().map(|e| (u32::from_be_bytes([e[0], e[1], e[2], e[3]]), u32::from_be_bytes([e[4], e[5], e[6], e[7]]))).collect())
}

/// `None` when the track is not video; `Some(None)` when it is, but not
/// one this reads.
fn video_track(trak: &[u8]) -> Option<Option<Vec<Frame>>> {
    let mdia = child(trak, b"mdia")?;
    let hdlr = child(mdia, b"hdlr")?;
    if hdlr.get(8..12)? != b"vide" {
        return None;
    }
    Some(read_video_track(trak, mdia))
}

fn read_video_track(trak: &[u8], mdia: &[u8]) -> Option<Vec<Frame>> {
    let stbl = child(child(mdia, b"minf")?, b"stbl")?;
    // H.264, as the source reader's stream is.
    let stsd = child(stbl, b"stsd")?;
    let (entry, _) = children(stsd.get(8..)?).next()?;
    if &entry != b"avc1" && &entry != b"avc3" {
        return None;
    }
    // An edit list that only maps the whole track from its start changes
    // nothing; any other is left to Media Foundation.
    if let Some(elst) = child(trak, b"edts").and_then(|edts| child(edts, b"elst")) {
        let version = *elst.first()?;
        let count = u32_at(elst, 4)?;
        let media_time = if version == 1 { u64_at(elst, 16)? as i64 } else { i64::from(u32_at(elst, 12)? as i32) };
        if count != 1 || media_time != 0 {
            return None;
        }
    }
    let mdhd = child(mdia, b"mdhd")?;
    let timescale = u64::from(if *mdhd.first()? == 1 { u32_at(mdhd, 20)? } else { u32_at(mdhd, 12)? });
    if timescale == 0 {
        return None;
    }
    let deltas = pairs(child(stbl, b"stts")?)?;
    // Composition offsets: unsigned in version 0, signed in version 1 (as
    // written, they are both read as signed).
    let offsets = match child(stbl, b"ctts") {
        Some(ctts) => Some(pairs(ctts)?),
        None => None,
    };
    let sync: Option<Vec<u32>> = match child(stbl, b"stss") {
        Some(stss) => {
            let count = u32_at(stss, 4)? as usize;
            let entries = stss.get(8..8 + count.checked_mul(4)?)?;
            Some(entries.as_chunks::<4>().0.iter().map(|e| u32::from_be_bytes(*e)).collect())
        }
        None => None,
    };
    let count: u64 = deltas.iter().map(|&(n, _)| u64::from(n)).sum();
    if count == 0 || count > 50_000_000 {
        return None;
    }
    let mut offset_runs = offsets.as_deref().unwrap_or(&[]).iter().flat_map(|&(n, o)| std::iter::repeat_n(o as i32, n as usize));
    let mut sync = sync.as_deref().map(|s| s.iter().copied().peekable());
    let to_time = |t: i64| media_time(t, timescale);
    let mut frames = Vec::with_capacity(count as usize);
    let mut decode = 0i64;
    let mut number = 1u32;
    for &(n, delta) in &deltas {
        for _ in 0..n {
            let offset = if offsets.is_some() { i64::from(offset_runs.next()?) } else { 0 };
            let key = match &mut sync {
                None => true,
                Some(sync) => {
                    while sync.next_if(|&s| s < number).is_some() {}
                    sync.next_if_eq(&number).is_some()
                }
            };
            let presented = decode + offset;
            frames.push(Frame { time: to_time(presented), duration: duration(i64::from(delta), timescale), key });
            decode += i64::from(delta);
            number += 1;
        }
    }
    Some(frames)
}

/// A time in the track's units in 100 ns units, as Media Foundation's MP4
/// source gives it: rounded down (166666 for the second frame at 60 fps).
fn media_time(t: i64, timescale: u64) -> i64 {
    (i128::from(t) * 10_000_000).div_euclid(timescale as i128) as i64
}

/// A duration in the track's units in 100 ns units, rounded down as the
/// times are, on its own: a frame's time and duration do not always add
/// up to the next one's time.
fn duration(d: i64, timescale: u64) -> i64 {
    media_time(d, timescale)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }

    fn table(entries: &[(u32, u32)]) -> Vec<u8> {
        let mut out = vec![0u8; 4];
        out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
        for &(a, b) in entries {
            out.extend_from_slice(&a.to_be_bytes());
            out.extend_from_slice(&b.to_be_bytes());
        }
        out
    }

    /// A video track of `deltas` at 30000 per second, its sync samples
    /// `sync`.
    fn track(deltas: &[(u32, u32)], sync: Option<&[u32]>) -> Vec<u8> {
        let mut hdlr = vec![0u8; 8];
        hdlr.extend_from_slice(b"vide");
        hdlr.extend_from_slice(&[0u8; 12]);
        let mut mdhd = vec![0u8; 12];
        mdhd.extend_from_slice(&30_000u32.to_be_bytes());
        mdhd.extend_from_slice(&[0u8; 8]);
        let mut stsd = vec![0u8, 0, 0, 0, 0, 0, 0, 1];
        stsd.extend(boxed(b"avc1", &[0u8; 78]));
        let mut stbl = boxed(b"stsd", &stsd);
        stbl.extend(boxed(b"stts", &table(deltas)));
        if let Some(sync) = sync {
            let mut stss = vec![0u8; 4];
            stss.extend_from_slice(&(sync.len() as u32).to_be_bytes());
            for s in sync {
                stss.extend_from_slice(&s.to_be_bytes());
            }
            stbl.extend(boxed(b"stss", &stss));
        }
        let minf = boxed(b"stbl", &stbl);
        let mut mdia = boxed(b"mdhd", &mdhd);
        mdia.extend(boxed(b"hdlr", &hdlr));
        mdia.extend(boxed(b"minf", &minf));
        boxed(b"trak", &boxed(b"mdia", &mdia))
    }

    #[test]
    fn lists_frames_from_the_sample_table() {
        let trak = track(&[(4, 1000)], Some(&[1, 3]));
        let (_, body) = children(&trak).next().unwrap();
        let frames = video_track(body).unwrap().unwrap();
        let times: Vec<i64> = frames.iter().map(|f| f.time).collect();
        assert_eq!(times, vec![0, 333_333, 666_666, 1_000_000]);
        assert_eq!(frames.iter().map(|f| f.key).collect::<Vec<_>>(), vec![true, false, true, false]);
        // Rounded down, as the source reader gives them.
        assert!(frames.iter().all(|f| f.duration == 333_333));
        // Without stss every frame is a key frame.
        let trak = track(&[(2, 1000)], None);
        let (_, body) = children(&trak).next().unwrap();
        assert!(video_track(body).unwrap().unwrap().iter().all(|f| f.key));
    }

    #[test]
    fn boxes_that_do_not_fit_end_the_list() {
        let mut data = boxed(b"free", &[1, 2, 3]);
        data.extend_from_slice(&[0, 0, 1, 0, b'b', b'a', b'd', b'!']);
        let kinds: Vec<[u8; 4]> = children(&data).map(|(k, _)| k).collect();
        assert_eq!(kinds, vec![*b"free"]);
    }
}
