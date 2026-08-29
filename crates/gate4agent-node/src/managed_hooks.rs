//! Installs Gate4Agent's managed provider Hook configuration for the
//! active fleet at node startup, and retracts only the published ingress
//! endpoint at shutdown.
//!
//! This is an observability channel, not a runtime dependency for any
//! provider session: every generated script reads
//! `GATE4AGENT_HOOK_URL`/`_TOKEN`/`_ROUTE` from the process environment at
//! invocation time and exits `0` silently whenever any of them is absent.
//! A provider with no node running behind it, or with a session that never
//! received a route, is simply an inert no-op on every Hook call -- that is
//! what makes installing this once at every node startup and leaving it
//! behind across restarts correct rather than invasive.
//!
//! Nothing here may fail node startup. Roots that cannot be resolved, or a
//! plan/apply failure for one provider, are logged with the provider named
//! and the cause, and never stop another provider from being attempted.

use gate4agent_adapters::ManagedHookConfigLocation;
use gate4agent_runtime_native::HookIngressEndpoint;
use gate4agent_shell_managed_hooks::{ManagedHookManager, ManagedHookOperation, ManagedHookRoots};
use gate4agent_types::{AdapterBinding, AgentSpec, RuntimePlatform};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// Resolves [`ManagedHookRoots`] from the process environment and builds a
/// [`ManagedHookManager`] from them. Returns `None` and logs a warning when
/// any required root cannot be resolved -- that is always a reason to skip
/// managed hook installation, never a reason to fail node startup.
pub(crate) fn resolve_manager_from_environment() -> Option<ManagedHookManager> {
    match ManagedHookManager::new(roots_from_environment()) {
        Ok(manager) => Some(manager),
        Err(error) => {
            tracing::warn!(
                error = %error,
                "managed provider hook roots are unavailable; skipping managed hook installation"
            );
            None
        }
    }
}

fn roots_from_environment() -> ManagedHookRoots {
    ManagedHookRoots {
        home: configured_home_directory().unwrap_or_default(),
        runtime_data: configured_runtime_data_directory().unwrap_or_default(),
        app_data: configured_app_data_directory(),
        platform: RuntimePlatform::current(),
        system_root: configured_system_root(),
        environment_homes: configured_environment_homes(),
    }
}

#[cfg(windows)]
fn configured_home_directory() -> Option<PathBuf> {
    absolute_env_path("USERPROFILE")
}

#[cfg(not(windows))]
fn configured_home_directory() -> Option<PathBuf> {
    absolute_env_path("HOME")
}

// Same standard runtime-state location the node already resolves its own
// endpoint (Unix) and state path (Windows) from -- see `platform.rs`. No
// spec currently roots a file here; this exists only so `ManagedHookRoots`
// stays fully populated and `validate()` never has an unnecessary reason
// to reject it.
#[cfg(windows)]
fn configured_runtime_data_directory() -> Option<PathBuf> {
    absolute_env_path("LOCALAPPDATA")
}

#[cfg(not(windows))]
fn configured_runtime_data_directory() -> Option<PathBuf> {
    absolute_env_path("XDG_RUNTIME_DIR").or_else(|| Some(std::env::temp_dir()))
}

#[cfg(windows)]
fn configured_app_data_directory() -> Option<PathBuf> {
    absolute_env_path("APPDATA")
}

#[cfg(not(windows))]
fn configured_app_data_directory() -> Option<PathBuf> {
    None
}

#[cfg(windows)]
fn configured_system_root() -> Option<PathBuf> {
    absolute_env_path("SystemRoot")
}

#[cfg(not(windows))]
fn configured_system_root() -> Option<PathBuf> {
    None
}

fn absolute_env_path(variable: &str) -> Option<PathBuf> {
    std::env::var_os(variable)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

/// Per-provider home overrides declared by the managed-hook spec table
/// (`GROK_HOME`, `KIMI_CODE_HOME`, ...), read from the process environment
/// where set. A variable left unset here is left absent from the map, so
/// the owning spec's own home-relative fallback applies untouched.
fn configured_environment_homes() -> BTreeMap<String, PathBuf> {
    gate4agent_adapters::managed_hook_specs()
        .iter()
        .filter_map(|spec| match spec.config_location {
            ManagedHookConfigLocation::EnvironmentHome { variable, .. } => Some(variable),
            _ => None,
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter_map(|variable| Some((variable.to_owned(), absolute_env_path(variable)?)))
        .collect()
}

/// Publishes the current loopback ingress endpoint, then installs
/// Gate4Agent's managed Hook configuration for every provider in `fleet`
/// whose spec declares a `managed_hook` adapter binding. A provider without
/// that binding is left untouched entirely.
///
/// Each provider is planned and applied independently -- a failure for one
/// is logged with its target name and cause and never prevents the
/// remaining providers from being attempted. Planning is idempotent: once a
/// provider's configuration already matches, `ManagedHookPlan::is_noop`
/// short-circuits and nothing is written, so re-running this at every node
/// startup is a warm no-op.
pub(crate) fn install_fleet<'a>(
    manager: &ManagedHookManager,
    fleet: impl Iterator<Item = &'a AgentSpec>,
    endpoint: &HookIngressEndpoint,
) {
    if let Err(error) = manager.publish_ingress_endpoint(endpoint) {
        tracing::warn!(
            error = %error,
            "failed to publish the managed hook ingress endpoint; provider hook scripts stay inert"
        );
        return;
    }
    for spec in fleet {
        let Some(binding) = spec.capabilities.adapters.managed_hook.as_ref() else {
            continue;
        };
        install_one(manager, spec.id.as_str(), binding);
    }
}

fn install_one(manager: &ManagedHookManager, target: &str, binding: &AdapterBinding) {
    let plan = match manager.plan(binding, ManagedHookOperation::Install) {
        Ok(plan) => plan,
        Err(error) => {
            tracing::warn!(
                provider = target,
                error = %error,
                "failed to plan managed provider hook installation"
            );
            return;
        }
    };
    if plan.is_noop() {
        return;
    }
    if let Err(error) = manager.apply(plan) {
        tracing::warn!(
            provider = target,
            error = %error,
            "failed to install managed provider hook"
        );
    }
}

/// Removes only the published ingress endpoint file at node shutdown. The
/// provider-side Hook configuration and generated scripts are left in
/// place on purpose: they are already inert without a live endpoint, so
/// uninstalling them on every stop would churn the user's provider config
/// for no gain.
pub(crate) fn remove_published_endpoint(manager: &ManagedHookManager) {
    if let Err(error) = manager.remove_published_ingress_endpoint() {
        tracing::warn!(
            error = %error,
            "failed to remove the published managed hook ingress endpoint"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gate4agent_catalog::{builtin_registry, AgentRegistry};
    use gate4agent_runtime_native::{HookIngressConfig, NativeRuntime, NativeRuntimeConfig};
    use gate4agent_types::AgentId;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "gate4agent-node-managed-hooks-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn test_manager(root: &TestRoot) -> ManagedHookManager {
        ManagedHookManager::new(ManagedHookRoots {
            home: root.0.join("home"),
            runtime_data: root.0.join("runtime"),
            app_data: Some(root.0.join("app-data")),
            platform: RuntimePlatform::Linux,
            system_root: None,
            environment_homes: BTreeMap::new(),
        })
        .unwrap()
    }

    fn fleet_spec(id: &str) -> AgentSpec {
        builtin_registry()
            .get(&AgentId::new(id).unwrap())
            .unwrap()
            .clone()
    }

    async fn test_endpoint() -> (NativeRuntime, HookIngressEndpoint) {
        let registry = AgentRegistry::new([]).unwrap();
        let (_, mut runtime) = NativeRuntime::new(registry, NativeRuntimeConfig::default());
        let endpoint = runtime
            .start_hook_ingress(HookIngressConfig::default())
            .await
            .unwrap();
        (runtime, endpoint)
    }

    #[tokio::test]
    async fn installation_is_attempted_only_for_fleet_providers_that_declare_a_managed_hook_binding(
    ) {
        let root = TestRoot::new();
        let manager = test_manager(&root);
        let (mut runtime, endpoint) = test_endpoint().await;

        let with_binding = fleet_spec("codex");
        assert!(with_binding.capabilities.adapters.managed_hook.is_some());
        let mut without_binding = fleet_spec("grok");
        without_binding.capabilities.adapters.managed_hook = None;

        let fleet = [with_binding, without_binding];
        install_fleet(&manager, fleet.iter(), &endpoint);

        assert!(root.0.join("home/.codex/hooks.json").exists());
        assert!(!root.0.join("home/.grok").exists());

        runtime.stop_hook_ingress().await;
    }

    #[tokio::test]
    async fn a_failure_installing_one_provider_does_not_prevent_the_others_and_never_propagates() {
        let root = TestRoot::new();
        let manager = test_manager(&root);
        let (mut runtime, endpoint) = test_endpoint().await;

        let broken_config = root.0.join("home/.codex/hooks.json");
        fs::create_dir_all(broken_config.parent().unwrap()).unwrap();
        fs::write(&broken_config, b"not json").unwrap();

        let fleet = [fleet_spec("codex"), fleet_spec("claude")];
        // `install_fleet` returns `()`: a per-provider failure cannot
        // propagate out of this call by construction. The assertions below
        // pin the other half -- that codex's failure did not stop claude.
        install_fleet(&manager, fleet.iter(), &endpoint);

        assert_eq!(fs::read(&broken_config).unwrap(), b"not json");
        assert!(root.0.join("home/.claude/settings.json").exists());

        runtime.stop_hook_ingress().await;
    }

    #[tokio::test]
    async fn reinstalling_an_already_installed_provider_is_a_no_op() {
        let root = TestRoot::new();
        let manager = test_manager(&root);
        let (mut runtime, endpoint) = test_endpoint().await;

        let fleet = [fleet_spec("codex")];
        install_fleet(&manager, fleet.iter(), &endpoint);
        let config_path = root.0.join("home/.codex/hooks.json");
        let installed = fs::read(&config_path).unwrap();

        let binding = fleet[0]
            .capabilities
            .adapters
            .managed_hook
            .clone()
            .unwrap();
        let plan = manager
            .plan(&binding, ManagedHookOperation::Install)
            .unwrap();
        assert!(plan.is_noop());

        install_fleet(&manager, fleet.iter(), &endpoint);
        assert_eq!(fs::read(&config_path).unwrap(), installed);

        runtime.stop_hook_ingress().await;
    }

    #[tokio::test]
    async fn shutdown_removes_the_published_endpoint_and_leaves_provider_configs_in_place() {
        let root = TestRoot::new();
        let manager = test_manager(&root);
        let (mut runtime, endpoint) = test_endpoint().await;

        let fleet = [fleet_spec("codex")];
        install_fleet(&manager, fleet.iter(), &endpoint);
        let config_path = root.0.join("home/.codex/hooks.json");
        assert!(config_path.exists());

        remove_published_endpoint(&manager);

        assert!(!root
            .0
            .join("home/.gate4agent/agent-hooks/endpoint.env")
            .exists());
        assert!(!root
            .0
            .join("home/.gate4agent/agent-hooks/endpoint.cmd")
            .exists());
        assert!(config_path.exists());

        runtime.stop_hook_ingress().await;
    }
}
