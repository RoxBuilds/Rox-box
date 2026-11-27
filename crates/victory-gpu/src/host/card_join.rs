//! Compares of copies in the card's memory called off when the next plan's
//! compare covers them (`VICTORY_CARD_JOIN=<k>`; off by default; with the
//! relay only, `crate::host::relay`).
//!
//! After every submission that declares shader writes to a copy in the card's
//! memory, a pass of ours hashes the declared pieces on the card and flags
//! those that changed ([`CARD_COMPARE`], `Host::card_before`). In a busy scene that is
//! tens of thousands of pieces a frame (gigabytes hashed), the title's big heap about
//! 13 times a frame, once for each submission that declares it. On an 8 GB card one
//! compare of that heap takes under a millisecond (the card's memory bandwidth), 13 of
//! them as the host sends them over ten milliseconds a frame (`card_cost`).
//!
//! The compares of a copy that consecutive plans declare are made one: the
//! compare of the first may be called off by the second, whose compare covers
//! its pieces and runs after both have written them, so it flags what both
//! changed. Nothing is held back: the first plan's compare goes to the queue
//! as before (mode 3 of [`CARD_COMPARE`]); each of its groups looks at a dword
//! ahead of the range's results first, and does nothing if it was set to 0.
//! The second plan sets it, once its own compare is recorded. If the card ran
//! the first compare already, or part of it, its results stand where it ran;
//! what it did not compare keeps [`SKIPPED`], and there the results of the
//! second compare count (and of a third, if the second was called off too: at
//! most `k` plans a compare).
//!
//! What would hide a change from the second compare is a hash stored in
//! between: the hash passes after title bytes went up to the copy (the sync
//! list of the second plan, `Host::card_before`). The pieces those passes
//! hash are compared first, in a list ahead of the sync list (the "pre"
//! passes of a [`Link`]), and their flags count as well.
//!
//! What changes: a flight whose compare was called off lands when the card
//! has run the compare that covered it (`Host::relay_worker`'s landing order is
//! kept: it waits for the later flight). Its fence signals and bottom-of-pipe
//! writes are told then, later than before, never earlier, as everything the
//! relay tells. Our own waits for memory a flight declared are not made
//! longer: the second plan declares everything the first did (else nothing is
//! called off; `VICTORY_CARD_JOIN_ANY=1` asks only that it cover the copy).
//! What is fetched is what the compares flag, as before: a piece the first
//! flight changed is flagged by the first compare (where it ran), by a pre
//! pass, or by the covering one; the landing fetches what the card holds then,
//! as the relay's fetches always did.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize};

use super::*;
use crate::shadow::{CardSpan, Shadow};

/// What a compare called off leaves in a piece's dword: it is never stored by
/// one that runs (mode 3 makes that result 0xffff_fffd). Odd, so that a reader
/// that does not know it fetches the piece.
pub(super) const SKIPPED: u32 = u32::MAX;
/// The dword ahead of a range's results while its compare is to run.
const GO: u32 = 1;
/// [`landing`]: a compare that covers the flight's is not on the queue yet.
pub(super) const UNSENT: u64 = u64::MAX - 1;
/// The most groups a compare called off may have: one dispatch reads its dword.
const MOST_PIECES: u32 = D3D12_CS_DISPATCH_MAX_THREAD_GROUPS_PER_DIMENSION;

/// For tests: `most`, set (`u32::MAX`: as the environment says).
static MOST_SET: AtomicU32 = AtomicU32::new(u32::MAX);

/// `VICTORY_CARD_JOIN=<k>`: at most `k` plans' compares of a copy are one; 0
/// or 1 (the default): none called off, as before.
pub(super) fn most() -> u32 {
    let set = MOST_SET.load(Ordering::Relaxed);
    if set != u32::MAX {
        return set;
    }
    static MOST: OnceLock<u32> = OnceLock::new();
    *MOST.get_or_init(|| {
        let most = std::env::var("VICTORY_CARD_JOIN").ok().and_then(|v| v.trim().parse::<u32>().ok()).unwrap_or(0);
        if most >= 2 {
            tracing::info!(
                most,
                least_mb = least_pieces() / 16,
                any = any(),
                "compares of copies in the card's memory called off when the next plan's covers them (VICTORY_CARD_JOIN)"
            );
        }
        most
    })
}

/// The least pieces of a range whose compare may be called off
/// (`VICTORY_CARD_JOIN_MB`, 16): the small ones cost the card little.
fn least_pieces() -> u32 {
    static LEAST: OnceLock<u32> = OnceLock::new();
    *LEAST.get_or_init(|| {
        let mb = std::env::var("VICTORY_CARD_JOIN_MB").ok().and_then(|v| v.trim().parse::<u32>().ok()).unwrap_or(16);
        mb.saturating_mul(16).max(1)
    })
}

/// `VICTORY_CARD_JOIN_ANY=1`: a compare is called off by a plan that covers
/// its pieces, whatever else the plans declare.
fn any() -> bool {
    static ANY: OnceLock<bool> = OnceLock::new();
    *ANY.get_or_init(|| std::env::var_os("VICTORY_CARD_JOIN_ANY").is_some_and(|v| v == "1"))
}

/// The compare's results as mapped (address, dwords), for the thread that
/// lands flights: it may not take `CARD_WORK`'s lock under the line's.
static DIFFERS: [AtomicUsize; 2] = [const { AtomicUsize::new(0) }; 2];

/// `CardWork::differs`' mapping and bytes.
pub(super) fn mapped(at: usize, bytes: u64) {
    DIFFERS[1].store((bytes / 4) as usize, Ordering::Relaxed);
    DIFFERS[0].store(at, Ordering::Release);
}

fn differs() -> &'static [u32] {
    let at = DIFFERS[0].load(Ordering::Acquire);
    if at == 0 {
        return &[];
    }
    // SAFETY: the mapping of `CardWork::differs`, which lives as long as the
    // host; read while compares the caller waited for have run.
    unsafe { std::slice::from_raw_parts(at as *const u32, DIFFERS[1].load(Ordering::Relaxed)) }
}

/// Stores `value` in dword `i` of the results, if it lies in them.
fn store(i: usize, value: u32) -> bool {
    let at = DIFFERS[0].load(Ordering::Acquire);
    if at == 0 || i >= DIFFERS[1].load(Ordering::Relaxed) {
        return false;
    }
    // SAFETY: inside the mapping (checked above); the callers' dwords are
    // theirs alone, or read by the card only.
    unsafe { (at as *mut u32).add(i).write_volatile(value) };
    true
}

/// Since the last frame-rate line: compares that could be called off, those
/// called off, pieces their card did not compare, pre passes and their
/// pieces, flights that waited for a later compare, and the microseconds
/// from when the card had run them.
static STATS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];

/// Results of a pass of the compare: its first piece's dword, its first piece
/// (in the copy), how many.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Pass {
    pub(super) at: u32,
    pub(super) first: u32,
    pub(super) pieces: u32,
}

impl Pass {
    fn holds(&self, piece: u32) -> bool {
        piece >= self.first && piece - self.first < self.pieces
    }
}

/// A compare that may be called off: set when it is.
#[derive(Default)]
pub(super) struct Called(Mutex<Option<Link>>);

/// What stands for a compare that was called off.
#[derive(Clone)]
pub(super) struct Link {
    /// Of its pieces that were to be hashed again after title bytes went up,
    /// the compares made before.
    pre: Vec<Pass>,
    /// The compare that covers it.
    by: Pass,
    /// That one's own, if it may be called off in turn.
    next: Option<Arc<Called>>,
    /// The host fence value behind that compare's plan, once sent (0 before).
    fence: Arc<AtomicU64>,
}

/// Of a range of a plan whose compare may be called off.
#[derive(Clone)]
pub(super) struct Joined {
    pub(super) called: Arc<Called>,
    /// Earlier plans' compares this one stands for, at most.
    depth: u32,
}

/// Of a plan ([`CardPlan`]).
#[derive(Default)]
pub(super) struct Plan {
    /// The host fence value signalled behind its lists, once sent.
    fence: Arc<AtomicU64>,
    /// When the card was found to have run it while it waited for a compare
    /// that covers its own.
    waiting: OnceLock<std::time::Instant>,
    counted: AtomicBool,
}

impl Plan {
    /// Its submission went to the queue, `fence` signalled behind it.
    pub(super) fn sent(&self, fence: u64) {
        self.fence.store(fence, Ordering::Release);
    }
}

/// A compare the next plan may call off.
pub(super) struct Offer {
    copy: Arc<Shadow>,
    span: CardSpan,
    /// Its results (its go dword is the one ahead).
    at: u32,
    called: Arc<Called>,
    depth: u32,
    /// What its plan declared, by copy, joined ([`by_copy`]).
    declared: Arc<Vec<(usize, Vec<(usize, usize)>)>>,
}

/// A compare the plan being made calls off once its own is recorded.
pub(super) struct Call {
    offer: Offer,
    pre: Vec<Pass>,
}

/// The previous plan's compares that may be called off, for the next plan
/// alone: whatever comes between (a plan with nothing for the card, one sent
/// and waited for) takes them.
static OFFERS: Mutex<Vec<Offer>> = Mutex::new(Vec::new());

/// [`OFFERS`], taken: at the start of every plan.
pub(super) fn take() -> Vec<Offer> {
    std::mem::take(&mut *OFFERS.lock().unwrap_or_else(|e| e.into_inner()))
}

/// Ranges by copy (its pointer), each copy's in order and joined where they
/// touch, as `Host::card_before` joins them for the compare.
fn by_copy(declared: &[Declared]) -> Vec<(usize, Vec<(usize, usize)>)> {
    let mut out: Vec<(usize, Vec<(usize, usize)>)> = Vec::new();
    for (shadow, ranges) in declared {
        let key = Arc::as_ptr(shadow) as usize;
        match out.iter_mut().find(|o| o.0 == key) {
            Some((_, all)) => all.extend_from_slice(ranges),
            None => out.push((key, ranges.clone())),
        }
    }
    for (_, ranges) in &mut out {
        ranges.sort_unstable();
        let mut joined: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
        for &(from, to) in ranges.iter() {
            match joined.last_mut() {
                Some(last) if from <= last.1 => last.1 = last.1.max(to),
                _ => joined.push((from, to)),
            }
        }
        *ranges = joined;
    }
    out
}

/// Whether every range of `inner` lies in a range of `outer` of the same copy
/// (both as [`by_copy`] gives them).
fn within(inner: &[(usize, Vec<(usize, usize)>)], outer: &[(usize, Vec<(usize, usize)>)]) -> bool {
    inner.iter().all(|(copy, ranges)| {
        let joined = outer.iter().find(|o| o.0 == *copy).map_or(&[][..], |o| &o.1[..]);
        ranges.iter().filter(|r| r.0 < r.1).all(|&(a, b)| {
            let i = joined.partition_point(|r| r.1 < b);
            joined.get(i).is_some_and(|r| r.0 <= a)
        })
    })
}

/// The span of `spans` that holds every piece of `first..first + pieces`.
fn cover(first: u32, pieces: u32, spans: &[CardSpan]) -> Option<usize> {
    spans.iter().position(|s| s.first <= first && first + pieces <= s.first + s.pieces)
}

/// The spans `Host::card_before` compares of `copy` for `declared` (the plan's
/// own, in the card's memory).
fn spans_of(copy: &Shadow, declared: &[(usize, Vec<(usize, usize)>)]) -> Vec<CardSpan> {
    let key = std::ptr::from_ref(copy) as usize;
    declared
        .iter()
        .find(|d| d.0 == key)
        .map(|d| d.1.iter().filter_map(|&(from, to)| copy.card_span(from, to)).collect())
        .unwrap_or_default()
}

/// Of `offers` (the plan before's), those this plan calls off: it is sent
/// ahead with the relay, covers their pieces and (unless [`any`]) declares
/// everything their plan declared. `all` is what this plan declares, `declared`
/// what of it lies in copies in the card's memory (as `Host::card_before`
/// compares it), `whole` the copies hashed whole now.
pub(super) fn calls(
    offers: Vec<Offer>,
    flown: bool,
    all: &[Declared],
    declared: &[Declared],
    whole: &[Arc<Shadow>],
) -> Vec<Call> {
    if !flown || most() < 2 || offers.is_empty() {
        return Vec::new();
    }
    let (all, declared) = (by_copy(all), by_copy(declared));
    offers
        .into_iter()
        .filter(|o| {
            o.depth + 1 < most()
                && !whole.iter().any(|w| Arc::ptr_eq(w, &o.copy))
                && (any() || within(&o.declared, &all))
                && cover(o.span.first, o.span.pieces, &spans_of(&o.copy, &declared)).is_some()
        })
        .map(|offer| Call { offer, pre: Vec::new() })
        .collect()
}

/// The pieces of `run` within `first..first + pieces`.
fn cut(run: CardSpan, first: u32, pieces: u32) -> Option<CardSpan> {
    let (lo, hi) = (run.first.max(first), (run.first + run.pieces).min(first + pieces));
    (lo < hi).then_some(CardSpan { first: lo, pieces: hi - lo, ..run })
}

/// For a range `Host::card_before` compares (at dword `*at` on, `bytes` of
/// results in all): whether the next plan may call its compare off. If so its
/// results move one dword on, past the one that says whether it is to run
/// (set to [`GO`]), and they read [`SKIPPED`] until it has run.
pub(super) fn range(flown: bool, copy: &Arc<Shadow>, span: &CardSpan, calls: &[Call], at: &mut u32, bytes: u64) -> Option<Joined> {
    if !flown || most() < 2 || span.pieces < least_pieces() || span.pieces > MOST_PIECES {
        return None;
    }
    let depth = calls
        .iter()
        .filter(|c| Arc::ptr_eq(&c.offer.copy, copy) && cover(c.offer.span.first, c.offer.span.pieces, &[*span]).is_some())
        .map(|c| c.offer.depth + 1)
        .max()
        .unwrap_or(0);
    let end = (*at + 1 + span.pieces) as usize;
    if depth + 1 >= most() || u64::from(*at + 1 + span.pieces) * 4 > bytes || differs().len() < end {
        return None;
    }
    // (No plan out uses these dwords, `card_done` starts the area again only
    // when none is, and the card reads them only once the compare that
    // follows is on the queue.)
    store(*at as usize, GO);
    for i in *at as usize + 1..end {
        store(i, SKIPPED);
    }
    *at += 1;
    STATS[0].fetch_add(1, Ordering::Relaxed);
    Some(Joined { called: Arc::new(Called::default()), depth })
}

impl Host {
    /// Ahead of the list that sends what changed to copies in the card's
    /// memory, for `calls`: the pieces it is to hash again (title bytes went
    /// up there) are compared first, each call's into results of its own
    /// (taken of `work.taken`), and the list goes to the queue now. The list,
    /// to be given back with the plan, if there was one; a call whose pieces
    /// could not be compared is dropped (its compare runs).
    pub(super) fn card_join_pre(
        &self,
        work: &mut CardWork,
        calls: &mut Vec<Call>,
        dirty: &[(Arc<Shadow>, Vec<(usize, usize)>)],
    ) -> Option<Prelude> {
        let capacity = work.differs.2;
        calls.retain_mut(|call| {
            let o = &call.offer;
            let Some((_, ranges)) = dirty.iter().find(|(s, _)| Arc::ptr_eq(s, &o.copy)) else {
                return true;
            };
            let runs = ranges.iter().filter_map(|&(a, b)| cut(o.copy.card_run(a, b), o.span.first, o.span.pieces)).collect();
            for run in crate::shadow::joined_piece_runs(runs) {
                if u64::from(work.taken + run.pieces) * 4 > capacity {
                    return false;
                }
                call.pre.push(Pass { at: work.taken, first: run.first, pieces: run.pieces });
                work.taken += run.pieces;
            }
            true
        });
        if calls.iter().all(|c| c.pre.is_empty()) {
            return None;
        }
        let (allocator, list) = match work.check.pop().map_or_else(|| self.try_prelude(), Ok) {
            Ok(prelude) => prelude,
            Err(e) => {
                tracing::warn!(error = %e, "no host list to compare pieces ahead of their sends: no compare called off");
                calls.clear();
                return None;
            }
        };
        // SAFETY: the list's last run finished (its plan was done with); live
        // buffers; the compare's own root signature; the pieces lie in the
        // buffers.
        let recorded = unsafe {
            (|| -> windows::core::Result<()> {
                allocator.Reset()?;
                list.Reset(&allocator, None)?;
                // What the flights before wrote, first.
                list.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
                    Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
                    Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                    Anonymous: D3D12_RESOURCE_BARRIER_0 {
                        UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER { pResource: std::mem::ManuallyDrop::new(None) }),
                    },
                }]);
                list.SetComputeRootSignature(&work.root);
                list.SetPipelineState(&work.pso);
                list.SetComputeRootUnorderedAccessView(3, work.differs.0.GetGPUVirtualAddress());
                for call in calls.iter().filter(|c| !c.pre.is_empty()) {
                    let o = &call.offer;
                    let (Some((_, Some(before), _)), size) = (o.copy.card_sides(), o.copy.card_hash_size()) else {
                        continue;
                    };
                    list.SetComputeRootShaderResourceView(1, o.copy.buffer.GetGPUVirtualAddress());
                    list.SetComputeRootUnorderedAccessView(2, before.GetGPUVirtualAddress());
                    for pass in &call.pre {
                        let run = CardSpan { first: pass.first, pieces: pass.pieces, ..o.span };
                        card_pass(&list, &run, size as u32, pass.at, 2);
                        STATS[3].fetch_add(1, Ordering::Relaxed);
                        STATS[4].fetch_add(u64::from(pass.pieces), Ordering::Relaxed);
                    }
                }
                list.Close()?;
                self.queue.ExecuteCommandLists(&[Some(list.cast()?)]);
                Ok(())
            })()
        };
        if let Err(e) = recorded {
            tracing::warn!(error = %e, "pieces could not be compared ahead of their sends: no compare called off");
            calls.clear();
            retire((allocator, list));
            return None;
        }
        Some((allocator, list))
    }
}

/// `plan`'s compare is recorded: the compares of `calls` are called off, each
/// standing for the range of `plan` that covers it, and `plan`'s own that may
/// be are offered to the next plan. `all` is what `plan` declares.
pub(super) fn after_compare(calls: Vec<Call>, plan: &CardPlan, all: &[Declared]) {
    for call in calls {
        let o = &call.offer;
        let Some(by) = plan.ranges.iter().find(|r| {
            r.compared && !r.picked && Arc::ptr_eq(&r.shadow, &o.copy) && cover(o.span.first, o.span.pieces, &[r.span]).is_some()
        }) else {
            continue;
        };
        *o.called.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(Link {
            pre: call.pre,
            by: Pass { at: by.at, first: by.span.first, pieces: by.span.pieces },
            next: by.called.as_ref().map(|j| j.called.clone()),
            fence: plan.join.fence.clone(),
        });
        // After the link: a compare the card skips is one whose link is there.
        if store(o.at as usize - 1, 0) {
            STATS[1].fetch_add(1, Ordering::Relaxed);
        }
    }
    let declared = Arc::new(by_copy(all));
    let offers: Vec<Offer> = plan
        .ranges
        .iter()
        .filter(|r| r.compared && !r.picked)
        .filter_map(|r| {
            let joined = r.called.as_ref()?;
            Some(Offer {
                copy: r.shadow.clone(),
                span: r.span,
                at: r.at,
                called: joined.called.clone(),
                depth: joined.depth,
                declared: declared.clone(),
            })
        })
        .collect();
    *OFFERS.lock().unwrap_or_else(|e| e.into_inner()) = offers;
}

/// Why a piece's result is not known yet.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Wait {
    /// Once the host fence has reached this.
    Fence(u64),
    /// A compare that covers it is not on the queue yet.
    Unsent,
}

/// The result of piece `piece` (its number in the copy) of a range whose
/// first piece is `first` and whose results start at dword `at`, with the
/// host fence at `completed`: the result of the compare that ran last over it,
/// flagged if any compare from the range's on flagged it. Where the range's
/// compare was called off, the pre passes and the compare that covers it say
/// (and so on, if that one was called off too).
pub(super) fn resolve(differs: &[u32], at: u32, first: u32, called: Option<&Arc<Called>>, piece: u32, completed: u64) -> Result<u32, Wait> {
    let (mut at, mut first, mut called) = (at, first, called.cloned());
    let mut flags = 0u32;
    // (A chain is as long as `most` at most.)
    for _ in 0..64 {
        let Some(&d) = piece.checked_sub(first).and_then(|p| differs.get((at + p) as usize)) else {
            return Ok(1);
        };
        let link = match &called {
            Some(cell) if d == SKIPPED => cell.0.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            _ => return Ok(d | flags),
        };
        // (Never called off, yet not compared: fetched.)
        let Some(link) = link else {
            return Ok(d | flags);
        };
        match link.fence.load(Ordering::Acquire) {
            0 => return Err(Wait::Unsent),
            fence if fence > completed => return Err(Wait::Fence(fence)),
            _ => {}
        }
        for pass in link.pre.iter().filter(|p| p.holds(piece)) {
            flags |= differs.get((pass.at + piece - pass.first) as usize).map_or(1, |d| d & 1);
        }
        (at, first, called) = (link.by.at, link.by.first, link.next);
    }
    Ok(1)
}

impl CardRange {
    /// The compare's result for piece `p` of the range ([`resolve`]), its
    /// flights landed; `None` if it is not known (the piece is fetched).
    pub(super) fn result(&self, differs: &[u32], p: u32) -> Option<u32> {
        match &self.called {
            None => differs.get((self.at + p) as usize).copied(),
            Some(joined) => resolve(differs, self.at, self.span.first, Some(&joined.called), self.span.first + p, u64::MAX).ok(),
        }
    }
}

/// For the thread that lands flights: the host fence value after which a
/// flight sent with `own` behind it and `plan` for copies in the card's memory
/// can land, the fence being at `completed`. Its own, unless the card has run
/// it and its compare was called off (in part): then the one behind the
/// compare that covered it, or [`UNSENT`].
pub(super) fn landing(plan: Option<&CardPlan>, own: u64, completed: u64) -> u64 {
    let Some(plan) = plan else {
        return own;
    };
    if own > completed || !plan.ranges.iter().any(|r| r.called.is_some()) {
        return own;
    }
    let differs = differs();
    let (mut need, mut skipped) = (own, 0u64);
    for r in plan.ranges.iter().filter(|r| r.compared) {
        let Some(joined) = &r.called else {
            continue;
        };
        for p in 0..r.span.pieces {
            if differs.get((r.at + p) as usize) == Some(&SKIPPED) {
                skipped += 1;
            }
            match resolve(differs, r.at, r.span.first, Some(&joined.called), r.span.first + p, completed) {
                Ok(_) => {}
                Err(Wait::Fence(fence)) => need = need.max(fence),
                Err(Wait::Unsent) => need = UNSENT,
            }
        }
    }
    if need > completed {
        plan.join.waiting.get_or_init(std::time::Instant::now);
    } else if !plan.join.counted.swap(true, Ordering::Relaxed) {
        STATS[2].fetch_add(skipped, Ordering::Relaxed);
        if let Some(since) = plan.join.waiting.get() {
            STATS[5].fetch_add(1, Ordering::Relaxed);
            STATS[6].fetch_add(since.elapsed().as_micros() as u64, Ordering::Relaxed);
        }
    }
    need
}

/// What a card compares in a second, for the line's estimate of the time
/// the pieces called off would have taken: a round figure, not a measurement
/// of the card in use.
const NOMINAL_GB_PER_S: f64 = 250.0;

/// The frame-rate line's part, with the switch on.
pub(super) fn line(frames: f64) {
    let [offered, called, skipped, pre, pre_pieces, waits, waited] = STATS.each_ref().map(|s| s.swap(0, Ordering::Relaxed));
    if most() < 2 || offered == 0 {
        return;
    }
    let per = |n: u64| n as f64 / frames;
    tracing::info!(
        "compares of copies in the card's memory per frame that the next plan's may call off (VICTORY_CARD_JOIN={}): {:.1}, \
         {:.1} called off, {:.0} pieces the card did not compare (some {:.1} ms of the card at a nominal {NOMINAL_GB_PER_S:.0} GB/s); {:.1} passes over \
         {:.0} pieces ahead of sends; {:.1} flights waited {:.1} ms for the compare that covered theirs",
        most(),
        per(offered),
        per(called),
        per(skipped),
        per(skipped) * 65536.0 / (NOMINAL_GB_PER_S * 1e6),
        per(pre),
        per(pre_pieces),
        per(waits),
        per(waited) / 1e3,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shader's mode 3 on the software adapter, on a device of its own:
    /// with its dword set it compares as mode 2 does (and never stores
    /// [`SKIPPED`]); with it 0 it stores nothing, neither results nor hashes.
    #[test]
    fn a_compare_called_off_compares_nothing() {
        use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
        use windows::Win32::Graphics::Direct3D::ID3DBlob;
        const PIECES: u32 = 64;
        const SIZE: u64 = PIECES as u64 * 65536;
        let Ok(adapter) = warp_adapter() else {
            return;
        };
        let mut device: Option<ID3D12Device> = None;
        // SAFETY: device creation on the software adapter.
        if unsafe { D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device) }.is_err() {
            return;
        }
        let device = device.unwrap();
        let mut code: Option<ID3DBlob> = None;
        let heap = |kind| D3D12_HEAP_PROPERTIES { Type: kind, ..Default::default() };
        let shared = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_CUSTOM,
            CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
            MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
            ..Default::default()
        };
        let buffer = |heap: D3D12_HEAP_PROPERTIES, size: u64, flags, state| {
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: size,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                Flags: flags,
                ..Default::default()
            };
            let mut made: Option<ID3D12Resource> = None;
            // SAFETY: plain creation.
            unsafe { device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut made) }.unwrap();
            made.unwrap()
        };
        let map = |b: &ID3D12Resource| {
            let mut data = std::ptr::null_mut();
            // SAFETY: a CPU-visible buffer.
            unsafe { b.Map(0, None, Some(&mut data)) }.unwrap();
            data.cast::<u8>()
        };
        let uav = D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS;
        let now = buffer(heap(D3D12_HEAP_TYPE_DEFAULT), SIZE, uav, D3D12_RESOURCE_STATE_COMMON);
        let hashes = buffer(heap(D3D12_HEAP_TYPE_DEFAULT), crate::shadow::card_hash_bytes(SIZE), uav, D3D12_RESOURCE_STATE_COMMON);
        let results = buffer(shared, 4096, uav, D3D12_RESOURCE_STATE_COMMON);
        let upload = buffer(heap(D3D12_HEAP_TYPE_UPLOAD), SIZE, D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_GENERIC_READ);
        let (words, bytes) = (map(&results).cast::<u32>(), map(&upload));
        // SAFETY: the mapping, `SIZE` bytes.
        let content = unsafe { std::slice::from_raw_parts_mut(bytes, SIZE as usize) };
        for (i, b) in content.iter_mut().enumerate() {
            *b = (i as u32).wrapping_mul(2_654_435_761).rotate_right(13) as u8;
        }
        // SAFETY: the source and names live through the call; the blob's bytes.
        let (root, pso) = unsafe {
            D3DCompile(
                CARD_COMPARE.as_ptr().cast(),
                CARD_COMPARE.len(),
                windows::core::s!("card compare"),
                None,
                None,
                windows::core::s!("main"),
                windows::core::s!("cs_5_1"),
                0,
                0,
                &mut code,
                None,
            )
            .unwrap();
            let code = code.unwrap();
            let bytes = std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize());
            let root: ID3D12RootSignature = device.CreateRootSignature(0, bytes).unwrap();
            let pso: ID3D12PipelineState = device
                .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&root),
                    CS: D3D12_SHADER_BYTECODE { pShaderBytecode: bytes.as_ptr().cast(), BytecodeLength: bytes.len() },
                    ..Default::default()
                })
                .unwrap();
            (root, pso)
        };
        // SAFETY: plain objects.
        let (queue, allocator, list, fence, event) = unsafe {
            let queue: ID3D12CommandQueue = device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() })
                .unwrap();
            let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
            let list: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None).unwrap();
            list.Close().unwrap();
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            (queue, allocator, list, fence, CreateEventW(None, false, false, None).unwrap())
        };
        let mut sent = 0;
        // Uploads `dirty` pieces of `content`, then one pass.
        let mut run = |dirty: &[u32], at: u32, mode: u32| {
            let span = CardSpan { from: 0, to: SIZE as u32, first: 0, pieces: PIECES, skew: 0 };
            // SAFETY: the list's last run was waited for; live buffers.
            unsafe {
                allocator.Reset().unwrap();
                list.Reset(&allocator, None).unwrap();
                for &p in dirty {
                    list.CopyBufferRegion(&now, u64::from(p) << 16, &upload, u64::from(p) << 16, 65536);
                }
                list.ResourceBarrier(&[transition(&now, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE)]);
                list.SetComputeRootSignature(&root);
                list.SetPipelineState(&pso);
                list.SetComputeRootUnorderedAccessView(3, results.GetGPUVirtualAddress());
                list.SetComputeRootShaderResourceView(1, now.GetGPUVirtualAddress());
                list.SetComputeRootUnorderedAccessView(2, hashes.GetGPUVirtualAddress());
                card_pass(&list, &span, SIZE as u32, at, mode);
                list.Close().unwrap();
                queue.ExecuteCommandLists(&[Some(list.cast::<ID3D12CommandList>().unwrap())]);
                sent += 1;
                queue.Signal(&fence, sent).unwrap();
                fence.SetEventOnCompletion(sent, event).unwrap();
                WaitForSingleObject(event, INFINITE);
            }
        };
        let all: Vec<u32> = (0..PIECES).collect();
        // SAFETY (each read and write of `words`): the mapping, 1,024 dwords;
        // nothing on the card meanwhile.
        let word = |i: u32| unsafe { words.add(i as usize).read_volatile() };
        let set = |i: u32, v: u32| unsafe { words.add(i as usize).write_volatile(v) };
        run(&all, 0, 0);
        // Two pieces change; mode 3 with its dword set: as mode 2.
        content[3 << 16] ^= 1;
        content[(10 << 16) + 777] ^= 0x40;
        set(100, GO);
        (0..PIECES).for_each(|p| set(101 + p, SKIPPED));
        run(&[3, 10], 101, 3);
        let flagged: Vec<u32> = (0..PIECES).filter(|&p| torn::flagged(word(101 + p))).collect();
        assert_eq!(flagged, [3, 10]);
        for p in 0..PIECES {
            assert_ne!(word(101 + p), SKIPPED);
            let piece = &content[(p as usize) << 16..][..65536];
            assert_eq!(torn::folded(word(101 + p)), torn::hash(piece), "piece {p}");
        }
        // Another changes; called off: nothing stored, hashes as they were.
        content[(20 << 16) + 5] ^= 2;
        set(300, 0);
        (0..PIECES).for_each(|p| set(301 + p, SKIPPED));
        run(&[20], 301, 3);
        assert!((0..PIECES).all(|p| word(301 + p) == SKIPPED), "a compare called off stores nothing");
        // Looked at (mode 1): only that piece is not as last hashed.
        run(&[], 500, 1);
        let flagged: Vec<u32> = (0..PIECES).filter(|&p| torn::flagged(word(500 + p))).collect();
        assert_eq!(flagged, [20], "the hashes stayed");
        // SAFETY: our own event.
        unsafe { windows::Win32::Foundation::CloseHandle(event) }.ok();
    }

    /// The copy's pieces as the card holds them in a model: each value is
    /// new (a version), so a piece is changed exactly when it is written.
    struct Card {
        content: Vec<u64>,
        hashes: Vec<u64>,
        differs: Vec<u32>,
        /// Fence values reached.
        completed: u64,
    }

    /// What the queue holds, in order.
    enum Op {
        /// A flight's lists write these pieces.
        Lists(Vec<u32>),
        /// A pass of the compare: its results' dword, first piece, pieces, mode;
        /// the pieces it did so far.
        Compare(u32, u32, u32, u32, u32),
        /// Title bytes go up to these pieces, which are hashed again.
        Send(Vec<u32>),
        Fence(u64),
    }

    impl Card {
        fn result(&self, piece: u32, flag: bool) -> u32 {
            let folded = (self.content[piece as usize] as u32).wrapping_mul(0x9E37_79B1) >> 1;
            (folded << 1) | u32::from(flag)
        }

        /// One step of the queue's front (a compare goes piece by piece, so
        /// that it can be called off in the middle). Whether it went on.
        fn step(&mut self, queue: &mut std::collections::VecDeque<Op>, version: &mut u64) -> bool {
            let Some(op) = queue.front_mut() else {
                return false;
            };
            match op {
                Op::Lists(pieces) => {
                    for &p in pieces.iter() {
                        *version += 1;
                        self.content[p as usize] = *version;
                    }
                }
                Op::Send(pieces) => {
                    for &p in pieces.iter() {
                        *version += 1;
                        self.content[p as usize] = *version;
                        self.hashes[p as usize] = *version;
                    }
                }
                Op::Fence(f) => self.completed = *f,
                Op::Compare(at, first, pieces, mode, done) => {
                    let (at, first, pieces, mode) = (*at, *first, *pieces, *mode);
                    if *done < pieces {
                        let p = first + *done;
                        let off = mode == 3 && self.differs[at as usize - 1] == 0;
                        if !off {
                            let flag = self.content[p as usize] != self.hashes[p as usize];
                            let mut r = self.result(p, flag);
                            if mode == 3 && r == SKIPPED {
                                r = 0xffff_fffd;
                            }
                            self.differs[(at + *done) as usize] = r;
                            self.hashes[p as usize] = self.content[p as usize];
                        }
                        *done += 1;
                        if *done < pieces {
                            return true;
                        }
                    }
                }
            }
            queue.pop_front();
            true
        }
    }

    /// Plans one after another, as the relay sends them, the card running
    /// behind them by a random stretch: each writes random pieces of a copy
    /// of 48 and may send title bytes to others first; each plan's compare
    /// may be called off by the next (`most` plans a compare at most), at
    /// whatever moment the card is at. When the card has run a plan and
    /// [`resolve`] says its pieces are known, every piece it wrote is flagged
    /// (the relay then fetches it), and no piece nothing ever wrote or sent
    /// to is. Returns compares called off before the card reached them, and
    /// those it ran part of.
    fn model(seed: u64, most: u32, plans: usize) -> (usize, usize) {
        const PIECES: u32 = 48;
        let mut seed = seed;
        let mut next = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n.max(1)
        };
        let mut card = Card { content: vec![0; PIECES as usize], hashes: vec![0; PIECES as usize], differs: vec![0; 1 << 16], completed: 0 };
        let (mut queue, mut version, mut taken) = (std::collections::VecDeque::new(), 0u64, 0u32);
        struct Made {
            writes: Vec<u32>,
            at: u32,
            joined: Option<Joined>,
            own: u64,
        }
        let mut made: Vec<Made> = Vec::new();
        // Pieces anything wrote or sent to so far.
        let mut ever = [false; PIECES as usize];
        // (offer's plan, its depth)
        let mut offers: Vec<(usize, u32)> = Vec::new();
        let (mut before, mut partly) = (0, 0);
        let whole = CardSpan { from: 0, to: PIECES << 16, first: 0, pieces: PIECES, skew: 0 };
        for i in 0..plans {
            // The card runs on meanwhile.
            for _ in 0..next(40) {
                card.step(&mut queue, &mut version);
            }
            // The plan before's compares this one calls off: it covers them.
            let calls: Vec<(usize, u32)> = std::mem::take(&mut offers).into_iter().filter(|&(_, d)| d + 1 < most).collect();
            // Title bytes up to some pieces: compared first for each call.
            let sends: Vec<u32> = (0..next(4)).map(|_| next(u64::from(PIECES)) as u32).collect();
            let mut pre = Vec::new();
            if !calls.is_empty() {
                for &p in &sends {
                    pre.push(Pass { at: taken, first: p, pieces: 1 });
                    queue.push_back(Op::Compare(taken, p, 1, 2, 0));
                    taken += 1;
                }
            }
            if !sends.is_empty() {
                queue.push_back(Op::Send(sends.clone()));
            }
            let writes: Vec<u32> = (0..next(6)).map(|_| next(u64::from(PIECES)) as u32).collect();
            for &p in sends.iter().chain(&writes) {
                ever[p as usize] = true;
            }
            let depth = calls.iter().map(|c| c.1 + 1).max().unwrap_or(0);
            let joined = (depth + 1 < most).then(|| {
                card.differs[taken as usize] = GO;
                for p in 0..PIECES {
                    card.differs[(taken + 1 + p) as usize] = SKIPPED;
                }
                taken += 1;
                Joined { called: Arc::new(Called::default()), depth }
            });
            let at = taken;
            taken += PIECES;
            let own = 2 * i as u64 + 2;
            queue.push_back(Op::Lists(writes.clone()));
            queue.push_back(Op::Compare(at, 0, PIECES, if joined.is_some() { 3 } else { 2 }, 0));
            queue.push_back(Op::Fence(own));
            let fence = Arc::new(AtomicU64::new(0));
            // Called off now, wherever the card is.
            for (j, _) in calls {
                let m: &Made = &made[j];
                let called = &m.joined.as_ref().expect("offered").called;
                *called.0.lock().unwrap() = Some(Link {
                    pre: pre.clone(),
                    by: Pass { at, first: whole.first, pieces: whole.pieces },
                    next: joined.as_ref().map(|j| j.called.clone()),
                    fence: fence.clone(),
                });
                // Where the card is at that compare.
                let progress = queue.iter().find_map(|op| match op {
                    Op::Compare(a, _, _, _, done) if *a == m.at => Some(*done),
                    _ => None,
                });
                match progress {
                    Some(0) => before += 1,
                    Some(_) => partly += 1,
                    None => {}
                }
                card.differs[m.at as usize - 1] = 0;
            }
            // Sent.
            fence.store(own, Ordering::Release);
            if joined.is_some() {
                offers.push((made.len(), depth));
            }
            made.push(Made { writes, at, joined, own });
            // Landings, as far as the card is.
            for m in made.iter().filter(|m| m.own <= card.completed) {
                for p in 0..PIECES {
                    match resolve(&card.differs, m.at, 0, m.joined.as_ref().map(|j| &j.called), p, card.completed) {
                        Ok(r) => {
                            let flag = torn::flagged(r);
                            assert!(!m.writes.contains(&p) || flag, "plan {i}: piece {p} written, not flagged");
                            assert!(ever[p as usize] || !flag, "piece {p} flagged, nothing ever wrote it");
                        }
                        Err(Wait::Fence(f)) => assert!(f > card.completed && f > m.own),
                        Err(Wait::Unsent) => panic!("a compare that covers it is always sent here"),
                    }
                }
            }
        }
        while card.step(&mut queue, &mut version) {}
        for m in &made {
            for p in 0..PIECES {
                let r = resolve(&card.differs, m.at, 0, m.joined.as_ref().map(|j| &j.called), p, card.completed).expect("known");
                assert!(!m.writes.contains(&p) || torn::flagged(r), "piece {p} written, not flagged in the end");
            }
        }
        (before, partly)
    }

    /// [`model`], seeds and lengths of chains: what a plan wrote is flagged
    /// for it whichever compares ran, and some compares are called off before
    /// the card reaches them, some while it is at them.
    #[test]
    fn what_a_flight_wrote_is_flagged_whichever_compares_ran() {
        let (mut before, mut partly) = (0, 0);
        for most in [2, 3, 4, 8] {
            for seed in 1..=60u64 {
                let (b, p) = model(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1, most, 80);
                before += b;
                partly += p;
            }
        }
        assert!(before > 100 && partly > 10, "called off before the card came to them {before}, in the middle {partly}");
    }

    /// Without calls nothing is called off and every piece's own result
    /// stands, as without the switch.
    #[test]
    fn a_compare_not_called_off_stands() {
        let differs = [7u32, 1, SKIPPED, 4];
        let called = Arc::new(Called::default());
        assert_eq!(resolve(&differs, 1, 10, None, 11, 0), Ok(SKIPPED));
        assert_eq!(resolve(&differs, 1, 10, Some(&called), 10, 0), Ok(1));
        // Marked and never called off: flagged (fetched).
        assert_eq!(resolve(&differs, 1, 10, Some(&called), 11, 0), Ok(SKIPPED));
        assert_eq!(resolve(&differs, 1, 10, Some(&called), 99, 0), Ok(1), "outside the results: fetched");
        // Called off: the covering compare's, once its fence is reached.
        let fence = Arc::new(AtomicU64::new(0));
        *called.0.lock().unwrap() = Some(Link {
            pre: vec![Pass { at: 0, first: 11, pieces: 1 }],
            by: Pass { at: 2, first: 10, pieces: 2 },
            next: None,
            fence: fence.clone(),
        });
        assert_eq!(resolve(&differs, 1, 10, Some(&called), 11, 0), Err(Wait::Unsent));
        fence.store(9, Ordering::Release);
        assert_eq!(resolve(&differs, 1, 10, Some(&called), 11, 8), Err(Wait::Fence(9)));
        // dword 3 (4: not flagged) with the pre pass's flag (7: flagged).
        assert_eq!(resolve(&differs, 1, 10, Some(&called), 11, 9), Ok(5));
    }

    #[test]
    fn declared_ranges_within_and_cover() {
        let outer = vec![(1, vec![(0, 100), (200, 300)]), (2, vec![(5, 6)])];
        assert!(within(&[(1, vec![(0, 10), (250, 300)])], &outer));
        assert!(within(&[(2, vec![(5, 6)]), (1, vec![(100, 100)])], &outer));
        assert!(!within(&[(1, vec![(90, 210)])], &outer));
        assert!(!within(&[(3, vec![(0, 1)])], &outer));
        assert!(within(&[], &outer));
        let span = |first, pieces| CardSpan { from: 0, to: 1, first, pieces, skew: 0 };
        assert_eq!(cover(3, 4, &[span(0, 2), span(2, 6)]), Some(1));
        assert_eq!(cover(3, 6, &[span(0, 2), span(2, 6)]), None);
        assert_eq!(cut(span(2, 6), 4, 10), Some(span(4, 4)));
        assert_eq!(cut(span(2, 2), 4, 10), None);
    }

    /// The test below wants a process of its own with the relay on:
    /// `VICTORY_D3D12_WARP=1 VICTORY_RELAY=2 VICTORY_RELAY_COPY=1 VICTORY_CARD_JOIN_MB=1 cargo test -p victory-gpu --lib
    /// --features diagnostics card_join -- --ignored --test-threads=1`.
    const LEN: usize = 4 << 20;

    /// Title memory of our own, write-watched, and the card copy of the title
    /// buffer it is (as `upcheck`'s tests have it).
    fn title(resource: usize) -> (&'static mut [u8], usize, Arc<Shadow>) {
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};
        const WRITE_WATCH: u32 = 0x20_0000;
        // SAFETY: memory of our own, never freed.
        let base = unsafe { VirtualAlloc(std::ptr::null(), LEN, MEM_RESERVE | MEM_COMMIT | WRITE_WATCH, PAGE_READWRITE) }.cast::<u8>();
        assert!(!base.is_null());
        crate::shadow::register(resource, base as u64, LEN as u64);
        let shadow = crate::shadow::find(base as u64, 64).expect("a copy");
        assert!(shadow.carded(), "the copy is in the card's memory");
        assert!(relay_exact() && relay_copy(), "set VICTORY_RELAY=2 VICTORY_RELAY_COPY=1");
        // SAFETY: `LEN` bytes just committed.
        (unsafe { std::slice::from_raw_parts_mut(base, LEN) }, base as usize, shadow)
    }

    /// A shader's stand-in: `bytes` go to offset `at` of the copy.
    fn write(host: &Host, list: &HostList, shadow: &Shadow, at: usize, bytes: &[u8]) {
        let (upload, data) = host.upload_buffer(bytes.len() as u64).unwrap();
        // SAFETY: the mapping; an open list and live buffers.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), data, bytes.len());
            list.list.CopyBufferRegion(&shadow.buffer, at as u64, &upload, 0, bytes.len() as u64);
            list.list.ResourceBarrier(&[transition(&shadow.buffer, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COMMON)]);
        }
        list.keep(&upload);
    }

    /// What the card's side holds, nothing being in the air.
    fn card(host: &Host, shadow: &Shadow) -> Vec<u8> {
        let (held, data) = host.shared_buffer(LEN as u64).unwrap();
        let (allocator, list) = host.make_prelude().unwrap();
        // SAFETY: objects of our own, waited for before they go; live buffers.
        unsafe {
            allocator.Reset().unwrap();
            list.Reset(&allocator, None).unwrap();
            list.CopyBufferRegion(&held, 0, &shadow.buffer, 0, LEN as u64);
            list.Close().unwrap();
            let fence: ID3D12Fence = host.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            let event = CreateEventW(None, false, false, None).unwrap();
            host.queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
            host.queue.Signal(&fence, 1).unwrap();
            fence.SetEventOnCompletion(1, event).unwrap();
            WaitForSingleObject(event, INFINITE);
            windows::Win32::Foundation::CloseHandle(event).ok();
            // The mapping; the copy has run.
            std::slice::from_raw_parts(data, LEN).to_vec()
        }
    }

    /// Declared writes beside the title's own in the same 64 KiB pieces,
    /// flights in the air behind work that keeps the card busy, as in
    /// `upcheck`'s `declared_writes_beside_the_titles_own_stay`. Title memory at
    /// every point where everything has landed, and what the card holds, are
    /// what was written last; and so the same with compares called off as
    /// without. Returns title memory at those points and what was called off.
    fn rounds(host: &Host, resource: usize, most: u32) -> (Vec<Vec<u8>>, [u64; 7]) {
        MOST_SET.store(most, Ordering::Relaxed);
        let (title, start, shadow) = title(resource);
        let slow = (host.uav_buffer(64 << 20).unwrap(), host.uav_buffer(64 << 20).unwrap());
        let mut seed = 0x1234_5678_9abc_def1u64;
        let mut next = move |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 11) as usize % n
        };
        // Pieces 0..8 are used, words of them by turns the title's (even)
        // and the shaders' (odd).
        const USED: usize = 8 << 16;
        let mut model = vec![0u8; USED];
        let (mut lists, mut seen) = (Vec::new(), Vec::new());
        let mut stats = [0u64; 7];
        for round in 0..240usize {
            for _ in 0..next(24) {
                let at = next(USED / 8) * 8;
                let value = ((round as u32) << 8 | next(256) as u32).to_le_bytes();
                title[at..at + 4].copy_from_slice(&value);
                model[at..at + 4].copy_from_slice(&value);
            }
            let list = host.new_list().unwrap();
            shadow.refresh(&list, start as u64, LEN as u64);
            if next(6) > 0 {
                shadow.gpu_writes(&list, start as u64, LEN as u64);
                // The card behind: a copy of 64 MB or two first.
                for _ in 0..1 + next(2) {
                    // SAFETY: an open list; live buffers.
                    unsafe {
                        list.list.CopyBufferRegion(&slow.1, 0, &slow.0, 0, 64 << 20);
                        list.list.ResourceBarrier(&[transition(&slow.1, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COMMON)]);
                    }
                }
                for _ in 0..1 + next(6) {
                    let at = next(USED / 8) * 8 + 4;
                    let value = (0x8000_0000 | (round as u32) << 8 | next(256) as u32).to_le_bytes();
                    write(host, &list, &shadow, at, &value);
                    model[at..at + 4].copy_from_slice(&value);
                }
            }
            list.close();
            host.execute_own(&list);
            lists.push(list);
            for (s, t) in stats.iter_mut().zip(&STATS) {
                *s += t.swap(0, Ordering::Relaxed);
            }
            if round % 30 == 29 {
                // Everything refreshed, landed and looked at.
                for _ in 0..2 {
                    let all = host.new_list().unwrap();
                    shadow.refresh(&all, start as u64, LEN as u64);
                    all.close();
                    host.execute_own(&all);
                    settle_now(WHY_SYNC);
                    lists.push(all);
                }
                let holds = card(host, &shadow);
                let wrong = |held: &[u8]| (0..USED / 4).filter(|w| held[w * 4..w * 4 + 4] != model[w * 4..w * 4 + 4]).collect::<Vec<_>>();
                let (on_card, in_title) = (wrong(&holds), wrong(title));
                assert!(
                    on_card.is_empty() && in_title.is_empty(),
                    "VICTORY_CARD_JOIN={most}, round {round}: words {on_card:x?} on the card and {in_title:x?} in title memory are \
                     not what was written last"
                );
                seen.push(title[..USED].to_vec());
                assert_eq!(host.log_messages(), None);
            }
        }
        for (s, t) in stats.iter_mut().zip(&STATS) {
            *s += t.swap(0, Ordering::Relaxed);
        }
        MOST_SET.store(u32::MAX, Ordering::Relaxed);
        (seen, stats)
    }

    #[test]
    #[ignore = "wants VICTORY_RELAY=2 VICTORY_RELAY_COPY=1 VICTORY_CARD_JOIN_MB=1 and a process of its own"]
    fn compares_called_off_land_what_the_compares_did() {
        let _views = crate::test_views_lock();
        let Some(host) = host() else {
            return;
        };
        assert_eq!(least_pieces(), 16, "set VICTORY_CARD_JOIN_MB=1");
        let (before, none) = rounds(host, 0x7a11, 0);
        let (after, stats) = rounds(host, 0x7a12, 4);
        eprintln!(
            "called off: {} of {} compares that could be, {} pieces not compared, {} passes ahead of sends over {} pieces, \
             {} flights waited {} us for the compare that covered theirs",
            stats[1], stats[0], stats[2], stats[3], stats[4], stats[5], stats[6]
        );
        assert_eq!(none[0], 0, "nothing offered with the switch off");
        assert!(stats[1] > 0 && stats[2] > 0, "some compares were called off before the card came to them: {stats:?}");
        assert_eq!(before, after, "title memory where everything has landed, without and with compares called off");
    }
}
