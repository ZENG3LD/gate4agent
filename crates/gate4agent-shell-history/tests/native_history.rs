use gate4agent_adapters::HistorySourceLayout;
use gate4agent_catalog::builtin_registry;
use gate4agent_provider_ports::{
    discover_history, load_history_session, HistoryAuthority, HistoryDiscoveryRequest,
    HistoryLoadRequest,
};
use gate4agent_shell_history::{
    orca_home_roots, NativeHistoryAuthority, NativeHistoryConfig, NativeHistoryDiscoveryIssueKind,
    NativeHistoryError, NativeHistoryLimits, NativeHistoryRoot,
};
use gate4agent_types::AdapterId;
use rusqlite::Connection;
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

struct FixtureDir(PathBuf);

impl FixtureDir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "gate4agent-history-{name}-{}",
            Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for FixtureDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn root(adapter: &str, layout: HistorySourceLayout, path: &Path) -> NativeHistoryRoot {
    NativeHistoryRoot::new(AdapterId::new(adapter).unwrap(), layout, path).unwrap()
}

fn request(agent: &str, limit: u16) -> HistoryDiscoveryRequest {
    HistoryDiscoveryRequest::from_spec(builtin_registry().get_by_id(agent).unwrap(), None, limit)
        .unwrap()
}

fn write(path: &Path, content: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

#[test]
fn orca_roots_cover_pinned_native_sources_without_exposing_paths_in_debug() {
    let fixture = FixtureDir::new("roots");
    let roots = orca_home_roots(fixture.path()).unwrap();

    assert_eq!(roots.len(), 19);
    assert_eq!(
        roots
            .iter()
            .filter(|root| root.adapter_id().as_str() == "opencode")
            .count(),
        2
    );
    assert!(format!("{:?}", roots[0]).contains("[REDACTED]"));
    assert!(!format!("{:?}", roots[0]).contains(&fixture.path().display().to_string()));
}

#[test]
fn grok_sibling_load_is_opaque_bounded_and_cache_coherent() {
    let fixture = FixtureDir::new("grok");
    let sessions = fixture.path().join("sessions");
    let session = sessions.join("repo").join("grok-1");
    write(
        &session.join("summary.json"),
        r#"{"info":{"id":"grok-1","cwd":"/repo"},"generated_title":"Grok title"}"#,
    );
    write(
        &session.join("chat_history.jsonl"),
        concat!(
            r#"{"type":"user","content":"hello"}"#,
            "\n",
            r#"{"type":"assistant","content":"world"}"#
        ),
    );
    let config = NativeHistoryConfig::new(vec![root(
        "grok",
        HistorySourceLayout::SummaryJsonWithSiblingNdjson,
        &sessions,
    )])
    .unwrap();
    let mut authority = NativeHistoryAuthority::new(config);
    let discovery = request("grok", 8);

    let candidates = discover_history(&mut authority, &discovery).unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].session_id_hint(), "grok-1");
    assert!(candidates[0].id().as_str().starts_with("hist_"));
    assert!(!candidates[0].id().as_str().contains("sessions"));
    let load = HistoryLoadRequest::new(&discovery, candidates[0].clone()).unwrap();
    let parsed = load_history_session(&mut authority, &load).unwrap();
    assert_eq!(parsed.title.as_deref(), Some("Grok title"));
    assert_eq!(parsed.messages.len(), 2);
    assert_eq!(authority.cache_stats().misses, 1);

    let cached = load_history_session(&mut authority, &load).unwrap();
    assert_eq!(cached.messages.len(), 2);
    assert_eq!(authority.cache_stats().hits, 1);

    write(
        &session.join("chat_history.jsonl"),
        concat!(
            r#"{"type":"user","content":"hello again"}"#,
            "\n",
            r#"{"type":"assistant","content":"world again"}"#,
            "\n",
            r#"{"type":"assistant","content":"changed"}"#
        ),
    );
    let refreshed = load_history_session(&mut authority, &load).unwrap();
    assert_eq!(refreshed.messages.len(), 3);
    assert_eq!(authority.cache_stats().misses, 2);
}

#[test]
fn discovery_applies_claude_and_antigravity_resume_filters() {
    let fixture = FixtureDir::new("filters");
    let claude = fixture.path().join("claude");
    write(&claude.join("project").join("parent.jsonl"), "{}");
    write(
        &claude
            .join("project")
            .join("session")
            .join("subagents")
            .join("worker.jsonl"),
        "{}",
    );
    let mut authority = NativeHistoryAuthority::new(
        NativeHistoryConfig::new(vec![root(
            "claude-code",
            HistorySourceLayout::SingleNdjson,
            &claude,
        )])
        .unwrap(),
    );
    let claude_request = request("claude", 8);
    let candidates = discover_history(&mut authority, &claude_request).unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].session_id_hint(), "parent");

    let brain = fixture.path().join("brain");
    write(
        &brain
            .join("conversation-1")
            .join(".system_generated")
            .join("logs")
            .join("transcript.jsonl"),
        "{}",
    );
    write(&brain.join("noise").join("transcript.jsonl"), "{}");
    let mut authority = NativeHistoryAuthority::new(
        NativeHistoryConfig::new(vec![root(
            "antigravity",
            HistorySourceLayout::SingleNdjson,
            &brain,
        )])
        .unwrap(),
    );
    let antigravity_request = request("antigravity", 8);
    let candidates = discover_history(&mut authority, &antigravity_request).unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].session_id_hint(), "conversation-1");
}

#[test]
fn codex_load_joins_the_home_index_without_exposing_it_as_a_candidate() {
    let fixture = FixtureDir::new("codex");
    let sessions = fixture.path().join("sessions");
    write(
        &sessions.join("2026").join("rollout-codex-1.jsonl"),
        concat!(
            r#"{"type":"session_meta","payload":{"id":"codex-1","thread_source":"user","cwd":"/repo"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"hello"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"agent_message","message":"world"}}"#
        ),
    );
    write(
        &fixture.path().join("session_index.jsonl"),
        concat!(
            r#"{"id":"codex-1","thread_name":"Old title"}"#,
            "\n",
            r#"{"id":"codex-1","thread_name":"Indexed title"}"#
        ),
    );
    let mut authority = NativeHistoryAuthority::new(
        NativeHistoryConfig::new(vec![root(
            "codex",
            HistorySourceLayout::NdjsonWithOptionalIndex,
            &sessions,
        )])
        .unwrap(),
    );
    let discovery = request("codex", 8);
    let candidates = discover_history(&mut authority, &discovery).unwrap();
    assert_eq!(candidates.len(), 1);
    assert!(!candidates[0].id().as_str().contains("session_index"));
    let load = HistoryLoadRequest::new(&discovery, candidates[0].clone()).unwrap();
    let parsed = load_history_session(&mut authority, &load).unwrap();

    assert_eq!(parsed.session_id, "codex-1");
    assert_eq!(parsed.title.as_deref(), Some("Indexed title"));
    assert_eq!(parsed.cwd.as_deref(), Some("/repo"));
    assert_eq!(parsed.messages.len(), 2);
}

#[test]
fn kimi_load_uses_index_cwd_and_primary_agent_wire() {
    let fixture = FixtureDir::new("kimi");
    let sessions = fixture.path().join("sessions");
    let session = sessions.join("wd_repo_hash").join("session_kimi_1");
    write(
        &session.join("state.json"),
        r#"{"title":"Kimi title","agents":{"agent-primary":{"type":"main","parentAgentId":null}}}"#,
    );
    write(
        &fixture.path().join("session_index.jsonl"),
        r#"{"sessionId":"session_kimi_1","workDir":"/repo/kimi"}"#,
    );
    write(
        &session
            .join("agents")
            .join("agent-primary")
            .join("wire.jsonl"),
        concat!(
            r#"{"type":"config.update","modelAlias":"kimi-k2"}"#,
            "\n",
            r#"{"type":"context.append_message","message":{"role":"user","origin":{"kind":"user"},"content":"question"}}"#,
            "\n",
            r#"{"type":"context.append_loop_event","event":{"type":"content.part","part":{"type":"text","text":"answer"}}}"#,
            "\n",
            r#"{"type":"context.append_loop_event","event":{"type":"step.end"}}"#
        ),
    );
    let mut authority = NativeHistoryAuthority::new(
        NativeHistoryConfig::new(vec![root(
            "kimi",
            HistorySourceLayout::StateJsonWithIndexAndSiblingNdjson,
            &sessions,
        )])
        .unwrap(),
    );
    let discovery = request("kimi", 8);
    let candidate = discover_history(&mut authority, &discovery)
        .unwrap()
        .remove(0);
    let load = HistoryLoadRequest::new(&discovery, candidate).unwrap();
    let parsed = load_history_session(&mut authority, &load).unwrap();

    assert_eq!(parsed.cwd.as_deref(), Some("/repo/kimi"));
    assert_eq!(parsed.model.as_deref(), Some("kimi-k2"));
    assert_eq!(parsed.messages.len(), 2);
}

#[test]
fn opencode_sqlite_wins_legacy_dedupe_and_loads_readonly_projection() {
    let fixture = FixtureDir::new("opencode");
    let storage = fixture.path().join("storage");
    write(
        &storage.join("session").join("project").join("ses_1.json"),
        r#"{"id":"ses_1","title":"stale legacy"}"#,
    );
    let database = fixture.path().join("opencode.db");
    let connection = Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, title TEXT, directory TEXT, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, model TEXT, tokens_input INTEGER, tokens_output INTEGER, tokens_reasoning INTEGER, tokens_cache_read INTEGER, parent_id TEXT, time_archived INTEGER);\
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, data TEXT NOT NULL);\
             CREATE TABLE part (message_id TEXT NOT NULL, time_created INTEGER NOT NULL, data TEXT NOT NULL);\
             INSERT INTO session VALUES ('ses_1','SQLite title','/repo',10,20,'{\"id\":\"glm-5\"}',3,4,2,1,NULL,NULL);\
             INSERT INTO message VALUES ('msg_1','ses_1','{\"role\":\"user\",\"summary\":{\"title\":\"Question\"}}');\
             INSERT INTO part VALUES ('msg_1',11,'{\"type\":\"text\",\"text\":\"hello\"}');",
        )
        .unwrap();
    drop(connection);

    let mut authority = NativeHistoryAuthority::new(
        NativeHistoryConfig::new(vec![
            root(
                "opencode",
                HistorySourceLayout::SessionJsonWithSiblingMessageJson,
                &storage,
            ),
            root(
                "opencode",
                HistorySourceLayout::ReadOnlySqliteProjection,
                fixture.path(),
            ),
        ])
        .unwrap(),
    );
    let discovery = request("opencode", 8);
    let candidates = discover_history(&mut authority, &discovery).unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].session_id_hint(), "ses_1");
    let load = HistoryLoadRequest::new(&discovery, candidates[0].clone()).unwrap();
    let parsed = load_history_session(&mut authority, &load).unwrap();

    assert_eq!(parsed.title.as_deref(), Some("SQLite title"));
    assert_eq!(parsed.cwd.as_deref(), Some("/repo"));
    assert_eq!(parsed.model.as_deref(), Some("glm-5"));
    assert_eq!(parsed.total_tokens, 10);
    assert_eq!(parsed.messages.len(), 1);
}

#[test]
fn rediscovery_revokes_disappeared_candidate() {
    let fixture = FixtureDir::new("revoke");
    let sessions = fixture.path().join("sessions");
    let transcript = sessions.join("gone.jsonl");
    write(&transcript, r#"{"type":"session","id":"gone"}"#);
    let mut authority = NativeHistoryAuthority::new(
        NativeHistoryConfig::new(vec![root(
            "pi",
            HistorySourceLayout::SingleNdjson,
            &sessions,
        )])
        .unwrap(),
    );
    let discovery = request("pi", 8);
    let candidate = authority.discover(&discovery).unwrap().remove(0);
    fs::remove_file(&transcript).unwrap();
    assert!(authority.discover(&discovery).unwrap().is_empty());
    let load = HistoryLoadRequest::new(&discovery, candidate).unwrap();
    assert_eq!(
        authority.load(&load),
        Err(NativeHistoryError::CandidateExpired)
    );
}

#[test]
fn traversal_reports_the_hard_entry_boundary_without_leaking_paths() {
    let fixture = FixtureDir::new("walk-limit");
    let sessions = fixture.path().join("sessions");
    write(&sessions.join("one.jsonl"), "{}");
    write(&sessions.join("two.jsonl"), "{}");
    let limits = NativeHistoryLimits {
        max_walk_entries: 1,
        ..NativeHistoryLimits::default()
    };
    let config = NativeHistoryConfig::with_limits(
        vec![root("pi", HistorySourceLayout::SingleNdjson, &sessions)],
        limits,
    )
    .unwrap();
    let mut authority = NativeHistoryAuthority::new(config);
    let discovery = request("pi", 8);

    assert!(authority.discover(&discovery).unwrap().len() <= 1);
    let issues = authority.take_discovery_issues();
    assert_eq!(issues.len(), 1);
    assert_eq!(
        issues[0].kind,
        NativeHistoryDiscoveryIssueKind::EntryLimitReached
    );
    assert!(!format!("{issues:?}").contains(&fixture.path().display().to_string()));
}
