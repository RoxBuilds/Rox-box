//! Draws on the host GPU. A list keeps the state
//! the title sets for its draws (pipeline, descriptor heaps, root
//! arguments, vertex and index buffers, targets); each draw then brings the
//! host list up to date: the host pipeline and our root signature, the
//! user data the translated shaders read, the host copies of the buffers
//! and descriptors they read, and the states of the textures involved.
//! State the host keeps across draws (viewports, scissors, blend factor,
//! stencil reference, topology, targets) is set on the host list as the
//! title sets it.

use std::sync::atomic::{AtomicU64, Ordering};

use victory_shader::resource::RootIndexedView;
use victory_shader::{ResourceType, RootType, UsedResource};

use crate::descriptor::View;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
use windows::core::Interface;

use crate::host::HostList;
use crate::object::This;
use crate::pipeline::host::HostPipeline;

/// A root argument as the title set it.
#[derive(Debug, Clone, Default)]
enum Root {
    #[default]
    None,
    /// A descriptor table's first descriptor (an address in a heap).
    Table(u64),
    Constants(Vec<u32>),
    /// A root view's GPU address.
    View(u64),
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct VertexView {
    address: u64,
    size: u32,
    stride: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct IndexView {
    address: u64,
    size: u32,
    format: u32,
}

/// Host states of a depth target, and of a texture shaders read.
const DEPTH_WRITE: u32 = 0x10;
const DEPTH_READ: u32 = 0x20;
const SHADER_RESOURCE: u32 = 0x40 | 0x80;

thread_local! {
    /// While a draw is prepared (`State::bind`, then its targets' states):
    /// the depth target the draw only tests (0: none, or one it writes),
    /// and whether a view its shaders read is of that same texture.
    static TESTED_DEPTH: std::cell::Cell<(usize, bool)> =
        const { std::cell::Cell::new((0, false)) };
    /// While a stage's tables are walked (`State::tables`): whether that
    /// stage's shader can store to buffers (`Translated::stores`).
    static STORES: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
    /// While `State::early_textures` runs the texture pass: the
    /// milliseconds and the bytes that run may take, in place of the
    /// pass's own per frame, and whether it is for a bake draw (which
    /// samples its textures at once and cannot be retried: the commit
    /// guard holds it back less, `crate::host::commit`).
    static PASS_BUDGET: std::cell::Cell<Option<(u64, u64, bool)>> = const { std::cell::Cell::new(None) };
}

/// The milliseconds of every frame the pass that uploads textures picked
/// by index takes before a shader that picks by index has asked for it
/// (`VICTORY_EARLY_TEXTURES_MS`, 12; 0: never, as before), and the
/// milliseconds it may take just before a draw of a title's composite
/// bake (`VICTORY_BAKE_TEXTURES_MS`, 1500; 0: nothing special).
///
/// The pass ran only from the first frame of a scene on, where it met tens
/// of thousands of slots at once and needed dozens of frames for them. A
/// title may bake composite textures of its own while it loads, hundreds of
/// frames before that, with a pixel shader
/// that picks its sources by index: whatever was not on the card then was
/// baked as nothing, once, for the run. On the console a texture is there
/// when its bytes are.
fn early_budgets() -> (u64, u64) {
    static MS: std::sync::OnceLock<(u64, u64)> = std::sync::OnceLock::new();
    *MS.get_or_init(|| {
        let ms = |name: &str, default: u64| {
            std::env::var(name).ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(default)
        };
        (ms("VICTORY_EARLY_TEXTURES_MS", 12), ms("VICTORY_BAKE_TEXTURES_MS", 1500))
    })
}

/// Whether a draw of the title's bake finds the textures it picks by index
/// as title memory has them at that moment (`VICTORY_BAKE_EXACT=0`: as the
/// pass leaves them, as before). The pass looks at a slot whose memory was
/// written or mapped a frame or two later, by when the rest of it has
/// landed, and at one uploaded whole a few frames later again, for a title
/// that fills memory unnoticed: a bake in between took the bytes from before
/// (another texture's), and what it bakes stays: coloured specks on the baked
/// surfaces for the whole scene in some runs, none in others. So the pass ahead
/// of a bake takes those slots too, and its uploads
/// are the exact ones (`crate::texture::BAKING`, which `crate::texture`'s
/// refresh and the heap's slot queue read), and the page writes of
/// DirectStorage that no pass took are kept for it
/// ([`crate::descriptor::host`]'s `BAKE_PAGE_WRITTEN`).
pub(crate) fn bake_exact() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_BAKE_EXACT").is_none_or(|v| v != "0"))
}

/// Whether a texture found changed with nothing to tell of it is looked at
/// again in the next frame (`VICTORY_RECHECK_SOON=0`: after four, as
/// before).
fn recheck_soon() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_RECHECK_SOON").is_none_or(|v| v != "0"))
}

/// Frames ahead the pass before a bake takes slots from that are due only
/// then ([`bake_exact`]): those of notices (1 and 2), and the first looks
/// again ([`RECHECK_FRAMES`]), these at the frame's first bake alone (a
/// look reads 48 KiB, and a frame bakes up to sixteen times).
const BAKE_AHEAD: [u64; 2] = [12, 2];

/// What [`crate::descriptor::host::HostHeap::texture_news`] said when the
/// pass before a bake last served all there was, plus one (0: it did not):
/// until something is new, the next bakes of the frame need no pass.
static BAKE_SERVED: AtomicU64 = AtomicU64::new(0);

/// What names a draw as an atlas bake (`VICTORY_BAKE_DETECT`), configured via
/// `[bake]` in an optional external shader profile (`VICTORY_SHADER_PROFILE`)
/// and/or structural draw-target detection (`VICTORY_BAKE_DETECT=shape`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BakeDetect {
    /// `=0`: only `old_pixel_shaders` from the external profile.
    Old,
    /// The default: `old_pixel_shaders`, `pixel_shaders`, and `geometry_shaders`
    /// from the external shader profile (empty by default when unconfigured).
    Hashes,
    /// `=shape`: configured hashes plus structural atlas detection
    /// ([`shaped_like_a_bake`]).
    Shape,
}

/// The value of `VICTORY_BAKE_DETECT` as [`BakeDetect`]: `0` uses only legacy
/// profile hashes, `shape` adds structural detection, anything else uses all
/// configured bake hashes.
fn bake_detect_of(value: Option<&std::ffi::OsStr>) -> BakeDetect {
    match value.and_then(|v| v.to_str()) {
        Some("0") => BakeDetect::Old,
        Some("shape") => BakeDetect::Shape,
        _ => BakeDetect::Hashes,
    }
}

/// [`BakeDetect`], read once from `VICTORY_BAKE_DETECT`.
fn bake_detect() -> BakeDetect {
    static MODE: std::sync::OnceLock<BakeDetect> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| bake_detect_of(std::env::var_os("VICTORY_BAKE_DETECT").as_deref()))
}

/// Whether a draw into `targets` render targets, the first of which is
/// `first` (width and height), with or without a depth target, looks like a
/// bake into an atlas: no depth, four targets or more, the first square and
/// at least 512 texels wide.
fn shaped_like_a_bake(depth: bool, targets: usize, first: Option<(u64, u32)>) -> bool {
    !depth && targets >= 4 && first.is_some_and(|(w, h)| w >= 512 && w == u64::from(h))
}

fn bake_hashes_with<'a>(
    profile: &crate::shader_profile::BakeProfile,
    mode: BakeDetect,
    hashes: impl IntoIterator<Item = &'a str>,
) -> bool {
    if profile.old_pixel_shaders.is_empty()
        && profile.pixel_shaders.is_empty()
        && profile.geometry_shaders.is_empty()
    {
        return false;
    }
    hashes.into_iter().any(|h| {
        profile.old_pixel_shaders.iter().any(|p| p == h)
            || (mode != BakeDetect::Old
                && (profile.pixel_shaders.iter().any(|p| p == h)
                    || profile.geometry_shaders.iter().any(|g| g == h)))
    })
}

/// Whether a draw whose stages have `hashes` matches an external bake profile.
fn bake_hashes<'a>(mode: BakeDetect, hashes: impl IntoIterator<Item = &'a str>) -> bool {
    bake_hashes_with(&crate::shader_profile::current().bake, mode, hashes)
}

/// Since the start: early runs of the texture pass, and those of them
/// made for a bake draw.
pub(crate) static EARLY_TEXTURE_PASSES: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];

/// Whether a buffer range a shader names through an unordered access view
/// is given to shaders as written (snapshot before the submission, compare
/// and write back after) only if that shader's code can store to buffers.
/// The title binds buffers its shaders only read through such views, and
/// every range declared costs a snapshot and a compare of its bytes:
/// gigabytes a frame, most of a second of snapshots and comparing. On by
/// default; `VICTORY_WRITE_SKIP=0` declares
/// every such range, as before.
fn write_skip() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_WRITE_SKIP").is_none_or(|v| v != "0"))
}

/// A run of indirect records clamped in one pass ([`windows`]): the host
/// copy they came from (its number), their kind and stride, the offset of
/// the first in that copy, how many, and the list's scratch buffer that
/// holds them clamped, record `i` at `i * step`.
#[derive(Clone)]
struct Window {
    copy: u64,
    kind: u32,
    stride: u32,
    /// How far apart the records are in the copy, and in the scratch
    /// buffer: what the title walks them by ([`windows`]).
    step: u32,
    base: u64,
    records: u32,
    scratch: ID3D12Resource,
}

/// The most records clamped in one pass ([`windows`]).
const WINDOW_MOST: u64 = 64;

/// Whether indirect draws that read record after record of one argument
/// buffer are clamped a window at a time (`VICTORY_INDIRECT_WINDOW=1`; off
/// by default).
///
/// The title issues its mesh draws one record per call, each from the next
/// slot of the same argument buffer: thousands a frame, and before each our
/// clamp ran as a compute dispatch of its own between two waits, so the
/// card went from drawing to compute and back for every record: a shadow
/// draw of next to nothing then cost over ten microseconds.
///
/// With the switch, when a draw asks for a record a short step after the
/// one drawn before, the clamp takes that record and the ones that step
/// apart after it (16, then twice as many each time, [`WINDOW_MOST`] at
/// most, never past the buffer) into
/// the list's scratch buffer in one pass, and the draws that follow read
/// their record there with no pass of ours and no barrier (the scratch
/// buffer stays readable as arguments until the next clamp), until one of
/// these: a compute
/// dispatch, a direct draw, an indirect call with a count buffer or more
/// than one record, a draw whose shaders can store to buffers, a record
/// outside the window, another host list.
///
/// The step is the one the title is seen to take, not the stride its
/// command signature names: it draws one record a call, for which the
/// stride means nothing, and a title may name 12 bytes (the three words of
/// a mesh dispatch) for records it keeps 32 bytes apart (most of a frame's
/// mesh records; some are 16 apart, which ends a window). Walked by the
/// named stride the window never opened, and thousands of records a frame
/// were still clamped one at a time.
///
/// What it costs the guard against garbage records: nothing against a
/// hang (every record is bounded as before and charged to the list's
/// budget as before), but the budget is charged when a window is clamped,
/// so also for records of the window's tail that are never drawn. Those
/// may hold garbage (slots of all ones: lists' budgets were spent on them
/// and the draws after emptied), so in a window a record
/// that asks for more than can run is left empty and charges nothing
/// (`crate::indirect::bound_ahead`); a tail record that looks real is
/// charged what it asks, a few thousand of 2^27. And arguments the title's CPU side changes are seen as
/// before, at the next submission.
fn windows() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_INDIRECT_WINDOW").is_some_and(|v| v == "1"))
}

/// Whether indirect executions of one record are clamped in batches
/// (`crate::indirect::batch`; `VICTORY_INDIRECT_BATCH=0`: each on its own,
/// or in windows ([`windows`])).
///
/// A busy frame has thousands of indirect executions (mesh draws,
/// dispatches and draws), and with the windows still half as many clamps:
/// each a dispatch between two full waits, with the scratch buffer moved
/// twice. A replayed frame was a tenth shorter with the clamps in batches.
///
/// A batch is opened by the first execution after a point where arguments
/// may have changed, and all executions up to the next such point read
/// theirs from it, each exactly the record it asks for (windows guess which
/// are next, and clamp what they do not know). Arguments in buffers change
/// by dispatches and by draws whose shaders store to buffers; those end the
/// batch, so the record an execution reads is clamped after all that was
/// recorded before it, and the pass waits for all of it, exactly as the
/// clamp of its own did. Between two executions of a batch is no wait of
/// ours: there is nothing between them to wait for but draws that store to
/// no buffer.
///
/// `VICTORY_INDIRECT_BATCH=2`: a dispatch does not end the batch before the
/// title waits for it (a barrier, or a pipeline sync) or draws: what a
/// dispatch writes is not there for a later command without a wait, and
/// dispatches the title runs side by side (its tile classes, up to 92 at a
/// time) are not held up by waits of ours. Off: whether the title has ways
/// to wait that do not pass through its lists' methods is not known.
fn batches() -> u8 {
    static ON: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *ON.get_or_init(|| match std::env::var("VICTORY_INDIRECT_BATCH").as_deref() {
        Ok("0") => 0,
        Ok("2") => 2,
        _ => 1,
    })
}

/// The records clamped by the pass opened last ([`batches`]).
struct Batch {
    table: crate::indirect::batch::Table,
    scratch: ID3D12Resource,
    /// Dispatches since, which may have written arguments
    /// (`VICTORY_INDIRECT_BATCH=2`).
    wrote: bool,
}

/// The host copies arguments were read from last: what a batch opens with,
/// as the executions after the first may read theirs from another one.
static ARGUMENT_COPIES: std::sync::Mutex<Vec<std::sync::Weak<crate::shadow::Shadow>>> = std::sync::Mutex::new(Vec::new());

/// [`ARGUMENT_COPIES`] that live, and `first` put in front of them.
fn argument_copies(first: &std::sync::Arc<crate::shadow::Shadow>) -> Vec<(u64, ID3D12Resource)> {
    let mut copies = ARGUMENT_COPIES.lock().unwrap_or_else(|e| e.into_inner());
    let live: Vec<_> = copies.iter().filter_map(std::sync::Weak::upgrade).filter(|s| s.id() != first.id()).collect();
    *copies = std::iter::once(first)
        .chain(&live)
        .take(crate::indirect::batch::SOURCES)
        .map(std::sync::Arc::downgrade)
        .collect();
    live.iter().map(|s| (s.id(), s.buffer.clone())).collect()
}

/// What one title list has set, and what its host list was last given.
#[derive(Default)]
pub(crate) struct State {
    pipeline: usize,
    /// View heap, sampler heap.
    heaps: [usize; 2],
    graphics: Vec<Root>,
    compute: Vec<Root>,
    vertex: [VertexView; 32],
    index: IndexView,
    /// Bound render target and depth resources, for their host states.
    targets: Vec<usize>,
    depth: usize,
    /// The host state the bound depth view needs ([`depth_state`]).
    depth_state: u32,
    /// The title's primitive topology (the host's may differ).
    topology: u32,
    /// What the title set that goes straight onto the host list, kept for
    /// the host list that continues this one after a cut ([`State::replay`]):
    /// whether a topology was set, the render target and depth handles, the
    /// viewports, the scissors, the blend factor and the stencil reference.
    topology_set: bool,
    target_handles: Vec<u64>,
    depth_handle: u64,
    targets_set: bool,
    /// The render target views and the depth view the host list has, once
    /// it has any ([`State::set_targets`]): per view the host texture (0:
    /// a null view) and what its descriptor says.
    host_targets: Option<(Vec<TargetKey>, Option<TargetKey>)>,
    viewports: Vec<D3D12_VIEWPORT>,
    scissors: Vec<RECT>,
    blend: Option<[f32; 4]>,
    stencil: Option<u32>,
    depth_bounds: Option<[f32; 2]>,
    /// The indirect records clamped last, if later draws may still read
    /// them ([`windows`]).
    window: Option<Window>,
    /// The records clamped in a batch, if later executions may still read
    /// them ([`batches`]).
    batch: Option<Batch>,
    /// The user data bound last, of the last stage that has some (for the
    /// report of a removed device, `crate::host::indirects`).
    #[cfg(feature = "diagnostics")]
    user: Vec<u32>,
    /// The tables the last bind made afresh for a program the stand-in for
    /// the indirect light follows (`crate::ambient`): root parameter, bytes
    /// into the view heap.
    ambient_tables: Vec<(u8, u32)>,
    /// What the host list has: pipeline, root signature, heaps.
    host_pipeline: usize,
    /// Graphics, compute.
    host_roots: [usize; 2],
    host_heaps: [usize; 2],
    /// Tables made afresh in this list: (table, slot count, contents) → first
    /// dynamic slot.
    fresh_tables: std::collections::HashMap<(u64, u32, u64), FreshTable, std::hash::BuildHasherDefault<crate::host::AddressHash>>,
    /// The dynamic slots the list holds: title heap, the number of the host
    /// heap they are of (`crate::descriptor::host::HostHeap::id`), first,
    /// count; given back when the list has run, is reset or released.
    dynamic: Vec<(usize, u64, u32, u32)>,
    /// Root SRVs bound as one-view tables in this list: (title heap, title
    /// address; 0 for none) → dynamic slot, and the host copy the slot
    /// shows: later draws of the list bring that one up to date, whatever
    /// copy the address would find by then (`shadow.rs` `tails`).
    root_srv_slots: std::collections::HashMap<(usize, u64), RootSlot>,
    /// Root UAVs bound as one-view tables in this list, likewise.
    root_uav_slots: std::collections::HashMap<(usize, u64), RootSlot>,
    /// The list ran and its dynamic slots were given back.
    tables_gone: bool,
    /// This recording, for [`crate::descriptor::host::HostHeap::make_listed`]
    /// (0: it has none yet).
    recording: u64,
    /// Draws and dispatches bound in the list ([`State::bind`]): a host list
    /// that recorded many is not kept idle (`crate::list` `idle_big`).
    binds: u32,
}

impl Drop for State {
    fn drop(&mut self) {
        for &(heap, id, first, n) in &self.dynamic {
            crate::descriptor::release_dynamic(heap, id, first, n);
        }
    }
}

impl State {
    /// The list's submission has run (submissions are waited for): the
    /// dynamic slots its tables were made in are given back, not kept
    /// until the title resets the list. A frame may ask for more than ten
    /// thousand tables; held until the resets they ran the 131,072 slots
    /// out, and thousands of draws a frame went without their tables.
    /// shortcut: a list the title runs a second time without a reset then
    /// reads slots others may have taken (logged once).
    pub(crate) fn tables_done(&mut self) {
        if self.tables_gone && self.dynamic.is_empty() {
            return;
        }
        if self.tables_gone {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| tracing::warn!("a list ran again after its descriptor tables were given back"));
        }
        for (heap, id, first, n) in self.dynamic.drain(..) {
            crate::descriptor::release_dynamic(heap, id, first, n);
        }
        self.fresh_tables.clear();
        self.recording = 0;
        self.root_srv_slots.clear();
        self.root_uav_slots.clear();
        self.tables_gone = true;
    }

    /// Draws and dispatches bound in the list ([`State::bind`]).
    pub(crate) fn binds(&self) -> u32 {
        self.binds
    }
}

/// An experiment on the title's scissor rectangles (`VICTORY_SCISSOR_TEST=full`):
/// they are all widened to the whole target. Where geometry is missing in
/// some runs and other geometry is cut by one rectangle, one run with this
/// on says whether the missing part is drawn and cut away by such a
/// rectangle, or not drawn at all. Off by default: a rectangle the title
/// sets is part of its picture.
fn scissor_test() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_SCISSOR_TEST").as_deref() == Ok("full"))
}

/// An experiment on mesh draws (`VICTORY_MESH_TEST`): 1 for `skip` (none
/// is issued), 2 for `scissor` (each runs its shaders but rasterises one
/// pixel), 0 otherwise. It tells a card hung by a mesh shader from one hung
/// by what the shader's triangles cover.
fn mesh_test() -> u8 {
    static ON: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *ON.get_or_init(|| match std::env::var("VICTORY_MESH_TEST").as_deref() {
        Ok("skip") => 1,
        Ok("scissor") => 2,
        _ => 0,
    })
}

/// `VICTORY_MESH_TEST=keep:<hex digits>[:<hex digits>]`: only the mesh
/// draws whose mesh shader's hash ends in one of the first digits (and
/// whose digit before it is one of the second, if given) are issued: a
/// search for the shaders that stop the card.
fn mesh_kept(pipeline: usize) -> bool {
    static KEEP: std::sync::OnceLock<Option<(String, String)>> = std::sync::OnceLock::new();
    let Some((last, before)) = KEEP.get_or_init(|| {
        let value = std::env::var("VICTORY_MESH_TEST").ok()?;
        let mut parts = value.strip_prefix("keep:")?.split(':');
        Some((parts.next()?.to_owned(), parts.next().unwrap_or("").to_owned()))
    }) else {
        return true;
    };
    let Some(p) = crate::pipeline::host_pipeline(pipeline as This) else {
        return true;
    };
    let Some(at) = p.name.find("mesh ") else {
        return true;
    };
    let hash = p.name[at + 5..].as_bytes();
    if hash.len() < 16 {
        return true;
    }
    last.as_bytes().contains(&hash[15]) && (before.is_empty() || before.as_bytes().contains(&hash[14]))
}

/// Draws recorded on the host GPU since the last frame line.
static DRAWN: AtomicU64 = AtomicU64::new(0);

/// Draws run on the host so far.
pub(crate) fn drawn() -> u64 {
    DRAWN.load(Ordering::Relaxed)
}

/// The draws traced since the last present (diagnostics builds, with
/// debug logging for this module): logged when the frame comes out black.
#[cfg(feature = "diagnostics")]
static FRAME: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// The draws traced since the last call.
#[cfg(feature = "diagnostics")]
pub(crate) fn take_frame() -> Vec<String> {
    std::mem::take(&mut *FRAME.lock().unwrap_or_else(|e| e.into_inner()))
}

/// The draws dropped since [`take_dropped`].
#[cfg_attr(not(feature = "diagnostics"), allow(dead_code))]
pub(crate) fn dropped_now() -> u64 {
    DROPPED.load(Ordering::Relaxed)
}

/// Draws, mesh dispatches and dispatches recorded but not run on the host
/// (no host pipeline) since the last call.
pub(crate) fn take_dropped() -> u64 {
    DROPPED.swap(0, Ordering::Relaxed)
}
/// A root SRV's dynamic slot in a list, and the host copy it shows.
type RootSlot = (u32, Option<std::sync::Arc<crate::shadow::Shadow>>);

/// Buffer views of the heap whose slot showed another host buffer than
/// the copy the pass brought up to date for them, since the start
/// (diagnostics; for the memory line). It stays 0 while the pass and the
/// slots agree; with `VICTORY_VIEW_EXTENT=0` it counts the views that were
/// read stale.
#[cfg(feature = "diagnostics")]
pub(crate) static HEAP_VIEWS_ELSEWHERE: AtomicU64 = AtomicU64::new(0);
/// Passes over the view heap's buffer views for shaders that pick them by
/// index ([`State::heap_buffers`]), and the microseconds they took.
pub(crate) static HEAP_BUFFER_PASSES: AtomicU64 = AtomicU64::new(0);
pub(crate) static HEAP_BUFFER_MICROS: AtomicU64 = AtomicU64::new(0);
/// Of the passes, nanoseconds since the last frame line: looking at slots,
/// the spans and refreshes of their copies, the texture side.
#[cfg(feature = "diagnostics")]
pub(crate) static HEAP_SPLIT: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// Passes after which every buffer slot of the heap was looked at again.
const HEAP_ROUND: u64 = 16;

/// Whether the slots looked at again for no write are a part of them at
/// every pass, each slot at every sixteenth (`VICTORY_HEAP_SPREAD=0`: all of
/// them at every sixteenth pass, as before).
///
/// All at once was some hundred milliseconds on a thread that records the
/// frame, every sixteenth frame (tens of thousands of slots; a pass took
/// twice as long in the frames with one). A slot is
/// looked at as often as before.
fn heap_spread() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_HEAP_SPREAD").is_none_or(|v| v != "0"))
}

/// The host copy and range the buffer view of each of a heap's slots reads
/// ([`State::heap_buffers`]), by slot.
#[derive(Default)]
struct HeapRanges<T>(Vec<Option<(T, u64, u64)>>);

impl<T> HeapRanges<T> {
    fn set(&mut self, slot: u32, range: Option<(T, u64, u64)>) {
        let slot = slot as usize;
        if range.is_none() && slot >= self.0.len() {
            return;
        }
        if slot >= self.0.len() {
            self.0.resize_with(slot + 1, || None);
        }
        self.0[slot] = range;
    }

    /// Forgets the slots of one of `parts` parts
    /// (`HostHeap::buffer_slots_of`).
    fn forget_part(&mut self, part: u32, parts: u32) {
        self.0.iter_mut().skip(part as usize).step_by(parts as usize).for_each(|r| *r = None);
    }

    fn ranges(&self) -> impl Iterator<Item = &(T, u64, u64)> {
        self.0.iter().flatten()
    }
}

#[cfg(test)]
mod heap_range_tests {
    use super::{HEAP_ROUND, HeapRanges};

    /// Round by round every slot is looked at again once, whatever was
    /// written between, and what a slot holds is what was found last.
    #[test]
    fn every_slot_is_looked_at_again_in_a_round() {
        const SLOTS: u32 = 1000;
        let mut ranges = HeapRanges(Vec::new());
        ranges.set(7, None);
        assert_eq!(ranges.ranges().count(), 0);
        for slot in (0..SLOTS).step_by(3) {
            ranges.set(slot, Some(((), 0, 1)));
        }
        let parts = HEAP_ROUND as u32;
        for pass in 1..=HEAP_ROUND {
            let part = (pass % HEAP_ROUND) as u32;
            ranges.forget_part(part, parts);
            for slot in (0..SLOTS).step_by(3).filter(|slot| slot % parts == part) {
                assert!(ranges.0[slot as usize].is_none(), "slot {slot} not forgotten");
                ranges.set(slot, Some(((), pass, pass)));
            }
            assert_eq!(ranges.ranges().count(), SLOTS.div_ceil(3) as usize);
        }
        assert!(ranges.ranges().all(|r| r.1 != 0), "a slot not looked at in a round");
        ranges.set(3, None);
        assert_eq!(ranges.ranges().count(), SLOTS.div_ceil(3) as usize - 1);
    }

    /// The spans of 4,400 copies from the ranges of 72,000 slots, as a pass
    /// makes them, by slot and from the table they were in. Run with
    /// `--ignored --nocapture`.
    #[test]
    #[ignore]
    fn spans_of_a_pass() {
        use std::collections::HashMap;
        use std::sync::Arc;
        let copies: Vec<Arc<u64>> = (0..4400).map(Arc::new).collect();
        let (mut table, mut slots) = (HashMap::new(), HeapRanges(Vec::new()));
        for slot in 0..72_000u32 {
            let copy = &copies[slot as usize * 7919 % 4400];
            table.insert(slot, (copy.clone(), u64::from(slot), u64::from(slot) + 64));
            slots.set(slot, Some((copy.clone(), u64::from(slot), u64::from(slot) + 64)));
        }
        fn join<S: std::hash::BuildHasher + Default>(ranges: &mut dyn Iterator<Item = &(Arc<u64>, u64, u64)>) -> usize {
            let mut spans: HashMap<usize, (Arc<u64>, u64, u64), S> = HashMap::default();
            for (copy, from, to) in ranges {
                let span = spans.entry(Arc::as_ptr(copy) as usize).or_insert_with(|| (copy.clone(), *from, *to));
                span.1 = span.1.min(*from);
                span.2 = span.2.max(*to);
            }
            spans.len()
        }
        let best = |f: &mut dyn FnMut() -> usize| {
            (0..30)
                .map(|_| {
                    let t = std::time::Instant::now();
                    assert_eq!(f(), 4400);
                    t.elapsed().as_secs_f64() * 1e3
                })
                .fold(f64::MAX, f64::min)
        };
        println!(
            "{:.2} ms from the table, {:.2} ms by slot",
            best(&mut || join::<std::hash::RandomState>(&mut table.values())),
            best(&mut || join::<std::hash::BuildHasherDefault<crate::host::AddressHash>>(&mut slots.ranges())),
        );
    }
}

/// Textures brought up to date for shaders that pick them by index
/// ([`State::heap_textures`]), the bytes of their memory, and the slots
/// still waiting.
pub(crate) static HEAP_TEXTURES: AtomicU64 = AtomicU64::new(0);
pub(crate) static HEAP_TEXTURE_BYTES: AtomicU64 = AtomicU64::new(0);
pub(crate) static HEAP_TEXTURES_WAITING: AtomicU64 = AtomicU64::new(0);
/// Passes that made no texture because the card had no room
/// (`crate::resource::room`), for the memory line.
pub(crate) static HEAP_TEXTURES_FULL: AtomicU64 = AtomicU64::new(0);

/// Frames after a whole upload of a texture picked by index at which its
/// memory is looked at again, each counted from the look before.
const RECHECK_FRAMES: [u64; 5] = [4, 12, 40, 120, 400];

/// Looks at uploaded textures picked by index that found the memory as it
/// was, and that found it changed (uploaded again), since the start; and of
/// the first, those of a texture whose pages are shown at other addresses
/// too ([`heap_recheck_aliased`]).
pub(crate) static HEAP_TEXTURES_RECHECKED: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// The most frames between two looks at an uploaded texture picked by index
/// whose pages the title shows at other addresses too ([`heap_recheck_aliased`]).
const ALIASED_FRAMES: u64 = 8;

/// Whether an uploaded texture picked by index whose memory's pages the
/// title shows at other addresses too is looked at again for good, at most
/// [`ALIASED_FRAMES`] frames apart, instead of [`RECHECK_FRAMES`] apart and
/// done after the last of them (`VICTORY_HEAP_RECHECK_ALIASED=0`: as the
/// others, as before).
///
/// What the title writes through another view of such pages is told of
/// there alone. The textures found changed after their upload with nothing
/// to tell of it had such pages, most of them at the fourth look, well over
/// a hundred frames after the upload: a slot done after the fifth look
/// showed what it held then for the rest of the run.
fn heap_recheck_aliased() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_HEAP_RECHECK_ALIASED").is_none_or(|v| v != "0"))
}

/// After the `looks`-th look at an uploaded texture picked by index found
/// its memory as it was: the frames to the next look and the count of looks
/// then, or `None` if it is done. `aliased`: its pages are shown at other
/// addresses too ([`heap_recheck_aliased`]): never done, and at most
/// [`ALIASED_FRAMES`] frames apart.
fn next_look(looks: u8, aliased: bool) -> Option<(u64, u8)> {
    match RECHECK_FRAMES.get(usize::from(looks) + 1) {
        Some(&wait) if aliased => Some((wait.min(ALIASED_FRAMES), looks + 1)),
        Some(&wait) => Some((wait, looks + 1)),
        None if aliased => Some((ALIASED_FRAMES, looks)),
        None => None,
    }
}

/// Uploads of textures already on the card made while it had no room for
/// new ones, since the start.
pub(crate) static HEAP_TEXTURES_REUPLOADED_FULL: AtomicU64 = AtomicU64::new(0);

/// Whether a pass over the textures picked by index on a full card holds
/// back the slots that need a new host copy (`VICTORY_FULL_CARD_PASS=0`:
/// no, as before: they stay due, each pass looks at the same ones again
/// and stops at the 257th). Held back, such a slot is looked at again
/// after about four seconds or as soon as the card has room, and the pass
/// goes on to the slots behind it: the ones whose host copy exists and
/// only wants bringing up to date, and the ones whose memory went (letting
/// their host copies go is what makes room). What is uploaded while the
/// card has room is the same either way.
fn full_card_pass() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_FULL_CARD_PASS").is_none_or(|v| v != "0"))
}

/// Whether the texture `view` shows takes `most` bytes of title memory or
/// fewer (0: none does): from the first level a placed texture's view shows,
/// or all of a texture object (`resource::small_when_full`).
fn small_texture(view: &View, most: u64) -> bool {
    if most == 0 {
        return false;
    }
    let bytes = match view {
        View::Placed(false, resource, desc) => {
            crate::resource::placed_bytes_from(resource, crate::resource::first_mip(desc))
        }
        View::Shader(object, _) => crate::resource::texture_bytes(*object as This),
        _ => None,
    };
    bytes.is_some_and(|b| b > 0 && b <= most)
}

/// Whether a texture that has its host copy is still brought up to date
/// while the card has no room for new ones (`VICTORY_REUPLOAD_WHEN_FULL=0`:
/// no).
fn reupload_when_full() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_REUPLOAD_WHEN_FULL").is_none_or(|v| v != "0"))
}

/// Whether textures picked by index are looked at again after their upload
/// (`VICTORY_HEAP_RECHECK=0`: no).
fn heap_recheck() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_HEAP_RECHECK").is_none_or(|v| v != "0"))
}
/// Times a placed texture's slot was passed over because its memory is not
/// mapped.
pub(crate) static HEAP_TEXTURES_SAME: AtomicU64 = AtomicU64::new(0);
pub(crate) static HEAP_TEXTURES_AWAY: AtomicU64 = AtomicU64::new(0);
/// Slots the pass found up to date already: their texture is uploaded by
/// the draws that read it through a table (it has a staging buffer).
pub(crate) static HEAP_TEXTURES_TABLED: AtomicU64 = AtomicU64::new(0);
/// The queue of textures picked by index at the last pass
/// (`TextureQueue::census`): slots in it, due, waiting for their memory,
/// uploaded in part, and the frames the oldest due one has waited.
pub(crate) static HEAP_TEXTURES_QUEUE: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];
/// Since the start: looks at a slot uploaded in part that found no more of
/// its memory mapped; uploads of textures not all mapped; and of those,
/// the ones whose first page was not there (asked by the first page alone,
/// as before, they counted as not there at all).
pub(crate) static HEAP_TEXTURES_PARTLY: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
static DROPPED: AtomicU64 = AtomicU64::new(0);

/// Marks the last traced draw as not run on the host.
#[cfg(feature = "diagnostics")]
fn dropped(pipeline: usize) {
    crate::pipeline::note_drop(pipeline as This);
    if let Some(last) = FRAME.lock().unwrap_or_else(|e| e.into_inner()).last_mut() {
        last.push_str(" DROPPED");
    }
}

/// Dispatches run on the host so far.
static DISPATCHED: AtomicU64 = AtomicU64::new(0);

pub(crate) fn dispatched() -> u64 {
    DISPATCHED.load(Ordering::Relaxed)
}

/// What a render target or depth view of the host is made from: the host
/// texture, and [`crate::descriptor::RtvRecord::key`].
type TargetKey = (usize, (usize, [u32; 7]));

/// Why a root view has no host address ([`root_view`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unbound {
    /// No host copy holds the address.
    NoCopy,
    /// A constant buffer's copy in upload memory found no scratch memory.
    NoScratch,
}

/// The host address of a root view: its buffer's host copy, or for a
/// constant buffer not 256-byte aligned there (the console needs 16) a
/// copy in upload memory.
/// shortcut: a root view has no size; constant buffers are taken as 64 KiB
/// (cut at their buffer's end), other views as all that is left of their
/// buffer. (They were taken as 1 MiB: most mesh vertex streams of a busy
/// frame reach further, and what lay past the first megabyte was never
/// brought up to date - meshes without bodies, exploded triangles.
/// `VICTORY_ROOT_VIEW_MB` sets a limit again.)
fn root_view(list: &HostList, address: u64, srv: bool) -> Result<u64, Unbound> {
    static LIMIT: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let limit = *LIMIT.get_or_init(|| {
        std::env::var("VICTORY_ROOT_VIEW_MB")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map_or(u64::MAX, |mb| mb << 20)
    });
    let shadow = crate::shadow::find(address, 1).ok_or(Unbound::NoCopy)?;
    let len = shadow
        .left(address)
        .min(if srv { limit } else { 1 << 16 });
    let gpu = shadow.gpu(address);
    if srv || gpu % 256 == 0 {
        shadow.refresh(list, address, len);
        #[cfg(feature = "diagnostics")]
        crate::book::root(gpu, len);
        return Ok(gpu);
    }
    list.fill_scratch(address as *const u8, len).ok_or(Unbound::NoScratch)
}

/// Whether a draw or dispatch whose constants found no scratch memory to be
/// copied into runs all the same, with null constant buffers in their place
/// (`VICTORY_NULL_CONSTANTS=1`, as before), instead of being dropped.
///
/// The lists' scratch chunks (256 KiB and 4 MiB, `crate::host`) were refused
/// tens of thousands of times on a PC that was out of commit. Every refusal
/// left a table's constant buffer null, and the draw ran
/// with all its constants zero: geometry collapsed, the tone map's exposure
/// zero (the picture white), the temporal pass's reprojection and weights
/// zero (two pictures at once), bloom from garbage. A pass dropped for a
/// frame leaves its target as the frame before had it.
fn null_constants() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_NULL_CONSTANTS").is_some_and(|v| v == "1"))
}

/// In what [`State::tables`] returns for a table made afresh: it found no
/// free dynamic slots (`u32::MAX`), or a constant buffer of it found no
/// scratch memory to be copied into (this). [`State::bind`] drops the draw
/// for either. (A table's own offset is a multiple of 32 bytes.)
const NO_CONSTANTS: u32 = u32::MAX - 1;

/// The byte offset [`State::tables`] hands on for a table made afresh at
/// dynamic slot `first`: [`NO_CONSTANTS`] if one of its constant buffers
/// found no scratch memory, unless such draws run with null constants
/// ([`null_constants`]).
fn fresh_offset(first: u32, constants_refused: bool, null_constants: bool) -> u32 {
    if constants_refused && !null_constants { NO_CONSTANTS } else { first * 32 }
}

/// Why [`State::bind`] drops a draw for its tables (what
/// [`State::tables`] made, by root parameter), if it does.
fn tables_refused(made: &[(u8, u32)]) -> Option<&'static str> {
    if made.iter().any(|m| m.1 == u32::MAX) {
        Some("no free slots for its descriptor tables")
    } else if made.iter().any(|m| m.1 == NO_CONSTANTS) {
        Some(CONSTANTS_REFUSED)
    } else {
        None
    }
}

/// The reason a draw or dispatch dropped for want of scratch memory for its
/// constants is counted under (`crate::pipeline::note_why`).
const CONSTANTS_REFUSED: &str = "no memory for its constants";

/// Draws and dispatches dropped because their constants found no scratch
/// memory ([`null_constants`]), since the start.
pub(crate) static CONSTANTS_DROPPED: AtomicU64 = AtomicU64::new(0);

/// Counts a draw or dispatch dropped for want of scratch memory for its
/// constants: in the drops of the frame line (diagnostics builds), and in a
/// line at the first and at every 4,096th.
fn constants_dropped() {
    let n = CONSTANTS_DROPPED.fetch_add(1, Ordering::Relaxed) + 1;
    crate::runreport::hint(victory_pipeline::report::LeftOut::OutOfMemory);
    #[cfg(feature = "diagnostics")]
    crate::pipeline::note_why(CONSTANTS_REFUSED.to_owned());
    if n == 1 || n.is_multiple_of(4096) {
        tracing::warn!(
            so_far = n,
            "a draw whose constants found no scratch memory is dropped, not drawn with null constants \
             (VICTORY_NULL_CONSTANTS=1: drawn so, as before)"
        );
    }
}

macro_rules! once {
    ($($arg:tt)*) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!($($arg)*));
    }};
}

fn set(roots: &mut Vec<Root>, slot: u32, root: Root) {
    let slot = slot as usize;
    if roots.len() <= slot {
        roots.resize(slot + 1, Root::None);
    }
    roots[slot] = root;
}

/// The pass over the buffer views of `heap` that shaders pick by index
/// ([`State::heap_buffers`]): their host slots are made, and the host copies
/// they show are to be refreshed.
fn heap_buffer_pass(heap: &crate::descriptor::Heap, host: &crate::descriptor::host::HostHeap, frame: u64) -> Vec<crate::host::aside::Refresh> {
    let started = std::time::Instant::now();
    // Per host heap: the host copy and range each slot's buffer view
    // reads, kept from pass to pass. Only the slots written since the
    // last pass are looked at again (83,000 slots a pass otherwise, 0.1 s
    // at best); all of them every 16 passes, because a view of a
    // released resource goes stale without a write, and a slot written
    // before its memory had a host copy is only made then.
    type Ranges = HeapRanges<std::sync::Arc<crate::shadow::Shadow>>;
    static RANGES: std::sync::Mutex<Option<std::collections::HashMap<usize, (u64, Ranges)>>> =
        std::sync::Mutex::new(None);
    let mut kept = RANGES.lock().unwrap_or_else(|e| e.into_inner());
    let (passes, ranges) = kept
        .get_or_insert_with(Default::default)
        .entry(std::ptr::from_ref(host) as usize)
        .or_insert_with(|| (0, HeapRanges(Vec::new())));
    let changed = host.buffer_changes();
    let all = *passes == 0 || (*passes % HEAP_ROUND == 0 && !heap_spread());
    let slots = if all {
        ranges.0.clear();
        host.buffer_slots()
    } else if heap_spread() {
        // The written ones, and this pass's part of the others.
        let (part, parts) = ((*passes % HEAP_ROUND) as u32, HEAP_ROUND as u32);
        ranges.forget_part(part, parts);
        let mut slots = host.buffer_slots_of(part, parts);
        slots.extend(changed);
        slots
    } else {
        changed
    };
    *passes += 1;
    // Whether the device was there before the pass: a view that removes
    // it is named below.
    static GONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let alive = !GONE.load(Ordering::Relaxed) && crate::descriptor::host::device_removed().is_none();
    let mut unordered = Vec::new();
    /// The title's heap, for the threads that look.
    struct Shared<'a>(&'a crate::descriptor::Heap);
    // SAFETY: a heap is free-threaded (its methods are called by any thread of
    // the title's), and it lives until all of them have looked.
    unsafe impl Sync for Shared<'_> {}
    impl Shared<'_> {
        fn handle(&self, index: u32) -> Option<u64> {
            self.0.handle(index)
        }
    }
    let heap = Shared(heap);
    // What a slot of the title's shows: its copy and range, and whether it is
    // an unordered view.
    let look = |index: u32| {
        // A draw's table's: the draw brings what it reads up to date.
        if host.copied_tabled(index, frame) {
            return None;
        }
        let view = host.mirror(index, heap.handle(index)?, true)?;
        // The copy the slot shows (`buffer_copy`), not the one the
        // view's range alone would find.
        let (shadow, address, len) = crate::descriptor::host::buffer_copy(&view)?;
        #[cfg(feature = "diagnostics")]
        if host.shows(index, &shadow.buffer) == Some(false) {
            HEAP_VIEWS_ELSEWHERE.fetch_add(1, Ordering::Relaxed);
        }
        let written = crate::picked::on() && view_kind(&view) == Some(ResourceType::Uav);
        Some(((shadow, address, address + len), written))
    };
    // Some slots at a time, on several threads.
    const AT_A_TIME: usize = 256;
    let parts: Vec<&[u32]> = slots.chunks(AT_A_TIME).collect();
    let found: Vec<std::sync::OnceLock<Vec<_>>> = parts.iter().map(|_| Default::default()).collect();
    crate::land::each_view(false, parts.len(), &|n| {
        let looked: Vec<_> = parts[n].iter().map(|&index| (index, look(index))).collect();
        // (After each piece while the device lives: a cheap query.)
        if alive
            && let Some(why) = crate::descriptor::host::device_removed()
            && !GONE.swap(true, Ordering::Relaxed)
        {
            let last: Vec<String> = parts[n]
                .iter()
                .filter_map(|&i| crate::descriptor::view(heap.handle(i)?).map(|v| format!("{i}: {v:?}")))
                .map(|s| s.chars().take(200).collect())
                .collect();
            tracing::error!(why, "the host device went while buffer views of the heap were made; perhaps one of: {last:#?}");
            crate::host::log_removal_context();
        }
        let _ = found[n].set(looked);
    });
    for (index, shows) in found.into_iter().filter_map(std::sync::OnceLock::into_inner).flatten() {
        let range = shows.map(|(range, written)| {
            if written {
                unordered.push(range.clone());
            }
            range
        });
        ranges.set(index, range);
    }
    if crate::picked::on() {
        crate::picked::shown(unordered, all);
    }
    #[cfg(feature = "diagnostics")]
    let looked = std::time::Instant::now();
    // Per host copy, the span its picked views cover.
    let mut spans: crate::host::ByCopy<(std::sync::Arc<crate::shadow::Shadow>, u64, u64)> = Default::default();
    let mut views = 0u32;
    for (shadow, from, to) in ranges.ranges() {
        views += 1;
        let key = std::sync::Arc::as_ptr(shadow) as usize;
        let span = spans.entry(key).or_insert_with(|| (shadow.clone(), *from, *to));
        span.1 = span.1.min(*from);
        span.2 = span.2.max(*to);
    }
    drop(kept);
    let bytes: u64 = spans.values().map(|s| s.2 - s.1).sum();
    // Memory that is not write-watched is copied whole by every refresh.
    // It was once refreshed only every 120 frames (unmapped pages read one
    // by one cost gigabytes a frame before they were remembered): streamed
    // geometry then drew from stale buffers, and the garbage ended runs
    // (the card stopped answering within a hundred frames; with every frame
    // it ran on). The pass costs tens of milliseconds a frame this way.
    static WHOLE: std::sync::Mutex<Option<std::collections::HashMap<usize, u64>>> = std::sync::Mutex::new(None);
    let mut whole = WHOLE.lock().unwrap_or_else(|e| e.into_inner());
    let whole = whole.get_or_insert_with(Default::default);
    let mut found = Vec::with_capacity(spans.len());
    for (shadow, from, to) in spans.values() {
        if shadow.unwatched() {
            // By the copy's number: a copy made where one let go was
            // (`shadow::let_go_idle`) has never been refreshed.
            let key = shadow.id() as usize;
            // `VICTORY_UNWATCHED_FRAMES` sets the interval (1: every frame).
            static EVERY: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
            let every = *EVERY.get_or_init(|| {
                std::env::var("VICTORY_UNWATCHED_FRAMES")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(1)
            });
            match whole.get(&key) {
                Some(&last) if frame < last + every => continue,
                _ => {
                    whole.insert(key, frame);
                }
            }
        }
        found.push((shadow.clone(), *from, (*to).min(*from + shadow.left(*from))));
    }
    #[cfg(feature = "diagnostics")]
    let refreshed = std::time::Instant::now();
    crate::watch::look_at_holes();
    #[cfg(feature = "diagnostics")]
    for (sum, time) in HEAP_SPLIT.iter().zip([looked - started, refreshed - looked]) {
        sum.fetch_add(time.as_nanos() as u64, Ordering::Relaxed);
    }
    static LOGGED: AtomicU64 = AtomicU64::new(0);
    // The first pass and every growth of the set by half.
    let seen = LOGGED.load(Ordering::Relaxed);
    if u64::from(views) > seen + seen / 2 {
        LOGGED.store(u64::from(views), Ordering::Relaxed);
        tracing::info!(
            slots = slots.len(),
            views,
            copies = spans.len(),
            mb = bytes >> 20,
            ms = started.elapsed().as_millis(),
            "buffer views of the heap made ready for shaders that pick them by index"
        );
    }
    found
}

impl State {
    pub(crate) fn reset(&mut self, pipeline: This) {
        *self = State::default();
        self.pipeline = pipeline as usize;
    }

    /// The title's topology; console-only ones become host lists (see
    /// [`crate::primitive`]).
    pub(crate) fn set_topology(&mut self, list: &HostList, topology: u32) {
        self.topology = topology;
        self.topology_set = true;
        let host = match topology {
            RECTANGLES => 4,
            TRIANGLE_FAN | LINE_LOOP | QUADS | QUAD_STRIP => {
                once!(topology, "console topology drawn as a triangle list");
                4
            }
            t => t,
        };
        // SAFETY: an open list.
        unsafe {
            list.list
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY(host as i32))
        };
    }

    pub(crate) fn set_pipeline(&mut self, pipeline: This) {
        self.pipeline = pipeline as usize;
    }

    pub(crate) fn set_heaps(&mut self, heaps: &[This]) {
        self.heaps = [0; 2];
        for &h in heaps {
            if !h.is_null() {
                let samplers = crate::descriptor::heap(h).samplers();
                self.heaps[usize::from(samplers)] = h as usize;
            }
        }
    }

    fn roots(&mut self, compute: bool) -> &mut Vec<Root> {
        if compute {
            &mut self.compute
        } else {
            &mut self.graphics
        }
    }

    pub(crate) fn set_table(&mut self, compute: bool, slot: u32, handle: u64) {
        set(self.roots(compute), slot, Root::Table(handle));
    }

    pub(crate) fn set_view(&mut self, compute: bool, slot: u32, address: u64) {
        set(self.roots(compute), slot, Root::View(address));
    }

    pub(crate) fn set_constants(&mut self, compute: bool, slot: u32, values: &[u32], offset: u32) {
        let roots = self.roots(compute);
        let s = slot as usize;
        if !matches!(roots.get(s), Some(Root::Constants(_))) {
            set(roots, slot, Root::Constants(Vec::new()));
        }
        if let Root::Constants(c) = &mut roots[s] {
            let end = offset as usize + values.len();
            if c.len() < end {
                c.resize(end, 0);
            }
            c[offset as usize..end].copy_from_slice(values);
        }
    }

    pub(crate) fn set_vertex_buffers(&mut self, start: u32, views: &[VertexView]) {
        for (i, v) in views.iter().enumerate() {
            if let Some(slot) = self.vertex.get_mut(start as usize + i) {
                *slot = *v;
            }
        }
    }

    pub(crate) fn clear_vertex_buffers(&mut self, start: u32, count: u32) {
        for slot in self.vertex.iter_mut().skip(start as usize).take(count as usize) {
            *slot = VertexView::default();
        }
    }

    pub(crate) fn set_index_buffer(&mut self, view: Option<&IndexView>) {
        self.index = view.copied().unwrap_or_default();
    }

    /// Render targets and depth, set on the host list at once, unless it has
    /// the same ones already: the title sets the same targets again and
    /// again, each time a host view per target and a call of the driver
    /// (tens of thousands of times a frame). `VICTORY_BIND_CACHE=0`
    /// sets them every time.
    pub(crate) fn set_targets(&mut self, list: &HostList, handles: &[u64], depth: u64) {
        let Some(host) = crate::host::host() else {
            return;
        };
        self.target_handles.clear();
        self.target_handles.extend_from_slice(handles);
        self.depth_handle = depth;
        self.targets_set = true;
        self.targets.clear();
        let mut views = Vec::with_capacity(handles.len());
        let mut keys = Vec::with_capacity(handles.len());
        for &h in handles {
            let Some(record) = crate::descriptor::rtv_record(h) else {
                once!(
                    handle = format_args!("{h:#x}"),
                    "render target without a recorded view"
                );
                continue;
            };
            let Some(texture) = crate::resource::host_resource(record.resource) else {
                // A typed null view (or a target without a host copy) still
                // takes its slot: left out, the next live target would move
                // into it and the pixel shader's outputs reach the wrong
                // targets.
                // shortcut: a view without a description or a format has no
                // null view to stand for it and is left out, as before.
                let null = record
                    .desc()
                    .filter(|d| d.Format.0 != 0 && d.ViewDimension.0 != 0);
                if let Some(desc) = null {
                    views.push((None, Some(desc)));
                    keys.push((0, record.key()));
                }
                continue;
            };
            self.targets.push(record.resource as usize);
            #[cfg(feature = "diagnostics")]
            crate::capture::note_target(record.resource);
            views.push((Some(texture), record.desc()));
            keys.push((texture.as_raw() as usize, record.key()));
        }
        self.depth = 0;
        self.depth_state = DEPTH_WRITE;
        let dsv = crate::descriptor::dsv_record(depth).and_then(|record| {
            let texture = crate::resource::host_resource(record.resource)?;
            self.depth = record.resource as usize;
            self.depth_state = depth_state(texture, record.depth_desc().as_ref());
            if crate::resource::depth_and_unordered(record.resource) {
                crate::resource::DEPTH_UNORDERED[1].fetch_add(1, Ordering::Relaxed);
            }
            #[cfg(feature = "diagnostics")]
            crate::capture::note_target(record.resource);
            Some((texture, record))
        });
        let now = (keys, dsv.as_ref().map(|(texture, record)| (texture.as_raw() as usize, record.key())));
        static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *CACHE.get_or_init(|| std::env::var("VICTORY_BIND_CACHE").map_or(true, |v| v != "0"))
            && self.host_targets.as_ref() == Some(&now)
        {
            return;
        }
        self.host_targets = Some(now);
        let rtvs: Vec<_> = views.iter().map(|(texture, desc)| host.rtv(*texture, desc.as_ref())).collect();
        let dsv = dsv.map(|(texture, record)| host.dsv(texture, record.depth_desc().as_ref()));
        // SAFETY: an open list; the views were just made.
        unsafe {
            list.list.OMSetRenderTargets(
                rtvs.len() as u32,
                (!rtvs.is_empty()).then_some(rtvs.as_ptr()),
                false,
                dsv.as_ref().map(std::ptr::from_ref),
            )
        };
    }

    /// Viewports, scissors, the blend factor and the stencil reference: set
    /// on the host list at once, and kept for [`State::replay`].
    pub(crate) fn set_viewports(&mut self, list: &HostList, viewports: &[D3D12_VIEWPORT]) {
        #[cfg(feature = "diagnostics")]
        if let Some(v) = viewports.first() {
            crate::host::spans::viewport(v.Width as u32, v.Height as u32);
        }
        self.viewports.clear();
        self.viewports.extend_from_slice(viewports);
        crate::draw::viewports(list, viewports);
    }

    pub(crate) fn set_scissors(&mut self, list: &HostList, rects: &[RECT]) {
        self.scissors.clear();
        self.scissors.extend_from_slice(rects);
        if scissor_test() {
            // The experiment: every rectangle the title sets is widened to
            // the largest target there is, so nothing is cut by one. What
            // the listing shows (`op_text`) stays the title's own.
            let full = RECT {
                left: 0,
                top: 0,
                right: 16384,
                bottom: 16384,
            };
            crate::draw::scissors(list, &vec![full; rects.len()]);
            return;
        }
        crate::draw::scissors(list, rects);
    }

    pub(crate) fn set_blend_factor(&mut self, list: &HostList, factor: Option<&[f32; 4]>) {
        self.blend = factor.copied();
        crate::draw::blend_factor(list, factor);
    }

    pub(crate) fn set_stencil_ref(&mut self, list: &HostList, reference: u32) {
        self.stencil = Some(reference);
        crate::draw::stencil_ref(list, reference);
    }

    pub(crate) fn set_depth_bounds(&mut self, list: &HostList, min: f32, max: f32) {
        self.depth_bounds = Some([min, max]);
        crate::draw::depth_bounds(list, min, max);
    }

    /// `list` is a new host list that continues the title's list (it was
    /// cut at a wait for a memory value, `crate::list::cut`): a new host
    /// list has nothing set, so what the title set so far is set again.
    /// The pipeline, the root signatures and the heaps are set by the next
    /// draw or dispatch (`bind`, which is told here that the host list has
    /// none); root arguments, vertex and index buffers are set by every
    /// draw anyway. The dynamic descriptor slots and the tables made in
    /// them stay: the parts before the cut have not run yet, and they are
    /// given back when the title's list is reset.
    /// shortcut: the render target views are made again from the handles'
    /// descriptors as they are now, not as they were when the title set
    /// the targets (a descriptor rewritten in between names another view).
    pub(crate) fn replay(&mut self, list: &HostList) {
        self.host_pipeline = 0;
        self.host_roots = [0; 2];
        self.host_heaps = [0; 2];
        if self.topology_set {
            self.set_topology(list, self.topology);
        }
        self.host_targets = None;
        if self.targets_set {
            let handles = std::mem::take(&mut self.target_handles);
            self.set_targets(list, &handles, self.depth_handle);
        }
        if !self.viewports.is_empty() {
            crate::draw::viewports(list, &self.viewports);
        }
        if !self.scissors.is_empty() {
            let rects = std::mem::take(&mut self.scissors);
            self.set_scissors(list, &rects);
        }
        if let Some(factor) = self.blend {
            crate::draw::blend_factor(list, Some(&factor));
        }
        if let Some(reference) = self.stencil {
            crate::draw::stencil_ref(list, reference);
        }
        if let Some([min, max]) = self.depth_bounds {
            crate::draw::depth_bounds(list, min, max);
        }
    }

    /// The user-data value of a root argument, as translated shaders read
    /// it: a table's byte offset in its heap, a constant's value.
    fn user_value(&self, compute: bool, slot: u8, root: RootType, dword: u32) -> u32 {
        match (self.roots_ref(compute).get(usize::from(slot)), root) {
            (Some(Root::Table(handle)), RootType::Table) => match self.heap_of(*handle) {
                Some(heap) => (handle - heap.start) as u32,
                None => {
                    once!(
                        handle = format_args!("{handle:#x}"),
                        "descriptor table outside the bound heaps"
                    );
                    0
                }
            },
            (Some(Root::Constants(c)), RootType::Constants) => {
                c.get(dword as usize).copied().unwrap_or(0)
            }
            // The host's own ([`Self::host_word`]): no root argument.
            (_, RootType::Other(_)) => 0,
            _ => {
                once!(
                    slot,
                    ?root,
                    "a shader reads a root argument the title did not set"
                );
                0
            }
        }
    }

    /// A user-data dword the host computes for a translated shader
    /// (`victory_shader::emit::USER_MEMORY_BASE`, `USER_VIEW_ADDRESS`).
    fn host_word(
        &self,
        compute: bool,
        kind: u32,
        slot: u8,
        dword: u32,
        memory: Option<&crate::memory::Ready>,
    ) -> u32 {
        use victory_shader::emit::{USER_MEMORY_BASE, USER_ROOT_ADDRESS, USER_VIEW_ADDRESS, USER_VIEW_BIAS};
        if kind == USER_MEMORY_BASE {
            return memory.map_or(0, |m| m.base);
        }
        // All of every group, unless the dispatch says otherwise.
        if kind == victory_shader::emit::USER_DISPATCH_THREADS_X {
            return 0;
        }
        if kind == USER_ROOT_ADDRESS || kind == USER_ROOT_ADDRESS + 1 {
            return match self.roots_ref(compute).get(usize::from(slot)) {
                Some(&Root::View(address)) if kind == USER_ROOT_ADDRESS => address as u32,
                Some(&Root::View(address)) => (address >> 32) as u32,
                _ => 0,
            };
        }
        if kind == USER_VIEW_BIAS {
            // The view begins on 16 bytes of the copy the address finds
            // (`root_uav`, `root_srv_table`).
            // shortcut: found again here, not taken from the list's slot.
            return match self.roots_ref(compute).get(usize::from(slot)) {
                Some(&Root::View(address)) => crate::shadow::find(address, 1).map_or(0, |s| {
                    // SAFETY: a live buffer.
                    let base = unsafe { s.buffer.GetGPUVirtualAddress() };
                    ((s.gpu(address) - base) & 15) as u32
                }),
                _ => 0,
            };
        }
        if kind != USER_VIEW_ADDRESS && kind != USER_VIEW_ADDRESS + 1 {
            once!(kind, "a shader reads a host value this build does not know");
            return 0;
        }
        let address = self.view_address(compute, slot, dword);
        if address == 0 {
            // The first few say what lies there instead.
            static SAID: AtomicU64 = AtomicU64::new(0);
            if SAID.fetch_add(1, Ordering::Relaxed) < 8 {
                let view = match self.roots_ref(compute).get(usize::from(slot)) {
                    Some(&Root::Table(table)) => crate::descriptor::view(table + u64::from(dword) * 4),
                    _ => None,
                };
                tracing::warn!(slot, dword, ?view, "a shader takes the address of a buffer view that is not there: it reads 0");
            }
        }
        if kind == USER_VIEW_ADDRESS {
            address as u32
        } else {
            (address >> 32) as u32
        }
    }

    /// The title address the view at dword `dword` of the table in root
    /// slot `slot` starts at, 0 without one.
    fn view_address(&self, compute: bool, slot: u8, dword: u32) -> u64 {
        match self.roots_ref(compute).get(usize::from(slot)) {
            Some(&Root::Table(table)) => crate::descriptor::view(table + u64::from(dword) * 4)
                .and_then(|view| crate::descriptor::host::view_address(&view))
                .unwrap_or(0),
            _ => 0,
        }
    }

    /// For a shader that reads title memory by address
    /// ([`crate::memory`]): its directory, fault notes and view slots,
    /// with the pages made ready of the root views the list has set and of
    /// the views whose addresses it takes (`t`: the ray-tracing structures
    /// it walks).
    fn memory_ready(
        &self,
        list: &HostList,
        t: &crate::pipeline::Translated,
        compute: bool,
        use_: crate::memory::Use,
    ) -> Option<crate::memory::Ready> {
        let heap = match self.heaps[0] {
            0 => None,
            h => crate::descriptor::heap(h as This).host(),
        }?;
        let (mut addresses, mut constants) = (Vec::new(), Vec::new());
        // (What it may write: its root views of unordered access.)
        let written: Vec<u64> = t
            .used
            .iter()
            .filter(|u| u.kind == ResourceType::Uav && u.root == RootType::Descriptor)
            .filter_map(|u| match self.roots_ref(compute).get(usize::from(u.slot)) {
                Some(&Root::View(address)) => Some(address),
                _ => None,
            })
            .collect();
        for root in self.roots_ref(compute) {
            match root {
                // (Of a program that only walks structures, the structures'
                // views below: its root views are constants and tables.)
                Root::View(address) if use_ != crate::memory::Use::Reads => addresses.push(*address),
                Root::Constants(values) => constants.extend_from_slice(values),
                _ => {}
            }
        }
        for u in &t.host.user_data {
            if u.root == RootType::Other(victory_shader::emit::USER_VIEW_ADDRESS) {
                addresses.push(self.view_address(compute, u.slot, u.dword));
            }
        }
        crate::memory::ready(list, heap, &addresses, &written, &constants, use_, t.threads)
    }

    /// The bound heap holding `handle`.
    fn heap_of(&self, handle: u64) -> Option<&'static crate::descriptor::Heap> {
        self.heaps
            .iter()
            .filter(|&&h| h != 0)
            .map(|&h| crate::descriptor::heap(h as This))
            .find(|h| h.index(handle).is_some())
    }

    /// Brings the host list up to date for a draw; false if the draw cannot
    /// run on the host.
    fn prepare(&mut self, list: &HostList, indexed: bool) -> bool {
        self.prepare_for(list, indexed, false)
    }

    /// As [`State::prepare`], for draws (`mesh` false) or mesh dispatches.
    fn prepare_for(&mut self, list: &HostList, indexed: bool, mesh: bool) -> bool {
        #[cfg(feature = "diagnostics")]
        if crate::pipeline::is_skipped(self.pipeline as This) {
            return false;
        }
        let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) else {
            return false;
        };
        if p.compute || p.mesh != mesh {
            once!(mesh, "a draw or mesh dispatch with another kind of pipeline");
            return false;
        }
        if p.rects && self.topology != RECTANGLES {
            // Its geometry stage would turn each triangle into a rectangle.
            once!("a rectangle pipeline drawn with another topology");
        }
        if !self.bind(list, p, false) {
            return false;
        }
        let l = &list.list;
        // Vertex buffers the input layout reads.
        let mut views = [D3D12_VERTEX_BUFFER_VIEW::default(); 32];
        let mut count = 0;
        for (slot, (view, &v)) in views.iter_mut().zip(&self.vertex).enumerate() {
            if p.slots & 1 << slot == 0 {
                continue;
            }
            count = slot + 1;
            if v.address == 0 || v.size == 0 {
                continue;
            }
            let Some(shadow) = crate::shadow::find_any(v.address, u64::from(v.size)) else {
                continue;
            };
            shadow.refresh(list, v.address, u64::from(v.size));
            *view = D3D12_VERTEX_BUFFER_VIEW {
                BufferLocation: shadow.gpu(v.address),
                SizeInBytes: v.size.min(u32::try_from(shadow.left(v.address)).unwrap_or(u32::MAX)),
                StrideInBytes: v.stride,
            };
        }
        // SAFETY: an open list; the views name live host buffers.
        unsafe {
            if count > 0 {
                l.IASetVertexBuffers(0, Some(&views[..count]));
            }
        }
        if indexed {
            let v = self.index;
            let Some(shadow) = crate::shadow::find_any(v.address, u64::from(v.size)) else {
                return false;
            };
            shadow.refresh(list, v.address, u64::from(v.size));
            let view = D3D12_INDEX_BUFFER_VIEW {
                BufferLocation: shadow.gpu(v.address),
                SizeInBytes: v.size.min(u32::try_from(shadow.left(v.address)).unwrap_or(u32::MAX)),
                Format: DXGI_FORMAT(v.format as i32),
            };
            // SAFETY: an open list; the view names a live host buffer.
            unsafe { l.IASetIndexBuffer(Some(&view)) };
        }
        const RENDER_TARGET: u32 = 0x4;
        for &t in &self.targets {
            crate::list::ensure_state(list, t as This, RENDER_TARGET);
        }
        if self.depth != 0 {
            // A depth texture shaders also write: what they wrote, first.
            list.twin_target(self.depth as This, self.depth_state == DEPTH_READ);
            // (If that recorded the pass that draws its depth plane, the
            // host list holds the pass's state: the title's again.)
            if list.take_state_lost() {
                self.replay(list);
                if !self.bind(list, p, false) {
                    return false;
                }
            }
            // A view that writes neither plane keeps the texture readable:
            // the depth read state, together with the shader read states
            // if this draw's shaders read the same texture (the write state
            // put it out of the state those views need, and a move to it
            // and back for every such draw).
            let state = if self.depth_state == DEPTH_READ {
                let (tested, read) = TESTED_DEPTH.get();
                if read && tested == self.depth {
                    DEPTH_READ | SHADER_RESOURCE
                } else {
                    DEPTH_READ
                }
            } else {
                DEPTH_WRITE
            };
            crate::list::ensure_state(list, self.depth as This, state);
        }
        true
    }

    /// A descriptor a shader reads through a table: its host slot made if
    /// it changed, constant data copied, textures made readable.
    /// What draws and dispatches share: heaps, our root signature, the
    /// pipeline, and per stage the user data, root views and the
    /// descriptors its tables hold.
    /// False if the draw or dispatch must not run: a root SRV it reads
    /// could not be given a view.
    ///
    /// A pass of ours recorded into the host list since the last draw, or
    /// by this bind itself when it brings a depth plane or its twin up to
    /// date (`crate::host::twins`), leaves the list with the pass's state:
    /// what the title set is then set again ([`State::replay`]) and bound
    /// anew (once: the second time finds the plane up to date).
    pub(crate) fn bind(&mut self, list: &HostList, p: &HostPipeline, compute: bool) -> bool {
        if list.take_state_lost() {
            self.replay(list);
        }
        let bound = self.bind_once(list, p, compute);
        if !list.take_state_lost() {
            return bound;
        }
        self.replay(list);
        self.bind_once(list, p, compute)
    }

    /// Whether the next bind sets the heaps, both root signatures and the
    /// pipeline on the host list whatever it had (tests).
    #[cfg(test)]
    pub(crate) fn host_has_nothing(&self) -> bool {
        self.host_pipeline == 0 && self.host_roots == [0; 2] && self.host_heaps == [0; 2]
    }

    /// [`State::bind`], without looking at whose state the host list has.
    fn bind_once(&mut self, list: &HostList, p: &HostPipeline, compute: bool) -> bool {
        self.binds = self.binds.saturating_add(1);
        let l = &list.list;
        // Textures picked by index, before anything asks for them.
        self.early_textures(list, p, compute);
        if crate::host::walls_trace() {
            self.walls_census(p, compute);
        }
        // A list without a view heap: one of the title's for the views the
        // host itself puts into heap slots.
        if self.heaps[0] == 0
            && p.stages.iter().any(|s| !s.root_uavs.is_empty() || !s.root_srvs.is_empty())
            && let Some(heap) = crate::descriptor::lent_heap()
        {
            once!("a dispatch without a view heap that needs one gets the title's");
            self.heaps[0] = heap;
        }
        // Heaps first: a root signature with directly indexed heaps must be
        // set after them.
        if self.host_heaps != self.heaps {
            let heaps: Vec<Option<ID3D12DescriptorHeap>> = self
                .heaps
                .iter()
                .filter(|&&h| h != 0)
                .filter_map(|&h| crate::descriptor::heap(h as This).host())
                .map(|h| Some(h.heap.clone()))
                .collect();
            heaps.iter().flatten().for_each(|h| list.keep(h));
            // SAFETY: an open list and shader-visible heaps.
            unsafe { l.SetDescriptorHeaps(&heaps) };
            self.host_heaps = self.heaps;
            self.host_roots = [0; 2];
        }
        // The depth target a draw only tests, for the views of it that the
        // draw's shaders read (`readable`).
        let tested = if !compute && self.depth_state == DEPTH_READ {
            self.depth
        } else {
            0
        };
        TESTED_DEPTH.set((tested, false));
        let root = p.root.as_raw() as usize;
        if self.host_roots[usize::from(compute)] != root {
            // SAFETY: an open list and a live root signature.
            unsafe {
                if compute {
                    l.SetComputeRootSignature(&p.root);
                } else {
                    l.SetGraphicsRootSignature(&p.root);
                }
            }
            self.host_roots[usize::from(compute)] = root;
        }
        // By host object: a title pipeline released and another made at its
        // address is another host pipeline (the list keeps the old one).
        if self.host_pipeline != p.pso.as_raw() as usize {
            // SAFETY: an open list and a live pipeline.
            unsafe { l.SetPipelineState(&p.pso) };
            self.host_pipeline = p.pso.as_raw() as usize;
            // The title may release the pipeline before it submits the list.
            list.bind_pipeline(&p.name, &p.pso, &p.root);
        }
        for stage in &p.stages {
            let t = stage.translated();
            STORES.set(t.stores);
            if t.stores && crate::picked::on() && crate::picked::picks(&stage.shader, &t.picks) {
                crate::picked::runs_in(list);
            }
            let made = self.tables(list, &t.used, &t.host.root_indexed_views, compute);
            STORES.set(true);
            match tables_refused(&made) {
                Some(CONSTANTS_REFUSED) => {
                    constants_dropped();
                    return false;
                }
                Some(_) => {
                    once!("a draw whose descriptor table found no free slots is dropped");
                    crate::runreport::hint(victory_pipeline::report::LeftOut::NoDescriptorSlots);
                    return false;
                }
                None => {}
            }
            if stage.shader.ambient.is_some() {
                self.ambient_tables.clear();
                self.ambient_tables.extend(made.iter().copied());
            }
            // What the shader stores through root views and raw addresses is
            // copied back into title memory afterwards.
            let writes = (t.stores || !write_skip())
                && !crate::pipeline::builder_quarantined(self.pipeline as This);
            // Title memory by address: what the shader finds it by.
            let memory = if t.host.root_srvs.contains(&victory_shader::emit::SLOT_MEMORY) {
                // Only through a store by address: what it stores through
                // views is declared with them.
                let use_ = if crate::pipeline::is_builder(self.pipeline as This) || writes && t.stores_by_address {
                    crate::memory::Use::Builds { back: writes }
                } else {
                    crate::memory::Use::Reads
                };
                let Some(ready) = self.memory_ready(list, t, compute, use_) else {
                    once!("a shader that reads title memory by address without a view heap to hold its views: the draw is dropped");
                    return false;
                };
                Some(ready)
            } else {
                None
            };
            if let Some((param, dwords)) = stage.user_data {
                // (A root signature holds 64 dwords.)
                let mut room = [0u32; 64];
                let mut more = Vec::new();
                let values: &mut [u32] = match room.get_mut(..dwords as usize) {
                    Some(values) => values,
                    None => {
                        more.resize(dwords as usize, 0);
                        &mut more
                    }
                };
                for u in &t.host.user_data {
                    if let RootType::Other(kind) = u.root {
                        values[u.index as usize] =
                            self.host_word(compute, kind, u.slot, u.dword, memory.as_ref());
                        continue;
                    }
                    let fresh = made
                        .iter()
                        .find(|m| m.0 == u.slot && u.root == RootType::Table);
                    values[u.index as usize] = match fresh {
                        Some(&(_, offset)) => offset,
                        None => self.user_value(compute, u.slot, u.root, u.dword),
                    };
                }
                #[cfg(feature = "diagnostics")]
                {
                    self.user.clear();
                    self.user.extend_from_slice(values);
                }
                // SAFETY: an open list; `values` covers the parameter.
                unsafe {
                    if compute {
                        l.SetComputeRoot32BitConstants(param, dwords, values.as_ptr().cast(), 0);
                    } else {
                        l.SetGraphicsRoot32BitConstants(param, dwords, values.as_ptr().cast(), 0);
                    }
                }
            }
            for (views, srv) in [(&stage.root_cbvs, false), (&stage.root_srvs, true)] {
                for &(slot, param) in views {
                    if srv && slot == victory_shader::emit::SLOT_MEMORY {
                        // Ours: the directory of title memory by address.
                        if let Some(m) = &memory {
                            // SAFETY: an open list; the directory lives as
                            // long as the process and the list holds it.
                            unsafe {
                                if compute {
                                    l.SetComputeRootShaderResourceView(param, m.directory);
                                } else {
                                    l.SetGraphicsRootShaderResourceView(param, m.directory);
                                }
                            }
                        }
                        continue;
                    }
                    if srv && slot == victory_shader::emit::SLOT_STRIDES {
                        // Ours, not the title's: the shader picks buffers
                        // from the view heap by an index it computes.
                        match self.heap_buffers(list) {
                            // SAFETY: an open list; the table lives as long
                            // as the heap the list holds.
                            Some(gpu) => unsafe {
                                if compute {
                                    l.SetComputeRootShaderResourceView(param, gpu);
                                } else {
                                    l.SetGraphicsRootShaderResourceView(param, gpu);
                                }
                            },
                            None => once!("a shader picks buffers from a view heap that is not set"),
                        }
                        continue;
                    }
                    // After upstream GPL-3.0 fork, commit f4b48ad766dd "Publish the
                    // pareto-residency Windows build and portable launcher": the bind of SLOT_TABLE_STRIDES, the
                    // table of strides and format kinds for a shader that reads views through descriptor tables.
                    // Here: `heap_strides` also keeps the table alive for the list.
                    if srv && slot == victory_shader::emit::SLOT_TABLE_STRIDES {
                        // Ours: the shader looks at the kinds of views in
                        // its descriptor tables only (the tables are made
                        // and mirrored by `tables`), so the table is bound
                        // without the heap pass that picking by index needs.
                        match self.heap_strides(list) {
                            // SAFETY: an open list; the table lives as long
                            // as the heap the list holds.
                            Some(gpu) => unsafe {
                                if compute {
                                    l.SetComputeRootShaderResourceView(param, gpu);
                                } else {
                                    l.SetGraphicsRootShaderResourceView(param, gpu);
                                }
                            },
                            None => once!("a shader inspects the kinds of views of its tables without a view heap set"),
                        }
                        continue;
                    }
                    let tabled = srv && crate::pipeline::host::bounded_root_srvs();
                    let Some(&Root::View(address)) = self.roots_ref(compute).get(usize::from(slot))
                    else {
                        once!(slot, "a shader reads a root view the title did not set");
                        // A table left unset is undefined: a null view.
                        if tabled && !self.root_srv_table(list, None, param, compute, false) {
                            return false;
                        }
                        continue;
                    };
                    // The shader adds the bytes a raw view begins early by.
                    let biased = srv
                        && t.host.user_data.iter().any(|u| {
                            u.slot == slot
                                && u.root == RootType::Other(victory_shader::emit::USER_VIEW_BIAS)
                        });
                    if tabled {
                        if !self.root_srv_table(list, Some(address), param, compute, biased) {
                            return false;
                        }
                        continue;
                    }
                    let mut gpu = match root_view(list, address, srv) {
                        Ok(gpu) => gpu,
                        // A constant buffer whose copy found no scratch
                        // memory: the draw is dropped ([`null_constants`];
                        // left unset it read whatever an earlier draw of the
                        // list bound there, or nothing defined).
                        Err(Unbound::NoScratch) if !null_constants() => {
                            constants_dropped();
                            return false;
                        }
                        Err(_) => continue,
                    };
                    if biased {
                        gpu -= u64::from(self.host_word(
                            compute,
                            victory_shader::emit::USER_VIEW_BIAS,
                            slot,
                            0,
                            None,
                        ));
                    }
                    // SAFETY: an open list; the address is in live memory.
                    unsafe {
                        match (compute, srv) {
                            (false, false) => l.SetGraphicsRootConstantBufferView(param, gpu),
                            (false, true) => l.SetGraphicsRootShaderResourceView(param, gpu),
                            (true, false) => l.SetComputeRootConstantBufferView(param, gpu),
                            (true, true) => l.SetComputeRootShaderResourceView(param, gpu),
                        }
                    }
                }
            }
            for &(slot, param) in &stage.root_uavs {
                if slot == victory_shader::emit::SLOT_FAULTS {
                    // Ours: where shaders note the pages they found empty.
                    if let Some(m) = &memory {
                        // SAFETY: an open list; the buffer lives as long as
                        // the process and the list holds it.
                        unsafe {
                            if compute {
                                l.SetComputeRootUnorderedAccessView(param, m.faults);
                            } else {
                                l.SetGraphicsRootUnorderedAccessView(param, m.faults);
                            }
                        }
                    }
                    continue;
                }
                let address = match self.roots_ref(compute).get(usize::from(slot)) {
                    Some(&Root::View(address)) => Some(address),
                    _ => {
                        once!(slot, "a shader writes through a root view the title did not set");
                        None
                    }
                };
                if !self.root_uav(list, address, param, compute, writes) {
                    return false;
                }
            }
        }
        // Depth textures it writes through unordered access: their colour
        // twins hold what they lack from here on.
        list.twins_written();
        true
    }

    /// Binds a root UAV: a raw unordered access view of the title buffer's
    /// host copy from `address` on, as a one-view table (a null view
    /// without an address, without a host copy shaders can write, or at an
    /// address not on 16 bytes), or as a root descriptor with
    /// `VICTORY_ROOT_SRV_DIRECT=1` (the dispatch is dropped then if there
    /// is nothing to bind). With `writes`, what the shader stored is copied
    /// back into title memory when the list has run. False if the draw
    /// must be dropped.
    /// shortcut: a root view has no size: the view, the refresh and the
    /// copy back cover all that is left of the title's buffer
    /// (`VICTORY_ROOT_UAV_MB` sets a limit for all three).
    fn root_uav(
        &mut self,
        list: &HostList,
        address: Option<u64>,
        param: u32,
        compute: bool,
        writes: bool,
    ) -> bool {
        const UNORDERED: i32 = D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0;
        static LIMIT: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        let limit = *LIMIT.get_or_init(|| {
            std::env::var("VICTORY_ROOT_UAV_MB")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .map_or(u64::MAX, |mb| mb << 20)
        });
        let find = |a: u64| {
            let s = crate::shadow::find(a, 1)?;
            // SAFETY: a live buffer.
            let flags = unsafe { s.buffer.GetDesc() }.Flags;
            if flags.0 & UNORDERED == 0 {
                once!("a root UAV of a buffer whose host copy shaders cannot write");
                return None;
            }
            Some(s)
        };
        let ready = |a: u64, s: &std::sync::Arc<crate::shadow::Shadow>| {
            let len = s.left(a).min(limit);
            s.refresh(list, a, len);
            if writes {
                #[cfg(feature = "diagnostics")]
                crate::host::wide_write(a, len, self.pipeline);
                s.gpu_writes(list, a, len);
            }
        };
        if !crate::pipeline::host::bounded_root_srvs() {
            let Some((a, s)) = address.and_then(|a| Some((a, find(a)?))) else {
                once!("a root UAV with nothing to bind: the draw is dropped");
                return false;
            };
            ready(a, &s);
            // From the 16 bytes below: the shader adds the rest.
            // SAFETY: a live buffer.
            let base = unsafe { s.buffer.GetGPUVirtualAddress() };
            let gpu = base + ((s.gpu(a) - base) & !15);
            // SAFETY: an open list; the address is in a live buffer.
            unsafe {
                if compute {
                    list.list.SetComputeRootUnorderedAccessView(param, gpu);
                } else {
                    list.list.SetGraphicsRootUnorderedAccessView(param, gpu);
                }
            }
            return true;
        }
        let heap = match self.heaps[0] {
            0 => None,
            h => Some(crate::descriptor::heap(h as This)),
        };
        let Some((heap, host)) = heap.and_then(|h| Some((h, h.host()?))) else {
            once!("a root UAV without a bound view heap: the draw is dropped");
            return false;
        };
        let key = (std::ptr::from_ref(heap) as usize, address.unwrap_or(0));
        let (index, shadow) = match self.root_uav_slots.get(&key) {
            Some((index, shadow)) => (*index, address.zip(shadow.clone())),
            None => {
                let shadow = address.and_then(|a| find(a).map(|s| (a, s)));
                let Some(index) = host.dynamic(1) else {
                    return false;
                };
                self.dynamic.push((key.0, host.id(), index, 1));
                if let Some((a, s)) = &shadow {
                    // SAFETY: a live buffer.
                    let base = unsafe { s.buffer.GetGPUVirtualAddress() };
                    let offset = s.gpu(*a) - base;
                    // Raw views start on 16 bytes: from the 16 bytes
                    // below, and the shader adds the rest to its offsets
                    // (`victory_shader::emit::USER_VIEW_BIAS`). (An address
                    // not on 16 bytes got a null view at first: the
                    // builder's counters are at such addresses.)
                    let bias = offset & 15;
                    host.raw_uav_at(index, &s.buffer, offset - bias, s.left(*a).min(limit) + bias);
                }
                if shadow.is_none() {
                    host.null_raw_uav_at(index);
                }
                self.root_uav_slots
                    .insert(key, (index, shadow.as_ref().map(|(_, s)| s.clone())));
                (index, shadow)
            }
        };
        if let Some((a, s)) = &shadow {
            ready(*a, s);
        }
        let handle = host.gpu_handle(index);
        // SAFETY: an open list; the slot is in the bound view heap.
        unsafe {
            if compute {
                list.list.SetComputeRootDescriptorTable(param, handle);
            } else {
                list.list.SetGraphicsRootDescriptorTable(param, handle);
            }
        }
        true
    }

    /// Binds a root SRV as a one-view table
    /// ([`crate::pipeline::host::bounded_root_srvs`]): a raw view of the
    /// title buffer's host copy from `address` to the copy's end, or a null
    /// view (no address, no host copy, or an address not on 16 bytes). One
    /// dynamic slot per address per list, given back with the list's
    /// others. False if no slot could be had or no view heap is bound.
    fn root_srv_table(
        &mut self,
        list: &HostList,
        address: Option<u64>,
        param: u32,
        compute: bool,
        biased: bool,
    ) -> bool {
        let heap = match self.heaps[0] {
            0 => None,
            h => Some(crate::descriptor::heap(h as This)),
        };
        let Some((heap, host)) = heap.and_then(|h| Some((h, h.host()?))) else {
            once!("a root SRV without a bound view heap: the draw is dropped");
            return false;
        };
        // A view that begins early is another view than one that does not
        // (title addresses stay below bit 63).
        let key = (
            std::ptr::from_ref(heap) as usize,
            address.unwrap_or(0) | u64::from(biased) << 63,
        );
        let (index, shadow) = match self.root_srv_slots.get(&key) {
            Some((index, shadow)) => (*index, address.zip(shadow.clone())),
            None => {
                let shadow = address.and_then(|a| crate::shadow::find(a, 1).map(|s| (a, s)));
                let Some(index) = host.dynamic(1) else {
                    return false;
                };
                self.dynamic.push((key.0, host.id(), index, 1));
                self.root_srv_slots
                    .insert(key, (index, shadow.as_ref().map(|(_, s)| s.clone())));
                match &shadow {
                    Some((a, s)) => {
                        // SAFETY: a live buffer.
                        let base = unsafe { s.buffer.GetGPUVirtualAddress() };
                        let offset = s.gpu(*a) - base;
                        if biased {
                            // From the 16 bytes below; the shader adds the
                            // rest (`victory_shader::emit::USER_VIEW_BIAS`).
                            let bias = offset & 15;
                            host.raw_at(index, &s.buffer, offset - bias, s.left(*a) + bias);
                        } else if offset.is_multiple_of(16) {
                            host.raw_at(index, &s.buffer, offset, s.left(*a));
                        } else {
                            // Raw views start on 16 bytes; one from the
                            // aligned start would shift every read.
                            once!(
                                address = format_args!("{a:#x}"),
                                "a root SRV not on 16 bytes reads zeros"
                            );
                            host.null_raw_at(index);
                        }
                    }
                    None => host.null_raw_at(index),
                }
                (index, shadow)
            }
        };
        // As for root descriptors (`root_view`): all that is left of the
        // buffer is brought up to date. The first 1 MiB only left what lay
        // past it stale (most mesh vertex streams reach further).
        if let Some((a, s)) = &shadow {
            s.refresh(list, *a, s.left(*a));
        }
        let handle = host.gpu_handle(index);
        // SAFETY: an open list; the slot is in the bound view heap.
        unsafe {
            if compute {
                list.list.SetComputeRootDescriptorTable(param, handle);
            } else {
                list.list.SetGraphicsRootDescriptorTable(param, handle);
            }
        }
        true
    }

    /// For a shader that picks buffers from the view heap by an index it
    /// computes (SM 6.6 dynamic resources): the GPU address of the heap's
    /// table of element strides, with every buffer view of the heap given
    /// its host slot and its memory brought up to date when `list` is
    /// submitted. Tables tell us which views a draw reads; nothing tells
    /// us which of the heap's a computed index names, so all of them are
    /// made ready, once per list.
    /// shortcut: the buffers are read, not written: what such a shader
    /// writes through a picked unordered access view stays in the host
    /// copy. A list refreshes them where it first runs such a shader;
    /// title writes recorded later in the same list are not seen.
    fn heap_buffers(&self, list: &HostList) -> Option<u64> {
        let heap = self
            .heaps
            .iter()
            .filter(|&&h| h != 0)
            .map(|&h| crate::descriptor::heap(h as This))
            .find(|h| !h.samplers())?;
        let host = heap.host()?;
        let gpu = host.strides_gpu()?;
        // One key per list, as textures have: no title resource is there.
        const ONCE: usize = usize::MAX - 0x5e;
        if !list.first_use(ONCE) {
            return Some(gpu);
        }
        // The list may outlive the title's heap object, which owns the table.
        if let Some(buffer) = host.strides_buffer() {
            list.keep(buffer);
        }
        // The pass itself once per frame: per list it ran dozens of times a
        // frame over tens of thousands of views and thousands of host
        // copies, seconds of every frame.
        // shortcut: what the title writes into such buffers between lists
        // of one frame is seen a frame late.
        static FRAME: AtomicU64 = AtomicU64::new(u64::MAX);
        let frame = crate::frame::current();
        if FRAME.swap(frame, Ordering::Relaxed) == frame {
            return Some(gpu);
        }
        let started = std::time::Instant::now();
        let mut stage = crate::timeline::span("the heap pass: begun, or done here");
        if crate::host::aside::on() {
            let at = std::ptr::from_ref(heap) as usize;
            list.refreshes_coming(move || {
                crate::descriptor::with_heap(at, |heap| heap.host().map(|host| heap_buffer_pass(heap, host, frame)))
                    .flatten()
                    .unwrap_or_default()
            });
        } else {
            for (shadow, from, to) in heap_buffer_pass(heap, host, frame) {
                shadow.refresh(list, from, to - from);
            }
        }
        #[cfg(feature = "diagnostics")]
        let refreshed = std::time::Instant::now();
        stage.next("the heap pass: textures");
        self.heap_textures(list, heap, host);
        stage.end();
        #[cfg(feature = "diagnostics")]
        HEAP_SPLIT[2].fetch_add(refreshed.elapsed().as_nanos() as u64, Ordering::Relaxed);
        HEAP_BUFFER_PASSES.fetch_add(1, Ordering::Relaxed);
        HEAP_BUFFER_MICROS.fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
        Some(gpu)
    }

    /// Whether a graphics draw looks like an atlas/decoration bake whose
    /// indexed textures must be uploaded beforehand ([`State::early_textures`]):
    /// matches either an optional external bake shader profile
    /// (`VICTORY_SHADER_PROFILE`) or, with `VICTORY_BAKE_DETECT=shape`, a draw
    /// into four or more square targets without depth ([`shaped_like_a_bake`]).
    fn is_bake_draw(&self, p: &HostPipeline) -> bool {
        let mode = bake_detect();
        if bake_hashes(mode, p.stages.iter().map(|s| s.shader.hash.as_str())) {
            return true;
        }
        mode == BakeDetect::Shape
            && shaped_like_a_bake(
                self.depth != 0,
                self.targets.len(),
                self.targets
                    .first()
                    .and_then(|&t| crate::resource::texture_kind(t as This))
                    .map(|(w, h, _)| (w, h)),
            )
    }

    /// The GPU address of the bound view heap's table of strides and format
    /// kinds, without the pass over the heap's views that [`State::heap_buffers`]
    /// makes: for shaders that read the kinds of views of their descriptor
    /// tables only (`SLOT_TABLE_STRIDES`), which the tables have made
    /// already, each slot's word with it (dynamic slots', too:
    /// `VICTORY_DYNAMIC_STRIDES`). A dynamic slot's word stays until the
    /// slot is made again, which is after the list that holds the slot has
    /// run (the slots are given back then, `VICTORY_TABLES_AT_LANDING` or
    /// the round-2 wait).
    ///
    /// After upstream GPL-3.0 fork, commit f4b48ad766dd "Publish the pareto-residency
    /// Windows build and portable launcher": `heap_strides`. Here: the same, with a key of its own for the list's
    /// hold on the table.
    fn heap_strides(&self, list: &HostList) -> Option<u64> {
        let heap = self
            .heaps
            .iter()
            .filter(|&&h| h != 0)
            .map(|&h| crate::descriptor::heap(h as This))
            .find(|h| !h.samplers())?;
        let host = heap.host()?;
        let gpu = host.strides_gpu()?;
        // One key per list, as `heap_buffers` has (its own, as a list may bind both).
        const ONCE: usize = usize::MAX - 0x5f;
        if list.first_use(ONCE)
            && let Some(buffer) = host.strides_buffer()
        {
            list.keep(buffer);
        }
        Some(gpu)
    }

    /// Runs the pass that uploads textures picked by index
    /// ([`State::heap_textures`]) where nothing has asked for it yet
    /// ([`early_budgets`]): once a frame for a few milliseconds, at the
    /// frame's first draw with a view heap set, and with a larger budget
    /// of its own before every draw whose pixel shader is one of the
    /// title's bake programs. The uploads are recorded into `list` ahead
    /// of the draw being bound, as the scene's pass records them.
    fn early_textures(&self, list: &HostList, p: &HostPipeline, compute: bool) {
        let (early_ms, bake_ms) = early_budgets();
        if compute || (early_ms == 0 && bake_ms == 0) {
            return;
        }
        let bake = bake_ms > 0 && self.is_bake_draw(p);
        static FRAME: AtomicU64 = AtomicU64::new(u64::MAX);
        // The milliseconds this frame's bake draws have taken so far: a
        // frame has several, and together they get `bake_ms`.
        static BAKED_MS: AtomicU64 = AtomicU64::new(0);
        let frame = crate::frame::current();
        let first = FRAME.swap(frame, Ordering::Relaxed) != frame;
        if first {
            BAKED_MS.store(0, Ordering::Relaxed);
        }
        let left = bake_ms.saturating_sub(BAKED_MS.load(Ordering::Relaxed));
        let budget = if bake && left > 0 {
            // What a bake may read must be there now: up to 2 GiB.
            (left, 2 << 30)
        } else if first && early_ms > 0 {
            (early_ms, 64 << 20)
        } else {
            return;
        };
        let Some(heap) = self
            .heaps
            .iter()
            .filter(|&&h| h != 0)
            .map(|&h| crate::descriptor::heap(h as This))
            .find(|h| !h.samplers())
        else {
            return;
        };
        let Some(host) = heap.host() else {
            return;
        };
        let bake = bake && left > 0;
        if bake && !first && bake_exact() {
            let (writes, noted) = host.texture_news();
            if !noted && BAKE_SERVED.load(Ordering::Relaxed) == writes + 1 {
                return;
            }
        }
        let n = EARLY_TEXTURE_PASSES[usize::from(bake)].fetch_add(1, Ordering::Relaxed) + 1;
        if bake && n.is_power_of_two() {
            let [slots, due, ..] = HEAP_TEXTURES_QUEUE.each_ref().map(|c| c.load(Ordering::Relaxed));
            tracing::info!(
                frame,
                bakes = n,
                queued_at_the_last_pass = slots,
                due_at_the_last_pass = due,
                uploaded_so_far = HEAP_TEXTURES.load(Ordering::Relaxed),
                "a draw of the title's decoration bake: textures picked by index are uploaded first (VICTORY_BAKE_TEXTURES_MS=0: not)"
            );
        }
        let began = std::time::Instant::now();
        PASS_BUDGET.set(Some((budget.0, budget.1, bake)));
        crate::texture::BAKING.set(bake && bake_exact());
        self.heap_textures(list, heap, host);
        crate::texture::BAKING.set(false);
        PASS_BUDGET.set(None);
        if bake {
            BAKED_MS.fetch_add(began.elapsed().as_millis() as u64, Ordering::Relaxed);
        }
    }

    /// The queue's side of the card accounting line
    /// (`crate::resource::card_accounting`, which says what the line
    /// holds): the slots that are due, by what their texture has on the
    /// card. Off unless `VICTORY_CARD_ACCOUNTING=1`; then at the first pass
    /// that finds the card without room, and every five seconds after it.
    /// Nothing is made, uploaded or counted as used here. For 18,000 due
    /// slots: 18,000 looks at a title slot and at the texture it views, a
    /// lock each, some milliseconds of the thread that records the frame.
    fn card_accounting(
        heap: &crate::descriptor::Heap,
        host: &crate::descriptor::host::HostHeap,
        frame: u64,
        room: bool,
    ) {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ON.get_or_init(|| std::env::var_os("VICTORY_CARD_ACCOUNTING").is_some_and(|v| v == "1")) {
            return;
        }
        // When the next line is due (0: none yet, the card has had room).
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let now = crate::host::now_ms().max(1);
        let next = NEXT.load(Ordering::Relaxed);
        // (With levels on demand the card may never be without room: the
        // lines then start with the first texture made that way.)
        let by_level = crate::tiles::STATS[4].load(Ordering::Relaxed) > 0;
        let due_now = if next == 0 { !room || by_level } else { now >= next };
        if !due_now || NEXT.compare_exchange(next, now + 5000, Ordering::Relaxed, Ordering::Relaxed).is_err() {
            return;
        }
        // Due slots; of them uploaded before; of them viewing nothing the
        // title fills; and the textures (each once) whose host copy is
        // alive, with their bytes on the card.
        let mut due = [0u64; 5];
        // Textures (each once) that never had a host copy, and ones whose
        // host copy was let go: where their memory is, and its bytes.
        let (mut never, mut gone) = (Vec::new(), Vec::new());
        let mut seen = std::collections::BTreeSet::new();
        for (index, uploaded) in host.texture_slots_all_due(frame) {
            due[0] += 1;
            due[1] += u64::from(uploaded);
            let state = match heap.handle(index).and_then(crate::descriptor::view) {
                Some(View::Placed(false, resource, desc)) if resource[0] != 1 => {
                    let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
                    match crate::resource::placed_known(address, &resource) {
                        Some(texture) => crate::resource::copy_state(texture),
                        // No title texture was made for it: the pass never
                        // got as far as making one.
                        None => Some((0, crate::resource::placed_bytes(&resource).unwrap_or(0), address)),
                    }
                }
                Some(View::Shader(object, _)) if crate::resource::title_fills(object as This) => {
                    crate::resource::copy_state(object as This)
                }
                _ => None,
            };
            let Some((state, bytes, memory)) = state else {
                due[2] += 1;
                continue;
            };
            if !seen.insert(memory) {
                continue;
            }
            match state {
                2 => {
                    due[3] += 1;
                    due[4] += bytes;
                }
                1 => gone.push((memory, bytes)),
                _ => never.push((memory, bytes)),
            }
        }
        crate::resource::card_accounting(room, due, never, gone);
    }

    /// The texture side of [`State::heap_buffers`]: textures the title fills
    /// itself (streamed pages, layout-library copies) whose views sit in the
    /// view heap are created (placed textures have no host texture until
    /// something reads them) and uploaded, as a table read would do, for
    /// shaders that pick them by a computed index. Within a budget of bytes
    /// per frame and in all; the rest waits for later lists and frames.
    /// The most recently written slots first (`TextureQueue::due`); slots
    /// draws read through tables are not in the queue.
    /// shortcut: uploaded once, and again only when the title rewrites the
    /// slot. A placed texture not all mapped yet is uploaded with what is
    /// there and looked at again (1, 2, 4, ... frames later, at most about
    /// a second): uploaded again when more of it is mapped. Pages mapped
    /// in place of others, and contents changed with no descriptor write,
    /// are missed (hook the page-mapping events for those).
    /// `VICTORY_HEAP_TEXTURES=0` turns it off.
    fn heap_textures(
        &self,
        list: &HostList,
        heap: &crate::descriptor::Heap,
        host: &crate::descriptor::host::HostHeap,
    ) {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ON.get_or_init(|| std::env::var_os("VICTORY_HEAP_TEXTURES").is_none_or(|v| v != "0")) {
            return;
        }
        let memory = crate::host::host().and_then(|h| h.video_memory());
        // `VICTORY_TEXTURE_STAGE_MB` sets it: a backlog of tens of thousands of
        // textures (a scene's first minutes) then drains in fewer frames.
        static PER_FRAME_MB: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
        let asked = *PER_FRAME_MB.get_or_init(|| {
            std::env::var("VICTORY_TEXTURE_STAGE_MB").ok().and_then(|v| v.trim().parse::<u64>().ok())
        });
        let per_frame = match asked {
            Some(mb) => mb << 20,
            None => staging_budget_per_frame(memory.map(|[budget, ..]| budget)),
        };
        static FRAME: AtomicU64 = AtomicU64::new(u64::MAX);
        static SPENT: AtomicU64 = AtomicU64::new(0);
        let frame = crate::frame::current();
        if FRAME.swap(frame, Ordering::Relaxed) != frame {
            SPENT.store(0, Ordering::Relaxed);
        }
        // No more of them are made while the card has no room
        // (`resource::room`): by what is alive now, not by what was ever
        // staged (4 GB in all, after which later textures stayed empty for
        // the run). The ones nothing names any more are let go, which
        // makes room again: so the slots are still looked at (the pass
        // returned here, and a slot whose texture had been streamed out
        // kept its host copy for as long as the card was full).
        let mut room = crate::resource::room(memory);
        // With room: the most texels a side of a new copy's first level, the
        // fewer the closer the card is to its line (`resource::detail`;
        // `u64::MAX`: every level).
        let mut side = if room { crate::resource::detail_side(memory) } else { u64::MAX };
        // Bytes uploaded by this pass since the card was last asked
        // whether it has room: asked again every 32 MiB (a pass may upload
        // 192 MB and more, and one asking a pass let the queue run hundreds
        // of MB past its line while a scene loaded).
        let mut since_asked = 0u64;
        if !room {
            HEAP_TEXTURES_FULL.fetch_add(1, Ordering::Relaxed);
            once!("textures picked by index: the card has no room for more; they wait");
        }
        let mut refused = 0;
        // Room again: the slots held back while the card was full are due
        // now (`full_card_pass`); in the band, once copies may have more
        // levels than when they were held (`resource::release_held`).
        if room && crate::resource::release_held(side) {
            host.texture_slots_room(frame);
        }
        // Slots whose texture's memory the runtime mapped, unmapped or
        // wrote since the last pass are due again.
        host.texture_slots_remapped(frame);
        let baking = crate::texture::BAKING.get();
        if baking {
            // And, for a bake, those whose memory DirectStorage wrote in
            // pages no pass has taken since (`BAKE_PAGE_WRITTEN`).
            host.texture_slots_bake_written(frame);
        }
        host.texture_slots_short(frame);
        // The slots of textures whose first level with memory (levels on
        // demand), or whose host copy for one with more levels, changed since
        // the last pass are made again from it; those whose new view's upload
        // has run are made from that.
        host.texture_views_remade();
        host.texture_slots_flip();
        // Slots that went on showing another texture than the title's for
        // too long are shown neutral and are due now
        // (`descriptor::host::keep_shown_frames`).
        host.texture_slots_kept_long(frame, |index| heap.handle(index));
        // And with room, the textures whose upload left a held level
        // without the card's memory are uploaded again.
        if room {
            host.texture_slots_waiting(frame);
        }
        let (mut due, census) = host.texture_slots_due(frame, if baking { 1 << 16 } else { 4096 });
        // (Those that are due later after all that are due.)
        let on_time = due.len();
        if baking {
            let known: std::collections::HashSet<u32> = due.iter().map(|&(index, _)| index).collect();
            static AHEAD: AtomicU64 = AtomicU64::new(u64::MAX);
            let again = AHEAD.swap(frame, Ordering::Relaxed) == frame;
            let (ahead, _) = host.texture_slots_due(frame + BAKE_AHEAD[usize::from(again)], 1 << 16);
            due.extend(ahead.into_iter().filter(|(index, _)| !known.contains(index)));
        }
        HEAP_TEXTURES_WAITING.store(census[0], Ordering::Relaxed);
        for (count, now) in HEAP_TEXTURES_QUEUE.iter().zip(census) {
            count.store(now, Ordering::Relaxed);
        }
        crate::resource::detail_line(side, room, census[5]);
        // Where the card's memory is, and what the due slots would need
        // (`VICTORY_CARD_ACCOUNTING=1`; a line when the card first has no
        // room, and every five seconds after it).
        Self::card_accounting(heap, host, frame, room);
        // The frames in about four seconds, between 16 and 256: how long a
        // slot with nothing to upload waits at most for its next look. In
        // frames alone it was 120: minutes at a fraction of a frame per
        // second, while the title streams by the clock.
        // (About a second before the runtime told of the memory it maps:
        // the looks are now only for what that does not tell.)
        static PASSED: AtomicU64 = AtomicU64::new(0);
        let now = crate::host::now_ms();
        let took = now.saturating_sub(PASSED.swap(now, Ordering::Relaxed)).max(1);
        let most = (4000 / took).clamp(16, 256);
        // What a table entry of a texture a shader samples says.
        let sampled = UsedResource {
            kind: ResourceType::Srv,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 8,
            user_data: 0,
            slot: 0,
        };
        // Also within a time per pass: the pass runs on a thread that
        // records the frame, and making and staging 192 MB of textures took
        // seconds of each of a scene's first frames.
        // The rest waits for the next frames, as it does past the bytes.
        static SPAN_MS: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
        let span_asked = *SPAN_MS
            .get_or_init(|| std::env::var("VICTORY_TEXTURE_STAGE_MS").ok().and_then(|v| v.trim().parse().ok()));
        let span = std::time::Duration::from_millis(span_asked.unwrap_or(250));
        // The draws since the pass before: about the frame's.
        static PASS_DRAWN: AtomicU64 = AtomicU64::new(0);
        let drawn = drawn();
        let draws = drawn.saturating_sub(PASS_DRAWN.swap(drawn, Ordering::Relaxed));
        // A backlog (a scene's first minutes: 22,000 slots due, 2-3 minutes of black
        // surfaces at 192 MB a frame) gets three times the bytes and the time until it
        // drains; `VICTORY_TEXTURE_CATCHUP=0` keeps the plain budget.
        static CATCHUP: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let catchup = *CATCHUP.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_CATCHUP").is_none_or(|v| v != "0"));
        let backlog = catchup && on_time >= 1024;
        let (per_frame, span) = if backlog { (per_frame * 3, span * 3) } else { (per_frame, span) };
        // In the world, within a share of the frame (`stage_share`).
        let span = if span_asked.is_none() { pass_span(span, stage_share(), took, draws, backlog) } else { span };
        // A run made early (`State::early_textures`) has a budget of its
        // own: its bytes on top of what the frame has spent so far.
        // (Not `most`: that is the longest wait between looks, used below.)
        let (span, bytes_at_most) = match PASS_BUDGET.get() {
            Some((ms, bytes, _)) => {
                (std::time::Duration::from_millis(ms), SPENT.load(Ordering::Relaxed).saturating_add(bytes))
            }
            None => (span, per_frame),
        };
        // After upstream GPL-3.0 fork, commit ab75ac3e02ba "Back off refused GPU
        // pool growth and defer uploads under low commit" (and 18bdf7932b79, "Scope bindless refreshes to their
        // heap and bound speculative staging"): `required`, the cap on the bytes the pass stages while memory is
        // short, and the checks of `admit_texture_pass`, `upload_refused` and `take_staging_deferred` below.
        // Here: the gates are `crate::host::commit`'s, with its own thresholds (all permissive with
        // `VICTORY_COMMIT_GUARD=0`); a bake draw is `required` by the third field of `PASS_BUDGET`, and the cap
        // leaves `VICTORY_TEXTURE_STAGE_MB` alone, as theirs does.
        let required = PASS_BUDGET.get().is_some_and(|budget| budget.2);
        // A bake samples its inputs at once and cannot retry that draw.
        let bytes_at_most = pass_bytes_cap(
            bytes_at_most,
            required || asked.is_some(),
            crate::host::commit::short(),
            crate::host::commit::critical(),
        );
        let began = std::time::Instant::now();
        BAKE_SERVED.store(0, Ordering::Relaxed);
        let mut served = true;
        for (index, write) in due {
            if SPENT.load(Ordering::Relaxed) >= bytes_at_most || began.elapsed() > span {
                served = false;
                break;
            }
            // While the card has no room: a placed texture's host copy made
            // for this slot holds only its smallest levels, and one that does
            // stays so (`crate::resource::tail_only`); the slot is held then,
            // for the rest of them when the card has room.
            let tail = !room && crate::resource::tail_only();
            let _tail = crate::resource::TailOnly::new(tail);
            // With room in the band below the line: a new host copy holds the
            // levels of at most `side` texels a side, and the slot is held as
            // one of the smallest levels is, for the rest of them.
            let fewer = room && side != u64::MAX;
            let _fewer = crate::resource::DetailCap::new(if fewer { side } else { u64::MAX });
            // A placed texture whose memory is not mapped (not streamed in
            // yet, or streamed out again: the heap keeps tens of thousands
            // of such views) gets no host texture; looked at again soon (a
            // title writes a texture's view before its pages are mapped,
            // and the pass runs early in a frame). One of which some is
            // mapped (a texture streaming in: its smaller mips lie at the
            // start of its memory and arrive first) is uploaded with what
            // is there, and again when more is. (It was uploaded again
            // every 120 frames, changed or not; and asked by its first
            // page alone, one whose first page is the one missing counted
            // as not there.)
            // A slot uploaded whole is looked at again a few times
            // (`RECHECK_FRAMES`): the title writes a texture's view, and
            // may fill the texture's memory itself only afterwards, with
            // no descriptor write and nothing from the runtime to tell of
            // it. The host copy then kept what the memory held at the
            // first look for the rest of the run. The look reads 48 KiB;
            // only a changed texture is uploaded again.
            // `VICTORY_HEAP_RECHECK=0`: uploaded once, as before.
            // One found changed by such a look is looked at in the next frame
            // again: the textures left that change unnoticed are ones the
            // title writes all the time through another view of their pages
            // (all their blocks are shown at other addresses too), and each
            // was shown four frames late, over and over.
            let mut again_in = RECHECK_FRAMES[0];
            if let Some((at, len, print, looks, uploaded)) = host.texture_slot_checked(index) {
                if crate::texture::fingerprint(at, len) == print {
                    HEAP_TEXTURES_RECHECKED[0].fetch_add(1, Ordering::Relaxed);
                    // (Its pages shown at other addresses too: looked at
                    // for good, every few frames, [`heap_recheck_aliased`].)
                    let aliased = heap_recheck_aliased()
                        && !crate::inplace::shown_elsewhere(at as usize & !0xffff, (at + len) as usize).is_empty();
                    match next_look(looks, aliased) {
                        Some((wait, looks)) => {
                            if aliased {
                                HEAP_TEXTURES_RECHECKED[2].fetch_add(1, Ordering::Relaxed);
                            }
                            host.texture_slot_recheck(index, write, frame + wait, (at, len), print, looks, uploaded)
                        }
                        None => host.texture_slot_done(index, write, None),
                    }
                    continue;
                }
                if recheck_soon() {
                    again_in = 1;
                }
                let n = HEAP_TEXTURES_RECHECKED[1].fetch_add(1, Ordering::Relaxed);
                if n < 200 || n.is_multiple_of(1000) {
                    // Whether its pages are the title's at other addresses
                    // too: what is written there is told of there alone.
                    let elsewhere = crate::inplace::shown_elsewhere(at as usize & !0xffff, (at + len) as usize);
                    let first = heap.handle(index).and_then(crate::descriptor::view).map_or(0, |view| match view {
                        View::Placed(false, _, desc) => crate::resource::first_mip(&desc),
                        _ => 0,
                    });
                    tracing::warn!(
                        first_level = first,
                        blocks_shown_elsewhere = elsewhere.len(),
                        slot = index,
                        address = format_args!("{at:#x}"),
                        bytes = len,
                        frames_after_upload = frame.saturating_sub(uploaded),
                        looks,
                        changed = n + 1,
                        unchanged = HEAP_TEXTURES_RECHECKED[0].load(Ordering::Relaxed),
                        unchanged_aliased = HEAP_TEXTURES_RECHECKED[2].load(Ordering::Relaxed),
                        "a texture picked by index changed after its upload with nothing to tell of it: uploaded again"
                    );
                }
            }
            let mut partly = None;
            let mut there = None;
            if let Some(View::Placed(false, resource, desc)) = heap.handle(index).and_then(crate::descriptor::view) {
                let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
                let first = crate::resource::first_mip(&desc);
                let len = crate::resource::placed_bytes_from(&resource, first).unwrap_or(1);
                let mapped = crate::texture::mapped_bytes(address as *const u8, len as usize);
                if mapped == 0 {
                    HEAP_TEXTURES_AWAY.fetch_add(1, Ordering::Relaxed);
                    // Streamed out: what the slot showed is let go.
                    host.texture_slot_gone(index);
                    host.texture_slot_later(index, write, frame, most);
                    continue;
                }
                host.texture_slot_here(index);
                there = Some(mapped);
                if mapped < len {
                    if mapped <= host.texture_slot_mapped(index) {
                        HEAP_TEXTURES_PARTLY[0].fetch_add(1, Ordering::Relaxed);
                        host.texture_slot_later(index, write, frame, most);
                        continue;
                    }
                    if !crate::texture::committed(address as *const u8, 1) {
                        HEAP_TEXTURES_PARTLY[2].fetch_add(1, Ordering::Relaxed);
                    }
                    partly = Some(mapped);
                }
            }
            // A texture object the host renders to or writes holds the
            // host's own contents: nothing to bring up to date, and this
            // pass is no use of it (its host copy is made when something
            // uses it, and its slots then: `descriptor::make_waiting`).
            if let Some(View::Shader(object, _)) = heap.handle(index).and_then(crate::descriptor::view)
                && crate::resource::buffer_bytes(object as This).is_none()
                && !crate::resource::title_fills(object as This)
            {
                host.texture_slot_done(index, write, None);
                continue;
            }
            // From here on a host texture is made: not without room. The
            // slot stays due.
            // Not so a slot already made from this very view: its host
            // texture exists, and uploading it again takes none of the
            // card's memory. It was refused all the same, so a texture the
            // title filled after its first upload kept its empty host copy
            // from the moment the card reached the line, which is usually a
            // scene's first frame: the same slots were found changed again
            // at every pass and never uploaded (tens of thousands of such
            // findings), and such a texture had its colour only when the
            // scene was entered under the line.
            // `VICTORY_REUPLOAD_WHEN_FULL=0`: refused, as before.
            if !room {
                let has_its_texture = reupload_when_full()
                    && heap.handle(index).is_some_and(|handle| host.texture_slot_made_from(index, handle));
                // A placed texture whose host copy would be new gets one of
                // its smallest levels (a few KB, with no room test:
                // `crate::resource::tail_only`), and the slot is held after
                // its upload as below.
                let tails = tail
                    && !has_its_texture
                    && heap.handle(index).and_then(crate::descriptor::view).is_some_and(|view| match view {
                        View::Placed(false, resource, desc) => {
                            crate::resource::placed_tail_base(&resource, crate::resource::first_mip(&desc)) > 0
                        }
                        _ => false,
                    });
                // One of which no copy of the smallest levels can be made
                // (of one level, of 64 texels a side or less, a texture
                // object) gets a whole one if it is small
                // (`resource::small_when_full`).
                let small = !has_its_texture
                    && !tails
                    && heap
                        .handle(index)
                        .and_then(crate::descriptor::view)
                        .is_some_and(|view| small_texture(&view, crate::resource::small_when_full()));
                if small {
                    crate::resource::DETAILED[4].fetch_add(1, Ordering::Relaxed);
                }
                if !has_its_texture && !tails && !small {
                    refused += 1;
                    if full_card_pass() {
                        // Held back for the frames of about four seconds
                        // (`most`), or until the card has room; the pass
                        // goes on, within a count that bounds one pass.
                        host.texture_slot_held(index, write, frame + most);
                        crate::resource::held_at(0);
                        if refused > 1024 {
                            break;
                        }
                    } else if refused > 256 {
                        break;
                    }
                    continue;
                }
                if has_its_texture {
                    let n = HEAP_TEXTURES_REUPLOADED_FULL.fetch_add(1, Ordering::Relaxed) + 1;
                    if n == 1 || n.is_multiple_of(500) {
                        tracing::info!(
                            so_far = n,
                            slot = index,
                            "a texture already on the card is brought up to date although the card has no room \
                             for new ones"
                        );
                    }
                }
            }
            // Making new host textures and staging buffers waits while the
            // PC's commit is short, once what a slot whose memory went was
            // let go (`texture_slot_gone`, above), and before a slot is
            // mirrored to a texture not uploaded yet. The slot stays due.
            if !crate::host::commit::admit_texture_pass(required) {
                served = false;
                break;
            }
            // A slot that shows the texture by an earlier view of it (one
            // from another level) goes on doing so until the upload of what
            // the new one shows has run.
            let shows_other = !baking
                && crate::resource::held_only()
                && heap.handle(index).is_some_and(|handle| host.texture_slot_shows_other(index, handle));
            // A slot that shows nothing of it yet, or something else, shows it
            // when the upload has run too.
            let late = !shows_other
                && !baking
                && crate::descriptor::host::show_after()
                && heap.handle(index).is_some_and(|handle| !host.texture_slot_made_from(index, handle));
            let view = heap.handle(index).and_then(|handle| {
                if shows_other || late { crate::descriptor::view(handle) } else { host.mirror(index, handle, true) }
            });
            let at_once = || {
                if late && let Some(handle) = heap.handle(index) {
                    let _ = host.mirror(index, handle, true);
                }
            };
            let placed = view.as_ref().and_then(crate::descriptor::host::placed_texture);
            if crate::host::commit::upload_refused() {
                served = false;
                break;
            }
            let resource = match placed.as_ref().or(view.as_ref()) {
                Some(&View::Shader(resource, _)) => resource as This,
                _ => {
                    at_once();
                    host.texture_slot_done(index, write, None);
                    continue;
                }
            };
            let texture = crate::resource::host_resource(resource);
            if crate::host::commit::upload_refused() {
                served = false;
                break;
            }
            // No host copy for want of memory, asked for again soon
            // (`crate::resource::retrying`): the slot is held for the next
            // frame, not done with (it was never uploaded so; upstream 4e95a78,
            // upstream).
            if texture.is_none() && crate::resource::retrying(resource) {
                host.texture_slot_held(index, write, frame + 1);
                continue;
            }
            // Its host copy holds only its smallest levels while the card
            // has no room: the slot is held for the rest of them (in about
            // four seconds, `most`, or when the card has room), and is not
            // looked at by fingerprint meanwhile.
            let tailed = (tail || fewer) && crate::resource::tail_copy(resource);
            if tailed {
                crate::resource::held_at(if tail { 0 } else { side });
            }
            let title_fills = texture.is_some_and(crate::texture::title_written);
            if !title_fills {
                at_once();
                // a render target, a depth or unordered access texture: the host's own
                host.texture_slot_done(index, write, None);
                continue;
            }
            // A texture that has a staging buffer now is one draws read
            // through a table: they upload it, and check it at every
            // submission that samples it.
            let tabled = crate::texture::is_staged(resource);
            // The host copy holds these very bytes: nothing to upload.
            let first = match (&placed, &view) {
                (Some(_), Some(View::Placed(_, _, desc))) => crate::resource::first_mip(desc),
                _ => 0,
            };
            let same = !tabled && crate::texture::unchanged(resource, first);
            if same {
                HEAP_TEXTURES_SAME.fetch_add(1, Ordering::Relaxed);
                if shows_other && let Some(handle) = heap.handle(index) {
                    let _ = host.mirror(index, handle, true);
                }
                // Its bytes are in, or on the way there in a list before.
                if late && texture.is_some_and(crate::pool::fresh) && !crate::texture::ahead::there(resource) {
                    if let Some(handle) = heap.handle(index) {
                        host.texture_slot_show_after(list, index, handle);
                    }
                } else {
                    at_once();
                }
                if tailed {
                    host.texture_slot_tail(index, write, frame + most);
                    continue;
                }
                let again = partly.map(|mapped| (frame + 1, mapped));
                let memory = if again.is_none() && heap_recheck() { crate::texture::title_span(resource, first) } else { None };
                match memory {
                    Some((at, len)) => {
                        let print = crate::texture::fingerprint(at, len);
                        host.texture_slot_recheck(index, write, frame + again_in, (at, len), print, 0, frame);
                    }
                    None => host.texture_slot_done(index, write, again),
                }
                continue;
            }
            // The first upload of a texture no list has used: on a queue of
            // its own, and the slot shows it as soon as that has run.
            let ahead = if late && !tabled && texture.is_some_and(crate::pool::fresh) {
                crate::texture::ahead::take(resource, first, there.filter(|_| partly.is_none()).unwrap_or(u64::MAX))
            } else {
                None
            };
            if let Some(ahead) = &ahead {
                use crate::texture::ahead::Taken;
                let (Taken::New(job) | Taken::Known(job)) = ahead;
                if let Some(handle) = heap.handle(index) {
                    host.texture_slot_show_ahead(job, index, handle);
                }
                if let Taken::New(job) = ahead {
                    crate::texture::ahead::send(job);
                }
            }
            if ahead.is_none() && crate::host::commit::upload_refused() {
                served = false;
                break;
            }
            let late = late && ahead.is_none();
            let _ = crate::host::commit::take_staging_deferred();
            if let Some(view) = &view
                && ahead.is_none()
            {
                // (What was found mapped of it a moment ago is not asked
                // again, unless the host copy may need more.)
                crate::texture::PICKING.set(Some(there.filter(|_| partly.is_none()).unwrap_or(u64::MAX)));
                readable(list, sampled.kind, view);
                crate::texture::PICKING.set(None);
            }
            // A staging buffer held back, or refused, leaves this slot due:
            // publishing it or counting it served would lose its only retry.
            if ahead.is_none()
                && (crate::host::commit::take_staging_deferred() || crate::host::commit::upload_refused())
            {
                served = false;
                break;
            }
            if shows_other && let Some(handle) = heap.handle(index) {
                host.texture_slot_flip(list, index, handle);
            }
            if late && let Some(handle) = heap.handle(index) {
                host.texture_slot_show_after(list, index, handle);
            }
            #[cfg(feature = "diagnostics")]
            if crate::book::on()
                && let Some(texture) = texture.filter(|t| crate::pool::fresh(t))
            {
                use windows::core::Interface;
                let raw = texture.as_raw() as usize;
                let asked = crate::host::submissions();
                list.defer_host(move || {
                    let (started, finished) = crate::host::submissions();
                    crate::book::note(raw, || {
                        format!(
                            "; upload for slot {index} recorded at submissions {asked:?}, sent as submission {started} (finished {finished}) at {:.3} s",
                            crate::host::now_ms() as f64 / 1e3
                        )
                    });
                });
            }
            // A texture view the host refuses removes the device: name it.
            static GONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if !GONE.load(Ordering::Relaxed)
                && let Some(why) = crate::descriptor::host::device_removed()
            {
                GONE.store(true, Ordering::Relaxed);
                let text: String = format!("{view:?}").chars().take(300).collect();
                tracing::error!(why, slot = index, "the host device went while textures picked by index were made ready; last: {text}");
                crate::host::log_removal_context();
                return;
            }
            let (bytes, whole) = crate::texture::staged(resource, there.is_none()).unwrap_or((0, true));
            // Not all of it there: looked at again next frame, and from
            // then on as a slot with nothing new (a placed texture: until
            // more of it is mapped; a texture object at an address the
            // title mapped: again in 120 frames, as before).
            let again = match partly {
                Some(mapped) => Some((frame + 1, mapped)),
                None => (!whole).then_some((frame + 120, 0)),
            };
            if again.is_some() {
                HEAP_TEXTURES_PARTLY[1].fetch_add(1, Ordering::Relaxed);
            }
            if tabled {
                // Nothing was staged here, so nothing is counted or spent,
                // and the staging buffer stays: taking it away made the
                // next draw that reads the texture through a table stage,
                // detile and upload all of it once more.
                HEAP_TEXTURES_TABLED.fetch_add(1, Ordering::Relaxed);
                if tailed {
                    host.texture_slot_tail(index, write, frame + most);
                } else {
                    host.texture_slot_done(index, write, again);
                }
                continue;
            }
            // Uploaded: the list holds the staging buffer until it has run;
            // a table read, or the next look at a texture not all mapped
            // yet, makes it anew. (Kept for those until now: every partly
            // streamed texture of the heap held a staging buffer of its full
            // size for good.)
            let memory = if again.is_none() && heap_recheck() {
                crate::texture::staged_memory(resource)
            } else {
                None
            };
            crate::texture::unstage(resource);
            SPENT.fetch_add(bytes, Ordering::Relaxed);
            since_asked += bytes;
            if room && since_asked >= 32 << 20 {
                since_asked = 0;
                let memory = crate::host::host().and_then(|h| h.video_memory());
                room = crate::resource::room(memory);
                side = if room { crate::resource::detail_side(memory) } else { u64::MAX };
            }
            HEAP_TEXTURE_BYTES.fetch_add(bytes, Ordering::Relaxed);
            HEAP_TEXTURES.fetch_add(1, Ordering::Relaxed);
            if tailed {
                host.texture_slot_tail(index, write, frame + most);
                continue;
            }
            match memory {
                // Whole and uploaded: looked at again (see above).
                Some((at, len)) => {
                    let print = crate::texture::fingerprint(at, len);
                    host.texture_slot_recheck(index, write, frame + again_in, (at, len), print, 0, frame);
                }
                None => host.texture_slot_done(index, write, again),
            }
        }
        if baking && served {
            BAKE_SERVED.store(host.texture_news().0 + 1, Ordering::Relaxed);
        }
    }

    /// For the frame listing, after a dropped dispatch's line: its group
    /// counts and the compute root arguments it was recorded with (root
    /// constants as hex dwords, root views with the title buffer they
    /// point into). A program that does not translate has no bindings to
    /// list; these say which memory it would have read and written (the
    /// ray-tracing structure builder passes its buffers this way).
    #[cfg(feature = "diagnostics")]
    fn list_dropped_roots(&self, groups: [u32; 3]) {
        crate::capture::op(0, || {
            use std::fmt::Write;
            let [x, y, z] = groups;
            let mut text = format!("  its roots ({x}x{y}x{z}):");
            for (slot, root) in self.compute.iter().enumerate() {
                match root {
                    Root::None => {}
                    Root::Table(a) => {
                        let _ = write!(text, " {slot}=table {a:#x}");
                    }
                    Root::Constants(v) => {
                        let _ = write!(text, " {slot}=constants {:x?}", &v[..v.len().min(48)]);
                    }
                    Root::View(a) => {
                        let _ = write!(text, " {slot}=view {a:#x}");
                        match crate::shadow::extent(*a) {
                            Some((start, len)) => {
                                let _ = write!(text, " in buffer {start:#x}+{len:#x}");
                            }
                            None => text.push_str(" in no buffer"),
                        }
                    }
                }
            }
            text
        });
    }

    /// Runs a dispatch on the host.
    pub(crate) fn dispatch(&mut self, list: &HostList, x: u32, y: u32, z: u32, flags: u32) {
        // It may write the arguments a window holds clamped.
        self.window = None;
        #[cfg(feature = "diagnostics")]
        if crate::pipeline::is_skipped(self.pipeline as This) {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            crate::runreport::left_out(self.pipeline);
            crate::pipeline::note_drop(self.pipeline as This);
            return;
        }
        // The ray-tracing structure builder's stages run all or none.
        if crate::pipeline::is_builder(self.pipeline as This) && !crate::pipeline::builder_runs() {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            crate::runreport::hint(victory_pipeline::report::LeftOut::BuilderOff);
            crate::runreport::left_out(self.pipeline);
            #[cfg(feature = "diagnostics")]
            {
                crate::pipeline::note_why("a stage of the ray-tracing structure builder, which is off".to_owned());
                self.list_dropped_roots([x, y, z]);
            }
            return;
        }
        let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) else {
            once!(
                pipeline = format_args!("{:#x}", self.pipeline),
                "a dispatch without a host pipeline dropped"
            );
            DROPPED.fetch_add(1, Ordering::Relaxed);
            crate::runreport::left_out(self.pipeline);
            #[cfg(feature = "diagnostics")]
            {
                crate::pipeline::note_drop(self.pipeline as This);
                self.list_dropped_roots([x, y, z]);
            }
            return;
        };
        if !p.compute {
            once!("a dispatch with a graphics pipeline");
            return;
        }
        if !self.bind(list, p, true) {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            crate::runreport::left_out(self.pipeline);
            // A stage of the structure builder that cannot be bound is a
            // dropped stage: the others stop too.
            if crate::pipeline::is_builder(self.pipeline as This) {
                crate::pipeline::builder_rests("one of its dispatches could not be bound");
            }
            return;
        }
        #[cfg(feature = "diagnostics")]
        list.note_op(|| self.op_text(format!("dispatch {x}x{y}x{z}"), true));
        #[cfg(feature = "diagnostics")]
        if crate::pipeline::is_builder(self.pipeline as This) {
            crate::capture::op(list as *const _ as usize, || format!("builder {}", p.name));
            self.list_dropped_roots([x, y, z]);
        }
        // Counted in threads: whole groups of them, and the program is told
        // how many of the last one run, where it asks (its `bind` set that to
        // all of them).
        let size = (flags & THREAD_DIMENSIONS != 0 && thread_dimensions())
            .then(|| p.stages.first().and_then(|s| s.translated().threads))
            .flatten();
        let threads = [x, y, z];
        let [x, y, z] = size.map_or(threads, |size| dispatch_groups(threads, size));
        if size.is_some() {
            if !crate::pipeline::is_builder(self.pipeline as This) {
                once!(pipeline = p.name, "a dispatch counted in threads that is not the structure builder's");
            }
            if let Some((param, word)) = compute_word(p, victory_shader::emit::USER_DISPATCH_THREADS_X) {
                // SAFETY: an open list; the root signature has the word.
                unsafe { list.list.SetComputeRoot32BitConstant(param, threads[0], word) };
            }
        }
        // SAFETY: an open list with everything set.
        unsafe { list.list.Dispatch(x, y, z) };
        self.buffers_written();
        DISPATCHED.fetch_add(1, Ordering::Relaxed);
        if let Some(role) = p.stages.first().and_then(|s| s.shader.ambient)
            && crate::ambient::on()
            && self.ambient(list, p, role)
        {
            self.host_pipeline = 0;
            self.host_roots[1] = 0;
        }
    }

    /// The stand-in for the indirect light after a dispatch of `p`
    /// (`crate::ambient`). Whether it recorded anything.
    fn ambient(&self, list: &HostList, p: &HostPipeline, role: crate::ambient::Role) -> bool {
        use crate::ambient::{Role, Views, binding, depth_binding};
        let Some(at) = binding(role) else {
            return false;
        };
        let Some(t) = p.stages.first().map(|s| s.translated()) else {
            return false;
        };
        let Some(heap) = (self.heaps[0] != 0).then(|| crate::descriptor::heap(self.heaps[0] as This).host()).flatten()
        else {
            return false;
        };
        let entry = |kind: ResourceType, (slot, offset): (u8, u32)| {
            t.used.iter().find(|u| {
                u.kind == kind && u.root == RootType::Table && (u.slot, u.table_offset_dwords) == (slot, offset)
            })?;
            let Some(Root::Table(table)) = self.roots_ref(true).get(usize::from(slot)) else {
                return None;
            };
            let resource = match crate::descriptor::view(table + u64::from(offset) * 4)? {
                View::Shader(r, _) | View::Unordered(r, ..) => r,
                _ => return None,
            };
            let start = match self.ambient_tables.iter().find(|m| m.0 == slot) {
                Some(m) => m.1,
                None => self.user_value(true, slot, RootType::Table, 0),
            };
            let host = crate::resource::host_resource(resource as This)?;
            // SAFETY: a live resource.
            let desc = unsafe { host.GetDesc() };
            Some((desc, heap.gpu_handle((start + offset * 4) / 32)))
        };
        let image = entry(ResourceType::Uav, at);
        let depth = match role {
            Role::Resolve => depth_binding().and_then(|d| entry(ResourceType::Srv, d)),
            Role::Radiance | Role::Air => image,
        };
        let (Some((of, image)), Some((seen, depth))) = (image, depth) else {
            once!(?role, "no stand-in for the indirect light: other tables");
            return false;
        };
        // (R32_UINT holding R9G9B9E5; R11G11B10_FLOAT.)
        let format = if role == Role::Resolve { 42 } else { 26 };
        let volume = of.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D;
        if of.Format.0 != format || (seen.Width, seen.Height) != (of.Width, of.Height) || volume != (role == Role::Air) {
            once!(?role, format = of.Format.0, "no stand-in for the indirect light: another image");
            return false;
        }
        crate::ambient::after(list, role, &Views {
            image,
            depth,
            size: [of.Width as u32, of.Height],
            slices: u32::from(of.DepthOrArraySize),
        })
    }

    /// What a draw or dispatch runs, for the device-removed report
    /// (diagnostics): the pipeline, then per stage the user data it gets
    /// (with the view at that heap index, if one is), the host addresses
    /// of its root views and the views its tables name.
    #[cfg(feature = "diagnostics")]
    fn op_text(&self, kind: String, compute: bool) -> String {
        use std::fmt::Write;
        let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) else {
            return kind;
        };
        let mut text = format!("{kind} {}", p.name);
        // What it draws into (a dispatch writes through its views below).
        if !compute {
            let described = |t: usize| {
                crate::resource::host_made(t as This).map(|r| {
                    // SAFETY: a live resource.
                    let d = unsafe { r.GetDesc() };
                    format!("{t:#x} {}x{} f{}", d.Width, d.Height, d.Format.0)
                })
            };
            let targets: Vec<String> = self.targets.iter().filter_map(|&t| described(t)).collect();
            let _ = write!(text, " targets {targets:?} depth {:?} state [{}]", described(self.depth), p.state);
            // The title's scissor and viewport at this draw (the first of
            // each): the title draws what it sees through a
            // portal (a window, from inside a room) under the portal's
            // rectangle, and the listing did not say which draws those are.
            if let Some(r) = self.scissors.first() {
                let _ = write!(text, " scissor [{} {} {} {}]", r.left, r.top, r.right, r.bottom);
            }
            if let Some(v) = self.viewports.first() {
                let _ = write!(
                    text,
                    " viewport [{} {} {} {} depth {}..{}]",
                    v.TopLeftX, v.TopLeftY, v.Width, v.Height, v.MinDepth, v.MaxDepth
                );
            }
        }
        let views = (self.heaps[0] != 0)
            .then(|| crate::descriptor::heap(self.heaps[0] as This).host())
            .flatten();
        for stage in &p.stages {
            let t = stage.translated();
            let _ = write!(text, " | {}:", stage.shader.hash);
            for u in &t.host.user_data {
                let v = match u.root {
                    RootType::Other(kind) => self.host_word(compute, kind, u.slot, u.dword, None),
                    _ => self.user_value(compute, u.slot, u.root, u.dword),
                };
                let _ = write!(text, " u{}={v:#x}", u.index);
                let index = if u.root == RootType::Table { v / 32 } else { v };
                if let Some(slot) = views.and_then(|h| h.describe_slot(index)) {
                    let _ = write!(text, " [heap {index}: {slot}]");
                }
            }
            for &(slot, _) in stage.root_cbvs.iter().chain(&stage.root_srvs).chain(&stage.root_uavs) {
                if let Some(Root::View(address)) = self.roots_ref(compute).get(usize::from(slot)) {
                    let host = crate::shadow::find(*address, 1)
                        .map(|s| (s.gpu(*address), s.left(*address)));
                    let _ = write!(text, " root{slot}={address:#x} host {host:#x?}");
                }
            }
        }
        let _ = write!(text, " tables {:?}", self.reads(compute));
        if !compute && !list_slots().is_empty() {
            text.push_str(&self.slots_text());
        }
        text
    }

    /// For the frame listing, after a draw whose shader `VICTORY_LIST_SLOTS`
    /// names: the heap slots it will pick textures from by index, and what
    /// the title and the host hold there (one `~slot` line each), with the
    /// words they were read from (`~raw` lines, to decode offline what is
    /// not decoded here).
    ///
    /// Where the slots are, for a title's material pixel shaders (from
    /// their native code): in the small
    /// buffers and constants of the draw's own table (every word of those
    /// that names a texture view gets a line); in the material record, at
    /// the byte offset the draw's constants (table 1, dword 56) hold at
    /// 0x1c, of the buffer at table 0, dword 160; and in the record at the
    /// byte offset the second root constant holds of the buffer at table 0,
    /// dword 688. A word that is a small number by chance and names a
    /// texture gets a line too.
    #[cfg(feature = "diagnostics")]
    fn slots_text(&self) -> String {
        use std::fmt::Write;
        const MATERIALS: u32 = 160;
        const GLOBAL: u32 = 688;
        const DRAW_CONSTANTS: u32 = 56;
        let wanted = list_slots();
        let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) else {
            return String::new();
        };
        let Some(stage) = p.stages.iter().find(|s| wanted.iter().any(|w| s.shader.hash.starts_with(w.as_str())))
        else {
            return String::new();
        };
        let Some(heap) = self
            .heaps
            .iter()
            .filter(|&&h| h != 0)
            .map(|&h| crate::descriptor::heap(h as This))
            .find(|h| !h.samplers())
        else {
            return String::new();
        };
        let t = stage.translated();
        let mut text = String::new();
        let entry = |slot: u8, dwords: u32| -> Option<View> {
            let Some(Root::Table(table)) = self.roots_ref(false).get(usize::from(slot)) else {
                return None;
            };
            crate::descriptor::view(table + u64::from(dwords) * 4)
        };
        let mut material = None;
        for u in t.used.iter().filter(|u| u.root == RootType::Table && u.kind != ResourceType::Sampler) {
            let label = format!("s{}+{}", u.slot, u.table_offset_dwords);
            let Some(view) = entry(u.slot, u.table_offset_dwords) else {
                let _ = write!(text, "\n    ~table {label} no view");
                continue;
            };
            let range = match &view {
                View::Constant(address, size) => Some((*address, u64::from(*size))),
                other => crate::descriptor::host::buffer_range(other),
            };
            match range {
                // A small buffer or constants: its words.
                Some((address, len)) if len <= 2048 => {
                    Self::slot_words(&mut text, heap, &label, address, len as usize);
                    if u.slot == 1 && u.table_offset_dwords == DRAW_CONSTANTS && len >= 0x20 {
                        let mut word = [0u8; 4];
                        if crate::host::read_into((address + 0x1c) as *const u8, &mut word) {
                            material = Some(u32::from_le_bytes(word));
                        }
                    }
                }
                Some(_) => {}
                // A texture read through the table: the title slot it sits in.
                None => {
                    let Some(Root::Table(table)) = self.roots_ref(false).get(usize::from(u.slot)) else {
                        continue;
                    };
                    match heap.index(table + u64::from(u.table_offset_dwords) * 4) {
                        Some(index) => text.push_str(&Self::slot_line(heap, &format!("table:{label}"), 0, index)),
                        None => {
                            let _ = write!(text, "\n    ~table {label} title=[{}]", view_state(Some(&view)));
                        }
                    }
                }
            }
        }
        let uses = |slot: u8, dwords: u32| {
            t.used.iter().any(|u| u.root == RootType::Table && u.slot == slot && u.table_offset_dwords == dwords)
        };
        if let Some(at) = material
            && uses(0, MATERIALS)
            && let Some((base, len)) = entry(0, MATERIALS).as_ref().and_then(crate::descriptor::host::buffer_range)
        {
            let at = u64::from(at);
            let _ = write!(text, "\n    ~material record at {at:#x} of the buffer at {base:#x} bytes {len}");
            if at + 64 <= len {
                Self::slot_words(&mut text, heap, "M", base + at, 64);
            }
            if at + 1936 + 512 <= len {
                Self::slot_words(&mut text, heap, "M+1936", base + at + 1936, 512);
            }
        }
        if uses(0, GLOBAL)
            && let Some(slot) = t.used.iter().find(|u| u.root == RootType::Constants).map(|u| u.slot)
            && let Some(Root::Constants(words)) = self.roots_ref(false).get(usize::from(slot))
            && let Some(&at) = words.get(1)
            && let Some((base, len)) = entry(0, GLOBAL).as_ref().and_then(crate::descriptor::host::buffer_range)
        {
            let at = u64::from(at);
            let _ = write!(text, "\n    ~root constants {words:08x?}: record at {at:#x} of the buffer at {base:#x} bytes {len}");
            if at + 64 <= len {
                Self::slot_words(&mut text, heap, "G", base + at, 64);
            }
        }
        text
    }

    /// Up to 512 bytes of title memory at `address` as hex words (a `~raw`
    /// line), and a `~slot` line for every word of them that names a
    /// texture view in `heap`.
    #[cfg(feature = "diagnostics")]
    fn slot_words(text: &mut String, heap: &crate::descriptor::Heap, label: &str, address: u64, len: usize) {
        use std::fmt::Write;
        let mut bytes = vec![0u8; len.min(512) & !3];
        if bytes.is_empty() || !crate::host::read_into(address as *const u8, &mut bytes) {
            let _ = write!(text, "\n    ~raw {label} at {address:#x} bytes {len}: unreadable");
            return;
        }
        let words: Vec<u32> =
            bytes.chunks_exact(4).map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]])).collect();
        let _ = write!(text, "\n    ~raw {label} at {address:#x} bytes {len}:");
        for w in &words {
            let _ = write!(text, " {w:08x}");
        }
        for (i, &index) in words.iter().enumerate() {
            let names = index != 0
                && heap
                    .handle(index)
                    .and_then(crate::descriptor::view)
                    .as_ref()
                    .is_some_and(crate::descriptor::host::is_texture_view);
            if names {
                text.push_str(&Self::slot_line(heap, label, i * 4, index));
            }
        }
    }

    /// One `~slot` line: where the slot number was read (`from`, byte
    /// `at`), what the title's heap holds in slot `index` with the state
    /// of that texture ([`view_state`]), what the host slot was made from,
    /// and what the queue of textures picked by index knows of it.
    #[cfg(feature = "diagnostics")]
    fn slot_line(heap: &crate::descriptor::Heap, from: &str, at: usize, index: u32) -> String {
        let title = heap.handle(index).and_then(crate::descriptor::view);
        let host: String = heap
            .host()
            .and_then(|h| h.describe_slot(index))
            .unwrap_or_else(|| "none".into())
            .chars()
            .take(90)
            .collect();
        let queue = match heap.host() {
            Some(h) => h.texture_slot_state(index, crate::frame::current()),
            None => "no host heap".into(),
        };
        format!(
            "\n    ~slot from={from}+{at:#x} slot={index} title=[{}] host=[{host}] queue=[{queue}]",
            view_state(title.as_ref())
        )
    }

    /// A mesh draw under `VICTORY_MESH_TEST=scissor` rasterises one pixel:
    /// the scissors before it (`on`), the title's after it.
    fn mesh_scissor(&self, list: &HostList, on: bool) {
        if mesh_test() != 2 {
            return;
        }
        if on {
            let one = RECT { left: 0, top: 0, right: 1, bottom: 1 };
            crate::draw::scissors(list, &vec![one; self.scissors.len().max(1)]);
        } else if !self.scissors.is_empty() {
            crate::draw::scissors(list, &self.scissors);
        }
    }

    /// Runs a mesh dispatch of `x` groups on the host (the title's mesh
    /// programs are one-dimensional).
    pub(crate) fn dispatch_mesh(&mut self, list: &HostList, x: u32) {
        self.window = None;
        #[cfg(feature = "diagnostics")]
        self.trace("mesh", x, 0);
        let ran = mesh_test() != 1
            && mesh_kept(self.pipeline)
            && self.prepare_for(list, false, true)
            && match list.list.cast::<ID3D12GraphicsCommandList6>() {
                Ok(l6) => {
                    #[cfg(feature = "diagnostics")]
                    list.note_op(|| self.op_text(format!("mesh {x}"), false));
                    self.mesh_scissor(list, true);
                    #[cfg(feature = "diagnostics")]
                    if let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) {
                        list.ran_indirect(p.pso.as_raw() as usize, &p.name, crate::host::indirects::DIRECT, x, &self.user);
                    }
                    // (The host takes 65,535 groups in a dimension: more is
                    // undefined there.)
                    if x > 65_535 {
                        once!(x, "a mesh dispatch of more groups than the host takes: cut");
                    }
                    // SAFETY: an open list with everything set.
                    unsafe { l6.DispatchMesh(x.min(65_535), 1, 1) };
                    self.drew(crate::pipeline::host_pipeline(self.pipeline as This));
                    self.mesh_scissor(list, false);
                    DRAWN.fetch_add(1, Ordering::Relaxed);
                    true
                }
                Err(_) => false,
            };
        if !ran {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            crate::runreport::left_out(self.pipeline);
            #[cfg(feature = "diagnostics")]
            dropped(self.pipeline);
        }
    }

    /// The walls trace (`crate::host::walls_trace`): one log line per frame
    /// summarizing the main pass draws (four or more colour targets) by scissor
    /// rectangle, plus any culling shaders labeled in `VICTORY_SHADER_PROFILE`.
    fn walls_census(&self, p: &HostPipeline, compute: bool) {
        let cull = &crate::shader_profile::current().cull_shaders;
        type Census = (u64, std::collections::BTreeMap<[i32; 4], u32>, Vec<u32>);
        static CENSUS: std::sync::Mutex<Census> =
            std::sync::Mutex::new((u64::MAX, std::collections::BTreeMap::new(), Vec::new()));
        let frame = crate::frame::current();
        let mut census = CENSUS.lock().unwrap_or_else(|e| e.into_inner());
        if census.2.len() != cull.len() {
            census.2.resize(cull.len(), 0);
        }
        if census.0 != frame {
            let draws: u32 = census.1.values().sum();
            if draws > 0 {
                let mut rects: Vec<([i32; 4], u32)> = census.1.iter().map(|(r, n)| (*r, *n)).collect();
                rects.sort_unstable_by_key(|r| std::cmp::Reverse(r.1));
                let scissors: Vec<String> = rects
                    .iter()
                    .take(5)
                    .map(|(r, n)| format!("{} {} {} {} x{n}", r[0], r[1], r[2], r[3]))
                    .collect();
                let culling: Vec<String> = cull
                    .iter()
                    .zip(&census.2)
                    .filter(|&(_, &n)| n > 0)
                    .map(|(c, &n)| format!("{}={n}", c.label))
                    .collect();
                tracing::info!(
                    frame = census.0,
                    main_pass_draws = draws,
                    rectangles = rects.len(),
                    scissors = ?scissors,
                    culling = ?culling,
                    "walls trace: the frame's main pass by scissor rectangle, and its culling dispatches"
                );
            }
            census.0 = frame;
            census.1.clear();
            census.2.fill(0);
        }
        if compute {
            for (i, c) in cull.iter().enumerate() {
                if p.stages.iter().any(|s| s.shader.hash.as_str() == c.hash) {
                    census.2[i] += 1;
                }
            }
        } else if self.targets.iter().filter(|&&t| t != 0).count() >= 4 {
            let r = self.scissors.first().map_or([0; 4], |r| [r.left, r.top, r.right, r.bottom]);
            *census.1.entry(r).or_insert(0) += 1;
        }
    }

    fn roots_ref(&self, compute: bool) -> &Vec<Root> {
        if compute {
            &self.compute
        } else {
            &self.graphics
        }
    }

    fn table_entry(&self, list: &HostList, u: &UsedResource, compute: bool) {
        let Some(Root::Table(table)) = self.roots_ref(compute).get(usize::from(u.slot)) else {
            return;
        };
        let handle = table + u64::from(u.table_offset_dwords) * 4;
        let Some(heap) = self.heap_of(handle) else {
            return;
        };
        let (Some(host), Some(index)) = (heap.host(), heap.index(handle)) else {
            return;
        };
        if let Some(view) = host.mirror(index, handle, true) {
            readable(list, u.kind, &view);
            // Up to date by this draw: not the texture pass's to make.
            if crate::descriptor::host::is_texture_view(&view) {
                host.texture_slot_tabled(index, true);
            } else {
                host.slot_tabled(index);
            }
        }
    }

    /// The descriptors a shader reads through tables. Tables holding a
    /// constant buffer are made afresh for the draw in dynamic slots, with
    /// the constant data copied to upload memory when the list is
    /// submitted: the host wants constant buffers 256-byte aligned, the
    /// console only 16. Returns those tables' new byte offsets by root
    /// parameter; the other tables' slots are made from the title's.
    ///
    /// Within a list, the same table holding the same descriptors is made
    /// once: its constant data is copied at submission either way, and the
    /// big tables the world binds would otherwise take hundreds of slots per
    /// draw, wrapping the dynamic slots onto lists not yet submitted.
    pub(crate) fn tables(
        &mut self,
        list: &HostList,
        used: &[UsedResource],
        indexed: &[RootIndexedView],
        compute: bool,
    ) -> Few<(u8, u32), 16> {
        let tables = |u: &&UsedResource| u.root == RootType::Table;
        let mut fresh: Few<u8, 16> = Few::default();
        for u in used.iter().filter(tables).filter(|u| u.kind == ResourceType::Cbv) {
            if !fresh.contains(&u.slot) {
                fresh.push(u.slot);
            }
        }
        // Tables reaching past a host heap smaller than the title's (the
        // host's limit, 1,000,000 views on NVIDIA) are made afresh too: the
        // title's slots there have no host slot of their own.
        for u in used.iter().filter(tables) {
            if fresh.contains(&u.slot) {
                continue;
            }
            let Some(&Root::Table(table)) = self.roots_ref(compute).get(usize::from(u.slot)) else {
                continue;
            };
            let end = table + u64::from(u.table_offset_dwords) * 4;
            let Some(heap) = self.heap_of(table) else {
                continue;
            };
            let (Some(host), Some(index)) = (heap.host(), heap.index(end)) else {
                continue;
            };
            if index >= host.count() {
                fresh.push(u.slot);
            }
        }
        fresh.sort_unstable();
        for u in used.iter().filter(tables) {
            if !fresh.contains(&u.slot) {
                self.table_entry(list, u, compute);
            }
        }
        let mut made = Few::default();
        for &slot in fresh.iter() {
            let Some(&Root::Table(table)) = self.roots_ref(compute).get(usize::from(slot)) else {
                continue;
            };
            // (Their places among `used`.)
            let mut places: Few<u16, 32> = Few::default();
            for (i, _) in used.iter().enumerate().filter(|(_, u)| tables(u) && u.slot == slot) {
                places.push(i as u16);
            }
            let entries = || places.iter().map(|&i| &used[usize::from(i)]);
            let Some(heap) = self.heap_of(table) else {
                continue;
            };
            if heap.samplers() || (table - heap.start) % 32 != 0 {
                continue;
            }
            let Some(host) = heap.host() else {
                continue;
            };
            let mut selected: Vec<u32> = indexed
                .iter()
                .filter(|index| index.table_slot == slot)
                .map(|index| {
                    let constant = self.user_value(
                        compute,
                        index.constant_slot,
                        RootType::Constants,
                        index.constant_dword,
                    );
                    // The shader adds i32 byte offsets and shifts by 5.
                    index.byte_offset.wrapping_add(constant << index.shift) / 32
                })
                .collect();
            selected.sort_unstable();
            selected.dedup();
            let n = entries()
                .map(|u| u.table_offset_dwords / 8)
                .chain(selected.last().copied())
                .max()
                .unwrap_or(0)
                + 1;
            let last = table.checked_add(u64::from(n - 1) * 32);
            if n > crate::descriptor::host::DYNAMIC
                || last.and_then(|handle| heap.index(handle)).is_none()
            {
                once!(slot, n, "fresh descriptor table exceeds the available span");
                continue;
            }
            // The table and what its entries hold (record words).
            let key = {
                let mut h = RecordHash::default();
                for k in 0..n {
                    let at = table + u64::from(k) * 32;
                    // SAFETY: a descriptor slot in title memory.
                    h.add(unsafe { (at as *const [u32; 3]).read_unaligned() });
                }
                (table, n, h.0)
            };
            // Who reads of it, and as what, as far as [`readable`] goes by it.
            let reader = (used.as_ptr() as usize, used.len(), STORES.get(), TESTED_DEPTH.get().0);
            if let Some(known) = self.fresh_tables.get_mut(&key) {
                let first = known.first;
                // This reader had it brought up to date, and nothing in the
                // list changed since: nothing to do again.
                let once = ready_once() && selected.is_empty() && entries().all(|u| u.kind != ResourceType::Uav);
                if once
                    && known.epoch == list.epoch()
                    && let Some((_, depth)) = known.ready.iter().find(|(r, _)| *r == reader)
                {
                    if *depth {
                        TESTED_DEPTH.set((reader.3, true));
                    }
                    made.push((slot, first * 32));
                    continue;
                }
                let depth_before = TESTED_DEPTH.get();
                TESTED_DEPTH.set((depth_before.0, false));
                let _ = crate::host::commit::take_staging_deferred();
                for u in entries().filter(|u| u.kind != ResourceType::Cbv) {
                    let handle = table + u64::from(u.table_offset_dwords) * 4;
                    if let Some(view) = crate::descriptor::view(handle) {
                        readable(list, u.kind, &view);
                    }
                }
                for k in selected {
                    if entries().any(|u| u.table_offset_dwords / 8 == k) {
                        continue;
                    }
                    let handle = table + u64::from(k) * 32;
                    if let Some(view) = crate::descriptor::view(handle)
                        && let Some(kind) = view_kind(&view)
                    {
                        readable(list, kind, &view);
                    }
                }
                let depth = TESTED_DEPTH.get().1;
                TESTED_DEPTH.set((depth_before.0, depth_before.1 || depth));
                // (Not up to date for a reader if a staging buffer was held back: the next draw asks again.)
                let deferred = crate::host::commit::take_staging_deferred();
                if once && !deferred && let Some(known) = self.fresh_tables.get_mut(&key) {
                    // (The epoch as it is now: what this changed itself it
                    // need not do again.)
                    let epoch = list.epoch();
                    if known.epoch != epoch {
                        known.epoch = epoch;
                        known.ready.clear();
                    }
                    if known.ready.len() < 16 {
                        known.ready.push((reader, depth));
                    }
                }
                made.push((slot, first * 32));
                continue;
            }
            let Some(first) = host.dynamic(n) else {
                // No room: the draw must not run with the title's own
                // offsets (slots never made for it); `bind` drops it.
                made.push((slot, u32::MAX));
                continue;
            };
            self.fresh_tables.insert(key, FreshTable { first, epoch: 0, ready: Vec::new() });
            self.dynamic
                .push((std::ptr::from_ref(heap) as usize, host.id(), first, n));
            // The title's own slots of this table: what the draw reads of
            // them it brings up to date here, in the dynamic slots.
            let title_first = heap.index(table).filter(|first| first + n <= host.count());
            // They are such a table's, whatever they hold. (Of a big one,
            // which may lie over slots shaders pick by index, only the ones
            // the shader names.)
            if let Some(title) = title_first {
                for k in (0..n).filter(|&k| n <= 64 || entries().any(|u| u.table_offset_dwords / 8 == k)) {
                    host.slot_tabled(title + k);
                }
            }
            let list_views = crate::descriptor::host::list_views();
            if self.recording == 0 {
                static NEXT: AtomicU64 = AtomicU64::new(1);
                self.recording = NEXT.fetch_add(1, Ordering::Relaxed);
            }
            let recording = self.recording;
            // A constant buffer of the table found no scratch memory
            // ([`null_constants`]).
            let mut refused = false;
            // Fills of constants into the table's slots were noted for the
            // list's submission (`HostList::defer_constants`).
            let mut filled = false;
            // The whole range: a shader may index past the entries its
            // used-resource list names (descriptor arrays).
            for k in 0..n {
                let index = first + k;
                // A view this list has made before: its descriptor copied,
                // and nothing more if nothing changed in the list since.
                if list_views
                    && let Some(words) = crate::descriptor::record_at(table + u64::from(k) * 32)
                    && let Some(mut listed) = host.make_listed(recording, words, index)
                {
                    let kind = entries()
                        .find(|u| u.table_offset_dwords / 8 == k)
                        .map(|u| u.kind)
                        .or_else(|| selected.binary_search(&k).ok().and(listed.kind));
                    let was = (listed.ready, listed.tabled);
                    if let Some(kind) = kind {
                        let tested = TESTED_DEPTH.get();
                        if kind == ResourceType::Srv && listed.ready.0 == list.epoch() && listed.ready.1 == tested.0 {
                            if listed.ready.2 {
                                TESTED_DEPTH.set((tested.0, true));
                            }
                        } else if let Some(view) = crate::descriptor::host::listed_view(recording, words) {
                            let _ = crate::host::commit::take_staging_deferred();
                            TESTED_DEPTH.set((tested.0, false));
                            readable(list, kind, &view);
                            let depth = TESTED_DEPTH.get().1;
                            TESTED_DEPTH.set((tested.0, tested.1 || depth));
                            // (A view whose staging was held back is not ready: the next draw asks again.)
                            if kind == ResourceType::Srv && !crate::host::commit::take_staging_deferred() {
                                listed.ready = (list.epoch(), tested.0, depth);
                            }
                        }
                    }
                    if let Some(title) = title_first
                        && listed.texture
                        && listed.tabled != Some((title + k, kind.is_some()))
                    {
                        listed.tabled = Some((title + k, kind.is_some()));
                        host.texture_slot_tabled(title + k, kind.is_some());
                        if kind.is_some() {
                            host.mirror_after(list, title + k, table + u64::from(k) * 32);
                        }
                    }
                    if was != (listed.ready, listed.tabled) {
                        crate::descriptor::host::note_listed(recording, words, listed);
                    }
                    continue;
                }
                let used = entries().find(|u| u.table_offset_dwords / 8 == k);
                match crate::descriptor::view(table + u64::from(k) * 32) {
                    Some(View::Constant(address, size)) => {
                        let Some((buffer, gpu, to)) = list.scratch_held(u64::from(size)) else {
                            host.make_null(index);
                            refused = true;
                            if null_constants() {
                                continue;
                            }
                            // The draw is dropped: the rest of the table is
                            // not made.
                            break;
                        };
                        let slot = (std::ptr::from_ref(heap) as usize, index);
                        list.defer_constants(to, address as *const u8, size as usize, slot);
                        filled = true;
                        host.constant_at(index, &buffer, gpu, size);
                    }
                    Some(view) => {
                        host.make_at(index, &view);
                        let kind = used.map(|u| u.kind).or_else(|| {
                            selected.binary_search(&k).ok().and_then(|_| view_kind(&view))
                        });
                        if let Some(kind) = kind {
                            readable(list, kind, &view);
                        }
                        // The title's slot is a table's: out of the queue
                        // of textures picked by index if the draw read it
                        // (its texture is up to date now), and made in
                        // place as the texture pass would have made it,
                        // for a shader that picks it by index all the same.
                        if let Some(title) = title_first
                            && crate::descriptor::host::is_texture_view(&view)
                        {
                            host.texture_slot_tabled(title + k, kind.is_some());
                            if kind.is_some() {
                                host.mirror_after(list, title + k, table + u64::from(k) * 32);
                            }
                        }
                    }
                    None => {
                        host.make_null(index);
                        if used.is_some() {
                            once!(slot, "a table entry without a view");
                        }
                    }
                }
            }
            let offset = fresh_offset(first, refused, null_constants());
            made.push((slot, offset));
            if offset == NO_CONSTANTS {
                // Not a table later draws of the list may take as it is: the
                // next one that reads it makes it again (and asks for scratch
                // memory again). The draw is dropped (`bind`): its other
                // tables are not made.
                self.fresh_tables.remove(&key);
                // Its slots are no draw's: given back now, unless a fill of
                // the constants before the one refused may make some of them
                // again at the list's submission (`crate::host::card_steps`);
                // then they stay the list's until it has run, as every
                // table's. (Kept, the slots of every table refused so could
                // run the ring dry within a frame.)
                let held = (std::ptr::from_ref(heap) as usize, host.id(), first, n);
                if !filled && let Some(at) = self.dynamic.iter().rposition(|&d| d == held) {
                    self.dynamic.swap_remove(at);
                    host.release(first, n);
                }
                return made;
            }
        }
        made
    }

    /// The first draws, at debug level: what they bind and read
    /// (diagnostics builds).
    #[cfg(feature = "diagnostics")]
    fn trace(&self, kind: &str, count: u32, first: u32) {
        static TRACED: AtomicU64 = AtomicU64::new(0);
        if !tracing::enabled!(tracing::Level::DEBUG) {
            return;
        }
        let listed = crate::capture::listed(crate::queue::presented() + 1);
        let logged = listed || TRACED.fetch_add(1, Ordering::Relaxed) < 64;
        let targets: Vec<String> = self
            .targets
            .iter()
            .filter_map(|&t| crate::resource::host_made(t as This))
            .map(|t| {
                // SAFETY: a live resource.
                let d = unsafe { t.GetDesc() };
                format!("{}x{} f{}", d.Width, d.Height, d.Format.0)
            })
            .collect();
        let v = self.vertex[0];
        let line = format!(
            "{kind} {count} from {first} topology {} pipeline {:#x} targets {targets:?} vb {:#x}+{} stride {} reads {:?}",
            self.topology,
            self.pipeline,
            v.address,
            v.size,
            v.stride,
            self.reads(false)
        );
        if logged {
            tracing::debug!("draw {line}");
        }
        let mut frame = FRAME.lock().unwrap_or_else(|e| e.into_inner());
        if frame.len() < 256 {
            frame.push(line);
        }
    }

    /// What a draw's shaders read, one line per used resource (diagnostics
    /// builds).
    #[cfg(feature = "diagnostics")]
    fn reads(&self, compute: bool) -> Vec<String> {
        let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) else {
            return vec!["no host pipeline".into()];
        };
        let mut out = Vec::new();
        for stage in &p.stages {
            let t = stage.translated();
            for u in &t.used {
                let what = match (u.root, self.roots_ref(compute).get(usize::from(u.slot))) {
                    (RootType::Table, Some(Root::Table(table))) => {
                        let handle = table + u64::from(u.table_offset_dwords) * 4;
                        describe(crate::descriptor::view(handle).as_ref())
                    }
                    (root, _) => format!("{root:?}"),
                };
                out.push(format!(
                    "{} {:?} s{}+{}: {what}",
                    stage.shader.hash, u.kind, u.slot, u.table_offset_dwords
                ));
            }
        }
        out
    }

    pub(crate) fn draw(
        &mut self,
        list: &HostList,
        vertices: u32,
        instances: u32,
        first: u32,
        first_instance: u32,
    ) {
        self.window = None;
        #[cfg(feature = "diagnostics")]
        self.trace("draw", vertices, first);
        let ready = self.prepare(list, false);
        // A pipeline with the rectangle geometry stage draws the title's
        // vertices as they are; any other gets its rectangles expanded.
        let rects = self.topology == RECTANGLES && !self.completes_rects();
        let ran = ready && (!rects || self.rectangles(list, vertices, first));
        if ran {
            // Rectangles were expanded into their own vertex buffers.
            let (count, from) = if rects {
                (vertices / 3 * 6, 0)
            } else {
                (vertices, first)
            };
            #[cfg(feature = "diagnostics")]
            list.note_op(|| self.op_text(format!("draw {count}x{instances}"), false));
            // SAFETY: an open list with everything set.
            unsafe {
                list.list
                    .DrawInstanced(count, instances, from, first_instance)
            };
            self.drew(crate::pipeline::host_pipeline(self.pipeline as This));
            DRAWN.fetch_add(1, Ordering::Relaxed);
        }
        if !ran {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            crate::runreport::left_out(self.pipeline);
            #[cfg(feature = "diagnostics")]
            dropped(self.pipeline);
        }
    }

    /// Whether the bound pipeline completes rectangles itself, in the
    /// geometry stage rectangle pipelines get (`HostPipeline::rects`).
    fn completes_rects(&self) -> bool {
        crate::pipeline::host_pipeline(self.pipeline as This).is_some_and(|p| p.rects)
    }

    /// Binds vertex buffers holding the rectangles of vertices `first..first
    /// + count` as triangles, made when the list is submitted.
    fn rectangles(&self, list: &HostList, count: u32, first: u32) -> bool {
        let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) else {
            return false;
        };
        let rects = u64::from(count / 3);
        for slot in 0..32 {
            let elements: Vec<(u32, u32)> = p
                .inputs
                .iter()
                .filter(|e| e.slot == slot && !e.per_instance)
                .map(|e| (e.offset, e.format))
                .collect();
            if elements.is_empty() {
                continue;
            }
            let v = self.vertex[slot as usize];
            let stride = u64::from(v.stride);
            let from = v.address + u64::from(first) * stride;
            if v.address == 0
                || stride == 0
                || u64::from(first + count) * stride > u64::from(v.size)
                || crate::shadow::find(from, rects * 3 * stride).is_none()
            {
                once!(slot, "rectangle vertices outside their buffer");
                return false;
            }
            let Some((gpu, to)) = list.scratch(rects * 6 * stride) else {
                // Dropped, as before, and now counted with the drops for
                // want of scratch memory ([`null_constants`]).
                #[cfg(feature = "diagnostics")]
                crate::pipeline::note_why("no memory for its rectangles' vertices".to_owned());
                crate::runreport::hint(victory_pipeline::report::LeftOut::OutOfMemory);
                return false;
            };
            let (from, to, stride) = (from as usize, to as usize, stride as usize);
            let n = rects as usize;
            list.defer_host_read(from, n * 3 * stride, move || {
                // The vertices are read through the kernel, at submission; in
                // place only if that fails, with no change of mapping under
                // the read (see `crate::watch::title_memory_reading`): while
                // the world streams a change is under way most of the time,
                // and this is the thread that submits.
                let mut read = vec![0u8; n * 3 * stride];
                let held = (!crate::host::read_into(from as *const u8, &mut read)).then(crate::watch::title_memory_reading);
                // SAFETY: the vertices lie in a title buffer (checked when
                // recorded) and the scratch memory is the list's.
                let (src, dst) = unsafe {
                    (
                        if held.is_some() { std::slice::from_raw_parts(from as *const u8, n * 3 * stride) } else { &read[..] },
                        std::slice::from_raw_parts_mut(to as *mut u8, n * 6 * stride),
                    )
                };
                crate::primitive::expand_rects(src, dst, stride, &elements);
            });
            let view = D3D12_VERTEX_BUFFER_VIEW {
                BufferLocation: gpu,
                SizeInBytes: (rects * 6 * stride as u64) as u32,
                StrideInBytes: v.stride,
            };
            // SAFETY: an open list; the view names live scratch memory.
            unsafe { list.list.IASetVertexBuffers(slot, Some(&[view])) };
        }
        true
    }

    pub(crate) fn draw_indexed(
        &mut self,
        list: &HostList,
        indices: u32,
        instances: u32,
        first: u32,
        base: i32,
        first_instance: u32,
    ) {
        self.window = None;
        #[cfg(feature = "diagnostics")]
        self.trace("indexed", indices, first);
        // Indexed rectangles are drawn by the rectangle geometry stage
        // alone (nothing expands them on the CPU).
        let lost = self.topology == RECTANGLES && !self.completes_rects();
        if lost {
            once!("indexed rectangles not drawn");
        }
        let ran = !lost && self.prepare(list, true);
        if ran {
            #[cfg(feature = "diagnostics")]
            list.note_op(|| self.op_text(format!("indexed {indices}x{instances}"), false));
            // SAFETY: an open list with everything set.
            unsafe {
                list.list
                    .DrawIndexedInstanced(indices, instances, first, base, first_instance)
            };
            self.drew(crate::pipeline::host_pipeline(self.pipeline as This));
            DRAWN.fetch_add(1, Ordering::Relaxed);
        }
        if !ran {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            crate::runreport::left_out(self.pipeline);
            #[cfg(feature = "diagnostics")]
            dropped(self.pipeline);
        }
    }
}

impl State {
    /// Shaders that can write buffers were recorded: arguments may be others
    /// from here on.
    fn buffers_written(&mut self) {
        match &mut self.batch {
            Some(batch) if batches() == 2 => batch.wrote = true,
            batch => *batch = None,
        }
    }

    /// A draw with the pipeline `p` was recorded (as [`State::buffers_written`]
    /// if its shaders can store to buffers).
    fn drew(&mut self, p: Option<&HostPipeline>) {
        if p.is_none_or(|p| p.stages.iter().any(|stage| stage.translated().stores)) {
            self.batch = None;
        }
    }

    /// A buffer was copied to: what was clamped of it ahead is clamped anew.
    pub(crate) fn copied(&mut self) {
        self.window = None;
        self.batch = None;
    }

    /// The title waits for earlier work (a barrier or a pipeline sync).
    pub(crate) fn synced(&mut self) {
        if self.batch.as_ref().is_some_and(|b| b.wrote) {
            self.batch = None;
        }
    }

    /// Where the clamped record of `kind` at `arguments` of the copy `args`
    /// is, from the open batch or a new one ([`batches`]). `None` if it
    /// cannot be clamped in one.
    fn batched(
        &mut self,
        list: &HostList,
        args: &std::sync::Arc<crate::shadow::Shadow>,
        arguments: u64,
        kind: u32,
    ) -> Option<(ID3D12Resource, u64)> {
        const BATCH_USE: usize = usize::MAX - 0x78;
        const DISPATCH: u32 = 2;
        // Another host list, or this one after its reset ([`windows`]).
        if list.first_use(BATCH_USE) {
            self.batch = None;
        }
        // A draw after dispatches waits for them.
        if kind != DISPATCH {
            self.synced();
        }
        let (id, offset) = (args.id(), args.offset(arguments));
        if let Some(b) = &mut self.batch
            && let Some(at) = b.table.place(id, offset, kind)
        {
            return Some((b.scratch.clone(), at));
        }
        self.batch = None;
        let (mut table, scratch) = crate::indirect::open_batch(list, (id, &args.buffer), &argument_copies(args))?;
        self.host_pipeline = 0;
        self.host_roots[1] = 0;
        let at = table.place(id, offset, kind)?;
        self.batch = Some(Batch { table, scratch: scratch.clone(), wrote: false });
        Some((scratch, at))
    }

    /// An indirect execution [`State::execute_indirect`] could not run is
    /// counted as left out (the run report).
    pub(crate) fn left_out(&self) {
        crate::runreport::left_out(self.pipeline);
    }

    /// Indirect execution of a one-argument signature of `kind` (PC
    /// numbering: 0 draw, 1 indexed draw, 2 dispatch, 10 mesh dispatch) with
    /// records `stride` bytes apart at title address `arguments`, and the
    /// record count at `count` if the title gives one: the GPU reads them
    /// from the buffer's host copy, where earlier dispatches may have
    /// written them. False if it cannot run on the host.
    pub(crate) fn execute_indirect(
        &mut self,
        list: &HostList,
        (kind, stride, max): (u32, u32, u32),
        arguments: u64,
        count: Option<u64>,
    ) -> bool {
        // Skip before the argument clamp can mutate the indirect buffer.
        #[cfg(feature = "diagnostics")]
        if crate::pipeline::is_skipped(self.pipeline as This) {
            crate::pipeline::note_drop(self.pipeline as This);
            return false;
        }
        if crate::pipeline::is_builder(self.pipeline as This) && !crate::pipeline::builder_runs() {
            crate::runreport::hint(victory_pipeline::report::LeftOut::BuilderOff);
            return false;
        }
        const DRAW: u32 = 0;
        const INDEXED: u32 = 1;
        const DISPATCH: u32 = 2;
        const MESH: u32 = 10;
        #[cfg(feature = "diagnostics")]
        let why = |what: &str| crate::pipeline::note_why(format!("indirect kind {kind}: {what}"));
        #[cfg(not(feature = "diagnostics"))]
        let why = |_: &str| {};
        let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) else {
            #[cfg(feature = "diagnostics")]
            crate::pipeline::note_drop(self.pipeline as This);
            return false;
        };
        // Indirect rectangles (the title's tiled lighting, 256
        // indirect draws a frame, makes their corners from the vertex
        // number) are completed by the pipeline's geometry stage: the
        // arguments stay the title's, three vertices a rectangle. They
        // cannot be expanded on the CPU, which does not know their count.
        // shortcut: in a pipeline without that stage, a rectangle with no
        // per-vertex inputs is drawn as its first triangle (three vertices,
        // half of it), and one with vertex buffers is not drawn.
        let made_rects = !p.rects
            && kind == DRAW
            && self.topology == RECTANGLES
            && !p.inputs.iter().any(|e| !e.per_instance);
        if made_rects {
            once!("indirect rectangles without vertex inputs drawn as their first triangle");
        }
        let matches_kind = match kind {
            DRAW | INDEXED => {
                (self.topology != RECTANGLES || made_rects || p.rects) && !p.compute && !p.mesh
            }
            MESH => !p.compute && p.mesh,
            DISPATCH => p.compute,
            _ => false,
        };
        if !matches_kind {
            why(&format!(
                "pipeline mismatch (compute {}, mesh {}, topology {})",
                p.compute, p.mesh, self.topology
            ));
            return false;
        }
        let len = u64::from(stride) * u64::from(max.max(1));
        let Some(args) = crate::shadow::find_any(arguments, len) else {
            why("arguments outside title buffers");
            return false;
        };
        let counter = match count {
            Some(at) => match crate::shadow::find_any(at, 4) {
                Some(s) => Some((s, at)),
                None => {
                    why("count outside title buffers");
                    return false;
                }
            },
            None => None,
        };
        let Some(signature) = matches!(kind, DRAW | INDEXED | DISPATCH | MESH)
            .then(|| host_signature(kind, stride))
            .flatten()
        else {
            why("no host signature");
            return false;
        };
        if kind == MESH && (mesh_test() == 1 || !mesh_kept(self.pipeline)) {
            why("mesh draws are switched off (VICTORY_MESH_TEST=skip)");
            return false;
        }
        // One record without a count: clamped in a batch (`batches`).
        let batch = if batches() > 0 && max == 1 && count.is_none() {
            args.refresh(list, arguments, len);
            self.batched(list, &args, arguments, kind)
        } else {
            None
        };
        if batch.is_some() {
            self.window = None;
        }
        // A window of records clamped in one pass (`windows`): for single
        // records without a count buffer, drawn by shaders that store to no
        // buffer. Anything else ends the window: it uses the scratch buffer
        // or may write the arguments.
        const WINDOW_USE: usize = usize::MAX - 0x77;
        let offset = args.offset(arguments);
        let windowed = windows()
            && kind != DISPATCH
            && max == 1
            && count.is_none()
            && !p.stages.iter().any(|stage| stage.translated().stores);
        // Another host list (a new one after a cut, or this one after its
        // reset) has another scratch buffer, or this one's with old bytes.
        if !windowed || list.first_use(WINDOW_USE) {
            self.window = None;
        }
        let same = |w: &Window| w.copy == args.id() && w.kind == kind && w.stride == stride;
        let hit = self
            .window
            .as_ref()
            .filter(|w| same(w) && offset >= w.base)
            .filter(|w| (offset - w.base) % u64::from(w.step) == 0 && (offset - w.base) / u64::from(w.step) < u64::from(w.records))
            .map(|w| (w.scratch.clone(), offset - w.base));
        let (bounded, at) = match batch.or(hit) {
            Some(hit) => hit,
            None => {
                // A record a short step after the last window's last one
                // (the same step as that window's, if it had one): the
                // title walks the buffer, so the ones that step apart
                // after it are clamped with it.
                let (step, records) = match &self.window {
                    Some(w) if windowed && same(w) => {
                        let last = w.base + u64::from(w.records - 1) * u64::from(w.step);
                        let step = offset.saturating_sub(last);
                        let walks = step >= u64::from(stride)
                            && step <= 256
                            && step % 4 == 0
                            && (w.records == 1 || step == u64::from(w.step));
                        if walks {
                            let fit = (args.left(arguments) / step).min(WINDOW_MOST) as u32;
                            (step as u32, (if w.records == 1 { 16 } else { w.records * 2 }).min(fit).max(1))
                        } else {
                            (stride, max)
                        }
                    }
                    _ => (stride, max),
                };
                args.refresh(list, arguments, len.max(u64::from(step) * u64::from(records)));
                if let Some((s, at)) = &counter {
                    s.refresh(list, *at, 4);
                }
                // Bounded first: the clamp binds a compute pipeline of its
                // own and copies clamped active records into a scratch
                // buffer at offset 0.
                let count_arg = counter.as_ref().map(|(s, at)| (&s.buffer, s.offset(*at)));
                // More than the record asked for: a window clamped ahead,
                // whose unfilled slots must not spend the list's budget.
                let bounded = if windowed && records > 1 {
                    crate::indirect::bound_ahead(list, (&args.buffer, offset), (kind, step, records))
                } else {
                    crate::indirect::bound(list, (&args.buffer, offset), count_arg, (kind, step, records))
                };
                let Some(bounded) = bounded else {
                    self.window = None;
                    why(crate::indirect::refused_why().says());
                    crate::runreport::hint(victory_pipeline::report::LeftOut::IndirectNotBounded);
                    return false;
                };
                self.host_pipeline = 0;
                self.host_roots[1] = 0;
                self.window = windowed.then(|| Window {
                    copy: args.id(),
                    kind,
                    stride,
                    step,
                    base: offset,
                    records,
                    scratch: bounded.clone(),
                });
                (bounded, 0)
            }
        };
        let ran = match kind {
            DRAW | INDEXED => self.prepare(list, kind == INDEXED),
            MESH => self.prepare_for(list, false, true),
            DISPATCH => self.bind(list, p, true),
            _ => false,
        };
        if !ran {
            why("prepare failed");
            return false;
        }
        #[cfg(feature = "diagnostics")]
        list.note_op(|| {
            self.op_text(format!("indirect kind {kind} max {max} at {arguments:#x}"), kind == DISPATCH)
        });
        #[cfg(feature = "diagnostics")]
        list.ran_indirect(p.pso.as_raw() as usize, &p.name, kind, max, &self.user);
        if kind == MESH {
            self.mesh_scissor(list, true);
        }
        // SAFETY: an open list with everything set; the scratch buffer and
        // host copies are kept alive by the list, and the signature is cached.
        unsafe {
            list.list.ExecuteIndirect(
                &signature,
                max,
                &bounded,
                at,
                counter.as_ref().map(|(s, _)| &s.buffer),
                counter.as_ref().map_or(0, |(s, at)| s.offset(*at)),
            )
        };
        if kind == MESH {
            self.mesh_scissor(list, false);
        }
        if kind == DISPATCH {
            self.buffers_written();
        } else {
            self.drew(Some(p));
        }
        // The scratch buffer stays readable as arguments: the next draw of
        // the window reads it with no barrier, and the next clamp moves it
        // back (`crate::indirect`).
        if kind == DISPATCH {
            DISPATCHED.fetch_add(1, Ordering::Relaxed);
        } else {
            DRAWN.fetch_add(1, Ordering::Relaxed);
        }
        true
    }
}

/// `DispatchX`: the counts are threads (the console's dispatch initiator bit
/// "use thread dimensions"; DexNom/VIctory bc3cd4b0). Taken for groups, a
/// rehydration stage of the structure builder ran up to 64 times its threads
/// and stored over the structures after its own.
const THREAD_DIMENSIONS: u32 = 0x20;

/// `VICTORY_THREAD_DIMENSIONS=0`: such counts are taken for groups, as before.
fn thread_dimensions() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_THREAD_DIMENSIONS").is_none_or(|v| v != "0"))
}

/// The groups of `size` threads that hold `threads`.
fn dispatch_groups(threads: [u32; 3], size: [u32; 3]) -> [u32; 3] {
    std::array::from_fn(|i| threads[i].div_ceil(size[i]))
}

/// Where a compute pipeline's user data has a word the host computes: the
/// root parameter and the dword in it.
fn compute_word(p: &HostPipeline, kind: u32) -> Option<(u32, u32)> {
    let stage = p.stages.first().filter(|_| p.compute)?;
    let (param, _) = stage.user_data?;
    let word = stage.translated().host.user_data.iter().find(|u| u.root == RootType::Other(kind))?;
    Some((param, word.index))
}

#[cfg(test)]
mod thread_tests {
    use super::*;

    #[test]
    fn threads_are_counted_in_the_programs_groups() {
        assert_eq!(dispatch_groups([8847, 1, 1], [64, 1, 1]), [139, 1, 1]);
        assert_eq!(dispatch_groups([74, 1, 1], [64, 1, 1]), [2, 1, 1]);
        assert_eq!(dispatch_groups([32, 1, 1], [32, 1, 1]), [1, 1, 1]);
        assert_eq!(dispatch_groups([0, 12, 9], [64, 4, 4]), [0, 3, 3]);
    }
}

/// The host command signature for one argument of `kind` with records
/// `stride` bytes apart (no root arguments, so no root signature), made
/// once.
fn host_signature(kind: u32, stride: u32) -> Option<ID3D12CommandSignature> {
    static MADE: std::sync::Mutex<Vec<((u32, u32), ID3D12CommandSignature)>> =
        std::sync::Mutex::new(Vec::new());
    let mut made = MADE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, s)) = made.iter().find(|(k, _)| *k == (kind, stride)) {
        return Some(s.clone());
    }
    let argument = D3D12_INDIRECT_ARGUMENT_DESC {
        Type: D3D12_INDIRECT_ARGUMENT_TYPE(kind as i32),
        ..Default::default()
    };
    let desc = D3D12_COMMAND_SIGNATURE_DESC {
        ByteStride: stride,
        NumArgumentDescs: 1,
        pArgumentDescs: &argument,
        NodeMask: 0,
    };
    let mut signature: Option<ID3D12CommandSignature> = None;
    // SAFETY: a description of one argument; no root arguments.
    let result = unsafe {
        crate::host::host()?.device.CreateCommandSignature(
            &desc,
            None::<&ID3D12RootSignature>,
            &mut signature,
        )
    };
    if let Err(e) = result {
        once!(error = %e, kind, stride, "host command signature refused");
        return None;
    }
    let signature = signature?;
    made.push(((kind, stride), signature.clone()));
    Some(signature)
}

/// Gets what a shader reads or writes through a view ready: buffer data
/// copied, textures moved to the state they need.
pub(crate) fn view_kind(view: &View) -> Option<ResourceType> {
    match view {
        View::Shader(..) | View::Placed(false, ..) => Some(ResourceType::Srv),
        View::Unordered(..) | View::Placed(true, ..) => Some(ResourceType::Uav),
        _ => None,
    }
}

/// The host state a depth view needs of its texture: the read state if the
/// view writes no plane the format has (the title's read-only flags), else
/// the write state.
/// shortcut: whole textures have one state, so a view that writes one of
/// two planes counts as writing.
fn depth_state(texture: &ID3D12Resource, desc: Option<&D3D12_DEPTH_STENCIL_VIEW_DESC>) -> u32 {
    // D16_UNORM and D32_FLOAT: depth formats without a stencil plane.
    const ONE_PLANE: [i32; 2] = [55, 40];
    let Some(desc) = desc else {
        return DEPTH_WRITE;
    };
    let format = if desc.Format.0 == 0 {
        // SAFETY: a live texture; a view without a format has its own.
        unsafe { texture.GetDesc() }.Format.0
    } else {
        desc.Format.0
    };
    let mut needed = D3D12_DSV_FLAG_READ_ONLY_DEPTH.0;
    if !ONE_PLANE.contains(&format) {
        needed |= D3D12_DSV_FLAG_READ_ONLY_STENCIL.0;
    }
    if desc.Flags.0 & needed == needed {
        DEPTH_READ
    } else {
        DEPTH_WRITE
    }
}

/// A few things, up to `N` of them without memory of the heap: what a draw's
/// tables are gone through with, some six lists a stage.
pub(crate) struct Few<T, const N: usize> {
    few: [T; N],
    len: usize,
    /// All of them, if they are more.
    many: Vec<T>,
}

impl<T: Copy + Default, const N: usize> Default for Few<T, N> {
    fn default() -> Self {
        Few { few: [T::default(); N], len: 0, many: Vec::new() }
    }
}

impl<T: Copy, const N: usize> Few<T, N> {
    fn push(&mut self, what: T) {
        if self.many.is_empty() && self.len < N {
            self.few[self.len] = what;
            self.len += 1;
        } else {
            if self.many.is_empty() {
                self.many.extend_from_slice(&self.few);
            }
            self.many.push(what);
        }
    }
}

impl<T, const N: usize> std::ops::Deref for Few<T, N> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        if self.many.is_empty() { &self.few[..self.len] } else { &self.many }
    }
}

impl<T, const N: usize> std::ops::DerefMut for Few<T, N> {
    fn deref_mut(&mut self) -> &mut [T] {
        if self.many.is_empty() { &mut self.few[..self.len] } else { &mut self.many }
    }
}

/// A table made afresh in dynamic slots, in a list ([`State::tables`]).
struct FreshTable {
    first: u32,
    /// The list's epoch ([`HostList::epoch`]) at which all its entries that
    /// these readers (a stage's list of what it reads, whether it stores, the
    /// tested depth target) read were brought up to date, and whether that read the
    /// tested depth target.
    epoch: u64,
    ready: Vec<((usize, usize, bool, usize), bool)>,
}

/// Whether a table a list reads again, with nothing changed in the list
/// since, is left as it is (`VICTORY_READY_ONCE=0`: every entry is gone
/// through again at every draw).
fn ready_once() -> bool {
    crate::toggled!("VICTORY_READY_ONCE")
}

/// A hash of records' words: 64 bits, two multiplications a record. (The
/// standard library's took 3 % of the recording threads' time.)
#[derive(Default)]
struct RecordHash(u64);

impl RecordHash {
    fn add(&mut self, [a, b, c]: [u32; 3]) {
        let fold = |x: u64, y: u64| {
            let p = u128::from(x) * u128::from(y);
            p as u64 ^ (p >> 64) as u64
        };
        let words = u64::from(a) << 32 | u64::from(b);
        let first = fold(self.0 ^ words ^ 0x2d35_8dcc_aa6c_78a5, 0x8bb8_4b93_962e_acc9 ^ u64::from(c));
        self.0 = fold(first ^ 0x4b33_a62e_d433_d4a3, 0x4d5a_2da5_1de1_aa47 ^ self.0.rotate_left(23)) ^ first;
    }
}

fn readable(list: &HostList, kind: ResourceType, view: &View) {
    const UNORDERED_ACCESS: u32 = 0x8;
    // (Unordered access to a placed texture is to the resource made for its
    // memory too: `descriptor::host::placed_unordered`.)
    let placed = crate::descriptor::host::placed_texture(view)
        .or_else(|| crate::descriptor::host::placed_unordered(view));
    let view = placed.as_ref().unwrap_or(view);
    // Buffer views read title memory: their range is copied at submission;
    // what shaders write through unordered access goes back to it after.
    if let Some((shadow, address, len)) = crate::descriptor::host::buffer_copy(view) {
        shadow.refresh(list, address, len);
        if kind == ResourceType::Uav {
            // A shader that cannot store to buffers leaves the range as
            // the title has it: nothing to find afterwards.
            if STORES.get() || !write_skip() {
                #[cfg(feature = "diagnostics")]
                crate::host::wide_write(address, len, 0);
                shadow.gpu_writes(list, address, len);
            } else {
                #[cfg(feature = "diagnostics")]
                {
                    crate::copystat::add(crate::copystat::C::GpuWritesLeftOut, 1);
                    crate::copystat::add(crate::copystat::C::GpuWritesLeftOutBytes, len);
                }
            }
        }
    }
    match (view, kind) {
        (View::Shader(resource, _), ResourceType::Srv) => {
            // A view the title placed, in another format, in the memory of
            // a texture the host writes: that texture's texels, copied on
            // the host (title memory does not have them). The texture is
            // held until the copy is recorded (`alias_held`, after fxceless
            // #1336, a463dcb).
            let aliased = crate::resource::alias_held(*resource as This)
                .is_some_and(|source| crate::texture::alias(list, *resource as This, source.get()));
            if !aliased
                && let Some(t) = crate::resource::host_copy(*resource as This)
                && t.flags() & 0x7 == 0
            {
                // (A texture the title writes: no target, depth or unordered
                // access, `crate::texture::title_written`.)
                // Of a texture the title placed, the levels the view shows.
                let first = match (&placed, view) {
                    (Some(_), View::Shader(_, Some(desc))) => crate::resource::first_mip(desc),
                    _ => 0,
                };
                crate::texture::upload_from(list, *resource as This, first);
            }
            // A depth texture shaders also write: what they wrote, first.
            list.twin_use(*resource as This, false, false);
            // The depth target the draw only tests is read in both ways at
            // once: both read states, which its preparation asks for too.
            let (tested, _) = TESTED_DEPTH.get();
            let state = if tested != 0 && tested == *resource {
                TESTED_DEPTH.set((tested, true));
                DEPTH_READ | SHADER_RESOURCE
            } else {
                SHADER_RESOURCE
            };
            crate::list::ensure_state(list, *resource as This, state);
            #[cfg(feature = "diagnostics")]
            if tracing::enabled!(tracing::Level::DEBUG)
                && let Some(t) = crate::resource::host_resource(*resource as This)
            {
                // SAFETY: a live resource.
                let d = unsafe { t.GetDesc() };
                if d.Flags.0 & 7 == 0 {
                    tracing::debug!(
                        resource = format_args!("{resource:#x}"),
                        ?d,
                        "a shader reads a texture the title fills"
                    );
                }
            }
        }
        (View::Unordered(resource, _, _), ResourceType::Uav) => {
            #[cfg(feature = "diagnostics")]
            crate::capture::note_target(*resource as This);
            // Some compute-written textures are also read by the title CPU.
            list.texture_written(*resource as This);
            // Unordered access to a depth texture is its colour twin's.
            if !list.twin_use(*resource as This, true, true) {
                crate::list::ensure_state(list, *resource as This, UNORDERED_ACCESS);
            }
        }
        _ => {}
    }
}

/// The pixel shaders whose draws get their slots listed
/// ([`State::slots_text`]): `VICTORY_LIST_SLOTS`, hash prefixes separated
/// by commas. None by default.
#[cfg(feature = "diagnostics")]
fn list_slots() -> &'static [String] {
    static LIST: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    LIST.get_or_init(|| {
        std::env::var("VICTORY_LIST_SLOTS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect()
    })
}

/// A fingerprint of the 4 KiB of title memory at `at` (FNV-1a, 64 bits; of
/// 4 KiB of zeros it is always the same value, which `slots.py` knows), or
/// "unreadable".
#[cfg(feature = "diagnostics")]
fn print_4k(at: u64) -> String {
    let mut bytes = [0u8; 4096];
    if at == 0 || !crate::host::read_into(at as *const u8, &mut bytes) {
        return "unreadable".into();
    }
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for b in bytes {
        hash = (hash ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// The `bytes` of title memory at `at`: how many are mapped, and the
/// fingerprints of the first and the last 4 KiB (a texture's smallest
/// levels lie first, its largest last).
#[cfg(feature = "diagnostics")]
fn memory_state(at: u64, bytes: u64) -> String {
    let mapped = crate::texture::mapped_bytes(at as *const u8, bytes as usize);
    let last = at + bytes.saturating_sub(4096);
    format!("memory={at:#x} mapped={mapped}/{bytes} first4k={} last4k={}", print_4k(at), print_4k(last))
}

/// Title texture `this` for the frame listing: its size, whether it has a
/// host copy (not made here), its staging buffer's state, whether the host
/// copied into it, and its memory ([`memory_state`]).
#[cfg(feature = "diagnostics")]
fn texture_state(this: This) -> String {
    let Some((at, desc)) = crate::resource::texture_memory(this) else {
        return "not a texture".into();
    };
    // SAFETY: 56 bytes in the description's layout.
    let words = unsafe { std::ptr::from_ref(&desc).cast::<[u32; 14]>().read_unaligned() };
    let bytes = crate::resource::placed_bytes(&words).unwrap_or(0);
    let staged = match crate::texture::staged_state(this) {
        Some((uploaded, dirty)) => format!("staged=yes uploaded={uploaded} dirty={dirty}"),
        None => "staged=no".into(),
    };
    format!(
        "{}x{} f{} levels={} host_copy={} {staged} host_written={} title_fills={} {}",
        u64::from(words[4]) | u64::from(words[5]) << 32,
        words[6],
        words[8],
        words[7] >> 16,
        crate::resource::host_made(this).is_some(),
        crate::texture::host_wrote(this as usize),
        crate::resource::title_fills(this),
        memory_state(at as u64, bytes)
    )
}

/// A view of the title's heap for the frame listing: a texture with its
/// state ([`texture_state`]; a placed one that has no resource yet with
/// its memory alone), anything else in a few words.
#[cfg(feature = "diagnostics")]
fn view_state(view: Option<&View>) -> String {
    match view {
        None => "no view".into(),
        Some(View::Placed(false, r, d)) if r[0] != 1 => {
            let address = u64::from(d[10]) | u64::from(d[11]) << 32;
            let state = match crate::resource::texture_at_made(address, r) {
                Some(this) => texture_state(this),
                None => format!(
                    "{}x{} f{} levels={} resource=none {}",
                    u64::from(r[4]) | u64::from(r[5]) << 32,
                    r[6],
                    r[8],
                    r[7] >> 16,
                    memory_state(address, crate::resource::placed_bytes(r).unwrap_or(0))
                ),
            };
            // (Last, so that readers of the older lines still find
            // theirs: the level the view starts at, and how many it shows.)
            format!("placed texture view_format={} {state} view_levels={}+{}", d[0], d[4], d[5] as i32)
        }
        Some(View::Shader(r, d)) if crate::resource::texture_memory(*r as This).is_some() => {
            format!(
                "texture object {r:#x} view_format={} {} view_levels={}",
                d.as_ref().map_or(0, |d| d[0]),
                texture_state(*r as This),
                d.as_ref().map_or("all".to_owned(), |d| format!("{}+{}", d[4], d[5] as i32))
            )
        }
        Some(v) => format!("{v:?}").chars().take(80).collect(),
    }
}

/// A view in a few words (diagnostics builds).
#[cfg(feature = "diagnostics")]
fn describe(view: Option<&View>) -> String {
    let texture = |r: This| {
        crate::resource::host_made(r).map(|t| {
            // SAFETY: a live resource.
            let d = unsafe { t.GetDesc() };
            format!("{}x{} f{} flags {:#x} of {r:?}", d.Width, d.Height, d.Format.0, d.Flags.0)
        })
    };
    match view {
        None => "no view".into(),
        Some(v @ View::Shader(r, _)) => texture(*r as This)
            .unwrap_or_else(|| format!("buffer {:?}", crate::descriptor::host::buffer_range(v))),
        Some(v @ View::Unordered(r, ..)) => match texture(*r as This) {
            Some(t) => format!("uav texture {t}"),
            None => format!(
                "uav resource {r:#x} buffer {:?}",
                crate::descriptor::host::buffer_range(v)
            ),
        },
        Some(View::Placed(u, r, d)) => format!(
            "placed uav={u} dim {} {}x{} f{} layout {:#x} at {:#x}",
            r[0],
            u64::from(r[4]) | u64::from(r[5]) << 32,
            r[6],
            r[8],
            r[11],
            u64::from(d[10]) | u64::from(d[11]) << 32
        ),
        // The first four values, as a colour rectangle's constants are.
        Some(View::Constant(address, size))
            if *size >= 16 && crate::texture::committed(*address as *const u8, 16) =>
        {
            // SAFETY: 16 committed bytes of title memory.
            let v = unsafe { (*address as *const [f32; 4]).read_unaligned() };
            format!("Constant({address:#x}, {size}) {v:?}")
        }
        Some(v) => format!("{v:?}").chars().take(60).collect(),
    }
}

/// The console's own primitive topologies.
const TRIANGLE_FAN: u32 = 6;
const RECTANGLES: u32 = 17;
const LINE_LOOP: u32 = 18;
const QUADS: u32 = 19;
const QUAD_STRIP: u32 = 20;

/// Fixed-function state, straight onto the host list.
pub(crate) fn viewports(list: &HostList, viewports: &[D3D12_VIEWPORT]) {
    // SAFETY: an open list.
    unsafe { list.list.RSSetViewports(viewports) };
}

pub(crate) fn scissors(list: &HostList, rects: &[RECT]) {
    // SAFETY: an open list.
    unsafe { list.list.RSSetScissorRects(rects) };
}

pub(crate) fn blend_factor(list: &HostList, factor: Option<&[f32; 4]>) {
    // SAFETY: an open list.
    unsafe { list.list.OMSetBlendFactor(factor) };
}

pub(crate) fn stencil_ref(list: &HostList, reference: u32) {
    #[cfg(feature = "diagnostics")]
    crate::capture::op(std::ptr::from_ref(list) as usize, || format!("STENCIL REF {reference:#x}"));
    // SAFETY: an open list.
    unsafe { list.list.OMSetStencilRef(reference) };
}

pub(crate) fn depth_bounds(list: &HostList, min: f32, max: f32) {
    #[cfg(feature = "diagnostics")]
    crate::capture::op(std::ptr::from_ref(list) as usize, || {
        format!("DEPTH BOUNDS {min}..{max}")
    });
    if crate::host::host().is_some_and(|h| h.depth_bounds())
        && let Ok(l1) = list.list.cast::<ID3D12GraphicsCommandList1>()
    {
        // SAFETY: an open list on a device that supports depth bounds.
        unsafe { l1.OMSetDepthBounds(min, max) };
    }
}

/// The bytes a texture pass may stage by the end of this frame, `bytes`
/// unless memory is `short` or `critical` ([`crate::host::commit::prefetch_limit`]);
/// `exempt` (a bake draw, which cannot be retried, or a budget the user set
/// with `VICTORY_TEXTURE_STAGE_MB`) leaves it as it is.
fn pass_bytes_cap(bytes: u64, exempt: bool, short: bool, critical: bool) -> u64 {
    if exempt { bytes } else { crate::host::commit::prefetch_limit(bytes, short, critical) }
}

/// The share of the time between two passes over textures picked by index
/// that a pass may take in the world, in hundredths
/// (`VICTORY_TEXTURE_STAGE_SHARE`, 10; `0`: no share, the time of
/// `VICTORY_TEXTURE_STAGE_MS` alone, as before).
///
/// The pass runs on a thread that records the frame, inside the first list
/// that picks views by index, and the submission waits for that list. With
/// 250 ms a pass (750 with a backlog) it was the frame line's "heap buffers
/// ... passes" at up to 200 ms, for hundreds of textures and tens of MB a
/// frame: per texture its making and its view, not its bytes. A
/// share of the frame shrinks with the frame: 50 ms of a 500 ms frame, the
/// least ([`STAGE_LEAST_MS`]) at 30 frames a second, where the old time was
/// most of the frame. Not for a bake's own budget, outside the world (under
/// [`STAGE_WORLD_DRAWS`] draws a frame: loading screens, where textures are
/// wanted fast and frames do not matter), or with `VICTORY_TEXTURE_STAGE_MS`
/// set. What a pass leaves stays due for the next.
fn stage_share() -> u64 {
    static SHARE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *SHARE.get_or_init(|| {
        std::env::var("VICTORY_TEXTURE_STAGE_SHARE").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(10).min(100)
    })
}

/// The least time of a pass held to [`stage_share`].
const STAGE_LEAST_MS: u64 = 4;

/// Draws a frame from which the title is taken to be in the world
/// ([`stage_share`]): its loading screens draw 1 to 10.
const STAGE_WORLD_DRAWS: u64 = 300;

/// The time a pass over textures picked by index may take: `span`, or with
/// a `share` (hundredths, [`stage_share`]) of the `took` ms since the pass
/// before, in a frame of `draws` draws, that share (three times it for a
/// `backlog`, as `span` is), at least [`STAGE_LEAST_MS`], at most `span`.
fn pass_span(span: std::time::Duration, share: u64, took: u64, draws: u64, backlog: bool) -> std::time::Duration {
    if share == 0 || draws < STAGE_WORLD_DRAWS {
        return span;
    }
    let ms = (took.saturating_mul(share) / 100).max(STAGE_LEAST_MS) * if backlog { 3 } else { 1 };
    span.min(std::time::Duration::from_millis(ms))
}

/// Per-frame texture staging byte budget scaled from the adapter's local VRAM
/// budget (`None` keeps the 192 MiB 12 GiB baseline).
fn staging_budget_per_frame(vram_budget: Option<u64>) -> u64 {
    const GIB: u64 = 1 << 30;
    const MIB: u64 = 1 << 20;
    match vram_budget {
        Some(budget) if budget <= 6 * GIB => 96 * MIB,
        Some(budget) if budget >= 20 * GIB => 384 * MIB,
        Some(budget) if budget > 12 * GIB => 256 * MIB,
        _ => 192 * MIB,
    }
}

#[cfg(test)]
mod tests {
    use super::staging_budget_per_frame;

    /// A pass in the world takes its share of the frame, at least the least
    /// and at most the time it had; outside the world, or without a share,
    /// the time it had.
    #[test]
    fn a_texture_pass_takes_a_share_of_the_frame() {
        use super::{STAGE_LEAST_MS, STAGE_WORLD_DRAWS, pass_span};
        use std::time::Duration;
        let ms = Duration::from_millis;
        let world = STAGE_WORLD_DRAWS;
        assert_eq!(pass_span(ms(250), 10, 500, world, false), ms(50));
        assert_eq!(pass_span(ms(750), 10, 500, world, true), ms(150));
        assert_eq!(pass_span(ms(250), 10, 33, 4000, false), ms(STAGE_LEAST_MS));
        assert_eq!(pass_span(ms(250), 10, 33, 4000, true), ms(3 * STAGE_LEAST_MS));
        assert_eq!(pass_span(ms(250), 10, 10_000, world, false), ms(250));
        assert_eq!(pass_span(ms(250), 10, 500, world - 1, false), ms(250));
        assert_eq!(pass_span(ms(250), 0, 500, world, false), ms(250));
        assert_eq!(pass_span(ms(250), 100, 120, world, false), ms(120));
    }

    #[test]
    fn staging_budget_scales_with_vram_budget() {
        const GIB: u64 = 1 << 30;
        const MIB: u64 = 1 << 20;
        assert_eq!(staging_budget_per_frame(None), 192 * MIB);
        assert_eq!(staging_budget_per_frame(Some(4 * GIB)), 96 * MIB);
        assert_eq!(staging_budget_per_frame(Some(6 * GIB)), 96 * MIB);
        assert_eq!(staging_budget_per_frame(Some(8 * GIB)), 192 * MIB);
        assert_eq!(staging_budget_per_frame(Some(12 * GIB)), 192 * MIB);
        assert_eq!(staging_budget_per_frame(Some(13 * GIB)), 256 * MIB);
        assert_eq!(staging_budget_per_frame(Some(14 * GIB)), 256 * MIB);
        assert_eq!(staging_budget_per_frame(Some(16 * GIB)), 256 * MIB);
        assert_eq!(staging_budget_per_frame(Some(20 * GIB)), 384 * MIB);
        assert_eq!(staging_budget_per_frame(Some(24 * GIB)), 384 * MIB);
        assert_eq!(staging_budget_per_frame(Some(32 * GIB)), 384 * MIB);
    }
}

#[cfg(test)]
mod record_hash_tests {
    use super::{Few, RecordHash};

    #[test]
    fn a_few_things_are_kept_in_order_however_many() {
        for n in 0..10u32 {
            let mut few: Few<u32, 4> = Few::default();
            (0..n).for_each(|i| few.push(n - i));
            assert_eq!(few.to_vec(), (0..n).map(|i| n - i).collect::<Vec<_>>());
            few.sort_unstable();
            assert_eq!(few.to_vec(), (1..=n).collect::<Vec<_>>());
        }
    }

    #[test]
    fn records_that_differ_hash_apart() {
        let of = |records: &[[u32; 3]]| {
            let mut h = RecordHash::default();
            records.iter().for_each(|r| h.add(*r));
            h.0
        };
        let mut seen = std::collections::HashSet::new();
        // One record, one word or two of it different; runs of zeros; order.
        for a in 0..64u32 {
            for b in 0..64u32 {
                for c in [0, 1, 0x8000_0000, u32::MAX] {
                    assert!(seen.insert(of(&[[a, b, c]])));
                    assert!(seen.insert(of(&[[1, 2, 3], [a, b, c]])));
                    assert!(seen.insert(of(&[[a, b, c], [1, 2, 3], [0, 0, 0]])));
                }
            }
        }
        assert!(seen.insert(of(&[])));
        // (One record of zeros is among the above.)
        let zeros: Vec<u64> = (2..100).map(|n| of(&vec![[0; 3]; n])).collect();
        assert!(zeros.iter().all(|h| seen.insert(*h)));
    }
}

#[cfg(test)]
mod bake_detect_tests {
    use super::{BakeDetect, bake_detect_of, bake_hashes_with, shaped_like_a_bake};
    use crate::shader_profile::BakeProfile;
    use std::ffi::OsStr;

    #[test]
    fn the_switch_has_three_values() {
        assert_eq!(bake_detect_of(None), BakeDetect::Hashes);
        assert_eq!(bake_detect_of(Some(OsStr::new("0"))), BakeDetect::Old);
        assert_eq!(bake_detect_of(Some(OsStr::new("shape"))), BakeDetect::Shape);
        assert_eq!(bake_detect_of(Some(OsStr::new("1"))), BakeDetect::Hashes);
    }

    #[test]
    fn the_hashes_the_mode_counts() {
        let profile = BakeProfile {
            old_pixel_shaders: vec!["aaaa000011112222".into(), "aaaa000011112223".into()],
            pixel_shaders: vec!["bbbb000011112222".into(), "bbbb000011112223".into()],
            geometry_shaders: vec!["cccc000011112222".into(), "cccc000011112223".into()],
        };
        let first = "aaaa000011112222";
        let later = "bbbb000011112223";
        let geometry = "cccc000011112222";
        assert!(bake_hashes_with(&profile, BakeDetect::Old, ["1234", first]));
        assert!(!bake_hashes_with(&profile, BakeDetect::Old, [later]));
        assert!(!bake_hashes_with(&profile, BakeDetect::Old, [geometry]));
        for mode in [BakeDetect::Hashes, BakeDetect::Shape] {
            assert!(bake_hashes_with(&profile, mode, [first]));
            assert!(bake_hashes_with(&profile, mode, ["bbbb000011112222"]));
            assert!(bake_hashes_with(&profile, mode, [later]));
            assert!(bake_hashes_with(&profile, mode, ["0000", "cccc000011112223"]));
            assert!(!bake_hashes_with(&profile, mode, ["0000", "1111"]));
            assert!(!bake_hashes_with(&profile, mode, []));
            // Default empty profile matches no hashes.
            assert!(!bake_hashes_with(&BakeProfile::default(), mode, [first, later, geometry]));
        }
    }

    #[test]
    fn a_bake_atlas_is_square_has_no_depth_and_four_targets() {
        assert!(shaped_like_a_bake(false, 4, Some((2048, 2048))));
        assert!(shaped_like_a_bake(false, 6, Some((512, 512))));
        assert!(!shaped_like_a_bake(true, 4, Some((2048, 2048))), "depth");
        assert!(!shaped_like_a_bake(false, 3, Some((2048, 2048))), "targets");
        assert!(!shaped_like_a_bake(false, 4, Some((256, 256))), "small");
        assert!(!shaped_like_a_bake(false, 4, Some((2048, 1024))), "not square");
        assert!(!shaped_like_a_bake(false, 4, None), "no first target");
    }
}

#[cfg(test)]
mod commit_guard_tests {
    use super::pass_bytes_cap;

    /// With memory not short, the guard takes nothing from a pass's bytes,
    /// and nothing at all from a bake's or a user's own: the pass stages
    /// what it did before the guard.
    #[test]
    fn a_pass_with_memory_to_spare_or_exempt_keeps_its_bytes() {
        const MIB: u64 = 1 << 20;
        for bytes in [0, 1, 64 * MIB, 192 * MIB, 2 << 30] {
            assert_eq!(pass_bytes_cap(bytes, false, false, false), bytes);
            for (short, critical) in [(false, true), (true, false), (true, true)] {
                assert_eq!(pass_bytes_cap(bytes, true, short, critical), bytes, "exempt");
                assert!(pass_bytes_cap(bytes, false, short, critical) <= bytes);
            }
        }
    }
}

#[cfg(test)]
mod constants_refused_tests {
    use super::{
        ALIASED_FRAMES, CONSTANTS_DROPPED, CONSTANTS_REFUSED, NO_CONSTANTS, RECHECK_FRAMES, constants_dropped,
        fresh_offset, next_look, tables_refused,
    };
    use std::sync::atomic::Ordering;

    /// A table one of whose constant buffers found no scratch memory drops
    /// its draw, counted as dropped for that, where it ran with a null
    /// constant buffer before (and does with `VICTORY_NULL_CONSTANTS=1`:
    /// the table's own offset); a table that found no free slots drops it
    /// as before.
    #[test]
    fn a_draw_whose_constants_found_no_memory_is_dropped_not_nulled() {
        assert_eq!(fresh_offset(5, false, false), 160);
        assert_eq!(fresh_offset(5, true, false), NO_CONSTANTS);
        assert_eq!(fresh_offset(5, true, true), 160, "drawn with null constants, as before");
        // No table's own offset, which is whole slots of 32 bytes.
        assert_ne!(NO_CONSTANTS % 32, 0);
        assert_eq!(tables_refused(&[(0, 64), (3, NO_CONSTANTS)]), Some(CONSTANTS_REFUSED));
        let both = [(0, 64), (1, u32::MAX), (3, NO_CONSTANTS)];
        assert_eq!(tables_refused(&both), Some("no free slots for its descriptor tables"));
        assert_eq!(tables_refused(&[(0, 64), (1, fresh_offset(5, true, true))]), None);
        assert_eq!(tables_refused(&[]), None);
        let before = CONSTANTS_DROPPED.load(Ordering::Relaxed);
        constants_dropped();
        assert!(CONSTANTS_DROPPED.load(Ordering::Relaxed) > before);
    }

    /// A texture picked by index whose pages are shown at other addresses
    /// too is never done with: looked at every few frames for good. Any
    /// other is done after the last of its looks, as before.
    #[test]
    fn a_texture_of_pages_shown_elsewhere_is_never_done() {
        let mut looks = 0;
        let mut waits = Vec::new();
        while let Some((wait, next)) = next_look(looks, false) {
            waits.push(wait);
            looks = next;
        }
        assert_eq!(waits, RECHECK_FRAMES[1..]);
        let mut looks = 0;
        for _ in 0..1000 {
            let (wait, next) = next_look(looks, true).expect("an aliased texture is never done");
            assert!((1..=ALIASED_FRAMES).contains(&wait), "{wait}");
            looks = next;
        }
        assert_eq!(next_look(looks, true), Some((ALIASED_FRAMES, looks)));
    }
}
