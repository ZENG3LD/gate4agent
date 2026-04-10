//! Read-only access to CLI session history files written by the CLIs themselves.
//!
//! We do NOT manage these files. Each CLI stores sessions in its own format
//! inside its own dotfolder under our workdir. This module just reads them.

use std::path::Path;
use crate::pty::snapshot::{AgentCli, ChatMessage};

pub mod claude;
pub mod codex;
pub mod gemini;
pub mod opencode;

pub use claude::invalidate_projects_dir_cache;

/// Lightweight metadata for a session listing.
#[derive(Debug, Clone)]
pub struct SessionMeta {
    pub id: String,
    pub timestamp: i64,        // unix seconds, 0 if unknown
    pub preview: String,       // first user message, max 80 chars
}

/// Read-only history accessor for one CLI.
pub trait HistoryReader {
    /// List sessions newest-first. Empty vec on errors or no sessions.
    fn list_sessions(&self, workdir: &Path) -> Vec<SessionMeta>;

    /// Load full message list for one session. Empty vec on errors.
    fn load_session(&self, workdir: &Path, session_id: &str) -> Vec<ChatMessage>;

    /// Convenience: latest session id, if any.
    fn latest_session(&self, workdir: &Path) -> Option<String> {
        self.list_sessions(workdir).into_iter().next().map(|m| m.id)
    }
}

/// Get a reader for the given CLI.
///
/// Returns a best-effort reader for each CLI. Non-Claude readers attempt to
/// locate session files on disk but return empty vecs if none are found.
pub fn reader_for(cli: AgentCli) -> Box<dyn HistoryReader> {
    match cli {
        AgentCli::Claude => Box::new(claude::ClaudeHistoryReader),
        AgentCli::Codex => Box::new(codex::CodexHistoryReader),
        AgentCli::Gemini => Box::new(gemini::GeminiHistoryReader),
        AgentCli::OpenCode => Box::new(opencode::OpenCodeHistoryReader),
    }
}
