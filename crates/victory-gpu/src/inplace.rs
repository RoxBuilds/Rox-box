//! Title memory as the card's own memory: a host heap opened on memory
//! the title already has, and a buffer placed in it, so that shaders read
//! and write the title's bytes themselves and no host copy stands between
//! (the console's one memory). This file holds the two openings Direct3D
//! has, a probe that says what this PC's driver makes of them
//! (`VICTORY_INPLACE_PROBE`), and a census of which title memory could be
//! opened.
//!
//! What the Direct3D runtime and the kernel answer, measured
//! with the software adapter (the rules are theirs, not the adapter's;
//! what the card's driver adds is what the probe is for):
//!
//! - `OpenExistingHeapFromAddress` takes the base address of a whole
//!   `VirtualAlloc` allocation and nothing inside it; every page must be
//!   committed and have one protection. Opening locks every page into the
//!   PC's memory at once (256 MiB never touched: 70 ms and 256 MB of
//!   working set).
//! - `OpenExistingHeapFromAddress1` (`ID3D12Device13`) takes any committed
//!   part of an allocation, with the same rule for its pages; parts may
//!   overlap, and the whole may be opened beside them. It also takes a
//!   committed view of a section.
//! - `OpenExistingHeapFromFileMapping` takes a section made with
//!   `SEC_COMMIT` and refuses one made with `SEC_RESERVE` (how `xmem`
//!   makes the title's page pool today). The heap is the section, not a
//!   view of it: views may be unmapped, mapped elsewhere or made
//!   read-only, and the section's handle closed, with the heap alive.
//!   Opening it locks nothing (256 MiB: 0.2 ms, no working set).
//! - The heap is a custom heap in the PC's memory (write-back pages, pool
//!   L0), shared and cross-adapter shared, 64 KiB aligned. A buffer in it
//!   must have `ALLOW_CROSS_ADAPTER`; unordered access beside it is taken.
//!   A texture must be row-major with one level. Tile mappings into such
//!   a heap are refused and REMOVE THE DEVICE: never call them.
//! - While a heap opened from an address stands, the title cannot
//!   decommit, release or protect the pages in it (`VirtualFree` and
//!   `VirtualProtect` fail with error 87), nor unmap a view in it. Pages
//!   outside the opened part are free to change.

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::{Interface, s};
use windows_sys::Win32::System::Memory as mem;

/// The unit heaps and placed buffers are cut on, and the console's page.
const K64: usize = 64 << 10;
/// The host's write watching (not in windows-sys' memory module).
const MEM_WRITE_WATCH: u32 = 0x20_0000;

/// `VICTORY_INPLACE_PROBE`: 0 (unset) nothing; 1 the probe on memory of
/// its own at the first sweep, and the census every 30 seconds after it,
/// six times (for runs of two to three minutes, so the last is near their
/// end); 2 also opens one part of the title's own memory for a moment at the
/// first census.
pub(crate) fn probe_level() -> u8 {
    static LEVEL: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *LEVEL.get_or_init(|| std::env::var("VICTORY_INPLACE_PROBE").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0))
}

/// `VICTORY_INPLACE`: 0 (unset) off. 1: a window of title memory outside
/// buffers that lies in private memory committed whole is not copied: a
/// part of the title's allocation is opened as a host heap and the window
/// is a buffer placed over it ([`place_for`]). 2: title buffers too,
/// except the big ones in the card's memory.
///
/// What stage 1 covers is private memory (`XMemVirtualAlloc`'s). View
/// memory (the page pool) is turned down here: its section is made
/// reserved, which the host refuses, and what a view shows changes when
/// the title maps pages.
///
/// What it assumes, for 2: nothing reads title memory that a submission
/// not yet run writes through such a buffer. With every submission waited
/// for (the default) that holds; with `VICTORY_LATE_WAIT` it does not,
/// since the flights' rules know shader writes only by the ranges copies
/// declare. Do not combine 2 with late wait.
///
/// shortcut: while a window is in place the title cannot decommit, free
/// or protect its pages (the call fails with error 87; the probe shows
/// it). The title's heaps are allocated once and kept, and nothing of the
/// kind was seen; [`title_memory_freeing`] is there for the runtime to
/// call before such a call once it takes them over.
pub(crate) fn level() -> u8 {
    static LEVEL: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *LEVEL.get_or_init(|| std::env::var("VICTORY_INPLACE").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0))
}

/// `VICTORY_INPLACE_POOL=1` (off unless set, read once): the title's page
/// pool is placed in place too. Needs the pool made committed whole
/// (`SEC_COMMIT`, which `xmem` does under the same switch, falling back to
/// a reserved section if the host refuses): each section is then opened as
/// one heap, lazily, and a range of title memory that is one run of a
/// section ([`Cover::Run`]) is a buffer placed in that heap at the run's
/// offset (rounded down to the title's 64 KiB page, the bias as for private
/// memory). Scattered pages and runs with holes stay copies. Opening a
/// section locks nothing, so these windows do not count against [`most`].
///
/// For windows shaders never write (stage 1): a window of the pool is
/// placed whatever [`level`] is; a title buffer over the pool only with
/// level 2. What shaders write to such a window lands in the pages the
/// title sees at that address, but not in the copies or guards of the same
/// pages seen elsewhere ([`shown_elsewhere`]); [`pool_written`] counts and
/// logs the declarations of such writes.
pub(crate) fn pool_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| crate::switch::on("VICTORY_INPLACE_POOL"))
}

/// The most title memory in place at once, in bytes
/// (`VICTORY_INPLACE_MB`, 4096): opening locks the pages into the PC's
/// memory, also those the title never touched.
fn most() -> u64 {
    static MOST: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MOST.get_or_init(|| std::env::var("VICTORY_INPLACE_MB").ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(4096) << 20)
}

/// In place now: windows and their bytes, title buffers and their bytes;
/// then, since the start: asks turned down because the memory is a view
/// or not private memory committed whole, because the host refused,
/// because of [`most`]; and what was let go because its memory changed.
/// Then, in place now: windows in section heaps and their bytes (they are
/// among the windows above); since the start: asks turned down as scattered
/// pages and as runs with holes; declarations of shader writes to a window
/// in a section heap ([`pool_written`]); remaps that found something in
/// place in their range, and remaps that did not.
static STATS: [AtomicU64; 15] = [const { AtomicU64::new(0) }; 15];

/// Where title memory is in place, so that a remap (the title makes about a
/// million a run) is told by a lookup whether anything there is: start and
/// bytes, and whether in a section heap. An entry is added when something is
/// placed and taken out when a remap lets the range go; one whose copy went
/// some other way stays until a remap touches its range (that remap then
/// looks at the tables, as it did before the record).
struct Placements {
    map: BTreeMap<(usize, usize), bool>,
    /// The longest entry ever noted: the lookup starts that far below.
    longest: usize,
}

static PLACED: std::sync::RwLock<Placements> = std::sync::RwLock::new(Placements { map: BTreeMap::new(), longest: 0 });

fn note_placed(start: usize, len: usize, pool: bool) {
    let mut p = PLACED.write().unwrap_or_else(|e| e.into_inner());
    p.map.insert((start, len), pool);
    p.longest = p.longest.max(len);
}

/// Whether something in place (in a section heap, if `only_pool`) may
/// overlap `[at, end)`.
fn placed_overlap(at: usize, end: usize, only_pool: bool) -> bool {
    let p = PLACED.read().unwrap_or_else(|e| e.into_inner());
    if p.map.is_empty() || at >= end {
        return false;
    }
    p.map
        .range((at.saturating_sub(p.longest), 0)..(end, 0))
        .any(|(&(start, len), &pool)| start.saturating_add(len) > at && (pool || !only_pool))
}

/// Takes what is noted in `[at, end)` out; true if there was any.
fn forget_placed(at: usize, end: usize) -> bool {
    let mut p = PLACED.write().unwrap_or_else(|e| e.into_inner());
    if p.map.is_empty() || at >= end {
        return false;
    }
    let keys: Vec<(usize, usize)> = p
        .map
        .range((at.saturating_sub(p.longest), 0)..(end, 0))
        .filter(|&(&(start, len), _)| start.saturating_add(len) > at)
        .map(|(key, _)| *key)
        .collect();
    for key in &keys {
        p.map.remove(key);
    }
    if p.map.is_empty() {
        p.longest = 0;
    }
    !keys.is_empty()
}

/// The lengths of the windows in section heaps that are alive, to tell which
/// kind a dropped one was ([`gone`] is told the length only). Exact while
/// private memory is not placed too (`VICTORY_INPLACE` with the pool
/// switch): a private window of the same length may then be counted as the
/// pool's.
static POOL_LENS: std::sync::Mutex<BTreeMap<u64, u32>> = std::sync::Mutex::new(BTreeMap::new());

/// A buffer placed over title memory, and its heap.
pub(crate) struct Placed {
    pub(crate) buffer: ID3D12Resource,
    pub(crate) heap: ID3D12Heap,
    /// The host GPU address of the title memory's first byte.
    pub(crate) gpu: u64,
    /// Bytes from the buffer's first byte to the title memory's first.
    pub(crate) bias: u64,
}

/// Opens the 64 KiB pages that hold title memory `[start, start + len)`
/// as a heap of their own and places one buffer over all of them. `Err`
/// with nothing if the memory is not private memory committed whole
/// (nothing was asked of the host), with the host's error if it refused.
pub(crate) fn place_part(device: &ID3D12Device, start: u64, len: u64) -> Result<Placed, Option<windows::core::Error>> {
    let at = start & !(K64 as u64 - 1);
    let bytes = (start + len.max(1)).next_multiple_of(K64 as u64) - at;
    if !private_whole(at as usize, bytes as usize) {
        return Err(None);
    }
    let heap = open(device, From::Part(at as usize, bytes as usize)).map_err(Some)?;
    let buffer = place(device, &heap, 0, bytes).map_err(Some)?;
    // SAFETY: a live buffer.
    let gpu = unsafe { buffer.GetGPUVirtualAddress() } + (start - at);
    Ok(Placed { buffer, heap, gpu, bias: start - at })
}

/// Opens the section the title sees `[start, start + len)` of, as one heap
/// (kept for the next ask), and places one buffer over the run of pages the
/// views show there, from the 64 KiB page that holds its first byte. `Err`
/// with nothing if the memory is no run of one section made committed
/// (nothing was asked of the host), with the host's error if it refused.
pub(crate) fn place_run(device: &ID3D12Device, start: u64, len: u64) -> Result<Placed, Option<windows::core::Error>> {
    let Cover::Run(section, offset) = cover(start as usize, len as usize) else {
        return Err(None);
    };
    let at = offset & !(K64 as u64 - 1);
    let bias = offset - at;
    let bytes = (bias + len.max(1)).next_multiple_of(K64 as u64);
    let heap = section_heap(device, (start as usize, len as usize), Cover::Run(section, offset), at + bytes)?;
    let buffer = place(device, &heap, at, bytes).map_err(Some)?;
    // SAFETY: a live buffer.
    let gpu = unsafe { buffer.GetGPUVirtualAddress() } + bias;
    Ok(Placed { buffer, heap, gpu, bias })
}

/// The heap of the section `want` says the views show `range` of, opened
/// when first asked for. `reach` is the end of the bytes needed in the
/// section. The views are looked at again with the table locked, which the
/// runtime's letting go of the section ([`title_memory_section_gone`]) takes
/// too: the handle is open while a view the title sees is of the section.
fn section_heap(
    device: &ID3D12Device,
    range: (usize, usize),
    want: Cover,
    reach: u64,
) -> Result<ID3D12Heap, Option<windows::core::Error>> {
    let Cover::Run(section, _) = want else {
        return Err(None);
    };
    // The heap is there already (a read of the table), or it is opened (a
    // write of it).
    for write in [false, true] {
        let found = if write {
            let mut v = views_mut();
            let Some(&(_, bytes, committed)) = v.sections.iter().find(|s| s.0 == section) else {
                return Err(None);
            };
            if !committed || reach > bytes || cover_in(&v.views, range.0, range.1) != want {
                return Err(None);
            }
            match v.heaps.iter().find(|h| h.0 == section) {
                Some(h) => Some(h.1.clone()),
                None => {
                    let heap = open(device, From::Section(section)).map_err(Some)?;
                    v.heaps.push((section, heap.clone()));
                    Some(heap)
                }
            }
        } else {
            let v = views();
            let Some(&(_, bytes, committed)) = v.sections.iter().find(|s| s.0 == section) else {
                return Err(None);
            };
            if !committed || reach > bytes {
                return Err(None);
            }
            v.heaps.iter().find(|h| h.0 == section).map(|h| h.1.clone())
        };
        if let Some(heap) = found {
            return Ok(heap);
        }
    }
    Err(None)
}

/// Title memory `[start, start + len)` in place, for a window outside
/// buffers or a title buffer, if [`level`] or [`pool_on`] asks for it and
/// the memory can be opened; `None` leaves the caller to make a copy, as
/// before.
pub(crate) fn place_for(start: u64, len: u64, window: bool) -> Option<Placed> {
    let private_ok = level() >= if window { 1 } else { 2 };
    let pool_ok = pool_on() && (window || level() >= 2);
    if !private_ok && !pool_ok {
        return None;
    }
    // View memory: known from the runtime without asking Windows.
    let covered = cover(start as usize, len as usize);
    if covered != Cover::None {
        return match covered {
            Cover::Run(..) if pool_ok => place_pooled(start, len, covered),
            Cover::Scattered if pool_ok => {
                STATS[10].fetch_add(1, Ordering::Relaxed);
                None
            }
            Cover::Holes if pool_ok => {
                STATS[11].fetch_add(1, Ordering::Relaxed);
                None
            }
            _ => {
                STATS[4].fetch_add(1, Ordering::Relaxed);
                None
            }
        };
    }
    if !private_ok {
        return None;
    }
    if STATS[1].load(Ordering::Relaxed) + STATS[3].load(Ordering::Relaxed) + len > most() {
        STATS[6].fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let host = crate::host::host()?;
    match place_part(&host.device, start, len) {
        Ok(placed) => {
            let at = if window { 0 } else { 2 };
            STATS[at].fetch_add(1, Ordering::Relaxed);
            STATS[at + 1].fetch_add(len, Ordering::Relaxed);
            note_placed(start as usize, len as usize, false);
            Some(placed)
        }
        Err(None) => {
            STATS[4].fetch_add(1, Ordering::Relaxed);
            None
        }
        Err(Some(e)) => {
            if STATS[5].fetch_add(1, Ordering::Relaxed) < 8 {
                tracing::warn!(start = format_args!("{start:#x}"), len, error = %e, "title memory not opened in place: a copy instead");
            }
            None
        }
    }
}

/// [`place_for`] for a run of the page pool's pages (`covered`).
fn place_pooled(start: u64, len: u64, covered: Cover) -> Option<Placed> {
    let host = crate::host::host()?;
    match place_run(&host.device, start, len) {
        Ok(placed) => {
            // Counted and noted before the views are looked at again: a
            // remap of the range that comes after the look finds it noted
            // (and lets it go with the rest), one that came before shows in
            // the look (the runtime tells the views before it calls
            // `title_memory_freeing`).
            STATS[0].fetch_add(1, Ordering::Relaxed);
            STATS[1].fetch_add(len, Ordering::Relaxed);
            STATS[8].fetch_add(1, Ordering::Relaxed);
            STATS[9].fetch_add(len, Ordering::Relaxed);
            *POOL_LENS.lock().unwrap_or_else(|e| e.into_inner()).entry(len).or_default() += 1;
            note_placed(start as usize, len as usize, true);
            if cover(start as usize, len as usize) != covered {
                forget_placed(start as usize, (start + len) as usize);
                gone(true, len);
                STATS[4].fetch_add(1, Ordering::Relaxed);
                return None;
            }
            Some(placed)
        }
        Err(None) => {
            STATS[4].fetch_add(1, Ordering::Relaxed);
            None
        }
        Err(Some(e)) => {
            if STATS[5].fetch_add(1, Ordering::Relaxed) < 8 {
                tracing::warn!(start = format_args!("{start:#x}"), len, error = %e, "the page pool not opened in place: a copy instead");
            }
            None
        }
    }
}

/// What [`place_for`] gave is gone.
pub(crate) fn gone(window: bool, bytes: u64) {
    let at = if window { 0 } else { 2 };
    STATS[at].fetch_sub(1, Ordering::Relaxed);
    STATS[at + 1].fetch_sub(bytes, Ordering::Relaxed);
    if window && STATS[8].load(Ordering::Relaxed) > 0 {
        let mut lens = POOL_LENS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = lens.get_mut(&bytes).filter(|n| **n > 0) {
            *n -= 1;
            if *n == 0 {
                lens.remove(&bytes);
            }
            STATS[8].fetch_sub(1, Ordering::Relaxed);
            STATS[9].fetch_sub(bytes, Ordering::Relaxed);
        }
    }
}

/// A submission declares that shaders write `[address, address + len)`,
/// which is in place in a section heap: counted, and the first few logged.
/// Stage 1 is for windows shaders never write. The write lands in the pages
/// the title sees at that address, which is right; what it misses is the
/// copies and guards of the same pages at other addresses. For
/// `Shadow::gpu_writes` and `Shadow::writing` to call where the copy is in
/// place.
pub(crate) fn pool_written(address: u64, len: u64) {
    let (at, end) = (address as usize, address.saturating_add(len.max(1)) as usize);
    if !placed_overlap(at, end, true) {
        return;
    }
    if STATS[12].fetch_add(1, Ordering::Relaxed) < 8 {
        tracing::warn!(address = format_args!("{address:#x}"), len, "shaders are declared to write a window of the page pool that is in place");
    }
}

/// Title memory `[at, at + len)` is about to show something else, or the
/// title is about to decommit, free or protect it: what is in place there
/// is let go (the tables forget it; its heap goes when the last list and
/// slot that hold it do). Called for memory the runtime maps anew; for
/// the runtime to call before the title's own `VirtualFree` once it takes
/// that call over.
pub fn title_memory_freeing(at: usize, len: usize) {
    if STATS[0].load(Ordering::Relaxed) + STATS[2].load(Ordering::Relaxed) == 0 {
        return;
    }
    STATS[7].fetch_add(let_go(at, len), Ordering::Relaxed);
}

/// What [`title_memory_freeing`] does once something is in place: a look in
/// the record of where, which takes none of the copies' tables (the title
/// remaps about a million times a run); only a range that overlaps something
/// in place goes on to `in_place_let_go`, which takes the big locks. The
/// record is emptied of the range first: what is placed after it (the views
/// already tell the new pages) notes itself again.
fn let_go(at: usize, len: usize) -> u64 {
    let end = at.saturating_add(len);
    if !placed_overlap(at, end, false) {
        STATS[14].fetch_add(1, Ordering::Relaxed);
        return 0;
    }
    STATS[13].fetch_add(1, Ordering::Relaxed);
    forget_placed(at, end);
    crate::shadow::in_place_let_go(at as u64, len as u64)
}

/// The line of what is in place, every ten seconds while [`level`] is on.
fn say() {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = crate::host::now_ms();
    let last = LAST.load(Ordering::Relaxed);
    if now < last + 10_000 || LAST.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_err() {
        return;
    }
    let s: Vec<u64> = STATS.iter().map(|s| s.load(Ordering::Relaxed)).collect();
    tracing::info!(
        "title memory in place: {} windows {} MB, {} title buffers {} MB (no copy, no refresh, no copy-back); turned down: {} view memory or not private memory committed whole, \
         {} refused by the host, {} over VICTORY_INPLACE_MB; {} let go because their memory changed; \
         of the windows, {} in section heaps (the page pool) {} MB; pool turned down: {} scattered pages, {} holes; {} declared written by shaders; \
         remaps with something in place in their range {}, without {}",
        s[0],
        s[1] >> 20,
        s[2],
        s[3] >> 20,
        s[4],
        s[5],
        s[6],
        s[7],
        s[8],
        s[9] >> 20,
        s[10],
        s[11],
        s[12],
        s[13],
        s[14]
    );
}

/// What a heap is opened on.
pub(crate) enum From {
    /// A whole `VirtualAlloc` allocation, by its base address.
    Region(usize),
    /// A committed part of one: address and bytes.
    Part(usize, usize),
    /// A section made with `SEC_COMMIT`, by its handle.
    Section(usize),
}

/// Opens a heap on memory the process has.
pub(crate) fn open(device: &ID3D12Device, from: From) -> windows::core::Result<ID3D12Heap> {
    // SAFETY: the calls take an address or a handle and check both
    // themselves; a refusal is an error code.
    unsafe {
        match from {
            From::Region(at) => device.cast::<ID3D12Device3>()?.OpenExistingHeapFromAddress(at as *const c_void),
            From::Part(at, bytes) => device.cast::<ID3D12Device13>()?.OpenExistingHeapFromAddress1(at as *const c_void, bytes),
            From::Section(handle) => device.cast::<ID3D12Device3>()?.OpenExistingHeapFromFileMapping(HANDLE(handle as *mut c_void)),
        }
    }
}

fn buffer_desc(size: u64, flags: D3D12_RESOURCE_FLAGS) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size.max(1),
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: flags,
        ..Default::default()
    }
}

/// A buffer of `size` bytes at `offset` (a multiple of 64 KiB) of an
/// opened heap, which shaders may read and write, in the common state.
pub(crate) fn place(device: &ID3D12Device, heap: &ID3D12Heap, offset: u64, size: u64) -> windows::core::Result<ID3D12Resource> {
    let mut placed: Option<ID3D12Resource> = None;
    // The heap is cross-adapter shared, so the buffer must say so too.
    let flags = D3D12_RESOURCE_FLAG_ALLOW_CROSS_ADAPTER | D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS;
    // SAFETY: plain creation in a live heap.
    unsafe { device.CreatePlacedResource(heap, offset, &buffer_desc(size, flags), D3D12_RESOURCE_STATE_COMMON, None, &mut placed)? };
    placed.ok_or_else(windows::core::Error::empty)
}

// The title's views of its physical pages, as the runtime maps them: the
// census needs them to say which view memory is one run of a section (one
// placed buffer could show it) and which is scattered pages.

struct Views {
    /// By address: bytes, the section's handle, the offset in it.
    views: BTreeMap<usize, (usize, usize, u64)>,
    /// By section and byte offset of a 64 KiB page: the address of that
    /// page in every view that shows it. A write through one view of a
    /// page is a write of the bytes every other view shows
    /// (`crate::watch` guards them all).
    pages: BTreeMap<(usize, u64), Vec<usize>>,
    /// Of those, how many are shown more than once.
    pages_twice: usize,
    /// Sections by handle: bytes, and whether made committed.
    sections: Vec<(usize, u64, bool)>,
    /// Our own view of a whole section, by its handle (0: the host gave
    /// none), made when first asked for ([`beside`]) and kept.
    whole: Vec<(usize, usize)>,
    /// A committed section as one host heap, by its handle: opened when
    /// first asked for ([`place_run`]), kept until the section goes.
    heaps: Vec<(usize, ID3D12Heap)>,
}

/// Read by many threads at once (write faults, landings), changed when the
/// runtime maps or unmaps.
static VIEWS: std::sync::RwLock<Views> = std::sync::RwLock::new(Views {
    views: BTreeMap::new(),
    pages: BTreeMap::new(),
    pages_twice: 0,
    sections: Vec::new(),
    whole: Vec::new(),
    heaps: Vec::new(),
});

fn views() -> std::sync::RwLockReadGuard<'static, Views> {
    VIEWS.read().unwrap_or_else(|e| e.into_inner())
}

fn views_mut() -> std::sync::RwLockWriteGuard<'static, Views> {
    VIEWS.write().unwrap_or_else(|e| e.into_inner())
}

/// The runtime made a section of `bytes` for physical pages; `committed`
/// if with `SEC_COMMIT` (only such a section can be opened as a heap).
pub fn title_memory_section(section: usize, bytes: u64, committed: bool) {
    let mut v = views_mut();
    v.sections.retain(|s| s.0 != section);
    // A handle's number given out again: the view was of the section that
    // had it before (it stays mapped: someone may be writing through it).
    v.whole.retain(|w| w.0 != section);
    // The heap of the section that had the number before is not this one's.
    // It goes with the lock off: its last reference may free the pages.
    let old_heap: Vec<_> = v.heaps.iter().filter(|h| h.0 == section).cloned().collect();
    v.heaps.retain(|h| h.0 != section);
    v.sections.push((section, bytes, committed));
    drop(v);
    drop(old_heap);
}

/// The runtime lets go of a section, and of its handle: no view the title
/// sees is of it any more, and [`beside`]'s view goes.
pub fn title_memory_section_gone(section: usize) {
    let (whole, heap) = {
        let mut v = views_mut();
        let at = v.whole.iter().position(|w| w.0 == section);
        // No heap is opened on it from now on; the buffers placed in the one
        // there is keep it alive until the last of them goes.
        v.sections.retain(|s| s.0 != section);
        let heap = v.heaps.iter().position(|h| h.0 == section).map(|i| v.heaps.swap_remove(i).1);
        (at.map(|i| v.whole.swap_remove(i).1), heap)
    };
    drop(heap);
    if let Some(base) = whole.filter(|&base| base != 0) {
        // SAFETY: a view of ours. Who writes through it found the section
        // in a view the title saw and holds off its unmapping
        // (`crate::watch::title_memory_reading`), which came before this.
        unsafe { mem::UnmapViewOfFile(mem::MEMORY_MAPPED_VIEW_ADDRESS { Value: base as *mut c_void }) };
    }
}

/// The runtime mapped `[at, at + len)` as a view of `section` from
/// `offset` on. Call after [`crate::title_memory_remapped`] of the range.
pub fn title_memory_view(at: usize, len: usize, section: usize, offset: u64) {
    {
        let mut v = views_mut();
        v.views.insert(at, (len, section, offset));
        for k in 0..len.div_ceil(K64) {
            let shown = v.pages.entry((section, offset + (k * K64) as u64)).or_default();
            shown.push(at + k * K64);
            let twice = shown.len() == 2;
            v.pages_twice += usize::from(twice);
        }
    }
    // This view is writable from now on: a guard over the same bytes at
    // another view must look at them again, and the pages are noted written
    // there (`crate::watch`).
    crate::watch::views_mapped(section, offset, len);
}

/// What `[at, at + len)` showed is gone ([`crate::title_memory_remapped`]):
/// the views that touch it are forgotten (the runtime tells again those it
/// maps anew).
pub(crate) fn views_gone(at: usize, len: usize) {
    let mut v = views_mut();
    if v.views.is_empty() {
        return;
    }
    let end = at.saturating_add(len);
    let first = v.views.range(..=at).next_back().filter(|(base, view)| *base + view.0 > at).map(|(base, _)| *base);
    let gone: Vec<usize> = first.into_iter().chain(v.views.range(at.saturating_add(1)..end).map(|(base, _)| *base)).collect();
    for base in gone {
        let Some((bytes, section, offset)) = v.views.remove(&base) else {
            continue;
        };
        for k in 0..bytes.div_ceil(K64) {
            let key = (section, offset + (k * K64) as u64);
            if let Some(pages) = v.pages.get_mut(&key) {
                let page = base + k * K64;
                let before = pages.len();
                pages.retain(|&shown| shown != page);
                let after = pages.len();
                if after == 0 {
                    v.pages.remove(&key);
                }
                v.pages_twice = v.pages_twice.saturating_sub(usize::from(before >= 2 && after < 2));
            }
        }
    }
}

/// The 64 KiB pages of `[start, end)` that the title shows at other
/// addresses too, in address order, each with those; not the ones it shows
/// in very many views (`crate::watch::MANY_VIEWS`).
pub(crate) fn shown_elsewhere(start: usize, end: usize) -> Vec<(usize, Vec<usize>)> {
    let v = views();
    let mut out = Vec::new();
    if start >= end || v.pages_twice == 0 {
        return out;
    }
    let first = v.views.range(..=start).next_back().filter(|(base, view)| *base + view.0 > start).map(|(base, _)| *base);
    for (&base, &(bytes, section, offset)) in v.views.range(first.unwrap_or(start)..end) {
        let from = start.max(base) & !(K64 - 1);
        for page in (from..end.min(base + bytes)).step_by(K64) {
            let Some(shown) = v.pages.get(&(section, offset + (page - base) as u64)) else {
                continue;
            };
            if (2..=crate::watch::MANY_VIEWS).contains(&shown.len()) {
                out.push((page, shown.iter().copied().filter(|&at| at != page).collect()));
            }
        }
    }
    out
}

/// The address of the 64 KiB page shown at `page` in every title view that
/// shows it, `page`'s own address included; empty if `page` is not a view.
pub(crate) fn pages_of(page: usize) -> Vec<usize> {
    let v = views();
    let Some((&base, &(bytes, section, offset))) = v.views.range(..=page).next_back() else {
        return Vec::new();
    };
    let within = page.wrapping_sub(base);
    if within >= bytes || !within.is_multiple_of(K64) {
        return Vec::new();
    }
    v.pages.get(&(section, offset + within as u64)).cloned().unwrap_or_default()
}

/// The title views that show any of the physical bytes `[offset, offset +
/// len)` of `section`, as `(address, bytes)` ranges in address order; not
/// those of a page shown in very many views (`MANY_VIEWS`).
pub(crate) fn views_of_section(section: usize, offset: u64, len: usize) -> Vec<(usize, usize)> {
    if len == 0 {
        return Vec::new();
    }
    let v = views();
    let end = offset.saturating_add(len as u64);
    let mut pages: Vec<usize> = Vec::new();
    let mut at = offset - offset % K64 as u64;
    while at < end {
        if let Some(shown) = v.pages.get(&(section, at))
            && shown.len() <= crate::watch::MANY_VIEWS
        {
            pages.extend(shown.iter().copied());
        }
        match at.checked_add(K64 as u64) {
            Some(next) => at = next,
            None => break,
        }
    }
    pages.sort_unstable();
    pages.dedup();
    let mut runs: Vec<(usize, usize)> = Vec::with_capacity(pages.len());
    for page in pages {
        match runs.last_mut() {
            Some((first, bytes)) if *first + *bytes == page => *bytes += K64,
            _ => runs.push((page, K64)),
        }
    }
    runs
}

/// How many bytes of `[at, at + len)` views of physical pages show; `None`
/// if no view touches it (nothing is mapped there, or memory of another
/// kind). The runtime's own record of what it mapped: no question to the
/// host.
pub(crate) fn shown_bytes(at: usize, len: usize) -> Option<u64> {
    let end = at.saturating_add(len);
    let v = views();
    let first = v.views.range(..=at).next_back();
    let rest = v.views.range(at.saturating_add(1)..end.max(at.saturating_add(1)));
    let (mut there, mut any) = (0u64, false);
    for (&base, view) in first.into_iter().chain(rest) {
        let (from, to) = (base.max(at), base.saturating_add(view.0).min(end));
        if from < to {
            there += (to - from) as u64;
            any = true;
        }
    }
    any.then_some(there)
}

/// What of `[at, at + len)` views of physical pages show, in order, pieces
/// that touch as one: (address, bytes). Empty: nothing, or memory of another
/// kind.
pub(crate) fn shown_runs(at: usize, len: usize) -> Vec<(usize, usize)> {
    let end = at.saturating_add(len);
    let v = views();
    let first = v.views.range(..=at).next_back();
    let rest = v.views.range(at.saturating_add(1)..end.max(at.saturating_add(1)));
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for (&base, view) in first.into_iter().chain(rest) {
        let (from, to) = (base.max(at), base.saturating_add(view.0).min(end));
        if from >= to {
            continue;
        }
        match runs.last_mut() {
            Some((run, bytes)) if *run + *bytes == from => *bytes += to - from,
            _ => runs.push((from, to - from)),
        }
    }
    runs
}

/// How the views cover `[at, at + len)`.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum Cover {
    /// No view holds its first byte.
    None,
    /// Views hold a part of it only.
    Holes,
    /// Views hold all of it, from more than one place.
    Scattered,
    /// All of it is one run of one section: the section and the offset of
    /// `at` in it.
    Run(usize, u64),
}

fn cover_in(views: &BTreeMap<usize, (usize, usize, u64)>, at: usize, len: usize) -> Cover {
    let end = at + len.max(1);
    let Some((&base, &(bytes, section, offset))) = views.range(..=at).next_back().filter(|(base, view)| *base + view.0 > at) else {
        return Cover::None;
    };
    let first = (section, offset + (at - base) as u64);
    let (mut reached, mut one) = (base + bytes, true);
    while reached < end {
        let Some(&(bytes, s, o)) = views.get(&reached) else {
            return Cover::Holes;
        };
        one &= s == first.0 && o == first.1 + (reached - at) as u64;
        reached += bytes;
    }
    if one { Cover::Run(first.0, first.1) } else { Cover::Scattered }
}

/// [`Cover`] of `[at, at + len)` by the title's views as they are now.
pub(crate) fn cover(at: usize, len: usize) -> Cover {
    cover_in(&views().views, at, len)
}

/// Another address of the title memory `[at, at + len)`, if it is a view of
/// the title's pages, all of it one run of a section: in a view of ours of
/// the whole section, which is never guarded (`crate::watch`) nor changed.
///
/// The section's pages are reserved and the runtime commits each before it
/// shows it, which holds for every view of the section. The caller holds
/// `crate::watch::title_memory_reading`: until it lets go the title sees
/// the same pages at `at`.
pub(crate) fn beside(at: usize, len: usize) -> Option<usize> {
    let known = {
        let v = views();
        let Cover::Run(section, offset) = cover_in(&v.views, at, len) else {
            return None;
        };
        v.whole.iter().find(|w| w.0 == section).map(|w| (w.1, offset))
    };
    if let Some((base, offset)) = known {
        return (base != 0).then(|| base + offset as usize);
    }
    let mut v = views_mut();
    let Cover::Run(section, offset) = cover_in(&v.views, at, len) else {
        return None;
    };
    let base = match v.whole.iter().find(|w| w.0 == section) {
        Some(w) => w.1,
        None => {
            let &(_, bytes, _) = v.sections.iter().find(|s| s.0 == section)?;
            // SAFETY: the whole of a section the runtime made; a view the
            // title sees is of it, so its handle is open.
            let base = unsafe { mem::MapViewOfFile(section as _, mem::FILE_MAP_WRITE, 0, 0, bytes as usize) }.Value as usize;
            if base == 0 {
                tracing::warn!(error = %std::io::Error::last_os_error(), "no view of the title's pages beside its own");
            }
            v.whole.push((section, base));
            base
        }
    };
    (base != 0).then(|| base + offset as usize)
}

/// Our own address of the 64 KiB page the title sees at `page` ([`beside`]),
/// and every address the title sees it at (0: no more); `None` if the page is
/// in no view of a section, or in very many. While the caller holds
/// `crate::watch::title_memory_reading`.
pub(crate) fn beside_page(page: usize) -> Option<(usize, [usize; 8])> {
    for asked in [false, true] {
        {
            let v = views();
            let (&base, &(bytes, section, offset)) = v.views.range(..=page).next_back()?;
            let within = page.wrapping_sub(base);
            if within >= bytes || !within.is_multiple_of(K64) {
                return None;
            }
            let offset = offset + within as u64;
            if let Some(whole) = v.whole.iter().find(|w| w.0 == section) {
                let mut shown = [0; 8];
                let all = v.pages.get(&(section, offset))?;
                shown.get_mut(..all.len())?.copy_from_slice(all);
                return (whole.1 != 0).then(|| (whole.1 + offset as usize, shown));
            }
        }
        if asked || beside(page, K64).is_none() {
            return None;
        }
    }
    None
}

/// Whether `[at, at + len)` is private memory, all committed and
/// read-write: what [`From::Part`] takes. One question to Windows (it
/// walks the page tables to the end of the pages that share the first
/// one's state: a millisecond in the title's big blocks, so ask when a
/// copy is made, not when it is used).
pub(crate) fn private_whole(at: usize, len: usize) -> bool {
    // SAFETY: plain data filled in by the query.
    let mut info: mem::MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { mem::VirtualQuery(at as *const c_void, &mut info, size_of_val(&info)) };
    n != 0
        && info.State == mem::MEM_COMMIT
        && info.Type == mem::MEM_PRIVATE
        && info.Protect == mem::PAGE_READWRITE
        && (info.BaseAddress as usize).saturating_add(info.RegionSize) >= at.saturating_add(len)
}

fn hr<T>(result: &windows::core::Result<T>) -> String {
    match result {
        Ok(_) => "ok".to_owned(),
        Err(e) => format!("REFUSED {:#010x} ({})", e.code().0, e.message().trim()),
    }
}

/// A queue, a list and a fence of the probe's own on `device`: nothing of
/// the host's submissions is touched.
struct Rig {
    device: ID3D12Device,
    queue: ID3D12CommandQueue,
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    fence: ID3D12Fence,
    done: u64,
    event: HANDLE,
}

impl Drop for Rig {
    fn drop(&mut self) {
        // SAFETY: our event.
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.event);
        }
    }
}

impl Rig {
    fn new(device: &ID3D12Device) -> windows::core::Result<Rig> {
        // SAFETY: plain creation of objects of our own.
        unsafe {
            let queue: ID3D12CommandQueue =
                device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() })?;
            let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
            let list: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)?;
            list.Close()?;
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
            let event = CreateEventW(None, false, false, None)?;
            Ok(Rig { device: device.clone(), queue, allocator, list, fence, done: 0, event })
        }
    }

    /// Records with `record`, runs it and waits (ten seconds at most).
    fn run(&mut self, record: impl FnOnce(&ID3D12GraphicsCommandList)) -> windows::core::Result<()> {
        // SAFETY: our own list and queue, one run at a time.
        unsafe {
            self.allocator.Reset()?;
            self.list.Reset(&self.allocator, None)?;
            record(&self.list);
            self.list.Close()?;
            self.queue.ExecuteCommandLists(&[Some(self.list.cast()?)]);
            self.done += 1;
            self.queue.Signal(&self.fence, self.done)?;
            self.fence.SetEventOnCompletion(self.done, self.event)?;
            if WaitForSingleObject(self.event, 10_000).0 != 0 {
                return Err(windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL));
            }
            self.device.GetDeviceRemovedReason()
        }
    }

    fn committed(&self, kind: D3D12_HEAP_TYPE, size: u64, state: D3D12_RESOURCE_STATES) -> windows::core::Result<ID3D12Resource> {
        let heap = D3D12_HEAP_PROPERTIES { Type: kind, ..Default::default() };
        let mut buffer: Option<ID3D12Resource> = None;
        // SAFETY: plain creation.
        unsafe {
            self.device
                .CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &buffer_desc(size, D3D12_RESOURCE_FLAG_NONE), state, None, &mut buffer)?
        };
        buffer.ok_or_else(windows::core::Error::empty)
    }

    /// What the GPU reads of `size` bytes of `resource` from `offset`.
    fn fetch(&mut self, resource: &ID3D12Resource, offset: u64, size: usize) -> windows::core::Result<Vec<u8>> {
        let back = self.committed(D3D12_HEAP_TYPE_READBACK, size as u64, D3D12_RESOURCE_STATE_COPY_DEST)?;
        // SAFETY: two live buffers, the range inside both.
        self.run(|list| unsafe { list.CopyBufferRegion(&back, 0, resource, offset, size as u64) })?;
        let mut data = std::ptr::null_mut();
        // SAFETY: the mapping holds `size` bytes until unmapped.
        unsafe {
            back.Map(0, None, Some(&mut data))?;
            let bytes = std::slice::from_raw_parts(data.cast::<u8>(), size).to_vec();
            back.Unmap(0, None);
            Ok(bytes)
        }
    }

    /// The GPU writes `bytes` at `offset` of `resource` (a copy).
    fn store(&mut self, resource: &ID3D12Resource, offset: u64, bytes: &[u8]) -> windows::core::Result<()> {
        let up = self.committed(D3D12_HEAP_TYPE_UPLOAD, bytes.len() as u64, D3D12_RESOURCE_STATE_GENERIC_READ)?;
        let mut data = std::ptr::null_mut();
        // SAFETY: the mapping holds the buffer's bytes until unmapped.
        unsafe {
            up.Map(0, None, Some(&mut data))?;
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), data.cast::<u8>(), bytes.len());
            up.Unmap(0, None);
        }
        // SAFETY: two live buffers, the range inside both.
        self.run(|list| unsafe { list.CopyBufferRegion(resource, offset, &up, 0, bytes.len() as u64) })
    }

    /// A compute shader stores, through a root unordered-access view at
    /// `gpu`, the sum of the buffer's first two words in its third.
    fn shade(&mut self, gpu: u64) -> windows::core::Result<()> {
        const SOURCE: &str = "#define RS \"UAV(u0)\"\nRWByteAddressBuffer b : register(u0);\n[RootSignature(RS)]\n[numthreads(1, 1, 1)]\nvoid main() { b.Store(8, b.Load(0) + b.Load(4)); }\n";
        let mut code: Option<ID3DBlob> = None;
        // SAFETY: the source and names live through the call; then the
        // blob's own bytes, which hold the root signature too.
        let (root, pso): (ID3D12RootSignature, ID3D12PipelineState) = unsafe {
            D3DCompile(SOURCE.as_ptr().cast(), SOURCE.len(), s!("inplace"), None, None, s!("main"), s!("cs_5_1"), 0, 0, &mut code, None)?;
            let code = code.ok_or_else(windows::core::Error::empty)?;
            let bytes = std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize());
            let root: ID3D12RootSignature = self.device.CreateRootSignature(0, bytes)?;
            let pso = self.device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE { pShaderBytecode: bytes.as_ptr().cast(), BytecodeLength: bytes.len() },
                ..Default::default()
            })?;
            (root, pso)
        };
        // SAFETY: an open list of ours; the view names a live buffer.
        self.run(|list| unsafe {
            list.SetComputeRootSignature(&root);
            list.SetPipelineState(&pso);
            list.SetComputeRootUnorderedAccessView(0, gpu);
            list.Dispatch(1, 1, 1);
        })
    }

    /// Milliseconds the card takes to copy `bytes` of `from` into its own
    /// memory, the best of three.
    fn copy_ms(&mut self, from: &ID3D12Resource, bytes: u64) -> windows::core::Result<f64> {
        let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() };
        let mut to: Option<ID3D12Resource> = None;
        // SAFETY: plain creation.
        unsafe {
            self.device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &buffer_desc(bytes, D3D12_RESOURCE_FLAG_NONE), D3D12_RESOURCE_STATE_COMMON, None, &mut to)?
        };
        let to = to.ok_or_else(windows::core::Error::empty)?;
        let mut best = f64::MAX;
        for _ in 0..3 {
            let started = std::time::Instant::now();
            // SAFETY: two live buffers of `bytes` bytes or more.
            self.run(|list| unsafe { list.CopyBufferRegion(&to, 0, from, 0, bytes) })?;
            best = best.min(started.elapsed().as_secs_f64() * 1000.0);
        }
        Ok(best)
    }
}

fn fill(at: *mut u8, len: usize, seed: u8) {
    for i in 0..len {
        // SAFETY: the caller's own committed memory of `len` bytes.
        unsafe { at.add(i).write((i as u8).wrapping_mul(31).wrapping_add(seed)) };
    }
}

fn same(at: *const u8, bytes: &[u8]) -> bool {
    // SAFETY: the caller's own committed memory of that length.
    unsafe { std::slice::from_raw_parts(at, bytes.len()) == bytes }
}

/// The process's working set in MB.
fn working_set_mb() -> u64 {
    use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    let mut counters = PROCESS_MEMORY_COUNTERS { cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32, ..Default::default() };
    // SAFETY: our own process and a sized structure.
    unsafe { K32GetProcessMemoryInfo(windows_sys::Win32::System::Threading::GetCurrentProcess(), &mut counters, counters.cb) };
    counters.WorkingSetSize as u64 >> 20
}

/// What the probe found. The lines are the whole account; the flags are
/// what the stages build on.
#[derive(Default)]
pub(crate) struct Probe {
    pub(crate) lines: Vec<String>,
    /// A part of private, write-watched memory opens, a buffer in it shows
    /// the memory's bytes to the GPU, also those the CPU wrote afterwards.
    pub(crate) part_reads: bool,
    /// What the GPU writes there (a copy, and a shader through unordered
    /// access) the CPU reads at the memory's address.
    pub(crate) part_writes: bool,
    /// The same two for a committed section, through a view of it.
    pub(crate) section_reads: bool,
    pub(crate) section_writes: bool,
    /// The section's heap outlives the view: the GPU's write with no view
    /// mapped shows in a view mapped afterwards.
    pub(crate) section_outlives_views: bool,
    /// A section made with `SEC_RESERVE` (as `xmem` makes the page pool)
    /// was refused.
    pub(crate) reserved_section_refused: bool,
    /// `OpenExistingHeapFromAddress1` over a committed whole-section view
    /// of a section made `SEC_RESERVE` (no flag change needed) works, a
    /// buffer in it shows the bytes the CPU wrote, and (None: not measured)
    /// opening locked its pages into the PC's memory.
    pub(crate) reserved_view_opens: bool,
    pub(crate) reserved_view_reads: bool,
    pub(crate) reserved_view_locks: Option<bool>,
    /// Two buffers placed over the same heap pages, partly overlapping,
    /// both show what the CPU wrote there afterwards.
    pub(crate) overlap_reads: bool,
}

/// Tries the openings on memory of the probe's own, made the way `xmem`
/// makes title memory, with a queue of its own on `device`; `memory`
/// gives the card's memory account (`Host::video_memory`) where there is
/// one. Nothing of the title's is touched. `mb` is the size of the cost
/// measurement (0 leaves it out).
pub(crate) fn probe(device: &ID3D12Device, memory: &dyn Fn() -> Option<[u64; 4]>, mb: usize) -> Probe {
    let mut p = Probe::default();
    let mut rig = match Rig::new(device) {
        Ok(rig) => rig,
        Err(e) => {
            p.lines.push(format!("no queue of its own: {e}"));
            return p;
        }
    };
    let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
    // SAFETY: a plain query that fills our structure.
    let _ = unsafe { device.CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS, (&raw mut options).cast(), size_of_val(&options) as u32) };
    p.lines.push(format!(
        "the device: ID3D12Device3 {}, ID3D12Device13 {} (parts of an allocation); cross-adapter row-major textures {}; heap tier {}",
        device.cast::<ID3D12Device3>().is_ok(),
        device.cast::<ID3D12Device13>().is_ok(),
        options.CrossAdapterRowMajorTextureSupported.as_bool(),
        options.ResourceHeapTier.0
    ));
    // SAFETY: memory of the probe's own, made and released here; every
    // address used lies inside it.
    unsafe {
        private(&mut p, &mut rig);
        section(&mut p, &mut rig);
        reserved_view(&mut p, &mut rig, mb);
        if mb > 0 {
            cost(&mut p, &mut rig, memory, mb);
        }
    }
    p.lines.push(format!(
        "VERDICT: private memory in place: reads {}, writes {}; the page pool's section in place: reads {}, writes {}, whatever the views do {}; \
         a reserved section is refused {} (the pool must be made committed to be opened)",
        p.part_reads, p.part_writes, p.section_reads, p.section_writes, p.section_outlives_views, p.reserved_section_refused
    ));
    let locks = match p.reserved_view_locks {
        Some(locked) => locked.to_string(),
        None => "not measured".to_owned(),
    };
    p.lines.push(format!(
        "VERDICT: a committed view of a reserved section opened as a heap (OpenExistingHeapFromAddress1): works {}, the GPU reads what the CPU wrote {}, \
         opening locks its pages into the PC's memory {locks}; two buffers placed over the same heap pages both read what the CPU wrote {}",
        p.reserved_view_opens, p.reserved_view_reads, p.overlap_reads
    ));
    p
}

fn describe(heap: &ID3D12Heap) -> String {
    // SAFETY: a plain query.
    let d = unsafe { heap.GetDesc() };
    format!(
        "heap of {} KiB: type {} (4 = custom), CPU pages {} (3 = write-back), pool {} (1 = the PC's memory), alignment {}, flags {:#x} (0x1 shared, 0x20 cross-adapter, 0x200 write watch, 0x400 shader atomics)",
        d.SizeInBytes >> 10,
        d.Properties.Type.0,
        d.Properties.CPUPageProperty.0,
        d.Properties.MemoryPoolPreference.0,
        d.Alignment,
        d.Flags.0
    )
}

/// Private memory, reserved as `xmem` reserves the title's heaps
/// (committed, read-write, write-watched): a part of it opened.
unsafe fn private(p: &mut Probe, rig: &mut Rig) {
    const SIZE: usize = 64 * K64;
    // SAFETY (the whole function): the allocation below, `SIZE` bytes.
    unsafe {
        let base = mem::VirtualAlloc(std::ptr::null(), SIZE, mem::MEM_RESERVE | mem::MEM_COMMIT | MEM_WRITE_WATCH, mem::PAGE_READWRITE).cast::<u8>();
        if base.is_null() {
            p.lines.push("private memory: none to try with".to_owned());
            return;
        }
        // The part: 16 pages from the 16th on.
        let (at, bytes) = (base.add(16 * K64), 16 * K64);
        let whole = open(&rig.device, From::Region(base as usize));
        p.lines.push(format!("private memory, the whole allocation (OpenExistingHeapFromAddress): {}", hr(&whole)));
        drop(whole);
        let opened = open(&rig.device, From::Part(at as usize, bytes));
        p.lines.push(format!("private memory, a part of it (OpenExistingHeapFromAddress1): {}", hr(&opened)));
        if let Ok(heap) = &opened {
            p.lines.push(format!("  {}", describe(heap)));
            let placed = place(&rig.device, heap, K64 as u64, 4 * K64 as u64);
            p.lines.push(format!("  a buffer placed in it (cross-adapter, unordered access): {}", hr(&placed)));
            if let Ok(buffer) = &placed {
                let memory = at.add(K64);
                fill(memory, 4 * K64, 1);
                let first = rig.fetch(buffer, 0, 4 * K64).is_ok_and(|got| same(memory, &got));
                fill(memory, 4 * K64, 77);
                let second = rig.fetch(buffer, 0, 4 * K64).is_ok_and(|got| same(memory, &got));
                p.part_reads = first && second;
                p.lines.push(format!("  the GPU reads the memory's bytes: {first}; and those the CPU wrote afterwards, with no call in between: {second}"));
                mem::ResetWriteWatch(base.cast(), SIZE);
                let new: Vec<u8> = (0..K64).map(|i| (i as u8) ^ 0x5a).collect();
                let copied = rig.store(buffer, 4096, &new).is_ok() && same(memory.add(4096), &new);
                let (mut found, mut count, mut granularity) = ([0usize; 64], 64usize, 0u32);
                let watched = mem::GetWriteWatch(0, base.cast(), SIZE, found.as_mut_ptr().cast(), &mut count, &mut granularity) == 0;
                memory.cast::<[u32; 3]>().write([40, 2, 0]);
                let shaded = rig.shade(buffer.GetGPUVirtualAddress());
                let shader = shaded.is_ok() && memory.cast::<[u32; 3]>().read()[2] == 42;
                p.part_writes = copied && shader;
                p.lines.push(format!(
                    "  the CPU reads at the memory's address what the GPU wrote: by a copy {copied}; by a shader through unordered access {shader} ({}); \
                     write watching saw {} of the copy's 16 pages{}",
                    hr(&shaded),
                    if watched { count } else { 0 },
                    if watched { "" } else { " (the asking failed)" }
                ));
            }
            // What the title can still do with the pages while the heap stands.
            let mut old = 0;
            let protect_in = mem::VirtualProtect(at.add(8 * K64).cast(), K64, mem::PAGE_READONLY, &mut old) != 0;
            if protect_in {
                mem::VirtualProtect(at.add(8 * K64).cast(), K64, mem::PAGE_READWRITE, &mut old);
            }
            let decommit_in = mem::VirtualFree(at.add(9 * K64).cast(), K64, mem::MEM_DECOMMIT) != 0;
            let decommit_out = mem::VirtualFree(base.add(40 * K64).cast(), K64, mem::MEM_DECOMMIT) != 0;
            p.lines.push(format!(
                "  with the heap standing, the title's own calls: read-only inside the part {protect_in}, decommit inside {decommit_in}, decommit outside {decommit_out} \
                 (false inside means the heap must go before the title frees or guards those pages)"
            ));
            drop(placed);
        }
        drop(opened);
        let freed = mem::VirtualFree(base.cast(), 0, mem::MEM_RELEASE) != 0;
        p.lines.push(format!("  released after the heap went: {freed}"));
    }
}

/// A section and a view of it in a placeholder, as `xmem` maps physical
/// pages: reserved (as it is made today) and committed.
unsafe fn section(p: &mut Probe, rig: &mut Rig) {
    const SIZE: usize = 32 * K64;
    // SAFETY (the whole function): the sections, placeholder and views
    // made below.
    unsafe {
        let process = windows_sys::Win32::System::Threading::GetCurrentProcess();
        let invalid = windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        let reserved = mem::CreateFileMappingW(invalid, std::ptr::null(), mem::PAGE_READWRITE | mem::SEC_RESERVE, 0, SIZE as u32, std::ptr::null());
        if !reserved.is_null() {
            let opened = open(&rig.device, From::Section(reserved as usize));
            p.reserved_section_refused = opened.is_err();
            p.lines.push(format!("a section made reserved, as the page pool is today (OpenExistingHeapFromFileMapping): {}", hr(&opened)));
            drop(opened);
            windows_sys::Win32::Foundation::CloseHandle(reserved);
        }
        let handle = mem::CreateFileMappingW(invalid, std::ptr::null(), mem::PAGE_READWRITE | mem::SEC_COMMIT, 0, SIZE as u32, std::ptr::null());
        let hole = mem::VirtualAlloc2(process, std::ptr::null(), SIZE, mem::MEM_RESERVE | mem::MEM_RESERVE_PLACEHOLDER, mem::PAGE_NOACCESS, std::ptr::null_mut(), 0).cast::<u8>();
        if handle.is_null() || hole.is_null() {
            p.lines.push("a committed section: none to try with".to_owned());
            return;
        }
        // Pages 4..12 of the section at the placeholder's start.
        mem::VirtualFree(hole.cast(), 8 * K64, mem::MEM_RELEASE | mem::MEM_PRESERVE_PLACEHOLDER);
        let view = mem::MapViewOfFile3(handle, process, hole.cast(), (4 * K64) as u64, 8 * K64, mem::MEM_REPLACE_PLACEHOLDER, mem::PAGE_READWRITE, std::ptr::null_mut(), 0);
        let opened = open(&rig.device, From::Section(handle as usize));
        p.lines.push(format!("a section made committed (OpenExistingHeapFromFileMapping): {}", hr(&opened)));
        let mut unmapped = false;
        if let (Ok(heap), false) = (&opened, view.Value.is_null()) {
            p.lines.push(format!("  {}", describe(heap)));
            // The section's page 5 is the view's second page.
            let placed = place(&rig.device, heap, 5 * K64 as u64, 2 * K64 as u64);
            p.lines.push(format!("  a buffer placed at the section's offset of a view's pages: {}", hr(&placed)));
            if let Ok(buffer) = &placed {
                let memory = hole.add(K64);
                fill(memory, 2 * K64, 3);
                p.section_reads = rig.fetch(buffer, 0, 2 * K64).is_ok_and(|got| same(memory, &got));
                let new: Vec<u8> = (0..K64).map(|i| (i as u8) ^ 0x33).collect();
                p.section_writes = rig.store(buffer, 0, &new).is_ok() && same(memory, &new);
                // A second buffer over the section's pages 6 and 7: it
                // overlaps the first (pages 5 and 6) on page 6. The CPU
                // writes there, and each buffer reads it.
                let second = place(&rig.device, heap, 6 * K64 as u64, 2 * K64 as u64);
                p.lines.push(format!("  a second buffer placed over pages that overlap the first's: {}", hr(&second)));
                if let Ok(second) = &second {
                    let shared = hole.add(2 * K64);
                    fill(shared, 2 * K64, 9);
                    let (a, b) = (rig.fetch(buffer, K64 as u64, K64), rig.fetch(second, 0, K64));
                    let first_pass = a.as_ref().is_ok_and(|got| same(shared, got)) && b.as_ref().is_ok_and(|got| same(shared, got));
                    fill(shared, 2 * K64, 200);
                    let (a, b) = (rig.fetch(buffer, K64 as u64, K64), rig.fetch(second, 0, K64));
                    let again = a.as_ref().is_ok_and(|got| same(shared, got)) && b.as_ref().is_ok_and(|got| same(shared, got));
                    p.overlap_reads = first_pass && again;
                    p.lines.push(format!(
                        "  both buffers read the bytes the CPU wrote on the page they share: the first time {first_pass}, after the CPU wrote them again {again}"
                    ));
                }
                drop(second);
                // The guard of `watch.rs`: the view read-only.
                let mut old = 0;
                let guarded = mem::VirtualProtect(memory.cast(), K64, mem::PAGE_READONLY, &mut old) != 0;
                let new: Vec<u8> = (0..K64).map(|i| (i as u8) ^ 0x44).collect();
                let through = rig.store(buffer, 0, &new).is_ok() && same(memory, &new);
                // The view goes; the GPU writes; another view shows it.
                unmapped = mem::UnmapViewOfFile2(process, view, mem::MEM_PRESERVE_PLACEHOLDER) != 0;
                let new: Vec<u8> = (0..K64).map(|i| (i as u8) ^ 0x55).collect();
                let stored = rig.store(buffer, 0, &new).is_ok();
                let other = mem::MapViewOfFile(handle, mem::FILE_MAP_WRITE, 0, (5 * K64) as u32, K64);
                p.section_outlives_views = unmapped && stored && !other.Value.is_null() && same(other.Value.cast(), &new);
                p.lines.push(format!(
                    "  the GPU reads the view's bytes {}; the CPU reads through the view what the GPU wrote {}; the view made read-only under the heap {guarded}, \
                     and the GPU writes it all the same {through}; the view unmapped under the heap {unmapped}, the GPU's write then shows in a view mapped afterwards {}",
                    p.section_reads, p.section_writes, p.section_outlives_views
                ));
                if !other.Value.is_null() {
                    mem::UnmapViewOfFile(other);
                }
            }
        }
        drop(opened);
        if !unmapped && !view.Value.is_null() {
            mem::UnmapViewOfFile2(process, view, mem::MEM_PRESERVE_PLACEHOLDER);
        }
        windows_sys::Win32::Foundation::CloseHandle(handle);
        // The placeholder was split in two: each half is released.
        mem::VirtualFree(hole.cast(), 0, mem::MEM_RELEASE);
        mem::VirtualFree(hole.add(8 * K64).cast(), 0, mem::MEM_RELEASE);
    }
}

/// A whole view of a section made `SEC_RESERVE` (as the page pool is made
/// today), committed with `VirtualAlloc`, opened with
/// `OpenExistingHeapFromAddress1`: whether it works with no change of the
/// section's flag, whether a buffer in it shows the CPU's bytes, and (with
/// `mb` > 0, so that the view is big enough to see in the working set)
/// whether opening locked its pages into the PC's memory. Then what the
/// title can still do to the pages while the heap stands.
unsafe fn reserved_view(p: &mut Probe, rig: &mut Rig, mb: usize) {
    let size = if mb > 0 { mb.max(16) << 20 } else { 8 * K64 };
    // SAFETY (the whole function): the section and view made here, every
    // address inside the view.
    unsafe {
        let invalid = windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        let handle = mem::CreateFileMappingW(
            invalid,
            std::ptr::null(),
            mem::PAGE_READWRITE | mem::SEC_RESERVE,
            (size as u64 >> 32) as u32,
            size as u32,
            std::ptr::null(),
        );
        if handle.is_null() {
            p.lines.push("a view of a reserved section: no section to try with".to_owned());
            return;
        }
        let view = mem::MapViewOfFile(handle, mem::FILE_MAP_WRITE, 0, 0, size);
        let base = view.Value.cast::<u8>();
        let committed = !base.is_null() && !mem::VirtualAlloc(base.cast(), size, mem::MEM_COMMIT, mem::PAGE_READWRITE).is_null();
        if !committed {
            p.lines.push("a view of a reserved section: could not map and commit a view to try with".to_owned());
        } else {
            let (set, started) = (working_set_mb(), std::time::Instant::now());
            let opened = open(&rig.device, From::Part(base as usize, size));
            let took = started.elapsed().as_secs_f64() * 1000.0;
            let set_opened = working_set_mb();
            p.reserved_view_opens = opened.is_ok();
            if mb > 0 {
                p.reserved_view_locks = Some(opened.is_ok() && set_opened >= set + (size >> 21) as u64);
            }
            p.lines.push(format!(
                "a committed whole view ({} MB) of a section made reserved (OpenExistingHeapFromAddress1): {}; opened in {took:.1} ms; working set {set} -> {set_opened} MB",
                size >> 20,
                hr(&opened)
            ));
            if let Ok(heap) = &opened {
                p.lines.push(format!("  {}", describe(heap)));
                let placed = place(&rig.device, heap, 0, 2 * K64 as u64);
                p.lines.push(format!("  a buffer placed in it: {}", hr(&placed)));
                if let Ok(buffer) = &placed {
                    fill(base, 2 * K64, 21);
                    let first = rig.fetch(buffer, 0, 2 * K64).is_ok_and(|got| same(base, &got));
                    fill(base, 2 * K64, 99);
                    let second = rig.fetch(buffer, 0, 2 * K64).is_ok_and(|got| same(base, &got));
                    p.reserved_view_reads = first && second;
                    p.lines.push(format!("  the GPU reads the bytes the CPU wrote: {first}; and those it wrote afterwards, with no call in between: {second}"));
                }
                // What the title can still do while the heap stands: take a
                // page back, and unmap the view.
                let decommit = mem::VirtualFree(base.add(4 * K64).cast(), K64, mem::MEM_DECOMMIT) != 0;
                if decommit {
                    mem::VirtualAlloc(base.add(4 * K64).cast(), K64, mem::MEM_COMMIT, mem::PAGE_READWRITE);
                }
                p.lines.push(format!(
                    "  with the heap standing, decommit of one of its pages {decommit} (false: the pages are pinned until the heap goes)"
                ));
                drop(placed);
            }
            drop(opened);
        }
        if !base.is_null() {
            mem::UnmapViewOfFile(view);
        }
        windows_sys::Win32::Foundation::CloseHandle(handle);
    }
}

/// What opening costs, on `mb` MB each of private memory never touched
/// and of a committed section never touched: the time, the working set,
/// the card's account of the PC's memory shared with it; and how fast the
/// card copies out of such a heap against a shared buffer of today's kind.
unsafe fn cost(p: &mut Probe, rig: &mut Rig, memory: &dyn Fn() -> Option<[u64; 4]>, mb: usize) {
    let bytes = mb << 20;
    let shared = || memory().map_or(0, |m| m[3] >> 20);
    let ms = |speed: Option<f64>| speed.map_or("?".to_owned(), |ms| format!("{ms:.1}"));
    // SAFETY (the whole function): the allocation and the section below.
    unsafe {
        let base = mem::VirtualAlloc(std::ptr::null(), bytes, mem::MEM_RESERVE | mem::MEM_COMMIT | MEM_WRITE_WATCH, mem::PAGE_READWRITE).cast::<u8>();
        if !base.is_null() {
            let (set, account, started) = (working_set_mb(), shared(), std::time::Instant::now());
            let opened = open(&rig.device, From::Part(base as usize, bytes));
            let took = started.elapsed().as_secs_f64() * 1000.0;
            let placed = opened.as_ref().ok().and_then(|heap| place(&rig.device, heap, 0, bytes as u64).ok());
            let speed = placed.as_ref().and_then(|buffer| rig.copy_ms(buffer, bytes as u64).ok());
            p.lines.push(format!(
                "cost, {mb} MB of private memory never touched: {}; opened in {took:.1} ms; working set {set} -> {} MB; shared with the card {account} -> {} MB; \
                 the card copies it out in {} ms",
                hr(&opened),
                working_set_mb(),
                shared(),
                ms(speed)
            ));
            drop((placed, opened));
            mem::VirtualFree(base.cast(), 0, mem::MEM_RELEASE);
        }
        let invalid = windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        let handle = mem::CreateFileMappingW(invalid, std::ptr::null(), mem::PAGE_READWRITE | mem::SEC_COMMIT, (bytes as u64 >> 32) as u32, bytes as u32, std::ptr::null());
        if !handle.is_null() {
            let (set, account, started) = (working_set_mb(), shared(), std::time::Instant::now());
            let opened = open(&rig.device, From::Section(handle as usize));
            let took = started.elapsed().as_secs_f64() * 1000.0;
            let (set_opened, account_opened) = (working_set_mb(), shared());
            let placed = opened.as_ref().ok().and_then(|heap| place(&rig.device, heap, 0, bytes as u64).ok());
            let speed = placed.as_ref().and_then(|buffer| rig.copy_ms(buffer, bytes as u64).ok());
            p.lines.push(format!(
                "cost, {mb} MB of a committed section never touched: {}; opened in {took:.1} ms; working set {set} -> {set_opened} -> after the card read all of it {} MB; \
                 shared with the card {account} -> {account_opened} -> {} MB; the card copies it out in {} ms",
                hr(&opened),
                working_set_mb(),
                shared(),
                ms(speed)
            ));
            drop((placed, opened));
            windows_sys::Win32::Foundation::CloseHandle(handle);
        }
        // Today's kind of host copy, for the comparison.
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_CUSTOM,
            CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
            MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        let made = rig.device.CreateCommittedResource(
            &heap,
            D3D12_HEAP_FLAG_NONE,
            &buffer_desc(bytes as u64, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS),
            D3D12_RESOURCE_STATE_COMMON,
            None,
            &mut buffer,
        );
        if let (Ok(()), Some(buffer)) = (made, buffer) {
            let speed = rig.copy_ms(&buffer, bytes as u64).ok();
            p.lines.push(format!("cost, for comparison: the card copies {mb} MB out of a shared buffer of today's kind in {} ms", ms(speed)));
        }
    }
}

// The census: which of the title memory that has host copies today could
// be opened, and by which opening.

/// Counts and bytes by kind, in the order of [`KINDS`].
#[derive(Default, Clone, Copy)]
struct Tally([(u64, u64); 6]);

const KINDS: [&str; 6] = [
    "private, committed whole (a part can be opened)",
    "view memory, one run of a section (one placed buffer)",
    "view memory, scattered pages",
    "view memory with unmapped pages",
    "private but not committed whole, or guarded",
    "other",
];

fn kind_of(at: usize, len: usize, known: &mut Vec<(usize, usize)>) -> usize {
    match cover(at, len) {
        Cover::Run(..) => return 1,
        Cover::Scattered => return 2,
        Cover::Holes => return 3,
        Cover::None => {}
    }
    let end = at + len;
    if known.iter().any(|&(from, to)| from <= at && end <= to) {
        return 0;
    }
    // SAFETY: plain data filled in by the query.
    let mut info: mem::MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { mem::VirtualQuery(at as *const c_void, &mut info, size_of_val(&info)) };
    if n == 0 {
        return 5;
    }
    let reach = (info.BaseAddress as usize).saturating_add(info.RegionSize);
    if info.Type == mem::MEM_PRIVATE && info.State == mem::MEM_COMMIT && info.Protect == mem::PAGE_READWRITE {
        known.push((info.BaseAddress as usize, reach));
        return if reach >= end { 0 } else { 4 };
    }
    match info.Type {
        mem::MEM_PRIVATE => 4,
        mem::MEM_MAPPED => 3,
        _ => 5,
    }
}

/// The census lines for `spans` (start, bytes, whether a window outside
/// buffers).
fn census_of(spans: &[(u64, u64, bool)]) -> Vec<String> {
    let mut known = Vec::new();
    let (mut buffers, mut windows) = (Tally::default(), Tally::default());
    for &(start, len, window) in spans {
        let kind = kind_of(start as usize, len as usize, &mut known);
        let tally = if window { &mut windows } else { &mut buffers };
        tally.0[kind].0 += 1;
        tally.0[kind].1 += len;
    }
    let mut lines = Vec::new();
    for (name, tally) in [("title buffers", buffers), ("windows outside buffers", windows)] {
        let all = tally.0.iter().fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
        lines.push(format!("census, {name}: {} of {} MB", all.0, all.1 >> 20));
        for (kind, &(n, bytes)) in KINDS.iter().zip(&tally.0) {
            if n > 0 {
                lines.push(format!("census,   {kind}: {n} of {} MB", bytes >> 20));
            }
        }
    }
    let v = views();
    let (count, bytes) = (v.views.len(), v.views.values().map(|view| view.0 as u64).sum::<u64>());
    let pages = v.views.values().filter(|view| view.0 == K64).count();
    let big = v.views.values().map(|view| view.0).max().unwrap_or(0);
    lines.push(format!(
        "census, the title's views of physical pages: {count} of {} MB; {pages} of them one 64 KiB page; the longest {} MB",
        bytes >> 20,
        big >> 20
    ));
    for &(_, bytes, committed) in &v.sections {
        lines.push(format!(
            "census, a section of {} MB, {}",
            bytes >> 20,
            if committed { "committed: it can be opened as one heap" } else { "reserved: refused as a heap until it is made with SEC_COMMIT" }
        ));
    }
    lines
}

/// With [`probe_level`] 2: one part of the title's own private memory
/// (the start of the biggest such title buffer, 4 MiB at most) is opened
/// for a moment, a buffer placed, 64 KiB read by the GPU and compared
/// with what the CPU reads there (the title may be writing it meanwhile:
/// the count of equal bytes is given), and all of it released. While the
/// heap stands (milliseconds) a `VirtualFree` of the title's on those
/// pages would fail.
fn real_part(spans: &[(u64, u64, bool)]) -> Vec<String> {
    let Some(host) = crate::host::host() else {
        return Vec::new();
    };
    let mut known = Vec::new();
    let found = spans
        .iter()
        .filter(|s| !s.2 && s.1 >= 2 * K64 as u64)
        .filter(|s| kind_of(s.0 as usize, s.1 as usize, &mut known) == 0)
        .max_by_key(|s| s.1);
    let Some(&(start, len, _)) = found else {
        return vec!["the title's memory: no title buffer in private, committed memory to try".to_owned()];
    };
    let at = (start as usize).next_multiple_of(K64);
    let bytes = ((start + len) as usize).saturating_sub(at).min(4 << 20) & !(K64 - 1);
    if bytes == 0 || !private_whole(at, bytes) {
        return vec!["the title's memory: the buffer found holds no whole 64 KiB page that is still committed".to_owned()];
    }
    let mut lines = Vec::new();
    let (set, started) = (working_set_mb(), std::time::Instant::now());
    let opened = open(&host.device, From::Part(at, bytes));
    lines.push(format!(
        "the title's memory, {} KiB at {at:#x} (of the title buffer {start:#x}+{len:#x}): {}; in {:.1} ms; working set {set} -> {} MB",
        bytes >> 10,
        hr(&opened),
        started.elapsed().as_secs_f64() * 1000.0,
        working_set_mb()
    ));
    if let Ok(heap) = &opened {
        let placed = place(&host.device, heap, 0, bytes as u64);
        lines.push(format!("  a buffer placed over it: {}", hr(&placed)));
        if let (Ok(buffer), Ok(mut rig)) = (&placed, Rig::new(&host.device)) {
            let mut before = vec![0u8; K64];
            crate::host::copy_mapped(before.as_mut_ptr(), at as *const u8, K64, false);
            let got = rig.fetch(buffer, 0, K64);
            let mut after = vec![0u8; K64];
            crate::host::copy_mapped(after.as_mut_ptr(), at as *const u8, K64, false);
            match got {
                Ok(got) => {
                    let equal = (0..K64).filter(|&i| got[i] == before[i] || got[i] == after[i]).count();
                    let zeros = got.iter().filter(|&&b| b == 0).count();
                    lines.push(format!(
                        "  the GPU read 64 KiB of the title's own bytes: {equal} of {K64} equal to what the CPU read just before or just after ({zeros} of them zero)"
                    ));
                }
                Err(e) => lines.push(format!("  the GPU's read failed: {e}")),
            }
        }
    }
    lines
}

/// How many censuses are taken, 30 seconds apart.
const CENSUSES: u64 = 6;

/// Whether the census after `done` earlier ones is due `since` milliseconds
/// after the probe (the first at 30 s, the next every 30 s).
fn census_due(done: u64, since: u64) -> bool {
    done < CENSUSES && since >= 30_000 * (done + 1)
}

/// Called once a second by the thread that submits, before a submission
/// (`host::sweep`): with [`probe_level`], the probe at the first call, the
/// census (on a thread of its own: it asks Windows about every copy) every
/// 30 seconds after it, six times ([`CENSUSES`]).
pub(crate) fn tick() {
    if level() > 0 || pool_on() {
        say();
    }
    if probe_level() == 0 {
        return;
    }
    static STARTED: AtomicU64 = AtomicU64::new(0);
    static DONE: AtomicU64 = AtomicU64::new(0);
    let now = crate::host::now_ms().max(1);
    let started = match STARTED.compare_exchange(0, now, Ordering::Relaxed, Ordering::Relaxed) {
        Ok(_) => {
            let Some(host) = crate::host::host() else {
                return;
            };
            let mb = std::env::var("VICTORY_INPLACE_PROBE_MB").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(64usize);
            let took = std::time::Instant::now();
            let probe = probe(&host.device, &|| host.video_memory(), mb.min(1024));
            for line in &probe.lines {
                tracing::info!("in place probe: {line}");
            }
            tracing::info!("in place probe: took {:.0} ms", took.elapsed().as_secs_f64() * 1000.0);
            now
        }
        Err(started) => started,
    };
    let done = DONE.load(Ordering::Relaxed);
    let due = census_due(done, now.saturating_sub(started));
    if due {
        let which = DONE.fetch_add(1, Ordering::Relaxed);
        std::thread::spawn(move || {
            let spans = crate::shadow::spans();
            for line in census_of(&spans) {
                tracing::info!("in place {line}");
            }
            if which == 0 && probe_level() >= 2 {
                for line in real_part(&spans) {
                    tracing::info!("in place probe: {line}");
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Views as the runtime tells them: a range inside one view, or over
    /// views that continue each other in one section, is one run; over
    /// views from elsewhere it is scattered; over a gap it has holes.
    #[test]
    fn the_views_say_how_much_of_a_range_is_there() {
        let _views = crate::test_views_lock();
        const K64: usize = 64 << 10;
        // Addresses nothing has: only the record is asked.
        let at = 0x7ff1_0000_0000_usize;
        for (block, blocks) in [(0, 1), (1, 1), (4, 2)] {
            super::title_memory_view(at + block * K64, blocks * K64, 7, 0);
        }
        let shown = |from: usize, len: usize| super::shown_bytes(at + from, len);
        assert_eq!(shown(0, 8 * K64), Some(4 * K64 as u64));
        assert_eq!(shown(100, K64), Some(K64 as u64));
        assert_eq!(shown(K64 + 100, 2 * K64), Some(K64 as u64 - 100));
        assert_eq!(shown(5 * K64 + 1, 10), Some(10));
        assert_eq!(shown(2 * K64, 2 * K64), None);
        assert_eq!(shown(6 * K64, 2 * K64), None);
        // The same as runs: views that touch are one, cut to the range.
        let runs = |from: usize, len: usize| super::shown_runs(at + from, len);
        assert_eq!(runs(0, 8 * K64), vec![(at, 2 * K64), (at + 4 * K64, 2 * K64)]);
        assert_eq!(runs(100, K64), vec![(at + 100, K64)]);
        assert_eq!(runs(5 * K64 + 1, 10), vec![(at + 5 * K64 + 1, 10)]);
        assert_eq!(runs(2 * K64, 2 * K64), vec![]);
        super::views_gone(at + K64, K64);
        assert_eq!(shown(0, 2 * K64), Some(K64 as u64));
        assert_eq!(runs(0, 2 * K64), vec![(at, K64)]);
        super::views_gone(at, 8 * K64);
        assert_eq!(shown(0, 8 * K64), None);
    }

    #[test]
    fn views_say_what_one_placed_buffer_could_show() {
        let mut views = BTreeMap::new();
        let at = 0x7_0000_0000usize;
        views.insert(at, (4 * K64, 9, 16 * K64 as u64));
        // Continues the first in the section.
        views.insert(at + 4 * K64, (2 * K64, 9, 20 * K64 as u64));
        // Follows in address, from elsewhere in the section.
        views.insert(at + 6 * K64, (K64, 9, 3 * K64 as u64));
        // After a gap of one page.
        views.insert(at + 8 * K64, (K64, 9, 4 * K64 as u64));
        assert_eq!(cover_in(&views, at + K64, 2 * K64), Cover::Run(9, 17 * K64 as u64));
        assert_eq!(cover_in(&views, at + K64, 5 * K64), Cover::Run(9, 17 * K64 as u64));
        assert_eq!(cover_in(&views, at, 7 * K64), Cover::Scattered);
        assert_eq!(cover_in(&views, at + 6 * K64, 3 * K64), Cover::Holes);
        assert_eq!(cover_in(&views, at + 7 * K64, K64), Cover::None);
        assert_eq!(cover_in(&views, at - K64, 2 * K64), Cover::None);
    }

    /// What the runtime forgets when memory is mapped anew.
    #[test]
    fn remapped_memory_forgets_its_views() {
        let _views = crate::test_views_lock();
        let at = 0x7_4000_0000usize;
        title_memory_view(at, 4 * K64, 77, 0);
        title_memory_view(at + 4 * K64, K64, 77, 4 * K64 as u64);
        assert_eq!(cover(at, 5 * K64), Cover::Run(77, 0));
        // One page inside the first view goes: the whole view is forgotten
        // (the runtime maps and tells again what stays of it).
        views_gone(at + K64, K64);
        assert_eq!(cover(at, K64), Cover::None);
        assert_eq!(cover(at + 4 * K64, K64), Cover::Run(77, 4 * K64 as u64));
        views_gone(at + 4 * K64, K64);
        assert_eq!(cover(at + 4 * K64, K64), Cover::None);
    }

    /// Only private memory committed whole, read-write, is taken for a part.
    #[test]
    fn only_private_memory_committed_whole_is_a_part() {
        // SAFETY: an allocation of the test's own.
        unsafe {
            let base = mem::VirtualAlloc(std::ptr::null(), 8 * K64, mem::MEM_RESERVE, mem::PAGE_NOACCESS).cast::<u8>();
            assert!(!base.is_null());
            assert!(!private_whole(base as usize, K64), "reserved only");
            mem::VirtualAlloc(base.cast(), 4 * K64, mem::MEM_COMMIT, mem::PAGE_READWRITE);
            assert!(private_whole(base as usize, 4 * K64));
            assert!(private_whole(base as usize + K64, 2 * K64));
            assert!(!private_whole(base as usize, 5 * K64), "reaches into the reserved half");
            let mut old = 0;
            mem::VirtualProtect(base.cast(), K64, mem::PAGE_READONLY, &mut old);
            assert!(!private_whole(base as usize, 2 * K64), "a guarded page");
            mem::VirtualFree(base.cast(), 0, mem::MEM_RELEASE);
        }
    }

    pub(crate) fn warp() -> Option<ID3D12Device> {
        use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
        use windows::Win32::Graphics::Dxgi::*;
        // SAFETY: plain DXGI queries and device creation on the software
        // adapter; a card is never opened.
        unsafe {
            let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
            let adapter: IDXGIAdapter = factory.EnumWarpAdapter().ok()?;
            let desc = adapter.cast::<IDXGIAdapter1>().ok()?.GetDesc1().ok()?;
            if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0 {
                return None;
            }
            let mut device: Option<ID3D12Device> = None;
            D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).ok()?;
            device
        }
    }

    /// The probe, whole, on the software adapter: the same calls the card
    /// gets. The runtime's and the kernel's rules hold there; the card's
    /// driver is what a run with `VICTORY_INPLACE_PROBE=1` adds.
    #[test]
    fn warp_probe_opens_title_like_memory_and_the_gpu_sees_its_bytes() {
        let _turn = crate::indirect::tests::warp_turn();
        let Some(device) = warp() else { return };
        if device.cast::<ID3D12Device13>().is_err() {
            eprintln!("no ID3D12Device13 here: parts of an allocation cannot be opened");
            return;
        }
        let probe = probe(&device, &|| None, 4);
        for line in &probe.lines {
            eprintln!("in place probe: {line}");
        }
        assert!(probe.part_reads, "the GPU reads private memory in place");
        assert!(probe.part_writes, "GPU writes land in private memory");
        assert!(probe.section_reads && probe.section_writes, "a committed section in place");
        assert!(probe.section_outlives_views, "the section's heap does not depend on views");
        assert!(probe.reserved_section_refused, "a reserved section is refused");
        assert!(probe.overlap_reads, "two buffers over the same heap pages both read what the CPU wrote");
        assert!(probe.lines.iter().any(|l| l.starts_with("VERDICT: a committed view of a reserved section")));
    }

    /// The census runs every 30 seconds after the probe, six times.
    #[test]
    fn the_census_is_due_every_thirty_seconds_six_times() {
        assert!(!census_due(0, 29_999));
        assert!(census_due(0, 30_000));
        assert!(!census_due(1, 59_999));
        assert!(census_due(1, 60_000));
        assert!(census_due(5, 180_000));
        assert!(!census_due(5, 179_999));
        assert!(!census_due(6, 400_000), "six and no more");
        let due: Vec<u64> = (0..CENSUSES + 3).filter(|&done| census_due(done, 30_000 * (done + 1))).collect();
        assert_eq!(due.len(), 6);
    }

    /// A remap is checked against the record of what is in place: ranges that
    /// overlap an entry hit, also one that began far below; the others miss
    /// and take no table; a hit empties its range and nothing else.
    #[test]
    fn a_remap_is_checked_against_what_is_in_place() {
        let base = 0x7_c000_0000usize;
        note_placed(base + 10 * K64, 4 * K64, true);
        note_placed(base + 100 * K64, K64, false);
        // A long one that begins far below what is asked.
        note_placed(base + 200 * K64, 64 * K64, true);
        let hit = |at: usize, len: usize| placed_overlap(at, at + len, false);
        assert!(hit(base + 10 * K64, 1), "its first byte");
        assert!(hit(base + 13 * K64 + 5, 100), "inside its last page");
        assert!(hit(base + 9 * K64, 2 * K64), "reaching in from below");
        assert!(hit(base + 8 * K64, 20 * K64), "holding it");
        assert!(!hit(base + 14 * K64, 10 * K64), "from its end on");
        assert!(!hit(base + 8 * K64, 2 * K64), "up to its start");
        assert!(!hit(base + 14 * K64, 0), "nothing");
        assert!(hit(base + 250 * K64, 8), "far inside the long one");
        assert!(!hit(base + 264 * K64, 8), "just after it");
        // Only the section heaps' (what a shader's write is asked of).
        assert!(placed_overlap(base + 10 * K64, base + 11 * K64, true));
        assert!(!placed_overlap(base + 100 * K64, base + 101 * K64, true));
        // A remap that hits lets go of its range and nothing else.
        assert_eq!(let_go(base + 12 * K64, K64), 0, "nothing in the tables to take");
        assert!(!hit(base + 10 * K64, 4 * K64));
        assert!(hit(base + 100 * K64, 1) && hit(base + 250 * K64, 8));
        // A miss changes nothing.
        assert_eq!(let_go(base + 50 * K64, K64), 0);
        assert!(hit(base + 100 * K64, 1));
        assert!(forget_placed(base + 100 * K64, base + 101 * K64));
        assert!(forget_placed(base + 220 * K64, base + 221 * K64));
        assert!(!forget_placed(base, base + 400 * K64));
    }

    /// A section made reserved, or views that are no run of one, are no ask
    /// of the host.
    #[test]
    fn a_pool_window_needs_a_run_of_a_committed_section() {
        let _views = crate::test_views_lock();
        let _turn = crate::indirect::tests::warp_turn();
        let Some(device) = warp() else { return };
        let at = 0x7_d000_0000usize;
        // Not committed: never opened (numbers no handle has: handles are
        // multiples of 4, and a real section of a test beside this one may
        // have any of those).
        title_memory_section(0x7e57, 4 * K64 as u64, false);
        title_memory_view(at, K64, 0x7e57, 0);
        assert!(matches!(place_run(&device, at as u64 + 8, 100), Err(None)), "reserved");
        title_memory_section(0x7e5b, 4 * K64 as u64, true);
        title_memory_view(at + 8 * K64, K64, 0x7e5b, 0);
        title_memory_view(at + 10 * K64, K64, 0x7e5b, 2 * K64 as u64);
        assert!(matches!(place_run(&device, (at + 8 * K64) as u64, 2 * K64 as u64), Err(None)), "holes");
        title_memory_view(at + 9 * K64, K64, 0x7e5b, 5 * K64 as u64);
        assert!(matches!(place_run(&device, (at + 8 * K64) as u64, 3 * K64 as u64), Err(None)), "scattered");
        assert!(matches!(place_run(&device, (at + 20 * K64) as u64, 8), Err(None)), "no view");
        views_gone(at, 12 * K64);
        title_memory_section_gone(0x7e57);
        title_memory_section_gone(0x7e5b);
        assert!(views().heaps.iter().all(|h| h.0 != 0x7e57 && h.0 != 0x7e5b));
    }

    /// Stage 1 on the software adapter: a window that is one run of a
    /// committed section is a buffer in the section's heap, at the page that
    /// holds its first byte; the GPU reads what the CPU writes there with no
    /// copy call, two windows that overlap both do, the GPU's write lands at
    /// the title's address, and a remap lets the window go.
    #[test]
    fn warp_a_pool_window_in_a_section_heap_shows_the_cpu_writes_with_no_copy() {
        let _views = crate::test_views_lock();
        let _turn = crate::indirect::tests::warp_turn();
        let Some(device) = warp() else { return };
        // SAFETY: a section, a view and buffers of the test's own, all let go
        // below (the section's heap with the section).
        unsafe {
            let invalid = windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
            let handle = mem::CreateFileMappingW(invalid, std::ptr::null(), mem::PAGE_READWRITE | mem::SEC_COMMIT, 0, (16 * K64) as u32, std::ptr::null());
            assert!(!handle.is_null());
            // The view shows the section's pages 3 to 10.
            let view = mem::MapViewOfFile(handle, mem::FILE_MAP_WRITE, 0, (3 * K64) as u32, 8 * K64);
            let at = view.Value as usize;
            assert_ne!(at, 0);
            let section = handle as usize;
            title_memory_section(section, (16 * K64) as u64, true);
            title_memory_view(at, 8 * K64, section, 3 * K64 as u64);
            let mut rig = Rig::new(&device).unwrap();
            // A window off the page lines, in the view's second page.
            let (a_start, a_len) = ((at + K64 + 0x1230) as u64, K64 as u64 + 100);
            let a = place_run(&device, a_start, a_len).unwrap_or_else(|e| panic!("not placed: {e:?}"));
            assert_eq!(a.bias, 0x1230, "the offset in its page of the section");
            assert_eq!(a.buffer.GetDesc().Width, 2 * K64 as u64);
            assert_eq!(a.gpu, a.buffer.GetGPUVirtualAddress() + 0x1230);
            let first = place_run(&device, at as u64, 8).unwrap();
            assert_eq!(first.bias, 0);
            assert_eq!(first.buffer.GetDesc().Width, K64 as u64);
            for seed in [5, 77] {
                fill(a_start as *mut u8, a_len as usize, seed);
                let got = rig.fetch(&a.buffer, a.bias, a_len as usize).unwrap();
                assert!(same(a_start as *const u8, &got), "the GPU reads what the CPU wrote, seed {seed}, no call between");
            }
            // An overlapping window: both read what is written in the overlap.
            let (b_start, b_len) = (a_start + 0x4000, 3 * K64 as u64);
            let b = place_run(&device, b_start, b_len).unwrap_or_else(|e| panic!("not placed: {e:?}"));
            assert_eq!(a.heap.as_raw(), b.heap.as_raw(), "one heap for the section");
            assert_eq!(views().heaps.iter().filter(|h| h.0 == section).count(), 1);
            fill(b_start as *mut u8, 0x2000, 9);
            let (got_a, got_b) = (rig.fetch(&a.buffer, a.bias + 0x4000, 0x2000).unwrap(), rig.fetch(&b.buffer, b.bias, 0x2000).unwrap());
            assert!(same(b_start as *const u8, &got_a) && same(b_start as *const u8, &got_b));
            // The GPU's write is at the title's address.
            rig.store(&b.buffer, b.bias + 8, &[0xa5; 64]).unwrap();
            assert!(same((b_start + 8) as *const u8, &[0xa5; 64]));
            // No run: nothing is placed.
            assert!(matches!(place_run(&device, (at + 8 * K64) as u64, 100), Err(None)));
            // The title remaps the range: its views are forgotten and what is
            // noted of it goes; nothing is placed there until the runtime
            // tells the views again.
            note_placed(a_start as usize, a_len as usize, true);
            views_gone(at + 2 * K64, K64);
            assert_eq!(let_go(at + 2 * K64, K64), 0);
            assert!(!placed_overlap(a_start as usize, (a_start + a_len) as usize, false), "let go");
            assert!(matches!(place_run(&device, a_start, a_len), Err(None)), "no longer shown");
            drop((a, b, first));
            // The heap goes with the section, not with the views.
            title_memory_section_gone(section);
            assert!(views().heaps.iter().all(|h| h.0 != section));
            mem::UnmapViewOfFile(view);
            windows_sys::Win32::Foundation::CloseHandle(handle);
        }
    }

    /// Stage 1's own call on the software adapter: a title buffer that
    /// begins and ends off the 64 KiB lines is placed over its pages, the
    /// GPU reads the buffer's bytes at the offset the views use, and its
    /// write lands at the title's address. Memory that is not committed
    /// whole is turned down without asking the host.
    #[test]
    fn warp_a_title_buffer_in_place_shows_its_bytes_at_its_offset() {
        let _turn = crate::indirect::tests::warp_turn();
        let Some(device) = warp() else { return };
        if device.cast::<ID3D12Device13>().is_err() {
            return;
        }
        // SAFETY: an allocation of the test's own.
        unsafe {
            let base = mem::VirtualAlloc(std::ptr::null(), 8 * K64, mem::MEM_RESERVE | mem::MEM_COMMIT | MEM_WRITE_WATCH, mem::PAGE_READWRITE).cast::<u8>();
            let (start, len) = (base as u64 + K64 as u64 + 0x1230, 2 * K64 as u64 + 5);
            let placed = place_part(&device, start, len).unwrap_or_else(|e| panic!("not placed: {e:?}"));
            assert_eq!(placed.bias, 0x1230);
            assert_eq!(placed.gpu, placed.buffer.GetGPUVirtualAddress() + 0x1230);
            assert_eq!(placed.buffer.GetDesc().Width, 3 * K64 as u64, "the pages that hold the buffer");
            let mut rig = Rig::new(&device).unwrap();
            fill(start as *mut u8, len as usize, 7);
            let got = rig.fetch(&placed.buffer, placed.bias, len as usize).unwrap();
            assert!(same(start as *const u8, &got), "the GPU reads the title buffer's bytes");
            rig.store(&placed.buffer, placed.bias + 16, &[0xa5; 64]).unwrap();
            assert!(same((start + 16) as *const u8, &[0xa5; 64]), "the GPU's write is at the title's address");
            drop(placed);
            // Half of it decommitted: turned down, the host not asked.
            // The heap lets go of the pages a moment after its last
            // reference, if the adapter is busy with other tests' work.
            let gone = (0..500).any(|_| {
                let done = mem::VirtualFree(base.add(2 * K64).cast(), K64, mem::MEM_DECOMMIT) != 0;
                if !done {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                done
            });
            assert!(gone, "the heap keeps the pages");
            assert!(matches!(place_part(&device, start, len), Err(None)));
            mem::VirtualFree(base.cast(), 0, mem::MEM_RELEASE);
        }
    }

    /// The census names each kind of memory.
    #[test]
    fn census_tells_private_memory_from_views() {
        let _views = crate::test_views_lock();
        // SAFETY: an allocation of the test's own.
        unsafe {
            let base = mem::VirtualAlloc(std::ptr::null(), 8 * K64, mem::MEM_RESERVE | mem::MEM_COMMIT, mem::PAGE_READWRITE) as u64;
            let view = 0x7_8000_0000u64;
            title_memory_view(view as usize, 2 * K64, 5, 0);
            title_memory_view(view as usize + 2 * K64, K64, 5, 9 * K64 as u64);
            let lines = census_of(&[(base, 2 * K64 as u64, false), (base + 4 * K64 as u64, K64 as u64, true), (view, K64 as u64, true), (view, 3 * K64 as u64, false)]);
            let text = lines.join("\n");
            assert!(text.contains("census, title buffers: 2 of"), "{text}");
            assert!(text.contains("census,   private, committed whole (a part can be opened): 1 of"), "{text}");
            assert!(text.contains("census,   view memory, scattered pages: 1 of"), "{text}");
            assert!(text.contains("census,   view memory, one run of a section (one placed buffer): 1 of"), "{text}");
            views_gone(view as usize, 3 * K64);
            mem::VirtualFree(base as *mut c_void, 0, mem::MEM_RELEASE);
        }
    }
}
