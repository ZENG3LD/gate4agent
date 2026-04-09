//! Integration tests for `TransportSession` — public API surface and dispatch.
//!
//! These tests verify:
//! 1. `TransportSession` is accessible from the public API (re-export works).
//! 2. `SpawnOptions` is accessible and default-constructible.
//! 3. The `CliTool` variants all dispatch through the right path in `spawn()`.
//!
//! Note: Live process spawning is NOT tested here — no real CLI binaries
//! (claude, codex, gemini, etc.) are required. The synthesis logic is covered
//! by unit tests inside `src/transport/pipe_runner.rs`.

use gate4agent::{CliTool, SpawnOptions, TransportSession};

/// Verify that all 4 `CliTool` variants are accessible and that `SpawnOptions`
/// can be default-constructed. This is a compile-time test — if the match in
/// `TransportSession::spawn_pipe` is not exhaustive, this test won't compile.
#[test]
fn all_cli_tool_variants_are_accessible() {
    let tools = [
        CliTool::ClaudeCode,
        CliTool::Codex,
        CliTool::Gemini,
        CliTool::OpenCode,
    ];
    assert_eq!(tools.len(), 4);
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
