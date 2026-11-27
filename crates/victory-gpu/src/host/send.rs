//! What goes up to copies in the card's memory goes up as it was when its
//! submission was prepared (`VICTORY_SEND_OWN`, on by default; `0`: as
//! before).
//!
//! The sends were copies out of the copies' CPU sides, which the card makes
//! when it comes to the submission. By then later submissions are prepared
//! (two frames of them with `VICTORY_RELAY`), and their refreshes have
//! written the same CPU sides: the earlier submission's shaders then read a
//! later one's bytes. For what the title's CPU writes that is as on the
//! console (it does not write what the card still reads). But the title's
//! own buffer copies are commands in its lists, and those are done by us
//! when a submission is prepared: "a value to a buffer, a draw that reads
//! it" in one submission and the same with another value in the next is
//! ordinary, and both draws read the second value.
//!
//! So each submission's bytes are taken out of the CPU sides into memory of
//! the submission's own when they are taken from the dirty lists, and the
//! card copies them from there. Runs of a few words, which a refresh that
//! sends only what changed makes many of, go as pairs of offset and value,
//! stored by one dispatch per copy; the others as a copy each.

use super::*;
use crate::shadow::Shadow;

pub(super) fn own() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SEND_OWN").is_none_or(|v| v != "0"))
}

/// Runs up to this many bytes go as pairs.
const SHORT: usize = 256;

/// Whether a run of `len` bytes at offset `at` of a copy goes as pairs.
fn short(at: usize, len: usize) -> bool {
    len <= SHORT && at.is_multiple_of(4) && len.is_multiple_of(4)
}

/// Whether a run of `len` bytes at offset `at` of a copy in the card's memory
/// is no copy command of its own, but pairs of one dispatch.
pub(crate) fn goes_as_words(at: usize, len: usize) -> bool {
    own() && short(at, len)
}

/// Whether the title's bytes that go in under flights' writes
/// (`Shadow::merge_under_writes`) go in by whole words
/// (`VICTORY_UNDER_WORDS=0`: by the bytes that changed, as before).
///
/// By bytes nearly every run (a float's three low bytes) was a copy command
/// of its own, and a submission with more than 4,096 of them stood until the
/// flights had landed: milliseconds of a steady frame ("more changed runs
/// than a submission sends up"), for some 20 KB. Whole
/// words are pairs of one dispatch, and are not counted.
///
/// A word is taken to be the title's or the shaders', not shared: where the
/// title changed a byte of a word of which a flight's shader writes another
/// byte, that byte is as it was before the flight. The landing rests on the
/// same ([`crate::shadow::exact_back`]).
pub(crate) fn words_under() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| own() && std::env::var_os("VICTORY_UNDER_WORDS").is_none_or(|v| v != "0"))
}

const SCATTER: &str = r#"
#define RS "RootConstants(num32BitConstants=2, b0), SRV(t0), UAV(u0)"
cbuffer Part : register(b0) { uint at; uint count; };
ByteAddressBuffer pairs : register(t0);
RWByteAddressBuffer card : register(u0);
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    if (id.x >= count) return;
    uint2 p = pairs.Load2(at + id.x * 8u);
    card.Store(p.x, p.y);
}
"#;

struct Pass {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
}

fn pass(host: &Host) -> Option<&'static Pass> {
    static PASS: OnceLock<Option<Pass>> = OnceLock::new();
    PASS.get_or_init(|| {
        let code = compile(SCATTER, windows::core::s!("main"), windows::core::s!("cs_5_1")).ok()?;
        // SAFETY: the code lives through the calls; the root signature is
        // inside it.
        unsafe {
            let root: ID3D12RootSignature = host.device.CreateRootSignature(0, &code).ok()?;
            let pso: ID3D12PipelineState = host
                .device
                .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&root),
                    CS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: code.as_ptr().cast(),
                        BytecodeLength: code.len(),
                    },
                    ..Default::default()
                })
                .ok()?;
            name(&pso, || "victory: changed words sent to the card".into());
            Some(Pass { root, pso })
        }
    })
    .as_ref()
}

/// Memory a submission's sends come from, mapped.
pub(super) struct Memory {
    buffer: ID3D12Resource,
    data: usize,
    size: u64,
}

// SAFETY: a D3D12 object (free-threaded) and its mapping, with one owner.
unsafe impl Send for Memory {}

/// What submissions that have run gave back.
static SPARE: Mutex<Vec<Memory>> = Mutex::new(Vec::new());

/// At most this much is kept ([`keep_for`]), read once.
fn kept() -> u64 {
    static KEPT: OnceLock<u64> = OnceLock::new();
    *KEPT.get_or_init(|| {
        let mb = std::env::var("VICTORY_SEND_KEEP_MB").ok().and_then(|v| v.trim().parse().ok());
        keep_for(mb, on_card(), total_physical_ram())
    })
}

/// The most bytes of sends' memory kept while no submission holds it:
/// `explicit_mb` (`VICTORY_SEND_KEEP_MB`) if given; else 256 MB of the card's
/// memory with [`on_card`]; else 128 MB on a PC with `ram` of 24 GiB or less,
/// 768 MB on others (and there before: `VICTORY_SEND_KEEP_MB=768`). On a
/// small PC what it holds is the PC's memory that runs out, and a frame's
/// sends are some tens of megabytes, in buffers of 8 MiB and more.
fn keep_for(explicit_mb: Option<u64>, on_card: bool, ram: Option<u64>) -> u64 {
    match explicit_mb {
        Some(mb) => mb.saturating_mul(1 << 20),
        None if on_card => 256 << 20,
        None if ram.is_some_and(|bytes| bytes <= 24 << 30) => 128 << 20,
        None => 768 << 20,
    }
}

/// Memory ran out (`super::budget::relieve`): the spare memory of sends goes,
/// the biggest first, until `need` bytes have gone; their bytes. The lock is
/// only tried: the caller may hold it.
pub(super) fn relieve(need: u64) -> u64 {
    let gone: Vec<Memory> = {
        let mut spare = match SPARE.try_lock() {
            Ok(spare) => spare,
            Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return 0,
        };
        // (Kept the biggest first, `give_back`.)
        let (mut n, mut bytes) = (0, 0);
        while n < spare.len() && bytes < need {
            bytes += spare[n].size;
            n += 1;
        }
        spare.drain(..n).collect()
    };
    // Freed outside the lock.
    gone.iter().map(|m| m.size).sum()
}

/// For the memory line: the bytes of sends' memory kept now.
#[cfg(feature = "diagnostics")]
pub(super) fn spare_bytes() -> u64 {
    SPARE.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|m| m.size).sum()
}

/// For the run report: the bytes of sends' memory kept now, or `None` while
/// the list is in use (nothing is waited for).
pub(super) fn report_spare() -> Option<u64> {
    match SPARE.try_lock() {
        Ok(spare) => Some(spare.iter().map(|m| m.size).sum()),
        Err(std::sync::TryLockError::Poisoned(e)) => Some(e.into_inner().iter().map(|m| m.size).sum()),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

/// Whether the memory lies in the card's memory, written by the CPU through
/// the bus (`VICTORY_SEND_BAR=1`): the copies on the card's drawing queue, which
/// nothing overlaps, then cross no bus. In a captured frame they took a sixth
/// of the card's time, and take next to none this way; but filling it takes the
/// submitting thread three times as long, so it is a loss while the card waits
/// for that thread: off by
/// default.
fn on_card() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SEND_BAR").is_some_and(|v| v == "1"))
}

fn memory(host: &Host, size: u64) -> Option<Memory> {
    {
        let mut spare = SPARE.lock().unwrap_or_else(|e| e.into_inner());
        let fit = spare.iter().enumerate().filter(|(_, m)| m.size >= size).min_by_key(|(_, m)| m.size).map(|(i, _)| i);
        if let Some(i) = fit {
            return Some(spare.swap_remove(i));
        }
    }
    // By doubles, so that the next of about this size finds it: what
    // submissions send differs from frame to frame.
    let size = size.max(8 << 20).next_power_of_two();
    let mapped = if on_card() { host.mapped_card_buffer(size, false) } else { None };
    let (buffer, data) = match mapped {
        Some(made) => made,
        None => host.upload_buffer(size)?,
    };
    name(&buffer, || "bytes a submission sends up".into());
    Some(Memory {
        buffer,
        data: data as usize,
        size,
    })
}

/// The submission has run.
pub(super) fn give_back(memory: Memory) {
    let mut spare = SPARE.lock().unwrap_or_else(|e| e.into_inner());
    spare.push(memory);
    spare.sort_by_key(|m| std::cmp::Reverse(m.size));
    while spare.iter().map(|m| m.size).sum::<u64>() > kept() {
        spare.remove(0);
    }
}

/// Since the last frame-rate line: bytes copied, runs copied, words stored,
/// microseconds of taking them out of the CPU sides.
pub(super) static STATS: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// One copy's sends: ranges copied (offset in the copy, in the memory,
/// bytes), and its pairs (where in the memory, how many).
struct Copy {
    runs: Vec<(usize, u64, usize)>,
    pairs: (u64, u32),
}

/// Where a submission's bytes are taken to.
pub(super) struct Own {
    memory: Memory,
    copies: Vec<Copy>,
}

/// Runs whose pairs are one piece of work.
const RUNS_AT_A_TIME: usize = 4096;

/// Runs of a copy's sends that lie fewer than this many bytes apart go up as
/// one, the bytes between them with them (`VICTORY_UNDER_COALESCE=<bytes>`; 0,
/// the default: each run as it is, as before), where those bytes are known
/// to be what the card's side holds already: no range that a submission's
/// shaders may be writing touches them ([`Shadow::writes_pending_in`]: flights
/// in the air, and the submission being prepared). The card's side then holds
/// there what the CPU's side holds (what `VICTORY_SEND_CHANGED` rests on too:
/// the CPU's side is what the card's side was last known to hold, for every
/// range no shader may be writing), so sending them changes no byte on the
/// card, and none of title memory.
///
/// Why 0 by default: the runs it was meant for, the title's bytes put in under
/// flights' writes (`Shadow::merge_under_writes`, hundreds of thousands of
/// runs for a megabyte or so a frame), all lie in ranges flights may be
/// writing, by
/// what that is; between two of them the card's side may hold a shader's
/// write the CPU's side does not have yet, which the bytes from the CPU's side
/// would undo (and with the relay `Shadow::card_merge_relay` takes such sent
/// bytes for the title's, so the write would be lost from title memory too).
/// There nothing is joined, whatever the switch says, and what it may join
/// elsewhere (sends of refreshes, which take whole pages) is a few hundred
/// copies a frame. Nor is it exact for a copy shaders store into outside
/// what is declared (`Shadow::keep_stores`): such a store between two runs
/// would be undone too; that cannot be told from here.
pub(super) fn coalesce_gap() -> usize {
    static GAP: OnceLock<usize> = OnceLock::new();
    *GAP.get_or_init(|| std::env::var("VICTORY_UNDER_COALESCE").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0))
}

/// `ranges` (offsets of a copy, in order and apart), those fewer than `gap`
/// bytes apart joined where `free(from, to)` says the bytes between them may go
/// up with them. Every byte of `ranges` is in the result, and the only others
/// are those of gaps `free` let through.
pub(super) fn coalesced(ranges: &[(usize, usize)], gap: usize, free: impl Fn(usize, usize) -> bool) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for &(from, to) in ranges {
        match out.last_mut() {
            // (Ranges that touch have no bytes between them.)
            Some(last) if from <= last.1 => last.1 = last.1.max(to),
            Some(last) if from - last.1 < gap && free(last.1, from) => last.1 = to,
            _ => out.push((from, to)),
        }
    }
    out
}

/// Since the last frame-rate line, with [`coalesce_gap`]: runs before the
/// joining, runs after it, bytes between them sent with them.
pub(super) static COALESCED: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// Takes what `dirty` sends up out of the CPU sides.
pub(super) fn take(host: &Host, dirty: &[(std::sync::Arc<Shadow>, Vec<(usize, usize)>)]) -> Option<Own> {
    let began = std::time::Instant::now();
    pass(host)?;
    // With `coalesce_gap`: what goes up of each copy, runs joined over bytes
    // no shader may be writing.
    let joined: Vec<Vec<(usize, usize)>>;
    let dirty: Vec<(&std::sync::Arc<Shadow>, &[(usize, usize)])> = match coalesce_gap() {
        0 => dirty.iter().map(|(shadow, ranges)| (shadow, &ranges[..])).collect(),
        gap => {
            joined = dirty
                .iter()
                .map(|(shadow, ranges)| {
                    let start = shadow.span().0 as usize;
                    let out = coalesced(ranges, gap, |a, b| !shadow.writes_pending_in(start + a, start + b));
                    let bytes = |r: &[(usize, usize)]| r.iter().map(|r| (r.1 - r.0) as u64).sum::<u64>();
                    COALESCED[0].fetch_add(ranges.len() as u64, Ordering::Relaxed);
                    COALESCED[1].fetch_add(out.len() as u64, Ordering::Relaxed);
                    COALESCED[2].fetch_add(bytes(&out).saturating_sub(bytes(ranges)), Ordering::Relaxed);
                    out
                })
                .collect();
            dirty.iter().zip(&joined).map(|((shadow, _), ranges)| (shadow, &ranges[..])).collect()
        }
    };
    // Runs of whole words of the copy: only those are pairs. (A byte beside
    // them in a word may be the card's.)
    let short = |&(a, b): &(usize, usize)| short(a, b - a);
    let mut at = 0u64;
    let mut copies = Vec::with_capacity(dirty.len());
    for (_, ranges) in &dirty {
        let mut runs = Vec::new();
        for &(a, b) in ranges.iter().filter(|r| !short(r)) {
            runs.push((a, at, b - a));
            at += ((b - a) as u64).next_multiple_of(16);
        }
        let words: usize = ranges.iter().filter(|r| short(r)).map(|r| (r.1 - r.0) / 4).sum();
        copies.push(Copy { runs, pairs: (at, words as u32) });
        at += (words as u64 * 8).next_multiple_of(16);
    }
    if at == 0 {
        return None;
    }
    let memory = memory(host, at)?;
    let mut moves: Vec<(usize, usize, usize)> = Vec::new();
    let mut work: Vec<(usize, &[(usize, usize)], usize)> = Vec::new();
    for ((shadow, ranges), copy) in dirty.iter().zip(&copies) {
        let from = shadow.card_data() as usize;
        moves.extend(copy.runs.iter().map(|&(a, to, n)| (from + a, memory.data + to as usize, n)));
        // (80,000 runs of some words a frame: in pieces of work.)
        let mut to = memory.data + copy.pairs.0 as usize;
        for ranges in ranges.chunks(RUNS_AT_A_TIME) {
            let words: usize = ranges.iter().filter(|r| short(r)).map(|r| (r.1 - r.0) / 4).sum();
            if words > 0 {
                work.push((from, ranges, to));
                to += words * 8;
            }
        }
    }
    let one = |i: usize| {
        let (from, ranges, to) = work[i];
        let mut to = to as *mut [u32; 2];
        for &(a, b) in ranges.iter().filter(|r| short(r)) {
            for offset in (a..b).step_by(4) {
                // SAFETY: inside the copy's mapping (its CPU side, which
                // the thread that asked, and waits, alone writes in these
                // ranges) and inside the memory, which has room for every
                // pair, a piece of work's behind the last one's.
                unsafe {
                    let word = ((from + offset) as *const u32).read_unaligned();
                    to.write([offset as u32, word]);
                    to = to.add(1);
                }
            }
        }
    };
    crate::land::each_under(work.len(), &one);
    STATS[0].fetch_add(moves.iter().map(|m| m.2 as u64).sum(), Ordering::Relaxed);
    STATS[1].fetch_add(moves.len() as u64, Ordering::Relaxed);
    STATS[2].fetch_add(copies.iter().map(|c| u64::from(c.pairs.1)).sum(), Ordering::Relaxed);
    crate::shadow::copy_all(moves);
    STATS[3].fetch_add(began.elapsed().as_micros() as u64, Ordering::Relaxed);
    Some(Own { memory, copies })
}

impl Own {
    /// Records the sends into `list`; the copies that `looked` says are in
    /// `COPY_DEST`, the others in the common state. Those with a "before" are
    /// left readable by compute shaders.
    ///
    /// # Safety
    /// An open list of ours, whose compute state is set anew behind this.
    pub(super) unsafe fn record(
        &self,
        host: &Host,
        list: &ID3D12GraphicsCommandList,
        dirty: &[(std::sync::Arc<Shadow>, Vec<(usize, usize)>)],
        looked: impl Fn(&Shadow) -> bool,
    ) {
        let read = D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE;
        let write = D3D12_RESOURCE_STATE_UNORDERED_ACCESS;
        let dest = D3D12_RESOURCE_STATE_COPY_DEST;
        let (mut first, mut last) = (Vec::new(), Vec::new());
        for ((shadow, _), copy) in dirty.iter().zip(&self.copies) {
            for &(a, from, n) in &copy.runs {
                // SAFETY: the caller's; live buffers, ranges inside them.
                unsafe { list.CopyBufferRegion(&shadow.buffer, a as u64, &self.memory.buffer, from, n as u64) };
            }
            let mut state = (looked(shadow) || !copy.runs.is_empty()).then_some(dest);
            if copy.pairs.1 > 0 {
                if let Some(was) = state {
                    first.push(transition(&shadow.buffer, was, write));
                }
                state = Some(write);
            }
            let hashed = shadow.card_sides().is_some_and(|(_, before, _)| before.is_some());
            if let Some(was) = state
                && hashed
                && (!copy.runs.is_empty() || copy.pairs.1 > 0)
            {
                last.push(transition(&shadow.buffer, was, read));
            }
        }
        // SAFETY: the caller's; live objects.
        unsafe {
            if !first.is_empty() {
                list.ResourceBarrier(&first);
            }
            if let Some(pass) = pass(host)
                && self.copies.iter().any(|c| c.pairs.1 > 0)
            {
                list.SetComputeRootSignature(&pass.root);
                list.SetPipelineState(&pass.pso);
                list.SetComputeRootShaderResourceView(1, self.memory.buffer.GetGPUVirtualAddress());
                for ((shadow, _), copy) in dirty.iter().zip(&self.copies).filter(|(_, c)| c.pairs.1 > 0) {
                    list.SetComputeRootUnorderedAccessView(2, shadow.buffer.GetGPUVirtualAddress());
                    const MOST: u32 = 64 * D3D12_CS_DISPATCH_MAX_THREAD_GROUPS_PER_DIMENSION;
                    let (at, words) = copy.pairs;
                    let mut done = 0;
                    while done < words {
                        let n = (words - done).min(MOST);
                        let constants = [at as u32 + done * 8, n];
                        list.SetComputeRoot32BitConstants(0, 2, constants.as_ptr().cast(), 0);
                        list.Dispatch(n.div_ceil(64), 1, 1);
                        done += n;
                    }
                }
            }
            if !last.is_empty() {
                list.ResourceBarrier(&last);
            }
        }
    }

    /// The memory, to be kept until the submission has run.
    pub(super) fn memory(self) -> Memory {
        self.memory
    }
}

#[cfg(test)]
mod tests {
    use super::coalesced;

    /// What is kept of sends' memory: what is set; 256 MB of the card's with
    /// `VICTORY_SEND_BAR=1`; 128 MB on a PC of 24 GiB or less; 768 MB on
    /// bigger ones and where the PC's memory is not known.
    #[test]
    fn sends_keep_less_on_small_pcs() {
        use super::keep_for;
        let (gib, mib) = (1u64 << 30, 1u64 << 20);
        assert_eq!(keep_for(None, false, Some(16 * gib)), 128 * mib);
        assert_eq!(keep_for(None, false, Some(24 * gib)), 128 * mib);
        assert_eq!(keep_for(None, false, Some(24 * gib + 1)), 768 * mib);
        assert_eq!(keep_for(None, false, None), 768 * mib);
        assert_eq!(keep_for(None, true, Some(16 * gib)), 256 * mib);
        assert_eq!(keep_for(Some(768), false, Some(16 * gib)), 768 * mib);
        assert_eq!(keep_for(Some(0), true, None), 0);
    }

    /// The next of a sequence of numbers that look random (xorshift).
    fn next(seed: &mut u64) -> u64 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        *seed
    }

    /// Ranges in order and apart, some of `len` bytes: (from, to).
    fn runs(seed: &mut u64, len: usize) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut at = (next(seed) % 16) as usize;
        while at < len {
            let n = 1 + (next(seed) % 12) as usize;
            let to = (at + n).min(len);
            out.push((at, to));
            at = to + 1 + (next(seed) % 40) as usize;
        }
        out
    }

    fn meets(ranges: &[(usize, usize)], from: usize, to: usize) -> bool {
        ranges.iter().any(|&(a, b)| a < to && from < b)
    }

    /// The joined runs hold every byte of the runs; besides them only the
    /// bytes of gaps shorter than asked that `free` let through; and with no
    /// gap, or nothing free, they are the runs.
    #[test]
    fn joined_runs_cover_the_runs_and_only_free_gaps() {
        let mut seed = 0x9e37_79b9_7f4a_7c15;
        for round in 0..200 {
            let len = 64 + (next(&mut seed) % 2000) as usize;
            let runs = runs(&mut seed, len);
            let busy = runs_of_busy(&mut seed, len);
            let gap = (next(&mut seed) % 64) as usize;
            let free = |a: usize, b: usize| !meets(&busy, a, b);
            let joined = coalesced(&runs, gap, free);
            for at in 0..len {
                let was = meets(&runs, at, at + 1);
                let now = meets(&joined, at, at + 1);
                assert!(!was || now, "round {round}: byte {at} of a run left out");
                if now && !was {
                    // A gap's byte: the gap is short and free.
                    let before = runs.iter().rev().find(|r| r.1 <= at).expect("a run before it");
                    let after = runs.iter().find(|r| r.0 > at).expect("a run after it");
                    assert!(after.0 - before.1 < gap, "round {round}: byte {at} of a long gap");
                    assert!(free(before.1, after.0), "round {round}: byte {at} of a busy gap");
                }
            }
            // In order and apart.
            assert!(joined.windows(2).all(|w| w[0].1 < w[1].0), "round {round}");
            assert_eq!(coalesced(&runs, 0, |_, _| true), runs);
            assert_eq!(coalesced(&runs, usize::MAX, |_, _| false), runs);
        }
        // Ranges that touch are one, whatever is asked.
        assert_eq!(coalesced(&[(0, 4), (4, 8), (10, 12)], 0, |_, _| false), [(0, 8), (10, 12)]);
        assert_eq!(coalesced(&[(0, 4), (6, 8), (60, 64)], 3, |_, _| true), [(0, 8), (60, 64)]);
        assert!(coalesced(&[], 64, |_, _| true).is_empty());
    }

    /// Ranges shaders may be writing: in order and apart.
    fn runs_of_busy(seed: &mut u64, len: usize) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut at = (next(seed) % 64) as usize;
        while at < len {
            let to = (at + 1 + (next(seed) % 30) as usize).min(len);
            out.push((at, to));
            at = to + 20 + (next(seed) % 200) as usize;
        }
        out
    }

    /// On random bytes: the card's side after the joined runs went up is
    /// what it is after the runs went up one by one, where the gaps a shader
    /// may be writing are never let through; let through, the card's bytes
    /// there are undone.
    #[test]
    fn joined_sends_leave_the_card_as_the_runs_do() {
        let mut seed = 0x2545_f491_4f6c_dd1d;
        let mut undone = 0;
        for round in 0..300 {
            let len = 64 + (next(&mut seed) % 3000) as usize;
            // The CPU's side; the card's side is the same but where shaders
            // wrote (ranges they may be writing), and where the title's bytes
            // came into the CPU's side (the runs to send).
            let held: Vec<u8> = (0..len).map(|_| next(&mut seed) as u8).collect();
            let busy = runs_of_busy(&mut seed, len);
            let mut card = held.clone();
            for &(a, b) in &busy {
                for byte in &mut card[a..b] {
                    *byte = byte.wrapping_add(1 + (next(&mut seed) % 255) as u8);
                }
            }
            let runs = runs(&mut seed, len);
            let mut held = held;
            for &(a, b) in &runs {
                for byte in &mut held[a..b] {
                    *byte = next(&mut seed) as u8;
                }
            }
            let send = |runs: &[(usize, usize)]| {
                let mut card = card.clone();
                for &(a, b) in runs {
                    card[a..b].copy_from_slice(&held[a..b]);
                }
                card
            };
            let one_by_one = send(&runs);
            let gap = 1 + (next(&mut seed) % 128) as usize;
            let joined = coalesced(&runs, gap, |a, b| !meets(&busy, a, b));
            assert_eq!(send(&joined), one_by_one, "round {round}");
            // Without the look at what shaders may write.
            let blind = coalesced(&runs, gap, |_, _| true);
            undone += usize::from(send(&blind) != one_by_one);
        }
        assert!(undone > 0, "joining over what shaders write undoes it");
    }
}
