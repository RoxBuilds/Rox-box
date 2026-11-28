//! The planes of a present put together onto what is shown. Inferred, not
//! seen yet: a title may present its picture in more than one plane (the 3D
//! one and an overlay over it), where the first alone was shown before; such
//! runs never draw the clear and the blit into the back buffer that
//! single-plane runs draw in their first frames, and stay black.
//!
//! The title gives each plane one texture, the whole of it as the source and
//! the whole picture as the destination (`crate::queue`'s `PLANE_STRIDE`).
//! The console's display hardware puts the planes together; taken here as
//! the later over the earlier through their alpha. A triangle over each
//! plane's place on the picture samples its texture into the swap chain's
//! buffer ([`Host::present_planes`]): the first plane as it is, the others
//! blended over it (`VICTORY_PLANE_ALPHA`). One plane shown whole on the
//! whole picture never comes here: it is copied, as before (`Host::present`).

use std::sync::atomic::Ordering;
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::{DXGI_PRESENT, DXGI_SWAP_CHAIN_FLAG};
use windows::core::s;

use super::{BUFFERS, FINISHED, Host, Presenter, WHY_SYNC, compile, late_free_present, settle_now, swap_format, transition};
use crate::queue::{Layer, WHOLE, inside, placed};

/// Planes put together at one present, at most (the title has two).
const LAYERS: usize = 4;

/// The shaders: a triangle over the viewport (the plane's place on the
/// picture), whose corners carry the source's texture coordinates (`part`:
/// left, top, right, bottom), and the plane's texel there.
const SHADERS: &str = r#"
#define RS "RootConstants(num32BitConstants = 4, b0), DescriptorTable(SRV(t0), visibility = SHADER_VISIBILITY_PIXEL), StaticSampler(s0, filter = FILTER_MIN_MAG_MIP_LINEAR, addressU = TEXTURE_ADDRESS_CLAMP, addressV = TEXTURE_ADDRESS_CLAMP, addressW = TEXTURE_ADDRESS_CLAMP, visibility = SHADER_VISIBILITY_PIXEL)"
cbuffer Part : register(b0) { float4 part; };
Texture2D<float4> plane : register(t0);
SamplerState linear_clamp : register(s0);
struct Corner { float4 at : SV_Position; float2 uv : TEXCOORD0; };
[RootSignature(RS)]
Corner corner(uint i : SV_VertexID) {
    float2 c = float2((i << 1) & 2, i & 2);
    Corner o;
    o.at = float4(c.x * 2.0 - 1.0, 1.0 - c.y * 2.0, 0.0, 1.0);
    o.uv = part.xy + (part.zw - part.xy) * c;
    return o;
}
[RootSignature(RS)]
float4 texel(Corner i) : SV_Target {
    return plane.SampleLevel(linear_clamp, i.uv, 0);
}
"#;

/// How a plane goes onto the picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Blend {
    /// As it is (the first plane).
    Opaque,
    /// Its colour, plus what its alpha leaves of the picture: colours that
    /// were multiplied by their alpha already.
    Premultiplied,
    /// Its colour times its alpha, plus what that leaves of the picture.
    Straight,
}

/// How the planes after the first go over it (`VICTORY_PLANE_ALPHA`:
/// `premultiplied`, the default, or `straight`).
fn alpha() -> Blend {
    static ALPHA: OnceLock<Blend> = OnceLock::new();
    *ALPHA.get_or_init(|| alpha_by(std::env::var("VICTORY_PLANE_ALPHA").ok().as_deref()))
}

fn alpha_by(value: Option<&str>) -> Blend {
    match value.map(str::trim) {
        Some("straight") => Blend::Straight,
        _ => Blend::Premultiplied,
    }
}

/// The blend of the plane at place `i` of the present's planes.
fn blend_of(i: usize, alpha: Blend) -> Blend {
    if i == 0 { Blend::Opaque } else { alpha }
}

/// The picture the planes are put together on: as wide and as high as the
/// largest of them (the console's display, which a plane placed on all of it
/// fills).
fn picture(sizes: &[(u32, u32)]) -> (u32, u32) {
    sizes.iter().fold((1, 1), |(w, h), &(x, y)| (w.max(x), h.max(y)))
}

/// `source` (pixels: left, top, right, bottom) as texture coordinates of a
/// texture that size, the same order; the whole texture where there is none
/// or it does not lie within the texture.
fn source_uv(source: Option<[i32; 4]>, (width, height): (u32, u32)) -> [f32; 4] {
    match source {
        Some(s) if inside(s, (u64::from(width), height)) => {
            let (w, h) = (width as f32, height as f32);
            [s[0] as f32 / w, s[1] as f32 / h, s[2] as f32 / w, s[3] as f32 / h]
        }
        _ => [0.0, 0.0, 1.0, 1.0],
    }
}

/// Where on a picture that size a plane placed at `placement` (fractions of
/// it: left, top, right, bottom) goes, as a viewport: left, top, width and
/// height in pixels; the whole picture where there is none or it is not one.
fn viewport(placement: Option<[f32; 4]>, (width, height): (u32, u32)) -> [f32; 4] {
    let [l, t, r, b] = placement.filter(|&p| placed(p)).unwrap_or(WHOLE);
    let (w, h) = (width as f32, height as f32);
    [l * w, t * h, (r - l) * w, (b - t) * h]
}

/// The pixels a viewport touches, within the picture: left, top, right,
/// bottom.
fn scissor([x, y, w, h]: [f32; 4], (width, height): (u32, u32)) -> [i32; 4] {
    let at = |v: f32, most: u32| (v.max(0.0) as u32).min(most) as i32;
    [at(x.floor(), width), at(y.floor(), height), at((x + w).ceil(), width), at((y + h).ceil(), height)]
}

/// The format a plane's texture is read through, and whether it is sRGB:
/// its own, or for a typeless one the plain format of its family; `None` for
/// what is not read as colour here.
fn read_format(format: u32) -> Option<(u32, bool)> {
    Some(match format {
        // Typeless: R32G32B32A32, R16G16B16A16, R10G10B10A2, R8G8B8A8,
        // B8G8R8A8, B8G8R8X8.
        1 => (2, false),
        9 => (10, false),
        23 => (24, false),
        27 => (28, false),
        90 => (87, false),
        92 => (88, false),
        29 | 91 | 93 => (format, true),
        // Float, UNORM and SNORM formats with colour in them.
        2 | 10 | 11 | 13 | 24 | 26 | 28 | 31 | 67 | 85 | 86 | 87 | 88 | 115 => (format, false),
        _ => return None,
    })
}

/// The format the picture (a swap chain buffer of format `picture`) is
/// written through for a plane read through an sRGB format or not: its sRGB
/// twin for one that is, so the plane's bytes land as they are, as a copy
/// leaves them.
fn write_format(picture: u32, srgb: bool) -> u32 {
    match (picture, srgb) {
        (28, true) => 29,
        (87, true) => 91,
        (p, _) => p,
    }
}

/// The views of the planes' textures: [`LAYERS`] slots for each of the
/// presenter's lists (the one waited for, then those sent ahead, in their
/// order), so a slot is written again only when the list that read it has
/// run: a list sent ahead is used again once its flight finished. Made
/// larger when more lists fly; the heaps made before stay (small), for the
/// lists that may still read them.
struct Views {
    heap: ID3D12DescriptorHeap,
    lists: u32,
    cpu: usize,
    gpu: u64,
    step: u32,
    old: Vec<ID3D12DescriptorHeap>,
}

impl Views {
    fn new(device: &ID3D12Device, lists: u32) -> windows::core::Result<Views> {
        let kind = D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV;
        // SAFETY: plain heap creation and queries.
        unsafe {
            let heap: ID3D12DescriptorHeap = device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: kind,
                NumDescriptors: lists * LAYERS as u32,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                NodeMask: 0,
            })?;
            Ok(Views {
                cpu: heap.GetCPUDescriptorHandleForHeapStart().ptr,
                gpu: heap.GetGPUDescriptorHandleForHeapStart().ptr,
                step: device.GetDescriptorHandleIncrementSize(kind),
                heap,
                lists,
                old: Vec::new(),
            })
        }
    }

    /// The first slot of list `list`'s, on the CPU and the GPU.
    fn of(&mut self, device: &ID3D12Device, list: u32) -> Option<(usize, u64)> {
        if list >= self.lists {
            let mut more = Views::new(device, (list + 1).next_power_of_two().max(8)).ok()?;
            more.old = std::mem::take(&mut self.old);
            more.old.push(self.heap.clone());
            *self = more;
        }
        let at = list * LAYERS as u32 * self.step;
        Some((self.cpu + at as usize, self.gpu + u64::from(at)))
    }
}

/// What planes are drawn with, made at the first present that puts planes
/// together: the root signature, the shaders, a pipeline per format written
/// and blend (made when first needed; `None`: the host refused), the views.
struct Composer {
    root: ID3D12RootSignature,
    vs: Vec<u8>,
    ps: Vec<u8>,
    pipelines: Vec<((u32, Blend), Option<ID3D12PipelineState>)>,
    views: Views,
}

// SAFETY: D3D12 objects are free-threaded; the composer is used under its lock.
unsafe impl Send for Composer {}

impl Composer {
    fn new(device: &ID3D12Device) -> windows::core::Result<Composer> {
        let vs = compile(SHADERS, s!("corner"), s!("vs_5_1"))?;
        let ps = compile(SHADERS, s!("texel"), s!("ps_5_1"))?;
        // SAFETY: the root signature is inside the vertex shader.
        let root: ID3D12RootSignature = unsafe { device.CreateRootSignature(0, &vs[..])? };
        Ok(Composer { root, vs, ps, pipelines: Vec::new(), views: Views::new(device, 8)? })
    }

    /// The pipeline that writes `format` with `blend`.
    fn pipeline(&mut self, device: &ID3D12Device, format: u32, blend: Blend) -> Option<ID3D12PipelineState> {
        if let Some((_, made)) = self.pipelines.iter().find(|(key, _)| *key == (format, blend)) {
            return made.clone();
        }
        let code = |bytes: &[u8]| D3D12_SHADER_BYTECODE { pShaderBytecode: bytes.as_ptr().cast(), BytecodeLength: bytes.len() };
        let mut blending = D3D12_BLEND_DESC::default();
        blending.RenderTarget[0] = D3D12_RENDER_TARGET_BLEND_DESC {
            BlendEnable: (blend != Blend::Opaque).into(),
            LogicOpEnable: false.into(),
            SrcBlend: if blend == Blend::Straight { D3D12_BLEND_SRC_ALPHA } else { D3D12_BLEND_ONE },
            DestBlend: D3D12_BLEND_INV_SRC_ALPHA,
            BlendOp: D3D12_BLEND_OP_ADD,
            SrcBlendAlpha: D3D12_BLEND_ONE,
            DestBlendAlpha: D3D12_BLEND_INV_SRC_ALPHA,
            BlendOpAlpha: D3D12_BLEND_OP_ADD,
            LogicOp: D3D12_LOGIC_OP_NOOP,
            RenderTargetWriteMask: D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8,
        };
        let mut formats = [DXGI_FORMAT_UNKNOWN; 8];
        formats[0] = DXGI_FORMAT(format as i32);
        // SAFETY: the description points at live data and never releases
        // its pointer.
        let made = unsafe {
            let desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&self.root),
                VS: code(&self.vs),
                PS: code(&self.ps),
                BlendState: blending,
                SampleMask: u32::MAX,
                RasterizerState: D3D12_RASTERIZER_DESC {
                    FillMode: D3D12_FILL_MODE_SOLID,
                    CullMode: D3D12_CULL_MODE_NONE,
                    DepthClipEnable: true.into(),
                    ..Default::default()
                },
                PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                NumRenderTargets: 1,
                RTVFormats: formats,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                ..Default::default()
            };
            device.CreateGraphicsPipelineState::<ID3D12PipelineState>(&desc)
        };
        let made = match made {
            Ok(pipeline) => Some(pipeline),
            Err(e) => {
                tracing::warn!(error = %e, format, ?blend, "no pipeline to put the planes of a present together: such planes are left out");
                None
            }
        };
        self.pipelines.push(((format, blend), made.clone()));
        made
    }
}

/// The composer once made; `Some(None)`: it could not be (not tried again).
static COMPOSER: Mutex<Option<Option<Composer>>> = Mutex::new(None);

/// A plane as it is drawn: its texture, the host state that is in (and stays
/// in), its description, the format it is read through and whether that one
/// is sRGB, its source and its destination.
#[derive(Clone, Copy)]
struct Drawn<'a> {
    texture: &'a ID3D12Resource,
    state: u32,
    desc: D3D12_RESOURCE_DESC,
    read: u32,
    srgb: bool,
    source: Option<[i32; 4]>,
    placement: Option<[f32; 4]>,
}

impl<'a> Drawn<'a> {
    /// `texture` as a plane, if it can be read here: 2D, one sample, one
    /// layer, readable by shaders, in a colour format; else said once why not.
    fn of(texture: &'a ID3D12Resource, state: u32, source: Option<[i32; 4]>, placement: Option<[f32; 4]>) -> Option<Self> {
        // SAFETY: a live resource.
        let desc = unsafe { texture.GetDesc() };
        let readable = desc.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE2D
            && desc.SampleDesc.Count <= 1
            && desc.DepthOrArraySize == 1
            && desc.Flags.0 & D3D12_RESOURCE_FLAG_DENY_SHADER_RESOURCE.0 == 0;
        let Some((read, srgb)) = read_format(desc.Format.0 as u32).filter(|_| readable) else {
            static SAID: std::sync::Once = std::sync::Once::new();
            SAID.call_once(|| {
                tracing::warn!(
                    format = desc.Format.0,
                    dimension = desc.Dimension.0,
                    samples = desc.SampleDesc.Count,
                    layers = desc.DepthOrArraySize,
                    flags = format_args!("{:#x}", desc.Flags.0),
                    "a plane of a present is not a texture read here (2D, one sample, one layer, colour): it is left out"
                )
            });
            return None;
        };
        Some(Drawn { texture, state, desc, read, srgb, source, placement })
    }
}

/// A render target view description of a 2D texture in `format`.
fn target_view(format: u32) -> D3D12_RENDER_TARGET_VIEW_DESC {
    D3D12_RENDER_TARGET_VIEW_DESC {
        Format: DXGI_FORMAT(format as i32),
        ViewDimension: D3D12_RTV_DIMENSION_TEXTURE2D,
        Anonymous: D3D12_RENDER_TARGET_VIEW_DESC_0 { Texture2D: D3D12_TEX2D_RTV { MipSlice: 0, PlaneSlice: 0 } },
    }
}

/// Records into `list` the planes drawn onto the picture, of that size and
/// in the render-target state, cleared black first: through the view
/// `target` makes of it in format `format`, and in that format's sRGB twin
/// for planes read through an sRGB format ([`write_format`]). Each plane's
/// texture goes from its state to one pixel shaders read and back (once for
/// a texture in two planes). `views`: the CPU and GPU handles of the first
/// of the list's [`LAYERS`] view slots.
///
/// # Safety
/// `list` is open; the textures are live and in the states their planes
/// name; the slots are the list's alone until it has run.
#[allow(clippy::too_many_arguments)]
unsafe fn record(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    composer: &mut Composer,
    planes: &[Drawn<'_>],
    picture: (u32, u32),
    format: u32,
    (cpu, gpu): (usize, u64),
    target: &dyn Fn(u32) -> D3D12_CPU_DESCRIPTOR_HANDLE,
) {
    let shader = D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE;
    let mut before = Vec::new();
    let mut after = Vec::new();
    let mut moved: Vec<*mut std::ffi::c_void> = Vec::new();
    for plane in planes {
        let raw = windows::core::Interface::as_raw(plane.texture);
        let state = D3D12_RESOURCE_STATES(plane.state as i32);
        if state.0 & shader.0 != 0 || moved.contains(&raw) {
            continue;
        }
        moved.push(raw);
        before.push(transition(plane.texture, state, shader));
        after.push(transition(plane.texture, shader, state));
    }
    let step = composer.views.step as usize;
    // SAFETY: the caller's.
    unsafe {
        if !before.is_empty() {
            list.ResourceBarrier(&before);
        }
        let plain = target(format);
        // What no plane covers is black.
        list.ClearRenderTargetView(plain, &[0.0, 0.0, 0.0, 1.0], None);
        list.SetDescriptorHeaps(&[Some(composer.views.heap.clone())]);
        list.SetGraphicsRootSignature(&composer.root);
        list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        for (i, plane) in planes.iter().take(LAYERS).enumerate() {
            let written = write_format(format, plane.srgb);
            let Some(pipeline) = composer.pipeline(device, written, blend_of(i, alpha())) else {
                continue;
            };
            let view = D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: DXGI_FORMAT(plane.read as i32),
                ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
                Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    Texture2D: D3D12_TEX2D_SRV { MostDetailedMip: 0, MipLevels: 1, ..Default::default() },
                },
            };
            device.CreateShaderResourceView(plane.texture, Some(&view), D3D12_CPU_DESCRIPTOR_HANDLE { ptr: cpu + i * step });
            let to = if written == format { plain } else { target(written) };
            list.OMSetRenderTargets(1, Some(&to), false, None);
            let [x, y, w, h] = viewport(plane.placement, picture);
            list.RSSetViewports(&[D3D12_VIEWPORT { TopLeftX: x, TopLeftY: y, Width: w, Height: h, MinDepth: 0.0, MaxDepth: 1.0 }]);
            let [left, top, right, bottom] = scissor([x, y, w, h], picture);
            list.RSSetScissorRects(&[RECT { left, top, right, bottom }]);
            let part = source_uv(plane.source, (plane.desc.Width as u32, plane.desc.Height));
            list.SetPipelineState(&pipeline);
            list.SetGraphicsRoot32BitConstants(0, 4, part.as_ptr().cast(), 0);
            list.SetGraphicsRootDescriptorTable(1, D3D12_GPU_DESCRIPTOR_HANDLE { ptr: gpu + (i * step) as u64 });
            list.DrawInstanced(3, 1, 0, 0);
        }
        if !after.is_empty() {
            list.ResourceBarrier(&after);
        }
    }
}

impl Host {
    /// Shows the planes of a present put together, as frame `frame`: each
    /// one's host copy is in the host state its layer names and stays in it.
    /// As [`Host::present`] shows one texture (the swap chain made or made
    /// again for the picture's size and format, the list sent ahead or
    /// waited for, the panel, saved frames): `None` if nothing was shown;
    /// else, in diagnostics builds, whether the frame was black.
    pub(crate) fn present_planes(&self, layers: &[Layer], frame: u64) -> Option<bool> {
        let planes: Vec<Drawn<'_>> = layers
            .iter()
            .take(LAYERS)
            .filter_map(|l| Drawn::of(&l.texture.texture, l.state, l.source, l.placement))
            .collect();
        let first = *planes.first()?;
        let format = planes.iter().find_map(|p| swap_format(p.desc.Format)).unwrap_or(DXGI_FORMAT_R8G8B8A8_UNORM);
        let sizes: Vec<(u32, u32)> = planes.iter().map(|p| (p.desc.Width as u32, p.desc.Height)).collect();
        let (width, height) = picture(&sizes);
        let mut presenter = self.presenter.lock().unwrap_or_else(|e| e.into_inner());
        let p = self.ready(&mut presenter, (width, height), format, frame)?;
        let mut composer = COMPOSER.lock().unwrap_or_else(|e| e.into_inner());
        let composer = composer
            .get_or_insert_with(|| match Composer::new(&self.device) {
                Ok(c) => {
                    tracing::info!(alpha = ?alpha(), "the planes of presents are put together (VICTORY_PRESENT_PLANES)");
                    Some(c)
                }
                Err(e) => {
                    tracing::warn!(error = %e, "nothing to put the planes of presents together with: they are not shown");
                    None
                }
            })
            .as_mut()?;
        // SAFETY: our swap chain, list, heap and pipelines and live plane
        // textures, in the states their layers name.
        unsafe {
            let index = p.swap_chain.GetCurrentBackBufferIndex();
            let Ok(back) = p.swap_chain.GetBuffer::<ID3D12Resource>(index) else {
                return None;
            };
            // As in `Host::present`: sent ahead unless the frame is to be
            // read back at once.
            #[cfg(feature = "diagnostics")]
            let read_now = crate::capture::wanted(frame) || crate::capture::listed(frame);
            #[cfg(not(feature = "diagnostics"))]
            let read_now = false;
            // A picture of the run every few seconds (`shots.rs`), in every build.
            let shot = crate::shots::due();
            let read_now = read_now || shot.is_some();
            let slot = if late_free_present() && !read_now {
                let finished = FINISHED.load(Ordering::Acquire);
                p.ahead.iter().position(|(_, flown)| *flown <= finished).or_else(|| {
                    let list = self.new_list().ok()?;
                    list.close();
                    p.ahead.push((list, 0));
                    Some(p.ahead.len() - 1)
                })
            } else {
                None
            };
            // The views of this list's slots: its last flight has finished.
            let views = composer.views.of(&self.device, slot.map_or(0, |i| i as u32 + 1))?;
            let copy = match slot {
                Some(i) => &p.ahead[i].0,
                None => &p.copy,
            };
            copy.reset();
            copy.gpu_kind(super::gpu_split::Kind::Passes);
            let list = &copy.list;
            list.ResourceBarrier(&[transition(&back, D3D12_RESOURCE_STATE_PRESENT, D3D12_RESOURCE_STATE_RENDER_TARGET)]);
            let target = |f: u32| self.rtv(Some(&back), Some(&target_view(f)));
            record(&self.device, list, composer, &planes, (width, height), format.0 as u32, views, &target);
            // What is shown is read back from the buffer itself (saved
            // frames, the black-frame probe, the frame watch), and the panel
            // copied onto it.
            #[cfg(feature = "diagnostics")]
            let (capture, probe, watched, shot) = {
                list.ResourceBarrier(&[transition(&back, D3D12_RESOURCE_STATE_RENDER_TARGET, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
                let shot = shot.and_then(|seconds| Some((seconds, crate::shots::take(&self.device, list, &back)?)));
                let capture =
                    crate::capture::wanted(frame).then(|| crate::capture::read_back(&self.device, list, &back)).flatten();
                let probe = if slot.is_some() { None } else { crate::capture::probe(&self.device, list, &back) };
                let watched = crate::popwatch::take(&self.device, list, &back);
                list.ResourceBarrier(&[transition(&back, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COPY_DEST)]);
                p.panel.draw(self, copy, &back, (width, height), format);
                list.ResourceBarrier(&[transition(&back, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_PRESENT)]);
                (capture, probe, watched, shot)
            };
            #[cfg(not(feature = "diagnostics"))]
            let shot = match shot {
                Some(seconds) => {
                    list.ResourceBarrier(&[transition(&back, D3D12_RESOURCE_STATE_RENDER_TARGET, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
                    let read = crate::shots::take(&self.device, list, &back);
                    list.ResourceBarrier(&[transition(&back, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_PRESENT)]);
                    read.map(|read| (seconds, read))
                }
                None => {
                    list.ResourceBarrier(&[transition(&back, D3D12_RESOURCE_STATE_RENDER_TARGET, D3D12_RESOURCE_STATE_PRESENT)]);
                    None
                }
            };
            copy.close();
            let flown = match slot {
                Some(i) => {
                    let flown = self.execute_own(copy);
                    p.ahead[i].1 = flown;
                    flown
                }
                None => {
                    self.execute(&[copy]);
                    0
                }
            };
            #[cfg(feature = "diagnostics")]
            {
                if let Some(taken) = watched {
                    crate::popwatch::sent(taken, flown, frame);
                }
                if let Some((buffer, footprint)) = capture {
                    crate::capture::save_read_back(frame, &buffer, &footprint);
                }
                crate::replay::record::present(first.texture, D3D12_RESOURCE_STATES(first.state as i32));
            }
            if let Some((seconds, (buffer, footprint))) = shot {
                crate::shots::save(seconds, &buffer, &footprint);
            }
            #[cfg(not(feature = "diagnostics"))]
            let _ = (flown, first);
            let mut showing = crate::timeline::span("the host's Present");
            let status = p.swap_chain.Present(0, DXGI_PRESENT(0));
            showing.next("the frame meter");
            // A frame only if the swap chain took it, as `Host::present`
            // counts (after upstream 8699d96, `VICTORY_PRESENT_STATUS`).
            #[cfg(feature = "diagnostics")]
            if super::Meter::counts(status) {
                p.panel.frame(p.meter.line_due());
            }
            let presented = p.meter.record_present(p.hwnd, status);
            showing.end();
            if !presented {
                return None;
            }
            #[cfg(feature = "diagnostics")]
            return Some(probe.is_some_and(|b| crate::capture::black(&b)));
            #[cfg(not(feature = "diagnostics"))]
            Some(false)
        }
    }

    /// The presenter, made or its buffers made again for a picture of that
    /// size and format, as `Host::present` does.
    fn ready<'a>(
        &self,
        presenter: &'a mut Option<Presenter>,
        (width, height): (u32, u32),
        format: DXGI_FORMAT,
        frame: u64,
    ) -> Option<&'a mut Presenter> {
        if presenter.is_none() {
            *presenter = match self.presenter(width, height, format) {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!(error = %e, "no swap chain");
                    return None;
                }
            };
        }
        let p = presenter.as_mut()?;
        if (p.width, p.height, p.format) != (width, height, format) {
            // Presents sent ahead hold back buffers until they have run.
            settle_now(WHY_SYNC);
            // SAFETY: no back buffer is held; every submission was waited for.
            let resized = unsafe { p.swap_chain.ResizeBuffers(BUFFERS, width, height, format, DXGI_SWAP_CHAIN_FLAG(0)) };
            match resized {
                Ok(()) => {
                    (p.width, p.height, p.format) = (width, height, format);
                    tracing::info!(
                        width,
                        height,
                        format = format.0,
                        frame,
                        "the swap chain's buffers were made again for another size or format (planes put together)"
                    );
                    self.log_video_memory("right after the swap chain's buffers were made again");
                }
                Err(e) => {
                    tracing::warn!(error = %e, width, height, "swap chain not resized");
                    return None;
                }
            }
        }
        Some(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::Interface;

    /// The software adapter's device, if there is one.
    fn warp() -> Option<ID3D12Device> {
        use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
        use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, IDXGIAdapter, IDXGIFactory4};
        // SAFETY: plain DXGI queries and device creation on the software
        // adapter; a card is never opened.
        unsafe {
            let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
            let adapter: IDXGIAdapter = factory.EnumWarpAdapter().ok()?;
            let mut device: Option<ID3D12Device> = None;
            D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).ok()?;
            device
        }
    }

    /// Records with `record` into a list of the device's, runs it and waits.
    fn run(device: &ID3D12Device, record: impl FnOnce(&ID3D12GraphicsCommandList)) {
        use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
        // SAFETY: objects of our own, one list run and waited for.
        unsafe {
            let queue: ID3D12CommandQueue = device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() })
                .unwrap();
            let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
            let list: ID3D12GraphicsCommandList =
                device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None).unwrap();
            record(&list);
            list.Close().unwrap();
            queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            queue.Signal(&fence, 1).unwrap();
            let event = CreateEventW(None, false, false, None).unwrap();
            fence.SetEventOnCompletion(1, event).unwrap();
            assert_eq!(WaitForSingleObject(event, 10_000).0, 0, "the list ran");
            let _ = windows::Win32::Foundation::CloseHandle(event);
            device.GetDeviceRemovedReason().unwrap();
        }
    }

    /// The side of the test's textures, and the bytes between their rows in
    /// buffers.
    const SIDE: u32 = 4;
    const PITCH: u32 = 256;

    fn texture(device: &ID3D12Device, format: u32, flags: D3D12_RESOURCE_FLAGS, state: D3D12_RESOURCE_STATES) -> ID3D12Resource {
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: u64::from(SIDE),
            Height: SIDE,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT(format as i32),
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Flags: flags,
            ..Default::default()
        };
        let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() };
        let mut made: Option<ID3D12Resource> = None;
        // SAFETY: plain creation.
        unsafe { device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut made).unwrap() };
        made.unwrap()
    }

    fn buffer(device: &ID3D12Device, kind: D3D12_HEAP_TYPE, state: D3D12_RESOURCE_STATES) -> ID3D12Resource {
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: u64::from(SIDE * PITCH),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let heap = D3D12_HEAP_PROPERTIES { Type: kind, ..Default::default() };
        let mut made: Option<ID3D12Resource> = None;
        // SAFETY: plain creation.
        unsafe { device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut made).unwrap() };
        made.unwrap()
    }

    /// An upload buffer holding the texels `texel` gives, as rows.
    fn texels(device: &ID3D12Device, texel: impl Fn(u32, u32) -> [u8; 4]) -> ID3D12Resource {
        let up = buffer(device, D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ);
        let mut data = std::ptr::null_mut();
        // SAFETY: the buffer's mapping, of all its rows.
        unsafe {
            up.Map(0, None, Some(&mut data)).unwrap();
            for y in 0..SIDE {
                for x in 0..SIDE {
                    let at = data.cast::<u8>().add((y * PITCH + x * 4) as usize);
                    std::ptr::copy_nonoverlapping(texel(x, y).as_ptr(), at, 4);
                }
            }
            up.Unmap(0, None);
        }
        up
    }

    /// The texels in a read-back buffer, row by row.
    fn read(buffer: &ID3D12Resource) -> Vec<[u8; 4]> {
        let mut data = std::ptr::null_mut();
        // SAFETY: the buffer's mapping, after the list that filled it ran.
        unsafe {
            buffer.Map(0, None, Some(&mut data)).unwrap();
            let bytes = std::slice::from_raw_parts(data.cast::<u8>(), (SIDE * PITCH) as usize);
            let out = (0..SIDE * SIDE)
                .map(|i| {
                    let at = ((i / SIDE) * PITCH + (i % SIDE) * 4) as usize;
                    bytes[at..at + 4].try_into().unwrap()
                })
                .collect();
            buffer.Unmap(0, None);
            out
        }
    }

    fn rows(buffer: &ID3D12Resource, format: u32) -> D3D12_TEXTURE_COPY_LOCATION {
        D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                    Offset: 0,
                    Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                        Format: DXGI_FORMAT(format as i32),
                        Width: SIDE,
                        Height: SIDE,
                        Depth: 1,
                        RowPitch: PITCH,
                    },
                },
            },
        }
    }

    fn whole(texture: &ID3D12Resource) -> D3D12_TEXTURE_COPY_LOCATION {
        D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
        }
    }

    /// Puts together, on the software adapter, planes of `format` each
    /// filled by its `texel`, in the copy-source state (the first) and the
    /// shader-read one (the others), with the sources and destinations
    /// given, onto a picture of their size written as R8G8B8A8_UNORM through
    /// [`record`]: the picture's texels, row by row.
    fn put_together(
        device: &ID3D12Device,
        planes: &[(u32, &dyn Fn(u32, u32) -> [u8; 4], Option<[i32; 4]>, Option<[f32; 4]>)],
    ) -> Vec<[u8; 4]> {
        let mut composer = Composer::new(device).expect("the shaders, the root signature and the views");
        // Typeless, so that it has sRGB views too, as a swap chain's buffer does.
        let picture = texture(device, 27, D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET, D3D12_RESOURCE_STATE_RENDER_TARGET);
        let made: Vec<(ID3D12Resource, ID3D12Resource)> = planes
            .iter()
            .map(|&(format, texel, ..)| {
                (texture(device, format, D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_COPY_DEST), texels(device, texel))
            })
            .collect();
        let back = buffer(device, D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST);
        // SAFETY: plain heap creation and queries.
        let (targets, step) = unsafe {
            let heap: ID3D12DescriptorHeap = device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                    NumDescriptors: 2,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
                    NodeMask: 0,
                })
                .unwrap();
            (heap, device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV) as usize)
        };
        let target = |format: u32| {
            // SAFETY: a slot of our heap, a live texture.
            unsafe {
                let mut at = targets.GetCPUDescriptorHandleForHeapStart();
                at.ptr += step * usize::from(format == 29);
                device.CreateRenderTargetView(&picture, Some(&target_view(format)), at);
                at
            }
        };
        let views = composer.views.of(device, 0).unwrap();
        run(device, |list| {
            // SAFETY: an open list of ours; live resources in the states named.
            unsafe {
                for (k, ((texture, up), &(format, ..))) in made.iter().zip(planes).enumerate() {
                    list.CopyTextureRegion(&whole(texture), 0, 0, 0, &rows(up, format), None);
                    let state = if k == 0 { D3D12_RESOURCE_STATE_COPY_SOURCE } else { D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE };
                    list.ResourceBarrier(&[transition(texture, D3D12_RESOURCE_STATE_COPY_DEST, state)]);
                }
                let drawn: Vec<Drawn<'_>> = made
                    .iter()
                    .zip(planes)
                    .enumerate()
                    .map(|(k, ((texture, _), &(_, _, source, placement)))| {
                        Drawn::of(texture, if k == 0 { 0x800 } else { 0x80 }, source, placement).unwrap()
                    })
                    .collect();
                record(device, list, &mut composer, &drawn, (SIDE, SIDE), 28, views, &target);
                list.ResourceBarrier(&[transition(&picture, D3D12_RESOURCE_STATE_RENDER_TARGET, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
                list.CopyTextureRegion(&rows(&back, 27), 0, 0, 0, &whole(&picture), None);
            }
        });
        read(&back)
    }

    fn near(a: [u8; 4], b: [u8; 3]) -> bool {
        a.iter().zip(b).all(|(&a, b)| a.abs_diff(b) <= 1)
    }

    /// Two planes on the software adapter: the first as it is, the second
    /// over it through its (premultiplied) alpha, left and right, top and
    /// bottom where they were in their textures.
    #[test]
    fn two_planes_are_put_together_on_the_software_adapter() {
        let _turn = crate::indirect::tests::warp_turn();
        let Some(device) = warp() else {
            return;
        };
        assert_eq!(alpha(), Blend::Premultiplied, "VICTORY_PLANE_ALPHA is not set for the tests");
        let red = |_: u32, _: u32| [255, 0, 0, 255];
        // Half-alpha grey on the right, opaque blue in the top row's left
        // half, nothing elsewhere.
        let over = |x: u32, y: u32| match (x >= 2, y == 0) {
            (true, _) => [128, 128, 128, 128],
            (false, true) => [0, 0, 255, 255],
            _ => [0, 0, 0, 0],
        };
        let picture = put_together(&device, &[(28, &red, Some([0, 0, 4, 4]), Some(WHOLE)), (28, &over, None, None)]);
        for (i, &texel) in picture.iter().enumerate() {
            let (x, y) = (i as u32 % SIDE, i as u32 / SIDE);
            let want = match (x >= 2, y == 0) {
                (true, _) => [255, 128, 128],
                (false, true) => [0, 0, 255],
                _ => [255, 0, 0],
            };
            assert!(near(texel, want), "({x}, {y}): {texel:?}, not {want:?}");
        }
    }

    /// A plane placed on a quarter of the picture lands there and nowhere
    /// else; a part of its texture as the source fills its place.
    #[test]
    fn a_plane_lands_where_it_is_placed() {
        let _turn = crate::indirect::tests::warp_turn();
        let Some(device) = warp() else {
            return;
        };
        let red = |_: u32, _: u32| [255, 0, 0, 255];
        // Green in the left half, blue in the right; the source is the left
        // half, placed on the lower right quarter.
        let halves = |x: u32, _: u32| if x < 2 { [0, 255, 0, 255] } else { [0, 0, 255, 255] };
        let picture = put_together(&device, &[(28, &red, None, None), (28, &halves, Some([0, 0, 1, 4]), Some([0.5, 0.5, 1.0, 1.0]))]);
        for (i, &texel) in picture.iter().enumerate() {
            let (x, y) = (i as u32 % SIDE, i as u32 / SIDE);
            let want = if x >= 2 && y >= 2 { [0, 255, 0] } else { [255, 0, 0] };
            assert!(near(texel, want), "({x}, {y}): {texel:?}, not {want:?}");
        }
    }

    /// An sRGB plane read and written as sRGB lands with its bytes as they
    /// were, as a copy leaves them.
    #[test]
    fn an_srgb_plane_keeps_its_bytes() {
        let _turn = crate::indirect::tests::warp_turn();
        let Some(device) = warp() else {
            return;
        };
        let texel = |x: u32, y: u32| [(40 * x + 10) as u8, (60 * y + 5) as u8, 200, 255];
        let picture = put_together(&device, &[(29, &texel, None, None)]);
        for (i, &got) in picture.iter().enumerate() {
            let (x, y) = (i as u32 % SIDE, i as u32 / SIDE);
            let want = texel(x, y);
            assert!(near(got, [want[0], want[1], want[2]]), "({x}, {y}): {got:?}, not {want:?}");
        }
    }

    /// The picture is as large as the largest plane each way.
    #[test]
    fn planes_go_onto_a_picture_as_large_as_the_largest() {
        assert_eq!(picture(&[(1920, 1080)]), (1920, 1080));
        assert_eq!(picture(&[(1280, 720), (1920, 1080)]), (1920, 1080));
        assert_eq!(picture(&[(1920, 800), (1600, 1080)]), (1920, 1080));
        assert_eq!(picture(&[]), (1, 1));
    }

    /// A source in pixels becomes the texture coordinates of its corners; one
    /// that is not inside the texture, or none, the whole texture.
    #[test]
    fn sources_become_texture_coordinates() {
        assert_eq!(source_uv(Some([0, 0, 1920, 1080]), (1920, 1080)), [0.0, 0.0, 1.0, 1.0]);
        assert_eq!(source_uv(Some([480, 270, 1440, 810]), (1920, 1080)), [0.25, 0.25, 0.75, 0.75]);
        assert_eq!(source_uv(Some([0, 0, 960, 540]), (1920, 1080)), [0.0, 0.0, 0.5, 0.5]);
        assert_eq!(source_uv(None, (1920, 1080)), [0.0, 0.0, 1.0, 1.0]);
        for outside in [[0, 0, 1921, 1080], [-1, 0, 100, 100], [100, 0, 100, 100], [0, 50, 10, 40]] {
            assert_eq!(source_uv(Some(outside), (1920, 1080)), [0.0, 0.0, 1.0, 1.0], "{outside:?}");
        }
    }

    /// A destination in fractions of the picture becomes a viewport in its
    /// pixels, and the scissor the pixels it touches.
    #[test]
    fn destinations_become_viewports_and_scissors() {
        let picture = (1920, 1080);
        assert_eq!(viewport(Some(WHOLE), picture), [0.0, 0.0, 1920.0, 1080.0]);
        assert_eq!(viewport(None, picture), [0.0, 0.0, 1920.0, 1080.0]);
        assert_eq!(viewport(Some([0.5, 0.25, 1.0, 0.75]), picture), [960.0, 270.0, 960.0, 540.0]);
        // Not a destination: the whole picture.
        for not in [[0.0, 0.0, 2.0, 1.0], [0.5, 0.0, 0.5, 1.0], [f32::NAN, 0.0, 1.0, 1.0], [-0.1, 0.0, 1.0, 1.0]] {
            assert_eq!(viewport(Some(not), picture), [0.0, 0.0, 1920.0, 1080.0], "{not:?}");
        }
        assert_eq!(scissor([0.0, 0.0, 1920.0, 1080.0], picture), [0, 0, 1920, 1080]);
        assert_eq!(scissor([960.0, 270.0, 960.0, 540.0], picture), [960, 270, 1920, 810]);
        assert_eq!(scissor([10.5, 0.25, 100.0, 2000.0], picture), [10, 0, 111, 1080]);
        assert_eq!(scissor([-5.0, -5.0, 10.0, 10.0], picture), [0, 0, 5, 5]);
    }

    /// The first plane goes on as it is, the others through their alpha as
    /// `VICTORY_PLANE_ALPHA` says.
    #[test]
    fn planes_after_the_first_are_blended() {
        assert_eq!(alpha_by(None), Blend::Premultiplied);
        assert_eq!(alpha_by(Some("premultiplied")), Blend::Premultiplied);
        assert_eq!(alpha_by(Some(" straight")), Blend::Straight);
        assert_eq!(blend_of(0, Blend::Straight), Blend::Opaque);
        assert_eq!(blend_of(1, Blend::Straight), Blend::Straight);
        assert_eq!(blend_of(1, Blend::Premultiplied), Blend::Premultiplied);
    }

    /// A typeless texture is read through its plain format, an sRGB one as
    /// sRGB and written through the picture's sRGB twin, so its bytes land
    /// as they are; what holds no colour is not read.
    #[test]
    fn planes_are_read_as_their_bytes_say() {
        assert_eq!(read_format(27), Some((28, false)));
        assert_eq!(read_format(28), Some((28, false)));
        assert_eq!(read_format(29), Some((29, true)));
        assert_eq!(read_format(90), Some((87, false)));
        assert_eq!(read_format(23), Some((24, false)));
        assert_eq!(read_format(9), Some((10, false)));
        for no in [0, 19, 40, 45, 30, 57, 61] {
            assert_eq!(read_format(no), None, "{no}");
        }
        assert_eq!(write_format(28, true), 29);
        assert_eq!(write_format(87, true), 91);
        assert_eq!(write_format(28, false), 28);
        assert_eq!(write_format(24, true), 24);
    }
}
