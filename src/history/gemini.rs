//! Gemini CLI session history reader.
//!
//! ## Project scoping
//!
//! On Windows, Gemini CLI maintains `~/.gemini/projects.json` which maps lowercase
//! absolute directory paths to human-readable project slugs:
//! ```json
//! {"projects": {"c:\\users\\me\\myproject": "myproject"}}
//! ```
//! Sessions for a given project are stored in `~/.gemini/tmp/<slug>/chats/*.json`.
//!
//! On Linux/macOS (and older Windows installs), Gemini uses a SHA-256 hash of the
//! project path as the directory name instead.  Since the hash algorithm is not
//! publicly documented we cannot compute it ourselves.  In that case we fall back
//! to listing *all* session directories unfiltered and note the limitation.
//!
//! ## Session JSON format
//!
//! Each `.json` file under `chats/` has this structure:
//! ```json
//! {
//!   "sessionId": "...",
//!   "projectHash": "...",
//!   "startTime": "...",
//!   "messages": [
//!     {"id":"...","timestamp":"...","type":"user","content":"user text"},
//!     {"id":"...","timestamp":"...","type":"gemini","content":"model text"}
//!   ]
//! }
//! ```
//! Legacy files may also use the generic `{"role":"user"|"assistant"|"model","content":"..."}`
//! format — both are supported.

use std::fs;
use std::path::Path;
use std::time::SystemTime;

use crate::pty::snapshot::{ChatMessage, ChatRole};
use super::{HistoryReader, SessionMeta};

pub struct GeminiHistoryReader;

impl HistoryReader for GeminiHistoryReader {
    fn list_sessions(&self, workdir: &Path) -> Vec<SessionMeta> {
        let home = home_dir();
        let tmp = home.join(".gemini").join("tmp");

        // Primary path: use projects.json slug → tmp/<slug>/chats/
        if let Some(slug) = find_project_slug(&home, workdir) {
            let chats_dir = tmp.join(&slug).join("chats");
            if chats_dir.is_dir() {
                let metas = list_chats_dir(&chats_dir);
                if !metas.is_empty() {
                    return metas;
                }
            }
        }

        // Fallback: scan all subdirectories of tmp/ (hash-based or slug-based).
        // We cannot filter by project when we don't know which subdirectory maps to
        // workdir, so we return sessions from the first non-empty directory found.
        //
        // Limitation: on systems that use SHA-256 hashes as directory names the
        // hash algorithm is not publicly documented, so project-scoped filtering is
        // not possible without the projects.json slug mapping.
        if tmp.is_dir() {
            let entries = match fs::read_dir(&tmp) {
                Ok(e) => e,
                Err(_) => return Vec::new(),
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let chats_dir = path.join("chats");
                let candidate = if chats_dir.is_dir() { chats_dir } else { path };
                let metas = list_chats_dir(&candidate);
                if !metas.is_empty() {
                    return metas;
                }
            }
        }

        // Last resort: check legacy flat locations.
        for candidate in [
            home.join(".gemini").join("sessions"),
            home.join(".gemini"),
        ] {
            if !candidate.is_dir() {
                continue;
            }
            let metas = list_json_files_in(&candidate);
            if !metas.is_empty() {
                return metas;
            }
        }
        Vec::new()
    }

    fn load_session(&self, _workdir: &Path, session_id: &str) -> Vec<ChatMessage> {
        let home = home_dir();
        let tmp = home.join(".gemini").join("tmp");

        // Search all tmp/ subdirectories for a file whose stem matches session_id.
        if tmp.is_dir() {
            if let Ok(entries) = fs::read_dir(&tmp) {
                for entry in entries.flatten() {
                    let subdir = entry.path();
                    if !subdir.is_dir() {
                        continue;
                    }
                    let chats_dir = subdir.join("chats");
                    let search_in = if chats_dir.is_dir() { chats_dir } else { subdir };
                    if let Some(msgs) = find_and_load(&search_in, session_id) {
                        return msgs;
                    }
                }
            }
        }

        // Legacy flat layout fallback.
        for dir in [
            home.join(".gemini").join("tmp"),
            home.join(".gemini").join("sessions"),
            home.join(".gemini"),
        ] {
            if let Some(msgs) = find_and_load(&dir, session_id) {
                return msgs;
            }
            // Also try a directory named by session_id.
            let session_dir = dir.join(session_id);
            if session_dir.is_dir() {
                return load_from_dir(&session_dir);
            }
        }
        Vec::new()
    }
}

/// Look up `workdir` in `~/.gemini/projects.json` and return the project slug.
///
/// The JSON maps lowercased absolute paths to slugs:
/// `{"projects": {"c:\\users\\me\\project": "project"}}`.
fn find_project_slug(home: &Path, workdir: &Path) -> Option<String> {
    let projects_file = home.join(".gemini").join("projects.json");
    let raw = fs::read_to_string(&projects_file).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let projects = v.get("projects")?.as_object()?;

    let workdir_norm = normalise_path(&workdir.to_string_lossy());

    for (path, slug) in projects {
        if normalise_path(path) == workdir_norm {
            return slug.as_str().map(|s| s.to_string());
        }
    }
    None
}

/// Normalise a path string for comparison: lowercase, forward slashes, no trailing slash.
fn normalise_path(s: &str) -> String {
    s.replace('\\', "/").to_lowercase().trim_end_matches('/').to_string()
}

/// List all `.json` session files in a `chats/` directory.
fn list_chats_dir(dir: &Path) -> Vec<SessionMeta> {
    list_json_files_in(dir)
}

/// List all `.json` files in `dir` as `SessionMeta` entries.
fn list_json_files_in(dir: &Path) -> Vec<SessionMeta> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut metas: Vec<SessionMeta> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
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
        // Read the first user message for a preview.
        let preview = read_session_preview(&path);
        metas.push(SessionMeta { id, timestamp: ts, preview });
    }
    metas.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    metas
}

/// Read the first user message from a Gemini session file for use as a preview.
///
/// Handles both content formats:
/// - `"content": "plain string"` — used in some legacy and assistant messages.
/// - `"content": [{"text": "..."}]` — used in real Gemini CLI sessions for user messages.
fn read_session_preview(path: &Path) -> String {
    let raw = match fs::read_to_string(path) {
        Ok(r) => r,
        Err(_) => return String::new(),
    };
    let v: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };
    // New format: {"messages":[{"type":"user","content":"..." or [{"text":"..."}]},...]}
    if let Some(arr) = v.get("messages").and_then(|m| m.as_array()) {
        for msg in arr {
            let msg_type = msg.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if msg_type == "user" {
                let text = extract_gemini_content_text(msg.get("content"));
                if !text.is_empty() {
                    return text.chars().take(80).collect();
                }
            }
        }
    }
    // Legacy format: top-level array of {"role":"user","content":"..."}
    if let Some(arr) = v.as_array() {
        for msg in arr {
            if msg.get("role").and_then(|r| r.as_str()) == Some("user") {
                let text = extract_gemini_content_text(msg.get("content"));
                if !text.is_empty() {
                    return text.chars().take(80).collect();
                }
            }
        }
    }
    String::new()
}

/// Extract text from a Gemini `content` value which may be either a plain
/// string or an array of `{"text": "..."}` parts.
fn extract_gemini_content_text(content: Option<&serde_json::Value>) -> String {
    let c = match content {
        Some(c) => c,
        None => return String::new(),
    };
    // Plain string variant.
    if let Some(s) = c.as_str() {
        return s.to_string();
    }
    // Array of parts variant: [{"text": "..."}].
    if let Some(arr) = c.as_array() {
        let joined: String = arr
            .iter()
            .filter_map(|part| part.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n");
        return joined;
    }
    String::new()
}

/// Try to find a session file by id in `dir` and load it.
fn find_and_load(dir: &Path, session_id: &str) -> Option<Vec<ChatMessage>> {
    let entries = fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if stem == session_id {
            if let Ok(raw) = fs::read_to_string(&path) {
                return Some(parse_gemini_json(&raw));
            }
        }
    }
    None
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

/// Parse a Gemini JSON session file.
///
/// Supports two formats:
/// - New: `{"sessionId":"...","messages":[{"type":"user"|"gemini","content":"..."}]}`
/// - Legacy: top-level array or object with `role`/`content` pairs
fn parse_gemini_json(raw: &str) -> Vec<ChatMessage> {
    let v: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();

    // New format: object with "messages" array using type/content fields.
    if let Some(arr) = v.get("messages").and_then(|m| m.as_array()) {
        for item in arr {
            if let Some(msg) = extract_gemini_message(item) {
                out.push(msg);
            }
        }
        if !out.is_empty() {
            return out;
        }
    }

    // Legacy: top-level array of role/content objects.
    if let Some(arr) = v.as_array() {
        for item in arr {
            if let Some(msg) = extract_gemini_message(item) {
                out.push(msg);
            }
        }
        return out;
    }

    // Legacy: object with named array keys.
    for key in &["history", "turns", "conversation"] {
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
    // New format uses "type" field: "user" or "gemini".
    if let Some(msg_type) = v.get("type").and_then(|t| t.as_str()) {
        let role = match msg_type {
            "user" => ChatRole::User,
            "gemini" | "model" => ChatRole::Assistant,
            _ => return None,
        };
        let content = v
            .get("content")
            .and_then(|c| c.as_str())
            .filter(|s| !s.is_empty())
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
                    .filter(|s| !s.is_empty())
            })?;
        return Some(ChatMessage { role, content, tool_name: None });
    }

    // Legacy format uses "role" field: "user" / "assistant" / "model".
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
    fn parse_legacy_array_of_messages() {
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
    fn parse_legacy_object_with_history_key() {
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

    #[test]
    fn parse_new_format_with_type_field() {
        let raw = r#"{
            "sessionId": "abc123",
            "projectHash": "deadbeef",
            "messages": [
                {"id":"1","type":"user","content":"What is Rust?"},
                {"id":"2","type":"gemini","content":"Rust is a systems language."}
            ]
        }"#;
        let msgs = parse_gemini_json(raw);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, ChatRole::User);
        assert_eq!(msgs[0].content, "What is Rust?");
        assert_eq!(msgs[1].role, ChatRole::Assistant);
        assert_eq!(msgs[1].content, "Rust is a systems language.");
    }

    #[test]
    fn normalise_path_windows_and_unix() {
        assert_eq!(
            normalise_path(r"C:\Users\Me\Project"),
            "c:/users/me/project"
        );
        assert_eq!(normalise_path("/home/me/project/"), "/home/me/project");
    }

    #[test]
    fn find_project_slug_reads_projects_json() {
        let dir = std::env::temp_dir().join("gemini_test_home");
        let gemini_dir = dir.join(".gemini");
        std::fs::create_dir_all(&gemini_dir).unwrap();
        let projects_json = r#"{"projects": {"c:\\users\\me\\myproject": "myproject"}}"#;
        std::fs::write(gemini_dir.join("projects.json"), projects_json).unwrap();

        let workdir = std::path::Path::new(r"C:\Users\Me\MyProject");
        let slug = find_project_slug(&dir, workdir);
        assert_eq!(slug.as_deref(), Some("myproject"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parse_new_format_content_as_array() {
        // Real Gemini CLI format: user content is an array of parts.
        let raw = r#"{
            "sessionId": "14d02c74-4b93-4d88-9851-e0979f90ce3a",
            "startTime": "2026-04-10T16:13:02.964Z",
            "messages": [
                {"id":"1","type":"user","content":[{"text":"Say exactly: hello from gate4agent RPC."}]},
                {"id":"2","type":"gemini","content":"hello from gate4agent RPC."}
            ]
        }"#;
        let msgs = parse_gemini_json(raw);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, ChatRole::User);
        assert_eq!(msgs[0].content, "Say exactly: hello from gate4agent RPC.");
        assert_eq!(msgs[1].role, ChatRole::Assistant);
    }

    #[test]
    fn read_session_preview_handles_content_array() {
        // Verify the preview function handles array content (the bug that was fixed).
        let json = r#"{
            "sessionId": "test",
            "messages": [
                {"type":"user","content":[{"text":"Hello from content array"}]},
                {"type":"gemini","content":"Response here"}
            ]
        }"#;
        let dir = std::env::temp_dir();
        let path = dir.join("gemini_preview_test.json");
        std::fs::write(&path, json).unwrap();
        let preview = read_session_preview(&path);
        assert_eq!(preview, "Hello from content array");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn read_session_preview_handles_content_string() {
        // Verify string content still works (legacy format).
        let json = r#"{
            "sessionId": "test",
            "messages": [
                {"type":"user","content":"Hello plain string"},
                {"type":"gemini","content":"Response here"}
            ]
        }"#;
        let dir = std::env::temp_dir();
        let path = dir.join("gemini_preview_string_test.json");
        std::fs::write(&path, json).unwrap();
        let preview = read_session_preview(&path);
        assert_eq!(preview, "Hello plain string");
        std::fs::remove_file(&path).ok();
    }
}
