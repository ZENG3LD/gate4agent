//! Native Gate4Agent node server and runtime.

mod bundle_catalog;

mod context_pack;

mod harness_mcp_proxy;

mod bundle_provider;

mod git_worktree;

mod host_directory_browser;

mod environment_profiles;

mod session_environment;

mod worktree_service;

mod session_registry;

mod standalone_workspace;

#[cfg(windows)]
mod workspace_file_windows;

#[cfg(unix)]
mod workspace_file_unix;

#[cfg(feature = "dig2-station-probe")]
mod dig2_station_lease;
#[cfg(feature = "dig2-station-probe")]
mod dig2_station_probe;
#[cfg(feature = "kit")]
mod kit;
#[cfg(feature = "wireguard")]
mod kit_wireguard;
mod network_allowlist_catalog;
mod platform;
#[cfg(feature = "kit")]
pub use kit::{force_link, linked_core_names, start_mailbox};
#[cfg(feature = "wireguard")]
pub use kit_wireguard::{
    bring_up_node_wireguard, node_wg_from_env, NodeWgPeerConfig, NodeWgPeerError,
    LINK_TAG as WIREGUARD_LINK_TAG, WG_ADDRESS_ENV, WG_INTERFACE_ENV, WG_KEEPALIVE_ENV,
    WG_LISTEN_PORT_ENV, WG_PEER_ADDRESS_ENV, WG_PEER_ENDPOINT_ENV, WG_PEER_KEY_ENV,
    WG_PRIVATE_KEY_ENV,
};
mod provider_runtime;
mod server;
mod spawn_spec;

#[cfg(feature = "fixture")]
pub use bundle_catalog::protect_bundle_source_tree_fixture;
pub use bundle_catalog::{
    BundleCatalog, BundleCatalogError, NodeBundle, NodeBundleError, NodeBundleFile,
    MAX_BUNDLE_CATALOG_ENTRIES, MAX_BUNDLE_FILES, MAX_BUNDLE_FILE_BYTES, MAX_BUNDLE_PATH_BYTES,
    MAX_BUNDLE_TOTAL_BYTES,
};
pub use environment_profiles::{
    NodeEnvironmentProfile, NodeEnvironmentProfileError, MAX_NODE_ENVIRONMENT_PROFILES,
};
pub use gate4agent_node_protocol::WorktreeServiceMode;
pub use gate4agent_runtime_native::{
    orca_home_roots, HistorySourceLayout, NativeHistoryConfig, NativeHistoryRoot,
};
pub use network_allowlist_catalog::{
    claude_allowed_domains_from_permits, claude_bash_sandbox_network_os_supported,
    claude_settings_network_overlay_args, claude_station_network_settings_json,
    codex_network_access_config_overlay, load_network_allowlist_catalog_file,
    parse_network_allowlist_catalog_text, permit_peer_to_allowed_domain,
    provider_native_mapping_supported, resolve_network_allowlist_catalog,
    resolve_provider_native_launch_overlay, NetworkAllowlistCatalog, NetworkAllowlistCatalogError,
    NetworkAllowlistEntry, NetworkPermitProtocol, NetworkPermitSketch, ProviderNativeNetworkSketch,
    CLAUDE_STATION_NETWORK_SETTINGS_FILE, MAX_NETWORK_ALLOWLIST_CATALOG_ENTRIES,
    MAX_NETWORK_PERMITS_PER_ENTRY, NETWORK_ALLOWLIST_CATALOG_ENV,
    NETWORK_ALLOWLIST_CATALOG_SCHEMA_VERSION,
};
#[cfg(feature = "fixture")]
pub use server::SpawnManagedWorktreeV2FailureProbe;
pub use server::{
    default_node_endpoint, default_state_path, NodeServer, NodeServerConfig, NodeServerError,
    NodeShutdownHandle, WorkspaceConfig,
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
pub use spawn_spec::{
    SpawnProfileRegistry, SpawnProfileRegistryError, DEFAULT_SPAWN_PROFILE_ID, MAX_SPAWN_PROFILES,
};
pub use worktree_service::ManagedWorktreeProfile;

#[cfg(windows)]
pub use server::DEFAULT_NODE_ENDPOINT;

pub use gate4agent_node_protocol as protocol;

#[cfg(test)]
mod kit_manifest {
    #[test]
    fn default_features_mention_four_cores_and_bare_excludes_them() {
        let manifest = include_str!("../Cargo.toml");
        let features = manifest
            .split("[features]")
            .nth(1)
            .expect("features section")
            .split("\n[")
            .next()
            .expect("features body");
        assert!(
            features.contains("default = [\"kit\"]"),
            "default build must include the kit"
        );
        let kit = feature_array(features, "kit");
        for core in [
            "dep:dig2browser",
            "dep:mail4agent",
            "dep:claude-session-restore",
            "dep:codex-session-restore",
            "dep:grok-session-restore",
            "dep:kimi-session-restore",
            "wireguard",
        ] {
            assert!(
                kit.iter().any(|item| item == core),
                "kit feature missing {core}: {kit:?}"
            );
        }
        let bare = feature_array(features, "bare");
        assert!(
            bare.is_empty(),
            "bare must not enable kit cores, got {bare:?}"
        );
        let code = manifest
            .lines()
            .map(|line| line.split('#').next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !code.contains("hatchery-tui") && !code.contains("hatchery_tui"),
            "node kit must not depend on the hatchery TUI"
        );
        assert!(manifest.contains("path = \"../../../dig2browser\""));
        assert!(manifest.contains("path = \"../../../mail4agent\""));
        assert!(manifest.contains("claude-session-restore = { version = \"0.1.3\""));
        assert!(manifest.contains("codex-session-restore = { version = \"0.1.3\""));
        assert!(manifest.contains("grok-session-restore"));
        assert!(manifest.contains("kimi-session-restore"));
    }

    fn feature_array(features: &str, name: &str) -> Vec<String> {
        let line_start = features
            .find(&format!("{name} = "))
            .unwrap_or_else(|| panic!("missing feature {name}"));
        let rest = &features[line_start + name.len() + 3..];
        if rest.starts_with("[]") {
            return Vec::new();
        }
        assert!(rest.starts_with("["), "{name} must be an array");
        let end = rest.find(']').unwrap_or_else(|| panic!("{name} array"));
        rest[1..end]
            .split(',')
            .map(|item| item.trim().trim_matches('"').to_owned())
            .filter(|item| !item.is_empty())
            .collect()
    }
}


#[cfg(test)]
mod deploy_artifacts {
    //! Static checks for the three kit deploy modes. No Docker build, no
    //! QEMU boot, no network. Lives in `--lib` so the ordinary node test
    //! cycle refuses missing or gutted deploy entry points.

    #[test]
    fn service_unit_is_mode1_kit_entry() {
        let unit = include_str!("../deploy/node/gate4agent-node.service");
        assert!(unit.contains("ExecStart=/usr/local/bin/gate4agent-node"));
        assert!(unit.contains("gate4agent node (standard kit)"));
        assert!(
            !unit.contains("INSTALL_PROVIDER_CLIS"),
            "mode 1 unit must not install provider CLIs"
        );
        assert!(
            unit.contains("GATE4AGENT_WG_INTERFACE") || unit.contains("WireGuard"),
            "unit must document optional WG env"
        );
    }

    #[test]
    fn dockerfile_and_build_script_are_mode2() {
        let dockerfile = include_str!("../deploy/node/Dockerfile");
        assert!(dockerfile.contains("cargo build --locked --release -p gate4agent-node"));
        assert!(dockerfile.contains("ENTRYPOINT [\"gate4agent-node\"]"));
        assert!(dockerfile.contains("COPY dig2browser"));
        assert!(dockerfile.contains("COPY mail4agent"));
        assert!(dockerfile.contains("COPY session-restore"));
        assert!(
            !dockerfile.contains("GATE4AGENT_NODE_TOKEN"),
            "image must not bake a node token"
        );
        let build = include_str!("../deploy/node/docker-build.sh");
        assert!(build.contains("docker build"));
        assert!(build.contains("gate4agent-node:kit"));
        assert!(build.contains("copy_tree \"$WS/dig2browser\""));
        assert!(build.contains("copy_tree \"$WS/mail4agent\""));
        assert!(build.contains("copy_tree \"$WS/session-restore\""));
    }

    #[test]
    fn qemu_guest_script_is_mode3_underlay_only() {
        let script = include_str!("../deploy/node/qemu-guest.sh");
        assert!(script.contains("Mode 3"));
        assert!(script.contains("QEMU_NET"));
        assert!(script.contains("tap") && script.contains("user"));
        assert!(
            script.contains("Kernel WireGuard is not a QEMU network"),
            "script must keep WG out of the QEMU NIC story"
        );
        assert!(script.contains("GATE4AGENT_WG_"));
        assert!(script.contains("gate4agent-node"));
        // Guest boot stays opt-in: this test only reads the script text.
        assert!(
            !script.contains("cargo test"),
            "qemu guest script must not be wired into cargo test"
        );
    }

    #[test]
    fn deploy_readme_names_three_modes() {
        let readme = include_str!("../deploy/node/README.md");
        assert!(readme.contains("Mode 1 — service"));
        assert!(readme.contains("Mode 2 — container"));
        assert!(readme.contains("Mode 3 — QEMU guest"));
        assert!(readme.contains("--no-default-features --features bare"));
    }
}
