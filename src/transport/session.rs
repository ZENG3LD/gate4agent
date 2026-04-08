//! `TransportSession` — thin dispatch router for spawning CLI agent processes.
//!
//! # Design
//!
//! `TransportSession` is a dispatch-only router. It holds a `PipeSession` (the
//! only pipe transport) and exposes the same subscription/kill/send API on top.
//!
//! PTY mode is NOT routed through `TransportSession`. Use `PtySession::spawn`
//! directly for PTY-based screen-scraping sessions.
//!
//! The only transport currently is **Pipe**: NDJSON-streaming subprocesses for
//! Claude Code, Codex, Gemini, Cursor, and OpenCode. There is no Daemon
//! transport — that was a fiction (OpenClaw/acpx were never functional).

use std::path::Path;

use tokio::sync::broadcast;

use crate::error::AgentError;
use crate::pipe::PipeSession;
use crate::transport::SpawnOptions;
use crate::types::{AgentEvent, CliTool};

/// Thin dispatch router for spawning any of the 5 supported PIPE-mode CLI agents.
///
/// All five CLIs (Claude Code, Codex, Gemini, Cursor, OpenCode) use the Pipe
/// transport. PTY-mode sessions use `PtySession` directly — there is no PTY
/// routing through `TransportSession`.
pub struct TransportSession {
    /// Gate4agent-assigned session ID (not the CLI-native session ID).
    pub session_id: String,
    /// The CLI tool that was spawned.
    pub tool: CliTool,
    inner: PipeSession,
}

impl TransportSession {
    /// Spawn a new pipe session for the given tool and deliver the initial prompt.
    ///
    /// All five `CliTool` variants use the Pipe transport. PTY mode is not
    /// dispatched here — use `PtySession::spawn` directly.
    ///
    /// # Errors
    ///
    /// - `AgentError::Spawn` — the child process failed to start
    pub async fn spawn(
        tool: CliTool,
        working_dir: &Path,
        prompt: &str,
        options: SpawnOptions,
    ) -> Result<Self, AgentError> {
        use crate::types::SessionConfig;

        let config = SessionConfig {
            tool,
            working_dir: working_dir.to_path_buf(),
            env_vars: options.env_vars.clone(),
            name: None,
        };

        let pipe_opts = crate::pipe::process::PipeProcessOptions {
            extra_args: options.extra_args.clone(),
            claude: crate::pipe::process::ClaudeOptions {
                resume_session_id: options.resume_session_id.clone(),
                model: options.model.clone(),
                append_system_prompt: options.append_system_prompt.clone(),
            },
        };

        let inner = PipeSession::spawn(config, prompt, pipe_opts).await?;
        let session_id = inner.session_id().to_string();

        Ok(Self {
            session_id,
            tool,
            inner,
        })
    }

    /// Subscribe to all future `AgentEvent` values from this session.
    ///
    /// Events emitted before this call are not replayed. Subscribe before
    /// awaiting `spawn` if you need the `Started` event.
    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.inner.subscribe()
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
        self.inner.send_prompt(prompt).await
    }

    /// Kill the underlying process.
    pub async fn kill(&self) -> Result<(), AgentError> {
        self.inner.kill().await
    }
}
