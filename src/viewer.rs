//! Viewer session: connects to a host, pairs if needed, receives and repairs the media
//! stream, and hands complete frames to the UI thread for hardware decoding.

use crate::{
    audio::Player,
    config::SharedConfig,
    crypto::{self, Sealer},
    decoder::Decoder,
    fec::{Assembler, Complete},
    gfx::{Gfx, VideoTex},
    proto::*,
};
use std::{
    collections::VecDeque,
    io,
    net::{SocketAddr, TcpStream, ToSocketAddrs, UdpSocket},
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::{CloseHandle, HANDLE},
    System::Threading::{CreateEventW, SetEvent},
    UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN},
};

#[derive(Clone, PartialEq, Debug)]
pub enum ViewerState {
    Connecting,
    NeedPin,
    Waiting,
    Streaming,
    Failed(String),
}

#[derive(Clone, Default)]
pub struct ViewerStats {
    pub width: u32,
    pub height: u32,
    pub fps: f32,
    pub mbps: f32,
    pub latency_ms: f32,
    pub decode_ms: f32,
    pub rtt_ms: f32,
    pub lost_frames: u64,
    pub repaired_packets: u64,
    pub encoder: String,
    pub hardware_decode: bool,
}

struct Event(HANDLE);
unsafe impl Send for Event {}
unsafe impl Sync for Event {}
impl Drop for Event {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

#[derive(Default)]
struct Clock {
    offset_us: i64,
    samples: VecDeque<(u64, i64)>,
}

impl Clock {
    fn add(&mut self, rtt: u64, offset: i64) {
        self.samples.push_back((rtt, offset));
        if self.samples.len() > 16 {
            self.samples.pop_front();
        }
        // The sample with the lowest round-trip time gives the most accurate offset.
        if let Some(best) = self.samples.iter().min_by_key(|s| s.0) {
            self.offset_us = best.1;
        }
    }
}

struct Shared {
    state: Mutex<ViewerState>,
    host_name: Mutex<String>,
    pin: Mutex<Option<mpsc::Sender<Option<String>>>>,
    frames: Mutex<VecDeque<Complete>>,
    event: Event,
    /// Tells the session threads to wind down.
    stop: AtomicBool,
    /// The user closed the session (so failures are not reported).
    closed: AtomicBool,
    ctl: Mutex<Option<Tx>>,
    info: Mutex<Option<StreamInfo>>,
    audio_rate: AtomicU32,
    player: Mutex<Option<Player>>,
    clock: Mutex<Clock>,
    rtt_us: AtomicU64,
    bytes: AtomicU64,
    lost: AtomicU64,
    repaired: AtomicU64,
    want_key: AtomicBool,
}

impl Shared {
    fn set_state(&self, s: ViewerState) {
        let mut st = self.state.lock().unwrap();
        if !matches!(*st, ViewerState::Failed(_)) {
            *st = s;
        }
    }

    fn send(&self, m: &Msg) {
        if let Some(t) = self.ctl.lock().unwrap().as_mut() {
            let _ = t.send(m);
        }
    }
}

pub struct Session {
    sh: Arc<Shared>,
    pub addr: String,
    decoder: Option<Decoder>,
    stats: ViewerStats,
    win_start: Instant,
    win_frames: u32,
    win_decode: Duration,
    win_bytes: u64,
    latency_acc: (f64, u32),
    audio: (bool, f32),
}

fn resolve(addr: &str) -> io::Result<SocketAddr> {
    let a = addr.trim();
    let with_port = if a.parse::<SocketAddr>().is_ok() || (a.contains(':') && !a.contains("::") && a.rsplit(':').next().is_some_and(|p| p.parse::<u16>().is_ok())) {
        a.to_string()
    } else {
        format!("{a}:{PORT}")
    };
    with_port
        .to_socket_addrs()?
        .find(|s| s.is_ipv4())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "address not found"))
}

impl Session {
    pub fn connect(addr: &str, cfg: SharedConfig, audio: bool, volume: f32) -> Session {
        let event = unsafe { CreateEventW(None, false, false, None).unwrap_or_default() };
        let sh = Arc::new(Shared {
            state: Mutex::new(ViewerState::Connecting),
            host_name: Mutex::new(addr.to_string()),
            pin: Mutex::new(None),
            frames: Mutex::new(VecDeque::new()),
            event: Event(event),
            stop: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            ctl: Mutex::new(None),
            info: Mutex::new(None),
            audio_rate: AtomicU32::new(0),
            player: Mutex::new(None),
            clock: Mutex::new(Clock::default()),
            rtt_us: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            lost: AtomicU64::new(0),
            repaired: AtomicU64::new(0),
            want_key: AtomicBool::new(false),
        });
        let (s, a) = (sh.clone(), addr.to_string());
        std::thread::Builder::new()
            .name("viewer-control".into())
            .spawn(move || {
                if let Err(msg) = control_main(&s, &a, cfg) {
                    if !s.closed.load(Ordering::Relaxed) {
                        crate::log!("viewer: {msg}");
                        *s.state.lock().unwrap() = ViewerState::Failed(msg);
                    }
                }
                s.stop.store(true, Ordering::Relaxed);
                *s.player.lock().unwrap() = None;
            })
            .ok();
        Session {
            sh,
            addr: addr.to_string(),
            decoder: None,
            stats: ViewerStats::default(),
            win_start: Instant::now(),
            win_frames: 0,
            win_decode: Duration::ZERO,
            win_bytes: 0,
            latency_acc: (0.0, 0),
            audio: (audio, volume),
        }
    }

    pub fn state(&self) -> ViewerState {
        self.sh.state.lock().unwrap().clone()
    }

    pub fn host_name(&self) -> String {
        self.sh.host_name.lock().unwrap().clone()
    }

    pub fn submit_pin(&self, pin: Option<String>) {
        if let Some(tx) = self.sh.pin.lock().unwrap().take() {
            let _ = tx.send(pin);
        }
    }

    pub fn frame_event(&self) -> HANDLE {
        self.sh.event.0
    }

    pub fn stats(&self) -> ViewerStats {
        self.stats.clone()
    }

    pub fn video(&self) -> Option<&VideoTex> {
        self.decoder.as_ref().and_then(|d| d.out.as_ref())
    }

    pub fn set_audio(&mut self, enabled: bool, volume: f32) {
        self.audio = (enabled, volume);
    }

    pub fn device_lost(&mut self) {
        self.decoder = None;
        self.sh.want_key.store(true, Ordering::Relaxed);
    }

    /// Decodes everything that arrived since the last call. Returns true if the picture changed.
    pub fn poll(&mut self, gfx: &Gfx) -> bool {
        self.manage_audio();
        let frames: Vec<Complete> = self.sh.frames.lock().unwrap().drain(..).collect();
        let mut got = false;
        if !frames.is_empty() && self.decoder.is_none() {
            match Decoder::new(&gfx.device) {
                Ok(d) => {
                    self.stats.hardware_decode = d.hardware;
                    self.decoder = Some(d);
                }
                Err(e) => {
                    *self.sh.state.lock().unwrap() = ViewerState::Failed(format!("Video decoding is unavailable on this PC: {}", e.message()));
                    return false;
                }
            }
        }
        for f in frames {
            let Some(dec) = self.decoder.as_mut() else { break };
            let t0 = Instant::now();
            match dec.decode(&f.data) {
                Ok(true) => {
                    got = true;
                    self.win_frames += 1;
                    self.win_decode += t0.elapsed();
                    let offset = self.sh.clock.lock().unwrap().offset_us;
                    if offset != 0 {
                        let lat = (now_us() as i64 + offset - f.ts as i64) as f64 / 1000.0;
                        if (0.0..2000.0).contains(&lat) {
                            self.latency_acc.0 += lat;
                            self.latency_acc.1 += 1;
                        }
                    }
                }
                Ok(false) => {}
                Err(e) => {
                    crate::log!("viewer: decode error: {e}");
                    self.decoder = None;
                    self.sh.want_key.store(true, Ordering::Relaxed);
                }
            }
        }
        if got {
            let st = self.state();
            if st == ViewerState::Waiting {
                self.sh.set_state(ViewerState::Streaming);
            }
        }
        self.update_stats();
        got
    }

    fn manage_audio(&mut self) {
        let rate = self.sh.audio_rate.load(Ordering::Relaxed);
        let want = self.audio.0 && rate > 0 && !self.sh.stop.load(Ordering::Relaxed);
        let mut p = self.sh.player.lock().unwrap();
        match (&*p, want) {
            (Some(pl), true) if pl.rate == rate => pl.volume.store(self.audio.1.to_bits(), Ordering::Relaxed),
            (_, true) => *p = Some(Player::start(rate, self.audio.1)),
            (Some(_), false) => *p = None,
            (None, false) => {}
        }
    }

    fn update_stats(&mut self) {
        let el = self.win_start.elapsed();
        if el < Duration::from_millis(1000) {
            return;
        }
        let bytes = self.sh.bytes.load(Ordering::Relaxed);
        let s = &mut self.stats;
        s.fps = self.win_frames as f32 / el.as_secs_f32();
        s.mbps = (bytes - self.win_bytes) as f32 * 8.0 / el.as_secs_f32() / 1e6;
        s.decode_ms = if self.win_frames > 0 { self.win_decode.as_secs_f32() * 1000.0 / self.win_frames as f32 } else { 0.0 };
        if self.latency_acc.1 > 0 {
            s.latency_ms = (self.latency_acc.0 / self.latency_acc.1 as f64) as f32;
        }
        s.rtt_ms = self.sh.rtt_us.load(Ordering::Relaxed) as f32 / 1000.0;
        s.lost_frames = self.sh.lost.load(Ordering::Relaxed);
        s.repaired_packets = self.sh.repaired.load(Ordering::Relaxed);
        if let Some(i) = self.sh.info.lock().unwrap().as_ref() {
            s.width = i.width;
            s.height = i.height;
            s.encoder = i.encoder.clone();
        }
        self.win_bytes = bytes;
        self.win_frames = 0;
        self.win_decode = Duration::ZERO;
        self.latency_acc = (0.0, 0);
        self.win_start = Instant::now();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.sh.closed.store(true, Ordering::Relaxed);
        self.sh.stop.store(true, Ordering::Relaxed);
        self.submit_pin(None);
        if let Some(mut t) = self.sh.ctl.lock().unwrap().take() {
            let _ = t.send(&Msg::Bye);
            t.shutdown();
        }
        *self.sh.player.lock().unwrap() = None;
    }
}

fn io_msg(e: io::Error, host: &str) -> String {
    match e.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => format!("{host} stopped responding."),
        io::ErrorKind::ConnectionRefused => format!("{host} is not sharing. Turn on \"Share this PC\" in NexMirror on that PC."),
        io::ErrorKind::InvalidData => "The connection could not be secured.".into(),
        _ => format!("Lost connection to {host}."),
    }
}

fn control_main(sh: &Arc<Shared>, addr: &str, cfg: SharedConfig) -> Result<(), String> {
    let target = resolve(addr).map_err(|_| format!("Could not find \"{addr}\" on your network."))?;
    let s = TcpStream::connect_timeout(&target, Duration::from_secs(4)).map_err(|e| match e.kind() {
        io::ErrorKind::TimedOut => format!("{addr} did not answer. Check that both devices are on the same network and that Windows Firewall allows NexMirror on the PC you are connecting to."),
        _ => io_msg(e, addr),
    })?;
    let (mut tx, mut rx) = channel(s).map_err(|e| e.to_string())?;
    rx.set_timeout(Some(Duration::from_secs(10)));
    let (my_id, my_name) = {
        let c = cfg.lock().unwrap();
        (c.device_id.clone(), c.device_name.clone())
    };
    let screen = unsafe { (GetSystemMetrics(SM_CXSCREEN) as u32, GetSystemMetrics(SM_CYSCREEN) as u32) };
    tx.send(&Msg::Hello { version: VERSION, id: my_id.clone(), name: my_name, screen_w: screen.0, screen_h: screen.1 }).map_err(|e| io_msg(e, addr))?;
    let (host_id, host_name, trusted) = match rx.recv().map_err(|e| io_msg(e, addr))? {
        Msg::HelloAck { id, name, trusted, .. } => (id, name, trusted),
        Msg::Error { msg } => return Err(msg),
        _ => return Err("Unexpected reply from host.".into()),
    };
    *sh.host_name.lock().unwrap() = host_name.clone();
    let secret = cfg.lock().unwrap().host_secret(&host_id);
    let pin_mode = !trusted || secret.is_none();
    tx.send(&Msg::Auth { pin: pin_mode }).map_err(|e| io_msg(e, &host_name))?;
    let password = if pin_mode {
        let (ptx, prx) = mpsc::channel();
        *sh.pin.lock().unwrap() = Some(ptx);
        sh.set_state(ViewerState::NeedPin);
        let pin = prx.recv().ok().flatten().ok_or_else(|| "Pairing cancelled.".to_string())?;
        if sh.closed.load(Ordering::Relaxed) {
            return Err("Pairing cancelled.".into());
        }
        sh.set_state(ViewerState::Connecting);
        pin.into_bytes()
    } else {
        secret.unwrap()
    };
    let (pake, mine) = crypto::pake_start(true, &password, &my_id, &host_id);
    tx.send(&Msg::Spake { msg: mine }).map_err(|e| io_msg(e, &host_name))?;
    let theirs = match rx.recv().map_err(|e| io_msg(e, &host_name))? {
        Msg::Spake { msg } => msg,
        Msg::Error { msg } => return Err(msg),
        _ => return Err("Unexpected reply from host.".into()),
    };
    let key = crypto::pake_finish(pake, &theirs).ok_or("Secure key exchange failed.")?;
    let keys = crypto::derive_keys(&key);
    tx.enable_crypto(&keys.c2s);
    rx.enable_crypto(&keys.s2c);
    tx.send(&Msg::Confirm).map_err(|e| io_msg(e, &host_name))?;
    loop {
        match rx.recv() {
            Ok(Msg::Paired { secret }) => {
                let mut c = cfg.lock().unwrap();
                c.set_paired_host(&host_id, &host_name, &secret, addr);
                c.save();
            }
            Ok(Msg::Ready) => break,
            Ok(Msg::Error { msg }) => {
                if !pin_mode {
                    // The host forgot us; drop our stale secret so the next attempt asks for a code.
                    let mut c = cfg.lock().unwrap();
                    c.paired_hosts.retain(|h| h.id != host_id);
                    c.save();
                }
                return Err(msg);
            }
            Ok(_) => {}
            Err(e) => return Err(io_msg(e, &host_name)),
        }
    }
    {
        let mut c = cfg.lock().unwrap();
        c.remember_addr(addr);
        if let Some(h) = c.paired_hosts.iter_mut().find(|h| h.id == host_id) {
            h.last_addr = addr.to_string();
            h.name = host_name.clone();
        }
        c.save();
    }

    let udp = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, Some(socket2::Protocol::UDP)).map_err(|e| e.to_string())?;
    let _ = udp.set_recv_buffer_size(16 << 20);
    udp.bind(&SocketAddr::from(([0, 0, 0, 0], 0)).into()).map_err(|e| e.to_string())?;
    let udp: UdpSocket = udp.into();
    let _ = udp.set_read_timeout(Some(Duration::from_millis(20)));
    let host_udp = SocketAddr::new(target.ip(), PORT);
    *sh.ctl.lock().unwrap() = Some(tx);
    sh.set_state(ViewerState::Waiting);
    crate::log!("viewer: connected to {host_name} ({target})");

    let (s2, media, punch) = (sh.clone(), Sealer::new(&keys.media), Sealer::new(&keys.punch));
    let media_thread = std::thread::Builder::new().name("viewer-media".into()).spawn(move || media_main(&s2, udp, host_udp, media, punch)).ok();

    rx.set_timeout(Some(Duration::from_secs(6)));
    let result = loop {
        if sh.stop.load(Ordering::Relaxed) {
            break Ok(());
        }
        match rx.recv() {
            Ok(Msg::Stream(info)) => {
                crate::log!("viewer: stream {}x{}@{} via {}", info.width, info.height, info.fps, info.encoder);
                *sh.info.lock().unwrap() = Some(info);
            }
            Ok(Msg::Audio { rate }) => sh.audio_rate.store(rate, Ordering::Relaxed),
            Ok(Msg::Pong { t, host }) => {
                let now = now_us();
                let rtt = now.saturating_sub(t);
                sh.rtt_us.store(rtt, Ordering::Relaxed);
                sh.clock.lock().unwrap().add(rtt, host as i64 + (rtt / 2) as i64 - now as i64);
            }
            Ok(Msg::Error { msg }) => break Err(msg),
            Ok(Msg::Bye) => break Err(format!("{host_name} stopped sharing.")),
            Ok(_) => {}
            Err(e) => break if sh.closed.load(Ordering::Relaxed) { Ok(()) } else { Err(io_msg(e, &host_name)) },
        }
    };
    sh.stop.store(true, Ordering::Relaxed);
    if let Some(t) = media_thread {
        let _ = t.join();
    }
    result
}

fn media_main(sh: &Shared, udp: UdpSocket, host: SocketAddr, media: Sealer, punch: Sealer) {
    let mut asm = Assembler::default();
    let mut last_done: Option<u32> = None;
    let mut need_key = true;
    let mut last_key_req = Instant::now() - Duration::from_secs(1);
    let (mut last_ping, mut last_punch) = (Instant::now() - Duration::from_secs(1), Instant::now() - Duration::from_secs(1));
    let mut punch_seq = 0u64;
    let mut buf = vec![0u8; 2048];
    let mut samples: Vec<i16> = Vec::with_capacity(1024);
    while !sh.stop.load(Ordering::Relaxed) {
        if last_punch.elapsed() > Duration::from_millis(if last_done.is_some() { 1000 } else { 100 }) {
            last_punch = Instant::now();
            let mut p = vec![PKT_PUNCH];
            p.extend_from_slice(&punch_seq.to_le_bytes());
            p.extend_from_slice(b"nexmirror");
            punch.seal_tail(punch_seq, &mut p, 9);
            punch_seq += 1;
            let _ = udp.send_to(&p, host);
        }
        if last_ping.elapsed() > Duration::from_millis(500) {
            last_ping = Instant::now();
            sh.send(&Msg::Ping { t: now_us() });
        }
        if sh.want_key.swap(false, Ordering::Relaxed) {
            need_key = true;
        }
        if let Some(stale) = asm.stale(Duration::from_millis(80)) {
            // A frame never completed (packets lost beyond FEC capacity).
            asm.discard_upto(stale);
            if last_done.map_or(true, |d| stale > d) {
                sh.lost.fetch_add(1, Ordering::Relaxed);
                last_done = Some(stale);
                need_key = true;
            }
        }
        if need_key && last_key_req.elapsed() > Duration::from_millis(150) {
            last_key_req = Instant::now();
            sh.send(&Msg::RequestKeyframe);
        }
        let Ok((n, src)) = udp.recv_from(&mut buf) else { continue };
        if src.ip() != host.ip() || n < 9 {
            continue;
        }
        sh.bytes.fetch_add(n as u64, Ordering::Relaxed);
        match buf[0] {
            PKT_VIDEO => {
                let Some(h) = VideoHdr::read(&buf[..n]) else { continue };
                let Some(len) = media.open_tail(h.seq, &mut buf[..n], VIDEO_HDR) else { continue };
                let before = asm.recovered_shards;
                let done = asm.push(&h, &buf[VIDEO_HDR..VIDEO_HDR + len]);
                if asm.recovered_shards != before {
                    sh.repaired.store(asm.recovered_shards, Ordering::Relaxed);
                }
                let Some(c) = done else { continue };
                if last_done.is_some_and(|d| c.id <= d) {
                    continue;
                }
                if let Some(d) = last_done {
                    if c.id != d + 1 {
                        sh.lost.fetch_add((c.id - d - 1) as u64, Ordering::Relaxed);
                        need_key = true;
                    }
                }
                last_done = Some(c.id);
                asm.discard_upto(c.id);
                if need_key && !c.key {
                    continue; // A reference frame is missing; wait for the keyframe instead of showing corruption.
                }
                need_key = false;
                sh.frames.lock().unwrap().push_back(c);
                unsafe {
                    let _ = SetEvent(sh.event.0);
                }
            }
            PKT_AUDIO => {
                let Some(seq) = read_seq(&buf[..n]) else { continue };
                let Some(len) = media.open_tail(seq, &mut buf[..n], AUDIO_HDR) else { continue };
                samples.clear();
                samples.extend(buf[AUDIO_HDR..AUDIO_HDR + len].chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])));
                if let Some(p) = sh.player.lock().unwrap().as_ref() {
                    p.push(&samples);
                }
            }
            _ => {}
        }
    }
}
