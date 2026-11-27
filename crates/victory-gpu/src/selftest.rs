//! `victory selftest`: the frame path, end to end, driven by a stand-in
//! "title" made of our own code, so that it can be run on any PC, in the
//! tests and in CI, without a single file from a title.
//!
//! The stand-in calls what a title's calls arrive at and nothing else: the
//! device comes from the device-creation export a title imports (the
//! caller resolves it through the runtime's registry and passes it in),
//! and every object after that is reached through the function tables the
//! device hands out, at the offsets a title's code calls (the tables of
//! `device.rs`, `queue.rs`, `list.rs`, `resource.rs`, `descriptor.rs`,
//! `fence.rs`). The structures it passes are laid out as on PC, which is
//! what a title passes. Only the checks read behind the tables: a picture
//! is read back from the host texture the layer made, and what the swap
//! chain did is asked of the host (`host/selfcheck.rs`).
//!
//! The stages, each reported as it finishes:
//!
//! 1. the host device, a queue, a list, a fence and the window;
//! 2. frames cleared to known colours and presented through the queue's
//!    present, read back and compared;
//! 3. title memory: a buffer the title writes is copied by a list and
//!    read back by the title, then changed and copied again; a texture the
//!    title writes in the console's tiled layout (`victory_formats`) is
//!    copied into the render target, presented and read back, then partly
//!    rewritten and copied again;
//! 4. a translated shader: a compute program assembled by
//!    `victory_shader::native::synth`, in a container, made into a host
//!    pipeline the way a title's is, dispatched, and its stores read back
//!    from title memory (twice, with other constants the second time).
//!
//! With `bench`, a few of the layer's own hot paths are timed on synthetic
//! data ([`Rate`]); see `docs/selftest.md` for what they mean.
//!
//! Nothing here runs unless `victory selftest` asks: [`running`] is the
//! only state a title run could see, and it stays false there.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::core::Interface;

use crate::object::{HRESULT, Iid, S_OK, This, guid, slot};

static RUNNING: AtomicBool = AtomicBool::new(false);

/// Whether `victory selftest` is running in this process: the software
/// adapter may then be asked for in a build without diagnostics.
pub(crate) fn running() -> bool {
    RUNNING.load(Ordering::Relaxed)
}

/// `D3D12XboxCreateDevice(adapter, parameters, iid, out)`.
pub type CreateDevice = extern "system" fn(*const c_void, *const u32, *const Iid, *mut This) -> HRESULT;
/// `D3D12SerializeRootSignature(desc, version, blob, error)`.
pub type SerializeRoot =
    extern "system" fn(*const crate::root::Desc, u32, *mut *mut c_void, *mut *mut c_void) -> HRESULT;

/// The exports the stand-in title imports, as the runtime binds them.
#[derive(Clone, Copy)]
pub struct Imports {
    pub create_device: CreateDevice,
    pub serialize_root: SerializeRoot,
}

impl Imports {
    /// This crate's own entry points, unwrapped (the runtime's device
    /// creation also installs its memory allocator first).
    pub fn ours() -> Imports {
        Imports {
            create_device: crate::device::create,
            serialize_root: crate::root::serialize,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// The software adapter (`VICTORY_D3D12_WARP=1`).
    pub warp: bool,
    /// Frames of stage 2.
    pub frames: u32,
    pub bench: bool,
    /// The longest any one step may take before the run is given up as a
    /// hang (`hung` is then called).
    pub patience: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            warp: false,
            frames: 120,
            bench: false,
            patience: Duration::from_secs(60),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Ok(String),
    Fail(String),
    Skip(String),
}

#[derive(Clone, Debug)]
pub struct Stage {
    pub name: &'static str,
    pub outcome: Outcome,
}

/// A measured rate: `name value unit`.
#[derive(Clone, Debug)]
pub struct Rate {
    pub name: &'static str,
    pub value: f64,
    pub unit: &'static str,
    /// What was measured, in a few words.
    pub what: &'static str,
}

#[derive(Debug, Default)]
pub struct Report {
    pub stages: Vec<Stage>,
    pub rates: Vec<Rate>,
    /// Whether the failure was that there is no host device at all (a PC or
    /// runner without Direct3D 12): callers may treat that apart.
    pub no_device: bool,
}

impl Report {
    pub fn passed(&self) -> bool {
        self.stages.iter().all(|s| !matches!(s.outcome, Outcome::Fail(_)))
    }

    pub fn first_failure(&self) -> Option<&Stage> {
        self.stages.iter().find(|s| matches!(s.outcome, Outcome::Fail(_)))
    }
}

/// The stage names, in order.
pub const STAGES: [&str; 4] = ["Device", "Clear + present", "Unified memory", "Translated shader"];

/// Runs every stage, telling `said` of each as it finishes. `hung` is
/// called from another thread if a step takes longer than the patience
/// (with the stage it was in); it must end the process.
pub fn run(imports: &Imports, options: &Options, said: &mut dyn FnMut(&Stage), hung: fn(&'static str) -> !) -> Report {
    RUNNING.store(true, Ordering::Relaxed);
    if options.warp {
        // SAFETY: before the host device exists, on the thread that makes
        // it; nothing of ours reads the environment concurrently yet.
        unsafe { std::env::set_var("VICTORY_D3D12_WARP", "1") };
    }
    Watchdog::start(options.patience, hung);
    let mut report = Report::default();
    let mut bench = Bench::default();
    let mut tell = |report: &mut Report, name: &'static str, outcome: Outcome| {
        let stage = Stage { name, outcome };
        said(&stage);
        report.stages.push(stage);
    };

    Watchdog::arm(STAGES[0]);
    let title = match Title::new(imports, options) {
        Ok((title, detail)) => {
            tell(&mut report, STAGES[0], Outcome::Ok(detail));
            title
        }
        Err(Failed { reason, no_device }) => {
            report.no_device = no_device;
            tell(&mut report, STAGES[0], Outcome::Fail(reason));
            for name in &STAGES[1..] {
                tell(&mut report, name, Outcome::Skip("needs the device stage".into()));
            }
            Watchdog::disarm();
            return report;
        }
    };

    Watchdog::arm(STAGES[1]);
    let outcome = title.clear_and_present(options.frames, &mut bench).into_outcome();
    let presented = matches!(outcome, Outcome::Ok(_));
    tell(&mut report, STAGES[1], outcome);

    Watchdog::arm(STAGES[2]);
    let outcome = if presented {
        title.unified_memory().into_outcome()
    } else {
        Outcome::Skip("needs clear + present".into())
    };
    tell(&mut report, STAGES[2], outcome);

    Watchdog::arm(STAGES[3]);
    let outcome = title.translated_shader(imports, &mut bench);
    tell(&mut report, STAGES[3], outcome);

    if options.bench {
        Watchdog::arm("Benchmark");
        report.rates = title.bench(bench);
    }
    Watchdog::disarm();
    report
}

/// A stage's failure, in our words.
struct Failed {
    reason: String,
    no_device: bool,
}

impl From<String> for Failed {
    fn from(reason: String) -> Self {
        Failed { reason, no_device: false }
    }
}

impl From<&str> for Failed {
    fn from(reason: &str) -> Self {
        reason.to_owned().into()
    }
}

trait IntoOutcome {
    fn into_outcome(self) -> Outcome;
}

impl IntoOutcome for Result<String, Failed> {
    fn into_outcome(self) -> Outcome {
        match self {
            Ok(detail) => Outcome::Ok(detail),
            Err(f) => Outcome::Fail(f.reason),
        }
    }
}

/// Gives the run up when a step takes longer than its patience: a hung
/// driver or a submission that never completes would otherwise hang the
/// self-test, and the player learns nothing.
struct Watchdog;

static ARMED: Mutex<Option<(Instant, &'static str)>> = Mutex::new(None);

impl Watchdog {
    fn start(patience: Duration, hung: fn(&'static str) -> !) {
        static STARTED: AtomicBool = AtomicBool::new(false);
        if STARTED.swap(true, Ordering::Relaxed) {
            return;
        }
        let _ = std::thread::Builder::new().name("victory-selftest-watchdog".into()).spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(200));
                let armed = *ARMED.lock().unwrap_or_else(|e| e.into_inner());
                if let Some((since, stage)) = armed
                    && since.elapsed() > patience
                {
                    hung(stage);
                }
            }
        });
    }

    /// From now on, `stage` is being run; the clock starts again.
    fn arm(stage: &'static str) {
        *ARMED.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), stage));
    }

    /// A step that made progress: the clock starts again.
    fn pet() {
        if let Some((since, _)) = ARMED.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            *since = Instant::now();
        }
    }

    fn disarm() {
        *ARMED.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

// The structures a title passes, laid out as on PC (D3D12's headers).

#[repr(C)]
struct QueueDesc {
    kind: u32,
    priority: i32,
    flags: u32,
    nodes: u32,
}

/// `D3D12_DESCRIPTOR_HEAP_DESC`, then the room newer titles' callers have
/// after it (a placement of their own), left empty.
#[repr(C)]
struct HeapDesc {
    kind: u32,
    count: u32,
    flags: u32,
    nodes: u32,
    after: [u64; 2],
}

#[repr(C)]
struct HeapProperties {
    kind: u32,
    cpu_pages: u32,
    pool: u32,
    creation_nodes: u32,
    visible_nodes: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ResourceDesc {
    dimension: u32,
    alignment: u64,
    width: u64,
    height: u32,
    depth_or_array_size: u16,
    mip_levels: u16,
    format: u32,
    sample_count: u32,
    sample_quality: u32,
    layout: u32,
    flags: u32,
}

/// A transition barrier (`D3D12_RESOURCE_BARRIER`).
#[repr(C)]
struct Barrier {
    kind: u32,
    flags: u32,
    resource: This,
    subresource: u32,
    before: u32,
    after: u32,
    _pad: u32,
}

#[repr(C)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

/// `D3D12_UNORDERED_ACCESS_VIEW_DESC` of a raw buffer.
#[repr(C)]
struct RawUav {
    format: u32,
    dimension: u32,
    first: u64,
    count: u32,
    stride: u32,
    counter_offset: u64,
    flags: u32,
    _pad: u32,
}

/// `D3D12_COMPUTE_PIPELINE_STATE_DESC`.
#[repr(C)]
struct ComputeDesc {
    root: This,
    code: *const u8,
    size: usize,
    nodes: u32,
    cached: *const u8,
    cached_size: usize,
    flags: u32,
}

/// One plane of a present (`D3D12XBOX_PRESENT_PLANE_PARAMETERS`): the frame
/// token, the resources, source rectangles and destination (none: whole),
/// colour space; two planes' room, the second left empty.
#[repr(C)]
struct Planes {
    token: u64,
    count: u32,
    _pad: u32,
    resources: *const This,
    sources: u64,
    destination: u64,
    rest: [u8; 0x90 - 0x28],
}

// The interface ids a title asks with (the PC ones).
const DEVICE11: Iid = guid(0x5405c344, 0xd457, 0x444e, [0xb4, 0xdd, 0x23, 0x66, 0xe4, 0x5a, 0xee, 0x39]);
const QUEUE: Iid = guid(0x0ec870a6, 0x5d7e, 0x4c22, [0x8c, 0xfc, 0x5b, 0xaa, 0xe0, 0x76, 0x16, 0xed]);
const ALLOCATOR: Iid = guid(0x6102dee4, 0xaf59, 0x4b09, [0xb9, 0x99, 0xb4, 0x4d, 0x73, 0xf0, 0x9b, 0x24]);
const LIST: Iid = guid(0x5b160d0f, 0xac1b, 0x4185, [0x8b, 0xa8, 0xb3, 0xae, 0x42, 0xa5, 0xa4, 0x55]);
const FENCE: Iid = guid(0x0a753dcf, 0xc4d8, 0x4b91, [0xad, 0xf6, 0xbe, 0x5a, 0x60, 0xd9, 0x5a, 0x76]);
const RESOURCE: Iid = guid(0x696442be, 0xa72e, 0x4059, [0xbc, 0x79, 0x5b, 0x5c, 0x98, 0x04, 0x0f, 0xad]);
const DESCRIPTOR_HEAP: Iid = guid(0x8efb471d, 0x616c, 0x4f49, [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51]);
const ROOT_SIGNATURE: Iid = guid(0xc54a6b66, 0x72df, 0x4ee8, [0x8b, 0xe5, 0xa9, 0x46, 0xa1, 0x42, 0x92, 0x14]);
const PIPELINE: Iid = guid(0x765a30f3, 0xf624, 0x4c6f, [0xa8, 0x28, 0xac, 0xe9, 0x48, 0x62, 0x24, 0x45]);

// The methods, by their byte offset in the tables (as `device.rs`,
// `queue.rs`, `list.rs` and the others list them; the newer layouts, which
// a device created as `ID3D12Device11` hands out, move a queue's methods
// from 0x70 on by two slots).
type MakeQueue = extern "system" fn(This, *const QueueDesc, *const Iid, *mut This) -> HRESULT;
type MakeAllocator = extern "system" fn(This, u32, *const Iid, *mut This) -> HRESULT;
type MakeList = extern "system" fn(This, u32, u32, This, This, *const Iid, *mut This) -> HRESULT;
type MakeFence = extern "system" fn(This, u64, u32, *const Iid, *mut This) -> HRESULT;
type MakeHeap = extern "system" fn(This, *const HeapDesc, *const Iid, *mut This) -> HRESULT;
type MakeCommitted =
    extern "system" fn(This, *const HeapProperties, u32, *const ResourceDesc, u32, *const c_void, *const Iid, *mut This) -> HRESULT;
type MakeRtv = extern "system" fn(This, This, *const c_void, u64);
type MakeUav = extern "system" fn(This, This, This, *const RawUav, u64);
type MakeRoot = extern "system" fn(This, u32, *const u8, usize, *const Iid, *mut This) -> HRESULT;
type MakeCompute = extern "system" fn(This, *const ComputeDesc, *const Iid, *mut This) -> HRESULT;
type HeapStart = extern "system" fn(This) -> u64;
type Map = extern "system" fn(This, u32, *const c_void, *mut *mut u8) -> HRESULT;
type ResetAllocator = extern "system" fn(This) -> HRESULT;
type Close = extern "system" fn(This) -> HRESULT;
type ResetList = extern "system" fn(This, This, This) -> HRESULT;
type Barriers = extern "system" fn(This, u32, *const Barrier);
type ClearRtv = extern "system" fn(This, u64, *const [f32; 4], u32, *const Rect);
type CopyBuffer = extern "system" fn(This, This, u64, This, u64, u64);
type CopyResource = extern "system" fn(This, This, This);
type SetPipeline = extern "system" fn(This, This);
type SetRoot = extern "system" fn(This, This);
type SetHeaps = extern "system" fn(This, u32, *const This);
type SetTable = extern "system" fn(This, u32, u64);
type SetConstants = extern "system" fn(This, u32, u32, *const u32, u32);
type Dispatch = extern "system" fn(This, u32, u32, u32);
type Execute = extern "system" fn(This, u32, *const This);
type Signal = extern "system" fn(This, This, u64) -> HRESULT;
type Present = extern "system" fn(This, u32, *const Planes, *const u8) -> HRESULT;
type Completed = extern "system" fn(This) -> u64;
type WaitFor = extern "system" fn(This, u64, *mut c_void) -> HRESULT;

const DIRECT: u32 = 0;
const BUFFER: u32 = 1;
const TEXTURE_2D: u32 = 3;
const RGBA8: u32 = 28;
const DEFAULT_HEAP: u32 = 1;
const UPLOAD_HEAP: u32 = 2;
const RENDER_TARGET_FLAG: u32 = 1;
const UNORDERED_FLAG: u32 = 4;
const COMMON: u32 = 0;
const RENDER_TARGET: u32 = 4;
const COPY_SOURCE: u32 = 0x800;
const RTV_HEAP: u32 = 2;
const VIEW_HEAP: u32 = 0;
const SHADER_VISIBLE: u32 = 1;

/// The side of the picture of stages 2 and 3: small, so that the window
/// is small and a frame costs the card nothing.
const SIDE: u32 = 64;

fn hr_text(hr: HRESULT) -> String {
    format!("{:#010x}", hr as u32)
}

fn check(what: &str, hr: HRESULT) -> Result<(), Failed> {
    if hr == S_OK { Ok(()) } else { Err(format!("{what} returned {}", hr_text(hr)).into()) }
}

/// The stand-in title: its device and the objects it made.
struct Title {
    device: This,
    queue: This,
    allocator: This,
    list: This,
    fence: This,
    value: std::cell::Cell<u64>,
    /// The render target and its view, made by stage 1.
    target: This,
    rtv: u64,
}

impl Title {
    fn new(imports: &Imports, options: &Options) -> Result<(Title, String), Failed> {
        let null: This = std::ptr::null();
        let mut parameters = [0u32; 32];
        parameters[0] = crate::device::CHECKED_VERSION;
        let mut device = null;
        check("device creation", (imports.create_device)(std::ptr::null(), parameters.as_ptr(), &DEVICE11, &mut device))?;
        let Some(host) = crate::host::host() else {
            return Err(Failed {
                reason: "no host Direct3D 12 device at feature level 12_0 (the warning above says why)".into(),
                no_device: true,
            });
        };
        let facts = host.adapter_facts();
        let software = facts.as_ref().is_some_and(|a| a.software);
        if options.warp && !software {
            return Err("asked for the software adapter (WARP) and got another".into());
        }
        let adapter = match &facts {
            Some(a) if a.software => format!("{} (software)", a.name),
            Some(a) if a.driver.is_empty() => format!("{}, {} MB", a.name, a.vram_mb),
            Some(a) => format!("{}, driver {}, {} MB", a.name, a.driver, a.vram_mb),
            None => "an adapter DXGI does not describe".into(),
        };
        let (mut queue, mut allocator, mut list, mut fence) = (null, null, null, null);
        let desc = QueueDesc { kind: DIRECT, priority: 0, flags: 0, nodes: 0 };
        check("CreateCommandQueue", slot::<MakeQueue>(device, 0x40)(device, &desc, &QUEUE, &mut queue))?;
        check("CreateCommandAllocator", slot::<MakeAllocator>(device, 0x48)(device, DIRECT, &ALLOCATOR, &mut allocator))?;
        check("CreateCommandList", slot::<MakeList>(device, 0x60)(device, 0, DIRECT, allocator, null, &LIST, &mut list))?;
        check("CreateFence", slot::<MakeFence>(device, 0x120)(device, 0, 0, &FENCE, &mut fence))?;
        // A list is made open, as on PC.
        check("Close", slot::<Close>(list, 0x48)(list))?;
        let mut title = Title {
            device,
            queue,
            allocator,
            list,
            fence,
            value: std::cell::Cell::new(0),
            target: null,
            rtv: 0,
        };
        (title.target, title.rtv) = title.render_target()?;
        // The window comes with the first present.
        title.frame(|t| t.clear([0.0, 0.0, 0.0, 1.0]))?;
        title.present()?;
        title.sync()?;
        let presents = host.presents().ok_or("the first present made no swap chain")?;
        if !presents.window {
            return Err("the swap chain has no window".into());
        }
        Ok((title, format!("{adapter}; queue, list, fence and a {}x{} window", presents.size.0, presents.size.1)))
    }

    fn null() -> This {
        std::ptr::null()
    }

    fn committed(&self, heap: u32, desc: &ResourceDesc, state: u32) -> Result<This, Failed> {
        let properties = HeapProperties { kind: heap, cpu_pages: 0, pool: 0, creation_nodes: 0, visible_nodes: 0 };
        let mut out = Self::null();
        let make = slot::<MakeCommitted>(self.device, 0xd8);
        check("CreateCommittedResource", make(self.device, &properties, 0, desc, state, std::ptr::null(), &RESOURCE, &mut out))?;
        Ok(out)
    }

    fn buffer(&self, heap: u32, bytes: u64, flags: u32) -> Result<This, Failed> {
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

    fn texture_desc(flags: u32) -> ResourceDesc {
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
            // The console's own choice of tiling.
            layout: 0,
            flags,
        }
    }

    /// The title memory of a resource, as `Map` gives it.
    fn mapped(resource: This, len: usize) -> Result<&'static mut [u8], Failed> {
        let mut at = std::ptr::null_mut();
        check("Map", slot::<Map>(resource, 0x40)(resource, 0, std::ptr::null(), &mut at))?;
        if at.is_null() {
            return Err("Map gave no address".into());
        }
        // SAFETY: the resource's memory holds at least `len` bytes (the
        // caller made it so) and lives as long as the resource, which the
        // stand-in never releases; nothing else writes it while the title
        // looks (the GPU's work was waited for).
        Ok(unsafe { std::slice::from_raw_parts_mut(at, len) })
    }

    fn render_target(&self) -> Result<(This, u64), Failed> {
        let target = self.committed(DEFAULT_HEAP, &Self::texture_desc(RENDER_TARGET_FLAG), COMMON)?;
        let mut heap = Self::null();
        let desc = HeapDesc { kind: RTV_HEAP, count: 1, flags: 0, nodes: 0, after: [0; 2] };
        check("CreateDescriptorHeap", slot::<MakeHeap>(self.device, 0x70)(self.device, &desc, &DESCRIPTOR_HEAP, &mut heap))?;
        let rtv = slot::<HeapStart>(heap, 0x48)(heap);
        slot::<MakeRtv>(self.device, 0xa0)(self.device, target, std::ptr::null(), rtv);
        Ok((target, rtv))
    }

    /// Records one list (reset, `record`, close) and submits it.
    fn frame(&self, record: impl FnOnce(&Self) -> Result<(), Failed>) -> Result<(), Failed> {
        check("allocator Reset", slot::<ResetAllocator>(self.allocator, 0x40)(self.allocator))?;
        check("list Reset", slot::<ResetList>(self.list, 0x50)(self.list, self.allocator, Self::null()))?;
        record(self)?;
        check("Close", slot::<Close>(self.list, 0x48)(self.list))?;
        slot::<Execute>(self.queue, 0x50)(self.queue, 1, &self.list);
        Watchdog::pet();
        Ok(())
    }

    fn transition(&self, resource: This, before: u32, after: u32) {
        let b = Barrier { kind: 0, flags: 0, resource, subresource: u32::MAX, before, after, _pad: 0 };
        slot::<Barriers>(self.list, 0xd0)(self.list, 1, &b);
    }

    fn clear(&self, colour: [f32; 4]) -> Result<(), Failed> {
        self.transition(self.target, COMMON, RENDER_TARGET);
        slot::<ClearRtv>(self.list, 0x1a0)(self.list, self.rtv, &colour, 0, std::ptr::null());
        self.transition(self.target, RENDER_TARGET, COMMON);
        Ok(())
    }

    fn present(&self) -> Result<(), Failed> {
        let token = self.value.get() + 1;
        let planes = Planes {
            token,
            count: 1,
            _pad: 0,
            resources: &self.target,
            sources: 0,
            destination: 0,
            rest: [0; 0x90 - 0x28],
        };
        check("PresentX", slot::<Present>(self.queue, 0x100)(self.queue, 1, &planes, std::ptr::null()))
    }

    /// Signals the fence after what was submitted and waits for it, as a
    /// title with one frame in flight does.
    fn sync(&self) -> Result<(), Failed> {
        let value = self.value.get() + 1;
        self.value.set(value);
        check("Signal", slot::<Signal>(self.queue, 0x80)(self.queue, self.fence, value))?;
        if slot::<Completed>(self.fence, 0x40)(self.fence) < value {
            check("SetEventOnCompletion", slot::<WaitFor>(self.fence, 0x48)(self.fence, value, std::ptr::null_mut()))?;
        }
        let reached = slot::<Completed>(self.fence, 0x40)(self.fence);
        Watchdog::pet();
        if reached < value {
            return Err(format!("the fence stopped at {reached}, short of {value}").into());
        }
        Ok(())
    }

    fn clear_and_present(&self, frames: u32, bench: &mut Bench) -> Result<String, Failed> {
        let host = crate::host::host().ok_or("no host")?;
        let before = host.presents().map_or(0, |p| p.count);
        let frames = frames.max(1);
        let looked = [0, frames / 2, frames - 1];
        let mut intervals = Vec::with_capacity(frames as usize);
        let mut last = Instant::now();
        for f in 0..frames {
            let bytes = colour_of(f);
            let colour = bytes.map(|b| f32::from(b) / 255.0);
            self.frame(|t| t.clear(colour))?;
            self.present()?;
            self.sync()?;
            let now = Instant::now();
            intervals.push(now - last);
            last = now;
            if looked.contains(&f) {
                let (_, _, pixels) = read_back(self.target)?;
                if let Some(at) = pixels.chunks_exact(4).position(|p| !near(p, &bytes)) {
                    let p = &pixels[at * 4..at * 4 + 4];
                    return Err(format!(
                        "frame {f}: pixel ({}, {}) is {p:?}, the clear was {bytes:?}",
                        at as u32 % SIDE,
                        at as u32 / SIDE
                    )
                    .into());
                }
                last = Instant::now();
            }
        }
        let presents = host.presents().ok_or("no swap chain")?;
        let shown = presents.count.wrapping_sub(before);
        if shown < frames {
            return Err(format!(
                "the swap chain took {shown} of {frames} presents (last {})",
                hr_text(presents.last.0)
            )
            .into());
        }
        // The first interval holds the stage's start, the ones after a
        // read-back are left out above.
        intervals.remove(0);
        bench.present = median(&mut intervals);
        Ok(format!("{frames} frames cleared and presented; 3 of them read back and matched"))
    }

    fn unified_memory(&self) -> Result<String, Failed> {
        let buffer = self.buffer_round_trip()?;
        let texture = self.texture_round_trip()?;
        Ok(format!("{buffer}; {texture}"))
    }

    /// A buffer the title writes is copied into another by a list; the title
    /// reads the copy, changes the first and the change must be there after
    /// the next submission. (Neither buffer has a host copy yet, so the
    /// layer copies at submission, `HostList::defer_copy`; the copy on the
    /// card and its landing are stage 4's.)
    fn buffer_round_trip(&self) -> Result<String, Failed> {
        const LEN: usize = 1 << 20;
        let src = self.buffer(UPLOAD_HEAP, LEN as u64, 0)?;
        let dst = self.buffer(DEFAULT_HEAP, LEN as u64, 0)?;
        let from = Self::mapped(src, LEN)?;
        for (i, b) in from.iter_mut().enumerate() {
            *b = (i as u32).wrapping_mul(2_654_435_761).rotate_right(13) as u8;
        }
        let copy = |t: &Self| {
            slot::<CopyBuffer>(t.list, 0x78)(t.list, dst, 0, src, 0, LEN as u64);
            Ok(())
        };
        self.frame(copy)?;
        self.sync()?;
        let to = Self::mapped(dst, LEN)?;
        if let Some(at) = first_difference(from, to) {
            return Err(format!("the buffer copy differs at byte {at}").into());
        }
        // The title changes three stretches: one page, a few bytes in the
        // middle of another, the last bytes.
        from[4096..8192].fill(0xa5);
        from[500_000..500_100].fill(0x5a);
        from[LEN - 17..].fill(0x3c);
        self.frame(copy)?;
        self.sync()?;
        if let Some(at) = first_difference(from, to) {
            return Err(format!("after the title changed the source, the copy differs at byte {at}").into());
        }
        Ok("a 1 MiB buffer copied, changed and copied again".into())
    }

    /// A texture the title fills in the console's tiled layout is copied into
    /// the render target, presented and read back; then part of it is
    /// rewritten in title memory and the next copy must show that part.
    fn texture_round_trip(&self) -> Result<String, Failed> {
        use victory_formats::texture::{Dimension, Swizzle, Texture, TextureDesc};
        let layout = Texture::new(&TextureDesc {
            dimension: Dimension::Tex2d,
            width: SIDE,
            height: SIDE,
            depth_or_array: 1,
            mips: 1,
            format: RGBA8,
            samples: 1,
            // What the layer takes a texture of layout 0 without target or
            // depth flags to be in (`resource::texture_layout`).
            swizzle: Swizzle::SW_4KB_S,
        })
        .map_err(|e| format!("no console layout for the texture: {e:?}"))?;
        let plane = &layout.planes[0].layout;
        let texture = self.committed(DEFAULT_HEAP, &Self::texture_desc(0), COPY_SOURCE)?;
        let memory = Self::mapped(texture, layout.size as usize)?;
        let pitch = SIDE as usize * 4;
        let mut linear: Vec<u8> = (0..SIDE * SIDE)
            .flat_map(|i| {
                let (x, y) = (i % SIDE, i / SIDE);
                [(x * 4) as u8, (y * 4) as u8, ((x ^ y) * 4) as u8, 255]
            })
            .collect();
        plane.tile(memory, 0, 0, &linear, pitch, 0).map_err(|e| format!("tiling failed: {e:?}"))?;
        let copied = |t: &Self| {
            slot::<CopyResource>(t.list, 0x88)(t.list, t.target, texture);
            Ok(())
        };
        self.frame(copied)?;
        self.present()?;
        self.sync()?;
        let (_, _, pixels) = read_back(self.target)?;
        if let Some(at) = first_far(&pixels, &linear) {
            return Err(format!("the tiled texture came through wrong at pixel ({}, {})", at % SIDE, at / SIDE).into());
        }
        // A 16x16 block rewritten: only the bytes that change are stored,
        // as a title updating part of a texture writes only its pages.
        for y in 8..24 {
            for x in 16..32 {
                let at = (y * SIDE as usize + x) * 4;
                linear[at..at + 4].copy_from_slice(&[250, 10, 200, 255]);
            }
        }
        let mut tiled = memory.to_vec();
        plane.tile(&mut tiled, 0, 0, &linear, pitch, 0).map_err(|e| format!("tiling failed: {e:?}"))?;
        let mut changed = 0;
        for (m, t) in memory.iter_mut().zip(&tiled) {
            if m != t {
                *m = *t;
                changed += 1;
            }
        }
        self.frame(copied)?;
        self.present()?;
        self.sync()?;
        let (_, _, pixels) = read_back(self.target)?;
        if let Some(at) = first_far(&pixels, &linear) {
            return Err(format!(
                "after the title rewrote {changed} bytes of the texture, pixel ({}, {}) is still old",
                at % SIDE,
                at / SIDE
            )
            .into());
        }
        Ok(format!("a {SIDE}x{SIDE} tiled texture uploaded, presented, partly rewritten and seen again"))
    }

    /// The root signature the program's layout asks for: four root
    /// constants (`b0`) and a table of one UAV (`u0`), serialized through the
    /// title's import and created on the device.
    fn root_signature(&self, imports: &Imports) -> Result<This, Failed> {
        let range = D3D12_DESCRIPTOR_RANGE {
            RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_UAV,
            NumDescriptors: 1,
            BaseShaderRegister: 0,
            RegisterSpace: 0,
            OffsetInDescriptorsFromTableStart: 0,
        };
        let parameters = [
            D3D12_ROOT_PARAMETER {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
                Anonymous: D3D12_ROOT_PARAMETER_0 {
                    Constants: D3D12_ROOT_CONSTANTS { ShaderRegister: 0, RegisterSpace: 0, Num32BitValues: 4 },
                },
                ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
            },
            D3D12_ROOT_PARAMETER {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
                Anonymous: D3D12_ROOT_PARAMETER_0 {
                    DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE { NumDescriptorRanges: 1, pDescriptorRanges: &range },
                },
                ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
            },
        ];
        let desc = D3D12_ROOT_SIGNATURE_DESC {
            NumParameters: 2,
            pParameters: parameters.as_ptr(),
            NumStaticSamplers: 0,
            pStaticSamplers: std::ptr::null(),
            Flags: D3D12_ROOT_SIGNATURE_FLAG_NONE,
        };
        let (mut blob, mut error) = (std::ptr::null_mut(), std::ptr::null_mut());
        let hr = (imports.serialize_root)((&raw const desc).cast(), 1, &mut blob, &mut error);
        if !error.is_null() {
            // SAFETY: the serializer hands out a blob reference of its own.
            drop(unsafe { windows::Win32::Graphics::Direct3D::ID3DBlob::from_raw(error) });
        }
        check("D3D12SerializeRootSignature", hr)?;
        if blob.is_null() {
            return Err("D3D12SerializeRootSignature gave no blob".into());
        }
        // SAFETY: as above; the blob is ours to release.
        let blob = unsafe { windows::Win32::Graphics::Direct3D::ID3DBlob::from_raw(blob) };
        // SAFETY: the blob's own bytes.
        let (at, len) = unsafe { (blob.GetBufferPointer().cast::<u8>().cast_const(), blob.GetBufferSize()) };
        let mut root = Self::null();
        check("CreateRootSignature", slot::<MakeRoot>(self.device, 0x80)(self.device, 0, at, len, &ROOT_SIGNATURE, &mut root))?;
        Ok(root)
    }

    fn translated_shader(&self, imports: &Imports, bench: &mut Bench) -> Outcome {
        let host = match crate::host::host() {
            Some(h) => h,
            None => return Outcome::Skip("no device".into()),
        };
        let mut model = D3D12_FEATURE_DATA_SHADER_MODEL { HighestShaderModel: D3D_SHADER_MODEL_6_6 };
        // SAFETY: a plain query into our structure.
        let asked = unsafe {
            host.device.CheckFeatureSupport(D3D12_FEATURE_SHADER_MODEL, (&raw mut model).cast(), size_of_val(&model) as u32)
        };
        if asked.is_err() || model.HighestShaderModel.0 < D3D_SHADER_MODEL_6_6.0 {
            return Outcome::Skip(format!(
                "the adapter has shader model {}.{}; translated shaders need 6.6",
                model.HighestShaderModel.0 >> 4,
                model.HighestShaderModel.0 & 15
            ));
        }
        match self.compute(imports, bench) {
            Ok(detail) => Outcome::Ok(detail),
            Err(f) => Outcome::Fail(f.reason),
        }
    }

    /// The synthetic compute program, from its container to the values it
    /// stores in title memory.
    fn compute(&self, imports: &Imports, bench: &mut Bench) -> Result<String, Failed> {
        use victory_shader::native::synth;
        let container = synth::container(&[synth::pattern_store()], &[]);
        // The translation a pipeline's first use makes, timed apart (the
        // pipeline below translates again: there is no cache on disk here).
        let began = Instant::now();
        crate::shader_cache::host_shader(&container).map_err(|e| format!("the translator refused our program: {e}"))?;
        bench.translate = Some(began.elapsed());
        let root = self.root_signature(imports)?;
        let desc = ComputeDesc {
            root,
            code: container.as_ptr(),
            size: container.len(),
            nodes: 0,
            cached: std::ptr::null(),
            cached_size: 0,
            flags: 0,
        };
        let mut pipeline = Self::null();
        check("CreateComputePipelineState", slot::<MakeCompute>(self.device, 0x58)(self.device, &desc, &PIPELINE, &mut pipeline))?;
        let mut heap = Self::null();
        let heap_desc = HeapDesc { kind: VIEW_HEAP, count: 4, flags: SHADER_VISIBLE, nodes: 0, after: [0; 2] };
        check("CreateDescriptorHeap", slot::<MakeHeap>(self.device, 0x70)(self.device, &heap_desc, &DESCRIPTOR_HEAP, &mut heap))?;
        let (cpu, gpu) = (slot::<HeapStart>(heap, 0x48)(heap), slot::<HeapStart>(heap, 0x50)(heap));
        const LEN: usize = 64 << 10;
        let out = self.buffer(DEFAULT_HEAP, LEN as u64, UNORDERED_FLAG)?;
        let memory = Self::mapped(out, LEN)?;
        for word in memory.chunks_exact_mut(4) {
            word.copy_from_slice(&0xdead_beefu32.to_le_bytes());
        }
        // R32_TYPELESS, buffer, raw.
        let view = RawUav { format: 39, dimension: 1, first: 0, count: (LEN / 4) as u32, stride: 0, counter_offset: 0, flags: 1, _pad: 0 };
        slot::<MakeUav>(self.device, 0x98)(self.device, out, Self::null(), &view, cpu);
        let record = |t: &Self, constants: [u32; 4]| {
            slot::<SetHeaps>(t.list, 0x100)(t.list, 1, &heap);
            slot::<SetRoot>(t.list, 0x108)(t.list, root);
            slot::<SetPipeline>(t.list, 0xc8)(t.list, pipeline);
            slot::<SetConstants>(t.list, 0x138)(t.list, 0, 4, constants.as_ptr(), 0);
            slot::<SetTable>(t.list, 0x118)(t.list, 1, gpu);
            slot::<Dispatch>(t.list, 0x70)(t.list, 1, 1, 1);
        };
        // A host pipeline is made on the pipeline's first use, by the
        // pipeline workers; a dispatch before it is ready is dropped, as a
        // title's is. Bind it once, then wait for it.
        let began = Instant::now();
        self.frame(|t| {
            slot::<SetPipeline>(t.list, 0xc8)(t.list, pipeline);
            Ok(())
        })?;
        self.sync()?;
        while crate::pipeline::host_pipeline(pipeline).is_none() {
            if began.elapsed() > Duration::from_secs(30) {
                return Err("no host pipeline 30 s after the pipeline's first use (the host refused it, see the log)".into());
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        bench.pipeline = Some(began.elapsed());
        let threads = synth::PATTERN_THREADS as usize;
        for (round, constants) in [[0x1234_5678u32, 7, 0, 0], [0x0bad_f00d, 1000, 0, 0]].into_iter().enumerate() {
            self.frame(|t| {
                record(t, constants);
                Ok(())
            })?;
            self.sync()?;
            for i in 0..LEN / 4 {
                let got = u32::from_le_bytes(memory[4 * i..4 * i + 4].try_into().unwrap_or_default());
                let want = if i < threads { synth::pattern_value(i as u32, [constants[0], constants[1]]) } else { 0xdead_beef };
                if got != want {
                    return Err(format!(
                        "dispatch {}: title memory at dword {i} holds {got:#010x}, the program stores {want:#010x}",
                        round + 1
                    )
                    .into());
                }
            }
        }
        // The shader's buffer has a host copy now, so a list's copy into it
        // is made on the card at its place in the list (`host/place.rs`) and
        // must come back into title memory as the shader's stores did.
        const AT: usize = 8192;
        const PIECE: usize = 4096;
        let on_card = crate::shadow::peek(memory.as_ptr() as u64 + AT as u64, PIECE as u64).is_some();
        let src = self.buffer(UPLOAD_HEAP, PIECE as u64, 0)?;
        let from = Self::mapped(src, PIECE)?;
        for (i, b) in from.iter_mut().enumerate() {
            *b = (i * 7 + 3) as u8;
        }
        self.frame(|t| {
            slot::<CopyBuffer>(t.list, 0x78)(t.list, out, AT as u64, src, 0, PIECE as u64);
            Ok(())
        })?;
        self.sync()?;
        if let Some(at) = first_difference(from, &memory[AT..AT + PIECE]) {
            return Err(format!("a copy into the shader's buffer did not land: byte {} differs", AT + at).into());
        }
        let after = u32::from_le_bytes(memory[AT + PIECE..AT + PIECE + 4].try_into().unwrap_or_default());
        if after != 0xdead_beef {
            return Err(format!("a copy into the shader's buffer wrote past its end ({after:#010x})").into());
        }
        Ok(format!(
            "a {}-byte compute container translated, made a host pipeline, dispatched twice; its stores and a list's copy \
             {} landed in title memory",
            container.len(),
            if on_card { "on the card" } else { "at submission" }
        ))
    }

    fn bench(&self, measured: Bench) -> Vec<Rate> {
        let mut rates = Vec::new();
        if let Some(d) = measured.present {
            rates.push(Rate { name: "present_interval", value: ms(d), unit: "ms", what: "one cleared 64x64 frame, presented and waited for (median)" });
        }
        if let Ok(r) = self.bench_copy() {
            rates.push(r);
        }
        if let Some(r) = bench_watch() {
            rates.push(r);
        }
        if let Some(r) = bench_compare() {
            rates.push(r);
        }
        if let Ok(r) = self.bench_recording() {
            rates.push(r);
        }
        if let Some(d) = measured.translate {
            rates.push(Rate { name: "shader_translation", value: ms(d), unit: "ms", what: "the synthetic compute container to DXIL" });
        }
        if let Some(d) = measured.pipeline {
            rates.push(Rate { name: "pipeline_creation", value: ms(d), unit: "ms", what: "first use to a host compute pipeline, translation included" });
        }
        rates
    }

    /// A 64 MiB title buffer, all of it changed by the title before each
    /// copy, copied by a list into another and waited for.
    fn bench_copy(&self) -> Result<Rate, Failed> {
        const LEN: usize = 64 << 20;
        let src = self.buffer(UPLOAD_HEAP, LEN as u64, 0)?;
        let dst = self.buffer(DEFAULT_HEAP, LEN as u64, 0)?;
        let from = Self::mapped(src, LEN)?;
        let mut times = Vec::new();
        for round in 0..6u8 {
            from.fill(round);
            let began = Instant::now();
            self.frame(|t| {
                slot::<CopyBuffer>(t.list, 0x78)(t.list, dst, 0, src, 0, LEN as u64);
                Ok(())
            })?;
            self.sync()?;
            times.push(began.elapsed());
        }
        times.remove(0);
        let t = median(&mut times).ok_or("no rounds")?;
        Ok(Rate {
            name: "buffer_copy",
            value: LEN as f64 / (1 << 20) as f64 / t.as_secs_f64(),
            unit: "MB/s",
            what: "64 MiB the title rewrote, copied by a list into another title buffer (median of 5)",
        })
    }

    /// Commands a list records per second: render-target clears of one
    /// pixel, which go through the list's table and onto the host list.
    fn bench_recording(&self) -> Result<Rate, Failed> {
        const CALLS: u32 = 20_000;
        let mut took = Duration::ZERO;
        self.frame(|t| {
            t.transition(t.target, COMMON, RENDER_TARGET);
            let clear = slot::<ClearRtv>(t.list, 0x1a0);
            let one = Rect { left: 0, top: 0, right: 1, bottom: 1 };
            let began = Instant::now();
            for i in 0..CALLS {
                let c = (i % 255) as f32 / 255.0;
                clear(t.list, t.rtv, &[c, c, c, 1.0], 1, &one);
            }
            took = began.elapsed();
            t.transition(t.target, RENDER_TARGET, COMMON);
            Ok(())
        })?;
        self.sync()?;
        Ok(Rate {
            name: "list_recording",
            value: f64::from(CALLS) / took.as_secs_f64(),
            unit: "calls/s",
            what: "ClearRenderTargetView calls through the list's table onto the host list",
        })
    }
}

/// What stage 2 and the shader stage measured for the benchmark.
#[derive(Default)]
struct Bench {
    present: Option<Duration>,
    translate: Option<Duration>,
    pipeline: Option<Duration>,
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn median(times: &mut [Duration]) -> Option<Duration> {
    times.sort();
    times.get(times.len() / 2).copied()
}

/// The colour of frame `f`: different from one frame to the next.
fn colour_of(f: u32) -> [u8; 4] {
    [(f * 37 % 256) as u8, (255 - f * 11 % 256) as u8, (f * 101 % 256) as u8, 255]
}

/// Within one unit of each channel (a conversion of `n / 255` may round).
fn near(a: &[u8], b: &[u8]) -> bool {
    a.iter().zip(b).all(|(x, y)| x.abs_diff(*y) <= 1)
}

fn first_difference(a: &[u8], b: &[u8]) -> Option<usize> {
    a.iter().zip(b).position(|(x, y)| x != y)
}

/// The first pixel of `a` not [`near`] the same one of `b`.
fn first_far(a: &[u8], b: &[u8]) -> Option<u32> {
    a.chunks_exact(4).zip(b.chunks_exact(4)).position(|(x, y)| !near(x, y)).map(|i| i as u32)
}

/// The host copy of a title texture of RGBA8, read back: width, height and
/// tight rows. The title's work must have been waited for.
fn read_back(texture: This) -> Result<(u32, u32, Vec<u8>), Failed> {
    let host = crate::host::host().ok_or("no host")?;
    let image = crate::resource::host_resource(texture).ok_or("the texture has no host copy")?;
    // SAFETY: a live resource.
    let desc = unsafe { image.GetDesc() };
    let (width, height) = (desc.Width as u32, desc.Height);
    let pitch = (width * 4).next_multiple_of(D3D12_TEXTURE_DATA_PITCH_ALIGNMENT);
    let size = u64::from(pitch) * u64::from(height);
    let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() };
    let buffer = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    let mut back: Option<ID3D12Resource> = None;
    // SAFETY: plain creation of a readback buffer.
    unsafe {
        host.device
            .CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &buffer, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut back)
            .map_err(|e| format!("no readback buffer: {e}"))?;
    }
    let back = back.ok_or("no readback buffer")?;
    let list = host.new_list().map_err(|e| format!("no host list: {e}"))?;
    let before = crate::resource::host_transition(texture, COPY_SOURCE).map_or(COPY_SOURCE, |(_, b)| b);
    let state = |s: u32| D3D12_RESOURCE_STATES(s as i32);
    // SAFETY: an open list, live resources, locations in the host layout;
    // the borrowed pointers in the locations are never released by them.
    unsafe {
        if before != COPY_SOURCE {
            list.list.ResourceBarrier(&[crate::host::transition(image, state(before), D3D12_RESOURCE_STATE_COPY_SOURCE)]);
        }
        let to = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(&back),
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                    Offset: 0,
                    Footprint: D3D12_SUBRESOURCE_FOOTPRINT { Format: desc.Format, Width: width, Height: height, Depth: 1, RowPitch: pitch },
                },
            },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(image),
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
        };
        list.list.CopyTextureRegion(&to, 0, 0, 0, &from, None);
        if before != COPY_SOURCE {
            list.list.ResourceBarrier(&[crate::host::transition(image, D3D12_RESOURCE_STATE_COPY_SOURCE, state(before))]);
        }
    }
    if before != COPY_SOURCE {
        crate::resource::host_transition(texture, before);
    }
    list.close();
    host.execute(&[&list]);
    Watchdog::pet();
    let mut at = std::ptr::null_mut();
    // SAFETY: the copy has run (`execute` waits); the buffer holds `size`
    // bytes.
    let rows = unsafe {
        back.Map(0, Some(&D3D12_RANGE { Begin: 0, End: size as usize }), Some(&mut at)).map_err(|e| format!("readback Map: {e}"))?;
        std::slice::from_raw_parts(at.cast::<u8>(), size as usize)
    };
    let mut pixels = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height as usize {
        let row = y * pitch as usize;
        pixels.extend_from_slice(&rows[row..row + width as usize * 4]);
    }
    // SAFETY: mapped above.
    unsafe { back.Unmap(0, None) };
    Ok((width, height, pixels))
}

/// What asking the write watch costs: `crate::watch::writes` over 256 MiB of
/// write-watched memory with one byte written every 64 KiB, per GiB
/// tracked.
fn bench_watch() -> Option<Rate> {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree};
    const MEM_WRITE_WATCH: u32 = 0x20_0000;
    const LEN: usize = 256 << 20;
    // SAFETY: a fresh allocation of ours, written inside its bounds and
    // freed at the end.
    unsafe {
        let at = VirtualAlloc(std::ptr::null(), LEN, MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH, PAGE_READWRITE).cast::<u8>();
        if at.is_null() {
            return None;
        }
        let mut times = Vec::new();
        let mut answered = true;
        for round in 0..8u8 {
            for i in (0..LEN).step_by(64 << 10) {
                at.add(i).write(round);
            }
            let began = Instant::now();
            answered &= crate::watch::writes(at as usize, at as usize + LEN, false).is_some();
            times.push(began.elapsed());
        }
        VirtualFree(at.cast(), 0, MEM_RELEASE);
        if !answered {
            return None;
        }
        times.remove(0);
        let t = median(&mut times)?;
        Some(Rate {
            name: "write_watch_query",
            value: t.as_secs_f64() * 1e6 * f64::from(1u32 << 30) / LEN as f64,
            unit: "us/GiB",
            what: "asking which pages of write-watched memory were written (4096 of 65536 pages)",
        })
    }
}

/// The AVX2 compare that finds what changed (`crate::safecopy::differ`),
/// over two equal 64 MiB buffers, so that it reads all of both.
fn bench_compare() -> Option<Rate> {
    if !std::arch::is_x86_feature_detected!("avx2") {
        return None;
    }
    const LEN: usize = 64 << 20;
    let a = vec![0x5au8; LEN];
    let b = a.clone();
    let mut times = Vec::new();
    for _ in 0..6 {
        let began = Instant::now();
        // SAFETY: two buffers of ours of `LEN` bytes; the processor has AVX2.
        let differ = unsafe { crate::safecopy::differ(a.as_ptr(), b.as_ptr(), LEN) };
        times.push(began.elapsed());
        if differ {
            return None;
        }
    }
    times.remove(0);
    let t = median(&mut times)?;
    Some(Rate {
        name: "avx2_compare",
        value: 2.0 * LEN as f64 / 1e9 / t.as_secs_f64(),
        unit: "GB/s",
        what: "bytes read comparing two equal 64 MiB buffers (median of 5)",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The structures are laid out as the layer reads them.
    #[test]
    fn structures_have_the_pc_layouts() {
        assert_eq!(size_of::<ResourceDesc>(), 56);
        assert_eq!(size_of::<Barrier>(), size_of::<D3D12_RESOURCE_BARRIER>());
        assert_eq!(size_of::<RawUav>(), 40);
        assert_eq!(size_of::<ComputeDesc>(), size_of::<D3D12_COMPUTE_PIPELINE_STATE_DESC>());
        assert_eq!(std::mem::offset_of!(Planes, resources), 0x10);
        assert_eq!(size_of::<Planes>(), 0x90);
        assert_eq!(size_of::<QueueDesc>(), 16);
    }

    #[test]
    fn frames_differ_in_colour() {
        assert!((0..120).all(|f| colour_of(f) != colour_of(f + 1)));
        assert!(near(&[10, 20, 30, 255], &[11, 19, 30, 255]));
        assert!(!near(&[10, 20, 30, 255], &[12, 20, 30, 255]));
    }
}
