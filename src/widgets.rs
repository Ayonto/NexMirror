//! Reusable, custom-painted controls in the NexMirror style.

use crate::theme::{self, *};
use egui::{Align, Align2, Color32, CornerRadius, Frame, Layout, Margin, Response, RichText, Sense, Stroke, Ui, Vec2};

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Primary,
    Secondary,
    Danger,
    Ghost,
}

pub fn button(ui: &mut Ui, kind: Kind, glyph: Option<&str>, text: &str, enabled: bool) -> Response {
    let font = theme::semibold(13.5);
    let galley = ui.painter().layout_no_wrap(text.to_string(), font, TEXT);
    let icon_only = text.is_empty();
    let icon_w = if glyph.is_some() && !icon_only { 22.0 } else { 0.0 };
    let w = if icon_only { 34.0 } else { (galley.size().x + icon_w + 32.0).max(72.0) };
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 34.0), if enabled { Sense::click() } else { Sense::hover() });
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let (hov, down) = (enabled && resp.hovered(), enabled && resp.is_pointer_button_down_on());
    let (fill, stroke, fg) = match kind {
        Kind::Primary => (if down { ACCENT_LO } else if hov { ACCENT_HI } else { ACCENT }, Color32::TRANSPARENT, Color32::WHITE),
        Kind::Secondary => (if hov && !down { RAISED_HI } else { RAISED }, if hov { BORDER_HI } else { BORDER }, TEXT),
        Kind::Danger => (if hov { RED } else { soft(RED, 34) }, if hov { RED } else { soft(RED, 90) }, if hov { Color32::WHITE } else { RED }),
        Kind::Ghost => (if down { RAISED } else if hov { CARD_HI } else { Color32::TRANSPARENT }, Color32::TRANSPARENT, if hov { TEXT } else { TEXT_2 }),
    };
    let a = if enabled { 1.0 } else { 0.4 };
    let p = ui.painter();
    p.rect(rect, CornerRadius::same(6), fill.gamma_multiply(a), Stroke::new(1.0f32, stroke.gamma_multiply(a)), egui::StrokeKind::Inside);
    if icon_only {
        if let Some(g) = glyph {
            p.text(rect.center(), Align2::CENTER_CENTER, g, theme::icons(14.0), fg.gamma_multiply(a));
        }
    } else {
        let mut x = rect.center().x - (galley.size().x + icon_w) / 2.0;
        if let Some(g) = glyph {
            p.text(egui::pos2(x + 7.0, rect.center().y + 0.5), Align2::CENTER_CENTER, g, theme::icons(14.0), fg.gamma_multiply(a));
            x += icon_w;
        }
        p.galley_with_override_text_color(egui::pos2(x, rect.center().y - galley.size().y / 2.0), galley, fg.gamma_multiply(a));
    }
    if hov {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

pub fn icon_button(ui: &mut Ui, glyph: &str, tip: &str) -> Response {
    button(ui, Kind::Ghost, Some(glyph), "", true).on_hover_text(tip)
}

/// Windows 11 style toggle switch.
pub fn toggle(ui: &mut Ui, on: &mut bool) -> Response {
    let (rect, mut resp) = ui.allocate_exact_size(Vec2::new(42.0, 22.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool_with_time(resp.id, *on, 0.12);
    let p = ui.painter();
    let r = CornerRadius::same(11);
    if *on {
        p.rect_filled(rect, r, if resp.hovered() { ACCENT_HI } else { ACCENT });
    } else {
        p.rect(rect, r, if resp.hovered() { CARD_HI } else { Color32::TRANSPARENT }, Stroke::new(1.2f32, TEXT_2), egui::StrokeKind::Inside);
    }
    let x = egui::lerp(rect.left() + 11.0..=rect.right() - 11.0, t);
    let rad = if resp.hovered() { 7.0 } else { 6.0 } + t * 0.5;
    p.circle_filled(egui::pos2(x, rect.center().y), rad, if *on { Color32::WHITE } else { TEXT_2 });
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

pub fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0f32, BORDER))
        .corner_radius(10)
        .inner_margin(Margin::symmetric(22, 20))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

pub fn card_title(ui: &mut Ui, title: &str, desc: Option<&str>) {
    ui.label(RichText::new(title).font(theme::semibold(16.0)).color(TEXT));
    if let Some(d) = desc {
        ui.add_space(-4.0);
        ui.label(RichText::new(d).size(13.0).color(TEXT_3));
    }
    ui.add_space(6.0);
}

pub fn page_header(ui: &mut Ui, title: &str, subtitle: &str) {
    ui.label(RichText::new(title).font(theme::semibold(27.0)).color(TEXT));
    ui.add_space(-2.0);
    ui.label(RichText::new(subtitle).size(14.0).color(TEXT_2));
    ui.add_space(20.0);
}

pub fn divider(ui: &mut Ui) {
    let r = ui.available_rect_before_wrap();
    let y = ui.cursor().top() + 4.0;
    ui.painter().hline(r.left()..=r.right(), y, Stroke::new(1.0f32, BORDER));
    ui.add_space(9.0);
}

/// Label/description on the left, control on the right.
pub fn setting_row(ui: &mut Ui, title: &str, desc: &str, control: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.set_min_height(46.0);
        ui.vertical(|ui| {
            ui.add_space(2.0);
            ui.label(RichText::new(title).size(14.0).color(TEXT));
            if !desc.is_empty() {
                ui.add_space(-6.0);
                ui.label(RichText::new(desc).size(12.5).color(TEXT_3));
            }
        });
        ui.with_layout(Layout::right_to_left(Align::Center), control);
    });
}

pub fn badge(ui: &mut Ui, text: &str, color: Color32) {
    let galley = ui.painter().layout_no_wrap(text.to_string(), theme::semibold(10.5), color);
    let (rect, _) = ui.allocate_exact_size(galley.size() + Vec2::new(14.0, 6.0), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(255), soft(color, 34));
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, color);
}

pub fn dot(ui: &mut Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 3.5, color);
}

pub fn icon_tile(ui: &mut Ui, glyph: &str, size: f32, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same((size * 0.25) as u8), soft(color, 30));
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, glyph, theme::icons(size * 0.45), color);
}

pub fn banner(ui: &mut Ui, color: Color32, glyph: &str, text: &str) {
    Frame::new().fill(soft(color, 22)).stroke(Stroke::new(1.0f32, soft(color, 80))).corner_radius(8).inner_margin(Margin::symmetric(14, 10)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(RichText::new(glyph).font(theme::icons(15.0)).color(color));
            ui.add_space(4.0);
            ui.add(egui::Label::new(RichText::new(text).size(13.0).color(TEXT)).wrap());
        });
    });
}

pub fn keycap(ui: &mut Ui, key: &str) {
    let galley = ui.painter().layout_no_wrap(key.to_string(), theme::semibold(12.0), TEXT);
    let (rect, _) = ui.allocate_exact_size(Vec2::new((galley.size().x + 16.0).max(28.0), 26.0), Sense::hover());
    let p = ui.painter();
    p.rect(rect, CornerRadius::same(5), RAISED, Stroke::new(1.0f32, BORDER_HI), egui::StrokeKind::Inside);
    p.hline(rect.left() + 3.0..=rect.right() - 3.0, rect.bottom() - 1.0, Stroke::new(1.0f32, Color32::from_black_alpha(90)));
    p.galley(rect.center() - galley.size() / 2.0, galley, TEXT);
}

pub fn text_input(ui: &mut Ui, value: &mut String, hint: &str, width: f32) -> Response {
    Frame::new()
        .fill(ui.visuals().extreme_bg_color)
        .stroke(Stroke::new(1.0f32, BORDER_HI))
        .corner_radius(6)
        .inner_margin(Margin::symmetric(10, 0))
        .show(ui, |ui| {
            ui.add_sized(
                [width, 32.0],
                egui::TextEdit::singleline(value).hint_text(RichText::new(hint).color(TEXT_3)).frame(false).vertical_align(Align::Center).font(theme::ui(14.0)),
            )
        })
        .inner
}

pub fn stat_tile(ui: &mut Ui, label: &str, value: &str, width: f32) {
    Frame::new().fill(CARD_HI).corner_radius(8).inner_margin(Margin::symmetric(14, 12)).show(ui, |ui| {
        ui.set_width(width - 28.0);
        ui.vertical(|ui| {
            ui.label(RichText::new(label).size(12.0).color(TEXT_3));
            ui.add_space(-4.0);
            ui.label(RichText::new(value).font(theme::semibold(18.0)).color(TEXT));
        });
    });
}

/// Civil date (UTC) from Unix seconds, as "12 Mar 2026".
pub fn date(secs: u64) -> String {
    let z = (secs / 86400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    const M: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    format!("{d} {} {y}", M[(m - 1) as usize])
}

#[cfg(test)]
mod tests {
    #[test]
    fn dates() {
        assert_eq!(super::date(0), "1 Jan 1970");
        assert_eq!(super::date(1_772_323_200), "1 Mar 2026");
    }
}
