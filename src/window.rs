//! Native Win32 window with a custom-drawn title bar.
//!
//! The client area is extended over the whole window (WM_NCCALCSIZE) while keeping the native
//! frame behaviour: DWM shadow, rounded corners, Aero Snap, resize borders and the Windows 11
//! snap-layout flyout on the maximize button (HTMAXBUTTON).

use crate::{app::App, gfx::Gfx, theme};
use std::{
    cell::{Cell, RefCell},
    time::{Duration, Instant},
};
use windows::{
    core::*,
    Win32::{
        Foundation::*,
        Graphics::{Dwm::*, Gdi::*},
        System::{DataExchange::*, LibraryLoader::GetModuleHandleW, Memory::*, Ole::CF_UNICODETEXT},
        UI::{Controls::{MARGINS, WM_MOUSELEAVE}, HiDpi::*, Input::KeyboardAndMouse::*, Shell::ShellExecuteW, WindowsAndMessaging::*},
    },
};

pub const TITLE_H: f32 = 40.0;
pub const BTN_W: f32 = 46.0;

pub enum WinCmd {
    Minimize,
    Close,
    Fullscreen(bool),
    CopyText(String),
    Attention,
    Open(String),
}

/// Window state exposed to the UI each frame.
pub struct WinIo {
    pub maximized: bool,
    pub fullscreen: bool,
    pub max_hover: bool,
    pub max_down: bool,
    pub cmds: Vec<WinCmd>,
    /// Interactive widgets placed inside the title bar (so they are not treated as drag area).
    pub caption_blockers: Vec<egui::Rect>,
    pub show_title_bar: bool,
    pub hide_cursor: bool,
}

/// State the window procedure needs without borrowing the app.
struct Hit {
    fullscreen: Cell<bool>,
    dpi: Cell<u32>,
    title_bar: Cell<bool>,
    max_hover: Cell<bool>,
    max_down: Cell<bool>,
    blockers: RefCell<Vec<RECT>>,
    saved: Cell<Option<(WINDOWPLACEMENT, isize)>>,
    cursor: Cell<HCURSOR>,
    high_surrogate: Cell<u16>,
}

#[derive(Default)]
struct Input {
    events: Vec<egui::Event>,
    size: (u32, u32),
    resized: bool,
    focused: bool,
    pointer_in: bool,
    tracking: bool,
}

struct Shared {
    hit: Hit,
    input: RefCell<Input>,
    state: RefCell<Option<State>>,
}

struct State {
    hwnd: HWND,
    gfx: Gfx,
    ctx: egui::Context,
    app: App,
    start: Instant,
    next_repaint: Option<Instant>,
    pending: egui::TexturesDelta,
}

fn scale(hit: &Hit) -> f32 {
    hit.dpi.get() as f32 / 96.0
}

fn modifiers() -> egui::Modifiers {
    let down = |vk: VIRTUAL_KEY| unsafe { GetKeyState(vk.0 as i32) } < 0;
    let ctrl = down(VK_CONTROL);
    egui::Modifiers { alt: down(VK_MENU), ctrl, shift: down(VK_SHIFT), mac_cmd: false, command: ctrl }
}

fn vk_to_key(vk: u16) -> Option<egui::Key> {
    use egui::Key::*;
    const LETTERS: [egui::Key; 26] = [A, B, C, D, E, F, G, H, I, J, K, L, M, N, O, P, Q, R, S, T, U, V, W, X, Y, Z];
    const DIGITS: [egui::Key; 10] = [Num0, Num1, Num2, Num3, Num4, Num5, Num6, Num7, Num8, Num9];
    const FKEYS: [egui::Key; 12] = [F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12];
    Some(match vk {
        0x41..=0x5A => LETTERS[(vk - 0x41) as usize],
        0x30..=0x39 => DIGITS[(vk - 0x30) as usize],
        0x60..=0x69 => DIGITS[(vk - 0x60) as usize],
        0x70..=0x7B => FKEYS[(vk - 0x70) as usize],
        0x08 => Backspace,
        0x09 => Tab,
        0x0D => Enter,
        0x1B => Escape,
        0x20 => Space,
        0x21 => PageUp,
        0x22 => PageDown,
        0x23 => End,
        0x24 => Home,
        0x25 => ArrowLeft,
        0x26 => ArrowUp,
        0x27 => ArrowRight,
        0x28 => ArrowDown,
        0x2D => Insert,
        0x2E => Delete,
        0xBD | 0x6D => Minus,
        0xBB => Equals,
        0x6B => Plus,
        0xBE | 0x6E => Period,
        0xBC => Comma,
        _ => return None,
    })
}

fn cursor_for(icon: egui::CursorIcon) -> HCURSOR {
    use egui::CursorIcon as C;
    let id = match icon {
        C::None => return HCURSOR::default(),
        C::PointingHand => IDC_HAND,
        C::Text | C::VerticalText => IDC_IBEAM,
        C::ResizeHorizontal | C::ResizeColumn | C::ResizeEast | C::ResizeWest => IDC_SIZEWE,
        C::ResizeVertical | C::ResizeRow | C::ResizeNorth | C::ResizeSouth => IDC_SIZENS,
        C::NotAllowed | C::NoDrop => IDC_NO,
        C::Wait => IDC_WAIT,
        C::Progress => IDC_APPSTARTING,
        C::Grab | C::Grabbing | C::Move | C::AllScroll => IDC_SIZEALL,
        _ => IDC_ARROW,
    };
    unsafe { LoadCursorW(None, id).unwrap_or_default() }
}

fn get_clipboard(hwnd: HWND) -> Option<String> {
    unsafe {
        OpenClipboard(hwnd).ok()?;
        let mut out = None;
        if let Ok(h) = GetClipboardData(CF_UNICODETEXT.0 as u32) {
            let p = GlobalLock(HGLOBAL(h.0)) as *const u16;
            if !p.is_null() {
                let mut n = 0;
                while *p.add(n) != 0 {
                    n += 1;
                }
                out = Some(String::from_utf16_lossy(std::slice::from_raw_parts(p, n)));
                let _ = GlobalUnlock(HGLOBAL(h.0));
            }
        }
        let _ = CloseClipboard();
        out
    }
}

fn set_clipboard(hwnd: HWND, text: &str) {
    let w: Vec<u16> = text.encode_utf16().chain([0]).collect();
    unsafe {
        if OpenClipboard(hwnd).is_err() {
            return;
        }
        let _ = EmptyClipboard();
        if let Ok(h) = GlobalAlloc(GMEM_MOVEABLE, w.len() * 2) {
            let p = GlobalLock(h) as *mut u16;
            if !p.is_null() {
                std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len());
                let _ = GlobalUnlock(h);
                let _ = SetClipboardData(CF_UNICODETEXT.0 as u32, HANDLE(h.0));
            }
        }
        let _ = CloseClipboard();
    }
}

fn make_icon(size: u32) -> HICON {
    let rgba = crate::logo::rgba(size);
    let bgra: Vec<u8> = rgba.chunks(4).flat_map(|p| [p[2], p[1], p[0], p[3]]).collect();
    unsafe {
        let color = CreateBitmap(size as i32, size as i32, 1, 32, Some(bgra.as_ptr() as _));
        let mask_bits = vec![0u8; (size * size / 8) as usize];
        let mask = CreateBitmap(size as i32, size as i32, 1, 1, Some(mask_bits.as_ptr() as _));
        let info = ICONINFO { fIcon: TRUE, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let icon = CreateIconIndirect(&info).unwrap_or_default();
        let _ = DeleteObject(color);
        let _ = DeleteObject(mask);
        icon
    }
}

fn shared_of(hwnd: HWND) -> Option<&'static Shared> {
    let p = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const Shared;
    unsafe { p.as_ref() }
}

fn push_event(sh: &Shared, e: egui::Event) {
    if let Ok(mut i) = sh.input.try_borrow_mut() {
        i.events.push(e);
    }
}

fn lparam_point(lp: LPARAM) -> (i32, i32) {
    ((lp.0 & 0xffff) as i16 as i32, ((lp.0 >> 16) & 0xffff) as i16 as i32)
}

fn hit_test(hwnd: HWND, hit: &Hit, sx: i32, sy: i32) -> u32 {
    if hit.fullscreen.get() {
        return HTCLIENT;
    }
    let mut pt = POINT { x: sx, y: sy };
    let mut rc = RECT::default();
    unsafe {
        let _ = ScreenToClient(hwnd, &mut pt);
        let _ = GetClientRect(hwnd, &mut rc);
    }
    let (x, y, w, h) = (pt.x, pt.y, rc.right, rc.bottom);
    let s = scale(hit);
    if !unsafe { IsZoomed(hwnd) }.as_bool() {
        let b = (6.0 * s) as i32;
        let (l, r, t, btm) = (x < b, x >= w - b, y < b, y >= h - b);
        match (l, r, t, btm) {
            (true, _, true, _) => return HTTOPLEFT,
            (_, true, true, _) => return HTTOPRIGHT,
            (true, _, _, true) => return HTBOTTOMLEFT,
            (_, true, _, true) => return HTBOTTOMRIGHT,
            (true, ..) => return HTLEFT,
            (_, true, ..) => return HTRIGHT,
            (_, _, true, _) => return HTTOP,
            (_, _, _, true) => return HTBOTTOM,
            _ => {}
        }
    }
    if !hit.title_bar.get() || y >= (TITLE_H * s) as i32 {
        return HTCLIENT;
    }
    let bw = (BTN_W * s) as i32;
    if x >= w - bw {
        return HTCLIENT; // close (handled by the UI)
    }
    if x >= w - 2 * bw {
        return HTMAXBUTTON;
    }
    if x >= w - 3 * bw {
        return HTCLIENT; // minimize
    }
    if hit.blockers.borrow().iter().any(|r| x >= r.left && x < r.right && y >= r.top && y < r.bottom) {
        return HTCLIENT;
    }
    HTCAPTION
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_NCCREATE {
            let cs = &*(lp.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
            let _ = EnableNonClientDpiScaling(hwnd);
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        let Some(sh) = shared_of(hwnd) else { return DefWindowProcW(hwnd, msg, wp, lp) };
        let hit = &sh.hit;
        let s = scale(hit);
        let pos = |lp: LPARAM| {
            let (x, y) = lparam_point(lp);
            egui::pos2(x as f32 / s, y as f32 / s)
        };
        let button = |b: egui::PointerButton, pressed: bool, lp: LPARAM| {
            push_event(sh, egui::Event::PointerButton { pos: pos(lp), button: b, pressed, modifiers: modifiers() });
            if pressed {
                SetCapture(hwnd);
            } else {
                let _ = ReleaseCapture();
            }
            LRESULT(0)
        };
        match msg {
            WM_NCCALCSIZE if wp.0 != 0 => {
                if !hit.fullscreen.get() && IsZoomed(hwnd).as_bool() {
                    let p = &mut *(lp.0 as *mut NCCALCSIZE_PARAMS);
                    let dpi = hit.dpi.get();
                    let f = GetSystemMetricsForDpi(SM_CXFRAME, dpi) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi);
                    let r = &mut p.rgrc[0];
                    r.left += f;
                    r.top += f;
                    r.right -= f;
                    r.bottom -= f;
                }
                LRESULT(0)
            }
            WM_NCHITTEST => {
                let (x, y) = lparam_point(lp);
                LRESULT(hit_test(hwnd, hit, x, y) as isize)
            }
            WM_NCMOUSEMOVE => {
                let over = wp.0 as u32 == HTMAXBUTTON;
                if over != hit.max_hover.get() {
                    hit.max_hover.set(over);
                    push_event(sh, egui::Event::PointerGone);
                }
                let mut t = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE | TME_NONCLIENT, hwndTrack: hwnd, dwHoverTime: 0 };
                let _ = TrackMouseEvent(&mut t);
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            WM_NCMOUSELEAVE => {
                hit.max_hover.set(false);
                hit.max_down.set(false);
                push_event(sh, egui::Event::PointerGone);
                LRESULT(0)
            }
            WM_NCLBUTTONDOWN if wp.0 as u32 == HTMAXBUTTON => {
                hit.max_down.set(true);
                push_event(sh, egui::Event::PointerGone);
                LRESULT(0)
            }
            WM_NCLBUTTONUP if wp.0 as u32 == HTMAXBUTTON => {
                if hit.max_down.replace(false) {
                    let _ = ShowWindow(hwnd, if IsZoomed(hwnd).as_bool() { SW_RESTORE } else { SW_MAXIMIZE });
                }
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                hit.max_hover.set(false);
                if let Ok(mut i) = sh.input.try_borrow_mut() {
                    i.pointer_in = true;
                    if !i.tracking {
                        let mut t = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
                        let _ = TrackMouseEvent(&mut t);
                        i.tracking = true;
                    }
                    i.events.push(egui::Event::PointerMoved(pos(lp)));
                }
                LRESULT(0)
            }
            WM_MOUSELEAVE => {
                if let Ok(mut i) = sh.input.try_borrow_mut() {
                    i.pointer_in = false;
                    i.tracking = false;
                    i.events.push(egui::Event::PointerGone);
                }
                LRESULT(0)
            }
            WM_LBUTTONDOWN => button(egui::PointerButton::Primary, true, lp),
            WM_LBUTTONUP => button(egui::PointerButton::Primary, false, lp),
            WM_RBUTTONDOWN => button(egui::PointerButton::Secondary, true, lp),
            WM_RBUTTONUP => button(egui::PointerButton::Secondary, false, lp),
            WM_MBUTTONDOWN => button(egui::PointerButton::Middle, true, lp),
            WM_MBUTTONUP => button(egui::PointerButton::Middle, false, lp),
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                let d = ((wp.0 >> 16) & 0xffff) as i16 as f32 / 120.0;
                let delta = if msg == WM_MOUSEWHEEL { egui::vec2(0.0, d) } else { egui::vec2(-d, 0.0) };
                push_event(sh, egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Line, delta, modifiers: modifiers() });
                LRESULT(0)
            }
            WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP => {
                let pressed = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
                let m = modifiers();
                let vk = wp.0 as u16;
                if pressed && m.ctrl && !m.alt {
                    match vk {
                        0x43 => push_event(sh, egui::Event::Copy),
                        0x58 => push_event(sh, egui::Event::Cut),
                        0x56 => {
                            if let Some(t) = get_clipboard(hwnd) {
                                push_event(sh, egui::Event::Paste(t));
                            }
                        }
                        _ => {}
                    }
                }
                if let Some(key) = vk_to_key(vk) {
                    push_event(sh, egui::Event::Key { key, physical_key: None, pressed, repeat: false, modifiers: m });
                }
                if msg == WM_SYSKEYDOWN || msg == WM_SYSKEYUP {
                    return DefWindowProcW(hwnd, msg, wp, lp);
                }
                LRESULT(0)
            }
            WM_CHAR => {
                let c = wp.0 as u16;
                let text = if (0xD800..0xDC00).contains(&c) {
                    hit.high_surrogate.set(c);
                    None
                } else if (0xDC00..0xE000).contains(&c) {
                    String::from_utf16(&[hit.high_surrogate.replace(0), c]).ok()
                } else {
                    char::from_u32(c as u32).map(|ch| ch.to_string())
                };
                let m = modifiers();
                if let Some(t) = text {
                    if !t.chars().any(|ch| ch.is_control()) && !(m.ctrl && !m.alt) {
                        push_event(sh, egui::Event::Text(t));
                    }
                }
                LRESULT(0)
            }
            WM_SETCURSOR if (lp.0 & 0xffff) as u32 == HTCLIENT => {
                SetCursor(hit.cursor.get());
                LRESULT(1)
            }
            WM_SIZE => {
                if wp.0 as u32 != SIZE_MINIMIZED {
                    if let Ok(mut i) = sh.input.try_borrow_mut() {
                        i.size = ((lp.0 & 0xffff) as u32, ((lp.0 >> 16) & 0xffff) as u32);
                        i.resized = true;
                    }
                    // Render right away so live resizing never shows stretched content.
                    let cmds = tick(sh);
                    exec(hwnd, sh, cmds);
                }
                LRESULT(0)
            }
            WM_ENTERSIZEMOVE => {
                SetTimer(hwnd, 1, 8, None);
                LRESULT(0)
            }
            WM_EXITSIZEMOVE => {
                let _ = KillTimer(hwnd, 1);
                LRESULT(0)
            }
            WM_TIMER if wp.0 == 1 => {
                let cmds = tick(sh);
                exec(hwnd, sh, cmds);
                LRESULT(0)
            }
            WM_ACTIVATE => {
                let focused = (wp.0 & 0xffff) != 0;
                if let Ok(mut i) = sh.input.try_borrow_mut() {
                    i.focused = focused;
                    i.events.push(egui::Event::WindowFocused(focused));
                }
                LRESULT(0)
            }
            WM_GETMINMAXINFO => {
                let mmi = &mut *(lp.0 as *mut MINMAXINFO);
                mmi.ptMinTrackSize = POINT { x: (920.0 * s) as i32, y: (600.0 * s) as i32 };
                LRESULT(0)
            }
            WM_DPICHANGED => {
                hit.dpi.set((wp.0 & 0xffff) as u32);
                let r = &*(lp.0 as *const RECT);
                let _ = SetWindowPos(hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                BeginPaint(hwnd, &mut ps);
                let _ = EndPaint(hwnd, &ps);
                if let Ok(mut i) = sh.input.try_borrow_mut() {
                    i.resized = true;
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

fn set_fullscreen(hwnd: HWND, hit: &Hit, on: bool) {
    if hit.fullscreen.get() == on {
        return;
    }
    unsafe {
        if on {
            let mut wp = WINDOWPLACEMENT { length: std::mem::size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
            let _ = GetWindowPlacement(hwnd, &mut wp);
            let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
            hit.saved.set(Some((wp, style)));
            let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
            let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
            let _ = GetMonitorInfoW(mon, &mut mi);
            hit.fullscreen.set(true);
            let new_style = (style as u32 & !WS_OVERLAPPEDWINDOW.0) | WS_POPUP.0;
            SetWindowLongPtrW(hwnd, GWL_STYLE, new_style as isize);
            let r = mi.rcMonitor;
            let _ = SetWindowPos(hwnd, HWND_TOP, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_FRAMECHANGED | SWP_NOOWNERZORDER);
        } else {
            hit.fullscreen.set(false);
            if let Some((wp, style)) = hit.saved.take() {
                SetWindowLongPtrW(hwnd, GWL_STYLE, style);
                let _ = SetWindowPlacement(hwnd, &wp);
            }
            let _ = SetWindowPos(hwnd, None, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOOWNERZORDER | SWP_FRAMECHANGED);
        }
    }
}

fn exec(hwnd: HWND, sh: &Shared, cmds: Vec<WinCmd>) {
    for c in cmds {
        unsafe {
            match c {
                WinCmd::Minimize => {
                    let _ = ShowWindow(hwnd, SW_MINIMIZE);
                }
                WinCmd::Close => {
                    let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
                }
                WinCmd::Fullscreen(on) => set_fullscreen(hwnd, &sh.hit, on),
                WinCmd::CopyText(t) => set_clipboard(hwnd, &t),
                WinCmd::Attention => {
                    if IsIconic(hwnd).as_bool() {
                        let _ = ShowWindow(hwnd, SW_RESTORE);
                    }
                    let _ = SetForegroundWindow(hwnd);
                    let fi = FLASHWINFO { cbSize: std::mem::size_of::<FLASHWINFO>() as u32, hwnd, dwFlags: FLASHW_ALL | FLASHW_TIMERNOFG, uCount: 3, dwTimeout: 0 };
                    let _ = FlashWindowEx(&fi);
                }
                WinCmd::Open(path) => {
                    let p = HSTRING::from(path);
                    ShellExecuteW(None, w!("open"), &p, None, None, SW_SHOWNORMAL);
                }
            }
        }
    }
}

fn tick(sh: &Shared) -> Vec<WinCmd> {
    let Ok(mut st) = sh.state.try_borrow_mut() else { return Vec::new() };
    let Some(st) = st.as_mut() else { return Vec::new() };
    st.frame(sh)
}

impl State {
    fn frame(&mut self, sh: &Shared) -> Vec<WinCmd> {
        let hit = &sh.hit;
        let (events, size, resized, focused, pointer_in) = {
            let mut i = sh.input.borrow_mut();
            (std::mem::take(&mut i.events), i.size, std::mem::take(&mut i.resized), i.focused, i.pointer_in)
        };
        if resized {
            if let Err(e) = self.gfx.resize(size.0, size.1) {
                crate::log!("resize failed: {e}");
            }
        }
        let new_video = self.app.poll(&self.gfx);
        let now = Instant::now();
        let due = new_video || resized || !events.is_empty() || self.next_repaint.map_or(true, |t| now >= t);
        if !due {
            return Vec::new();
        }
        let minimized = unsafe { IsIconic(self.hwnd) }.as_bool();
        let ppp = scale(hit);
        let (w, h) = self.gfx.size;
        let mut raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(w as f32 / ppp, h as f32 / ppp))),
            max_texture_side: Some(8192),
            time: Some(self.start.elapsed().as_secs_f64()),
            modifiers: modifiers(),
            events,
            focused,
            ..Default::default()
        };
        raw.viewports.entry(egui::ViewportId::ROOT).or_default().native_pixels_per_point = Some(ppp);
        let mut io = WinIo {
            maximized: unsafe { IsZoomed(self.hwnd) }.as_bool(),
            fullscreen: hit.fullscreen.get(),
            max_hover: hit.max_hover.get(),
            max_down: hit.max_down.get(),
            cmds: Vec::new(),
            caption_blockers: Vec::new(),
            show_title_bar: true,
            hide_cursor: false,
        };
        let out = self.ctx.run(raw, |ctx| self.app.ui(ctx, &mut io));

        let cursor = if io.hide_cursor { HCURSOR::default() } else { cursor_for(out.platform_output.cursor_icon) };
        if hit.cursor.replace(cursor) != cursor && pointer_in {
            unsafe { SetCursor(cursor) };
        }
        for c in &out.platform_output.commands {
            match c {
                egui::OutputCommand::CopyText(t) => io.cmds.push(WinCmd::CopyText(t.clone())),
                egui::OutputCommand::OpenUrl(u) => io.cmds.push(WinCmd::Open(u.url.clone())),
                _ => {}
            }
        }
        hit.title_bar.set(io.show_title_bar);
        *hit.blockers.borrow_mut() = io
            .caption_blockers
            .iter()
            .map(|r| RECT { left: (r.min.x * ppp) as i32, top: (r.min.y * ppp) as i32, right: (r.max.x * ppp) as i32, bottom: (r.max.y * ppp) as i32 })
            .collect();
        let delay = out.viewport_output.get(&egui::ViewportId::ROOT).map(|v| v.repaint_delay).unwrap_or(Duration::MAX);
        self.next_repaint = Some(now + delay.min(Duration::from_millis(500)));

        self.pending.append(out.textures_delta);
        if !minimized {
            let prims = self.ctx.tessellate(out.shapes, out.pixels_per_point);
            let delta = std::mem::take(&mut self.pending);
            let video = self.app.video().map(|(t, r)| (t, [r.min.x * ppp, r.min.y * ppp, r.width() * ppp, r.height() * ppp]));
            let clear = self.app.clear_color();
            if let Err(e) = self.gfx.render(clear, video, &prims, &delta, ppp) {
                crate::log!("render failed: {e}; recreating renderer");
                self.recreate();
            }
        }
        io.cmds
    }

    fn recreate(&mut self) {
        match Gfx::new(self.hwnd, self.gfx.size.0, self.gfx.size.1) {
            Ok(g) => {
                self.gfx = g;
                self.app.device_lost();
                self.ctx = egui::Context::default();
                theme::setup(&self.ctx);
                self.pending = Default::default();
                self.next_repaint = None;
            }
            Err(e) => crate::log!("renderer recreate failed: {e}"),
        }
    }
}

pub fn run(make_app: impl FnOnce(&egui::Context) -> App) -> Result<()> {
    unsafe {
        let hinst = GetModuleHandleW(None)?;
        let class = w!("NexMirror.Window");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            hIcon: make_icon(48),
            hIconSm: make_icon(16),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&wc);

        let sys_dpi = GetDpiForSystem();
        let shared: &'static Shared = Box::leak(Box::new(Shared {
            hit: Hit {
                fullscreen: Cell::new(false),
                dpi: Cell::new(sys_dpi),
                title_bar: Cell::new(true),
                max_hover: Cell::new(false),
                max_down: Cell::new(false),
                blockers: RefCell::new(Vec::new()),
                saved: Cell::new(None),
                cursor: Cell::new(LoadCursorW(None, IDC_ARROW)?),
                high_surrogate: Cell::new(0),
            },
            input: RefCell::new(Input { focused: true, ..Default::default() }),
            state: RefCell::new(None),
        }));

        // Centre on the primary monitor's work area.
        let s = sys_dpi as f32 / 96.0;
        let mut work = RECT::default();
        let _ = SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(&mut work as *mut _ as _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));
        let (ww, wh) = (((1180.0 * s) as i32).min(work.right - work.left), ((760.0 * s) as i32).min(work.bottom - work.top));
        let (x, y) = (work.left + (work.right - work.left - ww) / 2, work.top + (work.bottom - work.top - wh) / 2);

        let hwnd = CreateWindowExW(
            WS_EX_APPWINDOW,
            class,
            w!("NexMirror"),
            WS_OVERLAPPEDWINDOW,
            x,
            y,
            ww,
            wh,
            None,
            None,
            hinst,
            Some(shared as *const Shared as _),
        )?;
        shared.hit.dpi.set(GetDpiForWindow(hwnd));

        let dark = BOOL(1);
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const _ as _, 4);
        let corner = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &corner as *const _ as _, 4);
        let border: u32 = theme::BORDER_COLORREF;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_BORDER_COLOR, &border as *const _ as _, 4);
        let _ = DwmExtendFrameIntoClientArea(hwnd, &MARGINS { cxLeftWidth: 0, cxRightWidth: 0, cyTopHeight: 0, cyBottomHeight: 1 });
        let _ = SetWindowPos(hwnd, None, 0, 0, 0, 0, SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER);

        let mut rc = RECT::default();
        GetClientRect(hwnd, &mut rc)?;
        let (cw, ch) = (rc.right.max(1) as u32, rc.bottom.max(1) as u32);
        shared.input.borrow_mut().size = (cw, ch);
        let gfx = Gfx::new(hwnd, cw, ch)?;
        let ctx = egui::Context::default();
        theme::setup(&ctx);
        let app = make_app(&ctx);
        *shared.state.borrow_mut() = Some(State { hwnd, gfx, ctx, app, start: Instant::now(), next_repaint: None, pending: Default::default() });

        let _ = ShowWindow(hwnd, SW_SHOW);

        let mut msg = MSG::default();
        loop {
            let (timeout, event) = {
                let st = shared.state.borrow();
                let st = st.as_ref().unwrap();
                let t = match st.next_repaint {
                    Some(t) => t.saturating_duration_since(Instant::now()).as_millis().min(500) as u32,
                    None => 0,
                };
                (t, st.app.frame_event())
            };
            let handles: Vec<HANDLE> = event.into_iter().collect();
            MsgWaitForMultipleObjectsEx(Some(&handles), if timeout == 0 { 0 } else { timeout.max(1) }, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    // Drop the app (stops background threads) before leaving.
                    shared.state.borrow_mut().take();
                    return Ok(());
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            let cmds = tick(shared);
            exec(hwnd, shared, cmds);
        }
    }
}

