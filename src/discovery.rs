//! LAN discovery: viewers broadcast a probe; hosts that are sharing answer with their name.

use crate::proto::{Announce, PKT_ANNOUNCE, PKT_PROBE, PORT, PROBE_MAGIC, VERSION};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

/// IPv4 addresses of active, non-loopback adapters with their prefix length.
pub fn local_ipv4s() -> Vec<(Ipv4Addr, u8)> {
    use windows::Win32::{
        Foundation::ERROR_BUFFER_OVERFLOW,
        NetworkManagement::{IpHelper::*, Ndis::IfOperStatusUp},
        Networking::WinSock::{AF_INET, SOCKADDR_IN},
    };
    let mut out = Vec::new();
    unsafe {
        let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
        let mut size = 16 * 1024u32;
        let mut buf: Vec<u64> = vec![0; size as usize / 8];
        let mut r = GetAdaptersAddresses(AF_INET.0 as u32, flags, None, Some(buf.as_mut_ptr() as _), &mut size);
        if r == ERROR_BUFFER_OVERFLOW.0 {
            buf = vec![0; size as usize / 8 + 1];
            r = GetAdaptersAddresses(AF_INET.0 as u32, flags, None, Some(buf.as_mut_ptr() as _), &mut size);
        }
        if r != 0 {
            return out;
        }
        let mut a = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
        while let Some(ad) = a.as_ref() {
            if ad.OperStatus == IfOperStatusUp && ad.IfType != 24 {
                let mut u = ad.FirstUnicastAddress;
                while let Some(ua) = u.as_ref() {
                    let sa = ua.Address.lpSockaddr;
                    if !sa.is_null() && (*sa).sa_family == AF_INET {
                        let sin = &*(sa as *const SOCKADDR_IN);
                        out.push((Ipv4Addr::from(u32::from_be(sin.sin_addr.S_un.S_addr)), ua.OnLinkPrefixLength));
                    }
                    u = ua.Next;
                }
            }
            a = ad.Next;
        }
    }
    out
}

#[derive(Clone)]
pub struct Found {
    pub id: String,
    pub name: String,
    pub addr: IpAddr,
    pub busy: bool,
    last_seen: Instant,
}

pub struct Discovery {
    list: Arc<Mutex<Vec<Found>>>,
    stop: Arc<AtomicBool>,
    kick: Arc<AtomicBool>,
}

impl Discovery {
    pub fn start() -> Self {
        let list = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let kick = Arc::new(AtomicBool::new(true));
        let (l, s, k) = (list.clone(), stop.clone(), kick.clone());
        std::thread::Builder::new().name("discovery".into()).spawn(move || run(l, s, k)).ok();
        Self { list, stop, kick }
    }

    pub fn hosts(&self) -> Vec<Found> {
        let mut v: Vec<Found> = self.list.lock().unwrap().iter().filter(|f| f.last_seen.elapsed() < Duration::from_secs(6)).cloned().collect();
        v.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        v
    }

    pub fn refresh(&self) {
        self.list.lock().unwrap().clear();
        self.kick.store(true, Ordering::Relaxed);
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn run(list: Arc<Mutex<Vec<Found>>>, stop: Arc<AtomicBool>, kick: Arc<AtomicBool>) {
    let Ok(sock) = UdpSocket::bind("0.0.0.0:0") else { return };
    let _ = sock.set_broadcast(true);
    let _ = sock.set_read_timeout(Some(Duration::from_millis(200)));
    let mut probe = vec![PKT_PROBE];
    probe.extend_from_slice(PROBE_MAGIC);
    probe.extend_from_slice(&VERSION.to_le_bytes());
    let mut last = Instant::now() - Duration::from_secs(10);
    let mut buf = [0u8; 2048];
    while !stop.load(Ordering::Relaxed) {
        if kick.swap(false, Ordering::Relaxed) || last.elapsed() > Duration::from_millis(1500) {
            last = Instant::now();
            let mut targets = vec![Ipv4Addr::BROADCAST, Ipv4Addr::LOCALHOST];
            for (ip, prefix) in local_ipv4s() {
                if prefix > 0 && prefix < 32 {
                    let mask = u32::MAX << (32 - prefix);
                    targets.push(Ipv4Addr::from(u32::from(ip) | !mask));
                }
            }
            targets.dedup();
            for t in targets {
                let _ = sock.send_to(&probe, SocketAddr::new(IpAddr::V4(t), PORT));
            }
        }
        if let Ok((n, src)) = sock.recv_from(&mut buf) {
            if n > 1 && buf[0] == PKT_ANNOUNCE {
                if let Ok(a) = serde_json::from_slice::<Announce>(&buf[1..n]) {
                    if a.version != VERSION {
                        continue;
                    }
                    let mut l = list.lock().unwrap();
                    // Prefer a LAN address over loopback when the same host answers on both.
                    let addr = match l.iter().find(|f| f.id == a.id) {
                        Some(f) if src.ip().is_loopback() && !f.addr.is_loopback() => f.addr,
                        _ => src.ip(),
                    };
                    l.retain(|f| f.id != a.id);
                    l.push(Found { id: a.id, name: a.name, addr, busy: a.busy, last_seen: Instant::now() });
                }
            }
        }
    }
}
