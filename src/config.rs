//! Persistent settings and pairing secrets (%APPDATA%\NexMirror\config.json).
//! Secrets are protected with Windows DPAPI so they are bound to the current user account.

use crate::crypto;
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub enum Resolution {
    Auto,
    Native,
    P1440,
    P1080,
    P720,
}

impl Resolution {
    pub const ALL: [Resolution; 5] = [Self::Auto, Self::Native, Self::P1440, Self::P1080, Self::P720];
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Match viewer screen",
            Self::Native => "Native",
            Self::P1440 => "1440p",
            Self::P1080 => "1080p",
            Self::P720 => "720p",
        }
    }
    pub fn max_height(self) -> Option<u32> {
        match self {
            Self::P1440 => Some(1440),
            Self::P1080 => Some(1080),
            Self::P720 => Some(720),
            _ => None,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HostSettings {
    /// DXGI output device name (e.g. `\\.\DISPLAY1`). Empty = primary display.
    pub display: String,
    pub resolution: Resolution,
    pub fps: u32,
    /// 0 = automatic (scaled from resolution and frame rate).
    pub bitrate_mbps: u32,
    pub audio: bool,
}

impl Default for HostSettings {
    fn default() -> Self {
        Self { display: String::new(), resolution: Resolution::Auto, fps: 60, bitrate_mbps: 0, audio: true }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ViewerSettings {
    pub audio: bool,
    pub volume: f32,
    pub show_stats: bool,
}

impl Default for ViewerSettings {
    fn default() -> Self {
        Self { audio: true, volume: 1.0, show_stats: false }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PairedHost {
    pub id: String,
    pub name: String,
    pub secret: String,
    pub last_addr: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct TrustedViewer {
    pub id: String,
    pub name: String,
    pub secret: String,
    pub paired_at: u64,
}

#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub device_id: String,
    pub device_name: String,
    pub host: HostSettings,
    pub viewer: ViewerSettings,
    pub paired_hosts: Vec<PairedHost>,
    pub trusted_viewers: Vec<TrustedViewer>,
    pub recent_addrs: Vec<String>,
}

pub type SharedConfig = Arc<Mutex<Config>>;

impl Config {
    pub fn dir() -> PathBuf {
        let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        base.join("NexMirror")
    }

    fn path() -> PathBuf {
        Self::dir().join("config.json")
    }

    pub fn load() -> Config {
        let mut c: Config = std::fs::read(Self::path())
            .ok()
            .and_then(|b| serde_json::from_slice(b.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&b)).ok())
            .unwrap_or_default();
        let mut dirty = false;
        if c.device_id.is_empty() {
            c.device_id = crypto::hex(&crypto::random_bytes(16));
            dirty = true;
        }
        if c.device_name.trim().is_empty() {
            c.device_name = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "Windows PC".into());
            dirty = true;
        }
        if dirty {
            c.save();
        }
        c
    }

    pub fn save(&self) {
        let dir = Self::dir();
        let _ = std::fs::create_dir_all(&dir);
        let tmp = dir.join("config.json.tmp");
        if let Ok(json) = serde_json::to_vec_pretty(self) {
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(&tmp, Self::path());
            }
        }
    }

    fn seal(secret: &[u8]) -> String {
        crypto::hex(&crypto::protect(secret))
    }

    fn unseal(s: &str) -> Option<Vec<u8>> {
        crypto::unprotect(&crypto::unhex(s)?)
    }

    pub fn host_secret(&self, host_id: &str) -> Option<Vec<u8>> {
        self.paired_hosts.iter().find(|h| h.id == host_id).and_then(|h| Self::unseal(&h.secret))
    }

    pub fn viewer_secret(&self, viewer_id: &str) -> Option<Vec<u8>> {
        self.trusted_viewers.iter().find(|v| v.id == viewer_id).and_then(|v| Self::unseal(&v.secret))
    }

    pub fn set_paired_host(&mut self, id: &str, name: &str, secret: &[u8], addr: &str) {
        self.paired_hosts.retain(|h| h.id != id);
        self.paired_hosts.push(PairedHost { id: id.into(), name: name.into(), secret: Self::seal(secret), last_addr: addr.into() });
    }

    pub fn set_trusted_viewer(&mut self, id: &str, name: &str, secret: &[u8]) {
        self.trusted_viewers.retain(|v| v.id != id);
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        self.trusted_viewers.push(TrustedViewer { id: id.into(), name: name.into(), secret: Self::seal(secret), paired_at: now });
    }

    pub fn remember_addr(&mut self, addr: &str) {
        self.recent_addrs.retain(|a| a != addr);
        self.recent_addrs.insert(0, addr.into());
        self.recent_addrs.truncate(5);
    }
}
