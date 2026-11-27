//! The executable's own static TLS and TLS callback, as a title's are.
//!
//! Under Rox-box the executable is mapped as a DLL with its TLS callbacks
//! detached, so they hear of the process and its threads only from the
//! loader (`victory-loader/src/threads.rs`), and its thread-locals are
//! reached through a TLS slot other than the one an executable assumes
//! (`victory-loader/src/tls.rs`). This callback notes what it hears under a
//! token per thread (as the DLLs do, `demo-static-dll/src/dll.rs`: a
//! thread id is handed on when its thread ends), and at the process's end
//! checks what the title's DLLs said of theirs, which nothing of the
//! title's can do later.
//!
//! The callback runs as a thread starts, before Rust's set-up of it: it
//! touches only atomics and thread-locals without a destructor.

use std::cell::Cell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::SeqCst};

use crate::dlls::Counts;
use crate::win;

const DLL_PROCESS_DETACH: u32 = 0;
const DLL_PROCESS_ATTACH: u32 = 1;
const DLL_THREAD_ATTACH: u32 = 2;
const DLL_THREAD_DETACH: u32 = 3;

/// The values every thread's TLS starts with (the image's TLS template).
pub const TEMPLATE: [u32; 4] = [0x7e57_e001, 0x7e57_e002, 0x7e57_e003, 0x7e57_e004];
const GUARD_MAGIC: u32 = 0x6a7d_e001;

thread_local! {
    /// The thread's token; 0 until its start was heard of.
    static TOKEN: Cell<u32> = const { Cell::new(0) };
    /// The template's values; a worker puts its own in the last two.
    pub static VALUES: Cell<[u32; 4]> = const { Cell::new(TEMPLATE) };
    /// Its destructor marks the thread's notes.
    static GUARD: Guard = const { Guard { magic: GUARD_MAGIC } };
}

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

/// Tokens the notes hold (every thread of the process gets one).
const TOKENS: usize = 8192;
static NEXT_TOKEN: AtomicU32 = AtomicU32::new(1);
static TWICE: AtomicU32 = AtomicU32::new(0);
static OVERFLOW: AtomicU32 = AtomicU32::new(0);
static PROCESS_ATTACHES: AtomicU32 = AtomicU32::new(0);
static PROCESS_DETACHES: AtomicU32 = AtomicU32::new(0);

pub struct Slot {
    pub attaches: AtomicU32,
    pub detaches: AtomicU32,
    pub drops: AtomicU32,
}

static SLOTS: [Slot; TOKENS] = [const {
    Slot {
        attaches: AtomicU32::new(0),
        detaches: AtomicU32::new(0),
        drops: AtomicU32::new(0),
    }
}; TOKENS];

pub fn slot(token: u32) -> Option<&'static Slot> {
    match token as usize {
        0 => None,
        t if t < TOKENS => Some(&SLOTS[t]),
        _ => {
            OVERFLOW.fetch_add(1, SeqCst);
            None
        }
    }
}

/// Windows calls every pointer between the C runtime's markers `.CRT$XLA`
/// and `.CRT$XLZ`; Rust's own (its TLS destructors) is in `.CRT$XLB`, so
/// on a thread's end those have run when this one hears of it.
#[used]
#[unsafe(link_section = ".CRT$XLC")]
static ON_THREAD: unsafe extern "system" fn(*mut c_void, u32, *mut c_void) = on_thread;

unsafe extern "system" fn on_thread(_: *mut c_void, reason: u32, _: *mut c_void) {
    match reason {
        DLL_PROCESS_ATTACH => {
            PROCESS_ATTACHES.fetch_add(1, SeqCst);
        }
        DLL_THREAD_ATTACH => TOKEN.with(|t| {
            if t.get() != 0 {
                TWICE.fetch_add(1, SeqCst);
                return;
            }
            t.set(NEXT_TOKEN.fetch_add(1, SeqCst));
            if let Some(slot) = slot(t.get()) {
                slot.attaches.fetch_add(1, SeqCst);
            }
        }),
        DLL_THREAD_DETACH => {
            if let Some(slot) = slot(TOKEN.with(Cell::get))
                && slot.detaches.fetch_add(1, SeqCst) > 0
            {
                TWICE.fetch_add(1, SeqCst);
            }
        }
        DLL_PROCESS_DETACH => {
            PROCESS_DETACHES.fetch_add(1, SeqCst);
            at_exit();
        }
        _ => {}
    }
}

/// The process attach this image's TLS callback heard of: once, before its
/// entry point.
pub fn process_attaches() -> u32 {
    // SAFETY: reading our own static keeps the linker from dropping it.
    unsafe { std::ptr::from_ref(&ON_THREAD).read_volatile() };
    PROCESS_ATTACHES.load(SeqCst)
}

/// This thread's token, from its start's notice (0: none was heard).
pub fn token() -> u32 {
    TOKEN.with(Cell::get)
}

/// Uses the value with a destructor on this thread (which registers the
/// destructor); whether it held the template's value.
pub fn guard() -> bool {
    GUARD.with(|g| g.magic == GUARD_MAGIC)
}

/// Notices or destructors that came twice to one thread, and threads past
/// the notes.
pub fn twice() -> (u32, u32) {
    (TWICE.load(SeqCst), OVERFLOW.load(SeqCst))
}

/// What the title's DLLs write at their process detach, in the title's
/// own memory (`demo_dll_report_to`), and whether the end is to be checked.
pub static STATIC_AT_EXIT: Ending = Ending::new();
pub static LATE_AT_EXIT: Ending = Ending::new();
/// `demo_twin.dll`'s, which the title frees before its end.
pub static TWIN_AT_EXIT: Ending = Ending::new();
/// Set once both DLLs report to the above: the end is checked from then.
pub static CHECK_EXIT: AtomicBool = AtomicBool::new(false);

pub struct Ending(std::cell::UnsafeCell<Counts>);

// SAFETY: written only by the DLL's `DllMain` (volatile, at its detach) and
// read by `at_exit`, on the thread that detaches the images, after it.
unsafe impl Sync for Ending {}

impl Ending {
    const fn new() -> Ending {
        Ending(std::cell::UnsafeCell::new(Counts::zero()))
    }

    pub fn place(&self) -> *mut Counts {
        self.0.get()
    }

    /// Empty again, for a DLL loaded again.
    pub fn clear(&self) {
        // SAFETY: see `Sync`; the DLL that wrote it is gone.
        unsafe { self.0.get().write_volatile(Counts::zero()) }
    }

    pub fn read(&self) -> Counts {
        // SAFETY: see `Sync`.
        unsafe { self.0.get().read_volatile() }
    }
}

/// The process ends (the loader detaches the images: the DLLs first, then
/// this one): each DLL must have heard of the end exactly once by now, and
/// no thread of its start or end twice. Said in one line, written straight
/// to standard output; a mismatch ends the process with code 1 at once.
fn at_exit() {
    if !CHECK_EXIT.load(SeqCst) {
        return;
    }
    let (statics, late, twin) = (STATIC_AT_EXIT.read(), LATE_AT_EXIT.read(), TWIN_AT_EXIT.read());
    let (twice, overflow) = twice();
    let problem = if statics.process_detaches != 1 {
        Some("demo_static.dll did not hear of the process's end once")
    } else if late.process_detaches != 1 {
        Some("demo_late.dll did not hear of the process's end once")
    } else if twin.process_detaches != 1 {
        Some("demo_twin.dll did not hear of the process's end once")
    } else if statics.twice + late.twice + twin.twice + twice != 0 {
        Some("a thread's start or end, or a TLS destructor, came twice")
    } else if statics.overflow + late.overflow + twin.overflow + overflow != 0 {
        Some("more threads than its notes hold")
    } else if PROCESS_DETACHES.load(SeqCst) != 1 {
        Some("its executable heard of the process's end more than once")
    } else {
        None
    };
    match problem {
        None => win::say_raw(
            "demo title: at its exit, each of its DLLs heard of the process's end once, and no thread's start or end came twice\n",
        ),
        Some(problem) => {
            // Not formatted: the heap's lock may be held by a thread that
            // was ended in it.
            win::say_raw("demo title failed: at its exit, ");
            win::say_raw(problem);
            win::say_raw("\n");
            // SAFETY: ends this process, which is ending anyway.
            unsafe { win::NtTerminateProcess(-1isize as win::Handle, 1) };
        }
    }
}
