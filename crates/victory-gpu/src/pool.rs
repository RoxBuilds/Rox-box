//! Host memory for the textures the title fills, and for their staging
//! buffers, cut from blocks made once.
//!
//! Each such texture had an allocation of its own and its staging buffer
//! another. On an RDNA 3 an allocation takes about 310 microseconds
//! whatever its size, on one thread (`texture::allocation_costs`), and a
//! texture cut from a block 6: the pass that makes the textures shaders
//! pick by index (`draw.rs` `heap_textures`) got through a few hundred of
//! the tens of thousands queued when a scene loads in each of its 250 ms
//! turns, most of a minute of a scene without its textures.
//! `VICTORY_TEXTURE_POOL=0` makes every one an allocation of its own again.
//!
//! A piece given back is used again only two seconds later (nothing the card
//! was told before that is still running: Windows resets a card that takes
//! that long over one submission), and by then no list names it (the owners' rule, as for allocations of their own): a
//! texture's piece comes back when the last reference to the host texture
//! goes (an object of ours the texture holds says so), a staging buffer's
//! when the last list that recorded its copy is reset.

use std::collections::{BTreeMap, VecDeque};
use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::{GUID, HRESULT, IUnknown, Interface};

/// Bytes of a block of staging buffers, and of textures unless
/// [`texture_block`] says otherwise; pieces of more than half of one are not
/// pooled (with blocks made ahead, [`ahead`]: of more than one).
const BLOCK: u64 = 64 << 20;
/// Blocks with nothing in them that are kept.
const SPARE: usize = 2;
/// The same while the card has room ([`room`]): 4 GiB of them (1 GiB of
/// blocks of 16 MiB).
const SPARE_KEPT: usize = 64;
/// The same of blocks of staging buffers, which are the PC's memory (kept
/// without a limit they came to gigabytes).
const STAGING_KEPT: usize = 8;
/// How many are kept now.
static SPARES: AtomicUsize = AtomicUsize::new(SPARE);

/// Sets how many empty blocks stay, by the card's memory (budget and use):
/// [`SPARE_KEPT`] below 85 % ([`crate::host::keep_freed`]), else [`SPARE`].
/// Asked once a second; with [`trim`], the blocks of textures empty for
/// long enough past those are let go now.
pub(crate) fn room(memory: Option<[u64; 4]>) {
    SPARES.store(spares(memory, crate::host::keep_freed()), Ordering::Relaxed);
    if trim() {
        lock(&TEXTURES).rested(now());
    }
}

fn spares(memory: Option<[u64; 4]>, keep: bool) -> usize {
    match memory {
        Some([budget, used, ..]) if keep && u128::from(used) * 100 < u128::from(budget) * 85 => SPARE_KEPT,
        _ => SPARE,
    }
}

/// Whether the blocks of textures are trimmed (`VICTORY_POOL_TRIM`; on
/// unless `0`, which keeps 64 MiB blocks, let go at once past the spare ones,
/// as before): on a card of 8 GiB or less they are 16 MiB ([`texture_block`]),
/// and one with nothing in it is let go only once it has been empty for
/// [`TRIM_MS`], past the spare ones ([`room`]: two while the card's use is
/// over 85 % of its budget), at a look once a second as well as when a
/// texture is made. On a card of 8 GB nearly a gigabyte of the blocks was
/// free inside blocks of 64 MiB that held a texture or more, with the card
/// at 95 % of its budget.
fn trim() -> bool {
    crate::toggled!("VICTORY_POOL_TRIM")
}

/// How long a block of textures has had nothing in it before it is let go
/// ([`trim`]), in milliseconds: its last piece came back [`REST`] after its
/// texture went already, so nothing the card was told before is running.
const TRIM_MS: u64 = 2000;

/// The bytes of a block of textures, decided with the first one: a setting
/// of `VICTORY_TEXTURE_BLOCK_MB` (a power of two from 4 to 256) wins; else 16
/// MiB on a card whose budget is 8 GiB or less, with [`trim`] on; else 64
/// MiB, as before.
fn texture_block_for(set: Option<u64>, budget: Option<u64>, trim: bool) -> u64 {
    match set {
        Some(mb) if mb.is_power_of_two() && (4..=256).contains(&mb) => mb << 20,
        _ if trim && budget.is_some_and(|b| b > 0 && b <= 8 << 30) => 16 << 20,
        _ => BLOCK,
    }
}

/// [`texture_block_for`] for this card, asked once.
fn texture_block() -> u64 {
    static BYTES: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *BYTES.get_or_init(|| {
        let set = std::env::var("VICTORY_TEXTURE_BLOCK_MB").ok().and_then(|v| v.trim().parse::<u64>().ok());
        let budget = crate::host::host().and_then(|h| h.video_memory()).map(|m| m[0]);
        let bytes = texture_block_for(set, budget, trim());
        tracing::info!(
            mb = bytes >> 20,
            budget_mb = budget.unwrap_or(0) >> 20,
            "blocks of textures (VICTORY_TEXTURE_BLOCK_MB, VICTORY_POOL_TRIM)"
        );
        bytes
    })
}

/// The bytes of a block of staging buffers, decided with the first one: a
/// setting of `VICTORY_STAGING_BLOCK_MB` (a power of two from 4 to 256) wins;
/// else 16 MiB on a PC with 24 GiB of memory or less, with [`trim`] on; else
/// 64 MiB, as before. Staging buffers are the PC's memory: blocks of 64 MiB
/// held nine times the live staging's bytes on a small PC.
fn staging_block_for(set: Option<u64>, ram: Option<u64>, trim: bool) -> u64 {
    match set {
        Some(mb) if mb.is_power_of_two() && (4..=256).contains(&mb) => mb << 20,
        _ if trim && ram.is_some_and(|r| r > 0 && r <= 24 << 30) => 16 << 20,
        _ => BLOCK,
    }
}

/// Whether the blocks of staging buffers are trimmed like those of textures
/// ([`TRIM_MS`]): [`trim`] is on and the blocks are smaller than 64 MiB; the
/// old 64 MiB blocks are let go at once past the spare ones.
fn staging_trims(block: u64) -> bool {
    trim() && block < BLOCK
}

/// [`staging_block_for`] for this PC, asked once.
fn staging_block() -> u64 {
    static BYTES: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *BYTES.get_or_init(|| {
        let set = std::env::var("VICTORY_STAGING_BLOCK_MB").ok().and_then(|v| v.trim().parse::<u64>().ok());
        let ram = crate::host::total_physical_ram();
        let bytes = staging_block_for(set, ram, trim());
        tracing::info!(
            mb = bytes >> 20,
            ram_mb = ram.unwrap_or(0) >> 20,
            "blocks of staging buffers (VICTORY_STAGING_BLOCK_MB, VICTORY_POOL_TRIM)"
        );
        bytes
    })
}

/// Milliseconds until a piece given back is used again. (It was three
/// frames, a tenth of a second while the title shows a loading screen at 30
/// a second, with up to 18 submissions in the air.)
const REST: u64 = 2500;

fn now() -> u64 {
    crate::host::now_ms()
}

/// How long no new block is tried after one was refused, in milliseconds
/// (`VICTORY_POOL_BACKOFF_MS`, 500 unless set; `0`: a block is tried each
/// time one is wanted, as before the back-off), read once.
fn backoff_ms() -> u64 {
    static MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MS.get_or_init(|| std::env::var("VICTORY_POOL_BACKOFF_MS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(500))
}

/// Whether growth is still in its back-off at `now` (it ends at `retry_at`).
fn refused(now: u64, retry_at: u64) -> bool {
    now < retry_at
}

pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_POOL").is_none_or(|v| v != "0"))
}

/// How many blocks of textures are made ahead, so that one has been there
/// for a while when its first piece is taken (`VICTORY_RIPE_BLOCKS`, 12;
/// 0: none, and new blocks are used at once, as before).
///
/// A page fault of lists already on the card, in a texture made the same tenth
/// of a second as the first piece of a block made for it, lost the device: the
/// title gives a slot that
/// those lists read by index a new texture. Why the card does not have the
/// new block's addresses yet is not known (one such run waited for `MakeResident`;
/// `indirect::tests::new_blocks_under_readers` never loses it). A block that
/// every submission begun before its making has left behind ([`Ripe`]) has
/// no such lists: pieces are cut from those first.
fn ahead() -> usize {
    static AHEAD: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *AHEAD.get_or_init(|| std::env::var("VICTORY_RIPE_BLOCKS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(12))
}

/// Submissions begun and finished ([`crate::host::submissions`]), and how
/// many blocks to make ahead (0: every block is ripe at once).
#[derive(Clone, Copy)]
struct Ripe {
    started: u64,
    finished: u64,
    ahead: usize,
}

impl Ripe {
    const ALL: Ripe = Ripe { started: 0, finished: 0, ahead: 0 };

    fn now() -> Ripe {
        let (started, finished) = crate::host::submissions();
        // Not more than are kept ([`room`]): they would go and be made again.
        Ripe { started, finished, ahead: ahead().min(SPARES.load(Ordering::Relaxed)) }
    }

    fn is<B>(self, block: &Block<B>) -> bool {
        self.ahead == 0 || block.made <= self.finished
    }
}

/// Pieces cut from blocks that were not ripe, for want of a ripe one.
static UNRIPE: AtomicU64 = AtomicU64::new(0);

/// The free ranges of a block: start and length, never two that touch.
struct Ranges(BTreeMap<u64, u64>);

impl Ranges {
    fn new(size: u64) -> Ranges {
        Ranges(BTreeMap::from([(0, size)]))
    }

    /// The lowest place for `len` bytes at a multiple of `align`.
    fn take(&mut self, len: u64, align: u64) -> Option<u64> {
        let (start, size, at) = self.0.iter().find_map(|(&start, &size)| {
            let at = start.next_multiple_of(align);
            (at + len <= start + size).then_some((start, size, at))
        })?;
        self.0.remove(&start);
        if at > start {
            self.0.insert(start, at - start);
        }
        if at + len < start + size {
            self.0.insert(at + len, start + size - at - len);
        }
        Some(at)
    }

    fn give(&mut self, mut start: u64, mut len: u64) {
        if let Some((&before, &size)) = self.0.range(..start).next_back()
            && before + size == start
        {
            self.0.remove(&before);
            (start, len) = (before, len + size);
        }
        if let Some(size) = self.0.remove(&(start + len)) {
            len += size;
        }
        self.0.insert(start, len);
    }

    fn whole(&self, size: u64) -> bool {
        self.0.get(&0) == Some(&size)
    }
}

/// A part of a block.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Piece {
    block: usize,
    start: u64,
    len: u64,
}

struct Block<B> {
    memory: B,
    free: Ranges,
    /// The end of the highest piece ever taken: above it the memory is as
    /// it was made (zeros).
    used_to: u64,
    /// The newest submission begun when it was made ([`Ripe`]).
    made: u64,
    /// When that was (ms).
    #[cfg_attr(not(feature = "diagnostics"), allow(dead_code))]
    since: u64,
    /// Since when (ms) nothing is in it: since it was made, or since its
    /// last piece came back; `None` while something is.
    empty_since: Option<u64>,
}

struct Pool<B> {
    /// `None`: a block let go, its place to be used again.
    blocks: Vec<Option<Block<B>>>,
    /// Pieces given back, and when each may be used again.
    resting: VecDeque<(u64, Piece)>,
    /// The empty blocks it was last told to keep, and the most it keeps.
    spare: usize,
    most: usize,
    /// The bytes of a block (0: not decided yet, [`Pool::sized`]).
    block: u64,
    /// Whether empty blocks past the spare ones go only once they have been
    /// empty for [`TRIM_MS`] ([`trim`]); else at once.
    trims: bool,
    /// Until when (ms) no new block is tried, after one was refused
    /// ([`Pool::grow`]).
    retry_growth_at: u64,
}

impl<B: Clone> Pool<B> {
    #[cfg(test)]
    const fn new(most: usize) -> Pool<B> {
        Pool { blocks: Vec::new(), resting: VecDeque::new(), spare: 0, most, block: BLOCK, trims: false, retry_growth_at: 0 }
    }

    /// A pool whose blocks have their size decided with the first one.
    const fn undecided(most: usize) -> Pool<B> {
        Pool { blocks: Vec::new(), resting: VecDeque::new(), spare: 0, most, block: 0, trims: false, retry_growth_at: 0 }
    }

    /// Decides the block size and the trim of a pool made [`Pool::undecided`].
    fn sized(&mut self, block: impl FnOnce() -> u64, trims: impl FnOnce() -> bool) {
        if self.block == 0 {
            self.block = block();
            self.trims = trims();
        }
    }

    /// A piece of `len` bytes and the memory it lies in, and whether that
    /// part was ever used before; `make` makes a block when none has room.
    fn take(&mut self, len: u64, align: u64, now: u64, make: impl FnMut() -> Option<B>) -> Option<(Piece, B, bool)> {
        self.take_ripe(len, align, now, Ripe::ALL, make)
    }

    /// The same from a block that is ripe, if one has room; `make` makes one
    /// ahead too, when few are empty.
    fn take_ripe(
        &mut self,
        len: u64,
        align: u64,
        now: u64,
        ripe: Ripe,
        mut make: impl FnMut() -> Option<B>,
    ) -> Option<(Piece, B, bool)> {
        // With blocks made ahead, what would be an allocation of its own (and
        // new when the card first reads it) lies in one of those too.
        let size = self.block;
        if len == 0 || len > if ripe.ahead > 0 { size } else { size / 2 } {
            return None;
        }
        self.rested(now);
        let mut found = None;
        for (i, block) in self.blocks.iter_mut().enumerate() {
            if let Some(block) = block
                && ripe.is(block)
                && let Some(start) = block.free.take(len, align)
            {
                found = Some((i, start));
                break;
            }
        }
        if found.is_none() && ripe.ahead > 0 {
            // The one that has been there the longest.
            let mut unripe: Vec<(u64, usize)> =
                self.blocks.iter().enumerate().filter_map(|(i, b)| Some((b.as_ref()?.made, i))).collect();
            unripe.sort_unstable();
            found = unripe.into_iter().find_map(|(_, i)| Some((i, self.blocks[i].as_mut()?.free.take(len, align)?)));
        }
        let (i, start) = match found {
            Some(found) => found,
            None => {
                let memory = self.grow(now, &mut make)?;
                let i = self.add(memory, ripe.started, now);
                (i, self.blocks[i].as_mut()?.free.take(len, align)?)
            }
        };
        if ripe.ahead > 0 {
            if !ripe.is(self.blocks[i].as_ref()?) {
                let n = UNRIPE.fetch_add(1, Ordering::Relaxed) + 1;
                if n.is_power_of_two() {
                    tracing::info!("textures cut from blocks made while lists ran that still run: {n}");
                }
            }
            let empty = self.blocks.iter().flatten().filter(|b| b.free.whole(size)).count();
            if empty < ripe.ahead
                && let Some(memory) = self.grow(now, &mut make)
            {
                self.add(memory, ripe.started, now);
            }
        }
        let block = self.blocks[i].as_mut()?;
        block.empty_since = None;
        let again = start < block.used_to;
        block.used_to = block.used_to.max(start + len);
        Some((Piece { block: i, start, len }, block.memory.clone(), again))
    }

    /// A new block from `make`, unless one was refused less than
    /// [`backoff_ms`] ago: a refused block otherwise costs a driver call for
    /// each small texture (and blocks of textures are 16 MiB, so growth is
    /// four times as frequent as with 64). Pieces free in blocks that exist
    /// stay usable meanwhile.
    ///
    /// After upstream GPL-3.0 fork, commit
    /// ab75ac3e02ba "Back off refused GPU pool growth and defer uploads under
    /// low commit": its `grow` and `retry_growth_at`. Here: the window is
    /// `VICTORY_POOL_BACKOFF_MS` (500 unless set, `0`: tried every time, as
    /// before), and a refused block is the `make` that gave `None`.
    fn grow(&mut self, now: u64, make: &mut impl FnMut() -> Option<B>) -> Option<B> {
        if refused(now, self.retry_growth_at) {
            return None;
        }
        let block = make();
        if block.is_none() {
            self.retry_growth_at = now.saturating_add(backoff_ms());
        }
        block
    }

    fn add(&mut self, memory: B, made: u64, now: u64) -> usize {
        let i = match self.blocks.iter().position(Option::is_none) {
            Some(i) => i,
            None => {
                self.blocks.push(None);
                self.blocks.len() - 1
            }
        };
        self.blocks[i] = Some(Block {
            memory,
            free: Ranges::new(self.block),
            used_to: 0,
            made,
            since: crate::host::now_ms(),
            empty_since: Some(now),
        });
        i
    }

    fn give(&mut self, piece: Piece, now: u64) {
        self.resting.push_back((now + REST, piece));
    }

    /// Makes the pieces that rested long enough free, and lets go of empty
    /// blocks past [`SPARE`] (with `trims`, of those empty for [`TRIM_MS`]).
    fn rested(&mut self, now: u64) {
        let size = self.block;
        let mut any = false;
        while let Some(&(ready, piece)) = self.resting.front()
            && ready <= now
        {
            self.resting.pop_front();
            if let Some(block) = self.blocks[piece.block].as_mut() {
                block.free.give(piece.start, piece.len);
                if block.free.whole(size) {
                    any = true;
                    // Empty since its last piece had rested.
                    block.empty_since = Some(ready);
                }
            }
        }
        // (Without a piece given back too, once there is less room.)
        let most = SPARES.load(Ordering::Relaxed).min(self.most);
        if self.trims {
            self.spare = most;
            self.let_go_empty(now, most);
        } else if any || most < self.spare {
            self.spare = most;
            let mut spare = 0;
            for block in &mut self.blocks {
                if block.as_ref().is_some_and(|b| b.free.whole(size)) {
                    spare += 1;
                    if spare > most {
                        *block = None;
                    }
                }
            }
        }
    }

    /// Lets go of the empty blocks past the `most` kept that have been empty
    /// for [`TRIM_MS`]: those kept are the ones made first (ripe soonest,
    /// [`Ripe`]); a block with a piece taken, or resting, is never let go.
    fn let_go_empty(&mut self, now: u64, most: usize) {
        let size = self.block;
        let mut empty: Vec<(u64, usize)> = self
            .blocks
            .iter()
            .enumerate()
            .filter_map(|(i, b)| b.as_ref().filter(|b| b.free.whole(size)).map(|b| (b.made, i)))
            .collect();
        if empty.len() <= most {
            return;
        }
        empty.sort_unstable();
        for &(_, i) in &empty[most..] {
            if self.blocks[i].as_ref().and_then(|b| b.empty_since).is_some_and(|since| now >= since + TRIM_MS) {
                self.blocks[i] = None;
            }
        }
    }

    fn blocks(&self) -> usize {
        self.blocks.iter().flatten().count()
    }

    /// Memory ran out: the pieces that rested long enough are free, and empty
    /// blocks go, spare ones too, the last made first, until `need` bytes have
    /// gone: with `trims` those empty for [`TRIM_MS`] (a slot that still shows
    /// a texture of one may be read, as for the trim's), else any. Their bytes.
    fn relieve(&mut self, now: u64, need: u64) -> u64 {
        self.rested(now);
        let size = self.block;
        let trims = self.trims;
        let mut empty: Vec<(u64, usize)> = self
            .blocks
            .iter()
            .enumerate()
            .filter_map(|(i, b)| b.as_ref().filter(|b| b.free.whole(size)).map(|b| (b, i)))
            .filter(|(b, _)| !trims || b.empty_since.is_some_and(|since| now >= since + TRIM_MS))
            .map(|(b, i)| (b.made, i))
            .collect();
        empty.sort_unstable_by(|a, b| b.cmp(a));
        let mut freed = 0;
        for (_, i) in empty {
            if freed >= need {
                break;
            }
            self.blocks[i] = None;
            freed += size;
        }
        freed
    }
}

/// `pool`, if nobody holds it now (memory ran out where its owner may be
/// making a block: `crate::host::Host::upload_buffer` is called with the lock
/// of staging held).
fn try_lock<T>(pool: &'static Mutex<T>) -> Option<std::sync::MutexGuard<'static, T>> {
    match pool.try_lock() {
        Ok(held) => Some(held),
        Err(std::sync::TryLockError::Poisoned(e)) => Some(e.into_inner()),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

/// Memory ran out (`crate::host::budget::relieve`): empty blocks of staging
/// buffers go, until `need` bytes have ([`Pool::relieve`]); their bytes.
pub(crate) fn relieve_staging(need: u64) -> u64 {
    try_lock(&STAGING).map_or(0, |mut pool| pool.relieve(now(), need))
}

/// The same of blocks of textures (the card's memory), as the trim of
/// [`room`] goes but past the spare ones too.
pub(crate) fn relieve_textures(need: u64) -> u64 {
    try_lock(&TEXTURES).map_or(0, |mut pool| pool.relieve(now(), need))
}

/// A mapped upload buffer's memory.
#[derive(Clone)]
struct Upload(ID3D12Resource, *mut u8);
// SAFETY: the mapping is written by the owners of its pieces, each its own.
unsafe impl Send for Upload {}

static TEXTURES: Mutex<Pool<ID3D12Heap>> = Mutex::new(Pool::undecided(SPARE_KEPT));
static STAGING: Mutex<Pool<Upload>> = Mutex::new(Pool::undecided(STAGING_KEPT));

fn lock<T>(m: &'static Mutex<T>) -> std::sync::MutexGuard<'static, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Bytes of the pieces textures hold now.
static HELD: AtomicU64 = AtomicU64::new(0);

/// Bytes of the texture blocks no texture holds: the card's memory in use
/// by us that new textures take first.
pub(crate) fn texture_room() -> u64 {
    let pool = lock(&TEXTURES);
    (pool.blocks() as u64 * pool.block).saturating_sub(HELD.load(Ordering::Relaxed))
}

/// For the memory line: blocks of textures and of staging buffers, in MiB.
#[cfg(feature = "diagnostics")]
pub(crate) fn memory() -> [u64; 2] {
    let textures = {
        let pool = lock(&TEXTURES);
        pool.blocks() as u64 * (pool.block >> 20)
    };
    let staging = {
        let pool = lock(&STAGING);
        pool.blocks() as u64 * (pool.block >> 20)
    };
    [textures, staging]
}

/// Part of a block of mapped upload memory, back in the pool when dropped.
pub(crate) struct StagingPiece(Piece);

impl Drop for StagingPiece {
    fn drop(&mut self) {
        lock(&STAGING).give(self.0, now());
    }
}

/// `bytes` of upload memory, all zeros: the buffer it lies in, where in
/// that buffer, and its mapping. `None`: not pooled.
pub(crate) fn staging(host: &crate::host::Host, bytes: u64) -> Option<(StagingPiece, ID3D12Resource, u64, *mut u8)> {
    if !on() {
        return None;
    }
    // Copies from a buffer begin at multiples of 512.
    let bytes = bytes.next_multiple_of(512);
    // (Asked before the pool is locked.)
    let block = staging_block();
    let mut pool = lock(&STAGING);
    pool.sized(|| block, || staging_trims(block));
    let taken = pool.take(bytes, 512, now(), || {
        // After upstream GPL-3.0 fork, commit
        // 18bdf7932b79 "Scope bindless refreshes to their heap and bound
        // speculative staging": only the growth of staging for textures nobody
        // needs yet (a pass of `heap_textures`, not a bake) waits for commit
        // headroom; what a table binds or a bake makes is sampled at once and
        // must not wait. Here: `crate::host::commit` with its own thresholds
        // (`VICTORY_COMMIT_GUARD=0`: always allowed, as before).
        let speculative = crate::texture::PICKING.get().is_some() && !crate::texture::BAKING.get();
        if speculative && !crate::host::commit::allow_staging_growth(false) {
            return None;
        }
        let (buffer, data) = host.upload_buffer(block)?;
        crate::host::name(&buffer, || "staging of textures (a block)".into());
        #[cfg(feature = "diagnostics")]
        crate::book::tag(&buffer, "staging of textures, in blocks");
        Some(Upload(buffer, data))
    });
    drop(pool);
    let (piece, memory, again) = taken?;
    let owned = StagingPiece(piece);
    // SAFETY: the piece lies in the block's mapping.
    let data = unsafe { memory.1.add(piece.start as usize) };
    if again {
        // What is not detiled into it (an unmapped texture) uploads zeros,
        // as from a buffer of its own.
        // SAFETY: the piece is ours alone.
        unsafe { std::ptr::write_bytes(data, 0, bytes as usize) };
    }
    Some((owned, memory.0, piece.start, data))
}

/// An object the host texture holds and releases when it goes: its piece
/// is free then.
#[repr(C)]
struct Reclaim {
    methods: &'static [*const (); 3],
    references: AtomicU32,
    piece: Piece,
}

struct Methods([*const (); 3]);
// SAFETY: function addresses only.
unsafe impl Sync for Methods {}

static RECLAIM: Methods = Methods([query as *const (), add_ref as *const (), release as *const ()]);

unsafe extern "system" fn query(this: *mut c_void, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    const E_NOINTERFACE: HRESULT = HRESULT(0x8000_4002_u32 as i32);
    // SAFETY: COM's arguments.
    unsafe {
        if out.is_null() {
            return HRESULT(0x8000_4003_u32 as i32);
        }
        if iid.as_ref().is_some_and(|iid| *iid == IUnknown::IID) {
            add_ref(this);
            *out = this;
            return HRESULT(0);
        }
        *out = std::ptr::null_mut();
    }
    E_NOINTERFACE
}

unsafe extern "system" fn add_ref(this: *mut c_void) -> u32 {
    // SAFETY: one of ours, alive while a reference to it is.
    unsafe { (*this.cast::<Reclaim>()).references.fetch_add(1, Ordering::Relaxed) + 1 }
}

unsafe extern "system" fn release(this: *mut c_void) -> u32 {
    // SAFETY: as above; the last reference frees the box it was made as.
    unsafe {
        let left = (*this.cast::<Reclaim>()).references.fetch_sub(1, Ordering::AcqRel) - 1;
        if left == 0 {
            let gone = Box::from_raw(this.cast::<Reclaim>());
            HELD.fetch_sub(gone.piece.len, Ordering::Relaxed);
            lock(&TEXTURES).give(gone.piece, now());
        }
        left
    }
}

/// Textures made in the last second ([`fresh`]), and when.
static RECENT: Mutex<VecDeque<(usize, u64)>> = Mutex::new(VecDeque::new());

/// Whether `texture` was made in a block in the last second.
pub(crate) fn fresh(texture: &ID3D12Resource) -> bool {
    let raw = texture.as_raw() as usize;
    let since = now().saturating_sub(1000);
    RECENT.lock().unwrap_or_else(|e| e.into_inner()).iter().rev().take_while(|r| r.1 >= since).any(|r| r.0 == raw)
}

fn made_now(texture: &ID3D12Resource) {
    let now = now();
    let mut recent = RECENT.lock().unwrap_or_else(|e| e.into_inner());
    while recent.front().is_some_and(|r| r.1 + 1000 < now) {
        recent.pop_front();
    }
    recent.push_back((texture.as_raw() as usize, now));
}

/// What a texture cut from a block carries, the object that gives its piece
/// back ([`give_back_with`]).
const KEY: GUID = GUID::from_u128(0x6f0e2f0c_5a59_4b1e_9a3f_7f4d2c9be713);

/// Whether `texture` was cut from a block ([`texture`]), so it is a placed
/// resource: one that is not (a committed one, made where the piece was too
/// large for a block) takes no aliasing barrier, and a list that records one
/// fails to close with E_INVALIDARG (the
/// test `an_aliasing_barrier_on_a_committed_texture_and_on_a_placed_one`).
pub(crate) fn in_block(texture: &ID3D12Resource) -> bool {
    let mut size = 0u32;
    // SAFETY: a size query of a live resource; no data is asked for.
    let found = unsafe { texture.GetPrivateData(&KEY, &mut size, None) };
    found.is_ok() && size > 0
}

/// Tests: marks `texture` as cut from a block, as [`give_back_with`] does,
/// without a block of the pool: a test's own device must leave no heap in
/// the pool the other tests of the process take from (they lost their
/// device when one did).
#[cfg(test)]
pub(crate) fn mark_in_block(texture: &ID3D12Resource) {
    let mark = 1u8;
    // SAFETY: one byte of private data on a live resource.
    unsafe { texture.SetPrivateData(&KEY, 1, Some(std::ptr::from_ref(&mark).cast())) }.expect("private data");
}

/// Makes `texture` give `piece` back when the last reference to it goes.
fn give_back_with(texture: &ID3D12Resource, piece: Piece) {
    let made = Box::into_raw(Box::new(Reclaim { methods: &RECLAIM.0, references: AtomicU32::new(1), piece }));
    // SAFETY: a COM object of ours with one reference, which this takes and
    // drops at the end; the texture takes its own.
    unsafe {
        let object = IUnknown::from_raw(made.cast());
        if texture.SetPrivateDataInterface(&KEY, &object).is_err() {
            // Never given back: the piece stays taken, nothing is reused
            // under a texture that may be alive.
            std::mem::forget(object);
        }
    }
}

/// A texture of `desc` (no target, depth or unordered access) in a block;
/// `None`: not pooled, or the host refused.
///
/// # Safety
/// `desc` is a valid description.
pub(crate) unsafe fn texture(
    device: &ID3D12Device,
    desc: *const D3D12_RESOURCE_DESC,
    state: D3D12_RESOURCE_STATES,
) -> Option<ID3D12Resource> {
    if !on() {
        return None;
    }
    // Small textures lie at multiples of 4 KiB where the host allows it
    // (the top mip is 64 KiB at most): 64 KiB each otherwise.
    // SAFETY: the caller's description.
    let mut d = unsafe { *desc };
    d.Alignment = u64::from(D3D12_SMALL_RESOURCE_PLACEMENT_ALIGNMENT);
    // SAFETY: plain queries.
    let mut info = unsafe { device.GetResourceAllocationInfo(0, &[d]) };
    if info.SizeInBytes == u64::MAX || info.Alignment != d.Alignment {
        d.Alignment = 0;
        info = unsafe { device.GetResourceAllocationInfo(0, &[d]) };
    }
    if info.SizeInBytes == u64::MAX {
        return None;
    }
    // (Asked before the pool is locked: it asks the card's budget.)
    let block = texture_block();
    let mut pool = lock(&TEXTURES);
    pool.sized(|| block, trim);
    #[cfg_attr(not(feature = "diagnostics"), allow(unused_variables))]
    let (piece, heap, again) = pool.take_ripe(info.SizeInBytes, info.Alignment.max(1), now(), Ripe::now(), || {
        let desc = D3D12_HEAP_DESC {
            SizeInBytes: block,
            Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
            Alignment: 0,
            Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES | crate::host::not_resident(),
        };
        let mut heap: Option<ID3D12Heap> = None;
        // SAFETY: plain creation.
        if let Err(e) = unsafe { device.CreateHeap(&desc, &mut heap) } {
            // After upstream GPL-3.0 fork, commit
            // ab75ac3e02ba "Back off refused GPU pool growth and defer uploads
            // under low commit": a refusal for lack of memory holds uploads back
            // for a while. Here: `crate::host::commit`, switched by
            // `VICTORY_COMMIT_GUARD`.
            crate::host::commit::allocation_error(&e);
            tracing::warn!(error = %e, "no block of texture memory");
        }
        if let Some(h) = &heap
            && let Err(e) = crate::host::resident(device, h, crate::host::NewMemory::TextureBlock)
                .and_then(|()| crate::host::mapped(device, crate::host::NewMemory::TextureBlock))
        {
            tracing::warn!(error = %e, "a block of texture memory is not resident");
            return None;
        }
        heap
    })?;
    drop(pool);
    let mut made: Option<ID3D12Resource> = None;
    // SAFETY: the piece is no one else's; the heap lives while a piece of
    // it is taken.
    let result = unsafe { device.CreatePlacedResource(&heap, piece.start, &d, state, None, &mut made) };
    match (result, made) {
        (Ok(()), Some(made)) => {
            HELD.fetch_add(piece.len, Ordering::Relaxed);
            give_back_with(&made, piece);
            made_now(&made);
            #[cfg(feature = "diagnostics")]
            {
                crate::book::made(&made, || "a texture in a block".into());
                crate::book::block(&heap, &made, piece.start, block);
                crate::book::note(made.as_raw() as usize, || {
                    let block = lock(&TEXTURES).blocks.get(piece.block).and_then(Option::as_ref).map(|b| (b.made, b.since));
                    let (submission, since) = block.unwrap_or_default();
                    format!(
                        "; piece {:#x}+{:#x} of block {} {:#x} made {:.1} s before it at submission {submission}, used before: {again}, made at submissions {:?}",
                        piece.start,
                        piece.len,
                        piece.block,
                        heap.as_raw() as usize,
                        now().saturating_sub(since) as f64 / 1e3,
                        crate::host::submissions()
                    )
                });
            }
            Some(made)
        }
        _ => {
            lock(&TEXTURES).give(piece, now());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn growth_is_refused_inside_its_window_only() {
        assert!(refused(0, 1));
        assert!(refused(499, 500));
        assert!(!refused(500, 500));
        assert!(!refused(0, 0));
        // (`VICTORY_POOL_BACKOFF_MS` unset: 500 ms.)
        assert_eq!(backoff_ms(), 500);
    }

    // After upstream GPL-3.0 fork, commit
    // ab75ac3e02ba: its test of the same name, with `add(memory, made, now)`.
    #[test]
    fn a_failed_block_is_not_retried_for_each_small_upload() {
        let mut pool: Pool<u32> = Pool::new(2);
        let calls = std::cell::Cell::new(0);
        let refuse = || {
            calls.set(calls.get() + 1);
            None
        };
        for size in [704008, 179720, 179720, 24080, 48648, 524288, 89616, 351760] {
            assert!(pool.take(size, 512, 12, refuse).is_none());
        }
        assert_eq!(calls.get(), 1);
        // Still inside the window just before it ends, open right at its end.
        assert!(pool.take(24080, 512, 12 + backoff_ms() - 1, || Some(1)).is_none());
        assert!(pool.take(24080, 512, 12 + backoff_ms(), || Some(1)).is_some());
    }

    #[test]
    fn a_refused_block_only_holds_back_new_blocks() {
        let mut pool: Pool<u32> = Pool::new(2);
        assert!(pool.take(BLOCK / 2, 512, 0, || Some(7)).is_some());
        assert!(pool.take(BLOCK / 2, 512, 0, || None).is_some());
        // The block is full: the next one is refused and growth waits...
        assert!(pool.take(BLOCK / 2, 512, 1, || None).is_none());
        assert!(pool.take(BLOCK / 2, 512, 2, || Some(8)).is_none());
        // ...until the window is over.
        assert!(pool.take(BLOCK / 2, 512, 1 + backoff_ms(), || Some(8)).is_some());
    }

    #[test]
    fn a_block_made_ahead_is_refused_during_the_window_too() {
        let mut pool: Pool<u32> = Pool::new(2);
        let now = 100;
        let ripe = Ripe { started: 0, finished: 0, ahead: 2 };
        let calls = std::cell::Cell::new(0);
        let make = || {
            calls.set(calls.get() + 1);
            (calls.get() == 1).then_some(1)
        };
        // The block for the piece, then the one ahead is refused...
        assert!(pool.take_ripe(4096, 512, now, ripe, make).is_some());
        assert_eq!(calls.get(), 2);
        // ...and neither a piece that fits nor ahead asks again in the window.
        assert!(pool.take_ripe(4096, 512, now + 1, ripe, make).is_some());
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn ranges_are_cut_aligned_and_join_again() {
        let mut r = Ranges::new(1024);
        assert_eq!(r.take(100, 64), Some(0));
        assert_eq!(r.take(100, 64), Some(128));
        assert_eq!(r.take(28, 4), Some(100));
        assert_eq!(r.take(2000, 4), None);
        r.give(128, 100);
        r.give(0, 100);
        assert!(!r.whole(1024));
        r.give(100, 28);
        assert!(r.whole(1024));
    }

    #[test]
    fn empty_blocks_stay_while_the_card_has_room() {
        assert_eq!(spares(Some([100, 84, 0, 0]), true), SPARE_KEPT);
        assert_eq!(spares(Some([100, 85, 0, 0]), true), SPARE);
        assert_eq!(spares(Some([100, 10, 0, 0]), false), SPARE);
        assert_eq!(spares(None, true), SPARE);
    }

    #[test]
    fn pieces_never_overlap_and_rest_before_they_are_used_again() {
        let mut pool: Pool<u32> = Pool::new(SPARE);
        let made = std::cell::Cell::new(0);
        let make = || {
            made.set(made.get() + 1);
            Some(made.get())
        };
        let mut taken: Vec<Piece> = Vec::new();
        for i in 0..500u64 {
            let (p, _, again) = pool.take(65536 * (1 + i % 7), 65536, 0, make).unwrap();
            assert!(!again);
            assert!(taken.iter().all(|q| q.block != p.block || p.start + p.len <= q.start || q.start + q.len <= p.start));
            taken.push(p);
        }
        let blocks = pool.blocks();
        assert!(blocks >= 2);
        // Given back at 10: not used again before 10 + REST.
        let first = taken[0];
        pool.give(first, 10);
        let (p, _, _) = pool.take(first.len, 65536, 10 + REST - 1, make).unwrap();
        assert_ne!((p.block, p.start), (first.block, first.start));
        let (q, _, again) = pool.take(first.len, 65536, 10 + REST, make).unwrap();
        assert_eq!((q.block, q.start, again), (first.block, first.start, true));
        // Everything back: the empty blocks past the spare ones go.
        for p in taken.drain(1..).chain([p, q]) {
            pool.give(p, 20);
        }
        pool.rested(20 + REST);
        assert_eq!(pool.blocks(), blocks.min(SPARE));
        // Too big for a block, and a block that cannot be made.
        assert!(pool.take(BLOCK, 65536, 30, make).is_none());
        let mut none: Pool<u32> = Pool::new(SPARE);
        assert!(none.take(65536, 65536, 0, || None).is_none());
    }

    #[test]
    fn pieces_come_from_blocks_older_than_the_lists_that_run() {
        let mut pool: Pool<u64> = Pool::new(SPARE_KEPT);
        let made = std::cell::Cell::new(0);
        let make = || {
            made.set(made.get() + 1);
            Some(made.get())
        };
        let at = |started, finished| Ripe { started, finished, ahead: 2 };
        let take = |pool: &mut Pool<u64>, ripe| pool.take_ripe(BLOCK / 2, 65536, 0, ripe, make).unwrap().1;
        // Nothing runs: at once, and two are made ahead, one a time.
        assert_eq!(take(&mut pool, at(0, 0)), 1);
        assert_eq!(made.get(), 2);
        assert_eq!(take(&mut pool, at(0, 0)), 1);
        assert_eq!(made.get(), 3);
        // 4 is made while submission 5 runs.
        assert_eq!(take(&mut pool, at(5, 4)), 2);
        assert_eq!(made.get(), 4);
        assert_eq!(take(&mut pool, at(5, 4)), 2);
        assert_eq!(take(&mut pool, at(6, 4)), 3);
        assert_eq!(take(&mut pool, at(6, 4)), 3);
        // None is ripe: the oldest; it is ripe once 5 has finished.
        assert_eq!(take(&mut pool, at(7, 4)), 4);
        assert_eq!(take(&mut pool, at(7, 5)), 4);
        // A whole block's worth too.
        assert_eq!(pool.take_ripe(BLOCK, 65536, 0, at(8, 7), make).unwrap().1, 5);
        // Off: the first with room, whatever its age, none ahead.
        let n = made.get();
        assert_eq!(pool.take(BLOCK / 2, 65536, 0, make).unwrap().1, 6);
        assert_eq!(made.get(), n);
        assert!(pool.take(BLOCK, 65536, 0, make).is_none());
    }

    /// The block of textures: a setting wins; else 16 MiB on a card whose
    /// budget is 8 GiB or less with the trim on, 64 MiB otherwise.
    #[test]
    fn blocks_of_textures_are_smaller_on_cards_of_8_gib() {
        const GIB: u64 = 1 << 30;
        assert_eq!(texture_block_for(None, Some(7 * GIB), true), 16 << 20);
        assert_eq!(texture_block_for(None, Some(8 * GIB), true), 16 << 20);
        assert_eq!(texture_block_for(None, Some(8 * GIB + 1), true), BLOCK);
        assert_eq!(texture_block_for(None, Some(20 * GIB), true), BLOCK);
        // The trim off, or the card not known: as before.
        assert_eq!(texture_block_for(None, Some(7 * GIB), false), BLOCK);
        assert_eq!(texture_block_for(None, None, true), BLOCK);
        assert_eq!(texture_block_for(None, Some(0), true), BLOCK);
        // Set: powers of two from 4 to 256 MiB, whatever the card.
        assert_eq!(texture_block_for(Some(32), Some(20 * GIB), false), 32 << 20);
        assert_eq!(texture_block_for(Some(4), Some(7 * GIB), true), 4 << 20);
        assert_eq!(texture_block_for(Some(64), Some(7 * GIB), true), BLOCK);
        assert_eq!(texture_block_for(Some(256), None, false), 256 << 20);
        for bad in [0, 1, 2, 3, 24, 100, 512] {
            assert_eq!(texture_block_for(Some(bad), Some(7 * GIB), true), 16 << 20, "{bad}");
            assert_eq!(texture_block_for(Some(bad), Some(20 * GIB), true), BLOCK, "{bad}");
        }
    }

    /// With the trim, a block with nothing in it goes only once it has been
    /// empty for its time, past the spare ones (none here); a block with a
    /// piece taken, or one resting, never goes; and a piece taken from an
    /// empty block before its time starts that time again.
    #[test]
    fn an_empty_block_goes_only_after_its_time_and_a_used_one_never() {
        const MIB: u64 = 1 << 20;
        let mut pool: Pool<u32> = Pool::undecided(0);
        pool.sized(|| 4 * MIB, || true);
        assert_eq!(pool.block, 4 * MIB);
        let made = std::cell::Cell::new(0);
        let make = || {
            made.set(made.get() + 1);
            Some(made.get())
        };
        let (a, _, _) = pool.take(MIB, 65536, 0, make).unwrap();
        let (b, _, _) = pool.take(MIB, 65536, 0, make).unwrap();
        assert_eq!((a.block, pool.blocks()), (b.block, 1));
        // One back: the other is in it, however late it is looked at.
        pool.give(a, 100);
        for t in [100 + REST, 100 + REST + TRIM_MS, 1_000_000] {
            pool.rested(t);
            assert_eq!(pool.blocks(), 1, "{t}");
        }
        // The other back: resting, then empty, then gone at its time.
        let t0 = 2_000_000;
        pool.give(b, t0);
        for t in [t0 + REST - 1, t0 + REST, t0 + REST + TRIM_MS - 1] {
            pool.rested(t);
            assert_eq!(pool.blocks(), 1, "{t}");
        }
        pool.rested(t0 + REST + TRIM_MS);
        assert_eq!(pool.blocks(), 0);
        // A new block, its piece back; taken again before its time: kept
        // while the piece is out, and gone its time after the piece is back.
        let t1 = 3_000_000;
        let (c, _, _) = pool.take(MIB, 65536, t1, make).unwrap();
        pool.give(c, t1);
        pool.rested(t1 + REST);
        let (d, _, again) = pool.take(MIB, 65536, t1 + REST + TRIM_MS - 1, make).unwrap();
        assert_eq!((d.block, d.start, again), (c.block, c.start, true));
        pool.rested(t1 + 100 * TRIM_MS);
        assert_eq!(pool.blocks(), 1);
        let t2 = t1 + 100 * TRIM_MS;
        pool.give(d, t2);
        pool.rested(t2 + REST + TRIM_MS - 1);
        assert_eq!(pool.blocks(), 1);
        pool.rested(t2 + REST + TRIM_MS);
        assert_eq!(pool.blocks(), 0);
    }

    /// The spare empty blocks stay however long they are empty (the ones
    /// made first); without the trim empty blocks past them go at once.
    #[test]
    fn spare_empty_blocks_stay_and_without_the_trim_the_others_go_at_once() {
        const MIB: u64 = 1 << 20;
        for trims in [true, false] {
            let mut pool: Pool<u32> = Pool::undecided(1);
            pool.sized(|| 4 * MIB, || trims);
            let made = std::cell::Cell::new(0);
            let make = || {
                made.set(made.get() + 1);
                Some(made.get())
            };
            // Two pieces a block, three blocks.
            let pieces: Vec<Piece> = (0..6).map(|_| pool.take(2 * MIB, 65536, 0, make).unwrap().0).collect();
            assert_eq!(pool.blocks(), 3);
            for p in pieces {
                pool.give(p, 10);
            }
            pool.rested(10 + REST);
            assert_eq!(pool.blocks(), if trims { 3 } else { 1 }, "{trims}");
            pool.rested(10 + REST + TRIM_MS);
            assert_eq!(pool.blocks(), 1, "{trims}");
            assert!(pool.blocks[0].is_some(), "the one made first stays");
            pool.rested(u64::MAX / 2);
            assert_eq!(pool.blocks(), 1, "{trims}");
        }
    }

    /// A pooled texture's piece comes back when its last reference goes, not
    /// before; its staging piece begins as zeros.
    #[test]
    fn a_texture_gives_its_piece_back_when_it_goes() {
        let Some(host) = crate::host::host() else { return };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 256,
            Height: 256,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..Default::default()
        };
        // SAFETY: a valid description.
        let Some(t) = (unsafe { texture(&host.device, &desc, D3D12_RESOURCE_STATE_COMMON) }) else { return };
        assert_eq!(crate::host::references(&t), 1, "what the texture holds is no reference to it");
        let resting = || lock(&TEXTURES).resting.len();
        let before = resting();
        let other = t.clone();
        drop(t);
        assert_eq!(resting(), before);
        drop(other);
        assert_eq!(resting(), before + 1);
        // SAFETY: the piece's mapping, 4096 bytes of it ours.
        unsafe {
            let (piece, _, at, data) = staging(host, 4096).unwrap();
            assert!(at.is_multiple_of(512));
            std::ptr::write_bytes(data, 0xAB, 4096);
            drop(piece);
            lock(&STAGING).rested(u64::MAX);
            let (_piece, _, _, data) = staging(host, 4096).unwrap();
            assert!((0..4096).all(|i| *data.add(i) == 0));
            // SAFETY: plain query.
            assert!(host.device.GetDeviceRemovedReason().is_ok());
        }
    }

    /// A relief lets go of empty blocks only, spare ones too, until it has
    /// what it needs: with the trim those empty for its time, without it any;
    /// a block with a piece out, or resting, never. (How many empty ones the
    /// pool itself keeps is the card's, [`room`], which other tests may set:
    /// the counts below hold either way.)
    #[test]
    fn a_relief_lets_go_of_empty_blocks_only() {
        const MIB: u64 = 1 << 20;
        for trims in [true, false] {
            let mut pool: Pool<u32> = Pool::undecided(SPARE_KEPT);
            pool.sized(|| 4 * MIB, || trims);
            let made = std::cell::Cell::new(0);
            let make = || {
                made.set(made.get() + 1);
                Some(made.get())
            };
            // Four blocks of two pieces; the pieces of the first three back.
            let pieces: Vec<Piece> = (0..8).map(|_| pool.take(2 * MIB, 65536, 0, make).unwrap().0).collect();
            assert_eq!(pool.blocks(), 4);
            for p in &pieces[..6] {
                pool.give(*p, 10);
            }
            // Resting: nothing to let go of.
            assert_eq!(pool.relieve(10 + REST - 1, u64::MAX), 0, "{trims}");
            assert_eq!(pool.blocks(), 4, "{trims}");
            // Rested, but with the trim not empty for its time yet: nothing.
            let mut t = 10 + REST;
            if trims {
                assert_eq!(pool.relieve(t, u64::MAX), 0);
                assert_eq!(pool.blocks(), 4);
                t += TRIM_MS;
            }
            // Empty for long enough: as many as asked for, past the spare
            // ones too.
            pool.rested(t);
            let before = pool.blocks();
            assert!(before >= 3, "{trims}: the pool keeps two empty ones at least");
            assert_eq!(pool.relieve(t, 4 * MIB), 4 * MIB, "{trims}");
            assert_eq!(pool.blocks(), before - 1, "{trims}");
            assert!(pool.relieve(t, u64::MAX) >= 4 * MIB, "{trims}");
            assert_eq!(pool.blocks(), 1, "{trims}: the block with pieces out stays");
            assert!(pool.blocks[3].is_some(), "{trims}");
            assert_eq!(pool.relieve(u64::MAX / 2, u64::MAX), 0, "{trims}");
        }
    }

    /// Blocks of staging buffers are 16 MiB on a PC of 24 GiB or less, 64 MiB else or with the trim off, and
    /// `VICTORY_STAGING_BLOCK_MB` wins when it is a power of two from 4 to 256.
    #[test]
    fn blocks_of_staging_are_smaller_on_pcs_of_24_gib_or_less() {
        const GIB: u64 = 1 << 30;
        let mib = |n: u64| n << 20;
        assert_eq!(staging_block_for(None, Some(16 * GIB), true), mib(16));
        assert_eq!(staging_block_for(None, Some(24 * GIB), true), mib(16));
        assert_eq!(staging_block_for(None, Some(32 * GIB), true), mib(64));
        assert_eq!(staging_block_for(None, None, true), mib(64));
        assert_eq!(staging_block_for(None, Some(16 * GIB), false), mib(64));
        assert_eq!(staging_block_for(Some(64), Some(16 * GIB), true), mib(64));
        assert_eq!(staging_block_for(Some(32), Some(64 * GIB), false), mib(32));
        assert_eq!(staging_block_for(Some(48), Some(16 * GIB), true), mib(16));
        assert_eq!(staging_block_for(Some(2), Some(16 * GIB), true), mib(16));
    }

    /// Staging blocks of 16 MiB: an empty one goes after its time, a piece over half a block is not pooled (an
    /// allocation of its own, as before), and one under it is.
    #[test]
    fn an_empty_block_of_staging_goes_after_its_time_and_a_large_piece_is_not_pooled() {
        const MIB: u64 = 1 << 20;
        let block = staging_block_for(None, Some(16 << 30), true);
        assert_eq!(block, 16 * MIB);
        let mut pool: Pool<u32> = Pool::undecided(0);
        pool.sized(|| block, || block < BLOCK);
        assert_eq!(pool.block, 16 * MIB);
        let made = std::cell::Cell::new(0);
        let make = || {
            made.set(made.get() + 1);
            Some(made.get())
        };
        assert!(pool.take(9 * MIB, 512, 0, make).is_none(), "over half a block: its own buffer");
        assert_eq!(made.get(), 0);
        let (a, _, _) = pool.take(8 * MIB, 512, 0, make).unwrap();
        pool.give(a, 100);
        pool.rested(100 + REST);
        assert_eq!(pool.blocks(), 1, "its time has not come");
        pool.rested(100 + REST + TRIM_MS - 1);
        assert_eq!(pool.blocks(), 1);
        pool.rested(100 + REST + TRIM_MS);
        assert_eq!(pool.blocks(), 0);
        // A block with a piece out stays however long.
        let (_b, _, _) = pool.take(MIB, 512, 1_000_000, make).unwrap();
        pool.rested(1_000_000 + 100 * TRIM_MS);
        assert_eq!(pool.blocks(), 1);
    }
}
