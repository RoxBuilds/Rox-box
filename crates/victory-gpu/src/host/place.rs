//! The title's buffer copies, done on the card at their place in the list
//! (`VICTORY_COPIES_IN_PLACE`, on by default; `0`: all at submission, on the
//! CPU, as before).
//!
//! A buffer copy was done in title memory when its list was submitted, before
//! any draw of the submission ran, and the host copies refreshed after it.
//! That is another order than the list's: draws ahead of the copy read what
//! it put; of two copies to the same bytes with a draw behind each, both
//! draws read the second; a source that a dispatch of the same submission
//! writes first was read as it was before it; and a destination the
//! submissions in the air still read changed under them (where its host copy
//! has one side only, `crate::shadow::card_all`).
//!
//! Now the list copies, in the destination's host copy: out of the source's
//! host copy where shaders write that one, else out of upload memory of the
//! list that takes the source's bytes when the list is submitted (a source
//! shaders never wrote is the title's CPU's, or a copy's put there earlier in
//! the submission). What is written there is a shader's write from then on:
//! the bytes are declared, and what the card made of them is put into title
//! memory before the title is told of the submission.
//!
//! Title memory takes the copy at submission as well, as it did: fills of
//! upload memory, textures and host copies that lie over the same bytes read
//! it there. The reference of the destination's host copy takes the same
//! bytes ([`Shadow::placed_copy`]), so what the landing finds is what the card
//! made of them afterwards (a counter copied to zero and counted up; a source
//! that a shader wrote in between), and nothing is sent up over it. Where
//! flights in the air may write the source or the destination this is left to
//! the landing: what they wrote is not in title memory yet, and what they
//! will put there must not stand over this copy's bytes. If a step of the
//! submission reads the destination on the CPU, it waits for them instead
//! ([`Placed::waits`]).
//!
//! Copies to memory that has no host copy yet are done as before: nothing on
//! the card has read or written it. So are those of more than 8 MiB out of
//! memory shaders do not write (streaming).

use super::*;
use crate::shadow::Shadow;

pub(super) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_COPIES_IN_PLACE").is_none_or(|v| v != "0"))
}

/// The most bytes copied out of memory shaders do not write.
const MOST: usize = 8 << 20;

/// Since the last frame-rate line: copies at their place, their bytes, those
/// out of a host copy, and texture copies out of one.
pub(super) static STATS: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// What the CPU does at submission for a copy at its place in a list.
pub(super) struct Placed {
    pub(super) dst: usize,
    pub(super) src: usize,
    pub(super) len: usize,
    /// The destination's host copy.
    pub(super) shadow: std::sync::Arc<Shadow>,
    /// Upload memory the list copies from, to be given the source's bytes.
    from: Option<usize>,
}

impl Placed {
    /// Whether the source's bytes are taken on the CPU at submission (into
    /// upload memory the list copies from), not out of a host copy shaders
    /// write.
    pub(super) fn takes_source(&self) -> bool {
        self.from.is_some()
    }

    /// Whether flights that write `written` are waited for.
    pub(super) fn waits(&self, written: &[(usize, usize)]) -> Waits {
        let source = touches(written, self.src, self.src + self.len);
        if source && self.from.is_some() {
            return Waits::Yes;
        }
        match source || touches(written, self.dst, self.dst + self.len) {
            true => Waits::ForReaders,
            false => Waits::No,
        }
    }

    /// At submission, in the order of the steps: the list's upload memory
    /// takes the source's bytes and, if `free` (no flight in the air may write
    /// source or destination), title memory at the destination does. Gives
    /// the bytes if it took them.
    pub(super) fn prepare(&self, free: bool) -> Option<Vec<u8>> {
        if !free && self.from.is_none() {
            return None;
        }
        let mut bytes = vec![0u8; self.len];
        // (What is not there reads as zeros, as any copy's source.)
        copy_mapped(bytes.as_mut_ptr(), self.src as *const u8, self.len, false);
        if let Some(to) = self.from {
            // SAFETY: the list's own upload memory of that many bytes.
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), to as *mut u8, self.len) };
        }
        if !free {
            return None;
        }
        copy_mapped(self.dst as *mut u8, bytes.as_ptr(), self.len, true);
        crate::watch::wrote(self.dst, self.len);
        // Title memory that is not all there was left alone.
        let mut there = vec![0u8; self.len];
        (read_into(self.dst as *const u8, &mut there) && there == bytes).then_some(bytes)
    }
}

/// Whether a copy waits for flights before its submission is prepared: not;
/// yes, as it takes a source on the CPU that they may write; or only if
/// another step of the submission reads its destination on the CPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Waits {
    No,
    ForReaders,
    Yes,
}

/// Whether a step of `runs` reads one of `ranges` on the CPU.
pub(super) fn read_by(runs: &[&HostList], mut ranges: Vec<(usize, usize)>) -> bool {
    join_ranges(&mut ranges, []);
    runs.iter().any(|list| {
        list.deferred().copies.iter().any(|step| match step {
            &Step::Copy(_, src, len) | &Step::Fill(_, src, len, _) => touches(&ranges, src, src + len),
            Step::Texture(staging) => {
                let (at, len) = staging.memory();
                touches(&ranges, at, at + len)
            }
            // After upstream GPL-3.0 fork, commit 8cc31ab37afc "Port NVIDIA Wave32
            // lowering, smooth movie-plane uploads, XMA audio fixes, and 32GB RAM residency budgets": a movie
            // plane reads title memory on the CPU. Here: the same.
            &Step::Plane((at, len), _) => touches(&ranges, at, at.saturating_add(len)),
            Step::Host(_, (from, to)) => touches(&ranges, *from, *to),
            Step::Placed(p) => p.from.is_some() && touches(&ranges, p.src, p.src + p.len),
            _ => false,
        })
    })
}

/// Whether a fill of upload memory that reads only bytes a copy at its place
/// ahead of it in the same list wrote reads them at the copy's source
/// instead (`VICTORY_FILL_FORWARD`, on by default; `=0`: at the destination,
/// as before).
///
/// At submission the CPU puts the copy into title memory and the fill reads
/// it there, in the order of the steps ([`Placed::prepare`]). While flights
/// the destination is declared written by are in the air it cannot: such a
/// fill waited for them, as the card could not do it either (it would read
/// the destination before the copy ran, `card_steps`' "a copy at its place
/// ... reads or writes what it writes"). With the card busy, that was a fill of
/// a few hundred bytes under a copy of a few thousand bytes there, made every
/// frame and declared written by the frame before's: a wait of up to hundreds
/// of milliseconds nearly every frame. The
/// source holds the same bytes at that step, so the fill reads them there and
/// is no longer in the flights' way for the destination (it is for the
/// source, as the copy is). Not across lists: which lists a submission has,
/// and in what order, is known at submission only.
pub(super) fn fill_forward() -> bool {
    crate::toggled!("VICTORY_FILL_FORWARD")
}

/// Fills that read a copy's source in place of its destination
/// ([`forwarded`]), since the last frame-rate line.
pub(super) static FORWARDED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The source of a write of title memory that a fill does not read through.
pub(super) const NO_SOURCE: usize = usize::MAX;

/// What a step that may write title memory anywhere writes (`Step::Run`).
pub(super) const ANYWHERE: (usize, usize, usize) = (0, usize::MAX, NO_SOURCE);

/// Where a fill of `[src, src + len)` recorded behind `titled` reads its
/// bytes: what the list's steps write of title memory on the CPU's side, in
/// order (from, to, and for a copy at its place its source, else
/// [`NO_SOURCE`]). If the last of them that writes any of those bytes is a
/// copy at its place that writes all of them, and neither its source's bytes
/// for them lie in what it writes nor a step behind it writes them, that
/// source's bytes; otherwise `src`.
pub(super) fn forwarded(titled: &[(usize, usize, usize)], src: usize, len: usize) -> usize {
    let end = src.saturating_add(len);
    let Some(k) = titled.iter().rposition(|&(from, to, _)| from < end && src < to) else {
        return src;
    };
    let (from, to, source) = titled[k];
    if len == 0 || source == NO_SOURCE || src < from || end > to {
        return src;
    }
    let at = source + (src - from);
    let at_end = at + len;
    let written = |&(from, to, _): &(usize, usize, usize)| from < at_end && at < to;
    if written(&titled[k]) || titled[k + 1..].iter().any(written) {
        return src;
    }
    at
}

/// [`forwarded`] for a fill recorded now in a list whose steps wrote `titled`,
/// with [`fill_forward`] on, counted.
pub(super) fn fill_source(titled: &[(usize, usize, usize)], src: usize, len: usize) -> usize {
    if titled.is_empty() || !fill_forward() {
        return src;
    }
    let at = forwarded(titled, src, len);
    if at != src {
        FORWARDED.fetch_add(1, Ordering::Relaxed);
    }
    at
}

/// Everything recorded so far is done, and seen, before what follows.
///
/// # Safety
/// An open list.
unsafe fn wait_for_all(host: &Host, list: &ID3D12GraphicsCommandList) {
    static ENHANCED: OnceLock<bool> = OnceLock::new();
    let enhanced = *ENHANCED.get_or_init(|| {
        let mut o = D3D12_FEATURE_DATA_D3D12_OPTIONS12::default();
        // SAFETY: a feature structure of its size.
        let known = unsafe {
            host.device.CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS12,
                (&raw mut o).cast(),
                size_of::<D3D12_FEATURE_DATA_D3D12_OPTIONS12>() as u32,
            )
        };
        known.is_ok() && o.EnhancedBarriersSupported.as_bool()
    });
    // SAFETY: the caller's.
    unsafe {
        if enhanced && let Ok(list) = list.cast::<ID3D12GraphicsCommandList7>() {
            let all = [D3D12_GLOBAL_BARRIER {
                SyncBefore: D3D12_BARRIER_SYNC_ALL,
                SyncAfter: D3D12_BARRIER_SYNC_ALL,
                AccessBefore: D3D12_BARRIER_ACCESS_COMMON,
                AccessAfter: D3D12_BARRIER_ACCESS_COMMON,
            }];
            list.Barrier(&[D3D12_BARRIER_GROUP {
                Type: D3D12_BARRIER_TYPE_GLOBAL,
                NumBarriers: 1,
                Anonymous: D3D12_BARRIER_GROUP_0 {
                    pGlobalBarriers: all.as_ptr(),
                },
            }]);
            return;
        }
        let alias = D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_ALIASING,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                Aliasing: std::mem::ManuallyDrop::new(D3D12_RESOURCE_ALIASING_BARRIER {
                    pResourceBefore: std::mem::ManuallyDrop::new(None),
                    pResourceAfter: std::mem::ManuallyDrop::new(None),
                }),
            },
        };
        let uav = D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                    pResource: std::mem::ManuallyDrop::new(None),
                }),
            },
        };
        list.ResourceBarrier(&[alias, uav]);
    }
}

/// The host copy of `[src, src + len)` if shaders write it: what it holds at
/// a place in a list is not what title memory holds at submission.
fn shaders_own(src: usize, len: usize) -> Option<std::sync::Arc<Shadow>> {
    crate::shadow::peek(src as u64, len as u64).filter(|s| s.written() && !s.in_place())
}

impl HostList {
    /// Copies `len` bytes of title memory at `src` to `dst`, at this place of
    /// the list (see the file). `false` if it is to be done at submission.
    pub(crate) fn place_copy(&self, dst: usize, src: usize, len: usize) -> bool {
        if !on() {
            return false;
        }
        let Some(host) = host() else {
            return false;
        };
        let Some(to) = crate::shadow::peek(dst as u64, len as u64).filter(|s| !s.in_place()) else {
            return false;
        };
        let own = shaders_own(src, len);
        let scratch = match &own {
            Some(_) => None,
            None if len <= MOST => match self.scratch_at(len as u64) {
                Some(scratch) => Some(scratch),
                None => return false,
            },
            None => return false,
        };
        let at = to.offset(dst as u64);
        // What the title's CPU wrote around and under it so far goes first.
        to.refresh(self, dst as u64, len as u64);
        // SAFETY (both): an open list; live buffers (the list holds the
        // copies and its scratch memory), ranges inside them.
        unsafe { wait_for_all(host, &self.list) };
        match (&own, &scratch) {
            (Some(from), _) => {
                from.refresh(self, src as u64, len as u64);
                let offset = from.offset(src as u64);
                if from.buffer.as_raw() == to.buffer.as_raw() {
                    let Some(between) = self.between(host, len as u64) else {
                        return false;
                    };
                    unsafe {
                        self.list.CopyBufferRegion(&between, 0, &from.buffer, offset, len as u64);
                        self.list.ResourceBarrier(&[transition(&between, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
                        wait_for_all(host, &self.list);
                        self.list.CopyBufferRegion(&to.buffer, at, &between, 0, len as u64);
                    }
                } else {
                    unsafe { self.list.CopyBufferRegion(&to.buffer, at, &from.buffer, offset, len as u64) };
                }
                STATS[2].fetch_add(1, Ordering::Relaxed);
            }
            (None, Some((from, offset, _))) => unsafe {
                self.list.CopyBufferRegion(&to.buffer, at, from, *offset, len as u64);
            },
            (None, None) => return false,
        }
        unsafe { wait_for_all(host, &self.list) };
        STATS[0].fetch_add(1, Ordering::Relaxed);
        STATS[1].fetch_add(len as u64, Ordering::Relaxed);
        to.gpu_writes(self, dst as u64, len as u64);
        let mut deferred = self.deferred();
        deferred.copies.push(Step::Placed(Placed {
            dst,
            src,
            len,
            shadow: to,
            from: scratch.map(|s| s.2 as usize),
        }));
        deferred.titled.push((dst, dst + len, src));
        deferred.title_written();
        self.changed_something();
        true
    }

    /// For a copy of `[src, src + len)` into a texture: a buffer (readable by
    /// copies) that holds what shaders wrote there, at this place of the list.
    /// `None` where shaders do not write it.
    pub(crate) fn place_source(&self, src: usize, len: usize) -> Option<ID3D12Resource> {
        if !on() {
            return None;
        }
        let host = host()?;
        let from = shaders_own(src, len)?;
        let between = self.between(host, len as u64)?;
        from.refresh(self, src as u64, len as u64);
        STATS[3].fetch_add(1, Ordering::Relaxed);
        // SAFETY: an open list; live buffers, ranges inside them.
        unsafe {
            wait_for_all(host, &self.list);
            self.list.CopyBufferRegion(&between, 0, &from.buffer, from.offset(src as u64), len as u64);
            self.list.ResourceBarrier(&[transition(&between, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
            wait_for_all(host, &self.list);
        }
        Some(between)
    }

    /// A buffer on the card for this recording alone.
    fn between(&self, host: &Host, len: u64) -> Option<ID3D12Resource> {
        let buffer = host.uav_buffer(len.next_multiple_of(1 << 16))?;
        name(&buffer, || "bytes between copies".into());
        self.keep(&buffer);
        Some(buffer)
    }
}

#[cfg(test)]
mod tests {
    use super::{ANYWHERE, NO_SOURCE, forwarded};

    /// A fill under a copy at its place reads the copy's source, and only
    /// where nothing else is in the way.
    #[test]
    fn a_fill_under_a_copy_reads_its_source() {
        let placed = (0x1000, 0x1bd0, 0x8000);
        assert_eq!(forwarded(&[placed], 0x1000, 0xf0), 0x8000);
        assert_eq!(forwarded(&[placed], 0x1100, 0x10), 0x8100);
        assert_eq!(forwarded(&[placed], 0x1bc0, 0x10), 0x8bc0);
        // Not all of it under the copy; nothing under it; nothing at all.
        assert_eq!(forwarded(&[placed], 0x1bc0, 0x20), 0x1bc0);
        assert_eq!(forwarded(&[placed], 0xff0, 0x20), 0xff0);
        assert_eq!(forwarded(&[placed], 0x1bd0, 0x20), 0x1bd0);
        assert_eq!(forwarded(&[], 0x1000, 0x20), 0x1000);
        assert_eq!(forwarded(&[placed], 0x1000, 0), 0x1000);
        // Written later by another copy, a step of ours, or a copy at its
        // place of other bytes: the last write is the one.
        assert_eq!(forwarded(&[placed, (0x1010, 0x1020, NO_SOURCE)], 0x1000, 0xf0), 0x1000);
        assert_eq!(forwarded(&[placed, ANYWHERE], 0x1000, 0xf0), 0x1000);
        assert_eq!(forwarded(&[placed, (0x1000, 0x1100, 0x9000)], 0x1000, 0xf0), 0x9000);
        assert_eq!(forwarded(&[(0x1000, 0x1100, 0x9000), placed], 0x1000, 0xf0), 0x8000);
        assert_eq!(forwarded(&[placed, (0x5000, 0x6000, NO_SOURCE)], 0x1000, 0xf0), 0x8000);
        // Its source written behind it, or ahead of it where it has those
        // bytes from; the copy over its own bytes.
        assert_eq!(forwarded(&[placed, (0x80f0, 0x8100, NO_SOURCE)], 0x1000, 0xf8), 0x1000);
        assert_eq!(forwarded(&[placed, (0x80f0, 0x8100, NO_SOURCE)], 0x1000, 0xf0), 0x8000);
        assert_eq!(forwarded(&[(0x1000, 0x1100, 0x1080)], 0x1000, 0x10), 0x1000);
        assert_eq!(forwarded(&[(0x1080, 0x1180, 0x1000)], 0x1080, 0x10), 0x1000);
        assert_eq!(forwarded(&[(0x1080, 0x1180, 0x1000)], 0x10f0, 0x20), 0x10f0);
    }

    /// Steps of one list, one after the other as the CPU does them at
    /// submission: copies at their place and other copies, steps of ours
    /// that write anywhere, fills. Each fill reads the same bytes where it is
    /// sent as at what it was recorded to read.
    #[test]
    fn a_fill_sent_elsewhere_reads_the_same_bytes() {
        let mut seed = 777u32;
        let mut random = move |n: u32| {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (seed >> 16) % n
        };
        let mut sent = 0;
        for _ in 0..3000 {
            let mut memory: Vec<u32> = (0..96).collect();
            let mut titled = Vec::new();
            for _ in 0..2 + random(10) {
                let (at, other, len) = (random(80) as usize, random(80) as usize, 1 + random(16) as usize);
                match random(10) {
                    0..3 => {
                        titled.push((at, at + len, other));
                        memory.copy_within(other..other + len, at);
                    }
                    3..5 => {
                        titled.push((at, at + len, NO_SOURCE));
                        memory.copy_within(other..other + len, at);
                    }
                    5 => {
                        titled.push(ANYWHERE);
                        let shift = random(1000);
                        memory.iter_mut().for_each(|m| *m = m.wrapping_mul(31).wrapping_add(shift));
                    }
                    _ => {
                        let from = forwarded(&titled, at, len);
                        sent += usize::from(from != at);
                        assert_eq!(memory[from..from + len], memory[at..at + len], "{titled:?}: {at}+{len} from {from}");
                    }
                }
            }
        }
        assert!(sent > 100, "{sent}");
    }

    /// What a list records of what its steps write: copies, joined ones as
    /// one, steps of ours; and the fills behind them read accordingly
    /// (`VICTORY_FILL_FORWARD`, unless set to 0 for the test run).
    #[test]
    fn a_list_names_what_it_writes() {
        let Some(host) = super::host() else {
            return;
        };
        let list = host.new_list().unwrap();
        let fills = |list: &super::HostList| -> Vec<usize> {
            list.deferred()
                .copies
                .iter()
                .filter_map(|s| match s {
                    super::Step::Fill(_, src, ..) => Some(*src),
                    _ => None,
                })
                .collect()
        };
        let base = 0x7000_0000usize;
        list.defer_copy((base + 0x100) as *mut u8, (base + 0x4000) as *const u8, 0x40);
        list.defer_copy((base + 0x140) as *mut u8, (base + 0x4040) as *const u8, 0x40);
        // (One step with `VICTORY_JOIN_COPIES=1`, two without.)
        let named: Vec<_> = list.deferred().titled.iter().map(|t| (t.0 - base, t.1 - base, t.2)).collect();
        assert!(named == [(0x100, 0x180, NO_SOURCE)] || named == [(0x100, 0x140, NO_SOURCE), (0x140, 0x180, NO_SOURCE)], "{named:?}");
        // As `place_copy` names a copy at its place.
        list.deferred().titled.push((base + 0x1000, base + 0x1bd0, base + 0x8000));
        let mut scratch = [0u8; 0x100];
        list.defer_fill(scratch.as_mut_ptr(), (base + 0x1000) as *const u8, 0xf0);
        list.defer_fill(scratch.as_mut_ptr(), (base + 0x150) as *const u8, 0x10);
        list.defer(|| {});
        list.defer_fill(scratch.as_mut_ptr(), (base + 0x1000) as *const u8, 0xf0);
        let on = super::fill_forward();
        assert_eq!(fills(&list), [if on { base + 0x8000 } else { base + 0x1000 }, base + 0x150, base + 0x1000]);
        assert_eq!(list.deferred().titled.last(), Some(&ANYWHERE));
        list.close();
    }

    /// What copies with two sides cost to make (ignored: a measurement for
    /// `VICTORY_CARD_ALL`): buffers of 64 KiB on the card and in the PC's
    /// memory.
    #[test]
    #[ignore]
    fn small_buffers_cost() {
        let Some(host) = super::host() else {
            return;
        };
        const N: usize = 1000;
        let used = || host.video_memory().map_or(0, |m| m[1] >> 20);
        let before = used();
        let t = std::time::Instant::now();
        let card: Vec<_> = (0..N).map(|_| host.uav_buffer(1 << 16)).collect();
        let card_us = t.elapsed().as_micros() as f64 / N as f64;
        let after = used();
        let t = std::time::Instant::now();
        let pc: Vec<_> = (0..N).map(|_| host.shared_buffer(1 << 16)).collect();
        let pc_us = t.elapsed().as_micros() as f64 / N as f64;
        println!(
            "{N} buffers of 64 KiB: on the card {card_us:.0} us each, {} MB of its memory; in the PC's memory {pc_us:.0} us each",
            after.saturating_sub(before)
        );
        drop((card, pc));
    }
}
