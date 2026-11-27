//! The title's own threads (docs/demo-title.md, "Threads"), made every way
//! a title makes them: `CreateThread`, the C runtime's `_beginthreadex` and
//! Rust's `std::thread` (itself `CreateThread`); ended every way: by
//! returning, by `ExitThread`, by `_endthreadex` and by
//! `FreeLibraryAndExitThread` (with a reference of its own on the late DLL,
//! which the title still holds after it).
//!
//! Each worker checks its own static TLS, the executable's and each DLL's:
//! the template's values at its start, only its own values later (all of
//! them are alive at once, so a slot shared between threads shows), its
//! start heard of once by each image (a token per thread, `tls.rs`), and
//! it uses a value with a destructor in each. It says its thread id, waits
//! for the main thread's go (an event), fills its part of a buffer in title
//! memory, which the main thread then copies on the GPU and checks, and
//! ends. One of them (the waiter) is made before the late DLL is loaded:
//! it hears of no start from that DLL, but gets its TLS and its end.
//!
//! After joining them the main thread checks, per thread, that each image
//! heard of its end once and that each destructor ran once; their exit
//! codes; and that their ids were all different.
//!
//! In the run that ends with everything alive (`--alive`) two more threads
//! (one `CreateThread`, one `std::thread`) wait on an event nobody sets: the
//! process must still end with the title's code.

use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering::SeqCst};

use crate::dlls::Dll;
use crate::tls;
use crate::win::{self, Handle};

/// How a worker is made and how it ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Way {
    /// `CreateThread`, before the late DLL is loaded; waits for the go
    /// before it uses that DLL; returns.
    Waiter,
    /// `CreateThread`; returns.
    Win32Returns,
    /// `CreateThread`; `ExitThread`.
    Win32Exits,
    /// `_beginthreadex`; returns.
    CrtReturns,
    /// `_beginthreadex`; `_endthreadex`.
    CrtEnds,
    /// `CreateThread`; `FreeLibraryAndExitThread` of a reference of its own
    /// on the late DLL.
    Win32FreesLate,
    /// `std::thread`; returns.
    Std,
    /// `CreateThread`; waits for ever (`--alive`).
    StuckWin32,
    /// `std::thread`; waits for ever (`--alive`).
    StuckStd,
}

impl Way {
    /// The code a worker that ends by itself ends with.
    fn code(self, index: usize) -> u32 {
        0x60 + index as u32
    }

    fn stuck(self) -> bool {
        matches!(self, Way::StuckWin32 | Way::StuckStd)
    }

    fn name(self) -> &'static str {
        match self {
            Way::Waiter | Way::Win32Returns => "a CreateThread thread that returns",
            Way::Win32Exits => "a CreateThread thread that calls ExitThread",
            Way::CrtReturns => "a _beginthreadex thread that returns",
            Way::CrtEnds => "a _beginthreadex thread that calls _endthreadex",
            Way::Win32FreesLate => "a CreateThread thread that calls FreeLibraryAndExitThread",
            Way::Std => "a std::thread thread",
            Way::StuckWin32 => "a CreateThread thread left waiting",
            Way::StuckStd => "a std::thread thread left waiting",
        }
    }
}

/// The workers of a run that ends normally, the waiter first.
pub const WAYS: [Way; 7] = [
    Way::Waiter,
    Way::Win32Returns,
    Way::Win32Exits,
    Way::CrtReturns,
    Way::CrtEnds,
    Way::Win32FreesLate,
    Way::Std,
];
/// Their parts of the buffer they fill.
pub const PART: usize = 4096;
const MAX: usize = 10;

/// What a worker said, by its index.
struct Said {
    thread_id: AtomicU32,
    exe_token: AtomicU32,
    static_token: AtomicU32,
    late_token: AtomicU32,
}

static SAID: [Said; MAX] = [const {
    Said {
        thread_id: AtomicU32::new(0),
        exe_token: AtomicU32::new(0),
        static_token: AtomicU32::new(0),
        late_token: AtomicU32::new(0),
    }
}; MAX];

/// What went wrong on the workers, in our words.
static PROBLEMS: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn problem(index: usize, way: Way, what: impl std::fmt::Display) {
    let mut problems = PROBLEMS.lock().unwrap_or_else(|e| e.into_inner());
    problems.push(format!("{} (worker {index}): {what}", way.name()));
}

/// What the workers share: the DLLs, the events, the buffer.
struct Shared {
    statics: Dll,
    /// The late DLL, once loaded (the waiter starts before).
    late: Mutex<Option<Dll>>,
    /// Set once every worker said its id (manual-reset).
    ready: Handle,
    readies: AtomicU32,
    expected: u32,
    /// Set by the main thread: the workers go on (manual-reset).
    go: Handle,
    /// Set once every worker filled its part (manual-reset).
    filled: Handle,
    fills: AtomicU32,
    /// The buffer's address in title memory.
    buffer: AtomicUsize,
    /// Set by the waiter as it runs its own code: the notices of its start
    /// are over, so the late DLL, loaded after, hears of none.
    begun: Handle,
    /// Nobody sets it: the stuck workers wait on it.
    never: Handle,
    /// Set once every stuck worker is about to wait.
    stuck: Handle,
    stucks: AtomicU32,
    stuck_expected: u32,
}

// SAFETY: event handles are process-wide and may be used from any thread.
unsafe impl Send for Shared {}
// SAFETY: as above; the rest is atomics and a mutex.
unsafe impl Sync for Shared {}

static SHARED: std::sync::OnceLock<Shared> = std::sync::OnceLock::new();

fn shared() -> &'static Shared {
    SHARED.get().expect("the workers' shared state is made before they start")
}

/// A started worker.
pub struct Worker {
    index: usize,
    way: Way,
    /// `CreateThread`'s and `_beginthreadex`'s handle.
    handle: Handle,
    joiner: Option<std::thread::JoinHandle<()>>,
}

/// The workers, from the first (the waiter, made before the late DLL is
/// loaded) to the last.
pub struct Workers {
    workers: Vec<Worker>,
}

/// The value at byte `at` of worker `index`'s part.
pub fn pattern(index: usize, at: usize) -> u8 {
    (index as u8).wrapping_mul(31) ^ (at as u8).wrapping_mul(7) ^ 0xa5
}

impl Workers {
    /// The shared state and the waiter, before the late DLL is loaded;
    /// `stuck` more workers will be made for `--alive`.
    pub fn begin(statics: Dll, stuck: u32) -> Result<Workers, String> {
        let made = Shared {
            statics,
            late: Mutex::new(None),
            ready: win::event(true),
            readies: AtomicU32::new(0),
            expected: WAYS.len() as u32,
            go: win::event(true),
            filled: win::event(true),
            fills: AtomicU32::new(0),
            buffer: AtomicUsize::new(0),
            begun: win::event(true),
            never: win::event(true),
            stuck: win::event(true),
            stucks: AtomicU32::new(0),
            stuck_expected: stuck,
        };
        if [made.ready, made.go, made.filled, made.begun, made.never, made.stuck].iter().any(|h| h.is_null()) {
            return Err("CreateEventW failed".into());
        }
        if SHARED.set(made).is_err() {
            return Err("the workers were begun twice".into());
        }
        let mut workers = Workers { workers: Vec::new() };
        workers.start(Way::Waiter)?;
        wait(shared().begun, "the waiter's start")?;
        Ok(workers)
    }

    /// The rest, once the late DLL is loaded and the buffer is made;
    /// `reference` is a `LoadLibraryW` of the late DLL of the worker that
    /// gives it back with `FreeLibraryAndExitThread`.
    pub fn start_rest(&mut self, late: Dll, reference: Handle, buffer: usize, stuck: &[Way]) -> Result<(), String> {
        *shared().late.lock().unwrap_or_else(|e| e.into_inner()) = Some(late);
        REFERENCE.store(reference as usize, SeqCst);
        shared().buffer.store(buffer, SeqCst);
        for &way in WAYS[1..].iter().chain(stuck) {
            self.start(way)?;
        }
        Ok(())
    }

    fn start(&mut self, way: Way) -> Result<(), String> {
        let index = self.workers.len();
        let parameter = std::ptr::without_provenance_mut::<c_void>(index);
        let mut id = 0;
        let (handle, joiner) = match way {
            Way::Waiter | Way::Win32Returns | Way::Win32Exits | Way::Win32FreesLate | Way::StuckWin32 => {
                // SAFETY: a thread that runs `win32_start` with its index.
                let h = unsafe { win::CreateThread(std::ptr::null(), 0, win32_start, parameter, 0, &mut id) };
                (h, None)
            }
            Way::CrtReturns | Way::CrtEnds => {
                // SAFETY: as above, through the C runtime.
                let h = unsafe { win::_beginthreadex(std::ptr::null_mut(), 0, crt_start, parameter, 0, &mut id) };
                (h as Handle, None)
            }
            Way::Std | Way::StuckStd => {
                let thread = std::thread::Builder::new()
                    .name(format!("demo worker {index}"))
                    .spawn(move || body(index, way))
                    .map_err(|e| format!("std::thread::spawn failed: {e}"))?;
                (std::ptr::null_mut(), Some(thread))
            }
        };
        if handle.is_null() && joiner.is_none() {
            return Err(format!("{} could not be made", way.name()));
        }
        WAYS_OF[index].store(way as u32 + 1, SeqCst);
        self.workers.push(Worker { index, way, handle, joiner });
        Ok(())
    }

    /// Waits until every worker said its id and checked its start (an
    /// event, at the frame the main thread chose), then checks that the ids
    /// differ (all of them are alive now) and lets them go on.
    pub fn go(&self) -> Result<(), String> {
        let s = shared();
        wait(s.ready, "the workers' ready")?;
        let mut ids: Vec<u32> = self.workers.iter().filter(|w| !w.way.stuck()).map(|w| SAID[w.index].thread_id.load(SeqCst)).collect();
        // SAFETY: a plain query of the calling thread.
        ids.push(unsafe { win::GetCurrentThreadId() });
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        if sorted.len() != ids.len() || sorted.contains(&0) {
            return Err(format!("the threads alive at once do not have different thread ids ({ids:?})"));
        }
        // SAFETY: our own event.
        unsafe { win::SetEvent(s.go) };
        Ok(())
    }

    /// Waits until every worker filled its part of the buffer.
    pub fn filled(&self) -> Result<(), String> {
        wait(shared().filled, "the workers' fill")
    }

    /// Waits until the stuck workers are about to wait for ever.
    pub fn stuck(&self) -> Result<(), String> {
        wait(shared().stuck, "the stuck workers")
    }

    /// Joins every worker but the stuck ones and checks what they and the
    /// images said of them; the summary.
    pub fn join(self) -> Result<String, String> {
        let mut joined = 0;
        for worker in self.workers {
            if worker.way.stuck() {
                continue;
            }
            let Worker { index, way, handle, joiner } = worker;
            let code = match joiner {
                Some(thread) => {
                    thread.join().map_err(|_| format!("{} (worker {index}) panicked", way.name()))?;
                    None
                }
                None => {
                    wait(handle, way.name())?;
                    let mut code = 0;
                    // SAFETY: the thread's handle, which it ended with.
                    unsafe {
                        win::GetExitCodeThread(handle, &mut code);
                        win::CloseHandle(handle);
                    }
                    Some(code)
                }
            };
            if code.is_some_and(|c| c != way.code(index)) {
                return Err(format!("{} (worker {index}) ended with {code:?}, not {:#x}", way.name(), way.code(index)));
            }
            ended_once(index, way)?;
            joined += 1;
        }
        if let Some(first) = PROBLEMS.lock().unwrap_or_else(|e| e.into_inner()).first() {
            return Err(first.clone());
        }
        Ok(format!(
            "{joined} threads of its own (CreateThread, _beginthreadex, std::thread; ended by returning, ExitThread, \
             _endthreadex, FreeLibraryAndExitThread) ran beside its frames with their own TLS in it and in each DLL, the template's values first; \
             each image heard of each one's start and end once, and each TLS destructor ran once"
        ))
    }
}

/// The way each index was made (`Way as u32 + 1`; 0: none), for the start
/// routines, which get only the index.
static WAYS_OF: [AtomicU32; MAX] = [const { AtomicU32::new(0) }; MAX];

fn way_of(index: usize) -> Way {
    const ALL: [Way; 9] = [
        Way::Waiter,
        Way::Win32Returns,
        Way::Win32Exits,
        Way::CrtReturns,
        Way::CrtEnds,
        Way::Win32FreesLate,
        Way::Std,
        Way::StuckWin32,
        Way::StuckStd,
    ];
    // The thread may run before `start` stored its way: wait for it.
    loop {
        match WAYS_OF[index].load(SeqCst) {
            0 => std::thread::yield_now(),
            n => return ALL[n as usize - 1],
        }
    }
}

unsafe extern "system" fn win32_start(parameter: *mut c_void) -> u32 {
    let index = parameter as usize;
    let way = way_of(index);
    body(index, way);
    match way {
        // SAFETY: ends this thread, which holds nothing.
        Way::Win32Exits => unsafe { win::ExitThread(way.code(index)) },
        // SAFETY: gives back the reference the title took for this thread
        // (nothing of this thread uses the DLL any more) and ends it.
        Way::Win32FreesLate => unsafe {
            win::FreeLibraryAndExitThread(REFERENCE.load(SeqCst) as Handle, way.code(index))
        },
        _ => way.code(index),
    }
}

/// The late DLL's module, from a `LoadLibraryW` taken for the worker that
/// ends with `FreeLibraryAndExitThread`.
static REFERENCE: AtomicUsize = AtomicUsize::new(0);

unsafe extern "system" fn crt_start(parameter: *mut c_void) -> u32 {
    let index = parameter as usize;
    let way = way_of(index);
    body(index, way);
    match way {
        // SAFETY: ends this thread through the C runtime; it holds nothing.
        Way::CrtEnds => unsafe { win::_endthreadex(way.code(index)) },
        _ => way.code(index),
    }
}

fn wait(handle: Handle, what: &str) -> Result<(), String> {
    // SAFETY: a handle of ours.
    match unsafe { win::WaitForSingleObject(handle, win::INFINITE) } {
        win::WAIT_OBJECT_0 => Ok(()),
        other => Err(format!("waiting for {what} failed ({other:#x})")),
    }
}

/// The checks of a thread's own TLS in one DLL on one call: the DLL heard
/// of the thread's start (unless it was loaded after the thread began), and
/// its TLS held the template's values, then the thread's own.
fn check_work(index: usize, way: Way, dll: &Dll, calls: u32, attached: bool) -> u32 {
    let work = dll.work();
    let name = dll.name;
    // SAFETY: a plain query of the calling thread.
    let id = unsafe { win::GetCurrentThreadId() };
    if work.calls != calls {
        problem(index, way, format!("{name} counted {} calls on this thread, not {calls}", work.calls));
    }
    if work.tls_ok != 1 {
        problem(index, way, format!("{name}'s TLS on this thread did not hold the template's values, then its own"));
    }
    if work.thread_id != id {
        problem(index, way, format!("{name}'s GetCurrentThreadId says another thread"));
    }
    if work.attaches != u32::from(attached) {
        problem(index, way, format!("{name} heard of this thread's start {} times, not {}", work.attaches, u32::from(attached)));
    }
    work.token
}

/// A worker's life, but its end.
fn body(index: usize, way: Way) {
    let s = shared();
    let said = &SAID[index];
    // SAFETY: a plain query of the calling thread.
    let id = unsafe { win::GetCurrentThreadId() };
    said.thread_id.store(id, SeqCst);
    if way == Way::Waiter {
        // SAFETY: our own event.
        unsafe { win::SetEvent(s.begun) };
    }
    // The executable's TLS: the template's values, its start heard of, the
    // destructor's value from the template too.
    if tls::VALUES.with(std::cell::Cell::get) != tls::TEMPLATE {
        problem(index, way, "the executable's TLS did not start with its template's values");
    }
    let token = tls::token();
    match tls::slot(token) {
        None => problem(index, way, "the executable's TLS callback did not hear of this thread's start"),
        Some(slot) if slot.attaches.load(SeqCst) != 1 => {
            problem(index, way, "the executable's TLS callback heard of this thread's start more than once")
        }
        Some(_) => {}
    }
    said.exe_token.store(token, SeqCst);
    if !tls::guard() {
        problem(index, way, "the executable's TLS value with a destructor did not start as its template");
    }
    tls::VALUES.with(|v| v.set([tls::TEMPLATE[0], tls::TEMPLATE[1], index as u32, id]));
    said.static_token.store(check_work(index, way, &s.statics, 1, true), SeqCst);
    let late = *s.late.lock().unwrap_or_else(|e| e.into_inner());
    if way != Way::Waiter {
        match late {
            Some(late) => said.late_token.store(check_work(index, way, &late, 1, true), SeqCst),
            None => problem(index, way, "started before the late DLL was loaded"),
        }
    }
    if way.stuck() {
        if s.stucks.fetch_add(1, SeqCst) + 1 == s.stuck_expected {
            // SAFETY: our own event.
            unsafe { win::SetEvent(s.stuck) };
        }
        let _ = wait(s.never, "nothing");
        return;
    }
    if s.readies.fetch_add(1, SeqCst) + 1 == s.expected {
        // SAFETY: our own event.
        unsafe { win::SetEvent(s.ready) };
    }
    // The other workers are alive now, using their own TLS.
    let _ = wait(s.go, "the main thread's go");
    if tls::VALUES.with(std::cell::Cell::get) != [tls::TEMPLATE[0], tls::TEMPLATE[1], index as u32, id] {
        problem(index, way, "the executable's TLS on this thread holds another thread's values");
    }
    check_work(index, way, &s.statics, 2, true);
    // The waiter looks again: the DLL was loaded while it waited.
    let late = *s.late.lock().unwrap_or_else(|e| e.into_inner());
    match (way, late) {
        // Made before the DLL was loaded: no start heard of, but its own TLS
        // with the template's values all the same.
        (Way::Waiter, Some(late)) => said.late_token.store(check_work(index, way, &late, 1, false), SeqCst),
        (_, Some(late)) => {
            check_work(index, way, &late, 2, true);
        }
        (_, None) => problem(index, way, "the late DLL was not loaded by the go"),
    }
    let at = s.buffer.load(SeqCst) + index * PART;
    for i in 0..PART {
        // SAFETY: the worker's own part of the buffer, in title memory that
        // lives as long as the process; the GPU reads it only after `filled`.
        unsafe { ((at + i) as *mut u8).write_volatile(pattern(index, i)) };
    }
    if s.fills.fetch_add(1, SeqCst) + 1 == s.expected {
        // SAFETY: our own event.
        unsafe { win::SetEvent(s.filled) };
    }
}

/// After worker `index` ended: each image heard of its end once (and of its
/// start, unless loaded after it), and each TLS destructor ran once.
fn ended_once(index: usize, way: Way) -> Result<(), String> {
    let said = &SAID[index];
    let fail = |what: String| Err(format!("{} (worker {index}): {what}", way.name()));
    let exe = tls::slot(said.exe_token.load(SeqCst)).map(|s| [s.attaches.load(SeqCst), s.detaches.load(SeqCst), s.drops.load(SeqCst)]);
    if exe != Some([1, 1, 1]) {
        return fail(format!("the executable heard of its start and end and ran its TLS destructor {exe:?} times, not once each"));
    }
    let s = shared();
    let late = (*s.late.lock().unwrap_or_else(|e| e.into_inner())).ok_or("the late DLL was not loaded")?;
    for (dll, token, attaches) in [
        (&s.statics, said.static_token.load(SeqCst), 1),
        (&late, said.late_token.load(SeqCst), u32::from(way != Way::Waiter)),
    ] {
        let notes = dll.notes(token);
        let want = crate::dlls::Notes { attaches, detaches: 1, drops: 1 };
        if token == 0 || notes != want {
            return fail(format!("{} heard of its start, its end and ran its TLS destructor {notes:?} times, not {want:?}", dll.name));
        }
    }
    Ok(())
}
