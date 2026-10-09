//! The window: the area, the few options, the record button and what
//! the last recording came to.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use egui::{Color32, RichText, Vec2};

use crate::audio::Source;
use crate::display::{self, Monitor};
use crate::editor::Editor;
use crate::hotkey::{Chord, Hotkey};
use crate::i18n::LangChoice;
use crate::overlay::{self, Border};
use crate::recorder::{self, Config, Quality, Recorder};
use crate::region::{Aspect, Region};
use crate::sessions;
use crate::tray::{self, Tray};
use crate::win;

/// The window's size in points.
pub const WINDOW_SIZE: [f32; 2] = [460.0, 398.0];
/// The window's corners, painted over a transparent window (as Windows 11
/// rounds its own).
const CORNER_RADIUS: f32 = 8.0;

const MONITOR_KEY: &str = "monitor";
const REGION_KEY: &str = "region";
const FPS_KEY: &str = "fps";
const QUALITY_KEY: &str = "quality";
const AUDIO_KEY: &str = "audio";
const AUDIO_APP_KEY: &str = "audio_app";
const AUDIO_BOOST_KEY: &str = "audio_boost";
const CURSOR_KEY: &str = "cursor";
const FOLDER_KEY: &str = "folder";
const HOTKEY_KEY: &str = "hotkey";
const ASPECT_KEY: &str = "aspect";
const TASKBAR_KEY: &str = "taskbar";
const CLOSE_TO_TRAY_KEY: &str = "close_to_tray";
const MINIMISE_ON_RECORD_KEY: &str = "minimise_on_record";
const TRIM_AFTER_RECORD_KEY: &str = "trim_after_record";
const LANGUAGE_KEY: &str = "language";

/// Width of the language list in About, enough for its longest entry.
const LANG_WIDTH: f32 = 220.0;

const RECORD_COLOUR: Color32 = Color32::from_rgb(0xe5, 0x39, 0x35);
/// The dark of the icon and of the record button's ring.
const DARK_COLOUR: Color32 = Color32::from_rgb(0x2a, 0x33, 0x40);

pub struct App {
    monitors: Vec<Monitor>,
    /// Index into `monitors`.
    monitor: usize,
    /// The area, on the virtual screen; `None` records the whole display.
    region: Option<Region>,
    fps: u32,
    quality: Quality,
    audio: bool,
    /// The program whose sound is recorded, by its executable's full path
    /// (or its file name, as `--audio-app` takes it); `None` for the
    /// whole system.
    audio_app: Option<String>,
    /// Whether the program's sound is brought to full volume, whatever
    /// its volume in the Volume Mixer.
    boost: bool,
    /// The programs that play sound, as last listed.
    apps: Vec<sessions::App>,
    /// Whether `apps` was listed since the list was opened, so it is
    /// listed once each time it opens.
    apps_listed: bool,
    cursor: bool,
    folder: PathBuf,
    /// `None` when the hotkey was removed.
    chord: Option<Chord>,
    hotkey: Option<(Hotkey, mpsc::Receiver<()>)>,
    hotkey_error: Option<String>,
    /// The icon in the notification area; `None` when Windows refused it.
    tray: Option<(Tray, mpsc::Receiver<tray::Command>)>,
    /// The proportions of the next area selected.
    aspect: Aspect,
    /// Whether the window has a button on the taskbar; without one it is
    /// reached from the icon in the notification area.
    taskbar: bool,
    /// Whether the window's cross hides the window instead of closing the
    /// program.
    close_to_tray: bool,
    /// Whether the window is put out of the way when a recording starts.
    minimise_on_record: bool,
    /// Whether a recording opens in the trimming window when it ends.
    trim_after_record: bool,
    /// Interface language, chosen in About.
    lang: LangChoice,
    /// Whether the About window is open.
    about: bool,
    /// The trimming window, while one is open.
    editor: Option<Editor>,
    /// The icon of About, rasterised at the display's pixel density.
    about_icon: Option<egui::TextureHandle>,
    /// The next key press becomes the hotkey.
    capturing_hotkey: bool,
    recorder: Option<Recorder>,
    border: Option<Border>,
    selecting: Option<mpsc::Receiver<Option<Region>>>,
    notice: Notice,
    /// The window's handle, to hide it while an area is being selected.
    window: Option<isize>,
    /// The window size in pixels and the corner radius its region was
    /// made for (`win::round_window`).
    rounded: Option<(egui::Vec2, i32)>,
}

/// The status line.
enum Notice {
    None,
    Saved(PathBuf),
    Error(String),
    Info(String),
}

impl App {
    /// `open`: a recording to trim straight away.
    pub fn new(cc: &eframe::CreationContext<'_>, open: Option<PathBuf>) -> App {
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
        let audio_app = get(AUDIO_APP_KEY).filter(|a| !a.is_empty());
        let boost = get(AUDIO_BOOST_KEY).as_deref() != Some("false");
        // For the name of the program chosen.
        let apps = if audio_app.is_some() { sessions::apps() } else { Vec::new() };
        let taskbar = get(TASKBAR_KEY).as_deref() != Some("false");
        let close_to_tray = get(CLOSE_TO_TRAY_KEY).as_deref() == Some("true");
        let minimise_on_record = get(MINIMISE_ON_RECORD_KEY).as_deref() == Some("true");
        let trim_after_record = get(TRIM_AFTER_RECORD_KEY).as_deref() == Some("true");
        let lang = get(LANGUAGE_KEY).and_then(|l| LangChoice::from_name(&l)).unwrap_or_default();
        crate::i18n::set_lang(lang.resolve());
        let cursor = get(CURSOR_KEY).as_deref() != Some("false");
        let folder = get(FOLDER_KEY).map(PathBuf::from).unwrap_or_else(default_folder);
        let aspect = get(ASPECT_KEY).and_then(|a| Aspect::from_name(&a)).unwrap_or_default();
        // An empty setting is a removed hotkey; a missing one is the default.
        let chord = match get(HOTKEY_KEY) {
            Some(h) if h.is_empty() => None,
            h => Some(h.and_then(|h| Chord::from_label(&h)).filter(Chord::is_usable).unwrap_or_default()),
        };

        // The window must not appear in its own recordings.
        let window = {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            match cc.window_handle().map(|h| h.as_raw()) {
                Ok(RawWindowHandle::Win32(w)) => Some(w.hwnd.get()),
                _ => None,
            }
        };
        if let Some(hwnd) = window {
            win::exclude_from_capture(hwnd);
            log::debug!("window at {:?}", win::window_rect(hwnd));
        }

        // Light, whatever Windows uses, like the other small tools.
        cc.egui_ctx.set_theme(egui::ThemePreference::Light);
        // Tooltips are 500 points wide by default, wider than the window,
        // which cuts them off at its edge: a long one wraps within it.
        cc.egui_ctx.all_styles_mut(|style| style.spacing.tooltip_width = WINDOW_SIZE[0] - 80.0);
        let mut app = App {
            monitors,
            monitor,
            region,
            fps,
            quality,
            audio,
            audio_app,
            boost,
            apps,
            apps_listed: false,
            cursor,
            folder,
            chord,
            aspect,
            taskbar,
            close_to_tray,
            minimise_on_record,
            trim_after_record,
            lang,
            about: false,
            editor: None,
            about_icon: None,
            hotkey: None,
            hotkey_error: None,
            tray: Tray::new(cc.egui_ctx.clone()).inspect_err(|e| log::warn!("no tray icon: {e}")).ok(),
            capturing_hotkey: false,
            recorder: None,
            border: None,
            selecting: None,
            notice: Notice::None,
            window,
            rounded: None,
        };
        app.register_hotkey(&cc.egui_ctx);
        if let Some(path) = open {
            app.open_editor(path, &cc.egui_ctx);
        }
        // Before eframe shows the window, so that no button appears.
        if let (Some(hwnd), true) = (app.window, app.tray_only()) {
            win::set_taskbar_button(hwnd, false, false);
        }
        app
    }

    fn register_hotkey(&mut self, ctx: &egui::Context) {
        self.hotkey = None;
        self.hotkey_error = None;
        let Some(chord) = self.chord else { return };
        match Hotkey::register(chord, ctx.clone()) {
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
            self.toggle(ctx);
        }
        let commands: Vec<tray::Command> = self.tray.as_ref().map(|(_, rx)| rx.try_iter().collect()).unwrap_or_default();
        for command in commands {
            log::debug!("tray: {command:?}");
            match command {
                // While an area is selected the window stays hidden.
                _ if self.selecting.is_some() => {}
                tray::Command::Toggle => self.toggle(ctx),
                tray::Command::Show => {
                    if let Some(hwnd) = self.window {
                        win::show_window(hwnd, true);
                    }
                }
                tray::Command::Taskbar => {
                    self.taskbar = !self.taskbar;
                    if let Some(hwnd) = self.window {
                        win::set_taskbar_button(hwnd, !self.tray_only(), true);
                    }
                }
                tray::Command::CloseToTray => self.close_to_tray = !self.close_to_tray,
                tray::Command::MinimiseOnRecord => self.minimise_on_record = !self.minimise_on_record,
                tray::Command::TrimAfterRecord => self.trim_after_record = !self.trim_after_record,
                tray::Command::About => {
                    if let Some(hwnd) = self.window {
                        win::show_window(hwnd, true);
                    }
                    self.about = true;
                }
                tray::Command::Exit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            }
        }
        if self.recorder.as_ref().is_some_and(Recorder::failed) {
            self.stop(Some(ctx));
        }
    }

    fn toggle(&mut self, ctx: &egui::Context) {
        log::debug!("toggle: recording {}", self.recorder.is_some());
        if self.recorder.is_some() {
            self.stop(Some(ctx));
        } else {
            self.start(ctx);
        }
    }

    fn start(&mut self, ctx: &egui::Context) {
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
        let audio = self.audio.then(|| match &self.audio_app {
            Some(program) => Source::App { program: program.clone(), boost: self.boost },
            None => Source::System,
        });
        let config = Config { monitor, region, fps: self.fps, quality: self.quality, audio, cursor: self.cursor, path };
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
                if self.minimise_on_record {
                    self.minimise(ctx);
                }
            }
            Err(e) => self.notice = Notice::Error(e),
        }
    }

    /// Stops the recording; with `ctx` (not at exit) the file opens in
    /// the trimming window when that is wanted.
    fn stop(&mut self, ctx: Option<&egui::Context>) {
        self.border = None;
        if let Some(recorder) = self.recorder.take() {
            let path = recorder.path.clone();
            log::debug!("stopping");
            self.notice = match recorder.stop() {
                Ok(_) => {
                    if let (true, Some(ctx)) = (self.trim_after_record, ctx) {
                        self.open_editor(path.clone(), ctx);
                    }
                    Notice::Saved(path)
                }
                Err(e) => Notice::Error(e),
            };
        }
    }

    /// The selection over the screens, with the window hidden so that it
    /// does not cover what is to be recorded. A hidden window is not
    /// repainted, so the selection's thread shows it again itself.
    fn select_area(&mut self, ctx: &egui::Context) {
        self.monitors = display::monitors();
        let window = self.window;
        if let Some(hwnd) = window {
            win::show_window(hwnd, false);
        }
        let ctx = ctx.clone();
        self.selecting = Some(overlay::select(self.monitors.clone(), self.aspect, move || {
            if let Some(hwnd) = window {
                win::show_window(hwnd, true);
            }
            ctx.request_repaint();
        }));
    }

    /// The window without a taskbar button; only with the icon in the
    /// notification area, the one way back to a hidden window.
    fn tray_only(&self) -> bool {
        !self.taskbar && self.tray.is_some()
    }

    /// The window out of the way: hidden when it has no taskbar button
    /// (there is nothing to minimise to, and the icon brings it back),
    /// minimised otherwise.
    fn minimise(&self, ctx: &egui::Context) {
        match (self.tray_only(), self.window) {
            (true, Some(hwnd)) => win::show_window(hwnd, false),
            _ => ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true)),
        }
    }

    /// About, as in qview: the icon, the name, the version, what the
    /// program does, the author, the homepage and the licence from
    /// Cargo.toml, where the settings are kept, and the interface language.
    fn about_window(&mut self, ctx: &egui::Context) {
        if !self.about {
            return;
        }
        const ICON: f32 = 64.0;
        const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");
        const LICENSE: &str = env!("CARGO_PKG_LICENSE");
        /// Cargo joins several authors with `:`.
        const AUTHORS: &str = env!("CARGO_PKG_AUTHORS");
        let px = (ICON * ctx.pixels_per_point()).round() as usize;
        if self.about_icon.as_ref().is_none_or(|t| t.size() != [px, px]) {
            let image = egui::ColorImage::from_rgba_unmultiplied([px, px], &crate::icon::rgba(px as u32));
            self.about_icon = Some(ctx.load_texture("about_icon", image, egui::TextureOptions::LINEAR));
        }
        let icon = self.about_icon.clone().expect("set above");
        let modal = egui::Modal::new(egui::Id::new("about")).show(ctx, |ui| {
            ui.set_width(320.0);
            // The cross in the top right corner, over the centred content
            // (a child Ui takes no room in the layout).
            let corner = egui::Rect::from_min_size(egui::pos2(ui.max_rect().right() - 24.0, ui.cursor().top()), Vec2::splat(24.0));
            let closed = title_button(&mut ui.new_child(egui::UiBuilder::new().max_rect(corner)), TitleButton::Close);
            ui.vertical_centered(|ui| {
                ui.image((icon.id(), Vec2::splat(ICON)));
                ui.add_space(4.0);
                ui.heading("qrec");
                ui.label(tr!(format!("Version {}", crate::VERSION), format!("Версия {}", crate::VERSION)));
                ui.add_space(6.0);
                ui.label(tr!("A simple screen area recorder for Windows.", "Простая запись области экрана для Windows."));
                ui.add_space(6.0);
                let authors = AUTHORS.replace(':', ", ");
                ui.label(tr!(format!("Author: {authors}"), format!("Автор: {authors}")));
                if ui.link(tr!("Homepage", "Сайт проекта")).on_hover_text(REPOSITORY).clicked() && !win::shell_open(REPOSITORY) {
                    log::warn!("could not open {REPOSITORY}");
                }
                ui.weak(tr!(format!("{LICENSE} License"), format!("Лицензия {LICENSE}")));
                let place = if crate::portable_dir().is_some() {
                    ui.weak(tr!("Settings: beside the program (portable)", "Настройки: рядом с программой (переносная версия)"))
                } else {
                    ui.weak(tr!("Settings: in the user profile", "Настройки: в профиле пользователя"))
                };
                if let Some(file) = crate::settings_file() {
                    place.on_hover_text(file.display().to_string());
                }
                ui.add_space(6.0);
                ui.separator();
                ui.add_space(2.0);
                // The label over the list, both centred; a fixed width
                // keeps the list from jumping when the language changes.
                ui.weak(tr!("Interface language", "Язык интерфейса"));
                let mut choice = self.lang;
                // A combo box lays itself out left to right, ignoring the
                // centring: indented by hand (`width` is its outer width).
                ui.horizontal(|ui| {
                    ui.add_space(((ui.available_width() - LANG_WIDTH) / 2.0).max(0.0));
                    egui::ComboBox::from_id_salt("language").selected_text(choice.label()).width(LANG_WIDTH).show_ui(ui, |ui| {
                        for c in LangChoice::ALL {
                            ui.selectable_value(&mut choice, c, c.label());
                        }
                    });
                });
                if choice != self.lang {
                    self.lang = choice;
                    crate::i18n::set_lang(choice.resolve());
                }
                ui.add_space(4.0);
            });
            closed
        });
        if modal.inner || modal.should_close() {
            self.about = false;
        }
    }

    /// Opens the trimming window on `path` (one at a time: a second file
    /// replaces the first).
    fn open_editor(&mut self, path: PathBuf, ctx: &egui::Context) {
        // The trimming window is drawn with the main window's pass, which
        // a minimised or hidden window has none of: shown first.
        if let Some(hwnd) = self.window {
            win::show_window(hwnd, true);
        }
        self.editor = None;
        self.editor = Some(Editor::open(path, ctx.clone(), editor_viewport()));
    }

    /// The trimming window, a viewport of its own.
    fn editor_window(&mut self, ctx: &egui::Context) {
        let Some(editor) = &mut self.editor else { return };
        let name = editor.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let builder = egui::ViewportBuilder::default()
            .with_title(format!("{} — {name}", tr!("Trim", "Подрезка")))
            .with_inner_size(crate::editor::WINDOW_SIZE)
            .with_min_inner_size(crate::editor::MIN_WINDOW_SIZE)
            .with_icon(crate::embedded_icon());
        let close = ctx.show_viewport_immediate(editor_viewport(), builder, |ui, _class| editor.ui(ui));
        if close {
            self.editor = None;
        }
    }

    /// A recording dropped on the window opens in the trimming window.
    fn dropped_files(&mut self, ctx: &egui::Context) {
        let dropped = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect::<Vec<_>>());
        if let Some(path) = dropped.into_iter().find(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("mp4"))) {
            self.open_editor(path, ctx);
        }
    }

    /// The icon's state: the clock while recording, and the menu's ticks.
    fn update_tray(&self) {
        if let Some((tray, _)) = &self.tray {
            tray.set(tray::Status {
                clock: self.clock(),
                tray_only: !self.taskbar,
                close_to_tray: self.close_to_tray,
                minimise_on_record: self.minimise_on_record,
                trim_after_record: self.trim_after_record,
            });
        }
    }

    /// The time recorded, `00:01:23`, while recording.
    fn clock(&self) -> Option<String> {
        self.recorder.as_ref().map(|recorder| {
            let elapsed = recorder.started.elapsed().as_secs();
            format!("{:02}:{:02}:{:02}", elapsed / 3600, elapsed / 60 % 60, elapsed % 60)
        })
    }

    /// Clips the window to the painted rounded background, again when its
    /// size in pixels changes (another display scale). The region's corners
    /// are a pixel less round than the painted ones, so their smoothed edge
    /// stays inside.
    fn round_corners(&mut self, ctx: &egui::Context) {
        let Some(hwnd) = self.window else { return };
        let ppp = ctx.pixels_per_point();
        let wanted = ((ctx.content_rect().size() * ppp).round(), (CORNER_RADIUS * ppp).round() as i32 - 1);
        if self.rounded != Some(wanted) && win::round_window(hwnd, wanted.1) {
            self.rounded = Some(wanted);
        }
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
                    self.chord = Some(chord);
                    self.capturing_hotkey = false;
                    self.register_hotkey(ctx);
                    return;
                }
            }
        }
    }

    /// The most the recording takes: the bitrate the encoder is given,
    /// and the file per minute with the sound. A still screen takes less.
    fn estimate(&self) -> Option<String> {
        let monitor = self.monitors.get(self.monitor)?;
        let area = self.region.unwrap_or_else(|| Region::from_rect(monitor.rect));
        let video = f64::from(recorder::bitrate(area.width, area.height, self.fps, self.quality));
        let audio = if self.audio { f64::from(crate::encoder::AAC_BYTES_PER_SECOND) } else { 0.0 };
        let per_minute = (video / 8.0 + audio) * 60.0 / 1e6;
        let mbits = video / 1e6;
        let mbits = if mbits < 10.0 { format!("{mbits:.1}").replace('.', tr!(".", ",")) } else { format!("{mbits:.0}") };
        Some(format!(
            "{} {mbits} {}, {per_minute:.0} {}",
            tr!("up to", "до"),
            tr!("Mbit/s", "Мбит/с"),
            tr!("MB/min", "МБ/мин")
        ))
    }

    /// The area already chosen takes the new proportions around its
    /// centre, so what is shown is what will be recorded.
    fn apply_aspect(&mut self) {
        let (Some(region), Some(monitor)) = (self.region, self.monitors.get(self.monitor)) else { return };
        if let Some(fitted) = region.with_aspect(self.aspect, monitor.rect) {
            self.region = Some(fitted);
            self.notice = Notice::None;
        }
    }

    /// How the program whose sound is recorded is named: its description
    /// when it was listed, or the name of its executable.
    fn app_name<'a>(&'a self, program: &'a str) -> &'a str {
        self.apps.iter().find(|a| a.path.eq_ignore_ascii_case(program)).map_or_else(|| sessions::stem(program), |a| a.name.as_str())
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

        // The bar with the area buttons, the record button and the status
        // line sits at the bottom; the settings take the rest. The window
        // has no title bar: a cross in the corner closes it, and the free
        // parts of the settings area drag it. The window is transparent: the
        // panels have no fill of their own, one rounded background with a
        // thin line at its edge is under both.
        ui.painter().rect(
            ctx.content_rect(),
            CORNER_RADIUS,
            ui.visuals().panel_fill,
            egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
            egui::StrokeKind::Inside,
        );
        self.round_corners(&ctx);
        egui::Panel::bottom("bar")
            .show_separator_line(false)
            .frame(egui::Frame::new().inner_margin(egui::Margin { left: 12, right: 12, top: 10, bottom: 8 }))
            .show(ui, |ui| self.bottom_bar(ui, busy));
        egui::CentralPanel::default().frame(egui::Frame::new().inner_margin(12)).show(ui, |ui| {
            let drag = ui.interact(ui.max_rect(), ui.id().with("drag"), egui::Sense::click_and_drag());
            if drag.drag_started_by(egui::PointerButton::Primary) {
                ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
            }
            // The buttons of the title row from the right: close, minimise,
            // about.
            let top_right = ui.max_rect().right_top();
            let corner = |n: f32| egui::Rect::from_min_size(egui::pos2(top_right.x - 28.0 * n + 4.0, top_right.y), Vec2::splat(24.0));
            // Only the cross hides the window: Alt+F4 and WM_CLOSE from
            // outside (the installer, a shutdown) still close the program.
            let hide_on_close = self.close_to_tray && self.tray.is_some();
            let kind = if hide_on_close { TitleButton::CloseToTray } else { TitleButton::Close };
            if title_button(&mut ui.new_child(egui::UiBuilder::new().max_rect(corner(1.0))), kind) {
                match (hide_on_close, self.window) {
                    (true, Some(hwnd)) => win::show_window(hwnd, false),
                    _ => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                }
            }
            let kind = if self.tray_only() { TitleButton::Hide } else { TitleButton::Minimise };
            if title_button(&mut ui.new_child(egui::UiBuilder::new().max_rect(corner(2.0))), kind) {
                self.minimise(&ctx);
            }
            if title_button(&mut ui.new_child(egui::UiBuilder::new().max_rect(corner(3.0))), TitleButton::About) {
                self.about = true;
            }
            title(ui, egui::Rect::from_min_size(ui.max_rect().min, Vec2::new(ui.max_rect().width() - 84.0, 24.0)));
            ui.add_space(28.0);
            egui::Frame::group(ui.style()).inner_margin(10).show(ui, |ui| {
                ui.set_width(ui.available_width());
                self.settings(ui, &ctx, recording);
            });
        });

        self.about_window(&ctx);
        self.editor_window(&ctx);
        self.dropped_files(&ctx);

        self.update_tray();
        // winit sets the window's style again whenever it changes its
        // state (it shows the window after the first frame, for one).
        if let (Some(hwnd), true) = (self.window, self.tray_only()) {
            win::set_taskbar_button(hwnd, false, false);
        }

        if busy {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
    }

    /// While the window is minimised or hidden, eframe runs no egui pass
    /// and calls this instead of `ui`: the hotkey, the tray and a failed
    /// recording are attended to all the same, so a recording can be
    /// stopped while the window is out of the way.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll(ctx);
        self.update_tray();
        if self.recorder.is_some() {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }

    /// Transparent, so the corners outside the rounded background show what
    /// is behind the window.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        egui::Rgba::TRANSPARENT.to_array()
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        if let Some(m) = self.monitors.get(self.monitor) {
            storage.set_string(MONITOR_KEY, m.device_name.clone());
        }
        storage.set_string(REGION_KEY, self.region.map(|r| r.to_setting()).unwrap_or_default());
        storage.set_string(FPS_KEY, self.fps.to_string());
        storage.set_string(QUALITY_KEY, self.quality.name().to_owned());
        storage.set_string(AUDIO_KEY, self.audio.to_string());
        storage.set_string(AUDIO_APP_KEY, self.audio_app.clone().unwrap_or_default());
        storage.set_string(AUDIO_BOOST_KEY, self.boost.to_string());
        storage.set_string(TASKBAR_KEY, self.taskbar.to_string());
        storage.set_string(CLOSE_TO_TRAY_KEY, self.close_to_tray.to_string());
        storage.set_string(MINIMISE_ON_RECORD_KEY, self.minimise_on_record.to_string());
        storage.set_string(TRIM_AFTER_RECORD_KEY, self.trim_after_record.to_string());
        storage.set_string(LANGUAGE_KEY, self.lang.name().to_owned());
        storage.set_string(CURSOR_KEY, self.cursor.to_string());
        storage.set_string(FOLDER_KEY, self.folder.display().to_string());
        storage.set_string(HOTKEY_KEY, self.chord.map(|c| c.label()).unwrap_or_default());
        storage.set_string(ASPECT_KEY, self.aspect.name().to_owned());
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // A recording still running is completed, so the file is playable.
        self.stop(None);
    }
}

impl App {
    /// The settings, a label and its controls per row.
    fn settings(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, recording: bool) {
        egui::Grid::new("settings").num_columns(2).spacing([16.0, 10.0]).show(ui, |ui| {
            ui.label(tr!("Display", "Экран"));
            ui.add_enabled_ui(!recording, |ui| {
                let current = self.monitors.get(self.monitor).map(|m| m.label(self.monitor)).unwrap_or_default();
                let width = ui.available_width();
                egui::ComboBox::from_id_salt("monitor").selected_text(current).width(width).show_ui(ui, |ui| {
                    for (i, m) in self.monitors.iter().enumerate() {
                        if ui.selectable_label(self.monitor == i, m.label(i)).clicked() && self.monitor != i {
                            self.monitor = i;
                            self.region = None;
                        }
                    }
                });
            });
            ui.end_row();

            ui.label(tr!("Proportions", "Пропорции"));
            ui.add_enabled_ui(!recording, |ui| {
                let before = self.aspect;
                let options = Aspect::ALL.map(|a| (a, aspect_label(a)));
                segmented(ui, &mut self.aspect, &options);
                if self.aspect != before {
                    self.apply_aspect();
                }
            });
            ui.end_row();

            ui.label(tr!("Frame rate", "Частота кадров"));
            ui.add_enabled_ui(!recording, |ui| {
                ui.horizontal(|ui| {
                    segmented(ui, &mut self.fps, &[(30, "30"), (60, "60")]);
                    ui.label(RichText::new(tr!("frames per second", "кадров в секунду")).weak());
                });
            });
            ui.end_row();

            ui.label(tr!("Quality", "Качество"));
            ui.horizontal(|ui| {
                ui.add_enabled_ui(!recording, |ui| {
                    let options = Quality::ALL.map(|q| (q, q.label()));
                    segmented(ui, &mut self.quality, &options);
                });
                // What the area at this rate and quality comes to.
                if let Some(estimate) = self.estimate() {
                    ui.label(RichText::new(estimate).small().weak()).on_hover_text(tr!(
                        "The most the recording takes: the bitrate given to the encoder, and the file per minute with the sound. A still screen takes less.",
                        "Наибольший объём записи: битрейт, заданный кодеру, и размер файла за минуту вместе со звуком. Неподвижный экран занимает меньше."
                    ));
                }
            });
            ui.end_row();

            ui.label(tr!("Record", "Записывать"));
            ui.add_enabled_ui(!recording, |ui| {
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.audio, tr!("Sound", "Звук"));
                    ui.add_space(8.0);
                    ui.checkbox(&mut self.cursor, tr!("Pointer", "Указатель"));
                });
            });
            ui.end_row();

            ui.label(tr!("Sound from", "Источник звука"));
            ui.add_enabled_ui(!recording && self.audio, |ui| self.audio_source(ui));
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
            ui.horizontal_wrapped(|ui| {
                if self.capturing_hotkey {
                    ui.label(RichText::new(tr!("Press the keys…", "Нажмите клавиши…")).color(ui.visuals().selection.bg_fill));
                } else if let Some(chord) = self.chord {
                    ui.label(RichText::new(chord.label()).strong());
                } else {
                    ui.weak(tr!("none", "нет"));
                }
                let text = if self.capturing_hotkey { tr!("Cancel", "Отмена") } else { tr!("Change", "Изменить") };
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
                        self.register_hotkey(ctx);
                    }
                }
                if self.chord.is_some()
                    && !self.capturing_hotkey
                    && ui.add_enabled(!recording, cross_button).on_hover_text(tr!("Remove the hotkey", "Убрать клавишу")).clicked()
                {
                    self.chord = None;
                    self.register_hotkey(ctx);
                }
                if let Some(e) = &self.hotkey_error {
                    // Usually another program (or another qrec) holds the key.
                    ui.label(RichText::new(tr!("taken by another program", "занята другой программой")).color(ui.visuals().error_fg_color))
                        .on_hover_text(e);
                }
            });
            ui.end_row();
        });
    }

    /// The list of whose sound is recorded: the whole system, or one of
    /// the programs that have played sound since they started (their
    /// sessions in the Volume Mixer), listed again each time it opens;
    /// and, at the right, whether a program's sound is boosted.
    fn audio_source(&mut self, ui: &mut egui::Ui) {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let boost = ui.add_enabled(self.audio_app.is_some(), egui::Checkbox::new(&mut self.boost, tr!("Boost", "Усиление")));
            boost.on_hover_text(tr!(
                "On: the program's sound is recorded at full volume, whatever its volume in the Volume Mixer (up to 100 times louder); a muted program is still silent. Off: it is recorded as it is heard.",
                "Включено: звук программы записывается на полной громкости, какой бы ни была её громкость в микшере (усиление до 100 раз); выключенная в микшере программа всё равно записывается тишиной. Выключено: звук записывается так, как слышится."
            ))
            .on_disabled_hover_text(tr!("Only for the sound of one program.", "Только для звука одной программы."));
            self.audio_list(ui);
        });
    }

    fn audio_list(&mut self, ui: &mut egui::Ui) {
        let system = tr!("Whole system", "Вся система");
        let selected = match &self.audio_app {
            Some(exe) => self.app_name(exe).to_owned(),
            None => system.to_owned(),
        };
        let mut choice = self.audio_app.clone();
        let mut open = false;
        let width = ui.available_width();
        let combo = egui::ComboBox::from_id_salt("audio_source").selected_text(elide(&selected, 40)).width(width).show_ui(ui, |ui| {
            open = true;
            if !self.apps_listed {
                self.apps = sessions::apps();
                self.apps_listed = true;
            }
            ui.selectable_value(&mut choice, None, system);
            ui.separator();
            if self.apps.is_empty() {
                ui.weak(tr!("No program has played sound", "Ни одна программа не воспроизводила звук"));
            }
            for app in &self.apps {
                // The programs playing now in the normal colour.
                let text = if app.active { RichText::new(&app.name) } else { RichText::new(&app.name).weak() };
                let selected = choice.as_ref().is_some_and(|c| c.eq_ignore_ascii_case(&app.path));
                if ui.selectable_label(selected, text).on_hover_text(&app.path).clicked() {
                    choice = Some(app.path.clone());
                }
            }
        });
        combo.response.on_hover_text(tr!(
            "The whole system: everything the computer plays, at the volume it is played. A program: only its sound, with Boost at full volume whatever its volume in the Volume Mixer. The program must be running when the recording starts.",
            "Вся система: всё, что воспроизводит компьютер, с той громкостью, с которой оно звучит. Программа: только её звук, с «Усилением» на полной громкости, какой бы ни была её громкость в микшере. Программа должна быть запущена к началу записи."
        ));
        if !open {
            self.apps_listed = false;
        }
        self.audio_app = choice;
    }

    /// The area buttons, the record button with the time recorded, and
    /// the status line: the area, the last file or what went wrong.
    fn bottom_bar(&mut self, ui: &mut egui::Ui, busy: bool) {
        // The row is as high as the record button from the start, so the
        // buttons and the record button are centred on one line (a plain
        // horizontal row centres on the height it has when each widget
        // is placed).
        let row = Vec2::new(ui.available_width(), BAR_HEIGHT);
        ui.allocate_ui_with_layout(row, egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.set_min_height(BAR_HEIGHT);
            ui.add_enabled_ui(!busy, |ui| {
                // Two halves of one choice, the current one filled: an
                // area of its own or the whole display.
                ui.spacing_mut().item_spacing.x = 1.0;
                let size = Vec2::new(0.0, BAR_HEIGHT);
                let area = self.region.is_some();
                let r = BAR_CORNER;
                let left = egui::CornerRadius { nw: r, sw: r, ne: 0, se: 0 };
                let right = egui::CornerRadius { nw: 0, sw: 0, ne: r, se: r };
                let select = choice(ui, area, tr!("Select area…", "Выделить область…"), Some(16.0)).corner_radius(left).min_size(size);
                if ui.add(select).clicked() {
                    self.select_area(ui.ctx());
                }
                let whole = choice(ui, !area, tr!("Whole display", "Весь экран"), Some(16.0)).corner_radius(right).min_size(size);
                if ui.add(whole).clicked() {
                    self.region = None;
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let clock = self.clock();
                if record_button(ui, clock.as_deref(), self.selecting.is_none()) {
                    self.toggle(&ui.ctx().clone());
                }
            });
        });
        ui.add_space(6.0);
        // Two lines of text at most; wrapped, so a long message stays in
        // the window.
        let height = ui.text_style_height(&egui::TextStyle::Body) * 2.0 + 4.0;
        ui.allocate_ui_with_layout(Vec2::new(ui.available_width(), height), egui::Layout::top_down(egui::Align::LEFT), |ui| {
            ui.set_min_height(height);
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
            let mut trim = None;
            match &self.notice {
                Notice::None => {
                    ui.label(RichText::new(self.area_label()).weak());
                }
                Notice::Saved(path) => {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(tr!("Saved:", "Сохранено:"));
                        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        if ui.link(name).on_hover_text(tr!("Show in Explorer", "Показать в Проводнике")).clicked() {
                            win::show_in_explorer(path);
                        }
                        if ui.small_button(tr!("Trim…", "Подрезать…")).on_hover_text(tr!("Cut the start and the end off, without re-encoding", "Отрезать начало и конец без перекодирования")).clicked() {
                            trim = Some(path.clone());
                        }
                    });
                }
                Notice::Error(e) => {
                    ui.label(RichText::new(e).color(ui.visuals().error_fg_color));
                }
                Notice::Info(text) => {
                    let dropped = self.recorder.as_ref().map_or(0, Recorder::dropped);
                    let text = if dropped > 0 {
                        format!("{text}, {} {dropped}", tr!("skipped frames:", "пропущено кадров:"))
                    } else {
                        text.clone()
                    };
                    ui.label(RichText::new(text).weak());
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::BOTTOM), |ui| {
                ui.label(RichText::new(format!("qrec {}", crate::VERSION)).small().weak());
            });
            if let Some(path) = trim {
                self.open_editor(path, ui.ctx());
            }
        });
    }
}

/// The name in the top left corner, its `q` in the dark of the icon
/// and its `rec` in the red of the record dot. Only painted, so it
/// drags the window like the rest of the free area.
fn title(ui: &egui::Ui, rect: egui::Rect) {
    let painter = ui.painter_at(rect);
    let font = egui::FontId::proportional(18.0);
    let mut job = egui::text::LayoutJob::default();
    job.append("q", 0.0, egui::TextFormat::simple(font.clone(), DARK_COLOUR));
    job.append("rec", 0.0, egui::TextFormat::simple(font, RECORD_COLOUR));
    let galley = ui.fonts_mut(|f| f.layout_job(job));
    let pos = egui::pos2(rect.left(), rect.center().y - galley.size().y / 2.0);
    painter.galley(pos, galley, DARK_COLOUR);
}

/// The trimming window's viewport.
fn editor_viewport() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("editor")
}

/// The height of the area buttons and of the record button.
const BAR_HEIGHT: f32 = 42.0;
/// The rounding of their corners.
const BAR_CORNER: u8 = 6;

/// How the proportions are named in the window.
fn aspect_label(aspect: Aspect) -> &'static str {
    match aspect {
        Aspect::Free => tr!("Free", "Свободные"),
        other => other.name(),
    }
}

/// A button that is one of the values of a choice: filled with the
/// selection colour when it is the current one.
fn choice(ui: &egui::Ui, selected: bool, text: &str, size: Option<f32>) -> egui::Button<'static> {
    let selection = ui.visuals().selection;
    let text = size.map_or_else(|| RichText::new(text), |size| RichText::new(text).size(size));
    if selected {
        egui::Button::new(text.color(selection.stroke.color)).fill(selection.bg_fill)
    } else {
        egui::Button::new(text)
    }
}

/// A few values side by side as one control, the current one filled.
fn segmented<T: Copy + PartialEq>(ui: &mut egui::Ui, value: &mut T, options: &[(T, &str)]) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 1.0;
        let last = options.len().saturating_sub(1);
        for (i, &(v, text)) in options.iter().enumerate() {
            let r = 4;
            let corner = egui::CornerRadius {
                nw: if i == 0 { r } else { 0 },
                sw: if i == 0 { r } else { 0 },
                ne: if i == last { r } else { 0 },
                se: if i == last { r } else { 0 },
            };
            let button = choice(ui, *value == v, text, None).corner_radius(corner).min_size(Vec2::new(44.0, 0.0));
            if ui.add(button).clicked() {
                *value = v;
            }
        }
    });
}

/// The record button: red, with a white dot and the word; while
/// recording, a white square, the stop, and the time recorded. True
/// when clicked.
fn record_button(ui: &mut egui::Ui, clock: Option<&str>, enabled: bool) -> bool {
    let sense = if enabled { egui::Sense::click() } else { egui::Sense::hover() };
    let (rect, response) = ui.allocate_exact_size(Vec2::new(150.0, BAR_HEIGHT), sense);
    let fill = if !enabled {
        RECORD_COLOUR.gamma_multiply(0.5)
    } else if response.hovered() {
        Color32::from_rgb(0xf0, 0x4a, 0x45)
    } else {
        RECORD_COLOUR
    };
    let painter = ui.painter();
    painter.rect_filled(rect, BAR_CORNER, fill);
    let (text, font) = match clock {
        Some(clock) => (clock.to_owned(), egui::FontId::monospace(18.0)),
        None => (tr!("RECORD", "ЗАПИСЬ").to_owned(), egui::FontId::proportional(17.0)),
    };
    let galley = painter.layout_no_wrap(text, font, Color32::WHITE);
    // The mark and the text, centred together.
    let (mark, gap) = (14.0, 10.0);
    let left = rect.center().x - (mark + gap + galley.size().x) / 2.0;
    let centre = egui::pos2(left + mark / 2.0, rect.center().y);
    if clock.is_some() {
        painter.rect_filled(egui::Rect::from_center_size(centre, Vec2::splat(mark - 2.0)), 2.0, Color32::WHITE);
    } else {
        painter.circle_filled(centre, mark / 2.0, Color32::WHITE);
    }
    painter.galley(egui::pos2(left + mark + gap, rect.center().y - galley.size().y / 2.0), galley, Color32::WHITE);
    let hint = if clock.is_some() { tr!("Stop", "Стоп") } else { tr!("Record", "Запись") };
    response.on_hover_text(hint).clicked()
}

#[derive(Clone, Copy)]
enum TitleButton {
    Minimise,
    Hide,
    Close,
    CloseToTray,
    About,
}

/// A 24 x 24 button of the title row: a dash that minimises (or hides
/// the window when it has no taskbar button) or a cross that closes (or
/// hides the window), as the dialogs of qview have it. True when clicked.
fn title_button(ui: &mut egui::Ui, kind: TitleButton) -> bool {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(24.0), egui::Sense::click());
    let visuals = ui.style().interact(&response);
    if response.hovered() {
        ui.painter().rect_filled(rect, 3.0, visuals.bg_fill);
    }
    let (c, r) = (rect.center(), 5.0);
    let stroke = egui::Stroke::new(1.5, visuals.fg_stroke.color);
    let hint = match kind {
        TitleButton::Minimise | TitleButton::Hide => {
            ui.painter().line_segment([c + Vec2::new(-r, 0.0), c + Vec2::new(r, 0.0)], stroke);
            match kind {
                TitleButton::Hide => tr!("Hide to the notification area", "Скрыть в область уведомлений"),
                _ => tr!("Minimise", "Свернуть"),
            }
        }
        TitleButton::Close | TitleButton::CloseToTray => {
            ui.painter().line_segment([c + Vec2::new(-r, -r), c + Vec2::new(r, r)], stroke);
            ui.painter().line_segment([c + Vec2::new(-r, r), c + Vec2::new(r, -r)], stroke);
            match kind {
                TitleButton::CloseToTray => tr!("Hide to the notification area", "Скрыть в область уведомлений"),
                _ => tr!("Close", "Закрыть"),
            }
        }
        TitleButton::About => {
            // An "i" in a circle.
            ui.painter().circle_stroke(c, r + 1.5, egui::Stroke::new(1.2, visuals.fg_stroke.color));
            ui.painter().circle_filled(c + Vec2::new(0.0, -2.8), 1.0, visuals.fg_stroke.color);
            ui.painter().line_segment([c + Vec2::new(0.0, -0.8), c + Vec2::new(0.0, 3.5)], stroke);
            tr!("About", "О программе")
        }
    };
    response.on_hover_text(hint).clicked()
}

/// A square button with a cross, as high as the buttons beside it.
fn cross_button(ui: &mut egui::Ui) -> egui::Response {
    let side = ui.spacing().interact_size.y;
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(side), egui::Sense::click());
    let visuals = ui.style().interact(&response);
    ui.painter().rect_filled(rect, visuals.corner_radius, visuals.weak_bg_fill);
    let (c, r) = (rect.center(), side * 0.2);
    let stroke = egui::Stroke::new(1.5, visuals.fg_stroke.color);
    ui.painter().line_segment([c + Vec2::new(-r, -r), c + Vec2::new(r, r)], stroke);
    ui.painter().line_segment([c + Vec2::new(-r, r), c + Vec2::new(r, -r)], stroke);
    response
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
