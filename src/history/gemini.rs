//! Gemini CLI session history reader.
//!
//! Gemini CLI stores session data under `~/.gemini/tmp/` (each session in its
//! own subdirectory named by session ID).  The exact on-disk format is not
//! publicly documented, so we do a best-effort scan: list subdirectories and
//! attempt to parse any JSON/JSONL files found inside them.

use std::fs;
use std::path::Path;
use std::time::SystemTime;

use crate::pty::snapshot::{ChatMessage, ChatRole};
use super::{HistoryReader, SessionMeta};

pub struct GeminiHistoryReader;

impl HistoryReader for GeminiHistoryReader {
    fn list_sessions(&self, _workdir: &Path) -> Vec<SessionMeta> {
        let home = home_dir();
        // Try both known candidate locations.
        for candidate in [
            home.join(".gemini").join("tmp"),
            home.join(".gemini").join("sessions"),
            home.join(".gemini"),
        ] {
            if !candidate.is_dir() {
                continue;
            }
            let entries = match fs::read_dir(&candidate) {
                Ok(e) => e,
                Err(_) => continue,
            };
            let mut metas: Vec<SessionMeta> = Vec::new();
            for entry in entries.flatten() {
                let path = entry.path();
                // Gemini sessions are typically stored as directories named by
                // session ID, or as JSON files.
                if !path.is_dir() && path.extension().and_then(|s| s.to_str()) != Some("json") {
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
            if !metas.is_empty() {
                metas.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
                return metas;
            }
        }
        Vec::new()
    }

    fn load_session(&self, _workdir: &Path, session_id: &str) -> Vec<ChatMessage> {
        let home = home_dir();
        // Try directory-based layout first, then file-based.
        for candidate in [
            home.join(".gemini").join("tmp").join(session_id),
            home.join(".gemini").join("sessions").join(session_id),
        ] {
            if candidate.is_dir() {
                return load_from_dir(&candidate);
            }
        }
        // Try JSON file layout.
        for dir in [
            home.join(".gemini").join("tmp"),
            home.join(".gemini").join("sessions"),
            home.join(".gemini"),
        ] {
            let file = dir.join(format!("{}.json", session_id));
            if file.is_file() {
                if let Ok(raw) = fs::read_to_string(&file) {
                    return parse_gemini_json(&raw);
                }
            }
        }
        Vec::new()
    }
}

/// Load messages from a session directory — tries *.jsonl and *.json files inside.
fn load_from_dir(dir: &Path) -> Vec<ChatMessage> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
        if ext != "json" && ext != "jsonl" {
            continue;
        }
        if let Ok(raw) = fs::read_to_string(&path) {
            if ext == "jsonl" {
                for line in raw.lines() {
                    if let Some(msg) = parse_gemini_ndjson_line(line) {
                        out.push(msg);
                    }
                }
            } else {
                out.extend(parse_gemini_json(&raw));
            }
        }
    }
    out
}

/// Parse a single NDJSON line from a Gemini session file.
fn parse_gemini_ndjson_line(line: &str) -> Option<ChatMessage> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    extract_gemini_message(&v)
}

/// Parse a Gemini JSON session file (array or object with messages).
fn parse_gemini_json(raw: &str) -> Vec<ChatMessage> {
    let v: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    if let Some(arr) = v.as_array() {
        for item in arr {
            if let Some(msg) = extract_gemini_message(item) {
                out.push(msg);
            }
        }
    }
    for key in &["messages", "history", "turns", "conversation"] {
        if let Some(arr) = v.get(key).and_then(|a| a.as_array()) {
            for item in arr {
                if let Some(msg) = extract_gemini_message(item) {
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

fn extract_gemini_message(v: &serde_json::Value) -> Option<ChatMessage> {
    // Gemini uses `role` field with "user" / "assistant" / "model".
    let role_str = v.get("role").and_then(|r| r.as_str()).unwrap_or("");
    let role = match role_str {
        "user" => ChatRole::User,
        "assistant" | "model" => ChatRole::Assistant,
        _ => return None,
    };
    let content = v
        .get("content")
        .and_then(|c| c.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
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
            {"role":"user","content":"Hello Gemini"},
            {"role":"model","content":"Hello! How can I help?"}
        ]"#;
        let msgs = parse_gemini_json(raw);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, ChatRole::User);
        assert_eq!(msgs[1].role, ChatRole::Assistant);
    }

    #[test]
    fn parse_object_with_history_key() {
        let raw = r#"{"history":[
            {"role":"user","content":"Hi"},
            {"role":"assistant","content":"Hello"}
        ]}"#;
        let msgs = parse_gemini_json(raw);
        assert_eq!(msgs.len(), 2);
    }

    #[test]
    fn skip_unknown_roles() {
        let raw = r#"[{"role":"system","content":"prompt"},{"role":"user","content":"go"}]"#;
        let msgs = parse_gemini_json(raw);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "go");
    }
}
