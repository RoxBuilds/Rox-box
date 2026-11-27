//! `mwaitx` as a wait (`VICTORY_MWAITX_WAIT=1`, off by default): the title's
//! `monitorx` and `mwaitx` places call code of ours that does what the two
//! instructions do on the console, where by default they become a no-op and a
//! `pause` ([`super::wait_patch`]).
//!
//! One thread of the title waits for a word of its own memory in a loop of
//! `monitorx`, a look at the word, `mwaitx` with a timer, another look, and
//! `Sleep(0)`. With `mwaitx` a `pause` the loop spins (roughly three quarters of
//! a core). On the console `mwaitx` sleeps until the line `monitorx` armed is
//! written or its timer ends. Nothing here tells us of a write, so [`wait`]
//! returns at once if the line changed since `monitorx` and else sleeps one
//! slice on a timer of the thread's own (`VICTORY_MWAITX_WAIT_US`, the timer due
//! 250 us on; it ends on its next OS timer tick past that, typically 0.5-0.8 ms
//! or 1.0-1.3 ms for 500 us). The title looks at its word again right after
//! after any wake-up, so a wait of ours can only make it see a change later: by
//! a slice at most.
//!
//! A place is replaced by a jump to a stub near the code (in the memory of the
//! `clzero` stubs). The jump takes the instruction's 3 bytes and the whole
//! instructions after it, 5 bytes or more in all, inside its aligned 16, so one
//! store puts it there ([`super::patch_bytes`]). The stub keeps every register
//! and the flags (the general registers on the stack; the x87, SSE and AVX state
//! by `xsave`), calls [`armed`] or [`wait`], puts them back, runs the
//! instructions the jump took the place of and jumps back past them. No thread
//! may be inside those bytes when the jump is stored, so a place is replaced at
//! the first fault of its instruction, before any thread ran past it (in this
//! mode that fault is not stepped over: the thread runs the jump), and only
//! where nothing in its function branches into the bytes taken and those do the
//! same at any address ([`plain_length`]). Any other place is replaced as by
//! default. Stack walks (the profiler, crash reports) stop in a stub: it has no
//! unwind data.

use std::cell::Cell;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use windows_sys::Win32::Foundation::HANDLE;

use super::decode::ConsoleOnly;

/// Whether `monitorx` and `mwaitx` places call ours ([`replace`]):
/// `VICTORY_MWAITX_WAIT=1`. Off by default: they spin as before.
pub(super) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_MWAITX_WAIT").is_some_and(|v| v == "1"))
}

/// When a wait's timer is due, in microseconds: `VICTORY_MWAITX_WAIT_US`
/// (250; 50 to 20,000). It ends on its next tick past that.
fn slice_us() -> u32 {
    static US: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *US.get_or_init(|| {
        let set = std::env::var("VICTORY_MWAITX_WAIT_US").ok().and_then(|v| v.trim().parse::<u32>().ok());
        set.map_or(250, |us| us.clamp(50, 20_000))
    })
}

/// Places seen at their first fault (address, the bytes there then) and
/// whether each was replaced. One that was not is not replaced later either: a
/// thread may have been stepped past its instruction by then.
static PLACES: Mutex<Vec<(usize, [u8; 16], bool)>> = Mutex::new(Vec::new());

/// Replaces the wait instruction (`monitorx` or `mwaitx` of 3 bytes) at `at`,
/// whose bytes and those after it are `code`, by a jump to a stub that calls
/// ours: whether the jump is there, for the faulting thread to run (its `rip`
/// left as it is). `false`: not replaced, now or later; the caller does what it
/// does by default.
///
/// # Safety
/// `at` is in code that stays mapped, where the instruction just faulted, and
/// every fault at `at` so far came here first (none was stepped over before).
pub(super) unsafe fn replace(at: usize, code: &[u8], what: ConsoleOnly) -> bool {
    let call: extern "system" fn(&Saved) = match what {
        ConsoleOnly::Monitorx => armed,
        ConsoleOnly::Mwaitx => wait,
        _ => return false,
    };
    let mut seen = [0u8; 16];
    let n = code.len().min(16);
    seen[..n].copy_from_slice(&code[..n]);
    let mut places = PLACES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(&(_, _, done)) = places.iter().find(|p| p.0 == at && p.1 == seen) {
        // (Another thread's fault at the same place, handled first.)
        return done;
    }
    // SAFETY: the caller's.
    let made = unsafe { make(at, code, call as usize) };
    match made {
        Ok(stub) => tracing::info!(
            instruction = ?what,
            at = format_args!("{at:#x}"),
            stub = format_args!("{stub:#x}"),
            slice_us = slice_us(),
            "a console wait instruction calls a wait of ours now (VICTORY_MWAITX_WAIT=1)"
        ),
        Err(why) => tracing::warn!(
            instruction = ?what,
            at = format_args!("{at:#x}"),
            why,
            "a console wait instruction is replaced as without VICTORY_MWAITX_WAIT"
        ),
    }
    places.push((at, seen, made.is_ok()));
    made.is_ok()
}

/// [`replace`] by a jump to a new stub that calls `call`: where the stub is, or
/// why the place is not replaced.
///
/// # Safety
/// As [`replace`].
unsafe fn make(at: usize, code: &[u8], call: usize) -> Result<usize, &'static str> {
    use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let taken = taken_len(code).ok_or("the instructions after it are not of a form known here")?;
    if (at & 15) + taken > 16 {
        return Err("the bytes a jump takes cross the end of its aligned 16");
    }
    no_branch_into(at, taken)?;
    let state = xsave_state().ok_or("no xsave")?;
    let mut stubs = super::STUBS.lock().unwrap_or_else(|e| e.into_inner());
    if stubs.is_none() {
        *stubs = super::near(at).map(|place| (place, 0));
    }
    let (place, used) = stubs.as_mut().ok_or("no memory for stubs within a jump's reach")?;
    let stub = *place + *used;
    let body = stub_code(stub, call, state, &code[3..taken], at + taken).ok_or("out of a jump's reach")?;
    let jump = super::jump(at, stub).ok_or("out of a jump's reach")?;
    if *used + body.len() > super::STUBS_LEN {
        return Err("the stubs' memory is full");
    }
    // The bytes after the jump are never run: the stub comes back past them.
    let mut new = [0xcc_u8; 16];
    new[..5].copy_from_slice(&jump);
    // SAFETY: inside the memory made for stubs, which nothing runs yet; then the
    // place, by the caller's contract.
    unsafe {
        std::ptr::copy_nonoverlapping(body.as_ptr(), stub as *mut u8, body.len());
        FlushInstructionCache(GetCurrentProcess(), stub as *const _, body.len());
        *used += body.len().next_multiple_of(16);
        if !super::patch_bytes(at, &code[..taken], &new[..taken]) {
            return Err("the place could not be written");
        }
    }
    Ok(stub)
}

/// How many bytes a jump at a wait instruction takes, whose bytes and those
/// after it are `code`: its own 3 and the whole instructions after it, 5 or
/// more in all. `None` where one of those is not of a form [`plain_length`]
/// knows.
fn taken_len(code: &[u8]) -> Option<usize> {
    let mut len = 3;
    while len < 5 {
        len += plain_length(code.get(len..)?)?;
    }
    Some(len)
}

/// The length of the instruction at the start of `code` if it does the same
/// at any address and is of a form known here: no prefix but one `REX`, then an
/// arithmetic, `test`, `mov`, `lea` or `movsxd` of a register with a register or
/// memory (`mod r/m`), the same with an immediate, a `mov` of an immediate to a
/// register, or `nop`. Never a branch or an address relative to `rip`.
fn plain_length(code: &[u8]) -> Option<usize> {
    let rex = code.first().is_some_and(|&b| b & 0xf0 == 0x40);
    let (&op, rest) = code.get(usize::from(rex)..)?.split_first()?;
    let wide = rex && code[0] & 8 != 0;
    let (modrm, immediate) = match op {
        0x90 if !rex => return Some(1),
        // add, or, and, sub, xor, cmp (both ways), movsxd, test, mov (both ways), lea
        0x01 | 0x03 | 0x09 | 0x0b | 0x21 | 0x23 | 0x29 | 0x2b | 0x31 | 0x33 | 0x39 | 0x3b | 0x63 | 0x85 | 0x89
        | 0x8b | 0x8d => (true, 0),
        0x83 => (true, 1),
        0x81 => (true, 4),
        0xc7 if rest.first().is_some_and(|m| (m >> 3) & 7 == 0) => (true, 4),
        0xb8..=0xbf => (false, if wide { 8 } else { 4 }),
        _ => return None,
    };
    let mut len = usize::from(rex) + 1;
    if modrm {
        let &m = rest.first()?;
        len += 1;
        let (md, rm) = (m >> 6, m & 7);
        if md == 3 {
            // `lea` of a register is no instruction.
            if op == 0x8d {
                return None;
            }
        } else if rm == 4 {
            let &sib = rest.get(1)?;
            len += 1 + if md == 0 && sib & 7 == 5 { 4 } else { 0 };
        } else if md == 0 && rm == 5 {
            // Relative to `rip`.
            return None;
        }
        len += match md {
            1 => 1,
            2 => 4,
            _ => 0,
        };
    }
    len += immediate;
    (len <= code.len()).then_some(len)
}

/// `Ok` where nothing in the function that holds `at` (by its unwind data)
/// branches into the `len` bytes from `at`, past their first. Every byte is
/// looked at as if an instruction began there, so no branch is missed; bytes
/// that only look like one keep the place from being replaced, no more. A
/// function in pieces (chained unwind data) is refused: another piece may
/// branch here.
fn no_branch_into(at: usize, len: usize) -> Result<(), &'static str> {
    use windows_sys::Win32::System::Diagnostics::Debug::{RtlLookupFunctionEntry, UNW_FLAG_CHAININFO};
    let mut image = 0u64;
    // SAFETY: looks up the function table entry that covers `at`, if any.
    let entry = unsafe { RtlLookupFunctionEntry(at as u64, &mut image, std::ptr::null_mut()) };
    if entry.is_null() {
        return Err("no unwind data covers it");
    }
    // SAFETY: the entry the system found and the unwind data it names, in the
    // same image.
    let (begin, end, flags) = unsafe {
        let entry = &*entry;
        let info = entry.Anonymous.UnwindInfoAddress;
        let flags = if info & 1 != 0 { u32::MAX } else { u32::from(*((image + u64::from(info)) as *const u8) >> 3) };
        (entry.BeginAddress as usize, entry.EndAddress as usize, flags)
    };
    if flags & UNW_FLAG_CHAININFO != 0 {
        return Err("its function is in pieces");
    }
    let (begin, end) = (image as usize + begin, image as usize + end);
    // SAFETY: the function's own code, mapped with its image.
    let code = unsafe { std::slice::from_raw_parts(begin as *const u8, end.saturating_sub(begin)) };
    if branches_into(code, begin, at + 1..at + len) {
        return Err("a branch of its function may lead inside the bytes a jump takes");
    }
    Ok(())
}

/// Whether a branch read at any byte of `code` (which lies at `base`) leads
/// into `inside`: `jcc`, `jmp`, `loop` and `jrcxz` of 8 bits, `call`, `jmp` and
/// `jcc` of 32.
fn branches_into(code: &[u8], base: usize, inside: std::ops::Range<usize>) -> bool {
    // Where a branch at `p` of `len` bytes leads with `rel`.
    let to = |p: usize, len: usize, rel: i64| (base + p + len) as i64 + rel;
    (0..code.len()).any(|p| {
        let rel8 = || code.get(p + 1).map(|&r| to(p, 2, i64::from(r as i8)));
        let rel32 = |k: usize| {
            let rel = code.get(p + k..p + k + 4)?.try_into().ok().map(i32::from_le_bytes)?;
            Some(to(p, k + 4, i64::from(rel)))
        };
        let target = match code[p] {
            0x70..=0x7f | 0xe0..=0xe3 | 0xeb => rel8(),
            0xe8 | 0xe9 => rel32(1),
            0x0f if code.get(p + 1).is_some_and(|b| b & 0xf0 == 0x80) => rel32(2),
            _ => None,
        };
        target.is_some_and(|t| inside.contains(&(t as usize)))
    })
}

/// What a stub keeps by `xsave`: the state components (x87, SSE, AVX and those
/// of AVX-512, as far as the system has them on) and the bytes they take.
#[derive(Clone, Copy, Debug)]
struct Xsave {
    mask: u64,
    size: u32,
}

/// [`Xsave`] for this processor and system; `None` without `xsave`.
fn xsave_state() -> Option<Xsave> {
    static STATE: std::sync::OnceLock<Option<Xsave>> = std::sync::OnceLock::new();
    *STATE.get_or_init(ask_xsave)
}

#[allow(unused_unsafe)]
fn ask_xsave() -> Option<Xsave> {
    use std::arch::x86_64::{__cpuid, __cpuid_count};
    // SAFETY: `cpuid` is there on every x86-64 processor (leaf 13 wherever
    // `xsave` is); `xgetbv` is asked only where the system turned `xsave` on
    // (CPUID 1, ECX bit 27), and reads a register.
    unsafe {
        if __cpuid(1).ecx & (1 << 27) == 0 {
            return None;
        }
        let (low, high): (u32, u32);
        std::arch::asm!(
            "xgetbv",
            in("ecx") 0,
            out("eax") low,
            out("edx") high,
            options(nomem, nostack, preserves_flags)
        );
        // x87, SSE, AVX, AVX-512's three; not the others (MPX, PKRU, AMX).
        let mask = ((u64::from(high) << 32) | u64::from(low)) & 0xe7;
        let mut size = 576;
        for component in 2..8 {
            if mask & (1 << component) != 0 {
                let c = __cpuid_count(0xd, component);
                size = size.max(c.ebx + c.eax);
            }
        }
        Some(Xsave { mask, size: size.next_multiple_of(64) })
    }
}

/// The stub of a place: at `stub`, it keeps the title's registers, calls
/// `call` with the general ones it pushed ([`Saved`]), puts all back, runs
/// `taken` (the instructions the jump took the place of) and jumps to `back`.
/// `None` where `back` is out of a jump's reach. Below the title's `rsp` it uses
/// some 80 bytes and the state's (under a page), then what `call` does.
fn stub_code(stub: usize, call: usize, state: Xsave, taken: &[u8], back: usize) -> Option<Vec<u8>> {
    // push rbp; mov rbp, rsp; pushfq; cld (our code runs with the direction flag clear)
    let mut c = vec![0x55, 0x48, 0x89, 0xe5, 0x9c, 0xfc];
    // push rax, rcx, rdx, rbx, r8, r9, r10, r11
    c.extend([0x50, 0x51, 0x52, 0x53, 0x41, 0x50, 0x41, 0x51, 0x41, 0x52, 0x41, 0x53]);
    // sub rsp, size; and rsp, -64: room for the state, aligned as `xsave` wants it
    c.extend([0x48, 0x81, 0xec]);
    c.extend(state.size.to_le_bytes());
    c.extend([0x48, 0x83, 0xe4, 0xc0]);
    // xor eax, eax; mov [rsp + 512 + 8k], rax for k < 8: the header `xrstor`
    // checks, of which `xsave` writes a part
    c.extend([0x31, 0xc0]);
    for k in 0..8u32 {
        c.extend([0x48, 0x89, 0x84, 0x24]);
        c.extend((512 + 8 * k).to_le_bytes());
    }
    // mov eax, low; mov edx, high: the components
    let mask = |c: &mut Vec<u8>| {
        c.push(0xb8);
        c.extend((state.mask as u32).to_le_bytes());
        c.push(0xba);
        c.extend(((state.mask >> 32) as u32).to_le_bytes());
    };
    mask(&mut c);
    // xsave64 [rsp]
    c.extend([0x48, 0x0f, 0xae, 0x24, 0x24]);
    if state.mask & 4 != 0 {
        // vzeroupper: our code runs without the title's upper halves, kept above
        c.extend([0xc5, 0xf8, 0x77]);
    }
    // lea rcx, [rbp - 72] (the registers pushed); sub rsp, 32; mov rax, call;
    // call rax; add rsp, 32
    c.extend([0x48, 0x8d, 0x4d, 0xb8, 0x48, 0x83, 0xec, 0x20, 0x48, 0xb8]);
    c.extend((call as u64).to_le_bytes());
    c.extend([0xff, 0xd0, 0x48, 0x83, 0xc4, 0x20]);
    mask(&mut c);
    // xrstor64 [rsp]; lea rsp, [rbp - 72]
    c.extend([0x48, 0x0f, 0xae, 0x2c, 0x24, 0x48, 0x8d, 0x65, 0xb8]);
    // pop r11, r10, r9, r8, rbx, rdx, rcx, rax; popfq; pop rbp
    c.extend([0x41, 0x5b, 0x41, 0x5a, 0x41, 0x59, 0x41, 0x58, 0x5b, 0x5a, 0x59, 0x58, 0x9d, 0x5d]);
    c.extend_from_slice(taken);
    let end = stub + c.len();
    c.extend(super::jump(end, back)?);
    Some(c)
}

/// The title's general registers as a stub pushed them, the lowest address
/// first. (All of them are there; few are read.)
#[repr(C)]
#[allow(dead_code)]
struct Saved {
    r11: u64,
    r10: u64,
    r9: u64,
    r8: u64,
    rbx: u64,
    rdx: u64,
    rcx: u64,
    rax: u64,
    flags: u64,
    rbp: u64,
}

thread_local! {
    /// The line this thread's last `monitorx` armed (its address; 0: none) and
    /// what it held then.
    static ARMED: Cell<(usize, [u64; 8])> = const { Cell::new((0, [0; 8])) };
}

/// The 64 bytes of the line at `line`, a word at a time.
///
/// # Safety
/// The line is mapped and readable.
unsafe fn read_line(line: usize) -> [u64; 8] {
    // SAFETY: the caller's; words other threads may be writing, read whole.
    std::array::from_fn(|k| unsafe { (*((line + 8 * k) as *const AtomicU64)).load(Relaxed) })
}

/// `monitorx`: arms the line at `rax` for this thread's next `mwaitx`.
extern "system" fn armed(saved: &Saved) {
    let line = saved.rax as usize & !63;
    // (No page is ever mapped there; the console would fault.)
    if line < 0x1_0000 {
        return;
    }
    // SAFETY: the console's `monitorx` faults where a load of the byte at `rax`
    // would, and the whole line lies in that byte's page.
    let now = unsafe { read_line(line) };
    let _ = ARMED.try_with(|a| a.set((line, now)));
}

/// `mwaitx`: returns at once if the line this thread armed has changed since,
/// else sleeps a slice; the line is disarmed. A thread with no line armed
/// sleeps a slice too (its `monitorx` may be one not replaced). The thread's
/// last error stays what the title left there.
extern "system" fn wait(saved: &Saved) {
    use windows_sys::Win32::Foundation::{GetLastError, SetLastError};
    // SAFETY: plain calls about this thread.
    let error = unsafe { GetLastError() };
    let (line, then) = ARMED.try_with(|a| a.replace((0, [0; 8]))).unwrap_or((0, [0; 8]));
    // SAFETY: read whole by `armed` on this thread, a moment ago.
    let changed = line != 0 && unsafe { read_line(line) } != then;
    if !changed {
        sleep(slice_us());
    }
    count(changed, saved);
    // SAFETY: as above.
    unsafe { SetLastError(error) };
}

/// Waits so far, those that returned at once (the line had changed), and the
/// console timers the title gave `mwaitx` (`ebx`, in its clock's ticks, with
/// `ecx` bit 1): how many, the shortest and the longest.
static WAITS: AtomicU64 = AtomicU64::new(0);
static AT_ONCE: AtomicU64 = AtomicU64::new(0);
static TIMED: AtomicU64 = AtomicU64::new(0);
static SHORTEST: AtomicU64 = AtomicU64::new(u64::MAX);
static LONGEST: AtomicU64 = AtomicU64::new(0);

/// Counts a wait; a line when the count reaches a power of two from 1,024 on.
fn count(changed: bool, saved: &Saved) {
    if changed {
        AT_ONCE.fetch_add(1, Relaxed);
    }
    if saved.rcx & 2 != 0 {
        let ticks = saved.rbx & 0xffff_ffff;
        TIMED.fetch_add(1, Relaxed);
        SHORTEST.fetch_min(ticks, Relaxed);
        LONGEST.fetch_max(ticks, Relaxed);
    }
    let waits = WAITS.fetch_add(1, Relaxed) + 1;
    if waits >= 1 << 10 && waits.is_power_of_two() {
        let timed = TIMED.load(Relaxed);
        tracing::info!(
            waits,
            at_once = AT_ONCE.load(Relaxed),
            slice_us = slice_us(),
            timed,
            shortest = if timed == 0 { 0 } else { SHORTEST.load(Relaxed) },
            longest = LONGEST.load(Relaxed),
            "the title's mwaitx waits so far (VICTORY_MWAITX_WAIT=1): those that returned at once (its line had \
             been written), the slice the others slept, and the console timers it gave (ebx ticks, ecx bit 1)"
        );
    }
}

#[link(name = "kernel32")]
unsafe extern "system" {
    /// (The `windows-sys` declaration needs its `Win32_Security` part, which
    /// this crate does not take; the attributes are always null here.)
    fn CreateWaitableTimerExW(attributes: *const std::ffi::c_void, name: *const u16, flags: u32, access: u32) -> HANDLE;
}

/// A timer of this thread's own that ends to a fraction of a millisecond;
/// null where the system has none of those (before Windows 10 1803).
struct Timer(HANDLE);

impl Drop for Timer {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: our own handle, closed once.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
        }
    }
}

thread_local! {
    static TIMER: Timer = {
        use windows_sys::Win32::System::Threading::{CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS};
        // SAFETY: a timer of no name with default security.
        let (attributes, name, flags) = (std::ptr::null(), std::ptr::null(), CREATE_WAITABLE_TIMER_HIGH_RESOLUTION);
        Timer(unsafe { CreateWaitableTimerExW(attributes, name, flags, TIMER_ALL_ACCESS) })
    };
}

/// Sleeps `us` microseconds on the thread's timer; without one a millisecond
/// or two (`Sleep(1)` with the process's millisecond timers).
fn sleep(us: u32) {
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::System::Threading::{SetWaitableTimer, Sleep, WaitForSingleObject};
    let slept = TIMER.try_with(|t| {
        // Relative, in units of 100 ns.
        let due = -i64::from(us) * 10;
        // SAFETY: the thread's own timer, set and waited for (20 ms at most,
        // should it never end).
        !t.0.is_null()
            && unsafe {
                SetWaitableTimer(t.0, &due, 0, None, std::ptr::null(), 0) != 0
                    && WaitForSingleObject(t.0, 20) == WAIT_OBJECT_0
            }
    });
    if !matches!(slept, Ok(true)) {
        // SAFETY: a plain call.
        unsafe { Sleep(1) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Diagnostics::Debug::{
        CONTEXT, IMAGE_RUNTIME_FUNCTION_ENTRY, IMAGE_RUNTIME_FUNCTION_ENTRY_0, RtlAddFunctionTable,
        RtlDeleteFunctionTable,
    };

    #[test]
    fn instructions_taken_along_are_those_that_do_the_same_anywhere() {
        // mov rdx, [rdi] (the title's); mov rdx, [r8]; mov rax, rax
        assert_eq!(plain_length(&[0x48, 0x8b, 0x17]), Some(3));
        assert_eq!(plain_length(&[0x49, 0x8b, 0x10]), Some(3));
        assert_eq!(plain_length(&[0x48, 0x8b, 0xc0]), Some(3));
        // mov rax, [rsp + 8]; mov eax, [rbx + rcx*4 + 0x10]; mov rax, [r13 + 0] (disp8)
        assert_eq!(plain_length(&[0x48, 0x8b, 0x44, 0x24, 0x08]), Some(5));
        assert_eq!(plain_length(&[0x8b, 0x44, 0x8b, 0x10]), Some(4));
        assert_eq!(plain_length(&[0x49, 0x8b, 0x45, 0x00]), Some(4));
        // mov eax, [disp32] by a SIB without base; add rax, 0x40; cmp rdx, 0x12345678
        assert_eq!(plain_length(&[0x8b, 0x04, 0x25, 1, 2, 3, 4]), Some(7));
        assert_eq!(plain_length(&[0x48, 0x83, 0xc0, 0x40]), Some(4));
        assert_eq!(plain_length(&[0x48, 0x81, 0xfa, 0x78, 0x56, 0x34, 0x12]), Some(7));
        // mov ecx, 2; mov rax, imm64; mov dword [rax], imm32; nop
        assert_eq!(plain_length(&[0xb9, 2, 0, 0, 0]), Some(5));
        assert_eq!(plain_length(&[0x48, 0xb8, 1, 2, 3, 4, 5, 6, 7, 8]), Some(10));
        assert_eq!(plain_length(&[0xc7, 0x00, 1, 2, 3, 4]), Some(6));
        assert_eq!(plain_length(&[0x90]), Some(1));
        // Relative to rip: mov eax, [rip + x]; lea rax, [rip + x].
        assert_eq!(plain_length(&[0x8b, 0x05, 1, 2, 3, 4]), None);
        assert_eq!(plain_length(&[0x48, 0x8d, 0x05, 1, 2, 3, 4]), None);
        // Branches, a return, other prefixes, other maps, a cut-off instruction.
        let others: [&[u8]; 6] =
            [&[0xeb, 0x10], &[0x74, 0x10], &[0xe8, 0, 0, 0, 0], &[0xc3], &[0x66, 0x8b, 0x00], &[0x0f, 0x01, 0xfb]];
        for code in others {
            assert_eq!(plain_length(code), None, "{code:x?}");
        }
        assert_eq!(plain_length(&[0x48, 0x8b, 0x44, 0x24]), None);
        assert_eq!(plain_length(&[0xc7, 0x08, 1, 2, 3, 4]), None, "c7 /1 is no mov");
        assert_eq!(plain_length(&[0x48, 0x8d, 0xc0]), None, "lea of a register");
        // The title's places: 3 bytes and the load after them.
        assert_eq!(taken_len(&[0x0f, 0x01, 0xfb, 0x48, 0x8b, 0x17, 0x48, 0x83]), Some(6));
        assert_eq!(taken_len(&[0x0f, 0x01, 0xfa, 0x90, 0x90, 0x48, 0x8b, 0x17]), Some(5));
        assert_eq!(taken_len(&[0x0f, 0x01, 0xfa, 0x0f, 0x01, 0xfb, 0xc3]), None);
        assert_eq!(taken_len(&[0x0f, 0x01, 0xfb, 0xc3]), None);
        assert_eq!(taken_len(&[0x0f, 0x01, 0xfb]), None);
    }

    #[test]
    fn a_branch_into_the_bytes_taken_is_found_wherever_it_is() {
        let base = 0x1000;
        let inside = base + 0x41..base + 0x46;
        let mut code = vec![0xcc; 0x100];
        assert!(!branches_into(&code, base, inside.clone()));
        // jne to 0x43 from 0x20; jmp rel32 to 0x41 from 0x80; je rel32 to 0x45 from 0x90
        code[0x20..0x22].copy_from_slice(&[0x75, 0x21]);
        assert!(branches_into(&code, base, inside.clone()));
        code[0x20..0x22].copy_from_slice(&[0xcc, 0xcc]);
        code[0x80..0x85].copy_from_slice(&[0xe9, 0xbc, 0xff, 0xff, 0xff]);
        assert!(branches_into(&code, base, inside.clone()));
        code[0x80..0x85].fill(0xcc);
        code[0x90..0x96].copy_from_slice(&[0x0f, 0x84, 0xaf, 0xff, 0xff, 0xff]);
        assert!(branches_into(&code, base, inside.clone()));
        // To the first byte (the jump itself) or past the end: nothing inside.
        code[0x90..0x96].copy_from_slice(&[0x0f, 0x84, 0xaa, 0xff, 0xff, 0xff]);
        assert!(!branches_into(&code, base, inside.clone()));
        code[0x90..0x96].copy_from_slice(&[0x0f, 0x84, 0xb0, 0xff, 0xff, 0xff]);
        assert!(!branches_into(&code, base, inside));
    }

    #[test]
    fn a_stub_keeps_the_registers_runs_what_it_took_and_jumps_back() {
        let state = Xsave { mask: 7, size: 832 };
        let (stub, back) = (0x10_0000, 0x12_3456);
        let code = stub_code(stub, 0x1122_3344_5566_7788, state, &[0x48, 0x8b, 0x17], back).unwrap();
        assert_eq!(code[..6], [0x55, 0x48, 0x89, 0xe5, 0x9c, 0xfc]);
        let end = code.len();
        assert_eq!(code[end - 5], 0xe9);
        let rel = i32::from_le_bytes(code[end - 4..].try_into().unwrap());
        assert_eq!((stub + end) as i64 + i64::from(rel), back as i64);
        assert_eq!(code[end - 8..end - 5], [0x48, 0x8b, 0x17]);
        // pops in the order of the pushes, then the flags and rbp.
        let pops = [0x41, 0x5b, 0x41, 0x5a, 0x41, 0x59, 0x41, 0x58, 0x5b, 0x5a, 0x59, 0x58, 0x9d, 0x5d];
        assert_eq!(code[end - 22..end - 8], pops);
        let call = 0x1122_3344_5566_7788u64.to_le_bytes();
        assert!(code.windows(8).any(|w| w == call));
        assert!(code.windows(3).any(|w| w == [0xc5, 0xf8, 0x77]), "vzeroupper with AVX");
        let plain = stub_code(stub, 0, Xsave { mask: 3, size: 576 }, &[0x90, 0x90], back).unwrap();
        assert!(!plain.windows(3).any(|w| w == [0xc5, 0xf8, 0x77]), "no vzeroupper without AVX");
        assert!(stub_code(stub, 0, state, &[], 0x1_0000_0000_0000).is_none(), "out of reach");
        let state = xsave_state().expect("xsave on this PC");
        assert!(state.mask & 3 == 3 && state.size >= 576 && state.size.is_multiple_of(64) && state.size < 3072, "{state:?}");
    }

    /// A page of code with `pieces` (offset, bytes) in it and unwind data over
    /// all of it, as the title's functions have, made executable. Never freed:
    /// places in it are remembered by address ([`replace`]).
    fn page(pieces: &[(usize, &[u8])]) -> (usize, &'static IMAGE_RUNTIME_FUNCTION_ENTRY) {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READ, PAGE_READWRITE, VirtualAlloc, VirtualProtect,
        };
        // SAFETY: memory of our own, written before it is made executable.
        unsafe {
            let page = VirtualAlloc(std::ptr::null(), 4096, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) as usize;
            assert_ne!(page, 0);
            std::ptr::write_bytes(page as *mut u8, 0xcc, 4096);
            for (at, bytes) in pieces {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), (page + at) as *mut u8, bytes.len());
            }
            // Unwind data of a function with no prologue: version 1, nothing else.
            std::ptr::copy_nonoverlapping([1u8, 0, 0, 0].as_ptr(), (page + 0xf00) as *mut u8, 4);
            let table = Box::leak(Box::new(IMAGE_RUNTIME_FUNCTION_ENTRY {
                BeginAddress: 0,
                EndAddress: 0xf00,
                Anonymous: IMAGE_RUNTIME_FUNCTION_ENTRY_0 { UnwindInfoAddress: 0xf00 },
            }));
            assert!(RtlAddFunctionTable(table, 1, page as u64));
            let mut old = 0;
            assert_ne!(VirtualProtect(page as *const _, 4096, PAGE_EXECUTE_READ, &mut old), 0);
            (page, table)
        }
    }

    /// The fault of the place `at`, handled as the vectored handler does with
    /// the switch `waits`: where the thread goes on (at the place, replaced by a
    /// jump, or past it, stepped over).
    fn fault(at: usize, waits: bool) -> usize {
        // SAFETY: an all-zero context is valid; the place is ours, mapped.
        unsafe {
            let mut context: CONTEXT = std::mem::zeroed();
            context.Rip = at as u64;
            assert_eq!(super::super::emulate_with(&mut context, waits), super::super::CONTINUE_EXECUTION);
            context.Rip as usize
        }
    }

    /// The title's loop as a function of (word, value): `monitorx` at 0x117 and
    /// `mwaitx` at 0x129, each with a load of the word after it, until the word
    /// holds the value; it returns how many times it waited.
    const LOOP: [u8; 59] = [
        0x49, 0x89, 0xc8, // mov r8, rcx
        0x49, 0x89, 0xd1, // mov r9, rdx
        0x4d, 0x31, 0xd2, // xor r10, r10
        0x0f, 0x1f, 0x80, 0, 0, 0, 0, // nop to 0x10
        0x4c, 0x89, 0xc0, // 0x10: mov rax, r8
        0x31, 0xc9, // xor ecx, ecx
        0x31, 0xd2, // xor edx, edx
        0x0f, 0x01, 0xfa, // 0x17: monitorx
        0x49, 0x8b, 0x10, // mov rdx, [r8]
        0x4c, 0x39, 0xca, // cmp rdx, r9
        0x74, 0x15, // je 0x37
        0xb9, 0x02, 0x00, 0x00, 0x00, // mov ecx, 2 (with a timer)
        0x31, 0xc0, // xor eax, eax
        0x0f, 0x01, 0xfb, // 0x29: mwaitx
        0x49, 0x8b, 0x10, // mov rdx, [r8]
        0x49, 0xff, 0xc2, // inc r10
        0x4c, 0x39, 0xca, // cmp rdx, r9
        0x75, 0xd9, // jne 0x10
        0x4c, 0x89, 0xd0, // 0x37: mov rax, r10
        0xc3, // ret
    ];

    /// Runs the loop at `f` until another thread writes the word, 30 ms on:
    /// how many times it waited, and the time it took.
    fn run_loop(f: usize) -> (u64, std::time::Duration) {
        let word = Box::leak(Box::new(AtomicU64::new(1)));
        let at = word as *const AtomicU64 as usize;
        // SAFETY: the loop of `LOOP`: a function of two integers, the first the
        // word's address.
        let run: extern "system" fn(usize, u64) -> u64 = unsafe { std::mem::transmute(f) };
        let began = std::time::Instant::now();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(30));
            // SAFETY: the leaked word.
            unsafe { (*(at as *const AtomicU64)).store(7, Relaxed) };
        });
        let waits = run(at, 7);
        let took = began.elapsed();
        writer.join().unwrap();
        (waits, took)
    }

    /// Whether the default replaces wait instructions as it does unless told
    /// otherwise (`VICTORY_PATCH_WAITS` unset): what the tests here expect of it.
    fn default_patches() -> bool {
        super::super::wait_patches() == super::super::WaitPatches::All
    }

    /// Both ways of the switch on the title's loop: by default the places are a
    /// no-op and a `pause` and the loop spins until the word is written; with
    /// `VICTORY_MWAITX_WAIT=1` they are jumps into stubs, and it waits.
    #[test]
    fn the_title_loop_spins_by_default_and_waits_with_the_switch() {
        for waits in [false, true] {
            if !waits && !default_patches() {
                continue;
            }
            let (base, table) = page(&[(0x100, &LOOP)]);
            let places = [base + 0x117, base + 0x129];
            for at in places {
                assert_eq!(fault(at, waits), if waits { at } else { at + 3 }, "waits {waits}");
            }
            // SAFETY: the page's bytes, readable.
            let at = |k: usize| unsafe { std::slice::from_raw_parts(places[k] as *const u8, 6) };
            if waits {
                assert_eq!(at(0)[0], 0xe9);
                assert_eq!((at(1)[0], at(1)[5]), (0xe9, 0xcc), "a jump and the byte left over");
                // The fault of a thread that fetched the old bytes, handled after the
                // jump was stored: it runs the jump.
                assert_eq!(fault(places[1], true), places[1]);
            } else {
                assert_eq!(at(0)[..3], [0x0f, 0x1f, 0x00]);
                assert_eq!(at(1)[..3], [0xf3, 0x90, 0x90]);
            }
            let (passes, took) = run_loop(base + 0x100);
            assert!(took >= std::time::Duration::from_millis(29), "{took:?}");
            if waits {
                assert!(passes < 1000, "waited {passes} times in {took:?}");
            } else {
                assert!(passes > 1000, "spun {passes} times in {took:?}");
            }
            // SAFETY: the table registered by `page`.
            assert!(unsafe { RtlDeleteFunctionTable(table) });
        }
    }

    /// Where the bytes after a wait instruction cannot be taken along, the jump
    /// would cross the end of the aligned 16, a branch of the function leads
    /// inside, or no unwind data covers the place, it is replaced as by default
    /// (and the thread stepped over it).
    #[test]
    fn a_place_that_cannot_take_a_jump_is_replaced_as_by_default() {
        let mwaitx_load = [0x0f, 0x01, 0xfb, 0x48, 0x8b, 0xc0, 0xc3];
        // mwaitx; ret: nothing to take along. mwaitx at 14 of its 16.
        let (one, t1) = page(&[(0x10, &[0x0f, 0x01, 0xfb, 0xc3]), (0x3e, &mwaitx_load)]);
        // A branch of the function to the load after the mwaitx.
        let (two, t2) = page(&[(0x10, &mwaitx_load), (0x40, &[0xeb, 0xd1])]);
        // No unwind data: its table goes before the fault.
        let (three, t3) = page(&[(0x10, &mwaitx_load)]);
        // SAFETY: the tables registered by `page`; the pages' bytes, readable.
        unsafe {
            assert!(RtlDeleteFunctionTable(t3));
            let places = [one + 0x10, one + 0x3e, two + 0x10, three + 0x10];
            for at in places {
                assert_eq!(fault(at, true), at + 3, "{at:#x}: stepped over");
            }
            let at = |place: usize| std::slice::from_raw_parts(place as *const u8, 3);
            for place in places {
                assert_ne!(at(place)[0], 0xe9, "{place:#x}: no jump");
            }
            if default_patches() {
                for place in [one + 0x10, two + 0x10, three + 0x10] {
                    assert_eq!(at(place), [0xf3, 0x90, 0x90], "{place:#x}");
                }
                // Over the end of its 16: one byte of it, as by default.
                let (k, byte) = super::super::wait_byte(ConsoleOnly::Mwaitx).unwrap();
                let mut want = [0x0f, 0x01, 0xfb];
                want[k] = byte;
                assert_eq!(at(one + 0x3e), want);
            }
            assert!(RtlDeleteFunctionTable(t1) && RtlDeleteFunctionTable(t2));
        }
    }

    /// What the register test hands over and gets back besides the registers
    /// it names: rbx and rbp in, rbx and rbp out, the flags out, `mxcsr` in, out
    /// and as it was.
    static SPARE: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];
    /// A line for `monitorx` to arm.
    static LINE: [AtomicU64; 16] = [const { AtomicU64::new(5) }; 16];

    /// Calls `f` with the general registers but `rsp` (`rax`, `rcx`, `rdx`,
    /// `rsi`, `rdi`, `r8` to `r10`, `r12` to `r15`; `r11` holds `f`; `rbx` and
    /// `rbp` from [`SPARE`]), all the flags a program sets, `xmm0` to `xmm15` and
    /// `mxcsr` set; what they are after.
    #[inline(never)]
    fn round_trip(f: usize, gp: [u64; 12], x: [u128; 16]) -> ([u64; 12], usize, [u128; 16]) {
        use std::arch::x86_64::__m128i;
        let [mut rax, mut rcx, mut rdx, mut rsi, mut rdi, mut r8, mut r9, mut r10] = *gp.first_chunk().unwrap();
        let [mut r12, mut r13, mut r14, mut r15] = *gp.last_chunk().unwrap();
        // SAFETY: plain values of the same size.
        let mut v: [__m128i; 16] = unsafe { std::mem::transmute(x) };
        let mut r11 = f;
        // SAFETY: `f` is a function of ours that keeps every register; the block
        // puts back what it changes (rbx, rbp, the direction flag, mxcsr).
        unsafe {
            std::arch::asm!(
                "push rbx",
                "push rbp",
                "stmxcsr dword ptr [rip + {s} + 56]",
                "ldmxcsr dword ptr [rip + {s} + 40]",
                "mov rbx, qword ptr [rip + {s}]",
                "mov rbp, qword ptr [rip + {s} + 8]",
                // CF, PF, AF, ZF, SF, DF and OF
                "push 0xcd5",
                "popfq",
                "call r11",
                "pushfq",
                "pop qword ptr [rip + {s} + 32]",
                "cld",
                "mov qword ptr [rip + {s} + 16], rbx",
                "mov qword ptr [rip + {s} + 24], rbp",
                "stmxcsr dword ptr [rip + {s} + 48]",
                "ldmxcsr dword ptr [rip + {s} + 56]",
                "pop rbp",
                "pop rbx",
                s = sym SPARE,
                inout("rax") rax, inout("rcx") rcx, inout("rdx") rdx, inout("rsi") rsi, inout("rdi") rdi,
                inout("r8") r8, inout("r9") r9, inout("r10") r10, inout("r11") r11, inout("r12") r12,
                inout("r13") r13, inout("r14") r14, inout("r15") r15,
                inout("xmm0") v[0], inout("xmm1") v[1], inout("xmm2") v[2], inout("xmm3") v[3],
                inout("xmm4") v[4], inout("xmm5") v[5], inout("xmm6") v[6], inout("xmm7") v[7],
                inout("xmm8") v[8], inout("xmm9") v[9], inout("xmm10") v[10], inout("xmm11") v[11],
                inout("xmm12") v[12], inout("xmm13") v[13], inout("xmm14") v[14], inout("xmm15") v[15],
            );
        }
        // SAFETY: as above.
        let x = unsafe { std::mem::transmute::<[__m128i; 16], [u128; 16]>(v) };
        ([rax, rcx, rdx, rsi, rdi, r8, r9, r10, r12, r13, r14, r15], r11, x)
    }

    /// The upper halves of `ymm0` to `ymm15` across a call of `f`.
    #[target_feature(enable = "avx")]
    fn upper_round_trip(f: usize, y: [[u128; 2]; 16]) -> [[u128; 2]; 16] {
        use std::arch::x86_64::__m256i;
        // SAFETY: plain values of the same size.
        let mut v: [__m256i; 16] = unsafe { std::mem::transmute(y) };
        // SAFETY: as in `round_trip`; nothing else changes.
        unsafe {
            std::arch::asm!(
                "call r11",
                inout("r11") f => _,
                inout("ymm0") v[0], inout("ymm1") v[1], inout("ymm2") v[2], inout("ymm3") v[3],
                inout("ymm4") v[4], inout("ymm5") v[5], inout("ymm6") v[6], inout("ymm7") v[7],
                inout("ymm8") v[8], inout("ymm9") v[9], inout("ymm10") v[10], inout("ymm11") v[11],
                inout("ymm12") v[12], inout("ymm13") v[13], inout("ymm14") v[14], inout("ymm15") v[15],
            );
        }
        // SAFETY: as above.
        unsafe { std::mem::transmute(v) }
    }

    /// Every general register but `rsp`, the flags, `xmm0` to `xmm15` (whole
    /// `ymm` with AVX) and `mxcsr` come out of a replaced `monitorx` and a
    /// replaced `mwaitx` as they went in, and the thread's last error stays.
    #[test]
    fn a_replaced_place_keeps_every_register() {
        use windows_sys::Win32::Foundation::{GetLastError, SetLastError};
        // monitorx; mov rax, rax; ret and mwaitx; mov rax, rax; ret
        let monitorx = [0x0f, 0x01, 0xfa, 0x48, 0x8b, 0xc0, 0xc3];
        let mwaitx = [0x0f, 0x01, 0xfb, 0x48, 0x8b, 0xc0, 0xc3];
        let (base, table) = page(&[(0x10, &monitorx), (0x30, &mwaitx)]);
        for at in [base + 0x10, base + 0x30] {
            assert_eq!(fault(at, true), at, "not stepped over: the thread runs the jump");
        }
        let line = LINE.as_ptr() as u64;
        let gp: [u64; 12] = std::array::from_fn(|k| 0x0101_0101_0101_0101 * (k as u64 + 1));
        let x: [u128; 16] = std::array::from_fn(|k| 0x0123_4567_89ab_cdef_fedc_ba98_7654_3210 ^ (k as u128 * 0x1111));
        SPARE[0].store(0xb0b0_b0b0_b0b0_b0b0, Relaxed);
        SPARE[1].store(0xbebe_bebe_bebe_bebe, Relaxed);
        // Round toward zero, every exception masked, denormals as zero.
        SPARE[5].store(0x7fc0, Relaxed);
        for (f, rax) in [(base + 0x10, line), (base + 0x30, gp[0])] {
            let mut gp = gp;
            gp[0] = rax;
            // SAFETY: plain calls about this thread.
            unsafe { SetLastError(1234) };
            let (out, r11, xo) = round_trip(f, gp, x);
            // SAFETY: as above.
            assert_eq!(unsafe { GetLastError() }, 1234);
            assert_eq!(out, gp, "{f:#x}");
            assert_eq!(r11, f);
            assert_eq!(xo, x, "{f:#x}");
            assert_eq!(SPARE[2].load(Relaxed), 0xb0b0_b0b0_b0b0_b0b0);
            assert_eq!(SPARE[3].load(Relaxed), 0xbebe_bebe_bebe_bebe);
            assert_eq!(SPARE[4].load(Relaxed) & 0xcd5, 0xcd5, "the flags");
            assert_eq!(SPARE[6].load(Relaxed) as u32 & 0xffff, 0x7fc0, "mxcsr");
        }
        if std::is_x86_feature_detected!("avx") {
            let y: [[u128; 2]; 16] = std::array::from_fn(|k| [k as u128 * 3 + 1, u128::MAX - k as u128]);
            // SAFETY: the processor has AVX (asked above).
            assert_eq!(unsafe { upper_round_trip(base + 0x30, y) }, y);
        }
        // SAFETY: the table registered above.
        assert!(unsafe { RtlDeleteFunctionTable(table) });
    }

    /// In a child process of the next test: the title's loop run with its
    /// faults handled by the vectored handler, as in a run, with the switch as
    /// the parent set it. Says how many times it waited and the first byte of
    /// each place after.
    #[test]
    fn loop_child() {
        if std::env::var_os("VICTORY_WAIT_CHILD").is_none() {
            return;
        }
        crate::report_crashes();
        let (base, _) = page(&[(0x100, &LOOP)]);
        let (passes, took) = run_loop(base + 0x100);
        // SAFETY: the page's bytes, readable.
        let first = |at: usize| unsafe { *((base + at) as *const u8) };
        println!("passes={passes} ms={} monitorx={:02x} mwaitx={:02x}", took.as_millis(), first(0x117), first(0x129));
    }

    /// The loop of [`loop_child`] in a process with `VICTORY_MWAITX_WAIT=1` and
    /// in one without: the first waits on jumps of ours, the second spins on a
    /// no-op and a `pause`.
    #[test]
    fn real_faults_wait_with_the_switch_and_spin_without_it() {
        let exe = std::env::current_exe().unwrap();
        for switch in [Some("1"), None] {
            let mut child = std::process::Command::new(&exe);
            child.args(["--exact", "console_insn::wait::tests::loop_child", "--nocapture"]);
            child.env("VICTORY_WAIT_CHILD", "1");
            // The defaults of the other switches, whatever this process was given.
            child.env_remove("VICTORY_PATCH_WAITS").env_remove("VICTORY_MWAITX_WAIT_US");
            match switch {
                Some(on) => child.env("VICTORY_MWAITX_WAIT", on),
                None => child.env_remove("VICTORY_MWAITX_WAIT"),
            };
            let out = child.output().unwrap();
            let text = String::from_utf8_lossy(&out.stdout);
            assert!(out.status.success(), "{switch:?}: {text}");
            let said = text.lines().find(|l| l.starts_with("passes=")).unwrap_or_else(|| panic!("{switch:?}: {text}"));
            let field = |name: &str| said.split(' ').find_map(|w| w.strip_prefix(name)).unwrap();
            let passes: u64 = field("passes=").parse().unwrap();
            if switch.is_some() {
                assert!(passes < 1000, "{said}");
                assert_eq!((field("monitorx="), field("mwaitx=")), ("e9", "e9"), "{said}");
            } else {
                assert!(passes > 1000, "{said}");
                assert_eq!((field("monitorx="), field("mwaitx=")), ("0f", "f3"), "{said}");
            }
        }
    }
}
