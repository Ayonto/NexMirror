//! H.264 decoding with the Windows decoder MFT using DXVA hardware acceleration.
//! Low-latency mode makes it emit each frame as soon as it is decoded (no reorder buffering).
//! Output stays on the GPU and is copied into the renderer's NV12 texture.

use crate::gfx::VideoTex;
use std::mem::ManuallyDrop;
use windows::{
    core::*,
    Win32::{
        Graphics::Direct3D11::*,
        Media::MediaFoundation::*,
        System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER},
    },
};

pub struct Decoder {
    mft: IMFTransform,
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    _mgr: Option<IMFDXGIDeviceManager>,
    pub out: Option<VideoTex>,
    disp: (u32, u32),
    pub hardware: bool,
}

fn unpack(v: u64) -> (u32, u32) {
    ((v >> 32) as u32, v as u32)
}

impl Decoder {
    pub fn new(device: &ID3D11Device) -> Result<Self> {
        unsafe {
            let mft: IMFTransform = CoCreateInstance(&CLSID_MSH264DecoderMFT, None, CLSCTX_INPROC_SERVER)?;
            let attrs = mft.GetAttributes()?;
            let mut mgr = None;
            let mut hardware = false;
            if attrs.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) != 0 {
                let mut token = 0u32;
                let mut m = None;
                MFCreateDXGIDeviceManager(&mut token, &mut m)?;
                let m = m.unwrap();
                m.ResetDevice(device, token)?;
                if mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, m.as_raw() as usize).is_ok() {
                    hardware = true;
                    mgr = Some(m);
                }
            }
            attrs.SetUINT32(&MF_LOW_LATENCY, 1).ok();
            if let Ok(codec) = mft.cast::<ICodecAPI>() {
                let _ = codec.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true));
            }
            let t = MFCreateMediaType()?;
            t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            mft.SetInputType(0, &t, 0)?;
            let mut d = Self {
                mft,
                device: device.clone(),
                ctx: device.GetImmediateContext()?,
                _mgr: mgr,
                out: None,
                disp: (0, 0),
                hardware,
            };
            d.set_output_type()?;
            d.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            d.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            crate::log!("decoder ready (hardware: {hardware})");
            Ok(d)
        }
    }

    fn set_output_type(&mut self) -> Result<()> {
        unsafe {
            let mut i = 0;
            while let Ok(t) = self.mft.GetOutputAvailableType(0, i) {
                if t.GetGUID(&MF_MT_SUBTYPE)? == MFVideoFormat_NV12 {
                    self.mft.SetOutputType(0, &t, 0)?;
                    if let Ok(fs) = t.GetUINT64(&MF_MT_FRAME_SIZE) {
                        self.disp = unpack(fs);
                    }
                    let mut ap = MFVideoArea::default();
                    if t.GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, std::slice::from_raw_parts_mut(&mut ap as *mut _ as *mut u8, std::mem::size_of::<MFVideoArea>()), None).is_ok() {
                        if ap.Area.cx > 0 && ap.Area.cy > 0 {
                            self.disp = (ap.Area.cx as u32, ap.Area.cy as u32);
                        }
                    }
                    return Ok(());
                }
                i += 1;
            }
        }
        Err(Error::new(MF_E_INVALIDMEDIATYPE, "decoder offers no NV12 output"))
    }

    /// Feeds one access unit; returns true if a new picture landed in `out`.
    pub fn decode(&mut self, data: &[u8]) -> Result<bool> {
        unsafe {
            let buf = MFCreateMemoryBuffer(data.len() as u32)?;
            let mut p = std::ptr::null_mut();
            buf.Lock(&mut p, None, None)?;
            std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
            buf.Unlock()?;
            buf.SetCurrentLength(data.len() as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buf)?;
            let mut got = false;
            if let Err(e) = self.mft.ProcessInput(0, &sample, 0) {
                if e.code() != MF_E_NOTACCEPTING {
                    return Err(e);
                }
                got |= self.drain()?;
                self.mft.ProcessInput(0, &sample, 0)?;
            }
            got |= self.drain()?;
            Ok(got)
        }
    }

    fn drain(&mut self) -> Result<bool> {
        let mut got = false;
        loop {
            unsafe {
                let info = self.mft.GetOutputStreamInfo(0)?;
                let provides = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32) != 0;
                if !provides {
                    return Err(Error::new(MF_E_UNSUPPORTED_D3D_TYPE, "hardware video decoding is unavailable"));
                }
                let mut bufs = [MFT_OUTPUT_DATA_BUFFER { dwStreamID: 0, pSample: ManuallyDrop::new(None), dwStatus: 0, pEvents: ManuallyDrop::new(None) }];
                let mut status = 0;
                let r = self.mft.ProcessOutput(0, &mut bufs, &mut status);
                let sample = ManuallyDrop::take(&mut bufs[0].pSample);
                drop(ManuallyDrop::take(&mut bufs[0].pEvents));
                match r {
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(got),
                    Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                        self.set_output_type()?;
                        continue;
                    }
                    Err(e) => return Err(e),
                    Ok(()) => {}
                }
                let Some(sample) = sample else { continue };
                let buf = sample.GetBufferByIndex(0)?;
                let dxgi: IMFDXGIBuffer = buf.cast()?;
                let mut raw = std::ptr::null_mut();
                dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw)?;
                let tex = ID3D11Texture2D::from_raw(raw);
                let idx = dxgi.GetSubresourceIndex()?;
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                tex.GetDesc(&mut desc);
                let fits = self.out.as_ref().is_some_and(|o| o.width == desc.Width && o.height == desc.Height);
                if !fits {
                    self.out = Some(VideoTex::new(&self.device, desc.Width, desc.Height)?);
                }
                let out = self.out.as_mut().unwrap();
                out.disp_w = self.disp.0.clamp(1, desc.Width);
                out.disp_h = self.disp.1.clamp(1, desc.Height);
                self.ctx.CopySubresourceRegion(&out.tex, 0, 0, 0, 0, &tex, idx, None);
                got = true;
            }
        }
    }
}
