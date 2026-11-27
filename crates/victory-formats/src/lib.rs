//! Platform-level data formats, shared by every title of the console.
//!
//! Each module is pure (bytes in, bytes out, no I/O): [`texture`] (the
//! GPU's tiled texture layouts and copies to and from linear rows), [`bcpack`]
//! (BCPack-compressed textures), [`xma`] (the XMA2 audio codec).

pub mod bcpack;
pub mod texture;
pub mod xma;
