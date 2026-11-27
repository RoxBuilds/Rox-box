//! The platform calls the demo makes beyond its graphics, as a title links
//! them: `kernel32` (bound to the host's, but for the module lookups,
//! `LoadLibraryW`, `GetProcAddress`, `FreeLibrary` and `ExitProcess`, which
//! are Rox-box's), the C runtime's thread calls (`api-ms-win-crt-runtime`)
//! and `ntdll`'s process end.

use std::ffi::{c_char, c_void};

pub type Handle = *mut c_void;
pub type ThreadStart = unsafe extern "system" fn(*mut c_void) -> u32;

pub const INFINITE: u32 = u32::MAX;
pub const WAIT_OBJECT_0: u32 = 0;
pub const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
pub const GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS: u32 = 4;
pub const GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT: u32 = 2;

#[link(name = "kernel32")]
unsafe extern "system" {
    pub fn CreateThread(
        security: *const c_void,
        stack: usize,
        start: ThreadStart,
        parameter: *mut c_void,
        flags: u32,
        id: *mut u32,
    ) -> Handle;
    pub fn ExitThread(code: u32) -> !;
    pub fn GetExitCodeThread(thread: Handle, code: *mut u32) -> i32;
    pub fn GetCurrentThreadId() -> u32;
    pub fn CreateEventW(security: *const c_void, manual: i32, initial: i32, name: *const u16) -> Handle;
    pub fn SetEvent(event: Handle) -> i32;
    pub fn WaitForSingleObject(handle: Handle, milliseconds: u32) -> u32;
    pub fn CloseHandle(handle: Handle) -> i32;
    pub fn LoadLibraryW(name: *const u16) -> Handle;
    pub fn GetProcAddress(module: Handle, name: *const c_char) -> *const c_void;
    pub fn FreeLibrary(module: Handle) -> i32;
    pub fn FreeLibraryAndExitThread(module: Handle, code: u32) -> !;
    pub fn GetModuleHandleW(name: *const u16) -> Handle;
    pub fn GetModuleHandleA(name: *const c_char) -> Handle;
    pub fn GetModuleHandleExW(flags: u32, name: *const u16, module: *mut Handle) -> i32;
    pub fn GetModuleFileNameW(module: Handle, out: *mut u16, size: u32) -> u32;
    pub fn GetStdHandle(which: u32) -> Handle;
    pub fn WriteFile(
        file: Handle,
        data: *const u8,
        len: u32,
        written: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
}

// The C runtime's threads (`api-ms-win-crt-runtime`).
unsafe extern "C" {
    pub fn _beginthreadex(
        security: *mut c_void,
        stack: u32,
        start: ThreadStart,
        parameter: *mut c_void,
        flags: u32,
        id: *mut u32,
    ) -> usize;
    pub fn _endthreadex(code: u32) -> !;
}

#[link(name = "ntdll")]
unsafe extern "system" {
    /// The process's end with nothing of the C runtime's or Rox-box's on
    /// the way: what the demo's own check at its exit uses to fail.
    pub fn NtTerminateProcess(process: Handle, status: u32) -> i32;
}

/// A wide, NUL-terminated copy of `text`.
pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

/// An event; manual-reset if `manual`, not set.
pub fn event(manual: bool) -> Handle {
    // SAFETY: a fresh unnamed event.
    unsafe { CreateEventW(std::ptr::null(), i32::from(manual), 0, std::ptr::null()) }
}

/// A module's file name (`GetModuleFileNameW`), `None` if it fails.
pub fn module_file(module: Handle) -> Option<String> {
    let mut out = [0u16; 1024];
    // SAFETY: a buffer of its length.
    let n = unsafe { GetModuleFileNameW(module, out.as_mut_ptr(), out.len() as u32) } as usize;
    (n > 0 && n < out.len()).then(|| String::from_utf16_lossy(&out[..n]))
}

/// `text` written straight to standard output: no lock of the C runtime's
/// or Rust's, for the demo's last words, said while the process ends (its
/// other threads ended wherever they were).
pub fn say_raw(text: &str) {
    // SAFETY: the process's standard output handle and a buffer of `len`
    // bytes.
    unsafe {
        let out = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut written = 0;
        WriteFile(out, text.as_ptr(), text.len() as u32, &mut written, std::ptr::null_mut());
    }
}
