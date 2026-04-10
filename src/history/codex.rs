//! Codex session history reader.
//!
//! Codex stores sessions in `~/.codex/sessions/` in two layouts:
//!
//! **New layout** (v0.8+): `YYYY/MM/DD/rollout-<timestamp>-<uuid>.jsonl`
//! Each file is plain JSONL. The first line is a `session_meta` record:
//! ```json
//! {"type":"session_meta","payload":{"id":"...","cwd":"/path/to/project",...}}
//! ```
//! Subsequent lines are `response_item` or other records.
//!
//! **Old layout**: flat `rollout-<timestamp>-<uuid>.json` files with a top-level
//! `{"session":{...},"items":[...]}` structure (no `cwd` field).
//!
//! Project scoping: the new layout includes `cwd` in the `session_meta` header,
//! so we filter by comparing `cwd` with `workdir`. Old-layout files have no `cwd`
//! and are included unfiltered.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::time::SystemTime;

use crate::pty::snapshot::{ChatMessage, ChatRole};
use super::{HistoryReader, SessionMeta};

pub struct CodexHistoryReader;

impl HistoryReader for CodexHistoryReader {
    fn list_sessions(&self, workdir: &Path) -> Vec<SessionMeta> {
        let home = home_dir();
        let sessions_root = home.join(".codex").join("sessions");
        if !sessions_root.is_dir() {
            return Vec::new();
        }
        let mut metas: Vec<SessionMeta> = Vec::new();
        collect_sessions(&sessions_root, workdir, &mut metas);
        metas.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        metas
    }

    fn load_session(&self, _workdir: &Path, session_id: &str) -> Vec<ChatMessage> {
        let home = home_dir();
        let sessions_root = home.join(".codex").join("sessions");
        // Search for the session file recursively by id stem.
        if let Some(path) = find_session_file(&sessions_root, session_id) {
            let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
            if ext == "jsonl" {
                if let Ok(raw) = fs::read_to_string(&path) {
                    return parse_codex_jsonl(&raw);
                }
            } else if ext == "json" {
                if let Ok(raw) = fs::read_to_string(&path) {
                    return parse_codex_session(&raw);
                }
            }
        }
        Vec::new()
    }
}

/// Recursively collect sessions from `dir`, filtering new-layout `.jsonl` files
/// by `workdir` via the `session_meta` header, and including all old-layout `.json`
/// files unfiltered (they have no cwd).
fn collect_sessions(dir: &Path, workdir: &Path, out: &mut Vec<SessionMeta>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Recurse into YYYY/MM/DD subdirectories.
            collect_sessions(&path, workdir, out);
            continue;
        }
        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
        let ts = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        if ext == "jsonl" {
            // New layout: read first line to get session_meta with cwd + preview.
            if let Some((session_id, cwd, preview)) = read_jsonl_meta(&path) {
                if paths_match(&cwd, workdir) {
                    out.push(SessionMeta {
                        id: session_id,
                        timestamp: ts,
                        preview,
                    });
                }
            }
        } else if ext == "json" {
            // Old layout: no cwd available, include unfiltered.
            let id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            if !id.is_empty() {
                out.push(SessionMeta { id, timestamp: ts, preview: String::new() });
            }
        }
    }
}

/// Read the first line of a `.jsonl` file and extract session id, cwd, and first message
/// preview from the `session_meta` record.
///
/// Returns `None` if the file cannot be opened or the first line is not a valid
/// `session_meta` record.
fn read_jsonl_meta(path: &Path) -> Option<(String, String, String)> {
    let file = fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    if v.get("type").and_then(|t| t.as_str()) != Some("session_meta") {
        return None;
    }
    let payload = v.get("payload")?;
    let session_id = payload.get("id").and_then(|i| i.as_str())?.to_string();
    let cwd = payload.get("cwd").and_then(|c| c.as_str()).unwrap_or("").to_string();
    // `first_message` is an optional field in newer Codex versions.
    let preview = payload
        .get("first_message")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .chars()
        .take(80)
        .collect();
    Some((session_id, cwd, preview))
}

/// Find a session file by its id (UUID) anywhere under `root`.
fn find_session_file(root: &Path, session_id: &str) -> Option<std::path::PathBuf> {
    let entries = fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_session_file(&path, session_id) {
                return Some(found);
            }
            continue;
        }
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        // Session id may appear as the full stem or as a suffix after `rollout-<timestamp>-`.
        if stem == session_id || stem.ends_with(session_id) {
            return Some(path);
        }
    }
    None
}

/// Compare two path strings ignoring case and trailing path separators.
///
/// Both paths are normalised to lowercase with forward slashes before comparison
/// to handle Windows paths reliably.
fn paths_match(a: &str, b: &Path) -> bool {
    if a.is_empty() {
        return false;
    }
    let normalise = |s: &str| {
        s.replace('\\', "/").to_lowercase().trim_end_matches('/').to_string()
    };
    let a_norm = normalise(a);
    let b_norm = normalise(&b.to_string_lossy());
    a_norm == b_norm
}

/// Parse a new-layout Codex JSONL session file into chat messages.
///
/// Each line is a JSON record with `type` and `payload`. We look for
/// `response_item` records that contain user/assistant messages.
fn parse_codex_jsonl(raw: &str) -> Vec<ChatMessage> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let record_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if record_type != "response_item" {
            continue;
        }
        let payload = match v.get("payload") {
            Some(p) => p,
            None => continue,
        };
        if let Some(msg) = extract_codex_item(payload) {
            out.push(msg);
        }
    }
    out
}

fn extract_codex_item(v: &serde_json::Value) -> Option<ChatMessage> {
    let item_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if item_type != "message" {
        return None;
    }
    let role_str = v.get("role").and_then(|r| r.as_str()).unwrap_or("");
    let role = match role_str {
        "user" => ChatRole::User,
        "assistant" => ChatRole::Assistant,
        _ => return None,
    };
    let content = extract_content(v.get("content")?);
    if content.is_empty() {
        return None;
    }
    Some(ChatMessage { role, content, tool_name: None })
}

fn extract_content(v: &serde_json::Value) -> String {
    if let Some(s) = v.as_str() {
        return s.to_string();
    }
    if let Some(arr) = v.as_array() {
        return arr
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n");
    }
    String::new()
}

/// Parse an old-layout Codex JSON session file into chat messages.
///
/// Old format: `{"session":{...},"items":[{"role":"user"|"system","content":[...]}]}`.
fn parse_codex_session(raw: &str) -> Vec<ChatMessage> {
    let v: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    // New-layout top-level array or object with messages.
    if let Some(arr) = v.as_array() {
        for item in arr {
            if let Some(msg) = extract_message(item) {
                out.push(msg);
            }
        }
        return out;
    }
    // Old layout: {"session":{...},"items":[...]}
    for key in &["items", "messages", "history", "turns"] {
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
    let content_val = v.get("content")?;
    let content = extract_content(content_val);
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

    #[test]
    fn paths_match_case_insensitive() {
        assert!(paths_match(
            r"C:\Users\VA PC\CODING\ML_TRADING\nemo",
            Path::new(r"c:\users\va pc\coding\ml_trading\nemo"),
        ));
        assert!(paths_match(
            "/home/user/project",
            Path::new("/home/user/project"),
        ));
        assert!(!paths_match(
            "/home/user/project-a",
            Path::new("/home/user/project-b"),
        ));
        assert!(!paths_match("", Path::new("/home/user")));
    }

    #[test]
    fn paths_match_trailing_separator() {
        assert!(paths_match(
            "/home/user/project/",
            Path::new("/home/user/project"),
        ));
    }

    #[test]
    fn read_jsonl_meta_extracts_cwd_and_preview() {
        let jsonl = r#"{"timestamp":"2026-04-09T11:50:10Z","type":"session_meta","payload":{"id":"abc-123","cwd":"/home/user/project","first_message":"Hello world"}}
{"type":"response_item","payload":{"type":"message","role":"user","content":"Hello world"}}
"#;
        // Write to a temp file.
        let dir = std::env::temp_dir();
        let path = dir.join("test_codex_meta.jsonl");
        std::fs::write(&path, jsonl).unwrap();
        let result = read_jsonl_meta(&path);
        assert!(result.is_some());
        let (id, cwd, preview) = result.unwrap();
        assert_eq!(id, "abc-123");
        assert_eq!(cwd, "/home/user/project");
        assert_eq!(preview, "Hello world");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn parse_codex_jsonl_extracts_messages() {
        let raw = concat!(
            r#"{"type":"session_meta","payload":{"id":"abc","cwd":"/p","first_message":"Hi"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Hello"}]}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":"World"}}"#,
            "\n",
        );
        let msgs = parse_codex_jsonl(raw);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, ChatRole::User);
        assert_eq!(msgs[0].content, "Hello");
        assert_eq!(msgs[1].role, ChatRole::Assistant);
        assert_eq!(msgs[1].content, "World");
    }
}
