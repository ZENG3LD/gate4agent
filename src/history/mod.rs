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

/// Token usage extracted from a loaded session file.
#[derive(Debug, Clone, Default)]
pub struct SessionUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

/// Read-only history accessor for one CLI.
pub trait HistoryReader {
    /// List sessions newest-first. Empty vec on errors or no sessions.
    fn list_sessions(&self, workdir: &Path) -> Vec<SessionMeta>;

    /// Load full message list for one session. Empty vec on errors.
    fn load_session(&self, workdir: &Path, session_id: &str) -> Vec<ChatMessage>;

    /// Load full message list plus token usage for one session.
    ///
    /// The default implementation delegates to `load_session` and returns
    /// `SessionUsage::default()`. CLIs that embed token data in their session
    /// files (e.g. Claude JSONL) should override this.
    fn load_session_with_usage(
        &self,
        workdir: &Path,
        session_id: &str,
    ) -> (Vec<ChatMessage>, SessionUsage) {
        let messages = self.load_session(workdir, session_id);
        (messages, SessionUsage::default())
    }

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
