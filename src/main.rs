#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

/// A line to stderr in the command line modes. Unlike `eprintln!` it does
/// not panic when stderr is a pipe already closed (`qrec --info f |
/// Select-Object -First 1`): what cannot be written is dropped.
macro_rules! say {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), $($arg)*);
    }};
}

#[macro_use]
mod i18n;
mod app;
mod audio;
mod capture;
mod cli;
mod convert;
mod cursor;
mod display;
mod editor;
mod encoder;
mod hotkey;
mod icon;
mod instance;
mod overlay;
mod playback;
mod recorder;
mod region;
mod sessions;
mod tray;
mod trim;
mod venc;
mod win;

use std::path::PathBuf;

/// Version from Cargo.toml, shown in About; a development version
/// ("0.1.0-dev") carries the commit it was built from (`build.rs`) and is
/// also shown in the corner of the window.
pub const VERSION: &str = env!("QREC_VERSION");
/// eframe app id; also names the settings folder in `%APPDATA%`.
pub const APP_ID: &str = "qrec";
/// The settings file; one beside the exe (the portable archive ships it
/// empty) keeps the settings there instead of in `%APPDATA%`.
pub const SETTINGS_FILE: &str = "app.ron";

/// The folder beside the exe when it holds `SETTINGS_FILE`: the program
/// is portable and keeps its settings there. Looked up once: eframe takes
/// the settings path at start-up, and About shows where they go.
fn portable_dir() -> Option<PathBuf> {
    static DIR: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
        dir.join(SETTINGS_FILE).is_file().then_some(dir)
    })
    .clone()
}

/// The settings file: beside the exe when portable, else where eframe
/// keeps it (`%APPDATA%\qrec\data`).
fn settings_file() -> Option<PathBuf> {
    portable_dir().or_else(|| eframe::storage_dir(APP_ID)).map(|d| d.join(SETTINGS_FILE))
}

/// The app icon rasterised at build time (`build.rs`) as straight RGBA,
/// 64 x 64 pixels.
pub(crate) fn embedded_icon() -> egui::IconData {
    egui::IconData { rgba: include_bytes!(concat!(env!("OUT_DIR"), "/app_icon_64.rgba")).to_vec(), width: 64, height: 64 }
}

fn main() -> eframe::Result {
    // QREC_TRACE=1 logs what the recording does (to stderr).
    let mut log = env_logger::Builder::from_default_env();
    if std::env::var_os("QREC_TRACE").is_some() {
        log.filter_module("qrec", log::LevelFilter::Debug);
    }
    log.init();
    i18n::set_lang(i18n::system_lang());
    install_panic_hook();
    win::set_dpi_aware();

    // The command line modes: a recording without the window, a check of
    // the selection overlay, the icon for the installer, a cut.
    let mut open = None;
    match std::env::args().nth(1).as_deref() {
        Some("--record") => {
            console_mode();
            std::process::exit(cli::record(std::env::args().skip(2).collect()));
        }
        Some("--test-select") => {
            console_mode();
            std::process::exit(cli::test_select());
        }
        Some("--export-icon") => {
            console_mode();
            std::process::exit(cli::export_icon(std::env::args_os().nth(2).map(PathBuf::from)));
        }
        Some("--cut") => {
            console_mode();
            std::process::exit(cli::cut(std::env::args().skip(2).collect()));
        }
        Some("--info") => {
            console_mode();
            std::process::exit(cli::info(std::env::args_os().nth(2).map(PathBuf::from)));
        }
        // A recording to trim, as Explorer's "Open with" passes it.
        Some(file) if is_mp4(file) => open = Some(PathBuf::from(std::env::args_os().nth(1).unwrap_or_default())),
        Some(other) => {
            console_mode();
            say!("qrec: unknown argument {other}");
            std::process::exit(2);
        }
        None => {}
    }

    // One window at a time: a copy started while another runs brings that
    // one to the front, or has it open the file given, and exits.
    let Some(_claim) = instance::claim() else {
        if !instance::hand_over(open.as_deref()) {
            log::warn!("the running copy did not take the request");
        }
        return Ok(());
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("qrec")
            .with_inner_size(app::WINDOW_SIZE)
            .with_resizable(false)
            .with_maximize_button(false)
            // The title row is the program's own: the name, minimise and close.
            .with_decorations(false)
            // The corners are rounded by the program (app::CORNER_RADIUS):
            // Windows 10 has no rounded corners for windows of its own.
            .with_transparent(true)
            .with_icon(embedded_icon()),
        renderer: eframe::Renderer::Glow,
        centered: true,
        persist_window: false,
        persistence_path: portable_dir().map(|d| d.join(SETTINGS_FILE)),
        ..Default::default()
    };
    eframe::run_native(APP_ID, options, Box::new(move |cc| Ok(Box::new(app::App::new(cc, open)))))
}

fn is_mp4(arg: &str) -> bool {
    std::path::Path::new(arg).extension().is_some_and(|e| e.eq_ignore_ascii_case("mp4"))
}

/// Whether the program runs in a command line mode, with the console of
/// the process that started it.
static CONSOLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn console_mode() {
    CONSOLE.store(true, std::sync::atomic::Ordering::Relaxed);
    win::attach_parent_console();
}

/// The release build aborts on a panic and has no console, so the window
/// would vanish without a word: say what happened in a message box. A
/// command line mode has the console for that, and a message box would
/// hold up the script that ran it.
fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default(info);
        if CONSOLE.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let text = tr!(
            format!("qrec stopped because of an internal error.\n\n{info}"),
            format!("qrec остановлен из-за внутренней ошибки.\n\n{info}")
        );
        win::error_box("qrec", &text);
    }));
}
