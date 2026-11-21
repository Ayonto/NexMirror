//! Screen capture with DXGI Desktop Duplication.
//!
//! Per frame (all on the GPU): copy the desktop image, composite the mouse pointer
//! (exact AND/XOR semantics for monochrome and masked pointers), then scale and convert
//! BGRA -> NV12 BT.709 with the hardware video processor for the encoder.

use crate::gfx::{compile, constant_buffer, create_device, write_buffer};
use std::mem::ManuallyDrop;
use windows::{
    core::*,
    Win32::{
        Foundation::*,
        Graphics::{
            Direct3D::*,
            Direct3D11::*,
            Dxgi::{Common::*, *},
        },
    },
};

#[derive(Clone, Debug)]
pub struct DisplayInfo {
    pub name: String,
    pub label: String,
    pub primary: bool,
}

fn device_name(d: &DXGI_OUTPUT_DESC) -> String {
    let n = d.DeviceName.iter().position(|&c| c == 0).unwrap_or(d.DeviceName.len());
    String::from_utf16_lossy(&d.DeviceName[..n])
}

fn outputs() -> Vec<(IDXGIAdapter1, IDXGIOutput, DXGI_OUTPUT_DESC)> {
    let mut v = Vec::new();
    let Ok(f) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else { return v };
    let mut ai = 0;
    while let Ok(a) = unsafe { f.EnumAdapters1(ai) } {
        let mut oi = 0;
        while let Ok(o) = unsafe { a.EnumOutputs(oi) } {
            if let Ok(d) = unsafe { o.GetDesc() } {
                if d.AttachedToDesktop.as_bool() {
                    v.push((a.clone(), o, d));
                }
            }
            oi += 1;
        }
        ai += 1;
    }
    v
}

pub fn list_displays() -> Vec<DisplayInfo> {
    outputs()
        .iter()
        .enumerate()
        .map(|(i, (_, _, d))| {
            let r = d.DesktopCoordinates;
            let (w, h) = ((r.right - r.left) as u32, (r.bottom - r.top) as u32);
            let primary = r.left == 0 && r.top == 0;
            DisplayInfo {
                name: device_name(d),
                label: format!("Display {}  ·  {}×{}{}", i + 1, w, h, if primary { "  ·  Primary" } else { "" }),
                primary,
            }
        })
        .collect()
}

const CURSOR_HLSL: &str = r#"
cbuffer C : register(b0) { float4 rect; };
Texture2D tex : register(t0);
SamplerState samp : register(s0);
struct V { float4 pos : SV_POSITION; float2 uv : TEXCOORD0; };
V vs_main(uint id : SV_VertexID) {
    V o;
    float2 uv = float2(id & 1, id >> 1);
    o.uv = uv;
    o.pos = float4(lerp(rect.xy, rect.zw, uv), 0, 1);
    return o;
}
float4 ps_main(V i) : SV_TARGET { return tex.Sample(samp, i.uv); }
"#;

enum PointerShape {
    Color(ID3D11ShaderResourceView),
    /// (AND mask, XOR mask) rendered as two passes.
    Masked(ID3D11ShaderResourceView, ID3D11ShaderResourceView),
}

struct Pointer {
    visible: bool,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    shape: Option<PointerShape>,
    buf: Vec<u8>,
}

struct CursorPipe {
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    cb: ID3D11Buffer,
    sampler: ID3D11SamplerState,
    blend_alpha: ID3D11BlendState,
    blend_and: ID3D11BlendState,
    blend_xor: ID3D11BlendState,
}

fn blend(dev: &ID3D11Device, src: D3D11_BLEND, dst: D3D11_BLEND) -> Result<ID3D11BlendState> {
    let mut d = D3D11_BLEND_DESC::default();
    d.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
        BlendEnable: true.into(),
        SrcBlend: src,
        DestBlend: dst,
        BlendOp: D3D11_BLEND_OP_ADD,
        SrcBlendAlpha: D3D11_BLEND_ZERO,
        DestBlendAlpha: D3D11_BLEND_ONE,
        BlendOpAlpha: D3D11_BLEND_OP_ADD,
        RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
    };
    let mut b = None;
    unsafe { dev.CreateBlendState(&d, Some(&mut b))? };
    Ok(b.unwrap())
}

impl CursorPipe {
    fn new(dev: &ID3D11Device) -> Result<Self> {
        let vsb = compile(CURSOR_HLSL, s!("vs_main"), s!("vs_4_0"))?;
        let psb = compile(CURSOR_HLSL, s!("ps_main"), s!("ps_4_0"))?;
        let (mut vs, mut ps) = (None, None);
        unsafe {
            dev.CreateVertexShader(&vsb, None, Some(&mut vs))?;
            dev.CreatePixelShader(&psb, None, Some(&mut ps))?;
        }
        Ok(Self {
            vs: vs.unwrap(),
            ps: ps.unwrap(),
            cb: constant_buffer(dev, 16)?,
            sampler: {
                let desc = D3D11_SAMPLER_DESC {
                    Filter: D3D11_FILTER_MIN_MAG_MIP_POINT,
                    AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                    MaxLOD: f32::MAX,
                    ..Default::default()
                };
                let mut s = None;
                unsafe { dev.CreateSamplerState(&desc, Some(&mut s))? };
                s.unwrap()
            },
            blend_alpha: blend(dev, D3D11_BLEND_SRC_ALPHA, D3D11_BLEND_INV_SRC_ALPHA)?,
            // dst = dst * and
            blend_and: blend(dev, D3D11_BLEND_ZERO, D3D11_BLEND_SRC_COLOR)?,
            // dst = src * (1 - dst) + dst * (1 - src)  == dst XOR src for 0/1 values
            blend_xor: blend(dev, D3D11_BLEND_INV_DEST_COLOR, D3D11_BLEND_INV_SRC_COLOR)?,
        })
    }
}

fn texture_from_bgra(dev: &ID3D11Device, w: u32, h: u32, px: &[u8]) -> Result<ID3D11ShaderResourceView> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_IMMUTABLE,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        ..Default::default()
    };
    let init = D3D11_SUBRESOURCE_DATA { pSysMem: px.as_ptr() as _, SysMemPitch: w * 4, SysMemSlicePitch: 0 };
    let (mut t, mut v) = (None, None);
    unsafe {
        dev.CreateTexture2D(&desc, Some(&init), Some(&mut t))?;
        dev.CreateShaderResourceView(t.as_ref().unwrap(), None, Some(&mut v))?;
    }
    Ok(v.unwrap())
}

struct VideoProc {
    vctx: ID3D11VideoContext,
    proc_: ID3D11VideoProcessor,
    input: ID3D11VideoProcessorInputView,
    outputs: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    next: usize,
}

pub enum Acquired {
    Nothing,
    Updated,
}

pub struct Capture {
    pub device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    pub adapter_luid: LUID,
    dup: IDXGIOutputDuplication,
    pub src_w: u32,
    pub src_h: u32,
    desktop: ID3D11Texture2D,
    composed: ID3D11Texture2D,
    composed_rtv: ID3D11RenderTargetView,
    pointer: Pointer,
    cursor: CursorPipe,
    vp: Option<VideoProc>,
    pub out_w: u32,
    pub out_h: u32,
    pub display_name: String,
}

fn texture(dev: &ID3D11Device, w: u32, h: u32, fmt: DXGI_FORMAT, bind: u32) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: fmt,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: bind,
        ..Default::default()
    };
    let mut t = None;
    unsafe { dev.CreateTexture2D(&desc, None, Some(&mut t))? };
    Ok(t.unwrap())
}

impl Capture {
    /// `display` is a DXGI device name; empty selects the primary display.
    pub fn new(display: &str) -> Result<Self> {
        let outs = outputs();
        let pick = outs
            .iter()
            .find(|(_, _, d)| !display.is_empty() && device_name(d) == display)
            .or_else(|| outs.iter().find(|(_, _, d)| d.DesktopCoordinates.left == 0 && d.DesktopCoordinates.top == 0))
            .or(outs.first())
            .ok_or_else(|| Error::new(E_FAIL, "No display is attached to the desktop"))?;
        let (adapter, output, desc) = pick.clone();
        let (device, ctx) = create_device(Some(&adapter.cast()?))?;
        let output: IDXGIOutput1 = output.cast()?;
        let dup = Self::duplicate(&device, &output)?;
        let dd = unsafe { dup.GetDesc() };
        let (src_w, src_h) = (dd.ModeDesc.Width, dd.ModeDesc.Height);
        let bind = (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32;
        let desktop = texture(&device, src_w, src_h, DXGI_FORMAT_B8G8R8A8_UNORM, bind)?;
        let composed = texture(&device, src_w, src_h, DXGI_FORMAT_B8G8R8A8_UNORM, bind)?;
        let mut rtv = None;
        unsafe { device.CreateRenderTargetView(&composed, None, Some(&mut rtv))? };
        let luid = unsafe { adapter.GetDesc1()?.AdapterLuid };
        crate::log!("capture: {} {}x{} rotation {:?}", device_name(&desc), src_w, src_h, dd.Rotation);
        Ok(Self {
            cursor: CursorPipe::new(&device)?,
            device,
            ctx,
            adapter_luid: luid,
            dup,
            src_w,
            src_h,
            desktop,
            composed,
            composed_rtv: rtv.unwrap(),
            pointer: Pointer { visible: false, x: 0, y: 0, w: 0, h: 0, shape: None, buf: Vec::new() },
            vp: None,
            out_w: src_w,
            out_h: src_h,
            display_name: device_name(&desc),
        })
    }

    fn duplicate(device: &ID3D11Device, output: &IDXGIOutput1) -> Result<IDXGIOutputDuplication> {
        if let Ok(o5) = output.cast::<IDXGIOutput5>() {
            if let Ok(d) = unsafe { o5.DuplicateOutput1(device, 0, &[DXGI_FORMAT_B8G8R8A8_UNORM]) } {
                return Ok(d);
            }
        }
        unsafe { output.DuplicateOutput(device) }
    }

    /// Sets up scaling + colour conversion to an NV12 surface of `w`x`h`.
    pub fn set_output_size(&mut self, w: u32, h: u32) -> Result<()> {
        let vdev: ID3D11VideoDevice = self.device.cast()?;
        let vctx: ID3D11VideoContext = self.ctx.cast()?;
        let rate = DXGI_RATIONAL { Numerator: 60, Denominator: 1 };
        let cd = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: self.src_w,
            InputHeight: self.src_h,
            OutputFrameRate: rate,
            OutputWidth: w,
            OutputHeight: h,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        unsafe {
            let en = vdev.CreateVideoProcessorEnumerator(&cd)?;
            let proc_ = vdev.CreateVideoProcessor(&en, 0)?;
            let ivd = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 } },
            };
            let mut input = None;
            vdev.CreateVideoProcessorInputView(&self.composed, &en, &ivd, Some(&mut input))?;
            let mut outputs = Vec::new();
            for _ in 0..3 {
                let tex = texture(&self.device, w, h, DXGI_FORMAT_NV12, (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_VIDEO_ENCODER.0) as u32)
                    .or_else(|_| texture(&self.device, w, h, DXGI_FORMAT_NV12, D3D11_BIND_RENDER_TARGET.0 as u32))?;
                let ovd = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                    ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
                };
                let mut ov = None;
                vdev.CreateVideoProcessorOutputView(&tex, &en, &ovd, Some(&mut ov))?;
                outputs.push((tex, ov.unwrap()));
            }
            vctx.VideoProcessorSetStreamFrameFormat(&proc_, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            vctx.VideoProcessorSetStreamAutoProcessingMode(&proc_, 0, false);
            let src = RECT { left: 0, top: 0, right: self.src_w as i32, bottom: self.src_h as i32 };
            let dst = RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 };
            vctx.VideoProcessorSetStreamSourceRect(&proc_, 0, true, Some(&src));
            vctx.VideoProcessorSetStreamDestRect(&proc_, 0, true, Some(&dst));
            vctx.VideoProcessorSetOutputTargetRect(&proc_, true, Some(&dst));
            if let Ok(v1) = vctx.cast::<ID3D11VideoContext1>() {
                v1.VideoProcessorSetStreamColorSpace1(&proc_, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
                v1.VideoProcessorSetOutputColorSpace1(&proc_, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
            } else {
                // Usage=0, RGB_Range=0 (full), YCbCr_Matrix=1 (BT.709), Nominal_Range=1 (16-235)
                let input_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0b0100 };
                let output_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0b0100 | (1 << 4) };
                vctx.VideoProcessorSetStreamColorSpace(&proc_, 0, &input_cs);
                vctx.VideoProcessorSetOutputColorSpace(&proc_, &output_cs);
            }
            self.vp = Some(VideoProc { vctx, proc_, input: input.unwrap(), outputs, next: 0 });
        }
        self.out_w = w;
        self.out_h = h;
        Ok(())
    }

    /// Waits up to `timeout_ms` for a desktop or pointer update.
    pub fn acquire(&mut self, timeout_ms: u32) -> Result<Acquired> {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut res: Option<IDXGIResource> = None;
        if let Err(e) = unsafe { self.dup.AcquireNextFrame(timeout_ms, &mut info, &mut res) } {
            if e.code() == DXGI_ERROR_WAIT_TIMEOUT {
                return Ok(Acquired::Nothing);
            }
            return Err(e);
        }
        let r = self.process_frame(&info, res);
        unsafe {
            let _ = self.dup.ReleaseFrame();
        }
        r
    }

    fn process_frame(&mut self, info: &DXGI_OUTDUPL_FRAME_INFO, res: Option<IDXGIResource>) -> Result<Acquired> {
        let mut updated = false;
        if info.LastPresentTime != 0 {
            if let Some(r) = res {
                let tex: ID3D11Texture2D = r.cast()?;
                unsafe { self.ctx.CopyResource(&self.desktop, &tex) };
                updated = true;
            }
        }
        if info.LastMouseUpdateTime != 0 {
            let p = &mut self.pointer;
            p.visible = info.PointerPosition.Visible.as_bool();
            p.x = info.PointerPosition.Position.x;
            p.y = info.PointerPosition.Position.y;
            updated = true;
            if info.PointerShapeBufferSize > 0 {
                self.update_pointer_shape(info.PointerShapeBufferSize)?;
            }
        }
        Ok(if updated { Acquired::Updated } else { Acquired::Nothing })
    }

    fn update_pointer_shape(&mut self, size: u32) -> Result<()> {
        let p = &mut self.pointer;
        p.buf.resize(size as usize, 0);
        let mut needed = 0;
        let mut si = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
        unsafe { self.dup.GetFramePointerShape(size, p.buf.as_mut_ptr() as _, &mut needed, &mut si)? };
        let (w, pitch) = (si.Width, si.Pitch as usize);
        let t = si.Type as i32;
        let dev = &self.device;
        if t == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 {
            let h = si.Height;
            let mut px = vec![0u8; (w * h * 4) as usize];
            for y in 0..h as usize {
                px[y * w as usize * 4..(y + 1) * w as usize * 4].copy_from_slice(&p.buf[y * pitch..y * pitch + w as usize * 4]);
            }
            p.shape = Some(PointerShape::Color(texture_from_bgra(dev, w, h, &px)?));
            (p.w, p.h) = (w, h);
        } else if t == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 {
            let h = si.Height;
            let (mut and, mut xor) = (vec![0u8; (w * h * 4) as usize], vec![0u8; (w * h * 4) as usize]);
            for y in 0..h as usize {
                for x in 0..w as usize {
                    let s = &p.buf[y * pitch + x * 4..y * pitch + x * 4 + 4];
                    let o = (y * w as usize + x) * 4;
                    let a = if s[3] == 0 { 0 } else { 255 };
                    and[o..o + 4].copy_from_slice(&[a, a, a, 255]);
                    xor[o..o + 4].copy_from_slice(&[s[0], s[1], s[2], 255]);
                }
            }
            p.shape = Some(PointerShape::Masked(texture_from_bgra(dev, w, h, &and)?, texture_from_bgra(dev, w, h, &xor)?));
            (p.w, p.h) = (w, h);
        } else if t == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 {
            let h = si.Height / 2;
            let (mut and, mut xor) = (vec![0u8; (w * h * 4) as usize], vec![0u8; (w * h * 4) as usize]);
            let bit = |row: usize, x: usize| (p.buf[row * pitch + x / 8] >> (7 - (x % 8))) & 1;
            for y in 0..h as usize {
                for x in 0..w as usize {
                    let o = (y * w as usize + x) * 4;
                    let a = bit(y, x) * 255;
                    let b = bit(y + h as usize, x) * 255;
                    and[o..o + 4].copy_from_slice(&[a, a, a, 255]);
                    xor[o..o + 4].copy_from_slice(&[b, b, b, 255]);
                }
            }
            p.shape = Some(PointerShape::Masked(texture_from_bgra(dev, w, h, &and)?, texture_from_bgra(dev, w, h, &xor)?));
            (p.w, p.h) = (w, h);
        }
        Ok(())
    }

    fn draw_pointer(&self) -> Result<()> {
        let p = &self.pointer;
        let Some(shape) = &p.shape else { return Ok(()) };
        if !p.visible {
            return Ok(());
        }
        let (sw, sh) = (self.src_w as f32, self.src_h as f32);
        let ndc = |x: f32, y: f32| [x / sw * 2.0 - 1.0, 1.0 - y / sh * 2.0];
        let a = ndc(p.x as f32, p.y as f32);
        let b = ndc((p.x + p.w as i32) as f32, (p.y + p.h as i32) as f32);
        let c = &self.cursor;
        let ctx = &self.ctx;
        write_buffer(ctx, &c.cb, &[a[0], a[1], b[0], b[1]])?;
        unsafe {
            ctx.OMSetRenderTargets(Some(&[Some(self.composed_rtv.clone())]), None);
            ctx.RSSetViewports(Some(&[D3D11_VIEWPORT { TopLeftX: 0.0, TopLeftY: 0.0, Width: sw, Height: sh, MinDepth: 0.0, MaxDepth: 1.0 }]));
            ctx.RSSetState(None);
            ctx.IASetInputLayout(None);
            ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            ctx.VSSetShader(&c.vs, None);
            ctx.VSSetConstantBuffers(0, Some(&[Some(c.cb.clone())]));
            ctx.PSSetShader(&c.ps, None);
            ctx.PSSetSamplers(0, Some(&[Some(c.sampler.clone())]));
            let pass = |srv: &ID3D11ShaderResourceView, blend: &ID3D11BlendState| {
                ctx.OMSetBlendState(blend, None, 0xffff_ffff);
                ctx.PSSetShaderResources(0, Some(&[Some(srv.clone())]));
                ctx.Draw(4, 0);
            };
            match shape {
                PointerShape::Color(t) => pass(t, &c.blend_alpha),
                PointerShape::Masked(and, xor) => {
                    pass(and, &c.blend_and);
                    pass(xor, &c.blend_xor);
                }
            }
            ctx.PSSetShaderResources(0, Some(&[None]));
            ctx.OMSetRenderTargets(None, None);
        }
        Ok(())
    }

    /// Composites the pointer and converts the latest desktop image into an NV12 texture.
    pub fn render(&mut self) -> Result<ID3D11Texture2D> {
        unsafe { self.ctx.CopyResource(&self.composed, &self.desktop) };
        self.draw_pointer()?;
        let vp = self.vp.as_mut().ok_or_else(|| Error::new(E_FAIL, "output size not set"))?;
        let (tex, view) = vp.outputs[vp.next].clone();
        vp.next = (vp.next + 1) % vp.outputs.len();
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: ManuallyDrop::new(Some(vp.input.clone())),
            ..Default::default()
        };
        let mut streams = [stream];
        let r = unsafe { vp.vctx.VideoProcessorBlt(&vp.proc_, &view, 0, &streams) };
        unsafe { ManuallyDrop::drop(&mut streams[0].pInputSurface) };
        r?;
        Ok(tex)
    }
}
