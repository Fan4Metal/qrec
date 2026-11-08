//! The controls of the main window drawn by the program: the name, the
//! buttons of the title row (window, About, the recordings, a newer
//! release), the record and pause buttons, choices side by side, and the
//! card of a recording (its picture, its name, trim and discard), some
//! of them shared with the trimming and recordings windows.

use std::path::Path;

use egui::{Color32, RichText, Vec2};

use super::{DARK_COLOUR, FileAction, RECORD_COLOUR};
use crate::region::Aspect;
use crate::update;

/// The name in the top left corner, its `q` in the dark of the icon
/// and its `rec` in the red of the record dot. Only painted, so it
/// drags the window like the rest of the free area.
pub(crate) fn title(ui: &egui::Ui, rect: egui::Rect) {
    let painter = ui.painter_at(rect);
    let font = egui::FontId::proportional(18.0);
    let mut job = egui::text::LayoutJob::default();
    job.append("q", 0.0, egui::TextFormat::simple(font.clone(), DARK_COLOUR));
    job.append("rec", 0.0, egui::TextFormat::simple(font, RECORD_COLOUR));
    let galley = ui.fonts_mut(|f| f.layout_job(job));
    let pos = egui::pos2(rect.left(), rect.center().y - galley.size().y / 2.0);
    let right = pos.x + galley.size().x;
    painter.galley(pos, galley, DARK_COLOUR);
    // A development build says which one it is; a release has it in About
    // only.
    if crate::VERSION.contains("-dev") {
        let colour = ui.visuals().weak_text_color();
        painter.text(egui::pos2(right + 8.0, rect.center().y + 1.0), egui::Align2::LEFT_CENTER, crate::VERSION, egui::FontId::proportional(11.0), colour);
    }
}

/// The height of the area buttons and of the record button.
pub(crate) const BAR_HEIGHT: f32 = 42.0;
/// The rounding of their corners.
pub(crate) const BAR_CORNER: u8 = 6;

/// How the proportions are named in the window.
pub(crate) fn aspect_label(aspect: Aspect) -> &'static str {
    match aspect {
        Aspect::Free => tr!("Free", "Свободные"),
        other => other.name(),
    }
}

/// A button that is one of the values of a choice: filled with the
/// selection colour when it is the current one.
pub(crate) fn choice(ui: &egui::Ui, selected: bool, text: &str, size: Option<f32>) -> egui::Button<'static> {
    let selection = ui.visuals().selection;
    let text = size.map_or_else(|| RichText::new(text), |size| RichText::new(text).size(size));
    if selected {
        egui::Button::new(text.color(selection.stroke.color)).fill(selection.bg_fill)
    } else {
        egui::Button::new(text)
    }
}

/// A few values side by side as one control, the current one filled.
/// `hint`, when not empty, is the tooltip of every option.
pub(crate) fn segmented<T: Copy + PartialEq>(ui: &mut egui::Ui, value: &mut T, options: &[(T, &str)], hint: &str) {
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
            let mut response = ui.add(button);
            if !hint.is_empty() {
                response = response.on_hover_text(hint);
            }
            if response.clicked() {
                *value = v;
            }
        }
    });
}

/// The record button: red, with a white dot and the word; while
/// recording, a white square, the stop, and the time recorded. True
/// when clicked.
pub(crate) fn record_button(ui: &mut egui::Ui, clock: Option<&str>, enabled: bool) -> bool {
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

/// The pause button beside the record button while recording, as high as
/// it and square: two bars, or a triangle to resume, filled with the
/// selection colour while paused. True when clicked.
pub(crate) fn pause_button(ui: &mut egui::Ui, paused: bool) -> bool {
    let selection = ui.visuals().selection;
    let button = egui::Button::new("").min_size(Vec2::splat(BAR_HEIGHT)).corner_radius(BAR_CORNER);
    let button = if paused { button.fill(selection.bg_fill) } else { button };
    let response = ui.add(button);
    let colour = if paused { selection.stroke.color } else { ui.style().interact(&response).fg_stroke.color };
    let c = response.rect.center();
    let painter = ui.painter();
    if paused {
        let points = vec![c + Vec2::new(-4.5, -7.0), c + Vec2::new(7.0, 0.0), c + Vec2::new(-4.5, 7.0)];
        painter.add(egui::Shape::convex_polygon(points, colour, egui::Stroke::NONE));
    } else {
        for x in [-3.5, 3.5] {
            painter.rect_filled(egui::Rect::from_center_size(c + Vec2::new(x, 0.0), Vec2::new(4.0, 14.0)), 1.0, colour);
        }
    }
    let hint = if paused { tr!("Resume recording", "Продолжить запись") } else { tr!("Pause recording", "Приостановить запись") };
    response.on_hover_text(hint).clicked()
}

#[derive(Clone, Copy)]
pub(crate) enum TitleButton {
    Minimise,
    Hide,
    Close,
    CloseToTray,
    About,
    /// An "i" too, with this text as its tooltip.
    Help(&'static str),
}

/// A 24 x 24 button of the title row: a dash that minimises (or hides
/// the window when it has no taskbar button) or a cross that closes (or
/// hides the window), as the dialogs of qview have it. True when clicked.
pub(crate) fn title_button(ui: &mut egui::Ui, kind: TitleButton) -> bool {
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
        TitleButton::About | TitleButton::Help(_) => {
            // An "i" in a circle.
            ui.painter().circle_stroke(c, r + 1.5, egui::Stroke::new(1.2, visuals.fg_stroke.color));
            ui.painter().circle_filled(c + Vec2::new(0.0, -2.8), 1.0, visuals.fg_stroke.color);
            ui.painter().line_segment([c + Vec2::new(0.0, -0.8), c + Vec2::new(0.0, 3.5)], stroke);
            match kind {
                TitleButton::Help(text) => text,
                _ => tr!("About", "О программе"),
            }
        }
    };
    response.on_hover_text(hint).clicked()
}

/// The title row's button of the window that lists the recordings, its
/// right edge at `right`: a list of pictures and lines, and how many
/// there are. Filled while the window is open; a click opens or closes it.
pub(crate) fn recordings_button(ui: &mut egui::Ui, right: f32, count: &str, open: bool) -> egui::Response {
    let font = egui::FontId::proportional(13.0);
    let galley = ui.painter().layout_no_wrap(count.to_owned(), font, ui.visuals().text_color());
    let width = 6.0 + 14.0 + 5.0 + galley.size().x + 7.0;
    let top = ui.max_rect().top();
    let rect = egui::Rect::from_min_max(egui::pos2(right - width, top), egui::pos2(right, top + 24.0));
    let response = ui.interact(rect, ui.id().with("recordings"), egui::Sense::click());
    let visuals = ui.style().interact(&response);
    let painter = ui.painter();
    if open {
        painter.rect_filled(rect, 3.0, ui.visuals().selection.bg_fill);
    } else if response.hovered() {
        painter.rect_filled(rect, 3.0, visuals.bg_fill);
    }
    let colour = if open { ui.visuals().selection.stroke.color } else { visuals.fg_stroke.color };
    // Three rows, each a small picture and a line.
    let stroke = egui::Stroke::new(1.5, colour);
    let left = rect.left() + 6.0;
    for i in -1..=1 {
        let y = rect.center().y + i as f32 * 4.5;
        painter.rect_filled(egui::Rect::from_center_size(egui::pos2(left + 2.0, y), Vec2::new(4.0, 3.0)), 0.5, colour);
        painter.line_segment([egui::pos2(left + 6.0, y), egui::pos2(left + 14.0, y)], stroke);
    }
    painter.galley(egui::pos2(left + 19.0, rect.center().y - galley.size().y / 2.0), galley, colour);
    let hint = if open {
        tr!("Close the list of recordings", "Закрыть список записей")
    } else {
        tr!("All the recordings of the folder, beside this window", "Все записи папки, рядом с этим окном")
    };
    response.on_hover_text(hint)
}

/// The title row's button of a newer release, its right edge at `right`:
/// a download arrow and the version in the link colour; a click opens the
/// release page. Shown only while one has been found.
pub(crate) fn update_button(ui: &mut egui::Ui, right: f32, tag: &str) -> egui::Response {
    let version = update::version_of(tag);
    let colour = ui.visuals().hyperlink_color;
    let galley = ui.painter().layout_no_wrap(version.to_owned(), egui::FontId::proportional(13.0), colour);
    let width = 6.0 + 14.0 + 5.0 + galley.size().x + 7.0;
    let top = ui.max_rect().top();
    let rect = egui::Rect::from_min_max(egui::pos2(right - width, top), egui::pos2(right, top + 24.0));
    let response = ui.interact(rect, ui.id().with("update"), egui::Sense::click());
    let painter = ui.painter();
    if response.hovered() {
        painter.rect_filled(rect, 3.0, ui.style().interact(&response).bg_fill);
    }
    // The arrow down onto a tray, on the 14 pt grid of the family's icons,
    // its ends round (egui ends a stroke flat).
    let c = egui::pos2(rect.left() + 6.0 + 7.0, rect.center().y);
    let p = |x: f32, y: f32| c + Vec2::new(x, y);
    let stroke = egui::Stroke::new(1.5, colour);
    let lines = [vec![p(0.0, -5.5), p(0.0, 2.5)], vec![p(-4.0, -1.5), p(0.0, 2.5), p(4.0, -1.5)], vec![p(-5.5, 5.5), p(5.5, 5.5)]];
    for line in lines {
        for end in [line[0], line[line.len() - 1]] {
            painter.circle_filled(end, stroke.width / 2.0, colour);
        }
        painter.add(egui::Shape::line(line, stroke));
    }
    painter.galley(egui::pos2(rect.left() + 25.0, rect.center().y - galley.size().y / 2.0), galley, colour);
    response
        .on_hover_text(tr!(
            format!("Version {version} is available: open its release page"),
            format!("Доступна версия {version}: открыть страницу выпуска")
        ))
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A square button with a cross, as high as the buttons beside it.
pub(crate) fn cross_button(ui: &mut egui::Ui) -> egui::Response {
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

/// A recording's picture in a box of `size` (dark until it is decoded),
/// fitted inside it; a click trims the recording.
pub(crate) fn thumbnail(ui: &mut egui::Ui, texture: Option<&egui::TextureHandle>, size: Vec2) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let painter = ui.painter();
    painter.rect_filled(rect, 4.0, DARK_COLOUR);
    if let Some(texture) = texture {
        let [w, h] = texture.size();
        let scale = (rect.width() / w as f32).min(rect.height() / h as f32);
        let image = egui::Rect::from_center_size(rect.center(), Vec2::new(w as f32, h as f32) * scale);
        painter.image(texture.id(), image, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
    }
    if response.hovered() {
        painter.rect_stroke(rect, 4.0, egui::Stroke::new(1.5, ui.visuals().selection.bg_fill), egui::StrokeKind::Inside);
    }
    response.on_hover_text(tr!("Trim", "Обрезать")).on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A file's name as a link, cut short with an ellipsis when it does not
/// fit.
pub(crate) fn file_link(ui: &mut egui::Ui, name: &str) -> egui::Response {
    let colour = ui.visuals().hyperlink_color;
    let response = ui.add(egui::Label::new(RichText::new(name).color(colour)).truncate().sense(egui::Sense::click()));
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response
}

/// What a small icon button shows.
#[derive(Clone, Copy)]
pub(crate) enum Icon {
    Scissors,
    Trash,
}

/// A button with a drawn icon: an ordinary egui button (its fill, its
/// hover and press), the icon in the text colour on the 14-point grid of
/// the family's catalog (`Cut`, `Delete`), drawn at 16 points. The bin
/// turns red under the pointer.
pub(crate) fn icon_button(ui: &mut egui::Ui, icon: Icon) -> egui::Response {
    const BUTTON: Vec2 = Vec2::new(30.0, 26.0);
    const SIZE: f32 = 16.0;
    let response = ui.add(egui::Button::new("").min_size(BUTTON));
    let visuals = ui.style().interact(&response);
    let colour = match icon {
        Icon::Trash if response.hovered() => ui.visuals().error_fg_color,
        _ => visuals.text_color(),
    };
    let c = response.rect.center();
    let k = SIZE / 14.0;
    let p = |x: f32, y: f32| c + Vec2::new(x, y) * k;
    let line = egui::Stroke::new(1.3 * k, colour);
    let thin = egui::Stroke::new(1.0 * k, colour);
    let painter = ui.painter();
    match icon {
        Icon::Scissors => {
            // Two rings on the left, a handle from each crossing the other
            // in the middle into a solid blade pointing right, rounded on
            // its outer edge at the tip.
            let ring = egui::Stroke::new(1.4 * k, colour);
            let handle = egui::Stroke::new(1.3 * k, colour);
            let blade = [(-1.1, -0.9), (5.1, -5.15), (5.9, -5.2), (6.5, -4.8), (6.8, -4.2), (6.8, -3.8), (1.2, 0.1)];
            for side in [-1.0f32, 1.0] {
                let centre = Vec2::new(-4.6, 3.6 * side);
                painter.circle_stroke(p(centre.x, centre.y), 1.6 * k, ring);
                let end = Vec2::new(0.6, -0.5 * side);
                let start = centre + (end - centre).normalized() * 1.6;
                painter.line_segment([p(start.x, start.y), p(end.x, end.y)], handle);
                // Clockwise on the screen: mirrored, the points go the other way.
                let mut points: Vec<egui::Pos2> = blade.iter().map(|&(x, y)| p(x, y * side)).collect();
                if side < 0.0 {
                    points.reverse();
                }
                painter.add(egui::Shape::convex_polygon(points, colour, egui::Stroke::NONE));
            }
        }
        Icon::Trash => {
            // The handle, the lid, the body narrowing downwards, two ribs.
            painter.add(egui::Shape::line(vec![p(-2.2, -4.8), p(-2.2, -6.6), p(2.2, -6.6), p(2.2, -4.8)], line));
            painter.line_segment([p(-6.2, -4.8), p(6.2, -4.8)], line);
            painter.add(egui::Shape::line(vec![p(-4.4, -2.6), p(-3.5, 6.2), p(3.5, 6.2), p(4.4, -2.6)], line));
            painter.line_segment([p(-1.3, -0.4), p(-1.1, 4.0)], thin);
            painter.line_segment([p(1.3, -0.4), p(1.1, 4.0)], thin);
        }
    }
    response
}

/// Trim and move to the Recycle Bin (the name shows the file in
/// Explorer): laid out right to left, so the bin is last.
pub(crate) fn file_buttons(ui: &mut egui::Ui, path: &Path) -> Option<FileAction> {
    ui.spacing_mut().item_spacing.x = 4.0;
    let mut action = None;
    if icon_button(ui, Icon::Trash).on_hover_text(tr!("Move the file to the Recycle Bin", "Удалить файл в корзину")).clicked() {
        action = Some(FileAction::Discard(path.to_path_buf()));
    }
    if icon_button(ui, Icon::Scissors)
        .on_hover_text(tr!("Trim: cut the start and the end off, without re-encoding", "Обрезать: отрезать начало и конец без перекодирования"))
        .clicked()
    {
        action = Some(FileAction::Trim(path.to_path_buf()));
    }
    action
}

