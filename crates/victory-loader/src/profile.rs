//! Where the process's time goes: `VICTORY_PROFILE=<from>,<to>,<ms>,<file>`
//! samples the stacks of the busiest threads every `<ms>` milliseconds
//! between two times (seconds, as `VICTORY_STACKS`), and writes them counted
//! to `<file>` at the end (several of them, one after another, with `;`
//! between). A thread that waits is sampled like one that
//! runs: its top frame says which (`ntdll`'s system calls).
//!
//! The file: `thread <id> <start> <cpu ms> <samples> <name>`, then
//! `stack <thread> <phase> <count> <frame>;<frame>;...` (innermost first, as
//! `module+rva`), where the phase is what the caller's function says of the
//! frame at that moment.

use std::collections::HashMap;
use std::io::Write;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
use windows_sys::Win32::System::Threading::{
    GetExitCodeThread, GetThreadTimes, OpenThread, THREAD_GET_CONTEXT, THREAD_QUERY_INFORMATION,
    THREAD_SUSPEND_RESUME,
};

use crate::crash::locate;
use crate::stacks::{frames_of, name_of, other_threads, start_of};

/// The threads sampled: those that ran most since they were last chosen.
const BUSIEST: usize = 28;
/// How often they are chosen anew.
const CHOOSE: Duration = Duration::from_secs(1);
/// How often what was sampled so far is written out.
const SAVE_EVERY: Duration = Duration::from_secs(30);

/// What `VICTORY_PROFILE` says: from, to, every, the file.
fn asked(list: &str) -> Option<(Duration, Duration, Duration, String)> {
    let mut parts = list.splitn(4, ',');
    let mut seconds = || parts.next()?.trim().parse::<f64>().ok();
    let (from, to, ms) = (seconds()?, seconds()?, seconds()?);
    let of = |s: f64| Duration::try_from_secs_f64(s).ok();
    let file = parts.next()?.trim().to_string();
    Some((of(from)?, of(to)?, of(ms / 1000.0)?, file)).filter(|(a, b, ms, f)| a < b && !ms.is_zero() && !f.is_empty())
}

struct Thread {
    handle: HANDLE,
    start: String,
    name: String,
    /// Its processor time, 100 ns: at first, when last chosen, now.
    first: u64,
    last: u64,
    now: u64,
    samples: u64,
}

/// A thread's processor time, or none when the system does not say.
fn cpu(thread: HANDLE) -> Option<u64> {
    // SAFETY: zeroed is a valid FILETIME, and the calls write them.
    let mut t: [FILETIME; 4] = unsafe { std::mem::zeroed() };
    let [a, b, c, d] = &mut t;
    // SAFETY: an open thread; the pointers are to the structures above.
    if unsafe { GetThreadTimes(thread, a, b, c, d) } == 0 {
        return None;
    }
    let of = |f: &FILETIME| u64::from(f.dwHighDateTime) << 32 | u64::from(f.dwLowDateTime);
    Some(of(&t[2]) + of(&t[3]))
}

/// What `GetExitCodeThread` says of a thread that runs.
const STILL_ACTIVE: u32 = 259;

/// Whether the thread has exited. (One that exited with code 259 looks as if
/// it ran on: it keeps its entry, as every thread did before.)
fn exited(thread: HANDLE) -> bool {
    let mut code = 0;
    // SAFETY: an open thread and a place for its code.
    unsafe { GetExitCodeThread(thread, &mut code) != 0 && code != STILL_ACTIVE }
}

/// Opens thread `id`. Kept open for the profile's life, so the id stays its
/// own (see `close`).
fn open(id: u32) -> Thread {
    // SAFETY: opens a thread of our own process.
    let handle = unsafe { OpenThread(THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_INFORMATION, 0, id) };
    let first = if handle.is_null() { 0 } else { cpu(handle).unwrap_or(0) };
    Thread {
        handle,
        start: if handle.is_null() { String::new() } else { locate(start_of(handle)) },
        name: if handle.is_null() { String::new() } else { name_of(handle) },
        first,
        last: first,
        now: first,
        samples: 0,
    }
}

/// Closes a handle `open` gave, if it gave one.
fn close_one(handle: HANDLE) {
    if !handle.is_null() {
        // SAFETY: opened by `open` and no longer in any entry.
        unsafe { CloseHandle(handle) };
    }
}

/// Closes the threads' handles, once, when the profile is over. Closing them
/// early allows Windows to reuse a handle value for an unrelated file or object
/// opened concurrently by the title, which subsequent profile saves would close.
fn close(threads: &mut HashMap<u32, Thread>) {
    for t in threads.values_mut() {
        if !t.handle.is_null() {
            // SAFETY: opened by `choose`, closed only here.
            unsafe { CloseHandle(t.handle) };
            t.handle = std::ptr::null_mut();
        }
    }
}

/// Reads `VICTORY_PROFILE` and, if set, starts the thread that samples.
/// `phase` is asked at each round.
pub fn profile_on_schedule(phase: fn() -> u32) {
    let Ok(list) = std::env::var("VICTORY_PROFILE") else {
        return;
    };
    let start = Instant::now();
    let all: Vec<_> = list.split(';').filter_map(asked).collect();
    std::thread::spawn(move || {
        for one in all {
            profile(one, start, phase);
        }
    });
}

fn profile((from, to, every, file): (Duration, Duration, Duration, String), start: Instant, phase: fn() -> u32) {
    std::thread::sleep(from.saturating_sub(start.elapsed()));
    eprintln!("Rox-box: the profile begins");
    let mut threads: HashMap<u32, Thread> = HashMap::new();
    let mut chosen: Vec<u32> = Vec::new();
    let mut counts: HashMap<(u32, u32, Vec<u64>), u64> = HashMap::new();
    let mut phases: HashMap<u32, u64> = HashMap::new();
    let mut choose_at = Instant::now();
    let mut save_at = Instant::now() + SAVE_EVERY;
    let mut rounds = 0u64;
    while start.elapsed() < to {
        let round = Instant::now();
        if round >= choose_at {
            choose_at = round + CHOOSE;
            choose(&mut threads, &mut chosen);
        }
        let phase = phase();
        *phases.entry(phase).or_default() += 1;
        for id in &chosen {
            if let Some(t) = threads.get_mut(id) {
                let (frames, _) = frames_of(t.handle);
                t.samples += 1;
                if !frames.is_empty() {
                    *counts.entry((*id, phase, frames)).or_default() += 1;
                }
            }
        }
        rounds += 1;
        // Written now and then too: a run that ends before `to` (the title
        // closed, a crash) keeps what was sampled until then.
        if round >= save_at {
            save_at = round + SAVE_EVERY;
            let _ = write(&file, &threads, &counts, &phases, rounds);
        }
        std::thread::sleep(every.saturating_sub(round.elapsed()));
    }
    choose(&mut threads, &mut chosen);
    if let Err(e) = write(&file, &threads, &counts, &phases, rounds) {
        eprintln!("Rox-box: the profile was not written: {e}");
    } else {
        eprintln!("Rox-box: the profile is in {file}");
    }
    close(&mut threads);
}

/// Opens the threads that are new, and chooses the ones to sample.
///
/// A thread the system lists under the id of an entry whose thread has
/// exited, or was never opened, may be another thread with the id given
/// again: it is opened, and if it runs, the entry is made anew from it and
/// the old handle closed here (once: it leaves the map, so `close` never
/// sees it). The old entry used to stay, with its handle to the dead thread,
/// and the new thread was never sampled. The stacks counted for the old
/// thread stay under the id. A thread that exited and is still listed (the
/// handle keeps it) opens as one that exited: its entry stays as it was.
/// `VICTORY_PROFILE_RENEW=0`: entries are never made anew, as before.
fn choose(threads: &mut HashMap<u32, Thread>, chosen: &mut Vec<u32>) {
    static RENEW: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let renew = *RENEW.get_or_init(|| std::env::var_os("VICTORY_PROFILE_RENEW").is_none_or(|v| v != "0"));
    for id in other_threads() {
        let Some(t) = threads.get_mut(&id) else {
            threads.insert(id, open(id));
            continue;
        };
        if !renew || (!t.handle.is_null() && !exited(t.handle)) {
            continue;
        }
        let new = open(id);
        if new.handle.is_null() || exited(new.handle) {
            close_one(new.handle);
            continue;
        }
        close_one(std::mem::replace(t, new).handle);
    }
    let mut ran: Vec<(u64, u32)> = Vec::new();
    for (&id, t) in threads.iter_mut().filter(|(_, t)| !t.handle.is_null()) {
        // A thread the system says nothing of keeps its last time.
        if let Some(now) = cpu(t.handle) {
            t.now = now;
        }
        ran.push((t.now.saturating_sub(t.last), id));
        t.last = t.now;
    }
    ran.sort_unstable_by(|a, b| b.cmp(a));
    *chosen = ran.into_iter().take(BUSIEST).map(|(_, id)| id).collect();
}

fn write(
    file: &str,
    threads: &HashMap<u32, Thread>,
    counts: &HashMap<(u32, u32, Vec<u64>), u64>,
    phases: &HashMap<u32, u64>,
    rounds: u64,
) -> std::io::Result<()> {
    // Written to a temporary sibling file and renamed atomically so process
    // termination mid-write preserves the last complete profile snapshot.
    let part = format!("{file}.part");
    let mut out = std::io::BufWriter::new(std::fs::File::create(&part)?);
    writeln!(out, "rounds {rounds}")?;
    for (phase, n) in phases {
        writeln!(out, "phase {phase} {n}")?;
    }
    let mut names: HashMap<u64, String> = HashMap::new();
    // The handles stay open: this runs every `SAVE_EVERY` while the profile
    // goes on (see `close`).
    for (id, t) in threads {
        let ms = t.now.saturating_sub(t.first) / 10_000;
        writeln!(out, "thread {id} {} {ms} {} {}", t.start, t.samples, t.name)?;
    }
    for ((id, phase, frames), n) in counts {
        write!(out, "stack {id} {phase} {n} ")?;
        for f in frames {
            let name = names.entry(*f).or_insert_with(|| locate(*f as usize));
            write!(out, "{name};")?;
        }
        writeln!(out)?;
    }
    out.flush()?;
    drop(out);
    std::fs::rename(&part, file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_is_asked_is_read_whole_or_not_at_all() {
        assert_eq!(
            asked("150, 160.5,5,D:\\a b,c.txt"),
            Some((
                Duration::from_secs(150),
                Duration::from_millis(160_500),
                Duration::from_millis(5),
                "D:\\a b,c.txt".to_string()
            ))
        );
        assert_eq!(asked("150,160,5"), None);
        assert_eq!(asked("160,150,5,x"), None);
        assert_eq!(asked("a,160,5,x"), None);
        assert_eq!(asked("-1,160,5,x"), None);
        assert_eq!(asked("1,160,0,x"), None);
    }

    #[test]
    fn writes_keep_the_threads_open_until_the_profile_is_over() {
        use windows_sys::Win32::System::Threading::GetCurrentThreadId;
        // SAFETY: no preconditions.
        let id = unsafe { GetCurrentThreadId() };
        // SAFETY: opens the test's own thread, closed by `close` below.
        let handle = unsafe { OpenThread(THREAD_QUERY_INFORMATION, 0, id) };
        assert!(!handle.is_null());
        let mut threads = HashMap::new();
        // A monotonic delta underflow (clock skew): written as 0 rather than wrapping.
        let t = Thread { handle, start: "s".into(), name: "n".into(), first: 50_000, last: 0, now: 0, samples: 3 };
        threads.insert(id, t);
        let dir = std::env::temp_dir().join(format!("victory-profile-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("profile.txt").to_string_lossy().into_owned();
        for rounds in 1..=3 {
            write(&file, &threads, &HashMap::new(), &HashMap::new(), rounds).unwrap();
            assert!(cpu(handle).is_some(), "the thread's handle is still open after write {rounds}");
        }
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.starts_with("rounds 3\n"), "{text}");
        assert!(text.contains(&format!("thread {id} s 0 3 n")), "{text}");
        assert!(!std::path::Path::new(&format!("{file}.part")).exists());
        close(&mut threads);
        assert!(threads[&id].handle.is_null());
        close(&mut threads);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A thread listed under the id of an entry whose thread has exited is
    /// another thread: the entry is made anew from it, with a handle of its
    /// own to a thread that runs; and a thread that runs keeps its entry.
    #[test]
    fn a_new_thread_under_an_old_id_is_sampled() {
        use windows_sys::Win32::System::Threading::GetCurrentThreadId;
        let parked = |name: &str| {
            let (id_tx, id_rx) = std::sync::mpsc::channel();
            let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
            let thread = std::thread::Builder::new()
                .name(name.into())
                .spawn(move || {
                    // SAFETY: no preconditions.
                    id_tx.send(unsafe { GetCurrentThreadId() }).unwrap();
                    stop_rx.recv().ok();
                })
                .unwrap();
            (id_rx.recv().unwrap(), stop_tx, thread)
        };
        // A handle to a thread that has exited.
        let (gone_id, stop, thread) = parked("victory-test-gone");
        let dead = open(gone_id);
        assert!(!dead.handle.is_null() && !exited(dead.handle));
        stop.send(()).unwrap();
        thread.join().unwrap();
        assert!(exited(dead.handle));
        // The entry of another thread's id holds it, as if that thread had
        // been given the id of the one that exited.
        let (id, stop, thread) = parked("victory-test-new");
        let mut threads = HashMap::new();
        let old = Thread { name: "the thread that exited".into(), samples: 5, ..dead };
        threads.insert(id, old);
        let mut chosen = Vec::new();
        choose(&mut threads, &mut chosen);
        let new = &threads[&id];
        assert!(!new.handle.is_null() && !exited(new.handle), "the entry holds the thread that runs");
        assert!(cpu(new.handle).is_some());
        assert_ne!(new.name, "the thread that exited");
        assert_eq!(new.samples, 0);
        // A thread that runs keeps its entry and its handle.
        let kept = new.handle;
        choose(&mut threads, &mut chosen);
        assert_eq!(threads[&id].handle, kept);
        stop.send(()).unwrap();
        thread.join().unwrap();
        close(&mut threads);
    }
}
