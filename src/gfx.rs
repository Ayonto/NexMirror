//! Direct3D 11 presentation: flip-model swapchain, egui renderer and NV12 video renderer.
//! Video and UI share one device, so decoded frames are drawn without any CPU copy.

use std::collections::HashMap;
use windows::{
    core::*,
    Win32::{
        Foundation::*,
        Graphics::{
            Direct3D::{Fxc::*, *},
            Direct3D11::*,
            Dxgi::{Common::*, *},
        },
    },
};

pub fn create_device(adapter: Option<&IDXGIAdapter>) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_10_0];
    let driver = if adapter.is_some() { D3D_DRIVER_TYPE_UNKNOWN } else { D3D_DRIVER_TYPE_HARDWARE };
    let mut last = Error::from(E_FAIL);
    for flags in [
        D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    ] {
        let (mut dev, mut ctx) = (None, None);
        match unsafe {
            D3D11CreateDevice(adapter, driver, HMODULE::default(), flags, Some(&levels), D3D11_SDK_VERSION, Some(&mut dev), None, Some(&mut ctx))
        } {
            Ok(()) => {
                let dev = dev.unwrap();
                if let Ok(mt) = dev.cast::<ID3D11Multithread>() {
                    unsafe {
                        let _ = mt.SetMultithreadProtected(true);
                    }
                }
                return Ok((dev, ctx.unwrap()));
            }
            Err(e) => last = e,
        }
    }
    Err(last)
}

pub fn compile(src: &str, entry: PCSTR, target: PCSTR) -> Result<Vec<u8>> {
    let mut code = None;
    let mut errs = None;
    let r = unsafe {
        D3DCompile(src.as_ptr() as _, src.len(), PCSTR::null(), None, None, entry, target, D3DCOMPILE_OPTIMIZATION_LEVEL3, 0, &mut code, Some(&mut errs))
    };
    if let Err(e) = r {
        if let Some(b) = errs {
            let msg = unsafe { std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()) };
            crate::log!("shader compile failed: {}", String::from_utf8_lossy(msg));
        }
        return Err(e);
    }
    let b = code.unwrap();
    Ok(unsafe { std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()).to_vec() })
}

pub fn constant_buffer(dev: &ID3D11Device, size: u32) -> Result<ID3D11Buffer> {
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: size,
        Usage: D3D11_USAGE_DYNAMIC,
        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        ..Default::default()
    };
    let mut b = None;
    unsafe { dev.CreateBuffer(&desc, None, Some(&mut b))? };
    Ok(b.unwrap())
}

pub fn write_buffer<T: Copy>(ctx: &ID3D11DeviceContext, buf: &ID3D11Buffer, data: &[T]) -> Result<()> {
    unsafe {
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        ctx.Map(buf, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut m))?;
        std::ptr::copy_nonoverlapping(data.as_ptr() as *const u8, m.pData as *mut u8, std::mem::size_of_val(data));
        ctx.Unmap(buf, 0);
    }
    Ok(())
}

pub fn linear_sampler(dev: &ID3D11Device) -> Result<ID3D11SamplerState> {
    let desc = D3D11_SAMPLER_DESC {
        Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
        AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
        MaxLOD: f32::MAX,
        ..Default::default()
    };
    let mut s = None;
    unsafe { dev.CreateSamplerState(&desc, Some(&mut s))? };
    Ok(s.unwrap())
}

/// A decoded NV12 frame that the video shader can sample.
pub struct VideoTex {
    pub tex: ID3D11Texture2D,
    srv_y: ID3D11ShaderResourceView,
    srv_uv: ID3D11ShaderResourceView,
    pub width: u32,
    pub height: u32,
    pub disp_w: u32,
    pub disp_h: u32,
}

impl VideoTex {
    pub fn new(dev: &ID3D11Device, width: u32, height: u32) -> Result<Self> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            ..Default::default()
        };
        let mut tex = None;
        unsafe { dev.CreateTexture2D(&desc, None, Some(&mut tex))? };
        let tex = tex.unwrap();
        let view = |fmt| -> Result<ID3D11ShaderResourceView> {
            let d = D3D11_SHADER_RESOURCE_VIEW_DESC {
                Format: fmt,
                ViewDimension: D3D_SRV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_SRV { MostDetailedMip: 0, MipLevels: 1 } },
            };
            let mut v = None;
            unsafe { dev.CreateShaderResourceView(&tex, Some(&d), Some(&mut v))? };
            Ok(v.unwrap())
        };
        let srv_y = view(DXGI_FORMAT_R8_UNORM)?;
        let srv_uv = view(DXGI_FORMAT_R8G8_UNORM)?;
        Ok(Self { tex, srv_y, srv_uv, width, height, disp_w: width, disp_h: height })
    }
}

const VIDEO_HLSL: &str = r#"
Texture2D<float>  texY  : register(t0);
Texture2D<float2> texUV : register(t1);
SamplerState samp : register(s0);
cbuffer C : register(b0) { float2 uvScale; float2 pad; };
struct V { float4 pos : SV_POSITION; float2 uv : TEXCOORD0; };
V vs_main(uint id : SV_VertexID) {
    V o;
    float2 p = float2((id << 1) & 2, id & 2);
    o.uv = p * uvScale;
    o.pos = float4(p * float2(2, -2) + float2(-1, 1), 0, 1);
    return o;
}
float4 ps_main(V i) : SV_TARGET {
    // BT.709 limited range -> full range RGB
    float y = (texY.Sample(samp, i.uv) - 16.0 / 255.0) * (255.0 / 219.0);
    float2 c = (texUV.Sample(samp, i.uv) - 128.0 / 255.0) * (255.0 / 224.0);
    float3 rgb = float3(y + 1.5748 * c.y, y - 0.1873 * c.x - 0.4681 * c.y, y + 1.8556 * c.x);
    return float4(saturate(rgb), 1);
}
"#;

struct VideoPipeline {
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    cb: ID3D11Buffer,
    sampler: ID3D11SamplerState,
}

impl VideoPipeline {
    fn new(dev: &ID3D11Device) -> Result<Self> {
        let vsb = compile(VIDEO_HLSL, s!("vs_main"), s!("vs_4_0"))?;
        let psb = compile(VIDEO_HLSL, s!("ps_main"), s!("ps_4_0"))?;
        let (mut vs, mut ps) = (None, None);
        unsafe {
            dev.CreateVertexShader(&vsb, None, Some(&mut vs))?;
            dev.CreatePixelShader(&psb, None, Some(&mut ps))?;
        }
        Ok(Self { vs: vs.unwrap(), ps: ps.unwrap(), cb: constant_buffer(dev, 16)?, sampler: linear_sampler(dev)? })
    }

    fn draw(&self, ctx: &ID3D11DeviceContext, v: &VideoTex, rect: [f32; 4]) -> Result<()> {
        write_buffer(ctx, &self.cb, &[v.disp_w as f32 / v.width as f32, v.disp_h as f32 / v.height as f32, 0.0, 0.0])?;
        unsafe {
            ctx.RSSetViewports(Some(&[D3D11_VIEWPORT { TopLeftX: rect[0], TopLeftY: rect[1], Width: rect[2], Height: rect[3], MinDepth: 0.0, MaxDepth: 1.0 }]));
            ctx.RSSetState(None);
            ctx.OMSetBlendState(None, None, 0xffff_ffff);
            ctx.IASetInputLayout(None);
            ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.VSSetShader(&self.vs, None);
            ctx.PSSetShader(&self.ps, None);
            ctx.PSSetConstantBuffers(0, Some(&[Some(self.cb.clone())]));
            ctx.VSSetConstantBuffers(0, Some(&[Some(self.cb.clone())]));
            ctx.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            ctx.PSSetShaderResources(0, Some(&[Some(v.srv_y.clone()), Some(v.srv_uv.clone())]));
            ctx.Draw(3, 0);
            ctx.PSSetShaderResources(0, Some(&[None, None]));
        }
        Ok(())
    }
}

const EGUI_HLSL: &str = r#"
cbuffer C : register(b0) { float2 screen; float2 pad; };
struct VI { float2 pos : POSITION; float2 uv : TEXCOORD0; float4 col : COLOR0; };
struct PI { float4 pos : SV_POSITION; float2 uv : TEXCOORD0; float4 col : COLOR0; };
Texture2D tex : register(t0);
SamplerState samp : register(s0);
PI vs_main(VI i) {
    PI o;
    o.pos = float4(2.0 * i.pos.x / screen.x - 1.0, 1.0 - 2.0 * i.pos.y / screen.y, 0, 1);
    o.uv = i.uv;
    o.col = i.col;
    return o;
}
float4 ps_main(PI i) : SV_TARGET { return i.col * tex.Sample(samp, i.uv); }
"#;

struct GpuTex {
    tex: ID3D11Texture2D,
    srv: ID3D11ShaderResourceView,
}

struct EguiPainter {
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    layout: ID3D11InputLayout,
    cb: ID3D11Buffer,
    blend: ID3D11BlendState,
    raster: ID3D11RasterizerState,
    sampler: ID3D11SamplerState,
    vb: Option<(ID3D11Buffer, usize)>,
    ib: Option<(ID3D11Buffer, usize)>,
    textures: HashMap<egui::TextureId, GpuTex>,
}

impl EguiPainter {
    fn new(dev: &ID3D11Device) -> Result<Self> {
        let vsb = compile(EGUI_HLSL, s!("vs_main"), s!("vs_4_0"))?;
        let psb = compile(EGUI_HLSL, s!("ps_main"), s!("ps_4_0"))?;
        let (mut vs, mut ps, mut layout, mut blend, mut raster) = (None, None, None, None, None);
        let elems = [
            D3D11_INPUT_ELEMENT_DESC { SemanticName: s!("POSITION"), Format: DXGI_FORMAT_R32G32_FLOAT, AlignedByteOffset: 0, ..Default::default() },
            D3D11_INPUT_ELEMENT_DESC { SemanticName: s!("TEXCOORD"), Format: DXGI_FORMAT_R32G32_FLOAT, AlignedByteOffset: 8, ..Default::default() },
            D3D11_INPUT_ELEMENT_DESC { SemanticName: s!("COLOR"), Format: DXGI_FORMAT_R8G8B8A8_UNORM, AlignedByteOffset: 16, ..Default::default() },
        ];
        let mut bd = D3D11_BLEND_DESC::default();
        bd.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
            BlendEnable: true.into(),
            SrcBlend: D3D11_BLEND_ONE,
            DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
            BlendOp: D3D11_BLEND_OP_ADD,
            SrcBlendAlpha: D3D11_BLEND_INV_DEST_ALPHA,
            DestBlendAlpha: D3D11_BLEND_ONE,
            BlendOpAlpha: D3D11_BLEND_OP_ADD,
            RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
        };
        let rd = D3D11_RASTERIZER_DESC {
            FillMode: D3D11_FILL_SOLID,
            CullMode: D3D11_CULL_NONE,
            ScissorEnable: true.into(),
            DepthClipEnable: false.into(),
            ..Default::default()
        };
        unsafe {
            dev.CreateVertexShader(&vsb, None, Some(&mut vs))?;
            dev.CreatePixelShader(&psb, None, Some(&mut ps))?;
            dev.CreateInputLayout(&elems, &vsb, Some(&mut layout))?;
            dev.CreateBlendState(&bd, Some(&mut blend))?;
            dev.CreateRasterizerState(&rd, Some(&mut raster))?;
        }
        Ok(Self {
            vs: vs.unwrap(),
            ps: ps.unwrap(),
            layout: layout.unwrap(),
            cb: constant_buffer(dev, 16)?,
            blend: blend.unwrap(),
            raster: raster.unwrap(),
            sampler: linear_sampler(dev)?,
            vb: None,
            ib: None,
            textures: HashMap::new(),
        })
    }

    fn set_texture(&mut self, dev: &ID3D11Device, ctx: &ID3D11DeviceContext, id: egui::TextureId, delta: &egui::epaint::ImageDelta) -> Result<()> {
        let [w, h] = delta.image.size();
        let pixels: Vec<egui::Color32> = match &delta.image {
            egui::ImageData::Color(c) => c.pixels.clone(),
            egui::ImageData::Font(f) => f.srgba_pixels(None).collect(),
        };
        if let (Some([x, y]), Some(t)) = (delta.pos, self.textures.get(&id)) {
            let bx = D3D11_BOX { left: x as u32, top: y as u32, front: 0, right: (x + w) as u32, bottom: (y + h) as u32, back: 1 };
            unsafe { ctx.UpdateSubresource(&t.tex, 0, Some(&bx), pixels.as_ptr() as _, (w * 4) as u32, 0) };
            return Ok(());
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w as u32,
            Height: h as u32,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            ..Default::default()
        };
        let init = D3D11_SUBRESOURCE_DATA { pSysMem: pixels.as_ptr() as _, SysMemPitch: (w * 4) as u32, SysMemSlicePitch: 0 };
        let (mut tex, mut srv) = (None, None);
        unsafe {
            dev.CreateTexture2D(&desc, Some(&init), Some(&mut tex))?;
            dev.CreateShaderResourceView(tex.as_ref().unwrap(), None, Some(&mut srv))?;
        }
        self.textures.insert(id, GpuTex { tex: tex.unwrap(), srv: srv.unwrap() });
        Ok(())
    }

    fn ensure_buffer(dev: &ID3D11Device, slot: &mut Option<(ID3D11Buffer, usize)>, bytes: usize, bind: D3D11_BIND_FLAG) -> Result<ID3D11Buffer> {
        if let Some((b, cap)) = slot {
            if *cap >= bytes {
                return Ok(b.clone());
            }
        }
        let cap = bytes.next_power_of_two().max(64 * 1024);
        let desc = D3D11_BUFFER_DESC {
            ByteWidth: cap as u32,
            Usage: D3D11_USAGE_DYNAMIC,
            BindFlags: bind.0 as u32,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            ..Default::default()
        };
        let mut b = None;
        unsafe { dev.CreateBuffer(&desc, None, Some(&mut b))? };
        let b = b.unwrap();
        *slot = Some((b.clone(), cap));
        Ok(b)
    }

    fn draw(&mut self, dev: &ID3D11Device, ctx: &ID3D11DeviceContext, prims: &[egui::ClippedPrimitive], size: (u32, u32), ppp: f32) -> Result<()> {
        let meshes: Vec<(&egui::Rect, &egui::Mesh)> = prims
            .iter()
            .filter_map(|p| match &p.primitive {
                egui::epaint::Primitive::Mesh(m) if !m.indices.is_empty() => Some((&p.clip_rect, m)),
                _ => None,
            })
            .collect();
        if meshes.is_empty() {
            return Ok(());
        }
        let nv: usize = meshes.iter().map(|m| m.1.vertices.len()).sum();
        let ni: usize = meshes.iter().map(|m| m.1.indices.len()).sum();
        let vb = Self::ensure_buffer(dev, &mut self.vb, nv * 20, D3D11_BIND_VERTEX_BUFFER)?;
        let ib = Self::ensure_buffer(dev, &mut self.ib, ni * 4, D3D11_BIND_INDEX_BUFFER)?;
        unsafe {
            let mut mv = D3D11_MAPPED_SUBRESOURCE::default();
            let mut mi = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(&vb, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mv))?;
            ctx.Map(&ib, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mi))?;
            let (mut pv, mut pi) = (mv.pData as *mut egui::epaint::Vertex, mi.pData as *mut u32);
            for (_, m) in &meshes {
                std::ptr::copy_nonoverlapping(m.vertices.as_ptr(), pv, m.vertices.len());
                std::ptr::copy_nonoverlapping(m.indices.as_ptr(), pi, m.indices.len());
                pv = pv.add(m.vertices.len());
                pi = pi.add(m.indices.len());
            }
            ctx.Unmap(&vb, 0);
            ctx.Unmap(&ib, 0);
        }
        let (w, h) = (size.0 as f32, size.1 as f32);
        write_buffer(ctx, &self.cb, &[w / ppp, h / ppp, 0.0, 0.0])?;
        unsafe {
            ctx.RSSetViewports(Some(&[D3D11_VIEWPORT { TopLeftX: 0.0, TopLeftY: 0.0, Width: w, Height: h, MinDepth: 0.0, MaxDepth: 1.0 }]));
            ctx.RSSetState(&self.raster);
            ctx.OMSetBlendState(&self.blend, None, 0xffff_ffff);
            ctx.IASetInputLayout(&self.layout);
            ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.IASetVertexBuffers(0, 1, Some(&Some(vb.clone())), Some(&20), Some(&0));
            ctx.IASetIndexBuffer(&ib, DXGI_FORMAT_R32_UINT, 0);
            ctx.VSSetShader(&self.vs, None);
            ctx.PSSetShader(&self.ps, None);
            ctx.VSSetConstantBuffers(0, Some(&[Some(self.cb.clone())]));
            ctx.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
        }
        let (mut base_v, mut base_i) = (0i32, 0u32);
        for (clip, m) in &meshes {
            let x0 = (clip.min.x * ppp).round().clamp(0.0, w) as i32;
            let y0 = (clip.min.y * ppp).round().clamp(0.0, h) as i32;
            let x1 = (clip.max.x * ppp).round().clamp(0.0, w) as i32;
            let y1 = (clip.max.y * ppp).round().clamp(0.0, h) as i32;
            if x1 > x0 && y1 > y0 {
                if let Some(t) = self.textures.get(&m.texture_id) {
                    unsafe {
                        ctx.RSSetScissorRects(Some(&[RECT { left: x0, top: y0, right: x1, bottom: y1 }]));
                        ctx.PSSetShaderResources(0, Some(&[Some(t.srv.clone())]));
                        ctx.DrawIndexed(m.indices.len() as u32, base_i, base_v);
                    }
                }
            }
            base_v += m.vertices.len() as i32;
            base_i += m.indices.len() as u32;
        }
        Ok(())
    }
}

pub struct Gfx {
    pub device: ID3D11Device,
    pub ctx: ID3D11DeviceContext,
    swap: IDXGISwapChain1,
    rtv: Option<ID3D11RenderTargetView>,
    pub size: (u32, u32),
    tearing: bool,
    egui: EguiPainter,
    video: VideoPipeline,
}

impl Gfx {
    pub fn new(hwnd: HWND, w: u32, h: u32) -> Result<Self> {
        let (device, ctx) = create_device(None)?;
        let dxgi_dev: IDXGIDevice1 = device.cast()?;
        let factory: IDXGIFactory2 = unsafe { dxgi_dev.GetAdapter()?.GetParent()? };
        let mut tearing = false;
        if let Ok(f5) = factory.cast::<IDXGIFactory5>() {
            let mut allow = BOOL(0);
            unsafe {
                let _ = f5.CheckFeatureSupport(DXGI_FEATURE_PRESENT_ALLOW_TEARING, &mut allow as *mut _ as _, 4);
            }
            tearing = allow.as_bool();
        }
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: w.max(1),
            Height: h.max(1),
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: if tearing { DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0 as u32 } else { 0 },
            Stereo: false.into(),
        };
        let swap = unsafe { factory.CreateSwapChainForHwnd(&device, hwnd, &desc, None, None)? };
        unsafe {
            factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER)?;
            dxgi_dev.SetMaximumFrameLatency(1)?;
        }
        let egui = EguiPainter::new(&device)?;
        let video = VideoPipeline::new(&device)?;
        crate::log!("renderer ready (tearing support: {tearing})");
        Ok(Self { device, ctx, swap, rtv: None, size: (w.max(1), h.max(1)), tearing, egui, video })
    }

    pub fn resize(&mut self, w: u32, h: u32) -> Result<()> {
        let (w, h) = (w.max(1), h.max(1));
        if (w, h) == self.size {
            return Ok(());
        }
        self.rtv = None;
        unsafe {
            self.ctx.OMSetRenderTargets(None, None);
            self.ctx.Flush();
            let flags = if self.tearing { DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING } else { DXGI_SWAP_CHAIN_FLAG(0) };
            self.swap.ResizeBuffers(0, w, h, DXGI_FORMAT_UNKNOWN, flags)?;
        }
        self.size = (w, h);
        Ok(())
    }

    pub fn render(
        &mut self,
        clear: [f32; 4],
        video: Option<(&VideoTex, [f32; 4])>,
        prims: &[egui::ClippedPrimitive],
        delta: &egui::TexturesDelta,
        ppp: f32,
    ) -> Result<()> {
        for (id, d) in &delta.set {
            self.egui.set_texture(&self.device, &self.ctx, *id, d)?;
        }
        if self.rtv.is_none() {
            let back: ID3D11Texture2D = unsafe { self.swap.GetBuffer(0)? };
            let mut rtv = None;
            unsafe { self.device.CreateRenderTargetView(&back, None, Some(&mut rtv))? };
            self.rtv = rtv;
        }
        let rtv = self.rtv.clone().unwrap();
        unsafe {
            self.ctx.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            self.ctx.ClearRenderTargetView(&rtv, &clear);
        }
        if let Some((v, r)) = video {
            if r[2] >= 1.0 && r[3] >= 1.0 {
                self.video.draw(&self.ctx, v, r)?;
            }
        }
        self.egui.draw(&self.device, &self.ctx, prims, self.size, ppp)?;
        for id in &delta.free {
            self.egui.textures.remove(id);
        }
        let flags = if self.tearing { DXGI_PRESENT_ALLOW_TEARING } else { DXGI_PRESENT(0) };
        unsafe { self.swap.Present(0, flags).ok() }
    }
}
