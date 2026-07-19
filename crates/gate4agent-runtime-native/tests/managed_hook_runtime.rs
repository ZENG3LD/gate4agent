use gate4agent_catalog::AgentRegistry;
use gate4agent_runtime_native::{HookIngressConfig, NativeRuntime, NativeRuntimeConfig};
use gate4agent_shell_managed_hooks::{ManagedHookManager, ManagedHookRoots};
use gate4agent_types::RuntimePlatform;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

struct TestRoot(PathBuf);

impl TestRoot {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "gate4agent-managed-endpoint-{}-{nonce}",
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

#[tokio::test]
async fn explicit_endpoint_publication_rotates_only_gate4agent_ingress_authority() {
    let registry = AgentRegistry::new([]).unwrap();
    let (_, mut runtime) = NativeRuntime::new(registry, NativeRuntimeConfig::default());
    let root = TestRoot::new();
    let manager = ManagedHookManager::new(ManagedHookRoots {
        home: root.0.join("home"),
        runtime_data: root.0.join("runtime"),
        app_data: Some(root.0.join("app-data")),
        platform: RuntimePlatform::Linux,
        system_root: None,
        environment_homes: BTreeMap::new(),
    })
    .unwrap();

    let first = runtime
        .start_hook_ingress(HookIngressConfig::default())
        .await
        .unwrap();
    let first_token = first.authorization_token().to_owned();
    let paths = manager.publish_ingress_endpoint(&first).unwrap();
    let first_file = fs::read_to_string(&paths.posix_path).unwrap();
    assert!(first_file.contains("Managed by Gate4Agent"));
    assert!(first_file.contains(&format!("GATE4AGENT_HOOK_PORT={}", first.port())));
    assert!(first_file.contains(&format!("GATE4AGENT_HOOK_TOKEN={first_token}")));
    assert!(!first_file.to_ascii_lowercase().contains("api_key"));

    runtime.stop_hook_ingress().await;
    let second = runtime
        .start_hook_ingress(HookIngressConfig::default())
        .await
        .unwrap();
    assert_ne!(second.authorization_token(), first_token);
    manager.publish_ingress_endpoint(&second).unwrap();
    let second_file = fs::read_to_string(&paths.posix_path).unwrap();
    assert!(!second_file.contains(&first_token));
    assert!(second_file.contains(second.authorization_token()));
    assert!(!PathBuf::from(format!("{}.bak", paths.posix_path.display())).exists());

    manager.remove_published_ingress_endpoint().unwrap();
    assert!(!paths.posix_path.exists());
    assert!(!paths.windows_path.exists());
    runtime.stop_hook_ingress().await;
}
