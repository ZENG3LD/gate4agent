//! `TransportSession` — unified entry point for spawning any of the 6 supported CLIs.
//!
//! # Design
//!
//! `TransportSession` wraps an internal [`TransportHandle`] enum that routes to the
//! appropriate transport backend:
//!
//! - **Pipe** — Claude Code, Codex, Gemini, Cursor, OpenCode (NDJSON pipe mode)
//! - **Daemon** — OpenClaw (pipe client after daemon liveness check)
//! - **Pty** — NOT routed through `TransportSession::spawn` in Phase 5.
//!   `PtySession` remains its own direct entry point for consumers that need the
//!   screen-scraping path. The `Pty` variant is reserved in the enum for future
//!   routing but is not constructed by `spawn()` in this phase.
//!
//! All six CLIs produce `AgentEvent` values on a `tokio::sync::broadcast` channel.
//! Subscribers call `subscribe()` and receive events regardless of which transport
//! is running underneath.
//!
//! # SessionEnd synthesis
//!
//! The pipe and daemon paths synthesize a `AgentEvent::SessionEnd` event on process
//! exit if the CLI's NDJSON parser did not already emit one. This covers Codex, which
//! exits with code 0 but never emits a terminal event. See `pipe_runner` for details.

use std::path::Path;

use tokio::sync::broadcast;

use crate::cli::openclaw::default_daemon_probe;
use crate::error::AgentError;
use crate::transport::daemon_runner::run_daemon;
use crate::transport::pipe_runner::{run_pipe, PipeRunnerHandle};
use crate::transport::SpawnOptions;
use crate::types::{AgentEvent, CliTool};

// ---------------------------------------------------------------------------
// Internal handle variants
// ---------------------------------------------------------------------------

enum TransportHandle {
    Pipe(PipeRunnerHandle),
    /// Placeholder for future PTY routing via `TransportSession::spawn`.
    /// Not constructed in Phase 5 — `PtySession` is the direct entry point.
    #[allow(dead_code)]
    Pty,
    /// Daemon-backed tools (OpenClaw). Uses a `PipeRunnerHandle` after the
    /// daemon liveness probe passes.
    Daemon(PipeRunnerHandle),
}

// ---------------------------------------------------------------------------
// TransportSession
// ---------------------------------------------------------------------------

/// Unified entry point for spawning any of the 6 supported CLI agents.
///
/// See the [module documentation](self) for design notes and transport routing rules.
pub struct TransportSession {
    /// Gate4agent-assigned session ID (not the CLI-native session ID).
    pub session_id: String,
    /// The CLI tool that was spawned.
    pub tool: CliTool,
    handle: TransportHandle,
}

impl TransportSession {
    /// Spawn a new session for the given tool and deliver the initial prompt.
    ///
    /// # Dispatch rules
    ///
    /// | Tool | Transport |
    /// |---|---|
    /// | `ClaudeCode`, `Codex`, `Gemini`, `Cursor`, `OpenCode` | Pipe runner |
    /// | `OpenClaw` | Daemon probe → pipe runner via `acpx` client |
    ///
    /// PTY mode is NOT dispatched here. Use `PtySession::spawn` directly for
    /// PTY-based sessions.
    ///
    /// # Errors
    ///
    /// - `AgentError::Spawn` — the child process failed to start
    /// - `AgentError::DaemonNotRunning` — OpenClaw daemon is not reachable
    /// - `AgentError::DaemonProbeTimeout` — OpenClaw daemon probe timed out
    pub async fn spawn(
        tool: CliTool,
        working_dir: &Path,
        prompt: &str,
        mut options: SpawnOptions,
    ) -> Result<Self, AgentError> {
        // Bake working_dir and prompt into SpawnOptions.
        options.working_dir = working_dir.to_path_buf();
        options.prompt = prompt.to_string();

        let (handle, session_id) = match tool {
            CliTool::ClaudeCode
            | CliTool::Codex
            | CliTool::Gemini
            | CliTool::Cursor
            | CliTool::OpenCode => {
                let handle = run_pipe(tool, options)?;
                let sid = handle.session_id().to_string();
                (TransportHandle::Pipe(handle), sid)
            }
            CliTool::OpenClaw => {
                let probe = default_daemon_probe();
                let handle = run_daemon(tool, options, &probe)?;
                let sid = handle.session_id().to_string();
                (TransportHandle::Daemon(handle), sid)
            }
        };

        Ok(Self {
            session_id,
            tool,
            handle,
        })
    }

    /// Subscribe to all future `AgentEvent` values from this session.
    ///
    /// Events emitted before this call are not replayed. Subscribe before
    /// awaiting `spawn` if you need the `Started` event.
    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        match &self.handle {
            TransportHandle::Pipe(h) => h.subscribe(),
            TransportHandle::Pty => {
                // This variant is never constructed in Phase 5.
                // Return a receiver from a detached channel so callers get
                // a valid type — they will simply receive no events.
                let (tx, rx) = broadcast::channel(1);
                drop(tx);
                rx
            }
            TransportHandle::Daemon(h) => h.subscribe(),
        }
    }

    /// Gate4agent-assigned session ID (not the CLI-native session ID).
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Send a follow-up prompt via stdin (for CLIs that support multi-turn pipe mode).
    ///
    /// Currently only Claude Code supports multi-turn pipe sessions. For other CLIs
    /// this will write to stdin but may have no effect.
    pub async fn send_prompt(&self, prompt: &str) -> Result<(), AgentError> {
        match &self.handle {
            TransportHandle::Pipe(h) => h.send_prompt(prompt).await,
            TransportHandle::Pty => Ok(()),
            TransportHandle::Daemon(h) => h.send_prompt(prompt).await,
        }
    }

    /// Kill the underlying process.
    pub async fn kill(&self) -> Result<(), AgentError> {
        match &self.handle {
            TransportHandle::Pipe(h) => h.kill().await,
            TransportHandle::Pty => Ok(()),
            TransportHandle::Daemon(h) => h.kill().await,
        }
    }
}
