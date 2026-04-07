//! NDJSON persistence helpers for agent chat sessions.

use std::fs::{self, OpenOptions};
use std::io::Write as IoWrite;
use std::path::Path;

use crate::snapshot::{AgentCli, ChatMessage};

/// Append a single `ChatMessage` as a JSON line to the session's messages.ndjson.
///
/// Creates `{sessions_dir}/{cli}/{session_id}/` directories as needed.
pub fn persist_message(
    sessions_dir: &Path,
    cli: AgentCli,
    session_id: &str,
    msg: &ChatMessage,
) -> std::io::Result<()> {
    let dir = sessions_dir.join(cli.as_str()).join(session_id);
    fs::create_dir_all(&dir)?;
    let path = dir.join("messages.ndjson");
    let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
    let line = serde_json::to_string(msg)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    writeln!(f, "{}", line)
}

/// Read all messages from a session's messages.ndjson.
///
/// Lines that fail to parse are silently skipped.
pub fn load_session(
    sessions_dir: &Path,
    cli: AgentCli,
    session_id: &str,
) -> std::io::Result<Vec<ChatMessage>> {
    let path = sessions_dir
        .join(cli.as_str())
        .join(session_id)
        .join("messages.ndjson");
    let content = fs::read_to_string(&path)?;
    let messages = content
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    Ok(messages)
}

/// List subdirectory names (session IDs) under `{sessions_dir}/{cli}/`,
/// sorted newest-first by mtime. Returns an empty vec on any error.
pub fn scan_past_sessions(sessions_dir: &Path, cli: AgentCli) -> Vec<String> {
    let base = sessions_dir.join(cli.as_str());
    let entries = match fs::read_dir(&base) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut ids: Vec<(std::time::SystemTime, String)> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let mtime = e
                .metadata()
                .ok()?
                .modified()
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            Some((mtime, name))
        })
        .collect();
    ids.sort_by(|a, b| b.0.cmp(&a.0));
    ids.into_iter().map(|(_, name)| name).collect()
}
