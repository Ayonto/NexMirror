//! Visual design system: palette, typography (Segoe UI + Segoe Fluent Icons from Windows),
//! and egui style overrides.

use egui::{epaint::Shadow, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Stroke, TextStyle};
use std::sync::Arc;

pub const TITLE_BG: Color32 = Color32::from_rgb(0x0F, 0x11, 0x15);
pub const SIDEBAR: Color32 = Color32::from_rgb(0x0F, 0x11, 0x15);
pub const BG: Color32 = Color32::from_rgb(0x15, 0x18, 0x1D);
pub const CARD: Color32 = Color32::from_rgb(0x1B, 0x1F, 0x26);
pub const CARD_HI: Color32 = Color32::from_rgb(0x22, 0x27, 0x30);
pub const RAISED: Color32 = Color32::from_rgb(0x2A, 0x30, 0x3B);
pub const RAISED_HI: Color32 = Color32::from_rgb(0x33, 0x3A, 0x47);
pub const BORDER: Color32 = Color32::from_rgb(0x2A, 0x2F, 0x39);
pub const BORDER_HI: Color32 = Color32::from_rgb(0x3A, 0x41, 0x4E);
pub const TEXT: Color32 = Color32::from_rgb(0xEC, 0xEE, 0xF2);
pub const TEXT_2: Color32 = Color32::from_rgb(0xA4, 0xAB, 0xB8);
pub const TEXT_3: Color32 = Color32::from_rgb(0x6F, 0x77, 0x85);
pub const ACCENT: Color32 = Color32::from_rgb(0x4C, 0x8D, 0xFF);
pub const ACCENT_HI: Color32 = Color32::from_rgb(0x6B, 0xA1, 0xFF);
pub const ACCENT_LO: Color32 = Color32::from_rgb(0x3D, 0x79, 0xE8);
pub const GREEN: Color32 = Color32::from_rgb(0x3C, 0xCB, 0x7F);
pub const AMBER: Color32 = Color32::from_rgb(0xF5, 0xB0, 0x41);
pub const RED: Color32 = Color32::from_rgb(0xF0, 0x58, 0x58);
pub const CLOSE_RED: Color32 = Color32::from_rgb(0xC4, 0x2B, 0x1C);

/// DWM window border colour (COLORREF is 0x00BBGGRR).
pub const BORDER_COLORREF: u32 = 0x0039_2F2A;

pub fn soft(c: Color32, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a)
}

pub mod icon {
    pub const MINIMIZE: &str = "\u{E921}";
    pub const MAXIMIZE: &str = "\u{E922}";
    pub const RESTORE: &str = "\u{E923}";
    pub const CLOSE: &str = "\u{E8BB}";
    pub const MONITOR: &str = "\u{E7F4}";
    pub const SHARE: &str = "\u{E72D}";
    pub const SETTINGS: &str = "\u{E713}";
    pub const REFRESH: &str = "\u{E72C}";
    pub const DELETE: &str = "\u{E74D}";
    pub const FULLSCREEN: &str = "\u{E740}";
    pub const EXIT_FULLSCREEN: &str = "\u{E73F}";
    pub const LOCK: &str = "\u{E72E}";
    pub const CHECK: &str = "\u{E73E}";
    pub const WARNING: &str = "\u{E7BA}";
    pub const ERROR: &str = "\u{E783}";
    pub const COPY: &str = "\u{E8C8}";
    pub const FOLDER: &str = "\u{E838}";
    pub const CHART: &str = "\u{E9D2}";
    pub const LINK: &str = "\u{E71B}";
    pub const DEVICES: &str = "\u{E772}";
    pub const DISCONNECT: &str = "\u{E8CD}";
    pub const PLAY: &str = "\u{E768}";
    pub const WIFI: &str = "\u{E701}";
}

pub fn ui(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}

pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("semibold".into()))
}

pub fn icons(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("icons".into()))
}

pub fn mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Monospace)
}

fn system_font(names: &[&str]) -> Option<Vec<u8>> {
    let dir = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into());
    names.iter().find_map(|n| std::fs::read(format!("{dir}\\Fonts\\{n}")).ok())
}

pub fn setup(ctx: &egui::Context) {
    let mut defs = FontDefinitions::default();
    let mut add = |key: &str, files: &[&str]| -> bool {
        match system_font(files) {
            Some(bytes) => {
                defs.font_data.insert(key.into(), Arc::new(FontData::from_owned(bytes)));
                true
            }
            None => false,
        }
    };
    let has_ui = add("segoe", &["segoeui.ttf"]);
    let has_sb = add("segoe-sb", &["seguisb.ttf", "segoeuib.ttf"]);
    let has_icons = add("icons", &["SegoeIcons.ttf", "segmdl2.ttf"]);
    let has_mono = add("mono", &["CascadiaMono.ttf", "consola.ttf"]);
    let prop = defs.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let mut body = Vec::new();
    if has_ui {
        body.push("segoe".to_string());
    }
    body.extend(prop.iter().cloned());
    let mut sb = Vec::new();
    if has_sb {
        sb.push("segoe-sb".to_string());
    }
    sb.extend(body.iter().cloned());
    let mut ic = Vec::new();
    if has_icons {
        ic.push("icons".to_string());
    }
    ic.extend(body.iter().cloned());
    if has_mono {
        defs.families.entry(FontFamily::Monospace).or_default().insert(0, "mono".into());
    }
    defs.families.insert(FontFamily::Proportional, body);
    defs.families.insert(FontFamily::Name("semibold".into()), sb);
    defs.families.insert(FontFamily::Name("icons".into()), ic);
    ctx.set_fonts(defs);

    let mut style = (*ctx.style()).clone();
    style.text_styles = [
        (TextStyle::Heading, semibold(22.0)),
        (TextStyle::Body, ui(14.0)),
        (TextStyle::Button, ui(14.0)),
        (TextStyle::Small, ui(12.0)),
        (TextStyle::Monospace, mono(12.5)),
    ]
    .into();
    let s = &mut style.spacing;
    s.item_spacing = egui::vec2(8.0, 8.0);
    s.button_padding = egui::vec2(14.0, 7.0);
    s.interact_size = egui::vec2(40.0, 32.0);
    s.combo_width = 260.0;
    s.text_edit_width = 280.0;
    s.menu_margin = egui::Margin::same(6);
    s.combo_height = 320.0;
    let mut scroll = egui::style::ScrollStyle::floating();
    scroll.bar_width = 8.0;
    scroll.floating_width = 3.0;
    scroll.bar_inner_margin = 3.0;
    scroll.bar_outer_margin = 3.0;
    scroll.handle_min_length = 32.0;
    s.scroll = scroll;

    let v = &mut style.visuals;
    *v = egui::Visuals::dark();
    v.override_text_color = Some(TEXT);
    v.panel_fill = BG;
    v.window_fill = CARD;
    v.window_stroke = Stroke::new(1.0f32, BORDER);
    v.window_corner_radius = CornerRadius::same(10);
    v.menu_corner_radius = CornerRadius::same(8);
    v.window_shadow = Shadow { offset: [0, 8], blur: 28, spread: 0, color: Color32::from_black_alpha(110) };
    v.popup_shadow = Shadow { offset: [0, 6], blur: 18, spread: 0, color: Color32::from_black_alpha(100) };
    v.extreme_bg_color = Color32::from_rgb(0x12, 0x15, 0x1A);
    v.faint_bg_color = CARD_HI;
    v.code_bg_color = CARD_HI;
    v.hyperlink_color = ACCENT_HI;
    v.selection.bg_fill = soft(ACCENT, 90);
    v.selection.stroke = Stroke::new(1.0f32, ACCENT_HI);
    v.text_cursor.stroke = Stroke::new(2.0f32, ACCENT_HI);
    v.striped = false;
    let r = CornerRadius::same(6);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = CARD;
    w.noninteractive.weak_bg_fill = CARD;
    w.noninteractive.bg_stroke = Stroke::new(1.0f32, BORDER);
    w.noninteractive.fg_stroke = Stroke::new(1.0f32, TEXT_2);
    w.noninteractive.corner_radius = r;
    w.inactive.bg_fill = RAISED;
    w.inactive.weak_bg_fill = CARD_HI;
    w.inactive.bg_stroke = Stroke::new(1.0f32, BORDER_HI);
    w.inactive.fg_stroke = Stroke::new(1.0f32, TEXT);
    w.inactive.corner_radius = r;
    w.hovered.bg_fill = RAISED_HI;
    w.hovered.weak_bg_fill = RAISED;
    w.hovered.bg_stroke = Stroke::new(1.0f32, Color32::from_rgb(0x4A, 0x52, 0x60));
    w.hovered.fg_stroke = Stroke::new(1.5f32, TEXT);
    w.hovered.corner_radius = r;
    w.hovered.expansion = 0.0;
    w.active.bg_fill = RAISED;
    w.active.weak_bg_fill = RAISED;
    w.active.bg_stroke = Stroke::new(1.0f32, ACCENT);
    w.active.fg_stroke = Stroke::new(1.5f32, TEXT);
    w.active.corner_radius = r;
    w.active.expansion = 0.0;
    w.open = w.hovered;
    w.open.bg_stroke = Stroke::new(1.0f32, ACCENT);
    ctx.set_style(style);
}
