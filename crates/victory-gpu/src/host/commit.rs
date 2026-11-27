//! The commit guard: what the layer holds back while the PC's commit (RAM
//! plus page file) is short or an allocation was just refused, so that a
//! refusal ends as a texture shown later instead of a failed allocation. On a
//! PC of 16 GB the process's private memory is well over the PC's: it
//! pages, so the page file's room (commit headroom) is what runs out first.
//!
//! After upstream GPL-3.0 fork, commits
//! ab75ac3e02ba "Back off refused GPU pool growth and defer uploads under low
//! commit" and 18bdf7932b79 "Scope bindless refreshes to their heap and bound
//! speculative staging": its residency.rs gates (a 500 ms refusal window after
//! `E_OUTOFMEMORY`, commit sampled at most every 32 ms, growth of staging only
//! with commit to spare, a texture pass only with a reserve, the prefetch
//! limit). Here: thresholds for a small PC (1 GiB for growth, 512 MiB for a pass,
//! 256 MiB for a bake, critical under 1,536 MiB), none of its 32 GB policies
//! (`pressured`, `limited_host_ram`), `short` on the line `shadow::ram_keep`
//! uses, and `VICTORY_COMMIT_GUARD=0` (read once) making every gate permissive
//! again, as before the guard.
//!
//! Every decision is a pure function of the sample (commit and free RAM) and
//! the time ([`Guard`]); the statics only hold the last sample, when it was
//! taken and when the refusal window ends.
//!
//! With the budget governor (`super::budget`, `VICTORY_BUDGET`, on by default)
//! the guard is its client: the sample is the governor's (one look at the
//! system for both), and [`critical`] is commit alone, no longer "or an
//! allocation was refused in the last 500 ms": what lets go of caches by it
//! (the cap policy's spare scratch, `VICTORY_SCRATCH_POLICY=cap`) let one
//! refusal empty them and every later allocation be refused in turn (a hundred
//! times the upload buffers made). Holding uploads back after a refusal stays
//! ([`upload_refused`], and [`critical_or_refused`] for the policy that wants
//! the older sign).

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

const MIB: u64 = 1 << 20;
/// How long uploads are held back after an allocation failed with
/// `E_OUTOFMEMORY` (ms).
const REFUSAL_MS: u64 = 500;
/// The most often commit is asked of the system (ms).
const SAMPLE_MS: u64 = 32;
/// Commit headroom a staging buffer may grow into for speculative work.
const GROWTH: u64 = 1024 * MIB;
/// Commit headroom a pass over textures needs, and a bake.
const PASS: u64 = 512 * MIB;
const PASS_BAKE: u64 = 256 * MIB;
/// Commit headroom under which the layer is critical, and free RAM under
/// which it is short (`shadow::ram_keep`'s line).
const CRITICAL: u64 = 1536 * MIB;
const SHORT: u64 = 1536 * MIB;

/// What the system said last: the page file's room (commit headroom) and the
/// free physical memory; both `u64::MAX` while unknown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Sample {
    commit: u64,
    ram: u64,
}

impl Sample {
    const UNKNOWN: Sample = Sample { commit: u64::MAX, ram: u64::MAX };
}

/// Whether `now` lies inside the refusal window that ends at `retry_at`.
fn refused(now: u64, retry_at: u64) -> bool {
    now < retry_at
}

/// Where the refusal window ends when an allocation fails at `now`; never
/// earlier than it did.
fn window_after(now: u64, retry_at: u64) -> u64 {
    retry_at.max(now.saturating_add(REFUSAL_MS))
}

/// The decisions, from a sample and the time. With `on` false (the guard
/// switched off) every one is permissive.
#[derive(Clone, Copy)]
struct Guard {
    on: bool,
    sample: Sample,
    now: u64,
    retry_at: u64,
}

impl Guard {
    fn upload_refused(&self) -> bool {
        self.on && refused(self.now, self.retry_at)
    }

    fn allow_staging_growth(&self, bake: bool) -> bool {
        !self.on || !self.upload_refused() && self.sample.commit >= if bake { PASS_BAKE } else { GROWTH }
    }

    fn admit_texture_pass(&self, bake: bool) -> bool {
        !self.on || !self.upload_refused() && self.sample.commit >= if bake { PASS_BAKE } else { PASS }
    }

    fn critical(&self) -> bool {
        self.on && (self.upload_refused() || self.commit_critical())
    }

    /// [`Guard::critical`] without the refusal window: commit alone.
    fn commit_critical(&self) -> bool {
        self.on && self.sample.commit < CRITICAL
    }

    fn short(&self) -> bool {
        self.on && self.sample.ram < SHORT
    }
}

static RETRY_AT: AtomicU64 = AtomicU64::new(0);
static NEXT_SAMPLE: AtomicU64 = AtomicU64::new(0);
static COMMIT: AtomicU64 = AtomicU64::new(u64::MAX);
static RAM: AtomicU64 = AtomicU64::new(u64::MAX);
/// Allocations refused, and stagings held back, so far (for the log lines).
static REFUSALS: AtomicU64 = AtomicU64::new(0);
static DEFERRED: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static STAGING_DEFERRED: Cell<bool> = const { Cell::new(false) };
}

/// Whether the guard acts (`VICTORY_COMMIT_GUARD`, on unless `0`, read once;
/// `0` makes every function here permissive, as before the guard).
pub(crate) fn on() -> bool {
    crate::toggled!("VICTORY_COMMIT_GUARD")
}

/// The system's commit headroom and free RAM now: the budget governor's
/// sample, or with it off (`VICTORY_BUDGET=0`) the system's answer.
fn ask() -> Option<Sample> {
    if super::budget::on() {
        let s = super::budget::sample();
        return Some(Sample { commit: s.commit, ram: s.ram });
    }
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: plain data the call fills in.
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: a sized structure.
    let ok = unsafe { GlobalMemoryStatusEx(&mut m) } != 0;
    ok.then_some(Sample { commit: m.ullAvailPageFile, ram: m.ullAvailPhys })
}

/// The last sample, asked again when [`SAMPLE_MS`] have passed (the texture
/// pass runs for seconds inside one submission, so this is not once per
/// frame). Logs when commit crosses the critical line either way.
fn sample(now: u64) -> Sample {
    let next = NEXT_SAMPLE.load(Ordering::Relaxed);
    if now >= next
        && NEXT_SAMPLE.compare_exchange(next, now.saturating_add(SAMPLE_MS), Ordering::Relaxed, Ordering::Relaxed).is_ok()
        && let Some(s) = ask()
    {
        let was = COMMIT.swap(s.commit, Ordering::Relaxed);
        RAM.store(s.ram, Ordering::Relaxed);
        let (before, after) = (was < CRITICAL, s.commit < CRITICAL);
        if before != after {
            tracing::warn!(
                commit_headroom_mib = s.commit / MIB,
                free_ram_mib = s.ram / MIB,
                refusals = REFUSALS.load(Ordering::Relaxed),
                stagings_deferred = DEFERRED.load(Ordering::Relaxed),
                "commit guard: {}",
                if after { "commit is critical, holding back uploads" } else { "commit is no longer critical" }
            );
        }
    }
    Sample { commit: COMMIT.load(Ordering::Relaxed), ram: RAM.load(Ordering::Relaxed) }
}

fn guard() -> Guard {
    if !on() {
        return Guard { on: false, sample: Sample::UNKNOWN, now: 0, retry_at: 0 };
    }
    let now = super::coarse_ms();
    Guard { on: true, sample: sample(now), now, retry_at: RETRY_AT.load(Ordering::Relaxed) }
}

/// An allocation of the card's or the PC's memory was refused: with
/// `E_OUTOFMEMORY`, uploads are held back for [`REFUSAL_MS`].
pub(crate) fn allocation_error(error: &windows::core::Error) {
    if !on() || error.code() != windows::Win32::Foundation::E_OUTOFMEMORY {
        return;
    }
    let now = super::coarse_ms();
    let before = RETRY_AT.load(Ordering::Relaxed);
    RETRY_AT.fetch_max(window_after(now, before), Ordering::Relaxed);
    let n = REFUSALS.fetch_add(1, Ordering::Relaxed) + 1;
    if !refused(now, before) || n.is_power_of_two() {
        let s = sample(now);
        tracing::warn!(
            refusals = n,
            commit_headroom_mib = s.commit / MIB,
            free_ram_mib = s.ram / MIB,
            stagings_deferred = DEFERRED.load(Ordering::Relaxed),
            "commit guard: an allocation was refused (out of memory), holding back uploads for {REFUSAL_MS} ms"
        );
    }
}

/// Whether uploads are held back now (an allocation was refused moments ago).
pub(crate) fn upload_refused() -> bool {
    guard().upload_refused()
}

/// Whether a staging buffer may be made for a texture that is not needed
/// yet (speculative), or for a bake (`bake`): not in the refusal window, and
/// with 1 GiB of commit headroom (256 MiB for a bake).
pub(crate) fn allow_staging_growth(bake: bool) -> bool {
    guard().allow_staging_growth(bake)
}

/// Whether a pass over textures may make staging buffers now (`bake`: for a
/// one-shot texture bake): not in the refusal window, and with 512 MiB of
/// commit headroom (256 MiB for a bake).
pub(crate) fn admit_texture_pass(bake: bool) -> bool {
    guard().admit_texture_pass(bake)
}

/// Commit is critically short (under 1,536 MiB of headroom); with
/// `VICTORY_BUDGET=0`, or an allocation was just refused, as before.
pub(crate) fn critical() -> bool {
    let g = guard();
    if super::budget::on() { g.commit_critical() } else { g.critical() }
}

/// Commit is critically short, or an allocation was refused in the last 500
/// ms: the older [`critical`], for the spare scratch's cap policy
/// (`VICTORY_SCRATCH_POLICY=cap`).
pub(crate) fn critical_or_refused() -> bool {
    guard().critical()
}

/// Free physical memory is short (under 1,536 MiB, the line of
/// `shadow::ram_keep`), from the same cached sample as [`critical`].
pub(crate) fn short() -> bool {
    guard().short()
}

/// This thread held a staging buffer back ([`take_staging_deferred`]).
pub(crate) fn note_staging_deferred() {
    STAGING_DEFERRED.set(true);
    let n = DEFERRED.fetch_add(1, Ordering::Relaxed) + 1;
    if n.is_power_of_two() {
        let s = sample(super::coarse_ms());
        tracing::info!(
            stagings_deferred = n,
            refusals = REFUSALS.load(Ordering::Relaxed),
            commit_headroom_mib = s.commit / MIB,
            free_ram_mib = s.ram / MIB,
            "commit guard: a staging buffer was held back"
        );
    }
}

/// Whether this thread held a staging buffer back since it last asked.
pub(crate) fn take_staging_deferred() -> bool {
    STAGING_DEFERRED.replace(false)
}

/// How many bytes a texture pass may stage ahead: `bytes`, at most 16 MiB
/// when commit is `critical`, at most 64 MiB when memory is `short`.
pub(crate) fn prefetch_limit(bytes: u64, short: bool, critical: bool) -> u64 {
    if critical {
        bytes.min(16 << 20)
    } else if short {
        bytes.min(64 << 20)
    } else {
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(on: bool, commit: u64, ram: u64, now: u64, retry_at: u64) -> Guard {
        Guard { on, sample: Sample { commit, ram }, now, retry_at }
    }

    #[test]
    fn the_refusal_window_is_half_a_second_and_never_shrinks() {
        assert!(refused(0, 1));
        assert!(refused(499, 500));
        assert!(!refused(500, 500));
        assert!(!refused(0, 0));
        assert_eq!(window_after(1000, 0), 1500);
        assert_eq!(window_after(1000, 1400), 1500);
        assert_eq!(window_after(1000, 1900), 1900);
        assert_eq!(window_after(u64::MAX, 0), u64::MAX);
    }

    #[test]
    fn growth_and_passes_need_their_commit_reserve() {
        // (commit headroom MiB, growth, growth for a bake, pass, pass for a bake)
        for (commit, growth, growth_bake, pass, pass_bake) in [
            (0, false, false, false, false),
            (255, false, false, false, false),
            (256, false, true, false, true),
            (511, false, true, false, true),
            (512, false, true, true, true),
            (1023, false, true, true, true),
            (1024, true, true, true, true),
            (8192, true, true, true, true),
        ] {
            let g = at(true, commit * MIB, u64::MAX, 100, 0);
            assert_eq!(g.allow_staging_growth(false), growth, "growth at {commit} MiB");
            assert_eq!(g.allow_staging_growth(true), growth_bake, "bake growth at {commit} MiB");
            assert_eq!(g.admit_texture_pass(false), pass, "pass at {commit} MiB");
            assert_eq!(g.admit_texture_pass(true), pass_bake, "bake pass at {commit} MiB");
        }
    }

    #[test]
    fn a_refusal_holds_everything_back_whatever_the_commit() {
        let g = at(true, 64 << 30, 64 << 30, 100, 600);
        assert!(g.upload_refused() && g.critical());
        // Commit alone is not critical: caches are not let go for a refusal.
        assert!(!g.commit_critical());
        assert!(!g.allow_staging_growth(false) && !g.allow_staging_growth(true));
        assert!(!g.admit_texture_pass(false) && !g.admit_texture_pass(true));
        // The window over: open again.
        let g = at(true, 64 << 30, 64 << 30, 600, 600);
        assert!(!g.upload_refused() && !g.critical());
        assert!(g.allow_staging_growth(false) && g.admit_texture_pass(true));
    }

    #[test]
    fn critical_and_short_have_their_lines() {
        // (commit MiB, ram MiB, critical, short)
        for (commit, ram, critical, short) in [
            (1535, 4096, true, false),
            (1536, 4096, false, false),
            (8192, 1535, false, true),
            (8192, 1536, false, false),
            (100, 100, true, true),
        ] {
            let g = at(true, commit * MIB, ram * MIB, 100, 0);
            assert_eq!((g.critical(), g.short()), (critical, short), "commit {commit} MiB, ram {ram} MiB");
            assert_eq!(g.commit_critical(), critical, "commit {commit} MiB, ram {ram} MiB");
            // In a refusal window: critical by the old sign, not by commit.
            let refusing = at(true, commit * MIB, ram * MIB, 100, 600);
            assert!(refusing.critical());
            assert_eq!(refusing.commit_critical(), critical);
        }
        // An unknown sample (the system said nothing) holds nothing back.
        let g = at(true, u64::MAX, u64::MAX, 100, 0);
        assert!(!g.critical() && !g.short() && g.allow_staging_growth(false) && g.admit_texture_pass(false));
    }

    #[test]
    fn with_the_guard_off_everything_is_permissive() {
        for (commit, ram, now, retry_at) in [(0, 0, 100, 10_000), (1 << 20, 1 << 20, 0, 1), (0, u64::MAX, 5, 6)] {
            let g = at(false, commit, ram, now, retry_at);
            assert!(!g.upload_refused() && !g.critical() && !g.commit_critical() && !g.short());
            assert!(g.allow_staging_growth(false) && g.allow_staging_growth(true));
            assert!(g.admit_texture_pass(false) && g.admit_texture_pass(true));
        }
        let off = Guard { on: false, sample: Sample::UNKNOWN, now: 0, retry_at: 0 };
        assert!(!off.critical() && off.admit_texture_pass(false));
    }

    #[test]
    fn prefetch_is_limited_by_the_shortage() {
        // (bytes MiB, short, critical, expected MiB)
        for (bytes, short, critical, expected) in [
            (512, false, false, 512),
            (512, true, false, 64),
            (512, false, true, 16),
            (512, true, true, 16),
            (8, true, true, 8),
            (0, true, true, 0),
            (192, false, false, 192),
            (32, true, false, 32),
        ] {
            assert_eq!(prefetch_limit(bytes * MIB, short, critical), expected * MIB);
        }
    }

    #[test]
    fn a_deferred_staging_is_taken_once_per_thread() {
        assert!(!take_staging_deferred());
        STAGING_DEFERRED.set(true);
        assert!(take_staging_deferred());
        assert!(!take_staging_deferred());
        STAGING_DEFERRED.set(true);
        std::thread::spawn(|| assert!(!take_staging_deferred())).join().unwrap();
        assert!(take_staging_deferred());
    }
}
