//! Native Gate4Agent node server and runtime.

mod bundle_catalog;

mod context_pack;

mod bundle_provider;

mod git_worktree;

mod environment_profiles;

mod session_environment;

mod worktree_service;

mod session_registry;

#[cfg(windows)]
mod workspace_file_windows;

#[cfg(unix)]
mod workspace_file_unix;

mod platform;
mod provider_runtime;
mod spawn_spec;
mod server;

pub use server::{
    default_node_endpoint, default_state_path, NodeServer, NodeServerConfig, NodeServerError,
    NodeShutdownHandle, WorkspaceConfig, WorktreeServiceMode,
};
pub use bundle_catalog::{
    BundleCatalog, BundleCatalogError, NodeBundle, NodeBundleError, NodeBundleFile,
    MAX_BUNDLE_CATALOG_ENTRIES, MAX_BUNDLE_FILES, MAX_BUNDLE_FILE_BYTES,
    MAX_BUNDLE_PATH_BYTES, MAX_BUNDLE_TOTAL_BYTES,
};
#[cfg(feature = "fixture")]
pub use bundle_catalog::protect_bundle_source_tree_fixture;
pub use environment_profiles::{
    NodeEnvironmentProfile, NodeEnvironmentProfileError, MAX_NODE_ENVIRONMENT_PROFILES,
};
pub use session_environment::{
    NodeSecretReference, NodeSecretResolveError, NodeSecretResolver, NodeSecretValue,
    NodeSecretValueError, NodeSessionEnvironmentMutation, NodeSessionFile,
    NodeSessionMaterializationProfile, NodeSessionMaterializationProfileError,
    NodeSessionPathBinding, NodeSessionPathClass, MAX_NODE_SECRET_REFERENCE_BYTES,
    MAX_NODE_SECRET_VALUE_BYTES, MAX_SESSION_ENVIRONMENT_ENTRIES,
    MAX_SESSION_MATERIALIZATION_FILES, MAX_SESSION_MATERIALIZATION_FILE_BYTES,
    MAX_SESSION_MATERIALIZATION_RELATIVE_PATH_BYTES,
};
pub use worktree_service::ManagedWorktreeProfile;
pub use spawn_spec::{
    SpawnProfileRegistry, SpawnProfileRegistryError, DEFAULT_SPAWN_PROFILE_ID,
    MAX_SPAWN_PROFILES,
};
pub use gate4agent_runtime_native::{
    HistorySourceLayout, NativeHistoryConfig, NativeHistoryRoot,
};

#[cfg(windows)]
pub use server::DEFAULT_NODE_ENDPOINT;

pub use gate4agent_node_protocol as protocol;
