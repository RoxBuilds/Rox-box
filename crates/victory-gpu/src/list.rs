//! Command allocators and command lists.
//!
//! The title's inlined code writes packets into a list only while its put
//! pointer is below a limit, and calls the list's method otherwise. Our
//! lists keep the pointer and every limit at zero, so every command reaches
//! the table as a call.
//!
//! Each list records onto a host command list as its methods are called;
//! draw state lives in `crate::draw`. Methods not
//! written yet log once with their caller, so a run lists what to do next.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::mem::offset_of;

use crate::object::{
    self, ALLOCATOR, E_INVALIDARG, HRESULT, Iid, LIST, Object, S_OK, Table, This, guid,
};
use crate::queue::Written;
use crate::resource::PAGEABLE_IIDS;

#[cfg(windows)]
pub(crate) mod clear;
#[cfg(windows)]
pub(crate) mod thin;

const ALLOCATOR_IIDS: [Iid; 4] = [
    guid(
        0x6102dee4,
        0xaf59,
        0x4b09,
        [0xb9, 0x99, 0xb4, 0x4d, 0x73, 0xf0, 0x9b, 0x24],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

/// The list interfaces up to `ID3D12GraphicsCommandList6` (the table has
/// all their slots): those whose ids are the same as on PC, the console's
/// own List5 and List6 (the title asks for List6), then the
/// device child's.
const LIST_IIDS: [Iid; 10] = [
    guid(
        0x7116d91c,
        0xe7e4,
        0x47ce,
        [0xb8, 0xc6, 0xec, 0x81, 0x68, 0xf4, 0x37, 0xe5],
    ),
    guid(
        0x5b160d0f,
        0xac1b,
        0x4185,
        [0x8b, 0xa8, 0xb3, 0xae, 0x42, 0xa5, 0xa4, 0x55],
    ),
    guid(
        0x553103fb,
        0x1fe7,
        0x4557,
        [0xbb, 0x38, 0x94, 0x6d, 0x7d, 0x0e, 0x7c, 0xa7],
    ),
    guid(
        0x38c3e585,
        0xff17,
        0x412c,
        [0x91, 0x50, 0x4f, 0xc6, 0xf9, 0xd7, 0x2a, 0x28],
    ),
    guid(
        0x6fda83a7,
        0xb84c,
        0x4e38,
        [0x9a, 0xc8, 0xc7, 0xbd, 0x22, 0x01, 0x6b, 0x3d],
    ),
    guid(
        0x8754318e,
        0xd3a9,
        0x4541,
        [0x98, 0xcf, 0x64, 0x5b, 0x50, 0xdc, 0x48, 0x74],
    ),
    guid(
        0x55050859,
        0x4024,
        0x474c,
        [0x98, 0xcf, 0x64, 0x5b, 0x51, 0xdc, 0x48, 0x74],
    ),
    guid(
        0xc3827890,
        0x4024,
        0x474c,
        [0x98, 0xcf, 0x64, 0x5b, 0x51, 0xdc, 0x48, 0x74],
    ),
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

#[repr(C)]
struct Allocator {
    object: Object,
    kind: u32,
}

/// The layout the title's inlined code reads.
#[repr(C)]
struct List {
    object: Object,
    kind: u32,
    /// The put pointer, then the plain, draw, API and unbiased API limits:
    /// all zero, so every inline check fails.
    put: [u64; 5],
    reserved: [u32; 64],
    /// State the inline code keeps for building packets; unused by us.
    graphics_root_headers: [u32; 128],
    compute_root_headers: [u32; 128],
    pipeline_shadow: [u32; 8],
    /// Newer titles' lists have a larger inline struct past 0x560 (state
    /// flush writes 32 bytes around +0x544..+0x564); keep our fields clear of
    /// that region.
    newer: [u32; 256],
    /// Ours, after the console layout: memory handed out by
    /// [`allocate_upload`], kept until the list is reset; the host list the
    /// calls record into, and the draw state the title set.
    uploads: std::sync::Mutex<Vec<Upload>>,
    #[cfg(windows)]
    host: Option<crate::host::HostList>,
    #[cfg(windows)]
    state: std::sync::Mutex<crate::draw::State>,
    /// The parts of the recording before `host`, in order, each closed: the
    /// list is cut at its waits for memory values ([`cut`]).
    #[cfg(windows)]
    earlier: Vec<Part>,
    /// The waits recorded before the first work of `host`: the queue stands
    /// at them before it runs `host`.
    #[cfg(windows)]
    gate: Vec<ValueWait>,
    /// With `VICTORY_THIN=1`, of a list the title has: what it wrote down
    /// ([`thin`]; then there is no `host`).
    #[cfg(windows)]
    thin: thin::Front,
    /// What `host` is for the memory line's census of host lists
    /// ([`census`]).
    #[cfg(windows)]
    holding: Holding,
    /// Draws and dispatches the list's last submitted recording bound: the
    /// size of host list it is given at its next reset ([`lazy`]).
    #[cfg(windows)]
    last_binds: u32,
}

/// Host lists held by title lists, for the memory line: open for recording
/// (made, or reset since the list's last submission), and handed over at a
/// submission for the next reset (closed and empty: what [`lazy`] keeps in
/// the pool instead).
#[cfg(windows)]
static HELD: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// Host sides taken by submissions and not yet back ([`Detached`]: held
/// by a queue, in the line of flights, or buried until the bury lag has
/// passed; with [`reset_at_landing`] back at their landing).
#[cfg(windows)]
static DETACHED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Host lists reset at their flight's landing since the start
/// ([`tables_landed`]).
#[cfg(windows)]
static LANDED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Since the start, for want of memory: host lists taken from the reserve
/// ([`reserve_take`]); title lists' host lists whose reset gave up and were
/// let go ([`host_for_reset`]); lists that ran whose reset gave up, let go
/// instead of kept ([`recycle`], [`tables_landed`]); submitted title lists
/// left with none, to take one at their reset ([`hand_over_as`]).
#[cfg(windows)]
static FOR_MEMORY: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// Host lists made because none was idle, since the start: by [`detach`]
/// on the submitting thread, at a reset ([`lazy`]), at a cut, and by the
/// thread that makes them ahead ([`ahead`]).
#[cfg(windows)]
static MADE: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// Host lists that ran and were let go instead of kept idle, since the
/// start: for want of room among the idle ones (those of their size were as
/// many as [`idle_most`] keeps, or the upload memory they keep was), and
/// because their recording bound more draws and dispatches than
/// [`idle_big`]. Where a list is wanted next another is made ([`MADE`]).
#[cfg(windows)]
static LET_GO: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// Where a host list is for [`fresh_list`]'s count of those made.
#[cfg(windows)]
#[derive(Clone, Copy)]
enum For {
    Detach = 0,
    Reset = 1,
    Cut = 2,
    Ahead = 3,
}

/// What a title list's host list is: none, open, or handed over at a
/// submission ([`HELD`]); counted down when the list goes.
#[cfg(windows)]
#[derive(Default)]
struct Holding(std::sync::atomic::AtomicU8);

#[cfg(windows)]
impl Holding {
    const NONE: u8 = 0;
    const OPEN: u8 = 1;
    const HANDED: u8 = 2;

    fn set(&self, now: u8) {
        use std::sync::atomic::Ordering::Relaxed;
        let was = self.0.swap(now, Relaxed);
        if was == now {
            return;
        }
        if was != Self::NONE {
            HELD[usize::from(was - 1)].fetch_sub(1, Relaxed);
        }
        if now != Self::NONE {
            HELD[usize::from(now - 1)].fetch_add(1, Relaxed);
        }
    }

    /// As `host` has one or not.
    fn open_if(&self, has: bool) {
        self.set(if has { Self::OPEN } else { Self::NONE });
    }
}

#[cfg(windows)]
impl Drop for Holding {
    fn drop(&mut self) {
        self.set(Self::NONE);
    }
}

/// A host side taken by a submission, counted in [`DETACHED`] while it lives.
#[cfg(windows)]
struct Taken;

#[cfg(windows)]
impl Taken {
    fn new() -> Taken {
        DETACHED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Taken
    }
}

#[cfg(windows)]
impl Drop for Taken {
    fn drop(&mut self) {
        DETACHED.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// For the memory line, the host lists by where they are: idle small, idle
/// bigger ([`Idle`]); held by title lists open for recording, and handed
/// over at their submission waiting for their reset ([`HELD`]); taken by
/// submissions ([`DETACHED`]); and made since the start because none was
/// idle, by [`detach`], at a reset, at a cut, by the thread ahead ([`MADE`]).
/// What the line says beside them (lists alive) less all of these is the
/// parts of lists cut at their waits, and lists of ours (the thin path's
/// back lists are title lists here).
///
/// Then (9 on): the lists kept back for want of memory now ([`reserve_most`]);
/// since the start, the lists reset at their landing ([`LANDED`]) and, for
/// want of memory ([`FOR_MEMORY`]), those taken from the reserve, title
/// lists' host lists let go after their reset gave up, lists that ran let go
/// instead of kept, submitted lists left with none; and the resets that ran
/// out of memory, those of them that worked after a trim and waits, and those
/// that gave up ([`oom`]).
///
/// Then (18 on): since the start, the lists that ran let go for want of idle
/// room and past the big line ([`LET_GO`]); then since the last call (the
/// memory line's), for its counts per frame: the frames begun
/// (`crate::frame::current`), the lists reset at their landing, those made
/// because none was idle at submissions, at resets and at cuts, those let go
/// for want of idle room and past the big line ([`Since`]); and the most idle
/// at once ([`Classes::take_most`]).
#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) fn census() -> [u64; 28] {
    use std::sync::atomic::Ordering::Relaxed;
    static LAST: std::sync::Mutex<Since> = std::sync::Mutex::new(Since::new());
    let (small, big, reserve, most) = {
        let mut idle = IDLE.lock().unwrap_or_else(|e| e.into_inner());
        (idle.small.len() as u64, idle.big.len() as u64, idle.reserve.len() as u64, idle.take_most() as u64)
    };
    let ran_out = oom::counts();
    let now = [
        LANDED.load(Relaxed),
        MADE[0].load(Relaxed),
        MADE[1].load(Relaxed),
        MADE[2].load(Relaxed),
        LET_GO[0].load(Relaxed),
        LET_GO[1].load(Relaxed),
    ];
    let (frames, per) = LAST.lock().unwrap_or_else(|e| e.into_inner()).take(crate::frame::current(), now);
    [
        small,
        big,
        HELD[0].load(Relaxed),
        HELD[1].load(Relaxed),
        DETACHED.load(Relaxed),
        MADE[0].load(Relaxed),
        MADE[1].load(Relaxed),
        MADE[2].load(Relaxed),
        MADE[3].load(Relaxed),
        reserve,
        LANDED.load(Relaxed),
        FOR_MEMORY[0].load(Relaxed),
        FOR_MEMORY[1].load(Relaxed),
        FOR_MEMORY[2].load(Relaxed),
        FOR_MEMORY[3].load(Relaxed),
        ran_out[0],
        ran_out[1],
        ran_out[2],
        now[4],
        now[5],
        frames,
        per[0],
        per[1],
        per[2],
        per[3],
        per[4],
        per[5],
        most,
    ]
}

/// The counts the memory line gives per frame, as they were at its last
/// call ([`census`]): the frame then, and the counts.
#[cfg(all(windows, any(test, feature = "diagnostics")))]
struct Since {
    frame: u64,
    counts: [u64; 6],
}

#[cfg(all(windows, any(test, feature = "diagnostics")))]
impl Since {
    const fn new() -> Self {
        Since { frame: 0, counts: [0; 6] }
    }

    /// The frames since the last call and what each count grew by meanwhile,
    /// `now` being the counts at `frame`; this call is the last one then.
    fn take(&mut self, frame: u64, now: [u64; 6]) -> (u64, [u64; 6]) {
        let grew = std::array::from_fn(|i| now[i].saturating_sub(self.counts[i]));
        let frames = frame.saturating_sub(self.frame);
        *self = Since { frame, counts: now };
        (frames, grew)
    }
}

/// One part of a title list cut at its waits: the waits the queue stands
/// at, then the host list that runs once they are met.
#[cfg(windows)]
struct Part {
    waits: Vec<ValueWait>,
    host: crate::host::HostList,
}
const _: () = assert!(offset_of!(List, put) == 0x18);
const _: () = assert!(offset_of!(List, pipeline_shadow) + 32 == 0x560);

/// Runs `f` with the host list and draw state of a title list, if there
/// is a host GPU.
#[cfg(windows)]
fn with_state(this: This, f: impl FnOnce(&crate::host::HostList, &mut crate::draw::State)) {
    let Some(list) = host_list(this) else { return };
    #[cfg(feature = "diagnostics")]
    let started = std::time::Instant::now();
    // SAFETY: the title passes one of our lists.
    let state = unsafe { &(*this.cast::<List>()).state };
    f(list, &mut state.lock().unwrap_or_else(|e| e.into_inner()));
    #[cfg(feature = "diagnostics")]
    crate::copystat::recorded(started);
}

/// The submission of `lists` has run: each gives its descriptor tables'
/// slots back ([`crate::draw::State::tables_done`]). A list cut at waits
/// keeps them: its state goes with its last part.
#[cfg(windows)]
pub(crate) fn tables_done(lists: &[This]) {
    for &this in lists {
        if this.is_null() {
            continue;
        }
        // SAFETY: the title passes our lists.
        let list = unsafe { &*this.cast::<List>() };
        if list.earlier.is_empty() {
            list.state.lock().unwrap_or_else(|e| e.into_inner()).tables_done();
        }
    }
}

/// The host list behind a title list, if there is a host GPU.
#[cfg(windows)]
pub(crate) fn host_list<'a>(this: This) -> Option<&'a crate::host::HostList> {
    if this.is_null() {
        return None;
    }
    // SAFETY: the title passes one of our lists.
    unsafe { (*this.cast::<List>()).host.as_ref() }
}

/// The host side of a submitted list, taken from it: a queue that waits
/// for a fence runs it later, and the title may reset the list meanwhile.
/// With it go the upload memory and the dynamic descriptor slots its
/// commands name, kept until it has run.
#[cfg(windows)]
pub(crate) struct Detached {
    host: crate::host::HostList,
    _uploads: Vec<Upload>,
    _state: crate::draw::State,
    _taken: Taken,
}

#[cfg(windows)]
impl Detached {
    pub(crate) fn host(&self) -> &crate::host::HostList {
        &self.host
    }
}

/// The submission of these host sides has run (it has landed): the slots of
/// their descriptor tables go back now, and not with the lists' reset after
/// the bury lag (some 64 submissions, `crate::host` `bury_lag`). Nothing the
/// card runs reads them once their submission has, and held two frames longer
/// they ran the ring of dynamic slots out on a host heap of 1,000,000 views
/// (NVIDIA: up to a sixth of a frame's tables refused, their draws dropped).
/// `VICTORY_TABLES_AT_LANDING=0`: at the reset, as before.
///
/// With [`reset_at_landing`] the host lists are reset now too and kept for
/// reuse ([`reset_landed`]): `lists` is left empty, and what their recordings
/// held waits out the bury lag in one record ([`Landed`]).
#[cfg(windows)]
pub(crate) fn tables_landed(lists: &mut Vec<Detached>) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("VICTORY_TABLES_AT_LANDING").is_none_or(|v| v != "0")) {
        for detached in lists.iter_mut() {
            detached._state.tables_done();
        }
    }
    if reset_at_landing() && pools() {
        reset_landed(std::mem::take(lists), |host| host.reset_ran(true), give_back, crate::host::retire);
    }
}

/// Whether the host lists of a flight are reset when it lands
/// ([`tables_landed`]; `VICTORY_RESET_AT_LANDING`, on unless `0`, read once;
/// `0`: after the bury lag, by [`recycle`], as before).
///
/// A list that ran waited for the bury lag (some 64 submissions) in the line
/// of flights before its reset, holding its allocators, and others were made
/// meanwhile: a third of the host lists alive were landed and buried, thousands
/// were made at submissions within minutes, dozens a frame (over a hundred
/// milliseconds), and a list's allocators keep some 2 MB of the process's
/// memory. What the lag is for (memory a stale descriptor slot may still name)
/// is what the recording held, not the allocators: that waits out the lag as
/// before ([`Landed`]), and the list is used again at once. Nothing
/// still in the air is reset: only the lists of the flight that landed.
#[cfg(windows)]
fn reset_at_landing() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("VICTORY_RESET_AT_LANDING").is_none_or(|v| v != "0");
        tracing::info!(on, "host lists reset when their flight lands (VICTORY_RESET_AT_LANDING)");
        on
    })
}

/// What the host sides of a landed flight leave for the bury lag
/// ([`reset_landed`]), let go as one when it has passed (`crate::host::retire`):
/// what each recording held (`crate::host::HostList::reset_ran`), the title's
/// upload memory and the draw state that went with it, and the host lists not
/// kept for reuse. Nothing of it is let go earlier than when the lists were
/// reset after the lag.
#[cfg(windows)]
struct Landed {
    _recordings: Vec<Box<dyn std::any::Any>>,
    _uploads: Vec<Vec<Upload>>,
    _states: Vec<crate::draw::State>,
    _let_go: Vec<crate::host::HostList>,
}

/// [`tables_landed`]'s resets: each host list of the landed flight is reset
/// (`reset`: `crate::host::HostList::reset_ran`, which gives back what its
/// recording held) and given to `back` with the binds of its recording, which
/// keeps it (else it comes back); `bury` takes the rest ([`Landed`]). A list
/// whose reset ran out of memory is not kept.
#[cfg(windows)]
fn reset_landed(
    lists: Vec<Detached>,
    mut reset: impl FnMut(&crate::host::HostList) -> (Option<Box<dyn std::any::Any>>, windows::core::Result<()>),
    mut back: impl FnMut(crate::host::HostList, u32) -> Option<crate::host::HostList>,
    bury: impl FnOnce(Landed),
) {
    use std::sync::atomic::Ordering::Relaxed;
    let mut landed = Landed {
        _recordings: Vec::with_capacity(lists.len()),
        _uploads: Vec::new(),
        _states: Vec::with_capacity(lists.len()),
        _let_go: Vec::new(),
    };
    for Detached { host, _uploads, _state, _taken } in lists {
        // No longer the submission's.
        drop(_taken);
        let binds = _state.binds();
        let (recording, done) = reset(&host);
        landed._recordings.extend(recording);
        if !_uploads.is_empty() {
            landed._uploads.push(_uploads);
        }
        landed._states.push(_state);
        let not_kept = match done {
            Ok(()) => {
                LANDED.fetch_add(1, Relaxed);
                back(host, binds)
            }
            Err(_) => {
                FOR_MEMORY[2].fetch_add(1, Relaxed);
                Some(host)
            }
        };
        landed._let_go.extend(not_kept);
    }
    bury(landed);
}

/// Keeps `host`, a host list that ran and was reset, whose recording bound
/// `binds` draws and dispatches: in the reserve while it is short, else with
/// the idle ones if there is room ([`Classes::give`]); else it comes back,
/// counted ([`let_go`]).
///
/// The bytes the idle ones keep between them are the upload memory a list
/// holds after its reset ([`crate::host::HostList::scratch_kept`]): none while
/// lists share their scratch chunks (`VICTORY_SCRATCH_POOL`, the default), for
/// the reset gave them up, so then the bytes let none of the lists go that
/// [`idle_most`] keeps; with `VICTORY_SCRATCH_POOL=0` a list keeps its own
/// chunks of 4 MiB, and the bytes hold 16 such lists on a PC of 24 GB or
/// less, as before. What the list's allocators keep is not counted there.
#[cfg(windows)]
fn give_back(host: crate::host::HostList, binds: u32) -> Option<crate::host::HostList> {
    let keep = idle_keep();
    let bytes = host.scratch_kept();
    let back = IDLE.lock().unwrap_or_else(|e| e.into_inner()).give(Idle(host), binds, bytes, keep);
    if back.is_some() {
        let_go(binds, keep);
    }
    back.map(|Idle(host)| host)
}

/// Counts a host list that ran and is let go instead of kept ([`LET_GO`]):
/// past the big line, or for want of room.
#[cfg(windows)]
fn let_go(binds: u32, keep: Keep) {
    LET_GO[usize::from(keep.past_big(binds))].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Host lists that have run and wait to be used again, each with the bytes
/// of upload memory it keeps, by the size of the recording they held last:
/// those that bound at most `small` draws and dispatches ([`Keep`]), and
/// bigger ones. A host list keeps what its command allocator grew to for the
/// longest recording it held, so a recording takes one of its own size
/// first ([`Classes::take`]), and few big ones are kept.
///
/// Before them, while it is short, the reserve: lists kept back for want of
/// memory, which only [`Classes::take_reserve`] takes ([`reserve_most`]).
#[cfg(windows)]
struct Classes<T> {
    small: Vec<(T, u64)>,
    big: Vec<(T, u64)>,
    bytes: u64,
    reserve: Vec<T>,
    /// The most idle ones at once since [`Classes::take_most`].
    most: usize,
}

/// What [`Classes`] keep: lists of recordings of at most `small` binds up
/// to `lists` (`small` 0: every list counts as small, one pool as before);
/// bigger ones up to a quarter of `lists`, and none of more than `big`
/// binds (0: no such limit); and no more than `bytes` of upload memory
/// between them. In the reserve up to `reserve` lists of small recordings
/// that keep no upload memory.
#[cfg(windows)]
#[derive(Clone, Copy, Debug)]
struct Keep {
    lists: usize,
    bytes: u64,
    small: u32,
    big: u32,
    reserve: usize,
}

#[cfg(windows)]
impl Keep {
    fn is_small(&self, binds: u32) -> bool {
        self.small == 0 || binds <= self.small
    }

    /// Whether a list that last bound `binds` is past the big line, never
    /// kept.
    fn past_big(&self, binds: u32) -> bool {
        self.big > 0 && binds > self.big
    }

    /// The most lists kept of recordings bigger than `small`.
    fn bigger(&self) -> usize {
        (self.lists / 4).max(1)
    }
}

#[cfg(windows)]
impl<T> Classes<T> {
    const fn new() -> Self {
        Classes { small: Vec::new(), big: Vec::new(), bytes: 0, reserve: Vec::new(), most: 0 }
    }

    /// The idle lists (not those of the reserve).
    fn len(&self) -> usize {
        self.small.len() + self.big.len()
    }

    /// The most idle lists there were at once since the last call (or the
    /// start), for the memory line; from now on counted from those there are.
    #[cfg_attr(not(feature = "diagnostics"), allow(dead_code))]
    fn take_most(&mut self) -> usize {
        let now = self.len();
        std::mem::replace(&mut self.most, now)
    }

    /// Whether the reserve is short and a list that last bound `binds` would
    /// go there.
    fn short(&self, binds: u32, keep: Keep) -> bool {
        self.reserve.len() < keep.reserve && keep.is_small(binds)
    }

    /// Whether a list that last bound `binds` would be kept (before the
    /// bytes it keeps are known).
    fn room(&self, binds: u32, keep: Keep) -> bool {
        self.short(binds, keep) || self.fits(binds, keep)
    }

    /// Whether the idle lists have room for a list that last bound `binds`.
    fn fits(&self, binds: u32, keep: Keep) -> bool {
        if keep.past_big(binds) {
            return false;
        }
        if keep.is_small(binds) { self.small.len() < keep.lists } else { self.big.len() < keep.bigger() }
    }

    /// Keeps `item`, whose recording bound `binds` and which keeps `bytes`
    /// of upload memory, if there is room for it: else it comes back.
    fn give(&mut self, item: T, binds: u32, bytes: u64, keep: Keep) -> Option<T> {
        if bytes == 0 && self.short(binds, keep) {
            self.reserve.push(item);
            return None;
        }
        if !self.fits(binds, keep) || self.bytes + bytes > keep.bytes {
            return Some(item);
        }
        self.bytes += bytes;
        if keep.is_small(binds) {
            self.small.push((item, bytes));
        } else {
            self.big.push((item, bytes));
        }
        self.most = self.most.max(self.len());
        None
    }

    /// A list for a recording like one of `binds` (`None`: not known, as a
    /// small one): of its own size if there is one, else of the other
    /// (making a list costs more than the memory a bigger one keeps), the
    /// one given back last.
    fn take(&mut self, binds: Option<u32>, keep: Keep) -> Option<T> {
        let big = binds.is_some_and(|b| !keep.is_small(b));
        let (own, other) = if big { (&mut self.big, &mut self.small) } else { (&mut self.small, &mut self.big) };
        let (item, bytes) = own.pop().or_else(|| other.pop())?;
        self.bytes -= bytes;
        Some(item)
    }

    /// One of the reserve, the one given back last.
    fn take_reserve(&mut self) -> Option<T> {
        self.reserve.pop()
    }
}

/// A host list in [`IDLE`].
#[cfg(windows)]
struct Idle(crate::host::HostList);

// SAFETY: D3D12 objects are free-threaded, and an idle list is touched by
// nobody until it is taken out again.
#[cfg(windows)]
unsafe impl Send for Idle {}

/// Host lists that have run, reset and ready to record: the host side of
/// a list a queue held, or of a part of a list cut at a wait, is taken
/// from the title's list at its submission, and the title's list needs
/// another. Each was made new and thrown away after its run: a hundred a
/// frame, each two command allocators and two lists, and for
/// four in ten a budget and an argument buffer (tens of milliseconds a
/// frame, and the graphics kernel's time to free them).
#[cfg(windows)]
static IDLE: std::sync::Mutex<Classes<Idle>> = std::sync::Mutex::new(Classes::new());

/// What [`IDLE`] keeps ([`idle_most`], [`small_binds`], [`idle_big`],
/// [`reserve_most`]).
#[cfg(windows)]
fn idle_keep() -> Keep {
    static KEEP: std::sync::OnceLock<Keep> = std::sync::OnceLock::new();
    *KEEP.get_or_init(|| {
        let (lists, bytes) = idle_most();
        Keep { lists, bytes, small: small_binds(), big: idle_big(), reserve: reserve_most() }
    })
}

/// How many host lists are kept back for want of memory
/// (`VICTORY_LIST_RESERVE`, 16; 0: none, as before; read once): reset and
/// open, of small recordings, taken only where a host list could not be
/// made or reset for want of memory ([`reserve_take`]). The reserve is filled
/// before the idle lists from the lists that come back after they ran, at a
/// flight's landing or in [`recycle`] (and by the thread of
/// `VICTORY_LIST_AHEAD`), so nothing is made for it where a list is wanted.
///
/// A run on a PC short of memory ended on a reset that ran out of memory ("a
/// host command list did not reset"), milliseconds after the second of two
/// lists that could not close for want of memory was dropped from its
/// submission.
#[cfg(windows)]
fn reserve_most() -> usize {
    static MOST: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *MOST.get_or_init(|| {
        let n = std::env::var("VICTORY_LIST_RESERVE").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(16);
        tracing::info!(lists = n, "host lists kept back for want of memory (VICTORY_LIST_RESERVE)");
        n
    })
}

/// One of the host lists kept back for want of memory ([`reserve_most`]),
/// reset and open as a new one is: for a making of a host list (`crate::host`
/// `Host::make_list`) or a title list's reset ([`host_for_reset`]) that ran
/// out of memory.
#[cfg(windows)]
pub(crate) fn reserve_take() -> Option<crate::host::HostList> {
    let taken = IDLE.lock().unwrap_or_else(|e| e.into_inner()).take_reserve();
    if taken.is_some() {
        let n = FOR_MEMORY[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        if n <= 8 || n.is_power_of_two() {
            tracing::warn!("host lists taken from those kept back for want of memory (VICTORY_LIST_RESERVE): {n}");
        }
    }
    taken.map(|Idle(host)| host)
}

/// What a host list's reset does when it runs out of memory
/// (`crate::host::HostList::try_reset`; `VICTORY_OOM_FATAL=1`: the run ends
/// at once, as before). Such a reset ended a run on a PC short of memory.
/// Now what is idle is let go
/// (`crate::host::emergency_trim`, at most every 250 ms) and the reset is
/// tried again at once, then after waits of 1, 2, 4 ... ms, for up to
/// `VICTORY_OOM_RETRY_MS` (250) in all, while the page file may grow; in the
/// second after a reset that gave up the others do not wait, so that a run
/// out of memory is not held up a quarter of a second a list. Lists that ran
/// do not wait at all ([`recycle`], [`tables_landed`]): they are not needed,
/// and letting one go frees its memory. What still fails comes back to the
/// caller, which lets the list go ([`host_for_reset`] takes another).
#[cfg(windows)]
pub(crate) mod oom {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

    /// After a reset gave up, the others wait no longer than this (ms).
    const QUIET_MS: u64 = 1000;

    /// `VICTORY_OOM_RETRY_MS` (250), read once: how long the reset of a title
    /// list, or of a list of ours, that runs out of memory is tried again.
    pub(crate) fn retry_ms() -> u64 {
        static MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        *MS.get_or_init(|| {
            std::env::var("VICTORY_OOM_RETRY_MS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(250)
        })
    }

    /// When a reset last gave up (`crate::host::now_ms`, at least 1; 0: none).
    static GAVE_UP: AtomicU64 = AtomicU64::new(0);

    /// Resets that ran out of memory since the start; of them those that
    /// worked after the trim and the waits, and those that gave up.
    static RAN_OUT: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

    /// How long a reset that ran out of memory may be tried again now, asked
    /// for `asked` ms ([`patience_at`]).
    pub(crate) fn patience(asked: u64) -> u64 {
        patience_at(crate::host::now_ms(), GAVE_UP.load(Relaxed), asked)
    }

    /// `asked`, or none in the [`QUIET_MS`] after `gave_up` (0: never).
    pub(super) fn patience_at(now: u64, gave_up: u64, asked: u64) -> u64 {
        if gave_up != 0 && now < gave_up.saturating_add(QUIET_MS) { 0 } else { asked }
    }

    /// The wait before the next try, `waited` ms into `patience`, after
    /// `waits` waits: twice the one before from 1 ms on, at most what is left
    /// of `patience`; `None` once nothing is.
    pub(crate) fn next_wait(waited: u64, patience: u64, waits: u32) -> Option<u64> {
        (waited < patience).then(|| (1u64 << waits.min(16)).min(patience - waited))
    }

    /// A reset ran out of memory and, after `waited` ms of tries, worked
    /// (`failed` none) or gave up: counted, and said, the first eight and
    /// then at powers of two.
    pub(crate) fn ran_out(failed: Option<&windows::core::Error>, waited: u64) {
        let n = RAN_OUT[0].fetch_add(1, Relaxed) + 1;
        let worked = RAN_OUT[1].fetch_add(u64::from(failed.is_none()), Relaxed) + u64::from(failed.is_none());
        let gave_up = RAN_OUT[2].fetch_add(u64::from(failed.is_some()), Relaxed) + u64::from(failed.is_some());
        if failed.is_some() {
            GAVE_UP.store(crate::host::now_ms().max(1), Relaxed);
        }
        if n <= 8 || n.is_power_of_two() {
            tracing::warn!(
                "host list resets that ran out of memory: {n}, {worked} of them reset after letting go of what \
                 was idle and waiting, {gave_up} let go (this one {} after {waited} ms; VICTORY_OOM_RETRY_MS, \
                 VICTORY_OOM_FATAL=1: fatal)",
                failed.map_or_else(|| "reset".to_owned(), |e| format!("gave up ({e})"))
            );
        }
    }

    /// [`RAN_OUT`], for the memory line.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn counts() -> [u64; 3] {
        RAN_OUT.each_ref().map(|n| n.load(Relaxed))
    }

    /// Diagnostics: whether this reset fails for want of memory, all its
    /// tries, without asking the host (`VICTORY_OOM_INJECT=<n>`: every n-th
    /// reset of a title list or of one that ran, counted here, for the
    /// switch's other sites count their own; unset or 0: none; read once).
    #[cfg(feature = "diagnostics")]
    pub(crate) fn injected() -> bool {
        static EVERY: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        static RESETS: AtomicU64 = AtomicU64::new(0);
        let every = *EVERY.get_or_init(|| {
            let every = inject_every(std::env::var("VICTORY_OOM_INJECT").ok().as_deref());
            if every > 0 {
                tracing::warn!(every, "every n-th host list reset fails for want of memory (VICTORY_OOM_INJECT)");
            }
            every
        });
        every > 0 && every_nth(every, RESETS.fetch_add(1, Relaxed) + 1)
    }

    /// `VICTORY_OOM_INJECT`'s value as a number (0: none).
    #[cfg(any(test, feature = "diagnostics"))]
    pub(super) fn inject_every(value: Option<&str>) -> u64 {
        value.and_then(|v| v.trim().parse().ok()).unwrap_or(0)
    }

    /// Whether the `count`-th (from 1) is one of every `every` (0: none).
    #[cfg(any(test, feature = "diagnostics"))]
    pub(super) fn every_nth(every: u64, count: u64) -> bool {
        every > 0 && count.is_multiple_of(every)
    }
}

/// The recordings whose host lists are kept as small ones ([`Classes`]):
/// those that bound at most this many draws and dispatches
/// (`VICTORY_IDLE_SMALL`, 256; 0: one pool for all, as before).
#[cfg(windows)]
fn small_binds() -> u32 {
    static SMALL: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *SMALL.get_or_init(|| std::env::var("VICTORY_IDLE_SMALL").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(256))
}

/// Whether a title list holds no host list from its submission to its
/// reset (`VICTORY_LAZY_LISTS`; by default on PCs of 24 GiB or less,
/// [`lazy_for`]; off: each submitted list is given a new, closed one at
/// once, as before). [`detach`] gave every list of every submission another
/// host list on the submitting thread, which then sat in the title's list,
/// closed and empty, until the title reset it: thousands of host lists were
/// alive and over a hundred idle, dozens made a frame on the submitting
/// thread, and the process's private memory grew by gigabytes; hundreds
/// were held so. With this on, the reset takes one, of the size the list's
/// last
/// recording was ([`Classes::take`]), on the thread that records; one that
/// is never reset again holds none.
#[cfg(windows)]
fn lazy() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let set = std::env::var_os("VICTORY_LAZY_LISTS");
        let ram = crate::host::total_physical_ram();
        let on = lazy_for(set.as_deref(), ram);
        tracing::info!(
            on,
            set = set.is_some(),
            ram_gib = ram.unwrap_or(0) >> 30,
            "host lists taken at the title's reset (VICTORY_LAZY_LISTS; on by default on PCs of 24 GiB or less)"
        );
        on
    })
}

/// [`lazy`] for what `VICTORY_LAZY_LISTS` is set to and the PC's RAM: a
/// setting wins (on unless `0`, as `crate::switch::on`), else on with 24 GiB
/// or less, as [`idle_most`] tells PCs apart; off where the RAM is not known.
#[cfg(windows)]
fn lazy_for(set: Option<&std::ffi::OsStr>, ram: Option<u64>) -> bool {
    match set {
        Some(value) => value != "0",
        None => ram.is_some_and(|bytes| bytes <= 24 << 30),
    }
}

/// The most host lists kept idle (128 were too few: flights land several at
/// a time, so lists were thrown away and others made in the same frame, with
/// their preludes and buffers, an eighth of the submitting thread's time),
/// and the most upload memory they may keep
/// between them (a list keeps scratch chunks over a reset only while lists
/// do not share them, `VICTORY_SCRATCH_POOL=0`, [`give_back`]): 512 and
/// 256 MiB, and on a PC of 24 GB or less 384 lists and 64 MiB
/// ([`idle_most_for`]). `VICTORY_IDLE_LISTS` sets the number of lists.
#[cfg(windows)]
fn idle_most() -> (usize, u64) {
    static MOST: std::sync::OnceLock<(usize, u64)> = std::sync::OnceLock::new();
    *MOST.get_or_init(|| {
        let small = crate::host::total_physical_ram().is_some_and(|bytes| bytes <= 24 << 30);
        let set = std::env::var("VICTORY_IDLE_LISTS").ok().and_then(|v| v.trim().parse().ok());
        let (lists, bytes) = idle_most_for(small, set);
        tracing::info!(lists, mb = bytes >> 20, small, "host lists kept idle at most");
        (lists, bytes)
    })
}

/// [`idle_most`] on a PC of 24 GB or less (`small`) or not, with the number
/// of lists `set` (`VICTORY_IDLE_LISTS`) if it is.
///
/// On such a PC the idle lists were 128 (and a quarter of that bigger): an
/// idle list keeps what its command allocators grew to for the longest
/// recording they held, and on a PC short of memory that memory is paged out
/// and in again (thousands of host lists alive, most of the process's private
/// memory paged out, and the submitting thread's work taking four times as
/// long). Since host lists are reset when their flight lands
/// ([`reset_at_landing`]), every list of a frame comes back to the idle ones
/// before the title's resets take it again, and a title records a few hundred a
/// frame: what did not fit was let go (its allocators kept for the bury lag
/// with its recording all the same) and as many were made at the next resets
/// ([`MADE`]), each with its prelude made again at its first submission on the
/// submitting thread (`Host::try_prelude`: a tenth of every frame). 384 hold a
/// frame of the titles seen.
///
/// What an idle list costs, measured on one 8 GB card (host driver,
/// 100 lists): made, some 700 KB of the process's commit (205 KB of it in the
/// PC's memory shared with the card); after recordings of 2,000 commands 810
/// KB, of 16,000 1.3 MB (some 38 bytes a command); its prelude another 560 KB
/// (61 KB shared). Let go, it gives back some 550 KB, and the driver keeps the
/// shared part for the next list made. So the 320 more idle lists cost at most
/// some 470 MB of commit (90 MB of it shared) with every one of them full and
/// with a prelude; as long as they hold no more than the lists of a frame that
/// used to be let go and made again, far less.
#[cfg(windows)]
fn idle_most_for(small: bool, set: Option<usize>) -> (usize, u64) {
    let (lists, bytes) = if small { (384, 64 << 20) } else { (512, 256 << 20) };
    (set.unwrap_or(lists), bytes)
}

/// Host lists kept idle now ([`IDLE`]), for the memory line.
#[cfg(windows)]
#[cfg_attr(not(feature = "diagnostics"), allow(dead_code))]
pub(crate) fn idle_count() -> usize {
    IDLE.lock().unwrap_or_else(|e| e.into_inner()).len()
}

/// A host list whose recording bound more draws and dispatches than this is
/// not kept idle but let go (`VICTORY_IDLE_BIG`; 0: every one is kept): its
/// allocators keep what that recording took, and the next title list to
/// take it may record a dozen draws. 2048, as before ([`idle_big_for`]);
/// `VICTORY_IDLE_BIG=512` to measure on a PC with little memory.
#[cfg(windows)]
fn idle_big() -> u32 {
    static BIG: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *BIG.get_or_init(|| {
        idle_big_for(std::env::var("VICTORY_IDLE_BIG").ok().and_then(|v| v.trim().parse().ok()))
    })
}

/// [`idle_big`] for a setting: a setting wins, else 2048 on every PC. On
/// a PC short of memory the memory big idle lists keep is paged out and in
/// again (thousands of host lists alive, most of the process's private
/// memory paged out); but the idle lists are at most
/// 64 MB there already (`idle_most`), and each big list let go is made again
/// on the submitting thread, the frame's wall: a lower line waits for a run
/// that measures it (the memory line's idle small and bigger lists).
#[cfg(windows)]
fn idle_big_for(set: Option<u32>) -> u32 {
    set.unwrap_or(2048)
}

/// Whether host lists that have run are used again (on by default;
/// `VICTORY_LIST_POOL=0`: each is made new and thrown away, as before).
#[cfg(windows)]
fn pools() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_LIST_POOL").is_none_or(|v| v != "0"))
}

/// A host list open for recording: one that has run, as a title's list is
/// after its reset, or a new one (counted by what it is `made_for`; where
/// none can be made for want of memory, one of the reserve, `crate::host`
/// `Host::make_list`). `binds`: the draws and dispatches of the recording it
/// is for, if known ([`Classes::take`]).
#[cfg(windows)]
fn fresh_list(binds: Option<u32>, made_for: For) -> Option<crate::host::HostList> {
    let idle = IDLE.lock().unwrap_or_else(|e| e.into_inner()).take(binds, idle_keep());
    ahead::wake();
    match idle {
        Some(Idle(list)) => Some(list),
        None => {
            let made = crate::host::host().and_then(|h| h.new_list().ok());
            if made.is_some() {
                MADE[made_for as usize].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            made
        }
    }
}

/// The host sides of lists whose submission has run (submissions are
/// waited for; `ran` names the ones it ran, by their place in `lists`):
/// what they held is let go, as before, and the host lists are reset and
/// kept for [`fresh_list`] (or in the reserve, [`give_back`]). (With
/// [`reset_at_landing`] the lists of flights were reset at their landing,
/// and those after the bury lag come here empty.)
#[cfg(windows)]
pub(crate) fn recycle(lists: Vec<Detached>, ran: &[usize]) {
    if !pools() {
        return;
    }
    let keep = idle_keep();
    for (i, Detached { host, _uploads, _state, .. }) in lists.into_iter().enumerate() {
        // The descriptor slots and the upload memory its commands named.
        let binds = _state.binds();
        drop(_state);
        drop(_uploads);
        if !ran.contains(&i) {
            continue;
        }
        if !IDLE.lock().unwrap_or_else(|e| e.into_inner()).room(binds, keep) {
            let_go(binds, keep);
            continue;
        }
        // As the title's reset does: what the list kept for its commands
        // goes, its allocators stay. One whose reset runs out of memory is
        // let go instead: it is not needed, and that frees its memory.
        if host.reset_ran(false).1.is_err() {
            FOR_MEMORY[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            continue;
        }
        // (One not kept goes outside the lock.)
        drop(give_back(host, binds));
    }
}

/// Small host lists kept ready by a thread of ours, so that none is made
/// where one is wanted (the submitting thread, without [`lazy`]):
/// `VICTORY_LIST_AHEAD=<n>` keeps `n` (at most the idle pool's lists; 0 or
/// unset: none, as before). The thread runs below normal priority and
/// makes lists while fewer than `n` small ones are idle (filling the reserve
/// first, [`reserve_most`]).
#[cfg(windows)]
mod ahead {
    use super::{FOR_MEMORY, For, IDLE, Idle, MADE, idle_keep, pools};

    fn most() -> usize {
        static MOST: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
        *MOST.get_or_init(|| {
            let n = std::env::var("VICTORY_LIST_AHEAD").ok().and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(0);
            let n = if pools() { n.min(idle_keep().lists) } else { 0 };
            if n > 0 {
                tracing::info!(lists = n, "small host lists kept ready by a thread of ours (VICTORY_LIST_AHEAD)");
            }
            n
        })
    }

    /// Told when a list is taken from the idle ones.
    static WAKE: (std::sync::Mutex<bool>, std::sync::Condvar) = (std::sync::Mutex::new(false), std::sync::Condvar::new());

    /// A list was taken: the thread looks whether it has lists to make
    /// (started on the first call).
    pub(super) fn wake() {
        let most = most();
        if most == 0 {
            return;
        }
        static STARTED: std::sync::Once = std::sync::Once::new();
        STARTED.call_once(|| {
            let spawned = std::thread::Builder::new().name("victory-lists-ahead".into()).spawn(move || run(most));
            if let Err(e) = spawned {
                tracing::warn!(error = %e, "no thread to make host lists ahead");
            }
        });
        *WAKE.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
        WAKE.1.notify_one();
    }

    fn run(most: usize) {
        use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL};
        // SAFETY: plain calls on this thread.
        unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL) };
        let Some(host) = crate::host::host() else {
            return;
        };
        loop {
            {
                let mut woken = WAKE.0.lock().unwrap_or_else(|e| e.into_inner());
                while !*woken {
                    woken = WAKE.1.wait(woken).unwrap_or_else(|e| e.into_inner());
                }
                *woken = false;
            }
            loop {
                let small = IDLE.lock().unwrap_or_else(|e| e.into_inner()).small.len();
                if small >= most {
                    break;
                }
                let reserved = FOR_MEMORY[0].load(std::sync::atomic::Ordering::Relaxed);
                let Ok(list) = host.new_list() else {
                    break;
                };
                // A new list is open and empty, as an idle one: of the small
                // ones (the reserve first while it is short). One of the
                // reserve, which a making that ran out of memory gives, goes
                // back there, and none is made now.
                let back = IDLE.lock().unwrap_or_else(|e| e.into_inner()).give(Idle(list), 0, 0, idle_keep());
                if FOR_MEMORY[0].load(std::sync::atomic::Ordering::Relaxed) != reserved {
                    break;
                }
                MADE[For::Ahead as usize].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if back.is_some() {
                    break;
                }
            }
        }
    }
}

/// A wait for a memory value recorded into list `this`, with waits cut
/// into lists (`crate::queue::cuts`). On the console the queue runs the
/// list up to the wait, stands there until the value is in memory, and
/// runs on; the list's bottom-of-pipe writes before the wait are done by
/// then, and other queues may be waiting for exactly those. So the
/// recording is cut here: the host list so far is closed and kept as a
/// part of its own ([`Part`]), the title's calls record into a new host
/// list from here on, and the wait stands before that one. A submission
/// then gives the queue the parts in order ([`detach_parts`]).
///
/// A wait before any work (the first call after a reset, or a wait right
/// after another) cuts nothing: it joins the waits before the current
/// host list.
/// shortcut: a bottom-of-pipe write is done when its part has run, not at
/// its place in the part: a queue waiting for it starts later than on the
/// console, never earlier, and nothing can wait for itself that way (a
/// part has no wait after its first work).
#[cfg(windows)]
fn cut(this: This, wait: ValueWait) {
    if this.is_null() {
        return;
    }
    // SAFETY: the title passes one of our lists; a list is recorded by one
    // thread at a time and is not submitted while it records.
    let list = unsafe { &mut *this.cast_mut().cast::<List>() };
    let Some(current) = list.host.as_ref() else {
        return;
    };
    if current.untouched() {
        list.gate.push(wait);
        return;
    }
    let Some(fresh) = fresh_list(None, For::Cut) else {
        // shortcut: without a new host list the wait stands before the part
        // it was recorded in.
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!("no host list for the part after a wait: the wait moves to the part's start"));
        list.gate.push(wait);
        return;
    };
    current.close();
    static CUTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if CUTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 4 {
        tracing::info!(parts = list.earlier.len() + 2, "a list is cut at a wait for a memory value");
    }
    if let Some(done) = list.host.replace(fresh) {
        let waits = std::mem::replace(&mut list.gate, vec![wait]);
        list.earlier.push(Part { waits, host: done });
    }
    // What the title set so far, on the new host list.
    if let Some(host) = list.host.as_ref() {
        list.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replay(host);
    }
}

/// Whether list `this` recorded a wait for a memory value (with waits cut
/// into lists).
#[cfg(windows)]
pub(crate) fn cut_list(this: This) -> bool {
    if this.is_null() {
        return false;
    }
    // SAFETY: one of our lists, closed and submitted: nothing records
    // into it.
    let list = unsafe { &*this.cast::<List>() };
    !list.earlier.is_empty() || !list.gate.is_empty()
}

/// What a queue is given for submitted lists that were cut at their waits:
/// the waits it stands at, the host lists it runs once they are met, and
/// the order those run in.
#[cfg(windows)]
pub(crate) type Parts = Vec<(Vec<ValueWait>, Vec<Detached>, Vec<usize>)>;

/// Takes the host sides of the submitted `lists`, part by part ([`cut`]),
/// and leaves each list a new, closed host list for its next reset, as
/// [`detach`] does. What comes back is in the order the queue runs it: a
/// new entry starts at every part that has waits; host lists without
/// waits join the entry before them. The list's upload memory and draw
/// state (its dynamic descriptor slots) go with its last part, which runs
/// last.
/// shortcut: a list the submission names a second time runs again only if
/// it has one part and that part is in the entry being filled; otherwise
/// the second naming is dropped (logged once).
#[cfg(windows)]
pub(crate) fn detach_parts(lists: &[This]) -> Parts {
    let mut parts: Parts = vec![(Vec::new(), Vec::new(), Vec::new())];
    // The lists taken: where its one host list is (entry, place), or none
    // for a list of several parts.
    let mut seen: Vec<(This, Option<(usize, usize)>)> = Vec::new();
    for &this in lists {
        if this.is_null() {
            continue;
        }
        if let Some(&(_, at)) = seen.iter().find(|(t, _)| *t == this) {
            match at {
                Some((entry, place)) if entry + 1 == parts.len() => parts[entry].2.push(place),
                _ => {
                    static ONCE: std::sync::Once = std::sync::Once::new();
                    ONCE.call_once(|| {
                        tracing::warn!("a list named twice by a submission that waits for memory values runs once")
                    });
                }
            }
            continue;
        }
        // SAFETY: one of our lists, closed and submitted: nothing records
        // into it until the title resets it.
        let list = unsafe { &mut *this.cast_mut().cast::<List>() };
        let Some(host) = hand_over(list) else {
            continue;
        };
        let earlier = std::mem::take(&mut list.earlier);
        let gate = std::mem::take(&mut list.gate);
        let uploads = std::mem::take(&mut *list.uploads.lock().unwrap_or_else(|e| e.into_inner()));
        let state = std::mem::take(&mut *list.state.lock().unwrap_or_else(|e| e.into_inner()));
        let single = earlier.is_empty() && gate.is_empty();
        let mut pieces: Vec<(Vec<ValueWait>, Detached)> = earlier
            .into_iter()
            .map(|part| {
                (
                    part.waits,
                    Detached {
                        host: part.host,
                        _uploads: Vec::new(),
                        _state: Default::default(),
                        _taken: Taken::new(),
                    },
                )
            })
            .collect();
        pieces.push((
            gate,
            Detached {
                host,
                _uploads: uploads,
                _state: state,
                _taken: Taken::new(),
            },
        ));
        let mut last = None;
        for (waits, detached) in pieces {
            if !waits.is_empty() {
                parts.push((waits, Vec::new(), Vec::new()));
            }
            let entry = parts.len() - 1;
            let (_, taken, order) = &mut parts[entry];
            let place = taken.len();
            order.push(place);
            taken.push(detached);
            last = Some((entry, place));
        }
        seen.push((this, if single { last } else { None }));
    }
    // (The first entry is empty if the first list begins with a wait.)
    parts.retain(|(_, taken, _)| !taken.is_empty());
    parts
}

/// Takes the host sides of submitted lists (each once, however often the
/// submission names it) and leaves each list a new, closed host list for
/// its next reset: what was taken, and which of them run in what order.
#[cfg(windows)]
pub(crate) fn detach(lists: &[This]) -> (Vec<Detached>, Vec<usize>) {
    let mut taken: Vec<(This, Detached)> = Vec::new();
    let mut order = Vec::new();
    for &this in lists {
        if this.is_null() {
            continue;
        }
        if let Some(i) = taken.iter().position(|(t, _)| *t == this) {
            order.push(i);
            continue;
        }
        // SAFETY: one of our lists, closed and submitted: nothing records
        // into it until the title resets it.
        let list = unsafe { &mut *this.cast_mut().cast::<List>() };
        let Some(host) = hand_over(list) else {
            continue;
        };
        let uploads = std::mem::take(&mut *list.uploads.lock().unwrap_or_else(|e| e.into_inner()));
        let state = std::mem::take(&mut *list.state.lock().unwrap_or_else(|e| e.into_inner()));
        order.push(taken.len());
        taken.push((
            this,
            Detached {
                host,
                _uploads: uploads,
                _state: state,
                _taken: Taken::new(),
            },
        ));
    }
    (taken.into_iter().map(|(_, d)| d).collect(), order)
}

/// Takes the host list of the submitted `list` (`None`: it has none) and
/// leaves the list what its next reset records into: a new, closed host
/// list, of the size of the recording just taken, or with [`lazy`] none
/// (the reset takes one).
#[cfg(windows)]
fn hand_over(list: &mut List) -> Option<crate::host::HostList> {
    hand_over_as(list, lazy(), |binds| fresh_list(Some(binds), For::Detach))
}

/// [`hand_over`], lazily or not, the new host list from `fresh` (given the
/// binds of the recording taken). Where `fresh` has none (for want of
/// memory) the list is handed over lazily all the same: before, the
/// submission then left the list out and ran nothing of its recording.
#[cfg(windows)]
fn hand_over_as(
    list: &mut List,
    lazy: bool,
    fresh: impl FnOnce(u32) -> Option<crate::host::HostList>,
) -> Option<crate::host::HostList> {
    let binds = list.state.lock().unwrap_or_else(|e| e.into_inner()).binds();
    if list.host.is_some() {
        list.last_binds = binds;
    }
    let fresh = if lazy { None } else { fresh(binds) };
    let Some(fresh) = fresh else {
        if !lazy && list.host.is_some() {
            let n = FOR_MEMORY[3].fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if n <= 8 || n.is_power_of_two() {
                tracing::warn!("submitted title lists left with no host list, to take one at their reset: {n}");
            }
        }
        let host = list.host.take();
        list.holding.set(Holding::NONE);
        return host;
    };
    // A new host list is open; the title's next call on its list is a
    // reset, which wants it closed.
    fresh.close();
    let host = list.host.replace(fresh);
    list.holding.set(Holding::HANDED);
    host
}

pub(crate) extern "system" fn create_allocator(
    _: This,
    kind: u32,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    let allocator = Box::new(Allocator {
        object: Object::new(&ALLOCATOR_FUNCTIONS),
        kind,
    });
    object::hand_out(ALLOCATOR, allocator, iid, &ALLOCATOR_IIDS, out)
}

/// Lists start open, as in D3D12.
pub(crate) extern "system" fn create_list(
    _: This,
    _nodes: u32,
    kind: u32,
    allocator: This,
    pipeline: This,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    new_list(kind, allocator, pipeline, iid, out)
}

/// The console's list creation: type, flags, node mask and barrier batch
/// size in a description.
#[repr(C)]
#[derive(Debug)]
pub(crate) struct ListDesc {
    kind: u32,
    flags: u32,
    nodes: u32,
    barrier_batch: u32,
}

pub(crate) extern "system" fn create_list_x(
    _: This,
    desc: *const ListDesc,
    allocator: This,
    pipeline: This,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    tracing::debug!(?desc, "console command list");
    new_list(desc.kind, allocator, pipeline, iid, out)
}

/// `pipeline` is the list's initial pipeline (null: none), bound until the
/// title sets another, as after a reset.
fn new_list(
    kind: u32,
    allocator: This,
    pipeline: This,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    if allocator.is_null() {
        return E_INVALIDARG;
    }
    tracing::debug!(kind, "command list");
    #[cfg(windows)]
    if thin::on() {
        let mut list = boxed(kind, pipeline, true);
        list.thin.begin(kind, pipeline);
        return object::hand_out(LIST, list, iid, &LIST_IIDS, out);
    }
    object::hand_out(LIST, boxed(kind, pipeline, false), iid, &LIST_IIDS, out)
}

/// A list; with `thin` one whose calls are written down (it has no host list).
fn boxed(kind: u32, pipeline: This, thin: bool) -> Box<List> {
    #[cfg(not(windows))]
    let _ = thin;
    let list = Box::new(List {
        object: match (crate::device::newer(), thin) {
            (true, false) => Object::new(&NEWER_LIST_FUNCTIONS),
            (false, false) => Object::new(&LIST_FUNCTIONS),
            #[cfg(windows)]
            (true, true) => Object::new(&NEWER_THIN_FUNCTIONS),
            #[cfg(windows)]
            (false, true) => Object::new(&THIN_FUNCTIONS),
            #[cfg(not(windows))]
            _ => Object::new(&LIST_FUNCTIONS),
        },
        kind,
        put: [0; 5],
        reserved: [0; 64],
        graphics_root_headers: [0; 128],
        compute_root_headers: [0; 128],
        pipeline_shadow: [0; 8],
        newer: [0; 256],
        uploads: Default::default(),
        #[cfg(windows)]
        host: crate::host::host().filter(|_| !thin).and_then(|h| match h.new_list() {
            Ok(list) => Some(list),
            Err(e) => {
                tracing::warn!(error = %e, "no host command list");
                None
            }
        }),
        #[cfg(windows)]
        state: {
            let mut state = crate::draw::State::default();
            state.set_pipeline(pipeline);
            std::sync::Mutex::new(state)
        },
        #[cfg(windows)]
        earlier: Vec::new(),
        #[cfg(windows)]
        gate: Vec::new(),
        #[cfg(windows)]
        thin: Default::default(),
        #[cfg(windows)]
        holding: Holding::default(),
        #[cfg(windows)]
        last_binds: 0,
    });
    #[cfg(windows)]
    list.holding.open_if(list.host.is_some());
    #[cfg(not(windows))]
    let _ = pipeline;
    list
}

extern "system" fn allocator_query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    let found = object::is(iid, &ALLOCATOR_IIDS).then_some(this);
    object::answer(ALLOCATOR, iid, found, out)
}
extern "system" fn allocator_release(this: This) -> u32 {
    object::release_boxed::<Allocator>(this)
}
/// Nothing is recorded, so there is nothing to free.
extern "system" fn allocator_reset(_: This) -> HRESULT {
    S_OK
}

extern "system" fn list_query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    object::answer(LIST, iid, object::is(iid, &LIST_IIDS).then_some(this), out)
}
extern "system" fn list_release(this: This) -> u32 {
    object::release_boxed::<List>(this)
}
extern "system" fn list_type(this: This) -> u32 {
    // SAFETY: `this` is one of our lists.
    unsafe { (*this.cast::<List>()).kind }
}
extern "system" fn close(this: This) -> HRESULT {
    let _called = crate::timeline::span("the title's Close");
    #[cfg(windows)]
    if let Some(host) = host_list(this) {
        host.close();
    }
    S_OK
}
extern "system" fn reset(this: This, allocator: This, pipeline: This) -> HRESULT {
    if allocator.is_null() {
        return E_INVALIDARG;
    }
    let _called = crate::timeline::span("the title's list Reset");
    // A wait recorded and never submitted goes with the recording.
    VALUE_WAITS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|&(list, _)| list != this as usize);
    // Every submission of the list has run (submissions are waited for).
    // SAFETY: the title passes one of our lists.
    let uploads = unsafe { &(*this.cast::<List>()).uploads };
    uploads.lock().unwrap_or_else(|e| e.into_inner()).clear();
    // The parts of a recording that was cut and never submitted go with it
    // (a submission takes the parts it runs).
    #[cfg(windows)]
    {
        // SAFETY: the title passes one of our lists, and neither records
        // into it nor submits it during its reset.
        let list = unsafe { &mut *this.cast_mut().cast::<List>() };
        list.earlier.clear();
        list.gate.clear();
        host_for_reset(list);
    }
    // (A list left with no host list records nothing: its recording is
    // dropped, as before.)
    #[cfg(windows)]
    with_state(this, |_, state| state.reset(pipeline));
    crate::pipeline::bound(pipeline);
    #[cfg(not(windows))]
    let _ = this;
    S_OK
}

/// At the title's reset of `list`: its host list, reset, for what the title
/// records next.
///
/// The one it has (closed and empty, left by its submission, or with a
/// recording never submitted) is reset ([`crate::host::HostList::try_reset`]);
/// one whose reset still runs out of memory after the trim and the waits of
/// [`oom`] goes, and the list takes another, as a list with none does: with
/// [`lazy`] host lists every list since its submission, else one left none
/// for want of memory ([`hand_over_as`]) or whose host list could not be made.
/// That one is from [`fresh_list`], of the size of the list's last recording:
/// an idle one, reset when its flight landed or by [`recycle`], or a new one,
/// open and empty either way, so it is not reset again (before, it was closed
/// to be reset at once); failing both, one of the reserve ([`reserve_take`]).
/// Never one that is in the air: only lists whose submission has run are idle
/// or in the reserve.
#[cfg(windows)]
fn host_for_reset(list: &mut List) {
    host_for_reset_with(list, crate::host::HostList::try_reset, |binds| {
        fresh_list(Some(binds), For::Reset).or_else(reserve_take)
    });
}

/// [`host_for_reset`] with the reset and the list taken in place of one that
/// has none (given the binds of the list's last recording) as arguments.
#[cfg(windows)]
fn host_for_reset_with(
    list: &mut List,
    reset: impl FnOnce(&crate::host::HostList) -> windows::core::Result<()>,
    fresh: impl FnOnce(u32) -> Option<crate::host::HostList>,
) {
    // (A reset fails only for want of memory: every other failure ended the
    // run.)
    if list.host.as_ref().is_some_and(|host| reset(host).is_err()) {
        drop(list.host.take());
        let n = FOR_MEMORY[1].fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        if n <= 8 || n.is_power_of_two() {
            tracing::warn!("title lists' host lists let go for want of memory at their reset, another taken: {n}");
        }
    }
    if list.host.is_none() && crate::host::host().is_some() {
        list.host = fresh(list.last_binds);
        if list.host.is_none() {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| tracing::warn!("no host list for a title list's reset: its recording is dropped"));
        }
    }
    list.holding.open_if(list.host.is_some());
}

/// A resource barrier as titles pass it: type, flags, then a resource and
/// either a subresource and two states (transition) or a second resource
/// (aliasing). The same layout as the host's.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct Barrier {
    kind: u32,
    flags: u32,
    first: This,
    second: u64,
    last: u64,
}

/// The barrier on host resources, or `None` if it names a resource the host
/// GPU does not have (buffers so far).
#[cfg(windows)]
fn host_barrier(list: &crate::host::HostList, b: &Barrier) -> Option<Barrier> {
    use windows::core::Interface;
    let host = |r: This| crate::resource::host_resource(r).map(|h| h.as_raw().cast_const().cast());
    match b.kind {
        0 => {
            // A split barrier's first half: its second half transitions.
            const BEGIN_ONLY: u32 = 1;
            if b.flags & BEGIN_ONLY != 0 {
                return None;
            }
            // shortcut: whole resources, from the state the list tracked
            // (`HostList::transition`); subresources are not told apart.
            let wanted = crate::host::texture_state(b.last as u32);
            let (texture, before) = list.transition(b.first, wanted)?;
            Some(Barrier {
                kind: 0,
                flags: 0,
                first: texture.as_raw().cast_const().cast(),
                second: 0xffff_ffff | (u64::from(before) << 32),
                last: u64::from(crate::host::legal_state(texture, wanted)),
            })
        }
        1 => Some(Barrier {
            first: if b.first.is_null() {
                b.first
            } else {
                host(b.first)?
            },
            second: if b.second == 0 {
                0
            } else {
                host(b.second as This)? as u64
            },
            ..*b
        }),
        2 => Some(Barrier {
            first: if b.first.is_null() {
                b.first
            } else {
                // A depth texture's unordered access is its colour twin's
                // (the host gives the depth texture none): the barrier is
                // on the texture shaders write.
                match crate::resource::twin_resource(b.first) {
                    Some(twin) => twin.as_raw().cast_const().cast(),
                    None => host(b.first)?,
                }
            },
            ..*b
        }),
        _ => None,
    }
}

/// An unordered access barrier on every resource: earlier shader writes
/// finish before later work reads or writes.
const UAV_BARRIER: Barrier = Barrier {
    kind: 2,
    flags: 0,
    first: std::ptr::null(),
    second: 0,
    last: 0,
};

/// Returns the console's barrier "actions" (none).
extern "system" fn barrier(this: This, count: u32, barriers: *const Barrier) -> u32 {
    #[cfg(windows)]
    if let Some(list) = host_list(this)
        && !barriers.is_null()
    {
        if count > 0 {
            with_state(this, |_, state| state.synced());
        }
        // SAFETY: the title passes `count` barriers.
        let barriers = unsafe { std::slice::from_raw_parts(barriers, count as usize) };
        let mut host: Vec<Barrier> = barriers
            .iter()
            .filter_map(|b| host_barrier(list, b))
            .collect();
        // Buffers have host copies, not resources the barrier can name:
        // a barrier on one waits for every shader write before it.
        if barriers.iter().any(|b| {
            b.kind != 1 && !b.first.is_null() && crate::resource::buffer_bytes(b.first).is_some()
        }) {
            host.push(UAV_BARRIER);
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::WaitsForBuffers, 1);
        }
        if !host.is_empty() {
            use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_BARRIER;
            const _: () = assert!(size_of::<Barrier>() == size_of::<D3D12_RESOURCE_BARRIER>());
            // SAFETY: the same layout; borrowed resource pointers, never
            // released through this slice.
            let host = unsafe {
                std::slice::from_raw_parts(
                    host.as_ptr().cast::<D3D12_RESOURCE_BARRIER>(),
                    host.len(),
                )
            };
            // SAFETY: an open list and live resources.
            unsafe { list.list.ResourceBarrier(host) };
        }
    }
    0
}

/// Moves a title texture's host copy to `state` in the list, if needed.
#[cfg(windows)]
pub(crate) fn ensure_state(list: &crate::host::HostList, resource: This, state: u32) {
    use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_STATES as State;
    if let Some((texture, before)) = list.transition(resource, state) {
        let state = crate::host::legal_state(texture, state);
        let barrier = crate::host::transition(texture, State(before as i32), State(state as i32));
        // SAFETY: an open list and a live texture.
        unsafe { list.list.ResourceBarrier(&[barrier]) };
    }
}

#[cfg(windows)]
const COPY_DEST: u32 = 0x400;
#[cfg(windows)]
const COPY_SOURCE: u32 = 0x800;

/// `len` bytes at `offset` of a title buffer, if they are inside it.
#[cfg(windows)]
fn buffer_range(buffer: This, offset: u64, len: u64) -> Option<*mut u8> {
    let (data, size) = crate::resource::buffer_bytes(buffer)?;
    let end = offset.checked_add(len)?;
    if end > size {
        tracing::warn!(offset, len, size, "copy outside its buffer");
        return None;
    }
    // SAFETY: inside the buffer, checked above.
    Some(unsafe { data.add(offset as usize) })
}

/// A copy of title memory that `list` records: on the card at its place in
/// the list ([`crate::host::HostList::place_copy`]), or at submission.
#[cfg(windows)]
fn buffer_copy(this: This, list: &crate::host::HostList, to: *mut u8, from: *const u8, len: usize) {
    if len == 0 {
        return;
    }
    if list.place_copy(to as usize, from as usize, len) {
        with_state(this, |_, state| state.copied());
    } else {
        list.defer_copy(to, from, len);
    }
}

/// Buffer to buffer: both are host memory, copied at submission.
extern "system" fn copy_buffer_region(
    this: This,
    dst: This,
    dst_offset: u64,
    src: This,
    src_offset: u64,
    len: u64,
) {
    #[cfg(windows)]
    if let Some(list) = host_list(this)
        && let Some(to) = buffer_range(dst, dst_offset, len)
        && let Some(from) = buffer_range(src, src_offset, len)
    {
        buffer_copy(this, list, to, from, len as usize);
    }
    #[cfg(not(windows))]
    let _ = (this, dst, dst_offset, src, src_offset, len);
}

/// Whether one of two resources is a target in a stand-in format and the
/// other is not ([`crate::resource::target_format`]): their texels differ
/// in size, so the host cannot copy between them (logged once).
#[cfg(windows)]
fn unlike(dst: This, src: This) -> bool {
    let unlike = crate::resource::stands_in(dst) != crate::resource::stands_in(src);
    if unlike {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            tracing::warn!(?dst, ?src, "copy between a stand-in target and title-format texels skipped")
        });
    }
    unlike
}

/// Copies left out because both sides were one host texture
/// ([`copy_resource`], [`copy_texture_region`]), since the start.
#[cfg(windows)]
pub(crate) static SELF_COPIES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

extern "system" fn copy_resource(this: This, dst: This, src: This) {
    #[cfg(windows)]
    {
        let Some(list) = host_list(this) else {
            return;
        };
        if let (Some(to), Some(from)) = (
            crate::resource::host_resource(dst),
            crate::resource::host_resource(src),
        ) {
            if unlike(dst, src) {
                return;
            }
            // Two title textures over the same memory have one host
            // texture between them: the copy would be of that texture onto
            // itself, which the host does not allow, and it would change
            // nothing. It was recorded all the same, with the texture left
            // in the source state as the copy's destination (the debug
            // layer: "CopyResource with its destination in the COPY_SOURCE
            // state", for a depth texture).
            // Only the two state changes and the copy are left out: what
            // is noted of the textures stays as it was. After
            // triippywhale 6e502b3b.
            let same = windows::core::Interface::as_raw(to) == windows::core::Interface::as_raw(from);
            // A source the title fills itself is uploaded first (upstream
            // PR #671): a copy read whatever the host texture held.
            if crate::texture::title_written(from) {
                crate::texture::upload(list, src, from);
            }
            // Depth textures shaders also write: what they wrote, first.
            list.twin_use(dst, false, true);
            list.twin_use(src, false, false);
            // A destination made level by level gets its levels' memory
            // ahead of this list (`crate::tiles`).
            crate::texture::card_writes(list, dst, to, None);
            crate::texture::host_written(dst);
            if same {
                SELF_COPIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                static ONCE: std::sync::Once = std::sync::Once::new();
                ONCE.call_once(|| {
                    tracing::info!(
                        ?dst,
                        ?src,
                        "a copy between two title textures that share one host texture is left out: the first one"
                    );
                });
                return;
            }
            ensure_state(list, dst, COPY_DEST);
            ensure_state(list, src, COPY_SOURCE);
            #[cfg(feature = "diagnostics")]
            list.note_op(|| format!("copy resource {src:?} to {dst:?}"));
            list.gpu_kind(crate::host::gpu_split::Kind::TitleOther);
            // SAFETY: an open list and live textures.
            unsafe { list.list.CopyResource(to, from) };
        } else if let (Some((to, a)), Some((from, b))) = (
            crate::resource::buffer_bytes(dst),
            crate::resource::buffer_bytes(src),
        ) {
            buffer_copy(this, list, to, from, a.min(b) as usize);
        }
    }
    #[cfg(not(windows))]
    let _ = (this, dst, src);
}

extern "system" fn copy_resource_x(this: This, dst: This, src: This, _flags: u32) {
    copy_resource(this, dst, src);
}

/// A copy location as titles pass it, laid out as the host's: a resource,
/// then a subresource index (type 0) or a placed footprint (type 1).
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct CopyLocation {
    resource: This,
    kind: u32,
    footprint: crate::resource::PlacedFootprint,
}

/// Texture to texture on the GPU; buffer to texture through an upload
/// buffer filled at submission. Texture to buffer (read back) is not done
/// yet.
extern "system" fn copy_texture_region(
    this: This,
    dst: *const CopyLocation,
    x: u32,
    y: u32,
    z: u32,
    src: *const CopyLocation,
    source_box: *const [u32; 6],
) {
    #[cfg(windows)]
    {
        use windows::Win32::Graphics::Direct3D12::{D3D12_BOX, D3D12_TEXTURE_COPY_LOCATION};
        use windows::core::Interface;
        const FOOTPRINT: u32 = 1;
        // SAFETY: the title passes both locations (null is tolerated).
        let (Some(list), Some(&dst), Some(&src)) =
            (host_list(this), unsafe { dst.as_ref() }, unsafe {
                src.as_ref()
            })
        else {
            return;
        };
        let Some(to) = crate::resource::host_resource(dst.resource) else {
            // Texture to buffer: a read back. The list copies the texels
            // into a host buffer of ours; once it has run they go to the
            // title's buffer (`HostList::defer_read_back`).
            // A title may read tiny float targets back every frame (a few
            // texels), and read a buffer
            // nobody wrote. `VICTORY_READ_BACK=0` leaves them unmade, as
            // before.
            static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            let made = read_back(list, dst, x, y, z, src, source_box);
            if n <= 16 || n.is_multiple_of(4096) || made.is_err() {
                static REFUSED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                if made.is_err() && REFUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= 16 {
                    return;
                }
                let source = crate::resource::host_resource(src.resource).map(|r| {
                    // SAFETY: a live resource.
                    let d = unsafe { r.GetDesc() };
                    format!("{}x{} f{} mips {}", d.Width, d.Height, d.Format.0, d.MipLevels)
                });
                tracing::info!(
                    copies = n,
                    made = ?made,
                    source = ?source,
                    source_kind = src.kind,
                    destination_kind = dst.kind,
                    destination_offset = dst.footprint.offset,
                    destination_bytes = ?dst.footprint.bytes(),
                    "a texture to buffer copy (a read back)"
                );
            }
            return;
        };
        if unlike(dst.resource, src.resource) {
            return;
        }
        let mut from = src;
        // Both sides are the same subresource of one host texture (set below).
        let mut same = false;
        let _upload; // keeps the source alive while this call records it
        #[cfg(feature = "diagnostics")]
        let mut read = None;
        if src.kind == FOOTPRINT {
            let (Some(host), Some(bytes)) = (crate::host::host(), src.footprint.bytes()) else {
                return;
            };
            let Some(data) = buffer_range(src.resource, src.footprint.offset, bytes) else {
                return;
            };
            let (buffer, at) = match list.place_source(data as usize, bytes as usize) {
                // What shaders wrote to it, at this place of the list.
                Some(buffer) => (buffer, 0),
                None => {
                    let Some((buffer, at, staging)) = list.upload_at(host, bytes) else {
                        return;
                    };
                    // The list owns this mapped D3D12 upload memory. Wine can
                    // report its address as MEM_FREE, so use the host-upload
                    // path.
                    list.defer_fill_buffer(&buffer, staging, data, bytes as usize);
                    (buffer, at)
                }
            };
            from.resource = buffer.as_raw().cast_const().cast();
            from.footprint.offset = at;
            #[cfg(feature = "diagnostics")]
            {
                // SAFETY: a live buffer.
                read = Some((unsafe { buffer.GetGPUVirtualAddress() } + at, bytes));
            }
            _upload = buffer;
        } else {
            let Some(texture) = crate::resource::host_resource(src.resource) else {
                return;
            };
            // The same subresource of one host texture on both sides (two
            // title textures over one memory): not a copy the host allows.
            // The states and the copy are left out below; the rest is
            // noted as before.
            same = texture.as_raw() == to.as_raw()
                && src.kind != FOOTPRINT
                && dst.kind != FOOTPRINT
                && src.footprint.offset as u32 == dst.footprint.offset as u32;
            list.twin_use(src.resource, false, false);
            if crate::texture::title_written(texture) {
                crate::texture::upload(list, src.resource, texture);
            }
            if !same {
                ensure_state(list, src.resource, COPY_SOURCE);
            }
            from.resource = texture.as_raw().cast_const().cast();
        }
        // A depth texture shaders also write: what they wrote, first (the
        // source above too).
        list.twin_use(dst.resource, false, true);
        // The first copy into a texture of the title's: what it does not
        // write is zeros, as in a texture that was an allocation of its own
        // (`VICTORY_COPY_OVER_TITLE=title`: what title memory holds, which
        // may be another texture's, which left specks on the surfaces drawn with
        // it). (The host copy is cut from a block and held whatever lay there, and
        // no upload is made once a copy wrote it: an atlas a title copies small
        // baked tiles into was drawn with other textures' bytes wherever nothing
        // was baked.)
        static OVER: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
        let over = *OVER.get_or_init(|| match std::env::var("VICTORY_COPY_OVER_TITLE").as_deref() {
            Ok("0") => 0,
            Ok("title") => 1,
            _ => 2,
        });
        if over != 0 && crate::texture::title_written(to) && !crate::texture::host_wrote(dst.resource as usize) {
            // One a list uploaded, or a draw sampled, holds the title's.
            if over == 2 && !list.used(dst.resource as usize) && !crate::texture::is_staged(dst.resource) {
                if let Some(host) = crate::host::host() {
                    ensure_state(list, dst.resource, COPY_DEST);
                    let _ = list.first_use(dst.resource as usize);
                    crate::texture::zeros(list, host, to);
                }
            } else {
                crate::texture::upload(list, dst.resource, to);
            }
            // (Done before the title's copy writes over it.)
            if list.used(dst.resource as usize) {
                ensure_state(list, dst.resource, COPY_SOURCE);
            }
        }
        // A destination made level by level gets the level's memory ahead
        // of this list (`crate::tiles`): the title's bake copies each level
        // of its result into a block-compressed texture of that kind, and
        // a copy into a level without memory is dropped.
        crate::texture::card_writes(list, dst.resource, to, Some(dst.footprint.offset as u32));
        crate::texture::host_written(dst.resource);
        if same {
            SELF_COPIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return;
        }
        ensure_state(list, dst.resource, COPY_DEST);
        let mut to_location = dst;
        to_location.resource = to.as_raw().cast_const().cast();
        let host_box = (!source_box.is_null()).then(|| source_box.cast::<D3D12_BOX>());
        #[cfg(feature = "diagnostics")]
        {
            let text = || {
                // SAFETY: a live texture; the box is the title's or null.
                let (d, b) = unsafe { (to.GetDesc(), host_box.map(|b| *b)) };
                format!(
                    "copy to {:?} {}x{} f{} sub/kind {} at {x},{y},{z} from {:?} kind {} footprint {:?} box {b:?}",
                    dst.resource,
                    d.Width,
                    d.Height,
                    d.Format.0,
                    dst.kind,
                    src.resource,
                    src.kind,
                    (src.kind == FOOTPRINT).then_some(src.footprint)
                )
            };
            match read {
                Some(range) => list.note_read(text, range),
                None => list.note_op(text),
            }
        }
        const _: () =
            assert!(size_of::<CopyLocation>() == size_of::<D3D12_TEXTURE_COPY_LOCATION>());
        list.gpu_kind(crate::host::gpu_split::Kind::TitleOther);
        // SAFETY: an open list; both locations in the host's layout, naming
        // live resources; the box is the title's (same layout).
        unsafe {
            list.list.CopyTextureRegion(
                (&raw const to_location).cast(),
                x,
                y,
                z,
                (&raw const from).cast(),
                host_box,
            )
        };
    }
    #[cfg(not(windows))]
    let _ = (this, dst, x, y, z, src, source_box);
}

/// Records a texture to buffer copy (a read back) of [`copy_texture_region`]
/// into `list`, or says why not.
#[cfg(windows)]
#[allow(clippy::too_many_arguments)] // the method's own arguments
fn read_back(
    list: &crate::host::HostList,
    dst: CopyLocation,
    x: u32,
    y: u32,
    z: u32,
    src: CopyLocation,
    source_box: *const [u32; 6],
) -> Result<(), &'static str> {
    use windows::Win32::Graphics::Direct3D12::D3D12_BOX;
    use windows::core::Interface;
    const FOOTPRINT: u32 = 1;
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ON.get_or_init(|| std::env::var_os("VICTORY_READ_BACK").is_none_or(|v| v != "0")) {
        return Err("switched off");
    }
    if dst.kind != FOOTPRINT || src.kind == FOOTPRINT {
        return Err("not a texture subresource into a placed footprint");
    }
    if (x, y, z) != (0, 0, 0) {
        return Err("a destination corner other than the footprint's");
    }
    let host = crate::host::host().ok_or("no host")?;
    let (footprint, [size, row, rows, pitch]) = dst.footprint.host_rows().ok_or("an unknown format")?;
    // The title's buffer holds every row at its own pitch.
    let bytes = pitch * (rows - 1) + row;
    let data = buffer_range(dst.resource, dst.footprint.offset, bytes).ok_or("a destination outside its buffer")?;
    let texture = crate::resource::host_resource(src.resource).ok_or("no host texture")?;
    let buffer = host.read_back_buffer(size).ok_or("no read-back buffer")?;
    #[cfg(feature = "diagnostics")]
    crate::book::tag(&buffer, "read-backs of texture copies into title buffers");
    // As a texture source of any copy: what shaders wrote to a depth twin
    // and what the title wrote itself first, then the state.
    list.twin_use(src.resource, false, false);
    if crate::texture::title_written(texture) {
        crate::texture::upload(list, src.resource, texture);
    }
    ensure_state(list, src.resource, COPY_SOURCE);
    let mut from = src;
    from.resource = texture.as_raw().cast_const().cast();
    let mut to = dst;
    to.resource = buffer.as_raw().cast_const().cast();
    to.footprint = footprint;
    let host_box = (!source_box.is_null()).then(|| source_box.cast::<D3D12_BOX>());
    #[cfg(feature = "diagnostics")]
    list.note_op(|| {
        format!(
            "read back of {:?} sub {} into buffer {:?}+{:#x}: {rows} rows of {row} bytes",
            src.resource, src.footprint.offset as u32, dst.resource, dst.footprint.offset
        )
    });
    list.gpu_kind(crate::host::gpu_split::Kind::TitleOther);
    // SAFETY: an open list; both locations in the host's layout (asserted
    // in `copy_texture_region`), naming a live texture and our buffer, which
    // the list keeps until it is reset; the box is the title's.
    unsafe {
        list.list
            .CopyTextureRegion((&raw const to).cast(), 0, 0, 0, (&raw const from).cast(), host_box)
    };
    list.defer_read_back(buffer, data, [row as usize, rows as usize, pitch as usize]);
    Ok(())
}

#[allow(clippy::too_many_arguments)] // the method's own arguments
extern "system" fn copy_texture_region_x(
    this: This,
    dst: *const CopyLocation,
    x: u32,
    y: u32,
    z: u32,
    src: *const CopyLocation,
    source_box: *const [u32; 6],
    _flags: u32,
) {
    copy_texture_region(this, dst, x, y, z, src, source_box);
}

extern "system" fn resolve(
    this: This,
    dst: This,
    dst_sub: u32,
    src: This,
    src_sub: u32,
    format: u32,
) {
    #[cfg(windows)]
    if let Some(list) = host_list(this)
        && let (Some(to), Some(from)) = (
            crate::resource::host_resource(dst),
            crate::resource::host_resource(src),
        )
    {
        use windows::Win32::Foundation::RECT;
        use windows::Win32::Graphics::Direct3D12::{
            D3D12_RESOLVE_MODE_AVERAGE, ID3D12GraphicsCommandList1,
        };
        use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
        use windows::core::Interface;
        const RESOLVE_DEST: u32 = 0x1000;
        const RESOLVE_SOURCE: u32 = 0x2000;
        if unlike(dst, src) {
            return;
        }
        // Depth textures shaders also write: what they wrote, first.
        list.twin_use(dst, false, true);
        list.twin_use(src, false, false);
        // As for a copy (`crate::tiles`).
        crate::texture::card_writes(list, dst, to, Some(dst_sub));
        ensure_state(list, dst, RESOLVE_DEST);
        ensure_state(list, src, RESOLVE_SOURCE);
        crate::texture::host_written(dst);
        #[cfg(feature = "diagnostics")]
        list.note_op(|| format!("resolve {src:?} to {dst:?} format {format}"));
        list.gpu_kind(crate::host::gpu_split::Kind::TitleOther);
        // SAFETY: an open list and live textures; the region is inside both.
        unsafe {
            let (a, b) = (to.GetDesc(), from.GetDesc());
            let format = if crate::resource::stands_in(dst) { a.Format } else { DXGI_FORMAT(format as i32) };
            if (a.Width, a.Height) == (b.Width, b.Height) {
                list.list
                    .ResolveSubresource(to, dst_sub, from, src_sub, format);
            } else if let Ok(list1) = list.list.cast::<ID3D12GraphicsCommandList1>() {
                // The console clips a larger source to the destination.
                let rect = RECT {
                    left: 0,
                    top: 0,
                    right: a.Width.min(b.Width) as i32,
                    bottom: a.Height.min(b.Height) as i32,
                };
                list1.ResolveSubresourceRegion(
                    to,
                    dst_sub,
                    0,
                    0,
                    from,
                    src_sub,
                    Some(&rect),
                    format,
                    D3D12_RESOLVE_MODE_AVERAGE,
                );
            }
        }
    }
    #[cfg(not(windows))]
    let _ = (this, dst, dst_sub, src, src_sub, format);
}

#[repr(C)]
pub(crate) struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

extern "system" fn clear_rtv(
    this: This,
    handle: u64,
    color: *const [f32; 4],
    count: u32,
    rects: *const Rect,
) {
    #[cfg(windows)]
    {
        let (Some(list), Some(record)) = (host_list(this), crate::descriptor::rtv_record(handle))
        else {
            return;
        };
        let (Some(texture), Some(host)) = (
            crate::resource::host_resource(record.resource),
            crate::host::host(),
        ) else {
            return;
        };
        // SAFETY: the title passes four floats (null is tolerated).
        let Some(color) = (unsafe { color.as_ref() }) else {
            return;
        };
        let rects = (count > 0 && !rects.is_null()).then(|| {
            // SAFETY: the title passes `count` rectangles, laid out as the host's.
            unsafe { std::slice::from_raw_parts(rects.cast(), count as usize) }
        });
        const RENDER_TARGET: u32 = 0x4;
        let rtv = host.rtv(Some(texture), record.desc().as_ref());
        // SAFETY: an open list, a fresh view of a live texture.
        unsafe {
            if let Some((_, before)) = list.transition(record.resource, RENDER_TARGET) {
                use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_STATES as State;
                list.list.ResourceBarrier(&[crate::host::transition(
                    texture,
                    State(before as i32),
                    State(RENDER_TARGET as i32),
                )]);
            }
            list.gpu_kind(crate::host::gpu_split::Kind::TitleOther);
            list.list.ClearRenderTargetView(rtv, color, rects);
        }
    }
    #[cfg(not(windows))]
    let _ = (this, handle, color, count, rects);
}

/// `ClearDepthStencilView(handle, flags, depth, stencil, count, rects)`;
/// flags: 1 depth, 2 stencil.
extern "system" fn clear_dsv(
    this: This,
    handle: u64,
    flags: u32,
    depth: f32,
    stencil: u8,
    count: u32,
    rects: *const Rect,
) {
    #[cfg(windows)]
    {
        use windows::Win32::Graphics::Direct3D12::D3D12_CLEAR_FLAGS;
        let (Some(list), Some(record)) = (host_list(this), crate::descriptor::dsv_record(handle))
        else {
            return;
        };
        let (Some(texture), Some(host)) = (
            crate::resource::host_resource(record.resource),
            crate::host::host(),
        ) else {
            return;
        };
        let rects = (count > 0 && !rects.is_null()).then(|| {
            // SAFETY: the title passes `count` rectangles, laid out as the host's.
            unsafe { std::slice::from_raw_parts(rects.cast(), count as usize) }
        });
        const DEPTH_WRITE: u32 = 0x10;
        // A depth texture shaders also write: what they wrote, first (the
        // clear may be of one slice or of some rectangles).
        list.twin_use(record.resource, false, true);
        ensure_state(list, record.resource, DEPTH_WRITE);
        if crate::resource::depth_and_unordered(record.resource) {
            crate::resource::DEPTH_UNORDERED[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let dsv = host.dsv(texture, record.depth_desc().as_ref());
        list.gpu_kind(crate::host::gpu_split::Kind::TitleOther);
        // SAFETY: an open list, a fresh view of a live texture.
        unsafe {
            list.list
                .ClearDepthStencilView(dsv, D3D12_CLEAR_FLAGS(flags as i32 & 3), depth, stencil, rects)
        };
    }
    #[cfg(not(windows))]
    let _ = (this, handle, flags, depth, stencil, count, rects);
}

/// `ClearUnorderedAccessViewUint(GPU handle, CPU handle, resource, values, count, rects)`: the title's
/// upscaler clears its history with it (`clear.rs`).
extern "system" fn clear_uav_uint(
    this: This,
    gpu: u64,
    cpu: u64,
    resource: This,
    values: *const [u32; 4],
    count: u32,
    rects: *const Rect,
) {
    #[cfg(windows)]
    clear::run(this, clear::Kind::Uint, gpu, cpu, resource, values, count, rects);
    #[cfg(not(windows))]
    let _ = (this, gpu, cpu, resource, values, count, rects);
}

/// `ClearUnorderedAccessViewFloat(...)`: as [`clear_uav_uint`], the values four floats.
extern "system" fn clear_uav_float(
    this: This,
    gpu: u64,
    cpu: u64,
    resource: This,
    values: *const [u32; 4],
    count: u32,
    rects: *const Rect,
) {
    #[cfg(windows)]
    clear::run(this, clear::Kind::Float, gpu, cpu, resource, values, count, rects);
    #[cfg(not(windows))]
    let _ = (this, gpu, cpu, resource, values, count, rects);
}

/// `CopyMemoryToMemoryX(destination, source, size, flags)`: GPU addresses
/// are title memory, copied at submission like buffer copies (if mapped).
extern "system" fn copy_memory(this: This, dst: u64, src: u64, len: u64, _flags: u32) {
    #[cfg(windows)]
    if let Some(list) = host_list(this)
        && dst != 0
        && src != 0
    {
        let len = len as usize;
        if list.place_copy(dst as usize, src as usize, len) {
            with_state(this, |_, state| state.copied());
            return;
        }
        list.defer(move || {
            if crate::texture::committed(src as *const u8, len)
                && crate::texture::committed(dst as *const u8, len)
            {
                // SAFETY: both ranges are committed title memory.
                unsafe { std::ptr::copy(src as *const u8, dst as *mut u8, len) };
            }
        });
    }
    #[cfg(not(windows))]
    let _ = (this, dst, src, len);
}

/// `DriverHintX(feature, value)`: no hints for the host.
extern "system" fn driver_hint(_: This, _feature: u64, _value: u64) -> u64 {
    0
}

/// `DispatchX(x, y, z, flags)`: a dispatch with console flags, of which the
/// host follows one: the counts are threads, not groups (`draw.rs`
/// `THREAD_DIMENSIONS`; the ray-tracing structure builder dispatches this way).
extern "system" fn dispatch_x(this: This, x: u32, y: u32, z: u32, flags: u32) {
    count_dispatch();
    #[cfg(windows)]
    with_state(this, |list, s| s.dispatch(list, x, y, z, flags));
    #[cfg(not(windows))]
    let _ = (this, x, y, z, flags);
}

/// `BeginQuery`/`EndQuery(heap, type, index)`: queries are answered when
/// resolved.
extern "system" fn query_mark(_: This, _heap: This, _kind: u32, _index: u32) {}

/// `ResolveQueryData(heap, type, first, count, buffer, offset)`: writes
/// each query's result into the buffer once the list has run. Timestamps
/// (type 2) are the GPU clock then; occlusion counts (0) say a million
/// samples passed and binary occlusion (1) visible, so nothing is culled
/// by a query.
/// shortcut: no query runs on the host GPU; results are plausible values,
/// not measured ones (time spans come out as the submission's length).
extern "system" fn resolve_queries(
    this: This,
    _heap: This,
    kind: u32,
    _first: u32,
    count: u32,
    buffer: This,
    offset: u64,
) {
    const OCCLUSION: u32 = 0;
    const BINARY: u32 = 1;
    const TIMESTAMP: u32 = 2;
    const PIPELINE_STATISTICS: u32 = 3;
    let Some((data, size)) = crate::resource::buffer_bytes(buffer) else {
        return;
    };
    let (what, bytes) = match kind {
        OCCLUSION => (Written::Value(1 << 20), 8),
        BINARY => (Written::Value(1), 8),
        TIMESTAMP => (Written::GpuTimestamp, 8),
        PIPELINE_STATISTICS => (Written::Value(0), 88),
        _ => (Written::Value(0), 16),
    };
    let Some(end) = offset.checked_add(u64::from(count) * bytes) else {
        return;
    };
    if end > size {
        return;
    }
    for i in 0..u64::from(count) {
        // Wider results are written as 8-byte pieces.
        for piece in 0..bytes / 8 {
            // SAFETY: inside the buffer, checked above.
            let at = unsafe { data.add((offset + i * bytes + piece * 8) as usize) };
            let what = if piece == 0 { what } else { Written::Value(0) };
            defer_write(this, at as u64, what, 8);
        }
    }
}

/// IUnknown 3, object 4, device child 1, allocator 2.
static ALLOCATOR_FUNCTIONS: Table<24> = {
    let mut f = object::d3d12_object(object::missing_table::<ALLOCATOR, 24>());
    f[0] = allocator_query as *const ();
    f[2] = allocator_release as *const ();
    f[7] = object::parent_device as *const (); // 0x38
    f[8] = allocator_reset as *const (); // 0x40
    f[9] = object::ignore as *const (); // 0x48 allocation callback
    Table(f)
};

extern "system" fn set_pipeline(this: This, pipeline: This) {
    crate::pipeline::bound(pipeline);
    #[cfg(windows)]
    with_state(this, |_, state| state.set_pipeline(pipeline));
    #[cfg(not(windows))]
    let _ = this;
}

/// Pipeline syncs: waits for earlier work and cache flushes, at the top or
/// bottom of the pipe `(flags, control, address, size)`. The host orders
/// nothing between dispatches by itself: what shaders wrote is waited for
/// here as at the title's barriers (the ray-tracing structure builder has
/// no other wait between its stages).
/// (Arguments may be what earlier shaders wrote from here on:
/// `State::synced`.)
extern "system" fn sync(this: This, flags: u32, control: u32, address: u64, size: u64) {
    #[cfg(windows)]
    {
        with_state(this, |_, state| state.synced());
        if let Some(list) = host_list(this) {
            use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_BARRIER;
            #[cfg(feature = "diagnostics")]
            {
                // The shapes of the title's calls, each once.
                static SEEN: std::sync::Mutex<Vec<(u32, u32, bool)>> = std::sync::Mutex::new(Vec::new());
                let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
                let shape = (flags, control, address == 0);
                if seen.len() < 64 && !seen.contains(&shape) {
                    seen.push(shape);
                    tracing::info!(
                        flags = format_args!("{flags:#x}"),
                        control = format_args!("{control:#x}"),
                        address = format_args!("{address:#x}"),
                        size = format_args!("{size:#x}"),
                        "a pipeline sync"
                    );
                }
                crate::copystat::add(crate::copystat::C::WaitsForGroups, 1);
            }
            // SAFETY: the same layout; no resource named.
            let barrier = unsafe { std::mem::transmute::<Barrier, D3D12_RESOURCE_BARRIER>(UAV_BARRIER) };
            // SAFETY: an open list.
            unsafe { list.list.ResourceBarrier(&[barrier]) };
        }
    }
    let _ = (this, flags, control, address, size);
}

/// Reset with console flags `(allocator, pipeline, flags)`; the flags only
/// tune the console's command memory.
extern "system" fn reset_x(this: This, allocator: This, pipeline: This, _flags: u32) -> HRESULT {
    reset(this, allocator, pipeline)
}

#[cfg(windows)]
use crate::upload::Upload;

/// The memory of [`allocate_upload`] where no host GPU reads it by address.
#[cfg(not(windows))]
struct Upload(Vec<u8>);

#[cfg(not(windows))]
impl Upload {
    fn new(len: usize) -> Option<Upload> {
        Some(Upload(vec![0; len]))
    }
    fn start(&mut self) -> *mut u8 {
        self.0.as_mut_ptr()
    }
}

/// Newer lists' 0x4a0 `(size, alignment) -> pointer`: memory the list
/// owns until it is reset, which the CPU writes and the GPU reads at the
/// same address; null on failure. The title's BVH builder puts
/// its instance descriptions there (and wrote through our old
/// `E_NOTIMPL` as a pointer).
extern "system" fn allocate_upload(this: This, size: u64, alignment: u64) -> *mut u8 {
    let align = usize::try_from(alignment).unwrap_or(0).max(16);
    let Some(len) = usize::try_from(size).ok().and_then(|s| s.checked_add(align)) else {
        return std::ptr::null_mut();
    };
    if this.is_null() || !align.is_power_of_two() {
        return std::ptr::null_mut();
    }
    let Some(mut block) = Upload::new(len) else {
        return std::ptr::null_mut();
    };
    let start = block.start();
    // SAFETY: the offset is below `align`, so `size` bytes fit after it.
    let at = unsafe { start.add(start.align_offset(align)) };
    // SAFETY: the title passes one of our lists.
    let uploads = unsafe { &(*this.cast::<List>()).uploads };
    uploads.lock().unwrap_or_else(|e| e.into_inner()).push(block);
    at
}

/// Draw state the host has no use for (root signatures: ours come with
/// the pipeline; depth order and shading rate hints): any arguments,
/// nothing returned.
extern "system" fn ignored_state(_: This) {}

/// The title's draw state, kept for its draws or
/// set on the host list at once. Without a host GPU these do nothing.
#[cfg(windows)]
mod state {
    use super::{This, with_state};
    use crate::draw::{IndexView, VertexView};
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Direct3D12::D3D12_VIEWPORT;

    /// `count` items at `items`, or none.
    fn items<'a, T>(items: *const T, count: u32) -> &'a [T] {
        if items.is_null() || count == 0 {
            return &[];
        }
        // SAFETY: the title passes `count` items.
        unsafe { std::slice::from_raw_parts(items, count as usize) }
    }

    pub(super) extern "system" fn topology(this: This, topology: u32) {
        with_state(this, |list, s| s.set_topology(list, topology));
    }
    pub(super) extern "system" fn viewports(
        this: This,
        count: u32,
        viewports: *const D3D12_VIEWPORT,
    ) {
        // (Through the draw state, which keeps them for the host list
        // that continues this one after a cut: `State::replay`.)
        with_state(this, |list, s| s.set_viewports(list, items(viewports, count)));
    }
    pub(super) extern "system" fn scissors(this: This, count: u32, rects: *const RECT) {
        with_state(this, |list, s| s.set_scissors(list, items(rects, count)));
    }
    pub(super) extern "system" fn blend_factor(this: This, factor: *const [f32; 4]) {
        with_state(this, |list, s| {
            // SAFETY: the title passes four floats or null.
            s.set_blend_factor(list, unsafe { factor.as_ref() })
        });
    }
    pub(super) extern "system" fn stencil_ref(this: This, reference: u32) {
        with_state(this, |list, s| s.set_stencil_ref(list, reference));
    }
    pub(super) extern "system" fn depth_bounds(this: This, min: f32, max: f32) {
        with_state(this, |list, s| s.set_depth_bounds(list, min, max));
    }
    pub(super) extern "system" fn heaps(this: This, count: u32, heaps: *const This) {
        with_state(this, |_, s| s.set_heaps(items(heaps, count)));
    }
    pub(super) extern "system" fn compute_table(this: This, slot: u32, handle: u64) {
        with_state(this, |_, s| s.set_table(true, slot, handle));
    }
    pub(super) extern "system" fn graphics_table(this: This, slot: u32, handle: u64) {
        with_state(this, |_, s| s.set_table(false, slot, handle));
    }
    pub(super) extern "system" fn compute_constant(this: This, slot: u32, value: u32, offset: u32) {
        with_state(this, |_, s| s.set_constants(true, slot, &[value], offset));
    }
    pub(super) extern "system" fn graphics_constant(
        this: This,
        slot: u32,
        value: u32,
        offset: u32,
    ) {
        with_state(this, |_, s| s.set_constants(false, slot, &[value], offset));
    }
    pub(super) extern "system" fn compute_constants(
        this: This,
        slot: u32,
        count: u32,
        values: *const u32,
        offset: u32,
    ) {
        with_state(this, |_, s| {
            s.set_constants(true, slot, items(values, count), offset)
        });
    }
    pub(super) extern "system" fn graphics_constants(
        this: This,
        slot: u32,
        count: u32,
        values: *const u32,
        offset: u32,
    ) {
        with_state(this, |_, s| {
            s.set_constants(false, slot, items(values, count), offset)
        });
    }
    /// Root constant buffer, shader resource and unordered access views.
    pub(super) extern "system" fn compute_view(this: This, slot: u32, address: u64) {
        with_state(this, |_, s| s.set_view(true, slot, address));
    }
    pub(super) extern "system" fn graphics_view(this: This, slot: u32, address: u64) {
        with_state(this, |_, s| s.set_view(false, slot, address));
    }
    pub(super) extern "system" fn index_buffer(this: This, view: *const IndexView) {
        // SAFETY: the title passes a view or null.
        with_state(this, |_, s| s.set_index_buffer(unsafe { view.as_ref() }));
    }
    pub(super) extern "system" fn vertex_buffers(
        this: This,
        start: u32,
        count: u32,
        views: *const VertexView,
    ) {
        with_state(this, |_, s| {
            if views.is_null() {
                s.clear_vertex_buffers(start, count);
            } else {
                s.set_vertex_buffers(start, items(views, count));
            }
        });
    }
    /// `(count, handles, one range, depth)`: with one range, the handles
    /// follow the first at the render target view size (128 bytes).
    pub(super) extern "system" fn targets(
        this: This,
        count: u32,
        handles: *const u64,
        one_range: i32,
        depth: *const u64,
    ) {
        let handles: Vec<u64> = if one_range != 0 && !handles.is_null() {
            // SAFETY: the title passes the first handle.
            let first = unsafe { *handles };
            (0..u64::from(count)).map(|i| first + 128 * i).collect()
        } else {
            items(handles, count).to_vec()
        };
        // SAFETY: the title passes a handle or null.
        let depth = unsafe { depth.as_ref() }.copied().unwrap_or(0);
        with_state(this, |list, s| s.set_targets(list, &handles, depth));
    }
    pub(super) extern "system" fn draw(
        this: This,
        vertices: u32,
        instances: u32,
        first: u32,
        first_instance: u32,
    ) {
        super::count_draw();
        with_state(this, |list, s| {
            s.draw(list, vertices, instances, first, first_instance)
        });
    }
    pub(super) extern "system" fn dispatch(this: This, x: u32, y: u32, z: u32) {
        super::count_dispatch();
        with_state(this, |list, s| s.dispatch(list, x, y, z, 0));
    }
    /// `DispatchMesh(x, y, z)` (the PC method of List6); the title's mesh
    /// programs take `x` groups only.
    pub(super) extern "system" fn dispatch_mesh(this: This, x: u32, y: u32, z: u32) {
        super::count_draw();
        if y * z != 1 {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| tracing::warn!(x, y, z, "a mesh dispatch with y or z groups"));
        }
        with_state(this, |list, s| s.dispatch_mesh(list, x));
    }
    /// `DispatchMeshX(x, flags, indirect arguments, payload)`.
    /// shortcut: dispatches whose group count is in GPU memory (indirect)
    /// are dropped; make them ExecuteIndirect when the title uses them.
    pub(super) extern "system" fn dispatch_mesh_x(
        this: This,
        x: u32,
        _flags: u64,
        indirect: u64,
        _payload: u64,
    ) {
        super::count_draw();
        if indirect != 0 {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| tracing::warn!("indirect mesh dispatches dropped"));
            return;
        }
        with_state(this, |list, s| s.dispatch_mesh(list, x));
    }
    pub(super) extern "system" fn draw_indexed(
        this: This,
        indices: u32,
        instances: u32,
        first: u32,
        base: i32,
        first_instance: u32,
    ) {
        super::count_draw();
        with_state(this, |list, s| {
            s.draw_indexed(list, indices, instances, first, base, first_instance)
        });
    }
}

/// Without a host GPU every draw state call is ignored.
#[cfg(not(windows))]
mod state {
    pub(super) use super::dispatch;
    pub(super) use super::draw;
    pub(super) use super::draw as draw_indexed;
    pub(super) use super::draw as dispatch_mesh;
    pub(super) use super::draw as dispatch_mesh_x;
    pub(super) use super::ignored_state as blend_factor;
    pub(super) use super::ignored_state as compute_constant;
    pub(super) use super::ignored_state as compute_constants;
    pub(super) use super::ignored_state as compute_table;
    pub(super) use super::ignored_state as compute_view;
    pub(super) use super::ignored_state as graphics_constant;
    pub(super) use super::ignored_state as graphics_constants;
    pub(super) use super::ignored_state as graphics_table;
    pub(super) use super::ignored_state as graphics_view;
    pub(super) use super::ignored_state as heaps;
    pub(super) use super::ignored_state as index_buffer;
    pub(super) use super::ignored_state as scissors;
    pub(super) use super::ignored_state as stencil_ref;
    pub(super) use super::ignored_state as depth_bounds;
    pub(super) use super::ignored_state as targets;
    pub(super) use super::ignored_state as topology;
    pub(super) use super::ignored_state as vertex_buffers;
    pub(super) use super::ignored_state as viewports;
}

fn count_draw() {
    DRAWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Draws of the kinds the host does not run yet (indirect, mesh), and
/// dispatches, counted per frame for the log.
/// shortcut: dropped until their shaders translate.
#[cfg(not(windows))]
extern "system" fn draw(_: This) {
    count_draw();
}

/// `ExecuteIndirect(signature, max count, arguments, offset, count buffer,
/// offset)`. Supports single-argument signatures: draw (stride 16), indexed
/// draw (20), dispatch (12), mesh dispatch (12), and console single-pass
/// dispatch (type 105, stride 12), executed via [`State::execute_indirect`].
/// shortcut: signatures that change root arguments are dropped (logged
/// once per signature).
extern "system" fn execute_indirect(
    this: This,
    signature: This,
    max: u32,
    arguments: This,
    offset: u64,
    count: This,
    count_offset: u64,
) {
    count_draw();
    let Some((stride, args)) = crate::signature::layout(signature) else {
        return;
    };
    let address = crate::resource::buffer_bytes(arguments).map(|(p, _)| p as u64 + offset);
    let counter = crate::resource::buffer_bytes(count).map(|(p, _)| p as u64 + count_offset);
    let mut ran = false;
    #[cfg(windows)]
    if let ([[kind, ..]], Some(address)) = (args, address)
        && (count.is_null() || counter.is_some())
    {
        const DISPATCH: u32 = 2;
        const DISPATCH_SINGLE: u32 = 105;
        let kind = if *kind == DISPATCH_SINGLE { DISPATCH } else { *kind };
        with_state(this, |list, s| {
            ran = s.execute_indirect(list, (kind, stride, max), address, counter);
            if !ran {
                s.left_out();
            }
        });
    } else {
        victory_pipeline::report::left_out(victory_pipeline::report::LeftOut::Other);
        #[cfg(all(windows, feature = "diagnostics"))]
        crate::pipeline::note_why(format!(
            "indirect signature {args:?}: arguments in a buffer {}, count in a buffer {}",
            address.is_some(),
            count.is_null() || counter.is_some()
        ));
    }
    if ran {
        return;
    }
    INDIRECT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    static SEEN: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if !seen.contains(&(signature as usize)) {
        seen.push(signature as usize);
        tracing::info!(
            stride,
            ?args,
            max,
            address = ?address.map(|a| format!("{a:#x}")),
            counted = !count.is_null(),
            "indirect execution dropped (first time for this signature)"
        );
    }
}

/// Indirect executions dropped since the last call.
pub(crate) fn take_indirect() -> u64 {
    INDIRECT.swap(0, std::sync::atomic::Ordering::Relaxed)
}
static INDIRECT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
fn count_dispatch() {
    DISPATCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}
#[cfg(not(windows))]
extern "system" fn dispatch(_: This) {
    count_dispatch();
}
static DRAWS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static DISPATCHES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Draws (any kind, mesh and indirect included) and compute dispatches
/// recorded since the last call.
pub(crate) fn take_counts() -> (u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    (DRAWS.swap(0, Relaxed), DISPATCHES.swap(0, Relaxed))
}

/// A GPU wait for a memory value: the list's commands after it run when
/// the 32 or 64 bits at `address` compare as asked with `reference`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ValueWait {
    address: u64,
    comparison: u32,
    reference: u64,
    wide: bool,
}

impl ValueWait {
    /// Whether the memory holds what is waited for now.
    /// shortcut: the comparison codes are taken to be the comparison
    /// functions' (1 never .. 8 always, value against reference); one not
    /// known counts as met, and so does memory that is not mapped.
    ///
    /// The value is read through the kernel, which reports memory that is
    /// not there instead of faulting (as the copies of title memory are
    /// read, `crate::host::copy_mapped`). Asking Windows first whether the
    /// memory is committed (`VirtualQuery`) walks the page tables to the
    /// end of the region: the title keeps these values in a 1.1 GB
    /// allocation, where one question took milliseconds, and a queue asks
    /// at every submission and every signal, for each queue that stands at
    /// a wait (a title may have several queues). That was a second and more
    /// of every frame on the thread that submits ("between its submissions
    /// ... kernel").
    /// `VICTORY_WAIT_ASK=1` asks first, as before.
    pub(crate) fn met(&self) -> bool {
        #[cfg(all(windows, feature = "diagnostics"))]
        let started = std::time::Instant::now();
        let met = self.holds();
        #[cfg(all(windows, feature = "diagnostics"))]
        {
            use crate::copystat::{C, add};
            add(C::WaitChecks, 1);
            add(C::WaitCheckNs, started.elapsed().as_nanos() as u64);
        }
        met
    }

    fn holds(&self) -> bool {
        let bytes = if self.wide { 8 } else { 4 };
        if self.address == 0 {
            return true;
        }
        static ASK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *ASK.get_or_init(|| std::env::var_os("VICTORY_WAIT_ASK").is_some_and(|v| v == "1"))
            && !crate::texture::committed(self.address as *const u8, bytes)
        {
            return true;
        }
        // The low `bytes` bytes; the rest stays zero.
        let mut raw = [0u8; 8];
        if !crate::host::read_into(self.address as *const u8, &mut raw[..bytes]) {
            return true;
        }
        self.holds_for(u64::from_le_bytes(raw))
    }

    /// Where the value waited for is: address and bytes.
    #[cfg(windows)]
    pub(crate) fn place(&self) -> (usize, usize) {
        (self.address as usize, if self.wide { 8 } else { 4 })
    }

    /// Whether the wait is met with `value` at its address (its low half
    /// for a 32-bit wait).
    pub(crate) fn holds_for(&self, value: u64) -> bool {
        let (value, reference) = if self.wide {
            (value, self.reference)
        } else {
            (value & 0xFFFF_FFFF, self.reference & 0xFFFF_FFFF)
        };
        match self.comparison {
            1 => false,
            2 => value < reference,
            3 => value == reference,
            4 => value <= reference,
            5 => value > reference,
            6 => value != reference,
            7 => value >= reference,
            _ => true,
        }
    }
}

/// The waits lists recorded since their last reset, by list.
static VALUE_WAITS: std::sync::Mutex<Vec<(usize, ValueWait)>> = std::sync::Mutex::new(Vec::new());

/// The waits of the submitted `lists` that are not met yet (taken from
/// them): what the console's GPU would stop at. The title orders work
/// between its queues this way (the title: the compute queue's
/// lighting passes wait for a value the graphics queue's depth pass writes
/// at the bottom of the pipe; run at once, they read a cleared depth
/// buffer and nothing was lit).
/// shortcut: a wait holds its whole list (and submission) back, not only
/// the commands after it.
pub(crate) fn unmet_waits(lists: &[This]) -> Vec<ValueWait> {
    let mut waits = VALUE_WAITS.lock().unwrap_or_else(|e| e.into_inner());
    let mut unmet = Vec::new();
    waits.retain(|&(list, wait)| {
        if !lists.iter().any(|&l| l as usize == list) {
            return true;
        }
        if !wait.met() {
            unmet.push(wait);
        }
        false
    });
    unmet
}

/// The waits each of the submitted `lists` recorded (taken from them),
/// met or not, at the list's first place in the submission.
#[cfg(windows)]
pub(crate) fn waits_by_list(lists: &[This]) -> Vec<Vec<ValueWait>> {
    let mut waits = VALUE_WAITS.lock().unwrap_or_else(|e| e.into_inner());
    let mut by = vec![Vec::new(); lists.len()];
    waits.retain(|&(list, wait)| {
        match lists.iter().position(|&l| l as usize == list) {
            Some(i) => {
                by[i].push(wait);
                false
            }
            None => true,
        }
    });
    by
}

fn wait_for(this: This, address: u64, comparison: u32, reference: u64, wide: bool) {
    let wait = ValueWait {
        address,
        comparison,
        reference,
        wide,
    };
    static SEEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if SEEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 12 {
        tracing::info!(
            address = format_args!("{address:#x}"),
            comparison,
            reference,
            wide,
            met = wait.met(),
            "a list waits for a memory value"
        );
    }
    #[cfg(all(windows, feature = "diagnostics"))]
    if let Some(host) = host_list(this) {
        crate::capture::op(std::ptr::from_ref(host) as usize, || {
            format!(
                "WAIT VALUE at {address:#x} comparison {comparison} reference {reference:#x} wide {wide} met {}",
                wait.met()
            )
        });
    }
    // The list is cut here, and the wait stands before what follows.
    #[cfg(windows)]
    if crate::queue::cuts() {
        cut(this, wait);
        return;
    }
    VALUE_WAITS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((this as usize, wait));
}
extern "system" fn wait_value(this: This, address: u64, comparison: u32, reference: u64) {
    wait_for(this, address, comparison, reference, false);
}
extern "system" fn wait_value64(this: This, address: u64, comparison: u32, reference: u64) {
    wait_for(this, address, comparison, reference, true);
}

/// The enhanced barriers `(group count, groups)`: buffer, texture and
/// global barriers with layouts instead of states. Every host use of a
/// title texture (clears, copies) moves its host copy to the state it needs
/// first, so only their ordering matters: one barrier on every shader
/// write (the title's buffer barriers order compute passes).
extern "system" fn barrier_groups(this: This, count: u32, _groups: *const u8) {
    // Diagnostics: counted, and left out under `VICTORY_BARRIER_TEST=groups`.
    #[cfg(all(windows, feature = "diagnostics"))]
    if count > 0 {
        crate::copystat::add(crate::copystat::C::WaitsForGroups, 1);
        if crate::copystat::barrier_test() & 1 != 0 {
            return;
        }
    }
    #[cfg(windows)]
    if count > 0
        && let Some(list) = host_list(this)
    {
        use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_BARRIER;
        with_state(this, |_, state| state.synced());
        // SAFETY: the same layout; no resource named.
        let barrier = unsafe { std::mem::transmute::<Barrier, D3D12_RESOURCE_BARRIER>(UAV_BARRIER) };
        // SAFETY: an open list.
        unsafe { list.list.ResourceBarrier(&[barrier]) };
    }
    #[cfg(not(windows))]
    let _ = (this, count);
}

/// The bottom-of-pipe value writes `(address, value, flush, flags)`, 32 and
/// 64 bits: done when the list has run, as the GPU would. Flag 1 writes the
/// GPU timestamp instead (64-bit form), flag 2 the system timestamp.
extern "system" fn write32(this: This, address: u64, value: u32, _flush: u32, _flags: u32) {
    defer_write(this, address, Written::Value(u64::from(value)), 4);
    value_written(this, address, u64::from(value));
}
extern "system" fn write64(this: This, address: u64, value: u64, _flush: u32, flags: u32) {
    const GPU_TIMESTAMP: u32 = 1;
    const SYSTEM_TIMESTAMP: u32 = 2;
    let what = match flags {
        GPU_TIMESTAMP => Written::GpuTimestamp,
        SYSTEM_TIMESTAMP => Written::SystemTimestamp,
        _ => Written::Value(value),
    };
    defer_write(this, address, what, 8);
    if let Written::Value(value) = what {
        value_written(this, address, value);
    }
}

/// The title wrote a value at the bottom of the pipe (not a timestamp, not
/// a query result): listed (diagnostics), and with `VICTORY_CUT_WRITES=1`
/// the host list is cut right behind it, so that the value is stored when
/// the card has run the list up to there, as on the console, and not when
/// the whole list has run. Lists of other queues wait for these values
/// (the title: the compute list that builds the depth pyramid
/// waits for one, and the meshlet culling of the same frame reads that
/// pyramid). Off by default: every such write then ends a host list, and
/// each part is a submission of its own.
fn value_written(this: This, address: u64, value: u64) {
    #[cfg(windows)]
    {
        if address == 0 || this.is_null() {
            return;
        }
        #[cfg(not(feature = "diagnostics"))]
        let _ = value;
        #[cfg(feature = "diagnostics")]
        if let Some(host) = host_list(this) {
            crate::capture::op(std::ptr::from_ref(host) as usize, || {
                format!("WRITE VALUE at {address:#x} value {value:#x}")
            });
        }
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ON.get_or_init(|| std::env::var_os("VICTORY_CUT_WRITES").is_some_and(|v| v == "1"))
            || !crate::queue::cuts()
        {
            return;
        }
        // SAFETY: the title passes one of our lists; a list is recorded by
        // one thread at a time and is not submitted while it records.
        let list = unsafe { &mut *this.cast_mut().cast::<List>() };
        let Some(current) = list.host.as_ref() else {
            return;
        };
        let Some(fresh) = fresh_list(None, For::Cut) else {
            return;
        };
        current.close();
        if let Some(done) = list.host.replace(fresh) {
            // The part so far keeps its waits; nothing stands before the
            // next one but this part itself.
            let waits = std::mem::take(&mut list.gate);
            list.earlier.push(Part { waits, host: done });
        }
        if let Some(host) = list.host.as_ref() {
            list.state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .replay(host);
        }
    }
    #[cfg(not(windows))]
    let _ = (this, address, value);
}

fn defer_write(this: This, address: u64, what: Written, bytes: usize) {
    if address == 0 {
        return;
    }
    #[cfg(windows)]
    if let Some(list) = host_list(this) {
        list.defer_write(address as *mut u8, what, bytes);
        return;
    }
    let _ = this;
    // Without a host GPU nothing runs later: write now.
    what.write(address as *mut u8, bytes);
}

/// The list methods by their slot in older titles' lists (the 2020-era
/// layout): IUnknown 3, object 4, device child 1, list 1, graphics list 96
/// (59 as on PC, then console ones), then versions 1-6 (6 + 1 + 1 + 11 + 3
/// + 13): 140 slots.
const METHODS: [(usize, *const ()); 54] = [
    (0, list_query as *const ()),
    (2, list_release as *const ()),
    (7, object::parent_device as *const ()),      // 0x38
    (8, list_type as *const ()),                  // 0x40
    (9, close as *const ()),                      // 0x48
    (10, reset as *const ()),                     // 0x50
    (12, state::draw as *const ()),               // 0x60 instanced
    (13, state::draw_indexed as *const ()),       // 0x68 indexed
    (14, state::dispatch as *const ()),           // 0x70
    (15, copy_buffer_region as *const ()),        // 0x78
    (16, copy_texture_region as *const ()),       // 0x80
    (17, copy_resource as *const ()),             // 0x88
    (19, resolve as *const ()),                   // 0x98
    (20, state::topology as *const ()),           // 0xa0
    (21, state::viewports as *const ()),          // 0xa8
    (22, state::scissors as *const ()),           // 0xb0
    (23, state::blend_factor as *const ()),       // 0xb8
    (24, state::stencil_ref as *const ()),        // 0xc0
    (25, set_pipeline as *const ()),              // 0xc8
    (26, barrier as *const ()),                   // 0xd0
    (32, state::heaps as *const ()),              // 0x100
    (33, ignored_state as *const ()),             // 0x108 compute root signature
    (34, ignored_state as *const ()),             // 0x110 graphics root signature
    (35, state::compute_table as *const ()),      // 0x118
    (36, state::graphics_table as *const ()),     // 0x120
    (37, state::compute_constant as *const ()),   // 0x128
    (38, state::graphics_constant as *const ()),  // 0x130
    (39, state::compute_constants as *const ()),  // 0x138
    (40, state::graphics_constants as *const ()), // 0x140
    (41, state::compute_view as *const ()),       // 0x148 CBV
    (42, state::graphics_view as *const ()),      // 0x150 CBV
    (43, state::compute_view as *const ()),       // 0x158 SRV
    (44, state::graphics_view as *const ()),      // 0x160 SRV
    (45, state::compute_view as *const ()),       // 0x168 UAV
    (46, state::graphics_view as *const ()),      // 0x170 UAV
    (47, state::index_buffer as *const ()),       // 0x178
    (48, state::vertex_buffers as *const ()),     // 0x180
    (50, state::targets as *const ()),            // 0x190
    (51, clear_dsv as *const ()),                 // 0x198
    (52, clear_rtv as *const ()),                 // 0x1a0
    (53, clear_uav_uint as *const ()),            // 0x1a8
    (54, clear_uav_float as *const ()),           // 0x1b0
    (59, execute_indirect as *const ()),          // 0x1d8 indirect
    (60, copy_memory as *const ()),               // 0x1e0
    (68, state::depth_bounds as *const ()),       // 0x220 depth bounds
    (69, write32 as *const ()),                   // 0x228
    (70, write64 as *const ()),                   // 0x230
    (71, wait_value as *const ()),                // 0x238, 32 bits
    (72, wait_value64 as *const ()),              // 0x240, 64 bits
    (73, ignored_state as *const ()),             // 0x248 pixel shader depth order
    (83, copy_texture_region_x as *const ()),     // 0x298
    (84, copy_resource_x as *const ()),           // 0x2a0
    (124, ignored_state as *const ()),            // 0x3e0 shading rate
    (127, state::dispatch_mesh as *const ()),     // 0x3f8 mesh dispatch
];
/// The rest of [`METHODS`], kept apart only to stay readable.
const MORE_METHODS: [(usize, *const ()); 9] = [
    (28, query_mark as *const ()),             // 0xe0 BeginQuery
    (29, query_mark as *const ()),             // 0xe8 EndQuery
    (30, resolve_queries as *const ()),        // 0xf0
    (78, dispatch_x as *const ()),             // 0x270
    (98, state::dispatch_mesh_x as *const ()), // 0x310
    (99, driver_hint as *const ()),            // 0x318
    // Pipeline syncs at the top and bottom of the pipe, 0x408 and 0x410.
    (129, sync as *const ()),
    (130, sync as *const ()),
    (137, reset_x as *const ()), // 0x448
];

/// Newer titles' lists have two more methods in the console part of the
/// base list, after the clears (0x198/0x1a0 as before) and before
/// ExecuteIndirect, which moved from 0x1d8 to 0x1e8: every slot from 0x1d8
/// on is two later (taken here as starting exactly at 0x1d8). Inferred from
/// The title's call shapes: ExecuteIndirect 0x1e8, bottom-of-pipe
/// writes 0x238/0x240, waits 0x248/0x250, depth bounds 0x230, shading rate
/// 0x3f0/0x400, syncs 0x418/0x420, ResetX 0x458 (asserts on failure).
const fn newer_slot(slot: usize) -> usize {
    if slot >= 59 { slot + 2 } else { slot }
}

/// Methods only newer titles' lists have: enhanced barrier at 0x490 `(group
/// count, groups)` with 16-byte groups `(type, count, barriers)` and 40-byte
/// buffer barriers, plus 0x470 `(u64)`.
/// shortcut: 0x470's purpose is unknown; ignored until needed.
const NEWER_METHODS: [(usize, *const ()); 3] = [
    (142, ignored_state as *const ()),
    (146, barrier_groups as *const ()),
    (148, allocate_upload as *const ()),
];

const fn list_table(newer: bool) -> [*const (); 160] {
    let mut f = object::d3d12_object(object::missing_table::<LIST, 160>());
    let mut i = 0;
    while i < METHODS.len() + MORE_METHODS.len() {
        let (slot, method) = if i < METHODS.len() {
            METHODS[i]
        } else {
            MORE_METHODS[i - METHODS.len()]
        };
        f[if newer { newer_slot(slot) } else { slot }] = method;
        i += 1;
    }
    let mut i = 0;
    while newer && i < NEWER_METHODS.len() {
        f[NEWER_METHODS[i].0] = NEWER_METHODS[i].1;
        i += 1;
    }
    f
}

/// Every slot up to the limit exists and logs, for the methods newer
/// titles add.
static LIST_FUNCTIONS: Table<160> = Table(list_table(false));
static NEWER_LIST_FUNCTIONS: Table<160> = Table(list_table(true));
/// Of lists whose calls are written down.
#[cfg(windows)]
static THIN_FUNCTIONS: Table<160> = Table(thin::table(false));
#[cfg(windows)]
static NEWER_THIN_FUNCTIONS: Table<160> = Table(thin::table(true));

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::slot;

    #[test]
    fn every_inline_command_falls_back_to_the_table() {
        let null = std::ptr::null();
        let mut allocator = null;
        assert_eq!(
            create_allocator(null, 0, &ALLOCATOR_IIDS[0], &mut allocator),
            S_OK
        );
        let mut list = null;
        let hr = create_list(null, 0, 0, allocator, null, &LIST_IIDS[1], &mut list);
        assert_eq!(hr, S_OK);
        // What the title's inline draw checks: put < draw limit.
        // SAFETY: the list is 0x560 bytes.
        let (put, draw_limit, api_limit) = unsafe {
            let at = |o: usize| list.cast::<u8>().add(o).cast::<u64>().read();
            (at(0x18), at(0x28), at(0x30))
        };
        assert!(put >= draw_limit && put >= api_limit);
        // The console's ID3D12GraphicsCommandList6, as the title asks.
        let mut same = null;
        assert_eq!(list_query(list, &LIST_IIDS[7], &mut same), S_OK);
        assert_eq!(same, list);
        assert_eq!(slot::<extern "system" fn(This) -> u32>(list, 0x10)(list), 1);
        assert_eq!(slot::<extern "system" fn(This) -> u32>(list, 0x40)(list), 0);
        assert_eq!(
            slot::<extern "system" fn(This) -> HRESULT>(list, 0x48)(list),
            S_OK
        );
        // List-owned upload memory: aligned, writable, gone at reset.
        let p = allocate_upload(list, 40, 0x100);
        assert!(!p.is_null() && (p as usize).is_multiple_of(0x100));
        // SAFETY: 40 bytes were handed out.
        unsafe { p.write_bytes(0xab, 40) };
        assert!(allocate_upload(list, 8, 0x30).is_null(), "not a power of two");
        assert_eq!(reset(list, allocator, null), S_OK);
        // SAFETY: the list is ours.
        assert!(unsafe { &(*list.cast::<List>()).uploads }.lock().unwrap().is_empty());
        let release = |o: This| slot::<extern "system" fn(This) -> u32>(o, 0x10)(o);
        assert_eq!(release(list), 0);
        assert_eq!(release(allocator), 0);
    }

    /// Slots 53 and 54 (0x1a8, 0x1b0: a title's upscaler calls the first) are the unordered access clears
    /// in all four tables, the ones where the newer layout does not move them (it moves slots from 59 on); a
    /// call without a view at its handles skips the clear and the list still closes.
    #[test]
    fn slots_53_and_54_are_the_unordered_access_clears() {
        type Clear = extern "system" fn(This, u64, u64, This, *const [u32; 4], u32, *const Rect);
        assert_eq!((53 * 8, 54 * 8), (0x1a8, 0x1b0));
        assert_eq!((newer_slot(53), newer_slot(54), newer_slot(59)), (53, 54, 61));
        let direct = [clear_uav_uint as Clear as *const (), clear_uav_float as Clear as *const ()];
        for table in [&LIST_FUNCTIONS.0, &NEWER_LIST_FUNCTIONS.0] {
            assert_eq!([table[53], table[54]], direct);
        }
        #[cfg(windows)]
        for table in [&THIN_FUNCTIONS.0, &NEWER_THIN_FUNCTIONS.0] {
            // Written down, not done: other functions than the direct ones, and not the missing-method ones.
            assert!(table[53] != direct[0] && table[54] != direct[1] && table[53] != table[54]);
            assert!(table[53] != LIST_FUNCTIONS.0[52] && table[54] != LIST_FUNCTIONS.0[55]);
        }
        let null = std::ptr::null();
        let mut allocator = null;
        assert_eq!(create_allocator(null, 0, &ALLOCATOR_IIDS[0], &mut allocator), S_OK);
        let mut list = null;
        assert_eq!(create_list(null, 0, 0, allocator, null, &LIST_IIDS[1], &mut list), S_OK);
        let values = [0u32; 4];
        slot::<Clear>(list, 0x1a8)(list, 0, 0, null, &values, 0, std::ptr::null());
        slot::<Clear>(list, 0x1b0)(list, 0x10, 0x10, null, std::ptr::null(), 3, std::ptr::null());
        assert_eq!(slot::<extern "system" fn(This) -> HRESULT>(list, 0x48)(list), S_OK);
        let release = |o: This| slot::<extern "system" fn(This) -> u32>(o, 0x10)(o);
        assert_eq!(release(list), 0);
        assert_eq!(release(allocator), 0);
    }

    /// An offset near `u64::MAX` must not overflow and write outside the
    /// query destination buffer.
    #[test]
    fn resolve_queries_rejects_overflowing_offset() {
        let null = std::ptr::null();
        let mut allocator = null;
        assert_eq!(
            create_allocator(null, 0, &ALLOCATOR_IIDS[0], &mut allocator),
            S_OK
        );
        let mut list = null;
        assert_eq!(
            create_list(null, 0, 0, allocator, null, &LIST_IIDS[1], &mut list),
            S_OK
        );
        let buffer = crate::resource::test_buffer(64);
        // Used to overflow `offset + count * 8` (panic in debug, wrap in release).
        resolve_queries(list, null, 0, 0, 2, buffer, u64::MAX - 8);
        let release = |o: This| slot::<extern "system" fn(This) -> u32>(o, 0x10)(o);
        for object in [buffer, list, allocator] {
            assert_eq!(release(object), 0);
        }
    }

    #[cfg(windows)]
    const KEEP: Keep = Keep { lists: 8, bytes: 1000, small: 256, big: 512, reserve: 0 };

    /// Small recordings' lists are kept up to the number of lists, bigger
    /// ones up to a quarter of it, and none past the big line or the bytes.
    #[cfg(windows)]
    #[test]
    fn idle_lists_are_kept_by_the_size_of_their_recording() {
        let mut idle: Classes<u32> = Classes::new();
        for i in 0..8 {
            assert!(idle.room(i, KEEP));
            assert_eq!(idle.give(i, i * 32, 0, KEEP), None);
        }
        // The small ones are full; a ninth comes back.
        assert!(!idle.room(256, KEEP));
        assert_eq!(idle.give(100, 256, 0, KEEP), Some(100));
        // Bigger ones up to two (8 / 4), past the line none.
        assert_eq!(idle.give(200, 257, 0, KEEP), None);
        assert_eq!(idle.give(201, 512, 0, KEEP), None);
        assert_eq!(idle.give(202, 300, 0, KEEP), Some(202));
        assert_eq!(idle.give(203, 513, 0, KEEP), Some(203));
        assert_eq!((idle.small.len(), idle.big.len(), idle.len()), (8, 2, 10));
        // The bytes kept between them.
        let mut bytes: Classes<u32> = Classes::new();
        assert_eq!(bytes.give(1, 0, 600, KEEP), None);
        assert_eq!(bytes.give(2, 0, 401, KEEP), Some(2));
        assert_eq!(bytes.give(3, 300, 400, KEEP), None);
        assert_eq!(bytes.bytes, 1000);
        assert_eq!(bytes.take(Some(0), KEEP), Some(1));
        assert_eq!(bytes.bytes, 400);
    }

    /// A recording takes a list of its own size, the one given back last,
    /// else one of the other size, else none; unknown is small.
    #[cfg(windows)]
    #[test]
    fn a_recording_takes_a_list_of_its_size_first() {
        let mut idle: Classes<u32> = Classes::new();
        assert_eq!(idle.take(None, KEEP), None);
        assert_eq!(idle.give(1, 10, 0, KEEP), None);
        assert_eq!(idle.give(2, 20, 0, KEEP), None);
        assert_eq!(idle.give(3, 400, 0, KEEP), None);
        assert_eq!(idle.take(Some(1000), KEEP), Some(3));
        assert_eq!(idle.take(Some(1000), KEEP), Some(2));
        assert_eq!(idle.give(4, 300, 0, KEEP), None);
        assert_eq!(idle.take(None, KEEP), Some(1));
        assert_eq!(idle.take(Some(5), KEEP), Some(4));
        assert_eq!(idle.take(Some(5), KEEP), None);
    }

    /// Without classes (`VICTORY_IDLE_SMALL=0`) and without the big line
    /// (`VICTORY_IDLE_BIG=0`): one pool of the number of lists, as before.
    #[cfg(windows)]
    #[test]
    fn without_classes_the_pool_is_one() {
        let keep = Keep { lists: 3, bytes: u64::MAX, small: 0, big: 0, reserve: 0 };
        let mut idle: Classes<u32> = Classes::new();
        for (i, binds) in [0, 100_000, 3].into_iter().enumerate() {
            assert_eq!(idle.give(i as u32, binds, 0, keep), None);
        }
        assert_eq!(idle.give(9, 1, 0, keep), Some(9));
        assert_eq!((idle.small.len(), idle.big.len()), (3, 0));
        assert_eq!(idle.take(Some(100_000), keep), Some(2));
    }

    /// The line past which idle lists go: a setting wins, else 2048 as
    /// before.
    #[cfg(windows)]
    #[test]
    fn big_idle_lists_go_past_the_line_set() {
        assert_eq!(idle_big_for(None), 2048);
        assert_eq!(idle_big_for(Some(512)), 512);
        assert_eq!(idle_big_for(Some(0)), 0);
    }

    /// On a PC of 24 GB or less the idle lists hold a frame's (the title
    /// records some 220 a frame): 384 of small recordings and 96 bigger,
    /// beside the 16 kept back, and none past the big line. The bytes are
    /// the upload memory a list keeps after its reset: none while lists
    /// share their scratch chunks, so they let none of those go; lists with
    /// chunks of their own (`VICTORY_SCRATCH_POOL=0`) fit 16 to the 64 MiB,
    /// as before, and none of them is kept back. `VICTORY_IDLE_LISTS=128`
    /// gives the number of before; other PCs keep theirs.
    #[cfg(windows)]
    #[test]
    fn the_idle_lists_hold_a_frame_of_lists_on_a_pc_of_24_gb_or_less() {
        assert_eq!(idle_most_for(true, None), (384, 64 << 20));
        assert_eq!(idle_most_for(true, Some(128)), (128, 64 << 20));
        assert_eq!(idle_most_for(false, None), (512, 256 << 20));
        assert_eq!(idle_most_for(false, Some(64)), (64, 256 << 20));
        let (lists, bytes) = idle_most_for(true, None);
        let keep = Keep { lists, bytes, small: 256, big: idle_big_for(None), reserve: 16 };
        let mut idle: Classes<u32> = Classes::new();
        for i in 0..16 + 384 {
            assert_eq!(idle.give(i, i % 257, 0, keep), None, "list {i}");
        }
        assert_eq!((idle.reserve.len(), idle.small.len()), (16, 384));
        assert_eq!(idle.give(1000, 10, 0, keep), Some(1000));
        for i in 0..96 {
            assert_eq!(idle.give(2000 + i, 257 + i, 0, keep), None);
        }
        assert_eq!(idle.give(3000, 2048, 0, keep), Some(3000), "the bigger ones are full");
        assert_eq!(idle.give(3001, 2049, 0, keep), Some(3001), "past the big line");
        assert!(keep.past_big(2049) && !keep.past_big(2048));
        assert_eq!((idle.len(), idle.bytes), (480, 0));
        // Chunks of their own: 16 lists of 4 MiB.
        let mut own: Classes<u32> = Classes::new();
        let kept = (0..100).filter(|&i| own.give(i, 10, 4 << 20, keep).is_none()).count();
        assert_eq!((kept, own.reserve.len(), own.bytes), (16, 0, 64 << 20));
    }

    /// The memory line's most idle at once: the most since its last look,
    /// then counted again from those idle then.
    #[cfg(windows)]
    #[test]
    fn the_most_idle_at_once_is_counted_between_two_looks() {
        let mut idle: Classes<u32> = Classes::new();
        assert_eq!(idle.take_most(), 0);
        for i in 0..5 {
            assert_eq!(idle.give(i, 10, 0, KEEP), None);
        }
        assert_eq!((idle.take(None, KEEP), idle.take(None, KEEP)), (Some(4), Some(3)));
        assert_eq!(idle.take_most(), 5);
        assert_eq!(idle.take_most(), 3, "those idle now");
        assert_eq!(idle.give(9, 300, 0, KEEP), None);
        assert_eq!(idle.take_most(), 4);
    }

    /// The memory line's counts per frame: what each count grew by since its
    /// last look, and the frames begun since.
    #[cfg(windows)]
    #[test]
    fn the_memory_line_counts_since_its_last_look() {
        let mut since = Since::new();
        assert_eq!(since.take(10, [5, 0, 7, 1, 2, 0]), (10, [5, 0, 7, 1, 2, 0]));
        assert_eq!(since.take(25, [9, 0, 7, 4, 2, 3]), (15, [4, 0, 0, 3, 0, 3]));
        assert_eq!(since.take(25, [9, 0, 7, 4, 2, 3]), (0, [0; 6]));
    }

    /// With lazy host lists: a submitted list hands its host list over and
    /// keeps none; handed over again without a reset it has none to give;
    /// its reset takes one, open and empty already; and a host list in the
    /// air (taken, not yet given back) is never the one a reset takes.
    /// Without, the list keeps a closed one, as before, which its reset
    /// resets.
    #[cfg(windows)]
    #[test]
    fn a_lazy_list_holds_a_host_list_only_from_its_reset_to_its_submission() {
        let Some(host) = crate::host::host() else { return };
        let raw = |h: &crate::host::HostList| windows::core::Interface::as_raw(&h.list) as usize;
        let mut lists: Vec<Box<List>> = (0..3).map(|_| boxed(0, std::ptr::null(), false)).collect();
        let mut flying = Vec::new();
        for list in &mut lists {
            let made = list.host.as_ref().map(raw).expect("a host list");
            list.host.as_ref().unwrap().close();
            let taken = hand_over_as(list, true, |_| panic!("a lazy list is left none")).expect("its host list");
            assert_eq!(raw(&taken), made);
            assert!(list.host.is_none());
            // Twice without a reset: nothing to hand over, still none.
            assert!(hand_over_as(list, true, |_| panic!("a lazy list is left none")).is_none());
            assert!(list.host.is_none());
            flying.push(taken);
        }
        let in_air: Vec<usize> = flying.iter().map(raw).collect();
        // Each reset takes one, open and empty, and none of those in the air.
        for list in &mut lists {
            host_for_reset(list);
            let host_list = list.host.as_ref().expect("a host list from the reset");
            assert!(!in_air.contains(&raw(host_list)));
            assert!(host_list.untouched());
            assert_eq!(list.holding.0.load(std::sync::atomic::Ordering::Relaxed), Holding::OPEN);
        }
        // Not lazy: a closed one is left, and the recording is taken.
        let list = &mut lists[0];
        let recorded = list.host.as_ref().map(raw);
        list.host.as_ref().unwrap().close();
        let taken = hand_over_as(list, false, |binds| fresh_list(Some(binds), For::Detach)).expect("its host list");
        assert_eq!(Some(raw(&taken)), recorded);
        let left = list.host.as_ref().map(raw).expect("a closed host list left");
        assert_ne!(left, raw(&taken));
        assert!(!in_air.contains(&left));
        // The reset of such a list resets the one it has.
        host_for_reset(list);
        assert_eq!(list.host.as_ref().map(raw), Some(left));
        assert!(list.host.as_ref().unwrap().untouched());
        drop(taken);
        drop(flying);
        // SAFETY: plain query.
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
    }

    /// The lists that come back after they ran fill the reserve first, with
    /// those of small recordings that keep no upload memory, then the idle
    /// ones; a recording never takes one of the reserve, only a want of
    /// memory does ([`Classes::take_reserve`]); and a short reserve has room
    /// where the idle ones are full.
    #[cfg(windows)]
    #[test]
    fn the_reserve_fills_first_and_is_taken_only_for_want_of_memory() {
        let keep = Keep { reserve: 2, ..KEEP };
        let mut idle: Classes<u32> = Classes::new();
        assert_eq!(idle.give(1, 10, 0, keep), None);
        // A bigger recording's list, and one that keeps upload memory, are
        // not for the reserve.
        assert_eq!(idle.give(2, 300, 0, keep), None);
        assert_eq!(idle.give(3, 20, 64, keep), None);
        assert_eq!(idle.give(4, 30, 0, keep), None);
        assert_eq!((idle.reserve.len(), idle.small.len(), idle.big.len(), idle.len()), (2, 1, 1, 2));
        // Full: the next goes with the idle ones.
        assert_eq!(idle.give(5, 40, 0, keep), None);
        assert_eq!(idle.take(None, keep), Some(5));
        assert_eq!(idle.take(None, keep), Some(3));
        assert_eq!(idle.take(Some(10), keep), Some(2));
        assert_eq!(idle.take(None, keep), None);
        assert_eq!(idle.take_reserve(), Some(4));
        assert_eq!(idle.take_reserve(), Some(1));
        assert_eq!(idle.take_reserve(), None);
        // No room among the idle ones, but in the reserve while it is short.
        let tight = Keep { lists: 1, reserve: 1, ..KEEP };
        let mut idle: Classes<u32> = Classes::new();
        assert_eq!(idle.give(1, 10, 0, tight), None);
        assert_eq!(idle.give(2, 10, 0, tight), None);
        assert!(!idle.room(10, tight));
        assert_eq!(idle.give(3, 10, 0, tight), Some(3));
        assert_eq!(idle.take_reserve(), Some(1));
        assert!(idle.room(10, tight));
        assert!(!idle.room(10_000, tight), "past the big line, as before");
        // Without a reserve (`VICTORY_LIST_RESERVE=0`) nothing is kept back.
        let mut none: Classes<u32> = Classes::new();
        assert_eq!(none.give(1, 10, 0, KEEP), None);
        assert_eq!(none.take_reserve(), None);
    }

    /// Lazy host lists are the default on PCs of 24 GiB or less, and where
    /// the RAM is not known off; a setting wins either way.
    #[cfg(windows)]
    #[test]
    fn lazy_lists_are_the_default_on_pcs_of_24_gib_or_less() {
        use std::ffi::OsStr;
        let gib = 1u64 << 30;
        assert!(lazy_for(None, Some(16 * gib)));
        assert!(lazy_for(None, Some(24 * gib)));
        assert!(!lazy_for(None, Some(24 * gib + 1)));
        assert!(!lazy_for(None, Some(64 * gib)));
        assert!(!lazy_for(None, None));
        assert!(!lazy_for(Some(OsStr::new("0")), Some(16 * gib)));
        assert!(lazy_for(Some(OsStr::new("1")), Some(64 * gib)));
        assert!(lazy_for(Some(OsStr::new("1")), None));
    }

    /// A reset that runs out of memory is tried again after waits that
    /// double from 1 ms, the last cut to what is left, up to the time asked
    /// (none for the lists that ran, which ask none); in the second after one
    /// gave up the others wait no more.
    #[cfg(windows)]
    #[test]
    fn a_reset_out_of_memory_waits_doubling_up_to_its_patience() {
        let (mut waited, mut waits, mut all) = (0, 0, Vec::new());
        while let Some(ms) = oom::next_wait(waited, 250, waits) {
            all.push(ms);
            waited += ms;
            waits += 1;
        }
        assert_eq!(all, [1, 2, 4, 8, 16, 32, 64, 123]);
        assert_eq!(waited, 250);
        assert_eq!(oom::next_wait(0, 0, 0), None);
        assert_eq!(oom::next_wait(40, 30, 3), None);
        assert_eq!(oom::patience_at(5_000, 0, 250), 250);
        assert_eq!(oom::patience_at(5_000, 4_500, 250), 0);
        assert_eq!(oom::patience_at(5_499, 4_500, 250), 0);
        assert_eq!(oom::patience_at(5_500, 4_500, 250), 250);
    }

    /// `VICTORY_OOM_INJECT=<n>`: every n-th reset, counted from 1; nothing
    /// for 0, unset or what is not a number.
    #[cfg(windows)]
    #[test]
    fn every_nth_reset_fails_with_the_injection() {
        assert_eq!(oom::inject_every(None), 0);
        assert_eq!(oom::inject_every(Some("")), 0);
        assert_eq!(oom::inject_every(Some("x")), 0);
        assert_eq!(oom::inject_every(Some(" 3 ")), 3);
        let failed: Vec<u64> = (1..=10).filter(|&n| oom::every_nth(3, n)).collect();
        assert_eq!(failed, [3, 6, 9]);
        assert!((1..=10).all(|n| !oom::every_nth(0, n)));
        assert!((1..=5).all(|n| oom::every_nth(1, n)));
    }

    /// A title list whose host list's reset runs out of memory lets it go and
    /// takes the one it is given (an idle one, a new one or one of the
    /// reserve), open and empty; given none it holds none and records
    /// nothing. A submission that finds no host list to leave the list hands
    /// its own over all the same (as with lazy host lists), and the list's
    /// reset takes one.
    #[cfg(windows)]
    #[test]
    fn a_reset_out_of_memory_lets_the_host_list_go_and_takes_another() {
        let Some(host) = crate::host::host() else { return };
        use windows::Win32::Foundation::E_OUTOFMEMORY;
        let raw = |h: &crate::host::HostList| windows::core::Interface::as_raw(&h.list) as usize;
        let out_of_memory = |_: &crate::host::HostList| Err(windows::core::Error::from_hresult(E_OUTOFMEMORY));
        fn open(list: &List) -> u8 {
            list.holding.0.load(std::sync::atomic::Ordering::Relaxed)
        }
        let mut list = boxed(0, std::ptr::null(), false);
        list.last_binds = 7;
        let failing = list.host.as_ref().map(raw).expect("a host list");
        list.host.as_ref().unwrap().close();
        // (Made here, it stands for one of the reserve.)
        let spare = host.new_list().expect("a host list");
        let spare_raw = raw(&spare);
        let mut asked = None;
        host_for_reset_with(&mut list, out_of_memory, |binds| {
            asked = Some(binds);
            Some(spare)
        });
        assert_eq!(asked, Some(7), "of the size of the last recording");
        assert_eq!(list.host.as_ref().map(raw), Some(spare_raw));
        assert_ne!(spare_raw, failing);
        assert!(list.host.as_ref().unwrap().untouched());
        assert_eq!(open(&list), Holding::OPEN);
        // None to take.
        list.host.as_ref().unwrap().close();
        host_for_reset_with(&mut list, out_of_memory, |_| None);
        assert!(list.host.is_none());
        assert_eq!(open(&list), Holding::NONE);
        // A submission with no host list to leave: the list's own goes.
        let mut other = boxed(0, std::ptr::null(), false);
        let made = other.host.as_ref().map(raw).expect("a host list");
        other.host.as_ref().unwrap().close();
        let taken = hand_over_as(&mut other, false, |_| None).expect("its host list");
        assert_eq!(raw(&taken), made);
        assert!(other.host.is_none());
        assert_eq!(open(&other), Holding::NONE);
        host_for_reset_with(&mut other, crate::host::HostList::try_reset, |_| host.new_list().ok());
        assert!(other.host.as_ref().is_some_and(|h| h.untouched()));
        assert_eq!(open(&other), Holding::OPEN);
        drop(taken);
        // SAFETY: plain query.
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
    }

    /// At a landing only the lists of the flight that landed are reset: open,
    /// empty, their scratch memory out of them, kept for reuse; a list in the
    /// air keeps its recording. What the recording held goes to the record
    /// for the bury lag and is nobody's while that lives (the landed list's
    /// scratch chunk is not handed out again); a list whose reset ran out of
    /// memory is not kept but goes with the record.
    #[cfg(windows)]
    #[test]
    fn a_landed_flight_is_reset_at_once_and_its_recording_waits_for_the_bury_lag() {
        let Some(host) = crate::host::host() else { return };
        use windows::Win32::Foundation::E_OUTOFMEMORY;
        let raw = |h: &crate::host::HostList| windows::core::Interface::as_raw(&h.list) as usize;
        use windows::Win32::Graphics::Direct3D12::ID3D12Resource;
        let buffer = |b: &ID3D12Resource| windows::core::Interface::as_raw(b) as usize;
        let pooled = std::env::var_os("VICTORY_SCRATCH_POOL").is_none_or(|v| v != "0");
        let (mut to, from) = ([0u8; 4], [1u8; 4]);
        // A recording with scratch memory and a step (never run), closed.
        let mut recorded = || {
            let list = host.new_list().expect("a host list");
            let (scratch, _, data) = list.scratch_held(64).expect("scratch memory");
            // SAFETY: 64 bytes of the list's scratch memory.
            unsafe { data.write_bytes(0x5a, 64) };
            list.defer_copy(to.as_mut_ptr(), from.as_ptr(), 4);
            list.close();
            (list, scratch)
        };
        let detached = |host: crate::host::HostList| Detached {
            host,
            _uploads: Vec::new(),
            _state: Default::default(),
            _taken: Taken::new(),
        };
        let (landing, landing_scratch) = recorded();
        let (flying, _) = recorded();
        let landing_raw = raw(&landing);
        let flight = vec![detached(landing)];
        let in_air = vec![detached(flying)];
        let (mut kept, mut buried) = (Vec::new(), None);
        reset_landed(
            flight,
            |host| host.reset_ran(true),
            |list, binds| {
                kept.push((list, binds));
                None
            },
            |landed| buried = Some(landed),
        );
        let (back, binds) = kept.pop().expect("the landed list, kept");
        assert_eq!((raw(&back), binds), (landing_raw, 0));
        assert!(back.untouched());
        let air = in_air[0].host();
        assert!(!air.untouched(), "a list in the air is not reset");
        assert!(air.scratch_kept() > 0);
        let landed = buried.expect("the record for the bury lag");
        assert_eq!((landed._recordings.len(), landed._states.len(), landed._let_go.len()), (1, 1, 0));
        if pooled {
            assert_eq!(back.scratch_kept(), 0);
            let (again, _, _) = back.scratch_held(64).expect("scratch memory");
            assert_ne!(buffer(&again), buffer(&landing_scratch), "the landed chunk waits with the record");
        }
        drop(landed);
        // A reset that ran out of memory: not kept, with the record.
        let (failing, _) = recorded();
        let mut buried = None;
        reset_landed(
            vec![detached(failing)],
            |_| (None, Err(windows::core::Error::from_hresult(E_OUTOFMEMORY))),
            |_, _| panic!("a list whose reset failed is not kept"),
            |landed| buried = Some(landed),
        );
        assert_eq!(buried.expect("the record")._let_go.len(), 1);
        drop((back, in_air));
        // SAFETY: plain query.
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
    }
}
