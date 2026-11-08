//! The About window: the icon, the name, the version, what the program
//! does, the author, the homepage, the licence, where the settings are
//! kept, the interface language and the update check.

use egui::Vec2;

use super::widgets::{TitleButton, title_button};
use crate::i18n::LangChoice;
use crate::update::{self, Updates};
use crate::win;

/// Width of the language list, enough for its longest entry.
const LANG_WIDTH: f32 = 220.0;

impl super::App {
    /// About, as in qview: the icon, the name, the version, what the
    /// program does, the author, the homepage and the licence from
    /// Cargo.toml, where the settings are kept, the interface language and
    /// the update check.
    pub(super) fn about_window(&mut self, ctx: &egui::Context) {
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
            ui.set_width(340.0);
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
                    ui.weak(tr!("Settings: beside the program (portable)", "Настройки: рядом с программой (портативная версия)"))
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
                ui.add_space(6.0);
                ui.separator();
                ui.add_space(2.0);
                update_section(ui, &mut self.updates);
                ui.add_space(4.0);
            });
            closed
        });
        if modal.inner || modal.should_close() {
            self.about = false;
        }
    }
}

/// The update check in About: the start-up option, a button to check now
/// and the outcome.
fn update_section(ui: &mut egui::Ui, updates: &mut Updates) {
    let before = updates.enabled;
    ui.checkbox(&mut updates.enabled, tr!("Check for updates at start-up (once a day)", "Проверять обновления при запуске (раз в сутки)"))
        .on_hover_text(tr!(
            "Asks api.github.com for the latest release; nothing else is sent, and nothing is downloaded",
            "Запрашивает у api.github.com последний выпуск; больше ничего не отправляется и не скачивается"
        ));
    if updates.enabled && !before {
        updates.start_if_due(ui.ctx());
    }
    let checking = updates.status == update::Status::Checking;
    if ui.add_enabled(!checking, egui::Button::new(tr!("Check now", "Проверить сейчас"))).clicked() {
        updates.start(ui.ctx());
    }
    match &updates.status {
        update::Status::Unknown => {}
        update::Status::Checking => {
            ui.horizontal(|ui| {
                // Centred by hand, as the language list above.
                ui.add_space(((ui.available_width() - 110.0) / 2.0).max(0.0));
                ui.add(egui::Spinner::new());
                ui.weak(tr!("Checking…", "Проверка…"));
            });
        }
        update::Status::UpToDate => {
            ui.weak(tr!("This is the latest version", "Установлена последняя версия"));
        }
        update::Status::Newer(tag) => {
            let version = update::version_of(tag);
            let url = update::release_url(tag);
            if ui
                .link(tr!(format!("Version {version} is available"), format!("Доступна версия {version}")))
                .on_hover_text(url.as_str())
                .clicked()
                && !win::shell_open(&url)
            {
                log::warn!("could not open {url}");
            }
        }
        update::Status::Failed(e) => {
            ui.weak(tr!("Could not check for updates", "Не удалось проверить обновления")).on_hover_text(e);
        }
    }
}
