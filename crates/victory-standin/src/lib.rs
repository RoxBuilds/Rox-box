//! The title side of a stand-in title: what a small D3D12.X title does,
//! written once and shared by `victory selftest` (`victory_gpu::selftest`,
//! in-process) and the open demo title (`tests/fixtures/demo-title`, a real
//! executable that `victory run` maps, binds and starts).
//!
//! It reaches the console's graphics only as a title does: through the
//! entry points it imports (given here as function pointers, [`Imports`])
//! and then through the function tables of the objects the device hands
//! out, at the offsets a title's code calls them (the device is created as
//! `ID3D12Device11`, so queues and lists have the newer layouts current
//! titles see), with structures laid out as on PC ([`pc`]). Nothing here
//! knows what is behind the tables, so it depends on no graphics crate; a
//! caller that wants to look behind them (the self-test reads pictures
//! back from the host) does so on its own.
//!
//! The scene ([`scene`]) is the self-test's: frames cleared and presented,
//! a buffer and a tiled texture in title memory copied by lists, a
//! translated compute program whose stores land in title memory, and a
//! translated vertex and pixel program drawn from a vertex buffer in title
//! memory. The shader containers come from `victory_shader::native::synth`,
//! written from the public instruction encodings.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::cell::Cell;
use std::ffi::c_void;

pub mod pc;
pub mod scene;

use pc::*;

/// `D3D12XboxCreateDevice(adapter, parameters, iid, out)`.
pub type CreateDevice =
    extern "system" fn(*const c_void, *const u32, *const Iid, *mut Obj) -> HRESULT;
/// `D3D12SerializeRootSignature(desc, version, blob, error)`.
pub type SerializeRoot =
    extern "system" fn(*const c_void, u32, *mut *mut c_void, *mut *mut c_void) -> HRESULT;

/// The entry points a title imports from `d3d12_xs.dll`, as bound.
#[derive(Clone, Copy)]
pub struct Imports {
    pub create_device: CreateDevice,
    pub serialize_root: SerializeRoot,
}

/// The interface version the device's layouts were checked against (2.4),
/// the first of the creation parameters.
pub const INTERFACE_VERSION: u32 = 0x0002_0004;

/// The side of the render target and the window: small, so that a frame
/// costs the card nothing.
pub const SIDE: u32 = 64;

/// What went wrong, in our words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failed(pub String);

impl From<String> for Failed {
    fn from(reason: String) -> Self {
        Failed(reason)
    }
}

impl From<&str> for Failed {
    fn from(reason: &str) -> Self {
        Failed(reason.to_owned())
    }
}

impl std::fmt::Display for Failed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub fn hr_text(hr: HRESULT) -> String {
    format!("{:#010x}", hr as u32)
}

pub fn check(what: &str, hr: HRESULT) -> Result<(), Failed> {
    if hr == S_OK {
        Ok(())
    } else {
        Err(format!("{what} returned {}", hr_text(hr)).into())
    }
}

/// The method at byte `offset` of `object`'s function table, as `F`.
///
/// Every object passed here was handed out by the device's tables (or is
/// the device), whose first field is its table, and every offset is one
/// the layer's table modules list with this signature.
pub(crate) fn slot<F: Copy>(object: Obj, offset: usize) -> F {
    const { assert!(size_of::<F>() == size_of::<*const ()>()) };
    // SAFETY: see above: `object` points at a pointer to its table, which
    // holds a function of signature `F` at `offset`.
    unsafe {
        let table = *object.cast::<*const *const ()>();
        std::mem::transmute_copy(&*table.add(offset / 8))
    }
}

// The methods, by their byte offset in the tables (as the layer's
// `device.rs`, `queue.rs`, `list.rs` and the others list them; the newer
// layouts, which a device created as `ID3D12Device11` hands out, move a
// queue's methods from 0x70 on by two slots).
type MakeQueue = extern "system" fn(Obj, *const QueueDesc, *const Iid, *mut Obj) -> HRESULT;
type MakeAllocator = extern "system" fn(Obj, u32, *const Iid, *mut Obj) -> HRESULT;
type MakeList = extern "system" fn(Obj, u32, u32, Obj, Obj, *const Iid, *mut Obj) -> HRESULT;
type MakeFence = extern "system" fn(Obj, u64, u32, *const Iid, *mut Obj) -> HRESULT;
type MakeHeap = extern "system" fn(Obj, *const HeapDesc, *const Iid, *mut Obj) -> HRESULT;
type MakeCommitted = extern "system" fn(
    Obj,
    *const HeapProperties,
    u32,
    *const ResourceDesc,
    u32,
    *const c_void,
    *const Iid,
    *mut Obj,
) -> HRESULT;
type MakeRtv = extern "system" fn(Obj, Obj, *const c_void, u64);
type MakeUav = extern "system" fn(Obj, Obj, Obj, *const RawUav, u64);
type MakeRoot = extern "system" fn(Obj, u32, *const u8, usize, *const Iid, *mut Obj) -> HRESULT;
type MakeCompute = extern "system" fn(Obj, *const ComputeDesc, *const Iid, *mut Obj) -> HRESULT;
type MakeStream = extern "system" fn(Obj, *const StreamDesc, *const Iid, *mut Obj) -> HRESULT;
type SetInterval = extern "system" fn(Obj, Obj, u32, u32, u32) -> HRESULT;
type Schedule = extern "system" fn(Obj, u32, u32, *const u8, u32) -> HRESULT;
type WaitFrame = extern "system" fn(Obj, u32, u32, *const u8, u32, *mut u64) -> HRESULT;
type HeapStart = extern "system" fn(Obj) -> u64;
type GpuAddress = extern "system" fn(Obj) -> u64;
type Map = extern "system" fn(Obj, u32, *const c_void, *mut *mut u8) -> HRESULT;
type ResetAllocator = extern "system" fn(Obj) -> HRESULT;
type Close = extern "system" fn(Obj) -> HRESULT;
type ResetList = extern "system" fn(Obj, Obj, Obj) -> HRESULT;
type Barriers = extern "system" fn(Obj, u32, *const Barrier);
type ClearRtv = extern "system" fn(Obj, u64, *const [f32; 4], u32, *const Rect);
type CopyBuffer = extern "system" fn(Obj, Obj, u64, Obj, u64, u64);
type CopyResource = extern "system" fn(Obj, Obj, Obj);
type CopyRegion =
    extern "system" fn(Obj, *const CopyLocation, u32, u32, u32, *const CopyLocation, *const [u32; 6]);
type SetPipeline = extern "system" fn(Obj, Obj);
type SetRoot = extern "system" fn(Obj, Obj);
type SetHeaps = extern "system" fn(Obj, u32, *const Obj);
type SetTable = extern "system" fn(Obj, u32, u64);
type SetConstants = extern "system" fn(Obj, u32, u32, *const u32, u32);
type Dispatch = extern "system" fn(Obj, u32, u32, u32);
type SetTopology = extern "system" fn(Obj, u32);
type SetViewports = extern "system" fn(Obj, u32, *const Viewport);
type SetScissors = extern "system" fn(Obj, u32, *const Rect);
type SetVertexBuffers = extern "system" fn(Obj, u32, u32, *const VertexView);
type SetTargets = extern "system" fn(Obj, u32, *const u64, i32, *const u64);
type Draw = extern "system" fn(Obj, u32, u32, u32, u32);
type Execute = extern "system" fn(Obj, u32, *const Obj);
type Signal = extern "system" fn(Obj, Obj, u64) -> HRESULT;
type Present = extern "system" fn(Obj, u32, *const Planes, *const u8) -> HRESULT;
type Completed = extern "system" fn(Obj) -> u64;
type WaitFor = extern "system" fn(Obj, u64, *mut c_void) -> HRESULT;
/// `ID3DBlob`'s `Release`, `GetBufferPointer` and `GetBufferSize`.
type BlobRelease = extern "system" fn(*mut c_void) -> u32;
type BlobPointer = extern "system" fn(*mut c_void) -> *const u8;
type BlobSize = extern "system" fn(*mut c_void) -> usize;

/// The frame-origin event of the device's frame pacing: the one whose
/// wait starts a frame and hands out its token.
const ORIGIN: u32 = 0;

/// Title memory of `len` bytes, as `Map` gave it. The resource is never
/// released, so the memory lives as long as the process.
pub struct Memory {
    at: *mut u8,
    len: usize,
}

impl Memory {
    pub fn address(&self) -> u64 {
        self.at as u64
    }

    /// The bytes. The GPU's work on them must have been waited for.
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: `len` bytes of a live resource's memory; nothing writes
        // them while the title looks (the caller waited for the GPU).
        unsafe { std::slice::from_raw_parts(self.at, self.len) }
    }

    /// See [`Memory::bytes`].
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: as in `bytes`; `&mut self` keeps this the only view.
        unsafe { std::slice::from_raw_parts_mut(self.at, self.len) }
    }
}

/// The stand-in title: its device, one direct queue, allocator, list and
/// fence, and a render target with its view.
pub struct Title {
    pub imports: Imports,
    pub device: Obj,
    pub queue: Obj,
    pub allocator: Obj,
    pub list: Obj,
    pub fence: Obj,
    /// The fence's last value signalled.
    value: Cell<u64>,
    pub target: Obj,
    pub rtv: u64,
    /// Called whenever a submission or a wait made progress (the
    /// self-test's watchdog).
    progress: fn(),
}

impl Title {
    /// `D3D12XboxCreateDevice`, as `ID3D12Device11`.
    pub fn create_device(imports: &Imports) -> Result<Obj, Failed> {
        let mut parameters = [0u32; 32];
        parameters[0] = INTERFACE_VERSION;
        let mut device = std::ptr::null();
        check(
            "device creation",
            (imports.create_device)(
                std::ptr::null(),
                parameters.as_ptr(),
                &DEVICE11,
                &mut device,
            ),
        )?;
        if device.is_null() {
            return Err("device creation gave no device".into());
        }
        Ok(device)
    }

    /// The queue, allocator, list and fence, through the device's table,
    /// and the render target.
    pub fn on(device: Obj, imports: Imports, progress: fn()) -> Result<Title, Failed> {
        let null: Obj = std::ptr::null();
        let (mut queue, mut allocator, mut list, mut fence) = (null, null, null, null);
        let desc = QueueDesc {
            kind: DIRECT,
            priority: 0,
            flags: 0,
            nodes: 0,
        };
        check(
            "CreateCommandQueue",
            slot::<MakeQueue>(device, 0x40)(device, &desc, &QUEUE, &mut queue),
        )?;
        check(
            "CreateCommandAllocator",
            slot::<MakeAllocator>(device, 0x48)(device, DIRECT, &ALLOCATOR, &mut allocator),
        )?;
        check(
            "CreateCommandList",
            slot::<MakeList>(device, 0x60)(device, 0, DIRECT, allocator, null, &LIST, &mut list),
        )?;
        check(
            "CreateFence",
            slot::<MakeFence>(device, 0x120)(device, 0, 0, &FENCE, &mut fence),
        )?;
        // A list is made open, as on PC.
        check("Close", slot::<Close>(list, 0x48)(list))?;
        let mut title = Title {
            imports,
            device,
            queue,
            allocator,
            list,
            fence,
            value: Cell::new(0),
            target: null,
            rtv: 0,
            progress,
        };
        (title.target, title.rtv) = title.render_target()?;
        Ok(title)
    }

    pub fn committed(&self, heap: u32, desc: &ResourceDesc, state: u32) -> Result<Obj, Failed> {
        let properties = HeapProperties {
            kind: heap,
            cpu_pages: 0,
            pool: 0,
            creation_nodes: 0,
            visible_nodes: 0,
        };
        let mut out = std::ptr::null();
        let make = slot::<MakeCommitted>(self.device, 0xd8);
        check(
            "CreateCommittedResource",
            make(
                self.device,
                &properties,
                0,
                desc,
                state,
                std::ptr::null(),
                &RESOURCE,
                &mut out,
            ),
        )?;
        Ok(out)
    }

    pub fn buffer(&self, heap: u32, bytes: u64, flags: u32) -> Result<Obj, Failed> {
        let desc = ResourceDesc {
            dimension: BUFFER,
            alignment: 0,
            width: bytes,
            height: 1,
            depth_or_array_size: 1,
            mip_levels: 1,
            format: 0,
            sample_count: 1,
            sample_quality: 0,
            layout: 1,
            flags,
        };
        self.committed(heap, &desc, COMMON)
    }

    /// A [`SIDE`]-square RGBA8 texture in the console's own choice of
    /// tiling (layout 0).
    pub fn texture_desc(flags: u32) -> ResourceDesc {
        ResourceDesc {
            dimension: TEXTURE_2D,
            alignment: 0,
            width: u64::from(SIDE),
            height: SIDE,
            depth_or_array_size: 1,
            mip_levels: 1,
            format: RGBA8,
            sample_count: 1,
            sample_quality: 0,
            layout: 0,
            flags,
        }
    }

    /// The title memory of a resource, as `Map` gives it; the resource
    /// must hold at least `len` bytes.
    pub fn mapped(resource: Obj, len: usize) -> Result<Memory, Failed> {
        let mut at = std::ptr::null_mut();
        check(
            "Map",
            slot::<Map>(resource, 0x40)(resource, 0, std::ptr::null(), &mut at),
        )?;
        if at.is_null() {
            return Err("Map gave no address".into());
        }
        Ok(Memory { at, len })
    }

    pub fn gpu_address(resource: Obj) -> u64 {
        slot::<GpuAddress>(resource, 0x58)(resource)
    }

    fn render_target(&self) -> Result<(Obj, u64), Failed> {
        let target = self.committed(
            DEFAULT_HEAP,
            &Self::texture_desc(RENDER_TARGET_FLAG),
            COMMON,
        )?;
        let mut heap = std::ptr::null();
        let desc = HeapDesc {
            kind: RTV_HEAP,
            count: 1,
            flags: 0,
            nodes: 0,
            after: [0; 2],
        };
        check(
            "CreateDescriptorHeap",
            slot::<MakeHeap>(self.device, 0x70)(self.device, &desc, &DESCRIPTOR_HEAP, &mut heap),
        )?;
        let rtv = slot::<HeapStart>(heap, 0x48)(heap);
        slot::<MakeRtv>(self.device, 0xa0)(self.device, target, std::ptr::null(), rtv);
        Ok((target, rtv))
    }

    /// A descriptor heap of `count` views, visible to shaders: its first
    /// CPU and GPU handles.
    pub fn view_heap(&self, count: u32) -> Result<(Obj, u64, u64), Failed> {
        let mut heap = std::ptr::null();
        let desc = HeapDesc {
            kind: VIEW_HEAP,
            count,
            flags: SHADER_VISIBLE,
            nodes: 0,
            after: [0; 2],
        };
        check(
            "CreateDescriptorHeap",
            slot::<MakeHeap>(self.device, 0x70)(self.device, &desc, &DESCRIPTOR_HEAP, &mut heap),
        )?;
        Ok((
            heap,
            slot::<HeapStart>(heap, 0x48)(heap),
            slot::<HeapStart>(heap, 0x50)(heap),
        ))
    }

    pub fn raw_uav(&self, resource: Obj, view: &RawUav, cpu: u64) {
        slot::<MakeUav>(self.device, 0x98)(self.device, resource, std::ptr::null(), view, cpu);
    }

    /// Records one list (reset, `record`, close) and submits it.
    pub fn frame(&self, record: impl FnOnce(&Self) -> Result<(), Failed>) -> Result<(), Failed> {
        check(
            "allocator Reset",
            slot::<ResetAllocator>(self.allocator, 0x40)(self.allocator),
        )?;
        check(
            "list Reset",
            slot::<ResetList>(self.list, 0x50)(self.list, self.allocator, std::ptr::null()),
        )?;
        record(self)?;
        check("Close", slot::<Close>(self.list, 0x48)(self.list))?;
        slot::<Execute>(self.queue, 0x50)(self.queue, 1, &self.list);
        (self.progress)();
        Ok(())
    }

    pub fn transition(&self, resource: Obj, before: u32, after: u32) {
        let b = Barrier {
            kind: 0,
            flags: 0,
            resource,
            subresource: u32::MAX,
            before,
            after,
            _pad: 0,
        };
        slot::<Barriers>(self.list, 0xd0)(self.list, 1, &b);
    }

    /// The whole render target cleared, in whatever state the list left
    /// it (it must be a render target by then).
    pub fn clear_target(&self, colour: [f32; 4]) {
        slot::<ClearRtv>(self.list, 0x1a0)(self.list, self.rtv, &colour, 0, std::ptr::null());
    }

    /// Only `rects` of the render target cleared (see [`Title::clear_target`]).
    pub fn clear_rects(&self, colour: [f32; 4], rects: &[Rect]) {
        slot::<ClearRtv>(self.list, 0x1a0)(
            self.list,
            self.rtv,
            &colour,
            rects.len() as u32,
            rects.as_ptr(),
        );
    }

    /// The render target, from the common state, cleared and back.
    pub fn clear(&self, colour: [f32; 4]) -> Result<(), Failed> {
        self.transition(self.target, COMMON, RENDER_TARGET);
        self.clear_target(colour);
        self.transition(self.target, RENDER_TARGET, COMMON);
        Ok(())
    }

    pub fn copy_buffer(&self, to: Obj, to_at: u64, from: Obj, from_at: u64, bytes: u64) {
        slot::<CopyBuffer>(self.list, 0x78)(self.list, to, to_at, from, from_at, bytes);
    }

    pub fn copy_resource(&self, to: Obj, from: Obj) {
        slot::<CopyResource>(self.list, 0x88)(self.list, to, from);
    }

    /// `CopyTextureRegion`: all of `from` to the corner of `to`.
    pub fn copy_texture_region(&self, to: &CopyLocation, from: &CopyLocation) {
        slot::<CopyRegion>(self.list, 0x80)(self.list, to, 0, 0, 0, from, std::ptr::null());
    }

    pub fn set_pipeline(&self, pipeline: Obj) {
        slot::<SetPipeline>(self.list, 0xc8)(self.list, pipeline);
    }

    /// The render target presented through the queue's `PresentX` with the
    /// frame's `token`.
    pub fn present_with(&self, token: u64) -> Result<(), Failed> {
        let planes = Planes {
            token,
            count: 1,
            _pad: 0,
            resources: &self.target,
            sources: 0,
            destination: 0,
            rest: [0; 0x90 - 0x28],
        };
        check(
            "PresentX",
            slot::<Present>(self.queue, 0x100)(self.queue, 1, &planes, std::ptr::null()),
        )
    }

    /// [`Title::present_with`] the fence's next value as the token, as a
    /// title that does not pace its frames does.
    pub fn present(&self) -> Result<(), Failed> {
        self.present_with(self.value.get() + 1)
    }

    /// Signals the fence after what was submitted and waits for it, as a
    /// title with one frame in flight does.
    pub fn sync(&self) -> Result<(), Failed> {
        let value = self.value.get() + 1;
        self.value.set(value);
        check(
            "Signal",
            slot::<Signal>(self.queue, 0x80)(self.queue, self.fence, value),
        )?;
        if slot::<Completed>(self.fence, 0x40)(self.fence) < value {
            check(
                "SetEventOnCompletion",
                slot::<WaitFor>(self.fence, 0x48)(self.fence, value, std::ptr::null_mut()),
            )?;
        }
        let reached = slot::<Completed>(self.fence, 0x40)(self.fence);
        (self.progress)();
        if reached < value {
            return Err(format!("the fence stopped at {reached}, short of {value}").into());
        }
        Ok(())
    }

    /// The device's frame pacing: a frame every `interval_us`
    /// microseconds (`SetFrameIntervalX`), and the frame-origin event at
    /// each one (`ScheduleFrameEventX`).
    pub fn pace(&self, interval_us: u32) -> Result<(), Failed> {
        let null = std::ptr::null();
        check(
            "SetFrameIntervalX",
            slot::<SetInterval>(self.device, 0x270)(self.device, null, interval_us, 1, 0),
        )?;
        check(
            "ScheduleFrameEventX",
            slot::<Schedule>(self.device, 0x278)(self.device, ORIGIN, 0, null.cast(), 0),
        )
    }

    /// Waits for the next frame to begin (`WaitFrameEventX` on the origin
    /// event, at most `timeout_ms`): the frame's token.
    pub fn next_frame(&self, timeout_ms: u32) -> Result<u64, Failed> {
        let mut token = 0;
        let wait = slot::<WaitFrame>(self.device, 0x280);
        check(
            "WaitFrameEventX",
            wait(
                self.device,
                ORIGIN,
                timeout_ms,
                std::ptr::null(),
                0,
                &mut token,
            ),
        )?;
        Ok(token)
    }

    /// The root signature of `parameters`, serialized through the title's
    /// `D3D12SerializeRootSignature` import and created on the device.
    pub fn root_signature(&self, parameters: &[RootParameter], flags: u32) -> Result<Obj, Failed> {
        let desc = RootDesc {
            count: parameters.len() as u32,
            parameters: parameters.as_ptr(),
            samplers: 0,
            static_samplers: std::ptr::null(),
            flags,
        };
        let (mut blob, mut error) = (std::ptr::null_mut(), std::ptr::null_mut());
        let hr = (self.imports.serialize_root)((&raw const desc).cast(), 1, &mut blob, &mut error);
        if !error.is_null() {
            release(error);
        }
        check("D3D12SerializeRootSignature", hr)?;
        if blob.is_null() {
            return Err("D3D12SerializeRootSignature gave no blob".into());
        }
        let (at, len) = (
            blob_slot::<BlobPointer>(blob, 3)(blob),
            blob_slot::<BlobSize>(blob, 4)(blob),
        );
        let mut root = std::ptr::null();
        let made = check(
            "CreateRootSignature",
            slot::<MakeRoot>(self.device, 0x80)(
                self.device,
                0,
                at,
                len,
                &ROOT_SIGNATURE,
                &mut root,
            ),
        );
        release(blob);
        made.map(|()| root)
    }

    pub fn compute_pipeline(&self, root: Obj, container: &[u8]) -> Result<Obj, Failed> {
        let desc = ComputeDesc {
            root,
            code: container.as_ptr(),
            size: container.len(),
            nodes: 0,
            cached: std::ptr::null(),
            cached_size: 0,
            flags: 0,
        };
        let mut pipeline = std::ptr::null();
        check(
            "CreateComputePipelineState",
            slot::<MakeCompute>(self.device, 0x58)(self.device, &desc, &PIPELINE, &mut pipeline),
        )?;
        Ok(pipeline)
    }

    /// A pipeline from a pipeline-state stream (`CreatePipelineState`,
    /// 0x2b0), the description a title of the current generation uses.
    pub fn stream_pipeline(&self, stream: &GraphicsStream) -> Result<Obj, Failed> {
        let desc = StreamDesc {
            size: size_of::<GraphicsStream>(),
            stream: std::ptr::from_ref(stream).cast(),
        };
        let mut pipeline = std::ptr::null();
        check(
            "CreatePipelineState",
            slot::<MakeStream>(self.device, 0x2b0)(self.device, &desc, &PIPELINE, &mut pipeline),
        )?;
        Ok(pipeline)
    }
}

/// The method in slot `index` of a COM blob's table.
fn blob_slot<F: Copy>(blob: *mut c_void, index: usize) -> F {
    slot::<F>(blob.cast_const(), index * 8)
}

/// Lets a blob the serializer handed out go.
fn release(blob: *mut c_void) {
    blob_slot::<BlobRelease>(blob, 2)(blob);
}

// List methods the scene records, kept beside the others.
impl Title {
    pub fn set_heaps(&self, heaps: &[Obj]) {
        slot::<SetHeaps>(self.list, 0x100)(self.list, heaps.len() as u32, heaps.as_ptr());
    }

    pub fn set_compute_root(&self, root: Obj) {
        slot::<SetRoot>(self.list, 0x108)(self.list, root);
    }

    pub fn set_graphics_root(&self, root: Obj) {
        slot::<SetRoot>(self.list, 0x110)(self.list, root);
    }

    pub fn set_compute_table(&self, slot_index: u32, handle: u64) {
        slot::<SetTable>(self.list, 0x118)(self.list, slot_index, handle);
    }

    pub fn set_compute_constants(&self, slot_index: u32, values: &[u32]) {
        slot::<SetConstants>(self.list, 0x138)(
            self.list,
            slot_index,
            values.len() as u32,
            values.as_ptr(),
            0,
        );
    }

    pub fn set_graphics_constants(&self, slot_index: u32, values: &[u32]) {
        slot::<SetConstants>(self.list, 0x140)(
            self.list,
            slot_index,
            values.len() as u32,
            values.as_ptr(),
            0,
        );
    }

    pub fn dispatch(&self, x: u32, y: u32, z: u32) {
        slot::<Dispatch>(self.list, 0x70)(self.list, x, y, z);
    }

    pub fn set_topology(&self, topology: u32) {
        slot::<SetTopology>(self.list, 0xa0)(self.list, topology);
    }

    pub fn set_viewport(&self, viewport: &Viewport) {
        slot::<SetViewports>(self.list, 0xa8)(self.list, 1, viewport);
    }

    pub fn set_scissor(&self, rect: &Rect) {
        slot::<SetScissors>(self.list, 0xb0)(self.list, 1, rect);
    }

    pub fn set_vertex_buffer(&self, view: &VertexView) {
        slot::<SetVertexBuffers>(self.list, 0x180)(self.list, 0, 1, view);
    }

    pub fn set_target(&self) {
        slot::<SetTargets>(self.list, 0x190)(self.list, 1, &self.rtv, 0, std::ptr::null());
    }

    pub fn draw(&self, vertices: u32) {
        slot::<Draw>(self.list, 0x60)(self.list, vertices, 1, 0, 0);
    }
}
