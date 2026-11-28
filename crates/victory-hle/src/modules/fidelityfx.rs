//! The title's side of the host upscaler (`victory_gpu::upscale`): shims
//! for the public FidelityFX API's entry points, matched by their exported
//! names only, from whichever DLL a title takes them.
//!
//! When a title module's import of `ffxCreateContext`, `ffxDestroyContext`
//! or `ffxDispatch` resolves to a DLL's export (at import binding,
//! [`Interposing`], or through `GetProcAddress`, `kernel::GetProcAddress`),
//! it is bound to the shim of that name instead, and the export is kept to
//! call. `ffxConfigure` and `ffxQuery` are left bound to the title's DLL:
//! every context is the title's own, so they need nothing of ours.
//!
//! - `ffxCreateContext` goes to the title's DLL first, always: the title's
//!   context is made as before. If that worked and the chain has an upscale
//!   description, the context is noted with its sizes; the host context is
//!   made at its first dispatch.
//! - `ffxDispatch` of an upscale (and only that) of a noted context becomes
//!   a step of ours in the title's command list (`victory_gpu::upscale::record`).
//!   Whatever the host cannot do (no DLL, a DLL refused, a context it could
//!   not make, a dispatch it cannot take, an error recording it) goes to the
//!   title's DLL, with one line in the log saying why.
//! - `ffxDestroyContext` lets go of the host context (once the card is done
//!   with it), then goes to the title's DLL.
//!
//! The first DLL whose export is bound is the one served; another DLL's
//! exports (a second copy of the API) stay bound as they are.
//! `VICTORY_HOST_UPSCALER=0`, or no upscaler DLL found, binds nothing.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering::*};

use victory_gpu::upscale::ffx::{self, Entry};
use victory_loader::{ImportResolver, Provenance, Resolution};
use victory_pe::ImportSymbol;

/// The title DLL's entry points we stand in for, by [`Entry`] (0: not bound).
static ORIGINAL: [AtomicUsize; 5] = [const { AtomicUsize::new(0) }; 5];

/// The module the entry points came from (0: none yet).
static PROVIDER: AtomicUsize = AtomicUsize::new(0);

/// What the shims know of a context of the title's.
enum Host {
    /// Created for upscaling; no host context made yet.
    Noted(victory_gpu::upscale::Sizes),
    #[cfg(windows)]
    Made(std::sync::Arc<victory_gpu::upscale::Context>),
    /// The title's own upscaler does its dispatches.
    Title,
}

/// The title's upscale contexts, by handle.
static CONTEXTS: Mutex<Option<HashMap<usize, Host>>> = Mutex::new(None);

fn contexts() -> std::sync::MutexGuard<'static, Option<HashMap<usize, Host>>> {
    CONTEXTS.lock().unwrap_or_else(|e| e.into_inner())
}

/// The shim for entry point `entry`, if there is one.
fn shim(entry: Entry) -> Option<usize> {
    match entry {
        Entry::CreateContext => Some(ffx_create_context as *const () as usize),
        Entry::DestroyContext => Some(ffx_destroy_context as *const () as usize),
        Entry::Dispatch => Some(ffx_dispatch as *const () as usize),
        Entry::Configure | Entry::Query => None,
    }
}

/// What a title's binding of export `name` at `address` (of the module
/// `module`) becomes: our shim, if it is one we stand in for, it is the
/// first module seen or that one, and the host upscaler is wanted; else
/// `address` unchanged.
pub fn interpose_from(name: &str, address: usize, module: usize, wanted: bool) -> usize {
    let Some(entry) = Entry::named(name) else { return address };
    let Some(shim) = shim(entry) else { return address };
    if !wanted || address == 0 || address == shim {
        return address;
    }
    match PROVIDER.compare_exchange(0, module, AcqRel, Acquire) {
        Ok(_) => {}
        Err(first) if first == module => {}
        Err(_) => {
            victory_gpu::upscale::fell_back("a second DLL with the FidelityFX entry points is left as it is");
            return address;
        }
    }
    match ORIGINAL[entry as usize].compare_exchange(0, address, AcqRel, Acquire) {
        Ok(_) => tracing::info!(name, "host upscaler: the title's FidelityFX entry point is bound to ours"),
        Err(kept) if kept == address => {}
        // The same module, another address (never seen): left as it is.
        Err(_) => return address,
    }
    shim
}

/// [`interpose_from`] for a binding made now: the module is found from the
/// address, and the host upscaler is wanted if it is on and a DLL was found.
pub fn interpose(name: &str, address: usize) -> usize {
    if Entry::named(name).and_then(shim).is_none() {
        return address;
    }
    let wanted = victory_gpu::upscale::interposing();
    interpose_from(name, address, module_of(address), wanted)
}

/// The base of the module `address` is in (the address itself if none).
fn module_of(address: usize) -> usize {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::LibraryLoader::{
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, GetModuleHandleExW,
        };
        let mut module = std::ptr::null_mut();
        // SAFETY: a look-up by address that takes no reference.
        let found = unsafe {
            GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                address as *const u16,
                &mut module,
            )
        };
        if found != 0 && !module.is_null() {
            return module as usize;
        }
    }
    address
}

/// Resolves through `inner`, then binds the FidelityFX entry points a title
/// DLL exports to our shims ([`interpose`]).
pub struct Interposing<'a> {
    pub inner: &'a dyn ImportResolver,
}

impl ImportResolver for Interposing<'_> {
    fn resolve(&self, dll: &str, symbol: &ImportSymbol) -> Resolution {
        let found = self.inner.resolve(dll, symbol);
        match (&found, symbol) {
            (Resolution::Found { address, provenance: Provenance::Title }, ImportSymbol::Name { name, .. }) => {
                let bound = interpose(name, *address);
                Resolution::Found {
                    address: bound,
                    provenance: if bound == *address { Provenance::Title } else { Provenance::Hle },
                }
            }
            _ => found,
        }
    }
}

/// The title's entry point `entry`.
fn original<T: Copy>(entry: Entry) -> Option<T> {
    let address = ORIGINAL[entry as usize].load(Acquire);
    // SAFETY: a function pointer type of the same size; only called with the
    // signature of that export.
    (address != 0).then(|| unsafe { std::mem::transmute_copy::<usize, T>(&address) })
}

/// `ffxCreateContext`: the title's DLL makes the context; one for upscaling
/// is noted for the host.
unsafe extern "system" fn ffx_create_context(
    context: *mut *mut c_void,
    desc: *mut ffx::Header,
    callbacks: *const c_void,
) -> u32 {
    let Some(create) = original::<ffx::CreateContext>(Entry::CreateContext) else {
        return ffx::RETURN_NO_PROVIDER;
    };
    // SAFETY: the title's own call, passed on as it is.
    let rc = unsafe { create(context, desc, callbacks) };
    if rc != ffx::RETURN_OK || context.is_null() {
        return rc;
    }
    // SAFETY: the description chain the title passed, read only.
    let upscale = unsafe { ffx::find(desc, ffx::CREATE_CONTEXT_DESC_TYPE_UPSCALE) };
    // SAFETY: the context the call wrote.
    let handle = unsafe { *context } as usize;
    if let Some(at) = upscale
        && handle != 0
    {
        // SAFETY: an upscale creation description, by its type.
        let up = unsafe { at.cast::<ffx::CreateUpscale>().read_unaligned() };
        let sizes = victory_gpu::upscale::Sizes::of(&up);
        contexts().get_or_insert_with(HashMap::new).insert(handle, Host::Noted(sizes));
    }
    rc
}

/// `ffxDestroyContext`: the host context goes too.
unsafe extern "system" fn ffx_destroy_context(context: *mut *mut c_void, callbacks: *const c_void) -> u32 {
    if !context.is_null() {
        // SAFETY: the title passes its context.
        let handle = unsafe { *context } as usize;
        // Dropped after the lock: the host context is retired.
        let gone = contexts().as_mut().and_then(|c| c.remove(&handle));
        drop(gone);
    }
    let Some(destroy) = original::<ffx::DestroyContext>(Entry::DestroyContext) else {
        return ffx::RETURN_NO_PROVIDER;
    };
    // SAFETY: the title's own call, passed on.
    unsafe { destroy(context, callbacks) }
}

/// `ffxDispatch`: an upscale of a context noted for the host is taken by
/// the host upscaler if it can be; anything else goes to the title's DLL.
unsafe extern "system" fn ffx_dispatch(context: *mut *mut c_void, desc: *const ffx::Header) -> u32 {
    let Some(dispatch) = original::<ffx::Dispatch>(Entry::Dispatch) else {
        return ffx::RETURN_NO_PROVIDER;
    };
    // SAFETY: the title's own call; read only.
    if unsafe { took(context, desc) } {
        return ffx::RETURN_OK;
    }
    // SAFETY: the title's own call, passed on.
    unsafe { dispatch(context, desc) }
}

/// Whether the host upscaler took the dispatch.
///
/// # Safety
/// The arguments of a title's `ffxDispatch`.
unsafe fn took(context: *mut *mut c_void, desc: *const ffx::Header) -> bool {
    if context.is_null() || desc.is_null() {
        return false;
    }
    // SAFETY: the title passes a description that starts with a header.
    if unsafe { desc.read_unaligned() }.kind != ffx::DISPATCH_DESC_TYPE_UPSCALE {
        return false;
    }
    // SAFETY: the title passes its context.
    let handle = unsafe { *context } as usize;
    // SAFETY: an upscale dispatch description, by its type.
    let d = unsafe { desc.cast::<ffx::DispatchUpscale>().read_unaligned() };
    let taken = take(handle, &d);
    if !taken {
        victory_gpu::upscale::title_dispatched();
    }
    taken
}

/// The host's side of one upscale dispatch of context `handle`.
#[cfg(windows)]
fn take(handle: usize, d: &ffx::DispatchUpscale) -> bool {
    let context = {
        let mut all = contexts();
        let Some(host) = all.as_mut().and_then(|c| c.get_mut(&handle)) else {
            return false;
        };
        match host {
            Host::Title => return false,
            Host::Made(c) => c.clone(),
            Host::Noted(sizes) => match victory_gpu::upscale::context(*sizes) {
                Ok(c) => {
                    *host = Host::Made(c.clone());
                    c
                }
                Err(why) => {
                    *host = Host::Title;
                    victory_gpu::upscale::fell_back(&format!("no host context: {why}"));
                    return false;
                }
            },
        }
    };
    if context.failed() {
        return false;
    }
    match victory_gpu::upscale::record(&context, d) {
        Ok(()) => true,
        Err(why) => {
            victory_gpu::upscale::fell_back(&why);
            false
        }
    }
}

#[cfg(not(windows))]
fn take(_: usize, _: &ffx::DispatchUpscale) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    // The shims and their tables are the process's: one test drives them.

    static TITLE_CALLS: [AtomicUsize; 3] = [const { AtomicUsize::new(0) }; 3];

    unsafe extern "system" fn title_create(context: *mut *mut c_void, _: *mut ffx::Header, _: *const c_void) -> u32 {
        TITLE_CALLS[0].fetch_add(1, SeqCst);
        // SAFETY: the test passes a place for the handle.
        unsafe { *context = 0x7777 as *mut c_void };
        ffx::RETURN_OK
    }
    unsafe extern "system" fn title_destroy(_: *mut *mut c_void, _: *const c_void) -> u32 {
        TITLE_CALLS[1].fetch_add(1, SeqCst);
        ffx::RETURN_OK
    }
    unsafe extern "system" fn title_dispatch(_: *mut *mut c_void, _: *const ffx::Header) -> u32 {
        TITLE_CALLS[2].fetch_add(1, SeqCst);
        ffx::RETURN_OK
    }

    /// Without a DLL nothing is bound; with one the three entry points are
    /// ours, a second module's stay its own; and with no host upscaler to
    /// take it (no DLL loaded here), every call reaches the title's DLL:
    /// the context it made, the upscale dispatch, the destruction.
    #[test]
    fn the_titles_own_dll_does_what_the_host_cannot() {
        let (create, destroy, dispatch) =
            (title_create as *const () as usize, title_destroy as *const () as usize, title_dispatch as *const () as usize);
        // Not wanted (switched off, or no DLL): unchanged, nothing noted.
        assert_eq!(interpose_from("ffxDispatch", dispatch, 0x1000, false), dispatch);
        assert_eq!(ORIGINAL[Entry::Dispatch as usize].load(SeqCst), 0);
        // Names we do not stand in for are never touched.
        assert_eq!(interpose_from("ffxQuery", 0x55, 0x1000, true), 0x55);
        assert_eq!(interpose_from("ffxConfigure", 0x56, 0x1000, true), 0x56);
        assert_eq!(interpose_from("SomethingElse", 0x57, 0x1000, true), 0x57);
        // Wanted: the shims.
        assert_eq!(interpose_from("ffxCreateContext", create, 0x1000, true), ffx_create_context as *const () as usize);
        assert_eq!(interpose_from("ffxDestroyContext", destroy, 0x1000, true), ffx_destroy_context as *const () as usize);
        assert_eq!(interpose_from("ffxDispatch", dispatch, 0x1000, true), ffx_dispatch as *const () as usize);
        // Bound twice (an import and a look-up): the same shim.
        assert_eq!(interpose_from("ffxDispatch", dispatch, 0x1000, true), ffx_dispatch as *const () as usize);
        // Another module's: its own.
        assert_eq!(interpose_from("ffxDispatch", 0x99, 0x2000, true), 0x99);

        let up_desc = ffx::CreateUpscale {
            header: ffx::Header { kind: ffx::CREATE_CONTEXT_DESC_TYPE_UPSCALE, next: std::ptr::null_mut() },
            flags: 0,
            max_render: ffx::Dimensions { width: 1280, height: 720 },
            max_upscale: ffx::Dimensions { width: 1920, height: 1080 },
            message: None,
        };
        let mut handle: *mut c_void = std::ptr::null_mut();
        // SAFETY: the shims' own signatures, with descriptions made here.
        unsafe {
            let header = (&raw const up_desc.header).cast_mut();
            assert_eq!(ffx_create_context(&mut handle, header, std::ptr::null()), ffx::RETURN_OK);
            assert_eq!(handle as usize, 0x7777);
            assert!(matches!(contexts().as_ref().and_then(|c| c.get(&0x7777)), Some(Host::Noted(_))));
            // An upscale dispatch the host cannot take (no upscaler DLL in a
            // test run, and no title list): the title's DLL does it.
            let d = ffx::DispatchUpscale {
                header: ffx::Header { kind: ffx::DISPATCH_DESC_TYPE_UPSCALE, next: std::ptr::null_mut() },
                command_list: std::ptr::null_mut(),
                color: ffx::Resource::default(),
                depth: ffx::Resource::default(),
                motion_vectors: ffx::Resource::default(),
                exposure: ffx::Resource::default(),
                reactive: ffx::Resource::default(),
                transparency_and_composition: ffx::Resource::default(),
                output: ffx::Resource::default(),
                jitter_offset: ffx::Coords::default(),
                motion_vector_scale: ffx::Coords::default(),
                render_size: ffx::Dimensions::default(),
                upscale_size: ffx::Dimensions::default(),
                enable_sharpening: 0,
                sharpness: 0.0,
                frame_time_delta: 0.0,
                pre_exposure: 1.0,
                reset: 0,
                camera_near: 0.1,
                camera_far: 100.0,
                camera_fov_angle_vertical: 1.0,
                view_space_to_meters_factor: 1.0,
                flags: 0,
            };
            assert_eq!(ffx_dispatch(&mut handle, &d.header), ffx::RETURN_OK);
            assert_eq!(TITLE_CALLS[2].load(SeqCst), 1);
            // From then on the context is the title's own upscaler's.
            assert!(matches!(contexts().as_ref().and_then(|c| c.get(&0x7777)), Some(Host::Title)));
            // Another kind of dispatch: the title's, always.
            let other = ffx::Header { kind: 0x0002_0001, next: std::ptr::null_mut() };
            assert_eq!(ffx_dispatch(&mut handle, &other), ffx::RETURN_OK);
            assert_eq!(TITLE_CALLS[2].load(SeqCst), 2);
            assert_eq!(ffx_destroy_context(&mut handle, std::ptr::null()), ffx::RETURN_OK);
            assert_eq!(TITLE_CALLS[1].load(SeqCst), 1);
            assert!(contexts().as_ref().is_none_or(|c| !c.contains_key(&0x7777)));
        }
        assert_eq!(TITLE_CALLS[0].load(SeqCst), 1);
    }
}
