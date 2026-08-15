#![cfg(windows)]

use gate4agent_catalog::EnvMutation;
use gate4agent_node::protocol::{
    AdapterId, AgentId, CapabilityId, ClientRole, ManagedWorktreeLeaseState,
    ManagedWorktreeRetention, ManagedWorktreeSpawnRequest, NodeId, NodeRequest, NodeResponse,
    NodeSnapshot, SessionAddress, SessionMode, SpawnBundleDigest, SpawnBundleId,
    SpawnBundleRevision, SpawnDeadlineMs, SpawnEnvironmentProfileId,
    SpawnEnvironmentProfileRevision, SpawnFieldProvenance, SpawnIdempotencyKey, SpawnOverride,
    SpawnOverrides, SpawnProfileDefaults, SpawnProfileId, SpawnProfileRevision,
    SpawnRequiredCapabilities, SpawnSpec, SpawnTarget, WorktreeProfileId,
    WorktreeProfileRevision, WorkspaceId, NODE_HISTORY_CONTEXT_PACK_CAPABILITY,
    SPAWN_RUNTIME_RAW_PTY_LIFECYCLE,
};
use gate4agent_node::{
    protect_bundle_source_tree_fixture, HistorySourceLayout, ManagedWorktreeProfile,
    NativeHistoryConfig, NativeHistoryRoot, NodeBundle, NodeEnvironmentProfile,
    NodeSecretReference, NodeSecretResolveError, NodeSecretResolver, NodeSecretValue,
    NodeServer, NodeServerConfig, NodeSessionMaterializationProfile, NodeSessionPathBinding,
    NodeSessionPathClass, SpawnProfileRegistry, WorkspaceConfig, WorktreeServiceMode,
};
use gate4agent_node_wire::LocalNodeClient;
use gate4agent_runtime_native::{
    NativeChildEnvironmentResolveError, NativeChildEnvironmentResolver, NativeLaunchProfile,
    NativeLaunchProfileId,
};
use gate4agent_types::{SessionSnapshot, SessionStatus, TerminalSize, TransportKind};
use ring::digest::{digest, SHA256};
use serde::Serialize;
use serde_json::{json, Value};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::{sleep, timeout};

const BUNDLE_ID: &str = "review-tools";
const BUNDLE_REVISION: &str = "review-tools-r1";
const BUNDLE_DIGEST: &str =
    "sha256:7667f0d83d5a8cfa0f9ad232f22669a1b122c6253cf1ae8fe5171ae1f8df752d";
const SKILL: &[u8] = b"---\nname: review-code\ndescription: Review code for correctness and safety.\n---\n\nReview the selected change.\n";
const SKILL_SHA256: &str = "d78fe03be106b673fcf5415c8359e99f0298b5071cd11822c58ebcb27e52a68d";
const CONTEXT_USER: &str = "inspect the bounded Windows context transfer";
const CONTEXT_ASSISTANT: &str = "bounded context is ready for the target worktree";
const CONTEXT_SCHEMA: &str = "g4a-context-pack-v1";
const CONTEXT_MARKER: &str = "F7_CODEX_BUNDLE_CONTEXT_VALIDATED";
const REPOSITORY_COMMIT_SUMMARY: &str = "connect bounded context to git history";
const REPOSITORY_README: &str =
    "context pack fixture\nacceptance: correlate session intent with repository history\n";
const REPOSITORY_DIRTY_PATH: &str = "review-notes.txt";

struct UnusedSecretResolver;

impl NodeSecretResolver for UnusedSecretResolver {
    fn resolve(
        &self,
        _: &NodeSecretReference,
    ) -> Result<NodeSecretValue, NodeSecretResolveError> {
        Err(NodeSecretResolveError::Unavailable)
    }
}

struct FixtureChildEnvironmentResolver {
    user_profile: OsString,
}

impl NativeChildEnvironmentResolver for FixtureChildEnvironmentResolver {
    fn resolve_child_environment(
        &self,
    ) -> Result<Vec<EnvMutation>, NativeChildEnvironmentResolveError> {
        Ok(vec![EnvMutation {
            key: OsString::from("USERPROFILE"),
            value: Some(self.user_profile.clone()),
        }])
    }
}

struct RemoveFixtureDirectory(PathBuf);

impl Drop for RemoveFixtureDirectory {
    fn drop(&mut self) {
        let safe = self
            .0
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("gate4agent-f7-context-e2e-"));
        if safe {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

fn fixture_root(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "gate4agent-f7-context-e2e-{label}-{}-{nonce}",
        std::process::id(),
    ))
}

fn endpoint(label: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!(
        r"\\.\pipe\gate4agent-f7-context-{label}-{}-{nonce}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    )
}

fn agent(value: &str) -> AgentId {
    AgentId::new(value).unwrap()
}

fn adapter(value: &str) -> AdapterId {
    AdapterId::new(value).unwrap()
}

fn write(path: &Path, bytes: impl AsRef<[u8]>) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

fn write_json_lines(path: &Path, values: &[Value]) {
    let mut bytes = Vec::new();
    for value in values {
        serde_json::to_writer(&mut bytes, value).unwrap();
        bytes.push(b'\n');
    }
    write(path, bytes);
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
        "fixture Git command failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );
}

fn initialize_repository(root: &Path) {
    let output = Command::new("git")
        .args(["init", "-b", "main"])
        .arg(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "fixture Git repository initialization failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    write(&root.join("README.md"), REPOSITORY_README.as_bytes());
    assert_git_success(root, &["add", "--", "README.md"]);
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
            REPOSITORY_COMMIT_SUMMARY,
        ],
    );
    write(
        &root.join(REPOSITORY_DIRTY_PATH),
        b"uncommitted review evidence\n",
    );
}

fn git_worktree_count(repository: &Path) -> usize {
    let output = git_output(repository, &["worktree", "list", "--porcelain"]);
    assert!(output.status.success(), "Git worktree listing failed");
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .filter(|line| line.starts_with("worktree "))
        .count()
}

fn assert_git_clean(worktree: &Path) {
    let output = git_output(
        worktree,
        &["status", "--porcelain=v1", "--untracked-files=all"],
    );
    assert!(
        output.status.success(),
        "managed worktree status failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        output.stdout.is_empty(),
        "context target dirtied its managed worktree: {}",
        String::from_utf8_lossy(&output.stdout),
    );
}

fn direct_child_directories(root: &Path) -> Vec<PathBuf> {
    if !root.exists() {
        return Vec::new();
    }
    let mut directories = std::fs::read_dir(root)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    directories.sort();
    directories
}

fn context_history_fixtures(root: &Path, private_cwd: &str) -> NativeHistoryConfig {
    let claude_root = root.join("claude").join("projects");
    write_json_lines(
        &claude_root.join("project").join("claude-source.jsonl"),
        &[
            json!({
                "type": "user",
                "sessionId": "claude-source",
                "cwd": private_cwd,
                "message": { "content": "claude fixture question" },
            }),
            json!({
                "type": "assistant",
                "sessionId": "claude-source",
                "message": {
                    "model": "claude-fixture",
                    "content": [{ "type": "text", "text": "claude fixture answer" }],
                },
            }),
        ],
    );

    let codex_home = root.join("codex");
    let codex_sessions = codex_home.join("sessions");
    write_json_lines(
        &codex_sessions
            .join("2026")
            .join("rollout-codex-source.jsonl"),
        &[
            json!({
                "type": "session_meta",
                "payload": {
                    "id": "codex-source",
                    "thread_source": "user",
                    "cwd": private_cwd,
                },
            }),
            json!({
                "type": "turn_context",
                "payload": { "model": "gpt-fixture", "cwd": private_cwd },
            }),
            json!({
                "type": "event_msg",
                "payload": { "type": "user_message", "message": CONTEXT_USER },
            }),
            json!({
                "type": "event_msg",
                "payload": { "type": "agent_message", "message": CONTEXT_ASSISTANT },
            }),
        ],
    );
    write_json_lines(
        &codex_home.join("session_index.jsonl"),
        &[json!({ "id": "codex-source", "thread_name": "Bounded Windows handoff" })],
    );

    let kimi_home = root.join("kimi");
    let kimi_sessions = kimi_home.join("sessions");
    let kimi_session = kimi_sessions
        .join("wd_fixture")
        .join("session_kimi_source");
    write_json_lines(
        &kimi_home.join("session_index.jsonl"),
        &[json!({ "sessionId": "session_kimi_source", "workDir": private_cwd })],
    );
    write(
        &kimi_session.join("state.json"),
        serde_json::to_vec(&json!({
            "title": "Kimi fixture",
            "agents": {
                "agent-primary": { "type": "main", "parentAgentId": null },
            },
        }))
        .unwrap(),
    );
    write_json_lines(
        &kimi_session
            .join("agents")
            .join("agent-primary")
            .join("wire.jsonl"),
        &[
            json!({ "type": "config.update", "modelAlias": "kimi-fixture" }),
            json!({
                "type": "context.append_message",
                "message": {
                    "role": "user",
                    "origin": { "kind": "user" },
                    "content": "kimi fixture question",
                },
            }),
            json!({
                "type": "context.append_loop_event",
                "event": {
                    "type": "content.part",
                    "part": { "type": "text", "text": "kimi fixture answer" },
                },
            }),
            json!({ "type": "context.append_loop_event", "event": { "type": "step.end" } }),
        ],
    );

    let grok_sessions = root.join("grok").join("sessions");
    let grok_session = grok_sessions.join("project").join("grok-source");
    write(
        &grok_session.join("summary.json"),
        serde_json::to_vec(&json!({
            "info": { "id": "grok-source", "cwd": private_cwd },
            "generated_title": "Grok fixture",
            "current_model_id": "grok-fixture",
        }))
        .unwrap(),
    );
    write_json_lines(
        &grok_session.join("chat_history.jsonl"),
        &[
            json!({ "type": "user", "content": "grok fixture question" }),
            json!({ "type": "assistant", "content": "grok fixture answer" }),
        ],
    );

    let qwen_projects = root.join("qwen").join("projects");
    let qwen_session_id = "11111111-1111-4111-8111-111111111111";
    write_json_lines(
        &qwen_projects
            .join("c--fixture")
            .join("chats")
            .join(format!("{qwen_session_id}.jsonl")),
        &[
            json!({
                "uuid": "u1",
                "parentUuid": null,
                "sessionId": qwen_session_id,
                "timestamp": "2026-08-10T00:00:00Z",
                "type": "user",
                "provenance": "real_user",
                "cwd": private_cwd,
                "message": { "role": "user", "parts": [{ "text": CONTEXT_USER }] },
            }),
            json!({
                "uuid": "a1",
                "parentUuid": "u1",
                "sessionId": qwen_session_id,
                "type": "assistant",
                "provenance": "assistant_output",
                "cwd": private_cwd,
                "model": "qwen-fixture",
                "message": {
                    "role": "model",
                    "parts": [
                        { "text": "private qwen thought", "thought": true },
                        { "text": CONTEXT_ASSISTANT },
                    ],
                },
            }),
            json!({
                "uuid": "title",
                "parentUuid": "a1",
                "sessionId": qwen_session_id,
                "type": "system",
                "subtype": "custom_title",
                "cwd": private_cwd,
                "systemPayload": {
                    "customTitle": "Qwen fixture",
                    "titleSource": "manual",
                },
            }),
        ],
    );

    NativeHistoryConfig::new(vec![
        NativeHistoryRoot::new(
            adapter("claude-code"),
            HistorySourceLayout::SingleNdjson,
            claude_root,
        )
        .unwrap(),
        NativeHistoryRoot::new(
            adapter("codex"),
            HistorySourceLayout::NdjsonWithOptionalIndex,
            codex_sessions,
        )
        .unwrap(),
        NativeHistoryRoot::new(
            adapter("kimi"),
            HistorySourceLayout::StateJsonWithIndexAndSiblingNdjson,
            kimi_sessions,
        )
        .unwrap(),
        NativeHistoryRoot::new(
            adapter("grok"),
            HistorySourceLayout::SummaryJsonWithSiblingNdjson,
            grok_sessions,
        )
        .unwrap(),
        NativeHistoryRoot::new(
            adapter("qwen-code"),
            HistorySourceLayout::SingleNdjson,
            qwen_projects,
        )
        .unwrap(),
    ])
    .unwrap()
}

fn addressed_session<'a>(
    snapshot: &'a NodeSnapshot,
    address: &SessionAddress,
) -> Option<&'a SessionSnapshot> {
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

fn terminal_contains(session: &SessionSnapshot, marker: &str) -> bool {
    let marker = marker.as_bytes();
    session.terminal_frame.as_ref().is_some_and(|frame| {
        frame.contents.as_bytes().windows(marker.len()).any(|window| window == marker)
            || frame.formatted.windows(marker.len()).any(|window| window == marker)
            || frame.scrollback_formatted.iter().any(|line| {
                line.windows(marker.len()).any(|window| window == marker)
            })
    })
}

async fn snapshot(client: &mut LocalNodeClient) -> NodeSnapshot {
    let NodeResponse::Snapshot { snapshot, .. } = client
        .request(NodeRequest::Snapshot)
        .await
        .expect("authenticated Node snapshot failed")
    else {
        panic!("Node snapshot returned another response");
    };
    snapshot
}

async fn wait_for_session(
    client: &mut LocalNodeClient,
    address: &SessionAddress,
    predicate: impl Fn(&SessionSnapshot) -> bool,
) -> NodeSnapshot {
    timeout(Duration::from_secs(15), async {
        loop {
            let current = snapshot(client).await;
            if let Some(session) = addressed_session(&current, address) {
                assert!(
                    !matches!(session.status, SessionStatus::Failed { .. }),
                    "fixture session failed: {:?}",
                    session.status,
                );
                if predicate(session) {
                    return current;
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("fixture session did not reach the expected state")
}

async fn wait_for_managed_removal(
    client: &mut LocalNodeClient,
    address: &SessionAddress,
) -> NodeSnapshot {
    timeout(Duration::from_secs(15), async {
        loop {
            let current = snapshot(client).await;
            let has_session = addressed_session(&current, address).is_some();
            let has_lease = current.managed_worktrees.iter().any(|lease| {
                lease.workspace_id == address.workspace_id
                    && lease.state != ManagedWorktreeLeaseState::Removed
            });
            if !has_session && !has_lease {
                return current;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("managed session resources were not removed")
}

fn assert_metadata_only<T: Serialize>(value: &T, forbidden: &[&str], context: &str) {
    let encoded = serde_json::to_string(value).unwrap();
    let decoded_backslashes = encoded.replace("\\\\", "\\");
    for forbidden in forbidden {
        assert!(
            !encoded.contains(forbidden) && !decoded_backslashes.contains(forbidden),
            "{context} exposed private history or materialization bytes: {forbidden}",
        );
    }
}

async fn spawn_source(
    client: &mut LocalNodeClient,
    workspace_id: &WorkspaceId,
    provider: &str,
) -> SessionAddress {
    let NodeResponse::SpawnAccepted { session } = client
        .request(NodeRequest::Spawn {
            workspace_id: workspace_id.clone(),
            provider: agent(provider),
            mode: SessionMode::Pty,
            terminal_size: TerminalSize {
                rows: 32,
                columns: 120,
            },
            initial_prompt: None,
        })
        .await
        .expect("source provider fixture spawn failed")
    else {
        panic!("source provider fixture returned another response");
    };
    wait_for_session(client, &session, |current| {
        current.status == SessionStatus::Running
            && current.terminal_frame.as_ref().is_some_and(|frame| {
                !frame.contents.is_empty() && !frame.formatted.is_empty()
            })
    })
    .await;
    session
}

async fn discover_load_export(
    client: &mut LocalNodeClient,
    source: &SessionAddress,
    expected_candidate_hint: &str,
    expected_session_id: &str,
    expected_provider: &str,
    private_values: &[&str],
) -> gate4agent_node::protocol::ResolvedContextPackReceipt {
    let discovered = client
        .request(NodeRequest::DiscoverHistory {
            session: source.clone(),
            limit: 4,
        })
        .await
        .expect("history discovery failed");
    let NodeResponse::HistoryDiscovered {
        session,
        candidates,
    } = &discovered
    else {
        panic!("history discovery returned another response");
    };
    assert_eq!(session, source);
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].session_id_hint, expected_candidate_hint);
    assert!(candidates[0].id.starts_with("hist_"));
    assert_metadata_only(&discovered, private_values, "HistoryDiscovered");

    let loaded = client
        .request(NodeRequest::LoadHistory {
            session: source.clone(),
            candidate_id: candidates[0].id.clone(),
        })
        .await
        .expect("history load failed");
    let NodeResponse::HistoryLoaded {
        session,
        session_id,
        message_count,
        ..
    } = &loaded
    else {
        panic!("history load returned another response");
    };
    assert_eq!(session, source);
    assert_eq!(session_id, expected_session_id);
    assert_eq!(*message_count, 2);
    assert_metadata_only(&loaded, private_values, "HistoryLoaded");

    let public_after_load = snapshot(client).await;
    let public_source = addressed_session(&public_after_load, source)
        .expect("source disappeared after loading history");
    assert!(public_source.history.loaded.is_none());
    assert!(public_source.history.loaded_candidate_id.is_some());
    assert_metadata_only(
        &public_source.history,
        private_values,
        "public SessionSnapshot history",
    );

    let exported = client
        .request(NodeRequest::ExportContextPack {
            session: source.clone(),
        })
        .await
        .expect("context pack export failed");
    let NodeResponse::ContextPackExported { context } = &exported else {
        panic!("context pack export returned another response");
    };
    assert_eq!(&context.lineage.source_session, source);
    assert_eq!(context.lineage.source_provider, agent(expected_provider));
    assert_eq!(context.source_message_count, 2);
    assert_eq!(context.retained_message_count, 2);
    assert!(!context.truncated);
    assert_metadata_only(&exported, private_values, "ContextPackExported");
    context.clone()
}

async fn stop_remove_source(client: &mut LocalNodeClient, source: SessionAddress) {
    assert_eq!(
        client
            .request(NodeRequest::Stop {
                session: source.clone(),
                force: true,
            })
            .await
            .unwrap(),
        NodeResponse::Accepted,
    );
    wait_for_session(client, &source, |current| {
        matches!(
            current.status,
            SessionStatus::Exited { .. } | SessionStatus::Failed { .. }
        )
    })
    .await;
    assert_eq!(
        client
            .request(NodeRequest::Remove { session: source })
            .await
            .unwrap(),
        NodeResponse::Accepted,
    );
}

fn simple_node_config(
    endpoint: &str,
    token: &str,
    node_id: &NodeId,
    workspace_id: &WorkspaceId,
    repository: &Path,
    state_path: &Path,
    history: NativeHistoryConfig,
    provider: &str,
) -> NodeServerConfig {
    let profiles = SpawnProfileRegistry::new([SpawnProfileDefaults {
        profile_id: SpawnProfileId::new("matrix-default").unwrap(),
        revision: SpawnProfileRevision::new("matrix-r1").unwrap(),
        provider: agent(provider),
        mode: SessionMode::Pty,
        terminal_size: TerminalSize {
            rows: 32,
            columns: 120,
        },
        prompt: None,
        bundle_id: None,
        context_id: None,
        environment_profile_id: None,
    }])
    .unwrap();
    NodeServerConfig::new(
        endpoint,
        token,
        node_id.clone(),
        [WorkspaceConfig::new(workspace_id.clone(), repository).unwrap()],
    )
    .unwrap()
    .with_state_path(state_path)
    .unwrap()
    .with_spawn_profiles(profiles)
    .with_history(history)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windows_public_node_history_context_pack_matrix_is_bounded_for_all_five_providers() {
    gate4agent_testkit::suppress_windows_fault_dialogs_for_test();
    gate4agent_testkit::require_windows_headless_supervisor_for_test();
    let root = fixture_root("matrix");
    std::fs::create_dir_all(&root).unwrap();
    let _cleanup = RemoveFixtureDirectory(root.clone());
    let repository = root.join("repository");
    initialize_repository(&repository);
    let private_cwd = root.join("private-history-cwd").to_string_lossy().into_owned();
    let history = context_history_fixtures(&root.join("history"), &private_cwd);
    let workspace_id = WorkspaceId::new("primary").unwrap();
    let cases = [
        (
            "claude",
            "claude-source",
            "claude-source",
            "claude fixture question",
            "claude fixture answer",
        ),
        (
            "codex",
            "rollout-codex-source",
            "codex-source",
            CONTEXT_USER,
            CONTEXT_ASSISTANT,
        ),
        (
            "kimi",
            "session_kimi_source",
            "session_kimi_source",
            "kimi fixture question",
            "kimi fixture answer",
        ),
        (
            "grok",
            "grok-source",
            "grok-source",
            "grok fixture question",
            "grok fixture answer",
        ),
        (
            "qwen-code",
            "11111111-1111-4111-8111-111111111111",
            "11111111-1111-4111-8111-111111111111",
            CONTEXT_USER,
            CONTEXT_ASSISTANT,
        ),
    ];

    for (
        provider,
        expected_candidate_hint,
        expected_session_id,
        provider_question,
        provider_answer,
    ) in cases {
        let endpoint = endpoint(provider);
        let token = format!("history-{provider}-fixture-token");
        let node_id = NodeId::new(format!("history-{provider}-fixture-node")).unwrap();
        let config = simple_node_config(
            &endpoint,
            &token,
            &node_id,
            &workspace_id,
            &repository,
            &root.join(format!("{provider}-state.json")),
            history.clone(),
            provider,
        );
        let proof_path = root.join(format!("{provider}-child.proof"));
        let server = if provider == "codex" {
            NodeServer::new_provider_bundle_argv_fixture(
                config,
                agent(provider),
                proof_path,
            )
            .unwrap()
        } else {
            NodeServer::new_context_pack_fixture(config, proof_path).unwrap()
        };
        let server_task = tokio::spawn(server.run());
        let mut client = LocalNodeClient::connect(
            &endpoint,
            &node_id,
            ClientRole::Operator,
            &token,
        )
        .await
        .unwrap();
        assert!(client
            .hello()
            .compatibility
            .as_ref()
            .unwrap()
            .capabilities
            .iter()
            .any(|capability| capability.as_str() == NODE_HISTORY_CONTEXT_PACK_CAPABILITY));
        client
            .request(NodeRequest::AcquireController { lease_ms: 30_000 })
            .await
            .unwrap();
        let source = spawn_source(&mut client, &workspace_id, provider).await;
        let forbidden = [
            private_cwd.as_str(),
            provider_question,
            provider_answer,
            "private qwen thought",
        ];
        let context = discover_load_export(
            &mut client,
            &source,
            expected_candidate_hint,
            expected_session_id,
            provider,
            &forbidden,
        )
        .await;
        let context_id = context.id.clone();
        assert_eq!(
            client
                .request(NodeRequest::ForgetContextPack {
                    context_id: context_id.clone(),
                })
                .await
                .unwrap(),
            NodeResponse::ContextPackForgotten {
                context_id,
            },
        );
        stop_remove_source(&mut client, source).await;
        assert_eq!(
            client.request(NodeRequest::Shutdown).await.unwrap(),
            NodeResponse::ShuttingDown,
        );
        timeout(Duration::from_secs(5), server_task)
            .await
            .expect("fixture Node shutdown timed out")
            .expect("fixture Node task panicked")
            .expect("fixture Node failed");
    }
}

fn write_bundle(root: &Path) -> NodeBundle {
    write(
        &root.join("plugin.json"),
        br#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json","name":"review-tools"}"#,
    );
    write(&root.join("skills/review-code/SKILL.md"), SKILL);
    protect_bundle_source_tree_fixture(root).unwrap();
    NodeBundle::new(
        SpawnBundleId::new(BUNDLE_ID).unwrap(),
        SpawnBundleRevision::new(BUNDLE_REVISION).unwrap(),
        SpawnBundleDigest::new(BUNDLE_DIGEST).unwrap(),
        root,
    )
    .unwrap()
}

fn codex_materialization_profile() -> NodeSessionMaterializationProfile {
    NodeSessionMaterializationProfile::new(
        Vec::new(),
        vec![NodeSessionPathBinding::new(
            "CODEX_HOME",
            NodeSessionPathClass::ProviderHome,
        )
        .unwrap()],
        Vec::new(),
    )
    .unwrap()
}

struct ManagedConfig<'a> {
    endpoint: &'a str,
    token: &'a str,
    node_id: &'a NodeId,
    workspace_id: &'a WorkspaceId,
    repository: &'a Path,
    allocation_root: &'a Path,
    materialization_root: &'a Path,
    state_path: &'a Path,
    profile_id: &'a SpawnProfileId,
    environment_profile_id: &'a SpawnEnvironmentProfileId,
    history: NativeHistoryConfig,
}

fn managed_node_config(value: ManagedConfig<'_>) -> NodeServerConfig {
    let managed_profile = ManagedWorktreeProfile::new(
        WorktreeProfileId::new("context-review").unwrap(),
        WorktreeProfileRevision::new("fixture-v1").unwrap(),
        value.allocation_root,
        "codex/f7-context",
        "HEAD",
        ManagedWorktreeRetention::RemoveWhenReleased,
    )
    .unwrap();
    let workspace = WorkspaceConfig::new(value.workspace_id.clone(), value.repository)
        .unwrap()
        .with_worktree_service_mode(WorktreeServiceMode::Managed)
        .with_managed_worktree_profile(managed_profile)
        .unwrap();
    let profiles = SpawnProfileRegistry::new([SpawnProfileDefaults {
        profile_id: value.profile_id.clone(),
        revision: SpawnProfileRevision::new("context-target-r1").unwrap(),
        provider: agent("codex"),
        mode: SessionMode::Pty,
        terminal_size: TerminalSize {
            rows: 40,
            columns: 160,
        },
        prompt: None,
        bundle_id: None,
        context_id: None,
        environment_profile_id: Some(value.environment_profile_id.clone()),
    }])
    .unwrap();
    NodeServerConfig::new(value.endpoint, value.token, value.node_id.clone(), [workspace])
        .unwrap()
        .with_state_path(value.state_path)
        .unwrap()
        .with_spawn_profiles(profiles)
        .with_session_environment_materialization(
            value.materialization_root,
            Arc::new(UnusedSecretResolver),
        )
        .unwrap()
        .with_history(value.history)
}

fn managed_spawn_request(
    node_id: &NodeId,
    workspace_id: &WorkspaceId,
    profile_id: &SpawnProfileId,
    context_id: gate4agent_node::protocol::SpawnContextId,
) -> ManagedWorktreeSpawnRequest {
    ManagedWorktreeSpawnRequest {
        worktree_profile_id: WorktreeProfileId::new("context-review").unwrap(),
        spawn_spec: SpawnSpec {
            target: SpawnTarget {
                node_id: node_id.clone(),
                workspace_id: workspace_id.clone(),
                worktree_id: None,
            },
            profile_id: profile_id.clone(),
            expected_profile_revision: SpawnProfileRevision::new("context-target-r1").unwrap(),
            overrides: SpawnOverrides {
                provider: SpawnOverride::Inherit,
                mode: SpawnOverride::Inherit,
                terminal_size: SpawnOverride::Inherit,
                prompt: SpawnOverride::Inherit,
                bundle_id: SpawnOverride::Set {
                    value: SpawnBundleId::new(BUNDLE_ID).unwrap(),
                },
                context_id: SpawnOverride::Set { value: context_id },
                environment_profile_id: SpawnOverride::Inherit,
            },
            deadline_ms: SpawnDeadlineMs::new(20_000).unwrap(),
            idempotency_key: SpawnIdempotencyKey::new("context-target-once").unwrap(),
            required_capabilities: SpawnRequiredCapabilities::new([CapabilityId::new(
                SPAWN_RUNTIME_RAW_PTY_LIFECYCLE,
            )
            .unwrap()])
            .unwrap(),
        },
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    digest(&SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn windows_public_node_transfers_codex_context_into_managed_worktree_with_skill_bundle() {
    gate4agent_testkit::suppress_windows_fault_dialogs_for_test();
    gate4agent_testkit::require_windows_headless_supervisor_for_test();
    let root = fixture_root("vertical");
    std::fs::create_dir_all(&root).unwrap();
    let _cleanup = RemoveFixtureDirectory(root.clone());
    let repository = root.join("repository");
    let allocation_root = root.join("managed-worktrees");
    let materialization_root = root.join("private-materializations");
    let state_path = root.join("node-state.json");
    let proof_path = root.join("codex-child.proof");
    let user_profile = root.join("fixture-user-profile");
    let bundle_source = root.join("bundle-source");
    std::fs::create_dir_all(&allocation_root).unwrap();
    std::fs::create_dir_all(&user_profile).unwrap();
    initialize_repository(&repository);
    let bundle = write_bundle(&bundle_source);
    let private_cwd = root.join("private-source-cwd").to_string_lossy().into_owned();
    let history = context_history_fixtures(&root.join("history"), &private_cwd);

    let endpoint = endpoint("vertical");
    let token = "context-vertical-node-token";
    let node_id = NodeId::new("context-vertical-fixture-node").unwrap();
    let workspace_id = WorkspaceId::new("primary").unwrap();
    let profile_id = SpawnProfileId::new("context-target").unwrap();
    let environment_profile_id = SpawnEnvironmentProfileId::new("isolated-codex-home").unwrap();
    let environment_profile_revision =
        SpawnEnvironmentProfileRevision::new("isolated-codex-home-r1").unwrap();
    let config = managed_node_config(ManagedConfig {
        endpoint: &endpoint,
        token,
        node_id: &node_id,
        workspace_id: &workspace_id,
        repository: &repository,
        allocation_root: &allocation_root,
        materialization_root: &materialization_root,
        state_path: &state_path,
        profile_id: &profile_id,
        environment_profile_id: &environment_profile_id,
        history,
    });
    let mut server = NodeServer::new_context_pack_fixture(config, proof_path.clone()).unwrap();
    let native_profile = NativeLaunchProfile::new(
        NativeLaunchProfileId::new("isolated-codex-home-pty").unwrap(),
        agent("codex"),
        TransportKind::Pty,
        vec![OsString::from("USERPROFILE")],
        Arc::new(FixtureChildEnvironmentResolver {
            user_profile: user_profile.clone().into_os_string(),
        }),
    )
    .unwrap();
    server
        .install_environment_profile(
            NodeEnvironmentProfile::new_with_materialization(
                environment_profile_id.clone(),
                environment_profile_revision.clone(),
                agent("codex"),
                [native_profile],
                Some(codex_materialization_profile()),
            )
            .unwrap(),
        )
        .unwrap();
    server.install_bundle(bundle).unwrap();
    let server_task = tokio::spawn(server.run());

    let mut client = LocalNodeClient::connect(
        &endpoint,
        &node_id,
        ClientRole::Operator,
        token,
    )
    .await
    .unwrap();
    client
        .request(NodeRequest::AcquireController { lease_ms: 60_000 })
        .await
        .unwrap();
    let source = spawn_source(&mut client, &workspace_id, "qwen-code").await;
    let private_history = [private_cwd.as_str(), CONTEXT_USER, CONTEXT_ASSISTANT];
    let context = discover_load_export(
        &mut client,
        &source,
        "11111111-1111-4111-8111-111111111111",
        "11111111-1111-4111-8111-111111111111",
        "qwen-code",
        &private_history,
    )
    .await;

    let request = managed_spawn_request(
        &node_id,
        &workspace_id,
        &profile_id,
        context.id.clone(),
    );
    let accepted = client
        .request(NodeRequest::SpawnManagedWorktree {
            request: request.clone(),
        })
        .await
        .expect("context-bound managed spawn failed");
    let NodeResponse::ManagedWorktreeSpawnAccepted { receipt } = accepted else {
        panic!("context-bound managed spawn returned another response");
    };
    assert_eq!(receipt.spawn.context_id.as_ref(), Some(&context.id));
    assert_eq!(receipt.spawn.context.as_ref(), Some(&context));
    assert!(receipt.spawn.context_binding_is_valid());
    assert_eq!(receipt.spawn.bundle_id.as_ref().unwrap().as_str(), BUNDLE_ID);
    assert_eq!(
        receipt.spawn.bundle.as_ref().unwrap().digest.as_str(),
        BUNDLE_DIGEST,
    );
    assert_eq!(receipt.spawn.provenance.context_id, SpawnFieldProvenance::Override);
    assert_eq!(receipt.spawn.provenance.bundle_id, SpawnFieldProvenance::Override);
    assert_eq!(receipt.lease.state, ManagedWorktreeLeaseState::InUse);
    assert_eq!(receipt.lease.retention, ManagedWorktreeRetention::RemoveWhenReleased);
    assert_metadata_only(
        &receipt,
        &[
            private_cwd.as_str(),
            CONTEXT_USER,
            CONTEXT_ASSISTANT,
            REPOSITORY_COMMIT_SUMMARY,
            REPOSITORY_README,
            REPOSITORY_DIRTY_PATH,
            bundle_source.to_string_lossy().as_ref(),
            materialization_root.to_string_lossy().as_ref(),
            allocation_root.to_string_lossy().as_ref(),
        ],
        "managed spawn receipt",
    );

    let running = wait_for_session(&mut client, &receipt.spawn.session, |current| {
        current.status == SessionStatus::Running
            && terminal_contains(current, CONTEXT_MARKER)
    })
    .await;
    assert_eq!(
        addressed_session(&running, &source).unwrap().status,
        SessionStatus::Running,
        "target spawn disturbed the source session",
    );
    assert!(addressed_session(&running, &source)
        .unwrap()
        .history
        .loaded
        .is_none());
    assert_metadata_only(
        &running,
        &[
            private_cwd.as_str(),
            CONTEXT_USER,
            CONTEXT_ASSISTANT,
            REPOSITORY_COMMIT_SUMMARY,
            REPOSITORY_README,
            REPOSITORY_DIRTY_PATH,
            bundle_source.to_string_lossy().as_ref(),
            materialization_root.to_string_lossy().as_ref(),
        ],
        "public Node snapshot",
    );

    let worktrees = direct_child_directories(&allocation_root);
    assert_eq!(worktrees.len(), 1);
    let worktree = std::fs::canonicalize(&worktrees[0]).unwrap();
    assert_eq!(git_worktree_count(&repository), 2);
    assert_git_clean(&worktree);
    let materializations = direct_child_directories(&materialization_root);
    assert_eq!(materializations.len(), 1);
    let materialized = materializations[0].clone();
    let provider_home = materialized.join("home");
    let context_root = materialized.join("context");
    let context_path = context_root.join("context-pack.json");
    assert_eq!(
        std::fs::read(provider_home.join("skills/review-code/SKILL.md")).unwrap(),
        SKILL,
    );
    let context_bytes = std::fs::read(&context_path).unwrap();
    assert_eq!(context_bytes.len(), context.byte_len as usize);
    assert!(!String::from_utf8_lossy(&context_bytes).contains("private qwen thought"));
    let document: Value = serde_json::from_slice(&context_bytes).unwrap();
    assert_eq!(document["schema"], CONTEXT_SCHEMA);
    assert_eq!(document["source_provider"], "qwen-code");
    assert!(document.get("cwd").is_none());
    assert_eq!(document["retained_messages"][0]["text"], CONTEXT_USER);
    assert_eq!(
        document["retained_messages"][1]["text"],
        CONTEXT_ASSISTANT,
    );
    assert_eq!(document["repository"]["is_repository"], true);
    assert_eq!(document["repository"]["branch"], "main");
    assert_eq!(document["repository"]["truncated"], false);
    assert!(document["repository"]["recent_commits"]
        .as_array()
        .unwrap()
        .iter()
        .any(|commit| commit["summary"] == REPOSITORY_COMMIT_SUMMARY));
    assert!(document["repository"]["status"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| {
            entry["path"] == REPOSITORY_DIRTY_PATH
                && entry["index_status"] == "?"
                && entry["worktree_status"] == "?"
        }));
    let selected_readme = document["repository"]["selected_files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["path"] == "README.md")
        .expect("ContextPack omitted selected README.md");
    assert_eq!(selected_readme["content"], REPOSITORY_README);
    assert_eq!(selected_readme["truncated"], false);
    assert!(selected_readme.get("skipped").is_none());

    let proof = std::fs::read_to_string(&proof_path)
        .expect("Codex child omitted its context validation proof");
    let proof = proof.lines().collect::<Vec<_>>();
    assert_eq!(proof.len(), 11, "Codex child emitted a non-exact context proof");
    assert_eq!(proof[0], "contextual");
    assert_eq!(
        std::fs::canonicalize(Path::new(proof[1])).unwrap(),
        std::fs::canonicalize(&provider_home).unwrap(),
    );
    assert_eq!(
        std::fs::canonicalize(Path::new(proof[2])).unwrap(),
        worktree,
    );
    assert_eq!(proof[3], SKILL_SHA256);
    assert_eq!(
        std::fs::canonicalize(Path::new(proof[4])).unwrap(),
        std::fs::canonicalize(&context_root).unwrap(),
    );
    assert_eq!(proof[5], sha256_hex(&context_bytes));
    assert_eq!(proof[6], CONTEXT_SCHEMA);
    assert_eq!(proof[7], "qwen-code");
    assert_eq!(proof[8], "2");
    assert_eq!(proof[9], CONTEXT_USER);
    assert_eq!(proof[10], CONTEXT_ASSISTANT);

    assert_eq!(
        client
            .request(NodeRequest::Stop {
                session: receipt.spawn.session.clone(),
                force: true,
            })
            .await
            .unwrap(),
        NodeResponse::Accepted,
    );
    wait_for_session(&mut client, &receipt.spawn.session, |current| {
        matches!(
            current.status,
            SessionStatus::Exited { .. } | SessionStatus::Failed { .. }
        )
    })
    .await;
    assert!(worktree.is_dir(), "Stop removed the managed worktree");
    assert!(context_path.is_file(), "Stop removed the private context pack");
    assert!(provider_home.join("skills/review-code/SKILL.md").is_file());
    assert_git_clean(&worktree);

    assert_eq!(
        client
            .request(NodeRequest::Remove {
                session: receipt.spawn.session.clone(),
            })
            .await
            .unwrap(),
        NodeResponse::Accepted,
    );
    let after_remove = wait_for_managed_removal(&mut client, &receipt.spawn.session).await;
    assert!(!materialized.exists(), "Remove retained private context or bundle bytes");
    assert!(!worktree.exists(), "Remove retained the managed worktree");
    assert_eq!(git_worktree_count(&repository), 1);
    assert_eq!(
        addressed_session(&after_remove, &source).unwrap().status,
        SessionStatus::Running,
        "target removal disturbed the source session",
    );

    assert_eq!(
        client
            .request(NodeRequest::ForgetContextPack {
                context_id: context.id.clone(),
            })
            .await
            .unwrap(),
        NodeResponse::ContextPackForgotten {
            context_id: context.id,
        },
    );
    stop_remove_source(&mut client, source).await;
    assert_eq!(
        client.request(NodeRequest::Shutdown).await.unwrap(),
        NodeResponse::ShuttingDown,
    );
    timeout(Duration::from_secs(5), server_task)
        .await
        .expect("fixture Node shutdown timed out")
        .expect("fixture Node task panicked")
        .expect("fixture Node failed");
}
