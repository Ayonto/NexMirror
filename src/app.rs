//! Application shell: title bar, navigation, pages and the streaming view.

use crate::{
    capture::{list_displays, DisplayInfo},
    config::{Resolution, SharedConfig},
    discovery::{local_ipv4s, Discovery},
    gfx::{Gfx, VideoTex},
    host::{Host, HostState},
    proto::PORT,
    theme::{self, icon, *},
    viewer::{Session, ViewerState, ViewerStats},
    widgets::{self, Kind},
    window::{WinCmd, WinIo, BTN_W, TITLE_H},
};
use egui::{Align, Align2, Color32, CornerRadius, Frame, Layout, Margin, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::HANDLE;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Connect,
    Share,
    Settings,
}

struct Toast {
    text: String,
    color: Color32,
    glyph: &'static str,
    until: Instant,
}

pub struct App {
    cfg: SharedConfig,
    page: Page,
    host: Option<Host>,
    host_error: Option<String>,
    discovery: Discovery,
    session: Option<Session>,
    manual_addr: String,
    pin_input: String,
    displays: Vec<DisplayInfo>,
    local_ips: Vec<String>,
    refreshed: Instant,
    name_edit: String,
    toasts: Vec<Toast>,
    logo: egui::TextureHandle,
    video_rect: Option<Rect>,
    last_pointer: (Pos2, Instant),
    /// Full-screen controls: shown only while the mouse is at the top edge.
    bar_open: bool,
    /// When full screen was entered (for the brief "how to exit" hint).
    fs_since: Option<Instant>,
    last_host_state: Option<HostState>,
}

impl App {
    pub fn new(ctx: &egui::Context, cfg: SharedConfig) -> Self {
        let logo_px = crate::logo::rgba(64);
        let logo = ctx.load_texture("logo", egui::ColorImage::from_rgba_unmultiplied([64, 64], &logo_px), egui::TextureOptions::LINEAR);
        let name = cfg.lock().unwrap().device_name.clone();
        let mut app = Self {
            cfg,
            page: Page::Connect,
            host: None,
            host_error: None,
            discovery: Discovery::start(),
            session: None,
            manual_addr: String::new(),
            pin_input: String::new(),
            displays: Vec::new(),
            local_ips: Vec::new(),
            refreshed: Instant::now(),
            name_edit: name,
            toasts: Vec::new(),
            logo,
            video_rect: None,
            last_pointer: (Pos2::ZERO, Instant::now()),
            bar_open: false,
            fs_since: None,
            last_host_state: None,
        };
        app.refresh_system_info();
        app
    }

    fn refresh_system_info(&mut self) {
        self.displays = list_displays();
        self.local_ips = local_ipv4s().into_iter().map(|(ip, _)| ip.to_string()).collect();
        self.refreshed = Instant::now();
    }

    fn toast(&mut self, glyph: &'static str, color: Color32, text: impl Into<String>) {
        self.toasts.push(Toast { text: text.into(), color, glyph, until: Instant::now() + Duration::from_secs(4) });
    }

    // ------------------------------------------------------------------ window integration

    pub fn poll(&mut self, gfx: &Gfx) -> bool {
        let (audio, volume) = {
            let c = self.cfg.lock().unwrap();
            (c.viewer.audio, c.viewer.volume)
        };
        match self.session.as_mut() {
            Some(s) => {
                s.set_audio(audio, volume);
                s.poll(gfx)
            }
            None => false,
        }
    }

    pub fn video(&self) -> Option<(&VideoTex, Rect)> {
        Some((self.session.as_ref()?.video()?, self.video_rect?))
    }

    pub fn frame_event(&self) -> Option<HANDLE> {
        self.session.as_ref().map(|s| s.frame_event())
    }

    pub fn clear_color(&self) -> [f32; 4] {
        if self.session.is_some() {
            [0.0, 0.0, 0.0, 1.0]
        } else {
            let c = BG;
            [c.r() as f32 / 255.0, c.g() as f32 / 255.0, c.b() as f32 / 255.0, 1.0]
        }
    }

    pub fn device_lost(&mut self) {
        if let Some(s) = self.session.as_mut() {
            s.device_lost();
        }
    }

    pub fn ui(&mut self, ctx: &egui::Context, io: &mut WinIo) {
        if self.refreshed.elapsed() > Duration::from_secs(5) && self.session.is_none() {
            self.refresh_system_info();
        }
        self.watch_host(io);
        self.video_rect = None;
        if self.session.is_some() {
            self.session_ui(ctx, io);
        } else {
            if io.fullscreen {
                io.cmds.push(WinCmd::Fullscreen(false));
            }
            self.main_ui(ctx, io);
        }
        self.toasts_ui(ctx);
        ctx.request_repaint_after(Duration::from_millis(250));
    }

    fn watch_host(&mut self, io: &mut WinIo) {
        let Some(h) = &self.host else {
            self.last_host_state = None;
            return;
        };
        if h.shared.attention.swap(false, std::sync::atomic::Ordering::Relaxed) {
            io.cmds.push(WinCmd::Attention);
            if self.session.is_none() {
                self.page = Page::Share;
            }
        }
        let st = h.state();
        if self.last_host_state.as_ref() != Some(&st) {
            if let (Some(HostState::Pairing { viewer, .. }), HostState::Streaming { viewer: v2, .. }) = (&self.last_host_state, &st) {
                if viewer == v2 {
                    let v2 = v2.clone();
                    self.toast(icon::CHECK, GREEN, format!("Paired with {v2}"));
                }
            }
            self.last_host_state = Some(st);
        }
    }

    // ------------------------------------------------------------------ title bar

    fn title_bar(&self, ctx: &egui::Context, io: &mut WinIo, subtitle: Option<String>, extra: impl FnOnce(&mut Ui, &mut WinIo, Rect)) {
        egui::TopBottomPanel::top("titlebar").exact_height(TITLE_H).frame(Frame::new().fill(TITLE_BG)).show_separator_line(false).show(ctx, |ui| {
            let r = ui.max_rect();
            let p = ui.painter().clone();
            let logo_rect = Rect::from_center_size(egui::pos2(r.left() + 24.0, r.center().y), Vec2::splat(18.0));
            p.image(self.logo.id(), logo_rect, Rect::from_min_max(Pos2::ZERO, egui::pos2(1.0, 1.0)), Color32::WHITE);
            let t = p.text(egui::pos2(r.left() + 42.0, r.center().y), Align2::LEFT_CENTER, "NexMirror", theme::semibold(13.0), TEXT);
            if let Some(s) = subtitle {
                p.text(egui::pos2(t.right() + 10.0, r.center().y), Align2::LEFT_CENTER, s, theme::ui(13.0), TEXT_2);
            }

            let close = Rect::from_min_max(egui::pos2(r.right() - BTN_W, r.top()), r.right_bottom());
            let max = close.translate(Vec2::new(-BTN_W, 0.0));
            let min = max.translate(Vec2::new(-BTN_W, 0.0));

            let resp = ui.interact(min, ui.id().with("min"), Sense::click());
            if resp.hovered() {
                p.rect_filled(min, 0, Color32::from_white_alpha(14));
            }
            p.text(min.center(), Align2::CENTER_CENTER, icon::MINIMIZE, theme::icons(10.0), TEXT);
            if resp.clicked() {
                io.cmds.push(WinCmd::Minimize);
            }

            if io.max_down {
                p.rect_filled(max, 0, Color32::from_white_alpha(8));
            } else if io.max_hover {
                p.rect_filled(max, 0, Color32::from_white_alpha(14));
            }
            p.text(max.center(), Align2::CENTER_CENTER, if io.maximized { icon::RESTORE } else { icon::MAXIMIZE }, theme::icons(10.0), TEXT);

            let resp = ui.interact(close, ui.id().with("close"), Sense::click());
            if resp.hovered() {
                p.rect_filled(close, 0, if resp.is_pointer_button_down_on() { Color32::from_rgb(0x94, 0x1F, 0x14) } else { CLOSE_RED });
            }
            p.text(close.center(), Align2::CENTER_CENTER, icon::CLOSE, theme::icons(10.0), if resp.hovered() { Color32::WHITE } else { TEXT });
            if resp.clicked() {
                io.cmds.push(WinCmd::Close);
            }

            let extra_area = Rect::from_min_max(egui::pos2(r.left() + 220.0, r.top()), egui::pos2(min.left() - 8.0, r.bottom()));
            extra(ui, io, extra_area);
        });
    }

    // ------------------------------------------------------------------ main layout

    fn main_ui(&mut self, ctx: &egui::Context, io: &mut WinIo) {
        self.title_bar(ctx, io, None, |_, _, _| {});
        egui::SidePanel::left("nav")
            .exact_width(232.0)
            .resizable(false)
            .show_separator_line(false)
            .frame(Frame::new().fill(SIDEBAR).inner_margin(Margin { left: 10, right: 10, top: 6, bottom: 14 }))
            .show(ctx, |ui| self.sidebar(ui));
        egui::CentralPanel::default().frame(Frame::new().fill(BG)).show(ctx, |ui| {
            let r = ui.max_rect();
            // Rounded inner corner where the content meets the window chrome.
            ui.painter().rect_stroke(r.expand(1.0), CornerRadius { nw: 10, ..Default::default() }, Stroke::new(1.0f32, BORDER), egui::StrokeKind::Inside);
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                let w = ui.available_width();
                let content = (w - 72.0).clamp(300.0, 880.0);
                let side = ((w - content) / 2.0).max(36.0);
                ui.add_space(30.0);
                ui.horizontal(|ui| {
                    ui.add_space(side);
                    ui.vertical(|ui| {
                        ui.set_width(content);
                        match self.page {
                            Page::Connect => self.connect_page(ui),
                            Page::Share => self.share_page(ui, io),
                            Page::Settings => self.settings_page(ui, io),
                        }
                        ui.add_space(36.0);
                    });
                });
            });
        });
    }

    fn sidebar(&mut self, ui: &mut Ui) {
        ui.add_space(8.0);
        let host_state = self.host.as_ref().map(|h| h.state());
        let host_dot = host_state.as_ref().map(|s| match s {
            HostState::Ready => ACCENT,
            HostState::Pairing { .. } => AMBER,
            HostState::Streaming { .. } => GREEN,
        });
        for (page, glyph, label, dot) in [
            (Page::Connect, icon::MONITOR, "Connect", None),
            (Page::Share, icon::SHARE, "Share this PC", host_dot),
            (Page::Settings, icon::SETTINGS, "Settings", None),
        ] {
            if nav_item(ui, glyph, label, self.page == page, dot).clicked() {
                self.page = page;
            }
            ui.add_space(2.0);
        }
        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            ui.label(RichText::new(format!("Version {VERSION}")).size(11.5).color(TEXT_3));
            ui.add_space(2.0);
            Frame::new().fill(Color32::from_rgb(0x16, 0x19, 0x1F)).stroke(Stroke::new(1.0f32, BORDER)).corner_radius(8).inner_margin(Margin::same(12)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    widgets::icon_tile(ui, icon::DEVICES, 32.0, ACCENT);
                    ui.vertical(|ui| {
                        let name = self.cfg.lock().unwrap().device_name.clone();
                        ui.add(egui::Label::new(RichText::new(name).font(theme::semibold(13.0)).color(TEXT)).truncate());
                        ui.add_space(-6.0);
                        let (txt, col) = match &host_state {
                            None => ("Sharing off", TEXT_3),
                            Some(HostState::Ready) => ("Sharing on", ACCENT_HI),
                            Some(HostState::Pairing { .. }) => ("Pairing…", AMBER),
                            Some(HostState::Streaming { .. }) => ("Streaming", GREEN),
                        };
                        ui.label(RichText::new(txt).size(12.0).color(col));
                    });
                });
            });
        });
    }

    // ------------------------------------------------------------------ connect page

    fn connect(&mut self, addr: String) {
        let (audio, volume) = {
            let c = self.cfg.lock().unwrap();
            (c.viewer.audio, c.viewer.volume)
        };
        self.pin_input.clear();
        self.session = None;
        self.session = Some(Session::connect(&addr, self.cfg.clone(), audio, volume));
    }

    fn connect_page(&mut self, ui: &mut Ui) {
        widgets::page_header(ui, "Connect to a PC", "Mirror another PC's screen on this device over your local network.");
        let hosts = self.discovery.hosts();
        let (my_id, paired) = {
            let c = self.cfg.lock().unwrap();
            (c.device_id.clone(), c.paired_hosts.clone())
        };
        let mut connect_to = None;
        widgets::card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Available on your network").font(theme::semibold(16.0)).color(TEXT));
                ui.add_space(4.0);
                ui.add(egui::Spinner::new().size(12.0).color(TEXT_3));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if widgets::icon_button(ui, icon::REFRESH, "Search again").clicked() {
                        self.discovery.refresh();
                    }
                });
            });
            ui.add_space(6.0);
            if hosts.is_empty() {
                ui.vertical_centered(|ui| {
                    ui.add_space(16.0);
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(64.0), Sense::hover());
                    ui.painter().circle_filled(r.center(), 32.0, CARD_HI);
                    ui.painter().text(r.center(), Align2::CENTER_CENTER, icon::WIFI, theme::icons(26.0), TEXT_3);
                    ui.add_space(10.0);
                    ui.label(RichText::new("Looking for PCs on your network").font(theme::semibold(15.0)).color(TEXT));
                    ui.add_space(-4.0);
                    ui.label(RichText::new("On the PC you want to view, open NexMirror and turn on Share this PC.").size(13.0).color(TEXT_3));
                    ui.add_space(-6.0);
                    ui.label(RichText::new("Both devices need to be connected to the same router.").size(13.0).color(TEXT_3));
                    ui.add_space(16.0);
                });
            } else {
                for h in &hosts {
                    let is_paired = paired.iter().any(|p| p.id == h.id);
                    if host_row(ui, &h.name, &h.addr.to_string(), is_paired, h.busy, h.id == my_id) {
                        connect_to = Some(h.addr.to_string());
                    }
                }
            }
        });
        ui.add_space(14.0);
        widgets::card(ui, |ui| {
            widgets::card_title(ui, "Connect by address", Some("If a PC doesn't appear above, enter the address shown on its Share this PC page."));
            ui.horizontal(|ui| {
                let resp = widgets::text_input(ui, &mut self.manual_addr, "e.g. 192.168.1.20", 300.0);
                let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let ok = !self.manual_addr.trim().is_empty();
                if (widgets::button(ui, Kind::Primary, Some(icon::LINK), "Connect", ok).clicked() || enter) && ok {
                    connect_to = Some(self.manual_addr.trim().to_string());
                }
            });
            let recent = self.cfg.lock().unwrap().recent_addrs.clone();
            if !recent.is_empty() {
                ui.add_space(2.0);
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("Recent").size(12.5).color(TEXT_3));
                    for a in recent {
                        if chip(ui, &a).clicked() {
                            self.manual_addr = a.clone();
                        }
                    }
                });
            }
        });
        if !paired.is_empty() {
            ui.add_space(14.0);
            widgets::card(ui, |ui| {
                widgets::card_title(ui, "Paired PCs", Some("These PCs remember this device, so no code is needed."));
                let mut forget = None;
                for (i, p) in paired.iter().enumerate() {
                    if i > 0 {
                        widgets::divider(ui);
                    }
                    ui.horizontal(|ui| {
                        widgets::icon_tile(ui, icon::LOCK, 34.0, GREEN);
                        ui.vertical(|ui| {
                            ui.label(RichText::new(&p.name).font(theme::semibold(14.0)).color(TEXT));
                            ui.add_space(-6.0);
                            let sub = if p.last_addr.is_empty() { "Address unknown".to_string() } else { format!("Last seen at {}", p.last_addr) };
                            ui.label(RichText::new(sub).size(12.5).color(TEXT_3));
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if widgets::button(ui, Kind::Ghost, Some(icon::DELETE), "Forget", true).clicked() {
                                forget = Some(p.id.clone());
                            }
                            if !p.last_addr.is_empty() && widgets::button(ui, Kind::Secondary, None, "Connect", true).clicked() {
                                connect_to = Some(p.last_addr.clone());
                            }
                        });
                    });
                }
                if let Some(id) = forget {
                    let mut c = self.cfg.lock().unwrap();
                    c.paired_hosts.retain(|h| h.id != id);
                    c.save();
                }
            });
        }
        if let Some(a) = connect_to {
            self.connect(a);
        }
    }

    // ------------------------------------------------------------------ share page

    fn set_sharing(&mut self, on: bool) {
        if on && self.host.is_none() {
            match Host::start(self.cfg.clone()) {
                Ok(h) => {
                    self.host = Some(h);
                    self.host_error = None;
                }
                Err(e) => self.host_error = Some(e),
            }
        } else if !on {
            self.host = None;
        }
    }

    fn share_page(&mut self, ui: &mut Ui, io: &mut WinIo) {
        widgets::page_header(ui, "Share this PC", "Let a device on your local network mirror this screen.");
        let state = self.host.as_ref().map(|h| h.state());
        let name = self.cfg.lock().unwrap().device_name.clone();
        widgets::card(ui, |ui| {
            ui.horizontal(|ui| {
                let (color, glyph, title, desc) = match &state {
                    None => (TEXT_3, icon::SHARE, "Sharing is off".to_string(), "Turn on sharing so your other devices can find and view this PC.".to_string()),
                    Some(HostState::Ready) => (ACCENT, icon::SHARE, "Ready for connections".into(), format!("Devices on your network can now find \"{name}\".")),
                    Some(HostState::Pairing { viewer, .. }) => (AMBER, icon::LOCK, format!("Pairing with {viewer}"), "Enter the code below on the other device.".into()),
                    Some(HostState::Streaming { viewer, addr }) => (GREEN, icon::PLAY, format!("Streaming to {viewer}"), format!("Connected from {addr}")),
                };
                let (r, _) = ui.allocate_exact_size(Vec2::splat(52.0), Sense::hover());
                ui.painter().circle_filled(r.center(), 26.0, soft(color, 34));
                ui.painter().text(r.center(), Align2::CENTER_CENTER, glyph, theme::icons(22.0), color);
                ui.add_space(6.0);
                ui.vertical(|ui| {
                    ui.add_space(4.0);
                    ui.label(RichText::new(title).font(theme::semibold(18.0)).color(TEXT));
                    ui.add_space(-6.0);
                    ui.label(RichText::new(desc).size(13.0).color(TEXT_2));
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let mut on = self.host.is_some();
                    if widgets::toggle(ui, &mut on).changed() {
                        self.set_sharing(on);
                    }
                });
            });
            if let Some(e) = &self.host_error {
                ui.add_space(10.0);
                widgets::banner(ui, RED, icon::ERROR, e);
            }
            if let Some(p) = self.host.as_ref().and_then(|h| h.problem()) {
                ui.add_space(10.0);
                widgets::banner(ui, AMBER, icon::WARNING, &p);
            }
            if self.host.is_some() {
                ui.add_space(12.0);
                widgets::divider(ui);
                let addr = if self.local_ips.is_empty() { "No network connection".to_string() } else { self.local_ips.join(",  ") };
                let first = self.local_ips.first().cloned();
                let mut copied = false;
                widgets::setting_row(ui, "Address", "Enter this on the other device if this PC isn't listed there.", |ui| {
                    if let Some(first) = first {
                        if widgets::icon_button(ui, icon::COPY, "Copy address").clicked() {
                            io.cmds.push(WinCmd::CopyText(first));
                            copied = true;
                        }
                    }
                    ui.label(RichText::new(addr).font(theme::semibold(14.0)).color(TEXT));
                });
                if copied {
                    self.toast(icon::COPY, ACCENT, "Address copied");
                }
                widgets::divider(ui);
                widgets::setting_row(ui, "Network", "Local network only. Video, audio and control are encrypted end to end.", |ui| {
                    ui.label(RichText::new(format!("TCP + UDP {PORT}")).size(13.0).color(TEXT_2));
                });
            }
        });

        if let Some(HostState::Pairing { viewer, pin }) = &state {
            ui.add_space(14.0);
            let mut decline = false;
            Frame::new().fill(CARD).stroke(Stroke::new(1.0f32, soft(AMBER, 140))).corner_radius(10).inner_margin(Margin::symmetric(22, 20)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new("Pairing request").font(theme::semibold(16.0)).color(TEXT));
                ui.add_space(-4.0);
                ui.label(RichText::new(format!("{viewer} wants to view this screen. To allow it, enter this code on {viewer}.")).size(13.0).color(TEXT_2));
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    for (i, ch) in pin.chars().enumerate() {
                        if i == 3 {
                            ui.add_space(10.0);
                        }
                        let (r, _) = ui.allocate_exact_size(Vec2::new(50.0, 62.0), Sense::hover());
                        ui.painter().rect(r, CornerRadius::same(8), CARD_HI, Stroke::new(1.0f32, BORDER_HI), egui::StrokeKind::Inside);
                        ui.painter().text(r.center(), Align2::CENTER_CENTER, ch, theme::semibold(30.0), TEXT);
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        decline = widgets::button(ui, Kind::Secondary, None, "Decline", true).clicked();
                    });
                });
                ui.add_space(8.0);
                ui.label(RichText::new("Only share this code with a device you trust. It works once.").size(12.5).color(TEXT_3));
            });
            if decline {
                // Restarting the service drops the pending handshake.
                self.set_sharing(false);
                self.set_sharing(true);
            }
        }

        if let Some(HostState::Streaming { .. }) = &state {
            let st = self.host.as_ref().unwrap().stats();
            ui.add_space(14.0);
            widgets::card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Live session").font(theme::semibold(16.0)).color(TEXT));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if widgets::button(ui, Kind::Danger, Some(icon::DISCONNECT), "Disconnect viewer", true).clicked() {
                            if let Some(h) = &self.host {
                                h.disconnect_viewer();
                            }
                        }
                    });
                });
                ui.add_space(6.0);
                let w = (ui.available_width() - 3.0 * 10.0) / 4.0;
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    let res = if st.width > 0 { format!("{}×{}", st.width, st.height) } else { "—".into() };
                    widgets::stat_tile(ui, "Resolution", &res, w);
                    widgets::stat_tile(ui, "Frame rate", &format!("{:.0} fps", st.fps), w);
                    widgets::stat_tile(ui, "Bitrate", &format!("{:.1} Mbps", st.mbps), w);
                    widgets::stat_tile(ui, "Encode time", &format!("{:.1} ms", st.encode_ms), w);
                });
                if !st.encoder.is_empty() {
                    ui.add_space(2.0);
                    ui.label(RichText::new(format!("{}   ·   {}", st.encoder, st.display)).size(12.5).color(TEXT_3));
                }
            });
        }

        ui.add_space(14.0);
        widgets::card(ui, |ui| {
            widgets::card_title(ui, "Stream settings", Some("Changes apply immediately, including during a session."));
            let mut changed = false;
            let mut hs = self.cfg.lock().unwrap().host.clone();
            widgets::setting_row(ui, "Display", "Which screen to share.", |ui| {
                let current = self
                    .displays
                    .iter()
                    .find(|d| d.name == hs.display)
                    .or_else(|| self.displays.iter().find(|d| d.primary))
                    .map(|d| d.label.clone())
                    .unwrap_or_else(|| "Primary display".into());
                egui::ComboBox::from_id_salt("display").selected_text(current).width(270.0).show_ui(ui, |ui| {
                    for d in &self.displays {
                        changed |= ui.selectable_value(&mut hs.display, d.name.clone(), &d.label).changed();
                    }
                });
            });
            widgets::divider(ui);
            widgets::setting_row(ui, "Resolution", "Lower resolutions use less bandwidth on busy Wi-Fi.", |ui| {
                egui::ComboBox::from_id_salt("res").selected_text(hs.resolution.label()).width(270.0).show_ui(ui, |ui| {
                    for r in Resolution::ALL {
                        changed |= ui.selectable_value(&mut hs.resolution, r, r.label()).changed();
                    }
                });
            });
            widgets::divider(ui);
            widgets::setting_row(ui, "Frame rate", "Frames are only sent when the screen changes.", |ui| {
                egui::ComboBox::from_id_salt("fps").selected_text(format!("{} fps", hs.fps)).width(270.0).show_ui(ui, |ui| {
                    for f in [30, 60, 90, 120, 144] {
                        changed |= ui.selectable_value(&mut hs.fps, f, format!("{f} fps")).changed();
                    }
                });
            });
            widgets::divider(ui);
            widgets::setting_row(ui, "Bitrate", "Higher is sharper. Automatic suits most 5 GHz Wi-Fi.", |ui| {
                let label = |b: u32| if b == 0 { "Automatic".to_string() } else { format!("{b} Mbps") };
                egui::ComboBox::from_id_salt("bitrate").selected_text(label(hs.bitrate_mbps)).width(270.0).show_ui(ui, |ui| {
                    for b in [0, 10, 20, 30, 50, 80] {
                        changed |= ui.selectable_value(&mut hs.bitrate_mbps, b, label(b)).changed();
                    }
                });
            });
            widgets::divider(ui);
            widgets::setting_row(ui, "Share audio", "Play this PC's sound on the viewing device too.", |ui| {
                changed |= widgets::toggle(ui, &mut hs.audio).changed();
            });
            if changed {
                let mut c = self.cfg.lock().unwrap();
                c.host = hs;
                c.save();
                drop(c);
                if let Some(h) = &self.host {
                    h.settings_changed();
                }
            }
        });

        ui.add_space(14.0);
        widgets::card(ui, |ui| {
            widgets::card_title(ui, "Trusted devices", Some("Devices that paired with a code can reconnect without one."));
            let viewers = self.cfg.lock().unwrap().trusted_viewers.clone();
            if viewers.is_empty() {
                ui.label(RichText::new("No trusted devices yet.").size(13.0).color(TEXT_3));
            }
            let mut remove = None;
            for (i, v) in viewers.iter().enumerate() {
                if i > 0 {
                    widgets::divider(ui);
                }
                ui.horizontal(|ui| {
                    widgets::icon_tile(ui, icon::DEVICES, 34.0, ACCENT);
                    ui.vertical(|ui| {
                        ui.label(RichText::new(&v.name).font(theme::semibold(14.0)).color(TEXT));
                        ui.add_space(-6.0);
                        ui.label(RichText::new(format!("Paired {}", widgets::date(v.paired_at))).size(12.5).color(TEXT_3));
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if widgets::button(ui, Kind::Ghost, Some(icon::DELETE), "Remove", true).clicked() {
                            remove = Some(v.id.clone());
                        }
                    });
                });
            }
            if let Some(id) = remove {
                let mut c = self.cfg.lock().unwrap();
                c.trusted_viewers.retain(|v| v.id != id);
                c.save();
            }
        });
    }

    // ------------------------------------------------------------------ settings page

    fn settings_page(&mut self, ui: &mut Ui, io: &mut WinIo) {
        widgets::page_header(ui, "Settings", "Preferences for this device.");
        let mut saved_name = false;
        widgets::card(ui, |ui| {
            widgets::card_title(ui, "This device", None);
            widgets::setting_row(ui, "Device name", "How this PC appears to other devices on your network.", |ui| {
                let resp = widgets::text_input(ui, &mut self.name_edit, "Device name", 240.0);
                if resp.lost_focus() {
                    let n: String = self.name_edit.trim().chars().take(40).collect();
                    let mut c = self.cfg.lock().unwrap();
                    if !n.is_empty() && c.device_name != n {
                        c.device_name = n;
                        c.save();
                        saved_name = true;
                    }
                    self.name_edit = c.device_name.clone();
                }
            });
        });
        if saved_name {
            self.toast(icon::CHECK, GREEN, "Device name saved");
        }
        ui.add_space(14.0);
        widgets::card(ui, |ui| {
            widgets::card_title(ui, "Viewing", Some("Applies when this device views another PC."));
            let mut vs = self.cfg.lock().unwrap().viewer.clone();
            let mut changed = false;
            widgets::setting_row(ui, "Play audio", "Hear the shared PC's sound on this device.", |ui| {
                changed |= widgets::toggle(ui, &mut vs.audio).changed();
            });
            widgets::divider(ui);
            widgets::setting_row(ui, "Volume", "", |ui| {
                let mut pct = (vs.volume * 100.0).round();
                ui.spacing_mut().slider_width = 200.0;
                if ui.add(egui::Slider::new(&mut pct, 0.0..=100.0).suffix("%").integer()).changed() {
                    vs.volume = pct / 100.0;
                    changed = true;
                }
            });
            widgets::divider(ui);
            widgets::setting_row(ui, "Performance overlay", "Show latency, frame rate and bitrate while viewing.", |ui| {
                changed |= widgets::toggle(ui, &mut vs.show_stats).changed();
            });
            if changed {
                let mut c = self.cfg.lock().unwrap();
                c.viewer = vs;
                c.save();
            }
        });
        ui.add_space(14.0);
        widgets::card(ui, |ui| {
            widgets::card_title(ui, "Keyboard shortcuts", Some("While viewing another PC."));
            for (key, what) in [("F11", "Toggle full screen"), ("Double-click", "Toggle full screen"), ("Esc", "Leave full screen")] {
                ui.horizontal(|ui| {
                    ui.set_min_height(34.0);
                    ui.label(RichText::new(what).size(14.0).color(TEXT));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| widgets::keycap(ui, key));
                });
            }
        });
        ui.add_space(14.0);
        widgets::card(ui, |ui| {
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(Vec2::splat(44.0), Sense::hover());
                ui.painter().image(self.logo.id(), r, Rect::from_min_max(Pos2::ZERO, egui::pos2(1.0, 1.0)), Color32::WHITE);
                ui.add_space(4.0);
                ui.vertical(|ui| {
                    ui.label(RichText::new(format!("NexMirror {VERSION}")).font(theme::semibold(16.0)).color(TEXT));
                    ui.add_space(-6.0);
                    ui.label(RichText::new("Low-latency screen mirroring for your local network.").size(13.0).color(TEXT_3));
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if widgets::button(ui, Kind::Secondary, Some(icon::FOLDER), "Open logs", true).clicked() {
                        io.cmds.push(WinCmd::Open(crate::config::Config::dir().join("logs").to_string_lossy().into_owned()));
                    }
                });
            });
        });
    }

    // ------------------------------------------------------------------ streaming view

    fn session_ui(&mut self, ctx: &egui::Context, io: &mut WinIo) {
        let s = self.session.as_ref().unwrap();
        let state = s.state();
        let name = s.host_name();
        let stats = s.stats();
        let streaming = state == ViewerState::Streaming;
        let fs = io.fullscreen;
        let show_stats = self.cfg.lock().unwrap().viewer.show_stats;
        let now = Instant::now();

        let (f11, esc) = ctx.input(|i| (i.key_pressed(egui::Key::F11), i.key_pressed(egui::Key::Escape)));
        if f11 {
            io.cmds.push(WinCmd::Fullscreen(!fs));
        } else if esc && fs {
            io.cmds.push(WinCmd::Fullscreen(false));
        }
        // Prefer this frame's last movement: a move followed by "pointer left" still counts.
        let moved = ctx.input(|i| i.events.iter().rev().find_map(|e| if let egui::Event::PointerMoved(p) = e { Some(*p) } else { None }));
        let pointer = moved.or_else(|| ctx.input(|i| i.pointer.latest_pos())).unwrap_or(self.last_pointer.0);
        if (pointer - self.last_pointer.0).length() > 1.0 {
            self.last_pointer = (pointer, now);
        }
        let idle = now.duration_since(self.last_pointer.1) > Duration::from_millis(2500);

        let mut disconnect = false;
        let mut toggle_stats = false;
        if !fs {
            let lat = if streaming && stats.latency_ms > 0.0 { Some(stats.latency_ms) } else { None };
            self.title_bar(ctx, io, Some(format!("·   {name}")), |ui, io, area| {
                let mut x = area.right();
                let mut place = |ui: &mut Ui, w: f32, f: &mut dyn FnMut(&mut Ui) -> bool| -> bool {
                    let r = Rect::from_min_size(egui::pos2(x - w, area.center().y - 17.0), Vec2::new(w, 34.0));
                    x -= w + 6.0;
                    io.caption_blockers.push(r);
                    ui.scope_builder(egui::UiBuilder::new().max_rect(r), |ui| f(ui)).inner
                };
                if place(ui, 128.0, &mut |ui| widgets::button(ui, Kind::Danger, Some(icon::DISCONNECT), "Disconnect", true).clicked()) {
                    disconnect = true;
                }
                if place(ui, 34.0, &mut |ui| widgets::icon_button(ui, icon::FULLSCREEN, "Full screen (F11)").clicked()) {
                    io.cmds.push(WinCmd::Fullscreen(true));
                }
                if place(ui, 34.0, &mut |ui| widgets::icon_button(ui, icon::CHART, "Performance overlay").clicked()) {
                    toggle_stats = true;
                }
                if let Some(l) = lat {
                    let col = if l < 25.0 { GREEN } else if l < 50.0 { AMBER } else { RED };
                    let g = ui.painter().layout_no_wrap(format!("{l:.0} ms"), theme::semibold(12.0), col);
                    let r = Rect::from_min_size(egui::pos2(x - g.size().x - 34.0, area.center().y - 12.0), Vec2::new(g.size().x + 30.0, 24.0));
                    ui.painter().rect_filled(r, CornerRadius::same(255), soft(col, 30));
                    ui.painter().circle_filled(egui::pos2(r.left() + 12.0, r.center().y), 3.5, col);
                    ui.painter().galley(egui::pos2(r.left() + 21.0, r.center().y - g.size().y / 2.0), g, col);
                }
            });
        } else {
            io.show_title_bar = false;
            io.hide_cursor = idle && streaming;
        }

        // No fill: the video is drawn underneath the UI layer (the clear colour is black).
        egui::CentralPanel::default().frame(Frame::new()).show(ctx, |ui| {
            let area = ui.max_rect();
            let resp = ui.interact(area, ui.id().with("video"), Sense::click());
            if resp.double_clicked() && streaming {
                io.cmds.push(WinCmd::Fullscreen(!fs));
            }
            if let Some(v) = self.session.as_ref().and_then(|s| s.video()) {
                let ppp = ctx.pixels_per_point();
                let (vw, vh) = (v.disp_w as f32, v.disp_h as f32);
                let scale = (area.width() / vw).min(area.height() / vh);
                let r = Rect::from_center_size(area.center(), Vec2::new(vw * scale, vh * scale));
                let snap = |v: f32| (v * ppp).round() / ppp;
                self.video_rect = Some(Rect::from_min_max(egui::pos2(snap(r.left()), snap(r.top())), egui::pos2(snap(r.right()), snap(r.bottom()))));
            }

            match &state {
                ViewerState::Connecting => center_card(ui, area, |ui| {
                    ui.add(egui::Spinner::new().size(28.0).color(ACCENT));
                    ui.add_space(8.0);
                    ui.label(RichText::new(format!("Connecting to {name}…")).font(theme::semibold(16.0)).color(TEXT));
                    ui.add_space(8.0);
                    if widgets::button(ui, Kind::Secondary, None, "Cancel", true).clicked() {
                        disconnect = true;
                    }
                }),
                ViewerState::NeedPin => {
                    let mut submit = false;
                    center_card(ui, area, |ui| {
                        widgets::icon_tile(ui, icon::LOCK, 48.0, ACCENT);
                        ui.add_space(6.0);
                        ui.label(RichText::new(format!("Pair with {name}")).font(theme::semibold(18.0)).color(TEXT));
                        ui.add_space(-4.0);
                        ui.label(RichText::new("Enter the 6-digit code shown on that PC.").size(13.0).color(TEXT_2));
                        ui.add_space(10.0);
                        let resp = Frame::new()
                            .fill(Color32::from_rgb(0x12, 0x15, 0x1A))
                            .stroke(Stroke::new(1.0f32, ACCENT))
                            .corner_radius(8)
                            .inner_margin(Margin::symmetric(12, 6))
                            .show(ui, |ui| {
                                ui.add_sized(
                                    [210.0, 46.0],
                                    egui::TextEdit::singleline(&mut self.pin_input)
                                        .font(theme::semibold(28.0))
                                        .char_limit(6)
                                        .hint_text(RichText::new("000000").color(TEXT_3))
                                        .horizontal_align(Align::Center)
                                        .vertical_align(Align::Center)
                                        .frame(false),
                                )
                            })
                            .inner;
                        self.pin_input.retain(|c| c.is_ascii_digit());
                        if !resp.has_focus() {
                            resp.request_focus();
                        }
                        if self.pin_input.len() == 6 && (resp.changed() || ui.input(|i| i.key_pressed(egui::Key::Enter))) {
                            submit = true;
                        }
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            ui.add_space(70.0);
                            if widgets::button(ui, Kind::Secondary, None, "Cancel", true).clicked() {
                                disconnect = true;
                            }
                            if widgets::button(ui, Kind::Primary, None, "Pair", self.pin_input.len() == 6).clicked() {
                                submit = true;
                            }
                        });
                    });
                    if submit {
                        if let Some(s) = &self.session {
                            s.submit_pin(Some(self.pin_input.clone()));
                        }
                    }
                }
                ViewerState::Waiting => center_card(ui, area, |ui| {
                    ui.add(egui::Spinner::new().size(28.0).color(ACCENT));
                    ui.add_space(8.0);
                    ui.label(RichText::new("Starting stream…").font(theme::semibold(16.0)).color(TEXT));
                    ui.label(RichText::new(format!("Connected to {name}")).size(13.0).color(TEXT_3));
                }),
                ViewerState::Failed(msg) => {
                    let mut retry = false;
                    center_card(ui, area, |ui| {
                        widgets::icon_tile(ui, icon::ERROR, 48.0, RED);
                        ui.add_space(6.0);
                        ui.label(RichText::new("Disconnected").font(theme::semibold(18.0)).color(TEXT));
                        ui.add(egui::Label::new(RichText::new(msg).size(13.0).color(TEXT_2)).wrap());
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            ui.add_space(60.0);
                            if widgets::button(ui, Kind::Secondary, None, "Back", true).clicked() {
                                disconnect = true;
                            }
                            if widgets::button(ui, Kind::Primary, Some(icon::REFRESH), "Try again", true).clicked() {
                                retry = true;
                            }
                        });
                    });
                    if retry {
                        let addr = self.session.as_ref().unwrap().addr.clone();
                        self.connect(addr);
                    }
                }
                ViewerState::Streaming => {}
            }

            if streaming && show_stats {
                stats_overlay(ui, area, &stats);
            }

            if !(fs && streaming) {
                self.bar_open = false;
                self.fs_since = None;
            } else {
                // Brief hint right after entering full screen; afterwards only the video is shown.
                let shown = self.fs_since.get_or_insert(now).elapsed().as_secs_f32();
                if shown < 3.0 {
                    let a = ((3.0 - shown) / 0.6).min(1.0);
                    let text = "Press Esc to exit full screen  ·  Move the mouse to the top edge for controls";
                    let g = ui.painter().layout_no_wrap(text.into(), theme::ui(13.0), TEXT.gamma_multiply(a));
                    let r = Rect::from_center_size(egui::pos2(area.center().x, area.top() + 40.0), g.size() + Vec2::new(32.0, 18.0));
                    ui.painter().rect_filled(r, CornerRadius::same(255), Color32::from_rgba_unmultiplied(16, 18, 23, (210.0 * a) as u8));
                    ui.painter().galley(r.center() - g.size() / 2.0, g, TEXT);
                    ctx.request_repaint();
                }
                // Controls appear only when the mouse touches the top edge, and hide once it leaves them.
                let bar = Rect::from_center_size(egui::pos2(area.center().x, area.top() + 34.0), Vec2::new(320.0, 48.0));
                if pointer.y <= area.top() + 2.0 {
                    self.bar_open = true;
                } else if !bar.expand(36.0).contains(pointer) {
                    self.bar_open = false;
                }
                let t = ctx.animate_bool_with_time(ui.id().with("fs_bar"), self.bar_open, 0.15);
                if t > 0.0 {
                    let bar = bar.translate(Vec2::new(0.0, -(1.0 - t) * 60.0));
                    let fill = Color32::from_rgba_unmultiplied(22, 25, 31, (235.0 * t) as u8);
                    ui.painter().rect(bar, CornerRadius::same(12), fill, Stroke::new(1.0f32, BORDER_HI.gamma_multiply(t)), egui::StrokeKind::Inside);
                    ui.scope_builder(egui::UiBuilder::new().max_rect(bar.shrink2(Vec2::new(14.0, 7.0))), |ui| {
                        ui.multiply_opacity(t);
                        ui.horizontal_centered(|ui| {
                            ui.label(RichText::new(&name).font(theme::semibold(13.0)).color(TEXT));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if widgets::icon_button(ui, icon::DISCONNECT, "Disconnect").clicked() {
                                    disconnect = true;
                                }
                                if widgets::icon_button(ui, icon::EXIT_FULLSCREEN, "Exit full screen (Esc)").clicked() {
                                    io.cmds.push(WinCmd::Fullscreen(false));
                                }
                                if widgets::icon_button(ui, icon::CHART, "Performance overlay").clicked() {
                                    toggle_stats = true;
                                }
                            });
                        });
                    });
                }
            }
        });

        if toggle_stats {
            let mut c = self.cfg.lock().unwrap();
            c.viewer.show_stats = !c.viewer.show_stats;
            c.save();
        }
        if disconnect {
            self.session = None;
            self.video_rect = None;
            io.cmds.push(WinCmd::Fullscreen(false));
        }
    }

    fn toasts_ui(&mut self, ctx: &egui::Context) {
        self.toasts.retain(|t| t.until > Instant::now());
        if self.toasts.is_empty() {
            return;
        }
        let screen = ctx.screen_rect();
        let pos = egui::pos2(screen.right() - 340.0, screen.bottom() - 24.0 - 52.0 * self.toasts.len() as f32);
        egui::Area::new("toasts".into()).order(egui::Order::Foreground).fixed_pos(pos).show(ctx, |ui| {
            for t in &self.toasts {
                Frame::new().fill(RAISED).stroke(Stroke::new(1.0f32, BORDER_HI)).corner_radius(8).shadow(ui.visuals().popup_shadow).inner_margin(Margin::symmetric(14, 10)).show(ui, |ui| {
                    ui.set_width(290.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(t.glyph).font(theme::icons(14.0)).color(t.color));
                        ui.label(RichText::new(&t.text).size(13.5).color(TEXT));
                    });
                });
                ui.add_space(6.0);
            }
        });
    }
}

fn nav_item(ui: &mut Ui, glyph: &str, label: &str, selected: bool, dot: Option<Color32>) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 38.0), Sense::click());
    let p = ui.painter();
    if selected {
        p.rect_filled(rect, CornerRadius::same(6), Color32::from_rgb(0x1E, 0x22, 0x2A));
        let pill = Rect::from_center_size(egui::pos2(rect.left() + 2.0, rect.center().y), Vec2::new(3.0, 16.0));
        p.rect_filled(pill, CornerRadius::same(2), ACCENT);
    } else if resp.hovered() {
        p.rect_filled(rect, CornerRadius::same(6), Color32::from_rgb(0x18, 0x1B, 0x21));
    }
    p.text(egui::pos2(rect.left() + 22.0, rect.center().y), Align2::CENTER_CENTER, glyph, theme::icons(16.0), if selected { ACCENT_HI } else { TEXT_2 });
    p.text(egui::pos2(rect.left() + 42.0, rect.center().y), Align2::LEFT_CENTER, label, if selected { theme::semibold(14.0) } else { theme::ui(14.0) }, if selected { TEXT } else { TEXT_2 });
    if let Some(c) = dot {
        p.circle_filled(egui::pos2(rect.right() - 16.0, rect.center().y), 4.0, c);
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

fn chip(ui: &mut Ui, text: &str) -> egui::Response {
    let g = ui.painter().layout_no_wrap(text.to_string(), theme::ui(12.5), TEXT_2);
    let (r, resp) = ui.allocate_exact_size(g.size() + Vec2::new(18.0, 8.0), Sense::click());
    ui.painter().rect(r, CornerRadius::same(255), if resp.hovered() { RAISED } else { CARD_HI }, Stroke::new(1.0f32, BORDER), egui::StrokeKind::Inside);
    ui.painter().galley(r.center() - g.size() / 2.0, g, if resp.hovered() { TEXT } else { TEXT_2 });
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

/// One discovered PC. Returns true when the user chooses to connect.
fn host_row(ui: &mut Ui, name: &str, addr: &str, paired: bool, busy: bool, is_self: bool) -> bool {
    let mut go = false;
    let resp = Frame::new().corner_radius(8).inner_margin(Margin::symmetric(10, 8)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            widgets::icon_tile(ui, icon::MONITOR, 40.0, ACCENT);
            ui.add_space(4.0);
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(name).font(theme::semibold(15.0)).color(TEXT));
                    if is_self {
                        widgets::badge(ui, "THIS PC", TEXT_2);
                    }
                });
                ui.add_space(-6.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(addr).size(12.5).color(TEXT_3));
                    let (t, c) = if busy { ("In use", AMBER) } else if paired { ("Paired", GREEN) } else { ("Not paired", TEXT_3) };
                    widgets::dot(ui, c);
                    ui.add_space(-6.0);
                    ui.label(RichText::new(t).size(12.5).color(c));
                });
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                go = widgets::button(ui, Kind::Primary, None, "Connect", true).clicked();
            });
        });
    });
    if ui.rect_contains_pointer(resp.response.rect) {
        ui.painter().rect_stroke(resp.response.rect, CornerRadius::same(8), Stroke::new(1.0f32, BORDER_HI), egui::StrokeKind::Inside);
    }
    go
}

fn center_card(ui: &mut Ui, area: Rect, add: impl FnOnce(&mut Ui)) {
    let pos = egui::pos2(area.center().x - 190.0, area.center().y - 130.0);
    egui::Area::new(ui.id().with("center_card")).order(egui::Order::Middle).fixed_pos(pos).show(ui.ctx(), |ui| {
        Frame::new().fill(CARD).stroke(Stroke::new(1.0f32, BORDER)).corner_radius(12).shadow(ui.visuals().window_shadow).inner_margin(Margin::same(26)).show(ui, |ui| {
            ui.set_width(328.0);
            ui.vertical_centered(add);
        });
    });
}

fn stats_overlay(ui: &mut Ui, area: Rect, s: &ViewerStats) {
    let lines = [
        ("Resolution", format!("{}×{}", s.width, s.height)),
        ("Frame rate", format!("{:.0} fps", s.fps)),
        ("Latency", format!("{:.1} ms", s.latency_ms)),
        ("Network RTT", format!("{:.1} ms", s.rtt_ms)),
        ("Decode", format!("{:.2} ms{}", s.decode_ms, if s.hardware_decode { "  (GPU)" } else { "" })),
        ("Bitrate", format!("{:.1} Mbps", s.mbps)),
        ("Repaired pkts", s.repaired_packets.to_string()),
        ("Lost frames", s.lost_frames.to_string()),
        ("Encoder", s.encoder.clone()),
    ];
    let r = Rect::from_min_size(area.left_top() + Vec2::new(14.0, 14.0), Vec2::new(310.0, 22.0 + lines.len() as f32 * 19.0));
    let p = ui.painter();
    p.rect(r, CornerRadius::same(8), Color32::from_rgba_unmultiplied(12, 14, 18, 215), Stroke::new(1.0f32, soft(BORDER_HI, 160)), egui::StrokeKind::Inside);
    for (i, (k, v)) in lines.iter().enumerate() {
        let y = r.top() + 12.0 + i as f32 * 19.0;
        p.text(egui::pos2(r.left() + 14.0, y), Align2::LEFT_TOP, *k, theme::mono(12.0), TEXT_3);
        p.text(egui::pos2(r.left() + 124.0, y), Align2::LEFT_TOP, v, theme::mono(12.0), TEXT);
    }
}
