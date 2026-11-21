//! Minimal file logger: %APPDATA%\NexMirror\logs\nexmirror.log (previous run kept as .old.log).

use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::Path,
    sync::Mutex,
};

static FILE: Mutex<Option<File>> = Mutex::new(None);

pub fn init(dir: &Path) {
    let dir = dir.join("logs");
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("nexmirror.log");
    if fs::metadata(&path).map(|m| m.len() > 0).unwrap_or(false) {
        let _ = fs::rename(&path, dir.join("nexmirror.old.log"));
    }
    *FILE.lock().unwrap() = OpenOptions::new().create(true).append(true).open(path).ok();
}

pub fn write(msg: &str) {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    let line = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}  {}\n",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds, msg
    );
    #[cfg(debug_assertions)]
    eprint!("{line}");
    if let Some(f) = FILE.lock().unwrap().as_mut() {
        let _ = f.write_all(line.as_bytes());
    }
}

#[macro_export]
macro_rules! log {
    ($($a:tt)*) => { $crate::logging::write(&format!($($a)*)) };
}
