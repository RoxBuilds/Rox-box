//! The budget governor (`VICTORY_BUDGET`, on unless `0`, read once): one look
//! at the memory the layer lives in, at most every 32 ms, for the decisions
//! that depend on it, and the order in which what is idle is let go when an
//! allocation was refused.
//!
//! What binds on a PC of 16 GB or so is the PC's free memory. Every buffer
//! the CPU sees is made not resident and then made resident, which locks
//! its pages, so the system's count of the PC's memory shared with the card
//! stands for it: its budget was about 7 GB there, and all of it was in use
//! when allocations began to be refused, with gigabytes of the card's own
//! memory free and gigabytes left to commit; free memory was down to tens
//! of megabytes then.
//!
//! - [`sample`]: the card's own memory (budget, use), the PC's memory shared
//!   with it (budget, use), free physical memory and commit headroom, asked
//!   of the system at most every 32 ms. The commit guard takes its commit and
//!   free memory from it (`super::commit`).
//! - [`level`]: green, yellow or red, from the sample ([`level_of`]).
//! - [`place`]: where an allocation of some bytes should go: the card's own
//!   memory while it has room, else the PC's while that has, else nowhere
//!   ([`place_in`]). The allocations that may lie in either memory ask it
//!   through [`card_has_room`]: card copies of title buffers and cells of
//!   sparse windows (their alternative is the PC's memory).
//! - [`relieve`]: after a refusal, what is idle goes, in order, until the
//!   bytes asked for are let go: the lists' spare scratch above its floor,
//!   the spare memory of sends, empty blocks of texture staging, windows of
//!   title memory nothing holds, empty blocks of textures. Settling the
//!   flights and asking again is the caller's (`super::oom_retry`). What goes
//!   is what the shortage is of ([`chosen`], `VICTORY_RELIEF_BY_SHORTAGE`):
//!   the first four are the PC's memory, the last the card's.
//! - [`look`], once a second: with the PC's memory at red, its idle memory
//!   goes without waiting for a refusal ([`tend_need`],
//!   `VICTORY_RELIEF_AT_RED`); with the card's past 95 % of its budget, the
//!   card's, until it is back under 90 % ([`relief_next`],
//!   `VICTORY_CARD_RELIEF`): the system never refuses the card's memory, it
//!   moves what is past the budget into the PC's.
//!
//! `VICTORY_BUDGET=0`: none of it, as before: the commit guard asks the system
//! itself, [`relieve`] is the emergency trim (`super::emergency_trim`), and
//! [`place`] allows everything.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};

const MIB: u64 = 1 << 20;
/// The most often the system is asked (ms).
const SAMPLE_MS: u64 = 32;
/// Free physical memory under which the level is red.
const RED_RAM: u64 = 512 * MIB;
/// Commit headroom under which the level is red (the commit guard's
/// critical line).
const RED_COMMIT: u64 = 1536 * MIB;
/// Of the card's own budget, what an allocation that may go to either memory
/// may take it to before it goes to the PC's ([`place_in`]), in percent.
const CARD_PCT: u64 = 88;

/// Whether the governor acts (`VICTORY_BUDGET`, on unless `0`).
pub(crate) fn on() -> bool {
    crate::toggled!("VICTORY_BUDGET")
}

/// What the system said last.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Sample {
    /// The card's own memory: budget and use, in bytes (0 and 0 while not
    /// known).
    pub(crate) card: [u64; 2],
    /// The PC's memory shared with the card: budget and use.
    pub(crate) shared: [u64; 2],
    /// Free physical memory, and the page file's room (commit headroom);
    /// `u64::MAX` while not known.
    pub(crate) ram: u64,
    pub(crate) commit: u64,
}

impl Sample {
    pub(crate) const UNKNOWN: Sample = Sample { card: [0, 0], shared: [0, 0], ram: u64::MAX, commit: u64::MAX };

    fn words(&self) -> [u64; 6] {
        [self.card[0], self.card[1], self.shared[0], self.shared[1], self.ram, self.commit]
    }

    fn of(w: [u64; 6]) -> Sample {
        Sample { card: [w[0], w[1]], shared: [w[2], w[3]], ram: w[4], commit: w[5] }
    }
}

/// The last sample, as [`Sample::words`], and when the next is due
/// ([`super::coarse_ms`]).
static CACHED: [AtomicU64; 6] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(u64::MAX),
    AtomicU64::new(u64::MAX),
];
static NEXT: AtomicU64 = AtomicU64::new(0);

/// The system's answers now. The host is only looked at if it is made: an
/// allocation refused while it is being made asks too.
fn ask() -> Sample {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut s = Sample::UNKNOWN;
    if let Some([card_budget, card_used, budget, used]) = super::made_host().and_then(|h| h.video_memory()) {
        s.card = [card_budget, card_used];
        s.shared = [budget, used];
        crate::runreport::video_memory(s.card, s.shared);
    }
    // SAFETY: plain data the call fills in.
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: a sized structure.
    if unsafe { GlobalMemoryStatusEx(&mut m) } != 0 {
        s.ram = m.ullAvailPhys;
        s.commit = m.ullAvailPageFile;
    }
    crate::runreport::system_memory(s.ram, s.commit);
    s
}

/// The last sample, asked again when [`SAMPLE_MS`] have passed (by whichever
/// thread comes first; the others take the one before).
pub(crate) fn sample() -> Sample {
    let now = super::coarse_ms();
    let next = NEXT.load(Ordering::Relaxed);
    if now >= next
        && NEXT.compare_exchange(next, now.saturating_add(SAMPLE_MS), Ordering::Relaxed, Ordering::Relaxed).is_ok()
    {
        for (slot, word) in CACHED.iter().zip(ask().words()) {
            slot.store(word, Ordering::Relaxed);
        }
    }
    Sample::of(std::array::from_fn(|i| CACHED[i].load(Ordering::Relaxed)))
}

/// How short memory is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Level {
    Green,
    /// Free memory under [`Lines::free_ram`], or the PC's shared memory past
    /// [`Lines::shared_pct`] of its budget, or the card's own past 88 % of
    /// its.
    Yellow,
    /// Free memory under 512 MiB, or commit headroom under 1,536 MiB, or
    /// either memory group at or past its budget.
    Red,
}

/// The lines [`place_in`] and [`level_of`] go by.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Lines {
    /// The most of the card's own budget an allocation that may go either
    /// way takes it to (percent).
    pub(crate) card_pct: u64,
    /// The most of the shared budget an allocation in the PC's memory takes
    /// it to (`VICTORY_BUDGET_SHARED_PCT`, 60).
    pub(crate) shared_pct: u64,
    /// The free physical memory an allocation in the PC's memory leaves at
    /// least (`VICTORY_BUDGET_FREE_RAM_MB`, 1,536 MiB).
    pub(crate) free_ram: u64,
}

/// [`Lines`] from the switches, read once.
fn lines() -> Lines {
    static LINES: OnceLock<Lines> = OnceLock::new();
    *LINES.get_or_init(|| {
        let number = |name: &str| std::env::var(name).ok().and_then(|v| v.trim().parse::<u64>().ok());
        let lines = Lines {
            card_pct: CARD_PCT,
            shared_pct: number("VICTORY_BUDGET_SHARED_PCT").unwrap_or(60).clamp(1, 100),
            free_ram: number("VICTORY_BUDGET_FREE_RAM_MB").unwrap_or(1536).saturating_mul(MIB),
        };
        if on() {
            tracing::info!(
                shared_pct = lines.shared_pct,
                free_ram_mb = lines.free_ram / MIB,
                card_pct = lines.card_pct,
                "the budget governor (VICTORY_BUDGET=0: off; VICTORY_BUDGET_SHARED_PCT, VICTORY_BUDGET_FREE_RAM_MB)"
            );
        }
        lines
    })
}

/// Whether `used` is at or past `pct` percent of `budget` (an unknown budget,
/// 0, never is).
fn past([budget, used]: [u64; 2], pct: u64) -> bool {
    budget > 0 && u128::from(used) * 100 >= u128::from(budget) * u128::from(pct)
}

/// The level of sample `s` by `lines`.
pub(crate) fn level_of(s: &Sample, lines: &Lines) -> Level {
    if s.ram < RED_RAM || s.commit < RED_COMMIT || past(s.shared, 100) || past(s.card, 100) {
        Level::Red
    } else if s.ram < lines.free_ram || past(s.shared, lines.shared_pct) || past(s.card, lines.card_pct) {
        Level::Yellow
    } else {
        Level::Green
    }
}

/// The level now (green with the governor off).
pub(crate) fn level() -> Level {
    if !on() {
        return Level::Green;
    }
    level_of(&sample(), &lines())
}

/// Whether the PC's memory is short now ([`short_of`]; never with the
/// governor off). Asked at most every 32 ms of the system ([`sample`]).
pub(crate) fn pc_short() -> bool {
    on() && short_of(&sample(), &lines()).pc
}

/// Which memory a sample says is short: the PC's (free memory under
/// [`Lines::free_ram`], commit headroom under [`RED_COMMIT`], or the shared
/// memory past [`Lines::shared_pct`] of its budget) and the card's own (past
/// [`Lines::card_pct`] of its budget). What is not known is not short.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Short {
    pub(crate) pc: bool,
    pub(crate) card: bool,
}

/// [`Short`] of sample `s` by `lines`.
pub(crate) fn short_of(s: &Sample, lines: &Lines) -> Short {
    Short {
        pc: s.ram < lines.free_ram || s.commit < RED_COMMIT || past(s.shared, lines.shared_pct),
        card: past(s.card, lines.card_pct),
    }
}

/// What a way of letting go frees: the PC's memory (its free memory, commit
/// and the part it shares with the card) or the card's own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Frees {
    Pc,
    Card,
}

/// Whether [`relieve`] lets go of what the shortage is of
/// (`VICTORY_RELIEF_BY_SHORTAGE`, on unless `0`: every step in its order, as
/// before).
fn by_shortage() -> bool {
    crate::toggled!("VICTORY_RELIEF_BY_SHORTAGE")
}

/// Of steps that free `frees` (in their order), the ones taken for `short`,
/// as indices in the order they are taken. With the PC's memory short and the
/// card's not, only the PC's: free memory may fall under the line several
/// times a run with gigabytes of the card free, and empty blocks of textures
/// are the card's
/// memory, so letting them go frees none of the PC's and costs a block to make
/// again. With the card's short and the PC's not, the card's first, then the
/// PC's (the refusal may have been of either). Both short, or neither (the
/// refusal is the only sign), or `by_shortage` off: all, in order, as before.
fn chosen(frees: &[Frees], short: Short, by_shortage: bool) -> Vec<usize> {
    let all = (0..frees.len()).collect();
    if !by_shortage {
        return all;
    }
    let of = |kind: Frees| (0..frees.len()).filter(move |&i| frees[i] == kind);
    match (short.pc, short.card) {
        (true, false) => of(Frees::Pc).collect(),
        (false, true) => of(Frees::Card).chain(of(Frees::Pc)).collect(),
        _ => all,
    }
}

/// Whether the governor lets go of the PC's idle memory by itself at a look
/// while that memory is at red (`VICTORY_RELIEF_AT_RED`, on unless `0`: only
/// after a refusal, as before).
fn at_red() -> bool {
    crate::toggled!("VICTORY_RELIEF_AT_RED")
}

/// The bytes the PC's idle memory is let go of by at a look of sample `s`
/// ([`at_red`]): with the PC's memory at red (free memory under 512 MiB,
/// commit headroom under 1,536 MiB, or the shared memory at its budget),
/// what brings free memory back to [`Lines::free_ram`], commit headroom to
/// its red line and the shared memory to [`Lines::shared_pct`], whichever is
/// most (at least 64 MiB); else 0. The card's level plays no part: what a
/// shortage of the card's memory lets go of is the card's (the sweep of
/// textures, `crate::resource::let_go_idle`). Where nothing is refused,
/// nothing of [`relieve`] ever runs, while free memory stands at a few
/// hundred MB and the lists keep spare scratch
/// and the sends 120 MB.
fn tend_need(s: &Sample, lines: &Lines) -> u64 {
    let red = s.ram < RED_RAM || s.commit < RED_COMMIT || past(s.shared, 100);
    if !red {
        return 0;
    }
    let [budget, used] = s.shared;
    let shared = if budget > 0 { used.saturating_sub(budget / 100 * lines.shared_pct) } else { 0 };
    let ram = if s.ram == u64::MAX { 0 } else { lines.free_ram.saturating_sub(s.ram) };
    let commit = if s.commit == u64::MAX { 0 } else { RED_COMMIT.saturating_sub(s.commit) };
    ram.max(commit).max(shared).max(64 * MIB)
}

/// Of the card's own budget, the use at which [`look`] begins to relieve the
/// card, and the use under which it stops (percent; [`relief_next`]).
const RELIEF_ON_PCT: u64 = 95;
const RELIEF_OFF_PCT: u64 = 90;

/// Whether the governor relieves the card's own memory at its looks while
/// that memory is past 95 % of its budget, until it is back under 90 %
/// (`VICTORY_CARD_RELIEF`, on unless `0`: only after a refusal, as before;
/// [`card_relieving`] says where else it acts).
///
/// The system never refuses an allocation in the card's memory for want of
/// room: past the budget it moves what does not fit into the PC's memory, so
/// [`relieve`] never ran for the card. On an 8 GB card in a 16 GB PC the card
/// stood past its budget at most looks of an open area (8.2 GB in use of a
/// budget of 6.9), what was moved filled the PC's free memory to nothing, and
/// making new memory resident waited 62 s of a 700 s run.
fn card_relief_on() -> bool {
    crate::toggled!("VICTORY_CARD_RELIEF")
}

/// Whether the card is being relieved now ([`relief_next`]): the texture
/// sweep then goes by the system's own count of the card (not less what our
/// blocks of textures have free), the largest idle copies first, and new
/// copies are made of fewer levels (`crate::resource`). Never with the
/// governor off.
pub(crate) fn card_relieving() -> bool {
    CARD_RELIEF.load(Ordering::Relaxed)
}

/// The card is being relieved ([`card_relieving`]).
static CARD_RELIEF: AtomicBool = AtomicBool::new(false);

/// Whether the card is to be relieved after a look at its memory `card`
/// (budget and use) when it `was` being relieved, and the bytes to let go of:
/// from a use at or past [`RELIEF_ON_PCT`] of the budget on, until it is
/// back under [`RELIEF_OFF_PCT`] (the gap keeps it from going on and off at
/// each look), what brings it down to that line. Unknown (a budget of 0):
/// never.
fn relief_next(was: bool, card: [u64; 2]) -> (bool, u64) {
    let [budget, used] = card;
    let on = if was { past(card, RELIEF_OFF_PCT) } else { past(card, RELIEF_ON_PCT) };
    let line = (u128::from(budget) * u128::from(RELIEF_OFF_PCT) / 100) as u64;
    (on, if on { used.saturating_sub(line).max(1) } else { 0 })
}

/// Whether an allocation of `bytes` that may lie in either memory (one the
/// caller makes in the PC's memory otherwise) goes to the card's own memory,
/// by `memory` (the host's `video_memory`, asked now: a burst of them comes
/// faster than the governor's sample) and the line `pct` of the card's budget
/// the caller holds it under: [`place_in`] with that line. With nothing known,
/// the card.
///
/// Not refused for a relief of the card ([`card_relieving`]), which begins
/// past 95 %: the callers' lines (97 %) already keep these from taking the
/// card past its budget, and their other memory costs as much: a card copy
/// of a title buffer turned down stays in the PC's memory for as long as the
/// title keeps the buffer, and the CPU then looks for shader writes in it
/// (frames half as long again when a scene's copies were turned down); a
/// cell of sparse windows turned down is a window of the PC's memory.
pub(crate) fn card_has_room(memory: Option<[u64; 4]>, bytes: u64, pct: u64) -> bool {
    let Some([budget, used, shared_budget, shared_used]) = memory else {
        return true;
    };
    let s = Sample { card: [budget, used], shared: [shared_budget, shared_used], ram: u64::MAX, commit: u64::MAX };
    // (Only whether it goes to the card is asked: the PC's lines play no part.)
    takes_in(&s, bytes, &Lines { card_pct: pct, shared_pct: 100, free_ram: 0 })
}

/// [`card_has_room`] for sample `s` and `lines`.
fn takes_in(s: &Sample, bytes: u64, lines: &Lines) -> bool {
    place_in(s, bytes, Kind::Either, lines) == Place::Card
}

/// While the card is relieved: looks that relieved it; the bytes given back
/// to the system (blocks of textures that went, host copies of textures of
/// their own memory let go or cut down); the bytes of host copies let go or
/// made again elsewhere inside blocks of textures (which give nothing back
/// by themselves); blocks the compaction emptied that went
/// (`crate::pool::compact`); and what the system counted of the card less
/// after those went than before, summed.
static RELIEVED: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

/// For the run report: [`RELIEVED`].
pub(crate) fn relief_counts() -> [u64; 5] {
    RELIEVED.each_ref().map(|n| n.load(Ordering::Relaxed))
}

/// The texture sweep let a host copy of `bytes` go, or made it again (cut
/// down or elsewhere), while the card is relieved: given back to the system
/// unless it lay in a block of textures (`pooled`). Nothing otherwise.
pub(crate) fn relief_let_go(pooled: bool, bytes: u64) {
    if card_relieving() {
        RELIEVED[if pooled { 2 } else { 1 }].fetch_add(bytes, Ordering::Relaxed);
    }
}

/// The most bytes of host copies in the blocks of textures the compaction
/// chooses to empty at one look, in hundredths of the card's budget: each
/// is uploaded again.
const COMPACT_LIVE_PCT: u64 = 2;

/// The card is past its relief's line by `need` bytes (its budget `budget`):
/// what it holds idle goes. Empty blocks of textures at once (past the spare
/// ones too, not those made ahead); cells of sparse windows idle for half a
/// second at the next submission, with the heaps their tiles leave empty;
/// idle host copies of textures in the sweeps that follow
/// (`crate::resource::let_go_idle`, which goes by [`card_relieving`]); with
/// the compaction (`crate::pool::compacting`), the blocks of textures least
/// full are chosen to be emptied, and those emptied go. What went back to
/// the system at once, in bytes.
fn card_tend(need: u64, budget: u64) -> u64 {
    let mut blocks = crate::pool::relieve_textures_for_card;
    let mut cells = |_: u64| {
        crate::shadow::hurry_sparse();
        0
    };
    let done = relieve_with(
        need,
        &mut [("empty blocks of textures", &mut blocks), ("cells of sparse windows idle for half a second", &mut cells)],
    );
    let mut freed = tell(&done, &CARD_TENDED, || {
        format!(
            "the card's memory is past {RELIEF_ON_PCT} % of its budget ({} MB over {RELIEF_OFF_PCT} %; idle textures go in the \
             sweeps, new ones are made of fewer levels; VICTORY_CARD_RELIEF=0: only after a refusal)",
            need / MIB
        )
    });
    if crate::pool::compacting() {
        let card = || super::host().and_then(|h| h.video_memory()).map_or(0, |m| m[1]);
        let before = card();
        let most_live = budget / 100 * COMPACT_LIVE_PCT;
        if let Some((blocks, bytes)) = crate::pool::compact(need, most_live)
            && blocks > 0
        {
            let returned = before.saturating_sub(card());
            freed += bytes;
            RELIEVED[3].fetch_add(blocks, Ordering::Relaxed);
            RELIEVED[4].fetch_add(returned, Ordering::Relaxed);
            let n = COMPACTED.fetch_add(1, Ordering::Relaxed) + 1;
            if n <= 8 || n.is_power_of_two() {
                tracing::info!(
                    times = n,
                    blocks,
                    block_mb = bytes / MIB,
                    card_less_mb = returned / MIB,
                    "blocks of textures emptied by the card's compaction went (VICTORY_CARD_COMPACT)"
                );
            }
        }
    }
    freed
}

/// Looks at which blocks the compaction emptied went (for the log).
static COMPACTED: AtomicU64 = AtomicU64::new(0);

/// Reliefs of the card that let go of something at once (for the log).
static CARD_TENDED: AtomicU64 = AtomicU64::new(0);

/// The level the last [`look`] found, and how many times it changed.
static LOOKED: AtomicU8 = AtomicU8::new(0);
static CHANGES: AtomicU64 = AtomicU64::new(0);

/// Once a second (the sweep): with the PC's memory at red, lets go of its
/// idle memory ([`at_red`]); with the card's past its relief's line, of the
/// card's ([`card_relief_on`]); says when the level changed, the first 8
/// times and then at powers of two.
pub(crate) fn look() {
    if !on() {
        return;
    }
    let s = sample();
    let need = if at_red() { tend_need(&s, &lines()) } else { 0 };
    if need > 0 {
        tend(need);
    }
    if card_relief_on() {
        let was = CARD_RELIEF.load(Ordering::Relaxed);
        let (now, need) = relief_next(was, s.card);
        CARD_RELIEF.store(now, Ordering::Relaxed);
        if !now {
            crate::pool::compact_end();
        }
        if now != was {
            tracing::info!(
                card_mb = s.card[1] / MIB,
                card_budget_mb = s.card[0] / MIB,
                "the card's memory is {} (VICTORY_CARD_RELIEF)",
                if now { "relieved from now on" } else { "back under its line: no longer relieved" }
            );
        }
        if need > 0 {
            let freed = card_tend(need, s.card[0]);
            RELIEVED[0].fetch_add(1, Ordering::Relaxed);
            RELIEVED[1].fetch_add(freed, Ordering::Relaxed);
        }
    }
    let now = level_of(&s, &lines());
    crate::runreport::governor_look(&s, now);
    use victory_pipeline::report::{Stat, max, set};
    max(Stat::GovernorWorst, now as u64 + 1);
    let was = LOOKED.swap(now as u8, Ordering::Relaxed);
    if was == now as u8 {
        return;
    }
    let n = CHANGES.fetch_add(1, Ordering::Relaxed) + 1;
    set(Stat::GovernorChanges, n);
    if n <= 8 || n.is_power_of_two() {
        tracing::info!(
            changes = n,
            card_mb = s.card[1] / MIB,
            card_budget_mb = s.card[0] / MIB,
            shared_mb = s.shared[1] / MIB,
            shared_budget_mb = s.shared[0] / MIB,
            free_ram_mb = if s.ram == u64::MAX { 0 } else { s.ram / MIB },
            commit_headroom_mb = if s.commit == u64::MAX { 0 } else { s.commit / MIB },
            "the budget governor's level is now {now:?} (VICTORY_BUDGET)"
        );
    }
}

/// What memory an allocation may lie in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum Kind {
    /// The card's own or the PC's shared with it (a buffer the CPU writes
    /// and the card reads, which a mapped buffer of the card's memory can
    /// be): the card's first.
    Either,
    /// Only the PC's memory (the CPU maps it: upload, read-back, windows).
    Shared,
    /// Only the card's own (textures, targets).
    Card,
}

/// Where an allocation goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum Place {
    Card,
    Shared,
    /// In neither without going past their lines: let go of something first,
    /// or do without.
    Refuse,
}

/// Where `bytes` of `kind` go by sample `s` and `lines`: to the card's own
/// memory while its use with them stays under [`Lines::card_pct`] of its
/// budget (under all of it for [`Kind::Card`]); else to the PC's while the
/// shared use with them stays under [`Lines::shared_pct`] of its budget and
/// they leave [`Lines::free_ram`] free; else nowhere. What is not known (a
/// budget of 0, free memory `u64::MAX`) holds nothing back. ([`card_takes`]
/// asks it for allocations that may go either way.)
pub(crate) fn place_in(s: &Sample, bytes: u64, kind: Kind, lines: &Lines) -> Place {
    let fits = |[budget, used]: [u64; 2], pct: u64| {
        budget == 0 || (u128::from(used) + u128::from(bytes)) * 100 < u128::from(budget) * u128::from(pct)
    };
    let card = match kind {
        Kind::Either => fits(s.card, lines.card_pct),
        Kind::Card => fits(s.card, 100),
        Kind::Shared => false,
    };
    if card {
        return Place::Card;
    }
    let room = s.ram == u64::MAX || s.ram > lines.free_ram.saturating_add(bytes);
    if kind != Kind::Card && fits(s.shared, lines.shared_pct) && room { Place::Shared } else { Place::Refuse }
}

/// [`place_in`] for the sample now; with the governor off, the memory `kind`
/// prefers, as before.
#[allow(dead_code)]
pub(crate) fn place(bytes: u64, kind: Kind) -> Place {
    if !on() {
        return if kind == Kind::Shared { Place::Shared } else { Place::Card };
    }
    place_in(&sample(), bytes, kind, &lines())
}

/// One way of letting go of what is idle: what it lets go of (for the log),
/// and the call, given the bytes still wanted, that returns the bytes it let
/// go of.
type Step<'a> = (&'static str, &'a mut dyn FnMut(u64) -> u64);

/// Calls `steps` in order, each with the bytes still wanted, until `need`
/// bytes are let go of: what each one that was called let go of.
fn relieve_with(need: u64, steps: &mut [Step<'_>]) -> Vec<(&'static str, u64)> {
    let mut done = Vec::with_capacity(steps.len());
    let mut freed = 0u64;
    for (what, step) in steps.iter_mut() {
        if freed >= need {
            break;
        }
        let got = step(need - freed);
        freed = freed.saturating_add(got);
        done.push((*what, got));
    }
    done
}

/// Reliefs that let go of something, since the start (for the log): after
/// a refusal, and at red ([`tend`]).
static RELIEFS: AtomicU64 = AtomicU64::new(0);
static TENDED: AtomicU64 = AtomicU64::new(0);

/// An allocation of `need` bytes was refused for want of memory: what is
/// idle goes, in order, until `need` bytes have gone (some go later: windows
/// a running submission may still read wait for it), of the memory that is
/// short ([`chosen`]). The bytes let go of. Every step only tries the locks
/// it needs: it may be called with any of ours held (a block of staging
/// being made, a list's scratch). With the governor off, the emergency trim,
/// as before.
pub(crate) fn relieve(need: u64) -> u64 {
    if !on() {
        super::emergency_trim();
        return 0;
    }
    let short = short_of(&sample(), &lines());
    let mut scratch = super::scratch_relieve;
    let mut sends = super::send::relieve;
    let mut staging = crate::pool::relieve_staging;
    let mut windows = |_: u64| crate::shadow::let_go_short().1;
    let mut textures = crate::pool::relieve_textures;
    let all: [(&'static str, Frees, &mut dyn FnMut(u64) -> u64); 5] = [
        ("the lists' spare scratch above its floor", Frees::Pc, &mut scratch),
        ("the spare memory of sends", Frees::Pc, &mut sends),
        ("empty blocks of texture staging", Frees::Pc, &mut staging),
        ("windows of title memory nothing holds", Frees::Pc, &mut windows),
        ("empty blocks of textures", Frees::Card, &mut textures),
    ];
    let order = chosen(&all.each_ref().map(|s| s.1), short, by_shortage());
    let mut slots = all.map(|(what, _, step)| Some((what, step)));
    let mut steps: Vec<Step<'_>> = order.iter().filter_map(|&i| slots[i].take()).collect();
    let done = relieve_with(need.max(1), &mut steps);
    tell(&done, &RELIEFS, || format!("memory ran out ({} MB asked, short: {short:?}, level {:?})", need / MIB, level()))
}

/// The PC's memory is at red ([`tend_need`]): its idle memory goes until
/// `need` bytes have, as [`relieve`] lets it go but without the sends' spare
/// memory, which the next frame takes again (a frame's sends are tens of
/// megabytes, and they keep 128 MB at most on a small PC).
fn tend(need: u64) -> u64 {
    let mut scratch = super::scratch_relieve_at_red;
    let mut staging = crate::pool::relieve_staging;
    let mut windows = |_: u64| crate::shadow::let_go_short().1;
    let done = relieve_with(
        need,
        &mut [
            ("the lists' spare scratch above its floor", &mut scratch),
            ("empty blocks of texture staging", &mut staging),
            ("windows of title memory nothing holds", &mut windows),
        ],
    );
    tell(&done, &TENDED, || {
        format!("the PC's memory is at red ({} MB short, VICTORY_RELIEF_AT_RED=0: only after a refusal)", need / MIB)
    })
}

/// The bytes `done` let go of; says what they were, the first 8 times and
/// then at powers of two that `count` counts, as `why` says.
fn tell(done: &[(&'static str, u64)], count: &AtomicU64, why: impl FnOnce() -> String) -> u64 {
    let freed: u64 = done.iter().map(|d| d.1).sum();
    if freed > 0 {
        let n = count.fetch_add(1, Ordering::Relaxed) + 1;
        if n <= 8 || n.is_power_of_two() {
            let what: Vec<String> =
                done.iter().filter(|d| d.1 > 0).map(|(w, b)| format!("{w} {:.1} MB", *b as f64 / MIB as f64)).collect();
            tracing::warn!(
                times = n,
                "{}: let go of {} (VICTORY_BUDGET, VICTORY_RELIEF_BY_SHORTAGE)",
                why(),
                what.join(", ")
            );
        }
    }
    freed
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    fn lines() -> Lines {
        Lines { card_pct: CARD_PCT, shared_pct: 60, free_ram: 1536 * MIB }
    }

    /// A card of 8 GB and a PC of 16 GB: budgets of that size, the uses and
    /// free memory given.
    fn at(card_used: u64, shared_used: u64, ram: u64, commit: u64) -> Sample {
        Sample { card: [7168 * MIB, card_used], shared: [7360 * MIB, shared_used], ram, commit }
    }

    /// Either memory: the card's while it stays under 88 % of its budget,
    /// then the PC's while that stays under 60 % and 1.5 GB stay free, then
    /// nowhere; the PC's only, or the card's only, as their lines say; and
    /// what is not known holds nothing back.
    #[test]
    fn allocations_are_placed_by_the_table() {
        let l = lines();
        // The least use at or past each line.
        let card_line = (7168 * MIB * 88).div_ceil(100);
        let shared_line = (7360 * MIB * 60).div_ceil(100);
        // (card used, shared used, free RAM, bytes, kind, place)
        let table = [
            (GIB, GIB, 8 * GIB, 64 * MIB, Kind::Either, Place::Card),
            (card_line - 64 * MIB - 1, GIB, 8 * GIB, 64 * MIB, Kind::Either, Place::Card),
            (card_line - 64 * MIB, GIB, 8 * GIB, 64 * MIB, Kind::Either, Place::Shared),
            (card_line, GIB, 8 * GIB, 64 * MIB, Kind::Either, Place::Shared),
            (card_line, shared_line - 64 * MIB - 1, 8 * GIB, 64 * MIB, Kind::Either, Place::Shared),
            (card_line, shared_line - 64 * MIB, 8 * GIB, 64 * MIB, Kind::Either, Place::Refuse),
            (card_line, GIB, 1536 * MIB + 64 * MIB, 64 * MIB, Kind::Either, Place::Refuse),
            (card_line, GIB, 1536 * MIB + 64 * MIB + 1, 64 * MIB, Kind::Either, Place::Shared),
            // The PC's only: never the card's, whatever room it has.
            (0, GIB, 8 * GIB, 64 * MIB, Kind::Shared, Place::Shared),
            (0, 7 * GIB, 8 * GIB, 64 * MIB, Kind::Shared, Place::Refuse),
            (0, GIB, GIB, 64 * MIB, Kind::Shared, Place::Refuse),
            // The card's only: up to its whole budget, never the PC's.
            (card_line, GIB, 8 * GIB, 64 * MIB, Kind::Card, Place::Card),
            (7168 * MIB - 64 * MIB - 1, GIB, 8 * GIB, 64 * MIB, Kind::Card, Place::Card),
            (7168 * MIB - 64 * MIB, GIB, 8 * GIB, 64 * MIB, Kind::Card, Place::Refuse),
            // Out of memory: shared past its budget, 400 MB free.
            (5000 * MIB, 7600 * MIB, 400 * MIB, 4 * MIB, Kind::Either, Place::Card),
            (6400 * MIB, 7600 * MIB, 400 * MIB, 4 * MIB, Kind::Either, Place::Refuse),
            (5000 * MIB, 7600 * MIB, 400 * MIB, 4 * MIB, Kind::Shared, Place::Refuse),
        ];
        for (i, &(card, shared, ram, bytes, kind, place)) in table.iter().enumerate() {
            assert_eq!(place_in(&at(card, shared, ram, 8 * GIB), bytes, kind, &l), place, "row {i}");
        }
        // Nothing known: every kind where it prefers.
        let unknown = Sample::UNKNOWN;
        assert_eq!(place_in(&unknown, 1 << 40, Kind::Either, &l), Place::Card);
        assert_eq!(place_in(&unknown, 1 << 40, Kind::Shared, &l), Place::Shared);
        assert_eq!(place_in(&unknown, 1 << 40, Kind::Card, &l), Place::Card);
        // Another share of the PC's memory, as `VICTORY_BUDGET_SHARED_PCT` sets it.
        let wide = Lines { shared_pct: 100, ..l };
        assert_eq!(place_in(&at(card_line, 6 * GIB, 8 * GIB, 8 * GIB), 64 * MIB, Kind::Either, &wide), Place::Shared);
        assert_eq!(place_in(&at(card_line, 6 * GIB, 8 * GIB, 8 * GIB), 64 * MIB, Kind::Either, &l), Place::Refuse);
    }

    /// Green with room everywhere; yellow past one of the place lines; red
    /// with free memory or commit nearly gone, or a group past its budget.
    #[test]
    fn the_level_has_its_lines() {
        let l = lines();
        // (card used, shared used, free RAM, commit, level)
        let table = [
            (GIB, GIB, 8 * GIB, 8 * GIB, Level::Green),
            (GIB, 4300 * MIB, 8 * GIB, 8 * GIB, Level::Green),
            (GIB, 4500 * MIB, 8 * GIB, 8 * GIB, Level::Yellow),
            (6400 * MIB, GIB, 8 * GIB, 8 * GIB, Level::Yellow),
            (GIB, GIB, 1535 * MIB, 8 * GIB, Level::Yellow),
            (GIB, GIB, 511 * MIB, 8 * GIB, Level::Red),
            (GIB, GIB, 8 * GIB, 1535 * MIB, Level::Red),
            (GIB, 7360 * MIB, 8 * GIB, 8 * GIB, Level::Red),
            (7168 * MIB, GIB, 8 * GIB, 8 * GIB, Level::Red),
            // At its worst: shared past its budget, little free.
            (5000 * MIB, 7600 * MIB, 1000 * MIB, 2500 * MIB, Level::Red),
            (5000 * MIB, 7000 * MIB, 1000 * MIB, 2500 * MIB, Level::Yellow),
        ];
        for (i, &(card, shared, ram, commit, level)) in table.iter().enumerate() {
            assert_eq!(level_of(&at(card, shared, ram, commit), &l), level, "row {i}");
        }
        assert_eq!(level_of(&Sample::UNKNOWN, &l), Level::Green);
        assert!(Level::Green < Level::Yellow && Level::Yellow < Level::Red);
    }

    /// The steps go in their order, each given what is still wanted, and the
    /// rest are not called once enough was let go of.
    #[test]
    fn relief_goes_in_order_until_enough_is_let_go() {
        let calls = std::cell::RefCell::new(Vec::new());
        let step = |name: &'static str, gives: u64| {
            let calls = &calls;
            move |wanted: u64| {
                calls.borrow_mut().push((name, wanted));
                gives
            }
        };
        let (mut a, mut b, mut c, mut d, mut e) =
            (step("scratch", 3), step("sends", 0), step("staging", 4), step("windows", 5), step("textures", 9));
        let mut steps: [Step<'_>; 5] =
            [("scratch", &mut a), ("sends", &mut b), ("staging", &mut c), ("windows", &mut d), ("textures", &mut e)];
        let done = relieve_with(10, &mut steps);
        assert_eq!(*calls.borrow(), [("scratch", 10), ("sends", 7), ("staging", 7), ("windows", 3)]);
        assert_eq!(done, [("scratch", 3), ("sends", 0), ("staging", 4), ("windows", 5)]);
        // Enough at the first step: the others are not called.
        calls.borrow_mut().clear();
        let (mut a, mut b) = (step("scratch", 64), step("sends", 1));
        let done = relieve_with(8, &mut [("scratch", &mut a), ("sends", &mut b)]);
        assert_eq!(*calls.borrow(), [("scratch", 8)]);
        assert_eq!(done, [("scratch", 64)]);
        // Nothing to let go of anywhere: every step is asked, in order.
        calls.borrow_mut().clear();
        let (mut a, mut b, mut c) = (step("scratch", 0), step("sends", 0), step("staging", 0));
        let done = relieve_with(5, &mut [("scratch", &mut a), ("sends", &mut b), ("staging", &mut c)]);
        assert_eq!(*calls.borrow(), [("scratch", 5), ("sends", 5), ("staging", 5)]);
        assert_eq!(done.iter().map(|d| d.1).sum::<u64>(), 0);
    }

    /// The PC's memory is short under its free-memory or commit line or past
    /// its shared share, the card's past its own line; with free memory low
    /// and the card not full, the PC's alone.
    #[test]
    fn what_is_short_is_the_memory_past_its_line() {
        let l = lines();
        let card_line = (7168 * MIB * 88).div_ceil(100);
        // (card used, shared used, free RAM, commit, PC short, card short)
        let table = [
            (GIB, GIB, 8 * GIB, 8 * GIB, false, false),
            (GIB, GIB, 1535 * MIB, 8 * GIB, true, false),
            (GIB, GIB, 8 * GIB, 1535 * MIB, true, false),
            (GIB, 4500 * MIB, 8 * GIB, 8 * GIB, true, false),
            (card_line, GIB, 8 * GIB, 8 * GIB, false, true),
            (card_line - 1, GIB, 8 * GIB, 8 * GIB, false, false),
            (card_line, GIB, 400 * MIB, 8 * GIB, true, true),
            // Reds (card, shared, free, commit).
            (2500 * MIB, 1700 * MIB, 400 * MIB, 14000 * MIB, true, false),
            (4700 * MIB, 5000 * MIB, 400 * MIB, 7600 * MIB, true, false),
            (4900 * MIB, 5700 * MIB, 300 * MIB, 6800 * MIB, true, false),
        ];
        for (i, &(card, shared, ram, commit, pc, on_card)) in table.iter().enumerate() {
            assert_eq!(short_of(&at(card, shared, ram, commit), &l), Short { pc, card: on_card }, "row {i}");
        }
        assert_eq!(short_of(&Sample::UNKNOWN, &l), Short::default());
    }

    /// With the PC's memory short and the card's not, the card's step is not
    /// taken; with the card's short alone it comes first; otherwise, and with
    /// the switch off, every step in its order.
    #[test]
    fn relief_frees_what_the_shortage_is_of() {
        use Frees::{Card, Pc};
        let frees = [Pc, Pc, Pc, Pc, Card];
        let pc = Short { pc: true, card: false };
        let card = Short { pc: false, card: true };
        let both = Short { pc: true, card: true };
        assert_eq!(chosen(&frees, pc, true), [0, 1, 2, 3]);
        assert_eq!(chosen(&frees, card, true), [4, 0, 1, 2, 3]);
        for short in [both, Short::default()] {
            assert_eq!(chosen(&frees, short, true), [0, 1, 2, 3, 4], "{short:?}");
        }
        for short in [pc, card, both, Short::default()] {
            assert_eq!(chosen(&frees, short, false), [0, 1, 2, 3, 4], "{short:?}");
        }
        // The steps taken run as `relieve_with` runs them: the card's blocks are
        // never asked for a shortage of the PC's memory.
        let asked = std::cell::RefCell::new(Vec::new());
        let step = |name: &'static str| {
            let asked = &asked;
            move |wanted: u64| {
                asked.borrow_mut().push(name);
                wanted / 8
            }
        };
        let (mut a, mut b, mut c, mut d, mut e) =
            (step("scratch"), step("sends"), step("staging"), step("windows"), step("textures"));
        let all: [(&'static str, &mut dyn FnMut(u64) -> u64); 5] =
            [("scratch", &mut a), ("sends", &mut b), ("staging", &mut c), ("windows", &mut d), ("textures", &mut e)];
        let mut slots = all.map(Some);
        let mut steps: Vec<Step<'_>> = chosen(&frees, pc, true).iter().filter_map(|&i| slots[i].take()).collect();
        relieve_with(1 << 30, &mut steps);
        assert_eq!(*asked.borrow(), ["scratch", "sends", "staging", "windows"]);
    }

    /// At red on the PC's memory the governor lets go of what brings it back
    /// to its lines, 64 MiB at least; not at yellow, not for the card's red,
    /// and not when nothing is known.
    #[test]
    fn at_red_the_pcs_idle_memory_goes_by_itself() {
        let l = lines();
        // Of the shared memory, what is past 60 % of its budget.
        let over = |used: u64| used - 7360 * MIB / 100 * 60;
        // (card used, shared used, free RAM, commit, bytes let go of)
        let table = [
            (GIB, GIB, 8 * GIB, 8 * GIB, 0),
            // Yellow on free memory: nothing yet.
            (GIB, GIB, 700 * MIB, 8 * GIB, 0),
            // The card at its budget: the card's, not the PC's.
            (7168 * MIB, GIB, 8 * GIB, 8 * GIB, 0),
            // A red: under 512 MB free.
            (2500 * MIB, 1700 * MIB, 450 * MIB, 14000 * MIB, (1536 - 450) * MIB),
            (4900 * MIB, 4000 * MIB, 300 * MIB, 6800 * MIB, (1536 - 300) * MIB),
            // The same with the shared memory past its share by a little more.
            (4900 * MIB, 5700 * MIB, 300 * MIB, 6800 * MIB, over(5700 * MIB)),
            // Commit nearly gone, free memory fine.
            (GIB, GIB, 4 * GIB, 1500 * MIB, 64 * MIB),
            (GIB, GIB, 4 * GIB, 1000 * MIB, 536 * MIB),
            // The shared memory at its budget: down to 60 % of it.
            (GIB, 7360 * MIB, 4 * GIB, 8 * GIB, over(7360 * MIB)),
        ];
        for (i, &(card, shared, ram, commit, bytes)) in table.iter().enumerate() {
            assert_eq!(tend_need(&at(card, shared, ram, commit), &l), bytes, "row {i}");
        }
        assert_eq!(tend_need(&Sample::UNKNOWN, &l), 0);
    }

    /// The card is relieved from 95 % of its budget on and until it is back
    /// under 90 %, by what brings it to 90 %; on cards of 8, 12, 16 and 24 GB
    /// alike (the lines are shares of the budget the system gives), and never
    /// when the budget is not known.
    #[test]
    fn the_card_is_relieved_from_95_until_under_90_percent() {
        for budget in [6935 * MIB, 7168 * MIB, 11 * GIB, 15 * GIB, 23 * GIB] {
            let at = |pct: u64| budget / 100 * pct;
            let line = budget * 90 / 100;
            // (was relieved, use, relieved now)
            let table = [
                (false, at(50), false),
                (false, at(94), false),
                (false, at(95) + MIB, true),
                (false, at(118), true),
                (true, at(94), true),
                (true, at(91), true),
                (true, line - 1, false),
                (true, at(50), false),
            ];
            for (i, &(was, used, now)) in table.iter().enumerate() {
                let (on, need) = relief_next(was, [budget, used]);
                assert_eq!(on, now, "budget {} MB row {i}", budget / MIB);
                if on {
                    assert_eq!(need, used - line, "budget {} MB row {i}", budget / MIB);
                } else {
                    assert_eq!(need, 0);
                }
            }
        }
        // The run that showed it: 8,183 MB in use of a budget of 6,935.
        let (on, need) = relief_next(false, [6935 * MIB, 8183 * MIB]);
        assert!(on);
        assert_eq!(need / MIB, 8183 - 6935 * 90 / 100 - 1);
        assert_eq!(relief_next(false, [0, 8 * GIB]), (false, 0));
        assert_eq!(relief_next(true, [0, 8 * GIB]), (false, 0));
    }

    /// An allocation that may lie in either memory goes to the card under the
    /// caller's line, whatever the PC's memory, and always when nothing is
    /// known.
    #[test]
    fn either_way_allocations_go_to_the_card_while_it_has_room() {
        let l = Lines { card_pct: 97, shared_pct: 100, free_ram: 0 };
        let s = |used: u64| at(used, 7 * GIB, 100 * MIB, 8 * GIB);
        let line = 7168 * MIB / 100 * 97;
        assert!(takes_in(&s(GIB), 64 * MIB, &l));
        assert!(takes_in(&s(line - 64 * MIB - 1), 64 * MIB, &l));
        assert!(!takes_in(&s(line), 64 * MIB, &l));
        assert!(!takes_in(&s(8 * GIB), 4 * MIB, &l));
        // The line of 80 hundredths, with copies' room not kept.
        let low = Lines { card_pct: 80, ..l };
        assert!(!takes_in(&s(7168 * MIB / 100 * 80), 4 * MIB, &low));
        assert!(takes_in(&Sample::UNKNOWN, 1 << 40, &l));
        assert!(card_has_room(None, 1 << 40, 97));
        assert!(card_has_room(Some([7168 * MIB, GIB, 7 * GIB, GIB]), 64 * MIB, 97));
        assert!(!card_has_room(Some([7168 * MIB, 7 * GIB, 7 * GIB, GIB]), 64 * MIB, 97));
    }

    /// The words of a sample are the sample.
    #[test]
    fn a_sample_is_kept_as_words() {
        let s = at(1, 2, 3, 4);
        assert_eq!(Sample::of(s.words()), s);
        assert_eq!(Sample::of(Sample::UNKNOWN.words()), Sample::UNKNOWN);
    }
}
