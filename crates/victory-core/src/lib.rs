//! Shared plumbing for every Rox-box crate and tool.
//!
//! * [`config`]: where the user's files live (never inside the repository).
//! * [`policy`]: the runtime half of rule #1, refusing to write
//!   game-derived data into the source tree.
//! * [`logging`]: one tracing setup for all binaries.
//! * [`report`]: the shareable run report, fed by every crate (victory-gpu
//!   reaches it through `victory_pipeline::report`, victory-hle through
//!   `victory_loader::report`, their paths to this crate).

pub mod config;
pub mod logging;
pub mod policy;
pub mod report;

pub use config::Config;
pub use policy::{PolicyError, ensure_outside_repo};
