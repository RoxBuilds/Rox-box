//! `demo_late.dll`: the DLL the open demo title loads by name
//! (`LoadLibraryW`) and calls through `GetProcAddress`; the same code as
//! `demo_static.dll` (`demo-static-dll/src/dll.rs` says what it records).

#[path = "../../demo-static-dll/src/dll.rs"]
mod dll;
