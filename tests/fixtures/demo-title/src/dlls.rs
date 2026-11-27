//! The title's own DLLs (docs/demo-title.md, "Its DLLs"): `demo_static.dll`,
//! which it imports, so the loader maps it beside the executable, binds it
//! and runs its `DllMain` before the entry point; and `demo_late.dll`,
//! which nothing imports and the title loads by name (`LoadLibraryW`),
//! calls through `GetProcAddress` and frees. Both are the same code
//! (`tests/fixtures/demo-static-dll/src/dll.rs`), which notes what its
//! `DllMain` hears; the structures here are its.

use std::ffi::c_void;

use crate::win::{self, Handle};

/// `demo-static-dll`'s `Counts`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Counts {
    pub process_attaches: u32,
    pub process_detaches: u32,
    pub thread_attaches: u32,
    pub thread_detaches: u32,
    pub attached_on: u32,
    pub twice: u32,
    pub overflow: u32,
    pub _pad: u32,
    pub module: usize,
}

impl Counts {
    pub const fn zero() -> Counts {
        Counts {
            process_attaches: 0,
            process_detaches: 0,
            thread_attaches: 0,
            thread_detaches: 0,
            attached_on: 0,
            twice: 0,
            overflow: 0,
            _pad: 0,
            module: 0,
        }
    }
}

/// `demo-static-dll`'s `Work`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Work {
    pub token: u32,
    pub attaches: u32,
    pub calls: u32,
    pub tls_ok: u32,
    pub thread_id: u32,
    pub _pad: u32,
}

/// `demo-static-dll`'s `Notes`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Notes {
    pub attaches: u32,
    pub detaches: u32,
    pub drops: u32,
}

#[link(name = "demo_static", kind = "raw-dylib")]
unsafe extern "system" {
    fn demo_dll_counts(out: *mut Counts);
    fn demo_dll_report_to(out: *mut Counts);
    fn demo_dll_notes(token: u32, out: *mut Notes);
    fn demo_dll_work(out: *mut Work) -> u32;
    fn demo_dll_own_path(out: *mut u16, size: u32) -> u32;
}

type CountsFn = unsafe extern "system" fn(*mut Counts);
type NotesFn = unsafe extern "system" fn(u32, *mut Notes);
type WorkFn = unsafe extern "system" fn(*mut Work) -> u32;
type PathFn = unsafe extern "system" fn(*mut u16, u32) -> u32;

/// One of the DLLs, by its exports.
#[derive(Clone, Copy)]
pub struct Dll {
    pub name: &'static str,
    counts: CountsFn,
    report_to: CountsFn,
    notes: NotesFn,
    work: WorkFn,
    own_path: PathFn,
}

pub const STATIC: &str = "demo_static.dll";
pub const LATE: &str = "demo_late.dll";
/// A copy of `demo_late.dll`'s file under another name: one DLL a title
/// ships twice must be two modules, as on Windows.
pub const TWIN: &str = "demo_twin.dll";

impl Dll {
    /// `demo_static.dll`, through the executable's imports.
    pub fn imported() -> Dll {
        Dll {
            name: STATIC,
            counts: demo_dll_counts,
            report_to: demo_dll_report_to,
            notes: demo_dll_notes,
            work: demo_dll_work,
            own_path: demo_dll_own_path,
        }
    }

    /// `name` (`demo_late.dll`, `demo_twin.dll`), loaded by name, and its
    /// exports looked up.
    pub fn load(name: &'static str) -> Result<(Dll, Handle), String> {
        let wide = win::wide(name);
        // SAFETY: a NUL-terminated name.
        let module = unsafe { win::LoadLibraryW(wide.as_ptr()) };
        if module.is_null() {
            return Err(format!("LoadLibraryW({name}) failed"));
        }
        let find = |export: &std::ffi::CStr| -> Result<*const c_void, String> {
            // SAFETY: a module handle and a NUL-terminated name.
            let at = unsafe { win::GetProcAddress(module, export.as_ptr()) };
            if at.is_null() {
                Err(format!("GetProcAddress({name}, {export:?}) found nothing"))
            } else {
                Ok(at)
            }
        };
        // SAFETY: `GetProcAddress` gave each export of the DLL, whose
        // signatures are these (`demo-static-dll/src/dll.rs`).
        let dll = unsafe {
            Dll {
                name,
                counts: std::mem::transmute::<*const c_void, CountsFn>(find(c"demo_dll_counts")?),
                report_to: std::mem::transmute::<*const c_void, CountsFn>(find(c"demo_dll_report_to")?),
                notes: std::mem::transmute::<*const c_void, NotesFn>(find(c"demo_dll_notes")?),
                work: std::mem::transmute::<*const c_void, WorkFn>(find(c"demo_dll_work")?),
                own_path: std::mem::transmute::<*const c_void, PathFn>(find(c"demo_dll_own_path")?),
            }
        };
        if find(c"demo_dll_no_such_export").is_ok() {
            return Err(format!("GetProcAddress({name}) found an export the DLL does not have"));
        }
        Ok((dll, module))
    }

    pub fn counts(&self) -> Counts {
        let mut counts = Counts::zero();
        // SAFETY: the export, given a `Counts` to fill.
        unsafe { (self.counts)(&mut counts) };
        counts
    }

    /// Asks the DLL to write its notes into `place` at its detach.
    pub fn report_to(&self, place: *mut Counts) {
        // SAFETY: the export; `place` is the title's, for the process's life.
        unsafe { (self.report_to)(place) };
    }

    pub fn notes(&self, token: u32) -> Notes {
        let mut notes = Notes::default();
        // SAFETY: the export, given a `Notes` to fill.
        unsafe { (self.notes)(token, &mut notes) };
        notes
    }

    /// One call of the function the title calls from its threads.
    pub fn work(&self) -> Work {
        let mut work = Work::default();
        // SAFETY: the export, given a `Work` to fill.
        unsafe { (self.work)(&mut work) };
        work
    }

    /// What the DLL's own `GetModuleFileNameW` says it is.
    pub fn own_path(&self) -> Option<String> {
        let mut out = [0u16; 1024];
        // SAFETY: the export, with a buffer of its length.
        let n = unsafe { (self.own_path)(out.as_mut_ptr(), out.len() as u32) } as usize;
        (n > 0 && n < out.len()).then(|| String::from_utf16_lossy(&out[..n]))
    }

    /// The address of its function, for the module that holds it.
    fn code(&self) -> *const u16 {
        self.work as *const u16
    }
}

/// Before the title's first use of `dll` (a fresh load of it, or the
/// imported one at the title's start): its process attach ran once, on
/// `thread` (the one that loaded it: the title's main thread), and no
/// detach.
pub fn attached_before_use(dll: &Dll, thread: u32) -> Result<Counts, String> {
    let c = dll.counts();
    let name = dll.name;
    if c.process_attaches != 1 {
        return Err(format!("{name} heard of {} process attaches before its first use, not 1", c.process_attaches));
    }
    if c.process_detaches != 0 || c.twice != 0 {
        return Err(format!("{name} heard of its process's end before its first use"));
    }
    if c.attached_on != thread {
        return Err(format!("{name}'s process attach ran on another thread than the one that loaded it"));
    }
    if c.module == 0 {
        return Err(format!("{name}'s DllMain was given no instance handle"));
    }
    Ok(c)
}

/// The module that holds `code` (`GetModuleHandleExW`, the host's).
fn module_of(code: *const u16) -> Handle {
    let mut module = std::ptr::null_mut();
    // SAFETY: looks the address up, taking no reference.
    unsafe {
        win::GetModuleHandleExW(
            win::GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | win::GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            code,
            &mut module,
        )
    };
    module
}

fn by_name(name: &str) -> Handle {
    let name = win::wide(name);
    // SAFETY: a NUL-terminated name.
    unsafe { win::GetModuleHandleW(name.as_ptr()) }
}

fn folder(path: &str) -> &str {
    path.rsplit_once('\\').map_or("", |(folder, _)| folder)
}

fn file(path: &str) -> &str {
    path.rsplit_once('\\').map_or(path, |(_, file)| file)
}

/// What a title sees of its images by name and by handle: each found by
/// its file name (any case, with or without `.dll`), by the address of its
/// code, and as `DllMain`'s instance handle, all one module; and each named
/// by `GetModuleFileNameW` beside the executable, the same whether the
/// executable or the DLL itself asks. Returns what the names say of the
/// host (they are its full paths).
pub fn names(statics: (&Dll, &Counts), late: (&Dll, &Counts, Handle)) -> Result<&'static str, String> {
    let exe = by_name_null();
    let own_code = by_name_null as fn() -> Handle as *const u16;
    if exe.is_null() || module_of(own_code) != exe || by_name("demo-title.exe") != exe {
        return Err("GetModuleHandleW names another module than the executable's for it".into());
    }
    let (s, s_counts) = statics;
    let s_module = s_counts.module as Handle;
    let upper = c"DEMO_STATIC";
    // SAFETY: a NUL-terminated name.
    let s_upper = unsafe { win::GetModuleHandleA(upper.as_ptr()) };
    if by_name(STATIC) != s_module || s_upper != s_module || module_of(s.code()) != s_module {
        return Err(format!("{STATIC} is not one module by its name, its code and its DllMain"));
    }
    let (l, l_counts, l_module) = late;
    if l_counts.module as Handle != l_module || by_name(LATE) != l_module || module_of(l.code()) != l_module {
        return Err(format!("{LATE} is not one module by LoadLibraryW, its name, its code and its DllMain"));
    }
    let exe_path = win::module_file(std::ptr::null_mut()).ok_or("GetModuleFileNameW gave no name for the executable")?;
    if !file(&exe_path).eq_ignore_ascii_case("demo-title.exe") {
        return Err("GetModuleFileNameW names another file than the executable".into());
    }
    for (dll, module) in [(s, s_module), (l, l_module)] {
        let name = dll.name;
        let path = win::module_file(module).ok_or_else(|| format!("GetModuleFileNameW gave no name for {name}"))?;
        if !file(&path).eq_ignore_ascii_case(name) || !folder(&path).eq_ignore_ascii_case(folder(&exe_path)) {
            return Err(format!("GetModuleFileNameW does not name {name} beside the executable"));
        }
        if dll.own_path().as_deref() != Some(path.as_str()) {
            return Err(format!("{name}'s own GetModuleFileNameW names another file than the title's does"));
        }
    }
    let bytes = exe_path.as_bytes();
    let full = bytes.len() > 3 && bytes[0].is_ascii_alphabetic() && bytes[1..3] == *b":\\";
    Ok(if full {
        "full paths of the host's folder (a drive letter, then the folders above the game folder)"
    } else {
        "paths that are not a host drive's"
    })
}

fn by_name_null() -> Handle {
    // SAFETY: the process's own module.
    unsafe { win::GetModuleHandleW(std::ptr::null()) }
}

/// `FreeLibrary` on a DLL loaded by name (`dll`, from `LoadLibraryW` as
/// `module`) the title no longer calls, which writes what it heard into
/// `place` at its detach: where its process detach comes. Windows runs it
/// at the last `FreeLibrary` and unmaps the DLL; a runtime that keeps the
/// DLL runs it at the process's end. Either way it must come once (also
/// checked at the end, `tls::at_exit`). Whether the DLL is gone.
pub fn free(dll: &Dll, module: Handle, place: &crate::tls::Ending) -> Result<bool, String> {
    let name = dll.name;
    let before = dll.counts();
    // SAFETY: the module `LoadLibraryW` gave, which the title no longer uses.
    if unsafe { win::FreeLibrary(module) } == 0 {
        return Err(format!("FreeLibrary({name}) failed"));
    }
    if by_name(name).is_null() {
        // Gone: what it said at its detach is in the title's memory.
        let detaches = place.read().process_detaches;
        if detaches != 1 {
            return Err(format!("{name} was unloaded by FreeLibrary with {detaches} process detaches, not 1"));
        }
        return Ok(true);
    }
    let after = dll.counts();
    if after.process_detaches != before.process_detaches {
        return Err(format!("{name} heard of its detach at FreeLibrary but stayed loaded"));
    }
    Ok(false)
}

/// After a thread let go of its own reference with
/// `FreeLibraryAndExitThread` (`threads.rs`): the title still holds one, so
/// the DLL must still be there, attached.
pub fn still_held(dll: &Dll) -> Result<(), String> {
    if by_name(dll.name).is_null() || dll.counts().process_detaches != 0 {
        return Err(format!("{} went away at a thread's FreeLibraryAndExitThread, though the title still holds it", dll.name));
    }
    Ok(())
}

/// `demo_twin.dll`, a copy of the late DLL's file: loaded, it must be a
/// module of its own (its own base, its own `DllMain`'s first process
/// attach, its own statics), not the late DLL again; then freed.
pub fn twin(late: (&Dll, Handle)) -> Result<&'static str, String> {
    let (twin, module) = Dll::load(TWIN)?;
    // SAFETY: a plain query of the calling thread.
    let me = unsafe { win::GetCurrentThreadId() };
    if module == late.1 || by_name(TWIN) == late.1 {
        return Err(format!("{TWIN}, a copy of {LATE} under another name, is the same module as {LATE}"));
    }
    attached_before_use(&twin, me)?;
    twin.report_to(crate::tls::TWIN_AT_EXIT.place());
    Ok(if free(&twin, module, &crate::tls::TWIN_AT_EXIT)? {
        "a module of its own, freed and unloaded"
    } else {
        "a module of its own, left loaded by FreeLibrary"
    })
}
