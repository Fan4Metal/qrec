//! The recordings in the folder, for the card of the last one, the
//! window that lists them and the menu of the icon in the notification
//! area: the files qrec wrote (`qrec_*.mp4`, its cuts included), newest
//! first, with their length, size and picture size and a picture of each,
//! read on a thread of its own.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::SystemTime;

use crate::trim::{Picture, SECOND};

/// How many recordings are listed, newest first.
pub const COUNT: usize = 50;

/// The longer side of a picture as decoded: a whole factor down from the
/// frame, enough for the window's pictures on a display at 150 %.
const PICTURE_SIDE: usize = 480;

/// One recording.
#[derive(Clone, Debug, PartialEq)]
pub struct Recording {
    pub path: PathBuf,
    pub modified: SystemTime,
    pub bytes: u64,
    /// From the file's headers, once read; `None` until then, or when they
    /// cannot be read (a file still being written by another program).
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

/// What the thread read of a file: its headers and one frame.
struct Read {
    path: PathBuf,
    modified: SystemTime,
    duration: Option<i64>,
    size: (u32, u32),
    picture: Option<Picture>,
}

/// The recordings of a folder. A file's headers and its picture (a frame
/// a second in, or a tenth of the way through a shorter one: the first
/// frame is often still before what was recorded) are read on a thread of
/// their own, a few milliseconds a file, and kept until the file changes.
pub struct Recordings {
    folder: PathBuf,
    pub list: Vec<Recording>,
    /// Each file as last read, and its picture.
    known: HashMap<PathBuf, (Recording, Option<egui::TextureHandle>)>,
    requests: Option<mpsc::Sender<(PathBuf, SystemTime)>>,
    replies: mpsc::Receiver<Read>,
    /// Tells the thread to end without reading the rest of its queue.
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Recordings {
    pub fn new(ctx: egui::Context) -> Recordings {
        let (requests, request_rx) = mpsc::channel::<(PathBuf, SystemTime)>();
        let (reply_tx, replies) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("recordings".into())
            .spawn(move || {
                let _com = crate::win::com_init_mta();
                while let Ok((path, modified)) = request_rx.recv() {
                    if stopped.load(Relaxed) {
                        break;
                    }
                    let read = match crate::trim::Preview::open(&path) {
                        Ok(mut preview) => {
                            let info = preview.info.clone();
                            let time = (info.duration / 10).min(SECOND);
                            Read {
                                duration: (info.duration > 0).then_some(info.duration),
                                size: (info.width, info.height),
                                picture: preview.frame(time, PICTURE_SIDE).ok().flatten(),
                                path,
                                modified,
                            }
                        }
                        Err(_) => Read { path, modified, duration: None, size: (0, 0), picture: None },
                    };
                    if reply_tx.send(read).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
            })
            .inspect_err(|e| log::warn!("no recordings thread: {e}"))
            .ok();
        Recordings { folder: PathBuf::new(), list: Vec::new(), known: HashMap::new(), requests: Some(requests), replies, stop, thread }
    }

    /// Lists `folder` again; files new or changed since are read.
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
                Some((r, _)) if r.modified == modified && r.bytes == bytes => r.clone(),
                _ => {
                    let r = Recording { path: path.clone(), modified, bytes, duration: None, width: 0, height: 0 };
                    self.known.insert(path.clone(), (r.clone(), None));
                    if let Some(requests) = &self.requests {
                        let _ = requests.send((path, modified));
                    }
                    r
                }
            })
            .collect();
        self.known.retain(|path, _| self.list.iter().any(|r| &r.path == path));
    }

    /// Takes what the thread read since.
    pub fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(read) = self.replies.try_recv() {
            let Some((known, texture)) = self.known.get_mut(&read.path).filter(|(r, _)| r.modified == read.modified) else { continue };
            known.duration = read.duration;
            (known.width, known.height) = read.size;
            if let Some(picture) = read.picture {
                let image = egui::ColorImage::from_rgba_unmultiplied([picture.width, picture.height], &picture.rgba);
                *texture = Some(ctx.load_texture(format!("recording {}", read.path.display()), image, egui::TextureOptions::LINEAR));
            }
            let known = known.clone();
            if let Some(r) = self.list.iter_mut().find(|r| r.path == known.path) {
                *r = known;
            }
        }
    }

    pub fn get(&self, path: &Path) -> Option<&Recording> {
        self.list.iter().find(|r| r.path == path)
    }

    /// The picture of a recording, once decoded.
    pub fn picture(&self, path: &Path) -> Option<&egui::TextureHandle> {
        self.known.get(path).and_then(|(_, texture)| texture.as_ref())
    }

    /// The bytes of all the files listed.
    pub fn bytes(&self) -> u64 {
        self.list.iter().map(|r| r.bytes).sum()
    }
}

impl Drop for Recordings {
    /// The thread ends after the file it is reading.
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
        self.requests = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
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
