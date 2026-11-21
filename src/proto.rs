//! Wire protocol.
//!
//! * TCP (control): `[u32 len][u8 encrypted?][payload]`, payload is JSON `Msg`.
//!   After the key exchange every frame is AES-GCM sealed with an implicit per-direction counter.
//! * UDP (media): `[u8 kind][header...][sealed payload]`, header is the AEAD associated data.

use crate::crypto::Sealer;
use serde::{Deserialize, Serialize};
use std::{
    io::{self, Read, Write},
    net::TcpStream,
    sync::OnceLock,
    time::Instant,
};

pub const PORT: u16 = 47800;
pub const VERSION: u32 = 1;

pub const PKT_VIDEO: u8 = 1;
pub const PKT_AUDIO: u8 = 2;
pub const PKT_PUNCH: u8 = 3;
pub const PKT_PROBE: u8 = 4;
pub const PKT_ANNOUNCE: u8 = 5;
pub const PROBE_MAGIC: &[u8; 4] = b"NXMR";

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct StreamInfo {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub encoder: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum Msg {
    Hello { version: u32, id: String, name: String, screen_w: u32, screen_h: u32 },
    HelloAck { version: u32, id: String, name: String, trusted: bool },
    /// Viewer -> host: `pin == true` asks the host to display a pairing code.
    Auth { pin: bool },
    Spake { msg: Vec<u8> },
    Confirm,
    Paired { secret: Vec<u8> },
    Ready,
    Stream(StreamInfo),
    /// Host audio sample rate; 0 means audio sharing is off.
    Audio { rate: u32 },
    RequestKeyframe,
    Ping { t: u64 },
    Pong { t: u64, host: u64 },
    Error { msg: String },
    Bye,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Announce {
    pub id: String,
    pub name: String,
    pub version: u32,
    pub busy: bool,
}

/// Microseconds on a process-local monotonic clock.
pub fn now_us() -> u64 {
    static T0: OnceLock<Instant> = OnceLock::new();
    T0.get_or_init(Instant::now).elapsed().as_micros() as u64
}

const MAX_FRAME: usize = 1 << 20;

pub struct Tx {
    s: TcpStream,
    seal: Option<Sealer>,
    seq: u64,
}

pub struct Rx {
    s: TcpStream,
    open: Option<Sealer>,
    seq: u64,
}

pub fn channel(s: TcpStream) -> io::Result<(Tx, Rx)> {
    s.set_nodelay(true)?;
    let r = s.try_clone()?;
    Ok((Tx { s, seal: None, seq: 0 }, Rx { s: r, open: None, seq: 0 }))
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

impl Tx {
    pub fn enable_crypto(&mut self, key: &[u8; 32]) {
        self.seal = Some(Sealer::new(key));
    }

    pub fn send(&mut self, m: &Msg) -> io::Result<()> {
        let mut body = serde_json::to_vec(m).map_err(|_| bad("encode"))?;
        let flag = match &self.seal {
            Some(s) => {
                s.seal(self.seq, &[], &mut body);
                self.seq += 1;
                1u8
            }
            None => 0u8,
        };
        self.write_frame(flag, &body)
    }

    /// Sends an unencrypted message even after crypto is enabled (used to report auth failures).
    pub fn send_plain(&mut self, m: &Msg) -> io::Result<()> {
        let body = serde_json::to_vec(m).map_err(|_| bad("encode"))?;
        self.write_frame(0, &body)
    }

    fn write_frame(&mut self, flag: u8, body: &[u8]) -> io::Result<()> {
        let mut f = Vec::with_capacity(body.len() + 5);
        f.extend_from_slice(&(body.len() as u32 + 1).to_le_bytes());
        f.push(flag);
        f.extend_from_slice(body);
        self.s.write_all(&f)
    }

    pub fn shutdown(&self) {
        let _ = self.s.shutdown(std::net::Shutdown::Both);
    }
}

impl Rx {
    pub fn enable_crypto(&mut self, key: &[u8; 32]) {
        self.open = Some(Sealer::new(key));
    }

    pub fn set_timeout(&self, d: Option<std::time::Duration>) {
        let _ = self.s.set_read_timeout(d);
    }

    pub fn recv(&mut self) -> io::Result<Msg> {
        let mut len = [0u8; 4];
        self.s.read_exact(&mut len)?;
        let len = u32::from_le_bytes(len) as usize;
        if len == 0 || len > MAX_FRAME {
            return Err(bad("frame size"));
        }
        let mut buf = vec![0u8; len];
        self.s.read_exact(&mut buf)?;
        let flag = buf[0];
        let body = &mut buf[1..];
        let plain: &[u8] = match (flag, &self.open) {
            (1, Some(o)) => {
                let n = o.open(self.seq, &[], body).ok_or_else(|| bad("authentication failed"))?;
                self.seq += 1;
                &body[..n]
            }
            (0, Some(_)) => {
                // Only error reports may arrive in the clear once the session is encrypted.
                return match serde_json::from_slice::<Msg>(body) {
                    Ok(m @ Msg::Error { .. }) => Ok(m),
                    _ => Err(bad("unexpected plaintext")),
                };
            }
            (0, None) => body,
            _ => return Err(bad("unexpected encrypted frame")),
        };
        serde_json::from_slice(plain).map_err(|_| bad("decode"))
    }
}

// ---------------------------------------------------------------- media packets

pub const VIDEO_HDR: usize = 36;
pub const AUDIO_HDR: usize = 17;

pub const FLAG_KEY: u8 = 1;

#[derive(Clone, Copy, Debug, Default)]
pub struct VideoHdr {
    pub seq: u64,
    pub frame: u32,
    pub block: u16,
    pub nblocks: u16,
    pub shard: u16,
    pub k: u16,
    pub m: u16,
    pub frame_len: u32,
    pub ts: u64,
    pub flags: u8,
}

impl VideoHdr {
    pub fn write(&self, b: &mut Vec<u8>) {
        b.push(PKT_VIDEO);
        b.extend_from_slice(&self.seq.to_le_bytes());
        b.extend_from_slice(&self.frame.to_le_bytes());
        for v in [self.block, self.nblocks, self.shard, self.k, self.m] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&self.frame_len.to_le_bytes());
        b.extend_from_slice(&self.ts.to_le_bytes());
        b.push(self.flags);
    }

    pub fn read(b: &[u8]) -> Option<Self> {
        if b.len() < VIDEO_HDR || b[0] != PKT_VIDEO {
            return None;
        }
        let u16_at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
        Some(Self {
            seq: u64::from_le_bytes(b[1..9].try_into().ok()?),
            frame: u32::from_le_bytes(b[9..13].try_into().ok()?),
            block: u16_at(13),
            nblocks: u16_at(15),
            shard: u16_at(17),
            k: u16_at(19),
            m: u16_at(21),
            frame_len: u32::from_le_bytes(b[23..27].try_into().ok()?),
            ts: u64::from_le_bytes(b[27..35].try_into().ok()?),
            flags: b[35],
        })
    }
}

pub fn audio_hdr(seq: u64, ts: u64, b: &mut Vec<u8>) {
    b.push(PKT_AUDIO);
    b.extend_from_slice(&seq.to_le_bytes());
    b.extend_from_slice(&ts.to_le_bytes());
}

pub fn read_seq(b: &[u8]) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(1..9)?.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_hdr_roundtrip() {
        let h = VideoHdr { seq: 9, frame: 7, block: 1, nblocks: 3, shard: 5, k: 40, m: 8, frame_len: 123456, ts: 99, flags: 1 };
        let mut b = Vec::new();
        h.write(&mut b);
        assert_eq!(b.len(), VIDEO_HDR);
        let r = VideoHdr::read(&b).unwrap();
        assert_eq!((r.seq, r.frame, r.block, r.nblocks, r.shard, r.k, r.m, r.frame_len, r.ts, r.flags), (9, 7, 1, 3, 5, 40, 8, 123456, 99, 1));
    }
}
