//! Transport layer types for spawning CLI agent processes.
//!
//! Phase 2 deliverable: `SpawnOptions` struct used by per-CLI `build_command`
//! functions. The full `TransportSession` unified entry point arrives in Phase 5.

mod options;

pub use options::SpawnOptions;
