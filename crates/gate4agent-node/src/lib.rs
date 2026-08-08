//! Native Gate4Agent node server and runtime.

#[cfg(windows)]
mod git_worktree;

#[cfg(windows)]
mod session_registry;

#[cfg(windows)]
mod windows;

#[cfg(windows)]
pub use windows::{
    default_state_path, NodeServer, NodeServerConfig, NodeServerError, NodeShutdownHandle,
    WorkspaceConfig, DEFAULT_NODE_ENDPOINT,
};

pub use gate4agent_node_protocol as protocol;
