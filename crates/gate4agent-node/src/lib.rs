//! Native Gate4Agent node server and runtime.

mod git_worktree;

mod session_registry;

#[cfg(windows)]
mod workspace_file_windows;

#[cfg(unix)]
mod workspace_file_unix;

mod platform;
mod provider_runtime;
mod server;

pub use server::{
    default_node_endpoint, default_state_path, NodeServer, NodeServerConfig, NodeServerError,
    NodeShutdownHandle, WorkspaceConfig,
};

#[cfg(windows)]
pub use server::DEFAULT_NODE_ENDPOINT;

pub use gate4agent_node_protocol as protocol;
