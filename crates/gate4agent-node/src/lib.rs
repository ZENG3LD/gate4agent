//! Native Gate4Agent node and its reusable local client.

#[cfg(windows)]
mod git_worktree;

#[cfg(windows)]
mod windows;

#[cfg(windows)]
pub use windows::{
    NamedPipeNodeClient, NodeClientError, NodeServer, NodeServerConfig, NodeServerError,
    NodeShutdownHandle, WorkspaceConfig,
};

pub use gate4agent_node_protocol as protocol;
