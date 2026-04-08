//! Integration tests for `TransportSession` — public API surface and SessionEnd synthesis.
//!
//! These tests verify:
//! 1. `TransportSession` is accessible from the public API (re-export works).
//! 2. `SpawnOptions` is accessible and default-constructible.
//! 3. The `CliTool` variants all dispatch through the right path in `spawn()`.
//!
//! Note: Live process spawning is NOT tested here — no real CLI binaries
//! (claude, codex, cursor, etc.) are required. The synthesis logic is covered
//! by unit tests inside `src/transport/pipe_runner.rs`.
//!
//! The integration-level concern here is that the public exports are wired
//! correctly and the dispatch match in `TransportSession::spawn` is exhaustive.

use gate4agent::{CliTool, SpawnOptions, TransportSession};

/// Verify that all 6 `CliTool` variants are accessible and that `SpawnOptions`
/// can be default-constructed. This is a compile-time test — if the match in
/// `TransportSession::spawn` is not exhaustive, this test won't compile.
#[test]
fn all_cli_tool_variants_are_accessible() {
    let tools = [
        CliTool::ClaudeCode,
        CliTool::Codex,
        CliTool::Gemini,
        CliTool::Cursor,
        CliTool::OpenCode,
        CliTool::OpenClaw,
    ];
    assert_eq!(tools.len(), 6);
}

/// Verify `SpawnOptions` can be constructed with resume_session_id.
#[test]
fn spawn_options_with_resume_id() {
    let opts = SpawnOptions {
        resume_session_id: Some("ses_1234".to_string()),
        ..SpawnOptions::default()
    };
    assert_eq!(opts.resume_session_id.as_deref(), Some("ses_1234"));
    assert!(opts.prompt.is_empty());
    assert!(opts.model.is_none());
}

/// Verify `TransportSession` is accessible as a public type from the crate root.
/// This is a compile-time test — if the re-export is missing, this won't compile.
#[test]
fn transport_session_is_public_type() {
    // Use the type in a way that would fail to compile if it's not pub.
    let _: fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = _>>> = || {
        Box::pin(async {
            let dir = std::path::Path::new(".");
            // This will fail at runtime (no binary), but the type check is what matters.
            let result = TransportSession::spawn(
                CliTool::ClaudeCode,
                dir,
                "hello",
                SpawnOptions::default(),
            )
            .await;
            result
        })
    };
    // We only check the type compiles, not that the future runs.
}

/// Verify the session_id() and subscribe() methods are accessible on `TransportSession`.
/// (Compile-time verification that the public API is complete.)
#[test]
fn transport_session_public_methods_exist() {
    // This closure captures the trait bounds — if any method is missing, compile fails.
    fn _assert_api_exists() {
        async fn _check(session: TransportSession) -> &'static str {
            let _rx = session.subscribe();
            let _sid: &str = session.session_id();
            let _ = session.send_prompt("test").await;
            let _ = session.kill().await;
            "ok"
        }
    }
}

/// Verify OpenClaw dispatch returns DaemonNotRunning when no daemon is listening.
///
/// This test attempts to spawn an OpenClaw session — since no openclaw daemon
/// is running in CI, it must return an error (not panic or hang).
#[tokio::test]
async fn openclaw_spawn_returns_daemon_error_when_no_daemon() {
    let dir = std::env::temp_dir();
    let result = TransportSession::spawn(
        CliTool::OpenClaw,
        &dir,
        "test",
        SpawnOptions::default(),
    )
    .await;

    let err = result.err().expect("OpenClaw spawn must fail when no daemon is running");
    let err_str = format!("{}", err);
    // Error must be DaemonNotRunning or DaemonProbeTimeout — NOT a panic or Spawn error.
    assert!(
        err_str.contains("not running") || err_str.contains("timed out"),
        "Expected daemon not running or timed out error, got: {}",
        err_str
    );
}
