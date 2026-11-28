//! The twin of a depth plane, brought up to date where the plane or the
//! twin is first used (`VICTORY_TWIN_INLINE`).
//!
//! A depth texture with a stencil plane that the title also writes through
//! unordered access has a colour twin for each plane
//! ([`crate::resource::Twin`]). The stencil plane's is copied through a
//! buffer; the depth plane's cannot be copied (no host colour format holds a
//! 24-bit depth plane), so a pass of ours draws the twin into the plane
//! (`record_plane_pass`). That pass sets a pipeline and a depth target of
//! ours, and was therefore left for the list's close: a draw in the list
//! that had written the twin tested against the plane as the list found it.
//! A title's tiled lighting does exactly that, once a frame ("used before the
//! pass" and "not given back to a twin" each grew by one a frame): it writes
//! each
//! pixel's class into the twin, then draws each class with the depth test
//! "equal", against the classes of the frame before.
//!
//! Here the pass is recorded where the plane is first used after shaders
//! wrote its twin ([`HostList::twin_use`]), and the host list notes that its
//! state is no longer the title's ([`HostList::state_lost`]): the next draw
//! or dispatch sets the title's again (`crate::draw::State::bind`, with
//! `State::replay`, as after a clear of unordered access). The other way, a
//! twin first used after draws or clears wrote the plane is given the plane
//! by a compute pass ([`record_plane_back`]), in the list or, if an earlier
//! list wrote the plane, in the list's prelude.
//!
//! What a pass costs: a triangle or a dispatch over the texture, once a
//! frame each; the pass at the list's close is then not
//! recorded, the plane being up to date.

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;

use super::*;

/// Whether a depth plane and its twin are brought up to date where they are
/// first used. `VICTORY_TWIN_INLINE=0`: the plane is drawn from its twin
/// when the list closes and the twin is never given the plane, as before
/// (both are counted in the frame-rate line then).
pub(super) fn inline() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TWIN_INLINE").is_none_or(|v| v != "0"))
}

pub(super) const UNORDERED_ACCESS: u32 = 0x8;
const NON_PIXEL_SHADER_RESOURCE: u32 = 0x40;
const COPY_SOURCE: u32 = 0x800;
/// A texture no shader may read (`D3D12_RESOURCE_FLAG_DENY_SHADER_RESOURCE`).
const DENIES_SHADERS: u32 = 0x8;

/// Since the last frame-rate line: depth planes drawn from their twins
/// where the plane was first used (not at the list's close), and twins
/// given their planes.
pub(super) static INLINE: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Host lists whose state a pass of ours took and that no draw or dispatch
/// has set again yet: while there is none (all but a few moments of a
/// frame), [`HostList::take_state_lost`] asks no list.
static LOST: AtomicU32 = AtomicU32::new(0);

/// What a host list does with its depth textures that have twins, by twin,
/// and whether a pass of ours recorded into it took its state
/// ([`HostList::state_lost`]). Read as the map it holds.
#[derive(Default)]
pub(super) struct Uses {
    by_twin: HashMap<usize, TwinUse>,
    state_lost: bool,
}

impl std::ops::Deref for Uses {
    type Target = HashMap<usize, TwinUse>;
    fn deref(&self) -> &Self::Target {
        &self.by_twin
    }
}

impl std::ops::DerefMut for Uses {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.by_twin
    }
}

impl Uses {
    /// The list is reset: no use, and nothing of ours set. The uses are
    /// handed out: a reset that keeps what the recording held until the
    /// bury lag has passed (`HostList::reset_ran`) keeps them too.
    pub(super) fn take_uses(&mut self) -> HashMap<usize, TwinUse> {
        self.found();
        std::mem::take(&mut self.by_twin)
    }

    /// The state is the title's again (or nobody's: a list reset or gone).
    fn found(&mut self) -> bool {
        let was = std::mem::take(&mut self.state_lost);
        if was {
            LOST.fetch_sub(1, Ordering::Relaxed);
        }
        was
    }
}

impl Drop for Uses {
    fn drop(&mut self) {
        self.found();
    }
}

impl HostList {
    /// A pass of ours was just recorded into this list where the title's
    /// commands go on: what it set (descriptor heaps, a root signature, the
    /// pipeline, a depth target without colour targets, one viewport and
    /// scissor, the topology) is no longer what the title set.
    pub(crate) fn state_lost(&self) {
        let mut twins = self.twins.lock().unwrap_or_else(|e| e.into_inner());
        if !std::mem::replace(&mut twins.state_lost, true) {
            LOST.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Whether a pass of ours took this list's state since this was last
    /// asked ([`HostList::state_lost`]). Asked by every draw and dispatch
    /// before and after it binds: if so, it sets what the title set again
    /// (`crate::draw::State::replay`) and binds anew.
    pub(crate) fn take_state_lost(&self) -> bool {
        if LOST.load(Ordering::Relaxed) == 0 {
            return false;
        }
        self.twins.lock().unwrap_or_else(|e| e.into_inner()).found()
    }

    /// [`HostList::twin_use`] of title texture `resource` as the depth
    /// target of a draw about to be recorded, through a view that writes
    /// neither plane (`tested`) or one that may write.
    /// A view that cannot write leaves the twin as current as it was: the
    /// title's lighting draws against the plane only test it, and counting
    /// them as writes had the twin given the plane again every frame.
    /// shortcut: a view that may write still counts as a write, also for a
    /// pipeline that only tests. With `VICTORY_TWIN_INLINE=0` every depth
    /// target counts as written, as before.
    pub(crate) fn twin_target(&self, resource: crate::object::This, tested: bool) -> bool {
        self.twin_use(resource, false, !(tested && inline()))
    }

    /// Records the pass that gives the twin of the depth plane of `depth`
    /// the plane, if `twin` has one, with the moves to the states the pass
    /// needs: the depth texture is left readable by compute shaders, the
    /// twin in the unordered access state. The pass takes the list's state
    /// ([`HostList::state_lost`]).
    pub(super) fn plane_back(
        &self,
        depth: &std::sync::Arc<crate::resource::HostTexture>,
        twin: &crate::resource::Twin,
    ) {
        let Some(plane) = &twin.plane else { return };
        if depth.flags() & DENIES_SHADERS != 0 {
            crate::resource::PLANES[1].fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.settle(depth, NON_PIXEL_SHADER_RESOURCE);
        self.settle(plane, UNORDERED_ACCESS);
        if record_plane_back(&self.list, &depth.texture, &plane.texture) {
            INLINE[1].fetch_add(1, Ordering::Relaxed);
            self.state_lost();
        } else {
            crate::resource::PLANES[1].fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Whether a prelude that copies depth texture `depth` into its twin
/// (`colour`) also gives the twin of its depth plane, if it has one, the
/// plane.
pub(super) fn plane_back_in_prelude(depth: &crate::resource::HostTexture, colour: bool) -> bool {
    colour && inline() && depth.flags() & DENIES_SHADERS == 0
}

/// [`record_plane_back`] in a prelude, after the copy of `depth` into its
/// twin: `depth` is in the copy source state and is left in it, `plane` in
/// the unordered access state. A prelude holds nothing of the title's, so
/// no state is lost.
pub(super) fn plane_back_from_source(
    list: &ID3D12GraphicsCommandList,
    depth: &ID3D12Resource,
    plane: &ID3D12Resource,
) {
    let state = |s: u32| D3D12_RESOURCE_STATES(s as i32);
    let (copied, read) = (state(COPY_SOURCE), state(NON_PIXEL_SHADER_RESOURCE));
    // SAFETY: an open list and a live texture, in the first of the states.
    unsafe { list.ResourceBarrier(&[transition(depth, copied, read)]) };
    if record_plane_back(list, depth, plane) {
        INLINE[1].fetch_add(1, Ordering::Relaxed);
    } else {
        crate::resource::PLANES[1].fetch_add(1, Ordering::Relaxed);
    }
    // SAFETY: as above.
    unsafe { list.ResourceBarrier(&[transition(depth, read, copied)]) };
}

/// The pass the other way: a compute shader reads each texel of the depth
/// plane and stores it in the plane's twin, a group of 8 by 8 texels a
/// wave.
///
/// A 16-bit twin holds whole steps of 1/65535: the step nearest the depth.
/// The plane's pass writes step times the float nearest 1/65535
/// (`PLANE_STEPS`), which the host keeps as one of 2^24 - 1 depths, at most
/// half of one off: 1/512 of a step. So a twin drawn into the plane and
/// given the plane again holds what it held, for every step (checked by
/// `every_step_comes_back`).
const BACK_STEPS: &str = r#"
#define RS "DescriptorTable(SRV(t0)), DescriptorTable(UAV(u0))"
Texture2DArray<float> plane : register(t0);
RWTexture2DArray<uint> twin : register(u0);
[RootSignature(RS)]
[numthreads(8, 8, 1)]
void back(uint3 id : SV_DispatchThreadID) {
    twin[uint3(id.xy, 0)] = uint(plane.Load(int4(id.xy, 0, 0)) * 65535.0 + 0.5);
}
"#;
const BACK_FLOATS: &str = r#"
#define RS "DescriptorTable(SRV(t0)), DescriptorTable(UAV(u0))"
Texture2DArray<float> plane : register(t0);
RWTexture2DArray<float> twin : register(u0);
[RootSignature(RS)]
[numthreads(8, 8, 1)]
void back(uint3 id : SV_DispatchThreadID) {
    twin[uint3(id.xy, 0)] = plane.Load(int4(id.xy, 0, 0));
}
"#;

/// The pass's root signature and pipeline for a twin of 16-bit steps
/// (`steps`) or of floats, made when first needed (`None`: the host
/// refused, and such twins stay as they are).
fn back_pipeline(steps: bool) -> Option<PlanePipeline> {
    static MADE: [Mutex<Option<Option<PlanePipeline>>>; 2] = [const { Mutex::new(None) }; 2];
    let mut made = MADE[usize::from(steps)].lock().unwrap_or_else(|e| e.into_inner());
    made.get_or_insert_with(|| match make_back(steps) {
        Ok(pipeline) => Some(pipeline),
        Err(e) => {
            tracing::warn!(
                error = %e,
                steps,
                "no pipeline to give a depth plane's twin the plane: such twins stay as they are"
            );
            None
        }
    })
    .clone()
}

fn make_back(steps: bool) -> windows::core::Result<PlanePipeline> {
    use windows::core::s;
    let device = &host().ok_or_else(windows::core::Error::empty)?.device;
    let source = if steps { BACK_STEPS } else { BACK_FLOATS };
    let cs = compile(source, s!("back"), s!("cs_5_1"))?;
    // SAFETY: the root signature is inside the shader; the description
    // points at live data and never releases its pointer.
    unsafe {
        let root: ID3D12RootSignature = device.CreateRootSignature(0, &cs[..])?;
        let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(&root),
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: cs.as_ptr().cast(),
                BytecodeLength: cs.len(),
            },
            ..Default::default()
        };
        let pipeline: ID3D12PipelineState = device.CreateComputePipelineState(&desc)?;
        Ok((root, pipeline))
    }
}

/// Records into `list` the pass that gives `plane`, the twin of the depth
/// plane of `depth`, that plane: `depth` readable by compute shaders,
/// `plane` in the unordered access state. False if nothing was recorded
/// (no pipeline, or a depth format without a view of its depth plane).
/// The pass sets the list's descriptor heap, compute root signature and
/// pipeline and puts none of them back, as `record_plane_pass`.
fn record_plane_back(
    list: &ID3D12GraphicsCommandList,
    depth: &ID3D12Resource,
    plane: &ID3D12Resource,
) -> bool {
    let (Some(host), Some(pass)) = (host(), plane_pass()) else {
        return false;
    };
    // SAFETY: live resources.
    let (of, twin) = unsafe { (depth.GetDesc(), plane.GetDesc()) };
    // An R16 typeless twin (53) holds steps, written as R16_UINT (57); an
    // R32 typeless one floats, written as R32_FLOAT (41). The depth plane
    // is read in the format of its family that shows it alone (R24G8, 44
    // and 45: 46; R32G8X24, 19 and 20: 21).
    let steps = twin.Format.0 == 53;
    let shown = DXGI_FORMAT(match of.Format.0 {
        44 | 45 => 46,
        19 | 20 => 21,
        _ => return false,
    });
    let Some((root, pipeline)) = back_pipeline(steps) else {
        return false;
    };
    for slice in 0..u32::from(of.DepthOrArraySize) {
        for mip in 0..u32::from(of.MipLevels) {
            let read = D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: shown,
                ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2DARRAY,
                Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    Texture2DArray: D3D12_TEX2D_ARRAY_SRV {
                        MostDetailedMip: mip,
                        MipLevels: 1,
                        FirstArraySlice: slice,
                        ArraySize: 1,
                        ..Default::default()
                    },
                },
            };
            let written = D3D12_UNORDERED_ACCESS_VIEW_DESC {
                Format: DXGI_FORMAT(if steps { 57 } else { 41 }),
                ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2DARRAY,
                Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                    Texture2DArray: D3D12_TEX2D_ARRAY_UAV {
                        MipSlice: mip,
                        FirstArraySlice: slice,
                        ArraySize: 1,
                        PlaneSlice: 0,
                    },
                },
            };
            // Two slots of the ring the plane's pass takes its views from,
            // each a table of one view.
            let [cpu, gpu] = [0, 1].map(|_| {
                let slot = pass.next.fetch_add(1, Ordering::Relaxed) % PLANE_VIEWS;
                (pass.cpu + (slot * pass.step) as usize, pass.gpu + u64::from(slot * pass.step))
            });
            let handle = |(ptr, _): (usize, u64)| D3D12_CPU_DESCRIPTOR_HANDLE { ptr };
            let table = |(_, ptr): (usize, u64)| D3D12_GPU_DESCRIPTOR_HANDLE { ptr };
            let (width, height) = ((of.Width >> mip).max(1) as u32, (of.Height >> mip).max(1));
            // SAFETY: an open list; slots of our heap; live resources and
            // objects (the pass's own live as long as the process).
            unsafe {
                host.device.CreateShaderResourceView(depth, Some(&read), handle(cpu));
                host.device
                    .CreateUnorderedAccessView(plane, None, Some(&written), handle(gpu));
                list.SetDescriptorHeaps(&[Some(pass.views.clone())]);
                list.SetComputeRootSignature(&root);
                list.SetPipelineState(&pipeline);
                list.SetComputeRootDescriptorTable(0, table(cpu));
                list.SetComputeRootDescriptorTable(1, table(gpu));
                list.Dispatch(width.div_ceil(8), height.div_ceil(8), 1);
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::This;
    use windows::Win32::Foundation::RECT;

    const RENDER_TARGET: u32 = 0x4;
    const DEPTH_WRITE: u32 = 0x10;
    const COPY_DEST: u32 = 0x400;

    fn state(s: u32) -> D3D12_RESOURCE_STATES {
        D3D12_RESOURCE_STATES(s as i32)
    }

    /// A title texture of 4 by 4 texels in the title's `format`, with the
    /// title's `flags` (1: a render target; 6: depth and unordered access).
    fn title_texture(format: i32, flags: i32, first: u32) -> This {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 4,
            Height: 4,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT(format),
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Flags: D3D12_RESOURCE_FLAGS(flags),
            ..Default::default()
        };
        // ID3D12Resource, as the title names it.
        let iid = crate::object::guid(0x696442be, 0xa72e, 0x4059, [0xbc, 0x79, 0x5b, 0x5c, 0x98, 0x04, 0x0f, 0xad]);
        let mut made = std::ptr::null();
        // (The title's descriptions have the host's layouts: the console's
        // API is the host's.)
        let result = crate::resource::create_committed(
            std::ptr::null(),
            (&raw const heap).cast(),
            0,
            (&raw const desc).cast(),
            first,
            std::ptr::null(),
            &iid,
            &mut made,
        );
        assert_eq!(result, 0);
        made
    }

    /// A buffer of 1024 bytes to read back into.
    fn readback(host: &Host) -> ID3D12Resource {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_READBACK,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: 1024,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut made: Option<ID3D12Resource> = None;
        // SAFETY: plain creation of a buffer.
        unsafe {
            host.device
                .CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, state(COPY_DEST), None, &mut made)
                .unwrap();
        }
        made.unwrap()
    }

    /// The one subresource of a texture of 4 by 4 texels.
    fn whole(texture: &ID3D12Resource) -> D3D12_TEXTURE_COPY_LOCATION {
        D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
        }
    }

    /// Such a texture's rows in a buffer, 256 bytes apart, in `format`.
    fn rows(buffer: &ID3D12Resource, format: i32) -> D3D12_TEXTURE_COPY_LOCATION {
        D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                    Offset: 0,
                    Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                        Format: DXGI_FORMAT(format),
                        Width: 4,
                        Height: 4,
                        Depth: 1,
                        RowPitch: 256,
                    },
                },
            },
        }
    }

    /// The 1024 bytes of a read-back buffer, after the lists that wrote it ran.
    fn bytes_of(buffer: &ID3D12Resource) -> Vec<u8> {
        let mut mapped = std::ptr::null_mut();
        let range = D3D12_RANGE { Begin: 0, End: 1024 };
        // SAFETY: the copies finished (execute waits); the buffer is 1024 bytes.
        unsafe {
            buffer.Map(0, Some(&range), Some(&mut mapped)).unwrap();
            let bytes = std::slice::from_raw_parts(mapped.cast::<u8>(), 1024).to_vec();
            buffer.Unmap(0, None);
            bytes
        }
    }

    /// Records into `list`, where a shader of the title's would write them,
    /// 16 class numbers into the twin of a depth plane (16 bits each): the
    /// twin is in the unordered access state before and after. The buffer
    /// they come from must live until the list has run.
    fn classes_into(host: &Host, list: &HostList, plane: &ID3D12Resource, classes: [u16; 16]) -> ID3D12Resource {
        let mut pattern = vec![0u8; 1024];
        for (i, class) in classes.into_iter().enumerate() {
            let at = i / 4 * 256 + i % 4 * 2;
            pattern[at..at + 2].copy_from_slice(&class.to_le_bytes());
        }
        let (upload, data) = host.upload_buffer(1024).unwrap();
        // SAFETY: the buffer's mapping, 1024 bytes; an open list and live
        // resources.
        unsafe {
            std::ptr::copy_nonoverlapping(pattern.as_ptr(), data, 1024);
            let (written, dest) = (state(UNORDERED_ACCESS), state(COPY_DEST));
            list.list.ResourceBarrier(&[transition(plane, written, dest)]);
            list.list.CopyTextureRegion(&whole(plane), 0, 0, 0, &rows(&upload, 53), None);
            list.list.ResourceBarrier(&[transition(plane, dest, written)]);
        }
        upload
    }

    /// A step drawn into a 24-bit depth plane by the plane's pass and read
    /// back by the pass the other way is the same step, for every step,
    /// also if the host keeps a depth one of its 2^24 - 1 off (it may: its
    /// conversion is exact to 0.6 of one).
    #[test]
    fn every_step_comes_back() {
        let unit = f32::from_bits(0x3780_0080);
        for step in 0..=65535u32 {
            let depth = step as f32 * unit;
            let kept = (f64::from(depth) * 16_777_215.0).round() as i64;
            for kept in [kept - 1, kept, kept + 1] {
                let read = (kept.clamp(0, 16_777_215) as f64 / 16_777_215.0) as f32;
                assert_eq!((read * 65535.0 + 0.5) as u32, step, "step {step}, kept as {kept}");
            }
        }
    }

    /// The title's tiled lighting in one list: a shader writes class
    /// numbers through the twin of a depth plane, and draws with the depth
    /// test "equal" then find those classes in the plane, not the ones the
    /// list before left there. The pass that draws the twin into the plane
    /// in between takes the list's state: after `State::replay` the draws
    /// reach the title's target again, inside the title's viewport and
    /// scissor, and the next bind sets heaps, root signatures and pipeline.
    #[test]
    fn a_plane_is_drawn_from_its_twin_before_a_draw_tests_it() {
        use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY;
        use windows::core::s;
        let Some(host) = host() else {
            return; // no host GPU here
        };
        if !inline() {
            return; // switched off for this run
        }
        let depth = title_texture(118, 6, DEPTH_WRITE);
        let Some(plane) = crate::resource::twin_plane_resource(depth) else {
            return; // these twins are turned off
        };
        let target = title_texture(61, 1, RENDER_TARGET);
        let host_target = crate::resource::host_resource(target).unwrap();
        // The title's views of the two: descriptor slots of 128 bytes.
        let mut slots = vec![0u64; 32];
        let handles = [slots.as_mut_ptr() as u64, slots.as_mut_ptr() as u64 + 128];
        let null = std::ptr::null();
        crate::descriptor::create_rtv(null, target, null.cast(), handles[0]);
        crate::descriptor::create_dsv(null, depth, null.cast(), handles[1]);

        // The list before: class 65 everywhere, drawn into the plane when
        // the list closes.
        let before = host.new_list().unwrap();
        assert!(before.twin_use(depth, true, true));
        let _from = classes_into(host, &before, plane, [65; 16]);
        before.twins_written();
        before.close();
        host.execute(&[&before]);

        const CLASSES: [u16; 16] = [8, 65, 128, 129, 65, 8, 129, 128, 0, 255, 65, 64, 129, 130, 8, 65];
        const SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants = 1, b0)"
cbuffer Drawn : register(b0) { uint number; };
[RootSignature(RS)]
float4 corner(uint i : SV_VertexID) : SV_Position {
    float z = float(number) * asfloat(0x37800080);
    return float4(float(i & 1) * 4.0 - 1.0, float(i >> 1) * 4.0 - 1.0, z, 1.0);
}
float4 lit() : SV_Target { return 1.0; }
"#;
        let vs = compile(SOURCE, s!("corner"), s!("vs_5_1")).unwrap();
        let ps = compile(SOURCE, s!("lit"), s!("ps_5_1")).unwrap();
        let code = |bytes: &[u8]| D3D12_SHADER_BYTECODE {
            pShaderBytecode: bytes.as_ptr().cast(),
            BytecodeLength: bytes.len(),
        };
        let mut blend = D3D12_BLEND_DESC::default();
        blend.RenderTarget[0].RenderTargetWriteMask = 0xf;
        let mut formats = [DXGI_FORMAT(0); 8];
        formats[0] = DXGI_FORMAT(61);
        // SAFETY: plain creation; the description points at live data.
        let (signature, pipeline) = unsafe {
            let signature: ID3D12RootSignature = host.device.CreateRootSignature(0, &vs[..]).unwrap();
            let desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&signature),
                VS: code(&vs[..]),
                PS: code(&ps[..]),
                BlendState: blend,
                SampleMask: u32::MAX,
                RasterizerState: D3D12_RASTERIZER_DESC {
                    FillMode: D3D12_FILL_MODE_SOLID,
                    CullMode: D3D12_CULL_MODE_NONE,
                    DepthClipEnable: true.into(),
                    ..Default::default()
                },
                // As the title's lighting: test "equal", nothing written.
                DepthStencilState: D3D12_DEPTH_STENCIL_DESC {
                    DepthEnable: true.into(),
                    DepthWriteMask: D3D12_DEPTH_WRITE_MASK_ZERO,
                    DepthFunc: D3D12_COMPARISON_FUNC_EQUAL,
                    ..Default::default()
                },
                PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                NumRenderTargets: 1,
                RTVFormats: formats,
                DSVFormat: DXGI_FORMAT(45),
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                ..Default::default()
            };
            let pipeline: ID3D12PipelineState = host.device.CreateGraphicsPipelineState(&desc).unwrap();
            (signature, pipeline)
        };
        let back = readback(host);

        // The list: what the title sets, the shader's write, the draws.
        let list = host.new_list().unwrap();
        let mut title = crate::draw::State::default();
        title.set_topology(&list, 4);
        title.set_targets(&list, &handles[..1], handles[1]);
        // The three upper rows, and of them the two left columns.
        title.set_viewports(
            &list,
            &[D3D12_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: 4.0,
                Height: 3.0,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }],
        );
        title.set_scissors(&list, &[RECT { left: 0, top: 0, right: 2, bottom: 4 }]);
        crate::list::ensure_state(&list, target, RENDER_TARGET);
        // SAFETY: an open list and a view just made of a live texture.
        unsafe {
            list.list
                .ClearRenderTargetView(host.rtv(Some(host_target), None), &[0.0; 4], None);
        }
        assert!(list.twin_use(depth, true, true));
        let _from = classes_into(host, &list, plane, CLASSES);
        list.twins_written();
        assert!(!list.take_state_lost());
        // A draw's preparation: its depth target is brought up to date.
        assert!(list.twin_target(depth, false));
        assert!(list.take_state_lost(), "the plane's pass was recorded before the draw");
        title.replay(&list);
        assert!(title.host_has_nothing());
        assert!(!list.take_state_lost());
        crate::list::ensure_state(&list, depth, DEPTH_WRITE);
        // SAFETY: an open list and live objects; the root signature and the
        // pipeline are what a bind sets after a replay.
        unsafe {
            let l = &list.list;
            l.SetGraphicsRootSignature(&signature);
            l.SetPipelineState(&pipeline);
            // (The same topology the title set: a host list keeps one.)
            l.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY(4));
            for class in [65u32, 129] {
                l.SetGraphicsRoot32BitConstant(0, class, 0);
                l.DrawInstanced(3, 1, 0, 0);
            }
        }
        crate::list::ensure_state(&list, target, COPY_SOURCE);
        // SAFETY: an open list and live resources.
        unsafe {
            list.list
                .CopyTextureRegion(&rows(&back, 61), 0, 0, 0, &whole(host_target), None);
        }
        list.close();
        host.execute(&[&list]);
        let read = bytes_of(&back);
        for (i, class) in CLASSES.into_iter().enumerate() {
            let (row, column) = (i / 4, i % 4);
            let covered = read[row * 256 + column] != 0;
            let drawn = matches!(class, 65 | 129) && row < 3 && column < 2;
            assert_eq!(covered, drawn, "texel {i}, class {class}");
        }
        drop(slots);
    }

    /// The other way, for a twin of 16-bit steps and one of floats: a
    /// clear writes the depth plane, and the twin a shader then uses holds
    /// what was cleared to, in the same list (a pass in the list, which
    /// takes its state) and in a later one (a pass in that list's prelude).
    #[test]
    fn a_twin_is_given_its_plane_before_a_shader_uses_it() {
        let Some(host) = host() else {
            return; // no host GPU here
        };
        if !inline() {
            return; // switched off for this run
        }
        // (The title's format, the view's, the twin's in a buffer, and two
        // depths with what the twin then holds as 32 bits.)
        let unit = f32::from_bits(0x3780_0080);
        let cases = [
            (118, 45, 57, [(77.0 * unit, 77u32), (40000.0 * unit, 40000)]),
            (19, 20, 41, [(0.25, 0.25f32.to_bits()), (0.75, 0.75f32.to_bits())]),
        ];
        for (format, viewed, kept, depths) in cases {
            let depth = title_texture(format, 6, DEPTH_WRITE);
            let Some(twin) = crate::resource::twin(depth).filter(|t| t.plane.is_some()) else {
                return; // these twins are turned off
            };
            let plane = twin.plane.as_ref().unwrap();
            let host_depth = crate::resource::host_resource(depth).unwrap();
            let view = D3D12_DEPTH_STENCIL_VIEW_DESC {
                Format: DXGI_FORMAT(viewed),
                ViewDimension: D3D12_DSV_DIMENSION_TEXTURE2D,
                ..Default::default()
            };
            let back = readback(host);
            // The title clears the plane to `to` in `list`.
            let cleared = |list: &HostList, to: f32| {
                assert!(list.twin_use(depth, false, true));
                crate::list::ensure_state(list, depth, DEPTH_WRITE);
                let view = host.dsv(host_depth, Some(&view));
                // SAFETY: an open list and a view just made of a live texture.
                unsafe {
                    list.list
                        .ClearDepthStencilView(view, D3D12_CLEAR_FLAG_DEPTH, to, 0, None);
                }
            };
            // What the twin holds once the lists so far have run.
            let held = || {
                let now = state(plane.state.load(Ordering::Relaxed));
                let copied = state(COPY_SOURCE);
                let list = host.new_list().unwrap();
                // SAFETY: an open list and live resources.
                unsafe {
                    let l = &list.list;
                    l.ResourceBarrier(&[transition(&plane.texture, now, copied)]);
                    l.CopyTextureRegion(&rows(&back, kept), 0, 0, 0, &whole(&plane.texture), None);
                    l.ResourceBarrier(&[transition(&plane.texture, copied, now)]);
                }
                list.close();
                host.execute(&[&list]);
                let read = bytes_of(&back);
                let size = if kept == 57 { 2 } else { 4 };
                (0..16usize)
                    .map(|i| {
                        let at = i / 4 * 256 + i % 4 * size;
                        let mut word = [0u8; 4];
                        word[..size].copy_from_slice(&read[at..at + size]);
                        u32::from_le_bytes(word)
                    })
                    .collect::<Vec<_>>()
            };

            // In one list: the clear, then the shader's use of the twin.
            let list = host.new_list().unwrap();
            cleared(&list, depths[0].0);
            assert!(!list.take_state_lost());
            assert!(list.twin_use(depth, true, true));
            assert!(list.take_state_lost(), "the pass was recorded before the shader's use");
            list.twins_written();
            list.close();
            host.execute(&[&list]);
            assert_eq!(held(), [depths[0].1; 16], "format {format}, in the list");

            // In two lists: the second finds the twin behind.
            let first = host.new_list().unwrap();
            cleared(&first, depths[1].0);
            first.close();
            host.execute(&[&first]);
            let second = host.new_list().unwrap();
            assert!(second.twin_use(depth, true, true));
            assert!(!second.take_state_lost(), "a prelude's pass takes no state of the list's");
            second.twins_written();
            second.close();
            host.execute(&[&second]);
            assert_eq!(held(), [depths[1].1; 16], "format {format}, in the prelude");
        }
    }
}
