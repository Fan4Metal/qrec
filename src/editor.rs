//! The window that trims a recording: the frame under the cursor, a
//! timeline with the key frames, the start and the end of the stretch
//! kept, playback with sound (`playback`), and the cut itself
//! (`trim::cut`, without re-encoding).

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;

use egui::{Color32, Key, RichText, Sense, Vec2};

use crate::app::{CORNER_RADIUS, TitleButton, title_button};
use crate::playback::Playback;
use crate::trim::{self, Frame, Info, Picture, Progress, SECOND};
use crate::win;

/// The window's size in points when it opens, and the least it can be.
pub const WINDOW_SIZE: [f32; 2] = [860.0, 640.0];
pub const MIN_WINDOW_SIZE: [f32; 2] = [560.0, 420.0];

/// The preview is decoded no larger than this on its longer side.
const PREVIEW_SIDE: usize = 1920;

const CURSOR_COLOUR: Color32 = Color32::from_rgb(0xe5, 0x39, 0x35);

/// What the preview thread is asked.
enum Request {
    Frame(i64),
    Keys(Vec<i64>),
    /// Close the file and end the thread.
    Close,
}

/// What it answers: the file's headers first, then frames.
enum Reply {
    Info(Info),
    Picture(Picture),
    Error(String),
}

/// Which mark the pointer drags on the timeline.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mark {
    Cursor,
    Start,
    End,
}

/// A cut running on its own thread.
struct Export {
    progress: Arc<Progress>,
    done: mpsc::Receiver<Result<PathBuf, String>>,
    thread: Option<JoinHandle<()>>,
    /// The cut is written beside the source under another name, to take
    /// its place once complete.
    replace: bool,
}

/// Playback under way, up to `until`.
struct Playing {
    playback: Playback,
    until: i64,
}

/// The state of the window.
pub struct Editor {
    pub path: PathBuf,
    info: Option<Info>,
    /// Every frame of the file, once listed.
    frames: Option<Vec<Frame>>,
    frames_rx: Option<mpsc::Receiver<Result<Vec<Frame>, String>>>,
    /// Stops the listing and the preview when the window closes.
    cancel: Arc<Progress>,
    requests: mpsc::Sender<Request>,
    replies: mpsc::Receiver<Reply>,
    /// The frame on show and its time.
    texture: Option<(i64, egui::TextureHandle)>,
    /// The time last asked of the preview thread.
    asked: Option<i64>,
    /// The frame under the cursor, and the stretch kept: from `start`
    /// (a key frame) up to `end` (not included).
    cursor: i64,
    start: i64,
    end: i64,
    drag: Option<Mark>,
    playing: Option<Playing>,
    export: Option<Export>,
    /// The last cut written.
    saved: Option<PathBuf>,
    error: Option<String>,
    /// Whether a cut takes the place of the file (the file goes to the
    /// Recycle Bin) instead of going into a new one beside it.
    pub replace: bool,
    /// Whether the file on show is a cut that replaced the one opened:
    /// `Some(true)` when the original went to the Recycle Bin, `Some(false)`
    /// when it was deleted (a drive without one).
    replaced: Option<bool>,
    /// Whether `error` came from the preview, which a frame decoded later
    /// clears.
    preview_failed: bool,
    /// The threads reading the file, joined before it is replaced.
    readers: Vec<JoinHandle<()>>,
    ctx: egui::Context,
    viewport: egui::ViewportId,
    /// The window's handle once it exists (found by its title), and the
    /// size and radius its corners were last rounded for.
    window: Option<isize>,
    rounded: Option<(Vec2, i32)>,
}

impl Editor {
    /// Opens `path`: its headers come from the preview thread, its frames
    /// from another, both told to repaint `viewport` of `ctx`.
    pub fn open(path: PathBuf, ctx: egui::Context, viewport: egui::ViewportId) -> Editor {
        let cancel = Arc::new(Progress::default());
        let mut readers = Vec::new();
        let editor_ctx = ctx.clone();
        let (requests, request_rx) = mpsc::channel::<Request>();
        let (reply_tx, replies) = mpsc::channel();
        let preview_path = path.clone();
        let preview_ctx = ctx.clone();
        let spawned = std::thread::Builder::new().name("preview".into()).spawn(move || {
            let _com = win::com_init_mta();
            let mut preview = match trim::Preview::open(&preview_path) {
                Ok(p) => p,
                Err(e) => {
                    let _ = reply_tx.send(Reply::Error(win::describe(&e)));
                    preview_ctx.request_repaint_of(viewport);
                    return;
                }
            };
            let _ = reply_tx.send(Reply::Info(preview.info.clone()));
            preview_ctx.request_repaint_of(viewport);
            while let Ok(request) = request_rx.recv() {
                // Only the latest frame asked for is decoded.
                let mut wanted = None;
                let mut close = false;
                let mut handle = |request| match request {
                    Request::Frame(t) => wanted = Some(t),
                    Request::Keys(keys) => preview.set_keys(keys),
                    Request::Close => close = true,
                };
                handle(request);
                while let Ok(request) = request_rx.try_recv() {
                    handle(request);
                }
                if close {
                    break;
                }
                if let Some(time) = wanted {
                    let reply = match preview.frame(time, PREVIEW_SIDE) {
                        Ok(Some(picture)) => Reply::Picture(picture),
                        Ok(None) => continue,
                        Err(e) => Reply::Error(win::describe(&e)),
                    };
                    let _ = reply_tx.send(reply);
                    preview_ctx.request_repaint_of(viewport);
                }
            }
        });
        match spawned {
            Ok(thread) => readers.push(thread),
            Err(e) => log::error!("no preview thread: {e}"),
        }
        let (frames_tx, frames_rx) = mpsc::channel();
        let frames_path = path.clone();
        let frames_cancel = Arc::clone(&cancel);
        let frames_ctx = ctx;
        let spawned = std::thread::Builder::new().name("frames".into()).spawn(move || {
            let _com = win::com_init_mta();
            let _ = frames_tx.send(trim::frames(&frames_path, &frames_cancel.cancel).map_err(|e| win::describe(&e)));
            frames_ctx.request_repaint_of(viewport);
        });
        match spawned {
            Ok(thread) => readers.push(thread),
            Err(e) => log::error!("no frames thread: {e}"),
        }
        Editor {
            path,
            info: None,
            frames: None,
            frames_rx: Some(frames_rx),
            cancel,
            requests,
            replies,
            texture: None,
            asked: None,
            cursor: 0,
            start: 0,
            end: 0,
            drag: None,
            playing: None,
            export: None,
            saved: None,
            error: None,
            replace: false,
            replaced: None,
            preview_failed: false,
            readers,
            ctx: editor_ctx,
            viewport,
            window: None,
            rounded: None,
        }
    }

    /// The window's title: what the taskbar shows, and how the window is
    /// found for its rounded corners.
    pub fn title(&self) -> String {
        let name = self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        format!("{} — {name}", tr!("Trim", "Обрезка"))
    }

    /// The window's contents; `true` when it is to close.
    pub fn ui(&mut self, ui: &mut egui::Ui) -> bool {
        let ctx = ui.ctx().clone();
        self.poll(&ctx);
        self.follow();
        self.keys(ui);
        let mut close = ctx.input(|i| i.viewport().close_requested());
        // As the main window: no title bar of Windows, one rounded
        // background with a thin line at its edge over a transparent
        // window, the title row with the cross the program's own, and the
        // row drags the window.
        ui.painter().rect(
            ctx.content_rect(),
            CORNER_RADIUS,
            ui.visuals().panel_fill,
            egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
            egui::StrokeKind::Inside,
        );
        self.round_corners(&ctx);
        resize_edges(ui);
        egui::Panel::top("title")
            .show_separator_line(false)
            .frame(egui::Frame::new().inner_margin(egui::Margin { left: 12, right: 8, top: 8, bottom: 0 }))
            .show(ui, |ui| {
                let row = ui.max_rect();
                let drag = ui.interact(row, ui.id().with("drag"), Sense::click_and_drag());
                if drag.drag_started_by(egui::PointerButton::Primary) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }
                ui.allocate_ui_with_layout(Vec2::new(row.width(), 24.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.label(RichText::new(self.title()).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if title_button(ui, TitleButton::Close) {
                            close = true;
                        }
                        ui.add_space(4.0);
                        // The keys, on hover; the button does nothing else.
                        title_button(ui, TitleButton::Help(help_text()));
                    });
                });
            });
        egui::Panel::bottom("controls")
            .show_separator_line(false)
            .frame(egui::Frame::new().inner_margin(12))
            .show(ui, |ui| {
                close |= self.controls(ui);
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::new().inner_margin(egui::Margin {
                left: 12,
                right: 12,
                top: 12,
                bottom: 0,
            }))
            .show(ui, |ui| {
                self.preview(ui);
            });
        if close {
            self.cancel.cancel.store(true, Relaxed);
            if let Some(export) = &self.export {
                export.progress.cancel.store(true, Relaxed);
            }
        }
        close
    }

    /// Clips the window to rounded corners (`win::round_window`), again
    /// whenever its size changes; the window is found by its title once
    /// it exists. The caption is taken out of its style first, or Windows
    /// would paint its own title bar over the row (`win::strip_caption`).
    fn round_corners(&mut self, ctx: &egui::Context) {
        if self.window.is_none() {
            self.window = win::find_window(&self.title());
        }
        let Some(hwnd) = self.window else { return };
        if win::strip_caption(hwnd) {
            self.rounded = None;
        }
        let ppp = ctx.pixels_per_point();
        let wanted = ((ctx.content_rect().size() * ppp).round(), (CORNER_RADIUS * ppp).round() as i32 - 1);
        if self.rounded != Some(wanted) && win::round_window(hwnd, wanted.1) {
            self.rounded = Some(wanted);
        }
    }

    /// The window's handle, once it has been found.
    pub fn window(&self) -> Option<isize> {
        self.window
    }

    /// Results from the threads.
    fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(reply) = self.replies.try_recv() {
            match reply {
                Reply::Info(info) => {
                    self.end = info.duration;
                    self.info = Some(info);
                    self.ask(0);
                }
                Reply::Picture(picture) => {
                    let image = egui::ColorImage::from_rgba_unmultiplied([picture.width, picture.height], &picture.rgba);
                    match &mut self.texture {
                        Some((time, texture)) if texture.size() == [picture.width, picture.height] => {
                            texture.set(image, egui::TextureOptions::LINEAR);
                            *time = picture.time;
                        }
                        _ => self.texture = Some((picture.time, ctx.load_texture("preview", image, egui::TextureOptions::LINEAR))),
                    }
                    if self.preview_failed {
                        self.preview_failed = false;
                        self.error = None;
                    }
                }
                Reply::Error(e) => {
                    self.error = Some(e);
                    self.preview_failed = true;
                }
            }
        }
        if let Some(rx) = &self.frames_rx {
            match rx.try_recv() {
                Ok(Ok(frames)) => {
                    self.frames_rx = None;
                    let keys: Vec<i64> = frames.iter().filter(|f| f.key).map(|f| f.time).collect();
                    let _ = self.requests.send(Request::Keys(keys));
                    if let Some(last) = frames.last() {
                        // Headers without a length (0) leave the end to the
                        // frames.
                        let length = last.time + last.duration;
                        self.end = if self.end <= 0 { length } else { self.end.min(length) };
                    }
                    self.frames = Some(frames);
                    self.start = self.snap_start(self.start);
                    self.cursor = self.frame_at(self.cursor);
                }
                Ok(Err(e)) => {
                    self.frames_rx = None;
                    self.error = Some(e);
                }
                Err(mpsc::TryRecvError::Disconnected) => self.frames_rx = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(export) = &mut self.export {
            match export.done.try_recv() {
                Ok(Ok(path)) if export.replace => {
                    if let Some(thread) = export.thread.take() {
                        let _ = thread.join();
                    }
                    self.export = None;
                    self.take_place(path);
                }
                Ok(Ok(path)) => {
                    self.saved = Some(path);
                    self.error = None;
                    self.export = None;
                }
                Ok(Err(e)) => {
                    self.error = Some(e);
                    self.export = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => self.export = None,
                Err(mpsc::TryRecvError::Empty) => ctx.request_repaint_after(std::time::Duration::from_millis(100)),
            }
        }
    }

    /// Puts the cut written to `cut` in the place of the file: the readers
    /// let go of it, it goes to the Recycle Bin and the cut takes its name;
    /// then the window shows the cut. When the file cannot be moved (open
    /// in a player), the cut is deleted and the file stays as it was. When
    /// the file is gone but the cut cannot take its name (something holds
    /// the cut: a scanner, an indexer), the cut is kept under its own name
    /// and shown, so nothing is lost.
    fn take_place(&mut self, cut: PathBuf) {
        self.playing = None;
        let _ = self.requests.send(Request::Close);
        self.cancel.cancel.store(true, Relaxed);
        for thread in self.readers.drain(..) {
            let _ = thread.join();
        }
        let path = self.path.clone();
        let replace = self.replace;
        match win::recycle(&path) {
            Ok(recycled) => match std::fs::rename(&cut, &path) {
                Ok(()) => {
                    log::debug!("{} replaced by its cut", path.display());
                    *self = Editor::open(path, self.ctx.clone(), self.viewport);
                    self.replaced = Some(recycled);
                }
                Err(e) => {
                    log::warn!("{} is in the Recycle Bin, but the cut could not take its name: {e}", path.display());
                    let name = cut.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    *self = Editor::open(cut, self.ctx.clone(), self.viewport);
                    self.replaced = Some(recycled);
                    self.error = Some(format!(
                        "{}: {e}",
                        tr!(format!("The original is in the Recycle Bin; the cut stays as {name}"), format!("Исходный файл в корзине, обрезка осталась как {name}"))
                    ));
                }
            },
            Err(e) => {
                let _ = std::fs::remove_file(&cut);
                log::warn!("could not replace {}: {e}", path.display());
                *self = Editor::open(path, self.ctx.clone(), self.viewport);
                self.error = Some(format!("{}: {e}", tr!("The file could not be replaced", "Не удалось заменить файл")));
            }
        }
        self.replace = replace;
    }

    /// Asks the preview thread for the frame at `time`, once.
    fn ask(&mut self, time: i64) {
        if self.asked != Some(time) {
            self.asked = Some(time);
            let _ = self.requests.send(Request::Frame(time));
        }
    }

    /// The length of the file: of its frames, else as its headers say.
    fn length(&self) -> i64 {
        match (&self.frames, &self.info) {
            (Some(frames), _) if !frames.is_empty() => frames.last().map_or(0, |f| f.time + f.duration),
            (_, Some(info)) => info.duration,
            _ => 0,
        }
        .max(1)
    }

    /// The frames per second as the headers say, until the frames are
    /// listed.
    fn fps(&self) -> f64 {
        self.info.as_ref().map_or(30.0, |i| i.fps).max(1.0)
    }

    /// The length of one frame.
    fn step(&self) -> i64 {
        (SECOND as f64 / self.fps()).round().max(1.0) as i64
    }

    /// The time of frame `n` by the frame rate, rounded as the times in the
    /// file are.
    fn frame_time(&self, n: i64) -> i64 {
        (n as f64 * SECOND as f64 / self.fps()).round() as i64
    }

    /// The time of the frame shown at `time`.
    fn frame_at(&self, time: i64) -> i64 {
        let time = time.clamp(0, self.length() - 1);
        match &self.frames {
            Some(frames) if !frames.is_empty() => {
                let i = frames.partition_point(|f| f.time <= time).saturating_sub(1);
                frames[i].time
            }
            _ => {
                let n = (time as f64 * self.fps() / SECOND as f64).floor() as i64;
                // Rounding can put frame n just after `time`.
                if self.frame_time(n) > time { self.frame_time(n - 1) } else { self.frame_time(n) }
            }
        }
    }

    /// The frame `n` frames on from `time` (back when negative).
    fn frame_from(&self, time: i64, n: i64) -> i64 {
        match &self.frames {
            Some(frames) if !frames.is_empty() => {
                let i = frames.partition_point(|f| f.time <= time).saturating_sub(1) as i64;
                frames[(i + n).clamp(0, frames.len() as i64 - 1) as usize].time
            }
            _ => self.frame_at(time + n * self.step()),
        }
    }

    /// The key frame `n` key frames on from `time`; the file's first when
    /// none are listed yet.
    fn key_from(&self, time: i64, n: i64) -> i64 {
        let Some(frames) = &self.frames else { return 0 };
        let keys: Vec<i64> = frames.iter().filter(|f| f.key).map(|f| f.time).collect();
        if keys.is_empty() {
            return 0;
        }
        let i = keys.partition_point(|&k| k <= time) as i64 - 1;
        let i = if n > 0 {
            i + n
        } else {
            i + n + i64::from(keys.get(i.max(0) as usize).is_some_and(|&k| k < time))
        };
        keys[i.clamp(0, keys.len() as i64 - 1) as usize]
    }

    /// The key frame at or before `time`: where a cut starting there
    /// really starts.
    fn snap_start(&self, time: i64) -> i64 {
        let Some(frames) = &self.frames else {
            return self.frame_at(time);
        };
        frames.iter().rev().find(|f| f.key && f.time <= time).map_or(0, |f| f.time)
    }

    /// Moves the cursor by hand: playback stops.
    fn set_cursor(&mut self, time: i64) {
        self.playing = None;
        self.show(time);
    }

    /// The cursor on the frame shown at `time`, and that frame on show.
    fn show(&mut self, time: i64) {
        self.cursor = self.frame_at(time);
        self.ask(self.cursor);
    }

    /// Plays from `from` up to `until`, with sound.
    fn play(&mut self, from: i64, until: i64) {
        if self.info.is_none() || until <= from {
            return;
        }
        log::debug!("play from {} to {}", clock(from), clock(until));
        self.playing = None;
        self.show(from);
        self.playing = Some(Playing { playback: Playback::start(&self.path, from, until), until });
    }

    /// Plays from the cursor to the end of the file (from the start when
    /// the cursor is on the last frame), or pauses.
    fn toggle_play(&mut self) {
        if self.playing.take().is_some() {
            return;
        }
        let last = self.frame_at(self.length() - 1);
        let from = if self.cursor >= last { 0 } else { self.cursor };
        self.play(from, self.length());
    }

    /// Plays the stretch kept.
    fn play_cut(&mut self) {
        self.play(self.start, self.end);
    }

    /// The cursor follows playback; at the end the last frame played
    /// stays on show.
    fn follow(&mut self) {
        let Some(playing) = &self.playing else { return };
        let (position, until) = (playing.playback.position(), playing.until);
        if playing.playback.finished() || position >= until {
            self.playing = None;
            self.show(until - 1);
        } else {
            self.show(position);
            self.ctx.request_repaint_of(self.viewport);
        }
    }

    fn set_start(&mut self, time: i64) {
        self.start = self.snap_start(time);
        if self.end <= self.start {
            // The next frame, or the end of the file after the last one.
            let next = self.frame_from(self.start, 1);
            self.end = if next > self.start { next } else { self.length() };
        }
    }

    fn set_end(&mut self, time: i64) {
        // The end is the first frame not kept: a frame's start, or the
        // end of the file.
        self.end = if time >= self.length() { self.length() } else { self.frame_at(time) };
        if self.end <= self.start {
            self.start = self.snap_start(self.end - 1);
        }
    }

    /// The keys, in the order pressed (several can come in one frame).
    fn keys(&mut self, ui: &egui::Ui) {
        if self.info.is_none() || ui.ctx().egui_wants_keyboard_input() {
            return;
        }
        // Space belongs to the editor: a button that has the focus would
        // take it as a click.
        // Shift+Space first: the pattern without modifiers matches it too
        // (egui ignores an extra Shift).
        // The key's repeat while held down is consumed but does nothing.
        let first_press = ui.input(|i| {
            i.events
                .iter()
                .any(|e| matches!(e, egui::Event::Key { key: Key::Space, pressed: true, repeat: false, .. }))
        });
        let (cut, space) = ui.input_mut(|i| (i.consume_key(egui::Modifiers::SHIFT, Key::Space), i.consume_key(egui::Modifiers::NONE, Key::Space)));
        let (cut, space) = (cut && first_press, space && first_press);
        if cut {
            self.play_cut();
        } else if space {
            self.toggle_play();
        }
        let presses: Vec<(Key, egui::Modifiers)> = ui.input(|i| {
            i.events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::Key {
                        key,
                        pressed: true,
                        modifiers,
                        ..
                    } => Some((*key, *modifiers)),
                    _ => None,
                })
                .collect()
        });
        for (key, modifiers) in presses {
            let step = |this: &Editor, n: i64| {
                if modifiers.shift {
                    this.key_from(this.cursor, n)
                } else if modifiers.ctrl {
                    this.frame_at(this.cursor + n * SECOND)
                } else {
                    this.frame_from(this.cursor, n)
                }
            };
            match key {
                Key::ArrowLeft => self.set_cursor(step(self, -1)),
                Key::ArrowRight => self.set_cursor(step(self, 1)),
                Key::Home => self.set_cursor(0),
                Key::End => self.set_cursor(self.length() - 1),
                Key::I | Key::OpenBracket => self.set_start(self.cursor),
                Key::O | Key::CloseBracket => self.set_end(self.cursor),
                Key::S if modifiers.command && self.export.is_none() && self.end > self.start => self.save(),
                _ => {}
            }
        }
    }

    /// The frame on show, as large as the space allows.
    fn preview(&mut self, ui: &mut egui::Ui) {
        let avail = ui.available_size();
        match &self.texture {
            Some((_, texture)) => {
                let size = texture.size_vec2();
                let scale = (avail.x / size.x).min(avail.y / size.y).min(1.0 * ui.ctx().pixels_per_point());
                let shown = size * scale;
                let rect = egui::Rect::from_center_size(ui.max_rect().center(), shown);
                ui.painter().rect_filled(ui.max_rect(), 4.0, Color32::from_gray(24));
                ui.painter().image(
                    texture.id(),
                    rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
            None => {
                ui.painter().rect_filled(ui.max_rect(), 4.0, Color32::from_gray(24));
                let text = match &self.error {
                    Some(e) => e.clone(),
                    None => tr!("Opening…", "Открывается…").to_owned(),
                };
                ui.painter().text(
                    ui.max_rect().center(),
                    egui::Align2::CENTER_CENTER,
                    text,
                    egui::FontId::proportional(14.0),
                    Color32::from_gray(160),
                );
            }
        }
        ui.allocate_rect(ui.max_rect(), Sense::hover());
    }

    /// The timeline, the buttons and the status line; `true` when the
    /// window is to close.
    fn controls(&mut self, ui: &mut egui::Ui) -> bool {
        let ready = self.info.is_some();
        self.timeline(ui);
        ui.add_space(6.0);
        let close = false;
        ui.horizontal(|ui| {
            ui.add_enabled_ui(ready && self.export.is_none(), |ui| {
                let button =
                    |ui: &mut egui::Ui, text: &str, hint: &str| ui.add(egui::Button::new(text).min_size(Vec2::new(32.0, 26.0))).on_hover_text(hint);
                let playing = self.playing.is_some();
                let hint = if playing {
                    tr!("Pause (Space)", "Пауза (пробел)")
                } else {
                    tr!("Play from the cursor (Space)", "Воспроизвести с курсора (пробел)")
                };
                if play_button(ui, playing).on_hover_text(hint).clicked() {
                    self.toggle_play();
                }
                ui.add_space(6.0);
                if button(ui, "⏮", tr!("First frame (Home)", "Первый кадр (Home)")).clicked() {
                    self.set_cursor(0);
                }
                if button(
                    ui,
                    "◀",
                    tr!(
                        "A frame back (Left; Shift: a key frame, Ctrl: a second)",
                        "Кадр назад (стрелка влево; Shift — ключевой кадр, Ctrl — секунда)"
                    ),
                )
                .clicked()
                {
                    self.set_cursor(self.frame_from(self.cursor, -1));
                }
                if button(
                    ui,
                    "▶",
                    tr!(
                        "A frame on (Right; Shift: a key frame, Ctrl: a second)",
                        "Кадр вперёд (стрелка вправо; Shift — ключевой кадр, Ctrl — секунда)"
                    ),
                )
                .clicked()
                {
                    self.set_cursor(self.frame_from(self.cursor, 1));
                }
                if button(ui, "⏭", tr!("Last frame (End)", "Последний кадр (End)")).clicked() {
                    self.set_cursor(self.length() - 1);
                }
                ui.add_space(8.0);
                ui.label(RichText::new(clock(self.cursor)).monospace().strong());
                ui.add_space(8.0);
                if ui
                    .button(tr!("[ Start here", "[ Начало здесь"))
                    .on_hover_text(tr!(
                        "The stretch kept starts on the key frame at or before this one (I)",
                        "Оставляемый фрагмент начнётся с ключевого кадра на этом или перед ним (I)"
                    ))
                    .clicked()
                {
                    self.set_start(self.cursor);
                }
                if ui
                    .button(tr!("End here ]", "Конец здесь ]"))
                    .on_hover_text(tr!("This frame is the first left out (O)", "Этот кадр — первый из отброшенных (O)"))
                    .clicked()
                {
                    self.set_end(self.cursor);
                }
                ui.add_space(8.0);
                if ui.button(tr!("To start", "К началу")).clicked() {
                    self.set_cursor(self.start);
                }
                if ui.button(tr!("To end", "К концу")).clicked() {
                    self.set_cursor(self.frame_at(self.end - 1));
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let can_save = ready && self.export.is_none() && self.end > self.start;
                let save = egui::Button::new(RichText::new(tr!("Save the cut", "Сохранить фрагмент")).strong()).min_size(Vec2::new(0.0, 26.0));
                let hint = if self.replace {
                    tr!(
                        "In place of this file, without re-encoding; this file goes to the Recycle Bin (Ctrl+S)",
                        "Вместо этого файла, без перекодирования; этот файл уходит в корзину (Ctrl+S)"
                    )
                } else {
                    tr!(
                        "Into a new file beside this one, without re-encoding (Ctrl+S)",
                        "В новый файл рядом с этим, без перекодирования (Ctrl+S)"
                    )
                };
                if ui.add_enabled(can_save, save).on_hover_text(hint).clicked() {
                    self.save();
                }
                if let Some(export) = &self.export
                    && ui.button(tr!("Cancel", "Отмена")).clicked()
                {
                    export.progress.cancel.store(true, Relaxed);
                }
            });
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let kept = self.end - self.start;
            ui.label(
                RichText::new(format!(
                    "{}: {}   {}: {}   {}: {}",
                    tr!("Start", "Начало"),
                    clock(self.start),
                    tr!("End", "Конец"),
                    clock(self.end),
                    tr!("Kept", "Остаётся"),
                    clock(kept.max(0))
                ))
                .monospace(),
            );
            if ui
                .add_enabled(self.info.is_some() && kept > 0, egui::Button::new(tr!("▶ Play", "▶ Проиграть")).small())
                .on_hover_text(tr!("Play the stretch kept (Shift+Space)", "Проиграть оставляемый фрагмент (Shift+пробел)"))
                .clicked()
            {
                self.play_cut();
            }
            if let Some(info) = &self.info {
                ui.weak(format!("{}×{}, {} {}", info.width, info.height, fmt_fps(info.fps), tr!("fps", "к/с")));
                if info.other_audio {
                    ui.label(RichText::new(tr!("the sound is not AAC: not played, not kept", "звук не в AAC: не воспроизводится и не сохраняется")).color(ui.visuals().warn_fg_color));
                }
                if self.frames_rx.is_some() {
                    ui.weak(tr!("listing the frames…", "кадры перечисляются…"));
                }
            }
            // Under the save button, which it changes.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_enabled_ui(self.export.is_none(), |ui| {
                    ui.checkbox(&mut self.replace, tr!("Replace the original", "Заменить исходный файл")).on_hover_text(tr!(
                        "The cut takes the place of this file instead of going into a new one; this file goes to the Recycle Bin.",
                        "Фрагмент сохраняется вместо этого файла, а не в новый; этот файл уходит в корзину."
                    ));
                });
            });
        });
        ui.add_space(2.0);
        // A line, empty or not, so the preview above keeps its size.
        let height = ui.text_style_height(&egui::TextStyle::Body) + 4.0;
        ui.allocate_ui_with_layout(Vec2::new(ui.available_width(), height), egui::Layout::top_down(egui::Align::LEFT), |ui| {
            ui.set_min_height(height);
            ui.horizontal_wrapped(|ui| {
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
            if let Some(export) = &self.export {
                let done = export.progress.done.load(Relaxed);
                ui.add(egui::ProgressBar::new(done as f32 / 1000.0).desired_width(160.0).show_percentage());
                ui.weak(tr!("Saving…", "Сохраняется…"));
            } else if let Some(e) = &self.error {
                ui.label(RichText::new(e).color(ui.visuals().error_fg_color));
            } else if let (Some(recycled), None) = (self.replaced, &self.saved) {
                ui.label(if recycled {
                    tr!("The file was replaced by the cut; the original is in the Recycle Bin.", "Файл заменён фрагментом; исходный — в корзине.")
                } else {
                    tr!(
                        "The file was replaced by the cut; the original is deleted (the drive has no Recycle Bin).",
                        "Файл заменён фрагментом; исходный удалён (у диска нет корзины)."
                    )
                });
            } else if let Some(path) = &self.saved {
                ui.label(tr!("Saved:", "Сохранено:"));
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                if ui.link(name).on_hover_text(tr!("Show in Explorer", "Показать в Проводнике")).clicked() {
                    win::show_in_explorer(path);
                }
            }
            });
        });
        close
    }

    /// The timeline: the stretch kept, the key frames, the marks of its
    /// start and end, and the cursor.
    fn timeline(&mut self, ui: &mut egui::Ui) {
        const HEIGHT: f32 = 44.0;
        const GRAB: f32 = 8.0;
        let (rect, response) = ui.allocate_exact_size(Vec2::new(ui.available_width(), HEIGHT), Sense::click_and_drag());
        let length = self.length();
        let to_x = |t: i64| rect.left() + (t as f64 / length as f64) as f32 * rect.width();
        let to_t = |x: f32| (((x - rect.left()) / rect.width()).clamp(0.0, 1.0) as f64 * length as f64) as i64;
        let visuals = ui.visuals();
        let painter = ui.painter_at(rect);
        let bar = egui::Rect::from_min_max(egui::pos2(rect.left(), rect.top() + 8.0), egui::pos2(rect.right(), rect.bottom() - 10.0));
        painter.rect_filled(bar, 3.0, visuals.extreme_bg_color);
        let kept = egui::Rect::from_min_max(egui::pos2(to_x(self.start), bar.top()), egui::pos2(to_x(self.end), bar.bottom()));
        painter.rect_filled(kept, 0.0, visuals.selection.bg_fill);
        // Key frames as ticks under the bar, while they are at least a
        // few pixels apart.
        if let Some(frames) = &self.frames {
            let keys = frames.iter().filter(|f| f.key).count().max(1);
            if rect.width() / keys as f32 >= 3.0 {
                let stroke = egui::Stroke::new(1.0, visuals.weak_text_color());
                for f in frames.iter().filter(|f| f.key) {
                    let x = to_x(f.time);
                    painter.line_segment([egui::pos2(x, bar.bottom() + 1.0), egui::pos2(x, rect.bottom())], stroke);
                }
            }
        }
        let mark = |t: i64, colour: Color32, start: bool| {
            let x = to_x(t);
            painter.line_segment([egui::pos2(x, rect.top()), egui::pos2(x, bar.bottom())], egui::Stroke::new(2.0, colour));
            let w = if start { 6.0 } else { -6.0 };
            painter.rect_filled(
                egui::Rect::from_two_pos(egui::pos2(x, rect.top()), egui::pos2(x + w, rect.top() + 7.0)),
                0.0,
                colour,
            );
        };
        mark(self.start, visuals.selection.stroke.color, true);
        mark(self.end, visuals.selection.stroke.color, false);
        let x = to_x(self.cursor);
        painter.line_segment(
            [egui::pos2(x, rect.top()), egui::pos2(x, bar.bottom())],
            egui::Stroke::new(2.0, CURSOR_COLOUR),
        );

        if self.info.is_none() {
            return;
        }
        if response.drag_started() || response.clicked() {
            let near = |t: i64| response.interact_pointer_pos().is_some_and(|p| (p.x - to_x(t)).abs() <= GRAB);
            self.drag = Some(if near(self.start) {
                Mark::Start
            } else if near(self.end) {
                Mark::End
            } else {
                Mark::Cursor
            });
        }
        if let (Some(mark), Some(pos)) = (self.drag, response.interact_pointer_pos()) {
            let t = to_t(pos.x);
            match mark {
                Mark::Cursor => self.set_cursor(t),
                Mark::Start => {
                    self.set_start(t);
                    self.set_cursor(self.start);
                }
                Mark::End => {
                    self.set_end(t);
                    self.set_cursor(self.end - 1);
                }
            }
        }
        if response.drag_stopped() || (!response.dragged() && !response.clicked()) {
            self.drag = None;
        }
        if let Some(pos) = response.hover_pos() {
            response.on_hover_text_at_pointer(clock(self.frame_at(to_t(pos.x))));
        }
    }

    /// Starts the cut into a new file beside the source.
    fn save(&mut self) {
        self.playing = None;
        let replace = self.replace;
        let dst = if replace { replacement_path(&self.path) } else { cut_path(&self.path, self.start, self.end) };
        let (src, start, end) = (self.path.clone(), self.start, self.end);
        let progress = Arc::new(Progress::default());
        let worker = Arc::clone(&progress);
        let (tx, done) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("cut".into())
            .spawn(move || {
                let _com = win::com_init_mta();
                let result = match trim::cut(&src, &dst, start, end, &worker) {
                    Ok(cut) => {
                        log::debug!("cut {} frames from {} into {}", cut.frames, cut.start, dst.display());
                        Ok(dst)
                    }
                    Err(e) if worker.cancel.load(Relaxed) => Err(tr!("Cancelled", "Отменено").to_owned() + &format!(" ({})", win::describe(&e))),
                    Err(e) => Err(win::describe(&e)),
                };
                let _ = tx.send(result);
            })
            .ok();
        self.saved = None;
        self.error = None;
        self.replaced = None;
        if thread.is_none() {
            self.error = Some(tr!("The cut could not be started", "Не удалось начать сохранение").into());
            return;
        }
        self.export = Some(Export { progress, done, thread, replace });
    }
}

impl Drop for Editor {
    /// Waits for the threads, so the file is no longer open once the
    /// editor is gone (it may be deleted next). A cut into a new file is
    /// let finish (a copy without re-encoding takes seconds), so closing
    /// the program or opening another file does not lose it; one that
    /// would replace the file is cancelled, which leaves the file as it
    /// was.
    fn drop(&mut self) {
        self.cancel.cancel.store(true, Relaxed);
        if let Some(mut export) = self.export.take() {
            if export.replace {
                export.progress.cancel.store(true, Relaxed);
            } else {
                log::debug!("waiting for the cut to finish");
            }
            if let Some(thread) = export.thread.take() {
                let _ = thread.join();
            }
        }
        let _ = self.requests.send(Request::Close);
        for thread in self.readers.drain(..) {
            let _ = thread.join();
        }
    }
}

/// The tooltip of the "i" in the title row: the keys, and where a cut can
/// start.
fn help_text() -> &'static str {
    tr!(
        "Space — play or pause\n\
         Shift+Space — play the stretch kept\n\
         Left, Right — a frame back or on\n    \
         with Shift — a key frame, with Ctrl — a second\n\
         Home, End — the first or the last frame\n\
         I, O — the start or the end of the stretch here\n\
         Ctrl+S — save the cut\n\n\
         The stretch starts on a key frame (the marks under the timeline), the one at or before the frame chosen; it ends on any frame.",
        "Пробел — воспроизвести или поставить на паузу\n\
         Shift+пробел — проиграть оставляемый фрагмент\n\
         Стрелки влево и вправо — кадр назад или вперёд\n    \
         с Shift — ключевой кадр, с Ctrl — секунда\n\
         Home, End — первый или последний кадр\n\
         I, O — начало или конец фрагмента здесь\n\
         Ctrl+S — сохранить фрагмент\n\n\
         Фрагмент начинается на ключевом кадре (отметки под шкалой) — на выбранном или предшествующем ему; заканчивается на любом кадре."
    )
}

/// The play button: a triangle, or two bars while playing.
fn play_button(ui: &mut egui::Ui, playing: bool) -> egui::Response {
    let response = ui.add(egui::Button::new("").min_size(Vec2::new(40.0, 26.0)));
    let colour = ui.style().interact(&response).fg_stroke.color;
    let c = response.rect.center();
    let painter = ui.painter();
    if playing {
        for x in [-3.5, 3.5] {
            painter.rect_filled(egui::Rect::from_center_size(c + Vec2::new(x, 0.0), Vec2::new(3.5, 12.0)), 0.5, colour);
        }
    } else {
        let points = vec![c + Vec2::new(-4.5, -6.5), c + Vec2::new(6.5, 0.0), c + Vec2::new(-4.5, 6.5)];
        painter.add(egui::Shape::convex_polygon(points, colour, egui::Stroke::NONE));
    }
    response
}

/// The edges and corners of the window resize it, as a sizing frame would
/// (the window has none, see `App::editor_window`): a strip a few points
/// wide along each edge, with the resize pointer, hands a drag to Windows
/// (`ViewportCommand::BeginResize`). Placed before the panels, so their
/// widgets near an edge still take clicks first.
fn resize_edges(ui: &mut egui::Ui) {
    use egui::{CursorIcon, ResizeDirection as D};
    const EDGE: f32 = 5.0;
    const CORNER: f32 = 14.0;
    let r = ui.ctx().content_rect();
    let grips = [
        (D::NorthWest, egui::Rect::from_min_size(r.left_top(), Vec2::splat(CORNER)), CursorIcon::ResizeNwSe),
        (D::NorthEast, egui::Rect::from_min_size(r.right_top() - Vec2::new(CORNER, 0.0), Vec2::splat(CORNER)), CursorIcon::ResizeNeSw),
        (D::SouthWest, egui::Rect::from_min_size(r.left_bottom() - Vec2::new(0.0, CORNER), Vec2::splat(CORNER)), CursorIcon::ResizeNeSw),
        (D::SouthEast, egui::Rect::from_min_size(r.right_bottom() - Vec2::splat(CORNER), Vec2::splat(CORNER)), CursorIcon::ResizeNwSe),
        (D::North, egui::Rect::from_min_max(r.left_top(), egui::pos2(r.right(), r.top() + EDGE)), CursorIcon::ResizeVertical),
        (D::South, egui::Rect::from_min_max(egui::pos2(r.left(), r.bottom() - EDGE), r.right_bottom()), CursorIcon::ResizeVertical),
        (D::West, egui::Rect::from_min_max(r.left_top(), egui::pos2(r.left() + EDGE, r.bottom())), CursorIcon::ResizeHorizontal),
        (D::East, egui::Rect::from_min_max(egui::pos2(r.right() - EDGE, r.top()), r.right_bottom()), CursorIcon::ResizeHorizontal),
    ];
    // Corners first: where a corner and an edge overlap, the corner wins.
    let pointer = ui.ctx().pointer_hover_pos();
    let Some((direction, rect, icon)) = grips.into_iter().find(|(_, rect, _)| pointer.is_some_and(|p| rect.contains(p))) else { return };
    let response = ui.interact(rect, ui.id().with(("resize", direction as u8)), Sense::drag());
    if response.hovered() || response.dragged() {
        ui.ctx().set_cursor_icon(icon);
    }
    if response.drag_started_by(egui::PointerButton::Primary) {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::BeginResize(direction));
    }
}

/// Where a cut that is to replace `src` is written first: beside it,
/// `<name>.trimming.mp4` (left over from an interrupted cut, overwritten).
fn replacement_path(src: &Path) -> PathBuf {
    let stem = src.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "cut".into());
    src.with_file_name(format!("{stem}.trimming.mp4"))
}

/// `<name>_<start>-<end>.mp4` beside `src`, with a counter when taken.
fn cut_path(src: &Path, start: i64, end: i64) -> PathBuf {
    let stem = src.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "cut".into());
    let folder = src.parent().map(Path::to_path_buf).unwrap_or_default();
    let base = format!("{stem}_{}-{}", file_clock(start), file_clock(end));
    let mut path = folder.join(format!("{base}.mp4"));
    let mut n = 2;
    while path.exists() {
        path = folder.join(format!("{base}_{n}.mp4"));
        n += 1;
    }
    path
}

/// `m:ss.mmm`, with hours when there are any.
pub fn clock(time: i64) -> String {
    let ms = time.max(0) / 10_000;
    let (h, m, s, ms) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}.{ms:03}")
    } else {
        format!("{m}:{s:02}.{ms:03}")
    }
}

/// The time for a file name: `m.ss.mmm` (no colons).
fn file_clock(time: i64) -> String {
    clock(time).replace(':', ".")
}

fn fmt_fps(fps: f64) -> String {
    if (fps - fps.round()).abs() < 0.01 {
        format!("{}", fps.round() as i64)
    } else {
        format!("{fps:.2}")
    }
}

#[cfg(test)]
mod tests {
    use super::{clock, cut_path};

    #[test]
    fn formats_times() {
        assert_eq!(clock(0), "0:00.000");
        assert_eq!(clock(12_345_000), "0:01.234");
        assert_eq!(clock(3_600 * 10_000_000 + 65 * 10_000_000), "1:01:05.000");
    }

    #[test]
    fn names_the_cut() {
        let p = cut_path(std::path::Path::new("C:/none/qrec_1.mp4"), 10_000_000, 25_000_000);
        assert_eq!(p.to_string_lossy().replace('\\', "/"), "C:/none/qrec_1_0.01.000-0.02.500.mp4");
    }
}
