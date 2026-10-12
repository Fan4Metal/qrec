//! The latest recordings in the folder, for the window's list and the
//! menu of the icon in the notification area: the files qrec wrote
//! (`qrec_*.mp4`, its cuts included), newest first, with their length,
//! size and picture size, and a small picture of each, decoded on a
//! thread of its own.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::SystemTime;

use crate::trim::{Picture, SECOND};

/// How many recordings are listed.
pub const COUNT: usize = 8;

/// One recording.
#[derive(Clone, Debug, PartialEq)]
pub struct Recording {
    pub path: PathBuf,
    pub modified: SystemTime,
    pub bytes: u64,
    /// From the file's headers; `None` when they cannot be read (a file
    /// still being written by another program, or not a video).
    pub duration: Option<i64>,
    pub width: u32,
    pub height: u32,
}

impl Recording {
    pub fn name(&self) -> String {
        self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }

    /// `0:42 · 18 MB · 1280×720`, what is known of it.
    pub fn details(&self) -> String {
        let mut parts = Vec::new();
        if let Some(duration) = self.duration {
            parts.push(length(duration));
        }
        parts.push(size(self.bytes));
        if self.width > 0 {
            parts.push(format!("{}×{}", self.width, self.height));
        }
        parts.join(" · ")
    }
}

/// The recordings of a folder, with what was read of each file kept until
/// it changes: its headers take a few milliseconds to read.
#[derive(Default)]
pub struct Recordings {
    folder: PathBuf,
    pub list: Vec<Recording>,
    known: HashMap<PathBuf, Recording>,
}

impl Recordings {
    /// Lists `folder` again.
    pub fn refresh(&mut self, folder: &Path) {
        if self.folder != folder {
            self.folder = folder.to_path_buf();
            self.known.clear();
        }
        let mut files: Vec<(PathBuf, SystemTime, u64)> = std::fs::read_dir(folder)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                if !is_recording(&path) {
                    return None;
                }
                let meta = entry.metadata().ok()?;
                Some((path, meta.modified().ok()?, meta.len()))
            })
            .collect();
        files.sort_by_key(|f| std::cmp::Reverse(f.1));
        files.truncate(COUNT);
        self.list = files
            .into_iter()
            .map(|(path, modified, bytes)| match self.known.get(&path) {
                Some(r) if r.modified == modified && r.bytes == bytes => r.clone(),
                _ => {
                    let r = read(path, modified, bytes);
                    self.known.insert(r.path.clone(), r.clone());
                    r
                }
            })
            .collect();
        self.known.retain(|path, _| self.list.iter().any(|r| &r.path == path));
    }

    pub fn get(&self, path: &Path) -> Option<&Recording> {
        self.list.iter().find(|r| r.path == path)
    }
}

/// The longer side of a picture as decoded: a whole factor down from the
/// frame, enough for the list's pictures on a display at 200 %.
const THUMBNAIL_SIDE: usize = 320;

/// A small picture of each recording: one frame, a second in or a tenth
/// of the way through when it is shorter (the first frame is often still
/// before what was recorded), decoded on a thread of its own and kept
/// until the file changes.
pub struct Thumbnails {
    requests: Option<mpsc::Sender<(PathBuf, SystemTime, i64)>>,
    replies: mpsc::Receiver<(PathBuf, SystemTime, Option<Picture>)>,
    /// What was asked for each file (as of its time), and the picture once
    /// it came; `None` also when it could not be decoded.
    known: HashMap<PathBuf, (SystemTime, Option<egui::TextureHandle>)>,
    thread: Option<JoinHandle<()>>,
}

impl Thumbnails {
    pub fn new(ctx: egui::Context) -> Thumbnails {
        let (requests, request_rx) = mpsc::channel::<(PathBuf, SystemTime, i64)>();
        let (reply_tx, replies) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("thumbnails".into())
            .spawn(move || {
                let _com = crate::win::com_init_mta();
                while let Ok((path, modified, duration)) = request_rx.recv() {
                    let time = (duration / 10).min(SECOND);
                    let picture = crate::trim::Preview::open(&path).and_then(|mut p| p.frame(time, THUMBNAIL_SIDE)).ok().flatten();
                    if reply_tx.send((path, modified, picture)).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
            })
            .inspect_err(|e| log::warn!("no thumbnails thread: {e}"))
            .ok();
        Thumbnails { requests: Some(requests), replies, known: HashMap::new(), thread }
    }

    /// Asks for the pictures of `list` not asked for yet, and forgets the
    /// files no longer in it.
    pub fn want(&mut self, list: &[Recording]) {
        self.known.retain(|path, _| list.iter().any(|r| &r.path == path));
        for r in list {
            if self.known.get(&r.path).is_some_and(|(modified, _)| *modified == r.modified) {
                continue;
            }
            self.known.insert(r.path.clone(), (r.modified, None));
            if let Some(requests) = &self.requests {
                let _ = requests.send((r.path.clone(), r.modified, r.duration.unwrap_or(0)));
            }
        }
    }

    /// Takes the pictures decoded since.
    pub fn poll(&mut self, ctx: &egui::Context) {
        while let Ok((path, modified, picture)) = self.replies.try_recv() {
            let Some(picture) = picture else { continue };
            if let Some(entry) = self.known.get_mut(&path).filter(|(m, _)| *m == modified) {
                let image = egui::ColorImage::from_rgba_unmultiplied([picture.width, picture.height], &picture.rgba);
                entry.1 = Some(ctx.load_texture(format!("thumbnail {}", path.display()), image, egui::TextureOptions::LINEAR));
            }
        }
    }

    pub fn get(&self, path: &Path) -> Option<&egui::TextureHandle> {
        self.known.get(path).and_then(|(_, texture)| texture.as_ref())
    }
}

impl Drop for Thumbnails {
    /// The thread ends after the picture it is decoding.
    fn drop(&mut self) {
        self.requests = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn read(path: PathBuf, modified: SystemTime, bytes: u64) -> Recording {
    let info = crate::trim::info(&path).ok();
    let duration = info.as_ref().map(|i| i.duration).filter(|&d| d > 0);
    let (width, height) = info.map_or((0, 0), |i| (i.width, i.height));
    Recording { path, modified, bytes, duration, width, height }
}

/// A file qrec wrote: `qrec_<date>_<time>.mp4`, and its cuts, which keep
/// the name with the stretch after it. Not a cut being written to replace
/// its source (`.trimming.mp4`).
fn is_recording(path: &Path) -> bool {
    let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    name.starts_with("qrec_") && name.ends_with(".mp4") && !name.ends_with(".trimming.mp4")
}

/// `0:42`, `12:05`, `1:02:03`.
pub fn length(time: i64) -> String {
    let s = (time.max(0) + 5_000_000) / 10_000_000;
    let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

/// `18 MB`, `4,2 MB`, `640 KB`.
pub fn size(bytes: u64) -> String {
    let mb = bytes as f64 / 1e6;
    if mb >= 10.0 {
        format!("{mb:.0} {}", tr!("MB", "МБ"))
    } else if mb >= 1.0 {
        format!("{} {}", format!("{mb:.1}").replace('.', tr!(".", ",")), tr!("MB", "МБ"))
    } else {
        format!("{:.0} {}", bytes as f64 / 1e3, tr!("KB", "КБ"))
    }
}

/// When it was recorded: the time today, the date and time before.
pub fn when(modified: SystemTime) -> String {
    let stamp = |t: SystemTime| -> Option<(i64, u32, u32, u32, u32)> {
        // Local time through the system: the file time to a local
        // SYSTEMTIME.
        use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
        use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
        let since = t.duration_since(SystemTime::UNIX_EPOCH).ok()?;
        let ticks = since.as_nanos() as u64 / 100 + 116_444_736_000_000_000;
        let ft = FILETIME { dwLowDateTime: ticks as u32, dwHighDateTime: (ticks >> 32) as u32 };
        let (mut utc, mut local) = (SYSTEMTIME::default(), SYSTEMTIME::default());
        unsafe {
            FileTimeToSystemTime(&ft, &mut utc).ok()?;
            SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).ok()?;
        }
        Some((i64::from(local.wYear), u32::from(local.wMonth), u32::from(local.wDay), u32::from(local.wHour), u32::from(local.wMinute)))
    };
    let (Some(t), Some(now)) = (stamp(modified), stamp(SystemTime::now())) else { return String::new() };
    if (t.0, t.1, t.2) == (now.0, now.1, now.2) {
        format!("{:02}:{:02}", t.3, t.4)
    } else {
        format!("{:02}.{:02} {:02}:{:02}", t.2, t.1, t.3, t.4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knows_its_files() {
        assert!(is_recording(Path::new("C:/v/qrec_2026-10-12_16-45-12.mp4")));
        assert!(is_recording(Path::new("C:/v/qrec_2026-10-12_16-45-12_0.01.000-0.02.500.mp4")));
        assert!(is_recording(Path::new("C:/v/QREC_x.MP4")));
        assert!(!is_recording(Path::new("C:/v/qrec_x.trimming.mp4")));
        assert!(!is_recording(Path::new("C:/v/other.mp4")));
        assert!(!is_recording(Path::new("C:/v/qrec_x.mkv")));
    }

    #[test]
    fn formats_lengths_and_sizes() {
        assert_eq!(length(42 * 10_000_000), "0:42");
        assert_eq!(length(725 * 10_000_000), "12:05");
        assert_eq!(length(3723 * 10_000_000), "1:02:03");
        assert_eq!(size(18_400_000), "18 MB");
        assert_eq!(size(4_200_000), "4.2 MB");
        assert_eq!(size(640_000), "640 KB");
    }
}
