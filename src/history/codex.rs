//! Codex session history reader.
//!
//! Codex stores sessions in `~/.codex/sessions/` as JSON files.
//! Each file is named `{thread_id}.json` (or similar) and contains
//! the full thread history.

use std::fs;
use std::path::Path;
use std::time::SystemTime;

use crate::pty::snapshot::{ChatMessage, ChatRole};
use super::{HistoryReader, SessionMeta};

pub struct CodexHistoryReader;

impl HistoryReader for CodexHistoryReader {
    fn list_sessions(&self, _workdir: &Path) -> Vec<SessionMeta> {
        let home = home_dir();
        let sessions_dir = home.join(".codex").join("sessions");
        if !sessions_dir.is_dir() {
            return Vec::new();
        }
        let entries = match fs::read_dir(&sessions_dir) {
            Ok(e) => e,
            Err(_) => return Vec::new(),
        };
        let mut metas: Vec<SessionMeta> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            // Accept both .json files and directories (Codex may use either layout).
            let is_json = path.extension().and_then(|s| s.to_str()) == Some("json");
            let is_dir = path.is_dir();
            if !is_json && !is_dir {
                continue;
            }
            let id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            if id.is_empty() {
                continue;
            }
            let ts = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            metas.push(SessionMeta { id, timestamp: ts, preview: String::new() });
        }
        metas.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        metas
    }

    fn load_session(&self, _workdir: &Path, session_id: &str) -> Vec<ChatMessage> {
        let home = home_dir();
        let path = home.join(".codex").join("sessions").join(format!("{}.json", session_id));
        let raw = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        parse_codex_session(&raw)
    }
}

/// Parse a Codex session JSON file into chat messages.
///
/// Codex session format is not publicly documented — we attempt a best-effort
/// parse looking for `role` + `content` pairs at any nesting level.
fn parse_codex_session(raw: &str) -> Vec<ChatMessage> {
    let v: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    // Try top-level array of message objects.
    if let Some(arr) = v.as_array() {
        for item in arr {
            if let Some(msg) = extract_message(item) {
                out.push(msg);
            }
        }
    }
    // Try object with a "messages" or "items" array.
    for key in &["messages", "items", "history", "turns"] {
        if let Some(arr) = v.get(key).and_then(|a| a.as_array()) {
            for item in arr {
                if let Some(msg) = extract_message(item) {
                    out.push(msg);
                }
            }
            if !out.is_empty() {
                break;
            }
        }
    }
    out
}

fn extract_message(v: &serde_json::Value) -> Option<ChatMessage> {
    let role_str = v.get("role").and_then(|r| r.as_str()).unwrap_or("");
    let role = match role_str {
        "user" => ChatRole::User,
        "assistant" => ChatRole::Assistant,
        _ => return None,
    };
    let content = v
        .get("content")
        .and_then(|c| c.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            // Content may be an array of content blocks.
            v.get("content")
                .and_then(|c| c.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
        })?;
    if content.is_empty() {
        return None;
    }
    Some(ChatMessage { role, content, tool_name: None })
}

fn home_dir() -> std::path::PathBuf {
    #[cfg(target_os = "windows")]
    if let Some(p) = std::env::var_os("USERPROFILE") {
        return std::path::PathBuf::from(p);
    }
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_array_of_messages() {
        let raw = r#"[
            {"role":"user","content":"Hello"},
            {"role":"assistant","content":"Hi there"}
        ]"#;
        let msgs = parse_codex_session(raw);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, ChatRole::User);
        assert_eq!(msgs[0].content, "Hello");
        assert_eq!(msgs[1].role, ChatRole::Assistant);
        assert_eq!(msgs[1].content, "Hi there");
    }

    #[test]
    fn parse_object_with_messages_key() {
        let raw = r#"{"messages":[
            {"role":"user","content":"Question"},
            {"role":"assistant","content":"Answer"}
        ]}"#;
        let msgs = parse_codex_session(raw);
        assert_eq!(msgs.len(), 2);
    }

    #[test]
    fn skip_unknown_roles() {
        let raw = r#"[
            {"role":"system","content":"You are helpful"},
            {"role":"user","content":"Hi"}
        ]"#;
        let msgs = parse_codex_session(raw);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "Hi");
    }
}
