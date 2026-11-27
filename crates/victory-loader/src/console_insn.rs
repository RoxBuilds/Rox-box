//! Instructions the console's CPU (Zen 2) has and some host CPUs lack, emulated: guest
//! code raises STATUS_ILLEGAL_INSTRUCTION on them (e.g., cache-line zeroing loops with `clzero`),
//! and the vectored handler steps over them here.

#[path = "console_insn_decode.rs"]
mod decode;
#[path = "console_wait.rs"]
mod wait;

use decode::ConsoleOnly;
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use windows_sys::Win32::System::Diagnostics::Debug::CONTEXT;

const CONTINUE_EXECUTION: i32 = -1;

/// Whether what Windows said about memory is remembered per thread (the regions it found
/// writable, the code bytes it found readable) and not asked again at every emulated
/// instruction. VirtualQuery walks page tables for each region and becomes expensive in tight
/// `clzero` zeroing loops across large heaps. On by default; `VICTORY_CLZERO_ASK=1` asks every
/// time, as before.
fn remembers() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_CLZERO_ASK").is_none_or(|v| v != "1"))
}

/// Answers a remembered region gives before Windows is asked about it again: one freed or
/// protected meanwhile is forgotten then.
const ANSWERS: u32 = 4096;

thread_local! {
    /// The regions Windows last said are committed and writable (start, end), newest first,
    /// and the answers they gave since they were emptied.
    static WRITABLE: Cell<([(usize, usize); 4], u32)> = const { Cell::new(([(0, 0); 4], 0)) };
    /// Faulting addresses and the code bytes found readable there, newest first.
    static CODE: Cell<[(usize, usize); 8]> = const { Cell::new([(0, 0); 8]) };
}

/// Instructions emulated, the times Windows was asked about memory for them, and the bytes
/// `clzero` loops zeroed, since the start.
static EMULATED: AtomicU64 = AtomicU64::new(0);
static ASKED: AtomicU64 = AtomicU64::new(0);
static ZEROED: AtomicU64 = AtomicU64::new(0);

/// Whether `monitorx` and `mwaitx` are replaced in the title's code the first time they fault
/// (`monitorx` by a 3-byte no-op, `mwaitx` by `pause` and a no-op), so that they never fault
/// again. Emulated, each pass of such a wait loop is two trips into Windows and back, through
/// every vectored handler, and no lighter a wait for it: the emulation does no more than
/// `pause` either, and the loop looks at its value again after it. On by default;
/// `VICTORY_PATCH_WAITS=0` emulates each time, as before.
///
/// One that lies over the end of its aligned 16 bytes, which no one store of [`patch`] covers,
/// is replaced by one byte of it ([`wait_byte`]). Left as it was, such a place raised its
/// exception at every pass of its loop, and one place in eight lies so (a 3-byte instruction at
/// 14 or 15 of its 16). `VICTORY_PATCH_WAITS=aligned` leaves those emulated, as before.
#[derive(Clone, Copy, PartialEq, Eq)]
enum WaitPatches {
    Off,
    Aligned,
    All,
}

fn wait_patches() -> WaitPatches {
    static HOW: std::sync::OnceLock<WaitPatches> = std::sync::OnceLock::new();
    *HOW.get_or_init(|| match std::env::var("VICTORY_PATCH_WAITS").as_deref().map(str::trim) {
        Ok("0") => WaitPatches::Off,
        Ok("aligned") => WaitPatches::Aligned,
        _ => WaitPatches::All,
    })
}

fn patches_waits() -> bool {
    wait_patches() != WaitPatches::Off
}

/// What replaces the 3 bytes of a wait instruction.
fn wait_patch(what: ConsoleOnly) -> Option<[u8; 3]> {
    match what {
        ConsoleOnly::Monitorx => Some([0x0f, 0x1f, 0x00]),
        ConsoleOnly::Mwaitx => Some([0xf3, 0x90, 0x90]),
        _ => None,
    }
}

/// The byte that makes a wait instruction over the end of its aligned 16 bytes wait for
/// nothing, and where in it: one byte is seen whole by every thread, so none can run a mixture
/// of the old bytes and the new, wherever the instruction lies. `monitorx` becomes `0f 1e fa`
/// by its second (`endbr64` without its prefix: a hint no-op on every x86-64 processor, as
/// `endbr64` itself is on those without CET); `mwaitx` becomes `0f 01 e8` by its third
/// (`serialize`, a short stall as `pause` is) where the processor has that, else `0f 1e fb`,
/// a no-op as well.
fn wait_byte(what: ConsoleOnly) -> Option<(usize, u8)> {
    match what {
        ConsoleOnly::Monitorx => Some((1, 0x1e)),
        ConsoleOnly::Mwaitx if serialize() => Some((2, 0xe8)),
        ConsoleOnly::Mwaitx => Some((1, 0x1e)),
        _ => None,
    }
}

/// Whether the processor has `serialize` (CPUID leaf 7, EDX bit 14: Intel since Alder Lake).
fn serialize() -> bool {
    static HAS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *HAS.get_or_init(cpu_has_serialize)
}

#[allow(unused_unsafe)]
fn cpu_has_serialize() -> bool {
    use std::arch::x86_64::{__cpuid_count, __get_cpuid_max};
    // SAFETY: `cpuid` is there on every x86-64 processor; leaf 7 is asked only where it is.
    unsafe { __get_cpuid_max(0).0 >= 7 && (__cpuid_count(7, 0).edx & (1 << 14)) != 0 }
}

/// Whether a `clzero` loop ([`ConsoleOnly::ClzeroLoop`]) is replaced the first time it faults
/// by a jump to the same loop with plain stores ([`clzero_stub`]). On by default;
/// `VICTORY_PATCH_CLZERO=0` emulates each time, as before.
fn patches_clzero() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PATCH_CLZERO").is_none_or(|v| v != "0"))
}

/// `jmp rel32` at `at` to `to`, if it reaches.
pub(crate) fn jump(at: usize, to: usize) -> Option<[u8; 5]> {
    let rel = i32::try_from(to as i64 - (at as i64 + 5)).ok()?;
    let [a, b, c, d] = rel.to_le_bytes();
    Some([0xe9, a, b, c, d])
}

/// The `clzero` loop of 12 bytes at `at` with plain stores, as code at `stub`: every register
/// and the flags come out as the loop leaves them. `cmp` is its `cmp rax, reg` (3 bytes, the
/// register not `rax`), `None` for another form. (The scratch register is saved on the stack
/// and back before the compare, whose register it may be.)
fn clzero_stub(at: usize, stub: usize, cmp: [u8; 3]) -> Option<Vec<u8>> {
    if !matches!(cmp[0], 0x48 | 0x4c) || cmp[1] != 0x39 || cmp[2] & 0xc7 != 0xc0 || (cmp[0] == 0x48 && cmp[2] == 0xc0) {
        return None;
    }
    let mut code = vec![
        0x51, // push rcx
        0x48, 0x89, 0xc1, // mov rcx, rax
        0x48, 0x83, 0xe1, 0xc0, // and rcx, -64
    ];
    for k in 0..8 {
        code.extend([0x48, 0xc7, 0x41, k * 8, 0, 0, 0, 0]); // mov qword [rcx + 8k], 0
    }
    code.push(0x59); // pop rcx
    code.extend([0x48, 0x83, 0xc0, 0x40]); // add rax, 0x40
    code.extend(cmp);
    let back = -(code.len() as i64 + 2);
    code.extend([0x72, i8::try_from(back).ok()? as u8]); // jb the start
    let end = stub + code.len();
    code.extend(jump(end, at + 12)?);
    Some(code)
}

/// Room for [`clzero_stub`]s near the title's code: where it is and the bytes used.
static STUBS: std::sync::Mutex<Option<(usize, usize)>> = std::sync::Mutex::new(None);
pub(crate) const STUBS_LEN: usize = 64 << 10;

/// Memory for code within reach of a `jmp rel32` at `at`.
pub(crate) fn near(at: usize) -> Option<usize> {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READWRITE, VirtualAlloc};
    let at = at & !0xffff;
    (0x10_0000..0x7000_0000).step_by(0x10_0000).flat_map(|d| [at.checked_sub(d), at.checked_add(d)]).flatten().find_map(|place| {
        // SAFETY: reserves free memory at that address or fails.
        let got = unsafe { VirtualAlloc(place as *const _, STUBS_LEN, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE) } as usize;
        (got != 0).then_some(got)
    })
}

/// Replaces the `clzero` loop at `at`, whose 12 bytes are `code`: whether it did.
///
/// No thread is ever inside the loop's other instructions (the emulation does the whole loop
/// in one), so the five bytes of the jump may take the place of two of them.
///
/// # Safety
/// `at` is in the title's code, which stays mapped.
unsafe fn patch_clzero(at: usize, code: &[u8]) -> bool {
    let &[a, b, c, d, e, _, _, x, y, z, 0x72, 0xf4] = code else {
        return false;
    };
    let mut stubs = STUBS.lock().unwrap_or_else(|e| e.into_inner());
    if stubs.is_none() {
        *stubs = near(at).map(|place| (place, 0));
    }
    let Some((place, used)) = stubs.as_mut() else {
        return false;
    };
    let stub = *place + *used;
    let (Some(body), Some(jump)) = (clzero_stub(at, stub, [x, y, z]), jump(at, stub)) else {
        return false;
    };
    if *used + body.len() > STUBS_LEN || (at & 15) + 5 > 16 {
        return false;
    }
    // SAFETY: inside the memory made for it, which nothing runs yet; then the title's code.
    unsafe {
        std::ptr::copy_nonoverlapping(body.as_ptr(), stub as *mut u8, body.len());
        *used += body.len().next_multiple_of(16);
        patch(at, [a, b, c, d, e], jump)
    }
}

/// The aligned 16 bytes `word` around address `at` with the bytes at `at` replaced; `None`
/// where they do not all lie in those 16 (allowing instructions that straddle 8-byte boundaries
/// to be patched atomically).
fn with_patch(word: u128, at: usize, patch: &[u8]) -> Option<u128> {
    let offset = at & 15;
    if offset + patch.len() > 16 {
        return None;
    }
    let mut bytes = word.to_le_bytes();
    bytes[offset..offset + patch.len()].copy_from_slice(patch);
    Some(u128::from_le_bytes(bytes))
}

/// One thread at a time changes code: a second one would take the first one's loosened
/// protection for the page's own.
static PATCHING: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Replaces the bytes at `at`, which are `was`, by `patch`, in one store that other threads
/// see whole: whether they are replaced now (not where they cross 16 aligned bytes, or the page
/// cannot be written).
///
/// # Safety
/// `at` is in code that stays mapped, and `patch` does for it what `was` does.
unsafe fn patch<const N: usize>(at: usize, was: [u8; N], patch: [u8; N]) -> bool {
    // SAFETY: the caller's.
    unsafe { patch_bytes(at, &was, &patch) }
}

/// [`patch`] of up to 16 bytes, `was` and `patch` alike long.
///
/// # Safety
/// As [`patch`].
unsafe fn patch_bytes(at: usize, was: &[u8], patch: &[u8]) -> bool {
    use std::sync::atomic::Ordering::SeqCst;
    use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
    use windows_sys::Win32::System::Memory::{PAGE_EXECUTE_READWRITE, VirtualProtect};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let n = was.len();
    if patch.len() != n || (at & 15) + n > 16 || !std::is_x86_feature_detected!("cmpxchg16b") {
        return false;
    }
    let _one = PATCHING.lock().unwrap_or_else(|e| e.into_inner());
    let word = (at & !15) as *mut u128;
    let here = (at & 15)..(at & 15) + n;
    let mut old = 0u32;
    // SAFETY: the aligned 16 bytes around the instruction lie in one page of mapped code, made
    // writable for the store and put back; the processor has the instruction (asked above).
    unsafe {
        if VirtualProtect(word.cast(), 16, PAGE_EXECUTE_READWRITE, &mut old) == 0 {
            return false;
        }
        // What is there (zeros are replaced by zeros).
        let mut now = std::arch::x86_64::cmpxchg16b(word, 0, 0, SeqCst, SeqCst);
        let done = loop {
            let bytes = &now.to_le_bytes()[here.clone()];
            if bytes == patch {
                // (Another thread's fault at the same place: done already.)
                break true;
            }
            let Some(new) = (bytes == was).then(|| with_patch(now, at, patch)).flatten() else {
                break false;
            };
            let there = std::arch::x86_64::cmpxchg16b(word, now, new, SeqCst, SeqCst);
            if there == now {
                break true;
            }
            now = there;
        };
        VirtualProtect(word.cast(), 16, old, &mut old);
        FlushInstructionCache(GetCurrentProcess(), word.cast(), 16);
        done
    }
}

/// Stores `new` over the byte at `at`, which is `was`: whether it is `new` now (not when the
/// page cannot be written, or the byte is another). One byte is seen whole by every thread,
/// wherever it lies, so this needs no aligned 16 bytes around it.
///
/// # Safety
/// `at` is in code that stays mapped, and the instruction it is a byte of does with `new` what
/// it does with `was`.
unsafe fn patch_byte(at: usize, was: u8, new: u8) -> bool {
    use std::sync::atomic::{AtomicU8, Ordering::SeqCst};
    use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
    use windows_sys::Win32::System::Memory::{PAGE_EXECUTE_READWRITE, VirtualProtect};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let _one = PATCHING.lock().unwrap_or_else(|e| e.into_inner());
    let mut old = 0u32;
    // SAFETY: one byte of mapped code, made writable for the store and put back.
    unsafe {
        if VirtualProtect(at as *const _, 1, PAGE_EXECUTE_READWRITE, &mut old) == 0 {
            return false;
        }
        let byte = &*(at as *const AtomicU8);
        let done = match byte.compare_exchange(was, new, SeqCst, SeqCst) {
            Ok(_) => true,
            // (Another thread's fault at the same place: done already.)
            Err(now) => now == new,
        };
        VirtualProtect(at as *const _, 1, old, &mut old);
        FlushInstructionCache(GetCurrentProcess(), at as *const _, 1);
        done
    }
}

/// Diagnostics: how often each place in the title's code raises one of these exceptions, the
/// busiest logged every five seconds.
#[cfg(feature = "diagnostics")]
mod sites {
    use super::{AtomicU64, Relaxed};
    use std::sync::atomic::AtomicUsize;

    /// Address, what is there (an index into [`NAMES`]), exceptions since the last line.
    static SITES: [(AtomicUsize, AtomicUsize, AtomicU64); 512] =
        [const { (AtomicUsize::new(0), AtomicUsize::new(0), AtomicU64::new(0)) }; 512];
    const NAMES: [&str; 8] = ["other", "clwb", "monitorx", "mwaitx", "clzero", "clzero loop", "rdpru", "extrq/insertq"];
    static LAST: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
    static PATCHED: AtomicU64 = AtomicU64::new(0);
    /// Of those, wait instructions by one byte ([`super::wait_byte`]); the times one could not
    /// be replaced; faults run again on a replacement another thread stored ([`super::rerun`]).
    static ONE_BYTE: AtomicU64 = AtomicU64::new(0);
    static REFUSED: AtomicU64 = AtomicU64::new(0);
    static RERUN: AtomicU64 = AtomicU64::new(0);

    pub(super) fn patched() {
        PATCHED.fetch_add(1, Relaxed);
    }

    /// A wait instruction replaced (`done`) or not, by one byte of it or all three.
    pub(super) fn replaced(done: bool, one_byte: bool) {
        if !done {
            REFUSED.fetch_add(1, Relaxed);
            return;
        }
        patched();
        if one_byte {
            ONE_BYTE.fetch_add(1, Relaxed);
        }
    }

    pub(super) fn rerun() {
        RERUN.fetch_add(1, Relaxed);
    }

    pub(super) fn count(at: usize, what: super::ConsoleOnly) {
        use super::ConsoleOnly as C;
        let kind = match what {
            C::Clwb => 1,
            C::Monitorx => 2,
            C::Mwaitx => 3,
            C::Clzero => 4,
            C::ClzeroLoop { .. } => 5,
            C::Rdpru => 6,
            C::Extrq { .. } | C::Insertq { .. } => 7,
            _ => 0,
        };
        let mut i = (at ^ at >> 9) % SITES.len();
        for _ in 0..8 {
            let s = &SITES[i];
            let there = s.0.compare_exchange(0, at, Relaxed, Relaxed).unwrap_or_else(|there| there);
            if there == 0 || there == at {
                s.1.store(kind, Relaxed);
                s.2.fetch_add(1, Relaxed);
                break;
            }
            i = (i + 1) % SITES.len();
        }
        // Now and then: is a line due? Never waited for.
        if let Ok(mut last) = LAST.try_lock() {
            let since = *last.get_or_insert_with(std::time::Instant::now);
            if since.elapsed().as_secs() >= 5 {
                let s = since.elapsed().as_secs_f64();
                *last = Some(std::time::Instant::now());
                drop(last);
                line(s);
            }
        }
    }

    fn line(seconds: f64) {
        let mut all: Vec<(u64, usize, usize)> =
            SITES.iter().map(|s| (s.2.swap(0, Relaxed), s.0.load(Relaxed), s.1.load(Relaxed))).filter(|s| s.0 > 0).collect();
        all.sort_unstable_by_key(|s| std::cmp::Reverse(s.0));
        let mut kinds = [0u64; NAMES.len()];
        for s in &all {
            kinds[s.2] += s.0;
        }
        let kinds: Vec<String> = NAMES
            .iter()
            .zip(kinds)
            .filter(|(_, n)| *n > 0)
            .map(|(name, n)| format!("{name} {:.0}", n as f64 / seconds))
            .collect();
        let top: Vec<String> =
            all.iter().take(8).map(|s| format!("{:#x} {} {:.0}", s.1, NAMES[s.2], s.0 as f64 / seconds)).collect();
        tracing::info!(
            "console CPU instructions that fault, a second: {}; places replaced so far: {} ({} waits by one byte), refused {}, faults run again on another thread's replacement {}; the busiest places: {}",
            kinds.join(", "),
            PATCHED.load(Relaxed),
            ONE_BYTE.load(Relaxed),
            REFUSED.load(Relaxed),
            RERUN.load(Relaxed),
            top.join(", ")
        );
    }
}

fn readable(va: usize, bytes: usize) -> bool {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_NOACCESS, VirtualQuery};
    // SAFETY: plain data filled in by the query.
    let mut m: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let found = unsafe { VirtualQuery(va as *const _, &mut m, size_of_val(&m)) } != 0;
    ASKED.fetch_add(1, Relaxed);
    found && m.State == MEM_COMMIT && m.Protect & (PAGE_NOACCESS | PAGE_GUARD) == 0 && m.BaseAddress as usize + m.RegionSize >= va + bytes
}

fn writable(va: usize, bytes: usize) -> bool {
    writable_to(va, va + bytes) == Some(va + bytes)
}

/// Where the committed, writable memory that `va` lies in ends, at most `end`; None if `va`
/// is not in such memory. A region found is remembered ([`remembers`]).
/// shortcut: a remembered region that was freed or protected since is written all the same
/// for up to [`ANSWERS`] answers: the write then faults inside the handler, which the guard
/// on view memory opens as it does any write, and which is a crash otherwise (as the
/// unchecked single-line emulation behind this one already is).
fn writable_to(va: usize, end: usize) -> Option<usize> {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READWRITE, PAGE_READWRITE, VirtualQuery};
    let remember = remembers();
    if remember {
        let (regions, answers) = WRITABLE.with(Cell::get);
        if let Some(&(_, to)) = regions.iter().find(|&&(from, to)| from <= va && va < to) {
            let next = if answers >= ANSWERS { ([(0, 0); 4], 0) } else { (regions, answers + 1) };
            WRITABLE.with(|w| w.set(next));
            return Some(to.min(end));
        }
    }
    // SAFETY: plain data filled in by the query.
    let mut m: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let found = unsafe { VirtualQuery(va as *const _, &mut m, size_of_val(&m)) } != 0;
    ASKED.fetch_add(1, Relaxed);
    if !found || m.State != MEM_COMMIT || m.Protect & (PAGE_READWRITE | PAGE_EXECUTE_READWRITE) == 0 {
        return None;
    }
    let (from, to) = (m.BaseAddress as usize, m.BaseAddress as usize + m.RegionSize);
    if remember {
        WRITABLE.with(|w| {
            let (mut regions, answers) = w.get();
            regions.rotate_right(1);
            regions[0] = (from, to);
            w.set((regions, answers));
        });
    }
    Some(to.min(end))
}

/// Whether the bytes at `rip` (the first `n` of them readable) are one of the replacements made
/// here, none of which faults: a wait's ([`wait_patch`], [`wait_byte`]) or the jump of a `clzero`
/// loop into its stub ([`patch_clzero`]). Read afresh: another thread may be storing them.
fn replacement(rip: usize, n: usize) -> bool {
    let mut b = [0u8; 5];
    for (i, byte) in b.iter_mut().enumerate().take(n) {
        // SAFETY: `n` bytes at `rip` are readable.
        *byte = unsafe { std::ptr::read_volatile((rip + i) as *const u8) };
    }
    let three = [b[0], b[1], b[2]];
    if n >= 3 && (matches!(three, [0x0f, 0x1f, 0x00] | [0xf3, 0x90, 0x90] | [0x0f, 0x1e, 0xfa | 0xfb]) || three == [0x0f, 0x01, 0xe8] && serialize()) {
        return true;
    }
    if n >= 5 && b[0] == 0xe9 {
        let to = (rip as i64 + 5 + i64::from(i32::from_le_bytes([b[1], b[2], b[3], b[4]]))) as usize;
        let stubs = STUBS.lock().unwrap_or_else(|e| e.into_inner());
        return stubs.is_some_and(|(place, used)| (place..place + used).contains(&to));
    }
    false
}

thread_local! {
    /// The place this thread last ran again ([`rerun`]), and how many times in a row.
    static RERUNS: Cell<(usize, u32)> = const { Cell::new((0, 0)) };
}

/// The fault at `rip` was raised by bytes that decode as nothing console-only now. Where they are
/// a replacement of ours, another thread stored it after this one fetched the old bytes, and
/// this thread runs it (`CONTINUE_EXECUTION`, `rip` as it is). That was a crash before: two
/// threads in the same wait loop at its first pass, the second one's handler reading the first
/// one's new bytes. Looked at once more after a moment, as a 16-byte store may land between the
/// reads of one look; and no more than a few times in a row at one place, which faulting on and
/// on is something else, for the crash report.
fn rerun(rip: usize, n: usize) -> i32 {
    let mut ours = replacement(rip, n);
    if !ours {
        for _ in 0..64 {
            std::hint::spin_loop();
        }
        ours = replacement(rip, n);
    }
    if !ours {
        return 0;
    }
    let times = RERUNS.with(|r| {
        let (at, times) = r.get();
        let times = if at == rip { times + 1 } else { 1 };
        r.set((rip, times));
        times
    });
    #[cfg(feature = "diagnostics")]
    sites::rerun();
    if times > 4 { 0 } else { CONTINUE_EXECUTION }
}

/// An illegal instruction at `ctx.Rip`: a console-only one is emulated and stepped over
/// (`CONTINUE_EXECUTION`); anything else returns 0 (the crash report and the title's handlers
/// see it).
///
/// # Safety
/// `ctx` is the faulting thread's context, from the exception being dispatched.
pub(crate) unsafe fn emulate(ctx: &mut CONTEXT) -> i32 {
    // SAFETY: the caller's.
    unsafe { emulate_with(ctx, wait::on()) }
}

/// [`emulate`], `monitorx` and `mwaitx` places made to call a wait of ours
/// ([`wait::replace`], `VICTORY_MWAITX_WAIT=1`) where `waits`.
///
/// # Safety
/// As [`emulate`].
unsafe fn emulate_with(ctx: &mut CONTEXT, waits: bool) -> i32 {
    let rip = ctx.Rip as usize;
    // The same few instructions fault again and again: their bytes are asked about once.
    let known = if remembers() { CODE.with(Cell::get) } else { [(0, 0); 8] };
    let n = match known.iter().find(|&&(at, n)| at == rip && n > 0) {
        Some(&(_, n)) => n,
        None => {
            if !readable(rip, 1) {
                return 0;
            }
            let n = (1..=16).rev().find(|&n| readable(rip, n)).unwrap_or(0);
            if remembers() {
                let mut known = known;
                known.rotate_right(1);
                known[0] = (rip, n);
                CODE.with(|c| c.set(known));
            }
            n
        }
    };
    // SAFETY: `n` bytes at `rip` are readable.
    let code = unsafe { std::slice::from_raw_parts(rip as *const u8, n) };
    let Some((what, len)) = decode::decode(code) else { return rerun(rip, n) };
    static SEEN: std::sync::Mutex<Vec<std::mem::Discriminant<ConsoleOnly>>> = std::sync::Mutex::new(Vec::new());
    if let Ok(mut seen) = SEEN.lock()
        && !seen.contains(&std::mem::discriminant(&what))
    {
        seen.push(std::mem::discriminant(&what));
        if what != ConsoleOnly::Ud2 {
            tracing::warn!(instruction = ?what, at = format_args!("{rip:#x}"), "a console CPU instruction this CPU lacks: emulated (logged once)");
        }
    }
    #[cfg(feature = "diagnostics")]
    if what != ConsoleOnly::Ud2 {
        sites::count(rip, what);
    }
    match what {
        ConsoleOnly::Ud2 => return 0,
        ConsoleOnly::Clwb => {
            // nothing to write back on the PC: patched to NOPs, no exception next time
            use windows_sys::Win32::System::Memory::{PAGE_EXECUTE_READWRITE, VirtualProtect};
            let mut old = 0u32;
            let _one = PATCHING.lock().unwrap_or_else(|e| e.into_inner());
            // SAFETY: the instruction's own bytes, made writable for the patch and restored.
            unsafe {
                if VirtualProtect(rip as *const _, len, PAGE_EXECUTE_READWRITE, &mut old) != 0 {
                    std::slice::from_raw_parts_mut(rip as *mut u8, len).fill(0x90);
                    VirtualProtect(rip as *const _, len, old, &mut old);
                }
            }
        }
        ConsoleOnly::Monitorx | ConsoleOnly::Mwaitx => {
            // With the switch, a jump to a wait of ours, which this thread runs now, not
            // stepped over. SAFETY: the title's code, where the instruction just faulted;
            // with the switch every fault at a place comes here first, so no thread was
            // stepped past one before it is replaced or refused.
            if waits && len == 3 && unsafe { wait::replace(rip, code, what) } {
                #[cfg(feature = "diagnostics")]
                sites::patched();
                return CONTINUE_EXECUTION;
            }
            // Without a prefix, so 3 bytes: replaced, the next pass does not fault. Inside
            // their aligned 16 bytes all three in one store; over their end by one byte.
            if len == 3 && patches_waits() {
                let was = [code[0], code[1], code[2]];
                let inside = (rip & 15) + 3 <= 16;
                // SAFETY: the title's code, where the instruction just faulted; every
                // replacement is a wait that waits for nothing, as the emulation is.
                let done = unsafe {
                    match (inside, wait_patch(what), wait_byte(what)) {
                        (true, Some(new), _) => patch(rip, was, new),
                        (false, _, Some((k, new))) if wait_patches() == WaitPatches::All => patch_byte(rip + k, was[k], new),
                        _ => false,
                    }
                };
                #[cfg(feature = "diagnostics")]
                sites::replaced(done, !inside);
                let _ = done;
            }
            std::hint::spin_loop();
        }
        ConsoleOnly::Clzero => {
            // with an address-size prefix (0x67) the line is at EAX, zero-extended
            let a32 = code[..len - 3].contains(&0x67);
            let line = (if a32 { ctx.Rax & 0xFFFF_FFFF } else { ctx.Rax } as usize) & !63;
            if !writable(line, 64) {
                return 0; // the real instruction faults there too
            }
            // SAFETY: the 64-byte line is committed and writable.
            unsafe { std::ptr::write_bytes(line as *mut u8, 0, 64) };
            // A loop's first instruction, read as one alone because the jump of ours that took
            // its place was being stored (part of the old bytes and part of the new): stepping
            // 3 bytes would land inside the jump, so the jump runs instead, and the loop from
            // this line on (zeroed once more: `rax` is as it was).
            if replacement(rip, n) {
                return CONTINUE_EXECUTION;
            }
        }
        ConsoleOnly::ClzeroLoop { end_reg } => {
            // the whole `clzero; add rax,40h; cmp rax,end; jb` loop at once
            // (and not again: the next time it is plain stores)
            if len == 12
                && patches_clzero()
                // SAFETY: the title's code, where the loop's first instruction just faulted.
                && unsafe { patch_clzero(rip, &code[..12]) }
            {
                #[cfg(feature = "diagnostics")]
                sites::patched();
            }
            let regs: [u64; 16] = [
                ctx.Rax, ctx.Rcx, ctx.Rdx, ctx.Rbx, ctx.Rsp, ctx.Rbp, ctx.Rsi, ctx.Rdi, ctx.R8, ctx.R9, ctx.R10, ctx.R11, ctx.R12, ctx.R13, ctx.R14, ctx.R15,
            ];
            let end = regs[end_reg as usize & 15] as usize;
            let start = ctx.Rax as usize;
            // A pass zeroes the whole line `rax` is in, wherever in it, and the test comes
            // after it: lines are zeroed past an `end` inside one, and one is for an `end`
            // not above the start. (The bytes from `start` to `end` were, `rax` came out as
            // `end`, and the flags stayed what they were.)
            const MOST: usize = (2 << 30) / 64;
            let wanted = if end > start { (end - start).div_ceil(64) } else { 1 };
            let passes = wanted.min(MOST);
            let first = start & !63;
            let Some(stop) = first.checked_add(passes * 64) else { return 0 };
            // Region by region: a block may lie across several (the title's pages are
            // mapped one view each), and one answer for all of it was a refusal, after
            // which every 64-byte line took an exception of its own.
            let mut at = first;
            while at < stop {
                let Some(to) = writable_to(at, stop) else { break };
                // SAFETY: committed and writable from `at` to `to`.
                unsafe { std::ptr::write_bytes(at as *mut u8, 0, to - at) };
                at = to;
            }
            let done = (at - first) / 64;
            if done == 0 {
                return 0;
            }
            ZEROED.fetch_add((done * 64) as u64, Relaxed);
            ctx.Rax = ctx.Rax.wrapping_add((done * 64) as u64);
            if done < wanted {
                // What follows is not writable (or the block is huge): the loop goes on
                // there (the same instruction again), where the real one faults too.
                return CONTINUE_EXECUTION;
            }
            ctx.EFlags = ctx.EFlags & !crate::crash::CMP_FLAGS | crate::crash::cmp_flags(ctx.Rax, end as u64);
        }
        ConsoleOnly::Rdpru => {
            let mut v = 0i64;
            // SAFETY: a valid out pointer.
            unsafe { windows_sys::Win32::System::Performance::QueryPerformanceCounter(&mut v) };
            let v = if ctx.Rcx as u32 <= 1 { v as u64 } else { 0 };
            ctx.Rax = v & 0xFFFF_FFFF;
            ctx.Rdx = v >> 32;
        }
        ConsoleOnly::Rdpid(r) => {
            // SAFETY: no preconditions.
            let cpu = unsafe { windows_sys::Win32::System::Threading::GetCurrentProcessorNumber() } as u64;
            let regs: [&mut u64; 16] = [
                &mut ctx.Rax, &mut ctx.Rcx, &mut ctx.Rdx, &mut ctx.Rbx, &mut ctx.Rsp, &mut ctx.Rbp, &mut ctx.Rsi, &mut ctx.Rdi, &mut ctx.R8, &mut ctx.R9, &mut ctx.R10, &mut ctx.R11, &mut ctx.R12, &mut ctx.R13,
                &mut ctx.R14, &mut ctx.R15,
            ];
            *regs[r as usize & 15] = cpu;
        }
        ConsoleOnly::Movnt { src, wide, mem } => {
            let regs: [u64; 16] = [
                ctx.Rax, ctx.Rcx, ctx.Rdx, ctx.Rbx, ctx.Rsp, ctx.Rbp, ctx.Rsi, ctx.Rdi, ctx.R8, ctx.R9, ctx.R10, ctx.R11, ctx.R12, ctx.R13, ctx.R14, ctx.R15,
            ];
            let to = mem.address(&regs, ctx.Rip.wrapping_add(len as u64)) as usize;
            let bytes = if wide { 8 } else { 4 };
            if to.checked_add(bytes).is_none() || !writable(to, bytes) {
                return 0; // the real instruction faults there too
            }
            // SAFETY: the context's XMM registers (CONTEXT_FLOATING_POINT is captured for exceptions).
            let low = unsafe { ctx.Anonymous.FltSave.XmmRegisters[src as usize & 15].Low };
            // SAFETY: `bytes` bytes at `to` are committed and writable.
            unsafe { std::ptr::copy_nonoverlapping(low.to_le_bytes().as_ptr(), to as *mut u8, bytes) };
        }
        // SSE4a modifies the low quadword and zeroes the high quadword.
        ConsoleOnly::Extrq { dst, src, len, idx } => {
            // SAFETY: the context's XMM registers (CONTEXT_FLOATING_POINT is captured for exceptions).
            let x = unsafe { &mut ctx.Anonymous.FltSave.XmmRegisters };
            let (len, idx) = match src {
                Some(s) => {
                    let c = x[s as usize & 15].Low;
                    (c as u8, (c >> 8) as u8)
                }
                None => (len, idx),
            };
            let d = &mut x[dst as usize & 15];
            d.Low = decode::extrq(d.Low, len, idx);
            d.High = 0;
        }
        ConsoleOnly::Insertq { dst, src, reg, len, idx } => {
            // SAFETY: as above.
            let x = unsafe { &mut ctx.Anonymous.FltSave.XmmRegisters };
            let s = x[src as usize & 15];
            let (len, idx) = if reg { (s.High as u8, (s.High >> 8) as u8) } else { (len, idx) };
            let d = &mut x[dst as usize & 15];
            d.Low = decode::insertq(d.Low, s.Low, len, idx);
            d.High = 0;
        }
    }
    ctx.Rip += len as u64;
    // How often this runs at all (each one is an exception: a trip into Windows and back).
    let emulated = EMULATED.fetch_add(1, Relaxed) + 1;
    if emulated >= 1 << 16 && emulated.is_power_of_two() {
        tracing::info!(
            emulated,
            asked = ASKED.load(Relaxed),
            zeroed_mb = ZEROED.load(Relaxed) >> 20,
            "console CPU instructions emulated so far, the times Windows was asked about memory for them, and what clzero loops zeroed"
        );
    }
    CONTINUE_EXECUTION
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_instructions_are_replaced_inside_sixteen_aligned_bytes() {
        let word = u128::from_le_bytes([1, 2, 3, 4, 5, 6, 7, 0x0f, 0x01, 0xfb, 11, 12, 13, 14, 15, 16]);
        let new = with_patch(word, 0x1007, &[0xf3, 0x90, 0x90]).unwrap();
        assert_eq!(new.to_le_bytes(), [1, 2, 3, 4, 5, 6, 7, 0xf3, 0x90, 0x90, 11, 12, 13, 14, 15, 16]);
        assert!(with_patch(word, 0x100d, &[0; 3]).is_some());
        assert!(with_patch(word, 0x100e, &[0; 3]).is_none(), "crosses into the next sixteen");
    }

    /// `monitorx; mwaitx; ret` in a page of its own: each is replaced at its first fault, the
    /// one over the end of its aligned 16 bytes by one byte of it, and the page is protected as
    /// before. The same with `VICTORY_MWAITX_WAIT=1`: no jump fits there (the bytes after
    /// each are another wait instruction or a `ret`, and no unwind data covers the page), so
    /// each is replaced as by default. (Places that take a jump: `console_wait.rs`.)
    #[test]
    fn a_wait_faults_once_and_the_page_keeps_its_protection() {
        for waits in [false, true] {
            // SAFETY: as below.
            unsafe { a_wait_faults_once(waits) };
        }
    }

    /// [`a_wait_faults_once_and_the_page_keeps_its_protection`] with the switch `waits`.
    ///
    /// # Safety
    /// Runs code of its own.
    unsafe fn a_wait_faults_once(waits: bool) {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READ, PAGE_READWRITE, VirtualAlloc, VirtualFree,
            VirtualProtect, VirtualQuery,
        };
        // SAFETY: memory of our own, read, written and run within its page.
        unsafe {
            let page = VirtualAlloc(std::ptr::null(), 4096, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE).cast::<u8>();
            assert!(!page.is_null());
            let code = [0x0f, 0x01, 0xfa, 0x0f, 0x01, 0xfb, 0xc3];
            // The first over the end of its aligned eight bytes, as the title's is; and one
            // over the end of its sixteen (at 46, 14 of its 16).
            std::ptr::copy_nonoverlapping(code.as_ptr(), page.add(6), code.len());
            std::ptr::copy_nonoverlapping(code.as_ptr(), page.add(46), code.len());
            let mut old = 0;
            assert_ne!(VirtualProtect(page.cast(), 4096, PAGE_EXECUTE_READ, &mut old), 0);
            let at = page as usize;
            let mut context: CONTEXT = std::mem::zeroed();
            for rip in [at + 6, at + 9, at + 46, at + 49] {
                context.Rip = rip as u64;
                assert_eq!(emulate_with(&mut context, waits), CONTINUE_EXECUTION);
                assert_eq!(context.Rip, rip as u64 + 3, "waits {waits}");
            }
            if patches_waits() {
                let now = std::slice::from_raw_parts(page.add(6), 7);
                assert_eq!(now, [0x0f, 0x1f, 0x00, 0xf3, 0x90, 0x90, 0xc3]);
                // They run now.
                let run: extern "system" fn() = std::mem::transmute(page.add(6));
                run();
                // A second thread that faulted at the same place meanwhile.
                assert!(patch(at + 6, [0x0f, 0x01, 0xfa], [0x0f, 0x1f, 0x00]));
                assert!(!patch(at + 46, [0x0f, 0x01, 0xfa], [0x0f, 0x1f, 0x00]), "crosses its sixteen bytes");
            }
            // No one store of 16 bytes covers the one at 46: its second byte, a hint no-op
            // that runs here.
            let over = std::slice::from_raw_parts(page.add(46), 7);
            match wait_patches() {
                WaitPatches::All => {
                    assert_eq!(over, [0x0f, 0x1e, 0xfa, 0xf3, 0x90, 0x90, 0xc3]);
                    let run = std::mem::transmute::<*mut u8, extern "system" fn()>(page.add(46));
                    run();
                }
                WaitPatches::Aligned => assert_eq!(&over[..3], [0x0f, 0x01, 0xfa]),
                WaitPatches::Off => assert_eq!(over, code),
            }
            let mut m: MEMORY_BASIC_INFORMATION = std::mem::zeroed();
            VirtualQuery(page.cast(), &mut m, size_of_val(&m));
            assert_eq!(m.Protect, PAGE_EXECUTE_READ);
            VirtualFree(page.cast(), 0, MEM_RELEASE);
        }
    }

    /// An `mwaitx` at 15 of its aligned 16 bytes is replaced by its last byte (`serialize`, or a
    /// hint no-op) and runs; a thread whose fault the old bytes raised, and whose handler finds
    /// the new ones, runs those; and a fault where nothing of ours was replaced is reported.
    #[test]
    fn a_fault_on_bytes_replaced_meanwhile_runs_the_new_ones() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_EXECUTE_READ, PAGE_READWRITE, VirtualAlloc, VirtualFree, VirtualProtect,
        };
        // SAFETY: memory of our own, read, written and run within its page.
        unsafe {
            let page = VirtualAlloc(std::ptr::null(), 4096, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE).cast::<u8>();
            assert!(!page.is_null());
            std::ptr::copy_nonoverlapping([0x0f, 0x01, 0xfb, 0xc3].as_ptr(), page.add(79), 4);
            std::ptr::copy_nonoverlapping([0x90, 0x90, 0xc3].as_ptr(), page.add(200), 3);
            let mut old = 0;
            assert_ne!(VirtualProtect(page.cast(), 4096, PAGE_EXECUTE_READ, &mut old), 0);
            let at = page as usize;
            let mut context: CONTEXT = std::mem::zeroed();
            context.Rip = (at + 79) as u64;
            assert_eq!(emulate(&mut context), CONTINUE_EXECUTION);
            assert_eq!(context.Rip, (at + 82) as u64);
            if wait_patches() == WaitPatches::All {
                let new = if serialize() { [0x0f, 0x01, 0xe8] } else { [0x0f, 0x1e, 0xfb] };
                assert_eq!(std::slice::from_raw_parts(page.add(79), 3), new);
                let run = std::mem::transmute::<*mut u8, extern "system" fn()>(page.add(79));
                run();
                // The fault of a thread that fetched the old bytes, handled after the new ones
                // were stored: the same place runs again.
                context.Rip = (at + 79) as u64;
                assert_eq!(emulate(&mut context), CONTINUE_EXECUTION);
                assert_eq!(context.Rip, (at + 79) as u64);
            }
            // Nothing of ours there: left to the crash report.
            context.Rip = (at + 200) as u64;
            assert_eq!(emulate(&mut context), 0);
            VirtualFree(page.cast(), 0, MEM_RELEASE);
        }
    }

    /// `clzero` loops run as plain stores: the lines they name are zero and no other byte,
    /// and the pointer, the scratch register and the flags come out as from the loop.
    #[test]
    fn a_clzero_loop_becomes_plain_stores() {
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_EXECUTE_READWRITE, VirtualAlloc, VirtualFree};
        // Against rdx (rm rax, reg rdx) and against rcx, the scratch register.
        for (cmp, by) in [([0x48, 0x39, 0xd0], 2), ([0x48, 0x39, 0xc8], 1), ([0x4c, 0x39, 0xc0], 8)] {
            // SAFETY: memory of our own, written and run inside it.
            unsafe {
                let page = VirtualAlloc(std::ptr::null(), 4096, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE) as usize;
                assert_ne!(page, 0);
                // A function of (start, end): the start in rax, the end in its register, the
                // loop at 16, and what rax became.
                let mut head = vec![0x48, 0x89, 0xc8]; // mov rax, rcx
                match by {
                    1 => head.extend([0x48, 0x89, 0xd1]),  // mov rcx, rdx
                    8 => head.extend([0x49, 0x89, 0xd0]),  // mov r8, rdx
                    _ => {}
                }
                head.resize(16, 0x90);
                let mut code = head;
                code.extend([0x0f, 0x01, 0xfc, 0x48, 0x83, 0xc0, 0x40]);
                code.extend(cmp);
                code.extend([0x72, 0xf4]);
                code.push(0xc3);
                std::ptr::copy_nonoverlapping(code.as_ptr(), page as *mut u8, code.len());
                let mut context: CONTEXT = std::mem::zeroed();
                let mut block = vec![0xaau8; 64 * 8 + 64];
                let base = block.as_mut_ptr() as usize;
                let first = base.next_multiple_of(64);
                // The fault itself: emulated whole, and replaced.
                context.Rip = page as u64 + 16;
                context.Rax = first as u64;
                context.Rdx = first as u64 + 128;
                context.Rcx = context.Rdx;
                context.R8 = context.Rdx;
                assert_eq!(emulate(&mut context), CONTINUE_EXECUTION);
                assert_eq!((context.Rip, context.Rax), (page as u64 + 28, first as u64 + 128));
                if patches_clzero() {
                    assert_eq!(*((page + 16) as *const u8), 0xe9, "a jump");
                    block.fill(0xaa);
                    let run: extern "system" fn(usize, usize) -> usize = std::mem::transmute(page);
                    // From inside a line to inside the third: whole lines, one past the end.
                    assert_eq!(run(first + 8, first + 130), first + 8 + 128);
                    let at = first - base;
                    assert!(block[at..at + 128].iter().all(|&b| b == 0));
                    assert!(block[..at].iter().chain(&block[at + 128..]).all(|&b| b == 0xaa));
                    // An end at the start: one line.
                    block.fill(0xaa);
                    assert_eq!(run(first, first), first + 64);
                    assert!(block[at..at + 64].iter().all(|&b| b == 0) && block[at + 64] == 0xaa);
                }
                VirtualFree(page as *mut _, 0, MEM_RELEASE);
            }
        }
        assert!(clzero_stub(0x1000, 0x2000, [0x48, 0x39, 0xc0]).is_none(), "against rax itself");
        assert!(jump(0x1000, 0x9000_0000_0000).is_none(), "out of reach");
    }

    /// `movntss` and `movntsd` store the register's low 4 or 8 bytes where their operand
    /// says and no other byte, and are stepped over; one to memory that cannot be written,
    /// and `mcommit`, are left to the crash report.
    #[test]
    fn a_scalar_non_temporal_store_is_a_plain_store() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_EXECUTE_READ, PAGE_READWRITE, VirtualAlloc, VirtualFree, VirtualProtect,
        };
        // SAFETY: memory of our own, read and written within its pages.
        unsafe {
            let page = VirtualAlloc(std::ptr::null(), 8192, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE).cast::<u8>();
            assert!(!page.is_null());
            let at = page as usize;
            let data = at + 4096;
            let code: [&[u8]; 5] = [
                &[0xF3, 0x0F, 0x2B, 0x08],                         // movntss [rax], xmm1
                &[0xF2, 0x44, 0x0F, 0x2B, 0x4C, 0x88, 0x10],       // movntsd [rax + rcx*4 + 16], xmm9
                &[0xF2, 0x0F, 0x2B, 0x15, 0x00, 0x10, 0x00, 0x00], // movntsd [rip + 0x1000], xmm2
                &[0xF3, 0x0F, 0x2B, 0x0A],                         // movntss [rdx], xmm1: not writable
                &[0xF3, 0x0F, 0x01, 0xFA],                         // mcommit
            ];
            let starts = [0usize, 16, 32, 48, 64];
            for (bytes, start) in code.iter().zip(starts) {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), page.add(start), bytes.len());
            }
            let mut old = 0;
            assert_ne!(VirtualProtect(page.cast(), 4096, PAGE_EXECUTE_READ, &mut old), 0);
            let mut context: CONTEXT = std::mem::zeroed();
            context.Anonymous.FltSave.XmmRegisters[1].Low = 0x1122_3344_5566_7788;
            context.Anonymous.FltSave.XmmRegisters[2].Low = 0x0102_0304_0506_0708;
            context.Anonymous.FltSave.XmmRegisters[9].Low = 0xA1A2_A3A4_A5A6_A7A8;
            context.Rax = data as u64 + 8;
            context.Rcx = 6;
            context.Rdx = at as u64;
            std::slice::from_raw_parts_mut(data as *mut u8, 4096).fill(0xEE);
            let stored = |from: usize, n: usize| std::slice::from_raw_parts((data + from) as *const u8, n).to_vec();
            for (i, want) in [CONTINUE_EXECUTION, CONTINUE_EXECUTION, CONTINUE_EXECUTION, 0, 0].into_iter().enumerate() {
                context.Rip = (at + starts[i]) as u64;
                assert_eq!(emulate(&mut context), want, "instruction {i}");
                let step = if want == CONTINUE_EXECUTION { code[i].len() } else { 0 };
                assert_eq!(context.Rip, (at + starts[i] + step) as u64, "instruction {i}");
            }
            let ee = [0xEE; 8];
            assert_eq!(stored(0, 16), [ee.as_slice(), &[0x88, 0x77, 0x66, 0x55, 0xEE, 0xEE, 0xEE, 0xEE]].concat());
            // (from the end of the 8-byte instruction at 32, and at rax + rcx*4 + 16)
            let two = [8, 7, 6, 5, 4, 3, 2, 1, 0xA8, 0xA7, 0xA6, 0xA5, 0xA4, 0xA3, 0xA2, 0xA1];
            assert_eq!(stored(16, 48), [[0xEE; 24].as_slice(), &two, &ee].concat());
            VirtualFree(page.cast(), 0, MEM_RELEASE);
        }
    }
}
