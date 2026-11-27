//! What both of the demo title's DLLs do (docs/demo-title.md, "Its DLLs"):
//! the one it imports (`demo_static.dll`) and the one it loads by name
//! (`demo_late.dll`) are this same code, so the title can check the two
//! ways a DLL of its own is mapped, bound and told of the process's life.
//!
//! Each records, in a way the title reads back through its exports, what
//! its `DllMain` heard: the process's attach (on which thread, with which
//! instance handle) and detach, and every thread's start and end. The
//! notes of a thread are kept under a token the thread gets at its first
//! notice (in the DLL's own static TLS), not under its thread id: Windows
//! hands a finished thread's id to the next thread, which may start before
//! the title looks. A token is never reused.
//!
//! Its static TLS has initial values the title checks on each thread (the
//! loader must give a late DLL's TLS to threads that already ran, too),
//! and a value with a destructor, which must run once on each thread that
//! used it. It imports from the platform's DLLs itself (`kernel32`: the
//! thread id from the host, its own file name from Rox-box), so its
//! imports are bound by Rox-box as the title's are.
//!
//! Nothing here runs Rust's thread set-up or allocates in `DllMain`: it
//! runs as the thread starts, before Rust's own per-thread state exists.

use std::cell::Cell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicUsize, Ordering::SeqCst};

const DLL_PROCESS_DETACH: u32 = 0;
const DLL_PROCESS_ATTACH: u32 = 1;
const DLL_THREAD_ATTACH: u32 = 2;
const DLL_THREAD_DETACH: u32 = 3;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentThreadId() -> u32;
    fn GetModuleFileNameW(module: *mut c_void, out: *mut u16, size: u32) -> u32;
}

/// The process-wide notes, as `demo_dll_counts` hands them out and as
/// `DllMain` writes them into the title's memory at the process's end
/// (`demo_dll_report_to`): the title reads them after this DLL is gone.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Counts {
    pub process_attaches: u32,
    pub process_detaches: u32,
    pub thread_attaches: u32,
    pub thread_detaches: u32,
    /// The thread its process attach ran on.
    pub attached_on: u32,
    /// Notices or destructors that came a second time to one thread.
    pub twice: u32,
    /// Threads beyond what the notes can hold.
    pub overflow: u32,
    pub _pad: u32,
    /// The instance handle `DllMain` was given.
    pub module: usize,
}

/// What one call of `demo_dll_work` saw on its thread.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Work {
    /// The thread's token here.
    pub token: u32,
    /// Thread starts `DllMain` heard of under that token (0 for a thread
    /// that ran before the DLL was loaded).
    pub attaches: u32,
    /// Calls on this thread, this one included.
    pub calls: u32,
    /// 1 if the TLS held the template's values at the first call and only
    /// this thread's own since.
    pub tls_ok: u32,
    /// The thread's id, by the DLL's own import.
    pub thread_id: u32,
    pub _pad: u32,
}

/// What a token's thread heard of.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Notes {
    pub attaches: u32,
    pub detaches: u32,
    /// The TLS destructor ran.
    pub drops: u32,
}

static PROCESS_ATTACHES: AtomicU32 = AtomicU32::new(0);
static PROCESS_DETACHES: AtomicU32 = AtomicU32::new(0);
static THREAD_ATTACHES: AtomicU32 = AtomicU32::new(0);
static THREAD_DETACHES: AtomicU32 = AtomicU32::new(0);
static ATTACHED_ON: AtomicU32 = AtomicU32::new(0);
static TWICE: AtomicU32 = AtomicU32::new(0);
static OVERFLOW: AtomicU32 = AtomicU32::new(0);
static MODULE: AtomicUsize = AtomicUsize::new(0);
static REPORT_TO: AtomicPtr<Counts> = AtomicPtr::new(std::ptr::null_mut());

/// Threads the notes hold: every thread of the process starts here (the
/// runtime's and the host's too), a few hundred in a run.
const TOKENS: usize = 8192;
static NEXT_TOKEN: AtomicU32 = AtomicU32::new(1);

struct Slot {
    attaches: AtomicU32,
    detaches: AtomicU32,
    drops: AtomicU32,
}

static SLOTS: [Slot; TOKENS] = [const {
    Slot {
        attaches: AtomicU32::new(0),
        detaches: AtomicU32::new(0),
        drops: AtomicU32::new(0),
    }
}; TOKENS];

/// The values every thread's TLS starts with (the image's TLS template).
pub const TEMPLATE: [u32; 4] = [0x7e57_0001, 0x7e57_0002, 0x7e57_0003, 0x7e57_0004];

thread_local! {
    /// This thread's token; 0 until its first notice or call.
    static TOKEN: Cell<u32> = const { Cell::new(0) };
    /// The template's values, then this thread's id in the last place.
    static VALUES: Cell<[u32; 4]> = const { Cell::new(TEMPLATE) };
    static CALLS: Cell<u32> = const { Cell::new(0) };
    /// Its destructor marks the thread's notes.
    static GUARD: Guard = const { Guard { magic: GUARD_MAGIC } };
}

const GUARD_MAGIC: u32 = 0x6a7d_0001;

struct Guard {
    magic: u32,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(slot) = slot(TOKEN.with(Cell::get))
            && slot.drops.fetch_add(1, SeqCst) > 0
        {
            TWICE.fetch_add(1, SeqCst);
        }
    }
}

fn slot(token: u32) -> Option<&'static Slot> {
    match token as usize {
        0 => None,
        t if t < TOKENS => Some(&SLOTS[t]),
        _ => {
            OVERFLOW.fetch_add(1, SeqCst);
            None
        }
    }
}

/// This thread's token, given now if it has none.
fn token() -> u32 {
    TOKEN.with(|t| {
        if t.get() == 0 {
            t.set(NEXT_TOKEN.fetch_add(1, SeqCst));
        }
        t.get()
    })
}

fn counts() -> Counts {
    Counts {
        process_attaches: PROCESS_ATTACHES.load(SeqCst),
        process_detaches: PROCESS_DETACHES.load(SeqCst),
        thread_attaches: THREAD_ATTACHES.load(SeqCst),
        thread_detaches: THREAD_DETACHES.load(SeqCst),
        attached_on: ATTACHED_ON.load(SeqCst),
        twice: TWICE.load(SeqCst),
        overflow: OVERFLOW.load(SeqCst),
        _pad: 0,
        module: MODULE.load(SeqCst),
    }
}

/// Called by the C runtime's DLL entry point.
#[unsafe(no_mangle)]
pub extern "system" fn DllMain(module: *mut c_void, reason: u32, _: *mut c_void) -> i32 {
    match reason {
        DLL_PROCESS_ATTACH => {
            PROCESS_ATTACHES.fetch_add(1, SeqCst);
            // SAFETY: a plain query of the calling thread.
            ATTACHED_ON.store(unsafe { GetCurrentThreadId() }, SeqCst);
            MODULE.store(module as usize, SeqCst);
        }
        DLL_THREAD_ATTACH => {
            THREAD_ATTACHES.fetch_add(1, SeqCst);
            if TOKEN.with(Cell::get) != 0 {
                TWICE.fetch_add(1, SeqCst);
            } else if let Some(slot) = slot(token()) {
                slot.attaches.fetch_add(1, SeqCst);
            }
        }
        DLL_THREAD_DETACH => {
            THREAD_DETACHES.fetch_add(1, SeqCst);
            // A thread that ran before this DLL was loaded hears of its
            // end too, as on Windows; it has a token if it called us.
            if let Some(slot) = slot(TOKEN.with(Cell::get))
                && slot.detaches.fetch_add(1, SeqCst) > 0
            {
                TWICE.fetch_add(1, SeqCst);
            }
        }
        DLL_PROCESS_DETACH => {
            if PROCESS_DETACHES.fetch_add(1, SeqCst) > 0 {
                TWICE.fetch_add(1, SeqCst);
            }
            let to = REPORT_TO.load(SeqCst);
            if !to.is_null() {
                // SAFETY: the title gave a `Counts` of its own memory, which
                // lives as long as its process.
                unsafe { to.write_volatile(counts()) };
            }
        }
        _ => {}
    }
    1
}

/// The process-wide notes now.
#[unsafe(no_mangle)]
pub extern "system" fn demo_dll_counts(out: *mut Counts) {
    if !out.is_null() {
        // SAFETY: the title passes a `Counts` to fill.
        unsafe { out.write(counts()) };
    }
}

/// Where `DllMain` writes the notes at the process's end (or this DLL's).
#[unsafe(no_mangle)]
pub extern "system" fn demo_dll_report_to(out: *mut Counts) {
    REPORT_TO.store(out, SeqCst);
}

/// What `token`'s thread heard of.
#[unsafe(no_mangle)]
pub extern "system" fn demo_dll_notes(token: u32, out: *mut Notes) {
    let notes = slot(token).map_or(Notes::default(), |s| Notes {
        attaches: s.attaches.load(SeqCst),
        detaches: s.detaches.load(SeqCst),
        drops: s.drops.load(SeqCst),
    });
    if !out.is_null() {
        // SAFETY: the title passes a `Notes` to fill.
        unsafe { out.write(notes) };
    }
}

/// The function the title calls from its threads: it uses this thread's
/// TLS (the template's values at first, then the thread's own) and says
/// what it saw. Returns the calls on this thread.
#[unsafe(no_mangle)]
pub extern "system" fn demo_dll_work(out: *mut Work) -> u32 {
    // SAFETY: a plain query of the calling thread.
    let id = unsafe { GetCurrentThreadId() };
    let token = token();
    let calls = CALLS.with(|c| {
        c.set(c.get() + 1);
        c.get()
    });
    let want = if calls == 1 { TEMPLATE } else { [TEMPLATE[0], TEMPLATE[1], TEMPLATE[2], id] };
    let values = VALUES.with(Cell::get);
    VALUES.with(|v| v.set([TEMPLATE[0], TEMPLATE[1], TEMPLATE[2], id]));
    // Registers the destructor (on the first call), and checks the value
    // came from the template too.
    let guard_ok = GUARD.with(|g| g.magic == GUARD_MAGIC);
    let work = Work {
        token,
        attaches: slot(token).map_or(0, |s| s.attaches.load(SeqCst)),
        calls,
        tls_ok: u32::from(values == want && guard_ok),
        thread_id: id,
        _pad: 0,
    };
    if !out.is_null() {
        // SAFETY: the title passes a `Work` to fill.
        unsafe { out.write(work) };
    }
    calls
}

/// This DLL's own file name, by its own `GetModuleFileNameW` import with
/// the instance handle `DllMain` was given (what the title's runtime says
/// a title image's path is). Returns its length, as the import does.
#[unsafe(no_mangle)]
pub extern "system" fn demo_dll_own_path(out: *mut u16, size: u32) -> u32 {
    // SAFETY: the title passes a buffer of `size` units; the handle is ours.
    unsafe { GetModuleFileNameW(MODULE.load(SeqCst) as *mut c_void, out, size) }
}
