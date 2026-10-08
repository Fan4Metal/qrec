//! The window: the area, the few options, the record button and what
//! the last recording came to.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use egui::{Color32, RichText, Vec2};

use crate::display::{self, Monitor};
use crate::hotkey::{Chord, Hotkey};
use crate::overlay::{self, Border};
use crate::recorder::{Config, Quality, Recorder};
use crate::region::Region;
use crate::win;

/// The window's size in points.
pub const WINDOW_SIZE: [f32; 2] = [460.0, 318.0];

const MONITOR_KEY: &str = "monitor";
const REGION_KEY: &str = "region";
const FPS_KEY: &str = "fps";
const QUALITY_KEY: &str = "quality";
const AUDIO_KEY: &str = "audio";
const CURSOR_KEY: &str = "cursor";
const FOLDER_KEY: &str = "folder";
const HOTKEY_KEY: &str = "hotkey";

const RECORD_COLOUR: Color32 = Color32::from_rgb(0xd3, 0x2f, 0x2f);
const STOP_COLOUR: Color32 = Color32::from_rgb(0x45, 0x45, 0x45);

pub struct App {
    monitors: Vec<Monitor>,
    /// Index into `monitors`.
    monitor: usize,
    /// The area, on the virtual screen; `None` records the whole display.
    region: Option<Region>,
    fps: u32,
    quality: Quality,
    audio: bool,
    cursor: bool,
    folder: PathBuf,
    chord: Chord,
    hotkey: Option<(Hotkey, mpsc::Receiver<()>)>,
    hotkey_error: Option<String>,
    /// The next key press becomes the hotkey.
    capturing_hotkey: bool,
    recorder: Option<Recorder>,
    border: Option<Border>,
    selecting: Option<mpsc::Receiver<Option<Region>>>,
    notice: Notice,
}

/// The status line.
enum Notice {
    None,
    Saved(PathBuf),
    Error(String),
    Info(String),
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> App {
        let get = |key: &str| cc.storage.and_then(|s| s.get_string(key));
        let monitors = display::monitors();
        let monitor = get(MONITOR_KEY).and_then(|name| monitors.iter().position(|m| m.device_name == name)).unwrap_or(0);
        let region = get(REGION_KEY)
            .and_then(|r| Region::from_setting(&r))
            .filter(|r| monitors.get(monitor).is_some_and(|m| r.fit(m.rect) == Some(*r)));
        let fps = match get(FPS_KEY).as_deref() {
            Some("60") => 60,
            _ => 30,
        };
        let quality = get(QUALITY_KEY).and_then(|q| Quality::from_name(&q)).unwrap_or_default();
        let audio = get(AUDIO_KEY).as_deref() != Some("false");
        let cursor = get(CURSOR_KEY).as_deref() != Some("false");
        let folder = get(FOLDER_KEY).map(PathBuf::from).unwrap_or_else(default_folder);
        let chord = get(HOTKEY_KEY).and_then(|h| Chord::from_label(&h)).filter(Chord::is_usable).unwrap_or_default();

        // The window must not appear in its own recordings.
        {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            if let Ok(RawWindowHandle::Win32(w)) = cc.window_handle().map(|h| h.as_raw()) {
                win::exclude_from_capture(w.hwnd.get());
                log::debug!("window at {:?}", win::window_rect(w.hwnd.get()));
            }
        }

        // Light, whatever Windows uses, like the other small tools.
        cc.egui_ctx.set_theme(egui::ThemePreference::Light);
        let mut app = App {
            monitors,
            monitor,
            region,
            fps,
            quality,
            audio,
            cursor,
            folder,
            chord,
            hotkey: None,
            hotkey_error: None,
            capturing_hotkey: false,
            recorder: None,
            border: None,
            selecting: None,
            notice: Notice::None,
        };
        app.register_hotkey(&cc.egui_ctx);
        app
    }

    fn register_hotkey(&mut self, ctx: &egui::Context) {
        self.hotkey = None;
        match Hotkey::register(self.chord, ctx.clone()) {
            Ok(registered) => {
                self.hotkey = Some(registered);
                self.hotkey_error = None;
            }
            Err(e) => self.hotkey_error = Some(e),
        }
    }

    /// Results from the other threads: the selection, the hotkey, a
    /// recording that failed.
    fn poll(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.selecting {
            match rx.try_recv() {
                Ok(result) => {
                    self.selecting = None;
                    match result {
                        Some(region) => {
                            if let Some(i) = display::monitor_of(&self.monitors, &region.rect())
                                .and_then(|m| self.monitors.iter().position(|x| x == m))
                            {
                                self.monitor = i;
                            }
                            self.region = Some(region);
                            self.notice = Notice::None;
                        }
                        None => {
                            self.notice =
                                Notice::Info(tr!("Selection cancelled or too small", "Выделение отменено или слишком мало").into())
                        }
                    }
                    ctx.request_repaint();
                }
                Err(mpsc::TryRecvError::Disconnected) => self.selecting = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        let mut presses = 0;
        if let Some((_, rx)) = &self.hotkey {
            while rx.try_recv().is_ok() {
                presses += 1;
            }
        }
        if presses % 2 == 1 && self.selecting.is_none() {
            self.toggle();
        }
        if self.recorder.as_ref().is_some_and(Recorder::failed) {
            self.stop();
        }
    }

    fn toggle(&mut self) {
        if self.recorder.is_some() {
            self.stop();
        } else {
            self.start();
        }
    }

    fn start(&mut self) {
        self.monitors = display::monitors();
        let Some(monitor) = self.monitors.get(self.monitor).cloned() else {
            self.monitor = 0;
            self.notice = Notice::Error(tr!("No display found", "Экран не найден").into());
            return;
        };
        let region = self.region.unwrap_or_else(|| Region::from_rect(monitor.rect));
        let Some(region) = region.fit(monitor.rect) else {
            self.region = None;
            self.notice = Notice::Error(tr!("The area is outside the display", "Область вне экрана").into());
            return;
        };
        if let Err(e) = std::fs::create_dir_all(&self.folder) {
            self.notice = Notice::Error(format!("{}: {e}", tr!("Cannot create the folder", "Не удаётся создать папку")));
            return;
        }
        let path = unique_path(&self.folder);
        let config = Config { monitor, region, fps: self.fps, quality: self.quality, audio: self.audio, cursor: self.cursor, path };
        match Recorder::start(config) {
            Ok(recorder) => {
                self.border = Some(Border::show(region));
                let encoder = if recorder.info.hardware {
                    tr!("hardware encoder", "аппаратный кодер")
                } else {
                    tr!("software encoder", "программный кодер")
                };
                self.notice = Notice::Info(format!("{}×{}, {} fps, {encoder}", region.width, region.height, self.fps));
                self.recorder = Some(recorder);
            }
            Err(e) => self.notice = Notice::Error(e),
        }
    }

    fn stop(&mut self) {
        self.border = None;
        if let Some(recorder) = self.recorder.take() {
            let path = recorder.path.clone();
            self.notice = match recorder.stop() {
                Ok(_) => Notice::Saved(path),
                Err(e) => Notice::Error(e),
            };
        }
    }

    fn select_area(&mut self) {
        self.monitors = display::monitors();
        self.selecting = Some(overlay::select(self.monitors.clone()));
    }

    /// The next key press, while the hotkey is being chosen.
    fn capture_hotkey(&mut self, ctx: &egui::Context) {
        let events = ctx.input(|i| i.events.clone());
        for event in events {
            if let egui::Event::Key { key, pressed: true, modifiers, .. } = event {
                if key == egui::Key::Escape {
                    self.capturing_hotkey = false;
                    self.register_hotkey(ctx);
                    return;
                }
                if let Some(chord) = Chord::from_egui(key, modifiers)
                    && chord.is_usable()
                {
                    self.chord = chord;
                    self.capturing_hotkey = false;
                    self.register_hotkey(ctx);
                    return;
                }
            }
        }
    }

    fn area_label(&self) -> String {
        match self.region {
            Some(r) => format!("{}×{} {} ({}, {})", r.width, r.height, tr!("at", "в точке"), r.x, r.y),
            None => tr!("Whole display", "Весь экран").into(),
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll(&ctx);
        if self.capturing_hotkey {
            self.capture_hotkey(&ctx);
        }
        let recording = self.recorder.is_some();
        let busy = recording || self.selecting.is_some();

        ui.add_space(6.0);
        egui::Grid::new("settings").num_columns(2).spacing([16.0, 10.0]).show(ui, |ui| {
            ui.label(tr!("Display", "Экран"));
            ui.add_enabled_ui(!busy, |ui| {
                let current = self.monitors.get(self.monitor).map(|m| m.label(self.monitor)).unwrap_or_default();
                egui::ComboBox::from_id_salt("monitor").selected_text(current).width(300.0).show_ui(ui, |ui| {
                    for (i, m) in self.monitors.iter().enumerate() {
                        if ui.selectable_label(self.monitor == i, m.label(i)).clicked() && self.monitor != i {
                            self.monitor = i;
                            self.region = None;
                        }
                    }
                });
            });
            ui.end_row();

            ui.label(tr!("Area", "Область"));
            ui.horizontal(|ui| {
                ui.add_enabled_ui(!busy, |ui| {
                    if ui.button(tr!("Select…", "Выделить…")).clicked() {
                        self.select_area();
                    }
                    if ui.add_enabled(self.region.is_some(), egui::Button::new(tr!("Whole display", "Весь экран"))).clicked() {
                        self.region = None;
                    }
                });
                ui.label(self.area_label());
            });
            ui.end_row();

            ui.label(tr!("Frame rate", "Частота кадров"));
            ui.add_enabled_ui(!recording, |ui| {
                ui.horizontal(|ui| {
                    ui.radio_value(&mut self.fps, 30, "30");
                    ui.radio_value(&mut self.fps, 60, "60");
                });
            });
            ui.end_row();

            ui.label(tr!("Quality", "Качество"));
            ui.add_enabled_ui(!recording, |ui| {
                ui.horizontal(|ui| {
                    for q in Quality::ALL {
                        ui.radio_value(&mut self.quality, q, q.label());
                    }
                });
            });
            ui.end_row();

            ui.label(tr!("Record", "Записывать"));
            ui.add_enabled_ui(!recording, |ui| {
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.audio, tr!("System sound", "Звук системы"));
                    ui.add_space(8.0);
                    ui.checkbox(&mut self.cursor, tr!("Pointer", "Указатель"));
                });
            });
            ui.end_row();

            ui.label(tr!("Folder", "Папка"));
            ui.horizontal(|ui| {
                let text = elide(&self.folder.display().to_string(), 36);
                let button = ui.add_enabled(!recording, egui::Button::new(text)).on_hover_text(self.folder.display().to_string());
                if button.clicked()
                    && let Some(folder) = rfd::FileDialog::new().set_directory(&self.folder).pick_folder()
                {
                    self.folder = folder;
                }
                if ui.button(tr!("Open", "Открыть")).on_hover_text(tr!("Open the folder in Explorer", "Открыть папку в Проводнике")).clicked() {
                    win::open_folder(&self.folder);
                }
            });
            ui.end_row();

            ui.label(tr!("Hotkey", "Клавиша"));
            ui.horizontal(|ui| {
                let text = if self.capturing_hotkey { tr!("Press the keys…", "Нажмите клавиши…").to_owned() } else { self.chord.label() };
                let button = ui.add_enabled(!recording, egui::Button::new(text)).on_hover_text(tr!(
                    "Starts and stops the recording from anywhere. Click, then press a key with Ctrl, Alt or Win, or a function key.",
                    "Начинает и останавливает запись из любого окна. Нажмите кнопку, затем клавишу с Ctrl, Alt или Win, или функциональную клавишу."
                ));
                if button.clicked() {
                    self.capturing_hotkey = !self.capturing_hotkey;
                    if self.capturing_hotkey {
                        // Free the key while another is chosen.
                        self.hotkey = None;
                    } else {
                        self.register_hotkey(&ctx);
                    }
                }
                if let Some(e) = &self.hotkey_error {
                    // Usually another program (or another qrec) holds the key.
                    ui.label(RichText::new(tr!("taken by another program", "занята другой программой")).color(ui.visuals().error_fg_color))
                        .on_hover_text(e);
                }
            });
            ui.end_row();
        });

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let (text, colour) = if recording { (tr!("Stop", "Стоп"), STOP_COLOUR) } else { (tr!("Record", "Запись"), RECORD_COLOUR) };
            // Room on the left for the symbol, drawn by hand: the fonts have none.
            let button = egui::Button::new(RichText::new(format!("      {text}")).size(17.0).color(Color32::WHITE))
                .fill(colour)
                .min_size(Vec2::new(150.0, 38.0));
            let response = ui.add_enabled(self.selecting.is_none(), button);
            let centre = response.rect.left_center() + Vec2::new(24.0, 0.0);
            if recording {
                ui.painter().rect_filled(egui::Rect::from_center_size(centre, Vec2::splat(13.0)), 2.0, Color32::WHITE);
            } else {
                ui.painter().circle_filled(centre, 7.5, Color32::WHITE);
            }
            if response.clicked() {
                self.toggle();
            }
            if let Some(recorder) = &self.recorder {
                let elapsed = recorder.started.elapsed().as_secs();
                let clock = format!("{:02}:{:02}:{:02}", elapsed / 3600, elapsed / 60 % 60, elapsed % 60);
                ui.add_space(12.0);
                ui.label(RichText::new(clock).size(22.0).monospace());
                let dropped = recorder.dropped();
                if dropped > 0 {
                    ui.label(RichText::new(format!("{} {dropped}", tr!("skipped frames:", "пропущено кадров:"))).small());
                }
            }
        });
        ui.add_space(6.0);
        match &self.notice {
            Notice::None => {}
            Notice::Saved(path) => {
                ui.horizontal(|ui| {
                    ui.label(tr!("Saved:", "Сохранено:"));
                    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    if ui.link(name).on_hover_text(tr!("Show in Explorer", "Показать в Проводнике")).clicked() {
                        win::show_in_explorer(path);
                    }
                });
            }
            Notice::Error(e) => {
                ui.label(RichText::new(e).color(ui.visuals().error_fg_color));
            }
            Notice::Info(text) => {
                ui.label(RichText::new(text).weak());
            }
        }

        ui.with_layout(egui::Layout::bottom_up(egui::Align::RIGHT), |ui| {
            ui.label(RichText::new(format!("qrec {}", crate::VERSION)).small().weak());
        });

        if busy {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
    }

    /// eframe clears to near-black by default; the panel colour of the
    /// theme keeps the window light.
    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        visuals.panel_fill.to_normalized_gamma_f32()
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        if let Some(m) = self.monitors.get(self.monitor) {
            storage.set_string(MONITOR_KEY, m.device_name.clone());
        }
        storage.set_string(REGION_KEY, self.region.map(|r| r.to_setting()).unwrap_or_default());
        storage.set_string(FPS_KEY, self.fps.to_string());
        storage.set_string(QUALITY_KEY, self.quality.name().to_owned());
        storage.set_string(AUDIO_KEY, self.audio.to_string());
        storage.set_string(CURSOR_KEY, self.cursor.to_string());
        storage.set_string(FOLDER_KEY, self.folder.display().to_string());
        storage.set_string(HOTKEY_KEY, self.chord.label());
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // A recording still running is completed, so the file is playable.
        self.stop();
    }
}

/// The Videos folder, or the current directory.
fn default_folder() -> PathBuf {
    win::videos_dir().unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
}

/// `qrec_<date>_<time>.mp4` in `folder`, with a counter when it exists.
fn unique_path(folder: &Path) -> PathBuf {
    let stamp = win::local_time_stamp();
    let mut path = folder.join(format!("qrec_{stamp}.mp4"));
    let mut n = 2;
    while path.exists() {
        path = folder.join(format!("qrec_{stamp}_{n}.mp4"));
        n += 1;
    }
    path
}

/// The text shortened in the middle to about `max` characters.
fn elide(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_owned();
    }
    let head = max / 2 - 1;
    let tail = max - head - 1;
    format!("{}…{}", chars[..head].iter().collect::<String>(), chars[chars.len() - tail..].iter().collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::elide;

    #[test]
    fn elides_in_the_middle() {
        assert_eq!(elide("short", 10), "short");
        assert_eq!(elide("C:\\Users\\Someone\\Videos\\Recordings", 16), "C:\\User…cordings");
        assert_eq!(elide("C:\\Users\\Someone\\Videos\\Recordings", 16).chars().count(), 16);
    }
}
