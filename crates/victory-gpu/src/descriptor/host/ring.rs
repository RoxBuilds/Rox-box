//! The books of a view heap's dynamic slots ([`super::HostHeap::dynamic_as`]):
//! which are taken, where the search starts, and which recordings wait for
//! slots and in what order; and the counts the run report shows of them.
//!
//! The slots are a bit each, searched a word at a time: the search went
//! through a `bool` a slot, 262,144 of them on a host heap of 1,000,000 views
//! (NVIDIA), under the lock every table of every recording thread needs, and
//! in a scene that ran the ring out that took up to a quarter of a second of
//! those threads a frame. It finds what that search found (the first run
//! from the cursor on, then the first before it).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Whether tables that find no free run wait for slots in the order their
/// recordings began to take slots, for as long as slots keep coming back,
/// and whether a flight's tables give their slots back as soon as the card
/// has run it (`VICTORY_SLOTS_PACED`, on unless `0`, read once; `0`: a table
/// waits at most `VICTORY_DYNAMIC_WAIT_MS` in all, whoever asks, and after
/// one such wait every table is refused until slots come back; a flight's
/// slots go back after its landing's copies, as before).
///
/// Every table refused so is a draw left out: geometry missing from that
/// frame, near and far. On a host heap capped at 1,000,000 views (NVIDIA)
/// the ring is 2^18 slots, the title's own slots taking the rest; a list's
/// slots are held from its translation until its flight lands, through the
/// line of submissions (15 deep on average, up to 51 when the submitting
/// thread fell behind) and the landing (up to 105 ms a frame). In the slow
/// stretches of a run the 50 ms wait ran out before the next landing, and
/// every table after it was refused at once until slots came back (23,888
/// and 53,138 draws left out in two runs), while slots came back within the
/// next landings. Waiting while they do drops no draw as long as the line
/// moves; the oldest recording is served first, as the line sends it first;
/// and a table is refused only after [`STALL`] with no slot given back at
/// all.
pub(crate) fn paced() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SLOTS_PACED").is_none_or(|v| v != "0"))
}

/// How long a table waits with no slot given back at all before it is
/// refused ([`paced`]): the line of flights has stopped (what holds the slots
/// waits for this table's own list), and only a refusal lets it go on.
pub(super) const STALL: std::time::Duration =
    std::time::Duration::from_millis(if cfg!(test) { 200 } else { 1000 });

/// A recording's place in the order tables are served in when the ring is
/// short ([`Ring::turn`]): taken when it first asks for slots. 0 is first of
/// all (slots a heap keeps for its life).
pub(crate) fn next_order() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Relaxed)
}

/// The first `n` free slots in a row among `taken` (a bit a slot, set if
/// taken) from `from` up to `to` (not past it): the first one's index.
pub(super) fn free_run(taken: &[u64], from: usize, to: usize, n: usize) -> Option<usize> {
    if n == 0 {
        return None;
    }
    let mut start = from;
    let mut at = from;
    while at < to {
        let word = taken[at / 64] >> (at % 64);
        // (The bits shifted in from above are free; `room` keeps to this word.)
        let room = (64 - at % 64).min(to - at);
        if word & 1 == 0 {
            at += (word.trailing_zeros() as usize).min(room);
            if at - start >= n {
                return Some(start);
            }
        } else {
            at += ((!word).trailing_zeros() as usize).min(room);
            start = at;
        }
    }
    None
}

/// The dynamic slots of a heap: a bit each (set: taken by a list), where the
/// next search starts, and the recordings whose tables wait for slots.
pub(super) struct Ring {
    next: u32,
    len: u32,
    taken: Vec<u64>,
    /// By recording ([`next_order`]): how many of its tables wait now.
    waiting: BTreeMap<u64, u32>,
}

impl Ring {
    pub(super) fn new(len: u32) -> Ring {
        Ring {
            next: 0,
            len,
            taken: vec![0; (len as usize).div_ceil(64)],
            waiting: BTreeMap::new(),
        }
    }

    /// The first of `n` free slots in a row: from the cursor on, then from
    /// the start (a run may reach past the cursor), never across the end.
    pub(super) fn find(&self, n: u32) -> Option<u32> {
        let (n, len, cursor) = (n as usize, self.len as usize, self.next as usize);
        if n == 0 || n > len {
            return None;
        }
        free_run(&self.taken, cursor, len, n)
            .or_else(|| free_run(&self.taken, 0, cursor.saturating_add(n - 1).min(len), n))
            .map(|first| first as u32)
    }

    /// Takes `n` slots from `first` (a run [`Ring::find`] found); the cursor
    /// goes past them.
    pub(super) fn take(&mut self, first: u32, n: u32) {
        self.mark(first, n, true);
        self.next = (first + n) % self.len.max(1);
    }

    /// Gives back the slots from `first` to `end`: how many of them were
    /// taken.
    pub(super) fn give(&mut self, first: u32, end: u32) -> u32 {
        let end = end.min(self.len);
        if first >= end {
            return 0;
        }
        let taken = (first..end).filter(|&i| self.is_taken(i)).count() as u32;
        self.mark(first, end - first, false);
        taken
    }

    pub(super) fn is_taken(&self, index: u32) -> bool {
        self.taken
            .get(index as usize / 64)
            .is_some_and(|w| w >> (index % 64) & 1 == 1)
    }

    fn mark(&mut self, first: u32, n: u32, taken: bool) {
        let (mut at, end) = (first as usize, (first + n).min(self.len) as usize);
        while at < end {
            let bits = (64 - at % 64).min(end - at);
            let mask = if bits == 64 {
                u64::MAX
            } else {
                ((1u64 << bits) - 1) << (at % 64)
            };
            let word = &mut self.taken[at / 64];
            *word = if taken { *word | mask } else { *word & !mask };
            at += bits;
        }
    }

    /// Whether a table of the recording `order` may search now: no table of
    /// an older recording waits (slots that come back go to the oldest
    /// first, as the line of submissions sends the oldest first).
    pub(super) fn turn(&self, order: u64) -> bool {
        self.waiting
            .first_key_value()
            .is_none_or(|(&oldest, _)| oldest >= order)
    }

    /// A table of `order` waits.
    pub(super) fn wait_in(&mut self, order: u64) {
        *self.waiting.entry(order).or_default() += 1;
    }

    /// It waits no more: whether other tables still wait (and may have their
    /// turn now).
    pub(super) fn wait_out(&mut self, order: u64) -> bool {
        if let Some(n) = self.waiting.get_mut(&order) {
            *n -= 1;
            if *n == 0 {
                self.waiting.remove(&order);
            }
        }
        !self.waiting.is_empty()
    }

    /// Whether a table of `order` waits now.
    #[cfg(test)]
    pub(super) fn waits(&self, order: u64) -> bool {
        self.waiting.contains_key(&order)
    }
}

/// What the run report's `[gpu_work.descriptor_slots]` says
/// (`victory_core::report::DESCRIPTOR_SLOT_KEYS`, in that order).
#[derive(Clone, Copy)]
pub(crate) enum Count {
    /// The dynamic slots of the largest view heap, its title slots with a
    /// host slot, and the title's own count.
    Pool,
    TitleSlots,
    TitleHeap,
    /// The highest title slot index seen, plus one (0: none).
    HighestTitleSlot,
    /// Runs of dynamic slots taken, the slots in them, the longest.
    Tables,
    Slots,
    Longest,
    /// The fewest free after a take (`u64::MAX`: no take yet).
    FewestFree,
    /// Tables that waited, for how long in all and at most (microseconds).
    Waits,
    WaitedUs,
    LongestWaitUs,
    /// Tables refused: after waiting with no slot given back for
    /// [`STALL`], and at once (a thread that gives slots back, or no wait
    /// asked for, or the ring stalled with nothing given back since).
    RefusedStalled,
    RefusedAtOnce,
    /// Lists whose slots went back when the card had run them, before their
    /// flight's landing.
    BackWhenRun,
}

pub(crate) const COUNTS: usize = Count::BackWhenRun as usize + 1;

static COUNTED: [AtomicU64; COUNTS] = [
    const { AtomicU64::new(0) },
    const { AtomicU64::new(0) },
    const { AtomicU64::new(0) },
    const { AtomicU64::new(0) },
    const { AtomicU64::new(0) },
    const { AtomicU64::new(0) },
    const { AtomicU64::new(0) },
    AtomicU64::new(u64::MAX),
    const { AtomicU64::new(0) },
    const { AtomicU64::new(0) },
    const { AtomicU64::new(0) },
    const { AtomicU64::new(0) },
    const { AtomicU64::new(0) },
    const { AtomicU64::new(0) },
];

pub(crate) fn add(count: Count, n: u64) {
    COUNTED[count as usize].fetch_add(n, Relaxed);
}

/// `count` is at least `n`. (A load first: it is asked at every slot the
/// title's heap is mirrored at, from many threads, and seldom changes.)
pub(crate) fn most(count: Count, n: u64) {
    let at = &COUNTED[count as usize];
    if at.load(Relaxed) < n {
        at.fetch_max(n, Relaxed);
    }
}

/// `count` is at most `n`.
pub(crate) fn least(count: Count, n: u64) {
    let at = &COUNTED[count as usize];
    if at.load(Relaxed) > n {
        at.fetch_min(n, Relaxed);
    }
}

/// The counts for the run report: the fewest free is the pool's size while
/// nothing was taken.
pub(crate) fn counts() -> [u64; COUNTS] {
    let mut all: [u64; COUNTS] = std::array::from_fn(|i| COUNTED[i].load(Relaxed));
    all[Count::FewestFree as usize] =
        fewest_free(all[Count::FewestFree as usize], all[Count::Pool as usize]);
    all
}

/// [`counts`] as the run report writes them
/// (`victory_core::report::DESCRIPTOR_SLOT_KEYS`): the highest title slot as
/// the slots up to it, times in milliseconds.
pub(crate) fn report() -> [u64; COUNTS] {
    let mut all = counts();
    for us in [Count::WaitedUs, Count::LongestWaitUs] {
        all[us as usize] /= 1000;
    }
    all
}

/// The fewest free slots seen (`u64::MAX`: no take yet), or the pool's size.
fn fewest_free(fewest: u64, pool: u64) -> u64 {
    if fewest == u64::MAX { pool } else { fewest }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The search a `bool` a slot made, as [`Ring::find`] must match it.
    fn find_by_bools(taken: &[bool], cursor: usize, n: usize) -> Option<usize> {
        let end = taken.len();
        let second_end = cursor.saturating_add(n.saturating_sub(1)).min(end);
        let (mut run, mut last) = (0, usize::MAX);
        for at in (cursor..end).chain(0..second_end) {
            if at != last.wrapping_add(1) {
                run = 0;
            }
            last = at;
            run = if taken[at] { 0 } else { run + 1 };
            if run == n {
                return Some(at + 1 - n);
            }
        }
        None
    }

    #[test]
    fn the_search_finds_what_the_search_by_slot_found() {
        // A small generator: the same patterns on every run.
        let mut seed = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for len in [1u32, 63, 64, 65, 200, 1000] {
            for _ in 0..300 {
                let mut ring = Ring::new(len);
                let mut bools = vec![false; len as usize];
                // Runs taken and given back at random, some long.
                let density = next() % 4;
                for i in 0..len as usize {
                    if next() % 4 < density {
                        bools[i] = true;
                    }
                }
                for (i, &t) in bools.iter().enumerate() {
                    if t {
                        ring.mark(i as u32, 1, true);
                    }
                }
                ring.next = (next() % u64::from(len)) as u32;
                for n in [1u32, 2, 3, 7, 63, 64, 65, 130] {
                    assert_eq!(
                        ring.find(n).map(|f| f as usize),
                        find_by_bools(&bools, ring.next as usize, n as usize),
                        "len {len} cursor {} n {n}",
                        ring.next
                    );
                }
            }
        }
    }

    #[test]
    fn taken_runs_are_held_until_given_back() {
        let mut ring = Ring::new(300);
        let a = ring.find(100).unwrap();
        ring.take(a, 100);
        let b = ring.find(150).unwrap();
        ring.take(b, 150);
        assert_eq!((a, b), (0, 100));
        assert_eq!(ring.find(51), None, "50 left at the end");
        assert_eq!(ring.find(50), Some(250));
        assert_eq!(ring.give(a, a + 100), 100);
        assert_eq!(ring.give(a, a + 100), 0, "given back once");
        assert_eq!(ring.find(50), Some(250), "a run from the cursor first");
        assert_eq!(ring.find(51), Some(0), "then from the start");
        assert!(ring.is_taken(100) && ring.is_taken(249));
        assert!(
            !ring.is_taken(0) && !ring.is_taken(250) && !ring.is_taken(299) && !ring.is_taken(300)
        );
    }

    /// The oldest recording that waits is served first; one older than it
    /// (or one that waits itself, or with none waiting) may search.
    #[test]
    fn slots_go_to_the_oldest_waiting_recording_first() {
        let mut ring = Ring::new(64);
        assert!(ring.turn(7));
        ring.wait_in(5);
        ring.wait_in(9);
        ring.wait_in(9);
        assert!(ring.turn(5) && ring.turn(3));
        assert!(!ring.turn(9) && !ring.turn(6));
        assert!(ring.wait_out(5), "two of 9 still wait");
        assert!(ring.turn(9));
        assert!(ring.wait_out(9));
        assert!(!ring.wait_out(9), "none waits now");
        assert!(ring.turn(u64::MAX));
    }

    #[test]
    fn the_fewest_free_is_the_pool_until_something_is_taken() {
        assert_eq!(fewest_free(u64::MAX, 262_144), 262_144);
        assert_eq!(fewest_free(17, 262_144), 17);
        assert_eq!(fewest_free(0, 262_144), 0);
    }
}
