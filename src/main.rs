//! NexMirror — low-latency screen mirroring between Windows PCs on a local network.
#![windows_subsystem = "windows"]

mod app;
mod audio;
mod capture;
mod config;
mod crypto;
mod decoder;
mod discovery;
mod encoder;
mod fec;
mod gfx;
mod host;
mod logging;
mod logo;
mod proto;
mod theme;
mod viewer;
mod widgets;
mod window;

use std::sync::{Arc, Mutex};
use windows::Win32::{
    Media::{timeBeginPeriod, MediaFoundation::*},
    System::{
        Com::{CoInitializeEx, COINIT_MULTITHREADED},
        Power::{SetThreadExecutionState, ES_CONTINUOUS, ES_SYSTEM_REQUIRED},
    },
    UI::{HiDpi::*, WindowsAndMessaging::*},
};

fn main() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        timeBeginPeriod(1);
        // Keep the PC awake while NexMirror runs; it may be streaming with nobody touching it.
        SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED);
    }
    let cfg = config::Config::load();
    logging::init(&config::Config::dir());
    log!("NexMirror {} starting on {}", app::VERSION, cfg.device_name);
    std::panic::set_hook(Box::new(|info| log!("panic: {info}")));

    if let Err(e) = unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) } {
        log!("MFStartup failed: {e}");
    }
    let cfg = Arc::new(Mutex::new(cfg));
    if let Err(e) = window::run(|ctx| app::App::new(ctx, cfg.clone())) {
        log!("fatal: {e}");
        let msg = windows::core::HSTRING::from(format!("NexMirror could not start.\n\n{}", e.message()));
        unsafe {
            MessageBoxW(None, &msg, windows::core::w!("NexMirror"), MB_ICONERROR | MB_OK);
        }
    }
    unsafe {
        let _ = MFShutdown();
    }
}
