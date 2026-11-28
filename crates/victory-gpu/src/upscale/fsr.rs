//! The first host backend: AMD FSR 3.1 through the PC FidelityFX API DLL the
//! user supplies ([`super::find::DLL_NAME`]), loaded at run time. Only its
//! five exported calls are used, with descriptions laid out as in
//! [`super::ffx`].

use std::ffi::{CStr, c_void};
use std::path::Path;

use super::ffx;

/// The loaded DLL's entry points, and the upscaler version it offers.
pub(crate) struct Fsr {
    create: ffx::CreateContext,
    destroy: ffx::DestroyContext,
    dispatch: ffx::Dispatch,
    pub(crate) version: String,
}

/// A context of the DLL's, on our host device.
pub(crate) struct FsrContext {
    fsr: &'static Fsr,
    handle: usize,
}

/// The oldest upscaler this layout of the API is known for (FSR 3.1).
const OLDEST: (u32, u32) = (3, 1);

/// Whether a version name (`"3.1.4"`, as the DLL gives it) is one this
/// layout is known for: 3.1 and later.
pub(crate) fn version_fits(name: &str) -> bool {
    let mut parts = name.trim().split('.').map(|p| p.trim().parse::<u32>());
    match (parts.next(), parts.next()) {
        (Some(Ok(major)), Some(Ok(minor))) => (major, minor) >= OLDEST,
        _ => false,
    }
}

impl Fsr {
    /// Loads the DLL at `path`, checks its exports and asks which upscaler
    /// versions it has for `device`. The DLL is never let go: contexts may
    /// live to the end of the run.
    pub(crate) fn load(path: &Path, device: *mut c_void) -> Result<Fsr, String> {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LOAD_WITH_ALTERED_SEARCH_PATH, LoadLibraryExW};
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: a NUL-terminated path; the user put this DLL there for us
        // to load (docs/upscaling.md). Its own folder is searched for what it
        // needs.
        let module = unsafe { LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), LOAD_WITH_ALTERED_SEARCH_PATH) };
        if module.is_null() {
            return Err(format!("it does not load ({})", std::io::Error::last_os_error()));
        }
        let export = |name: &CStr| {
            // SAFETY: a loaded module and a NUL-terminated name.
            unsafe { GetProcAddress(module, name.as_ptr().cast()) }
                .ok_or_else(|| format!("it has no export {}", name.to_string_lossy()))
        };
        type Proc = unsafe extern "system" fn() -> isize;
        // SAFETY: the exports of the FidelityFX API have these signatures.
        let (create, destroy, dispatch, query) = unsafe {
            (
                std::mem::transmute::<Proc, ffx::CreateContext>(export(c"ffxCreateContext")?),
                std::mem::transmute::<Proc, ffx::DestroyContext>(export(c"ffxDestroyContext")?),
                std::mem::transmute::<Proc, ffx::Dispatch>(export(c"ffxDispatch")?),
                std::mem::transmute::<Proc, ffx::Query>(export(c"ffxQuery")?),
            )
        };
        export(c"ffxConfigure")?;
        let versions = versions(query, device)?;
        let Some(version) = versions.iter().find(|v| version_fits(v)).cloned() else {
            return Err(format!(
                "its upscaler is not FSR {}.{} or later (it offers {})",
                OLDEST.0,
                OLDEST.1,
                if versions.is_empty() { "none".to_owned() } else { versions.join(", ") }
            ));
        };
        Ok(Fsr { create, destroy, dispatch, version })
    }

    /// A context for upscales of at most `max_render` to at most
    /// `max_upscale`, with the title's `flags` (those [`ffx::UPSCALE_CREATE_FLAGS_KEPT`]).
    pub(crate) fn context(
        &'static self,
        device: *mut c_void,
        flags: u32,
        max_render: ffx::Dimensions,
        max_upscale: ffx::Dimensions,
    ) -> Result<FsrContext, String> {
        let mut backend = ffx::CreateBackendDx12 {
            header: ffx::Header { kind: ffx::CREATE_CONTEXT_DESC_TYPE_BACKEND_DX12, next: std::ptr::null_mut() },
            device,
        };
        let mut desc = ffx::CreateUpscale {
            header: ffx::Header { kind: ffx::CREATE_CONTEXT_DESC_TYPE_UPSCALE, next: &raw mut backend.header },
            flags: flags & ffx::UPSCALE_CREATE_FLAGS_KEPT,
            max_render,
            max_upscale,
            message: Some(message),
        };
        let mut handle: *mut c_void = std::ptr::null_mut();
        // SAFETY: a chain of two descriptions alive for the call; no
        // allocation callbacks (the DLL's own allocator).
        let rc = unsafe { (self.create)(&mut handle, &raw mut desc.header, std::ptr::null()) };
        if rc != ffx::RETURN_OK || handle.is_null() {
            return Err(format!("creating the context returned {rc}"));
        }
        Ok(FsrContext { fsr: self, handle: handle as usize })
    }
}

/// The upscaler versions the DLL offers for `device`, by name.
fn versions(query: ffx::Query, device: *mut c_void) -> Result<Vec<String>, String> {
    let mut count = 0u64;
    let mut desc = ffx::QueryVersions {
        header: ffx::Header { kind: ffx::QUERY_DESC_TYPE_GET_VERSIONS, next: std::ptr::null_mut() },
        create_kind: ffx::CREATE_CONTEXT_DESC_TYPE_UPSCALE,
        device,
        count: &mut count,
        ids: std::ptr::null_mut(),
        names: std::ptr::null_mut(),
    };
    // SAFETY: the query of versions takes no context; first the count.
    let rc = unsafe { query(std::ptr::null_mut(), &raw mut desc.header) };
    if rc != ffx::RETURN_OK {
        return Err(format!("it answers no upscaler versions ({rc})"));
    }
    let n = count.min(64) as usize;
    let mut ids = vec![0u64; n];
    let mut names = vec![std::ptr::null::<std::ffi::c_char>(); n];
    count = n as u64;
    desc.count = &mut count;
    desc.ids = ids.as_mut_ptr();
    desc.names = names.as_mut_ptr();
    // SAFETY: arrays of `count` entries.
    let rc = unsafe { query(std::ptr::null_mut(), &raw mut desc.header) };
    if rc != ffx::RETURN_OK {
        return Err(format!("it answers no upscaler versions ({rc})"));
    }
    Ok(names
        .iter()
        .take(count.min(n as u64) as usize)
        .filter(|p| !p.is_null())
        // SAFETY: NUL-terminated names the DLL keeps.
        .map(|&p| unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
        .collect())
}

impl FsrContext {
    /// Records the upscale `desc` (on host resources, `command_list` our
    /// host list) into that list.
    pub(crate) fn dispatch(&self, desc: &ffx::DispatchUpscale) -> Result<(), String> {
        let mut handle = self.handle as *mut c_void;
        // SAFETY: our context, and a description whose resources are live
        // host resources and whose list is open.
        let rc = unsafe { (self.fsr.dispatch)(&mut handle, &desc.header) };
        if rc == ffx::RETURN_OK { Ok(()) } else { Err(format!("the dispatch returned {rc}")) }
    }
}

impl Drop for FsrContext {
    fn drop(&mut self) {
        let mut handle = self.handle as *mut c_void;
        // SAFETY: our context, let go of once; what the card still runs of it
        // has finished (it is dropped through `crate::host::retire`).
        let rc = unsafe { (self.fsr.destroy)(&mut handle, std::ptr::null()) };
        if rc != ffx::RETURN_OK {
            tracing::warn!(rc, "host upscaler: destroying a context failed");
        }
    }
}

/// Messages of the DLL (with debug checking, which is not asked for: only
/// errors it reports anyway). The first few are logged.
unsafe extern "system" fn message(kind: u32, text: *const u16) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SAID: AtomicU32 = AtomicU32::new(0);
    if text.is_null() || SAID.fetch_add(1, Ordering::Relaxed) >= 20 {
        return;
    }
    // SAFETY: a NUL-terminated wide string for the call.
    let text = unsafe {
        let len = (0..4096).take_while(|&i| *text.add(i) != 0).count();
        String::from_utf16_lossy(std::slice::from_raw_parts(text, len))
    };
    if kind == 0 {
        tracing::warn!(%text, "host upscaler: error message");
    } else {
        tracing::info!(%text, "host upscaler: message");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_from_fsr_3_1_on_fit() {
        assert!(version_fits("3.1.4"));
        assert!(version_fits("3.1"));
        assert!(version_fits(" 4.0.2 "));
        assert!(!version_fits("3.0.3"));
        assert!(!version_fits("2.2.1"));
        assert!(!version_fits("three"));
        assert!(!version_fits(""));
    }

    /// A file that is not a library is refused with a reason, and nothing
    /// else happens (the fallback: the title's own upscaler stays).
    #[test]
    fn a_file_that_is_not_a_library_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = tmp.path().join(super::super::find::DLL_NAME);
        std::fs::write(&fake, b"not a real library").unwrap();
        let why = Fsr::load(&fake, std::ptr::null_mut()).err().expect("refused");
        assert!(why.contains("does not load"), "{why}");
        let why = Fsr::load(&tmp.path().join("absent.dll"), std::ptr::null_mut()).err().expect("refused");
        assert!(why.contains("does not load"), "{why}");
    }
}
