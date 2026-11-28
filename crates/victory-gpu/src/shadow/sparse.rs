//! Sparse windows of title memory (`VICTORY_SPARSE_WINDOWS`: on by default
//! where the CPU sides are plain, a PC of 24 GiB or less; `=0` off, as
//! before; `=1` on any PC; [`on`] says why).
//!
//! A window of title memory outside buffers was a host buffer in the PC's
//! memory, 4 MiB for an ask without a length, however little of it the
//! title has mapped; a sized ask in the same cell got another one, of the
//! same bytes. When allocations began to fail the windows held more than half
//! of the PC's memory the card shares: most of them lay over far less title
//! memory, gigabytes over pages the title has not mapped, and the trims freed
//! nothing, because a descriptor slot showed nearly every one.
//!
//! Here a 4 MiB cell of title addresses has one copy, whatever asks for it,
//! and its buffer has no memory of its own:
//! - the side shaders see is a reserved buffer of 4 MiB (64 tiles of
//!   64 KiB). A tile gets memory, from heaps of the card's own
//!   (`tilepool`), when something asks for bytes in it; a tile without
//!   reads zeros (mapped to nothing where the card drops writes there,
//!   tiled resources tier 2; else, or with `VICTORY_SPARSE_ZERO_TILE=1`, to
//!   one tile of zeros all share);
//! - the CPU's side is 4 MiB of addresses of ours, committed tile by tile
//!   as they are asked for. It is no host buffer, so it is nothing the PC
//!   shares with the card (`plain_sides` says why that works);
//! - an ask on any thread commits the CPU's tiles and notes the tiles it
//!   wants ([`Shadow::sparse_ensure`]; every path that writes the CPU's side
//!   goes through it first). The submitting thread gives the wanted tiles
//!   their memory at the top of `Host::card_before` ([`before`]), ahead on
//!   the queue of the list that sends the bytes up, and sends the whole
//!   tile from the CPU's side (a tile holds what its last owner left);
//! - a cell nothing asked for in `VICTORY_SPARSE_IDLE_MS` (1500) and that no
//!   list holds loses its tiles and the CPU side's pages ([`evict`]). The
//!   buffer and its address on the card stay, so a descriptor slot that
//!   shows it never dangles: it reads zeros until the next ask, which gives
//!   the tiles back and copies the bytes anew (every page counts as never
//!   copied);
//! - an ask without a length (a root view) reads from its address on: its
//!   tiles are those to the end of the run of memory the title has
//!   committed there ([`Cell::run_end`]; `VICTORY_WINDOW_RUNS=0`: to the
//!   cell's end);
//! - the tiles given are at most `VICTORY_SPARSE_WINDOWS_MB` (unset: 2048 or
//!   what the card had free of nine tenths of its budget at the first ask,
//!   whichever is less). With no room, a new cell is not made and the ask
//!   gets a window as before; new cells leave the last sixteenth of that to
//!   the cells there, and a sized ask whose tiles find no room in it gets a
//!   window as before too, instead of tiles that read zeros
//!   (`VICTORY_SPARSE_FALLBACK=0`: neither, as before; [`Shadow::sparse_fits`]).
//!
//! Before the first cell is made the same calls are tried on a buffer and a
//! queue of the probe's own ([`probe`]); if the card does not answer as
//! expected no cell is ever made. `VICTORY_SPARSE_PROBE=1` runs the probe
//! and says what it found, with the switch on or off.
//!
//! Never a tile of a heap opened on title memory (`crate::inplace`, at its
//! head: that removes the device). The pool's heaps are the card's own.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;
use windows_sys::Win32::System::Memory as mem;

use super::tilepool::{self, Pool, TILE, Tile};
use super::{CARD_BYTES, CARDED, Card, Shadow, WINDOW, card_hash_bytes};

/// Tiles of a cell.
const TILES: usize = (WINDOW / TILE) as usize;

/// Whether windows are made sparse: the switch, with what it rests on
/// (copies in the card's memory, and sends taken out of the CPU's sides into
/// the submission's own memory); [`sparse_of`].
pub(super) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let set = std::env::var("VICTORY_SPARSE_WINDOWS").ok();
        let send_own = std::env::var_os("VICTORY_SEND_OWN").is_none_or(|v| v != "0");
        let plain = super::plain_sides() && !cfg!(test);
        let on = sparse_of(set.as_deref(), plain, send_own, super::cards());
        tracing::info!(
            on,
            set = set.as_deref().unwrap_or("(not set)"),
            plain_sides = plain,
            send_own,
            card_buffers = super::cards(),
            "sparse windows of title memory (VICTORY_SPARSE_WINDOWS; on by default with plain CPU sides, =0: windows as before)"
        );
        on
    })
}

/// [`on`] for `VICTORY_SPARSE_WINDOWS` as set, whether the CPU sides are
/// plain (`super::plain_sides`; unit tests keep windows as before unless they
/// say otherwise: cells need the card), whether sends are taken into the
/// submission's own memory, and whether copies may lie in the card's memory.
///
/// On by default where the sides are plain, a PC of 24 GiB or less: there
/// thousands of windows held two thirds of the PC's memory the card shares,
/// over about half as much title memory (hundreds asked for without a length
/// at 4 MiB each), every byte of it resident, with a few hundred MB of the
/// PC's memory free while the card had gigabytes of its own free. As cells, a
/// 4 MiB cell is one copy
/// whatever asks for it, tiles of the card's memory only where something
/// asked, and the CPU's side pageable memory committed tile by tile: what
/// the windows of such a set cost is measured in
/// `tests::a_window_set_like_the_runs_costs_less_as_cells`. The probe passed
/// on an 8 GB card (`VICTORY_SPARSE_PROBE=1`), and it runs before
/// the first cell on every card: one that does not answer as expected gets
/// windows as before.
fn sparse_of(set: Option<&str>, plain: bool, send_own: bool, cards: bool) -> bool {
    send_own
        && cards
        && match set.map(str::trim) {
            Some("1") => true,
            Some("0") => false,
            _ => plain,
        }
}

/// Whether asks take cells now: [`on`], and the probe has not failed (nor
/// the card been found without what cells need). Once it has, answers are
/// the table's alone again, and threads may remember them
/// (`super::loose_found`).
pub(super) fn live() -> bool {
    on() && !matches!(RIG.get(), Some(None))
}

/// Whether the pool's last sixteenth is left to the cells there and a sized
/// ask whose tiles find no room gets a window as before
/// (`VICTORY_SPARSE_FALLBACK`, on unless `0`: the cell, whose tiles without
/// memory read zeros until there is room, as before).
///
/// A cell's buffer is what descriptor slots and lists show, so a cell cannot
/// give way to another copy once made; and nearly every window was held by
/// a slot or the pass over the heap, which keeps a cell's tiles from being
/// taken when idle.
/// With the pool full a new ask's tiles would get no memory and read zeros
/// until some cell lost its own: a window as before reads the title's bytes.
fn fallback() -> bool {
    crate::toggled!("VICTORY_SPARSE_FALLBACK")
}

/// Bytes of the tiles given ([`Pool::given_bytes`]), for asks that do not
/// take the pool's lock: set wherever the pool changes.
static GIVEN: AtomicU64 = AtomicU64::new(0);

/// The bytes of the tiles given now, for the run report.
pub(super) fn given_now() -> u64 {
    GIVEN.load(Ordering::Relaxed)
}

fn note_given(pool: &Pool) {
    GIVEN.store(pool.given_bytes(), Ordering::Relaxed);
}

/// Whether `missing` tiles more fit in a pool of `budget` bytes with `given`
/// bytes given.
fn tiles_fit(given: u64, missing: u32, budget: u64) -> bool {
    given.saturating_add(u64::from(missing) * TILE) <= budget
}

/// Whether a new cell may be made in a pool of `budget` bytes with `given`
/// bytes given: with `reserve` ([`fallback`]), while a whole cell's tiles
/// fit under the budget less its sixteenth; without, while a tile fits, as
/// before.
fn cell_fits(given: u64, budget: u64, reserve: bool) -> bool {
    match reserve {
        true => given.saturating_add(WINDOW) <= budget - budget / 16,
        false => given.saturating_add(TILE) <= budget,
    }
}

/// Asks given a window as before for want of room in the pool
/// ([`fallback`]), since the last line.
pub(super) fn fell_back() {
    STATS[5].fetch_add(1, Ordering::Relaxed);
}

/// Whether the probe is run and told at the first submission
/// (`VICTORY_SPARSE_PROBE=1`).
fn probe_asked() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SPARSE_PROBE").is_some_and(|v| v == "1"))
}

/// How long nothing must have asked for a cell before it loses its tiles
/// (`VICTORY_SPARSE_IDLE_MS`, 1500).
fn idle_ms() -> u64 {
    static MS: OnceLock<u64> = OnceLock::new();
    *MS.get_or_init(|| std::env::var("VICTORY_SPARSE_IDLE_MS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(1500))
}

/// Whether an ask without a length wants the tiles to the end of the run of
/// committed title memory it lies in, not to its cell's end (on unless
/// `VICTORY_WINDOW_RUNS=0`).
fn runs() -> bool {
    super::window_runs_set() != Some(false)
}

/// The tiles of the run's pool, and the tiles given.
static POOL: Mutex<Pool> = Mutex::new(Pool::new());

/// What cells are made with, once the probe has passed.
struct Rig {
    device: ID3D12Device,
    /// The most bytes of tiles given.
    budget: u64,
    /// The tile of zeros tiles without memory are mapped to; `None`: they
    /// are mapped to nothing.
    zero: Option<Tile>,
}

static RIG: OnceLock<Option<Rig>> = OnceLock::new();

fn rig() -> Option<&'static Rig> {
    RIG.get_or_init(|| {
        let host = crate::host::host()?;
        if !host.card_ready() {
            return None;
        }
        let mut device: Option<ID3D12Device> = None;
        // SAFETY: a live queue; the out pointer is ours.
        unsafe { host.queue().GetDevice(&mut device) }.ok()?;
        let device = device?;
        let found = probe(&device);
        found.tell();
        if !found.passed() {
            tracing::warn!("sparse windows are not used: the probe did not pass on this card (windows are made as before)");
            return None;
        }
        let set = std::env::var("VICTORY_SPARSE_WINDOWS_MB").ok().and_then(|v| v.trim().parse::<u64>().ok());
        let memory = host.video_memory();
        let budget = budget_of(set, memory.map(|[budget, used, ..]| (budget, used)));
        let zero_asked = std::env::var_os("VICTORY_SPARSE_ZERO_TILE").is_some_and(|v| v == "1");
        let zero = if zero_asked || found.tier < 2 {
            // The first tile of a new heap: zeros, and nothing of ours ever
            // writes it ([`Shadow::sparse_clip`]).
            let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
            let zero = pool.take(&device)?;
            note_given(&pool);
            Some(zero)
        } else {
            None
        };
        tracing::info!(
            "sparse windows: at most {} MB of the card in tiles ({}; the card has {} MB in use of a budget of {} MB); a tile \
             without memory {}; a cell idle for {} ms loses its tiles",
            budget >> 20,
            if set.is_some() { "VICTORY_SPARSE_WINDOWS_MB" } else { "2048 or what was free of nine tenths of the budget" },
            memory.map_or(0, |m| m[1] >> 20),
            memory.map_or(0, |m| m[0] >> 20),
            if zero.is_some() { "shows one tile of zeros" } else { "is mapped to nothing (reads zeros, drops writes)" },
            idle_ms()
        );
        (budget >= 64 << 20).then_some(Rig { device, budget, zero })
    })
    .as_ref()
}

/// The most bytes of tiles: `set` MiB, else 2048 MiB or what is free of nine
/// tenths of the card's budget (`memory`: budget and use), whichever is less.
fn budget_of(set: Option<u64>, memory: Option<(u64, u64)>) -> u64 {
    match (set, memory) {
        (Some(mb), _) => mb << 20,
        (None, Some((budget, used))) => (budget / 10 * 9).saturating_sub(used).min(2048 << 20),
        (None, None) => 2048 << 20,
    }
}

/// The cells alive.
static CELLS: Mutex<Vec<Weak<Shadow>>> = Mutex::new(Vec::new());

/// A cell wants tiles it has not, or has just been made.
static PENDING: AtomicBool = AtomicBool::new(false);

/// The last pass found no room for a wanted tile.
static STARVED: AtomicBool = AtomicBool::new(false);

/// The card is being relieved (`crate::host::budget`, once a second): the
/// next trim of idle cells takes those idle for half a second, as when a
/// wanted tile found no room, and the tile heaps that leaves empty go back.
static HURRY: AtomicBool = AtomicBool::new(false);

/// [`HURRY`].
pub(super) fn hurry() {
    HURRY.store(true, Ordering::Release);
}

/// Until when ([`crate::host::now_ms`]) no cell is made: the last one found
/// no room.
static NO_ROOM_UNTIL: AtomicU64 = AtomicU64::new(0);

/// Since the last line: tiles given, cells that lost their tiles and the
/// tiles they lost, wanted tiles left without memory, cells not made, asks
/// given a window as before ([`fell_back`]).
static STATS: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

/// What a sparse window has beside a copy's own: a bit for each of its 64
/// tiles.
pub(super) struct Cell {
    /// The tiles of the CPU's side that are committed.
    committed: AtomicU64,
    /// The tiles something asked for bytes in.
    wanted: AtomicU64,
    /// The tiles of the buffer that have memory. Written by [`before`] only.
    mapped: AtomicU64,
    /// With a tile of zeros: the buffer's tiles do not show it yet.
    fresh: AtomicBool,
    /// The pool's tile of each mapped tile; held while the CPU's side is
    /// committed or given up.
    tiles: Mutex<[Tile; TILES]>,
    /// The tiles of title memory with committed pages, and when that was
    /// asked ([`crate::host::now_ms`]; 0: never).
    title: [AtomicU64; 2],
}

/// The tiles that hold offsets `[from, to)` of a cell.
fn tiles_of(from: usize, to: usize) -> u64 {
    let to = to.min(WINDOW as usize);
    if from >= to {
        return 0;
    }
    let (a, b) = (from / TILE as usize, (to - 1) / TILE as usize);
    (u64::MAX >> (63 - (b - a))) << a
}

/// The runs of set bits of `bits`, as (first, one past the last).
fn runs_of(mut bits: u64) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    while bits != 0 {
        let a = bits.trailing_zeros() as usize;
        let n = (bits >> a).trailing_ones() as usize;
        out.push((a, a + n));
        bits &= if a + n >= 64 { 0 } else { u64::MAX << (a + n) };
    }
    out
}

/// The tiles of the 4 MiB of addresses from `cell` on in which this process
/// has committed pages it may read. Views the runtime keeps in its books
/// alone are committed memory as far as the title goes: with
/// `VICTORY_SOFT_VIEWS` they count as they are (their bytes are read from
/// their pages, `crate::host`'s reads), with `VICTORY_LAZY_VIEWS` they are
/// made mappings of the host's before Windows is asked.
fn committed_tiles(cell: u64) -> u64 {
    let (mut at, end) = (cell as usize, (cell + WINDOW) as usize);
    let mut bits = 0;
    if crate::watch::soft().is_some() {
        for (from, n) in crate::watch::soft_runs(at, end - at) {
            bits |= tiles_of(from - at, from + n - at);
        }
    } else {
        crate::watch::materialize(at, end - at);
    }
    while at < end {
        // SAFETY: plain data filled in by the query.
        let mut info: mem::MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        if unsafe { mem::VirtualQuery(at as *const _, &mut info, size_of_val(&info)) } == 0 {
            break;
        }
        let next = (info.BaseAddress as usize).saturating_add(info.RegionSize).max(at + 1);
        if info.State == mem::MEM_COMMIT && info.Protect & mem::PAGE_NOACCESS == 0 && info.Protect != 0 {
            bits |= tiles_of(at - cell as usize, next.min(end) - cell as usize);
        }
        at = next;
    }
    bits
}

/// For a window that is a host buffer of its own (`super::window_runs`): the
/// bytes from `cell` to the end of the run of committed memory that holds
/// `address`; `None` where `address` is not committed.
pub(super) fn run_bytes(cell: u64, address: u64) -> Option<u64> {
    run_end_of(committed_tiles(cell), ((address - cell) / TILE) as usize).map(|end| end as u64 * TILE)
}

/// Whether [`make`] may make a cell now: not while the last one found no
/// room, and never once the probe has failed.
pub(super) fn may_make() -> bool {
    crate::host::now_ms() >= NO_ROOM_UNTIL.load(Ordering::Relaxed) && !matches!(RIG.get(), Some(None))
}

/// The end of the run of set tiles of `bits` that holds tile `tile`, as a
/// tile; `None` if that tile is not set.
fn run_end_of(bits: u64, tile: usize) -> Option<usize> {
    (bits >> tile & 1 != 0).then(|| tile + (bits >> tile).trailing_ones() as usize)
}

impl Cell {
    pub(super) fn new(fresh: bool) -> Cell {
        Cell {
            committed: AtomicU64::new(0),
            wanted: AtomicU64::new(0),
            mapped: AtomicU64::new(0),
            fresh: AtomicBool::new(fresh),
            tiles: Mutex::new([Tile::default(); TILES]),
            title: [const { AtomicU64::new(0) }; 2],
        }
    }

    /// Where the run of committed title memory that holds `address` ends, cut
    /// to the cell from `cell` on: what an ask without a length at `address`
    /// can read. The cell's end where `address` itself is not committed
    /// (memory of a kind the question does not know: as before).
    ///
    /// What is committed is asked of the system at most every 250 ms for a
    /// cell, and at once when `address` is not in what was last answered: a
    /// run the title made longer is seen that much later, and reads past its
    /// old end are zeros until then.
    pub(super) fn run_end(&self, cell: u64, address: u64) -> u64 {
        const EVERY_MS: u64 = 250;
        let tile = ((address - cell) / TILE) as usize;
        let now = crate::host::now_ms().max(1);
        let asked = self.title[1].load(Ordering::Relaxed);
        let mut bits = self.title[0].load(Ordering::Relaxed);
        if asked == 0 || now >= asked + EVERY_MS || bits >> tile & 1 == 0 {
            bits = committed_tiles(cell);
            self.title[0].store(bits, Ordering::Relaxed);
            self.title[1].store(now, Ordering::Relaxed);
        }
        run_end_of(bits, tile).map_or(cell + WINDOW, |end| cell + end as u64 * TILE)
    }

    /// The cell's copy goes: its tiles go back to the pool and the CPU side's
    /// addresses to the system. (The pool's heaps stay.)
    pub(super) fn gone(self, data: *mut u8) {
        let tiles = self.tiles.into_inner().unwrap_or_else(|e| e.into_inner());
        let mapped = self.mapped.into_inner();
        if mapped != 0 {
            let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
            for t in (0..TILES).filter(|t| mapped >> t & 1 != 0) {
                pool.give_back(tiles[t]);
            }
            note_given(&pool);
        }
        // SAFETY: the reservation `make` made, with its copy gone.
        unsafe { mem::VirtualFree(data.cast(), 0, mem::MEM_RELEASE) };
    }
}

impl Shadow {
    /// Of a sparse window: bytes at offsets `[from, to)` of the copy are
    /// about to be asked for, read or written. The CPU side's tiles there are
    /// committed, and the buffer's are given memory before the next
    /// submission's lists run. Nothing for any other copy.
    pub(super) fn sparse_ensure(&self, from: usize, to: usize) {
        let Some(cell) = &self.sparse else {
            return;
        };
        let bits = tiles_of(from, to);
        if cell.committed.load(Ordering::Acquire) & bits != bits {
            let _held = cell.tiles.lock().unwrap_or_else(|e| e.into_inner());
            let missing = bits & !cell.committed.load(Ordering::Acquire);
            for (a, b) in runs_of(missing) {
                // SAFETY: inside the cell's reservation, which lives as long
                // as the copy.
                let commit = || unsafe {
                    let (at, bytes) = (self.data.add(a * TILE as usize), (b - a) * TILE as usize);
                    mem::VirtualAlloc(at.cast(), bytes, mem::MEM_COMMIT, mem::PAGE_READWRITE)
                };
                // (The system may be growing its page file, and the governor
                // letting go of what is idle once a second at red,
                // `host::budget`: tried for about 2 s before the run ends.
                // No trim from here: it may take this cell's lock.)
                let made = (0..40).any(|tried| {
                    if tried > 0 {
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    !commit().is_null()
                });
                if !made {
                    crate::host::fail(format_args!(
                        "no memory for {} KiB of the CPU's side of a sparse window of title memory at {:#x}",
                        (b - a) * 64,
                        self.start
                    ));
                }
            }
            COMMITTED.fetch_add(u64::from(missing.count_ones()), Ordering::Relaxed);
            cell.committed.fetch_or(missing, Ordering::Release);
        }
        if cell.wanted.load(Ordering::Relaxed) & bits != bits {
            cell.wanted.fetch_or(bits, Ordering::Relaxed);
            PENDING.store(true, Ordering::Release);
        }
    }

    /// Of a sparse window: an ask for `[address, end)`, or from `address` on
    /// without a length (`None`: to the end of the run of committed title
    /// memory, [`Cell::run_end`]).
    pub(super) fn sparse_ask(&self, address: u64, end: Option<u64>) {
        let Some(cell) = &self.sparse else {
            return;
        };
        let end = end.unwrap_or_else(|| if runs() { cell.run_end(self.start, address) } else { self.start + self.len });
        self.sparse_ensure((address - self.start) as usize, (end.max(address + 1) - self.start) as usize);
    }

    /// Of a sparse window, for [`Shadow::left`]: where what can be read from
    /// `address` on ends.
    pub(super) fn sparse_end(&self, address: u64) -> Option<u64> {
        let cell = self.sparse.as_ref()?;
        (runs() && address >= self.start && address < self.start + self.len).then(|| cell.run_end(self.start, address))
    }

    /// Whether an ask of `[address, end)` (`None`: without a length, as
    /// [`Shadow::sparse_ask`] takes it) may take this copy: any copy but a
    /// sparse window always, and a sparse window while the pool has room for
    /// the ask's tiles that have no memory yet ([`fallback`]). Asked before
    /// [`Shadow::sparse_ask`], which notes them as wanted.
    pub(super) fn sparse_fits(&self, address: u64, end: Option<u64>) -> bool {
        let Some(cell) = &self.sparse else {
            return true;
        };
        let Some(rig) = RIG.get().and_then(Option::as_ref) else {
            return true;
        };
        if !fallback() {
            return true;
        }
        let end = end.unwrap_or_else(|| if runs() { cell.run_end(self.start, address) } else { self.start + self.len });
        let bits = tiles_of((address - self.start) as usize, (end.max(address + 1) - self.start) as usize);
        let missing = bits & !cell.mapped.load(Ordering::Acquire);
        missing == 0 || tiles_fit(GIVEN.load(Ordering::Relaxed), missing.count_ones(), rig.budget)
    }

    /// Of a sparse window: `ranges` (offsets that go up to the card's side)
    /// cut to the tiles that have memory. A tile without shows nothing of
    /// its own (with a tile of zeros, what all cells share), so nothing is
    /// sent there; it is sent whole when it gets memory.
    pub(super) fn sparse_clip(&self, ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
        let Some(cell) = &self.sparse else {
            return ranges;
        };
        let mapped = cell.mapped.load(Ordering::Acquire);
        if mapped == u64::MAX {
            return ranges;
        }
        let tile = TILE as usize;
        let mut out = Vec::with_capacity(ranges.len());
        for (from, to) in ranges {
            for (a, b) in runs_of(tiles_of(from, to) & mapped) {
                out.push((from.max(a * tile), to.min(b * tile)));
            }
        }
        out
    }

    /// Whether offsets `[from, to)` of the CPU's side can be read: always,
    /// but for tiles of a sparse window that are not committed.
    pub(super) fn sparse_held(&self, from: usize, to: usize) -> bool {
        self.sparse.as_ref().is_none_or(|cell| {
            let bits = tiles_of(from, to);
            cell.committed.load(Ordering::Acquire) & bits == bits
        })
    }

    /// Takes a sparse window's tiles and the CPU side's pages away: what it
    /// knows of its bytes is forgotten with them. The caller has made sure
    /// that nothing holds or is handed the copy meanwhile.
    fn sparse_evict(&self, queue: &ID3D12CommandQueue, pool: &mut Pool, zero: Option<Tile>) -> u32 {
        let Some(cell) = &self.sparse else {
            return 0;
        };
        let tiles = cell.tiles.lock().unwrap_or_else(|e| e.into_inner());
        let mapped = cell.mapped.swap(0, Ordering::AcqRel);
        if mapped != 0 {
            match zero {
                Some(zero) => pool.map_all_to(queue, &self.buffer, 0, TILES as u32, zero),
                None => tilepool::unmap(queue, &self.buffer, 0, TILES as u32),
            }
            for t in (0..TILES).filter(|t| mapped >> t & 1 != 0) {
                pool.give_back(tiles[t]);
            }
        }
        cell.wanted.store(0, Ordering::Relaxed);
        // Every page counts as never copied, and nothing waits to go up.
        self.copied.lock().unwrap_or_else(|e| e.into_inner()).fill(0);
        *self.ranged.lock().unwrap_or_else(|e| e.into_inner()) = Default::default();
        self.forget_held(self.start as usize, (self.start + self.len) as usize);
        self.landings.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.landing.store(false, Ordering::Relaxed);
        if let Some(card) = &self.card {
            let mut dirty = card.dirty.lock().unwrap_or_else(|e| e.into_inner());
            dirty.clear();
            *card.sent.lock().unwrap_or_else(|e| e.into_inner()) = Default::default();
            card.touched.store(false, Ordering::Release);
            // The hashes of its pieces are of bytes that are gone: made anew,
            // whole, when shader writes are declared again.
            if let Some(before) = card.before.lock().unwrap_or_else(|e| e.into_inner()).take() {
                CARD_BYTES[1].fetch_sub(card_hash_bytes(card.size), Ordering::Relaxed);
                crate::host::retire_resource(before);
            }
        }
        let committed = cell.committed.swap(0, Ordering::AcqRel);
        if committed != 0 {
            // SAFETY: the cell's reservation; nothing holds the copy, so
            // nothing reads or writes its CPU side.
            unsafe { mem::VirtualFree(self.data.cast(), WINDOW as usize, mem::MEM_DECOMMIT) };
            COMMITTED.fetch_sub(u64::from(committed.count_ones()), Ordering::Relaxed);
        }
        mapped.count_ones()
    }
}

/// Tiles of CPU sides committed now.
static COMMITTED: AtomicU64 = AtomicU64::new(0);

/// The sparse window of the 4 MiB cell of title addresses from `cell` on,
/// made if there is none and the card has room; `map` is the table of
/// windows, held by the caller. `None`: the ask gets a window as before.
pub(super) fn make(map: &mut std::collections::BTreeMap<(u64, u64), Arc<Shadow>>, cell: u64) -> Option<Arc<Shadow>> {
    let now = crate::host::now_ms();
    if now < NO_ROOM_UNTIL.load(Ordering::Relaxed) {
        return None;
    }
    let rig = rig()?;
    // Room: under the tiles' budget (less what is left to the cells there,
    // `cell_fits`), and the card itself under the line a card copy is made up
    // to (`card_room_for`), with a heap more on it, as the budget governor
    // places what may go either way (by the line alone: a window instead of
    // the cell costs the PC no less, so a relief of the card is no reason).
    let given = POOL.lock().unwrap_or_else(|e| e.into_inner()).given_bytes();
    let full = !cell_fits(given, rig.budget, fallback());
    let card_full = !crate::host::budget::card_has_room(crate::host::host().and_then(|h| h.video_memory()), 16 << 20, 97);
    if full || card_full {
        NO_ROOM_UNTIL.store(now + 1000, Ordering::Relaxed);
        STATS[4].fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let buffer = match tilepool::reserved_buffer(&rig.device, WINDOW) {
        Ok(buffer) => buffer,
        Err(e) => {
            tracing::warn!(error = %e, "no reserved buffer for a sparse window");
            NO_ROOM_UNTIL.store(now + 1000, Ordering::Relaxed);
            return None;
        }
    };
    // SAFETY: a plain reservation of addresses.
    let data = unsafe { mem::VirtualAlloc(std::ptr::null(), WINDOW as usize, mem::MEM_RESERVE, mem::PAGE_READWRITE) };
    if data.is_null() {
        return None;
    }
    crate::host::name(&buffer, || format!("title memory {cell:#x}+{WINDOW:#x}, sparse"));
    // SAFETY: a live buffer.
    let gpu = unsafe { buffer.GetGPUVirtualAddress() };
    let mut shadow = Shadow::new(buffer, data.cast(), gpu, cell, WINDOW);
    shadow.window = true;
    shadow.card = Some(Card::new(None, WINDOW));
    shadow.sparse = Some(Cell::new(rig.zero.is_some()));
    let shadow = Arc::new(shadow);
    CARDED.lock().unwrap_or_else(|e| e.into_inner()).push(Arc::downgrade(&shadow));
    CELLS.lock().unwrap_or_else(|e| e.into_inner()).push(Arc::downgrade(&shadow));
    if rig.zero.is_some() {
        PENDING.store(true, Ordering::Release);
    }
    map.insert((cell, WINDOW), shadow.clone());
    Some(shadow)
}

/// The cells alive, each held.
fn alive() -> Vec<Arc<Shadow>> {
    let mut cells = CELLS.lock().unwrap_or_else(|e| e.into_inner());
    let mut out = Vec::with_capacity(cells.len());
    cells.retain(|weak| weak.upgrade().map(|shadow| out.push(shadow)).is_some());
    out
}

/// One submission at a time.
static TURN: Mutex<[u64; 2]> = Mutex::new([0; 2]);

/// At the top of `Host::card_before`, on the submitting thread, before
/// anything of the submission is on `queue`: cells idle for long lose their
/// tiles, and the tiles asked for since the last submission get memory.
pub(super) fn before(queue: &ID3D12CommandQueue) {
    if probe_asked() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let mut device: Option<ID3D12Device> = None;
            // SAFETY: a live queue; the out pointer is ours.
            if unsafe { queue.GetDevice(&mut device) }.is_ok()
                && let Some(device) = device
            {
                probe(&device).tell();
            }
        });
    }
    let Ok(mut turn) = TURN.try_lock() else {
        return;
    };
    let now = crate::host::now_ms();
    if now >= turn[1] + 10_000 {
        turn[1] = now;
        line();
    }
    if !on() {
        return;
    }
    let Some(rig) = RIG.get().and_then(Option::as_ref) else {
        return;
    };
    let starved = STARVED.swap(false, Ordering::AcqRel);
    if now >= turn[0] + 250 {
        turn[0] = now;
        // With no room for a wanted tile, or the card being relieved
        // ([`hurry`]): sooner.
        let hurried = HURRY.swap(false, Ordering::AcqRel);
        evict(queue, rig, if starved || hurried { idle_ms().min(500) } else { idle_ms() });
    }
    if PENDING.swap(false, Ordering::AcqRel) || starved {
        give(queue, rig);
    }
}

/// Gives the wanted tiles their memory, and sends them up whole.
fn give(queue: &ID3D12CommandQueue, rig: &Rig) {
    let cells = alive();
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    let mut starved = false;
    for shadow in &cells {
        let Some(cell) = &shadow.sparse else {
            continue;
        };
        if let Some(zero) = rig.zero
            && cell.fresh.swap(false, Ordering::AcqRel)
        {
            pool.map_all_to(queue, &shadow.buffer, 0, TILES as u32, zero);
        }
        let need = cell.wanted.load(Ordering::Acquire) & !cell.mapped.load(Ordering::Acquire);
        if need == 0 {
            continue;
        }
        let mut tiles = cell.tiles.lock().unwrap_or_else(|e| e.into_inner());
        let mut pairs = Vec::new();
        let mut got = 0u64;
        for t in (0..TILES).filter(|t| need >> t & 1 != 0) {
            if pool.given_bytes() + TILE > rig.budget {
                break;
            }
            let Some(tile) = pool.take(&rig.device) else {
                break;
            };
            tiles[t] = tile;
            pairs.push((t as u32, tile));
            got |= 1 << t;
        }
        drop(tiles);
        if got != 0 {
            pool.map(queue, &shadow.buffer, &pairs);
            cell.mapped.fetch_or(got, Ordering::AcqRel);
            STATS[0].fetch_add(pairs.len() as u64, Ordering::Relaxed);
            // A tile holds what its last owner left: the CPU side's bytes of
            // it go up whole with this submission.
            let committed = cell.committed.load(Ordering::Acquire);
            for (a, b) in runs_of(got & committed) {
                shadow.resend(a * TILE as usize, b * TILE as usize);
            }
        }
        if got != need {
            starved = true;
            STATS[3].fetch_add(u64::from((need & !got).count_ones()), Ordering::Relaxed);
        }
    }
    note_given(&pool);
    if starved {
        // Asked for again at the next submission, after a sooner trim.
        STARVED.store(true, Ordering::Release);
    }
}

/// Takes the tiles and the CPU side's pages of the cells nothing asked for
/// in `idle` ms (and four frames), that only the table of windows holds and
/// no shader may be writing.
fn evict(queue: &ID3D12CommandQueue, rig: &Rig, idle: u64) {
    const IDLE_FRAMES: u64 = 4;
    let cells = alive();
    let heaps = {
        // A window is handed out under this lock: none is meanwhile.
        let _table = super::LOOSE.write().unwrap_or_else(|e| e.into_inner());
        // (And none that a thread remembers is taken without the lock from
        // here on: `super::loose_found`.)
        super::loose_moved();
        let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
        for shadow in &cells {
            let Some(cell) = &shadow.sparse else {
                continue;
            };
            let has = cell.mapped.load(Ordering::Acquire) | cell.committed.load(Ordering::Acquire);
            // (Held by the table and by `cells`.)
            if has == 0
                || Arc::strong_count(shadow) > 2
                || !crate::host::long_ago(shadow.used.load(Ordering::Relaxed), idle, IDLE_FRAMES)
                || !shadow.pending.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
            {
                continue;
            }
            let tiles = shadow.sparse_evict(queue, &mut pool, rig.zero);
            STATS[1].fetch_add(1, Ordering::Relaxed);
            STATS[2].fetch_add(u64::from(tiles), Ordering::Relaxed);
        }
        note_given(&pool);
        pool.empty_heaps(1)
    };
    // Heaps with no tile given go back to the card when nothing recorded
    // while they had tiles mapped runs any more.
    for heap in heaps {
        crate::host::retire(heap);
    }
}

/// Every ten seconds: the CPU sides in pageable memory, and the sparse
/// windows.
fn line() {
    let [copies, bytes] = super::plain_stats();
    if copies > 0 {
        tracing::info!(
            "CPU sides of copies in pageable memory (VICTORY_CPU_SIDES=plain): {copies} copies, {} MB that the PC no longer shares \
             with the card",
            bytes >> 20
        );
    }
    if !on() {
        return;
    }
    let cells = CELLS.lock().unwrap_or_else(|e| e.into_inner()).iter().filter(|w| w.strong_count() > 0).count();
    let (given, heaps) = {
        let pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
        (pool.given_bytes(), pool.heap_bytes())
    };
    let s: [u64; 6] = std::array::from_fn(|i| STATS[i].swap(0, Ordering::Relaxed));
    tracing::info!(
        "sparse windows: {cells} cells ({} MB of title addresses), {} MB of the card in tiles of at most {} MB (heaps {} MB), {} MB of \
         CPU sides committed; since the line before: {} tiles given, {} idle cells lost {} tiles, {} wanted tiles left without \
         memory (no room), {} cells not made for want of room (windows as before), {} asks given a window as before for want \
         of room (VICTORY_SPARSE_FALLBACK)",
        (cells as u64 * WINDOW) >> 20,
        given >> 20,
        RIG.get().and_then(Option::as_ref).map_or(0, |r| r.budget >> 20),
        heaps >> 20,
        (COMMITTED.load(Ordering::Relaxed) * TILE) >> 20,
        s[0],
        s[1],
        s[2],
        s[3],
        s[4],
        s[5]
    );
}

/// What [`probe`] found.
#[derive(Default, Debug)]
pub(crate) struct Probe {
    /// The card's tier of tiled resources.
    pub(crate) tier: i32,
    /// A reserved buffer shaders may write was made.
    pub(crate) made: bool,
    /// A tile that never had memory reads zeros through a root view.
    pub(crate) fresh_zero: bool,
    /// A tile given memory and filled by a copy reads its bytes through a
    /// root view, and the tile beside it still reads zeros.
    pub(crate) mapped_reads: bool,
    /// A shader's store through a root view of the buffer is read back.
    pub(crate) stored: bool,
    /// A tile whose memory was taken reads zeros again.
    pub(crate) evicted_zero: bool,
    /// Given another tile of the pool and filled, it reads the new bytes.
    pub(crate) remapped_reads: bool,
    /// The buffer's address on the card was the same throughout.
    pub(crate) address_stable: bool,
    /// Tiles mapped to one tile of zeros read zeros.
    pub(crate) zero_tile: bool,
    /// What went wrong, if a call failed.
    pub(crate) error: Option<String>,
}

impl Probe {
    /// Whether sparse windows can be used: every answer as expected, with
    /// tiles without memory mapped to nothing (tier 2) or to a tile of zeros.
    pub(crate) fn passed(&self) -> bool {
        self.error.is_none()
            && self.made
            && self.mapped_reads
            && self.stored
            && self.remapped_reads
            && self.address_stable
            && (self.zero_tile || (self.tier >= 2 && self.fresh_zero && self.evicted_zero))
    }

    fn tell(&self) {
        tracing::info!(
            "sparse probe: tiled resources tier {}; a reserved buffer shaders may write {}; a tile that never had memory reads zeros \
             through a root view: {}; a tile given memory reads its bytes: {}; a shader's store through a root view is read back: \
             {}; a tile whose memory was taken reads zeros: {}; given other memory it reads the new bytes: {}; the buffer's address \
             stayed the same: {}; tiles mapped to one tile of zeros read zeros: {}{}; verdict: sparse windows {} be used on this card",
            self.tier,
            if self.made { "was made" } else { "was NOT made" },
            self.fresh_zero,
            self.mapped_reads,
            self.stored,
            self.evicted_zero,
            self.remapped_reads,
            self.address_stable,
            self.zero_tile,
            self.error.as_ref().map_or(String::new(), |e| format!("; a call failed: {e}")),
            if self.passed() { "can" } else { "can NOT" }
        );
    }
}

const READ: &str = r#"
#define RS "RootConstants(num32BitConstants=2, b0), SRV(t0), UAV(u0)"
cbuffer Part : register(b0) { uint at; uint value; };
ByteAddressBuffer window : register(t0);
RWByteAddressBuffer found : register(u0);
[RootSignature(RS)]
[numthreads(1, 1, 1)]
void main() { found.Store(0, window.Load(at)); }
"#;

const STORE: &str = r#"
#define RS "RootConstants(num32BitConstants=2, b0), UAV(u0)"
cbuffer Part : register(b0) { uint at; uint value; };
RWByteAddressBuffer window : register(u0);
[RootSignature(RS)]
[numthreads(1, 1, 1)]
void main() { window.Store(at, value); }
"#;

/// A queue, a list and two programs of the probe's own.
struct Bench {
    device: ID3D12Device,
    queue: ID3D12CommandQueue,
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    fence: ID3D12Fence,
    done: u64,
    read: (ID3D12RootSignature, ID3D12PipelineState),
    store: (ID3D12RootSignature, ID3D12PipelineState),
    /// What a read found (on the card), where it is fetched to, and the
    /// bytes a tile is filled from, mapped.
    found: ID3D12Resource,
    fetched: (ID3D12Resource, *const u32),
    fill: (ID3D12Resource, *mut u32),
}

impl Bench {
    fn program(device: &ID3D12Device, source: &str) -> windows::core::Result<(ID3D12RootSignature, ID3D12PipelineState)> {
        use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
        let mut code = None;
        // SAFETY: the source and names live through the call; the out
        // pointer is ours; the code holds its root signature.
        unsafe {
            D3DCompile(
                source.as_ptr().cast(),
                source.len(),
                windows::core::s!("sparse probe"),
                None,
                None,
                windows::core::s!("main"),
                windows::core::s!("cs_5_1"),
                0,
                0,
                &mut code,
                None,
            )?;
            let code = code.ok_or_else(windows::core::Error::empty)?;
            let bytes = std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize());
            let root: ID3D12RootSignature = device.CreateRootSignature(0, bytes)?;
            let pso: ID3D12PipelineState = device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: bytes.as_ptr().cast(),
                    BytecodeLength: bytes.len(),
                },
                ..Default::default()
            })?;
            Ok((root, pso))
        }
    }

    fn buffer(
        device: &ID3D12Device,
        kind: D3D12_HEAP_TYPE,
        size: u64,
        flags: D3D12_RESOURCE_FLAGS,
        state: D3D12_RESOURCE_STATES,
    ) -> windows::core::Result<ID3D12Resource> {
        let heap = D3D12_HEAP_PROPERTIES { Type: kind, ..Default::default() };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: flags,
            ..Default::default()
        };
        let mut made: Option<ID3D12Resource> = None;
        // SAFETY: plain creation.
        unsafe { device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut made)? };
        made.ok_or_else(windows::core::Error::empty)
    }

    fn new(device: &ID3D12Device) -> windows::core::Result<Bench> {
        // SAFETY: plain creation of objects of our own, and mappings of
        // buffers the CPU sees.
        unsafe {
            let direct = D3D12_COMMAND_LIST_TYPE_DIRECT;
            let queue: ID3D12CommandQueue =
                device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: direct, ..Default::default() })?;
            let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(direct)?;
            let list: ID3D12GraphicsCommandList = device.CreateCommandList(0, direct, &allocator, None)?;
            list.Close()?;
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
            let none = D3D12_RESOURCE_FLAG_NONE;
            let written = D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS;
            let found = Self::buffer(device, D3D12_HEAP_TYPE_DEFAULT, 256, written, D3D12_RESOURCE_STATE_COMMON)?;
            let fetched = Self::buffer(device, D3D12_HEAP_TYPE_READBACK, 256, none, D3D12_RESOURCE_STATE_COPY_DEST)?;
            let fill = Self::buffer(device, D3D12_HEAP_TYPE_UPLOAD, TILE, none, D3D12_RESOURCE_STATE_GENERIC_READ)?;
            let (mut from, mut to) = (std::ptr::null_mut(), std::ptr::null_mut());
            fetched.Map(0, None, Some(&mut from))?;
            fill.Map(0, None, Some(&mut to))?;
            Ok(Bench {
                device: device.clone(),
                queue,
                allocator,
                list,
                fence,
                done: 0,
                read: Self::program(device, READ)?,
                store: Self::program(device, STORE)?,
                found,
                fetched: (fetched, from.cast_const().cast()),
                fill: (fill, to.cast()),
            })
        }
    }

    /// Records with `record`, runs it and waits.
    fn run(&mut self, record: impl FnOnce(&Bench, &ID3D12GraphicsCommandList)) -> windows::core::Result<()> {
        // SAFETY: our own list and queue, one run at a time; a wait with no
        // event returns when the fence has the value.
        unsafe {
            self.allocator.Reset()?;
            self.list.Reset(&self.allocator, None)?;
            record(self, &self.list);
            self.list.Close()?;
            self.queue.ExecuteCommandLists(&[Some(self.list.cast()?)]);
            self.done += 1;
            self.queue.Signal(&self.fence, self.done)?;
            self.fence.SetEventOnCompletion(self.done, windows::Win32::Foundation::HANDLE::default())?;
            self.device.GetDeviceRemovedReason()
        }
    }

    /// The word at offset `at` of `window`, as a shader reads it through a
    /// root view.
    fn word(&mut self, window: &ID3D12Resource, at: u32) -> windows::core::Result<u32> {
        // SAFETY: an open list of ours; live buffers, in the common state
        // between runs (buffers go back to it by themselves).
        self.run(|b, list| unsafe {
            list.SetComputeRootSignature(&b.read.0);
            list.SetPipelineState(&b.read.1);
            list.SetComputeRoot32BitConstants(0, 2, [at, 0].as_ptr().cast(), 0);
            list.SetComputeRootShaderResourceView(1, window.GetGPUVirtualAddress());
            list.SetComputeRootUnorderedAccessView(2, b.found.GetGPUVirtualAddress());
            list.Dispatch(1, 1, 1);
        })?;
        // SAFETY: as above.
        self.run(|b, list| unsafe { list.CopyBufferRegion(&b.fetched.0, 0, &b.found, 0, 4) })?;
        // SAFETY: the mapping, which holds 256 bytes; the copy has run.
        Ok(unsafe { self.fetched.1.read_volatile() })
    }

    /// A shader stores `value` at offset `at` of `window` through a root
    /// view.
    fn store(&mut self, window: &ID3D12Resource, at: u32, value: u32) -> windows::core::Result<()> {
        // SAFETY: as `word`.
        self.run(|b, list| unsafe {
            list.SetComputeRootSignature(&b.store.0);
            list.SetPipelineState(&b.store.1);
            list.SetComputeRoot32BitConstants(0, 2, [at, value].as_ptr().cast(), 0);
            list.SetComputeRootUnorderedAccessView(1, window.GetGPUVirtualAddress());
            list.Dispatch(1, 1, 1);
        })
    }

    /// Fills tile `tile` of `window` by a copy: word `i` of it is `seed + i`.
    fn fill(&mut self, window: &ID3D12Resource, tile: u32, seed: u32) -> windows::core::Result<()> {
        for i in 0..(TILE / 4) as usize {
            // SAFETY: the mapping of a tile's bytes.
            unsafe { self.fill.1.add(i).write(seed.wrapping_add(i as u32)) };
        }
        // SAFETY: as `word`.
        self.run(|b, list| unsafe { list.CopyBufferRegion(window, u64::from(tile) * TILE, &b.fill.0, 0, TILE) })
    }
}

/// Tries what sparse windows do on a reserved buffer, a queue and a pool of
/// the probe's own on `device`: nothing of the title's or the run's is
/// touched. Some 300 KiB of the card for a moment.
pub(crate) fn probe(device: &ID3D12Device) -> Probe {
    let mut p = Probe {
        tier: tilepool::tier(device),
        ..Default::default()
    };
    if p.tier < 1 {
        return p;
    }
    let mut pool = Pool::new();
    let tried = (|| -> windows::core::Result<()> {
        let mut bench = Bench::new(device)?;
        let window = tilepool::reserved_buffer(device, WINDOW)?;
        p.made = true;
        // SAFETY: a live buffer.
        let address = unsafe { window.GetGPUVirtualAddress() };
        let tile = TILE as u32;
        if p.tier >= 2 {
            p.fresh_zero = bench.word(&window, 3 * tile + 20)? == 0;
        }
        let empty = || windows::core::Error::empty();
        // The first tile of the new heap, which nothing here writes: zeros.
        let zero = pool.take(device).ok_or_else(empty)?;
        let first = pool.take(device).ok_or_else(empty)?;
        pool.map(&bench.queue, &window, &[(3, first)]);
        bench.fill(&window, 3, 0xa5a5_0000)?;
        p.mapped_reads = bench.word(&window, 3 * tile + 20)? == 0xa5a5_0005
            && bench.word(&window, 4 * tile - 4)? == 0xa5a5_0000 + tile / 4 - 1
            && (p.tier < 2 || bench.word(&window, 4 * tile)? == 0);
        bench.store(&window, 3 * tile + 64, 0xdead_beef)?;
        p.stored = bench.word(&window, 3 * tile + 64)? == 0xdead_beef && bench.word(&window, 3 * tile + 68)? == 0xa5a5_0011;
        // Its memory is taken, as an idle cell's is.
        tilepool::unmap(&bench.queue, &window, 0, TILES as u32);
        if p.tier >= 2 {
            p.evicted_zero = bench.word(&window, 3 * tile + 20)? == 0;
        }
        // Another tile of the pool, and what the first now holds is not seen.
        let second = pool.take(device).ok_or_else(empty)?;
        pool.give_back(first);
        pool.map(&bench.queue, &window, &[(3, second)]);
        bench.fill(&window, 3, 0x5a5a_0000)?;
        p.remapped_reads =
            bench.word(&window, 3 * tile + 20)? == 0x5a5a_0005 && bench.word(&window, 3 * tile + 64)? == 0x5a5a_0010;
        // One tile of zeros for all.
        pool.map_all_to(&bench.queue, &window, 0, TILES as u32, zero);
        p.zero_tile = bench.word(&window, 3 * tile + 20)? == 0 && bench.word(&window, 40 * tile + 8)? == 0;
        // SAFETY: a live buffer.
        p.address_stable = unsafe { window.GetGPUVirtualAddress() } == address;
        Ok(())
    })();
    if let Err(e) = tried {
        p.error = Some(e.to_string());
    }
    p
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// The software adapter's device (one for the process).
    pub(crate) fn warp() -> Option<ID3D12Device> {
        use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
        use windows::Win32::Graphics::Dxgi::*;
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

    /// The probe, whole, on the software adapter: a tile without memory reads
    /// zeros, a tile with memory reads its bytes through a root view and
    /// takes a shader's store, and the buffer's address on the card is the
    /// same after its memory was taken and given again. The card's driver is
    /// what a run with `VICTORY_SPARSE_PROBE=1` adds.
    #[test]
    fn warp_a_sparse_window_reads_zeros_without_memory_and_its_bytes_with() {
        let Some(device) = warp() else {
            return;
        };
        let p = probe(&device);
        assert!(p.error.is_none(), "{p:?}");
        assert!(p.made && p.mapped_reads && p.stored && p.remapped_reads && p.address_stable && p.zero_tile, "{p:?}");
        if p.tier >= 2 {
            assert!(p.fresh_zero && p.evicted_zero, "{p:?}");
        }
        assert!(p.passed(), "{p:?}");
    }

    /// The probe on the PC's own card (the high-performance adapter), asked for by
    /// name only: `cargo test -p victory-gpu --lib the_cards_own_sparse_probe
    /// -- --ignored --nocapture`. It opens a device of its own beside
    /// whatever runs, so never with the other tests.
    #[test]
    #[ignore = "opens the PC's own card"]
    fn the_cards_own_sparse_probe() {
        use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
        use windows::Win32::Graphics::Dxgi::*;
        let mut device: Option<ID3D12Device> = None;
        // SAFETY: plain DXGI queries and device creation on the adapter the
        // system names for high performance (the discrete card, where there are two).
        let name = unsafe {
            let Ok(factory) = CreateDXGIFactory2::<IDXGIFactory6>(DXGI_CREATE_FACTORY_FLAGS(0)) else {
                return;
            };
            let Ok(adapter) = factory.EnumAdapterByGpuPreference::<IDXGIAdapter1>(0, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE) else {
                return;
            };
            if D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).is_err() {
                return;
            }
            adapter.GetDesc1().map(|d| String::from_utf16_lossy(&d.Description)).unwrap_or_default()
        };
        let p = probe(&device.expect("a device"));
        println!("{}: {p:?}", name.trim_end_matches('\0'));
        assert!(p.passed(), "{p:?}");
    }

    #[test]
    fn tiles_of_offsets_and_their_runs() {
        let t = TILE as usize;
        assert_eq!(tiles_of(0, 1), 1);
        assert_eq!(tiles_of(t - 1, t + 1), 0b11);
        assert_eq!(tiles_of(3 * t, 5 * t), 0b11000);
        assert_eq!(tiles_of(0, WINDOW as usize), u64::MAX);
        assert_eq!(tiles_of(63 * t, usize::MAX), 1 << 63);
        assert_eq!(tiles_of(5, 5), 0);
        assert_eq!(runs_of(0), []);
        assert_eq!(runs_of(u64::MAX), [(0, 64)]);
        assert_eq!(runs_of(0b0110_1001 | 1 << 63), [(0, 1), (3, 4), (5, 7), (63, 64)]);
        assert_eq!(run_end_of(0b0111_0110, 1), Some(3));
        assert_eq!(run_end_of(0b0111_0110, 4), Some(7));
        assert_eq!(run_end_of(0b0111_0110, 3), None);
        assert_eq!(run_end_of(u64::MAX, 10), Some(64));
    }

    /// What is committed is found tile by tile, and a run ends where
    /// committed memory does.
    #[test]
    fn a_run_ends_where_committed_memory_ends() {
        let t = TILE as usize;
        // SAFETY: memory of the test's own: 8 MiB reserved, so that a whole
        // 4 MiB cell lies in it; tiles 2 to 4 and 9 of the cell committed.
        unsafe {
            let base = mem::VirtualAlloc(std::ptr::null(), 2 * WINDOW as usize, mem::MEM_RESERVE, mem::PAGE_READWRITE) as usize;
            assert!(base != 0);
            let cell = base.next_multiple_of(WINDOW as usize);
            for (a, n) in [(2, 3), (9, 1)] {
                assert!(!mem::VirtualAlloc((cell + a * t) as *const _, n * t, mem::MEM_COMMIT, mem::PAGE_READWRITE).is_null());
            }
            assert_eq!(committed_tiles(cell as u64), 0b10_0001_1100);
            let c = Cell::new(false);
            let cell = cell as u64;
            assert_eq!(c.run_end(cell, cell + 2 * TILE + 100), cell + 5 * TILE);
            assert_eq!(c.run_end(cell, cell + 9 * TILE), cell + 10 * TILE);
            // Not committed there: as before, the cell's end.
            assert_eq!(c.run_end(cell, cell + 6 * TILE), cell + WINDOW);
            mem::VirtualFree(base as *mut _, 0, mem::MEM_RELEASE);
        }
    }

    /// On by default where the CPU sides are plain, set either way by the
    /// switch, never without own sends or copies on the card.
    #[test]
    fn cells_are_the_default_where_the_sides_are_plain() {
        for set in [None, Some(""), Some("2"), Some(" ")] {
            assert!(sparse_of(set, true, true, true), "{set:?}");
            assert!(!sparse_of(set, false, true, true), "{set:?}");
        }
        assert!(sparse_of(Some("1"), false, true, true));
        assert!(sparse_of(Some(" 1 "), false, true, true));
        assert!(!sparse_of(Some("0"), true, true, true));
        for set in [None, Some("1")] {
            assert!(!sparse_of(set, true, false, true), "{set:?}");
            assert!(!sparse_of(set, true, true, false), "{set:?}");
        }
    }

    /// New cells leave the pool's last sixteenth to the cells there, which
    /// may grow into it; an ask's missing tiles fit while the pool has them.
    #[test]
    fn new_cells_leave_room_for_the_cells_there() {
        const MIB: u64 = 1 << 20;
        let budget = 2048 * MIB;
        assert!(cell_fits(0, budget, true));
        assert!(cell_fits(1920 * MIB - WINDOW, budget, true));
        assert!(!cell_fits(1920 * MIB - WINDOW + 1, budget, true));
        // As before: while one tile fits.
        assert!(cell_fits(budget - TILE, budget, false));
        assert!(!cell_fits(budget - TILE + 1, budget, false));
        assert!(tiles_fit(1920 * MIB, 2048, budget));
        assert!(!tiles_fit(1920 * MIB, 2049, budget));
        assert!(tiles_fit(budget, 0, budget) && !tiles_fit(budget, 1, budget));
        assert!(!tiles_fit(u64::MAX, 1, budget));
    }

    /// A window set like one a long session holds at its end (thousands of
    /// windows, gigabytes of the PC's memory the card shares, hundreds of
    /// them asked for without a length at 4 MiB each): 444 cells of 4 MiB
    /// whose title memory is committed from their
    /// start for 2.5 to 4 MiB, 50 asks of up to 64 KiB in each of the 324
    /// cells a root view reads and 30 in the others, on the 1 MiB grid of a
    /// PC of 24 GiB or less. What it costs as windows of before, as those
    /// with `VICTORY_WINDOW_RUNS=1`, as cells without runs and as cells with
    /// them (the default), and what of it would still be windows of before
    /// (asks across a cell's end). Cells hold the card's memory (their tiles)
    /// and as much of the PC's in their CPU sides (pageable, committed tile
    /// by tile); windows of before hold the PC's memory the card shares,
    /// resident.
    #[test]
    fn a_window_set_like_the_runs_costs_less_as_cells() {
        use std::collections::{BTreeMap, BTreeSet};
        const MIB: u64 = 1 << 20;
        let grid = MIB;
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut below = |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 8) % n
        };
        let (roots, others) = (324u64, 120u64);
        let cells: Vec<(u64, u64, bool)> =
            (0..roots + others).map(|k| (0x7000_0000_0000 + k * WINDOW, (40 + below(25)) * TILE, k < roots)).collect();
        // Windows as before, and with runs; each cell's tiles with runs and
        // without; the windows asks across a cell's end still make.
        let (mut before, mut runs) = (BTreeSet::new(), BTreeSet::new());
        let mut tiles: BTreeMap<u64, [u64; 2]> = BTreeMap::new();
        let mut across = BTreeSet::new();
        for &(cell, run, root) in &cells {
            let committed = if run >= WINDOW { u64::MAX } else { (1u64 << (run / TILE)) - 1 };
            for _ in 0..if root { 50 } else { 30 } {
                let a = cell + below(run / 16) * 16;
                let e = a + 16 + below(64 << 10) / 16 * 16;
                let key = crate::shadow::window_of(a, e, grid);
                before.insert(key);
                runs.insert(key);
                if e > cell + WINDOW {
                    across.insert(key);
                    continue;
                }
                let bits = tiles_of((a - cell) as usize, (e - cell) as usize);
                let t = tiles.entry(cell).or_default();
                t[0] |= bits;
                t[1] |= bits;
            }
            if root {
                let a = cell + below(run / 16) * 16;
                let tile = ((a - cell) / TILE) as usize;
                let end = run_end_of(committed, tile).expect("asked where committed");
                before.insert((cell, WINDOW));
                runs.insert((cell, end as u64 * TILE));
                let t = tiles.entry(cell).or_default();
                t[0] |= tiles_of(tile * TILE as usize, end * TILE as usize);
                t[1] |= tiles_of(tile * TILE as usize, WINDOW as usize);
            }
        }
        let bytes = |set: &BTreeSet<(u64, u64)>| set.iter().map(|w| w.1).sum::<u64>();
        let in_tiles = |i: usize| tiles.values().map(|t| u64::from(t[i].count_ones()) * TILE).sum::<u64>();
        let (old, old_runs, cover) = (bytes(&before), bytes(&runs), crate::shadow::cover(before.iter().copied()));
        let (cells_runs, cells_whole, left) = (in_tiles(0), in_tiles(1), bytes(&across));
        let roots_asked = before.iter().filter(|w| w.1 == WINDOW).count();
        println!(
            "as before: {} windows {} MB ({roots_asked} of 4 MiB) over {} MB of title memory; with runs {} MB; as cells: {} cells, \
             {} MB of tiles without runs, {} MB with them, and {} windows as before ({} MB) for {} asks across a cell's end",
            before.len(),
            old / MIB,
            cover / MIB,
            old_runs / MIB,
            tiles.len(),
            cells_whole / MIB,
            cells_runs / MIB,
            across.len(),
            left / MIB,
            across.len()
        );
        // Like the run's set.
        assert!((2_000..=2_700).contains(&before.len()), "{}", before.len());
        assert!((3_300 * MIB..=4_200 * MIB).contains(&old), "{}", old / MIB);
        assert!((1_600 * MIB..=2_000 * MIB).contains(&cover), "{}", cover / MIB);
        assert_eq!(roots_asked, 324);
        // Runs alone cut the root views; cells keep each page once, and with
        // runs only what can be read.
        assert!(old_runs < old && cells_whole < cover && cells_runs < cells_whole);
        assert!(cells_runs * 3 < old, "{} of {} MB", cells_runs / MIB, old / MIB);
        // What asks across a cell's end still hold of the PC's shared memory.
        assert!(left < 32 * MIB, "{} MB", left / MIB);
    }

    #[test]
    fn the_budget_is_what_is_set_or_what_the_card_has_free() {
        const MIB: u64 = 1 << 20;
        assert_eq!(budget_of(Some(512), Some((7000 * MIB, 6900 * MIB))), 512 * MIB);
        assert_eq!(budget_of(None, Some((7000 * MIB, 3000 * MIB))), 2048 * MIB);
        assert_eq!(budget_of(None, Some((7000 * MIB, 6000 * MIB))), 300 * MIB);
        assert_eq!(budget_of(None, Some((7000 * MIB, 6900 * MIB))), 0);
        assert_eq!(budget_of(None, None), 2048 * MIB);
    }

    /// The CPU side of a cell is committed tile by tile as it is asked for,
    /// what goes up is cut to the tiles that have memory, and a cell that
    /// loses its pages knows nothing of its bytes any more.
    #[test]
    fn a_cells_cpu_side_is_committed_as_asked() {
        let t = TILE as usize;
        // SAFETY: a reservation of the test's own, never released (the copy
        // is never let go: `Shadow::unbacked`).
        let data = unsafe { mem::VirtualAlloc(std::ptr::null(), WINDOW as usize, mem::MEM_RESERVE, mem::PAGE_READWRITE) };
        assert!(!data.is_null());
        let start = 0x7_4000_0000u64;
        let shadow = Shadow::unbacked_sparse(start, data.cast());
        let cell = shadow.sparse.as_ref().unwrap();
        assert!(!shadow.sparse_held(0, 1));
        shadow.sparse_ensure(3 * t + 5, 5 * t + 1);
        assert_eq!(cell.committed.load(Ordering::Relaxed), 0b11_1000);
        assert_eq!(cell.wanted.load(Ordering::Relaxed), 0b11_1000);
        assert!(shadow.sparse_held(3 * t, 6 * t) && !shadow.sparse_held(2 * t, 4 * t));
        // SAFETY: committed just now.
        unsafe { data.cast::<u8>().add(4 * t).write(7) };
        // No tile has memory on the card yet: nothing goes up.
        assert!(shadow.sparse_clip(vec![(3 * t, 4 * t)]).is_empty());
        cell.mapped.store(0b1_1000, Ordering::Relaxed);
        assert_eq!(shadow.sparse_clip(vec![(2 * t + 9, 5 * t + 9), (40 * t, 41 * t)]), [(3 * t, 5 * t)]);
        shadow.sparse_ask(start + 7 * TILE + 8, Some(start + 7 * TILE + 16));
        assert_eq!(cell.wanted.load(Ordering::Relaxed), 0b1011_1000);
    }
}
