//! The window that lists the recordings of the folder (`recordings`),
//! newest first, each with its picture, when it was made, its name,
//! length, size and picture size, and its buttons: trim, move to the
//! Recycle Bin; below them how many there are and how much they take,
//! and Clear, which moves them all to the Recycle Bin once confirmed.
//!
//! Styled like the trimming window: an immediate viewport of the main
//! window without decorations, a rounded background over a transparent
//! window, its own title row. Docked to the main window (`dock`): its top
//! and height are the main window's, its outer edge resizes it, and
//! dragging it moves both.

use std::path::PathBuf;

use egui::{RichText, Sense, Vec2};

use crate::app::{CORNER_RADIUS, FileAction, OwnWindow, TitleButton, file_buttons, file_link, thumbnail, title_button};
use crate::recordings::{self, Recordings};
use crate::win;

/// The window's size in points when it opens, and the least it can be.
pub const WINDOW_SIZE: [f32; 2] = [640.0, 640.0];
pub const MIN_WINDOW_SIZE: [f32; 2] = [440.0, 320.0];

/// A recording's picture in the list, 16:9.
const PICTURE: Vec2 = Vec2::new(224.0, 126.0);

pub struct Library {
    /// The window, made hidden and shown once found by its title, and the
    /// size and radius its corners were last rounded for.
    own: OwnWindow,
    rounded: Option<(Vec2, i32)>,
    /// Clear was pressed and waits for the answer.
    confirm: bool,
}

impl Library {
    pub fn new() -> Library {
        Library { own: OwnWindow::default(), rounded: None, confirm: false }
    }

    /// The window's handle, once it has been found.
    pub fn window(&self) -> Option<isize> {
        self.own.hwnd()
    }

    /// The window's title: what the taskbar shows, and how the window is
    /// found for its rounded corners.
    pub fn title() -> String {
        format!("qrec — {}", tr!("Recordings", "Записи"))
    }

    /// The window's contents; what was asked of the recordings goes to
    /// `actions`. `true` when the window is to close.
    /// `left`: docked at the left of the main window, so resized by its
    /// left edge.
    pub fn ui(&mut self, ui: &mut egui::Ui, recordings: &Recordings, left: bool, actions: &mut Vec<FileAction>) -> bool {
        let ctx = ui.ctx().clone();
        let mut close = ctx.input(|i| i.viewport().close_requested());
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            if self.confirm {
                self.confirm = false;
            } else {
                close = true;
            }
        }
        ui.painter().rect(
            ctx.content_rect(),
            CORNER_RADIUS,
            ui.visuals().panel_fill,
            egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
            egui::StrokeKind::Inside,
        );
        self.round_corners(&ctx);
        crate::editor::resize_edges(ui, Some(if left { egui::ResizeDirection::West } else { egui::ResizeDirection::East }));
        egui::Panel::top("title")
            .show_separator_line(false)
            .frame(egui::Frame::new().inner_margin(egui::Margin { left: 12, right: 8, top: 8, bottom: 4 }))
            .show(ui, |ui| {
                let row = ui.max_rect();
                let drag = ui.interact(row, ui.id().with("drag"), Sense::click_and_drag());
                if drag.drag_started_by(egui::PointerButton::Primary) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }
                ui.allocate_ui_with_layout(Vec2::new(row.width(), 24.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.label(RichText::new(tr!("Recordings", "Записи")).strong());
                    ui.weak(recordings.list.len().to_string());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if title_button(ui, TitleButton::Close) {
                            close = true;
                        }
                    });
                });
            });
        egui::Panel::bottom("clear")
            .show_separator_line(false)
            .frame(egui::Frame::new().inner_margin(12))
            .show(ui, |ui| self.bottom(ui, recordings, actions));
        egui::CentralPanel::default()
            .frame(egui::Frame::new().inner_margin(egui::Margin { left: 12, right: 4, top: 4, bottom: 0 }))
            .show(ui, |ui| {
                if recordings.list.is_empty() {
                    ui.add_space(24.0);
                    ui.vertical_centered(|ui| ui.weak(tr!("No recordings in the folder", "В папке нет записей")));
                    return;
                }
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    for (i, r) in recordings.list.iter().enumerate() {
                        if i > 0 {
                            ui.separator();
                        }
                        ui.horizontal(|ui| {
                            if thumbnail(ui, recordings.picture(&r.path), PICTURE).clicked() {
                                actions.push(FileAction::Trim(r.path.clone()));
                            }
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                // The scroll bar's place.
                                ui.add_space(12.0);
                                actions.extend(file_buttons(ui, &r.path));
                                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                    ui.vertical(|ui| {
                                        ui.spacing_mut().item_spacing.y = 4.0;
                                        ui.label(RichText::new(recordings::when(r.modified)).strong().size(15.0));
                                        if file_link(ui, &r.name()).on_hover_text(tr!("Show in Explorer", "Показать в Проводнике")).clicked() {
                                            actions.push(FileAction::Show(r.path.clone()));
                                        }
                                        ui.add(egui::Label::new(RichText::new(r.details()).weak()).truncate());
                                    });
                                });
                            });
                        });
                    }
                });
            });
        if close {
            self.confirm = false;
        }
        close
    }

    /// How many recordings there are and how much they take, and Clear:
    /// asked, then confirmed in place.
    fn bottom(&mut self, ui: &mut egui::Ui, recordings: &Recordings, actions: &mut Vec<FileAction>) {
        let count = recordings.list.len();
        ui.horizontal(|ui| {
            ui.weak(format!("{} · {}", count_of(count), recordings::size(recordings.bytes())));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.confirm {
                    let delete = egui::Button::new(RichText::new(tr!("Move to the Recycle Bin", "Удалить в корзину")).color(ui.visuals().error_fg_color));
                    if ui.add(delete).clicked() {
                        let paths: Vec<PathBuf> = recordings.list.iter().map(|r| r.path.clone()).collect();
                        actions.push(FileAction::Clear(paths));
                        self.confirm = false;
                    }
                    if ui.button(tr!("Cancel", "Отмена")).clicked() {
                        self.confirm = false;
                    }
                    ui.label(tr!(format!("All {count} to the Recycle Bin?"), format!("Все {count} — в корзину?")));
                } else if ui
                    .add_enabled(count > 0, egui::Button::new(tr!("Clear…", "Очистить…")))
                    .on_hover_text(tr!("Move all the recordings of the list to the Recycle Bin", "Удалить все записи списка в корзину"))
                    .clicked()
                {
                    self.confirm = true;
                }
            });
        });
    }

    /// Clips the window to rounded corners, as the trimming window does
    /// (`Editor::round_corners`); docked by the app in the frame it is
    /// found, while still cloaked.
    fn round_corners(&mut self, ctx: &egui::Context) {
        let Some(hwnd) = self.own.follow(ctx, &Self::title()) else { return };
        if win::strip_caption(hwnd) {
            self.rounded = None;
        }
        let ppp = ctx.pixels_per_point();
        let wanted = ((ctx.content_rect().size() * ppp).round(), (CORNER_RADIUS * ppp).round() as i32 - 1);
        if self.rounded != Some(wanted) && win::round_window(hwnd, wanted.1) {
            self.rounded = Some(wanted);
        }
    }
}

/// `5 recordings`, `5 записей`.
fn count_of(n: usize) -> String {
    tr!(format!("{n} {}", if n == 1 { "recording" } else { "recordings" }), format!("{n} {}", ru_recordings(n)))
}

/// The Russian word for `n` recordings.
fn ru_recordings(n: usize) -> &'static str {
    match (n % 10, n % 100) {
        (1, r) if r != 11 => "запись",
        (2..=4, r) if !(12..=14).contains(&r) => "записи",
        _ => "записей",
    }
}

#[cfg(test)]
mod tests {
    use super::ru_recordings;

    #[test]
    fn counts_in_russian() {
        assert_eq!(ru_recordings(1), "запись");
        assert_eq!(ru_recordings(3), "записи");
        assert_eq!(ru_recordings(5), "записей");
        assert_eq!(ru_recordings(11), "записей");
        assert_eq!(ru_recordings(22), "записи");
        assert_eq!(ru_recordings(112), "записей");
    }
}
