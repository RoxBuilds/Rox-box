//! Which processors our own threads run on, where a processor has two kinds
//! of cores. `cpu_sets` and `keep_on_fast_cores` are fxceless/victory-fork's
//! (`framelog.rs`, commit 0b0074e0, GPL-3.0-or-later, as this).

/// The system's processor sets of group 0: each one's id and its efficiency
/// class (higher is faster).
fn cpu_sets() -> Vec<(u32, u8)> {
    use windows_sys::Win32::System::SystemInformation::{GetSystemCpuSetInformation, SYSTEM_CPU_SET_INFORMATION};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut bytes = 0u32;
    // SAFETY: a size query, then the call into a buffer of that size.
    unsafe { GetSystemCpuSetInformation(std::ptr::null_mut(), 0, &mut bytes, GetCurrentProcess(), 0) };
    let mut buffer = vec![0u64; (bytes as usize).div_ceil(8).max(1)];
    // SAFETY: as above; the buffer holds `bytes` bytes, aligned for the records.
    let ok = unsafe { GetSystemCpuSetInformation(buffer.as_mut_ptr().cast(), bytes, &mut bytes, GetCurrentProcess(), 0) };
    let mut sets = Vec::new();
    let mut at = 0usize;
    while ok != 0 && at + size_of::<SYSTEM_CPU_SET_INFORMATION>() <= bytes as usize {
        // SAFETY: a record inside the buffer; each says its own size.
        let info = unsafe { &*(buffer.as_ptr().cast::<u8>().add(at).cast::<SYSTEM_CPU_SET_INFORMATION>()) };
        if info.Size == 0 {
            break;
        }
        // SAFETY: the record's type is CpuSet (the only one there is).
        let set = unsafe { &info.Anonymous.CpuSet };
        if set.Group == 0 {
            sets.push((set.Id, set.EfficiencyClass));
        }
        at += info.Size as usize;
    }
    sets
}

/// The sets of the fastest class, if there are slower ones too.
fn fast_of(sets: &[(u32, u8)]) -> Vec<u32> {
    let best = sets.iter().map(|c| c.1).max().unwrap_or(0);
    let fast: Vec<u32> = sets.iter().filter(|c| c.1 == best).map(|c| c.0).collect();
    if fast.len() == sets.len() { Vec::new() } else { fast }
}

/// The sets of the slowest class, if there are faster ones too.
fn slow_of(sets: &[(u32, u8)]) -> Vec<u32> {
    let worst = sets.iter().map(|c| c.1).min().unwrap_or(0);
    let slow: Vec<u32> = sets.iter().filter(|c| c.1 == worst).map(|c| c.0).collect();
    if slow.len() == sets.len() { Vec::new() } else { slow }
}

/// Whether a thread's description is one of the driver's background shader
/// threads ("D3D Background Thread", with or without a number after it).
fn is_background_thread(description: &str) -> bool {
    description.trim().starts_with("D3D Background Thread")
}

/// `VICTORY_BACKGROUND_SLOW=1` (off by default): a second after the device,
/// then less and less often ([`next_wait`]), gives the "D3D Background
/// Thread"s of this process the slow cores at idle priority.
/// A fallback for when `VICTORY_D3D12_BACKGROUND` cannot switch the driver's
/// background shader work off (the system may want Developer Mode for that):
/// those threads can run close to fully busy for minutes, on a PC bound by
/// its CPU.
/// Called once, when the host device exists.
pub(crate) fn keep_background_slow() {
    if !crate::switch::on("VICTORY_BACKGROUND_SLOW") {
        return;
    }
    start_background_slow();
}

/// Whether the background threads are pinned: `VICTORY_BACKGROUND_SLOW` unset
/// pins them only when the system refused to switch the driver's background
/// work off; `1` always; `0` never.
fn pin_background(switch: Option<&str>, refused: bool) -> bool {
    match switch {
        Some("0") => false,
        Some(_) => true,
        None => refused,
    }
}

/// [`keep_background_slow`] for when the system refused to switch the driver's
/// background shader work off (no Developer Mode; the background threads then
/// burn minutes of CPU):
/// pins them unless `VICTORY_BACKGROUND_SLOW=0`. At most one watching thread
/// however often (or after `keep_background_slow`) this is called.
pub(crate) fn keep_background_slow_on_refusal() {
    let switch = std::env::var("VICTORY_BACKGROUND_SLOW").ok();
    if pin_background(switch.as_deref(), true) {
        start_background_slow();
    }
}

/// The longest wait between two looks for the background threads
/// (`VICTORY_BACKGROUND_SCAN=<seconds>`, with or without `s`, or `<n>ms`):
/// 30 s unless set to a positive number. A look takes a snapshot of every
/// thread on the system and opens each of ours: several percent of a core,
/// looking once a second. `1s` looks every second, as
/// before.
fn scan_cap(set: Option<&str>) -> std::time::Duration {
    const DEFAULT: std::time::Duration = std::time::Duration::from_secs(30);
    let Some(set) = set.map(str::trim).filter(|s| !s.is_empty()) else {
        return DEFAULT;
    };
    let (number, unit) = match set.strip_suffix("ms") {
        Some(number) => (number, 0.001),
        None => (set.strip_suffix('s').unwrap_or(set), 1.0),
    };
    number
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|n| n.is_finite() && *n > 0.0)
        .map_or(DEFAULT, |n| std::time::Duration::from_secs_f64((n * unit).clamp(0.1, 3600.0)))
}

/// The wait before the next look: the first one (1 s, or `cap` if that is
/// shorter) after a look that put a thread on the slow cores, else twice the
/// last, at most `cap`. The driver's four background threads were there at
/// the first look, a second after the device, and none came later, so the
/// looks soon come every `cap`.
fn next_wait(last: std::time::Duration, demoted: bool, cap: std::time::Duration) -> std::time::Duration {
    let first = std::time::Duration::from_secs(1).min(cap);
    if demoted { first } else { (last * 2).clamp(first, cap) }
}

fn start_background_slow() {
    static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if STARTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let sets = cpu_sets();
    let slow = slow_of(&sets);
    if slow.is_empty() {
        tracing::info!("VICTORY_BACKGROUND_SLOW: the processor has one kind of core; the background threads get idle priority only");
    }
    let cap = scan_cap(std::env::var("VICTORY_BACKGROUND_SCAN").ok().as_deref());
    tracing::info!(
        longest = ?cap,
        "VICTORY_BACKGROUND_SLOW: a look for the D3D background threads after 1 s, then twice as long after each look \
         that finds none new (VICTORY_BACKGROUND_SCAN=1s: every second, as before)"
    );
    let spawned = std::thread::Builder::new().name("victory-background-slow".into()).spawn(move || {
        let mut done = std::collections::HashSet::new();
        let mut wait = next_wait(std::time::Duration::ZERO, true, cap);
        loop {
            std::thread::sleep(wait);
            let before = done.len();
            for thread in background_threads(&done) {
                if done.insert(thread) {
                    demote(thread, &slow);
                }
            }
            wait = next_wait(wait, done.len() != before, cap);
        }
    });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "VICTORY_BACKGROUND_SLOW: no thread to watch for the background threads");
    }
}

/// The ids of this process's threads described as the driver's background
/// threads, but for those in `skip` (already on the slow cores), which are
/// not opened again.
fn background_threads(skip: &std::collections::HashSet<u32>) -> Vec<u32> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE, LocalFree};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcessId, GetThreadDescription, OpenThread, THREAD_QUERY_LIMITED_INFORMATION,
    };
    let mut found = Vec::new();
    // SAFETY: a snapshot of our own, walked with a sized entry; handles opened here are closed; the name is the
    // system's, freed as asked.
    unsafe {
        let own = GetCurrentProcessId();
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return found;
        }
        let mut entry = THREADENTRY32 { dwSize: size_of::<THREADENTRY32>() as u32, ..Default::default() };
        let mut more = Thread32First(snapshot, &mut entry) != 0;
        while more {
            if entry.th32OwnerProcessID == own && !skip.contains(&entry.th32ThreadID) {
                let thread = OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, entry.th32ThreadID);
                if !thread.is_null() {
                    let mut name = std::ptr::null_mut();
                    if GetThreadDescription(thread, &mut name) >= 0 && !name.is_null() {
                        let len = (0..).take_while(|&i| *name.add(i) != 0).count();
                        if is_background_thread(&String::from_utf16_lossy(std::slice::from_raw_parts(name, len))) {
                            found.push(entry.th32ThreadID);
                        }
                        LocalFree(name.cast());
                    }
                    CloseHandle(thread);
                }
            }
            more = Thread32Next(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
    }
    found
}

/// The driver's background threads of this process ("D3D Background Thread",
/// found as [`keep_background_slow`] finds them), held open to read the
/// processor time each one used (`victory warm --settle`).
#[derive(Default)]
pub struct BackgroundThreads {
    threads: Vec<(u32, windows_sys::Win32::Foundation::HANDLE)>,
}

impl BackgroundThreads {
    pub fn new() -> Self {
        Self::default()
    }

    /// Looks for those not held yet and holds them; with `let_run`, gives each
    /// new one normal priority (where it had less) on every core. How many were
    /// new. (A look snapshots every thread of the system: some tens of ms.)
    pub fn look(&mut self, let_run: bool) -> usize {
        use windows_sys::Win32::System::Threading::{
            OpenThread, THREAD_QUERY_LIMITED_INFORMATION, THREAD_SET_INFORMATION, THREAD_SET_LIMITED_INFORMATION,
        };
        let held: std::collections::HashSet<u32> = self.threads.iter().map(|t| t.0).collect();
        let mut new = 0;
        for id in background_threads(&held) {
            let rights = THREAD_QUERY_LIMITED_INFORMATION | THREAD_SET_INFORMATION | THREAD_SET_LIMITED_INFORMATION;
            // SAFETY: a thread of this process, opened to read its times and set its
            // priority and cores; closed when this is dropped.
            let thread = unsafe { OpenThread(rights, 0, id) };
            if thread.is_null() {
                tracing::warn!(id, error = %std::io::Error::last_os_error(), "a background thread could not be opened");
                continue;
            }
            if let_run {
                let_run_thread(id, thread);
            }
            self.threads.push((id, thread));
            new += 1;
        }
        new
    }

    /// How many are held.
    pub fn len(&self) -> usize {
        self.threads.len()
    }

    pub fn is_empty(&self) -> bool {
        self.threads.is_empty()
    }

    /// The processor time (user and kernel) each one held has used so far, by
    /// thread id. One that ended keeps its last.
    pub fn used(&self) -> Vec<(u32, std::time::Duration)> {
        self.threads.iter().filter_map(|&(id, thread)| thread_time(thread).map(|t| (id, t))).collect()
    }
}

impl Drop for BackgroundThreads {
    fn drop(&mut self) {
        for &(_, thread) in &self.threads {
            // SAFETY: opened in `look`, closed once.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(thread) };
        }
    }
}

/// The user and kernel time the thread `thread` has used.
fn thread_time(thread: windows_sys::Win32::Foundation::HANDLE) -> Option<std::time::Duration> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::GetThreadTimes;
    let [mut created, mut ended, mut kernel, mut user] = [FILETIME::default(); 4];
    // SAFETY: a handle with the right to ask, and four places for the answers.
    let ok = unsafe { GetThreadTimes(thread, &mut created, &mut ended, &mut kernel, &mut user) } != 0;
    // In units of 100 ns.
    let units = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
    ok.then(|| std::time::Duration::from_nanos((units(kernel) + units(user)).saturating_mul(100)))
}

/// Gives a background thread normal priority where it has less, and every
/// core (no selected sets), for [`BackgroundThreads::look`].
fn let_run_thread(id: u32, thread: windows_sys::Win32::Foundation::HANDLE) {
    use windows_sys::Win32::System::Threading::{
        GetThreadPriority, SetThreadPriority, SetThreadSelectedCpuSets, THREAD_PRIORITY_NORMAL,
    };
    // SAFETY: a thread of ours, opened with the rights these calls ask for.
    unsafe {
        let priority = GetThreadPriority(thread);
        let raised = priority < THREAD_PRIORITY_NORMAL && SetThreadPriority(thread, THREAD_PRIORITY_NORMAL) != 0;
        let every_core = SetThreadSelectedCpuSets(thread, std::ptr::null(), 0) != 0;
        tracing::info!(
            id,
            priority,
            raised,
            every_core,
            "a D3D background thread runs at normal priority or above, on every core"
        );
    }
}

/// Puts thread `id` on the `slow` cores (if any) at idle priority.
fn demote(id: u32, slow: &[u32]) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenThread, SetThreadPriority, SetThreadSelectedCpuSets, THREAD_PRIORITY_IDLE, THREAD_SET_INFORMATION,
        THREAD_SET_LIMITED_INFORMATION,
    };
    // SAFETY: a thread of ours opened for the two calls and closed; a list of the system's own set ids.
    unsafe {
        let thread = OpenThread(THREAD_SET_INFORMATION | THREAD_SET_LIMITED_INFORMATION, 0, id);
        if thread.is_null() {
            tracing::warn!(id, error = %std::io::Error::last_os_error(), "a background thread could not be opened");
            return;
        }
        let sets = slow.is_empty() || SetThreadSelectedCpuSets(thread, slow.as_ptr(), slow.len() as u32) != 0;
        let priority = SetThreadPriority(thread, THREAD_PRIORITY_IDLE) != 0;
        CloseHandle(thread);
        tracing::info!(id, sets, priority, slow = slow.len(), "a D3D background thread is on the slow cores at idle priority (VICTORY_BACKGROUND_SLOW=0: not)");
    }
}

/// `VICTORY_FAST_CORES`: 0: no thread asks; 1: the threads a frame waits for
/// one by one; 2 (the default): the threads they share their work with too;
/// 3: those that work beside the rest as well (lists translated, descriptors
/// made): more threads by then than the fast cores run at a time, while the
/// slow ones stood idle.
fn stage() -> u32 {
    static STAGE: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *STAGE.get_or_init(|| std::env::var("VICTORY_FAST_CORES").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(2))
}

/// Asks the system to run the calling thread, which a frame waits for, on
/// the fast cores only, where the processor has two kinds. That fork's submit
/// thread was found on a slow one in one look of sixteen. (A selection of
/// processors is a preference the scheduler keeps unless nothing of it is
/// free.) The title's threads are not asked for.
pub(crate) fn keep_on_fast_cores(what: &str) {
    ask(what, 1);
}

/// [`keep_on_fast_cores`] for one of the threads a submission's preparing or
/// a landing shares its work with.
pub(crate) fn keep_helper_on_fast_cores(what: &str) {
    ask(what, 2);
}

/// [`keep_on_fast_cores`] for one of many threads whose work is ahead of what
/// a frame waits for.
pub(crate) fn keep_many_on_fast_cores(what: &str) {
    ask(what, 3);
}

fn ask(what: &str, from: u32) {
    use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadSelectedCpuSets};
    if stage() < from {
        return;
    }
    static FAST: std::sync::OnceLock<(Vec<u32>, usize)> = std::sync::OnceLock::new();
    let (fast, of) = FAST.get_or_init(|| {
        let sets = cpu_sets();
        (fast_of(&sets), sets.len())
    });
    // All alike (or nothing known): nothing to ask for.
    if fast.is_empty() {
        return;
    }
    // SAFETY: our own thread and a list of the system's own set ids.
    let ok = unsafe { SetThreadSelectedCpuSets(GetCurrentThread(), fast.as_ptr(), fast.len() as u32) };
    if ok == 0 {
        tracing::warn!(what, error = %std::io::Error::last_os_error(), "the thread could not be kept on the fast cores");
    } else {
        tracing::info!(what, fast = fast.len(), of, "the thread is kept on the fast cores (VICTORY_FAST_CORES=0: not)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_background_threads_are_pinned_by_the_switch_or_by_a_refusal() {
        assert!(!pin_background(None, false));
        assert!(pin_background(None, true));
        assert!(pin_background(Some("1"), false));
        assert!(pin_background(Some("1"), true));
        assert!(!pin_background(Some("0"), false));
        assert!(!pin_background(Some("0"), true));
    }

    #[test]
    fn the_slow_cores_are_those_of_the_worst_class_if_there_is_a_better_one() {
        assert_eq!(slow_of(&[(256, 1), (257, 1), (258, 0), (259, 0)]), [258, 259]);
        assert!(slow_of(&[(256, 0), (257, 0)]).is_empty());
        assert!(slow_of(&[]).is_empty());
    }

    #[test]
    fn the_looks_for_background_threads_back_off_while_none_is_new() {
        use std::time::Duration;
        let cap = scan_cap(None);
        assert_eq!(cap, Duration::from_secs(30));
        let mut wait = next_wait(Duration::ZERO, true, cap);
        let mut waits = vec![wait.as_secs()];
        // Four threads found in the first look, none after; a fifth one later.
        for demoted in [true, false, false, false, false, false, false, false, true, false] {
            wait = next_wait(wait, demoted, cap);
            waits.push(wait.as_secs());
        }
        assert_eq!(waits, [1, 1, 2, 4, 8, 16, 30, 30, 30, 1, 2]);
        // `VICTORY_BACKGROUND_SCAN=1s`: every second, as before.
        let cap = scan_cap(Some("1s"));
        let mut wait = next_wait(Duration::ZERO, true, cap);
        for demoted in [false, true, false, false] {
            assert_eq!(wait, Duration::from_secs(1));
            wait = next_wait(wait, demoted, cap);
        }
        assert_eq!(wait, Duration::from_secs(1));
        // A longest wait under a second is the first one too.
        let cap = scan_cap(Some("500ms"));
        assert_eq!(next_wait(Duration::ZERO, true, cap), Duration::from_millis(500));
        assert_eq!(next_wait(Duration::from_millis(500), false, cap), Duration::from_millis(500));
    }

    #[test]
    fn the_longest_wait_between_looks_is_read_in_seconds() {
        use std::time::Duration;
        assert_eq!(scan_cap(Some("")), Duration::from_secs(30));
        assert_eq!(scan_cap(Some("1s")), Duration::from_secs(1));
        assert_eq!(scan_cap(Some(" 60 ")), Duration::from_secs(60));
        assert_eq!(scan_cap(Some("2.5 s")), Duration::from_millis(2500));
        assert_eq!(scan_cap(Some("250ms")), Duration::from_millis(250));
        // Not a positive number: the default.
        for set in ["0", "-1", "soon", "nan", "s"] {
            assert_eq!(scan_cap(Some(set)), Duration::from_secs(30), "{set:?}");
        }
        // Bounded: a tenth of a second to an hour.
        assert_eq!(scan_cap(Some("1ms")), Duration::from_millis(100));
        assert_eq!(scan_cap(Some("99999")), Duration::from_secs(3600));
    }

    /// A thread's processor time is read and grows while it works; a process
    /// with no device has no background threads to hold.
    #[test]
    fn thread_times_are_read_and_grow() {
        use windows_sys::Win32::System::Threading::GetCurrentThread;
        // SAFETY: the pseudo handle of this thread.
        let thread = unsafe { GetCurrentThread() };
        let before = thread_time(thread).expect("this thread's times");
        let began = std::time::Instant::now();
        let mut spun = 0u64;
        while thread_time(thread).unwrap() <= before && began.elapsed() < std::time::Duration::from_secs(5) {
            spun = std::hint::black_box(spun.wrapping_add(1));
        }
        assert!(thread_time(thread).unwrap() > before, "after {spun} turns");
        let mut held = BackgroundThreads::new();
        if held.look(false) == 0 {
            assert!(held.is_empty() && held.used().is_empty());
        }
    }

    #[test]
    fn background_threads_are_found_by_their_description() {
        assert!(is_background_thread("D3D Background Thread"));
        assert!(is_background_thread("D3D Background Thread 3"));
        assert!(!is_background_thread("D3D Foreground Thread"));
        assert!(!is_background_thread("victory-submit"));
        assert!(!is_background_thread(""));
    }

    #[test]
    fn the_fast_cores_are_those_of_the_best_class_if_there_is_a_worse_one() {
        assert_eq!(fast_of(&[(256, 1), (257, 1), (258, 0)]), [256, 257]);
        assert!(fast_of(&[(256, 0), (257, 0)]).is_empty());
        assert!(fast_of(&[]).is_empty());
        assert!(!cpu_sets().is_empty());
        std::thread::spawn(|| keep_on_fast_cores("test")).join().unwrap();
    }
}
