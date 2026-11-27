//! Title-side data that is not part of the platform: readers for container
//! formats titles ship their data in. Nothing here is about any one title:
//! no names, no hashes of executables or shaders, no tables of known
//! builds (docs/asset-policy.md). A run prints the BLAKE3 of the executable
//! it was given, computed on the spot; such hashes stay in the user's own
//! logs and are never committed.

pub mod sgd5;
