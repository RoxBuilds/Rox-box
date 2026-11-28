//! Blocks the copies of buffer views' own bytes are cut from
//! (`super::exact`, `VICTORY_VIEW_BYTES_POOL`).
//!
//! Each such copy was a buffer of its own: the title puts its parameter
//! blocks at ring positions that move every frame, so hundreds of these
//! copies came and went a frame, each a buffer of the PC's memory the card
//! shares, made resident and waited for until the card had it mapped (a run
//! of a title made tens of thousands in a few minutes, a minute and more of
//! waiting summed over its threads), and each took 64 KiB of memory for a
//! few hundred bytes. Cut from a few blocks of 1 MiB, a copy is a range of a
//! block: no new buffer, nothing made resident, nothing waited for.
//!
//! A range lies at a multiple of [`GRAIN`] (768 bytes: of 16, where raw views
//! begin, of 256, of every element a typed view has, 12 bytes among them), so
//! that every view the copy's own buffer took at its start takes it there
//! the same way. Its bytes past the copy's are zeros. The last [`GUARD`]
//! bytes of a block are never handed out: a shader that reads past its view
//! (a raw load through a typed view has no end on some cards) finds the block
//! there, as it found the rest of a buffer of 64 KiB before, and no page that
//! is not mapped.
//!
//! A range goes back when its copy goes: when nothing holds the copy, as a
//! buffer of its own was destroyed (no list, no slot, no slot's old view;
//! `HostHeap::keep` holds such copies, `descriptor::host::Held`). It is handed
//! out again only once every submission begun or queued then has finished,
//! and the lag `crate::host::retire` keeps after that. A block with nothing
//! in it for [`EMPTY_MS`] is let go, past the first such one.

use std::collections::BTreeMap;

/// The bytes of a block.
pub(super) const BLOCK: u64 = 1 << 20;
/// The bytes at a block's end that no range takes.
pub(super) const GUARD: u64 = 64 << 10;
/// Ranges begin at multiples of this and are as long as a multiple of it.
pub(super) const GRAIN: u64 = 768;
/// The longest copy cut from a block; a longer one is a buffer of its own.
pub(super) const MOST: u64 = 64 << 10;
/// How long a block has had nothing in it before it is let go (ms).
pub(super) const EMPTY_MS: u64 = 2000;
/// Empty blocks kept however long they are empty.
const SPARE: usize = 1;

/// A copy's range of a block: the block's place, where the range begins and
/// its bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Cut {
    pub(super) block: usize,
    pub(super) start: u64,
    pub(super) len: u64,
}

/// The free ranges of a block: start and length, never two that touch.
struct Ranges(BTreeMap<u64, u64>);

impl Ranges {
    fn new(size: u64) -> Ranges {
        Ranges(BTreeMap::from([(0, size)]))
    }

    /// The lowest place for `len` bytes (all lengths and starts are
    /// multiples of [`GRAIN`]).
    fn take(&mut self, len: u64) -> Option<u64> {
        let (&start, &size) = self.0.iter().find(|&(_, &size)| size >= len)?;
        self.0.remove(&start);
        if size > len {
            self.0.insert(start + len, size - len);
        }
        Some(start)
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
}

struct Block<B> {
    memory: B,
    free: Ranges,
    /// The bytes of its ranges handed out or waiting to come back.
    held: u64,
    /// Since when (ms) nothing is in it; `None` while something is.
    empty_since: Option<u64>,
}

/// The blocks, and the ranges given back that wait for their submissions.
/// `B` is a block's memory (tests: a number).
pub(super) struct Blocks<B> {
    blocks: Vec<Option<Block<B>>>,
    /// Ranges given back, by the submission number each waits for
    /// (`crate::host::retire_mark` when it was given back; copies go on
    /// several threads, so not always in its order).
    waiting: BTreeMap<u64, Vec<Cut>>,
}

impl<B: Clone> Blocks<B> {
    pub(super) const fn new() -> Blocks<B> {
        Blocks { blocks: Vec::new(), waiting: BTreeMap::new() }
    }

    /// The bytes of the range a copy of `len` bytes takes; `None` if it is
    /// not cut from a block.
    pub(super) fn size_of(len: u64) -> Option<u64> {
        (len > 0 && len <= MOST).then(|| len.next_multiple_of(GRAIN))
    }

    /// A range for a copy of `len` bytes and the memory of its block, from
    /// the lowest block with room; `None` if none has room (the caller makes
    /// a block, [`Blocks::add`]) or the copy is too long. Ranges given back
    /// whose submissions have `landed` are free again first.
    pub(super) fn take(&mut self, len: u64, now: u64, landed: impl Fn(u64) -> bool) -> Option<(Cut, B)> {
        let size = Self::size_of(len)?;
        self.settle(now, landed);
        self.blocks.iter_mut().enumerate().find_map(|(i, b)| {
            let b = b.as_mut()?;
            let start = b.free.take(size)?;
            b.held += size;
            b.empty_since = None;
            Some((Cut { block: i, start, len: size }, b.memory.clone()))
        })
    }

    /// A new block (`memory`, [`BLOCK`] bytes), empty since `now`.
    pub(super) fn add(&mut self, memory: B, now: u64) {
        let block = Block { memory, free: Ranges::new(BLOCK - GUARD), held: 0, empty_since: Some(now) };
        match self.blocks.iter().position(Option::is_none) {
            Some(i) => self.blocks[i] = Some(block),
            None => self.blocks.push(Some(block)),
        }
    }

    /// `cut` comes back, to be free once submissions up to `mark` have
    /// landed.
    pub(super) fn give(&mut self, cut: Cut, mark: u64) {
        self.waiting.entry(mark).or_default().push(cut);
    }

    /// The ranges given back whose submissions have `landed` are free; a
    /// block that holds nothing then is empty from `now`.
    pub(super) fn settle(&mut self, now: u64, landed: impl Fn(u64) -> bool) {
        // (What has landed at a number has at every lower one.)
        while let Some(entry) = self.waiting.first_entry()
            && landed(*entry.key())
        {
            for cut in entry.remove() {
                if let Some(b) = self.blocks.get_mut(cut.block).and_then(Option::as_mut) {
                    b.free.give(cut.start, cut.len);
                    b.held -= cut.len;
                    if b.held == 0 {
                        b.empty_since = Some(now);
                    }
                }
            }
        }
    }

    /// The blocks empty for [`EMPTY_MS`] at `now`, past the first [`SPARE`]
    /// empty ones (the lowest, which ranges are taken from first), taken out:
    /// their memory, for the caller to let go of.
    pub(super) fn let_go(&mut self, now: u64) -> Vec<B> {
        let mut spare = 0;
        let mut gone = Vec::new();
        for place in &mut self.blocks {
            let Some(b) = place else { continue };
            let Some(since) = b.empty_since else { continue };
            spare += 1;
            if spare > SPARE && now >= since + EMPTY_MS
                && let Some(b) = place.take()
            {
                gone.push(b.memory);
            }
        }
        gone
    }

    /// Blocks there are.
    pub(super) fn count(&self) -> usize {
        self.blocks.iter().flatten().count()
    }

    /// Whether the place `start` of the block whose memory is `which` is
    /// free.
    #[cfg(test)]
    pub(super) fn is_free(&self, which: impl Fn(&B) -> bool, start: u64) -> bool {
        self.blocks.iter().flatten().filter(|b| which(&b.memory)).any(|b| {
            b.free.0.range(..=start).next_back().is_some_and(|(&at, &len)| start < at + len)
        })
    }

    /// The bytes of the ranges handed out or waiting to come back.
    #[cfg(test)]
    fn held(&self) -> u64 {
        self.blocks.iter().flatten().map(|b| b.held).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::{BLOCK, Blocks, Cut, EMPTY_MS, GRAIN, GUARD, MOST};

    fn all(_: u64) -> bool {
        true
    }

    /// Ranges are cut on multiples of the grain, which every view the copy
    /// took at a buffer's start takes the same way: raw views (16), typed
    /// ones of 1, 2, 4, 8, 12 and 16 bytes, constant views (256).
    #[test]
    fn ranges_begin_where_every_view_does() {
        for unit in [4, 16, 256, 1, 2, 8, 12] {
            assert!(GRAIN.is_multiple_of(unit), "{unit}");
        }
        assert_eq!(Blocks::<u32>::size_of(0), None);
        assert_eq!(Blocks::<u32>::size_of(1), Some(GRAIN));
        assert_eq!(Blocks::<u32>::size_of(GRAIN + 1), Some(2 * GRAIN));
        assert_eq!(Blocks::<u32>::size_of(MOST), Some(MOST.next_multiple_of(GRAIN)));
        assert_eq!(Blocks::<u32>::size_of(MOST + 1), None, "a longer copy is a buffer of its own");
        assert!(MOST.next_multiple_of(GRAIN) <= BLOCK - GUARD);
    }

    /// Many copies lie in one block, one after another; a block is made only
    /// when none has room, and the guard at its end is never handed out.
    #[test]
    fn many_copies_lie_in_one_block() {
        let mut b = Blocks::<u32>::new();
        assert!(b.take(128, 0, all).is_none(), "no block yet");
        b.add(7, 0);
        let fits = (BLOCK - GUARD) / GRAIN;
        let cuts: Vec<Cut> = (0..fits).map(|_| b.take(128, 0, all).expect("room").0).collect();
        assert_eq!(b.count(), 1);
        for (i, c) in cuts.iter().enumerate() {
            assert_eq!((c.block, c.start, c.len), (0, i as u64 * GRAIN, GRAIN));
            assert!(c.start + c.len <= BLOCK - GUARD);
        }
        assert!(b.take(128, 0, all).is_none(), "full: the caller makes another block");
        b.add(8, 0);
        let (c, memory) = b.take(128, 0, all).unwrap();
        assert_eq!((c.block, c.start, memory), (1, 0, 8));
    }

    /// A range given back is free only once its submissions have landed, and
    /// then the next copy takes it (the lowest place first); neighbours given
    /// back join, so a longer copy fits where two shorter ones were.
    #[test]
    fn ranges_given_back_come_again_after_their_submissions() {
        let mut b = Blocks::<u32>::new();
        b.add(1, 0);
        let a = b.take(100, 0, all).unwrap().0;
        let c = b.take(100, 0, all).unwrap().0;
        let d = b.take(100, 0, all).unwrap().0;
        assert_eq!((a.start, c.start, d.start), (0, GRAIN, 2 * GRAIN));
        b.give(a, 10);
        b.give(c, 11);
        let landed_to = |n: u64| move |mark: u64| mark <= n;
        // Nothing landed: a new place.
        assert_eq!(b.take(100, 0, landed_to(9)).unwrap().0.start, 3 * GRAIN);
        // 10 landed: `a`'s place, not yet `c`'s.
        assert_eq!(b.take(100, 0, landed_to(10)).unwrap().0.start, 0);
        assert_eq!(b.take(100, 0, landed_to(10)).unwrap().0.start, 4 * GRAIN);
        // 11 landed; `c`'s place joins nothing (both neighbours are out):
        // two grains do not fit there, one does.
        assert_eq!(b.take(GRAIN + 1, 0, landed_to(11)).unwrap().0.start, 5 * GRAIN);
        assert_eq!(b.take(GRAIN, 0, landed_to(11)).unwrap().0.start, GRAIN);
        assert_eq!(b.held(), 7 * GRAIN);
    }

    /// A block with nothing in it goes once it has been empty for its time,
    /// past the first empty one, which stays; one that holds a range, or a
    /// range still waiting for its submissions, stays.
    #[test]
    fn an_emptied_block_is_let_go_after_a_while() {
        let mut b = Blocks::<u32>::new();
        b.add(1, 0);
        b.add(2, 0);
        let fits = (BLOCK - GUARD) / GRAIN;
        let first: Vec<Cut> = (0..fits).map(|_| b.take(GRAIN, 0, all).unwrap().0).collect();
        let x = b.take(GRAIN, 0, all).unwrap().0;
        assert_eq!((first[0].block, x.block), (0, 1));
        // The second block's one range comes back, waiting for 50.
        b.give(x, 50);
        b.settle(100, |m| m <= 5);
        assert!(b.let_go(100 + 100 * EMPTY_MS).is_empty(), "a range waits for its submissions");
        b.settle(200, |m| m <= 50);
        assert!(b.let_go(200 + 100 * EMPTY_MS).is_empty(), "the one empty block stays however long");
        // The first block empties too: it is the one kept (the lowest), and
        // the second goes once it has been empty for its time.
        for c in first {
            b.give(c, 60);
        }
        b.settle(300, all);
        assert!(b.let_go(200 + EMPTY_MS - 1).is_empty(), "not before its time");
        assert_eq!(b.let_go(200 + EMPTY_MS), [2], "empty for its time: let go");
        assert_eq!(b.count(), 1);
        assert_eq!(b.held(), 0);
        // A new block takes the place of the one let go; ranges come from
        // the lowest block with room first.
        b.add(3, 400);
        assert_eq!(b.take(GRAIN, 400, all).map(|(c, m)| (c.block, m)), Some((0, 1)));
        for _ in 1..fits {
            b.take(GRAIN, 400, all).unwrap();
        }
        assert_eq!(b.take(GRAIN, 400, all).map(|(c, m)| (c.block, m)), Some((1, 3)));
    }
}
