//! Hardware H.264 encoder (NVENC / AMF / Quick Sync via their Media Foundation transforms).
//!
//! Tuned for latency: low-latency mode, no B-frames, CBR with a ~2 frame VBV,
//! no periodic keyframes (keyframes only on demand), one frame in flight.

use std::{
    mem::ManuallyDrop,
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    time::{Duration, Instant},
};
use windows::{
    core::*,
    Win32::{
        Foundation::*,
        Graphics::Direct3D11::*,
        Media::MediaFoundation::*,
        System::Com::CoTaskMemFree,
    },
};

struct SendPtr<T>(T);
unsafe impl<T> Send for SendPtr<T> {}

pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub key: bool,
    pub ts: u64,
}

pub struct Encoder {
    mft: IMFTransform,
    codec: ICodecAPI,
    _mgr: IMFDXGIDeviceManager,
    events: Receiver<u32>,
    in_id: u32,
    out_id: u32,
    need: u32,
    frame_no: i64,
    fps: u32,
    seq_header: Vec<u8>,
    pending_ts: std::collections::VecDeque<u64>,
    pub name: String,
}

fn pack(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

fn has_sps(data: &[u8]) -> bool {
    let mut i = 0;
    while i + 4 < data.len().min(512) {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if data[i + 3] & 0x1f == 7 {
                return true;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}

impl Encoder {
    pub fn new(device: &ID3D11Device, luid: LUID, w: u32, h: u32, fps: u32, bitrate: u32) -> Result<Self> {
        let (mft, name) = Self::find(luid)?;
        unsafe {
            let attrs = mft.GetAttributes()?;
            attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
            attrs.SetUINT32(&MF_LOW_LATENCY, 1).ok();

            let mut token = 0u32;
            let mut mgr = None;
            MFCreateDXGIDeviceManager(&mut token, &mut mgr)?;
            let mgr = mgr.unwrap();
            mgr.ResetDevice(device, token)?;
            mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, std::mem::transmute::<_, usize>(mgr.as_raw()))?;

            let codec: ICodecAPI = mft.cast()?;
            let set = |guid: &GUID, v: VARIANT| {
                if let Err(e) = codec.SetValue(guid, &v) {
                    crate::log!("encoder: property {guid:?} not applied: {e}");
                }
            };
            set(&CODECAPI_AVLowLatencyMode, VARIANT::from(true));
            set(&CODECAPI_AVEncCommonRateControlMode, VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32));
            set(&CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(bitrate));
            set(&CODECAPI_AVEncCommonBufferSize, VARIANT::from((bitrate as u64 * 2 / fps as u64) as u32));
            set(&CODECAPI_AVEncMPVDefaultBPictureCount, VARIANT::from(0u32));
            set(&CODECAPI_AVEncMPVGOPSize, VARIANT::from(fps * 3600));
            set(&CODECAPI_AVEncCommonQualityVsSpeed, VARIANT::from(33u32));

            let (mut ni, mut no) = (0u32, 0u32);
            mft.GetStreamCount(&mut ni, &mut no)?;
            let (mut in_id, mut out_id) = ([0u32], [0u32]);
            if mft.GetStreamIDs(&mut in_id, &mut out_id).is_err() {
                (in_id, out_id) = ([0], [0]);
            }
            let (in_id, out_id) = (in_id[0], out_id[0]);

            let out = MFCreateMediaType()?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            out.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h))?;
            out.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            out.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
            out.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
            out.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
            out.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
            if mft.SetOutputType(out_id, &out, 0).is_err() {
                out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32)?;
                mft.SetOutputType(out_id, &out, 0)?;
            }

            let mut i = 0;
            loop {
                let t = mft.GetInputAvailableType(in_id, i)?;
                if t.GetGUID(&MF_MT_SUBTYPE)? == MFVideoFormat_NV12 {
                    t.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h))?;
                    t.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
                    t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
                    mft.SetInputType(in_id, &t, 0)?;
                    break;
                }
                i += 1;
            }

            // Async MFTs report readiness through events; pump them on a helper thread.
            let gen: IMFMediaEventGenerator = mft.cast()?;
            let (tx, rx) = mpsc::channel();
            let g = SendPtr(gen);
            std::thread::Builder::new()
                .name("encoder-events".into())
                .spawn(move || {
                    let g = g;
                    while let Ok(ev) = g.0.GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0)) {
                        if tx.send(ev.GetType().unwrap_or(0)).is_err() {
                            break;
                        }
                    }
                })
                .ok();

            mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            crate::log!("encoder: {name} {w}x{h}@{fps} {} kbps", bitrate / 1000);
            Ok(Self {
                mft,
                codec,
                _mgr: mgr,
                events: rx,
                in_id,
                out_id,
                need: 0,
                frame_no: 0,
                fps,
                seq_header: Vec::new(),
                pending_ts: Default::default(),
                name,
            })
        }
    }

    fn find(luid: LUID) -> Result<(IMFTransform, String)> {
        unsafe {
            let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
            let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
            let mut pp: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut n = 0u32;
            MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER, Some(&input), Some(&output), &mut pp, &mut n)?;
            let list: Vec<IMFActivate> = (0..n as usize).filter_map(|i| std::ptr::read(pp.add(i))).collect();
            if !pp.is_null() {
                CoTaskMemFree(Some(pp as _));
            }
            let luid_of = |a: &IMFActivate| -> Option<LUID> {
                let mut b = [0u8; 8];
                a.GetBlob(&MFT_ENUM_ADAPTER_LUID, &mut b, None).ok()?;
                Some(LUID { LowPart: u32::from_le_bytes(b[0..4].try_into().unwrap()), HighPart: i32::from_le_bytes(b[4..8].try_into().unwrap()) })
            };
            let mut ordered: Vec<&IMFActivate> = list.iter().filter(|a| luid_of(a).map_or(false, |l| l == luid)).collect();
            ordered.extend(list.iter().filter(|a| luid_of(a).map_or(true, |l| l != luid)));
            let mut last = Error::new(MF_E_NOT_FOUND, "No hardware H.264 encoder is available on this PC");
            for a in ordered {
                let name = {
                    let mut p = PWSTR::null();
                    let mut len = 0;
                    if a.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut p, &mut len).is_ok() {
                        let s = p.to_string().unwrap_or_default();
                        CoTaskMemFree(Some(p.0 as _));
                        s
                    } else {
                        "Hardware H.264 encoder".into()
                    }
                };
                match a.ActivateObject::<IMFTransform>() {
                    Ok(t) => return Ok((t, name)),
                    Err(e) => last = e,
                }
            }
            Err(last)
        }
    }

    fn wait_event(&mut self, until: Instant, want_output: bool, out: &mut Vec<EncodedFrame>) -> Result<()> {
        loop {
            let left = until.saturating_duration_since(Instant::now());
            match self.events.recv_timeout(left) {
                Ok(t) if t == METransformNeedInput.0 as u32 => {
                    self.need += 1;
                    if !want_output {
                        return Ok(());
                    }
                }
                Ok(t) if t == METransformHaveOutput.0 as u32 => {
                    self.pull(out)?;
                    if want_output {
                        return Ok(());
                    }
                }
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => return Ok(()),
                Err(RecvTimeoutError::Disconnected) => return Err(Error::new(E_FAIL, "encoder stopped")),
            }
        }
    }

    /// Encodes one NV12 texture. Returns the frames produced (normally exactly one).
    pub fn encode(&mut self, tex: &ID3D11Texture2D, ts: u64, keyframe: bool) -> Result<Vec<EncodedFrame>> {
        let mut out = Vec::new();
        // Drain any pending events without blocking.
        while let Ok(t) = self.events.try_recv() {
            if t == METransformNeedInput.0 as u32 {
                self.need += 1;
            } else if t == METransformHaveOutput.0 as u32 {
                self.pull(&mut out)?;
            }
        }
        if self.need == 0 {
            self.wait_event(Instant::now() + Duration::from_millis(250), false, &mut out)?;
            if self.need == 0 {
                return Err(Error::new(E_FAIL, "encoder is not accepting input"));
            }
        }
        unsafe {
            if keyframe {
                let _ = self.codec.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32));
            }
            let buf = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, tex, 0, false)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buf)?;
            let dur = 10_000_000 / self.fps as i64;
            sample.SetSampleTime(self.frame_no * dur)?;
            sample.SetSampleDuration(dur)?;
            self.frame_no += 1;
            self.pending_ts.push_back(ts);
            self.mft.ProcessInput(self.in_id, &sample, 0)?;
        }
        self.need -= 1;
        if out.is_empty() {
            self.wait_event(Instant::now() + Duration::from_millis(100), true, &mut out)?;
        }
        Ok(out)
    }

    fn pull(&mut self, out: &mut Vec<EncodedFrame>) -> Result<()> {
        unsafe {
            let info = self.mft.GetOutputStreamInfo(self.out_id)?;
            let provides = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32) != 0;
            let sample = if provides {
                None
            } else {
                let s = MFCreateSample()?;
                s.AddBuffer(&MFCreateMemoryBuffer(info.cbSize.max(1 << 20))?)?;
                Some(s)
            };
            let mut bufs = [MFT_OUTPUT_DATA_BUFFER { dwStreamID: self.out_id, pSample: ManuallyDrop::new(sample), dwStatus: 0, pEvents: ManuallyDrop::new(None) }];
            let mut status = 0;
            let r = self.mft.ProcessOutput(0, &mut bufs, &mut status);
            let sample = ManuallyDrop::take(&mut bufs[0].pSample);
            drop(ManuallyDrop::take(&mut bufs[0].pEvents));
            if let Err(e) = r {
                if e.code() == MF_E_TRANSFORM_STREAM_CHANGE {
                    let t = self.mft.GetOutputAvailableType(self.out_id, 0)?;
                    self.mft.SetOutputType(self.out_id, &t, 0)?;
                    return Ok(());
                }
                if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT {
                    return Ok(());
                }
                return Err(e);
            }
            let Some(sample) = sample else { return Ok(()) };
            let buf = sample.ConvertToContiguousBuffer()?;
            let mut p = std::ptr::null_mut();
            let mut len = 0;
            buf.Lock(&mut p, None, Some(&mut len))?;
            let mut data = std::slice::from_raw_parts(p, len as usize).to_vec();
            buf.Unlock()?;
            let key = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
            if self.seq_header.is_empty() {
                if let Ok(t) = self.mft.GetOutputCurrentType(self.out_id) {
                    let mut size = 0;
                    if t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER).map(|s| size = s).is_ok() && size > 0 {
                        let mut hdr = vec![0u8; size as usize];
                        if t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut hdr, None).is_ok() {
                            self.seq_header = hdr;
                        }
                    }
                }
            }
            if key && !has_sps(&data) && !self.seq_header.is_empty() {
                let mut d = self.seq_header.clone();
                d.extend_from_slice(&data);
                data = d;
            }
            let ts = self.pending_ts.pop_front().unwrap_or_else(crate::proto::now_us);
            out.push(EncodedFrame { data, key, ts });
        }
        Ok(())
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Ok(s) = self.mft.cast::<IMFShutdown>() {
                let _ = s.Shutdown();
            }
        }
    }
}
