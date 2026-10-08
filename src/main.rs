#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[macro_use]
mod i18n;
mod app;
mod audio;
mod capture;
mod cli;
mod convert;
mod cursor;
mod display;
mod encoder;
mod hotkey;
mod icon;
mod overlay;
mod recorder;
mod region;
mod venc;
mod win;

use std::path::PathBuf;

/// Version from Cargo.toml, shown in the window; a development version
/// ("0.1.0-dev") carries the commit it was built from (`build.rs`).
pub const VERSION: &str = env!("QREC_VERSION");
/// eframe app id; also names the settings folder in `%APPDATA%`.
pub const APP_ID: &str = "qrec";
/// The settings file; one beside the exe (the portable archive ships it
/// empty) keeps the settings there instead of in `%APPDATA%`.
pub const SETTINGS_FILE: &str = "app.ron";

/// The folder beside the exe when it holds `SETTINGS_FILE`: the program
/// is portable and keeps its settings there.
fn portable_dir() -> Option<PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    dir.join(SETTINGS_FILE).is_file().then_some(dir)
}

/// The app icon rasterised at build time (`build.rs`) as straight RGBA,
/// 64 x 64 pixels.
fn embedded_icon() -> egui::IconData {
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
    // the selection overlay, the icon for the installer.
    match std::env::args().nth(1).as_deref() {
        Some("--record") => {
            win::attach_parent_console();
            std::process::exit(cli::record(std::env::args().skip(2).collect()));
        }
        Some("--test-select") => {
            win::attach_parent_console();
            std::process::exit(cli::test_select());
        }
        Some("--export-icon") => {
            win::attach_parent_console();
            std::process::exit(cli::export_icon(std::env::args_os().nth(2).map(PathBuf::from)));
        }
        Some(other) => {
            win::attach_parent_console();
            eprintln!("qrec: unknown argument {other}");
            std::process::exit(2);
        }
        None => {}
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("qrec")
            .with_inner_size(app::WINDOW_SIZE)
            .with_resizable(false)
            .with_maximize_button(false)
            .with_icon(embedded_icon()),
        renderer: eframe::Renderer::Glow,
        centered: true,
        persist_window: false,
        persistence_path: portable_dir().map(|d| d.join(SETTINGS_FILE)),
        ..Default::default()
    };
    eframe::run_native(APP_ID, options, Box::new(|cc| Ok(Box::new(app::App::new(cc)))))
}

/// The release build aborts on a panic and has no console, so the window
/// would vanish without a word: say what happened in a message box.
fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default(info);
        let text = tr!(
            format!("qrec stopped because of an internal error.\n\n{info}"),
            format!("qrec остановлен из-за внутренней ошибки.\n\n{info}")
        );
        win::error_box("qrec", &text);
    }));
}
