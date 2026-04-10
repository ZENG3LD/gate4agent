//! OpenCode session history reader.
//!
//! OpenCode (sst/opencode) stores sessions in a SQLite database:
//! - macOS/Linux: `~/.local/share/opencode/opencode.db`
//! - Windows: `%APPDATA%\opencode\opencode.db` or `%LOCALAPPDATA%\opencode\`
//!
//! The `sessions` table has a `directory` column (absolute path where the session
//! was created) which enables project-scoped filtering.  SQLite reading is not
//! attempted in this reader (no SQLite dependency); sessions remain unreadable in
//! pure-SQLite installations.
//!
//! As a fallback, some older or custom OpenCode versions write flat JSON/JSONL
//! session files under `~/.opencode/sessions/` or `~/.opencode/`.  These may
//! contain a top-level `directory` field which is used for filtering when present.
//! If the field is absent the session is included unfiltered.

use std::fs;
use std::path::Path;
use std::time::SystemTime;

use crate::pty::snapshot::{ChatMessage, ChatRole};
use super::{HistoryReader, SessionMeta};

pub struct OpenCodeHistoryReader;

impl HistoryReader for OpenCodeHistoryReader {
    fn list_sessions(&self, workdir: &Path) -> Vec<SessionMeta> {
        let home = home_dir();
        // Try candidate directories where OpenCode may store sessions.
        for candidate in [
            home.join(".opencode").join("sessions"),
            home.join(".opencode"),
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
                let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
                // OpenCode sessions may be JSON files or directories.
                if !path.is_dir() && ext != "json" && ext != "jsonl" {
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
                // Filter by directory field if it can be read from the session.
                if path.is_file() {
                    if !session_matches_workdir(&path, workdir) {
                        continue;
                    }
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
        for dir in [
            home.join(".opencode").join("sessions"),
            home.join(".opencode"),
        ] {
            // Try JSON file.
            for ext in &["json", "jsonl"] {
                let file = dir.join(format!("{}.{}", session_id, ext));
                if file.is_file() {
                    if let Ok(raw) = fs::read_to_string(&file) {
                        if *ext == "jsonl" {
                            return parse_opencode_jsonl(&raw);
                        } else {
                            return parse_opencode_json(&raw);
                        }
                    }
                }
            }
            // Try directory layout.
            let session_dir = dir.join(session_id);
            if session_dir.is_dir() {
                return load_from_session_dir(&session_dir);
            }
        }
        Vec::new()
    }
}

/// Check whether a session file's `directory` field matches `workdir`.
///
/// Returns `true` if:
/// - The file cannot be read or parsed (pass-through: we cannot filter).
/// - The parsed JSON has no `directory` field (pass-through: unknown project).
/// - The `directory` field matches `workdir` after path normalisation.
///
/// Returns `false` only if a `directory` field is present and does NOT match.
fn session_matches_workdir(path: &Path, workdir: &Path) -> bool {
    let raw = match fs::read_to_string(path) {
        Ok(r) => r,
        Err(_) => return true, // cannot read → pass-through
    };
    // For JSONL files check the first line for a session header.
    let text = if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
        raw.lines().next().unwrap_or("").to_string()
    } else {
        raw
    };
    let v: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return true, // parse failure → pass-through
    };
    match v.get("directory").and_then(|d| d.as_str()) {
        None => true, // field absent → pass-through
        Some(dir) => paths_match(dir, workdir),
    }
}

/// Compare a path string with a `Path`, normalising case and separators.
fn paths_match(a: &str, b: &Path) -> bool {
    if a.is_empty() {
        return false;
    }
    let normalise = |s: &str| {
        s.replace('\\', "/").to_lowercase().trim_end_matches('/').to_string()
    };
    normalise(a) == normalise(&b.to_string_lossy())
}

fn load_from_session_dir(dir: &Path) -> Vec<ChatMessage> {
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
                out.extend(parse_opencode_jsonl(&raw));
            } else {
                out.extend(parse_opencode_json(&raw));
            }
        }
    }
    out
}

/// Parse an OpenCode NDJSON session file.
///
/// OpenCode streams events with `type` field: "text", "step_start",
/// "step_finish", "tool_use", "reasoning".  Each line also has a `sessionID`
/// field.  We only care about content-bearing events.
fn parse_opencode_jsonl(raw: &str) -> Vec<ChatMessage> {
    let mut out = Vec::new();
    // Accumulate assistant text deltas into a single message.
    let mut assistant_buf = String::new();

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                // Accumulate into assistant buffer.
                if let Some(text) = v.get("text").and_then(|t| t.as_str()) {
                    assistant_buf.push_str(text);
                }
            }
            Some("step_finish") => {
                // Flush accumulated assistant text.
                if !assistant_buf.is_empty() {
                    out.push(ChatMessage {
                        role: ChatRole::Assistant,
                        content: assistant_buf.clone(),
                        tool_name: None,
                    });
                    assistant_buf.clear();
                }
            }
            _ => {}
        }
    }
    // Flush any remaining text.
    if !assistant_buf.is_empty() {
        out.push(ChatMessage {
            role: ChatRole::Assistant,
            content: assistant_buf,
            tool_name: None,
        });
    }
    out
}

/// Parse an OpenCode JSON session file (array or object with messages).
fn parse_opencode_json(raw: &str) -> Vec<ChatMessage> {
    let v: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    if let Some(arr) = v.as_array() {
        for item in arr {
            if let Some(msg) = extract_opencode_message(item) {
                out.push(msg);
            }
        }
    }
    for key in &["messages", "history", "conversation"] {
        if let Some(arr) = v.get(key).and_then(|a| a.as_array()) {
            for item in arr {
                if let Some(msg) = extract_opencode_message(item) {
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

fn extract_opencode_message(v: &serde_json::Value) -> Option<ChatMessage> {
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
    fn parse_json_messages() {
        let raw = r#"[
            {"role":"user","content":"What is Rust?"},
            {"role":"assistant","content":"Rust is a systems language."}
        ]"#;
        let msgs = parse_opencode_json(raw);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, ChatRole::User);
        assert_eq!(msgs[1].role, ChatRole::Assistant);
    }

    #[test]
    fn parse_jsonl_accumulates_text() {
        let raw = concat!(
            "{\"type\":\"text\",\"text\":\"Hello \",\"sessionID\":\"ses_001\"}\n",
            "{\"type\":\"text\",\"text\":\"world\",\"sessionID\":\"ses_001\"}\n",
            "{\"type\":\"step_finish\",\"sessionID\":\"ses_001\"}\n",
        );
        let msgs = parse_opencode_jsonl(raw);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "Hello world");
        assert_eq!(msgs[0].role, ChatRole::Assistant);
    }

    #[test]
    fn parse_jsonl_skips_non_json_lines() {
        let raw = "not json\n{\"type\":\"text\",\"text\":\"ok\",\"sessionID\":\"s\"}\n{\"type\":\"step_finish\"}\n";
        let msgs = parse_opencode_jsonl(raw);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "ok");
    }

    #[test]
    fn session_matches_workdir_pass_through_on_no_field() {
        let dir = std::env::temp_dir();
        let path = dir.join("oc_test_no_dir.json");
        // JSON with no "directory" field.
        std::fs::write(&path, r#"{"messages":[]}"#).unwrap();
        let workdir = std::path::Path::new("/some/project");
        assert!(session_matches_workdir(&path, workdir));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn session_matches_workdir_filters_correctly() {
        let dir = std::env::temp_dir();
        let path = dir.join("oc_test_with_dir.json");
        let json = r#"{"directory":"/home/user/myproject","messages":[]}"#;
        std::fs::write(&path, json).unwrap();

        let matching = std::path::Path::new("/home/user/myproject");
        let other = std::path::Path::new("/home/user/other");

        assert!(session_matches_workdir(&path, matching));
        assert!(!session_matches_workdir(&path, other));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn paths_match_normalisation() {
        assert!(paths_match(
            r"C:\Users\Me\Project",
            std::path::Path::new(r"c:\users\me\project"),
        ));
        assert!(paths_match("/home/me/project/", std::path::Path::new("/home/me/project")));
        assert!(!paths_match("/a", std::path::Path::new("/b")));
        assert!(!paths_match("", std::path::Path::new("/a")));
    }
}
