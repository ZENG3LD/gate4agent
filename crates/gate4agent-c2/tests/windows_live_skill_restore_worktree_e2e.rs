#![cfg(windows)]

use gate4agent_c2::protocol::{
    C2ManagedSessionRecord, C2NodeEvent, C2NodeResponse, C2NodeSnapshot, C2SessionStatus,
    NodeId, NodeRoute, NodeTransportState,
};
use gate4agent_c2::{C2Config, C2NodeConfig, C2Running, C2Timings};
use gate4agent_c2_client::{connect_local, C2Client, C2ControlHandle};
use gate4agent_node::protocol::{
    AdapterId, CapabilityId, ClientRole, ManagedSessionState, ManagedWorktreeRetention,
    ManagedWorktreeSpawnRequest, NodeEvent, NodeFailureCode, NodeRequest, ServerFrame,
    SessionAddress, SessionMode, SpawnDeadlineMs, SpawnFieldProvenance, SpawnIdempotencyKey,
    SpawnOverride, SpawnOverrides, SpawnProfileDefaults, SpawnProfileId, SpawnProfileRevision,
    SpawnRequiredCapabilities, SpawnSpec, SpawnTarget, WorktreeProfileId,
    WorktreeProfileRevision, WorkspaceId, MAX_NODE_TERMINAL_BYTES,
    SPAWN_RUNTIME_RAW_PTY_LIFECYCLE,
};
use gate4agent_node::{
    HistorySourceLayout, ManagedWorktreeProfile, NativeHistoryConfig, NativeHistoryRoot,
    NodeServer, NodeServerConfig, SpawnProfileRegistry, WorkspaceConfig, WorktreeServiceMode,
};
use gate4agent_node_wire::NamedPipeNodeClient;
use gate4agent_types::{
    prepare_input, AdapterFamily, AgentId, ControlEventKind, InputAction, PreparedInputKind,
    PreparedWriteKind, PromptFraming, PromptPayload, ProviderActivity, ProviderEvent,
    ProviderSource, TerminalControl, TerminalFrame, TerminalSize, BRACKETED_PASTE_END,
    BRACKETED_PASTE_START, TERMINAL_SUBMIT_DELAY_MS,
};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io::Write;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::{sleep, timeout};

const LIVE_RESTORE_ENV: &str = "GATE4AGENT_LIVE_SKILL_RESTORE";
const HEADLESS_SUPERVISOR_ENV: &str = "GATE4AGENT_HEADLESS_SUPERVISOR";
const MAX_HELPER_OUTPUT_BYTES: usize = 512 * 1024;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
const PRIVATE_ROOT_MARKER: &str = ".gate4agent-live-owner";

const CLAUDE_SOURCE_NAME: &str = "Claude display-name planning";
const CODEX_TARGET_NAME: &str = "Codex display-name implementation";
const CODEX_SOURCE_NAME: &str = "Codex review-tag planning";
const CLAUDE_TARGET_NAME: &str = "Claude review-tag implementation";

const DISPLAY_COMMIT: &str = "feat: normalize display names";
const DISPLAY_SOURCE_DECISION: &str = "decision: reject display names containing ASCII NUL";
const DISPLAY_TEST: &str = "normalize_display_name_handles_ascii_whitespace";
const DISPLAY_TEST_COMMAND: &str =
    "cargo test --test normalize_display_name normalize_display_name_handles_ascii_whitespace -- --exact";
const TAGS_COMMIT: &str = "feat: normalize review tags";
const TAGS_SOURCE_DECISION: &str = "decision: drop review tags containing ASCII NUL";
const TAGS_TEST: &str = "parse_review_tags_deduplicates_in_first_seen_order";
const TAGS_TEST_COMMAND: &str =
    "cargo test --test parse_review_tags parse_review_tags_deduplicates_in_first_seen_order -- --exact";

const DISPLAY_TURN_ONE: &str = "We are planning an unfinished Rust library change, not implementing it in this session. Objective: implement pub fn normalize_display_name(input: &str) -> Option<String> in src/lib.rs so human-facing names have canonical ASCII whitespace. Explain the objective and leave all files unchanged for a continuation.";
const DISPLAY_TURN_TWO: &str = "Chosen behavior: trim leading and trailing ASCII whitespace and collapse every internal ASCII whitespace run to one ordinary space while preserving case. Whitespace-only input returns None. The required edge case is tabs and newlines. Unicode case folding and truncation are explicit non-goals. A repository-specific invariant is recorded in the latest Git decision; the continuation must inspect the supplied source workspace before implementing. Confirm the decision without editing files.";
const DISPLAY_TURN_THREE: &str = "Implementation handoff: modify only src/lib.rs and add tests/normalize_display_name.rs; run cargo test --test normalize_display_name normalize_display_name_handles_ascii_whitespace -- --exact; commit with subject feat: normalize display names. No code has changed. The next step is implementation in the continuation. Return a concise handoff.";

const TAGS_TURN_ONE: &str = "We are planning an unfinished Rust library change, not implementing it in this session. Objective: implement pub fn parse_review_tags(input: &str) -> Vec<String> in src/lib.rs for comma-separated review tags. Explain the objective and leave all files unchanged for a continuation.";
const TAGS_TURN_TWO: &str = "Chosen behavior: split on commas, trim ASCII whitespace, lowercase ASCII letters, drop empty pieces, and deduplicate while preserving first-seen order. The required edge case is Bug,bug, PERF ,,perf producing bug then perf. Unicode normalization is an explicit non-goal. A repository-specific invariant is recorded in the latest Git decision; the continuation must inspect the supplied source workspace before implementing. Confirm the decision without editing files.";
const TAGS_TURN_THREE: &str = "Implementation handoff: modify only src/lib.rs and add tests/parse_review_tags.rs; run cargo test --test parse_review_tags parse_review_tags_deduplicates_in_first_seen_order -- --exact; commit with subject feat: normalize review tags. No code has changed. The next step is implementation in the continuation. Return a concise handoff.";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Provider {
    Claude,
    Codex,
}

impl Provider {
    fn agent(self) -> AgentId {
        match self {
            Self::Claude => agent("claude"),
            Self::Codex => agent("codex"),
        }
    }

    fn adapter(self) -> AdapterId {
        match self {
            Self::Claude => adapter("claude-code"),
            Self::Codex => adapter("codex"),
        }
    }

    fn helper_name(self) -> &'static str {
        match self {
            Self::Claude => "session-summary.exe",
            Self::Codex => "codex-session-restore.exe",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

#[derive(Clone, Copy)]
struct LaneSpec {
    source_provider: Provider,
    target_provider: Provider,
    source_name: &'static str,
    target_name: &'static str,
    turns: [&'static str; 3],
    test_file: &'static str,
    test_name: &'static str,
    test_command: &'static str,
    commit_subject: &'static str,
    source_decision: &'static str,
    semantic_terms: [&'static str; 3],
    acceptance_test: &'static str,
    expected_files: [&'static str; 2],
}

const DISPLAY_LANE: LaneSpec = LaneSpec {
    source_provider: Provider::Claude,
    target_provider: Provider::Codex,
    source_name: CLAUDE_SOURCE_NAME,
    target_name: CODEX_TARGET_NAME,
    turns: [DISPLAY_TURN_ONE, DISPLAY_TURN_TWO, DISPLAY_TURN_THREE],
    test_file: "tests/normalize_display_name.rs",
    test_name: DISPLAY_TEST,
    test_command: DISPLAY_TEST_COMMAND,
    commit_subject: DISPLAY_COMMIT,
    source_decision: DISPLAY_SOURCE_DECISION,
    semantic_terms: ["normalize_display_name", "Whitespace-only input", "Implementation handoff"],
    acceptance_test: "display_name_acceptance",
    expected_files: ["src/lib.rs", "tests/normalize_display_name.rs"],
};

const TAGS_LANE: LaneSpec = LaneSpec {
    source_provider: Provider::Codex,
    target_provider: Provider::Claude,
    source_name: CODEX_SOURCE_NAME,
    target_name: CLAUDE_TARGET_NAME,
    turns: [TAGS_TURN_ONE, TAGS_TURN_TWO, TAGS_TURN_THREE],
    test_file: "tests/parse_review_tags.rs",
    test_name: TAGS_TEST,
    test_command: TAGS_TEST_COMMAND,
    commit_subject: TAGS_COMMIT,
    source_decision: TAGS_SOURCE_DECISION,
    semantic_terms: ["parse_review_tags", "first-seen order", "Implementation handoff"],
    acceptance_test: "review_tags_acceptance",
    expected_files: ["src/lib.rs", "tests/parse_review_tags.rs"],
};

struct PrivateRoot {
    path: PathBuf,
    temp_root: PathBuf,
    marker: String,
    armed: bool,
}

impl PrivateRoot {
    fn create(path: PathBuf) -> Self {
        let temp_root = long_temp_root();
        assert_eq!(path.parent(), Some(temp_root.as_path()));
        assert!(path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("gate4agent-live-skill-restore-")));
        std::fs::create_dir(&path).unwrap();
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        assert!(metadata.is_dir() && !metadata_is_reparse(&metadata));
        let marker = path.file_name().unwrap().to_string_lossy().into_owned();
        let marker_path = path.join(PRIVATE_ROOT_MARKER);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker_path)
            .unwrap();
        file.write_all(marker.as_bytes()).unwrap();
        file.sync_all().unwrap();
        Self {
            path,
            temp_root,
            marker,
            armed: true,
        }
    }

    fn owns_literal_root(&self) -> bool {
        if self.path.parent() != Some(self.temp_root.as_path())
            || !self
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("gate4agent-live-skill-restore-"))
        {
            return false;
        }
        let Ok(root_metadata) = std::fs::symlink_metadata(&self.path) else {
            return false;
        };
        let marker_path = self.path.join(PRIVATE_ROOT_MARKER);
        let Ok(marker_metadata) = std::fs::symlink_metadata(&marker_path) else {
            return false;
        };
        root_metadata.is_dir()
            && !metadata_is_reparse(&root_metadata)
            && marker_metadata.is_file()
            && !metadata_is_reparse(&marker_metadata)
            && std::fs::read_to_string(marker_path).ok().as_deref() == Some(self.marker.as_str())
    }

    fn recreate_owner_marker(&self) -> Result<(), ()> {
        if self.path.parent() != Some(self.temp_root.as_path())
            || self.path.file_name().and_then(|name| name.to_str())
                != Some(self.marker.as_str())
        {
            return Err(());
        }
        let root_metadata = std::fs::symlink_metadata(&self.path).map_err(|_| ())?;
        if !root_metadata.is_dir() || metadata_is_reparse(&root_metadata) {
            return Err(());
        }
        let marker_path = self.path.join(PRIVATE_ROOT_MARKER);
        if marker_path.parent() != Some(self.path.as_path()) {
            return Err(());
        }
        let mut marker = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker_path)
            .map_err(|_| ())?;
        marker.write_all(self.marker.as_bytes()).map_err(|_| ())?;
        marker.sync_all().map_err(|_| ())?;
        let marker_metadata = std::fs::symlink_metadata(&marker_path).map_err(|_| ())?;
        if !marker_metadata.is_file()
            || metadata_is_reparse(&marker_metadata)
            || std::fs::read_to_string(marker_path).ok().as_deref()
                != Some(self.marker.as_str())
        {
            return Err(());
        }
        Ok(())
    }

    fn remove_empty_root_with(
        &self,
        remove_root: impl FnOnce(&Path) -> std::io::Result<()>,
    ) -> Result<(), ()> {
        if remove_root(&self.path).is_ok() {
            return Ok(());
        }
        self.recreate_owner_marker()?;
        Err(())
    }

    fn cleanup_literal_root(&self) -> Result<(), ()> {
        if !self.owns_literal_root() {
            return Err(());
        }
        let mut children = Vec::new();
        for entry in std::fs::read_dir(&self.path).map_err(|_| ())? {
            let entry = entry.map_err(|_| ())?;
            if entry.file_name() != OsStr::new(PRIVATE_ROOT_MARKER) {
                let path = entry.path();
                if path.parent() != Some(self.path.as_path()) {
                    return Err(());
                }
                children.push(path);
            }
        }
        children.sort();
        for child in children {
            remove_literal_tree(&child)?;
        }
        if !self.owns_literal_root() {
            return Err(());
        }
        for entry in std::fs::read_dir(&self.path).map_err(|_| ())? {
            if entry.map_err(|_| ())?.file_name() != OsStr::new(PRIVATE_ROOT_MARKER) {
                return Err(());
            }
        }
        std::fs::remove_file(self.path.join(PRIVATE_ROOT_MARKER)).map_err(|_| ())?;
        self.remove_empty_root_with(|path| std::fs::remove_dir(path))
    }

    fn remove(&mut self) {
        assert!(self.owns_literal_root(), "private root identity changed before cleanup");
        assert!(
            self.cleanup_literal_root().is_ok(),
            "private test root cleanup failed",
        );
        assert!(!self.path.exists());
        self.armed = false;
    }

}

impl Drop for PrivateRoot {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.cleanup_literal_root();
        }
    }
}

fn remove_literal_tree(path: &Path) -> Result<(), ()> {
    let metadata = std::fs::symlink_metadata(path).map_err(|_| ())?;
    if metadata_is_reparse(&metadata) {
        return Err(());
    }
    if metadata.is_file() {
        return std::fs::remove_file(path).map_err(|_| ());
    }
    if !metadata.is_dir() {
        return Err(());
    }
    let mut children = Vec::new();
    for entry in std::fs::read_dir(path).map_err(|_| ())? {
        let entry = entry.map_err(|_| ())?;
        let child = entry.path();
        if child.parent() != Some(path) {
            return Err(());
        }
        children.push(child);
    }
    children.sort();
    for child in children {
        remove_literal_tree(&child)?;
    }
    std::fs::remove_dir(path).map_err(|_| ())
}

#[derive(Default)]
struct TargetTranscriptEvidence {
    exact_target_prompt_seen: bool,
    first_action_used_expected_helper: bool,
    helper_output_seen: bool,
    source_git_status_seen: bool,
    source_git_log_seen: bool,
    source_git_show_seen: bool,
    named_test_seen: bool,
    commit_seen: bool,
    context_pack_seen: bool,
    assistant_text_seen: bool,
}

struct SourceSession {
    address: SessionAddress,
    record: C2ManagedSessionRecord,
    native_session_id: String,
    source_revision: String,
}

struct TargetSession {
    record: C2ManagedSessionRecord,
    worktree_root: PathBuf,
}

type ProviderEventLog = Arc<Mutex<Vec<(SessionAddress, ProviderSource, ProviderEvent)>>>;

#[derive(Clone, Copy, Eq, PartialEq)]
enum TargetToolEvidence {
    Helper,
    SourceGitStatus,
    SourceGitLog,
    SourceGitShow,
    NamedTest,
    Commit,
}

fn agent(value: &str) -> AgentId {
    AgentId::new(value).unwrap()
}

fn adapter(value: &str) -> AdapterId {
    AdapterId::new(value).unwrap()
}

fn endpoint(label: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!(
        r"\\.\pipe\gate4agent-live-skill-restore-{label}-{}-{unique}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    )
}

fn test_root() -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    long_temp_root().join(format!(
        "gate4agent-live-skill-restore-{}-{unique}",
        std::process::id(),
    ))
}

fn long_temp_root() -> PathBuf {
    let local_app_data = std::env::var_os("LocalAppData")
        .or_else(|| std::env::var_os("LOCALAPPDATA"))
        .map(PathBuf::from)
        .expect("LocalAppData is unavailable");
    assert!(local_app_data.is_absolute(), "LocalAppData is not absolute");
    assert!(
        local_app_data.components().all(|component| !matches!(
            component,
            std::path::Component::CurDir | std::path::Component::ParentDir
        )),
        "LocalAppData is not a lexical absolute path",
    );
    let temp_root = local_app_data.join("Temp");
    let lexical = temp_root.to_string_lossy();
    assert!(
        !lexical.starts_with(r"\\?\") && !lexical.starts_with(r"\\.\"),
        "LocalAppData Temp uses an extended or device path",
    );
    for directory in [
        local_app_data.parent(),
        Some(local_app_data.as_path()),
        Some(temp_root.as_path()),
    ]
    .into_iter()
    .flatten()
    {
        let metadata = std::fs::symlink_metadata(directory)
            .expect("LocalAppData Temp authority is unavailable");
        assert!(
            metadata.is_dir() && !metadata_is_reparse(&metadata),
            "LocalAppData Temp authority is not a regular directory",
        );
    }
    temp_root
}

fn require_live_restore() {
    assert_eq!(
        std::env::var(LIVE_RESTORE_ENV).ok().as_deref(),
        Some("1"),
        "ignored live restore test requires explicit {LIVE_RESTORE_ENV}=1",
    );
    assert_eq!(
        std::env::var_os(HEADLESS_SUPERVISOR_ENV).as_deref(),
        Some(OsStr::new("1")),
        "live provider process tests must run through windows-headless-supervisor",
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
}

fn user_profile() -> PathBuf {
    PathBuf::from(std::env::var_os("USERPROFILE").expect("USERPROFILE is unavailable"))
}

fn assert_installed_skills_match_canonical() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("workspace layout is unavailable");
    let profile = user_profile();
    let skill_pairs = [
        (
            profile.join(".codex/skills/codex-restore-session/SKILL.md"),
            workspace.join("codex-session-restore/skill/codex-restore-session/SKILL.md"),
        ),
        (
            profile.join(".claude/skills/restore-session/SKILL.md"),
            workspace.join("claude-session-restore/skill/SKILL.md"),
        ),
    ];
    for (installed, canonical) in skill_pairs {
        let installed_metadata = std::fs::symlink_metadata(&installed)
            .expect("installed restore skill is unavailable");
        let canonical_metadata = std::fs::symlink_metadata(&canonical)
            .expect("canonical restore skill is unavailable");
        assert!(
            installed_metadata.is_file()
                && canonical_metadata.is_file()
                && !metadata_is_reparse(&installed_metadata)
                && !metadata_is_reparse(&canonical_metadata),
            "restore skill authority is not a regular file",
        );
        let installed = std::fs::read(installed).expect("installed restore skill is unreadable");
        let canonical = std::fs::read(canonical).expect("canonical restore skill is unreadable");
        assert!(
            installed == canonical,
            "installed restore skill differs from its canonical source",
        );
    }
}

fn write(path: &Path, contents: impl AsRef<[u8]>) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

fn metadata_is_reparse(metadata: &std::fs::Metadata) -> bool {
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

fn git_output(root: &Path, arguments: &[&str]) -> Output {
    Command::new("git")
        .arg("--no-pager")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .output()
        .unwrap()
}

fn assert_git_success(root: &Path, arguments: &[&str]) -> Output {
    let output = git_output(root, arguments);
    assert!(output.status.success(), "fixture Git command failed");
    output
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

fn initialize_repository(root: &Path, lane: LaneSpec, include_source_decision: bool) -> String {
    let initialized = Command::new("git")
        .args(["init", "-b", "main"])
        .arg(root)
        .output()
        .unwrap();
    assert!(initialized.status.success(), "fixture repository initialization failed");
    assert_git_success(root, &["config", "user.name", "Gate4Agent Live Test"]);
    assert_git_success(root, &["config", "user.email", "live@gate4agent.invalid"]);
    assert_git_success(root, &["config", "commit.gpgSign", "false"]);
    assert_git_success(root, &["config", "core.hooksPath", "NUL"]);
    write(
        &root.join("Cargo.toml"),
        b"[package]\nname = \"restore-lane\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n",
    );
    write(&root.join(".gitignore"), b"/target/\n");
    match lane.source_provider {
        Provider::Claude => {
            write(
                &root.join("src/lib.rs"),
                b"pub fn normalize_display_name(_input: &str) -> Option<String> {\n    None\n}\n",
            );
        }
        Provider::Codex => {
            write(
                &root.join("src/lib.rs"),
                b"pub fn parse_review_tags(_input: &str) -> Vec<String> {\n    Vec::new()\n}\n",
            );
        }
    }
    let lock = Command::new("cargo")
        .arg("generate-lockfile")
        .current_dir(root)
        .output()
        .unwrap();
    assert!(lock.status.success(), "fixture lockfile generation failed");
    commit_all(root, "initial restore fixture");
    if include_source_decision {
        let (decision, handoff) = match lane.source_provider {
            Provider::Claude => (
                DISPLAY_SOURCE_DECISION,
                "# Product continuation decision\n\n`normalize_display_name` must return `None` when the original input contains an ASCII NUL byte. Apply this invariant before whitespace normalization.\n",
            ),
            Provider::Codex => (
                TAGS_SOURCE_DECISION,
                "# Product continuation decision\n\n`parse_review_tags` must drop each comma-separated tag containing an ASCII NUL byte. Other tags continue through normalization in first-seen order.\n",
            ),
        };
        write(&root.join("HANDOFF.md"), handoff.as_bytes());
        commit_all(root, decision);
    }
    let base = assert_git_success(root, &["rev-parse", "HEAD"]);
    let base = String::from_utf8(base.stdout).unwrap().trim().to_owned();
    assert_eq!(base.len(), 40);
    base
}

fn assert_source_unchanged(root: &Path, base: &str) {
    let head = assert_git_success(root, &["rev-parse", "HEAD"]);
    assert_eq!(String::from_utf8(head.stdout).unwrap().trim(), base);
    let status = assert_git_success(root, &["status", "--porcelain=v1", "--untracked-files=all"]);
    assert!(status.stdout.is_empty(), "planning source repository was modified");
}

fn assert_target_result(root: &Path, base: &str, lane: LaneSpec) {
    let subject = assert_git_success(root, &["log", "-1", "--format=%s"]);
    assert_eq!(
        String::from_utf8(subject.stdout).unwrap().trim(),
        lane.commit_subject,
        "target commit subject differs from the restored handoff",
    );
    let count = assert_git_success(root, &["rev-list", "--count", &format!("{base}..HEAD")]);
    assert_eq!(String::from_utf8(count.stdout).unwrap().trim(), "1");
    let changed = assert_git_success(root, &["diff", "--name-only", &format!("{base}..HEAD")]);
    let changed = String::from_utf8(changed.stdout)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        changed,
        lane.expected_files.into_iter().map(str::to_owned).collect(),
        "target commit changed files outside the agreed scope",
    );
    assert!(root.join(lane.test_file).is_file());

    let named = Command::new("cargo")
        .args([
            "test",
            "--test",
            lane.test_file
                .strip_prefix("tests/")
                .and_then(|name| name.strip_suffix(".rs"))
                .unwrap(),
            lane.test_name,
            "--",
            "--exact",
        ])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        named.status.success() && command_output_proves_one_test(&named),
        "restored named test did not pass exactly one host test",
    );
    let host_acceptance_path = root.join("tests/gate4agent_host_acceptance.rs");
    let host_acceptance = match lane.source_provider {
        Provider::Claude => br#"use restore_lane::normalize_display_name;

#[test]
fn display_name_acceptance() {
    assert_eq!(normalize_display_name("  Alpha\t Beta\nGamma  "), Some("Alpha Beta Gamma".to_owned()));
    assert_eq!(normalize_display_name("\r\n\t "), None);
    assert_eq!(normalize_display_name("A\0B"), None);
    assert_eq!(normalize_display_name("MiXeD\u{a0}Name"), Some("MiXeD\u{a0}Name".to_owned()));
}
"#
        .as_slice(),
        Provider::Codex => r#"use restore_lane::parse_review_tags;

#[test]
fn review_tags_acceptance() {
    assert_eq!(parse_review_tags(" Bug,bug, PERF ,,perf"), vec!["bug", "perf"]);
    assert_eq!(parse_review_tags("API,security,api"), vec!["api", "security"]);
    assert_eq!(parse_review_tags("ok,bad\0tag, PERF"), vec!["ok", "perf"]);
    assert_eq!(parse_review_tags("Résumé,Résumé"), vec!["résumé"]);
}
"#
        .as_bytes(),
    };
    write(&host_acceptance_path, host_acceptance);
    let acceptance = Command::new("cargo")
        .args([
            "test",
            "--test",
            "gate4agent_host_acceptance",
            lane.acceptance_test,
            "--",
            "--exact",
        ])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        acceptance.status.success() && command_output_proves_one_test(&acceptance),
        "independent semantic acceptance did not pass exactly one host test",
    );
    std::fs::remove_file(&host_acceptance_path).expect("host acceptance cleanup failed");
    assert!(!host_acceptance_path.exists());
    let status = assert_git_success(root, &["status", "--porcelain=v1", "--untracked-files=all"]);
    assert!(status.stdout.is_empty(), "target worktree is dirty after its commit");
}

fn spawn_defaults(
    id: &str,
    revision: &str,
    provider: Provider,
) -> SpawnProfileDefaults {
    SpawnProfileDefaults {
        profile_id: SpawnProfileId::new(id).unwrap(),
        revision: SpawnProfileRevision::new(revision).unwrap(),
        provider: provider.agent(),
        mode: SessionMode::Pty,
        terminal_size: TerminalSize {
            rows: 44,
            columns: 180,
        },
        prompt: None,
        bundle_id: None,
        context_id: None,
        environment_profile_id: None,
    }
}

struct TestIds {
    node: NodeId,
    claude_source_workspace: WorkspaceId,
    codex_target_workspace: WorkspaceId,
    codex_source_workspace: WorkspaceId,
    claude_target_workspace: WorkspaceId,
    claude_source_profile: SpawnProfileId,
    codex_target_profile: SpawnProfileId,
    codex_source_profile: SpawnProfileId,
    claude_target_profile: SpawnProfileId,
}

impl TestIds {
    fn new() -> Self {
        Self {
            node: NodeId::new("live-skill-restore-node").unwrap(),
            claude_source_workspace: WorkspaceId::new("claude-display-plan").unwrap(),
            codex_target_workspace: WorkspaceId::new("codex-display-target").unwrap(),
            codex_source_workspace: WorkspaceId::new("codex-tags-plan").unwrap(),
            claude_target_workspace: WorkspaceId::new("claude-tags-target").unwrap(),
            claude_source_profile: SpawnProfileId::new("claude-display-source").unwrap(),
            codex_target_profile: SpawnProfileId::new("codex-display-target").unwrap(),
            codex_source_profile: SpawnProfileId::new("codex-tags-source").unwrap(),
            claude_target_profile: SpawnProfileId::new("claude-tags-target").unwrap(),
        }
    }
}

fn node_config(
    endpoint: &str,
    token: &str,
    ids: &TestIds,
    claude_source_repository: &Path,
    codex_target_repository: &Path,
    codex_source_repository: &Path,
    claude_target_repository: &Path,
    codex_allocation_root: &Path,
    claude_allocation_root: &Path,
    state_path: &Path,
    claude_history_root: &Path,
    codex_history_root: &Path,
) -> NodeServerConfig {
    let codex_managed_profile = ManagedWorktreeProfile::new(
        WorktreeProfileId::new("codex-display-implementation").unwrap(),
        WorktreeProfileRevision::new("live-v1").unwrap(),
        codex_allocation_root,
        "codex/display-name",
        "HEAD",
        ManagedWorktreeRetention::RemoveWhenReleased,
    )
    .unwrap();
    let claude_managed_profile = ManagedWorktreeProfile::new(
        WorktreeProfileId::new("claude-tags-implementation").unwrap(),
        WorktreeProfileRevision::new("live-v1").unwrap(),
        claude_allocation_root,
        "claude/review-tags",
        "HEAD",
        ManagedWorktreeRetention::RemoveWhenReleased,
    )
    .unwrap();
    let workspaces = [
        WorkspaceConfig::new(
            ids.claude_source_workspace.clone(),
            claude_source_repository,
        )
        .unwrap(),
        WorkspaceConfig::new(ids.codex_target_workspace.clone(), codex_target_repository)
            .unwrap()
            .with_worktree_service_mode(WorktreeServiceMode::Managed)
            .with_managed_worktree_profile(codex_managed_profile)
            .unwrap(),
        WorkspaceConfig::new(ids.codex_source_workspace.clone(), codex_source_repository)
            .unwrap(),
        WorkspaceConfig::new(
            ids.claude_target_workspace.clone(),
            claude_target_repository,
        )
        .unwrap()
        .with_worktree_service_mode(WorktreeServiceMode::Managed)
        .with_managed_worktree_profile(claude_managed_profile)
        .unwrap(),
    ];
    let profiles = SpawnProfileRegistry::new([
        spawn_defaults(
            ids.claude_source_profile.as_str(),
            "claude-source-r1",
            Provider::Claude,
        ),
        spawn_defaults(
            ids.codex_target_profile.as_str(),
            "codex-target-r1",
            Provider::Codex,
        ),
        spawn_defaults(
            ids.codex_source_profile.as_str(),
            "codex-source-r1",
            Provider::Codex,
        ),
        spawn_defaults(
            ids.claude_target_profile.as_str(),
            "claude-target-r1",
            Provider::Claude,
        ),
    ])
    .unwrap();
    let history = NativeHistoryConfig::new(vec![
        NativeHistoryRoot::new(
            Provider::Claude.adapter(),
            HistorySourceLayout::SingleNdjson,
            claude_history_root,
        )
        .unwrap(),
        NativeHistoryRoot::new(
            Provider::Codex.adapter(),
            HistorySourceLayout::NdjsonWithOptionalIndex,
            codex_history_root,
        )
        .unwrap(),
    ])
    .unwrap();
    NodeServerConfig::new(endpoint, token, ids.node.clone(), workspaces)
        .unwrap()
        .with_state_path(state_path)
        .unwrap()
        .with_spawn_profiles(profiles)
        .with_history(history)
}

fn source_spawn_spec(
    node_id: &NodeId,
    workspace_id: &WorkspaceId,
    profile_id: &SpawnProfileId,
    profile_revision: &SpawnProfileRevision,
    idempotency: &str,
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
            prompt: SpawnOverride::Clear,
            bundle_id: SpawnOverride::Clear,
            context_id: SpawnOverride::Clear,
            environment_profile_id: SpawnOverride::Inherit,
        },
        deadline_ms: SpawnDeadlineMs::new(30_000).unwrap(),
        idempotency_key: SpawnIdempotencyKey::new(idempotency).unwrap(),
        required_capabilities: SpawnRequiredCapabilities::new([CapabilityId::new(
            SPAWN_RUNTIME_RAW_PTY_LIFECYCLE,
        )
        .unwrap()])
        .unwrap(),
    }
}

fn target_spawn_request(
    node_id: &NodeId,
    workspace_id: &WorkspaceId,
    profile_id: &SpawnProfileId,
    profile_revision: &SpawnProfileRevision,
    worktree_profile_id: &str,
    idempotency: &str,
) -> ManagedWorktreeSpawnRequest {
    ManagedWorktreeSpawnRequest {
        worktree_profile_id: WorktreeProfileId::new(worktree_profile_id).unwrap(),
        spawn_spec: SpawnSpec {
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
                prompt: SpawnOverride::Clear,
                bundle_id: SpawnOverride::Clear,
                context_id: SpawnOverride::Clear,
                environment_profile_id: SpawnOverride::Inherit,
            },
            deadline_ms: SpawnDeadlineMs::new(30_000).unwrap(),
            idempotency_key: SpawnIdempotencyKey::new(idempotency).unwrap(),
            required_capabilities: SpawnRequiredCapabilities::new([CapabilityId::new(
                SPAWN_RUNTIME_RAW_PTY_LIFECYCLE,
            )
            .unwrap()])
            .unwrap(),
        },
    }
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
                            .expect("online node omitted its cursor")
                            .incarnation_id,
                    };
                }
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("Node did not become online through C2")
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
        .expect("routed snapshot request failed");
    match response.response {
        Ok(C2NodeResponse::Snapshot { snapshot, .. }) => snapshot,
        _ => panic!("routed snapshot returned an unexpected response variant"),
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

async fn wait_record(
    control: &C2ControlHandle,
    route: &NodeRoute,
    address: &SessionAddress,
    provider: Provider,
) -> C2ManagedSessionRecord {
    timeout(Duration::from_secs(60), async {
        loop {
            let current = snapshot(control, route).await;
            if let Some(record) = current.session_records.iter().find(|record| {
                record.provider == provider.agent()
                    && record.active_session.as_ref() == Some(address)
                    && !record.provider_identity_present
                    && record.state == ManagedSessionState::Live
            }) {
                return record.clone();
            }
            if find_session(&current, address).is_some_and(|session| {
                matches!(
                    session.status,
                    C2SessionStatus::Failed | C2SessionStatus::Exited { .. }
                )
            }) {
                panic!("provider stopped before its raw session record became live");
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("raw session record did not reach the C2 inventory")
}

async fn rename_record(
    control: &C2ControlHandle,
    route: &NodeRoute,
    client: &C2Client,
    node_id: &NodeId,
    record: &C2ManagedSessionRecord,
    display_name: &str,
) -> C2ManagedSessionRecord {
    let response = control
        .request(
            route.clone(),
            NodeRequest::RenameSessionRecord {
                record_id: record.record_id.clone(),
                display_name: display_name.to_owned(),
            },
        )
        .await
        .unwrap();
    let renamed = match response.response {
        Ok(C2NodeResponse::SessionRecordUpdated { record }) => record,
        _ => panic!("session rename returned an unexpected response variant"),
    };
    assert_eq!(renamed.record_id, record.record_id);
    assert_eq!(renamed.display_name, display_name);
    assert!(!renamed.provider_identity_present);
    timeout(Duration::from_secs(10), async {
        loop {
            if client.status().await.is_ok_and(|status| {
                status.nodes[node_id]
                    .inventory
                    .as_ref()
                    .is_some_and(|inventory| {
                        inventory.managed_sessions.iter().any(|candidate| {
                            candidate.record_id == record.record_id
                                && candidate.display_name == display_name
                                && !candidate.provider_identity_present
                        })
                    })
            }) {
                return;
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("renamed session was not projected by the C2 inventory");
    renamed
}

fn assert_no_context(snapshot: &C2NodeSnapshot) {
    for record in &snapshot.session_records {
        assert!(record.context_id.is_none());
        assert!(record.context.is_none());
        assert!(record.context_binding_is_valid());
    }
}

async fn terminal_frame(
    control: &C2ControlHandle,
    route: &NodeRoute,
    address: &SessionAddress,
) -> Option<TerminalFrame> {
    find_session(&snapshot(control, route).await, address)
        .and_then(|session| session.terminal_frame.clone())
}

struct TurnReadinessDiagnostics {
    samples: u32,
    saw_session: bool,
    saw_frame: bool,
    saw_nonempty_frame: bool,
    sequence_changed_vs_baseline: bool,
    content_changed_vs_baseline: bool,
    saw_idle_or_waiting: bool,
    saw_expected_composer: bool,
    last_interaction_pending: bool,
    ready_samples: u8,
    last_status: &'static str,
    last_activity: &'static str,
    last_exit_code: Option<i32>,
    last_terminal_tail: String,
}

impl TurnReadinessDiagnostics {
    fn new() -> Self {
        Self {
            samples: 0,
            saw_session: false,
            saw_frame: false,
            saw_nonempty_frame: false,
            sequence_changed_vs_baseline: false,
            content_changed_vs_baseline: false,
            saw_idle_or_waiting: false,
            saw_expected_composer: false,
            last_interaction_pending: false,
            ready_samples: 0,
            last_status: "not-observed",
            last_activity: "not-observed",
            last_exit_code: None,
            last_terminal_tail: "<no-frame>".to_owned(),
        }
    }
}

fn bounded_terminal_tail(contents: &str) -> String {
    let mut tail = contents.chars().rev().take(4_096).collect::<Vec<_>>();
    tail.reverse();
    let mut escaped = String::new();
    for character in tail {
        match character {
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            '\u{1b}' => escaped.push_str("\\x1b"),
            value if value.is_control() => {
                let _ = std::fmt::Write::write_fmt(
                    &mut escaped,
                    format_args!("\\u{{{:04X}}}", value as u32),
                );
            }
            value => escaped.push(value),
        }
    }
    escaped
}

fn session_status_label(status: &C2SessionStatus) -> &'static str {
    match status {
        C2SessionStatus::Registered => "registered",
        C2SessionStatus::Starting => "starting",
        C2SessionStatus::Running => "running",
        C2SessionStatus::Stopping => "stopping",
        C2SessionStatus::Exited { .. } => "exited",
        C2SessionStatus::Failed => "failed",
    }
}

fn provider_activity_label(activity: ProviderActivity) -> &'static str {
    match activity {
        ProviderActivity::Idle => "idle",
        ProviderActivity::Working => "working",
        ProviderActivity::WaitingForInput => "waiting-for-input",
        ProviderActivity::Blocked => "blocked",
    }
}

fn next_ready_sample_count(
    current: u8,
    observed_content_change: bool,
    ordinary_ready: bool,
) -> u8 {
    if observed_content_change && ordinary_ready {
        current.saturating_add(1)
    } else {
        0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExactStartupGate {
    WorkspaceTrust,
    VendorUpdate,
}

fn normalized_screen(contents: &str) -> String {
    contents
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn exact_startup_gate(contents: &str) -> Option<ExactStartupGate> {
    let normalized = normalized_screen(contents);
    if [
        "trust this folder",
        "trust the files in this folder",
        "trust the contents of this directory",
        "do you trust this directory",
    ]
    .iter()
    .any(|marker| normalized.contains(marker))
        || (normalized.contains("quick safety check")
            && (normalized.contains("yes, i trust this folder")
                || normalized.contains("continue without these permissions")))
    {
        return Some(ExactStartupGate::WorkspaceTrust);
    }
    if normalized.contains("kimi code update available")
        && normalized.contains("install update now")
    {
        return Some(ExactStartupGate::VendorUpdate);
    }
    None
}

fn exact_selected_option(contents: &str, options: &[&str]) -> bool {
    contents.lines().map(str::trim).any(|line| {
        let selected = line
            .strip_prefix('❯')
            .or_else(|| line.strip_prefix('›'))
            .map(str::trim_start);
        selected.is_some_and(|selected| options.iter().any(|option| selected == *option))
    })
}

fn enter_confirmation_is_explicit(contents: &str) -> bool {
    contents.lines().map(str::trim).any(|line| {
        line == "Enter confirm"
            || line == "Enter to confirm"
            || line.starts_with("Enter to confirm ·")
    })
}

fn claude_trust_enter_is_proven(contents: &str, workspace: &Path) -> bool {
    contents.contains(workspace.to_string_lossy().as_ref())
        && exact_selected_option(
            contents,
            &["Yes, I trust this folder", "1. Yes, I trust this folder"],
        )
        && enter_confirmation_is_explicit(contents)
}

fn codex_trust_enter_is_proven(contents: &str, workspace: &Path) -> bool {
    let workspace_line = format!("> You are in {}", workspace.to_string_lossy());
    let lines = contents.lines().map(str::trim).collect::<Vec<_>>();
    lines.iter().any(|line| *line == workspace_line.as_str())
        && lines
            .iter()
            .any(|line| {
                line.starts_with(
                    "Do you trust the contents of this directory? Working with untrusted contents comes with higher risk of prompt injection.",
                )
            })
        && lines.iter().any(|line| *line == "› 1. Yes, continue")
        && lines.iter().any(|line| *line == "2. No, quit")
        && lines
            .iter()
            .any(|line| *line == "Press enter to continue")
}

fn known_noncomposer_operator_screen(contents: &str) -> bool {
    let normalized = normalized_screen(contents);
    exact_startup_gate(contents).is_some()
        || normalized.contains("choose the text style that looks best with your terminal")
        || (normalized.contains("welcome to claude code for")
            && (normalized.contains("open files") || normalized.contains("selected lines")))
        || (normalized.contains("welcome to claude code")
            && (normalized.contains("press enter")
                || normalized.contains("enter to continue")))
        || (normalized.contains("migration")
            && normalized.contains("enter confirm")
            && normalized.contains("esc"))
}

fn expected_provider_composer_visible(provider: Provider, contents: &str) -> bool {
    if known_noncomposer_operator_screen(contents) {
        return false;
    }
    match provider {
        Provider::Claude => contents.contains('\u{276f}'),
        Provider::Codex => contents.contains('\u{203a}'),
    }
}

async fn wait_turn_ready(
    control: &C2ControlHandle,
    route: &NodeRoute,
    address: &SessionAddress,
    baseline: Option<TerminalFrame>,
    stage: &str,
    provider: Provider,
    startup_workspace: Option<&Path>,
) -> TerminalFrame {
    let mut diagnostics = TurnReadinessDiagnostics::new();
    let result = timeout(Duration::from_secs(240), async {
        let mut observed_content_change = false;
        let mut last_frame: Option<TerminalFrame> = None;
        let mut ready_samples = 0_u8;
        let mut operator_action_sent = false;
        loop {
            let current = snapshot(control, route).await;
            diagnostics.samples = diagnostics.samples.saturating_add(1);
            let session = find_session(&current, address)
                .unwrap_or_else(|| panic!("active session disappeared during {stage}"));
            diagnostics.saw_session = true;
            diagnostics.last_status = session_status_label(&session.status);
            diagnostics.last_activity = provider_activity_label(session.provider_activity);
            diagnostics.last_exit_code = match &session.status {
                C2SessionStatus::Exited { exit_code } => *exit_code,
                _ => None,
            };
            diagnostics.last_interaction_pending = session.provider_interaction_pending;
            diagnostics.saw_idle_or_waiting |= matches!(
                session.provider_activity,
                ProviderActivity::Idle | ProviderActivity::WaitingForInput
            );
            if let Some(frame) = &session.terminal_frame {
                diagnostics.saw_frame = true;
                diagnostics.saw_nonempty_frame |= !frame.contents.trim().is_empty();
                diagnostics.last_terminal_tail = bounded_terminal_tail(&frame.contents);
                diagnostics.saw_expected_composer |=
                    expected_provider_composer_visible(provider, &frame.contents);
                diagnostics.sequence_changed_vs_baseline |= baseline
                    .as_ref()
                    .is_some_and(|before| frame.sequence != before.sequence);
                diagnostics.content_changed_vs_baseline |= baseline
                    .as_ref()
                    .is_some_and(|before| frame.contents != before.contents);
                observed_content_change |= baseline.as_ref().map_or_else(
                    || !frame.contents.trim().is_empty(),
                    |before| {
                        frame.sequence != before.sequence && frame.contents != before.contents
                    },
                );
                last_frame = Some(frame.clone());
            }
            if matches!(
                session.status,
                C2SessionStatus::Failed | C2SessionStatus::Exited { .. }
            ) {
                panic!(
                    "provider stopped before ordinary turn readiness: stage={stage} status={} exit_code={} terminal_tail={}",
                    diagnostics.last_status,
                    diagnostics
                        .last_exit_code
                        .map(|code| code.to_string())
                        .unwrap_or_else(|| "none".to_owned()),
                    diagnostics.last_terminal_tail,
                );
            }
            if let Some(frame) = &session.terminal_frame {
                if let Some(gate) = exact_startup_gate(&frame.contents) {
                    ready_samples = 0;
                    diagnostics.ready_samples = 0;
                    if operator_action_sent {
                        sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                    let action_proven = match gate {
                        ExactStartupGate::WorkspaceTrust =>
                            startup_workspace.is_some_and(|workspace| match provider {
                                Provider::Claude => {
                                    claude_trust_enter_is_proven(&frame.contents, workspace)
                                }
                                Provider::Codex => {
                                    codex_trust_enter_is_proven(&frame.contents, workspace)
                                }
                            }),
                        ExactStartupGate::VendorUpdate => false,
                    };
                    if !action_proven {
                        panic!(
                            "startup operator key was not exactly proven: stage={stage} terminal_tail={}",
                            diagnostics.last_terminal_tail,
                        );
                    }
                    assert!(matches!(
                        control
                            .request(
                                route.clone(),
                                NodeRequest::TerminalControl {
                                    session: address.clone(),
                                    control: TerminalControl::Enter,
                                },
                            )
                            .await
                            .unwrap()
                            .response,
                        Ok(C2NodeResponse::Accepted)
                    ));
                    operator_action_sent = true;
                    sleep(Duration::from_millis(50)).await;
                    continue;
                }
            }
            let ordinary_ready = matches!(
                session.provider_activity,
                ProviderActivity::Idle | ProviderActivity::WaitingForInput
            ) && !session.provider_interaction_pending
                && session.terminal_frame.as_ref().is_some_and(|frame| {
                    expected_provider_composer_visible(provider, &frame.contents)
                });
            ready_samples = next_ready_sample_count(
                ready_samples,
                observed_content_change,
                ordinary_ready,
            );
            diagnostics.ready_samples = ready_samples;
            if ready_samples >= 8 {
                return last_frame
                    .clone()
                    .expect("content changed without a retained TerminalFrame");
            }
            sleep(Duration::from_millis(250)).await;
        }
    })
    .await;
    match result {
        Ok(frame) => frame,
        Err(_) => panic!(
            "turn readiness timeout: stage={stage} samples={} saw_session={} saw_frame={} saw_nonempty_frame={} sequence_changed_vs_baseline={} content_changed_vs_baseline={} saw_idle_or_waiting={} saw_expected_composer={} provider_interaction_pending={} ready_samples={} status={} activity={} exit_code={} terminal_tail={}",
            diagnostics.samples,
            diagnostics.saw_session,
            diagnostics.saw_frame,
            diagnostics.saw_nonempty_frame,
            diagnostics.sequence_changed_vs_baseline,
            diagnostics.content_changed_vs_baseline,
            diagnostics.saw_idle_or_waiting,
            diagnostics.saw_expected_composer,
            diagnostics.last_interaction_pending,
            diagnostics.ready_samples,
            diagnostics.last_status,
            diagnostics.last_activity,
            diagnostics
                .last_exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "none".to_owned()),
            diagnostics.last_terminal_tail,
        ),
    }
}

fn bounded_terminal_chunks(bytes: &[u8]) -> Vec<Vec<u8>> {
    assert!(!bytes.is_empty(), "prepared terminal write was empty");
    bytes
        .chunks(MAX_NODE_TERMINAL_BYTES)
        .map(<[u8]>::to_vec)
        .collect()
}

fn control_free_prompt_prefix(prompt: &str) -> Option<String> {
    let prefix = prompt.chars().take(32).collect::<String>();
    (!prefix.is_empty() && !prefix.chars().any(char::is_control)).then_some(prefix)
}

async fn submit_turn(
    control: &C2ControlHandle,
    route: &NodeRoute,
    address: &SessionAddress,
    prompt: &str,
    stage: &str,
    provider: Provider,
) -> TerminalFrame {
    let initial_frame = terminal_frame(control, route, address).await;
    let completion_baseline = match provider {
        Provider::Claude => {
            let before = initial_frame
                .clone()
                .expect("Claude prompt submission had no baseline TerminalFrame");
            let prompt_prefix = control_free_prompt_prefix(prompt)
                .expect("Claude prompt prefix was empty or contained controls");
            assert!(matches!(
                control
                    .request(
                        route.clone(),
                        NodeRequest::Input {
                            session: address.clone(),
                            text: prompt.to_owned(),
                        },
                    )
                    .await
                    .unwrap()
                    .response,
                Ok(C2NodeResponse::Accepted)
            ));
            let mut last_terminal_tail = bounded_terminal_tail(&before.contents);
            let draft_frame = timeout(Duration::from_secs(15), async {
                loop {
                    let current = snapshot(control, route).await;
                    let session = find_session(&current, address).unwrap_or_else(|| {
                        panic!(
                            "Claude draft session disappeared: stage={stage} terminal_tail={last_terminal_tail}",
                        )
                    });
                    if let Some(frame) = &session.terminal_frame {
                        last_terminal_tail = bounded_terminal_tail(&frame.contents);
                        if frame.sequence > before.sequence
                            && frame.contents != before.contents
                            && frame.contents.contains(&prompt_prefix)
                        {
                            return frame.clone();
                        }
                    }
                    if matches!(
                        session.status,
                        C2SessionStatus::Failed | C2SessionStatus::Exited { .. }
                    ) {
                        panic!(
                            "Claude stopped before draft visibility: stage={stage} terminal_tail={last_terminal_tail}",
                        );
                    }
                    sleep(Duration::from_millis(50)).await;
                }
            })
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "Claude draft visibility timeout: stage={stage} terminal_tail={last_terminal_tail}",
                )
            });
            sleep(Duration::from_millis(TERMINAL_SUBMIT_DELAY_MS)).await;
            assert!(matches!(
                control
                    .request(
                        route.clone(),
                        NodeRequest::TerminalControl {
                            session: address.clone(),
                            control: TerminalControl::Enter,
                        },
                    )
                    .await
                    .unwrap()
                    .response,
                Ok(C2NodeResponse::Accepted)
            ));
            Some(draft_frame)
        }
        Provider::Codex => {
            let prepared = prepare_input(InputAction::SubmitPrompt(PromptPayload {
                text: prompt.to_owned(),
                framing: PromptFraming::BracketedPaste,
            }))
            .expect("fixture prompt was rejected by the bounded input encoder");
            assert_eq!(prepared.kind(), PreparedInputKind::SubmitPrompt);
            for write in prepared.into_writes() {
                if write.delay_before_ms > 0 {
                    sleep(Duration::from_millis(write.delay_before_ms)).await;
                }
                for bytes in bounded_terminal_chunks(&write.bytes) {
                    assert!(matches!(
                        control
                            .request(
                                route.clone(),
                                NodeRequest::TerminalBytes {
                                    session: address.clone(),
                                    bytes,
                                },
                            )
                            .await
                            .unwrap()
                            .response,
                        Ok(C2NodeResponse::Accepted)
                    ));
                }
            }
            initial_frame
        }
    };
    wait_turn_ready(
        control,
        route,
        address,
        completion_baseline,
        stage,
        provider,
        None,
    )
    .await
}

fn provider_event_source_matches(source: &ProviderSource, provider: Provider) -> bool {
    let provider_id_matches = match provider {
        Provider::Claude => matches!(source.binding.id.as_str(), "claude-code" | "claude"),
        Provider::Codex => source.binding.id.as_str() == "codex",
    };
    provider_id_matches
        && matches!(
            source.family,
            AdapterFamily::PtySemantic | AdapterFamily::Hook | AdapterFamily::ManagedHook
        )
}

fn provider_hook_source_matches(source: &ProviderSource, provider: Provider) -> bool {
    let expected = match provider {
        Provider::Claude => "claude",
        Provider::Codex => "codex",
    };
    source.binding.id.as_str() == expected
        && matches!(source.family, AdapterFamily::Hook | AdapterFamily::ManagedHook)
}

fn history_session_id_matches_hint(session_id: &str, session_id_hint: &str) -> bool {
    let parts = session_id.split('-').collect::<Vec<_>>();
    if parts.iter().map(|part| part.len()).collect::<Vec<_>>() != [8, 4, 4, 4, 12]
        || !parts
            .iter()
            .all(|part| part.chars().all(|character| character.is_ascii_hexdigit()))
    {
        return false;
    }
    session_id_hint == session_id || session_id_hint.ends_with(&format!("-{session_id}"))
}

#[derive(Debug)]
struct HistoryCandidateDiagnostic {
    candidate_id: String,
    session_id_hint: String,
    outcome: &'static str,
    message_count: Option<u64>,
    completed_turn_count: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SourceHistorySelection {
    candidate_id: String,
    native_session_id: String,
    message_count: u64,
    completed_turn_count: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SourceHistoryRequirement {
    message_count: u64,
    completed_turn_count: Option<u64>,
}

fn required_source_history_progress(
    provider: Provider,
    selected: Option<&SourceHistorySelection>,
) -> SourceHistoryRequirement {
    match provider {
        Provider::Claude => SourceHistoryRequirement {
            message_count: 0,
            completed_turn_count: Some(selected.map_or(1, |selected| {
                selected
                    .completed_turn_count
                    .expect("retained Claude history omitted completed-turn metadata")
                    .checked_add(1)
                    .expect("source completed-turn count overflowed")
            })),
        },
        Provider::Codex => {
            assert!(
                selected.is_none_or(|selected| selected.completed_turn_count.is_none()),
                "retained Codex history unexpectedly had completed-turn metadata",
            );
            SourceHistoryRequirement {
                message_count: selected.map_or(2, |selected| {
                    selected
                        .message_count
                        .checked_add(2)
                        .expect("source history message count overflowed")
                }),
                completed_turn_count: None,
            }
        }
    }
}

fn source_history_progress_satisfied(
    provider: Provider,
    requirement: SourceHistoryRequirement,
    message_count: u64,
    completed_turn_count: Option<u64>,
) -> bool {
    match provider {
        Provider::Claude => completed_turn_count.is_some_and(|completed_turn_count| {
            requirement
                .completed_turn_count
                .is_some_and(|required| completed_turn_count >= required)
        }),
        Provider::Codex => {
            completed_turn_count.is_none() && message_count >= requirement.message_count
        }
    }
}

fn source_history_metadata_matches_provider(
    provider: Provider,
    completed_turn_count: Option<u64>,
) -> bool {
    match provider {
        Provider::Claude => completed_turn_count.is_some(),
        Provider::Codex => completed_turn_count.is_none(),
    }
}

fn source_history_pending_outcome(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "completed-turn-under-required",
        Provider::Codex => "message-count-under-required",
    }
}

fn history_candidate_diagnostics_text(diagnostics: &[HistoryCandidateDiagnostic]) -> String {
    let mut text = String::from("[");
    for (index, diagnostic) in diagnostics.iter().enumerate() {
        if index > 0 {
            text.push_str(", ");
        }
        let _ = std::fmt::Write::write_fmt(
            &mut text,
            format_args!(
                "{{candidate_id={:?}, session_id_hint={:?}, outcome={}, message_count={:?}, completed_turn_count={:?}}}",
                diagnostic.candidate_id,
                diagnostic.session_id_hint,
                diagnostic.outcome,
                diagnostic.message_count,
                diagnostic.completed_turn_count,
            ),
        );
    }
    text.push(']');
    text
}

async fn wait_source_history_progress(
    control: &C2ControlHandle,
    route: &NodeRoute,
    address: &SessionAddress,
    started_at_unix_ms: u64,
    provider: Provider,
    selected: Option<&SourceHistorySelection>,
    stage: &str,
) -> SourceHistorySelection {
    let requirement = required_source_history_progress(provider, selected);
    let mut history_diagnostics = Vec::new();
    let result = timeout(Duration::from_secs(300), async {
        'discover: loop {
            let discovered = control
                .request(
                    route.clone(),
                    NodeRequest::DiscoverHistory {
                        session: address.clone(),
                        limit: 8,
                    },
                )
                .await
                .unwrap();
            let candidates = match discovered.response {
                Ok(C2NodeResponse::HistoryDiscovered { candidates, .. }) => candidates,
                _ => panic!("history discovery returned an unexpected response variant"),
            };
            assert!(candidates.len() <= 8, "history discovery exceeded its requested bound");
            history_diagnostics = candidates
                .iter()
                .map(|candidate| {
                    let fresh = candidate.modified_at_unix_ms.is_some_and(|time| {
                        time.saturating_add(2_000) >= started_at_unix_ms
                    });
                    HistoryCandidateDiagnostic {
                        candidate_id: candidate.id.clone(),
                        session_id_hint: candidate.session_id_hint.clone(),
                        outcome: if fresh { "pending-load" } else { "stale" },
                        message_count: None,
                        completed_turn_count: None,
                    }
                })
                .collect();
            let candidates = candidates
                .into_iter()
                .enumerate()
                .filter(|candidate| {
                    candidate.1.modified_at_unix_ms.is_some_and(|time| {
                        time.saturating_add(2_000) >= started_at_unix_ms
                    })
                })
                .collect::<Vec<_>>();
            if candidates.is_empty() {
                sleep(Duration::from_millis(150)).await;
                continue;
            }

            if let Some(selected) = selected {
                let mut matching = candidates
                    .into_iter()
                    .filter(|(_, candidate)| candidate.id == selected.candidate_id)
                    .collect::<Vec<_>>();
                assert!(
                    matching.len() <= 1,
                    "retained source history candidate appeared more than once"
                );
                let Some((diagnostic_index, candidate)) = matching.pop() else {
                    sleep(Duration::from_millis(150)).await;
                    continue;
                };
                history_diagnostics[diagnostic_index].outcome = "retained-load-pending";
                let loaded = control
                    .request(
                        route.clone(),
                        NodeRequest::LoadHistory {
                            session: address.clone(),
                            candidate_id: selected.candidate_id.clone(),
                        },
                    )
                    .await
                    .unwrap();
                match loaded.response {
                    Ok(C2NodeResponse::HistoryLoaded {
                        session_id,
                        message_count,
                        completed_turn_count,
                        ..
                    }) => {
                        history_diagnostics[diagnostic_index].message_count = Some(message_count);
                        history_diagnostics[diagnostic_index].completed_turn_count =
                            completed_turn_count;
                        assert!(
                            source_history_metadata_matches_provider(
                                provider,
                                completed_turn_count,
                            ),
                            "retained source history completion metadata did not match its provider",
                        );
                        assert!(
                            history_session_id_matches_hint(
                                &session_id,
                                &candidate.session_id_hint,
                            ),
                            "retained native session ID did not match its history candidate",
                        );
                        assert_eq!(
                            session_id, selected.native_session_id,
                            "retained history candidate changed native session ID",
                        );
                        if source_history_progress_satisfied(
                            provider,
                            requirement,
                            message_count,
                            completed_turn_count,
                        ) {
                            history_diagnostics[diagnostic_index].outcome = "progressed";
                            eprintln!(
                                "source history progressed: provider={} stage={} native_session_id={} message_count={} completed_turn_count={:?}",
                                provider.label(),
                                stage,
                                session_id,
                                message_count,
                                completed_turn_count,
                            );
                            return SourceHistorySelection {
                                candidate_id: selected.candidate_id.clone(),
                                native_session_id: selected.native_session_id.clone(),
                                message_count,
                                completed_turn_count,
                            };
                        }
                        history_diagnostics[diagnostic_index].outcome =
                            source_history_pending_outcome(provider);
                    }
                    Err(failure) if failure.code == NodeFailureCode::BackendOperationFailed => {
                        history_diagnostics[diagnostic_index].outcome =
                            "backend-operation-failed";
                    }
                    Err(failure) if failure.code == NodeFailureCode::BackendBusy => {
                        history_diagnostics[diagnostic_index].outcome = "backend-busy";
                    }
                    Err(_) => {
                        history_diagnostics[diagnostic_index].outcome = "disallowed-error";
                        panic!("retained history candidate load failed with a disallowed code");
                    }
                    Ok(_) => {
                        history_diagnostics[diagnostic_index].outcome = "unexpected-ok";
                        panic!("retained history load returned an unexpected response variant");
                    }
                }
                sleep(Duration::from_millis(150)).await;
                continue;
            }

            let mut qualifying = Vec::new();
            for (diagnostic_index, candidate) in candidates {
                let candidate_id = candidate.id;
                let loaded = control
                    .request(
                        route.clone(),
                        NodeRequest::LoadHistory {
                            session: address.clone(),
                            candidate_id: candidate_id.clone(),
                        },
                    )
                    .await
                    .unwrap();
                match loaded.response {
                    Ok(C2NodeResponse::HistoryLoaded {
                        session_id,
                        message_count,
                        completed_turn_count,
                        ..
                    }) => {
                        history_diagnostics[diagnostic_index].message_count = Some(message_count);
                        history_diagnostics[diagnostic_index].completed_turn_count =
                            completed_turn_count;
                        assert!(
                            source_history_metadata_matches_provider(
                                provider,
                                completed_turn_count,
                            ),
                            "source history completion metadata did not match its provider",
                        );
                        assert!(
                            history_session_id_matches_hint(
                                &session_id,
                                &candidate.session_id_hint,
                            ),
                            "loaded native session ID did not match its history candidate",
                        );
                        if source_history_progress_satisfied(
                            provider,
                            requirement,
                            message_count,
                            completed_turn_count,
                        ) {
                            history_diagnostics[diagnostic_index].outcome = "qualified";
                            qualifying.push((candidate_id, session_id, diagnostic_index));
                        } else {
                            history_diagnostics[diagnostic_index].outcome =
                                source_history_pending_outcome(provider);
                        }
                    }
                    Err(failure) if failure.code == NodeFailureCode::BackendOperationFailed => {
                        history_diagnostics[diagnostic_index].outcome =
                            "backend-operation-failed";
                    }
                    Err(failure) if failure.code == NodeFailureCode::BackendBusy => {
                        history_diagnostics[diagnostic_index].outcome = "backend-busy";
                        sleep(Duration::from_millis(150)).await;
                        continue 'discover;
                    }
                    Err(_) => {
                        history_diagnostics[diagnostic_index].outcome = "disallowed-error";
                        panic!("history candidate load failed with a disallowed code");
                    }
                    Ok(_) => {
                        history_diagnostics[diagnostic_index].outcome = "unexpected-ok";
                        panic!("history load returned an unexpected response variant");
                    }
                }
            }
            if qualifying.is_empty() {
                sleep(Duration::from_millis(150)).await;
                continue;
            }
            assert_eq!(
                qualifying.len(),
                1,
                "fresh history qualification was ambiguous: qualifying_count={}",
                qualifying.len(),
            );
            let (candidate_id, expected_session_id, diagnostic_index) =
                qualifying.pop().unwrap();
            history_diagnostics[diagnostic_index].outcome = "reload-pending";
            let reloaded = control
                .request(
                    route.clone(),
                    NodeRequest::LoadHistory {
                        session: address.clone(),
                        candidate_id: candidate_id.clone(),
                    },
                )
                .await
                .unwrap();
            match reloaded.response {
                Ok(C2NodeResponse::HistoryLoaded {
                    session_id,
                    message_count,
                    completed_turn_count,
                    ..
                }) => {
                    history_diagnostics[diagnostic_index].outcome = "reloaded";
                    history_diagnostics[diagnostic_index].message_count = Some(message_count);
                    history_diagnostics[diagnostic_index].completed_turn_count =
                        completed_turn_count;
                    assert!(
                        source_history_metadata_matches_provider(
                            provider,
                            completed_turn_count,
                        ),
                        "reloaded source history completion metadata did not match its provider",
                    );
                    assert!(
                        session_id == expected_session_id
                            && source_history_progress_satisfied(
                                provider,
                                requirement,
                                message_count,
                                completed_turn_count,
                            ),
                        "qualified source history did not reload consistently",
                    );
                    eprintln!(
                        "source history selected: provider={} stage={} native_session_id={} message_count={} completed_turn_count={:?}",
                        provider.label(),
                        stage,
                        session_id,
                        message_count,
                        completed_turn_count,
                    );
                    return SourceHistorySelection {
                        candidate_id,
                        native_session_id: session_id,
                        message_count,
                        completed_turn_count,
                    };
                }
                _ => {
                    history_diagnostics[diagnostic_index].outcome = "reload-mismatch";
                    panic!("qualified source history did not reload consistently");
                }
            }
        }
    })
    .await;
    match result {
        Ok(selection) => selection,
        Err(_) => panic!(
            "source history progress timeout: provider={} stage={} required_message_count={} required_completed_turn_count={:?} candidates={}",
            provider.label(),
            stage,
            requirement.message_count,
            requirement.completed_turn_count,
            history_candidate_diagnostics_text(&history_diagnostics),
        ),
    }
}

fn command_from_tool_input(input_json: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(input_json).ok()?;
    value
        .get("command")
        .or_else(|| value.get("input"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn workdir_from_tool_input(input_json: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(input_json).ok()?;
    value
        .get("workdir")
        .or_else(|| value.get("cwd"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn command_matches_helper(
    command: &str,
    source_provider: Provider,
    source_session_id: &str,
) -> bool {
    let Some(tokens) = shell_tokens(command) else {
        return false;
    };
    let expected_len = if source_provider == Provider::Codex { 4 } else { 3 };
    tokens.len() == expected_len
        && tokens[0].eq_ignore_ascii_case(source_provider.helper_name())
        && tokens[1].eq_ignore_ascii_case("load")
        && tokens[2].eq_ignore_ascii_case(source_session_id)
        && (source_provider != Provider::Codex || tokens[3] == "--json")
}

fn json_object_from_output(output: &str) -> Option<Value> {
    serde_json::from_str(output.trim()).ok().or_else(|| {
        let start = output.find('{')?;
        let end = output.rfind('}')?.checked_add(1)?;
        serde_json::from_str(&output[start..end]).ok()
    })
}

fn decoded_tool_output(output: &str) -> String {
    match serde_json::from_str::<Value>(output.trim()) {
        Ok(Value::String(text)) => text,
        Ok(Value::Object(object)) => ["stdout", "output", "content", "text"]
            .into_iter()
            .find_map(|key| object.get(key).and_then(Value::as_str))
            .map(str::to_owned)
            .unwrap_or_else(|| output.to_owned()),
        _ => output.to_owned(),
    }
}

fn cargo_output_proves_one_test(output: &str) -> bool {
    let normalized = strip_ansi(&decoded_tool_output(output))
        .replace("\r\n", "\n")
        .to_ascii_lowercase();
    normalized.contains("running 1 test")
        && normalized.lines().any(|line| {
            let line = line.trim();
            line.starts_with("test result: ok.") && line.contains("1 passed; 0 failed")
        })
}

fn command_output_proves_one_test(output: &Output) -> bool {
    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    combined.push('\n');
    combined.push_str(&String::from_utf8_lossy(&output.stderr));
    cargo_output_proves_one_test(&combined)
}

fn strip_ansi(input: &str) -> String {
    enum State {
        Text,
        Escape,
        Csi,
    }

    let mut state = State::Text;
    let mut plain = String::with_capacity(input.len());
    for character in input.chars() {
        match state {
            State::Text if character == '\u{1b}' => state = State::Escape,
            State::Text => plain.push(character),
            State::Escape if character == '[' => state = State::Csi,
            State::Escape => state = State::Text,
            State::Csi if ('@'..='~').contains(&character) => state = State::Text,
            State::Csi => {}
        }
    }
    plain
}

fn claude_user_message_surface(output: &str) -> Option<(usize, usize)> {
    let lines = output.lines().collect::<Vec<_>>();
    let header_index = lines.iter().position(|line| line.contains("User Messages"))?;
    let header = lines[header_index];
    let count_start = header.rfind('(')?.checked_add(1)?;
    let count_end = header[count_start..].find(" messages)")? + count_start;
    let declared = header[count_start..count_end].trim().parse().ok()?;
    let mut surfaced = 0;
    for line in lines.iter().skip(header_index + 1) {
        let line = line.trim_start();
        if line.is_empty() {
            if surfaced > 0 {
                break;
            }
            continue;
        }
        let Some((number, message)) = line.split_once(". ") else {
            break;
        };
        if number.chars().all(|character| character.is_ascii_digit())
            && !number.is_empty()
            && !message.trim().is_empty()
        {
            surfaced += 1;
        } else {
            break;
        }
    }
    Some((declared, surfaced))
}

fn helper_output_proves_restore(
    output: &str,
    source_provider: Provider,
    source_session_id: &str,
    lane: LaneSpec,
) -> bool {
    if output.len() > MAX_HELPER_OUTPUT_BYTES {
        return false;
    }
    let output = strip_ansi(&decoded_tool_output(output));
    if !output.contains(source_session_id)
        || lane.semantic_terms.iter().any(|term| !output.contains(term))
    {
        return false;
    }
    if source_provider == Provider::Claude {
        let session_header = format!("Session: {source_session_id}");
        return output
            .lines()
            .any(|line| line.trim() == session_header)
            && output.lines().any(|line| line.trim_start().starts_with("Size:"))
            && output.lines().any(|line| line.trim_start().starts_with("Topic:"))
            && claude_user_message_surface(&output)
                .is_some_and(|(declared, surfaced)| declared >= 3 && surfaced >= 3);
    }
    json_object_from_output(&output).is_some_and(|report| {
        report.pointer("/meta/id").and_then(Value::as_str) == Some(source_session_id)
            && report
                .get("messages")
                .and_then(Value::as_array)
                .is_some_and(|messages| messages.len() >= 6)
    })
}

fn shell_tokens(command: &str) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    for character in command.chars() {
        if let Some(expected) = quote {
            if character == expected {
                quote = None;
            } else {
                current.push(character);
            }
            continue;
        }
        match character {
            '\'' | '"' => quote = Some(character),
            ';' | '&' | '|' | '>' | '<' | '`' | '\r' | '\n' => return None,
            character if character.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(character),
        }
    }
    if quote.is_some() {
        return None;
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    Some(tokens)
}

fn path_matches_workspace(value: &str, source_workspace: &Path) -> bool {
    value
        .replace('\\', "/")
        .trim_end_matches('/')
        .eq_ignore_ascii_case(
            source_workspace
                .to_string_lossy()
                .replace('\\', "/")
                .trim_end_matches('/'),
        )
}

fn token_sequence_matches(tokens: &[String], expected: &[&str]) -> bool {
    tokens.len() == expected.len()
        && tokens
            .iter()
            .zip(expected)
            .all(|(actual, expected)| actual.as_str() == *expected)
}

fn source_revision_matches(candidate: &str, source_revision: &str) -> bool {
    candidate == source_revision
        || (candidate.len() >= 7
            && candidate.len() < source_revision.len()
            && candidate.chars().all(|character| character.is_ascii_hexdigit())
            && source_revision.starts_with(candidate))
}

fn source_git_command(
    command: &str,
    workdir: Option<&str>,
    source_workspace: &Path,
    source_revision: &str,
) -> Option<TargetToolEvidence> {
    let tokens = shell_tokens(command)?;
    if tokens.is_empty() || !tokens[0].eq_ignore_ascii_case("git") {
        return None;
    }
    let subcommand_index = if tokens.len() >= 4
        && tokens[1] == "-C"
        && path_matches_workspace(&tokens[2], source_workspace)
    {
        3
    } else if tokens.len() >= 2
        && workdir.is_some_and(|workdir| path_matches_workspace(workdir, source_workspace))
    {
        1
    } else {
        return None;
    };
    let arguments = &tokens[subcommand_index..];
    if token_sequence_matches(arguments, &["status", "--short"]) {
        Some(TargetToolEvidence::SourceGitStatus)
    } else if token_sequence_matches(
        arguments,
        &["log", "--oneline", "--decorate", "-n", "20"],
    ) {
        Some(TargetToolEvidence::SourceGitLog)
    } else if arguments.len() == 3
        && arguments[0] == "show"
        && arguments[1] == "--stat"
        && source_revision_matches(&arguments[2], source_revision)
    {
        Some(TargetToolEvidence::SourceGitShow)
    } else {
        None
    }
}

fn source_log_output_proves_revision(
    output: &str,
    source_revision: &str,
    lane: LaneSpec,
) -> bool {
    let output = strip_ansi(&decoded_tool_output(output));
    output.lines().any(|line| {
        let Some((revision, subject)) = line.trim().split_once(' ') else {
            return false;
        };
        source_revision_matches(revision, source_revision)
            && subject.contains(lane.source_decision)
    })
}

fn source_show_output_proves_revision(
    output: &str,
    source_revision: &str,
    lane: LaneSpec,
) -> bool {
    let output = strip_ansi(&decoded_tool_output(output));
    output.contains(source_revision)
        && output.contains(lane.source_decision)
        && output.contains("HANDOFF.md")
}

fn command_is_named_test(command: &str, lane: LaneSpec) -> bool {
    shell_tokens(command).is_some_and(|tokens| {
        shell_tokens(lane.test_command).is_some_and(|expected| tokens == expected)
    })
}

fn command_is_exact_commit(command: &str, lane: LaneSpec) -> bool {
    let Some(tokens) = shell_tokens(command) else {
        return false;
    };
    tokens.len() == 4
        && tokens[0].eq_ignore_ascii_case("git")
        && tokens[1].eq_ignore_ascii_case("commit")
        && tokens[2] == "-m"
        && tokens[3] == lane.commit_subject
}

fn assert_target_provider_events(
    events: &[(SessionAddress, ProviderSource, ProviderEvent)],
    address: &SessionAddress,
    source_session_id: &str,
    lane: LaneSpec,
    source_workspace: &Path,
    source_revision: &str,
    target_prompt: &str,
) {
    let mut evidence = TargetTranscriptEvidence::default();
    let mut first_executable_seen = false;
    let mut helper_completed = false;
    let mut pending = BTreeMap::<String, TargetToolEvidence>::new();
    for (_, source, event) in events.iter().filter(|(seen, source, _)| {
        seen == address && provider_event_source_matches(source, lane.target_provider)
    }) {
        match event {
            ProviderEvent::TurnStarted { prompt } => {
                evidence.exact_target_prompt_seen |= provider_hook_source_matches(
                    source,
                    lane.target_provider,
                ) && prompt.as_deref() == Some(target_prompt);
            }
            ProviderEvent::ToolStarted { id, name, input_json, .. } => {
                let normalized_input = input_json.to_ascii_lowercase();
                evidence.context_pack_seen |= normalized_input.contains("context-pack")
                    || normalized_input.contains("contextpack")
                    || normalized_input.contains("gate4agent_context_root");
                if name.eq_ignore_ascii_case("skill") {
                    continue;
                }
                let command = command_from_tool_input(input_json);
                let workdir = workdir_from_tool_input(input_json);
                if !first_executable_seen {
                    evidence.first_action_used_expected_helper = command.as_deref().is_some_and(
                        |command| {
                            command_matches_helper(
                                command,
                                lane.source_provider,
                                source_session_id,
                            )
                        },
                    );
                    if evidence.first_action_used_expected_helper {
                        assert!(
                            pending
                                .insert(id.clone(), TargetToolEvidence::Helper)
                                .is_none(),
                            "target reused a provider tool ID",
                        );
                    }
                    first_executable_seen = true;
                }
                if !helper_completed {
                    continue;
                }
                if let Some(command) = command {
                    let matched = source_git_command(
                        &command,
                        workdir.as_deref(),
                        source_workspace,
                        source_revision,
                    )
                    .or_else(|| {
                        command_is_named_test(&command, lane)
                            .then_some(TargetToolEvidence::NamedTest)
                    })
                    .or_else(|| {
                        command_is_exact_commit(&command, lane)
                            .then_some(TargetToolEvidence::Commit)
                    });
                    if let Some(matched) = matched {
                        assert!(
                            pending.insert(id.clone(), matched).is_none(),
                            "target reused a provider tool ID",
                        );
                    }
                }
            }
            ProviderEvent::ToolCompleted { id, output, is_error, .. } => {
                let Some(matched) = pending.remove(id) else {
                    continue;
                };
                assert!(
                    !is_error,
                    "a matched target tool command completed with an error",
                );
                match matched {
                    TargetToolEvidence::Helper => {
                        evidence.helper_output_seen = helper_output_proves_restore(
                            output,
                            lane.source_provider,
                            source_session_id,
                            lane,
                        );
                        helper_completed = evidence.helper_output_seen;
                    }
                    TargetToolEvidence::SourceGitStatus => {
                        evidence.source_git_status_seen = decoded_tool_output(output).trim().is_empty();
                    }
                    TargetToolEvidence::SourceGitLog => {
                        evidence.source_git_log_seen = source_log_output_proves_revision(
                            output,
                            source_revision,
                            lane,
                        );
                    }
                    TargetToolEvidence::SourceGitShow => {
                        evidence.source_git_show_seen = source_show_output_proves_revision(
                            output,
                            source_revision,
                            lane,
                        );
                    }
                    TargetToolEvidence::NamedTest => {
                        evidence.named_test_seen = cargo_output_proves_one_test(output);
                    }
                    TargetToolEvidence::Commit => evidence.commit_seen = true,
                }
            }
            ProviderEvent::Text { text, .. } => {
                evidence.assistant_text_seen |= !text.trim().is_empty();
            }
            _ => {}
        }
    }
    assert!(pending.is_empty(), "a matched target tool call had no completion");
    assert!(
        evidence.exact_target_prompt_seen,
        "target hook stream omitted the exact submitted skill prompt",
    );
    assert!(
        evidence.first_action_used_expected_helper,
        "target's first executable action did not invoke the exact source parser",
    );
    assert!(
        evidence.helper_output_seen,
        "target transcript did not correlate the helper call with a bounded semantic report",
    );
    assert!(evidence.source_git_status_seen, "target omitted exact source Git status");
    assert!(evidence.source_git_log_seen, "target omitted exact source Git log");
    assert!(evidence.source_git_show_seen, "target omitted exact source Git show");
    assert!(evidence.named_test_seen, "target transcript omitted the agreed named test");
    assert!(evidence.commit_seen, "target transcript omitted the agreed commit command");
    assert!(!evidence.context_pack_seen, "target used automatic ContextPack data");
    assert!(evidence.assistant_text_seen, "target emitted no ordinary assistant response");
}

fn direct_directories(root: &Path) -> Vec<PathBuf> {
    if !root.exists() {
        return Vec::new();
    }
    let mut directories = std::fs::read_dir(root)
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            entry
                .file_type()
                .ok()
                .filter(|kind| kind.is_dir())
                .map(|_| entry.path())
        })
        .collect::<Vec<_>>();
    directories.sort();
    directories
}

async fn only_directory(root: &Path, label: &str) -> PathBuf {
    timeout(Duration::from_secs(15), async {
        loop {
            let directories = direct_directories(root);
            if directories.len() == 1 {
                return directories[0].clone();
            }
            assert!(directories.len() <= 1, "{label} created multiple directories");
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{label} was not materialized"))
}

async fn stop_session(
    control: &C2ControlHandle,
    route: &NodeRoute,
    address: &SessionAddress,
) {
    assert!(matches!(
        control
            .request(
                route.clone(),
                NodeRequest::Stop {
                    session: address.clone(),
                    force: true,
                },
            )
            .await
            .unwrap()
            .response,
        Ok(C2NodeResponse::Accepted)
    ));
    timeout(Duration::from_secs(20), async {
        loop {
            let current = snapshot(control, route).await;
            if find_session(&current, address).is_some_and(|session| {
                matches!(
                    session.status,
                    C2SessionStatus::Exited { .. } | C2SessionStatus::Failed
                )
            }) {
                return;
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("session did not stop");
}

async fn remove_session(
    control: &C2ControlHandle,
    route: &NodeRoute,
    address: &SessionAddress,
) {
    assert!(matches!(
        control
            .request(
                route.clone(),
                NodeRequest::Remove {
                    session: address.clone(),
                },
            )
            .await
            .unwrap()
            .response,
        Ok(C2NodeResponse::Accepted)
    ));
    timeout(Duration::from_secs(20), async {
        loop {
            if find_session(&snapshot(control, route).await, address).is_none() {
                return;
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("removed session remained in the C2 snapshot");
}

async fn forget_record(
    control: &C2ControlHandle,
    route: &NodeRoute,
    record: &C2ManagedSessionRecord,
) {
    assert!(matches!(
        control
            .request(
                route.clone(),
                NodeRequest::ForgetSessionRecord {
                    record_id: record.record_id.clone(),
                },
            )
            .await
            .unwrap()
            .response,
        Ok(C2NodeResponse::SessionRecordForgotten { ref record_id })
            if record_id == &record.record_id
    ));
}

async fn run_source_session(
    control: &C2ControlHandle,
    route: &NodeRoute,
    http: &C2Client,
    ids: &TestIds,
    lane: LaneSpec,
    workspace_id: &WorkspaceId,
    profile_id: &SpawnProfileId,
    profile_revision: &SpawnProfileRevision,
    source_repository: &Path,
    source_base: &str,
    idempotency: &str,
) -> SourceSession {
    let started_at_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let response = control
        .request(
            route.clone(),
            NodeRequest::SpawnSpec {
                spec: source_spawn_spec(
                    &ids.node,
                    workspace_id,
                    profile_id,
                    profile_revision,
                    idempotency,
                ),
            },
        )
        .await
        .unwrap();
    let receipt = match response.response {
        Ok(C2NodeResponse::SpawnSpecAccepted { receipt }) => receipt,
        _ => panic!("source PTY spawn returned an unexpected response variant"),
    };
    assert!(receipt.bundle_id.is_none() && receipt.bundle.is_none());
    assert!(receipt.context_id.is_none() && receipt.context.is_none());
    assert!(!receipt.prompt.present && receipt.prompt.byte_len == 0);
    assert_eq!(receipt.provenance.prompt, SpawnFieldProvenance::Cleared);
    assert_eq!(receipt.provenance.bundle_id, SpawnFieldProvenance::Cleared);
    assert_eq!(receipt.provenance.context_id, SpawnFieldProvenance::Cleared);
    let stage = format!("source-{}-turn-0-startup", lane.source_provider.label());
    wait_turn_ready(
        control,
        route,
        &receipt.session,
        None,
        &stage,
        lane.source_provider,
        Some(source_repository),
    )
    .await;
    let stage = format!("source-{}-turn-1", lane.source_provider.label());
    submit_turn(
        control,
        route,
        &receipt.session,
        lane.turns[0],
        &stage,
        lane.source_provider,
    )
    .await;
    let stage = format!("source-{}-turn-1-history", lane.source_provider.label());
    let mut source_history = wait_source_history_progress(
        control,
        route,
        &receipt.session,
        started_at_unix_ms,
        lane.source_provider,
        None,
        &stage,
    )
    .await;
    let record = wait_record(control, route, &receipt.session, lane.source_provider).await;
    let record = rename_record(
        control,
        route,
        http,
        &ids.node,
        &record,
        lane.source_name,
    )
    .await;
    let stage = format!("source-{}-turn-2", lane.source_provider.label());
    submit_turn(
        control,
        route,
        &receipt.session,
        lane.turns[1],
        &stage,
        lane.source_provider,
    )
    .await;
    let stage = format!("source-{}-turn-2-history", lane.source_provider.label());
    source_history = wait_source_history_progress(
        control,
        route,
        &receipt.session,
        started_at_unix_ms,
        lane.source_provider,
        Some(&source_history),
        &stage,
    )
    .await;
    let stage = format!("source-{}-turn-3", lane.source_provider.label());
    submit_turn(
        control,
        route,
        &receipt.session,
        lane.turns[2],
        &stage,
        lane.source_provider,
    )
    .await;
    let stage = format!("source-{}-turn-3-history", lane.source_provider.label());
    source_history = wait_source_history_progress(
        control,
        route,
        &receipt.session,
        started_at_unix_ms,
        lane.source_provider,
        Some(&source_history),
        &stage,
    )
    .await;
    assert!(source_history.message_count >= 6);
    match lane.source_provider {
        Provider::Claude => assert!(
            source_history
                .completed_turn_count
                .is_some_and(|completed| completed >= 3),
            "Claude source did not complete all three turns",
        ),
        Provider::Codex => assert!(source_history.completed_turn_count.is_none()),
    }
    let native_session_id = source_history.native_session_id;
    assert_source_unchanged(source_repository, source_base);
    assert_no_context(&snapshot(control, route).await);
    SourceSession {
        address: receipt.session,
        record,
        native_session_id,
        source_revision: source_base.to_owned(),
    }
}

fn target_prompt(lane: LaneSpec, source_session_id: &str, source_workspace: &Path) -> String {
    let source = match lane.source_provider {
        Provider::Claude => "claude",
        Provider::Codex => "codex",
    };
    let skill = match lane.target_provider {
        Provider::Codex => "$codex-restore-session",
        Provider::Claude => "/restore-session",
    };
    let workspace = source_workspace.to_string_lossy().replace('\\', "/");
    let prompt = format!(
        "{skill} source={source} session={source_session_id} source_workspace=\"{workspace}\" continue agreed unfinished work",
    );
    for forbidden in [
        lane.test_name,
        lane.commit_subject,
        lane.expected_files[0],
        lane.expected_files[1],
        lane.semantic_terms[0],
        lane.semantic_terms[1],
    ] {
        assert!(
            !prompt.contains(forbidden),
            "target prompt leaked source task semantics",
        );
    }
    assert!(!prompt.to_ascii_lowercase().contains("contextpack"));
    assert!(!prompt.to_ascii_lowercase().contains("context-pack"));
    prompt
}

async fn wait_target_commit(
    control: &C2ControlHandle,
    route: &NodeRoute,
    address: &SessionAddress,
    worktree_root: &Path,
    base: &str,
    lane: LaneSpec,
) {
    timeout(Duration::from_secs(300), async {
        loop {
            let current = snapshot(control, route).await;
            let session = find_session(&current, address).expect("managed target disappeared");
            if matches!(
                session.status,
                C2SessionStatus::Failed | C2SessionStatus::Exited { .. }
            ) {
                panic!("managed target stopped before committing restored work");
            }
            let head = git_output(worktree_root, &["rev-parse", "HEAD"]);
            let subject = git_output(worktree_root, &["log", "-1", "--format=%s"]);
            let status = git_output(
                worktree_root,
                &["status", "--porcelain=v1", "--untracked-files=all"],
            );
            if head.status.success()
                && subject.status.success()
                && status.status.success()
                && String::from_utf8_lossy(&head.stdout).trim() != base
                && String::from_utf8_lossy(&subject.stdout).trim() == lane.commit_subject
                && status.stdout.is_empty()
                && worktree_root.join(lane.test_file).is_file()
            {
                return;
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("managed target did not produce the exact clean commit");
}

async fn run_target_session(
    control: &C2ControlHandle,
    route: &NodeRoute,
    http: &C2Client,
    ids: &TestIds,
    lane: LaneSpec,
    workspace_id: &WorkspaceId,
    profile_id: &SpawnProfileId,
    profile_revision: &SpawnProfileRevision,
    worktree_profile_id: &str,
    source: &SourceSession,
    source_workspace: &Path,
    target_repository: &Path,
    target_base: &str,
    allocation_root: &Path,
    provider_events: &ProviderEventLog,
    idempotency: &str,
) -> TargetSession {
    let prompt = target_prompt(lane, &source.native_session_id, source_workspace);
    let response = control
        .request(
            route.clone(),
            NodeRequest::SpawnManagedWorktree {
                request: target_spawn_request(
                    &ids.node,
                    workspace_id,
                    profile_id,
                    profile_revision,
                    worktree_profile_id,
                    idempotency,
                ),
            },
        )
        .await
        .unwrap();
    let receipt = match response.response {
        Ok(C2NodeResponse::ManagedWorktreeSpawnAccepted { receipt }) => receipt,
        _ => panic!("managed target PTY spawn returned an unexpected response variant"),
    };
    assert!(receipt.spawn.bundle_id.is_none() && receipt.spawn.bundle.is_none());
    assert_eq!(receipt.spawn.provenance.bundle_id, SpawnFieldProvenance::Cleared);
    assert!(!receipt.spawn.prompt.present && receipt.spawn.prompt.byte_len == 0);
    assert_eq!(receipt.spawn.provenance.prompt, SpawnFieldProvenance::Cleared);
    assert!(receipt.spawn.context_id.is_none() && receipt.spawn.context.is_none());
    assert_eq!(receipt.spawn.provenance.context_id, SpawnFieldProvenance::Cleared);
    assert!(receipt.spawn.context_binding_is_valid());
    assert_eq!(receipt.lease.retention, ManagedWorktreeRetention::RemoveWhenReleased);

    let worktree_root = only_directory(allocation_root, "managed target worktree").await;
    let record = wait_record(control, route, &receipt.spawn.session, lane.target_provider).await;
    let record = rename_record(
        control,
        route,
        http,
        &ids.node,
        &record,
        lane.target_name,
    )
    .await;
    let stage = format!("target-{}-turn-0-startup", lane.target_provider.label());
    wait_turn_ready(
        control,
        route,
        &receipt.spawn.session,
        None,
        &stage,
        lane.target_provider,
        Some(&worktree_root),
    )
    .await;
    let stage = format!("target-{}-turn-1", lane.target_provider.label());
    submit_turn(
        control,
        route,
        &receipt.spawn.session,
        &prompt,
        &stage,
        lane.target_provider,
    )
    .await;
    wait_target_commit(
        control,
        route,
        &receipt.spawn.session,
        &worktree_root,
        target_base,
        lane,
    )
    .await;
    assert_no_context(&snapshot(control, route).await);
    let target_head = assert_git_success(&worktree_root, &["rev-parse", "HEAD"]);
    let target_head = String::from_utf8(target_head.stdout).unwrap().trim().to_owned();
    let target_branch = assert_git_success(
        &worktree_root,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
    );
    let target_branch = String::from_utf8(target_branch.stdout)
        .unwrap()
        .trim()
        .to_owned();
    assert!(!target_branch.is_empty() && !target_branch.starts_with('-'));

    stop_session(control, route, &receipt.spawn.session).await;
    timeout(Duration::from_secs(20), async {
        loop {
            let stopped = snapshot(control, route).await;
            let session_stopped = find_session(&stopped, &receipt.spawn.session).is_some_and(
                |session| {
                    matches!(
                        session.status,
                        C2SessionStatus::Exited { .. } | C2SessionStatus::Failed
                    )
                },
            );
            let record_detached = stopped.session_records.iter().any(|candidate| {
                candidate.record_id == record.record_id
                    && candidate.state == ManagedSessionState::Unavailable
                    && candidate.active_session.is_none()
                    && !candidate.provider_identity_present
            });
            let lease_retained = stopped.managed_worktrees.iter().any(|lease| {
                lease.lease_id == receipt.lease.lease_id
                    && lease.active_session_count == 1
                    && lease.managed_record_count == 0
            });
            if session_stopped && record_detached && lease_retained {
                return;
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("stopped target did not retain its raw record and managed worktree lease");
    assert!(worktree_root.is_dir());

    let observed = timeout(Duration::from_secs(10), async {
        let mut last_len = usize::MAX;
        let mut stable = 0_u8;
        loop {
            let current = provider_events
                .lock()
                .unwrap()
                .iter()
                .filter(|(seen, _, _)| seen == &receipt.spawn.session)
                .cloned()
                .collect::<Vec<_>>();
            if current.len() == last_len {
                stable = stable.saturating_add(1);
            } else {
                last_len = current.len();
                stable = 0;
            }
            if stable >= 8 {
                return current;
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("provider events were not flushed after physical exit");
    assert_target_provider_events(
        &observed,
        &receipt.spawn.session,
        &source.native_session_id,
        lane,
        source_workspace,
        &source.source_revision,
        &prompt,
    );
    assert_target_result(&worktree_root, target_base, lane);
    assert_source_unchanged(target_repository, target_base);
    let head_after_oracle = assert_git_success(&worktree_root, &["rev-parse", "HEAD"]);
    assert_eq!(String::from_utf8(head_after_oracle.stdout).unwrap().trim(), target_head);
    let status_after_oracle = assert_git_success(
        &worktree_root,
        &["status", "--porcelain=v1", "--untracked-files=all"],
    );
    assert!(status_after_oracle.stdout.is_empty());

    remove_session(control, route, &receipt.spawn.session).await;
    timeout(Duration::from_secs(20), async {
        loop {
            let current = snapshot(control, route).await;
            let record_retained = current.session_records.iter().any(|candidate| {
                candidate.record_id == record.record_id
                    && candidate.state == ManagedSessionState::Unavailable
                    && candidate.active_session.is_none()
                    && !candidate.provider_identity_present
            });
            let lease_absent = current
                .managed_worktrees
                .iter()
                .all(|lease| lease.lease_id != receipt.lease.lease_id);
            if record_retained
                && lease_absent
                && !worktree_root.exists()
                && direct_directories(allocation_root).is_empty()
            {
                return;
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("managed target cleanup did not release its worktree");
    let branch_ref = format!("refs/heads/{target_branch}");
    let retained = assert_git_success(target_repository, &["rev-parse", &branch_ref]);
    assert_eq!(String::from_utf8(retained.stdout).unwrap().trim(), target_head);
    let retained_subject = assert_git_success(
        target_repository,
        &["show", "-s", "--format=%s", &branch_ref],
    );
    assert_eq!(
        String::from_utf8(retained_subject.stdout).unwrap().trim(),
        lane.commit_subject,
    );
    let worktree_list = assert_git_success(target_repository, &["worktree", "list", "--porcelain"]);
    let listed_roots = String::from_utf8(worktree_list.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    assert_eq!(listed_roots.len(), 1);
    assert_eq!(
        std::fs::canonicalize(&listed_roots[0]).unwrap(),
        std::fs::canonicalize(target_repository).unwrap(),
    );
    forget_record(control, route, &record).await;
    TargetSession {
        record,
        worktree_root,
    }
}

#[test]
fn claude_draft_prefix_is_nonempty_control_free_and_bounded() {
    assert_eq!(control_free_prompt_prefix("visible prompt"), Some("visible prompt".to_owned()));
    assert_eq!(
        control_free_prompt_prefix(&"x".repeat(40)).unwrap().chars().count(),
        32,
    );
    assert!(control_free_prompt_prefix("").is_none());
    assert!(control_free_prompt_prefix("draft\nline").is_none());
}

#[test]
fn startup_gate_actions_require_exact_selected_safe_option_and_enter_key() {
    let workspace = Path::new(r"C:\fixture\workspace");
    let trust = format!(
        "Quick safety check: Is this a project you trust?\n{}\n❯ 1. Yes, I trust this folder\n2. No, continue without these permissions\nEnter to confirm · Esc to go back",
        workspace.display(),
    );
    assert_eq!(exact_startup_gate(&trust), Some(ExactStartupGate::WorkspaceTrust));
    assert!(claude_trust_enter_is_proven(&trust, workspace));
    assert!(!claude_trust_enter_is_proven(
        &trust,
        Path::new(r"C:\fixture\other"),
    ));
    assert!(!claude_trust_enter_is_proven(
        &trust.replace("❯ 1. Yes", "1. Yes"),
        workspace,
    ));

    let update = "Kimi Code Update Available\nInstall update now\n❯ Skip for now";
    assert_eq!(
        exact_startup_gate(update),
        Some(ExactStartupGate::VendorUpdate),
    );
    assert!(known_noncomposer_operator_screen(update));
}

#[test]
fn codex_trust_gate_requires_exact_workspace_selection_and_footer() {
    let workspace = Path::new(r"C:\fixture\codex-workspace");
    let trust = format!(
        "> You are in {}\nDo you trust the contents of this directory? Working with untrusted contents comes with higher risk of prompt injection.\n› 1. Yes, continue\n2. No, quit\nPress enter to continue",
        workspace.display(),
    );
    assert_eq!(exact_startup_gate(&trust), Some(ExactStartupGate::WorkspaceTrust));
    assert!(codex_trust_enter_is_proven(&trust, workspace));
    assert!(!codex_trust_enter_is_proven(
        &trust,
        Path::new(r"C:\fixture\other"),
    ));
    assert!(!codex_trust_enter_is_proven(
        &trust.replace(
            "Do you trust the contents of this directory? Working with untrusted contents comes with higher risk of prompt injection.",
            "Do you trust the contents of this directory?",
        ),
        workspace,
    ));
    assert!(!codex_trust_enter_is_proven(
        &trust.replace("› 1. Yes, continue", "1. Yes, continue"),
        workspace,
    ));
    assert!(!codex_trust_enter_is_proven(
        &trust.replace("2. No, quit", "2. No, continue"),
        workspace,
    ));
    assert!(!codex_trust_enter_is_proven(
        &trust.replace("Press enter to continue", "Enter to continue"),
        workspace,
    ));
}

#[test]
fn provider_composer_gate_requires_each_vendor_prompt_glyph() {
    assert!(!expected_provider_composer_visible(Provider::Claude, ">"));
    assert!(expected_provider_composer_visible(Provider::Claude, "❯"));
    assert!(!expected_provider_composer_visible(Provider::Codex, "❯"));
    assert!(expected_provider_composer_visible(Provider::Codex, "›"));
    assert!(!expected_provider_composer_visible(
        Provider::Claude,
        "Quick safety check\n❯ 1. Yes, I trust this folder",
    ));
    assert!(!expected_provider_composer_visible(
        Provider::Claude,
        "Welcome to Claude Code\n❯ Press Enter to continue",
    ));
}

#[test]
fn history_session_identity_requires_uuid_and_exact_or_suffix_hint() {
    let session_id = "01234567-89ab-cdef-0123-456789abcdef";
    assert!(history_session_id_matches_hint(session_id, session_id));
    assert!(history_session_id_matches_hint(
        session_id,
        &format!("rollout-{session_id}"),
    ));
    assert!(!history_session_id_matches_hint(
        session_id,
        &format!("{session_id}-worker"),
    ));
    assert!(!history_session_id_matches_hint(
        "not-a-native-uuid",
        "not-a-native-uuid",
    ));
}

#[test]
fn codex_submit_prompt_encoder_preserves_bracketed_paste_across_terminal_byte_chunks() {
    let text = "x".repeat(MAX_NODE_TERMINAL_BYTES + 1);
    let prepared = prepare_input(InputAction::SubmitPrompt(PromptPayload {
        text,
        framing: PromptFraming::BracketedPaste,
    }))
    .unwrap();
    assert_eq!(prepared.kind(), PreparedInputKind::SubmitPrompt);
    let writes = prepared.writes();
    assert_eq!(
        writes.iter().map(|write| write.kind).collect::<Vec<_>>(),
        vec![
            PreparedWriteKind::Framing,
            PreparedWriteKind::Data,
            PreparedWriteKind::Framing,
            PreparedWriteKind::Submit,
        ],
    );
    assert_eq!(writes[0].bytes, BRACKETED_PASTE_START);
    assert_eq!(writes[2].bytes, BRACKETED_PASTE_END);
    assert_eq!(writes[3].bytes, b"\r");
    assert_eq!(writes[3].delay_before_ms, TERMINAL_SUBMIT_DELAY_MS);
    assert!(writes[..3].iter().all(|write| write.delay_before_ms == 0));

    let expected = writes
        .iter()
        .flat_map(|write| write.bytes.iter().copied())
        .collect::<Vec<_>>();
    let chunks = writes
        .iter()
        .flat_map(|write| bounded_terminal_chunks(&write.bytes))
        .collect::<Vec<_>>();
    assert_eq!(chunks.len(), 5);
    assert!(chunks
        .iter()
        .all(|chunk| !chunk.is_empty() && chunk.len() <= MAX_NODE_TERMINAL_BYTES));
    assert_eq!(chunks.into_iter().flatten().collect::<Vec<_>>(), expected);
}

#[test]
fn turn_readiness_counts_ready_samples_independently_of_frame_changes() {
    let mut ready_samples = 0;
    for _ in 0..8 {
        ready_samples = next_ready_sample_count(ready_samples, true, true);
    }
    assert_eq!(ready_samples, 8);
    assert_eq!(next_ready_sample_count(ready_samples, true, false), 0);
    assert_eq!(next_ready_sample_count(7, false, true), 0);
}

#[test]
fn source_history_progression_requires_two_new_messages_per_turn() {
    assert_eq!(required_source_message_count(None), 2);
    let first = SourceHistorySelection {
        candidate_id: "candidate".to_owned(),
        native_session_id: "01234567-89ab-cdef-0123-456789abcdef".to_owned(),
        message_count: 6,
    };
    assert_eq!(required_source_message_count(Some(&first)), 8);
    assert!(first.message_count < required_source_message_count(Some(&first)));
    let second = SourceHistorySelection {
        message_count: 8,
        ..first
    };
    assert_eq!(required_source_message_count(Some(&second)), 10);
}

#[test]
fn private_root_restores_marker_after_final_remove_failure() {
    let mut root = PrivateRoot::create(test_root());
    std::fs::remove_file(root.path.join(PRIVATE_ROOT_MARKER)).unwrap();
    let removed = root.remove_empty_root_with(|_| {
        Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
    });
    assert!(removed.is_err());
    assert!(root.owns_literal_root());
    root.remove();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires installed Claude and Codex CLIs, GATE4AGENT_LIVE_SKILL_RESTORE=1, and windows-headless-supervisor"]
async fn windows_live_skill_restore_continues_two_cross_provider_worktrees() {
    require_live_restore();
    assert_installed_skills_match_canonical();

    let root = test_root();
    let mut root_cleanup = PrivateRoot::create(root.clone());
    let claude_history_root = user_profile().join(".claude").join("projects");
    let codex_history_root = user_profile().join(".codex").join("sessions");
    let claude_source_repository = root.join("claude-display-planning-repository");
    let codex_target_repository = root.join("codex-display-target-repository");
    let codex_source_repository = root.join("codex-tags-planning-repository");
    let claude_target_repository = root.join("claude-tags-target-repository");
    let claude_source_base = initialize_repository(&claude_source_repository, DISPLAY_LANE, true);
    let codex_target_base = initialize_repository(&codex_target_repository, DISPLAY_LANE, false);
    let codex_source_base = initialize_repository(&codex_source_repository, TAGS_LANE, true);
    let claude_target_base = initialize_repository(&claude_target_repository, TAGS_LANE, false);
    let codex_allocation_root = root.join("codex-managed-worktree");
    let claude_allocation_root = root.join("claude-managed-worktree");
    std::fs::create_dir_all(&codex_allocation_root).unwrap();
    std::fs::create_dir_all(&claude_allocation_root).unwrap();

    let ids = TestIds::new();
    let node_endpoint = endpoint("node");
    let control_endpoint = endpoint("control");
    let node_token = "live-skill-restore-node-token";
    let c2_token = "live-skill-restore-c2-token";
    let server = NodeServer::new(
        node_config(
            &node_endpoint,
            node_token,
            &ids,
            &claude_source_repository,
            &codex_target_repository,
            &codex_source_repository,
            &claude_target_repository,
            &codex_allocation_root,
            &claude_allocation_root,
            &root.join("node-state.json"),
            &claude_history_root,
            &codex_history_root,
        ),
    )
    .unwrap();
    let node_shutdown = server.shutdown_handle();
    let node_task = tokio::spawn(server.run());

    let c2_config = C2Config::new(
        "127.0.0.1:0".parse().unwrap(),
        c2_token,
        vec![C2NodeConfig::new(ids.node.clone(), &node_endpoint, node_token).unwrap()],
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
    let route = wait_online(&http, &ids.node).await;
    let (control, mut events) = connect_local(&control_endpoint, c2_token).await.unwrap();
    wait_relay_ready(&control, &route).await;
    let mut node_observer = NamedPipeNodeClient::connect(
        &node_endpoint,
        &ids.node,
        ClientRole::Observer,
        node_token,
    )
    .await
    .unwrap();
    let provider_events = Arc::new(Mutex::new(Vec::new()));
    let provider_events_writer = Arc::clone(&provider_events);
    let provider_event_collector = tokio::spawn(async move {
        while let Ok(frame) = node_observer.recv().await {
            let ServerFrame::Event(envelope) = frame else {
                continue;
            };
            let NodeEvent::Control { address, event } = envelope.event else {
                continue;
            };
            let ControlEventKind::ProviderEvent { source, event, .. } = event.event else {
                continue;
            };
            let mut events = provider_events_writer.lock().unwrap();
            assert!(events.len() < 20_000, "provider event evidence exceeded its bound");
            events.push((address, source, event));
        }
    });
    let renamed_events = Arc::new(Mutex::new(Vec::new()));
    let renamed_events_writer = Arc::clone(&renamed_events);
    let event_collector = tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            if let C2NodeEvent::SessionRecordUpserted { record } = event.event {
                renamed_events_writer
                    .lock()
                    .unwrap()
                    .push((
                        record.record_id,
                        record.display_name,
                        record.provider_identity_present,
                    ));
            }
        }
    });

    let claude_source = run_source_session(
        &control,
        &route,
        &http,
        &ids,
        DISPLAY_LANE,
        &ids.claude_source_workspace,
        &ids.claude_source_profile,
        &SpawnProfileRevision::new("claude-source-r1").unwrap(),
        &claude_source_repository,
        &claude_source_base,
        "claude-display-source-once",
    )
    .await;
    let codex_source = run_source_session(
        &control,
        &route,
        &http,
        &ids,
        TAGS_LANE,
        &ids.codex_source_workspace,
        &ids.codex_source_profile,
        &SpawnProfileRevision::new("codex-source-r1").unwrap(),
        &codex_source_repository,
        &codex_source_base,
        "codex-tags-source-once",
    )
    .await;

    let codex_target = run_target_session(
        &control,
        &route,
        &http,
        &ids,
        DISPLAY_LANE,
        &ids.codex_target_workspace,
        &ids.codex_target_profile,
        &SpawnProfileRevision::new("codex-target-r1").unwrap(),
        "codex-display-implementation",
        &claude_source,
        &claude_source_repository,
        &codex_target_repository,
        &codex_target_base,
        &codex_allocation_root,
        &provider_events,
        "codex-display-target-once",
    )
    .await;
    assert!(!codex_target.worktree_root.exists());
    let claude_target = run_target_session(
        &control,
        &route,
        &http,
        &ids,
        TAGS_LANE,
        &ids.claude_target_workspace,
        &ids.claude_target_profile,
        &SpawnProfileRevision::new("claude-target-r1").unwrap(),
        "claude-tags-implementation",
        &codex_source,
        &codex_source_repository,
        &claude_target_repository,
        &claude_target_base,
        &claude_allocation_root,
        &provider_events,
        "claude-tags-target-once",
    )
    .await;
    assert!(!claude_target.worktree_root.exists());
    assert_source_unchanged(&claude_source_repository, &claude_source_base);
    assert_source_unchanged(&codex_source_repository, &codex_source_base);

    let active_sources = snapshot(&control, &route).await;
    for source in [&claude_source, &codex_source] {
        assert!(active_sources.session_records.iter().any(|record| {
            record.record_id == source.record.record_id
                && record.state == ManagedSessionState::Live
                && record.active_session.as_ref() == Some(&source.address)
                && !record.provider_identity_present
        }));
    }
    assert_no_context(&active_sources);

    timeout(Duration::from_secs(10), async {
        let expected = [
            (&claude_source.record.record_id, CLAUDE_SOURCE_NAME),
            (&codex_source.record.record_id, CODEX_SOURCE_NAME),
            (&codex_target.record.record_id, CODEX_TARGET_NAME),
            (&claude_target.record.record_id, CLAUDE_TARGET_NAME),
        ];
        loop {
            let events = renamed_events.lock().unwrap();
            if expected.iter().all(|(record_id, name)| {
                events
                    .iter()
                    .any(|(seen_id, seen_name, identity_present)| {
                        seen_id == *record_id && seen_name == name && !identity_present
                    })
            }) {
                return;
            }
            drop(events);
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("C2 event stream omitted a human-readable SessionRecordUpserted event");

    for source in [&claude_source, &codex_source] {
        stop_session(&control, &route, &source.address).await;
        remove_session(&control, &route, &source.address).await;
        forget_record(&control, &route, &source.record).await;
    }
    let final_snapshot = snapshot(&control, &route).await;
    assert!(final_snapshot.session_records.is_empty());
    assert!(final_snapshot.managed_worktrees.is_empty());
    assert!(final_snapshot
        .workspaces
        .iter()
        .all(|workspace| workspace.sessions.is_empty()));
    assert_no_context(&final_snapshot);

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
    timeout(Duration::from_secs(5), provider_event_collector)
        .await
        .expect("Node provider event collector did not close")
        .expect("Node provider event collector failed");
    root_cleanup.remove();
}
