//! Transport core of the gate4agent node. Product code lives above this crate.

pub mod managed_worktree;
pub mod materialization;
pub mod drive_loop_profile;
pub mod platform;
pub mod session_registry;
pub mod spawn_spec;
#[cfg(feature = "wireguard")]
pub mod kit_wireguard;

pub use gate4agent_node_protocol as protocol;
