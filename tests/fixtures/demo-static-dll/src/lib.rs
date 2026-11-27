//! `demo_static.dll`: the DLL the open demo title imports, so the loader
//! maps it beside the executable as a title's own DLL and binds the
//! title's imports of it (`src/dll.rs` says what it records).

mod dll;
