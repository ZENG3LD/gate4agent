//! Native Gate4Agent node server and runtime.

#[cfg(windows)]
mod git_worktree;

#[cfg(windows)]
mod windows;

#[cfg(windows)]
pub use windows::{
    NodeServer, NodeServerConfig, NodeServerError, NodeShutdownHandle, WorkspaceConfig,
};

pub use gate4agent_node_protocol as protocol;
