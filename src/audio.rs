//! Audio: WASAPI loopback capture on the host, WASAPI playback on the viewer.
//! Audio travels as 16-bit stereo PCM in 5 ms packets (~1.5 Mbit/s); there is no codec
//! delay, and the playback queue is kept short (~20 ms) by dropping excess samples.

use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use windows::{
    core::*,
    Win32::{
        Foundation::*,
        Media::{Audio::*, KernelStreaming::WAVE_FORMAT_EXTENSIBLE, Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT}},
        System::{Com::*, Threading::*},
    },
};

pub const CHANNELS: usize = 2;

fn default_device() -> Result<(IMMDevice, String)> {
    unsafe {
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let dev = en.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let id = dev.GetId()?;
        let s = id.to_string().unwrap_or_default();
        CoTaskMemFree(Some(id.0 as _));
        Ok((dev, s))
    }
}

/// Captures what the PC is playing. Calls `sink(samples, rate)` with interleaved stereo i16
/// in 5 ms chunks. Re-opens automatically when the default output device changes.
pub fn run_loopback(stop: impl Fn() -> bool, mut sink: impl FnMut(&[i16], u32)) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    while !stop() {
        if let Err(e) = loopback_once(&stop, &mut sink) {
            crate::log!("audio capture: {e}");
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

fn loopback_once(stop: &impl Fn() -> bool, sink: &mut impl FnMut(&[i16], u32)) -> Result<()> {
    unsafe {
        let (dev, dev_id) = default_device()?;
        let client: IAudioClient = dev.Activate(CLSCTX_ALL, None)?;
        let fmt = client.GetMixFormat()?;
        let f = &*fmt;
        let (channels, rate, bits) = (f.nChannels as usize, f.nSamplesPerSec, f.wBitsPerSample);
        let is_float = f.wFormatTag as u32 == WAVE_FORMAT_IEEE_FLOAT
            || (f.wFormatTag as u32 == WAVE_FORMAT_EXTENSIBLE && {
                let ext = fmt as *const WAVEFORMATEXTENSIBLE;
                std::ptr::addr_of!((*ext).SubFormat).read_unaligned() == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
            });
        let init = client.Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, 200_000, 0, fmt, None);
        CoTaskMemFree(Some(fmt as _));
        init?;
        let cap: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        crate::log!("audio capture: {rate} Hz, {channels} ch, {bits} bit{}", if is_float { " float" } else { "" });
        let chunk = (rate as usize / 200) * CHANNELS;
        let mut acc: Vec<i16> = Vec::with_capacity(chunk * 4);
        let mut last_check = Instant::now();
        while !stop() {
            std::thread::sleep(Duration::from_millis(2));
            loop {
                let n = cap.GetNextPacketSize()?;
                if n == 0 {
                    break;
                }
                let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
                cap.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                for i in 0..frames as usize {
                    for c in 0..CHANNELS {
                        let src = c.min(channels - 1);
                        let idx = i * channels + src;
                        let v = if silent {
                            0.0
                        } else if is_float && bits == 32 {
                            (*(data as *const f32).add(idx)).clamp(-1.0, 1.0) * 32767.0
                        } else if bits == 16 {
                            *(data as *const i16).add(idx) as f32
                        } else if bits == 32 {
                            (*(data as *const i32).add(idx) >> 16) as f32
                        } else {
                            0.0
                        } as i16;
                        acc.push(v);
                    }
                }
                cap.ReleaseBuffer(frames)?;
            }
            let mut off = 0;
            while acc.len() - off >= chunk {
                sink(&acc[off..off + chunk], rate);
                off += chunk;
            }
            acc.drain(..off);
            if last_check.elapsed() > Duration::from_secs(2) {
                last_check = Instant::now();
                if default_device().map(|d| d.1 != dev_id).unwrap_or(false) {
                    crate::log!("audio capture: default device changed");
                    let _ = client.Stop();
                    return Ok(());
                }
            }
        }
        let _ = client.Stop();
        Ok(())
    }
}

/// Viewer-side playback. Push samples with `push`; a thread feeds the default output device.
pub struct Player {
    queue: Arc<Mutex<VecDeque<i16>>>,
    stop: Arc<AtomicBool>,
    pub volume: Arc<AtomicU32>,
    thread: Option<std::thread::JoinHandle<()>>,
    pub rate: u32,
}

impl Player {
    pub fn start(rate: u32, volume: f32) -> Self {
        let queue = Arc::new(Mutex::new(VecDeque::with_capacity(rate as usize)));
        let stop = Arc::new(AtomicBool::new(false));
        let vol = Arc::new(AtomicU32::new(volume.to_bits()));
        let (q, s, v) = (queue.clone(), stop.clone(), vol.clone());
        let thread = std::thread::Builder::new()
            .name("audio-out".into())
            .spawn(move || {
                unsafe {
                    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                }
                while !s.load(Ordering::Relaxed) {
                    if let Err(e) = play(rate, &q, &s, &v) {
                        crate::log!("audio playback: {e}");
                        std::thread::sleep(Duration::from_secs(1));
                    }
                }
            })
            .ok();
        Self { queue, stop, volume: vol, thread, rate }
    }

    pub fn push(&self, samples: &[i16]) {
        let mut q = self.queue.lock().unwrap();
        q.extend(samples);
        // Never let latency build up: keep at most ~80 ms queued.
        let max = self.rate as usize * CHANNELS * 80 / 1000;
        if q.len() > max {
            let target = self.rate as usize * CHANNELS * 20 / 1000;
            let drop = q.len() - target;
            q.drain(..drop);
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn play(rate: u32, q: &Mutex<VecDeque<i16>>, stop: &AtomicBool, vol: &AtomicU32) -> Result<()> {
    unsafe {
        let (dev, dev_id) = default_device()?;
        let client: IAudioClient = dev.Activate(CLSCTX_ALL, None)?;
        let fmt = WAVEFORMATEX {
            wFormatTag: 1, // WAVE_FORMAT_PCM
            nChannels: CHANNELS as u16,
            nSamplesPerSec: rate,
            nAvgBytesPerSec: rate * CHANNELS as u32 * 2,
            nBlockAlign: CHANNELS as u16 * 2,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            200_000,
            0,
            &fmt,
            None,
        )?;
        let event = CreateEventW(None, false, false, None)?;
        client.SetEventHandle(event)?;
        let render: IAudioRenderClient = client.GetService()?;
        let size = client.GetBufferSize()?;
        client.Start()?;
        let prebuffer = rate as usize * CHANNELS * 20 / 1000;
        let mut buffering = true;
        let mut last_check = Instant::now();
        let result = loop {
            if stop.load(Ordering::Relaxed) {
                break Ok(());
            }
            WaitForSingleObject(event, 100);
            let padding = match client.GetCurrentPadding() {
                Ok(p) => p,
                Err(e) => break Err(e),
            };
            let avail = (size - padding) as usize;
            if avail == 0 {
                continue;
            }
            let data = match render.GetBuffer(avail as u32) {
                Ok(d) => d as *mut i16,
                Err(e) => break Err(e),
            };
            let out = std::slice::from_raw_parts_mut(data, avail * CHANNELS);
            let gain = f32::from_bits(vol.load(Ordering::Relaxed));
            {
                let mut q = q.lock().unwrap();
                if buffering && q.len() >= prebuffer {
                    buffering = false;
                }
                let n = if buffering { 0 } else { q.len().min(out.len()) };
                for (o, s) in out.iter_mut().zip(q.drain(..n)) {
                    *o = (s as f32 * gain) as i16;
                }
                out[n..].fill(0);
                if !buffering && n < out.len() {
                    buffering = true;
                }
            }
            let _ = render.ReleaseBuffer(avail as u32, 0);
            if last_check.elapsed() > Duration::from_secs(2) {
                last_check = Instant::now();
                if default_device().map(|d| d.1 != dev_id).unwrap_or(false) {
                    break Ok(());
                }
            }
        };
        let _ = client.Stop();
        let _ = CloseHandle(event);
        result
    }
}
