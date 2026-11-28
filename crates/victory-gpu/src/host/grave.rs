//! What [`super::bury`] lets go of, dropped on a thread of its own
//! (`VICTORY_BURY_ASIDE`, on unless `0`).
//!
//! A landing let go of what waited for the bury lag ([`super::bury_lag`]) on
//! the thread that landed: tens of thousands of objects a frame (the
//! resources descriptor slots showed, upload buffers, host lists), each
//! release a call into the runtime and the driver, many of them a call into
//! the kernel. A profile had it at ten milliseconds a frame of the relay's
//! landings, before the landing told the submitting thread that lists had
//! come down (`relay_land`): that thread waited for it ("lists in the air").
//!
//! What changes: which thread drops them, and when: a little later, never
//! earlier. Which objects go, and at which submission's end, is decided where
//! it was, by the same rule ([`super::bury`]); nothing that a list may still
//! name is in what comes here. Nothing waits for the drops, as nothing waited
//! for them before but the landing itself: the memory they free is given back
//! a few milliseconds later than it was. With no thread (one could not be
//! made) they are dropped where they were.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};

/// Whether what the bury lag let go of is dropped on a thread of its own
/// (`VICTORY_BURY_ASIDE`, on unless `0`; `0`: by the thread that landed the
/// submission, as before): see the module's words.
pub(crate) fn on() -> bool {
    crate::toggled!("VICTORY_BURY_ASIDE")
}

/// What the thread drops, in the order given, and how many batches were
/// given and dropped.
struct Grave<T> {
    state: Mutex<(VecDeque<Vec<T>>, u64, u64)>,
    more: Condvar,
    gone: Condvar,
}

impl<T> Grave<T> {
    const fn new() -> Grave<T> {
        Grave { state: Mutex::new((VecDeque::new(), 0, 0)), more: Condvar::new(), gone: Condvar::new() }
    }

    /// Gives a batch; its number, which [`Grave::wait_dropped`] waits for.
    fn give(&self, what: Vec<T>) -> u64 {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.0.push_back(what);
        state.1 += 1;
        let number = state.1;
        drop(state);
        self.more.notify_one();
        number
    }

    /// The next batch, waited for.
    fn next(&self) -> Vec<T> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(what) = state.0.pop_front() {
                return what;
            }
            state = self.more.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Drops the next batch (waited for), counted.
    fn drop_next(&self) {
        let what = self.next();
        let n = what.len() as u64;
        let began = std::time::Instant::now();
        drop(what);
        STATS[0].fetch_add(1, Ordering::Relaxed);
        STATS[1].fetch_add(n, Ordering::Relaxed);
        STATS[2].fetch_add(began.elapsed().as_micros() as u64, Ordering::Relaxed);
        self.state.lock().unwrap_or_else(|e| e.into_inner()).2 += 1;
        self.gone.notify_all();
    }

    /// Waits until batch `number` ([`Grave::give`]) and those before it are
    /// dropped.
    #[cfg(test)]
    fn wait_dropped(&self, number: u64) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while state.2 < number {
            state = self.gone.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// Since the last frame line: batches, objects and microseconds the thread
/// dropped; and objects dropped where they were buried (the switch off, or
/// no thread), with their microseconds.
pub(super) static STATS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

static GRAVE: Grave<(u64, super::Retired)> = Grave::new();

/// Whether the thread is there (made at the first call).
fn thread() -> bool {
    static UP: OnceLock<bool> = OnceLock::new();
    *UP.get_or_init(|| {
        std::thread::Builder::new()
            .name("victory-bury".into())
            .spawn(|| {
                // (Not asked onto the fast cores: nothing waits for it.)
                loop {
                    GRAVE.drop_next();
                }
            })
            .is_ok()
    })
}

/// Drops `done`: on the thread with the switch on ([`on`]), else here.
pub(super) fn bury(done: Vec<(u64, super::Retired)>) {
    bury_as(done, on());
}

/// [`bury`] with the switch `aside`; the number of the batch given to the
/// thread, or `None` if it was dropped here (also when there is no thread).
fn bury_as(done: Vec<(u64, super::Retired)>, aside: bool) -> Option<u64> {
    if done.is_empty() {
        return None;
    }
    if aside && thread() {
        return Some(GRAVE.give(done));
    }
    let n = done.len() as u64;
    let began = std::time::Instant::now();
    drop(done);
    STATS[3].fetch_add(n, Ordering::Relaxed);
    STATS[4].fetch_add(began.elapsed().as_micros() as u64, Ordering::Relaxed);
    None
}

/// The frame line's words of [`STATS`], taken, per frame; with the ms moved
/// off the thread that buried them for the run report. `None` if nothing
/// was buried.
pub(super) fn line(frames: f64) -> Option<(String, f64)> {
    let s: Vec<u64> = STATS.iter().map(|s| s.swap(0, Ordering::Relaxed)).collect();
    if s[1] + s[3] == 0 {
        return None;
    }
    let per = |v: u64| v as f64 / frames.max(1e-9);
    let aside_ms = per(s[2]) / 1000.0;
    Some((
        format!(
            "objects let go past the bury lag per frame: {:.0} on a thread of their own in {:.1} batches, {:.2} ms there \
             (VICTORY_BURY_ASIDE=1), {:.0} where they were buried, {:.2} ms",
            per(s[1]),
            per(s[0]),
            aside_ms,
            per(s[3]),
            per(s[4]) / 1000.0,
        ),
        aside_ms,
    ))
}

#[cfg(test)]
mod tests {
    use super::{GRAVE, Grave, bury_as};
    use crate::host::Retired;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counts its drops, and which thread dropped it.
    struct Counted(Arc<AtomicUsize>, Arc<std::sync::Mutex<Option<std::thread::ThreadId>>>);

    impl Drop for Counted {
        fn drop(&mut self) {
            *self.1.lock().unwrap() = Some(std::thread::current().id());
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn counted(dropped: &Arc<AtomicUsize>, by: &Arc<std::sync::Mutex<Option<std::thread::ThreadId>>>) -> Counted {
        Counted(dropped.clone(), by.clone())
    }

    /// What is given is dropped, all of it and only by the one that takes
    /// it, batch after batch in the order given.
    #[test]
    fn what_is_given_is_dropped_in_order_by_who_takes_it() {
        let grave: Grave<Counted> = Grave::new();
        let (dropped, by) = (Arc::new(AtomicUsize::new(0)), Arc::default());
        assert_eq!(grave.give((0..3).map(|_| counted(&dropped, &by)).collect()), 1);
        assert_eq!(grave.give((0..5).map(|_| counted(&dropped, &by)).collect()), 2);
        // Given, not dropped: nothing took them yet.
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        assert_eq!(grave.next().len(), 3);
        assert_eq!(dropped.load(Ordering::SeqCst), 3);
        grave.drop_next();
        assert_eq!(dropped.load(Ordering::SeqCst), 8);
        assert!(grave.state.lock().unwrap().0.is_empty());
    }

    /// Both settings drop everything buried, once: off, here and at once;
    /// on, by the thread of its own, the batch it is given and nothing
    /// before that.
    #[test]
    fn what_is_buried_is_dropped_once_here_or_by_the_thread() {
        let here = std::thread::current().id();
        let (dropped, by) = (Arc::new(AtomicUsize::new(0)), Arc::default());
        let batch = |n: usize| -> Vec<(u64, Retired)> {
            (0..n).map(|i| (i as u64, Retired::Any(Box::new(counted(&dropped, &by))))).collect()
        };
        // Off: dropped before it returns, on this thread.
        assert_eq!(bury_as(batch(4), false), None);
        assert_eq!(dropped.load(Ordering::SeqCst), 4);
        assert_eq!(*by.lock().unwrap(), Some(here));
        // Nothing: nothing given.
        assert_eq!(bury_as(Vec::new(), true), None);
        // On: given to the thread, which drops it.
        let number = bury_as(batch(6), true).expect("the thread is made");
        GRAVE.wait_dropped(number);
        assert_eq!(dropped.load(Ordering::SeqCst), 10);
        assert_ne!(*by.lock().unwrap(), Some(here));
    }

    /// A taker waits for what comes, on another thread.
    #[test]
    fn the_thread_waits_for_what_comes() {
        static WAITING: Grave<Counted> = Grave::new();
        let (dropped, by) = (Arc::new(AtomicUsize::new(0)), Arc::default());
        let taker = std::thread::spawn(|| WAITING.next().len());
        WAITING.give(vec![counted(&dropped, &by), counted(&dropped, &by)]);
        assert_eq!(taker.join().unwrap(), 2);
        assert_eq!(dropped.load(Ordering::SeqCst), 2);
    }
}
