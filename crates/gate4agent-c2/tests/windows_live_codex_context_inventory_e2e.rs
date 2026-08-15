#![cfg(windows)]

use gate4agent_c2::protocol::{
    C2ManagedSessionRecord, C2NodeEvent, C2NodeResponse, C2NodeSnapshot, C2SessionStatus,
    NodeId, NodeRoute, NodeTransportState,
};
use gate4agent_c2::{C2Config, C2NodeConfig, C2Running, C2Timings};
use gate4agent_c2_client::{connect_local, C2Client, C2ControlHandle};
use gate4agent_node::protocol::{
    AdapterId, CapabilityId, ManagedSessionState, ManagedWorktreeLeaseState,
    ManagedWorktreeRetention, ManagedWorktreeSpawnRequest, NodeRequest, SessionAddress,
    SessionMode, SpawnDeadlineMs, SpawnFieldProvenance, SpawnIdempotencyKey, SpawnOverride,
    SpawnOverrides, SpawnPrompt, SpawnEnvironmentProfileId, SpawnEnvironmentProfileRevision,
    SpawnProfileDefaults, SpawnProfileId, SpawnProfileRevision, SpawnRequiredCapabilities,
    SpawnSpec, SpawnTarget, WorktreeProfileId, WorktreeProfileRevision, WorkspaceId,
    SPAWN_RUNTIME_STRUCTURED_PROMPT,
};
use gate4agent_node::{
    HistorySourceLayout, ManagedWorktreeProfile, NativeHistoryConfig, NativeHistoryRoot,
    NodeEnvironmentProfile, NodeSecretReference, NodeSecretResolveError, NodeSecretResolver,
    NodeSecretValue, NodeServer, NodeServerConfig, NodeSessionFile,
    NodeSessionMaterializationProfile, NodeSessionPathBinding, NodeSessionPathClass,
    SpawnProfileRegistry, WorkspaceConfig, WorktreeServiceMode,
};
use gate4agent_catalog::EnvMutation;
use gate4agent_runtime_native::{
    NativeChildEnvironmentResolveError, NativeChildEnvironmentResolver, NativeLaunchProfile,
    NativeLaunchProfileId, OneShotSessionPersistence,
};
use gate4agent_types::{AgentId, TerminalSize, TransportKind};
use serde::Deserialize;
use serde_json::Value;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::{sleep, timeout};

const LIVE_CANARY_ENV: &str = "GATE4AGENT_VENDOR_CANARY";

const SOURCE_CODEX_HOME_KEY: &str = "CODEX_HOME";
const TARGET_CANARY_KEY: &str = "GATE4AGENT_LIVE_CONTEXT_CANARY";
const TARGET_AUTH_REFERENCE: &str = "live-codex-auth";
const TARGET_CONFIG_REFERENCE: &str = "live-codex-config";

struct InMemorySecretResolver {
    auth: Arc<Vec<u8>>,
    config: Option<Arc<Vec<u8>>>,
}

impl NodeSecretResolver for InMemorySecretResolver {
    fn resolve(
        &self,
        reference: &NodeSecretReference,
    ) -> Result<NodeSecretValue, NodeSecretResolveError> {
        match reference.as_str() {
            TARGET_AUTH_REFERENCE => NodeSecretValue::bytes(self.auth.as_ref().clone())
                .map_err(|_| NodeSecretResolveError::Denied),
            TARGET_CONFIG_REFERENCE => self
                .config
                .as_ref()
                .ok_or(NodeSecretResolveError::Unavailable)
                .and_then(|bytes| {
                    NodeSecretValue::bytes(bytes.as_ref().clone())
                        .map_err(|_| NodeSecretResolveError::Denied)
                }),
            _ => Err(NodeSecretResolveError::Unavailable),
        }
    }
}

struct StaticInlineEnvironmentResolver {
    key: OsString,
    value: OsString,
}

impl NativeChildEnvironmentResolver for StaticInlineEnvironmentResolver {
    fn resolve_child_environment(
        &self,
    ) -> Result<Vec<EnvMutation>, NativeChildEnvironmentResolveError> {
        Ok(vec![EnvMutation {
            key: self.key.clone(),
            value: Some(self.value.clone()),
        }])
    }
}

struct RemoveTestRoot(PathBuf);

impl Drop for RemoveTestRoot {
    fn drop(&mut self) {
        let safe = self
            .0
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("gate4agent-live-codex-context-c2-"));
        if safe {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

struct SecretFileWiper {
    paths: Vec<PathBuf>,
    armed: bool,
}

impl SecretFileWiper {
    fn new(paths: Vec<PathBuf>) -> Self {
        assert!(!paths.is_empty() && paths.len() <= 2);
        for (index, path) in paths.iter().enumerate() {
            assert!(path.is_absolute());
            assert!(matches!(
                path.file_name().and_then(|name| name.to_str()),
                Some("auth.json") | Some("config.toml")
            ));
            assert!(!paths[..index].contains(path));
        }
        Self { paths, armed: true }
    }

    fn wipe_one(path: &Path) -> std::io::Result<()> {
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        if metadata.is_file() && !metadata.file_type().is_symlink() {
            let overwrite = (|| {
                let mut file = std::fs::OpenOptions::new().write(true).open(path)?;
                let mut remaining = metadata.len().min(256 * 1024) as usize;
                let zeros = [0_u8; 4_096];
                while remaining > 0 {
                    let chunk = remaining.min(zeros.len());
                    file.write_all(&zeros[..chunk])?;
                    remaining -= chunk;
                }
                file.flush()?;
                file.sync_all()?;
                file.set_len(0)?;
                Ok::<(), std::io::Error>(())
            })();
            let removed = std::fs::remove_file(path);
            overwrite?;
            return removed;
        }
        std::fs::remove_file(path)
    }

    fn wipe_and_disarm(&mut self) -> std::io::Result<()> {
        for path in &self.paths {
            Self::wipe_one(path)?;
            if path.exists() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "secret file remained after cleanup",
                ));
            }
        }
        self.armed = false;
        Ok(())
    }
}

impl Drop for SecretFileWiper {
    fn drop(&mut self) {
        if self.armed {
            for path in &self.paths {
                let _ = Self::wipe_one(path);
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SemanticProof {
    history_fact: String,
    commit_summary: String,
    repository_note: String,
    status_path: String,
}

fn require_live_canary() -> bool {
    if std::env::var(LIVE_CANARY_ENV).ok().as_deref() != Some("1") {
        eprintln!("skipped: set {LIVE_CANARY_ENV}=1 to run authenticated Codex acceptance");
        return false;
    }
    assert_eq!(
        std::env::var_os("GATE4AGENT_HEADLESS_SUPERVISOR").as_deref(),
        Some(std::ffi::OsStr::new("1")),
        "Windows vendor canaries must run through windows-headless-supervisor",
    );
    unsafe {
        const SEM_FAILCRITICALERRORS: u32 = 0x0001;
        const SEM_NOGPFAULTERRORBOX: u32 = 0x0002;
        const SEM_NOOPENFILEERRORBOX: u32 = 0x8000;
        const WER_FAULT_REPORTING_NO_UI: u32 = 0x0020;

        #[link(name = "kernel32")]
        extern "system" {
            fn SetErrorMode(mode: u32) -> u32;
            fn WerSetFlags(flags: u32) -> i32;
        }

        SetErrorMode(
            SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX,
        );
        let _ = WerSetFlags(WER_FAULT_REPORTING_NO_UI);
    }
    true
}

fn agent(value: &str) -> AgentId {
    AgentId::new(value).unwrap()
}

fn adapter(value: &str) -> AdapterId {
    AdapterId::new(value).unwrap()
}

fn endpoint(label: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!(
        r"\\.\pipe\gate4agent-live-codex-context-{label}-{}-{nonce}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    )
}

fn test_root() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "gate4agent-live-codex-context-c2-{}-{nonce}",
        std::process::id(),
    ))
}

fn remove_test_root_explicitly(root: &Path) {
    assert!(
        root.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("gate4agent-live-codex-context-c2-")),
        "explicit cleanup root identity is invalid",
    );
    let canonical_root = std::fs::canonicalize(root).expect("test root is unavailable at cleanup");
    let canonical_temp = std::fs::canonicalize(std::env::temp_dir())
        .expect("temporary root is unavailable at cleanup");
    assert!(
        canonical_root.starts_with(&canonical_temp) && canonical_root != canonical_temp,
        "explicit cleanup root escaped the temporary root",
    );
    std::fs::remove_dir_all(&canonical_root).expect("explicit test-root cleanup failed");
    assert!(!root.exists(), "explicit test-root cleanup left the root present");
}

fn nonce_label() -> String {
    format!(
        "{}{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    )
}

fn codex_home() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(|home| PathBuf::from(home).join(".codex")))
        .expect("Codex home is unavailable")
}

fn read_sensitive_file(path: &Path, required: bool) -> Option<Vec<u8>> {
    match std::fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(error) if !required && error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => panic!("required Codex credential/config file is unavailable: {error}"),
    }
}

fn write(path: &Path, contents: impl AsRef<[u8]>) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

fn protect_owner_tree(root: &Path) {
    fn collect(path: &Path, paths: &mut Vec<PathBuf>) {
        let metadata = std::fs::symlink_metadata(path).unwrap();
        assert!(!metadata.file_type().is_symlink(), "protected tree contains a link");
        if metadata.is_dir() {
            for entry in std::fs::read_dir(path).unwrap() {
                collect(&entry.unwrap().path(), paths);
            }
        }
        paths.push(path.to_path_buf());
    }

    let identity = Command::new("whoami").output().unwrap();
    assert!(identity.status.success(), "whoami failed for owner ACL setup");
    let principal = String::from_utf8(identity.stdout).unwrap().trim().to_owned();
    assert!(!principal.is_empty(), "whoami returned an empty owner");
    let mut paths = Vec::new();
    collect(root, &mut paths);
    for path in paths {
        let status = Command::new("icacls")
            .arg(path)
            .arg("/inheritance:r")
            .arg("/grant:r")
            .arg(format!("{principal}:(OI)(CI)(F)"))
            .arg("/q")
            .status()
            .unwrap();
        assert!(status.success(), "owner-only ACL setup failed");
    }
}

fn target_materialization_profile(has_config: bool) -> NodeSessionMaterializationProfile {
    let mut files = vec![NodeSessionFile::secret(
        NodeSessionPathClass::ProviderHome,
        "auth.json",
        NodeSecretReference::new(TARGET_AUTH_REFERENCE).unwrap(),
    )
    .unwrap()];
    if has_config {
        files.push(
            NodeSessionFile::secret(
                NodeSessionPathClass::ProviderHome,
                "config.toml",
                NodeSecretReference::new(TARGET_CONFIG_REFERENCE).unwrap(),
            )
            .unwrap(),
        );
    }
    NodeSessionMaterializationProfile::new(
        Vec::new(),
        vec![NodeSessionPathBinding::new(
            SOURCE_CODEX_HOME_KEY,
            NodeSessionPathClass::ProviderHome,
        )
        .unwrap()],
        files,
    )
    .unwrap()
}

fn git_output(root: &Path, arguments: &[&str]) -> std::process::Output {
    Command::new("git")
        .arg("--no-pager")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .output()
        .unwrap()
}

fn assert_git_success(root: &Path, arguments: &[&str]) {
    let output = git_output(root, arguments);
    assert!(
        output.status.success(),
        "fixture Git command failed",
    );
}

fn commit_all(root: &Path, summary: &str) {
    assert_git_success(root, &["add", "--all"]);
    assert_git_success(
        root,
        &[
            "-c",
            "user.name=Gate4Agent Fixture",
            "-c",
            "user.email=fixture@gate4agent.invalid",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "core.hooksPath=NUL",
            "commit",
            "--quiet",
            "-m",
            summary,
        ],
    );
}

fn initialize_repository(root: &Path, repository_note: &str, commit_summary: &str) {
    let output = Command::new("git")
        .args(["init", "-b", "main"])
        .arg(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "fixture Git repository initialization failed",
    );
    write(
        &root.join("README.md"),
        format!("# Context fixture\n\nREPOSITORY_NOTE={repository_note}\n"),
    );
    commit_all(root, commit_summary);
}

fn assert_target_repository_excludes(root: &Path, forbidden: &[&str]) {
    let files = git_output(root, &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(files.status.success(), "target tree inventory failed");
    assert!(
        String::from_utf8(files.stdout).unwrap().lines().eq(["README.md"]),
        "target repository contains an unexpected tracked file",
    );
    let log = git_output(root, &["log", "--all", "--format=%H%n%B"]);
    assert!(log.status.success(), "target Git log read failed");
    let readme = git_output(root, &["show", "HEAD:README.md"]);
    assert!(readme.status.success(), "target Git object read failed");
    let log = String::from_utf8(log.stdout).unwrap();
    let readme = String::from_utf8(readme.stdout).unwrap();
    for value in forbidden {
        assert!(
            !log.contains(value) && !readme.contains(value),
            "target Git objects contain a source-only expected value",
        );
    }
}

fn direct_child_directories(root: &Path) -> Vec<PathBuf> {
    if !root.exists() {
        return Vec::new();
    }
    let mut directories = std::fs::read_dir(root)
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_type().ok().filter(|kind| kind.is_dir()).map(|_| entry.path()))
        .collect::<Vec<_>>();
    directories.sort();
    directories
}

fn node_config(
    endpoint: &str,
    token: &str,
    node_id: &NodeId,
    source_workspace_id: &WorkspaceId,
    source_repository: &Path,
    target_workspace_id: &WorkspaceId,
    target_repository: &Path,
    allocation_root: &Path,
    materialization_root: &Path,
    state_path: &Path,
    history_root: &Path,
    source_spawn_profile_id: &SpawnProfileId,
    source_environment_profile_id: &SpawnEnvironmentProfileId,
    target_spawn_profile_id: &SpawnProfileId,
    target_environment_profile_id: &SpawnEnvironmentProfileId,
    secret_resolver: Arc<dyn NodeSecretResolver>,
) -> NodeServerConfig {
    let worktree_profile = ManagedWorktreeProfile::new(
        WorktreeProfileId::new("semantic-context").unwrap(),
        WorktreeProfileRevision::new("live-v1").unwrap(),
        allocation_root,
        "codex/context-acceptance",
        "HEAD",
        ManagedWorktreeRetention::RemoveWhenReleased,
    )
    .unwrap();
    let source_workspace =
        WorkspaceConfig::new(source_workspace_id.clone(), source_repository).unwrap();
    let target_workspace = WorkspaceConfig::new(target_workspace_id.clone(), target_repository)
        .unwrap()
        .with_worktree_service_mode(WorktreeServiceMode::Managed)
        .with_managed_worktree_profile(worktree_profile)
        .unwrap();
    let history = NativeHistoryConfig::new(vec![NativeHistoryRoot::new(
        adapter("codex"),
        HistorySourceLayout::NdjsonWithOptionalIndex,
        history_root,
    )
    .unwrap()])
    .unwrap();
    let profiles = SpawnProfileRegistry::new([
        SpawnProfileDefaults {
            profile_id: source_spawn_profile_id.clone(),
            revision: SpawnProfileRevision::new("live-source-r1").unwrap(),
            provider: agent("codex"),
            mode: SessionMode::Inline,
            terminal_size: TerminalSize {
                rows: 40,
                columns: 160,
            },
            prompt: None,
            bundle_id: None,
            context_id: None,
            environment_profile_id: Some(source_environment_profile_id.clone()),
        },
        SpawnProfileDefaults {
            profile_id: target_spawn_profile_id.clone(),
            revision: SpawnProfileRevision::new("live-target-r1").unwrap(),
            provider: agent("codex"),
            mode: SessionMode::Inline,
            terminal_size: TerminalSize {
                rows: 40,
                columns: 160,
            },
            prompt: None,
            bundle_id: None,
            context_id: None,
            environment_profile_id: Some(target_environment_profile_id.clone()),
        },
    ])
    .unwrap();
    NodeServerConfig::new(
        endpoint,
        token,
        node_id.clone(),
        [source_workspace, target_workspace],
    )
        .unwrap()
        .with_state_path(state_path)
        .unwrap()
        .with_spawn_profiles(profiles)
        .with_session_environment_materialization(
            materialization_root,
            secret_resolver,
        )
        .unwrap()
        .with_history(history)
}

async fn wait_online(client: &C2Client, node_id: &NodeId) -> NodeRoute {
    timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(status) = client.status().await {
                if status.nodes[node_id].transport == NodeTransportState::Online {
                    return NodeRoute {
                        node_id: node_id.clone(),
                        expected_incarnation_id: status.nodes[node_id]
                            .cursor
                            .expect("online Codex node has no cursor")
                            .incarnation_id,
                    };
                }
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("Codex node did not become online through C2")
}

async fn wait_relay_ready(control: &C2ControlHandle, route: &NodeRoute) {
    timeout(Duration::from_secs(15), async {
        loop {
            if control
                .request(route.clone(), NodeRequest::Snapshot)
                .await
                .is_ok_and(|response| {
                    matches!(response.response, Ok(C2NodeResponse::Snapshot { .. }))
                })
            {
                return;
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("C2 relay did not become ready")
}

async fn snapshot(control: &C2ControlHandle, route: &NodeRoute) -> C2NodeSnapshot {
    let response = control
        .request(route.clone(), NodeRequest::Snapshot)
        .await
        .expect("C2 snapshot route failed");
    match response.response {
        Ok(C2NodeResponse::Snapshot { snapshot, .. }) => snapshot,
        _ => panic!("C2 snapshot returned an unexpected response"),
    }
}

fn find_session<'a>(
    snapshot: &'a C2NodeSnapshot,
    address: &SessionAddress,
) -> Option<&'a gate4agent_c2::protocol::C2SessionSnapshot> {
    snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.workspace_id == address.workspace_id)?
        .sessions
        .iter()
        .find(|session| {
            session.instance_id == address.session.instance_id
                && session.generation == address.session.generation
        })
}

async fn wait_completed_codex_source(
    control: &C2ControlHandle,
    route: &NodeRoute,
    source: &SessionAddress,
) -> C2ManagedSessionRecord {
    timeout(Duration::from_secs(180), async {
        loop {
            let current = snapshot(control, route).await;
            let completed = find_session(&current, source).is_some_and(|session| {
                session.provider_identity_present
                    && session.status == C2SessionStatus::Exited { exit_code: Some(0) }
            });
            if completed {
                if let Some(record) = current.session_records.iter().find(|record| {
                    record.provider == agent("codex")
                        && record.state == ManagedSessionState::Dormant
                        && record.provider_identity_present
                }) {
                    return record.clone();
                }
            }
            if find_session(&current, source)
                .is_some_and(|session| matches!(session.status, C2SessionStatus::Failed))
            {
                panic!("real Codex source failed before producing history");
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("real Codex source did not complete within the bounded canary deadline")
}

async fn discover_newest_history(
    control: &C2ControlHandle,
    route: &NodeRoute,
    source: &SessionAddress,
    started_at_unix_ms: u64,
) -> gate4agent_types::HistoryCandidateSummary {
    timeout(Duration::from_secs(30), async {
        loop {
            let response = control
                .request(
                    route.clone(),
                    NodeRequest::DiscoverHistory {
                        session: source.clone(),
                        limit: 8,
                    },
                )
                .await
                .expect("routed Codex history discovery failed");
            let candidates = match response.response {
                Ok(C2NodeResponse::HistoryDiscovered { candidates, .. }) => candidates,
                _ => panic!("Codex history discovery returned another response"),
            };
            if let Some(candidate) = candidates
                .into_iter()
                .filter(|candidate| {
                    candidate
                        .modified_at_unix_ms
                        .is_some_and(|modified| modified.saturating_add(2_000) >= started_at_unix_ms)
                })
                .max_by_key(|candidate| candidate.modified_at_unix_ms)
            {
                return candidate;
            }
            sleep(Duration::from_millis(150)).await;
        }
    })
    .await
    .expect("new Codex history did not become discoverable through Node+C2")
}

async fn wait_inventory_name(
    client: &C2Client,
    node_id: &NodeId,
    record_id: &gate4agent_node::protocol::SessionRecordId,
    source_workspace_id: &WorkspaceId,
    expected_name: &str,
) {
    timeout(Duration::from_secs(10), async {
        loop {
            if client.status().await.is_ok_and(|status| {
                status.nodes[node_id]
                    .inventory
                    .as_ref()
                    .is_some_and(|inventory| {
                        inventory.managed_sessions.iter().any(|record| {
                            &record.record_id == record_id
                                && record.display_name == expected_name
                                && record.provider_identity_present
                                && record.state == ManagedSessionState::Dormant
                                && &record.workspace_id == source_workspace_id
                        })
                    })
            }) {
                return;
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("C2 inventory did not project the human session name")
}

fn source_spawn_spec(
    node_id: &NodeId,
    workspace_id: &WorkspaceId,
    profile_id: &SpawnProfileId,
    profile_revision: &SpawnProfileRevision,
    prompt: String,
) -> SpawnSpec {
    SpawnSpec {
        target: SpawnTarget {
            node_id: node_id.clone(),
            workspace_id: workspace_id.clone(),
            worktree_id: None,
        },
        profile_id: profile_id.clone(),
        expected_profile_revision: profile_revision.clone(),
        overrides: SpawnOverrides {
            provider: SpawnOverride::Inherit,
            mode: SpawnOverride::Inherit,
            terminal_size: SpawnOverride::Inherit,
            prompt: SpawnOverride::Set {
                value: SpawnPrompt::new(prompt).unwrap(),
            },
            bundle_id: SpawnOverride::Clear,
            context_id: SpawnOverride::Clear,
            environment_profile_id: SpawnOverride::Inherit,
        },
        deadline_ms: SpawnDeadlineMs::new(30_000).unwrap(),
        idempotency_key: SpawnIdempotencyKey::new("live-codex-source-once").unwrap(),
        required_capabilities: SpawnRequiredCapabilities::new([CapabilityId::new(
            SPAWN_RUNTIME_STRUCTURED_PROMPT,
        )
        .unwrap()])
        .unwrap(),
    }
}

fn managed_target_request(
    node_id: &NodeId,
    workspace_id: &WorkspaceId,
    spawn_profile_id: &SpawnProfileId,
    spawn_profile_revision: &SpawnProfileRevision,
    context_id: gate4agent_node::protocol::SpawnContextId,
    prompt: String,
) -> ManagedWorktreeSpawnRequest {
    ManagedWorktreeSpawnRequest {
        worktree_profile_id: WorktreeProfileId::new("semantic-context").unwrap(),
        spawn_spec: SpawnSpec {
            target: SpawnTarget {
                node_id: node_id.clone(),
                workspace_id: workspace_id.clone(),
                worktree_id: None,
            },
            profile_id: spawn_profile_id.clone(),
            expected_profile_revision: spawn_profile_revision.clone(),
            overrides: SpawnOverrides {
                provider: SpawnOverride::Set {
                    value: agent("codex"),
                },
                mode: SpawnOverride::Set {
                    value: SessionMode::Inline,
                },
                terminal_size: SpawnOverride::Set {
                    value: TerminalSize {
                        rows: 40,
                        columns: 160,
                    },
                },
                prompt: SpawnOverride::Set {
                    value: SpawnPrompt::new(prompt).unwrap(),
                },
                bundle_id: SpawnOverride::Clear,
                context_id: SpawnOverride::Set { value: context_id },
                environment_profile_id: SpawnOverride::Inherit,
            },
            deadline_ms: SpawnDeadlineMs::new(30_000).unwrap(),
            idempotency_key: SpawnIdempotencyKey::new("live-codex-context-once").unwrap(),
            required_capabilities: SpawnRequiredCapabilities::new([CapabilityId::new(
                SPAWN_RUNTIME_STRUCTURED_PROMPT,
            )
            .unwrap()])
            .unwrap(),
        },
    }
}

fn assert_metadata_only(value: &impl serde::Serialize, forbidden: &[&str], label: &str) {
    let encoded = serde_json::to_string(value).unwrap();
    let decoded_backslashes = encoded.replace("\\\\", "\\");
    for private in forbidden {
        assert!(
            !encoded.contains(private) && !decoded_backslashes.contains(private),
            "{label} exposed private context bytes",
        );
    }
}

async fn wait_target_exited(
    control: &C2ControlHandle,
    route: &NodeRoute,
    target: &SessionAddress,
) {
    timeout(Duration::from_secs(60), async {
        loop {
            let current = snapshot(control, route).await;
            if find_session(&current, target).is_some_and(|session| {
                session.status == C2SessionStatus::Exited { exit_code: Some(0) }
            }) {
                return;
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("real Codex target did not exit cleanly")
}

fn find_only_fresh_codex_session_file(
    sessions_root: &Path,
    started_at_unix_ms: u64,
) -> PathBuf {
    let canonical_root = std::fs::canonicalize(sessions_root)
        .expect("configured Codex sessions root is unavailable");
    let mut pending = vec![canonical_root.clone()];
    let mut visited = 0_usize;
    let mut matches = Vec::new();
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory).expect("Codex session directory is unreadable") {
            let entry = entry.expect("Codex session directory entry is unreadable");
            visited += 1;
            assert!(visited <= 8_192, "Codex session lookup exceeded its entry bound");
            let metadata = std::fs::symlink_metadata(entry.path())
                .expect("Codex session entry metadata is unavailable");
            assert!(!metadata.file_type().is_symlink(), "Codex session tree contains a link");
            if metadata.is_dir() {
                pending.push(entry.path());
                continue;
            }
            if !metadata.is_file()
                || entry.path().extension().and_then(|value| value.to_str()) != Some("jsonl")
            {
                continue;
            }
            let modified_at_unix_ms = metadata
                .modified()
                .expect("Codex session modification time is unavailable")
                .duration_since(UNIX_EPOCH)
                .expect("Codex session modification time predates the Unix epoch")
                .as_millis() as u64;
            if modified_at_unix_ms.saturating_add(2_000) < started_at_unix_ms {
                continue;
            }
            assert!(metadata.len() <= 4 * 1024 * 1024, "Codex session exceeded 4 MiB");
            let canonical = std::fs::canonicalize(entry.path())
                .expect("Codex session file cannot be canonicalized");
            assert!(canonical.starts_with(&canonical_root), "Codex session escaped its root");
            matches.push(canonical);
        }
    }
    assert_eq!(matches.len(), 1, "fresh Codex session identity was not path-exact");
    matches.pop().unwrap()
}

fn semantic_proof_from_codex_history(
    path: &Path,
    allowed_command: &str,
    forbidden_tool_inputs: &[&str],
) -> SemanticProof {
    let file = std::fs::File::open(path).expect("fresh Codex session is unavailable");
    let mut session_ids = Vec::new();
    let mut tool_calls = Vec::new();
    let mut assistant_text = String::new();
    for (index, line) in BufReader::new(file).lines().enumerate() {
        assert!(index < 16_384, "Codex session exceeded its line bound");
        let line = line.expect("Codex session contains an unreadable line");
        assert!(line.len() <= 256 * 1024, "Codex session line exceeded 256 KiB");
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) == Some("session_meta")
        {
            if let Some(session_id) = value.pointer("/payload/id").and_then(Value::as_str) {
                assert!(
                    !session_id.is_empty() && session_id.len() <= 512,
                    "Codex transcript session identity is invalid",
                );
                session_ids.push(session_id.to_owned());
            }
        }
        if value.get("type").and_then(Value::as_str) == Some("response_item") {
            let payload_type = value.pointer("/payload/type").and_then(Value::as_str);
            if payload_type.is_some_and(|kind| kind == "function_call" || kind.ends_with("_call")) {
                let name = value
                    .pointer("/payload/name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let arguments = value
                    .pointer("/payload/arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                tool_calls.push((name, arguments));
            }
        }
        if value.get("type").and_then(Value::as_str) != Some("response_item")
            || value.pointer("/payload/role").and_then(Value::as_str) != Some("assistant")
        {
            continue;
        }
        if let Some(parts) = value.pointer("/payload/content").and_then(Value::as_array) {
            for part in parts {
                if part.get("type").and_then(Value::as_str) == Some("output_text") {
                    if let Some(text) = part.get("text").and_then(Value::as_str) {
                        assert!(
                            assistant_text.len().saturating_add(text.len()) <= 16 * 1024,
                            "Codex assistant output exceeded 16 KiB",
                        );
                        assistant_text.push_str(text);
                        assistant_text.push('\n');
                    }
                }
            }
        }
    }
    session_ids.sort();
    session_ids.dedup();
    assert_eq!(session_ids.len(), 1, "target transcript session identity is not exact");
    assert_eq!(tool_calls.len(), 1, "target Codex did not use exactly one tool call");
    let (tool_name, arguments) = tool_calls.pop().unwrap();
    assert!(tool_name == "shell_command", "target Codex used a forbidden tool");
    let arguments = serde_json::from_str::<Value>(&arguments)
        .expect("target shell tool arguments were not JSON");
    let command = arguments
        .get("command")
        .and_then(Value::as_str)
        .expect("target shell tool omitted its command");
    assert!(command == allowed_command, "target Codex changed the bounded context read");
    let folded_command = command.to_ascii_lowercase();
    for forbidden in [
        "git", "readme", "codex_home", "userprofile", "get-childitem", "select-string",
        " dir ", " ls ", " rg ", "find", "tree", "search", "list",
    ] {
        assert!(
            !folded_command.contains(forbidden),
            "target Codex shell input used a forbidden discovery operation",
        );
    }
    for forbidden in forbidden_tool_inputs {
        assert!(
            !command.contains(forbidden),
            "target Codex shell input exposed a source-only value or host path",
        );
    }
    let starts = assistant_text
        .char_indices()
        .filter_map(|(index, character)| (character == '{').then_some(index))
        .collect::<Vec<_>>();
    let ends = assistant_text
        .char_indices()
        .filter_map(|(index, character)| (character == '}').then_some(index + 1))
        .collect::<Vec<_>>();
    for start in starts {
        for end in ends.iter().copied().filter(|end| *end > start) {
            if let Ok(proof) = serde_json::from_str::<SemanticProof>(&assistant_text[start..end]) {
                return proof;
            }
        }
    }
    panic!("fresh Codex history contained no exact semantic proof object")
}

async fn wait_session_absent(
    control: &C2ControlHandle,
    route: &NodeRoute,
    address: &SessionAddress,
) {
    timeout(Duration::from_secs(15), async {
        loop {
            if find_session(&snapshot(control, route).await, address).is_none() {
                return;
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("removed session remained in the routed C2 snapshot")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires authenticated Codex CLI, GATE4AGENT_VENDOR_CANARY=1, and windows-headless-supervisor"]
async fn windows_live_codex_semantically_consumes_context_pack_and_c2_renames_inventory() {
    if !require_live_canary() {
        return;
    }

    let global_codex_home = codex_home();
    let auth_path = global_codex_home.join("auth.json");
    let config_path = global_codex_home.join("config.toml");
    let auth_before = read_sensitive_file(&auth_path, true);
    let config_before = read_sensitive_file(&config_path, false);
    let auth_bytes = Arc::new(auth_before.clone().unwrap());
    let config_bytes = config_before.clone().map(Arc::new);
    assert!(
        !auth_bytes.is_empty() && auth_bytes.len() <= 256 * 1024,
        "Codex auth file is outside the bounded secret contract",
    );
    assert!(
        config_bytes
            .as_ref()
            .is_none_or(|bytes| !bytes.is_empty() && bytes.len() <= 256 * 1024),
        "Codex config file is outside the bounded secret contract",
    );

    let root = test_root();
    std::fs::create_dir_all(&root).unwrap();
    let _root_cleanup = RemoveTestRoot(root.clone());
    let source_repository = root.join("source-repository");
    let target_repository = root.join("target-repository");
    let source_codex_home = root.join("source-codex-home");
    let allocation_root = root.join("managed-worktrees");
    let materialization_root = root.join("private-materializations");
    let state_path = root.join("node-state.json");
    std::fs::create_dir_all(&allocation_root).unwrap();
    std::fs::create_dir_all(source_codex_home.join("sessions")).unwrap();
    let source_auth_path = source_codex_home.join("auth.json");
    let source_config_path = source_codex_home.join("config.toml");
    let mut source_secret_paths = vec![source_auth_path.clone()];
    if config_bytes.is_some() {
        source_secret_paths.push(source_config_path.clone());
    }
    let mut source_secret_wiper = SecretFileWiper::new(source_secret_paths);
    write(&source_auth_path, auth_bytes.as_ref());
    if let Some(config) = config_bytes.as_ref() {
        write(&source_config_path, config.as_ref());
    }
    protect_owner_tree(&source_codex_home);

    let nonce = nonce_label();
    let history_fact = format!("ORBITALCEDAR{nonce}");
    let commit_summary = format!("context baseline {nonce}");
    let repository_note = format!("ARCHIVENOTE{nonce}");
    let status_path = format!("notes/context-dirty-{nonce}.txt");
    initialize_repository(&source_repository, &repository_note, &commit_summary);
    write(
        &source_repository.join(&status_path),
        b"untracked at context export\n",
    );
    initialize_repository(
        &target_repository,
        "TARGET_ONLY_BASELINE",
        "target-only baseline",
    );
    let source_values = [
        history_fact.as_str(),
        commit_summary.as_str(),
        repository_note.as_str(),
        status_path.as_str(),
    ];
    assert_target_repository_excludes(&target_repository, &source_values);

    let node_endpoint = endpoint("node");
    let control_endpoint = endpoint("control");
    let node_id = NodeId::new("live-codex-context-node").unwrap();
    let source_workspace_id = WorkspaceId::new("source").unwrap();
    let target_workspace_id = WorkspaceId::new("target").unwrap();
    let source_spawn_profile_id = SpawnProfileId::new("live-codex-source").unwrap();
    let target_spawn_profile_id = SpawnProfileId::new("live-codex-target").unwrap();
    let source_environment_profile_id =
        SpawnEnvironmentProfileId::new("live-codex-source-home").unwrap();
    let target_environment_profile_id =
        SpawnEnvironmentProfileId::new("live-codex-target-home").unwrap();
    let node_token = "live-codex-context-node-token";
    let c2_token = "live-codex-context-c2-token";
    let mut server = NodeServer::new(
        node_config(
            &node_endpoint,
            node_token,
            &node_id,
            &source_workspace_id,
            &source_repository,
            &target_workspace_id,
            &target_repository,
            &allocation_root,
            &materialization_root,
            &state_path,
            &source_codex_home.join("sessions"),
            &source_spawn_profile_id,
            &source_environment_profile_id,
            &target_spawn_profile_id,
            &target_environment_profile_id,
            Arc::new(InMemorySecretResolver {
                auth: Arc::clone(&auth_bytes),
                config: config_bytes.clone(),
            }),
        ),
    )
    .unwrap();
    let source_native_profile = NativeLaunchProfile::new(
        NativeLaunchProfileId::new("live-codex-source-home").unwrap(),
        agent("codex"),
        TransportKind::Pipe,
        vec![OsString::from(SOURCE_CODEX_HOME_KEY)],
        Arc::new(StaticInlineEnvironmentResolver {
            key: OsString::from(SOURCE_CODEX_HOME_KEY),
            value: source_codex_home.clone().into_os_string(),
        }),
    )
    .unwrap()
    .with_one_shot_session_persistence(OneShotSessionPersistence::Persist)
    .unwrap();
    server
        .install_environment_profile(
            NodeEnvironmentProfile::new(
                source_environment_profile_id.clone(),
                SpawnEnvironmentProfileRevision::new("live-codex-source-home-r1").unwrap(),
                agent("codex"),
                [source_native_profile],
            )
            .unwrap(),
        )
        .unwrap();
    let target_native_profile = NativeLaunchProfile::new(
        NativeLaunchProfileId::new("live-codex-target-home").unwrap(),
        agent("codex"),
        TransportKind::Pipe,
        vec![OsString::from(TARGET_CANARY_KEY)],
        Arc::new(StaticInlineEnvironmentResolver {
            key: OsString::from(TARGET_CANARY_KEY),
            value: OsString::from("1"),
        }),
    )
    .unwrap()
    .with_one_shot_session_persistence(OneShotSessionPersistence::Persist)
    .unwrap();
    server
        .install_environment_profile(
            NodeEnvironmentProfile::new_with_materialization(
                target_environment_profile_id.clone(),
                SpawnEnvironmentProfileRevision::new("live-codex-target-home-r1").unwrap(),
                agent("codex"),
                [target_native_profile],
                Some(target_materialization_profile(config_bytes.is_some())),
            )
            .unwrap(),
        )
        .unwrap();
    let node_shutdown = server.shutdown_handle();
    let node_task = tokio::spawn(server.run());

    let c2_config = C2Config::new(
        "127.0.0.1:0".parse().unwrap(),
        c2_token,
        vec![C2NodeConfig::new(node_id.clone(), &node_endpoint, node_token).unwrap()],
    )
    .unwrap()
    .with_control_endpoint(&control_endpoint)
    .unwrap()
    .with_timings(C2Timings {
        poll_interval: Duration::from_millis(25),
        fresh_for: Duration::from_secs(2),
        attempt_deadline: Duration::from_secs(2),
        transient_backoffs: [Duration::from_millis(25); 5],
        parked_backoff: Duration::from_millis(100),
        http_io_deadline: Duration::from_secs(1),
    });
    let running = C2Running::start(c2_config).await.unwrap();
    let http = C2Client::new(running.api_addr(), c2_token)
        .unwrap()
        .with_deadline(Duration::from_secs(1));
    let route = wait_online(&http, &node_id).await;
    let (control, mut events) = connect_local(&control_endpoint, c2_token).await.unwrap();
    wait_relay_ready(&control, &route).await;

    let record_events = Arc::new(Mutex::new(Vec::new()));
    let collected_record_events = Arc::clone(&record_events);
    let event_collector = tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            if let C2NodeEvent::SessionRecordUpserted { record } = event.event {
                collected_record_events.lock().unwrap().push((
                    record.record_id,
                    record.display_name,
                    record.provider_identity_present,
                    record.workspace_id,
                ));
            }
        }
    });

    let started_at_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let source_prompt = format!(
        "Remember the project codename {history_fact}. Reply with one string only: concatenate ACK, CONTEXT, and READY without separators."
    );
    let source = match control
        .request(
            route.clone(),
            NodeRequest::SpawnSpec {
                spec: source_spawn_spec(
                    &node_id,
                    &source_workspace_id,
                    &source_spawn_profile_id,
                    &SpawnProfileRevision::new("live-source-r1").unwrap(),
                    source_prompt,
                ),
            },
        )
        .await
        .unwrap()
        .response
    {
        Ok(C2NodeResponse::SpawnSpecAccepted { receipt }) => {
            assert_eq!(
                receipt
                    .environment_profile
                    .as_ref()
                    .map(|profile| &profile.profile_id),
                Some(&source_environment_profile_id),
            );
            receipt.session
        }
        _ => panic!("real Codex source spawn failed"),
    };
    let source_record = wait_completed_codex_source(&control, &route, &source).await;
    let stable_record_id = source_record.record_id.clone();
    assert!(source_record.provider_identity_present);
    assert_eq!(source_record.workspace_id, source_workspace_id);

    let human_name = format!("Codex context source {}", &nonce[..nonce.len().min(12)]);
    let renamed = control
        .request(
            route.clone(),
            NodeRequest::RenameSessionRecord {
                record_id: stable_record_id.clone(),
                display_name: human_name.clone(),
            },
        )
        .await
        .unwrap();
    assert!(matches!(renamed.response,
        Ok(C2NodeResponse::SessionRecordUpdated { ref record })
            if record.record_id == stable_record_id
                && record.display_name == human_name
                && record.provider_identity_present
                && record.workspace_id == source_workspace_id));
    wait_inventory_name(
        &http,
        &node_id,
        &stable_record_id,
        &source_workspace_id,
        &human_name,
    )
    .await;

    let candidate = discover_newest_history(
        &control,
        &route,
        &source,
        started_at_unix_ms,
    )
    .await;
    let loaded = control
        .request(
            route.clone(),
            NodeRequest::LoadHistory {
                session: source.clone(),
                candidate_id: candidate.id,
            },
        )
        .await
        .unwrap();
    assert!(matches!(loaded.response,
        Ok(C2NodeResponse::HistoryLoaded { message_count, .. }) if message_count >= 2));
    assert_metadata_only(
        &loaded,
        &[
            history_fact.as_str(),
            source_repository.to_string_lossy().as_ref(),
            source_codex_home.to_string_lossy().as_ref(),
        ],
        "HistoryLoaded response",
    );

    let exported = control
        .request(
            route.clone(),
            NodeRequest::ExportContextPack {
                session: source.clone(),
            },
        )
        .await
        .unwrap();
    let context = match &exported.response {
        Ok(C2NodeResponse::ContextPackExported { context }) => context.clone(),
        _ => panic!("Codex context export failed"),
    };
    assert_eq!(context.lineage.source_provider, agent("codex"));
    assert_eq!(context.lineage.source_session, source);
    assert!(context.retained_message_count >= 2);
    assert_metadata_only(
        &exported,
        &[
            history_fact.as_str(),
            commit_summary.as_str(),
            repository_note.as_str(),
            status_path.as_str(),
            source_repository.to_string_lossy().as_ref(),
            target_repository.to_string_lossy().as_ref(),
            source_codex_home.to_string_lossy().as_ref(),
            materialization_root.to_string_lossy().as_ref(),
        ],
        "ContextPackExported response",
    );
    let inventoried_after_export = snapshot(&control, &route).await;
    assert!(inventoried_after_export.session_records.iter().any(|record| {
        record.record_id == stable_record_id
            && record.display_name == human_name
            && record.provider_identity_present
            && record.state == ManagedSessionState::Dormant
            && record.workspace_id == source_workspace_id
    }));

    write(
        &source_repository.join("README.md"),
        "# Decoy checkout\n\nREPOSITORY_NOTE=DECOY_AFTER_EXPORT\n",
    );
    write(
        &source_repository.join(&status_path),
        b"committed only after context export\n",
    );
    commit_all(&source_repository, "post-export decoy");
    assert_target_repository_excludes(&target_repository, &source_values);

    let allowed_context_read =
        "Get-Content -LiteralPath \"$env:GATE4AGENT_CONTEXT_ROOT\\context-pack.json\" -Raw";
    let target_prompt = format!(
        "Use exactly one shell_command tool call with exactly this command: {allowed_context_read}\nDo not use any other tool or command. Treat the returned bounded JSON as the only factual source. From retained user history recover the project codename. From captured repository context recover its newest commit summary, the value after REPOSITORY_NOTE= in the captured selected file, and the first captured dirty status path. Reply with one compact JSON object containing exactly these string keys: history_fact, commit_summary, repository_note, status_path. Do not write files, include explanations, or add keys."
    );
    for expected in [&history_fact, &commit_summary, &repository_note, &status_path] {
        assert!(!target_prompt.contains(expected));
    }
    let request = managed_target_request(
        &node_id,
        &target_workspace_id,
        &target_spawn_profile_id,
        &SpawnProfileRevision::new("live-target-r1").unwrap(),
        context.id.clone(),
        target_prompt,
    );
    let target_started_at_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let accepted = control
        .request(
            route.clone(),
            NodeRequest::SpawnManagedWorktree {
                request: request.clone(),
            },
        )
        .await
        .unwrap();
    let receipt = match accepted.response {
        Ok(C2NodeResponse::ManagedWorktreeSpawnAccepted { receipt }) => receipt,
        _ => panic!("real Codex managed target spawn failed"),
    };
    assert_eq!(receipt.spawn.context_id.as_ref(), Some(&context.id));
    assert_eq!(receipt.spawn.context.as_ref(), Some(&context));
    assert!(receipt.spawn.context_binding_is_valid());
    assert_eq!(
        receipt
            .spawn
            .environment_profile
            .as_ref()
            .map(|profile| &profile.profile_id),
        Some(&target_environment_profile_id),
    );
    assert_eq!(receipt.spawn.provenance.context_id, SpawnFieldProvenance::Override);
    assert_eq!(
        receipt.spawn.provenance.environment_profile_id,
        SpawnFieldProvenance::Profile,
    );
    assert_eq!(receipt.lease.state, ManagedWorktreeLeaseState::InUse);
    assert_eq!(receipt.lease.retention, ManagedWorktreeRetention::RemoveWhenReleased);
    assert_metadata_only(
        &receipt,
        &[
            history_fact.as_str(),
            commit_summary.as_str(),
            repository_note.as_str(),
            status_path.as_str(),
            allocation_root.to_string_lossy().as_ref(),
            materialization_root.to_string_lossy().as_ref(),
        ],
        "managed Codex receipt",
    );

    let target_root = timeout(Duration::from_secs(15), async {
        loop {
            let directories = direct_child_directories(&allocation_root);
            if directories.len() == 1 {
                return directories[0].clone();
            }
            assert!(directories.len() <= 1, "managed target allocated multiple worktrees");
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("managed Codex worktree was not allocated");
    let materialized = timeout(Duration::from_secs(15), async {
        loop {
            let directories = direct_child_directories(&materialization_root);
            if directories.len() == 1 {
                return directories[0].clone();
            }
            assert!(
                directories.len() <= 1,
                "target allocated multiple private materializations",
            );
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("target Codex home was not materialized");
    let target_provider_home = materialized.join("home");
    assert!(
        std::fs::read(target_provider_home.join("auth.json")).unwrap() == *auth_bytes,
        "target materialized auth bytes changed",
    );
    match config_bytes.as_ref() {
        Some(expected) => assert!(
            std::fs::read(target_provider_home.join("config.toml")).unwrap() == **expected,
            "target materialized config bytes changed",
        ),
        None => assert!(!target_provider_home.join("config.toml").exists()),
    }
    wait_target_exited(&control, &route, &receipt.spawn.session).await;
    let target_history_path = find_only_fresh_codex_session_file(
        &target_provider_home.join("sessions"),
        target_started_at_unix_ms,
    );
    let source_repository_text = source_repository.to_string_lossy().into_owned();
    let target_repository_text = target_repository.to_string_lossy().into_owned();
    let source_codex_home_text = source_codex_home.to_string_lossy().into_owned();
    let target_root_text = target_root.to_string_lossy().into_owned();
    let materialized_text = materialized.to_string_lossy().into_owned();
    let global_codex_home_text = global_codex_home.to_string_lossy().into_owned();
    let forbidden_tool_inputs = [
        history_fact.as_str(),
        commit_summary.as_str(),
        repository_note.as_str(),
        status_path.as_str(),
        source_repository_text.as_str(),
        target_repository_text.as_str(),
        source_codex_home_text.as_str(),
        target_root_text.as_str(),
        materialized_text.as_str(),
        global_codex_home_text.as_str(),
    ];
    let proof = semantic_proof_from_codex_history(
        &target_history_path,
        allowed_context_read,
        &forbidden_tool_inputs,
    );
    assert!(
        proof.history_fact.trim() == history_fact,
        "Codex semantic proof history field did not match",
    );
    assert!(
        proof.commit_summary.trim() == commit_summary,
        "Codex semantic proof commit field did not match",
    );
    assert!(
        proof.repository_note.trim() == repository_note,
        "Codex semantic proof selected-file field did not match",
    );
    assert!(
        proof.status_path.trim().replace('\\', "/") == status_path,
        "Codex semantic proof status field did not match",
    );
    assert_eq!(
        control
            .request(
                route.clone(),
                NodeRequest::Remove {
                    session: receipt.spawn.session.clone(),
                },
            )
            .await
            .unwrap()
            .response,
        Ok(C2NodeResponse::Accepted),
    );
    timeout(Duration::from_secs(20), async {
        loop {
            let current = snapshot(&control, &route).await;
            let target_absent = find_session(&current, &receipt.spawn.session).is_none();
            let lease_absent = current
                .managed_worktrees
                .iter()
                .all(|lease| lease.lease_id != receipt.lease.lease_id);
            if target_absent && lease_absent && !target_root.exists() {
                assert!(direct_child_directories(&materialization_root).is_empty());
                return;
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("managed Codex target was not removed exactly");

    assert_eq!(
        control
            .request(
                route.clone(),
                NodeRequest::ForgetContextPack {
                    context_id: context.id.clone(),
                },
            )
            .await
            .unwrap()
            .response,
        Ok(C2NodeResponse::ContextPackForgotten {
            context_id: context.id,
        }),
    );
    assert_eq!(
        control
            .request(
                route.clone(),
                NodeRequest::Remove {
                    session: source.clone(),
                },
            )
            .await
            .unwrap()
            .response,
        Ok(C2NodeResponse::Accepted),
    );
    wait_session_absent(&control, &route, &source).await;
    assert!(matches!(
        control
            .request(
                route.clone(),
                NodeRequest::ForgetSessionRecord {
                    record_id: stable_record_id.clone(),
                },
            )
            .await
            .unwrap()
            .response,
        Ok(C2NodeResponse::SessionRecordForgotten { ref record_id })
            if record_id == &stable_record_id
    ));

    timeout(Duration::from_secs(5), async {
        loop {
            if record_events.lock().unwrap().iter().any(
                |(record_id, name, identity, workspace_id)| {
                    record_id == &stable_record_id
                        && name == &human_name
                        && *identity
                        && workspace_id == &source_workspace_id
                },
            ) {
                return;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("C2 did not relay the renamed SessionRecordUpserted event");

    let c2_shutdown = running.shutdown_handle();
    c2_shutdown.shutdown();
    timeout(Duration::from_secs(10), running.wait())
        .await
        .expect("C2 shutdown timed out")
        .expect("C2 shutdown failed");
    drop(control);
    timeout(Duration::from_secs(5), event_collector)
        .await
        .expect("C2 event collector did not close")
        .expect("C2 event collector failed");
    node_shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(15), node_task)
        .await
        .expect("Node shutdown timed out")
        .expect("Node task panicked")
        .expect("Node shutdown failed");

    assert!(
        read_sensitive_file(&auth_path, true) == auth_before,
        "Codex auth file changed during the canary",
    );
    assert!(
        read_sensitive_file(&config_path, false) == config_before,
        "Codex config file changed during the canary",
    );

    let public_inventory = http.status().await;
    assert!(public_inventory.is_err(), "C2 HTTP API remained live after shutdown");
    let source_git_status = git_output(&source_repository, &["status", "--porcelain=v1"]);
    assert!(source_git_status.status.success());
    assert!(
        source_git_status.stdout.is_empty(),
        "test left its source repository dirty",
    );
    let target_git_status = git_output(&target_repository, &["status", "--porcelain=v1"]);
    assert!(target_git_status.status.success());
    assert!(
        target_git_status.stdout.is_empty(),
        "test left its target repository dirty",
    );
    source_secret_wiper
        .wipe_and_disarm()
        .expect("explicit source secret cleanup failed");
    assert!(!source_auth_path.exists());
    assert!(!source_config_path.exists());
    remove_test_root_explicitly(&root);
}
