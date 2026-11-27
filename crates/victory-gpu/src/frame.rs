//! Frame pacing: the title sets a frame interval and waits for each frame
//! to begin. Ours is a clock started by the first interval set, ticking
//! once per interval; the token is the frame number. Each event kind
//! (origin, vblank, flip) happens once per tick, so two waits for the same
//! kind never end on the same tick: the title polls the origin
//! with "token on time-out" (starting its next frame at once) and is paced
//! by a blocking wait for the flip before each present.
//!
//! shortcut: event offsets are ignored (every event is at the tick); tie
//! the clock to presentation if a title paces by them.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::object::{HRESULT, S_FALSE, S_OK, This};

/// What a wait returns when its time is up first.
const TIMEOUT: HRESULT = S_FALSE;
/// The frame-origin event: the one that starts frames and makes tokens.
const ORIGIN: u32 = 0;
/// Make the frame's token even if the wait times out (the title starts the
/// frame early).
const TOKEN_ON_TIMEOUT: u32 = 2;
const SIXTIETH: Duration = Duration::from_micros(16_667);

/// Whether the frame interval and missed-tick lines are logged (`VICTORY_FRAME_LOG=0` turns them off).
fn frame_log() -> bool {
    crate::toggled!("VICTORY_FRAME_LOG")
}

/// The whole ticks that passed unused between the last tick a wait ended on and `tick`, the one it ends on now
/// (0 when the title kept up: consecutive ticks).
fn missed_ticks(last: u64, tick: u64) -> u64 {
    tick.saturating_sub(last).saturating_sub(1)
}

/// The tick a frame begun by a wait begins on: the tick the wait ended on, or, for a frame begun on a time-out, the
/// tick that is current now (`tick` is the next one then).
fn begun_tick(ended: bool, tick: u64) -> u64 {
    if ended { tick } else { tick.saturating_sub(1) }
}

/// The ticks missed between the previous frame begun (`prev`, 0 before the first) and the one begun on `now`.
fn missed_since_begun(prev: u64, now: u64) -> u64 {
    if prev == 0 { 0 } else { missed_ticks(prev, now) }
}

/// The tick of the last frame begun (only kept while `VICTORY_FRAME_LOG` is on).
static BEGUN: AtomicU64 = AtomicU64::new(0);

/// The tick `elapsed` after the clock's start falls in, counted from `first` (the tick a late frame is due on).
fn tick_at(first: u64, elapsed: Duration, interval: Duration) -> u64 {
    first + (elapsed.as_nanos() / interval.as_nanos().max(1)) as u64
}

/// Missed ticks seen since the last line, and when that was.
static MISSED: AtomicU64 = AtomicU64::new(0);
static MISSED_SHOWN: Mutex<Option<Instant>> = Mutex::new(None);

/// Count `n` missed ticks, and say how many there were at most once a second.
fn note_missed(n: u64) {
    if n > 0 {
        MISSED.fetch_add(n, Ordering::Relaxed);
    }
    let mut shown = MISSED_SHOWN.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    match *shown {
        None => *shown = Some(now),
        Some(t) if now.duration_since(t) >= Duration::from_secs(1) => {
            *shown = Some(now);
            let missed = MISSED.swap(0, Ordering::Relaxed);
            if missed > 0 {
                tracing::info!(missed, "frame clock ticks missed in the last second");
            }
        }
        _ => {}
    }
}

struct Clock {
    start: Instant,
    interval: Duration,
    /// The tick number at `start` (ticks go on across interval changes).
    first: u64,
}

static CLOCK: Mutex<Option<Clock>> = Mutex::new(None);
static FRAME: AtomicU64 = AtomicU64::new(0);

fn clock() -> std::sync::MutexGuard<'static, Option<Clock>> {
    CLOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Device method: a frame every `length` microseconds. The period is how
/// many intervals a frame may take to reach the display (pipeline depth),
/// not part of the rate.
pub(crate) extern "system" fn set_interval(
    _: This,
    _output: This,
    length: u32,
    _period: u32,
    _flags: u32,
) -> HRESULT {
    let interval = match length {
        0 => SIXTIETH,
        us => Duration::from_micros(u64::from(us)),
    };
    if frame_log() {
        // Once per change: the title sets it again every so often with the same value.
        static SHOWN: AtomicU64 = AtomicU64::new(0);
        if SHOWN.swap(interval.as_nanos() as u64, Ordering::Relaxed) != interval.as_nanos() as u64 {
            tracing::info!(interval = %format_args!("{:.3}ms", interval.as_secs_f64() * 1000.0), "frame interval");
        }
    }
    // A new interval starts from now, after the last tick handed out.
    let first = LAST
        .iter()
        .map(|t| t.load(Ordering::Relaxed))
        .max()
        .unwrap_or(0);
    *clock() = Some(Clock {
        start: Instant::now(),
        interval,
        first,
    });
    S_OK
}

/// Device method: events are scheduled at every tick.
pub(crate) extern "system" fn schedule(
    _: This,
    _kind: u32,
    _offset: u32,
    _list: *const u8,
    _flags: u32,
) -> HRESULT {
    S_OK
}

/// Frames begun so far (frame waits that ended).
pub(crate) fn current() -> u64 {
    FRAME.load(Ordering::Relaxed)
}

/// For tests of what is done once a frame, in a process of their own: every
/// idle rule reads the frame count (`crate::host::long_ago`), so a test that
/// moves it beside others makes what those left idle, and the emergency trim
/// of windows (`crate::shadow::let_go_short`) let it go under them. A test
/// of an idle rule takes whether a thing is idle as an argument instead.
#[cfg(test)]
pub(crate) fn test_advance() {
    FRAME.fetch_add(1, Ordering::Relaxed);
}

/// Per event kind (origin, vblank, flip and later kinds): the last tick a
/// wait for it ended on.
static LAST: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

fn last(kind: u32) -> &'static AtomicU64 {
    &LAST[(kind as usize).min(LAST.len() - 1)]
}

/// The next tick after now and after the last one a wait for `kind` ended
/// on (an event happens once per tick: a wait woken a little early must not
/// let the next one end on the same tick), and how long until it.
fn next_tick(kind: u32) -> (u64, Duration) {
    let mut clock = clock();
    let c = clock.get_or_insert_with(|| Clock {
        start: Instant::now(),
        interval: SIXTIETH,
        first: 0,
    });
    // One reading of the clock for both the tick and the wait, as before.
    let since = c.start.elapsed();
    let elapsed = since.as_nanos();
    let interval = c.interval.as_nanos().max(1);
    let tick = (tick_at(c.first, since, c.interval) + 1).max(last(kind).load(Ordering::Relaxed) + 1);
    let at = u128::from(tick - c.first) * interval;
    (
        tick,
        Duration::from_nanos(at.saturating_sub(elapsed) as u64),
    )
}

/// Device method: blocks until the next tick (or the time-out); an origin
/// wait then hands out the new frame's token. Other events keep the token
/// the title passes. Ticks are absolute (start plus whole intervals), so
/// lateness never accumulates.
pub(crate) extern "system" fn wait(
    _: This,
    kind: u32,
    timeout_ms: u32,
    _list: *const u8,
    flags: u32,
    token: *mut u64,
) -> HRESULT {
    let origin = kind == ORIGIN;
    let (mut tick, mut wait) = next_tick(kind);
    let limit = Duration::from_millis(u64::from(timeout_ms));
    // A blocking wait for an event that has happened since the last wait
    // for it ended (a frame took longer than a tick) ends at once, on the
    // tick that passed: sleeping on to the next one cost a tick of every
    // long frame ("... frame waits sleeping ..."). Polls (a
    // time-out shorter than a tick) answer as before.
    // `VICTORY_LATE_WAITS=1` sleeps to the next tick, as before.
    static SLEEP: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*SLEEP.get_or_init(|| std::env::var_os("VICTORY_LATE_WAITS").is_some_and(|v| v == "1"))
        && limit >= wait
        && tick > last(kind).load(Ordering::Relaxed) + 1
    {
        tick -= 1;
        wait = Duration::ZERO;
    }
    let _called = crate::timeline::span("the title's frame wait");
    let hr = if wait > limit {
        std::thread::sleep(limit);
        TIMEOUT
    } else {
        std::thread::sleep(wait);
        last(kind).fetch_max(tick, Ordering::Relaxed);
        S_OK
    };
    #[cfg(all(windows, feature = "diagnostics"))]
    {
        use crate::copystat::{C, add};
        add(C::PaceWaits, 1);
        add(C::PaceUs, wait.min(limit).as_micros() as u64);
    }
    // A frame begun on a time-out takes no tick: its flip still does.
    if origin && (hr == S_OK || flags & TOKEN_ON_TIMEOUT != 0) {
        // Tokens count frames (consecutive), not ticks.
        let frame = FRAME.fetch_add(1, Ordering::Relaxed) + 1;
        if frame_log() {
            // Count the ticks between two frames begun, not between waits: a title that polls the origin and times
            // out never moves LAST[ORIGIN], so counting from it added the whole distance since the start on every
            // wait (thousands of ticks counted as missed, thousands a second at 30 ticks a second).
            let now = begun_tick(hr == S_OK, tick);
            note_missed(missed_since_begun(BEGUN.swap(now, Ordering::Relaxed), now));
        }
        if !token.is_null() {
            // SAFETY: checked non-null; the title passes its token.
            unsafe { token.write(frame) };
        }
    }
    #[cfg(feature = "diagnostics")]
    tracing::debug!(
        kind,
        timeout_ms,
        flags,
        hr,
        tick,
        wait_us = wait.as_micros() as u64,
        frame = current(),
        "frame wait"
    );
    hr
}

/// Device method: no statistics are kept, so none are returned.
pub(crate) extern "system" fn statistics(
    _: This,
    _token: u64,
    _kinds: u32,
    count: *mut u32,
    _out: *mut u8,
) -> HRESULT {
    if !count.is_null() {
        // SAFETY: checked non-null.
        unsafe { count.write(0) };
    }
    S_OK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_end_on_ticks_and_count_frames() {
        let null = std::ptr::null();
        assert_eq!(set_interval(null, null, 2_000, 1, 0), S_OK);
        let mut first = 0;
        assert_eq!(wait(null, 0, 1_000, std::ptr::null(), 0, &mut first), S_OK);
        let before = Instant::now();
        let mut second = 0;
        assert_eq!(wait(null, 0, 1_000, std::ptr::null(), 0, &mut second), S_OK);
        assert!(second > first);
        // At the tick, not at the time-out (a busy PC may wake it late).
        assert!(before.elapsed() <= Duration::from_millis(500));
        assert_eq!(wait(null, 0, 0, std::ptr::null(), 0, &mut second), TIMEOUT);
        // Tokens count frames, a frame started early on a time-out too.
        let mut early = 0;
        let flags = TOKEN_ON_TIMEOUT;
        assert_eq!(
            wait(null, 0, 0, std::ptr::null(), flags, &mut early),
            TIMEOUT
        );
        let mut next = 0;
        assert_eq!(wait(null, 0, 1_000, std::ptr::null(), 0, &mut next), S_OK);
        assert!(early == second + 1 && next == early + 1);
        assert!(LAST[0].load(Ordering::Relaxed) >= 2);

        // The title's loop (the clock is global, so in the same test): an
        // origin poll that times out (and starts the frame), then a
        // blocking flip wait. Each flip takes its own tick.
        const FLIP: u32 = 2;
        // (The ticks are far apart, and it may take a few tries: a busy PC
        // wakes a wait tens of milliseconds late at times.)
        assert_eq!(set_interval(null, null, 60_000, 3, 0), S_OK);
        let mut token = 0;
        let mut four = || -> Vec<u64> {
            (0..4)
                .map(|_| {
                    let _ = wait(null, 0, 0, std::ptr::null(), TOKEN_ON_TIMEOUT, &mut token);
                    assert_eq!(
                        wait(null, FLIP, u32::MAX, std::ptr::null(), 0, &mut token),
                        S_OK
                    );
                    LAST[2].load(Ordering::Relaxed)
                })
                .collect()
        };
        let mut flips = four();
        for _ in 0..10 {
            if flips.windows(2).all(|w| w[1] == w[0] + 1) {
                break;
            }
            flips = four();
        }
        assert!(flips.windows(2).all(|w| w[1] == w[0] + 1), "{flips:?}");
        // Even when a flip wait begins on the tick another just ended on.
        let (tick, _) = next_tick(FLIP);
        assert!(tick > flips[3]);
    }

    #[test]
    fn missed_ticks_count_the_gap() {
        assert_eq!(missed_ticks(10, 11), 0);
        assert_eq!(missed_ticks(10, 13), 2);
        assert_eq!(missed_ticks(10, 10), 0);
        assert_eq!(missed_ticks(12, 10), 0);
    }

    /// Origin waits that time out (frames begun early) a tick apart or on the same tick count no missed ticks, and
    /// a real gap counts once, however many waits there were.
    #[test]
    fn timed_out_frames_count_bounded_misses() {
        let mut prev = 0;
        let mut total = 0;
        // Next ticks 10, 10, 11, 12 (polls), then a 4-tick stall, then on time.
        for next in [10, 10, 11, 12, 16, 17] {
            let now = begun_tick(false, next);
            total += missed_since_begun(prev, now);
            prev = now;
        }
        assert_eq!(total, 3);
        assert_eq!(begun_tick(true, 7), 7);
        assert_eq!(missed_since_begun(0, 500), 0);
    }

    /// Frames that take 50 ms or 100 ms after a frame that ended on tick 3: the tick they end on is the one that
    /// passed (`tick_at`), and the ticks in between were missed.
    #[test]
    fn late_frames_miss_ticks_at_both_intervals() {
        let sixty = Duration::from_micros(16_667);
        let thirty = Duration::from_micros(33_333);
        let ms = Duration::from_millis;
        assert_eq!(tick_at(3, ms(50), sixty), 5);
        assert_eq!(missed_ticks(3, tick_at(3, ms(50), sixty)), 1);
        assert_eq!(tick_at(3, ms(50), thirty), 4);
        assert_eq!(missed_ticks(3, tick_at(3, ms(50), thirty)), 0);
        assert_eq!(missed_ticks(3, tick_at(3, ms(99), sixty)), 4);
        assert_eq!(missed_ticks(3, tick_at(3, ms(99), thirty)), 1);
        // On time: the tick one interval on follows at once.
        assert_eq!(missed_ticks(3, tick_at(3, sixty, sixty)), 0);
        assert_eq!(missed_ticks(3, tick_at(3, thirty, thirty)), 0);
    }
}
