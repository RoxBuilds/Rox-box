//! Write-combined title memory, and what shaders write into big copies of it
//! left on the card (`VICTORY_WC_NO_BACK=1`, off by default).
//!
//! Ported from trustno1-coder/VIctory-NVIDIA 50c15a1 and 2f81d0c (the minimum
//! size); its mapped single copy of such memory (`VICTORY_WC_BAR`) is not
//! taken.
//!
//! The title asks for some of its memory write-combined (the console's
//! protection bit 0x400): memory its CPU writes and the GPU reads. Its CPU
//! does not read it back (reads of it are uncached on the console, so a title
//! puts what its CPU reads in cacheable memory). Much of what our copy-back
//! of shader writes compares, fetches and merges each frame can lie in such
//! memory. With the switch, a title buffer's copy with a card side
//! (`super::cards`) of at least `VICTORY_WC_NO_BACK_MIN_KB` that lies whole
//! in one region the title asked write-combined declares no shader writes:
//! no snapshot, no compare on the card, no fetch and no merge into title
//! memory, and no wait on their account when the title writes the same
//! memory. What shaders wrote stays in the card side, where later shaders
//! read it.
//!
//! What it risks:
//! - the title's CPU reading what shaders wrote there reads older bytes (it
//!   should not read such memory; one that does sees what was there before);
//! - a page the title writes goes up from title memory as before, whole, and
//!   so takes back what shaders wrote in the same page since: such mixed pages
//!   lose the shaders' bytes, which with the copy-back were in title memory;
//! - another host copy of the same bytes (a texture placed there, a buffer
//!   aliasing it) does not see what shaders wrote;
//! - small results the title does read back (exposure and the like) came back
//!   stale with every size left on the card: the minimum, 1 MiB unless set,
//!   keeps those copied back while the large ranges, where most of the bytes
//!   are, stay on the card.
//!
//! Windows of title memory outside buffers are left out: one idle for a while
//! is let go and made again from title memory, which would lose what was left
//! on its card side.

use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Regions of title memory the title asked for write-combined, `[start,
/// end)`, sorted, apart from one another (touching ones joined).
static REGIONS: RwLock<Vec<(u64, u64)>> = RwLock::new(Vec::new());

/// Changes of [`REGIONS`], from 1: a copy's answer ([`cached`]) holds while
/// this is what it was found with.
static CHANGES: AtomicU64 = AtomicU64::new(1);

/// The title asked for `[at, at + len)` write-combined.
pub fn asked(at: usize, len: usize) {
    if len == 0 {
        return;
    }
    let (from, to) = (at as u64, (at as u64).saturating_add(len as u64));
    let mut all = REGIONS.write().unwrap_or_else(|e| e.into_inner());
    if joined(&mut all, from, to) {
        CHANGES.fetch_add(1, Relaxed);
    }
}

/// `[from, to)` into `all` (sorted, apart): joined with the regions it
/// touches. Whether `all` changed.
fn joined(all: &mut Vec<(u64, u64)>, mut from: u64, mut to: u64) -> bool {
    if all.iter().any(|&(a, e)| a <= from && to <= e) {
        return false;
    }
    all.retain(|&(a, e)| {
        let touches = a <= to && from <= e;
        if touches {
            from = from.min(a);
            to = to.max(e);
        }
        !touches
    });
    let at = all.partition_point(|&(a, _)| a < from);
    all.insert(at, (from, to));
    true
}

/// The title lets go of `[at, at + len)` (`len` 0: of the whole allocation at
/// `at`): the regions that touch it are forgotten whole (a part of one left
/// is no longer known to be what the title asked).
pub(crate) fn freed(at: usize, len: usize) {
    if REGIONS.read().unwrap_or_else(|e| e.into_inner()).is_empty() {
        return;
    }
    // (Before the system frees it: the allocation is still there to ask.)
    let end = if len == 0 { crate::watch::allocation_end(at, at).max(at + 1) } else { at.saturating_add(len) };
    let mut all = REGIONS.write().unwrap_or_else(|e| e.into_inner());
    if forget(&mut all, at as u64, end as u64) {
        CHANGES.fetch_add(1, Relaxed);
    }
}

/// The regions of `all` that touch `[from, to)` taken out. Whether any was.
fn forget(all: &mut Vec<(u64, u64)>, from: u64, to: u64) -> bool {
    let before = all.len();
    all.retain(|&(a, e)| e <= from || a >= to);
    all.len() != before
}

/// Whether all of `[start, start + len)` lies in one region the title asked
/// for write-combined.
pub(crate) fn holds(start: u64, len: u64) -> bool {
    let all = REGIONS.read().unwrap_or_else(|e| e.into_inner());
    let at = all.partition_point(|&(a, _)| a <= start);
    at > 0 && start.saturating_add(len) <= all[at - 1].1
}

/// [`holds`] for a copy, with its answer kept in `cache` (0: not asked;
/// else the changes it was found with, shifted, and the answer in bit 0).
pub(crate) fn cached(cache: &AtomicU64, start: u64, len: u64) -> bool {
    let changes = CHANGES.load(Relaxed);
    let known = cache.load(Relaxed);
    if known >> 1 == changes {
        return known & 1 != 0;
    }
    let yes = holds(start, len);
    cache.store(changes << 1 | u64::from(yes), Relaxed);
    yes
}

/// Whether what shaders write into a title buffer's copy of write-combined
/// memory stays on the card and is not copied back into title memory
/// (`VICTORY_WC_NO_BACK=1`; off by default: compared, fetched and merged
/// into title memory as any other copy's). Only copies with a card side, of
/// at least [`min_bytes`], lying whole in one region the title asked for
/// write-combined; see the module's text for what it risks.
///
/// Ported from trustno1-coder/VIctory-NVIDIA 50c15a1. It changes what the
/// title's CPU would read back of that memory, so it is off unless set.
pub(crate) fn no_back() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| crate::switch::on("VICTORY_WC_NO_BACK"))
}

/// The smallest copy whose shader writes [`no_back`] leaves on the card, in
/// bytes (`VICTORY_WC_NO_BACK_MIN_KB=<KiB>`; 1024 if not given; `0`: every
/// size). With every size left on the card, small results the title reads
/// back came back stale and the picture went dark, while most of the bytes
/// were in a few ranges of many megabytes. Ported from
/// trustno1-coder/VIctory-NVIDIA 2f81d0c (there 0 unless set, and run with
/// 1024).
pub(crate) fn min_bytes() -> u64 {
    static MIN: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MIN.get_or_init(|| min_of(std::env::var("VICTORY_WC_NO_BACK_MIN_KB").ok().as_deref()))
}

/// [`min_bytes`] of the variable's value.
fn min_of(set: Option<&str>) -> u64 {
    set.and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(1024).saturating_mul(1024)
}

/// Whether a copy's shader writes stay on the card: with the switch `on` and
/// the least size `min`, of a copy of `len` bytes with or without a card
/// side, a window or not, whose memory is write-combined or not (`wc`, asked
/// last: it takes a lock the first time).
pub(crate) fn leaves(on: bool, min: u64, card: bool, window: bool, len: u64, wc: impl FnOnce() -> bool) -> bool {
    on && card && !window && len >= min && wc()
}

/// Since the last frame line: bytes shaders were declared to write in copies
/// of write-combined memory that were left on the card, and the declarations.
static LEFT: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// A declaration of `len` bytes left on the card.
pub(crate) fn left(len: u64) {
    LEFT[0].fetch_add(len, Relaxed);
    LEFT[1].fetch_add(1, Relaxed);
}

/// The frame line's: MB a frame left on the card, and declarations (and the
/// run report's `shader_writes_left_on_card_mb`).
pub(crate) fn line(frames: f64) {
    let [bytes, times] = [0, 1].map(|i| LEFT[i].swap(0, Relaxed));
    crate::runreport::spared(victory_pipeline::report::Figure::SparedLeftOnCardBytes, bytes);
    let [bytes, times] = [bytes as f64 / frames, times as f64 / frames];
    if times > 0.0 {
        tracing::info!(
            "shader writes left on the card in copies of write-combined memory, per frame (VICTORY_WC_NO_BACK=1): {:.1} MB \
             declared in {times:.0} declarations, not compared or copied back",
            bytes / 1e6
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regions join where they touch and are forgotten whole where any of
    /// them is let go.
    #[test]
    fn regions_join_and_go_whole() {
        let mut all = Vec::new();
        assert!(joined(&mut all, 0x10_000, 0x20_000));
        assert!(joined(&mut all, 0x40_000, 0x50_000));
        assert!(!joined(&mut all, 0x11_000, 0x12_000), "inside one");
        assert!(joined(&mut all, 0x20_000, 0x30_000));
        assert_eq!(all, [(0x10_000, 0x30_000), (0x40_000, 0x50_000)]);
        assert!(joined(&mut all, 0x2f_000, 0x41_000));
        assert_eq!(all, [(0x10_000, 0x50_000)]);
        assert!(joined(&mut all, 0x1000, 0x2000));
        assert_eq!(all, [(0x1000, 0x2000), (0x10_000, 0x50_000)]);
        assert!(!forget(&mut all, 0x2000, 0x10_000), "between them");
        assert!(forget(&mut all, 0x30_000, 0x31_000));
        assert_eq!(all, [(0x1000, 0x2000)]);
    }

    /// A copy counts only whole inside one region, and not once the region
    /// is let go, in part or as a whole allocation; its kept answer follows.
    #[test]
    fn a_copy_is_write_combined_only_inside_a_region() {
        // Addresses of its own: the registry is the process's.
        let base = 0x7e10_0000_0000usize;
        asked(base, 0x100_0000);
        assert!(holds(base as u64, 0x1000));
        assert!(holds(base as u64 + 0xff_f000, 0x1000));
        assert!(!holds(base as u64 + 0xff_f000, 0x2000), "past its end");
        assert!(!holds(base as u64 - 0x1000, 0x2000), "before its start");
        let cache = AtomicU64::new(0);
        assert!(cached(&cache, base as u64, 0x1000));
        freed(base + 0x10_0000, 0x1000);
        assert!(!holds(base as u64, 0x1000), "part of it decommitted");
        assert!(!cached(&cache, base as u64, 0x1000), "the kept answer follows");
        // (Not memory of the process: the release takes what starts there.)
        let other = base + 0x1000_0000;
        asked(other, 0x10_0000);
        freed(other, 0);
        assert!(!holds(other as u64, 0x1000), "the allocation released");
    }

    /// The minimum is 1 MiB unless set, in KiB when set, 0 for every size.
    #[test]
    fn the_least_size_is_a_mib_unless_set() {
        assert_eq!(min_of(None), 1 << 20);
        assert_eq!(min_of(Some("64")), 64 << 10);
        assert_eq!(min_of(Some(" 0 ")), 0);
        assert_eq!(min_of(Some("junk")), 1 << 20);
    }

    /// Off, nothing is left on the card; on, only a buffer's copy with a card
    /// side, big enough, of write-combined memory, which is asked last.
    #[test]
    fn writes_stay_on_the_card_only_as_the_switch_says() {
        const MB: u64 = 1 << 20;
        let asked = std::cell::Cell::new(0);
        let wc = |yes: bool| {
            let asked = &asked;
            move || {
                asked.set(asked.get() + 1);
                yes
            }
        };
        assert!(!leaves(false, MB, true, false, 64 * MB, wc(true)), "off");
        assert_eq!(asked.get(), 0, "off asks nothing");
        assert!(leaves(true, MB, true, false, 64 * MB, wc(true)));
        assert!(!leaves(true, MB, true, false, 64 * MB, wc(false)), "not write-combined");
        assert!(!leaves(true, MB, false, false, 64 * MB, wc(true)), "no card side");
        assert!(!leaves(true, MB, true, true, 64 * MB, wc(true)), "a window");
        assert!(!leaves(true, MB, true, false, MB - 1, wc(true)), "too small");
        assert!(leaves(true, 0, true, false, 4096, wc(true)), "every size");
        assert_eq!(asked.get(), 3);
    }
}
