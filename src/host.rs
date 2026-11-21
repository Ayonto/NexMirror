//! Host service ("Share this PC"): accepts one viewer at a time, pairs it with a code,
//! then streams video (capture -> encode -> FEC -> UDP) and audio to it.

use crate::{
    audio,
    capture::{Acquired, Capture},
    config::{Resolution, SharedConfig},
    crypto::{self, Sealer},
    encoder::{EncodedFrame, Encoder},
    fec,
    proto::*,
};
use std::{
    io,
    net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket},
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

#[derive(Clone, PartialEq)]
pub enum HostState {
    Ready,
    Pairing { viewer: String, pin: String },
    Streaming { viewer: String, addr: String },
}

#[derive(Clone, Default)]
pub struct HostStats {
    pub width: u32,
    pub height: u32,
    pub fps: f32,
    pub mbps: f32,
    pub encode_ms: f32,
    pub encoder: String,
    pub display: String,
}

pub struct HostShared {
    cfg: SharedConfig,
    pub state: Mutex<HostState>,
    pub stats: Mutex<HostStats>,
    pub problem: Mutex<Option<String>>,
    stop: AtomicBool,
    settings_gen: AtomicU64,
    audio_enabled: AtomicBool,
    session: Mutex<Option<Arc<Session>>>,
    handshake_busy: AtomicBool,
    pub attention: AtomicBool,
}

struct Session {
    viewer_id: String,
    viewer_name: String,
    ip: IpAddr,
    viewer_screen: (u32, u32),
    stop: AtomicBool,
    keyframe: AtomicBool,
    peer: Mutex<Option<SocketAddr>>,
    udp: UdpSocket,
    media: Sealer,
    punch: Sealer,
    seq: AtomicU64,
    next_frame: AtomicU32,
    tx: Mutex<Tx>,
}

impl Session {
    fn send(&self, m: &Msg) {
        let _ = self.tx.lock().unwrap().send(m);
    }

    fn end(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.tx.lock().unwrap().shutdown();
    }
}

pub struct Host {
    pub shared: Arc<HostShared>,
    threads: Vec<JoinHandle<()>>,
}

fn friendly_bind_error(e: io::Error) -> String {
    if e.kind() == io::ErrorKind::AddrInUse {
        format!("Port {PORT} is already in use. Is another copy of NexMirror sharing this PC?")
    } else {
        format!("Could not open port {PORT}: {e}")
    }
}

impl Host {
    pub fn start(cfg: SharedConfig) -> Result<Host, String> {
        let listener = TcpListener::bind(("0.0.0.0", PORT)).map_err(friendly_bind_error)?;
        let udp = UdpSocket::bind(("0.0.0.0", PORT)).map_err(friendly_bind_error)?;
        let _ = socket2::SockRef::from(&udp).set_send_buffer_size(8 << 20);
        let audio = cfg.lock().unwrap().host.audio;
        let shared = Arc::new(HostShared {
            cfg,
            state: Mutex::new(HostState::Ready),
            stats: Mutex::new(HostStats::default()),
            problem: Mutex::new(None),
            stop: AtomicBool::new(false),
            settings_gen: AtomicU64::new(0),
            audio_enabled: AtomicBool::new(audio),
            session: Mutex::new(None),
            handshake_busy: AtomicBool::new(false),
            attention: AtomicBool::new(false),
        });
        let mut threads = Vec::new();
        let (sh, u) = (shared.clone(), udp.try_clone().map_err(|e| e.to_string())?);
        threads.push(std::thread::Builder::new().name("host-udp".into()).spawn(move || udp_loop(sh, u)).unwrap());
        let sh = shared.clone();
        threads.push(std::thread::Builder::new().name("host-accept".into()).spawn(move || accept_loop(sh, listener, udp)).unwrap());
        crate::log!("host: sharing started on port {PORT}");
        Ok(Host { shared, threads })
    }

    pub fn state(&self) -> HostState {
        self.shared.state.lock().unwrap().clone()
    }

    pub fn stats(&self) -> HostStats {
        self.shared.stats.lock().unwrap().clone()
    }

    pub fn problem(&self) -> Option<String> {
        self.shared.problem.lock().unwrap().clone()
    }

    /// Call after host settings changed; the running stream is rebuilt with the new settings.
    pub fn settings_changed(&self) {
        let audio = self.shared.cfg.lock().unwrap().host.audio;
        self.shared.audio_enabled.store(audio, Ordering::Relaxed);
        self.shared.settings_gen.fetch_add(1, Ordering::Relaxed);
    }

    pub fn disconnect_viewer(&self) {
        if let Some(s) = self.shared.session.lock().unwrap().as_ref() {
            s.send(&Msg::Error { msg: "The host ended the session.".into() });
            s.end();
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(s) = self.shared.session.lock().unwrap().take() {
            s.send(&Msg::Bye);
            s.end();
        }
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        crate::log!("host: sharing stopped");
    }
}

fn udp_loop(sh: Arc<HostShared>, udp: UdpSocket) {
    let _ = udp.set_read_timeout(Some(Duration::from_millis(200)));
    let mut buf = [0u8; 2048];
    while !sh.stop.load(Ordering::Relaxed) {
        let Ok((n, src)) = udp.recv_from(&mut buf) else { continue };
        match buf[0] {
            PKT_PROBE if n >= 9 && &buf[1..5] == PROBE_MAGIC => {
                let (id, name) = {
                    let c = sh.cfg.lock().unwrap();
                    (c.device_id.clone(), c.device_name.clone())
                };
                let busy = sh.session.lock().unwrap().is_some();
                let mut reply = vec![PKT_ANNOUNCE];
                reply.extend(serde_json::to_vec(&Announce { id, name, version: VERSION, busy }).unwrap_or_default());
                let _ = udp.send_to(&reply, src);
            }
            PKT_PUNCH if n > 9 => {
                let Some(seq) = read_seq(&buf[..n]) else { continue };
                if let Some(s) = sh.session.lock().unwrap().as_ref() {
                    if s.ip == src.ip() && s.punch.open_tail(seq, &mut buf[..n], 9).is_some() {
                        let mut p = s.peer.lock().unwrap();
                        if *p != Some(src) {
                            crate::log!("host: media path to {src}");
                            *p = Some(src);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

fn accept_loop(sh: Arc<HostShared>, listener: TcpListener, udp: UdpSocket) {
    let _ = listener.set_nonblocking(true);
    while !sh.stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((s, addr)) => {
                let _ = s.set_nonblocking(false);
                let (sh, u) = (sh.clone(), udp.try_clone());
                let Ok(u) = u else { continue };
                std::thread::Builder::new()
                    .name("host-session".into())
                    .spawn(move || {
                        if let Err(e) = session_main(&sh, u, s, addr) {
                            crate::log!("host: session with {addr} ended: {e}");
                        }
                    })
                    .ok();
            }
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

struct BusyGuard<'a>(&'a AtomicBool);
impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

fn session_main(sh: &Arc<HostShared>, udp: UdpSocket, s: TcpStream, addr: SocketAddr) -> Result<(), String> {
    let (mut tx, mut rx) = channel(s).map_err(|e| e.to_string())?;
    rx.set_timeout(Some(Duration::from_secs(10)));
    let e = |e: io::Error| e.to_string();
    let Msg::Hello { version, id, name, screen_w, screen_h } = rx.recv().map_err(e)? else { return Err("expected hello".into()) };
    if version != VERSION {
        let _ = tx.send(&Msg::Error { msg: "This PC runs a different version of NexMirror. Update both devices to the same version.".into() });
        return Err("version mismatch".into());
    }
    if let Some(cur) = sh.session.lock().unwrap().as_ref() {
        if cur.viewer_id != id {
            let _ = tx.send(&Msg::Error { msg: format!("This PC is already being viewed by {}.", cur.viewer_name) });
            return Err("busy".into());
        }
    }
    if sh.handshake_busy.swap(true, Ordering::Relaxed) {
        let _ = tx.send(&Msg::Error { msg: "Another device is connecting to this PC right now. Try again in a moment.".into() });
        return Err("handshake busy".into());
    }
    let _guard = BusyGuard(&sh.handshake_busy);

    let (host_id, host_name, secret) = {
        let c = sh.cfg.lock().unwrap();
        (c.device_id.clone(), c.device_name.clone(), c.viewer_secret(&id))
    };
    tx.send(&Msg::HelloAck { version: VERSION, id: host_id.clone(), name: host_name, trusted: secret.is_some() }).map_err(e)?;
    let Msg::Auth { pin } = rx.recv().map_err(e)? else { return Err("expected auth".into()) };
    let pin_mode = pin || secret.is_none();
    let restore = |sh: &HostShared| {
        let mut st = sh.state.lock().unwrap();
        if matches!(*st, HostState::Pairing { .. }) {
            *st = HostState::Ready;
        }
    };
    let password = if pin_mode {
        let code = crypto::random_pin();
        *sh.state.lock().unwrap() = HostState::Pairing { viewer: name.clone(), pin: code.clone() };
        sh.attention.store(true, Ordering::Relaxed);
        rx.set_timeout(Some(Duration::from_secs(180)));
        code.into_bytes()
    } else {
        secret.unwrap()
    };
    let spake = rx.recv();
    let Ok(Msg::Spake { msg }) = spake else {
        restore(sh);
        return Err("pairing cancelled".into());
    };
    let (pake, mine) = crypto::pake_start(false, &password, &id, &host_id);
    tx.send(&Msg::Spake { msg: mine }).map_err(e)?;
    let Some(key) = crypto::pake_finish(pake, &msg) else {
        restore(sh);
        return Err("key exchange failed".into());
    };
    let keys = crypto::derive_keys(&key);
    tx.enable_crypto(&keys.s2c);
    rx.enable_crypto(&keys.c2s);
    rx.set_timeout(Some(Duration::from_secs(10)));
    if !matches!(rx.recv(), Ok(Msg::Confirm)) {
        let msg = if pin_mode { "The code was incorrect. Try connecting again." } else { "This device is no longer trusted. Pair again with a code." };
        let _ = tx.send_plain(&Msg::Error { msg: msg.into() });
        restore(sh);
        if !pin_mode {
            // Stale pairing: forget it so the next attempt asks for a code.
            let mut c = sh.cfg.lock().unwrap();
            c.trusted_viewers.retain(|v| v.id != id);
            c.save();
        }
        return Err("authentication failed".into());
    }
    if pin_mode {
        let secret = crypto::random_bytes(32);
        {
            let mut c = sh.cfg.lock().unwrap();
            c.set_trusted_viewer(&id, &name, &secret);
            c.save();
        }
        tx.send(&Msg::Paired { secret }).map_err(e)?;
        crate::log!("host: paired with {name}");
    }

    let sess = Arc::new(Session {
        viewer_id: id,
        viewer_name: name.clone(),
        ip: addr.ip(),
        viewer_screen: (screen_w, screen_h),
        stop: AtomicBool::new(false),
        keyframe: AtomicBool::new(true),
        peer: Mutex::new(None),
        udp,
        media: Sealer::new(&keys.media),
        punch: Sealer::new(&keys.punch),
        seq: AtomicU64::new(0),
        next_frame: AtomicU32::new(0),
        tx: Mutex::new(tx),
    });
    if let Some(old) = sh.session.lock().unwrap().replace(sess.clone()) {
        old.end();
    }
    drop(_guard);
    *sh.state.lock().unwrap() = HostState::Streaming { viewer: name.clone(), addr: addr.ip().to_string() };
    *sh.problem.lock().unwrap() = None;
    sess.send(&Msg::Ready);
    crate::log!("host: streaming to {name} ({addr})");

    let (s1, h1) = (sess.clone(), sh.clone());
    let video = std::thread::Builder::new().name("host-video".into()).spawn(move || video_thread(h1, s1)).ok();
    let (s2, h2) = (sess.clone(), sh.clone());
    let audio = std::thread::Builder::new().name("host-audio".into()).spawn(move || audio_thread(h2, s2)).ok();

    rx.set_timeout(Some(Duration::from_secs(6)));
    let result = loop {
        if sess.stop.load(Ordering::Relaxed) || sh.stop.load(Ordering::Relaxed) {
            break Ok(());
        }
        match rx.recv() {
            Ok(Msg::RequestKeyframe) => sess.keyframe.store(true, Ordering::Relaxed),
            Ok(Msg::Ping { t }) => sess.send(&Msg::Pong { t, host: now_us() }),
            Ok(Msg::Bye) => break Ok(()),
            Ok(_) => {}
            Err(err) => break if sess.stop.load(Ordering::Relaxed) { Ok(()) } else { Err(err.to_string()) },
        }
    };
    sess.end();
    for t in [video, audio].into_iter().flatten() {
        let _ = t.join();
    }
    {
        let mut cur = sh.session.lock().unwrap();
        if cur.as_ref().is_some_and(|c| Arc::ptr_eq(c, &sess)) {
            *cur = None;
            *sh.state.lock().unwrap() = HostState::Ready;
            *sh.stats.lock().unwrap() = HostStats::default();
        }
    }
    crate::log!("host: {name} disconnected");
    result
}

fn stream_size(sw: u32, sh: u32, res: Resolution, viewer: (u32, u32)) -> (u32, u32) {
    let mut scale: f64 = 1.0;
    match res {
        Resolution::Native => {}
        Resolution::Auto => {
            if viewer.0 >= 320 && viewer.1 >= 240 {
                scale = (viewer.0 as f64 / sw as f64).min(viewer.1 as f64 / sh as f64).min(1.0);
            }
        }
        r => scale = (r.max_height().unwrap() as f64 / sh as f64).min(1.0),
    }
    let even = |v: f64| ((v.round() as u32) & !1).max(2);
    (even(sw as f64 * scale), even(sh as f64 * scale))
}

fn auto_bitrate(w: u32, h: u32, fps: u32) -> u32 {
    ((w as f64 * h as f64 * fps as f64 * 0.2) as u32).clamp(8_000_000, 80_000_000)
}

fn friendly(e: &windows::core::Error) -> String {
    use windows::Win32::{Foundation::E_ACCESSDENIED, Graphics::Dxgi::*};
    let c = e.code();
    if c == E_ACCESSDENIED {
        "Windows is showing a secure screen (UAC prompt or lock screen). Streaming resumes automatically.".into()
    } else if c == DXGI_ERROR_UNSUPPORTED || c == DXGI_ERROR_NOT_CURRENTLY_AVAILABLE {
        "This display cannot be captured right now. Try another display or close other capture tools.".into()
    } else {
        let m = e.message();
        if m.is_empty() { format!("Streaming error 0x{:08X}", c.0) } else { m.trim().to_string() }
    }
}

fn video_thread(sh: Arc<HostShared>, sess: Arc<Session>) {
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
    }
    let mut fails = 0u32;
    while !sess.stop.load(Ordering::Relaxed) {
        match run_video(&sh, &sess) {
            Ok(()) => fails = 0,
            Err(err) => {
                fails += 1;
                let msg = friendly(&err);
                if fails <= 3 || fails % 20 == 0 {
                    crate::log!("host: video pipeline: {err} ({msg})");
                }
                *sh.problem.lock().unwrap() = Some(msg);
                std::thread::sleep(Duration::from_millis((150 * fails as u64).min(1500)));
            }
        }
    }
}

fn run_video(sh: &HostShared, sess: &Session) -> windows::core::Result<()> {
    let settings = sh.cfg.lock().unwrap().host.clone();
    let gen = sh.settings_gen.load(Ordering::Relaxed);
    let mut cap = Capture::new(&settings.display)?;
    let (w, h) = stream_size(cap.src_w, cap.src_h, settings.resolution, sess.viewer_screen);
    cap.set_output_size(w, h)?;
    let fps = settings.fps.clamp(24, 240);
    let bitrate = if settings.bitrate_mbps > 0 { settings.bitrate_mbps * 1_000_000 } else { auto_bitrate(w, h, fps) };
    let mut enc = Encoder::new(&cap.device, cap.adapter_luid, w, h, fps, bitrate)?;
    *sh.problem.lock().unwrap() = None;
    sess.send(&Msg::Stream(StreamInfo { width: w, height: h, fps, encoder: enc.name.clone() }));
    {
        let mut st = sh.stats.lock().unwrap();
        *st = HostStats { width: w, height: h, encoder: enc.name.clone(), display: cap.display_name.clone(), ..Default::default() };
    }

    // Frames are encoded on a fixed schedule so the cap holds even when the display refreshes
    // faster; a small tolerance avoids holding back frames that arrive just before their slot.
    let interval = Duration::from_secs_f64(1.0 / fps as f64);
    let tol = (interval / 4).min(Duration::from_millis(2));
    let mut next = Instant::now();
    let mut dirty = true;
    let mut force_key = true;
    let mut rs = fec::RsCache::default();
    let mut pkt = Vec::with_capacity(1400);
    let (mut frames, mut bytes, mut enc_time) = (0u32, 0u64, Duration::ZERO);
    let mut window = Instant::now();
    loop {
        if sess.stop.load(Ordering::Relaxed) || sh.settings_gen.load(Ordering::Relaxed) != gen {
            return Ok(());
        }
        let timeout = if dirty { next.saturating_duration_since(Instant::now() + tol).as_millis() as u32 } else { 50 };
        match cap.acquire(timeout) {
            Ok(Acquired::Updated) => dirty = true,
            Ok(Acquired::Nothing) => {}
            Err(err) if err.code() == windows::Win32::Graphics::Dxgi::DXGI_ERROR_ACCESS_LOST => {
                // Display mode change, fullscreen switch or secure desktop: rebuild the pipeline.
                return Ok(());
            }
            Err(err) => return Err(err),
        }
        if sess.keyframe.swap(false, Ordering::Relaxed) {
            force_key = true;
            dirty = true;
        }
        let peer = *sess.peer.lock().unwrap();
        if dirty && Instant::now() + tol >= next {
            if let Some(peer) = peer {
                let t0 = Instant::now();
                let ts = now_us();
                let tex = cap.render()?;
                let out = enc.encode(&tex, ts, force_key)?;
                enc_time += t0.elapsed();
                for f in &out {
                    send_frame(sess, &mut rs, &mut pkt, peer, f);
                    bytes += f.data.len() as u64;
                    frames += 1;
                }
                force_key = false;
                dirty = false;
                next = if next + interval < t0 { t0 + interval } else { next + interval };
            }
        }
        let el = window.elapsed();
        if el >= Duration::from_secs(1) {
            let mut st = sh.stats.lock().unwrap();
            st.fps = frames as f32 / el.as_secs_f32();
            st.mbps = bytes as f32 * 8.0 / el.as_secs_f32() / 1e6;
            st.encode_ms = if frames > 0 { enc_time.as_secs_f32() * 1000.0 / frames as f32 } else { 0.0 };
            (frames, bytes, enc_time, window) = (0, 0, Duration::ZERO, Instant::now());
        }
    }
}

fn send_frame(sess: &Session, rs: &mut fec::RsCache, pkt: &mut Vec<u8>, peer: SocketAddr, f: &EncodedFrame) {
    let id = sess.next_frame.fetch_add(1, Ordering::Relaxed);
    fec::encode(rs, &f.data, |block, nblocks, shard, k, m, bytes| {
        let seq = sess.seq.fetch_add(1, Ordering::Relaxed);
        let h = VideoHdr { seq, frame: id, block, nblocks, shard, k, m, frame_len: f.data.len() as u32, ts: f.ts, flags: if f.key { FLAG_KEY } else { 0 } };
        pkt.clear();
        h.write(pkt);
        pkt.extend_from_slice(bytes);
        sess.media.seal_tail(seq, pkt, VIDEO_HDR);
        let _ = sess.udp.send_to(pkt, peer);
    });
}

fn audio_thread(sh: Arc<HostShared>, sess: Arc<Session>) {
    let mut announced = 0u32;
    let mut pkt = Vec::with_capacity(2048);
    while !sess.stop.load(Ordering::Relaxed) {
        if !sh.audio_enabled.load(Ordering::Relaxed) {
            if announced != 0 {
                sess.send(&Msg::Audio { rate: 0 });
                announced = 0;
            }
            std::thread::sleep(Duration::from_millis(200));
            continue;
        }
        audio::run_loopback(
            || sess.stop.load(Ordering::Relaxed) || !sh.audio_enabled.load(Ordering::Relaxed),
            |samples, rate| {
                if rate != announced {
                    sess.send(&Msg::Audio { rate });
                    announced = rate;
                }
                let Some(peer) = *sess.peer.lock().unwrap() else { return };
                let seq = sess.seq.fetch_add(1, Ordering::Relaxed);
                pkt.clear();
                audio_hdr(seq, now_us(), &mut pkt);
                for s in samples {
                    pkt.extend_from_slice(&s.to_le_bytes());
                }
                sess.media.seal_tail(seq, &mut pkt, AUDIO_HDR);
                let _ = sess.udp.send_to(&pkt, peer);
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_sizes() {
        assert_eq!(stream_size(2560, 1440, Resolution::Auto, (1920, 1080)), (1920, 1080));
        assert_eq!(stream_size(1920, 1080, Resolution::Auto, (2560, 1600)), (1920, 1080));
        assert_eq!(stream_size(2560, 1440, Resolution::P720, (0, 0)), (1280, 720));
        assert_eq!(stream_size(1366, 768, Resolution::Native, (0, 0)), (1366, 768));
    }
}
