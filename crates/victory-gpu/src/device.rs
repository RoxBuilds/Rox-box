//! The device titles get from device creation, laid out as their inlined
//! code expects: the object header, then device fields. Its table starts
//! like the PC device interface (IUnknown, the object methods, then the
//! device methods in PC order); console additions follow. Methods we have
//! not written log their table offset and fail, so a run shows what to
//! write next.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::c_void;

use crate::object::{
    self, E_INVALIDARG, E_NOINTERFACE, E_NOTIMPL, HRESULT, Iid, Object, S_FALSE, S_OK, Table, This,
    guid, guid_text,
};
use crate::{
    descriptor, dxgi, fence, frame, list, pipeline, query, queue, resource, root, signature,
};

/// What D3D returns when none of the requested feature levels is supported.
const UNSUPPORTED: HRESULT = 0x887A_0004_u32 as i32;

/// The device interface id (the same as on PC). Titles also import it as
/// data.
pub static IID: Iid = guid(
    0x189819f1,
    0x1db6,
    0x4b57,
    [0xbe, 0x54, 0x18, 0x21, 0x33, 0x9b, 0x85, 0xf7],
);

/// The levels the device answers to, as on PC: `ID3D12Device1` to
/// `ID3D12Device8`, and `ID3D12Device11`, which the title creates
/// its device as. Each level's table is the one before plus its methods.
const LEVEL_IIDS: [Iid; 9] = [
    guid(
        0x77acce80,
        0x638e,
        0x4e65,
        [0x88, 0x95, 0xc1, 0xf2, 0x33, 0x86, 0x86, 0x3e],
    ),
    guid(
        0x30baa41e,
        0xb15b,
        0x475c,
        [0xa0, 0xbb, 0x1a, 0xf5, 0xc5, 0xb6, 0x43, 0x28],
    ),
    guid(
        0x81dadc15,
        0x2bad,
        0x4392,
        [0x93, 0xc5, 0x10, 0x13, 0x45, 0xc4, 0xaa, 0x98],
    ),
    guid(
        0xe865df17,
        0xa9ee,
        0x46f9,
        [0xa4, 0x63, 0x30, 0x98, 0x31, 0x5a, 0xa2, 0xe5],
    ),
    guid(
        0x8b4f173b,
        0x2fea,
        0x4b80,
        [0x8f, 0x58, 0x43, 0x07, 0x19, 0x1a, 0xb9, 0x5d],
    ),
    guid(
        0xc70b221b,
        0x40e4,
        0x4a17,
        [0x89, 0xaf, 0x02, 0x5a, 0x07, 0x27, 0xa6, 0xdc],
    ),
    guid(
        0x5c014b53,
        0x68a1,
        0x4b9b,
        [0x8b, 0xd1, 0xdd, 0x60, 0x46, 0xb9, 0x35, 0x8b],
    ),
    guid(
        0x728f1fa2,
        0xea11,
        0x4a9f,
        [0x9b, 0x52, 0x9b, 0x42, 0xa4, 0x84, 0xd5, 0xc9],
    ),
    guid(
        0x5405c344,
        0xd457,
        0x444e,
        [0xb4, 0xdd, 0x23, 0x66, 0xe4, 0x5a, 0xee, 0x39],
    ),
];

/// Whether `iid` names the device: its first interface or a newer level.
pub(crate) fn is_device(iid: *const Iid) -> bool {
    object::is(iid, &[IID]) || object::is(iid, &LEVEL_IIDS)
}

/// The interface version these layouts were checked against (2.4).
pub const CHECKED_VERSION: u32 = 0x0002_0004;

/// Feature level 12_2, what the console hardware supports.
const HIGHEST_FEATURE_LEVEL: u32 = 0xc200;

#[repr(C)]
pub struct Device {
    object: Object,
    /// GPU address of a null constant buffer; none yet.
    null_constant_buffer: u64,
    /// Byte 0 non-zero: calls the title would otherwise inline come to the
    /// table. The rest is reserved.
    out_of_line: [u32; 16],
}
const _: () = assert!(size_of::<Device>() == 0x58);

/// One device per process, as on the console.
pub(crate) static DEVICE: Device = Device {
    object: Object::new(&FUNCTIONS),
    null_constant_buffer: 0,
    out_of_line: [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
};

pub(crate) fn device() -> This {
    (&raw const DEVICE).cast()
}

/// Device creation: `(adapter, parameters, iid, out)`. The parameters start
/// with the interface version; the rest (ring and scratch sizes, features
/// to leave out) does not matter to us yet.
pub extern "system" fn create(
    _adapter: *const c_void,
    parameters: *const u32,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    if parameters.is_null() || iid.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: checked non-null; the title passes its parameters and an id.
    let (version, iid) = unsafe { (parameters.read(), &*iid) };
    tracing::info!(version = format_args!("{version:#x}"), "D3D12.X device");
    if version != CHECKED_VERSION {
        tracing::warn!(
            version = format_args!("{version:#x}"),
            "device layouts were checked against {CHECKED_VERSION:#x} only"
        );
    }
    if !is_device(iid) {
        tracing::warn!(iid = %guid_text(iid), "device created as an unknown interface");
        return E_NOINTERFACE;
    }
    if out.is_null() {
        return S_FALSE;
    }
    if *iid == LEVEL_IIDS[8] {
        NEWER.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    object::answer(object::DEVICE, iid, Some(device()), out)
}

/// Set when the title creates the device as `ID3D12Device11`. Its lists and
/// queues then have the newer layouts (two more methods each; see
/// `list` and `queue`), although the interface version stays 2.4.
/// shortcut: the creation interface is the only sign of the newer layouts
/// we currently use; look for a better one if a title breaks the rule.
static NEWER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(crate) fn newer() -> bool {
    NEWER.load(std::sync::atomic::Ordering::Relaxed)
}

/// The device, or its DXGI side.
extern "system" fn query_interface(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    let found = if is_device(iid) {
        Some(this)
    } else if object::is(iid, &dxgi::DEVICE_IIDS) {
        Some(dxgi::device())
    } else {
        None
    };
    object::answer(object::DEVICE, iid, found, out)
}

/// What `D3D12ConfigureVirtualMemoryX` takes and fills in.
#[repr(C)]
pub struct VirtualMemory {
    flags: u32,
    /// GPU page table size in 4 MiB pages.
    page_table_pages: u32,
    /// Memory kept across title processes for the frame buffer.
    preserved_bytes: u32,
    /// Out: the range graphics addresses fall in.
    lowest: u64,
    highest: u64,
}
const _: () = assert!(size_of::<VirtualMemory>() == 0x20);

/// `d3d12_xs!D3D12ConfigureVirtualMemoryX`, called before any graphics
/// allocation. The inputs size console structures we do not have; the
/// range out is all of user space, since GPU addresses are CPU addresses.
pub extern "system" fn configure_virtual_memory(config: *mut VirtualMemory) -> HRESULT {
    // SAFETY: the title passes its configuration (null is tolerated).
    let Some(config) = (unsafe { config.as_mut() }) else {
        return E_INVALIDARG;
    };
    tracing::info!(
        flags = config.flags,
        page_table_pages = config.page_table_pages,
        preserved_bytes = config.preserved_bytes,
        "graphics virtual memory"
    );
    (config.lowest, config.highest) = user_space();
    S_OK
}

fn user_space() -> (u64, u64) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
        // SAFETY: plain data, filled in by the call.
        let mut info: SYSTEM_INFO = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer.
        unsafe { GetSystemInfo(&mut info) };
        (
            info.lpMinimumApplicationAddress as u64,
            info.lpMaximumApplicationAddress as u64,
        )
    }
    #[cfg(not(windows))]
    (0x1_0000, 0x7fff_fffe_ffff)
}

extern "system" fn node_count(_: This) -> u32 {
    1
}

/// Feature query 2: the highest of the requested feature levels we support.
#[repr(C)]
struct FeatureLevels {
    count: u32,
    requested: *const u32,
    highest: u32,
}

/// Feature query 0, what an RDNA 2 GPU reports. The console's layout has
/// one field more than PC's (index 11).
const OPTIONS: [u32; 16] = [
    1,  // double-precision shader operations
    1,  // output-merger logic operations
    2,  // minimum precision: 16-bit
    3,  // tiled resources tier
    3,  // resource binding tier
    1,  // stencil reference from pixel shaders
    1,  // typed UAV loads of more formats
    1,  // rasterizer-ordered views
    3,  // conservative rasterization tier
    40, // GPU address bits per resource
    0,  // 64 KiB standard swizzle
    0,  // console-only field
    0,  // cross-node sharing tier
    0,  // cross-adapter row-major textures
    1,  // viewport and render target index from any stage
    2,  // resource heap tier
];

extern "system" fn check_feature_support(
    _: This,
    feature: u32,
    data: *mut c_void,
    size: u32,
) -> HRESULT {
    const OPTIONS_QUERY: u32 = 0;
    const ARCHITECTURE: u32 = 1;
    const FEATURE_LEVELS: u32 = 2;
    const FORMAT_SUPPORT: u32 = 3;
    const MULTISAMPLE: u32 = 4;
    let expected = match feature {
        OPTIONS_QUERY => size_of_val(&OPTIONS),
        ARCHITECTURE => 16,
        FEATURE_LEVELS => size_of::<FeatureLevels>(),
        FORMAT_SUPPORT => 12,
        MULTISAMPLE => 16,
        _ => {
            tracing::warn!(feature, size, "device feature query not implemented");
            return E_NOTIMPL;
        }
    };
    if data.is_null() || size as usize != expected {
        return E_INVALIDARG;
    }
    match feature {
        OPTIONS_QUERY => {
            // SAFETY: checked size and non-null.
            unsafe { data.cast::<[u32; 16]>().write_unaligned(OPTIONS) };
            S_OK
        }
        ARCHITECTURE => {
            // SAFETY: checked size and non-null.
            let a = unsafe { &mut *data.cast::<[u32; 4]>() };
            if a[0] != 0 {
                return E_INVALIDARG; // one node only
            }
            // Not tile-based; unified, cache-coherent memory.
            a[1..].copy_from_slice(&[0, 1, 1]);
            S_OK
        }
        FORMAT_SUPPORT => {
            // SAFETY: checked size and non-null.
            let q = unsafe { &mut *data.cast::<[u32; 3]>() };
            // shortcut: every use of every format; answer from the host
            // device once there is one.
            q[1] = 0x7fff_ffff;
            q[2] = 0xffff;
            S_OK
        }
        MULTISAMPLE => {
            // SAFETY: checked size and non-null.
            let q = unsafe { &mut *data.cast::<[u32; 4]>() };
            // Format, sample count, flags in; one quality level out for the
            // counts the hardware has.
            q[3] = u32::from(matches!(q[1], 1 | 2 | 4 | 8));
            S_OK
        }
        _ => feature_levels(data),
    }
}

fn feature_levels(data: *mut c_void) -> HRESULT {
    // SAFETY: the caller checked size and non-null.
    let query = unsafe { &mut *data.cast::<FeatureLevels>() };
    if query.requested.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: the title passes `count` levels.
    let requested = unsafe { std::slice::from_raw_parts(query.requested, query.count as usize) };
    match requested
        .iter()
        .copied()
        .filter(|&l| l <= HIGHEST_FEATURE_LEVEL)
        .max()
    {
        Some(level) => {
            query.highest = level;
            S_OK
        }
        None => UNSUPPORTED,
    }
}

/// Device methods 0x1c8 and 0x218 (`SLOT`): console
/// `DeserializeGraphicsPipelineStateX` and `DeserializeComputePipelineStateX`.
/// The upscaler logs "DeserializeComputePipelineStateX FAILED hr=offset ...
/// falling back to NewBE" for its passes and calls slot 0x58 of the pipeline
/// it made instead, the matching serialize call ([`object::serialize_calls`]).
/// Titles call both as `(this, description, interface id, out)`, asking for
/// `ID3D12PipelineState`, and on success use the returned object. A description
/// holds no shader container but the parts of one ([`Serialized`]).
/// shortcut: not implemented: the calls fail with `out` null so the title falls
/// back to constructing pipelines through the standard path; the census
/// ([`census_on`]) counts descriptions whose code never came that way. Making
/// them would take containers built from the parts: an FSPX part for compute
/// (`victory_shader::native::fspx_program`), for graphics a metadata layout not
/// known yet. Answering `S_OK` here without creating a pipeline would return
/// uninitialized stack pointers as objects.
extern "system" fn unknown_pipeline<const SLOT: usize>(
    _: This,
    desc: *const u8,
    _iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    use std::sync::atomic::{AtomicU32, Ordering};
    static CALLS: [AtomicU32; 2] = [const { AtomicU32::new(0) }; 2];
    let compute = SLOT == 0x218;
    let calls = CALLS[usize::from(compute)].fetch_add(1, Ordering::Relaxed) + 1;
    if !out.is_null() {
        // SAFETY: the title's out pointer (a local of its own).
        unsafe { *out = std::ptr::null() };
    }
    let log = calls <= 4 || calls.is_power_of_two();
    let census = census_on();
    if !log && !census {
        return E_NOTIMPL;
    }
    // With the census, words enough for eight stage slots of a graphics
    // description (where the memory after it is there), else sixteen as
    // before.
    let description = if census { read_words(desc as usize, 40) } else { None }
        .or_else(|| read_words(desc as usize, 16));
    let parsed = description.as_deref().and_then(|w| serialized(compute, w));
    #[cfg(windows)]
    if census {
        let root = description.as_ref().map_or(0, |w| w[0]);
        census_refused(compute, root, parsed.as_ref());
    }
    if log {
        let first = description.as_ref().and_then(|w| read_words(w[0] as usize, 16));
        let stages = parsed.as_ref().map(|s| s.stages.clone());
        tracing::warn!(
            slot = format_args!("{:#x}", SLOT),
            calls,
            description = format_args!("{description:x?}"),
            first_points_at = format_args!("{first:x?}"),
            stages = format_args!("{stages:x?}"),
            "a pipeline creation method that is not implemented: the title goes without this pipeline"
        );
    }
    E_NOTIMPL
}

/// `n` 64-bit words at `at`, read through the kernel: a description's
/// pointers are not known to be mapped.
fn read_words(at: usize, n: usize) -> Option<Vec<u64>> {
    #[cfg(windows)]
    {
        let mut bytes = vec![0u8; 8 * n];
        (at != 0 && crate::host::read_into(at as *const u8, &mut bytes)).then(|| {
            bytes.as_chunks::<8>().0.iter().map(|c| u64::from_le_bytes(*c)).collect()
        })
    }
    #[cfg(not(windows))]
    {
        let _ = (at, n);
        None
    }
}

/// One stage slot of a serialized pipeline's description: its code and its
/// metadata, a pointer and a size each (`meta` is `None` where the words
/// read end before its size).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SerializedStage {
    code: (u64, u64),
    meta: Option<(u64, u64)>,
}

/// What a description given to device method 0x1c8 or 0x218 holds (same
/// layout across both console profiles). Word 0 is our root signature
/// (the words it points at are its table, then its blob's `Vec`). Words 1 to 4
/// are two pointer and size pairs of the pipeline's state; for compute 0x1c
/// and 0x14 bytes, the sizes of an FSPX part's registers and pipeline
/// metadata. Then come stage slots of four words: code aligned to 256 bytes,
/// size (a multiple of 64 bytes), metadata pointer and metadata size. A
/// compute description has one stage, then two pointers and a 64-bit hash
/// (words 9 to 11), the next description right after at word 12; graphics
/// descriptions have stage slots at words 5, 9, and 13+ (empty = four zero
/// words).
#[derive(Debug, PartialEq, Eq)]
struct Serialized {
    root: u64,
    state: [(u64, u64); 2],
    /// By slot; `None` an empty slot (none after the last stage).
    stages: Vec<Option<SerializedStage>>,
    /// Word 11 of a compute description: the same for the same pipeline on
    /// both console profiles, and from run to run.
    hash: Option<u64>,
}

/// The most stage slots read from a graphics description.
const STAGE_SLOTS: usize = 8;

/// The description in the words `w` read at it (16 or more), or `None` where
/// they are not one as described at [`Serialized`]: no root signature, a
/// state piece of no size or of more than 64 KiB, a compute stage missing,
/// or no stage at all. A graphics description's slots are read up to the
/// first group of four words that is neither empty nor a stage: code not at
/// a 256-byte boundary, of no size, not in whole dwords or of more than 4
/// MiB, or metadata of no size or of more than 1 MiB (what follows the last
/// slot, pointers and a hash in a compute one, is none of these).
fn serialized(compute: bool, w: &[u64]) -> Option<Serialized> {
    let at = |i: usize| w.get(i).copied();
    let pair = |i: usize, most: u64| -> Option<(u64, u64)> {
        let (p, n) = (at(i)?, at(i + 1)?);
        (p != 0 && (1..=most).contains(&n)).then_some((p, n))
    };
    let root = at(0).filter(|&r| r != 0)?;
    let state = [pair(1, 1 << 16)?, pair(3, 1 << 16)?];
    // `None`: not a slot; `Some(None)`: an empty one.
    let slot = |i: usize| -> Option<Option<SerializedStage>> {
        let code = (at(i)?, at(i + 1)?);
        let (meta, size) = (at(i + 2), at(i + 3));
        if code == (0, 0) {
            return [meta, size].iter().all(|w| w.is_none_or(|w| w == 0)).then_some(None);
        }
        if code.0 % 256 != 0 || code.1 % 4 != 0 || !(1..=4 << 20).contains(&code.1) {
            return None;
        }
        let meta = match (meta, size) {
            (Some(m), Some(n)) if m != 0 && (1..=1 << 20).contains(&n) => Some((m, n)),
            (Some(m), None) if m != 0 => None,
            (None, None) => None,
            _ => return None,
        };
        Some(Some(SerializedStage { code, meta }))
    };
    let stages = if compute {
        vec![Some(slot(5)??)]
    } else {
        let mut stages: Vec<_> = (0..STAGE_SLOTS).map_while(|k| slot(5 + 4 * k)).collect();
        while stages.last() == Some(&None) {
            stages.pop();
        }
        stages
    };
    if stages.is_empty() {
        return None;
    }
    Some(Serialized { root, state, stages, hash: if compute { at(11) } else { None } })
}

/// `VICTORY_SERIALIZED_CENSUS=0`: device methods 0x1c8 and 0x218 log as
/// before (sixteen words of the first calls) and the normal creation path is
/// not looked at. Else (the default), read once: the code of every refused
/// description ([`Serialized`]) is hashed as normal shaders are (BLAKE3),
/// every shader container the normal creation path is given is hashed, and
/// so is the code of each console program in it, once per container; once
/// the title has created no pipeline and been refused none for ten seconds,
/// a line counts the refused descriptions none of whose code came through
/// the normal path since (those the title went without), those of which some
/// did, and those of which all did. That a refused one is made the normal
/// way at once is counted apart (`followed`: on the same thread, with the
/// same root signature, next), since the upscaler's other compiler may make
/// other code for the same pass.
fn census_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SERIALIZED_CENSUS").is_none_or(|v| v != "0"))
}

/// A refused description, by its stages' code.
struct Refused {
    compute: bool,
    /// [`Serialized::hash`].
    hash: Option<u64>,
    /// Each stage's code's BLAKE3, in slot order.
    code: Vec<[u8; 32]>,
}

/// What the census has seen ([`census_on`]).
#[derive(Default)]
struct Census {
    /// Containers given to the normal creation path, each looked at once:
    /// by the digest in their header and their size ([`fingerprint`]).
    looked: std::collections::HashSet<([u8; 16], usize)>,
    /// The BLAKE3 of each of those containers and of each console program's
    /// code in them.
    seen: std::collections::HashSet<[u8; 32]>,
    /// Refused descriptions, by the BLAKE3 of their stages' hashes.
    refused: std::collections::HashMap<[u8; 32], Refused>,
    /// Calls of 0x1c8 and of 0x218.
    calls: [u64; 2],
    /// Calls whose description did not read as one, or whose code did not.
    unread: u64,
    /// Refusals the same thread followed with a normal creation with the
    /// same root signature.
    followed: u64,
    /// When the title last created a pipeline or was refused one.
    last: Option<std::time::Instant>,
    /// Something to report since the last report.
    dirty: bool,
    /// Containers not looked at because the census's thread had
    /// [`MAILBOX_SLOTS`] waiting (the title never waits for it).
    behind: u64,
}

static CENSUS: std::sync::Mutex<Option<Census>> = std::sync::Mutex::new(None);

/// The most containers waiting for the census's thread ([`census_mailbox`]).
const MAILBOX_SLOTS: usize = 64;

fn with_census<T>(f: impl FnOnce(&mut Census) -> T) -> T {
    let mut census = CENSUS.lock().unwrap_or_else(|e| e.into_inner());
    f(census.get_or_insert_with(Census::default))
}

thread_local! {
    /// The root signature of this thread's last refused description, until
    /// its next creation of a pipeline.
    static FOLLOW: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// A container's key in [`Census::looked`]: the digest its header holds and
/// its size, or (no digest) the first half of its BLAKE3.
fn fingerprint(container: &[u8]) -> ([u8; 16], usize) {
    let digest: Option<[u8; 16]> = container
        .get(4..20)
        .filter(|_| container.starts_with(b"DXBC"))
        .and_then(|d| d.try_into().ok())
        .filter(|d| *d != [0; 16]);
    let key = digest.unwrap_or_else(|| {
        let full = blake3::hash(container);
        let mut half = [0; 16];
        half.copy_from_slice(&full.as_bytes()[..16]);
        half
    });
    (key, container.len())
}

/// The hashes a container given to the normal creation path adds to
/// [`Census::seen`]: its own, and its console programs' code's.
fn container_hashes(container: &[u8]) -> Vec<[u8; 32]> {
    use victory_shader::{Container, FourCC};
    let mut hashes = vec![*blake3::hash(container).as_bytes()];
    if let Ok(c) = Container::parse(container)
        && let Some(part) = c.part(FourCC::DXIL).or(c.part(FourCC::ACCC))
        && let Ok(programs) = victory_shader::native_programs(part.data)
    {
        hashes.extend(programs.iter().map(|p| *blake3::hash(&p.code).as_bytes()));
    }
    hashes
}

/// Of the refused descriptions, by kind (graphics, compute): how many, and
/// how many of them have all of their stages' code in `seen`, some, none.
fn tally<'a>(
    refused: impl IntoIterator<Item = &'a Refused>,
    seen: &std::collections::HashSet<[u8; 32]>,
) -> [[u64; 4]; 2] {
    let mut counts = [[0; 4]; 2];
    for r in refused {
        let found = r.code.iter().filter(|h| seen.contains(*h)).count();
        let c = &mut counts[usize::from(r.compute)];
        c[0] += 1;
        c[if found == r.code.len() { 1 } else if found > 0 { 2 } else { 3 }] += 1;
    }
    counts
}

impl Census {
    fn report(&self) {
        let [graphics, compute] = tally(self.refused.values(), &self.seen);
        let says = |calls: u64, [distinct, all, some, none]: [u64; 4]| {
            format!("{calls} calls, {distinct} distinct: {all} with all their code seen, {some} some, {none} none")
        };
        let mut never: Vec<(&[u8; 32], &Refused)> =
            self.refused.iter().filter(|(_, r)| r.code.iter().all(|h| !self.seen.contains(h))).collect();
        never.sort_by_key(|(key, _)| **key);
        let examples: Vec<String> = never
            .iter()
            .take(4)
            .map(|(_, r)| match r.hash {
                Some(hash) => format!("compute {hash:016x}"),
                None => format!("graphics code {}", blake3::Hash::from(r.code[0]).to_hex()[..16].to_owned()),
            })
            .collect();
        tracing::info!(
            graphics = %says(self.calls[0], graphics),
            compute = %says(self.calls[1], compute),
            never_arrived = graphics[3] + compute[3],
            followed = self.followed,
            unread = self.unread,
            behind = self.behind,
            containers = self.looked.len(),
            serialize_calls = object::serialize_calls(),
            examples = ?examples,
            "serialized pipelines refused: how many never came the normal way (VICTORY_SERIALIZED_CENSUS=0: off)"
        );
    }
}

/// The census's thread: hashes what the normal creation path was given (a
/// container's console programs are unpacked), and reports ([`census_on`]).
fn census_mailbox() -> Option<&'static std::sync::mpsc::SyncSender<Vec<u8>>> {
    use std::sync::mpsc::RecvTimeoutError;
    static MAILBOX: std::sync::OnceLock<Option<std::sync::mpsc::SyncSender<Vec<u8>>>> = std::sync::OnceLock::new();
    const QUIET: std::time::Duration = std::time::Duration::from_secs(10);
    MAILBOX
        .get_or_init(|| {
            // Few containers waiting, so copies do not pile up; what finds
            // no room is not looked at ([`census_created`]).
            let (send, receive) = std::sync::mpsc::sync_channel::<Vec<u8>>(MAILBOX_SLOTS);
            let spawned = std::thread::Builder::new().name("victory-census".into()).spawn(move || {
                loop {
                    match receive.recv_timeout(QUIET) {
                        Ok(container) => {
                            let hashes = container_hashes(&container);
                            with_census(|c| {
                                c.seen.extend(hashes);
                                c.dirty = true;
                            });
                        }
                        Err(RecvTimeoutError::Timeout) => with_census(|c| {
                            if c.dirty && c.calls != [0; 2] && c.last.is_some_and(|t| t.elapsed() >= QUIET) {
                                c.dirty = false;
                                c.report();
                            }
                        }),
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
            });
            spawned
                .inspect_err(|e| tracing::warn!(error = %e, "no thread for the census of serialized pipelines"))
                .ok()?;
            Some(send)
        })
        .as_ref()
}

/// The census of a refused description: its stages' code, read and hashed.
#[cfg(windows)]
fn census_refused(compute: bool, root: u64, parsed: Option<&Serialized>) {
    FOLLOW.with(|f| f.set(root));
    let code: Option<Vec<[u8; 32]>> = parsed.and_then(|s| {
        s.stages
            .iter()
            .flatten()
            .map(|stage| {
                let (at, size) = stage.code;
                let mut bytes = vec![0u8; size as usize];
                crate::host::read_into(at as *const u8, &mut bytes).then(|| *blake3::hash(&bytes).as_bytes())
            })
            .collect()
    });
    census_mailbox();
    with_census(|c| {
        c.calls[usize::from(compute)] += 1;
        c.last = Some(std::time::Instant::now());
        c.dirty = true;
        let Some(code) = code else {
            c.unread += 1;
            return;
        };
        let mut key = blake3::Hasher::new();
        key.update(&[u8::from(compute)]);
        for h in &code {
            key.update(h);
        }
        let hash = parsed.and_then(|s| s.hash);
        c.refused.entry(*key.finalize().as_bytes()).or_insert(Refused { compute, hash, code });
    });
}

/// The census of a normal creation: `code` holds each shader's container
/// (pointer and size; null for a stage left out); new ones are copied to the
/// census's thread.
#[cfg(windows)]
fn census_created(root: usize, code: &[(usize, usize)]) {
    let followed = root != 0 && FOLLOW.with(|f| f.replace(0)) == root as u64;
    let containers: Vec<(&[u8], ([u8; 16], usize))> = code
        .iter()
        .filter(|&&(at, size)| at != 0 && size != 0)
        .map(|&(at, size)| {
            // SAFETY: the title passes `size` bytes of shader code (as `pipeline::copy` reads them).
            let bytes = unsafe { std::slice::from_raw_parts(at as *const u8, size) };
            (bytes, fingerprint(bytes))
        })
        .collect();
    let mailbox = census_mailbox();
    let fresh: Vec<&[u8]> = with_census(|c| {
        c.last = Some(std::time::Instant::now());
        if followed {
            c.followed += 1;
            c.dirty = true;
        }
        containers.into_iter().filter(|(_, key)| c.looked.insert(*key)).map(|(bytes, _)| bytes).collect()
    });
    if let Some(mailbox) = mailbox {
        for bytes in fresh {
            // Never a wait on the title's creating thread (loads create
            // pipelines in bursts of thousands): a container the census's
            // thread has no room for is counted, and looked at the next
            // time one like it is given.
            if mailbox.try_send(bytes.to_vec()).is_err() {
                with_census(|c| {
                    c.looked.remove(&fingerprint(bytes));
                    c.behind += 1;
                });
            }
        }
    }
}

/// The subobject sizes of a pipeline stream, as `pipeline::create_stream`
/// walks it (`pipeline::subobject`, repeated here for the census: keep the
/// two alike).
fn subobject_size(kind: u32) -> Option<usize> {
    Some(match kind {
        1..=6 | 23 | 24 | 12 | 19 => 24,
        0 | 17 => 16,
        7 | 15 => 40,
        8 => 336,
        10 => 48,
        11 => 56,
        21 => 64,
        22 => 32,
        9 | 13 | 14 | 16 | 18 | 20 => 8,
        _ => return None,
    })
}

/// The root signature and each shader's code (pointer and size) of the
/// `size` bytes of stream at `stream`, to the first subobject of a kind not
/// known (as `pipeline::create_stream`).
/// # Safety
/// `stream` holds `size` bytes.
unsafe fn stream_shaders(stream: *const u8, size: usize) -> (usize, Vec<(usize, usize)>) {
    // SAFETY: inside the stream (the caller's promise).
    let word = |at: usize| unsafe { stream.add(at).cast::<usize>().read_unaligned() };
    let (mut root, mut code, mut at) = (0, Vec::new(), 0);
    while at + 8 <= size {
        // SAFETY: as above.
        let kind = unsafe { stream.add(at).cast::<u32>().read_unaligned() };
        let Some(len) = subobject_size(kind) else {
            break;
        };
        match kind {
            0 if at + 16 <= size => root = word(at + 8),
            1..=6 | 23 | 24 if at + 24 <= size => code.push((word(at + 8), word(at + 16))),
            _ => {}
        }
        at += len;
    }
    (root, code)
}

/// [`pipeline::create_graphics`], for the census first ([`census_on`]).
extern "system" fn census_graphics(
    this: This,
    desc: *const pipeline::GraphicsHead,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    #[cfg(windows)]
    if census_on() {
        // SAFETY: the title passes a description (null is tolerated): the root
        // signature, then five stages' code and size (`pipeline::GraphicsHead`).
        if let Some(w) = unsafe { desc.cast::<[usize; 11]>().as_ref() } {
            census_created(w[0], &[(w[1], w[2]), (w[3], w[4]), (w[5], w[6]), (w[7], w[8]), (w[9], w[10])]);
        }
    }
    pipeline::create_graphics(this, desc, iid, out)
}

/// [`pipeline::create_compute`], for the census first ([`census_on`]).
extern "system" fn census_compute(
    this: This,
    desc: *const pipeline::ComputeHead,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    #[cfg(windows)]
    census_compute_head(desc);
    pipeline::create_compute(this, desc, iid, out)
}

/// [`pipeline::create_compute_x`], for the census first ([`census_on`]).
extern "system" fn census_compute_x(
    this: This,
    desc: *const pipeline::ComputeHead,
    extensions: u32,
    extended: *const u8,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    #[cfg(windows)]
    census_compute_head(desc);
    pipeline::create_compute_x(this, desc, extensions, extended, iid, out)
}

#[cfg(windows)]
fn census_compute_head(desc: *const pipeline::ComputeHead) {
    if census_on() {
        // SAFETY: the title passes a description (null is tolerated): the root
        // signature, then the stage's code and size (`pipeline::ComputeHead`).
        if let Some(w) = unsafe { desc.cast::<[usize; 3]>().as_ref() } {
            census_created(w[0], &[(w[1], w[2])]);
        }
    }
}

/// [`pipeline::create_stream`], for the census first ([`census_on`]).
extern "system" fn census_stream(
    this: This,
    desc: *const pipeline::StreamDesc,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    #[cfg(windows)]
    if census_on() {
        // SAFETY: the title passes a description (null is tolerated): the
        // stream's size and where it is (`pipeline::StreamDesc`).
        if let Some(&[size, at]) = unsafe { desc.cast::<[usize; 2]>().as_ref() }
            && at != 0
        {
            // SAFETY: the title's stream of `size` bytes, as `pipeline::create_stream` reads it.
            let (root, code) = unsafe { stream_shaders(at as *const u8, size) };
            census_created(root, &code);
        }
    }
    pipeline::create_stream(this, desc, iid, out)
}

/// The host device's state: the console's device is removed when the host's is
/// (VIctory 13185f5: the default answer, E_NOTIMPL, reads as a lost device).
extern "system" fn removed_reason(_: This) -> HRESULT {
    #[cfg(windows)]
    if let Some(host) = crate::host::host() {
        // SAFETY: a plain query.
        if let Err(e) = unsafe { host.device.GetDeviceRemovedReason() } {
            return e.code().0;
        }
    }
    S_OK
}

/// IUnknown 3, object methods 4, device methods 76 (0x18-0x290), then the
/// methods `ID3D12Device1` to `ID3D12Device8` add, in order: 3, 1, 3, 6, 9,
/// 1, 2, 9 (0x298-0x3a0, the console's own among them), then those of
/// `ID3D12Device9` to `ID3D12Device11`, whose console layout is unknown.
/// Every slot exists so that a call to one we have not written logs it.
static FUNCTIONS: Table<160> = {
    let mut f = object::d3d12_object(object::missing_table::<{ object::DEVICE }, 160>());
    f[0] = query_interface as *const (); // 0x00
    f[7] = node_count as *const (); // 0x38
    f[8] = queue::create as *const (); // 0x40
    f[9] = list::create_allocator as *const (); // 0x48
    f[12] = list::create_list as *const (); // 0x60
    f[10] = census_graphics as *const (); // 0x50
    f[11] = census_compute as *const (); // 0x58
    f[13] = check_feature_support as *const (); // 0x68
    f[14] = descriptor::create_heap as *const (); // 0x70
    f[15] = descriptor::increment_size as *const (); // 0x78
    f[16] = root::create as *const (); // 0x80
    f[17] = descriptor::create_cbv as *const (); // 0x88
    f[18] = descriptor::create_srv as *const (); // 0x90
    f[19] = descriptor::create_uav as *const (); // 0x98
    f[20] = descriptor::create_rtv as *const (); // 0xa0
    f[21] = descriptor::create_dsv as *const (); // 0xa8
    f[22] = descriptor::create_sampler as *const (); // 0xb0
    f[23] = descriptor::copy as *const (); // 0xb8
    f[24] = descriptor::copy_simple as *const (); // 0xc0
    f[25] = resource::allocation_info as *const (); // 0xc8
    f[26] = resource::custom_heap_properties as *const (); // 0xd0
    f[27] = resource::create_committed as *const (); // 0xd8
    f[28] = resource::create_heap as *const (); // 0xe0
    f[29] = resource::create_placed as *const (); // 0xe8
    f[36] = fence::create as *const (); // 0x120
    f[37] = removed_reason as *const (); // 0x128
    f[38] = resource::copyable_footprints as *const (); // 0x130
    f[39] = query::create as *const (); // 0x138
    f[41] = signature::create as *const (); // 0x148
    f[48] = queue::create_x as *const (); // 0x180
    f[57] = unknown_pipeline::<0x1c8> as *const (); // 0x1c8
    f[61] = descriptor::create_sampler_x as *const (); // 0x1e8
    f[62] = census_compute_x as *const (); // 0x1f0
    f[67] = unknown_pipeline::<0x218> as *const (); // 0x218
    f[69] = object::ignore as *const (); // 0x228 debug error filter
    f[70] = descriptor::create_placed_srv as *const (); // 0x230
    f[71] = descriptor::create_placed_uav as *const (); // 0x238
    f[73] = list::create_list_x as *const (); // 0x248
    f[74] = resource::register_page_pool as *const (); // 0x250
    f[75] = resource::unregister_page_pool as *const (); // 0x258
    // CreateCommittedResourceX: its description is PC's plus DCC options,
    // which we have no use for.
    f[76] = resource::create_committed as *const (); // 0x260
    f[78] = frame::set_interval as *const (); // 0x270
    f[79] = frame::schedule as *const (); // 0x278
    f[80] = frame::wait as *const (); // 0x280
    f[81] = frame::statistics as *const (); // 0x288
    f[86] = census_stream as *const (); // 0x2b0
    f[108] = resource::allocation_info2 as *const (); // 0x360
    f[127] = resource::create_committed3 as *const (); // 0x3f8
    f[133] = resource::create_at_address as *const (); // 0x428
    Table(f)
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::slot;

    fn create_as(iid: &Iid) -> (HRESULT, This) {
        let version = CHECKED_VERSION;
        let mut device = std::ptr::null();
        let hr = create(std::ptr::null(), &version, iid, &mut device);
        (hr, device)
    }

    #[test]
    fn creation_hands_out_the_device() {
        let (hr, device) = create_as(&IID);
        assert_eq!(hr, S_OK);
        assert_eq!(device, super::device());
        // The title tests byte 0x18 before inlining a device call.
        // SAFETY: the device is 0x58 bytes.
        assert_ne!(unsafe { device.cast::<u8>().add(0x18).read() }, 0);
        assert_eq!(create_as(&[0; 16]).0, E_NOINTERFACE);
        assert_eq!(
            slot::<extern "system" fn(This) -> u32>(device, 0x38)(device),
            1
        );
        let missing = slot::<extern "system" fn() -> HRESULT>(device, 0xf0);
        assert_eq!(missing(), E_NOTIMPL);
    }

    #[test]
    fn newer_device_levels_are_the_same_device() {
        // ID3D12Device11, as the title asks for it.
        let (hr, device) = create_as(&LEVEL_IIDS[8]);
        assert_eq!((hr, device), (S_OK, super::device()));
        let mut same = std::ptr::null();
        assert_eq!(query_interface(device, &LEVEL_IIDS[4], &mut same), S_OK);
        assert_eq!(same, device);
        // A Device8 method we have not written fails without crashing.
        let missing = slot::<extern "system" fn() -> HRESULT>(device, 0x3a0);
        assert_eq!(missing(), E_NOTIMPL);
    }

    /// A device child, and the DXGI device, hand out the device as any
    /// interface the device itself answers to.
    #[test]
    fn the_device_is_found_at_every_level_through_others() {
        type Get = extern "system" fn(This, *const Iid, *mut This) -> HRESULT;
        let fence_iid = &crate::fence::FENCE_IIDS[0];
        let mut fence = std::ptr::null();
        assert_eq!(
            crate::fence::create(std::ptr::null(), 0, 0, fence_iid, &mut fence),
            S_OK
        );
        let get_device = slot::<Get>(fence, 0x38);
        let dxgi = dxgi::device();
        let query = slot::<Get>(dxgi, 0);
        for iid in std::iter::once(&IID).chain(&LEVEL_IIDS) {
            let mut found = std::ptr::null();
            assert_eq!(get_device(fence, iid, &mut found), S_OK, "{iid:02x?}");
            assert_eq!(found, device());
            let mut found = std::ptr::null();
            assert_eq!(query(dxgi, iid, &mut found), S_OK, "{iid:02x?}");
            assert_eq!(found, device());
        }
        // What the device is not, it is not through them either.
        let mut found = device();
        assert_eq!(get_device(fence, fence_iid, &mut found), E_NOINTERFACE);
        assert!(found.is_null());
        slot::<extern "system" fn(This) -> u32>(fence, 0x10)(fence);
    }

    /// The host's: a fence's `GetDevice` for `ID3D12Device5`.
    #[cfg(windows)]
    #[test]
    fn the_hosts_children_hand_out_newer_device_levels() {
        use windows::Win32::Graphics::Direct3D12::{
            D3D12_FENCE_FLAG_NONE, ID3D12Device5, ID3D12Fence,
        };
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        // SAFETY: plain calls on the host device and its fence.
        unsafe {
            let fence: ID3D12Fence = host.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            let mut device: Option<ID3D12Device5> = None;
            assert!(fence.GetDevice(&mut device).is_ok());
            assert!(device.is_some());
        }
    }

    fn stage(code: (u64, u64), meta: Option<(u64, u64)>) -> SerializedStage {
        SerializedStage { code, meta }
    }

    /// Synthetic descriptor layouts exercising compute (`0x218`) and graphics (`0x1c8`).
    #[test]
    fn serialized_descriptions_read_as_the_logs_show_them() {
        // Compute (one stage, root signature, state spans, metadata pointers, and hash).
        let first = [
            0x1000_0001_0000, 0x2000_0001_0000, 0x1c, 0x2000_0001_0020, 0x14, 0x3000_0002_0000, 0x180,
            0x2000_0001_0040, 0x118, 0x2000_0003_0000, 0x2000_0003_0100, 0x1111_2222_3333_4444,
            0x1000_0001_0000, 0x2000_0001_0000, 0x1c, 0x2000_0001_0200,
        ];
        assert_eq!(
            serialized(true, &first),
            Some(Serialized {
                root: 0x1000_0001_0000,
                state: [(0x2000_0001_0000, 0x1c), (0x2000_0001_0020, 0x14)],
                stages: vec![Some(stage((0x3000_0002_0000, 0x180), Some((0x2000_0001_0040, 0x118))))],
                hash: Some(0x1111_2222_3333_4444),
            })
        );
        // Compute (large shader stage and synthetic hash).
        let w = [
            0x1000_0002_0000, 0x2000_0004_0000, 0x1c, 0x2000_0004_0020, 0x14, 0x3000_0005_0000, 0x1f9c0,
            0x2000_0004_0040, 0x1798, 0x2000_0006_0000, 0x2000_0006_0100, 0x5555_6666_7777_8888, 0,
            0x2000_0004_0200, 0x1c, 0x2000_0004_0400,
        ];
        let s = serialized(true, &w).unwrap();
        assert_eq!(s.stages, [Some(stage((0x3000_0005_0000, 0x1f9c0), Some((0x2000_0004_0040, 0x1798))))]);
        assert_eq!(s.hash, Some(0x5555_6666_7777_8888));
        // Graphics (two stages, third slot empty).
        let w = [
            0x1000_0007_0000, 0x2000_0007_0000, 0x184, 0x2000_0007_0200, 0xbc, 0x3000_0008_0000, 0x200,
            0x2000_0007_0300, 0xd0, 0x3000_0008_0200, 0x200, 0x2000_0007_0400, 0xb0, 0, 0, 0,
        ];
        let s = serialized(false, &w).unwrap();
        assert_eq!((s.root, s.state, s.hash), (0x1000_0007_0000, [(0x2000_0007_0000, 0x184), (0x2000_0007_0200, 0xbc)], None));
        assert_eq!(
            s.stages,
            [
                Some(stage((0x3000_0008_0000, 0x200), Some((0x2000_0007_0300, 0xd0)))),
                Some(stage((0x3000_0008_0200, 0x200), Some((0x2000_0007_0400, 0xb0)))),
            ]
        );
        // Graphics (first slot empty).
        let w = [
            0x1000_0008_0000, 0x2000_0008_0000, 0x17c, 0x2000_0008_0200, 0xbc, 0, 0, 0, 0, 0x3000_0009_0000, 0x140,
            0x2000_0008_0400, 0x30, 0, 0, 0,
        ];
        let only = stage((0x3000_0009_0000, 0x140), Some((0x2000_0008_0400, 0x30)));
        assert_eq!(serialized(false, &w).unwrap().stages, [None, Some(only)]);
        // Graphics (middle slot empty, third stage metadata size truncated past input slice).
        let w = [
            0x1000_0009_0000, 0x2000_0009_0000, 0x200, 0x2000_0009_0200, 0x50, 0x3000_000a_0500, 0x4a40,
            0x2000_0009_0400, 0x5b0, 0, 0, 0, 0, 0x3000_000a_5000, 0xd80, 0x2000_0009_0a00,
        ];
        assert_eq!(
            serialized(false, &w).unwrap().stages,
            [
                Some(stage((0x3000_000a_0500, 0x4a40), Some((0x2000_0009_0400, 0x5b0)))),
                None,
                Some(stage((0x3000_000a_5000, 0xd80), None)),
            ]
        );
        // Read as graphics, a compute description's slots end at its two
        // pointers and hash.
        assert_eq!(serialized(false, &first).unwrap().stages, serialized(true, &first).unwrap().stages);
        // Not descriptions: no root signature, a state piece of no size, too
        // few words, code off a 256-byte boundary, a compute stage empty.
        let not = |at: usize, v: u64| {
            let mut w = first;
            w[at] = v;
            serialized(true, &w)
        };
        assert_eq!(not(0, 0), None);
        assert_eq!(not(2, 0), None);
        assert_eq!(not(5, 0x3000_0002_0010), None);
        assert_eq!(not(6, 0x182), None);
        assert_eq!(not(8, 2 << 20), None);
        assert_eq!(serialized(true, &first[..6]), None);
        let mut empty = first;
        empty[5..9].fill(0);
        assert_eq!(serialized(true, &empty), None);
        assert_eq!(serialized(false, &empty), None);
    }

    #[test]
    fn the_census_counts_descriptions_by_how_much_of_their_code_came() {
        let h = |n: u8| [n; 32];
        let seen: std::collections::HashSet<[u8; 32]> = [h(1), h(2), h(3)].into();
        let refused = [
            Refused { compute: true, hash: Some(1), code: vec![h(1)] },
            Refused { compute: true, hash: Some(2), code: vec![h(9)] },
            Refused { compute: false, hash: None, code: vec![h(2), h(3)] },
            Refused { compute: false, hash: None, code: vec![h(2), h(8)] },
            Refused { compute: false, hash: None, code: vec![h(7), h(8)] },
        ];
        // How many, then all of their code seen, some, none.
        assert_eq!(tally(&refused, &seen), [[3, 1, 1, 1], [2, 1, 0, 1]]);
    }

    /// The stream layout of `pipeline`'s test of streams (the title's).
    #[test]
    fn streams_yield_their_root_signature_and_shaders() {
        let (ms, ps) = ([9u8; 7], [4u8; 5]);
        let mut stream = vec![0u8; 0x2b0];
        let mut put = |at: usize, v: u64| stream[at..at + 8].copy_from_slice(&v.to_le_bytes());
        for (at, kind) in [
            (0x10, 24),
            (0x28, 2),
            (0x40, 3),
            (0x58, 4),
            (0x70, 5),
            (0x88, 8),
            (0x1d8, 9),
            (0x1e0, 10),
            (0x210, 21),
            (0x250, 17),
            (0x260, 12),
            (0x278, 14),
            (0x280, 15),
            (0x2a8, 16),
        ] {
            put(at, kind);
        }
        put(0x08, 0x5eed);
        put(0x18, ms.as_ptr() as u64);
        put(0x20, ms.len() as u64);
        put(0x30, ps.as_ptr() as u64);
        put(0x38, ps.len() as u64);
        // SAFETY: the stream's own bytes.
        let (root, code) = unsafe { stream_shaders(stream.as_ptr(), stream.len()) };
        assert_eq!(root, 0x5eed);
        assert_eq!(code, [(ms.as_ptr() as usize, 7), (ps.as_ptr() as usize, 5), (0, 0), (0, 0), (0, 0)]);
        // A kind not known ends the walk, as `pipeline::create_stream`'s.
        stream[0x28..0x2c].copy_from_slice(&99u32.to_le_bytes());
        // SAFETY: as above.
        let (_, code) = unsafe { stream_shaders(stream.as_ptr(), stream.len()) };
        assert_eq!(code, [(ms.as_ptr() as usize, 7)]);
    }

    #[test]
    fn containers_are_known_by_their_digest_or_their_bytes() {
        let mut c = vec![0u8; 64];
        c[..4].copy_from_slice(b"DXBC");
        c[4..20].copy_from_slice(&[7; 16]);
        assert_eq!(fingerprint(&c), ([7; 16], 64));
        // No digest: by content.
        c[4..20].fill(0);
        let (key, size) = fingerprint(&c);
        assert_eq!((&key[..], size), (&blake3::hash(&c).as_bytes()[..16], 64));
        assert_eq!(fingerprint(b"code").0[..], blake3::hash(b"code").as_bytes()[..16]);
    }

    /// A container with an `ACCC` part holding `code` as an FSPX program
    /// (`victory_shader::native::fspx_program`'s layout).
    fn fspx_container(code: &[u8]) -> Vec<u8> {
        let put = |to: &mut Vec<u8>, words: &[u32]| words.iter().for_each(|w| to.extend_from_slice(&w.to_le_bytes()));
        // Registers: 64 threads of 32 lanes.
        let mut registers = vec![0u8; 0x1c];
        registers[0x0c..0x12].copy_from_slice(&[64, 0, 1, 0, 1, 0]);
        registers[0x12] = 1 << 4;
        // Program metadata: its header, then the last record (the hash).
        let mut metadata = Vec::new();
        put(&mut metadata, &[0x403, 0x38 - 0x14, code.len() as u32 / 4, 0, 0, 0x40]);
        metadata.resize(0x38, 0);
        metadata[0x18] = 0x43;
        let first = 0x10 + 3 * 16;
        let mut part = b"FSPX".to_vec();
        let size = (first + registers.len() + code.len() + metadata.len()) as u32;
        put(&mut part, &[1, 3, size]);
        put(&mut part, &[0, first as u32, 0x1c, 0]);
        put(&mut part, &[6, (first + 0x1c) as u32, code.len() as u32, 0]);
        put(&mut part, &[7, (first + 0x1c + code.len()) as u32, metadata.len() as u32, 0]);
        part.extend_from_slice(&registers);
        part.extend_from_slice(code);
        part.extend_from_slice(&metadata);
        let mut c = b"DXBC".to_vec();
        c.extend_from_slice(&[3; 16]);
        put(&mut c, &[1, (0x20 + 4 + 8 + part.len()) as u32, 1, 0x24]);
        c.extend_from_slice(b"ACCC");
        put(&mut c, &[part.len() as u32]);
        c.extend_from_slice(&part);
        c
    }

    #[test]
    fn containers_add_their_own_hash_and_their_programs_code() {
        let code: Vec<u8> = (0..64).collect();
        let c = fspx_container(&code);
        assert_eq!(container_hashes(&c), [*blake3::hash(&c).as_bytes(), *blake3::hash(&code).as_bytes()]);
        // Not a container: its own hash only.
        assert_eq!(container_hashes(&code), [*blake3::hash(&code).as_bytes()]);
    }

    /// Through the device's table: a refused compute description, then its
    /// code made the normal way at once.
    #[cfg(windows)]
    #[test]
    fn refused_descriptions_are_counted_with_what_came_the_normal_way() {
        type Create = extern "system" fn(This, *const u64, *const Iid, *mut This) -> HRESULT;
        let pipeline = guid(0x765a30f3, 0xf624, 0x4c6f, [0xa8, 0x28, 0xac, 0xe9, 0x48, 0x62, 0x24, 0x45]);
        let device = device();
        // Code at a 256-byte boundary, as in the title's heap of them.
        let code: Vec<u8> = (0..64).map(|i: u8| i ^ 0x5a).collect();
        let mut heap = vec![0u8; 512];
        let at = heap.as_ptr().align_offset(256);
        heap[at..at + 64].copy_from_slice(&code);
        let (registers, metadata, program) = ([0u8; 0x1c], [0u8; 0x14], [0u8; 0x38]);
        let (root, hash) = (0x5eed_0218, 0x7e57_c0de_0000_0218);
        let mut w = [0u64; 40];
        w[..12].copy_from_slice(&[
            root,
            registers.as_ptr() as u64,
            0x1c,
            metadata.as_ptr() as u64,
            0x14,
            heap.as_ptr().wrapping_add(at) as u64,
            64,
            program.as_ptr() as u64,
            0x38,
            1,
            2,
            hash,
        ]);
        let mut out = device;
        assert_eq!(slot::<Create>(device, 0x218)(device, w.as_ptr(), &pipeline, &mut out), E_NOTIMPL);
        assert!(out.is_null());
        let code_hash = *blake3::hash(&code).as_bytes();
        let ours = |c: &Census| c.refused.values().find(|r| r.hash == Some(hash)).map(|r| (r.compute, r.code.clone()));
        assert_eq!(with_census(|c| ours(c)), Some((true, vec![code_hash])));
        // The normal way, next on this thread, with the same root signature.
        let followed = with_census(|c| c.followed);
        let container = fspx_container(&code);
        let head = [root as usize, container.as_ptr() as usize, container.len()];
        type CreateCompute = extern "system" fn(This, *const usize, *const Iid, *mut This) -> HRESULT;
        let mut made = std::ptr::null();
        assert_eq!(slot::<CreateCompute>(device, 0x58)(device, head.as_ptr(), &pipeline, &mut made), S_OK);
        assert!(!made.is_null());
        assert!(with_census(|c| c.followed) > followed);
        // Its code is seen once the census's thread has looked at the container.
        let began = std::time::Instant::now();
        while !with_census(|c| c.seen.contains(&code_hash)) {
            assert!(began.elapsed() < std::time::Duration::from_secs(30), "the census never saw the code");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let counts = with_census(|c| tally(c.refused.values().filter(|r| r.hash == Some(hash)), &c.seen));
        assert_eq!(counts, [[0; 4], [1, 1, 0, 0]]);
        slot::<extern "system" fn(This) -> u32>(made, 0x10)(made);
    }

    #[test]
    fn virtual_memory_configuration_reports_user_space() {
        let mut config = VirtualMemory {
            flags: 0,
            page_table_pages: 7,
            preserved_bytes: 0,
            lowest: 0,
            highest: 0,
        };
        assert_eq!(configure_virtual_memory(&mut config), S_OK);
        assert!(0 < config.lowest && config.lowest < config.highest);
        let heap = Box::new(0u8);
        assert!((config.lowest..=config.highest).contains(&(&raw const *heap as u64)));
        assert_eq!(configure_virtual_memory(std::ptr::null_mut()), E_INVALIDARG);
    }

    #[test]
    fn feature_levels_pick_the_highest_supported() {
        type Check = extern "system" fn(This, u32, *mut c_void, u32) -> HRESULT;
        let device = device();
        let check = slot::<Check>(device, 0x68);
        let query = |levels: &[u32], size| {
            let mut query = FeatureLevels {
                count: levels.len() as u32,
                requested: levels.as_ptr(),
                highest: 0,
            };
            let hr = check(device, 2, (&raw mut query).cast(), size);
            (hr, query.highest)
        };
        let levels = [0xb000, 0xb100, 0xc000, 0xc100, 0xd000];
        assert_eq!(query(&levels, 0x18), (S_OK, 0xc100));
        assert_eq!(query(&levels, 0x10).0, E_INVALIDARG);
        assert_eq!(query(&levels[4..], 0x18).0, UNSUPPORTED);
    }
}
