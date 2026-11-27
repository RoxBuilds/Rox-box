//! The frame line's own thread (diagnostics builds): what the line says
//! every five seconds after the frame rate, the copy counters and the
//! memory census (`crate::copystat::write`) and the pipelines' drops, is said
//! by `victory-frame-line`; the thread that presents (with
//! `VICTORY_ASYNC_SUBMIT` the submitting thread, every frame's wall) only
//! takes the counters (`crate::copystat::take`) and goes on. Before, the
//! census stood that thread every five seconds: nearly all its samples
//! under `Meter::frame` lay in `copystat::log` (most in
//! `resource::census`), and one block took over a second from the frame
//! rate to the last drop, nearly all of it the memory census.
//! `VICTORY_FRAME_LINE_ASIDE=0`: said where it is taken, as before.
//!
//! The thread holds [`CENSUS`] while it says a line, and a sweep does not
//! begin meanwhile (`super::sweep`): the census holds the table of textures
//! for as long as it looks at them, and a sweep standing at that table holds
//! the submission lock, which would stand the submitting thread after all.

use std::sync::{Condvar, Mutex, MutexGuard, OnceLock, TryLockError, mpsc};

/// Whether the line is said by its own thread (`VICTORY_FRAME_LINE_ASIDE`,
/// on unless `0`, read once).
pub(super) fn aside() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_FRAME_LINE_ASIDE").is_none_or(|v| v != "0"))
}

type Job = Box<dyn FnOnce() + Send>;

/// Held by the line's thread while it says a line, and by a sweep for its
/// length ([`sweep_may`]).
static CENSUS: Mutex<()> = Mutex::new(());

/// Lines handed to the thread and lines it has said.
static LINES: (Mutex<[u64; 2]>, Condvar) = (Mutex::new([0; 2]), Condvar::new());

fn lines() -> MutexGuard<'static, [u64; 2]> {
    LINES.0.lock().unwrap_or_else(|e| e.into_inner())
}

/// `line` is said on the line's thread, after the lines handed before it;
/// here and now if there is no such thread. The caller never waits for it:
/// the channel has no bound (a line every five seconds).
pub(super) fn hand(line: impl FnOnce() + Send + 'static) {
    static THREAD: OnceLock<Option<mpsc::Sender<Job>>> = OnceLock::new();
    let thread = THREAD.get_or_init(|| {
        let (send, jobs) = mpsc::channel::<Job>();
        let spawned = std::thread::Builder::new().name("victory-frame-line".into()).spawn(move || {
            for job in jobs {
                {
                    let _census = CENSUS.lock().unwrap_or_else(|e| e.into_inner());
                    let _line = crate::timeline::span("the frame line: the counters said, the memory census");
                    // A census that panics loses its line, not the thread.
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)).is_err() {
                        tracing::warn!("the frame line's counters or census panicked: the rest of that line is lost");
                    }
                }
                lines()[1] += 1;
                LINES.1.notify_all();
            }
        });
        match spawned {
            Ok(_) => Some(send),
            Err(e) => {
                tracing::warn!(error = %e, "no thread for the frame line: it is said where it is taken");
                None
            }
        }
    });
    let Some(send) = thread else {
        return line();
    };
    lines()[0] += 1;
    if let Err(mpsc::SendError(line)) = send.send(Box::new(line)) {
        // (The thread is gone: said here, and counted as said.)
        line();
        lines()[1] += 1;
    }
}

/// For a sweep: `None` while the line's thread says a line (the sweep is
/// then due at the next moment with nothing in the air); else held for the
/// length of the sweep, and no line begins meanwhile. Only tried: the
/// caller holds the submission lock.
pub(super) fn sweep_may() -> Option<MutexGuard<'static, ()>> {
    match CENSUS.try_lock() {
        Ok(held) => Some(held),
        Err(TryLockError::Poisoned(e)) => Some(e.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}

/// Whether the line's thread says a line now (for the sweep's own thread,
/// which then looks again shortly, without the submission lock).
pub(super) fn census_runs() -> bool {
    matches!(CENSUS.try_lock(), Err(TryLockError::WouldBlock))
}

/// Lines handed to the thread so far, and those said.
#[cfg(test)]
pub(super) fn counts() -> [u64; 2] {
    *lines()
}

/// Waits until the thread has said every line handed to it.
#[cfg(test)]
pub(super) fn wait_said() {
    let mut counts = lines();
    while counts[1] < counts[0] {
        counts = LINES.1.wait(counts).unwrap_or_else(|e| e.into_inner());
    }
}

/// [`CENSUS`], held by a test so that the line's thread stands at its next
/// line.
#[cfg(test)]
pub(super) fn hold_census() -> MutexGuard<'static, ()> {
    CENSUS.lock().unwrap_or_else(|e| e.into_inner())
}
