//! The window: the area, the few options, the record button and what
//! the last recording came to. Its drawn controls are in `widgets`, the
//! About window in `about`.

mod about;
mod widgets;

pub(crate) use widgets::{TitleButton, file_buttons, file_link, thumbnail, title_button};
use widgets::*;

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use egui::{Color32, RichText, Vec2};

use crate::audio::Source;
use crate::display::{self, Monitor};
use crate::editor::Editor;
use crate::hotkey::{Chord, Hotkey};
use crate::i18n::LangChoice;
use crate::overlay::{self, Border, Countdown};
use crate::recorder::{self, Config, Quality, RateControl, Recorder};
use crate::library::Library;
use crate::recordings::{self, Recordings};
use crate::region::{Aspect, Region};
use crate::sessions;
use crate::tray::{self, Tray};
use crate::update::{self, Updates};
use crate::win;

/// The window's size in points.
pub const WINDOW_SIZE: [f32; 2] = [500.0, 524.0];
/// The window's corners, painted over a transparent window (as Windows 11
/// rounds its own).
pub(crate) const CORNER_RADIUS: f32 = 8.0;

const MONITOR_KEY: &str = "monitor";
const REGION_KEY: &str = "region";
const FPS_KEY: &str = "fps";
const QUALITY_KEY: &str = "quality";
const RATE_KEY: &str = "rate_control";
const AUDIO_KEY: &str = "audio";
const AUDIO_APP_KEY: &str = "audio_app";
const AUDIO_BOOST_KEY: &str = "audio_boost";
const CURSOR_KEY: &str = "cursor";
const CLICKS_KEY: &str = "clicks";
const MICROPHONE_KEY: &str = "microphone";
const FOLDER_KEY: &str = "folder";
const HOTKEY_KEY: &str = "hotkey";
const SELECT_HOTKEY_KEY: &str = "select_hotkey";
const PAUSE_HOTKEY_KEY: &str = "pause_hotkey";
const ASPECT_KEY: &str = "aspect";
const TASKBAR_KEY: &str = "taskbar";
const CLOSE_TO_TRAY_KEY: &str = "close_to_tray";
const MINIMISE_ON_RECORD_KEY: &str = "minimise_on_record";
const TRIM_AFTER_RECORD_KEY: &str = "trim_after_record";
const COUNTDOWN_KEY: &str = "countdown";
/// The seconds counted down before a recording, with the countdown on.
const COUNTDOWN_SECONDS: u32 = 3;
const LANGUAGE_KEY: &str = "language";
/// Not `editor_rect`, which held the position in points: a value of that
/// is not taken for pixels.
const EDITOR_RECT_KEY: &str = "editor_window";
const TRIM_REPLACE_KEY: &str = "trim_replace";
const CHECK_UPDATES_KEY: &str = "check_updates";
const LAST_UPDATE_CHECK_KEY: &str = "last_update_check";

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
    /// Whether the bitrate follows the quality or is fixed beforehand.
    rate: RateControl,
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
    /// Whether the clicks are drawn under the pointer.
    clicks: bool,
    /// Whether the default microphone is recorded, mixed with the sound.
    microphone: bool,
    folder: PathBuf,
    /// The latest recordings in `folder`, for the list and the tray.
    recordings: Recordings,
    /// When `recordings` was last listed: again every few seconds, so
    /// files cut or deleted elsewhere show.
    listed: std::time::Instant,
    /// The hotkey that starts and stops the recording.
    record_key: GlobalKey,
    /// The hotkey that opens the area selection.
    select_key: GlobalKey,
    /// The hotkey that pauses and resumes the recording.
    pause_key: GlobalKey,
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
    /// Whether the recording put the window out of the way, so that it
    /// comes back when the recording stops; not when it was already
    /// minimised or hidden (a recording started by the hotkey).
    restore_on_stop: bool,
    /// Whether a recording opens in the trimming window when it ends.
    trim_after_record: bool,
    /// Whether a recording starts after a few seconds counted down over
    /// the area.
    countdown: bool,
    /// The count before a recording, while it runs.
    counting: Option<Countdown>,
    /// Interface language, chosen in About.
    lang: LangChoice,
    /// Whether the About window is open.
    about: bool,
    /// The update check, enabled in About.
    updates: Updates,
    /// The trimming window, while one is open.
    editor: Option<Editor>,
    /// What copies of qrec started later ask of this one.
    instance: Option<mpsc::Receiver<crate::instance::Request>>,
    /// Where the trimming window was last: its outer position in physical
    /// pixels on the virtual screen, its inner size in points; where it
    /// opens next (`x,y,w,h` in the settings). Pixels for the position:
    /// points differ between displays of different scales, and winit turns
    /// a position in points into pixels by the scale of the display the
    /// window is created on, not of the one it goes to.
    editor_rect: Option<egui::Rect>,
    /// Where the open trimming window was placed, passed with its viewport
    /// every frame (a changed position would move it): the position in
    /// points by the main window's scale, a guess.
    editor_placement: Option<egui::Rect>,
    /// The position in pixels the open trimming window still has to be
    /// moved to, once its window exists, when the guess was off.
    editor_move: Option<(i32, i32)>,
    /// Whether the trimming window's viewport has been shown (its window
    /// created).
    editor_created: bool,
    /// The window that lists the recordings, while it is open.
    library: Option<Library>,
    /// Whether its viewport has been shown (see `editor_created`).
    library_created: bool,
    /// Keeps it beside this window, once its window exists.
    dock: Option<crate::dock::Dock>,
    /// Whether the main window was on the screen when this frame began
    /// (`logic`). eframe paints a hidden or minimised window outside
    /// winit's event handlers, where a new viewport cannot create its
    /// window: egui then panics ("the user callback was never called").
    /// The window shown during that pass (from the tray, for a file to
    /// trim) is still in it, so the trimming window waits a frame.
    shown_at_start: bool,
    /// Whether a cut replaces the file it was made from.
    trim_replace: bool,
    /// The icon of About, rasterised at the display's pixel density.
    about_icon: Option<egui::TextureHandle>,
    /// The next key press becomes the hotkey of this action.
    capturing_hotkey: Option<KeyAction>,
    recorder: Option<Recorder>,
    /// Why the sound of the last recording stopped before it did, shown
    /// beside the file when it did.
    audio_warning: Option<String>,
    border: Option<Border>,
    selecting: Option<mpsc::Receiver<Option<overlay::Picked>>>,
    /// The window clicked in the selection and the area it gave: a
    /// recording of that area follows the window (not kept between runs).
    window_picked: Option<(isize, Region)>,
    notice: Notice,
    /// The window's handle, to hide it while an area is being selected.
    window: Option<isize>,
    /// The window size in pixels and the corner radius its region was
    /// made for (`win::round_window`).
    rounded: Option<(egui::Vec2, i32)>,
    /// The window is cloaked at start until it has painted a few frames
    /// (Windows shows it white until its first frame reaches DWM): the
    /// time it is shown anyway, and the frame it was cloaked in.
    cloak: Option<(std::time::Instant, u64)>,
}

/// Frames the window paints, cloaked, before it is shown at start.
const PAINTED_TO_SHOW: u64 = 3;
/// How long the window stays cloaked at most.
const UNCLOAK_TIMEOUT: Duration = Duration::from_secs(1);

/// What a global hotkey does.
#[derive(Clone, Copy, PartialEq, Eq)]
enum KeyAction {
    Record,
    Select,
    Pause,
}

/// A global hotkey: its chord, registered while the program runs.
#[derive(Default)]
struct GlobalKey {
    /// `None` when the hotkey was removed.
    chord: Option<Chord>,
    registered: Option<(Hotkey, mpsc::Receiver<()>)>,
    /// Why Windows refused the chord.
    error: Option<String>,
}

impl GlobalKey {
    /// An empty setting is a removed hotkey; a missing one is `default`.
    fn from_setting(setting: Option<String>, default: Chord) -> GlobalKey {
        let chord = match setting {
            Some(h) if h.is_empty() => None,
            h => Some(h.and_then(|h| Chord::from_label(&h)).filter(Chord::is_usable).unwrap_or(default)),
        };
        GlobalKey { chord, ..GlobalKey::default() }
    }

    fn register(&mut self, ctx: &egui::Context) {
        self.registered = None;
        self.error = None;
        let Some(chord) = self.chord else { return };
        match Hotkey::register(chord, ctx.clone()) {
            Ok(registered) => self.registered = Some(registered),
            Err(e) => self.error = Some(e),
        }
    }

    /// The presses since the last call.
    fn presses(&self) -> usize {
        self.registered.as_ref().map_or(0, |(_, rx)| rx.try_iter().count())
    }
}

/// What is done with the recordings, from the card or the window that
/// lists them.
pub(crate) enum FileAction {
    Trim(PathBuf),
    Show(PathBuf),
    Discard(PathBuf),
    /// All of these to the Recycle Bin at once.
    Clear(Vec<PathBuf>),
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
        let rate = get(RATE_KEY).and_then(|r| RateControl::from_name(&r)).unwrap_or_default();
        let audio = get(AUDIO_KEY).as_deref() != Some("false");
        let audio_app = get(AUDIO_APP_KEY).filter(|a| !a.is_empty());
        let boost = get(AUDIO_BOOST_KEY).as_deref() != Some("false");
        // For the name of the program chosen.
        let apps = if audio_app.is_some() { sessions::apps() } else { Vec::new() };
        let taskbar = get(TASKBAR_KEY).as_deref() != Some("false");
        let close_to_tray = get(CLOSE_TO_TRAY_KEY).as_deref() == Some("true");
        let minimise_on_record = get(MINIMISE_ON_RECORD_KEY).as_deref() == Some("true");
        let trim_after_record = get(TRIM_AFTER_RECORD_KEY).as_deref() == Some("true");
        let countdown = get(COUNTDOWN_KEY).as_deref() == Some("true");
        let lang = get(LANGUAGE_KEY).and_then(|l| LangChoice::from_name(&l)).unwrap_or_default();
        let editor_rect = get(EDITOR_RECT_KEY).and_then(|r| rect_from_setting(&r));
        let trim_replace = get(TRIM_REPLACE_KEY).as_deref() == Some("true");
        let mut updates = Updates::new(
            get(CHECK_UPDATES_KEY).as_deref() == Some("true"),
            get(LAST_UPDATE_CHECK_KEY).and_then(|s| s.parse().ok()).unwrap_or(0),
        );
        updates.start_if_due(&cc.egui_ctx);
        crate::i18n::set_lang(lang.resolve());
        let cursor = get(CURSOR_KEY).as_deref() != Some("false");
        let clicks = get(CLICKS_KEY).as_deref() == Some("true");
        let microphone = get(MICROPHONE_KEY).as_deref() == Some("true");
        let folder = get(FOLDER_KEY).map(PathBuf::from).unwrap_or_else(default_folder);
        let aspect = get(ASPECT_KEY).and_then(|a| Aspect::from_name(&a)).unwrap_or_default();
        let record_key = GlobalKey::from_setting(get(HOTKEY_KEY), Chord::RECORD);
        let mut select_key = GlobalKey::from_setting(get(SELECT_HOTKEY_KEY), Chord::SELECT);
        // The recording's chord was set to the selection's default before
        // the selection had a hotkey: the recording keeps it.
        if select_key.chord.is_some() && select_key.chord == record_key.chord {
            select_key.chord = None;
        }
        // A chord the others already have (set before the pause had a
        // hotkey) stays theirs.
        let mut pause_key = GlobalKey::from_setting(get(PAUSE_HOTKEY_KEY), Chord::PAUSE);
        if pause_key.chord.is_some() && (pause_key.chord == record_key.chord || pause_key.chord == select_key.chord) {
            pause_key.chord = None;
        }

        // The window must not appear in its own recordings.
        let window = {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            match cc.window_handle().map(|h| h.as_raw()) {
                Ok(RawWindowHandle::Win32(w)) => Some(w.hwnd.get()),
                _ => None,
            }
        };
        // Cloaked while it is still hidden (eframe shows it after its first
        // frame), until it has painted (`uncloak`).
        let mut cloak = None;
        if let Some(hwnd) = window {
            win::exclude_from_capture(hwnd);
            win::cloak(hwnd, true);
            cloak = Some((std::time::Instant::now() + UNCLOAK_TIMEOUT, cc.egui_ctx.cumulative_frame_nr()));
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
            rate,
            audio,
            audio_app,
            boost,
            apps,
            apps_listed: false,
            cursor,
            clicks,
            microphone,
            folder,
            recordings: Recordings::new(cc.egui_ctx.clone()),
            listed: std::time::Instant::now(),
            record_key,
            select_key,
            pause_key,
            aspect,
            taskbar,
            close_to_tray,
            minimise_on_record,
            restore_on_stop: false,
            trim_after_record,
            countdown,
            counting: None,
            lang,
            about: false,
            updates,
            editor: None,
            instance: crate::instance::listen(cc.egui_ctx.clone()).inspect_err(|e| log::warn!("no instance window: {e}")).ok(),
            editor_rect,
            editor_placement: None,
            editor_move: None,
            editor_created: false,
            library: None,
            library_created: false,
            dock: None,
            shown_at_start: false,
            trim_replace,
            about_icon: None,
            tray: Tray::new(cc.egui_ctx.clone()).inspect_err(|e| log::warn!("no tray icon: {e}")).ok(),
            capturing_hotkey: None,
            recorder: None,
            border: None,
            selecting: None,
            window_picked: None,
            notice: Notice::None,
            audio_warning: None,
            window,
            rounded: None,
            cloak,
        };
        app.register_hotkeys(&cc.egui_ctx);
        app.relist();
        // Before eframe shows the window, so that no button appears.
        if let (Some(hwnd), true) = (app.window, app.tray_only()) {
            win::set_taskbar_button(hwnd, false, false);
        }
        // Not shown here: eframe shows the window after its first frame,
        // and shown now it would appear empty, with a taskbar button its
        // style had not settled.
        if let Some(path) = open {
            app.place_editor(path, &cc.egui_ctx);
        }
        // QREC_RECORDINGS=1: the recordings window at start (GUI checks).
        if std::env::var_os("QREC_RECORDINGS").is_some() {
            app.library = Some(Library::new());
        }
        // QREC_ABOUT=1: About at start (GUI checks).
        if std::env::var_os("QREC_ABOUT").is_some() {
            app.about = true;
        }
        app
    }

    fn register_hotkeys(&mut self, ctx: &egui::Context) {
        // All freed first, so a chord moved from one to another is free.
        self.free_hotkeys();
        self.record_key.register(ctx);
        self.select_key.register(ctx);
        self.pause_key.register(ctx);
    }

    /// Unregisters the hotkeys (while one is being chosen, so that any
    /// chord can be pressed).
    fn free_hotkeys(&mut self) {
        self.record_key.registered = None;
        self.select_key.registered = None;
        self.pause_key.registered = None;
    }

    fn global_key(&mut self, action: KeyAction) -> &mut GlobalKey {
        match action {
            KeyAction::Record => &mut self.record_key,
            KeyAction::Select => &mut self.select_key,
            KeyAction::Pause => &mut self.pause_key,
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
                        Some(picked) => {
                            let region = picked.region;
                            if let Some(i) = display::monitor_of(&self.monitors, &region.rect())
                                .and_then(|m| self.monitors.iter().position(|x| x == m))
                            {
                                self.monitor = i;
                            }
                            self.region = Some(region);
                            self.window_picked = picked.window.map(|w| (w, region));
                            self.notice = Notice::None;
                        }
                        None => {
                            self.notice =
                                Notice::Info(tr!("Selection cancelled or too small", "Выделение отменено или слишком мало").into())
                        }
                    }
                    ctx.request_repaint();
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    // The selection's thread is gone without a word (a
                    // panic in a debug build): the window must come back.
                    self.selecting = None;
                    if let Some(hwnd) = self.window {
                        win::show_window(hwnd, true);
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if self.record_key.presses() % 2 == 1 && self.selecting.is_none() {
            self.toggle(ctx);
        }
        if self.counting.as_ref().is_some_and(|c| c.left() == 0) {
            self.counting = None;
            self.start(ctx);
        }
        // Not while recording: the area is fixed then, as its buttons are.
        // Only while recording: a press otherwise does nothing.
        if self.pause_key.presses() % 2 == 1 && self.recorder.is_some() {
            self.toggle_pause();
        }
        if self.select_key.presses() > 0 && self.selecting.is_none() && self.recorder.is_none() {
            self.select_area(ctx, overlay::Mode::Area);
        }
        // While an area is selected the window stays hidden: the commands
        // wait in their channels until it is done.
        if self.selecting.is_some() {
            return;
        }
        let commands: Vec<tray::Command> = self.tray.as_ref().map(|(_, rx)| rx.try_iter().collect()).unwrap_or_default();
        for command in commands {
            log::debug!("tray: {command:?}");
            match command {
                tray::Command::Toggle => self.toggle(ctx),
                tray::Command::Pause => self.toggle_pause(),
                tray::Command::Show => {
                    if let Some(hwnd) = self.window {
                        win::show_window(hwnd, true);
                    }
                }
                tray::Command::Taskbar => {
                    self.taskbar = !self.taskbar;
                    if let Some(hwnd) = self.window {
                        // A minimised tool window shows as a small title bar
                        // at the edge of the screen: hidden instead.
                        if self.tray_only() && win::is_minimised(hwnd) {
                            win::show_window(hwnd, false);
                        }
                        win::set_taskbar_button(hwnd, !self.tray_only(), true);
                    }
                }
                tray::Command::CloseToTray => self.close_to_tray = !self.close_to_tray,
                tray::Command::MinimiseOnRecord => self.minimise_on_record = !self.minimise_on_record,
                tray::Command::TrimAfterRecord => self.trim_after_record = !self.trim_after_record,
                tray::Command::Countdown => self.countdown = !self.countdown,
                tray::Command::About => {
                    if let Some(hwnd) = self.window {
                        win::show_window(hwnd, true);
                    }
                    self.about = true;
                }
                tray::Command::Exit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                tray::Command::Trim(path) => self.open_editor(path, ctx),
                tray::Command::Folder => win::open_folder(&self.folder),
                tray::Command::Recordings => self.open_library(ctx),
                tray::Command::Update => {
                    if let Some(url) = self.updates.newer().map(update::release_url)
                        && !win::shell_open(&url)
                    {
                        log::warn!("could not open {url}");
                    }
                }
            }
        }
        let requests: Vec<crate::instance::Request> = self.instance.as_ref().map(|rx| rx.try_iter().collect()).unwrap_or_default();
        for request in requests {
            match request {
                crate::instance::Request::Show => {
                    if let Some(hwnd) = self.window {
                        win::show_window(hwnd, true);
                    }
                }
                crate::instance::Request::Open(path) => self.open_editor(path, ctx),
                crate::instance::Request::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            }
        }
        if self.recorder.as_ref().is_some_and(Recorder::failed) {
            self.stop(Some(ctx));
        } else if let Some(e) = self.recorder.as_ref().and_then(Recorder::audio_error)
            && self.audio_warning.is_none()
        {
            // The sound stopped; the picture goes on, the file is written
            // all the same.
            let warning = format!("{e}. {}", tr!("The recording goes on without sound", "Запись продолжается без звука"));
            self.notice = Notice::Error(warning.clone());
            self.audio_warning = Some(warning);
            ctx.request_repaint();
        } else if let Some(e) = self.recorder.as_ref().and_then(Recorder::microphone_error)
            && self.audio_warning.is_none()
        {
            // The microphone would not open (refused by Windows' privacy
            // settings or an antivirus); the rest is recorded.
            let warning = format!("{e}. {}", tr!("The recording goes on without the microphone", "Запись продолжается без микрофона"));
            self.notice = Notice::Error(warning.clone());
            self.audio_warning = Some(warning);
            ctx.request_repaint();
        }
    }

    /// Pauses the recording or resumes it.
    fn toggle_pause(&mut self) {
        if let Some(recorder) = &self.recorder {
            recorder.set_paused(!recorder.paused());
        }
        self.update_tray();
    }

    /// Starts or stops the recording; with the countdown, starts it after
    /// the count, and a second press during the count cancels it.
    fn toggle(&mut self, ctx: &egui::Context) {
        log::debug!("toggle: recording {}", self.recorder.is_some());
        if self.counting.take().is_some() {
            self.notice = Notice::Info(tr!("Recording cancelled", "Запись отменена").into());
        } else if self.recorder.is_some() {
            self.stop(Some(ctx));
        } else if self.countdown {
            if let Some((_, region)) = self.target() {
                self.counting = Some(Countdown::show(region, COUNTDOWN_SECONDS, ctx.clone()));
            }
        } else {
            self.start(ctx);
        }
    }

    /// The display and the area to record, fitted to it; `None` with the
    /// reason in the status line.
    fn target(&mut self) -> Option<(Monitor, Region)> {
        self.monitors = display::monitors();
        let Some(monitor) = self.monitors.get(self.monitor).cloned() else {
            self.monitor = 0;
            self.notice = Notice::Error(tr!("No display found", "Экран не найден").into());
            return None;
        };
        let region = self.region.unwrap_or_else(|| Region::from_rect(monitor.rect));
        let Some(region) = region.fit(monitor.rect) else {
            self.region = None;
            self.notice = Notice::Error(tr!("The area is outside the display", "Область вне экрана").into());
            return None;
        };
        Some((monitor, region))
    }

    fn start(&mut self, ctx: &egui::Context) {
        let Some((monitor, region)) = self.target() else { return };
        if !self.folder.is_absolute() {
            self.notice = Notice::Error(tr!("Choose a folder for the recordings", "Выберите папку для записей").into());
            return;
        }
        if let Err(e) = std::fs::create_dir_all(&self.folder) {
            self.notice = Notice::Error(format!("{}: {e}", tr!("Cannot create the folder", "Не удаётся создать папку")));
            return;
        }
        let path = unique_path(&self.folder);
        let audio = self.audio.then(|| match &self.audio_app {
            Some(program) => Source::App { program: program.clone(), boost: self.boost },
            None => Source::System,
        });
        let config = Config { monitor, region, fps: self.fps, quality: self.quality, rate: self.rate, audio,
            microphone: self.microphone,
            cursor: self.cursor,
            clicks: self.clicks,
            follow: self.followed(),
            path,
        };
        let follow = config.follow.map(|window| (window, config.monitor.rect));
        match Recorder::start(config) {
            Ok(recorder) => {
                self.border = Some(Border::show(region, follow));
                let encoder = if recorder.info.hardware {
                    tr!("hardware encoder", "аппаратный кодер")
                } else {
                    tr!("software encoder", "программный кодер")
                };
                // An encoder that refused the constant quality records at the
                // bitrate: said, since the file can then be larger or worse.
                let fallback = if self.rate == RateControl::Quality && !recorder.info.constant_quality {
                    tr!(", at the fixed bitrate", ", заданный битрейт")
                } else {
                    ""
                };
                self.notice = Notice::Info(format!("{}×{}, {} fps, {encoder}{fallback}", region.width, region.height, self.fps));
                self.audio_warning = None;
                self.recorder = Some(recorder);
                // `minimise` leaves the window when the trimming window is open.
                self.restore_on_stop = self.minimise_on_record && !self.child_open() && self.window.is_some_and(win::is_shown);
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
        // Back as it was before the recording put it away (not at exit).
        if std::mem::take(&mut self.restore_on_stop)
            && ctx.is_some()
            && let Some(hwnd) = self.window
        {
            win::show_window(hwnd, true);
        }
        if let Some(recorder) = self.recorder.take() {
            let path = recorder.path.clone();
            log::debug!("stopping");
            self.audio_warning = recorder.audio_error();
            self.notice = match recorder.stop() {
                Ok(_) => {
                    self.relist();
                    if let (true, Some(ctx)) = (self.trim_after_record, ctx) {
                        self.open_editor(path.clone(), ctx);
                    }
                    Notice::Saved(path)
                }
                Err(e) => {
                    // Out of the way while recording (minimise_on_record):
                    // the error would go unseen.
                    if let Some(hwnd) = self.window {
                        win::show_window(hwnd, true);
                    }
                    Notice::Error(e)
                }
            };
        }
    }

    /// The selection over the screens, with the window hidden so that it
    /// does not cover what is to be recorded. A hidden window is not
    /// repainted, so the selection's thread shows it again itself. A
    /// window already out of the way (the hotkey) stays so.
    /// Opens the selection of an area or of a window (`mode`).
    fn select_area(&mut self, ctx: &egui::Context, mode: overlay::Mode) {
        self.monitors = display::monitors();
        let window = self.window.filter(|&hwnd| win::is_shown(hwnd));
        if let Some(hwnd) = window {
            win::show_window(hwnd, false);
        }
        let ctx = ctx.clone();
        self.selecting = Some(overlay::select(self.monitors.clone(), mode, self.aspect, move || {
            if let Some(hwnd) = window {
                win::show_window(hwnd, true);
            }
            ctx.request_repaint();
        }));
    }

    /// Whether a window drawn with this one's pass would stay on the screen
    /// without it: the trimming window, the recordings until they are
    /// docked (`dock` hides and shows them with this one). This one is
    /// then not minimised or hidden, or that one would freeze.
    fn child_open(&self) -> bool {
        self.editor.is_some() || (self.library.is_some() && self.dock.is_none())
    }

    /// The window without a taskbar button; only with the icon in the
    /// notification area, the one way back to a hidden window.
    fn tray_only(&self) -> bool {
        !self.taskbar && self.tray.is_some()
    }

    /// The window out of the way: hidden when it has no taskbar button
    /// (there is nothing to minimise to, and the icon brings it back),
    /// minimised otherwise. Not while the trimming window is open: it is
    /// drawn with this window's pass, which a minimised or hidden window
    /// does not run, so it would freeze.
    fn minimise(&self, ctx: &egui::Context) {
        if self.child_open() {
            return;
        }
        match (self.tray_only(), self.window) {
            (true, Some(hwnd)) => win::show_window(hwnd, false),
            _ => ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true)),
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
        self.place_editor(path, ctx);
    }

    /// Opens the trimming window on `path` where it was last, the main
    /// window left as it is.
    fn place_editor(&mut self, path: PathBuf, ctx: &egui::Context) {
        self.editor = None;
        // Where it was last, when its title row is still on a display.
        let ppp = ctx.pixels_per_point();
        let wanted = self.editor_rect.filter(|r| {
            let (x, y) = (r.min.x + 100.0, r.min.y + 10.0);
            self.monitors
                .iter()
                .any(|m| (m.rect.left as f32..m.rect.right as f32).contains(&x) && (m.rect.top as f32..m.rect.bottom as f32).contains(&y))
        });
        self.editor_placement = wanted.map(|r| egui::Rect::from_min_size((r.min.to_vec2() / ppp).to_pos2(), r.size()));
        self.editor_move = wanted.map(|r| (r.min.x.round() as i32, r.min.y.round() as i32));
        let mut editor = Editor::open(path, ctx.clone(), editor_viewport());
        editor.replace = self.trim_replace;
        self.editor = Some(editor);
        self.editor_created = false;
    }

    /// The trimming window, a viewport of its own.
    fn editor_window(&mut self, ctx: &egui::Context) {
        let Some(editor) = &mut self.editor else { return };
        // Not created in a pass that began with the main window hidden
        // (see `shown_at_start`): the next one is an ordinary one.
        if !self.editor_created && !self.shown_at_start {
            ctx.request_repaint();
            return;
        }
        self.editor_created = true;
        // Styled like the main window: its own title row and rounded
        // corners over a transparent window. Made hidden, shown once it
        // has painted (`OwnWindow`).
        let mut builder = OwnWindow::builder(egui::ViewportBuilder::default())
            .with_title(editor.title())
            .with_inner_size(self.editor_placement.map_or(crate::editor::WINDOW_SIZE.into(), |r| r.size()))
            .with_min_inner_size(crate::editor::MIN_WINDOW_SIZE)
            .with_decorations(false)
            // Not resizable for Windows: DWM draws its rectangular shadow
            // around a window with a sizing frame whatever its region, and
            // the transparent corners show. The editor's edges resize it.
            .with_resizable(false)
            .with_transparent(true)
            .with_icon(crate::embedded_icon());
        if let Some(placement) = self.editor_placement {
            builder = builder.with_position(placement.min);
        }
        let (close, rect) = ctx.show_viewport_immediate(editor_viewport(), builder, |ui, _class| {
            // The outer position (winit leaves a pixel of non-client area
            // above an undecorated window) in pixels, by this window's own
            // scale, with the inner size in points.
            let ppp = ui.ctx().pixels_per_point();
            let rect = ui.ctx().input(|i| {
                let v = i.viewport();
                Some(egui::Rect::from_min_size((v.outer_rect?.min.to_vec2() * ppp).to_pos2(), v.inner_rect?.size()))
            });
            (editor.ui(ui), rect)
        });
        // The guessed position was off (the display there has another
        // scale than the main window's): moved there in pixels, once, in
        // the frame the window is found, before it is seen.
        if let (Some((x, y)), Some(hwnd)) = (self.editor_move, editor.window()) {
            self.editor_move = None;
            let (left, top, _, _) = win::window_rect(hwnd);
            if (left, top) != (x, y) {
                log::debug!("trimming window moved from ({left}, {top}) to ({x}, {y})");
                win::move_window(hwnd, x, y);
            }
        }
        if let (Some(rect), None) = (rect, self.editor_move) {
            self.editor_rect = Some(rect);
        }
        self.trim_replace = editor.replace;
        if close {
            self.editor = None;
            // It may have written a cut or replaced the file.
            self.relist();
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
                paused: self.recorder.as_ref().is_some_and(Recorder::paused),
                tray_only: !self.taskbar,
                close_to_tray: self.close_to_tray,
                minimise_on_record: self.minimise_on_record,
                trim_after_record: self.trim_after_record,
                countdown: self.countdown,
                counting: self.counting.is_some(),
                recent: self
                    .recordings
                    .list
                    .iter()
                    .take(5)
                    .map(|r| (format!("{}   {}", recordings::when(r.modified), r.details()), r.path.clone()))
                    .collect(),
                update: self.updates.newer().map(|tag| update::version_of(tag).to_owned()),
            });
        }
    }

    /// The time recorded, `00:01:23`, while recording; the seconds left
    /// while counting down.
    fn clock(&self) -> Option<String> {
        if let Some(counting) = &self.counting {
            return Some(counting.left().max(1).to_string());
        }
        self.recorder.as_ref().map(|recorder| {
            let elapsed = recorder.elapsed().as_secs();
            format!("{:02}:{:02}:{:02}", elapsed / 3600, elapsed / 60 % 60, elapsed % 60)
        })
    }

    /// Shows the window cloaked at start (`App::new`) once it has painted a
    /// few frames, or after `UNCLOAK_TIMEOUT` whatever it did.
    fn uncloak(&mut self, ctx: &egui::Context) {
        let Some((until, frame)) = self.cloak else { return };
        if ctx.cumulative_frame_nr() >= frame + PAINTED_TO_SHOW || std::time::Instant::now() >= until {
            if let Some(hwnd) = self.window {
                win::cloak(hwnd, false);
            }
            self.cloak = None;
        } else {
            ctx.request_repaint();
        }
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

    /// The next key press, while a hotkey is being chosen. A chord the
    /// other hotkey has is taken from it.
    fn capture_hotkey(&mut self, ctx: &egui::Context, action: KeyAction) {
        let events = ctx.input(|i| i.events.clone());
        for event in events {
            if let egui::Event::Key { key, pressed: true, modifiers, .. } = event {
                if key == egui::Key::Escape {
                    self.capturing_hotkey = None;
                    self.register_hotkeys(ctx);
                    return;
                }
                if let Some(chord) = Chord::from_egui(key, modifiers)
                    && chord.is_usable()
                {
                    for other in [&mut self.record_key, &mut self.select_key, &mut self.pause_key] {
                        if other.chord == Some(chord) {
                            other.chord = None;
                        }
                    }
                    self.global_key(action).chord = Some(chord);
                    self.capturing_hotkey = None;
                    self.register_hotkeys(ctx);
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
            Some(r) => {
                let area = format!("{}×{} {} ({}, {})", r.width, r.height, tr!("at", "в точке"), r.x, r.y);
                match self.followed() {
                    Some(window) => {
                        let title = win::window_title(window);
                        tr!(format!("The window «{title}», {area}; the area follows it"), format!("Окно «{title}», {area}; область следует за ним"))
                    }
                    None => area,
                }
            }
            None => tr!("Whole display", "Весь экран").into(),
        }
    }

    /// The window the area was picked from, while the area is still the
    /// one picked and the window is there: the recording follows it.
    fn followed(&self) -> Option<isize> {
        self.window_picked.filter(|&(window, region)| self.region == Some(region) && win::window_bounds(window).is_some()).map(|(w, _)| w)
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.recordings.poll(&ctx);
        if let Some(action) = self.capturing_hotkey {
            self.capture_hotkey(&ctx, action);
        }
        // The settings are fixed from the countdown on.
        let recording = self.recorder.is_some() || self.counting.is_some();
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
            .frame(egui::Frame::new().inner_margin(egui::Margin { left: 12, right: 12, top: 14, bottom: 12 }))
            .show(ui, |ui| self.bottom_bar(ui, busy));
        egui::CentralPanel::default().frame(egui::Frame::new().inner_margin(12)).show(ui, |ui| {
            let drag = ui.interact(ui.max_rect(), ui.id().with("drag"), egui::Sense::click_and_drag());
            if drag.drag_started_by(egui::PointerButton::Primary) {
                ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
            }
            // The buttons of the title row from the right: close, minimise,
            // about, the recordings.
            let top_right = ui.max_rect().right_top();
            let corner = |n: f32| egui::Rect::from_min_size(egui::pos2(top_right.x - 28.0 * n + 4.0, top_right.y), Vec2::splat(24.0));
            // Only the cross hides the window: Alt+F4 and WM_CLOSE from
            // outside (the installer, a shutdown) still close the program.
            let hide_on_close = self.close_to_tray && self.tray.is_some();
            let kind = if hide_on_close { TitleButton::CloseToTray } else { TitleButton::Close };
            if title_button(&mut ui.new_child(egui::UiBuilder::new().max_rect(corner(1.0))), kind) {
                match (hide_on_close, self.window) {
                    // Not hidden while the trimming window is open (see
                    // `minimise`): the cross does nothing then.
                    (true, Some(hwnd)) => {
                        if !self.child_open() {
                            win::show_window(hwnd, false);
                        }
                    }
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
            let count = self.recordings.list.len();
            let count = if count >= recordings::COUNT { format!("{count}+") } else { count.to_string() };
            let list = recordings_button(ui, corner(3.0).left() - 4.0, &count, self.library.is_some());
            if list.clicked() {
                if self.library.is_some() {
                    self.library = None;
                    self.dock = None;
                } else {
                    self.open_library(&ctx);
                }
            }
            // A newer release, left of the list.
            let mut title_right = list.rect.left();
            if let Some(tag) = self.updates.newer().map(str::to_owned) {
                let button = update_button(ui, title_right - 4.0, &tag);
                title_right = button.rect.left();
                if button.clicked() {
                    let url = update::release_url(&tag);
                    if !win::shell_open(&url) {
                        log::warn!("could not open {url}");
                    }
                }
            }
            title(ui, egui::Rect::from_min_max(ui.max_rect().min, egui::pos2(title_right - 8.0, ui.max_rect().top() + 24.0)));
            ui.add_space(28.0);
            egui::Frame::group(ui.style()).inner_margin(10).show(ui, |ui| {
                ui.set_width(ui.available_width());
                self.settings(ui, &ctx, recording);
            });
        });

        self.about_window(&ctx);
        self.editor_window(&ctx);
        self.library_window(&ctx);
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

    /// Called before `ui` in every frame, and alone while the window is
    /// minimised or hidden (eframe runs no egui pass then): the hotkey,
    /// the tray and a failed recording are attended to all the same, so a
    /// recording can be stopped while the window is out of the way.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.uncloak(ctx);
        self.shown_at_start = self.window.is_none_or(win::is_shown);
        self.poll(ctx);
        self.updates.poll();
        if self.listed.elapsed() > Duration::from_secs(5) {
            self.relist();
        }
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
        storage.set_string(RATE_KEY, self.rate.name().to_owned());
        storage.set_string(AUDIO_KEY, self.audio.to_string());
        storage.set_string(AUDIO_APP_KEY, self.audio_app.clone().unwrap_or_default());
        storage.set_string(AUDIO_BOOST_KEY, self.boost.to_string());
        storage.set_string(TASKBAR_KEY, self.taskbar.to_string());
        storage.set_string(CLOSE_TO_TRAY_KEY, self.close_to_tray.to_string());
        storage.set_string(MINIMISE_ON_RECORD_KEY, self.minimise_on_record.to_string());
        storage.set_string(TRIM_AFTER_RECORD_KEY, self.trim_after_record.to_string());
        storage.set_string(COUNTDOWN_KEY, self.countdown.to_string());
        storage.set_string(LANGUAGE_KEY, self.lang.name().to_owned());
        storage.set_string(EDITOR_RECT_KEY, self.editor_rect.map(rect_to_setting).unwrap_or_default());
        storage.set_string(TRIM_REPLACE_KEY, self.trim_replace.to_string());
        storage.set_string(CHECK_UPDATES_KEY, self.updates.enabled.to_string());
        storage.set_string(LAST_UPDATE_CHECK_KEY, self.updates.last_check.to_string());
        storage.set_string(CURSOR_KEY, self.cursor.to_string());
        storage.set_string(CLICKS_KEY, self.clicks.to_string());
        storage.set_string(MICROPHONE_KEY, self.microphone.to_string());
        storage.set_string(FOLDER_KEY, self.folder.display().to_string());
        storage.set_string(HOTKEY_KEY, self.record_key.chord.map(|c| c.label()).unwrap_or_default());
        storage.set_string(SELECT_HOTKEY_KEY, self.select_key.chord.map(|c| c.label()).unwrap_or_default());
        storage.set_string(PAUSE_HOTKEY_KEY, self.pause_key.chord.map(|c| c.label()).unwrap_or_default());
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
        egui::Grid::new("settings").num_columns(2).spacing([16.0, 12.0]).show(ui, |ui| {
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
                segmented(ui, &mut self.aspect, &options, "");
                if self.aspect != before {
                    self.apply_aspect();
                }
            });
            ui.end_row();

            ui.label(tr!("Frame rate", "Частота кадров"));
            ui.add_enabled_ui(!recording, |ui| {
                ui.horizontal(|ui| {
                    segmented(ui, &mut self.fps, &[(30, "30"), (60, "60")], "");
                    ui.label(RichText::new(tr!("frames per second", "кадров в секунду")).weak());
                });
            });
            ui.end_row();

            ui.label(tr!("Quality", "Качество"));
            // What each step means for the encoder in the mode chosen.
            let steps: Vec<String> = Quality::ALL
                .iter()
                .map(|q| match self.rate {
                    RateControl::Quality => format!("{}: QP {}", q.label(), q.qp()),
                    RateControl::Bitrate => {
                        let bpp = format!("{}", q.bits_per_pixel()).replace('.', tr!(".", ","));
                        format!("{}: {bpp} {}", q.label(), tr!("bit per pixel and frame", "бит на пиксель и кадр"))
                    }
                })
                .collect();
            let hint = match self.rate {
                RateControl::Quality => tr!(
                    "The quantisation parameter of every frame: lower is better and larger (H.264 takes 0 to 51).",
                    "Параметр квантования каждого кадра: чем меньше, тем лучше и больше файл (у H.264 от 0 до 51)."
                ),
                RateControl::Bitrate => tr!(
                    "The bitrate is the area times the frame rate times this.",
                    "Битрейт равен площади области, умноженной на частоту кадров и на это число."
                ),
            };
            let hint = format!("{}

{hint}", steps.join("
"));
            ui.horizontal(|ui| {
                ui.add_enabled_ui(!recording, |ui| {
                    let options = Quality::ALL.map(|q| (q, q.label()));
                    segmented(ui, &mut self.quality, &options, &hint);
                });
                // The step chosen, as the encoder takes it.
                let value = match self.rate {
                    RateControl::Quality => format!("QP {}", self.quality.qp()),
                    RateControl::Bitrate => {
                        let bpp = format!("{}", self.quality.bits_per_pixel()).replace('.', tr!(".", ","));
                        format!("{bpp} {}", tr!("bit/pixel", "бит/пиксель"))
                    }
                };
                ui.label(RichText::new(value).small().weak()).on_hover_text(&hint);
            });
            ui.end_row();

            ui.label(tr!("Bitrate", "Битрейт"));
            ui.horizontal(|ui| {
                let hint = tr!(
                    "By quality: every frame is encoded to the same quality, and the file follows what is on the screen: a still one takes almost nothing, motion as much as it needs.
Fixed: the encoder is given a bitrate from the area, the frame rate and the quality; the size is known beforehand, and fast motion loses detail.",
                    "По качеству: каждый кадр кодируется с одинаковым качеством, и размер файла следует за происходящим на экране: неподвижный экран почти ничего не занимает, движение — столько, сколько нужно.
Заданный: кодеру задаётся битрейт по размеру области, частоте кадров и качеству; размер известен заранее, а при быстром движении теряются детали."
                );
                ui.add_enabled_ui(!recording, |ui| {
                    let options = RateControl::ALL.map(|r| (r, r.label()));
                    segmented(ui, &mut self.rate, &options, hint);
                });
                match self.rate {
                    // What the area at this rate and quality comes to.
                    RateControl::Bitrate => {
                        if let Some(estimate) = self.estimate() {
                            ui.label(RichText::new(estimate).small().weak()).on_hover_text(tr!(
                                "The most the recording takes: the bitrate given to the encoder, and the file per minute with the sound. A still screen takes less.",
                                "Наибольший объём записи: битрейт, заданный кодеру, и размер файла за минуту вместе со звуком. Неподвижный экран занимает меньше."
                            ));
                        }
                    }
                    RateControl::Quality => {
                        ui.label(RichText::new(tr!("size follows the motion", "размер зависит от движения")).small().weak());
                    }
                }
            });
            ui.end_row();

            ui.label(tr!("Record", "Записывать"));
            ui.add_enabled_ui(!recording, |ui| {
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.audio, tr!("Sound", "Звук"));
                    ui.add_space(8.0);
                    ui.checkbox(&mut self.microphone, tr!("Microphone", "Микрофон")).on_hover_text(tr!(
                        "The default microphone of Windows, mixed with the sound",
                        "Микрофон Windows по умолчанию, смешанный со звуком"
                    ));
                    ui.add_space(8.0);
                    ui.checkbox(&mut self.cursor, tr!("Pointer", "Указатель"));
                    ui.add_space(8.0);
                    ui.add_enabled(self.cursor, egui::Checkbox::new(&mut self.clicks, tr!("Clicks", "Щелчки"))).on_hover_text(tr!(
                        "Clicks of the mouse buttons shown under the pointer: yellow for the left one, blue for the right one",
                        "Щелчки кнопок мыши под указателем: жёлтые — левой, голубые — правой"
                    ));
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
                    self.relist();
                }
                if ui.button(tr!("Open", "Открыть")).on_hover_text(tr!("Open the folder in Explorer", "Открыть папку в Проводнике")).clicked() {
                    win::open_folder(&self.folder);
                }
            });
            ui.end_row();

            ui.label(tr!("Record key", "Клавиша записи"));
            self.hotkey_row(
                ui,
                ctx,
                recording,
                KeyAction::Record,
                tr!(
                    "Starts and stops the recording from anywhere. Click, then press a key with Ctrl or Alt, or a function key.",
                    "Начинает и останавливает запись из любого окна. Нажмите кнопку, затем клавишу с Ctrl или Alt, или функциональную клавишу."
                ),
            );
            ui.end_row();

            ui.label(tr!("Area key", "Клавиша области"));
            self.hotkey_row(
                ui,
                ctx,
                recording,
                KeyAction::Select,
                tr!(
                    "Opens the area selection from anywhere, as Select area… does; not while recording. Click, then press a key with Ctrl or Alt, or a function key.",
                    "Открывает выделение области из любого окна, как кнопка «Выделить область…»; не во время записи. Нажмите кнопку, затем клавишу с Ctrl или Alt, или функциональную клавишу."
                ),
            );
            ui.end_row();

            ui.label(tr!("Pause key", "Клавиша паузы"));
            self.hotkey_row(
                ui,
                ctx,
                recording,
                KeyAction::Pause,
                tr!(
                    "Pauses and resumes the recording from anywhere, as the pause button does. Click, then press a key with Ctrl or Alt, or a function key.",
                    "Приостанавливает и продолжает запись из любого окна, как кнопка паузы. Нажмите кнопку, затем клавишу с Ctrl или Alt, или функциональную клавишу."
                ),
            );
            ui.end_row();
        });
    }

    /// A hotkey, the button that changes it and the cross that removes it.
    fn hotkey_row(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, recording: bool, action: KeyAction, hint: &str) {
        let capturing = self.capturing_hotkey == Some(action);
        ui.horizontal_wrapped(|ui| {
            let chord = self.global_key(action).chord;
            if capturing {
                ui.label(RichText::new(tr!("Press the keys…", "Нажмите клавиши…")).color(ui.visuals().selection.bg_fill));
            } else if let Some(chord) = chord {
                ui.label(RichText::new(chord.label()).strong());
            } else {
                ui.weak(tr!("none", "нет"));
            }
            let text = if capturing { tr!("Cancel", "Отмена") } else { tr!("Change", "Изменить") };
            let button = ui.add_enabled(!recording, egui::Button::new(text)).on_hover_text(hint);
            if button.clicked() {
                if capturing {
                    self.capturing_hotkey = None;
                    self.register_hotkeys(ctx);
                } else {
                    // All keys freed while one is chosen, so any can be
                    // pressed.
                    self.capturing_hotkey = Some(action);
                    self.free_hotkeys();
                }
            }
            if chord.is_some()
                && !capturing
                && ui.add_enabled(!recording, cross_button).on_hover_text(tr!("Remove the hotkey", "Убрать клавишу")).clicked()
            {
                self.global_key(action).chord = None;
                // Not while the other is being chosen: its keys stay free.
                if self.capturing_hotkey.is_none() {
                    self.register_hotkeys(ctx);
                }
            }
            if let Some(e) = &self.global_key(action).error {
                // Usually another program (or another qrec) holds the key.
                ui.label(RichText::new(tr!("taken by another program", "занята другой программой")).color(ui.visuals().error_fg_color))
                    .on_hover_text(e);
            }
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
                // Three parts of one choice, the current one filled: an
                // area drawn with the mouse, a window, the whole display.
                ui.spacing_mut().item_spacing.x = 1.0;
                let size = Vec2::new(0.0, BAR_HEIGHT);
                let window = self.followed().is_some();
                let area = self.region.is_some() && !window;
                let r = BAR_CORNER;
                let left = egui::CornerRadius { nw: r, sw: r, ne: 0, se: 0 };
                let right = egui::CornerRadius { nw: 0, sw: 0, ne: r, se: r };
                let select = choice(ui, area, tr!("Area…", "Область…"), Some(16.0)).corner_radius(left).min_size(size);
                if ui.add(select).on_hover_text(tr!("Draw the area to record with the mouse", "Выделить записываемую область мышью")).clicked() {
                    self.select_area(ui.ctx(), overlay::Mode::Area);
                }
                let pick = choice(ui, window, tr!("Window…", "Окно…"), Some(16.0)).corner_radius(egui::CornerRadius::ZERO).min_size(size);
                if ui
                    .add(pick)
                    .on_hover_text(tr!("Click the window to record; the area follows it as it moves", "Щёлкнуть по окну для записи; область следует за ним при перемещении"))
                    .clicked()
                {
                    self.select_area(ui.ctx(), overlay::Mode::Window);
                }
                let whole = choice(ui, self.region.is_none(), tr!("Whole display", "Весь экран"), Some(16.0)).corner_radius(right).min_size(size);
                if ui.add(whole).clicked() {
                    self.region = None;
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let clock = self.clock();
                if record_button(ui, clock.as_deref(), self.selecting.is_none()) {
                    self.toggle(&ui.ctx().clone());
                }
                if let Some(paused) = self.recorder.as_ref().map(Recorder::paused) {
                    ui.add_space(6.0 - ui.spacing().item_spacing.x);
                    if pause_button(ui, paused) {
                        self.toggle_pause();
                    }
                }
            });
        });
        ui.add_space(10.0);
        // The last recording as a card of its own, or a line of text: what
        // the area is, what the recording does, what went wrong. Wrapped,
        // so a long message stays in the window.
        let height = ui.text_style_height(&egui::TextStyle::Body) * 2.0 + 14.0;
        let mut action = None;
        ui.allocate_ui_with_layout(Vec2::new(ui.available_width(), height), egui::Layout::top_down(egui::Align::LEFT), |ui| {
            ui.set_min_height(height);
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
            match &self.notice {
                Notice::None => {
                    ui.label(RichText::new(self.area_label()).weak());
                }
                Notice::Saved(path) => {
                    if let Some(warning) = &self.audio_warning {
                        ui.label(RichText::new(warning).color(ui.visuals().error_fg_color));
                    }
                    self.recording_card(ui, path, &mut action);
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
        });
        self.file_action(action, &ui.ctx().clone());
    }

    /// The last recording: its picture (a click trims it), its name (a
    /// link to it in Explorer), its length, size and picture size, and
    /// what can be done with it.
    fn recording_card(&self, ui: &mut egui::Ui, path: &Path, action: &mut Option<FileAction>) {
        egui::Frame::new()
            .fill(ui.visuals().faint_bg_color)
            .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
            .corner_radius(6)
            .inner_margin(egui::Margin::symmetric(8, 6))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    if thumbnail(ui, self.recordings.picture(path), Vec2::new(64.0, 36.0)).clicked() {
                        *action = Some(FileAction::Trim(path.to_path_buf()));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Some(a) = file_buttons(ui, path) {
                            *action = Some(a);
                        }
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 2.0;
                                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                                if file_link(ui, &name).on_hover_text(tr!("Show in Explorer", "Показать в Проводнике")).clicked() {
                                    *action = Some(FileAction::Show(path.to_path_buf()));
                                }
                                if let Some(r) = self.recordings.get(path) {
                                    ui.add(egui::Label::new(RichText::new(r.details()).small().weak()).truncate());
                                }
                            });
                        });
                    });
                });
            });
    }

    /// Lists the recordings of the folder again; new ones are read on the
    /// recordings' thread.
    fn relist(&mut self) {
        self.recordings.refresh(&self.folder);
        self.listed = std::time::Instant::now();
    }

    fn file_action(&mut self, action: Option<FileAction>, ctx: &egui::Context) {
        match action {
            Some(FileAction::Trim(path)) => self.open_editor(path, ctx),
            Some(FileAction::Show(path)) => win::show_in_explorer(&path),
            Some(FileAction::Discard(path)) => {
                self.discard(&path);
                self.relist();
            }
            Some(FileAction::Clear(paths)) => {
                self.clear(&paths);
                self.relist();
            }
            None => {}
        }
    }

    /// Moves `paths` to the Recycle Bin in one operation, closing the
    /// trimming window first when it shows one of them.
    fn clear(&mut self, paths: &[PathBuf]) {
        if self.editor.as_ref().is_some_and(|e| paths.contains(&e.path)) {
            self.editor = None;
        }
        let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
        let n = paths.len();
        self.notice = match win::recycle_all(&refs) {
            Ok(true) => Notice::Info(tr!(format!("{n} recordings are in the Recycle Bin"), format!("Записи ({n}) удалены в корзину"))),
            Ok(false) => Notice::Info(tr!(format!("{n} recordings are deleted (the drive has no Recycle Bin)"), format!("Записи ({n}) удалены (у диска нет корзины)"))),
            Err(e) => Notice::Error(format!("{}: {e}", tr!("The recordings could not be deleted", "Не удалось удалить записи"))),
        };
    }

    /// Opens the window that lists the recordings (or brings it to the
    /// front). It is drawn with the main window's pass, so that one is
    /// shown first, as for the trimming window.
    fn open_library(&mut self, ctx: &egui::Context) {
        if let Some(hwnd) = self.window {
            win::show_window(hwnd, true);
        }
        self.relist();
        if self.library.is_some() {
            ctx.send_viewport_cmd_to(library_viewport(), egui::ViewportCommand::Focus);
        } else {
            self.library = Some(Library::new());
            self.library_created = false;
        }
    }

    /// The window that lists the recordings, a viewport of its own, docked
    /// beside the main window (`dock`).
    fn library_window(&mut self, ctx: &egui::Context) {
        if self.library.is_none() {
            return;
        }
        // As for the trimming window (see `shown_at_start`).
        if !self.library_created && !self.shown_at_start {
            ctx.request_repaint();
            return;
        }
        let mut builder = OwnWindow::builder(egui::ViewportBuilder::default())
            .with_title(Library::title())
            .with_inner_size(crate::library::WINDOW_SIZE)
            .with_min_inner_size(crate::library::MIN_WINDOW_SIZE)
            .with_decorations(false)
            .with_resizable(false)
            .with_transparent(true)
            .with_icon(crate::embedded_icon());
        // Right of the main window, its tops level, when it is first made.
        if !self.library_created
            && let Some(main) = ctx.input(|i| i.viewport().outer_rect)
        {
            builder = builder.with_position(main.right_top() + Vec2::new(8.0, 0.0));
        }
        self.library_created = true;
        let mut actions = Vec::new();
        let left = self.dock.as_ref().is_some_and(crate::dock::Dock::left);
        let (library, recordings) = (&mut self.library, &self.recordings);
        let Some(library) = library else { return };
        let close = ctx.show_viewport_immediate(library_viewport(), builder, |ui, _class| library.ui(ui, recordings, left, &mut actions));
        // Docked once its window exists (found by its title), in the frame
        // it is found, while it is still hidden and cloaked.
        if self.dock.is_none()
            && let (Some(main), Some(side)) = (self.window, library.window())
        {
            self.dock = Some(crate::dock::Dock::new(main, side));
        }
        if close {
            self.library = None;
            self.dock = None;
        }
        for action in actions {
            self.file_action(Some(action), ctx);
        }
    }

    /// Moves a recording to the Recycle Bin, closing the trimming window
    /// first when it shows that file (its readers hold the file open).
    fn discard(&mut self, path: &Path) {
        if self.editor.as_ref().is_some_and(|e| e.path == path) {
            self.editor = None;
        }
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let notice = match win::recycle(path) {
            Ok(true) => Notice::Info(tr!(format!("{name} is in the Recycle Bin"), format!("{name} удалён в корзину"))),
            Ok(false) => Notice::Info(tr!(format!("{name} is deleted (the drive has no Recycle Bin)"), format!("{name} удалён (у диска нет корзины)"))),
            Err(e) => Notice::Error(format!("{}: {e}", tr!("The file could not be deleted", "Не удалось удалить файл"))),
        };
        // The card of the last recording stays when another one went from
        // the list (which shows it gone), unless that failed.
        if !matches!((&self.notice, &notice), (Notice::Saved(shown), Notice::Info(_)) if shown != path) {
            self.notice = notice;
        }
    }
}

/// `x,y,w,h` of a rectangle, as the settings keep it.
fn rect_to_setting(rect: egui::Rect) -> String {
    // Not rounded: a point of the size is a pixel and a half at 150 %.
    format!("{},{},{},{}", rect.min.x, rect.min.y, rect.width(), rect.height())
}

fn rect_from_setting(setting: &str) -> Option<egui::Rect> {
    let v: Vec<f32> = setting.split(',').map(|s| s.trim().parse().ok()).collect::<Option<_>>()?;
    match v[..] {
        [x, y, w, h] if w >= 64.0 && h >= 64.0 => Some(egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(w, h))),
        _ => None,
    }
}

/// The trimming window's viewport.
fn editor_viewport() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("editor")
}

fn library_viewport() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("library")
}

/// A window of the program's own (the trimming window, the recordings
/// window: immediate viewports) shown without a white flash: Windows shows
/// a new window white until its first OpenGL frame reaches DWM. It is made
/// hidden (`builder`), found by its title among this thread's windows
/// (`win::thread_window`: eframe gives no handle for a viewport), cloaked
/// (`win::cloak`), given what the caller does with it (caption, region,
/// place, dock) in the same frame, shown, and uncloaked once it has
/// painted a few frames.
#[derive(Default)]
pub(crate) struct OwnWindow {
    /// The window, once found.
    hwnd: Option<isize>,
    /// Frames painted since it was found, and the frames it was looked for.
    painted: u32,
    looked: u32,
}

impl OwnWindow {
    /// Frames painted before the window is uncloaked.
    const PAINTED: u32 = 3;
    /// Frames it is looked for before it is shown as it is.
    const LOOKED: u32 = 10;

    /// The builder of a window made hidden. Passed every frame: egui sends
    /// only what changed, so the window is not hidden again once shown.
    pub fn builder(builder: egui::ViewportBuilder) -> egui::ViewportBuilder {
        builder.with_visible(false)
    }

    /// Each frame inside the viewport's closure (`ctx` is the viewport's):
    /// finds the window titled `title`, cloaks and shows it, uncloaks it
    /// once it has painted. Its handle, once found.
    pub fn follow(&mut self, ctx: &egui::Context, title: &str) -> Option<isize> {
        let more = || {
            ctx.request_repaint();
            ctx.request_repaint_of(egui::ViewportId::ROOT);
        };
        match self.hwnd {
            None => {
                self.looked += 1;
                self.hwnd = win::thread_window(title);
                match self.hwnd {
                    // Shown already: the trimming window's viewport keeps
                    // its window when another file replaces the first.
                    Some(hwnd) if win::is_visible(hwnd) => self.painted = Self::PAINTED,
                    Some(hwnd) => {
                        win::cloak(hwnd, true);
                        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    }
                    // Not found: shown as it is, and still looked for (its
                    // corners).
                    None if self.looked == Self::LOOKED => ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true)),
                    None => {}
                }
                if self.looked <= Self::LOOKED {
                    more();
                }
            }
            Some(hwnd) if self.painted < Self::PAINTED => {
                // eframe drops a viewport's commands in a pass painted
                // outside winit's event handlers (the main window
                // minimised meanwhile): asked again until it is shown, and
                // the frames counted from then.
                if win::is_visible(hwnd) {
                    self.painted += 1;
                } else {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                }
                if self.painted == Self::PAINTED {
                    win::cloak(hwnd, false);
                }
                more();
            }
            Some(_) => {}
        }
        self.hwnd
    }

    /// The window's handle, once it has been found.
    pub fn hwnd(&self) -> Option<isize> {
        self.hwnd
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
    use super::{elide, rect_from_setting, rect_to_setting};

    #[test]
    fn keeps_the_editor_rect() {
        let rect = egui::Rect::from_min_size(egui::pos2(100.0, 50.5), egui::vec2(860.0, 640.0));
        assert_eq!(rect_to_setting(rect), "100,50.5,860,640");
        assert_eq!(rect_from_setting("100,50.5,860,640"), Some(rect));
        assert_eq!(rect_from_setting(""), None);
        assert_eq!(rect_from_setting("1,2,3,4"), None);
    }

    #[test]
    fn elides_in_the_middle() {
        assert_eq!(elide("short", 10), "short");
        assert_eq!(elide("C:\\Users\\Someone\\Videos\\Recordings", 16), "C:\\User…cordings");
        assert_eq!(elide("C:\\Users\\Someone\\Videos\\Recordings", 16).chars().count(), 16);
    }
}
