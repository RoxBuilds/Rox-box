//! Overrides of shared Windows APIs whose console meaning differs.
//!
//! Only the differences live here; everything else passes through to the
//! host (see `HostPolicy`).
#![allow(non_snake_case)]
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::{CStr, c_char, c_void};

use crate::{HleModule, HostPolicy, hle_export, registry, runtime};

/// `GetModuleHandleW(NULL)` must return the title's image, not our exe;
/// title images and our console modules are found by name.
pub extern "system" fn GetModuleHandleW(name: *const u16) -> *mut c_void {
    if name.is_null() {
        return runtime::main_image().unwrap_or(0) as *mut c_void;
    }
    find_module(&wide(name), false).unwrap_or_else(|| host::get_module_handle_w(name))
}

/// See [`GetModuleHandleW`].
pub extern "system" fn GetModuleHandleA(name: *const u8) -> *mut c_void {
    if name.is_null() {
        return runtime::main_image().unwrap_or(0) as *mut c_void;
    }
    find_module(&narrow(name), false).unwrap_or_else(|| host::get_module_handle_a(name))
}

/// A NUL-terminated string from the title.
fn narrow(text: *const u8) -> String {
    // SAFETY: the title passes a NUL-terminated string.
    unsafe { CStr::from_ptr(text.cast()) }
        .to_string_lossy()
        .into_owned()
}

/// See [`narrow`].
fn wide(text: *const u16) -> String {
    // SAFETY: the title passes a NUL-terminated string.
    unsafe {
        let len = (0..).take_while(|&i| *text.add(i) != 0).count();
        String::from_utf16_lossy(std::slice::from_raw_parts(text, len))
    }
}

/// A title image's path is its original file, not our cache copy;
/// `NULL` means the title's exe.
pub extern "system" fn GetModuleFileNameW(module: *mut c_void, out: *mut u16, size: u32) -> u32 {
    let Some(path) = title_path(module) else {
        return host::get_module_file_name_w(module, out, size);
    };
    let wide: Vec<u16> = path.encode_utf16().collect();
    copy_truncated(&wide, title_buffer(out, size))
}

/// See [`GetModuleFileNameW`].
pub extern "system" fn GetModuleFileNameA(module: *mut c_void, out: *mut u8, size: u32) -> u32 {
    let Some(path) = title_path(module) else {
        return host::get_module_file_name_a(module, out, size);
    };
    copy_truncated(path.as_bytes(), title_buffer(out, size))
}

/// The title's buffer of `size` elements; none for a null pointer (a slice
/// may not start at null, even an empty one, so `(NULL, 0)` would be
/// undefined behaviour).
fn title_buffer<'a, T>(out: *mut T, size: u32) -> &'a mut [T] {
    if out.is_null() {
        return &mut [];
    }
    // SAFETY: the title passes a buffer of `size` elements.
    unsafe { std::slice::from_raw_parts_mut(out, size as usize) }
}

fn title_path(module: *mut c_void) -> Option<String> {
    let base = if module.is_null() {
        runtime::main_image()?
    } else {
        module as usize
    };
    Some(runtime::image_path(base)?.to_string_lossy().into_owned())
}

/// `GetModuleFileName`'s contract: copy with a NUL; if it does not fit,
/// truncate to the buffer, report `ERROR_INSUFFICIENT_BUFFER` and return
/// the buffer size.
fn copy_truncated<T: Copy + Default>(text: &[T], out: &mut [T]) -> u32 {
    let Some(room) = out.len().checked_sub(1) else {
        host::set_last_error(ERROR_INSUFFICIENT_BUFFER);
        return 0;
    };
    let n = text.len().min(room);
    out[..n].copy_from_slice(&text[..n]);
    out[n] = T::default();
    if n < text.len() {
        host::set_last_error(ERROR_INSUFFICIENT_BUFFER);
        return out.len() as u32;
    }
    n as u32
}

const ERROR_INSUFFICIENT_BUFFER: u32 = 122;

/// Console-only DLLs we implement load as a pseudo handle (titles load
/// `xgameruntime.dll` this way), the title's DLLs as the images we map
/// (on first use if nothing imports them); anything else loads on the host.
pub extern "system" fn LoadLibraryExA(
    name: *const u8,
    file: *mut c_void,
    flags: u32,
) -> *mut c_void {
    if !name.is_null()
        && let Some(handle) = find_module(&narrow(name), runs_code(flags))
    {
        return handle;
    }
    host::load_library_ex_a(name, file, flags)
}

/// See [`LoadLibraryExA`].
pub extern "system" fn LoadLibraryExW(
    name: *const u16,
    file: *mut c_void,
    flags: u32,
) -> *mut c_void {
    if !name.is_null()
        && let Some(handle) = find_module(&wide(name), runs_code(flags))
    {
        return handle;
    }
    host::load_library_ex_w(name, file, flags)
}

/// False for loads as data or resources (`DONT_RESOLVE_DLL_REFERENCES`,
/// `LOAD_LIBRARY_AS_DATAFILE(_EXCLUSIVE)`, `AS_IMAGE_RESOURCE`): those run
/// no code, so a title DLL we have not mapped can go to the host.
fn runs_code(flags: u32) -> bool {
    flags & (0x1 | 0x2 | 0x20 | 0x40) == 0
}

pub extern "system" fn LoadLibraryA(name: *const u8) -> *mut c_void {
    LoadLibraryExA(name, std::ptr::null_mut(), 0)
}

pub extern "system" fn LoadLibraryW(name: *const u16) -> *mut c_void {
    LoadLibraryExW(name, std::ptr::null_mut(), 0)
}

/// Looks up our implementations behind a pseudo handle; real modules go to
/// the host.
pub extern "system" fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void {
    // `NULL` is the process's exe, which for the title is its own image.
    let module = if module.is_null() {
        runtime::main_image().unwrap_or(0) as *mut c_void
    } else {
        module
    };
    let Some(m) = registry().iter().find(|m| handle(m) == module) else {
        if let Some(ours) = host_override(module, name) {
            return ours;
        }
        let found = host::get_proc_address(module, name);
        // The FidelityFX API's entry points, from whichever DLL: the host
        // upscaler's shims (`fidelityfx`), when it is on.
        if !found.is_null() && (name as usize) >= 0x1_0000 {
            return super::fidelityfx::interpose(&narrow(name), found as usize) as *mut c_void;
        }
        return found;
    };
    // Values below 0x10000 are ordinals; we export by name only.
    if (name as usize) < 0x1_0000 {
        tracing::warn!(
            module = m.name,
            ordinal = name as usize,
            "GetProcAddress by ordinal"
        );
        return std::ptr::null_mut();
    }
    // SAFETY: not an ordinal, so the title passes a NUL-terminated name.
    let name = unsafe { CStr::from_ptr(name.cast()) }.to_string_lossy();
    match m.exports.iter().find(|e| e.name == name) {
        Some(e) => e.address as *mut c_void,
        None => {
            tracing::warn!(module = m.name, %name, "GetProcAddress: not implemented");
            std::ptr::null_mut()
        }
    }
}

/// What a host module's `name` (or ordinal) is bound to at import binding
/// when we override it: a title that looks a function up by name must not
/// get past our overrides (the network guard, path rewriting).
fn host_override(module: *mut c_void, name: *const u8) -> Option<*mut c_void> {
    if module.is_null() || runtime::image_path(module as usize).is_some() {
        return None;
    }
    let symbol = if (name as usize) < 0x1_0000 {
        format!("#{}", name as usize)
    } else {
        narrow(name)
    };
    let mut path = [0u16; 260];
    let len = host::get_module_file_name_w(module, path.as_mut_ptr(), path.len() as u32) as usize;
    let path = String::from_utf16_lossy(&path[..len.min(path.len())]);
    let file = path.rsplit('\\').next()?;
    crate::registry::lookup(file, &symbol).map(|address| address as *mut c_void)
}

/// Our pseudo modules stay loaded, and the host's count of a title image
/// must not drop: `LoadLibrary` took no host reference on them. A title DLL
/// the title loaded itself is counted by the loader ([`title_dll_free`]):
/// its last `FreeLibrary` detaches and unmaps it, as the Windows loader
/// does.
pub extern "system" fn FreeLibrary(module: *mut c_void) -> i32 {
    if registry().iter().any(|m| handle(m) == module) {
        return 1;
    }
    if runtime::image_path(module as usize).is_some() {
        if title_dll_free() {
            let gone = runtime::free_title_dll(module as usize) == Some(false);
            if gone {
                tracing::info!(module = format_args!("{module:?}"), "a title DLL the title freed is unloaded");
            }
        }
        return 1;
    }
    host::free_library(module)
}

/// [`FreeLibrary`], then the thread's end, as a thread a DLL started ends
/// (the host's would take a reference off a title image the host counts
/// once, and unmap it under every thread that still hears of its starts and
/// ends: the next thread notice ran code that was gone). Ours with
/// [`title_dll_free`] only.
pub extern "system" fn FreeLibraryAndExitThread(module: *mut c_void, code: u32) -> ! {
    FreeLibrary(module);
    host::exit_thread(code)
}

/// Whether the title's `FreeLibrary` of a title DLL it loaded counts, as
/// the Windows loader counts: every `LoadLibrary` of it takes
/// a reference, every `FreeLibrary` gives one back, and the last detaches it
/// (`DLL_PROCESS_DETACH`, then unmapping; a later `LoadLibrary` maps and
/// attaches it again); `FreeLibraryAndExitThread` is ours too. A DLL the
/// title imports stays for the process's life, as on Windows. On by
/// default. `VICTORY_TITLE_DLL_FREE=0`: as before, `FreeLibrary` of a title
/// image does nothing (its process detach comes at the process's end, its
/// statics outlive a reload, and it still hears of threads), and
/// `FreeLibraryAndExitThread` is the host's, which unmaps a title DLL from
/// under the loader. Read once.
pub fn title_dll_free() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TITLE_DLL_FREE").is_none_or(|v| v != "0"))
}

/// Title images hear of thread starts and ends from us, not from the
/// Windows loader (`victory_loader::disable_thread_calls`).
pub extern "system" fn DisableThreadLibraryCalls(module: *mut c_void) -> i32 {
    host::disable_thread_library_calls(module)
}

/// Title debug output goes to our log (no debugger needs to be attached).
pub extern "system" fn OutputDebugStringA(text: *const u8) {
    if !text.is_null() {
        let text = narrow(text);
        host::note_title_output(text.trim());
        tracing::info!(target: "title", "{}", text.trim_end());
    }
}

/// See [`OutputDebugStringA`].
pub extern "system" fn OutputDebugStringW(text: *const u16) {
    if !text.is_null() {
        let text = wide(text);
        host::note_title_output(text.trim());
        tracing::info!(target: "title", "{}", text.trim_end());
    }
}

/// The CRT's `exit`, through which the title's entry point returns: logs
/// the code and the caller, then exits on the host (the loader ends the
/// process with `code` once the title images have detached).
pub extern "C" fn exit(code: i32) -> ! {
    tracing::warn!(code, from = %host::callers(), "title exits");
    let _ = victory_loader::report::finish(victory_loader::report::End::TitleExited(code));
    #[cfg(windows)]
    victory_loader::exiting_with(code);
    host::exit(code)
}

/// The CRT's `abort` (also reached from `terminate`): logged like [`exit`].
pub extern "C" fn abort() -> ! {
    tracing::error!(from = %host::callers(), "title aborts");
    let _ = victory_loader::report::finish(victory_loader::report::End::TitleAborted);
    host::abort()
}

/// The CRT's `terminate` and the STL's invalid-argument exit: logged, then
/// the host's `abort`.
pub extern "C" fn terminate() -> ! {
    tracing::error!(from = %host::callers(), "title calls terminate");
    let _ = victory_loader::report::finish(victory_loader::report::End::TitleAborted);
    host::abort()
}

/// See [`terminate`].
pub extern "C" fn _invalid_parameter_noinfo_noreturn() -> ! {
    tracing::error!(from = %host::callers(), "title passed an invalid parameter");
    let _ = victory_loader::report::finish(victory_loader::report::End::TitleAborted);
    host::abort()
}

/// The title's unhandled-exception filter. Ours stands in front of it and
/// reports the crash first; the title still gets its own back.
static TITLE_FILTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub extern "system" fn SetUnhandledExceptionFilter(filter: usize) -> usize {
    host::set_unhandled_exception_filter(report_unhandled);
    TITLE_FILTER.swap(filter, std::sync::atomic::Ordering::AcqRel)
}

extern "system" fn report_unhandled(info: *const c_void) -> i32 {
    host::report_unhandled(info);
    match TITLE_FILTER.load(std::sync::atomic::Ordering::Acquire) {
        0 => 0, // EXCEPTION_CONTINUE_SEARCH
        filter => {
            type Filter = extern "system" fn(*const c_void) -> i32;
            // SAFETY: the title registered this filter.
            let filter: Filter = unsafe { std::mem::transmute(filter) };
            filter(info)
        }
    }
}

/// Whether the title's own ways of ending its process without the C
/// runtime's `exit` are ours (`ExitProcess`, `_exit`, `_Exit`, `quick_exit`,
/// an entry point that returns). On by default; `VICTORY_TITLE_EXITS=0`: the
/// host's, as before (and an entry point that returns ends `victory run`
/// with code 0). Ours record the end with the title's code, then leave as
/// [`exit`] does: the title images detached, the process ended with that
/// code (`victory_loader::exit_process`). The host's ended the process with
/// nothing recorded (the run report stayed the last periodic one,
/// `killed_or_unknown`) and no title image detached; a title's own exit is
/// always an exit on the platform's terms.
pub fn title_exits() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TITLE_EXITS").is_none_or(|v| v != "0"))
}

/// `ExitProcess` called by the title: see [`title_exits`].
pub extern "system" fn ExitProcess(code: u32) -> ! {
    tracing::warn!(code, from = %host::callers(), "title ends the process");
    ends_itself(code as i32)
}

/// The CRT's `_exit` (no exit functions run): see [`title_exits`].
pub extern "C" fn _exit(code: i32) -> ! {
    tracing::warn!(code, from = %host::callers(), "title exits without its exit functions");
    ends_itself(code)
}

/// See [`_exit`], of which it is the C99 name.
pub extern "C" fn _Exit(code: i32) -> ! {
    tracing::warn!(code, from = %host::callers(), "title exits without its exit functions");
    ends_itself(code)
}

/// The CRT's `quick_exit`: the functions `at_quick_exit` registered run,
/// then the process ends. The host's runs them; the loader's detaching is
/// registered with it before the title's, so it comes last, as for `exit`.
pub extern "C" fn quick_exit(code: i32) -> ! {
    tracing::warn!(code, from = %host::callers(), "title quick-exits");
    let _ = victory_loader::report::finish(victory_loader::report::End::TitleExited(code));
    #[cfg(windows)]
    victory_loader::exiting_with(code);
    host::quick_exit(code)
}

/// The end is recorded, then the process ends with `code` once the title
/// images have detached.
fn ends_itself(code: i32) -> ! {
    let _ = victory_loader::report::finish(victory_loader::report::End::TitleExited(code));
    host::exit_process(code)
}

/// See [`exit`]; only the current process is logged.
pub extern "system" fn TerminateProcess(process: *mut c_void, code: u32) -> i32 {
    if process as isize == -1 {
        tracing::warn!(code, from = %host::callers(), "title terminates the process");
        use victory_loader::report::{End, finish};
        let _ = finish(End::TitleExited(code as i32));
    }
    host::terminate_process(process, code)
}

/// The pseudo handle of one of our modules: its address, never a real image.
fn handle(m: &HleModule) -> *mut c_void {
    m as *const HleModule as *mut c_void
}

/// The title's images and our console modules by name; `None` leaves the
/// name to the host, which only DLLs `HostPolicy` passes through get (as at
/// import binding); others fail. With `load`, a title DLL not mapped yet is
/// mapped now.
fn find_module(path: &str, load: bool) -> Option<*mut c_void> {
    let file = path.rsplit(['\\', '/']).next().unwrap_or(path);
    // As the Windows loader does, a name without extension means a DLL.
    let file = if file.contains('.') {
        file.to_owned()
    } else {
        format!("{file}.dll")
    };
    // Loads go through the title DLL loader first: it waits for a DLL that
    // another thread is still initialising, as the Windows loader lock does.
    if load {
        match runtime::load_title_dll(&file) {
            Ok(Some(base)) => return Some(base as *mut c_void),
            Ok(None) => {}
            // Not the host: it would load the unpatched DLL from the title's folder.
            Err(error) => {
                tracing::error!(dll = file, %error, "mapping a title DLL failed");
                host::set_last_error(ERROR_MOD_NOT_FOUND);
                return Some(std::ptr::null_mut());
            }
        }
    }
    if let Some(base) = runtime::image_named(&file) {
        return Some(base as *mut c_void);
    }
    // DLLs the host has too are real host modules, whatever `--strict` says.
    let host_dll = HostPolicy::default().allows(&file);
    if !host_dll && let Some(m) = registry().iter().find(|m| m.serves(&file)) {
        if load {
            tracing::info!(dll = file, "loaded as a pseudo module");
        }
        return Some(handle(m));
    }
    if host_dll {
        return None;
    }
    // Any other host DLL (wininet, urlmon, ...) is beyond what we guard, and
    // the console does not have it.
    refuse(&file);
    host::set_last_error(ERROR_MOD_NOT_FOUND);
    Some(std::ptr::null_mut())
}

/// Logs a DLL name the title is refused, once.
fn refuse(file: &str) {
    static REFUSED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    let file = file.to_ascii_lowercase();
    let mut refused = REFUSED.lock().unwrap_or_else(|e| e.into_inner());
    if !refused.contains(&file) {
        tracing::warn!(dll = file, "a host DLL outside the host policy: refused");
        refused.push(file);
    }
}

const ERROR_MOD_NOT_FOUND: u32 = 126;

/// The title's command line, every way a title reads it: `GetCommandLineW`
/// and `GetCommandLineA`, the C runtime's argument lists (`__p___argc`,
/// `__p___argv`, `__p___wargv`), its copies of the line (`__p__acmdln`,
/// `__p__wcmdln`) and what `WinMain` gets (`_get_narrow_winmain_command_line`,
/// `_get_wide_winmain_command_line`). All say the same: the title's image
/// name, then the arguments given after `--`, quoted as the C runtime parses
/// them back.
///
/// The host's own would be ours (`victory.exe run <exe> --execute ... --
/// <its arguments>`), and a title looks at fixed places in it (`-skipToIIS`
/// only as the first argument). The C runtime's lists were already the
/// title's while `GetCommandLine` passed to the host, so a title that reads
/// both (Rust's `std::env::args` reads `GetCommandLineW`) saw two different
/// command lines.
///
/// The program is the image's file name alone, as the title's manifest names
/// its executable (`<Executable Name=...>`), not the path the player keeps the
/// files at: a console starts a title from its package, which has no folders
/// of a PC above it, and a title must not learn the player's folder names
/// from its command line (it may write it to its logs or crash reports).
/// shortcut: whether the console puts the package's drive before the name is
/// not known; a bare name is what every parser of the program's name takes.
///
/// Made once, at the first call, and kept for the whole run: the title holds
/// on to the pointers, and gets the same ones every time, as from Windows and
/// the C runtime. The narrow forms are in the ANSI code page, as Windows
/// makes `GetCommandLineA` from the wide line.
///
/// `VICTORY_TITLE_COMMAND_LINE=0`: as before, `GetCommandLine` and the C
/// runtime's wide list and lines are the host's (ours), and only
/// `__p___argc` and `__p___argv` are the title's, with the host's program
/// name.
struct CommandLine {
    argc: std::cell::UnsafeCell<i32>,
    argv: std::cell::UnsafeCell<*mut *mut c_char>,
    wargv: std::cell::UnsafeCell<*mut *mut u16>,
    /// What `GetCommandLineW` and `A` return.
    wide: *mut u16,
    narrow: *mut c_char,
    /// `_wcmdln` and `_acmdln`: the C runtime's variables, which a title may
    /// write; they start as the lines above.
    wcmdln: std::cell::UnsafeCell<*mut u16>,
    acmdln: std::cell::UnsafeCell<*mut c_char>,
}

// SAFETY: never written here after it was made; what the title writes
// through `__p__acmdln` and the like it writes as into the C runtime's own
// variables, which have no lock either.
unsafe impl Send for CommandLine {}
// SAFETY: as above.
unsafe impl Sync for CommandLine {}

/// Whether the title's command line is its own ([`CommandLine`]): its image
/// name and the arguments after `--`, however it reads it. On by default: a
/// console title never sees a host launcher's command line.
/// `VICTORY_TITLE_COMMAND_LINE=0`: `GetCommandLine` and the C runtime's wide
/// list and lines are the host's (Rox-box's command line) and only
/// `__p___argc` and `__p___argv` the title's, with the host's program name,
/// as before. Read once.
fn title_command_line() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TITLE_COMMAND_LINE").is_none_or(|v| v != "0"))
}

/// Appends `arg` as the C runtime parses an argument back (and
/// `CommandLineToArgvW`): bare if it has no blank and no quote; else in
/// quotes, with a quote inside as `\"` and the backslashes before a quote or
/// the closing one doubled.
fn quote_arg(arg: &str, line: &mut String) {
    if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '\x0b', '"']) {
        line.push_str(arg);
        return;
    }
    line.push('"');
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                line.extend(std::iter::repeat_n('\\', 2 * backslashes + 1));
                line.push('"');
                backslashes = 0;
            }
            c => {
                line.extend(std::iter::repeat_n('\\', backslashes));
                line.push(c);
                backslashes = 0;
            }
        }
    }
    line.extend(std::iter::repeat_n('\\', 2 * backslashes));
    line.push('"');
}

/// The command line of `program` and `args`. The program's name is parsed
/// by other rules (a quote only opens and closes, no backslash escapes), and
/// a file name holds no quote: it is quoted only when it holds a blank.
fn command_line(program: &str, args: &[String]) -> String {
    let mut line = if program.is_empty() || program.contains([' ', '\t']) {
        format!("\"{program}\"")
    } else {
        program.to_owned()
    };
    for arg in args {
        line.push(' ');
        quote_arg(arg, &mut line);
    }
    line
}

/// A NUL-terminated copy of `units`, kept for the whole run.
fn leaked<T: Copy + Default>(units: impl IntoIterator<Item = T>) -> *mut T {
    let all: Box<[T]> = units.into_iter().chain(Some(T::default())).collect();
    Box::leak(all).as_mut_ptr()
}

/// A null-terminated list of those, kept for the whole run.
fn leaked_list<T>(items: Vec<*mut T>) -> *mut *mut T {
    let all: Box<[*mut T]> = items.into_iter().chain(Some(std::ptr::null_mut())).collect();
    Box::leak(all).as_mut_ptr()
}

/// What `program` and `args` give a title: the lists, and the line. The
/// narrow forms are in the ANSI code page (`ansi`) or, as the argument list
/// was before, in UTF-8.
fn make_command_line(program: &str, args: &[String], ansi: bool) -> CommandLine {
    let all: Vec<&str> = std::iter::once(program)
        .chain(args.iter().map(String::as_str))
        .collect();
    // A NUL inside would end it early in both forms alike.
    let wide = |s: &str| leaked(s.encode_utf16().take_while(|&u| u != 0));
    let bytes = |s: &str| if ansi { host::ansi(s) } else { s.as_bytes().to_vec() };
    let narrow = |s: &str| {
        leaked(bytes(s).into_iter().take_while(|&b| b != 0).map(|b| b as c_char))
    };
    let line = command_line(program, args);
    let (wide_line, narrow_line) = (wide(&line), narrow(&line));
    CommandLine {
        argc: (all.len() as i32).into(),
        argv: leaked_list(all.iter().map(|a| narrow(a)).collect()).into(),
        wargv: leaked_list(all.iter().map(|a| wide(a)).collect()).into(),
        wide: wide_line,
        narrow: narrow_line,
        wcmdln: wide_line.into(),
        acmdln: narrow_line.into(),
    }
}

/// The title's image name, for its command line; without a title (the
/// tests), this program's.
fn image_name() -> String {
    let path = runtime::main_image()
        .and_then(runtime::image_path)
        .or_else(|| std::env::current_exe().ok());
    path.as_deref()
        .and_then(std::path::Path::file_name)
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn the_command_line() -> &'static CommandLine {
    static LINE: std::sync::OnceLock<CommandLine> = std::sync::OnceLock::new();
    LINE.get_or_init(|| {
        let args = runtime::title_args();
        tracing::info!(?args, "the title's arguments");
        if title_command_line() {
            make_command_line(&image_name(), args, true)
        } else {
            // The program's name as the host says it (as it was before).
            let program = std::env::args_os().next().unwrap_or_default();
            make_command_line(&program.to_string_lossy(), args, false)
        }
    })
}

/// See [`CommandLine`].
pub extern "system" fn GetCommandLineW() -> *const u16 {
    the_command_line().wide
}

/// See [`CommandLine`].
pub extern "system" fn GetCommandLineA() -> *const c_char {
    the_command_line().narrow
}

/// See [`CommandLine`].
pub extern "C" fn __p___argc() -> *mut i32 {
    the_command_line().argc.get()
}

/// See [`CommandLine`].
pub extern "C" fn __p___argv() -> *mut *mut *mut c_char {
    the_command_line().argv.get()
}

/// See [`CommandLine`].
pub extern "C" fn __p___wargv() -> *mut *mut *mut u16 {
    the_command_line().wargv.get()
}

/// See [`CommandLine`].
pub extern "C" fn __p__acmdln() -> *mut *mut c_char {
    the_command_line().acmdln.get()
}

/// See [`CommandLine`].
pub extern "C" fn __p__wcmdln() -> *mut *mut u16 {
    the_command_line().wcmdln.get()
}

/// The arguments part of `line` (a NUL-terminated command line), as the C
/// runtime gives `WinMain` its `lpCmdLine`: past the program's name (to the
/// first blank, or the closing quote of a quoted one) and the blanks after
/// it.
///
/// # Safety
/// `line` is NUL-terminated.
unsafe fn winmain_tail<T: Copy + Into<u32>>(line: *mut T) -> *mut T {
    let (quote, space, tab) = (u32::from(b'"'), u32::from(b' '), u32::from(b'\t'));
    // SAFETY: every read stops at the NUL.
    unsafe {
        let at = |p: *mut T| (*p).into();
        let mut p = line;
        let mut quoted = false;
        while at(p) != 0 && (quoted || (at(p) != space && at(p) != tab)) {
            if at(p) == quote {
                quoted = !quoted;
            }
            p = p.add(1);
        }
        while at(p) == space || at(p) == tab {
            p = p.add(1);
        }
        p
    }
}

/// See [`CommandLine`] and [`winmain_tail`]; of the line `_acmdln` holds now.
pub extern "C" fn _get_narrow_winmain_command_line() -> *mut c_char {
    // SAFETY: the C runtime's variable, read as it reads its own; it holds
    // a NUL-terminated line (ours, or one the title put there).
    unsafe { winmain_tail((*the_command_line().acmdln.get()).cast::<u8>()).cast() }
}

/// See [`_get_narrow_winmain_command_line`].
pub extern "C" fn _get_wide_winmain_command_line() -> *mut u16 {
    // SAFETY: as above.
    unsafe { winmain_tail(*the_command_line().wcmdln.get()) }
}

/// The title's thread priorities, but for the lowest two, which become
/// "below normal". Six of the title's threads run at "idle", the streaming
/// pager's among them; on the console they have what the rest of the title
/// leaves of its cores. A PC's processors are also wanted by other programs
/// and by our own workers (sixteen when pipelines are made): with every
/// one of them busy such a thread ran once in four seconds or so, when the
/// host's scheduler took pity, and the loading that waits for it with it
/// (loads seen to be done a second late on average).
/// `VICTORY_TITLE_PRIORITIES=0`: as the title sets them.
fn title_priority(asked: i32) -> i32 {
    const IDLE: i32 = -15;
    const LOWEST: i32 = -2;
    const BELOW_NORMAL: i32 = -1;
    match asked {
        IDLE..=LOWEST => BELOW_NORMAL,
        other => other,
    }
}

/// [`title_priority`].
pub extern "system" fn SetThreadPriority(thread: *mut c_void, priority: i32) -> i32 {
    static AS_SET: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let as_set = *AS_SET.get_or_init(|| std::env::var_os("VICTORY_TITLE_PRIORITIES").is_some_and(|v| v == "0"));
    let priority = if as_set { priority } else { title_priority(priority) };
    // SAFETY: the title passes a thread handle.
    unsafe { windows_sys::Win32::System::Threading::SetThreadPriority(thread, priority) }
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "kernel",
        dlls: &[
            "kernel32.dll",
            "kernelbase.dll",
            "api-ms-win-core-libraryloader-*",
            "api-ms-win-core-processthreads-*",
            // `GetCommandLineW` and `A`.
            "api-ms-win-core-processenvironment-*",
            "api-ms-win-core-debug-*",
            "api-ms-win-core-errorhandling-*",
            "api-ms-win-crt-runtime-*",
        ],
        exports: exports(),
    }
}

fn exports() -> Vec<crate::HleExport> {
    let mut exports = vec![
        hle_export!(__p___argc),
        hle_export!(__p___argv),
        hle_export!(GetModuleHandleW),
        hle_export!(GetModuleHandleA),
        hle_export!(GetModuleFileNameW),
        hle_export!(GetModuleFileNameA),
        hle_export!(LoadLibraryExA),
        hle_export!(LoadLibraryExW),
        hle_export!(LoadLibraryA),
        hle_export!(LoadLibraryW),
        hle_export!(GetProcAddress),
        hle_export!(FreeLibrary),
        hle_export!(DisableThreadLibraryCalls),
        hle_export!(OutputDebugStringA),
        hle_export!(OutputDebugStringW),
        hle_export!(exit),
        hle_export!(abort),
        hle_export!(terminate),
        hle_export!(_invalid_parameter_noinfo_noreturn),
        hle_export!(TerminateProcess),
        hle_export!(SetUnhandledExceptionFilter),
        hle_export!(SetThreadPriority),
    ];
    // With either switch at 0 these imports are bound to the host's, as
    // before.
    if title_command_line() {
        exports.extend([
            hle_export!(GetCommandLineW),
            hle_export!(GetCommandLineA),
            hle_export!(__p___wargv),
            hle_export!(__p__acmdln),
            hle_export!(__p__wcmdln),
            hle_export!(_get_narrow_winmain_command_line),
            hle_export!(_get_wide_winmain_command_line),
        ]);
    }
    if title_dll_free() {
        exports.push(hle_export!(FreeLibraryAndExitThread));
    }
    if title_exits() {
        exports.extend([
            hle_export!(ExitProcess),
            hle_export!(_exit),
            hle_export!(_Exit),
            hle_export!(quick_exit),
        ]);
    }
    exports
}

/// The return addresses above the caller, as `module+rva`, for logs.
#[cfg(feature = "diagnostics")]
pub(crate) fn callers() -> String {
    host::callers()
}

#[cfg(windows)]
mod host {
    use std::ffi::c_void;

    use windows_sys::Win32::Foundation::{FreeLibrary, SetLastError};
    use windows_sys::Win32::System::Diagnostics::Debug::RtlCaptureStackBackTrace;
    use windows_sys::Win32::System::LibraryLoader::{
        DisableThreadLibraryCalls, GetModuleFileNameA, GetModuleFileNameW, GetModuleHandleA,
        GetModuleHandleW, GetProcAddress, LoadLibraryExA, LoadLibraryExW,
    };
    use windows_sys::Win32::System::Threading::TerminateProcess;

    unsafe extern "C" {
        #[link_name = "exit"]
        fn crt_exit(code: i32) -> !;
        #[link_name = "abort"]
        fn crt_abort() -> !;
        #[link_name = "quick_exit"]
        fn crt_quick_exit(code: i32) -> !;
    }

    // SAFETY (every function): forwarding the title's own arguments to the
    // host API of the same name.

    pub(super) fn get_module_handle_w(name: *const u16) -> *mut c_void {
        // SAFETY: see above.
        unsafe { GetModuleHandleW(name) }
    }

    pub(super) fn get_module_handle_a(name: *const u8) -> *mut c_void {
        // SAFETY: see above.
        unsafe { GetModuleHandleA(name) }
    }

    pub(super) fn get_module_file_name_w(module: *mut c_void, out: *mut u16, size: u32) -> u32 {
        // SAFETY: see above.
        unsafe { GetModuleFileNameW(module, out, size) }
    }

    pub(super) fn get_module_file_name_a(module: *mut c_void, out: *mut u8, size: u32) -> u32 {
        // SAFETY: see above.
        unsafe { GetModuleFileNameA(module, out, size) }
    }

    pub(super) fn set_last_error(code: u32) {
        // SAFETY: see above.
        unsafe { SetLastError(code) }
    }

    pub(super) fn load_library_ex_a(name: *const u8, file: *mut c_void, flags: u32) -> *mut c_void {
        // SAFETY: see above.
        unsafe { LoadLibraryExA(name, file, flags) }
    }

    pub(super) fn load_library_ex_w(
        name: *const u16,
        file: *mut c_void,
        flags: u32,
    ) -> *mut c_void {
        // SAFETY: see above.
        unsafe { LoadLibraryExW(name, file, flags) }
    }

    pub(super) fn get_proc_address(module: *mut c_void, name: *const u8) -> *mut c_void {
        // SAFETY: see above.
        unsafe { GetProcAddress(module, name) }.map_or(std::ptr::null_mut(), |f| f as *mut c_void)
    }

    pub(super) fn free_library(module: *mut c_void) -> i32 {
        // SAFETY: see above.
        unsafe { FreeLibrary(module) }
    }

    pub(super) fn exit_thread(code: u32) -> ! {
        // SAFETY: see above.
        unsafe { windows_sys::Win32::System::Threading::ExitThread(code) }
    }

    pub(super) fn disable_thread_library_calls(module: *mut c_void) -> i32 {
        match victory_loader::disable_thread_calls(module as usize) {
            Some(done) => i32::from(done),
            // SAFETY: see above.
            None => unsafe { DisableThreadLibraryCalls(module) },
        }
    }

    pub(super) fn exit(code: i32) -> ! {
        // SAFETY: see above.
        unsafe { crt_exit(code) }
    }

    pub(super) fn abort() -> ! {
        // SAFETY: see above.
        unsafe { crt_abort() }
    }

    pub(super) fn quick_exit(code: i32) -> ! {
        // SAFETY: see above.
        unsafe { crt_quick_exit(code) }
    }

    pub(super) fn exit_process(code: i32) -> ! {
        victory_loader::exit_process(code)
    }

    /// `text` in the ANSI code page, as Windows makes the narrow command
    /// line from the wide one (a character it lacks becomes its default).
    pub(super) fn ansi(text: &str) -> Vec<u8> {
        use windows_sys::Win32::Globalization::{CP_ACP, WideCharToMultiByte};
        let wide: Vec<u16> = text.encode_utf16().collect();
        if wide.is_empty() {
            return Vec::new();
        }
        let len = i32::try_from(wide.len()).unwrap_or(i32::MAX);
        let convert = |out: *mut u8, size: i32| {
            let (default, used) = (std::ptr::null(), std::ptr::null_mut());
            // SAFETY: `len` units of `wide`; `out` holds `size` bytes (none
            // when it is null, which only sizes the conversion).
            unsafe { WideCharToMultiByte(CP_ACP, 0, wide.as_ptr(), len, out, size, default, used) }
        };
        let size = convert(std::ptr::null_mut(), 0);
        let mut out = vec![0u8; size.max(0) as usize];
        let n = convert(out.as_mut_ptr(), size);
        out.truncate(n.max(0) as usize);
        out
    }

    pub(super) fn terminate_process(process: *mut c_void, code: u32) -> i32 {
        // SAFETY: see above.
        unsafe { TerminateProcess(process, code) }
    }

    pub(super) fn set_unhandled_exception_filter(filter: extern "system" fn(*const c_void) -> i32) {
        // SAFETY: our filter takes the exception pointers as an opaque pointer.
        unsafe {
            windows_sys::Win32::System::Diagnostics::Debug::SetUnhandledExceptionFilter(
                std::mem::transmute::<
                    extern "system" fn(*const c_void) -> i32,
                    windows_sys::Win32::System::Diagnostics::Debug::LPTOP_LEVEL_EXCEPTION_FILTER,
                >(filter),
            )
        };
    }

    pub(super) fn report_unhandled(info: *const c_void) {
        // SAFETY: the system's exception pointers, passed to the filter.
        unsafe { victory_loader::report_unhandled(info.cast()) };
    }

    pub(super) fn note_title_output(line: &str) {
        victory_loader::note_title_output(line);
    }

    /// Our caller's callers, as `module+rva`.
    pub(super) fn callers() -> String {
        let mut frames = [std::ptr::null_mut(); 12];
        // SAFETY: fills at most `frames.len()` return addresses.
        let n = unsafe {
            RtlCaptureStackBackTrace(
                2,
                frames.len() as u32,
                frames.as_mut_ptr(),
                std::ptr::null_mut(),
            )
        };
        let names: Vec<_> = frames[..usize::from(n)]
            .iter()
            .map(|&f| victory_loader::locate(f as usize))
            .collect();
        names.join(" < ")
    }
}

#[cfg(not(windows))]
mod host {
    use std::ffi::c_void;

    pub(super) fn get_module_handle_w(_: *const u16) -> *mut c_void {
        std::ptr::null_mut()
    }

    pub(super) fn get_module_handle_a(_: *const u8) -> *mut c_void {
        std::ptr::null_mut()
    }

    pub(super) fn load_library_ex_a(_: *const u8, _: *mut c_void, _: u32) -> *mut c_void {
        std::ptr::null_mut()
    }

    pub(super) fn get_module_file_name_w(_: *mut c_void, _: *mut u16, _: u32) -> u32 {
        0
    }

    pub(super) fn get_module_file_name_a(_: *mut c_void, _: *mut u8, _: u32) -> u32 {
        0
    }

    pub(super) fn set_last_error(_: u32) {}

    pub(super) fn load_library_ex_w(_: *const u16, _: *mut c_void, _: u32) -> *mut c_void {
        std::ptr::null_mut()
    }

    pub(super) fn get_proc_address(_: *mut c_void, _: *const u8) -> *mut c_void {
        std::ptr::null_mut()
    }

    pub(super) fn free_library(_: *mut c_void) -> i32 {
        0
    }

    pub(super) fn exit_thread(_: u32) -> ! {
        panic!("threads end only on Windows")
    }

    pub(super) fn disable_thread_library_calls(_: *mut c_void) -> i32 {
        0
    }

    pub(super) fn exit(code: i32) -> ! {
        std::process::exit(code)
    }

    pub(super) fn abort() -> ! {
        std::process::abort()
    }

    pub(super) fn quick_exit(code: i32) -> ! {
        std::process::exit(code)
    }

    pub(super) fn exit_process(code: i32) -> ! {
        std::process::exit(code)
    }

    pub(super) fn ansi(text: &str) -> Vec<u8> {
        text.as_bytes().to_vec()
    }

    pub(super) fn terminate_process(_: *mut c_void, code: u32) -> i32 {
        std::process::exit(code as i32)
    }

    pub(super) fn set_unhandled_exception_filter(_: extern "system" fn(*const c_void) -> i32) {}

    pub(super) fn report_unhandled(_: *const c_void) {}

    pub(super) fn note_title_output(_: &str) {}

    pub(super) fn callers() -> String {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The title's idle and lowest threads run below normal; the others as
    /// it sets them.
    #[test]
    fn no_title_thread_waits_for_idle_processors() {
        let given: Vec<i32> = [-15, -2, -1, 0, 1, 2, 15].into_iter().map(title_priority).collect();
        assert_eq!(given, [-1, -1, -1, 0, 1, 2, 15]);
        use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadPriority};
        std::thread::spawn(|| {
            // SAFETY: plain calls on this thread.
            let thread = unsafe { GetCurrentThread() };
            assert_eq!(SetThreadPriority(thread, -15), 1);
            // SAFETY: as above.
            assert_eq!(unsafe { GetThreadPriority(thread) }, -1);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn title_images_report_their_original_path() {
        let base = 0x7000_0000_0000;
        runtime::set_image_path(base, std::path::Path::new(r"C:\title\game.exe"));
        let module = base as *mut c_void;
        let mut buf = [0xFFFFu16; 32];
        let n = GetModuleFileNameW(module, buf.as_mut_ptr(), buf.len() as u32);
        assert_eq!(
            String::from_utf16_lossy(&buf[..n as usize]),
            r"C:\title\game.exe"
        );
        assert_eq!(buf[n as usize], 0);

        // Too small: truncated, NUL-terminated, the buffer size returned.
        let mut small = [0u8; 4];
        assert_eq!(GetModuleFileNameA(module, small.as_mut_ptr(), 4), 4);
        assert_eq!(&small, &[b'C', b':', b'\\', 0]);
    }

    /// The title's arguments come right after the program, and the list ends
    /// in a null pointer; without any there is only the program.
    /// A NUL-terminated wide string, read.
    fn wide_at(p: *const u16) -> String {
        wide(p)
    }

    /// Every list and line of `made`.
    fn views(made: &CommandLine) -> (Vec<String>, Vec<String>, String, String) {
        // SAFETY: made above, kept for the whole run; every list ends in a
        // null pointer and every string in a NUL.
        unsafe {
            let count = *made.argc.get() as usize;
            let (narrow, wide) = (*made.argv.get(), *made.wargv.get());
            assert!((*narrow.add(count)).is_null() && (*wide.add(count)).is_null());
            let narrow_list = (0..count)
                .map(|i| CStr::from_ptr(*narrow.add(i)).to_str().unwrap().to_owned())
                .collect();
            let wide_list = (0..count).map(|i| wide_at(*wide.add(i))).collect();
            let line = CStr::from_ptr(made.narrow).to_str().unwrap().to_owned();
            (narrow_list, wide_list, line, wide_at(made.wide))
        }
    }

    /// The title's arguments come right after the program, and the lists
    /// end in a null pointer; without any there is only the program. Both
    /// encodings and both forms (lists and lines) say the same.
    #[test]
    fn the_title_gets_its_own_arguments() {
        let some = make_command_line("game.exe", &["-skipToIIS".into(), "-x=1".into()], true);
        let (narrow, wide, line, wide_line) = views(&some);
        assert_eq!(narrow, ["game.exe", "-skipToIIS", "-x=1"]);
        assert_eq!(wide, narrow);
        assert_eq!(line, "game.exe -skipToIIS -x=1");
        assert_eq!(wide_line, line);
        let (narrow, _, line, _) = views(&make_command_line("game.exe", &[], true));
        assert_eq!((narrow, line), (vec!["game.exe".to_owned()], "game.exe".to_owned()));
        // What the title is given: the same pointers every time, and the
        // same arguments however it reads them.
        assert_eq!(__p___argc(), __p___argc());
        assert_eq!(GetCommandLineW(), GetCommandLineW());
        // SAFETY: as above.
        unsafe {
            assert_eq!((*__p___argc(), (**__p___argv()).is_null()), (1, false));
            assert_eq!(*__p__wcmdln() as *const u16, GetCommandLineW());
            assert_eq!(*__p__acmdln() as *const c_char, GetCommandLineA());
            let program = CStr::from_ptr(**__p___argv()).to_str().unwrap().to_owned();
            assert_eq!(wide_at(**__p___wargv()), program);
            // The image's name, never a path.
            assert!(!program.contains(['\\', '/']) && !program.is_empty(), "{program}");
            assert_eq!(wide_at(GetCommandLineW()), command_line(&program, &[]));
            assert_eq!(*_get_wide_winmain_command_line(), 0);
            assert_eq!(*_get_narrow_winmain_command_line(), 0);
        }
    }

    /// `WinMain`'s line is what follows the program's name and its blanks,
    /// a quoted name with blanks in it included.
    #[test]
    fn winmain_gets_what_follows_the_program() {
        let tail = |line: &str| {
            let mut units: Vec<u16> = line.encode_utf16().chain(Some(0)).collect();
            // SAFETY: NUL-terminated above.
            wide_at(unsafe { winmain_tail(units.as_mut_ptr()) })
        };
        assert_eq!(tail("game.exe  -a b"), "-a b");
        assert_eq!(tail("\"my game.exe\"\t-a"), "-a");
        assert_eq!(tail("game.exe"), "");
        assert_eq!(tail(""), "");
    }

    /// The line parses back into the program and the arguments, as Windows'
    /// own parser (the C runtime's rules) reads it: blanks, quotes,
    /// backslashes before quotes and at the end, empty arguments.
    #[cfg(windows)]
    #[test]
    fn the_command_line_parses_back_into_the_arguments() {
        #[link(name = "shell32")]
        unsafe extern "system" {
            fn CommandLineToArgvW(line: *const u16, count: *mut i32) -> *mut *mut u16;
        }
        let parsed = |line: &str| {
            let units: Vec<u16> = line.encode_utf16().chain(Some(0)).collect();
            let mut count = 0;
            // SAFETY: a NUL-terminated line; the list it returns has `count`
            // NUL-terminated strings (left for the process to free).
            unsafe {
                let list = CommandLineToArgvW(units.as_ptr(), &mut count);
                assert!(!list.is_null());
                (0..count as usize).map(|i| wide_at(*list.add(i))).collect::<Vec<_>>()
            }
        };
        let args: Vec<String> = [
            "plain",
            "two words",
            "",
            "tab\there",
            "a\"quote",
            "back\\slash",
            "trailing\\",
            "trailing two\\\\",
            "\\\"both\\\\\"",
            "--frames",
            "40",
            "unicode \u{e9}\u{4e2d}",
        ]
        .map(str::to_owned)
        .into();
        for program in ["game.exe", "my game.exe"] {
            let line = command_line(program, &args);
            let mut expected = vec![program.to_owned()];
            expected.extend(args.iter().cloned());
            assert_eq!(parsed(&line), expected, "{line}");
        }
    }

    fn w(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }

    #[cfg(windows)]
    #[test]
    fn host_lookups_by_name_get_our_overrides() {
        let kernel32 = GetModuleHandleW(w("kernel32.dll").as_ptr());
        assert!(!kernel32.is_null());
        let ours = crate::registry::lookup("kernel32.dll", "CreateFileW").unwrap();
        assert_eq!(
            GetProcAddress(kernel32, c"CreateFileW".as_ptr().cast()) as usize,
            ours
        );
        // A plain name stays the host's.
        let host = host::get_proc_address(kernel32, c"GetTickCount".as_ptr().cast());
        assert!(!host.is_null());
        assert_eq!(
            GetProcAddress(kernel32, c"GetTickCount".as_ptr().cast()),
            host
        );
        // Host DLLs outside the host policy do not exist for the title.
        assert!(LoadLibraryW(w("wininet.dll").as_ptr()).is_null());
        assert!(LoadLibraryA(c"C:\\Windows\\System32\\urlmon".as_ptr().cast()).is_null());
        assert!(GetModuleHandleW(w("WININET.DLL").as_ptr()).is_null());
        assert_eq!(
            // SAFETY: reads this thread's last error.
            unsafe { windows_sys::Win32::Foundation::GetLastError() },
            ERROR_MOD_NOT_FOUND
        );
        assert!(!LoadLibraryW(w("kernel32").as_ptr()).is_null());
        // By ordinal too (ws2_32 #4 is `connect`).
        let ws2 = LoadLibraryW(w("ws2_32.dll").as_ptr());
        assert_eq!(
            GetProcAddress(ws2, 4 as *const u8) as usize,
            crate::registry::lookup("ws2_32.dll", "connect").unwrap()
        );
    }

    #[test]
    fn title_dlls_and_pseudo_modules_are_found_by_name() {
        let base = 0x7100_0000_0000usize;
        runtime::set_image_path(base, std::path::Path::new(r"C:\title\Mapped.dll"));
        runtime::set_title_dll_loader(Box::new(|file| match file {
            "late.dll" => Ok(Some(0x7200_0000_0000)),
            "broken.dll" => Err("bad image".into()),
            _ => Ok(None),
        }));
        let mapped = base as *mut c_void;
        assert_eq!(GetModuleHandleW(w("MAPPED").as_ptr()), mapped);
        assert_eq!(GetModuleHandleA(c"sub\\mapped.dll".as_ptr().cast()), mapped);
        assert_eq!(LoadLibraryW(w("mapped.dll").as_ptr()), mapped);
        assert_eq!(FreeLibrary(mapped), 1, "title images stay loaded");

        // Not mapped yet: `LoadLibrary` maps it, `GetModuleHandle` does not.
        assert!(GetModuleHandleW(w("late.dll").as_ptr()).is_null());
        assert_eq!(
            LoadLibraryA(c"late".as_ptr().cast()) as usize,
            0x7200_0000_0000
        );
        assert!(LoadLibraryA(c"broken.dll".as_ptr().cast()).is_null());
        // As data it runs no code, so the host loads it (and finds nothing).
        let data = LoadLibraryExW(w("late.dll").as_ptr(), std::ptr::null_mut(), 0x2);
        assert_ne!(data as usize, 0x7200_0000_0000);

        let runtime = LoadLibraryA(c"xgameruntime.dll".as_ptr().cast());
        assert!(!runtime.is_null());
        assert_eq!(GetModuleHandleW(w("XGameRuntime.dll").as_ptr()), runtime);
    }

    /// No buffer: nothing written, the error says it was too small.
    #[test]
    fn a_title_image_name_into_no_buffer() {
        let base = 0x7300_0000_0000;
        runtime::set_image_path(base, std::path::Path::new(r"C:\title\game.exe"));
        let module = base as *mut c_void;
        assert_eq!(GetModuleFileNameW(module, std::ptr::null_mut(), 0), 0);
        assert_eq!(GetModuleFileNameA(module, std::ptr::null_mut(), 0), 0);
        #[cfg(windows)]
        {
            // SAFETY: reads this thread's last error.
            let last = unsafe { windows_sys::Win32::Foundation::GetLastError() };
            assert_eq!(last, ERROR_INSUFFICIENT_BUFFER);
        }
    }
}
