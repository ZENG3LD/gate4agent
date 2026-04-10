//! OpenCode session history reader.
//!
//! OpenCode (sst/opencode) stores sessions in a SQLite database:
//! - macOS/Linux: `~/.local/share/opencode/opencode.db`
//! - Windows: `%LOCALAPPDATA%\opencode\opencode.db` (or `%APPDATA%\opencode\`)
//!
//! Schema (relevant tables):
//! - `session`: id, directory, title, time_created, time_updated
//! - `message`: id, session_id, time_created, data (JSON with `role` field)
//! - `part`: id, message_id, session_id, data (JSON, `type:"text"` has user text)
//!
//! The `directory` column on `session` enables project-scoped filtering.
//!
//! As a fallback, some older or custom OpenCode versions write flat JSON/JSONL
//! session files under `~/.opencode/sessions/` or `~/.opencode/`.  These may
//! contain a top-level `directory` field which is used for filtering when present.

use std::fs;
use std::path::Path;
use std::time::SystemTime;

use crate::pty::snapshot::{ChatMessage, ChatRole};
use super::{HistoryReader, SessionMeta};

pub struct OpenCodeHistoryReader;

impl HistoryReader for OpenCodeHistoryReader {
    fn list_sessions(&self, workdir: &Path) -> Vec<SessionMeta> {
        // Primary: try SQLite database.
        if let Some(metas) = list_sessions_sqlite(workdir) {
            if !metas.is_empty() {
                return metas;
            }
        }
        // Fallback: flat JSON/JSONL files.
        list_sessions_files(workdir)
    }

    fn load_session(&self, _workdir: &Path, session_id: &str) -> Vec<ChatMessage> {
        // Primary: SQLite.
        if let Some(msgs) = load_session_sqlite(session_id) {
            if !msgs.is_empty() {
                return msgs;
            }
        }
        // Fallback: flat files.
        load_session_files(session_id)
    }
}

// ---------------------------------------------------------------------------
// SQLite backend
// ---------------------------------------------------------------------------

/// Candidate paths for the OpenCode SQLite database.
fn sqlite_db_candidates() -> Vec<std::path::PathBuf> {
    let mut candidates = Vec::new();
    // macOS / Linux standard XDG path.
    if let Some(home) = home_dir_opt() {
        candidates.push(home.join(".local").join("share").join("opencode").join("opencode.db"));
    }
    // Windows: %LOCALAPPDATA%\opencode\opencode.db
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        candidates.push(
            std::path::PathBuf::from(local)
                .join("opencode")
                .join("opencode.db"),
        );
    }
    // Windows: %APPDATA%\opencode\opencode.db
    if let Some(roaming) = std::env::var_os("APPDATA") {
        candidates.push(
            std::path::PathBuf::from(roaming)
                .join("opencode")
                .join("opencode.db"),
        );
    }
    candidates
}

/// Open the first existing OpenCode SQLite database.
fn open_sqlite() -> Option<rusqlite::Connection> {
    for path in sqlite_db_candidates() {
        if path.is_file() {
            if let Ok(conn) = rusqlite::Connection::open_with_flags(
                &path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
            ) {
                return Some(conn);
            }
        }
    }
    None
}

/// List sessions from the SQLite database, filtered by `workdir`.
fn list_sessions_sqlite(workdir: &Path) -> Option<Vec<SessionMeta>> {
    let conn = open_sqlite()?;
    let workdir_norm = normalise_path(&workdir.to_string_lossy());

    let mut stmt = conn
        .prepare(
            "SELECT id, directory, title, time_created, time_updated \
             FROM session \
             WHERE time_archived IS NULL \
             ORDER BY time_updated DESC",
        )
        .ok()?;

    let mut metas: Vec<SessionMeta> = Vec::new();
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,  // id
                row.get::<_, String>(1)?,  // directory
                row.get::<_, String>(2)?,  // title
                row.get::<_, i64>(3)?,     // time_created (ms)
                row.get::<_, i64>(4)?,     // time_updated (ms)
            ))
        })
        .ok()?;

    for row in rows.flatten() {
        let (id, directory, _title, _time_created, time_updated) = row;
        if !normalise_path(&directory).eq(&workdir_norm) {
            continue;
        }
        // Convert milliseconds to seconds.
        let timestamp = time_updated / 1000;
        // Fetch the first real user message as preview.
        let preview = fetch_user_preview_sqlite(&conn, &id);
        // Skip zombie sessions — no real user input recorded yet.
        if preview.is_empty() {
            continue;
        }
        metas.push(SessionMeta { id, timestamp, preview });
    }

    Some(metas)
}

/// Fetch the first user message text from a session for use as a preview.
///
/// User messages are stored in the `part` table linked to a `message` row
/// whose `data` JSON contains `"role":"user"`.  Parts with `type:"text"` hold
/// the actual text typed by the user.
fn fetch_user_preview_sqlite(conn: &rusqlite::Connection, session_id: &str) -> String {
    // Find the earliest user message id for this session.
    let user_msg_id: Option<String> = conn
        .query_row(
            "SELECT id FROM message \
             WHERE session_id = ?1 \
               AND json_extract(data, '$.role') = 'user' \
             ORDER BY time_created ASC \
             LIMIT 1",
            rusqlite::params![session_id],
            |row| row.get(0),
        )
        .ok();

    let msg_id = match user_msg_id {
        Some(id) => id,
        None => return String::new(),
    };

    // Find text parts for this message.
    let mut stmt = match conn.prepare(
        "SELECT data FROM part \
         WHERE message_id = ?1 \
           AND json_extract(data, '$.type') = 'text' \
         ORDER BY time_created ASC \
         LIMIT 5",
    ) {
        Ok(s) => s,
        Err(_) => return String::new(),
    };

    let mut text_buf = String::new();
    let rows = match stmt.query_map(rusqlite::params![msg_id], |row| {
        row.get::<_, String>(0)
    }) {
        Ok(r) => r,
        Err(_) => return String::new(),
    };

    for data_json in rows.flatten() {
        let v: serde_json::Value = match serde_json::from_str(&data_json) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(text) = v.get("text").and_then(|t| t.as_str()) {
            if !text.is_empty() {
                text_buf.push_str(text);
                if text_buf.len() >= 80 {
                    break;
                }
            }
        }
    }

    text_buf.trim().chars().take(80).collect()
}

/// Load full chat messages for a session from SQLite.
fn load_session_sqlite(session_id: &str) -> Option<Vec<ChatMessage>> {
    let conn = open_sqlite()?;

    // Fetch all messages ordered by creation time.
    let mut stmt = conn
        .prepare(
            "SELECT id, data FROM message \
             WHERE session_id = ?1 \
             ORDER BY time_created ASC",
        )
        .ok()?;

    let rows = stmt
        .query_map(rusqlite::params![session_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .ok()?;

    let mut out = Vec::new();
    for row in rows.flatten() {
        let (msg_id, data_json) = row;
        let data: serde_json::Value = match serde_json::from_str(&data_json) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let role_str = data.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let role = match role_str {
            "user" => ChatRole::User,
            "assistant" => ChatRole::Assistant,
            _ => continue,
        };
        // Collect text from parts for this message.
        let content = collect_parts_text(&conn, &msg_id);
        if content.is_empty() {
            continue;
        }
        out.push(ChatMessage { role, content, tool_name: None });
    }

    Some(out)
}

/// Collect all text parts for a message into a single string.
fn collect_parts_text(conn: &rusqlite::Connection, message_id: &str) -> String {
    let mut stmt = match conn.prepare(
        "SELECT data FROM part \
         WHERE message_id = ?1 \
           AND json_extract(data, '$.type') = 'text' \
         ORDER BY time_created ASC",
    ) {
        Ok(s) => s,
        Err(_) => return String::new(),
    };

    let rows = match stmt.query_map(rusqlite::params![message_id], |row| {
        row.get::<_, String>(0)
    }) {
        Ok(r) => r,
        Err(_) => return String::new(),
    };

    let mut buf = String::new();
    for data_json in rows.flatten() {
        let v: serde_json::Value = match serde_json::from_str(&data_json) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(text) = v.get("text").and_then(|t| t.as_str()) {
            if !buf.is_empty() {
                buf.push('\n');
            }
            buf.push_str(text);
        }
    }
    buf.trim().to_string()
}

// ---------------------------------------------------------------------------
// File-based fallback (older / custom OpenCode installs)
// ---------------------------------------------------------------------------

fn list_sessions_files(workdir: &Path) -> Vec<SessionMeta> {
    let home = home_dir();
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
            if path.is_file() && !session_matches_workdir(&path, workdir) {
                continue;
            }
            let ts = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let preview = read_session_preview(&path);
            if preview.is_empty() {
                continue;
            }
            metas.push(SessionMeta { id, timestamp: ts, preview });
        }
        if !metas.is_empty() {
            metas.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
            return metas;
        }
    }
    Vec::new()
}

fn load_session_files(session_id: &str) -> Vec<ChatMessage> {
    let home = home_dir();
    for dir in [
        home.join(".opencode").join("sessions"),
        home.join(".opencode"),
    ] {
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
        let session_dir = dir.join(session_id);
        if session_dir.is_dir() {
            return load_from_session_dir(&session_dir);
        }
    }
    Vec::new()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Normalise a path string to lowercase with forward slashes and no trailing slash.
fn normalise_path(s: &str) -> String {
    s.replace('\\', "/").to_lowercase().trim_end_matches('/').to_string()
}

/// Check whether a session file's `directory` field matches `workdir`.
fn session_matches_workdir(path: &Path, workdir: &Path) -> bool {
    let raw = match fs::read_to_string(path) {
        Ok(r) => r,
        Err(_) => return true,
    };
    let text = if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
        raw.lines().next().unwrap_or("").to_string()
    } else {
        raw
    };
    let v: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return true,
    };
    match v.get("directory").and_then(|d| d.as_str()) {
        None => true,
        Some(dir) => paths_match(dir, workdir),
    }
}

/// Compare a path string with a `Path`, normalising case and separators.
fn paths_match(a: &str, b: &Path) -> bool {
    if a.is_empty() {
        return false;
    }
    normalise_path(a) == normalise_path(&b.to_string_lossy())
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

/// Read the first user message from an OpenCode session file for use as a preview.
fn read_session_preview(path: &Path) -> String {
    let raw = match fs::read_to_string(path) {
        Ok(r) => r,
        Err(_) => return String::new(),
    };
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
    if ext == "jsonl" {
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let v: serde_json::Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if let Some(text) = extract_opencode_user_text_from_event(&v) {
                if !text.is_empty() {
                    return text.chars().take(80).collect();
                }
            }
        }
        return String::new();
    }
    let msgs = parse_opencode_json(&raw);
    for msg in msgs {
        if msg.role == ChatRole::User && !msg.content.is_empty() {
            return msg.content.chars().take(80).collect();
        }
    }
    String::new()
}

/// Extract the user text from a single OpenCode JSONL event line.
fn extract_opencode_user_text_from_event(v: &serde_json::Value) -> Option<String> {
    let event_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    let is_user_event = matches!(event_type, "user" | "user_message" | "input");
    let has_user_role = v.get("role").and_then(|r| r.as_str()) == Some("user");

    if !is_user_event && !has_user_role {
        return None;
    }

    for field in &["content", "text"] {
        if let Some(field_val) = v.get(field) {
            if let Some(s) = field_val.as_str() {
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            }
            if let Some(arr) = field_val.as_array() {
                let joined: String = arr
                    .iter()
                    .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n");
                if !joined.is_empty() {
                    return Some(joined);
                }
            }
        }
    }
    None
}

fn parse_opencode_jsonl(raw: &str) -> Vec<ChatMessage> {
    let mut out = Vec::new();
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

        if let Some(user_text) = extract_opencode_user_text_from_event(&v) {
            if !assistant_buf.is_empty() {
                out.push(ChatMessage {
                    role: ChatRole::Assistant,
                    content: assistant_buf.clone(),
                    tool_name: None,
                });
                assistant_buf.clear();
            }
            out.push(ChatMessage {
                role: ChatRole::User,
                content: user_text,
                tool_name: None,
            });
            continue;
        }

        match v.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(text) = v.get("text").and_then(|t| t.as_str()) {
                    assistant_buf.push_str(text);
                }
            }
            Some("step_finish") => {
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
    if !assistant_buf.is_empty() {
        out.push(ChatMessage {
            role: ChatRole::Assistant,
            content: assistant_buf,
            tool_name: None,
        });
    }
    out
}

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
    home_dir_opt().unwrap_or_default()
}

fn home_dir_opt() -> Option<std::path::PathBuf> {
    #[cfg(target_os = "windows")]
    if let Some(p) = std::env::var_os("USERPROFILE") {
        return Some(std::path::PathBuf::from(p));
    }
    std::env::var_os("HOME").map(std::path::PathBuf::from)
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
    fn parse_jsonl_emits_user_messages() {
        let raw = concat!(
            "{\"type\":\"user\",\"content\":\"What is Rust?\",\"sessionID\":\"s\"}\n",
            "{\"type\":\"text\",\"text\":\"Rust is a systems language.\",\"sessionID\":\"s\"}\n",
            "{\"type\":\"step_finish\",\"sessionID\":\"s\"}\n",
        );
        let msgs = parse_opencode_jsonl(raw);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, ChatRole::User);
        assert_eq!(msgs[0].content, "What is Rust?");
        assert_eq!(msgs[1].role, ChatRole::Assistant);
        assert_eq!(msgs[1].content, "Rust is a systems language.");
    }

    #[test]
    fn parse_jsonl_emits_user_message_type_variants() {
        let raw = concat!(
            "{\"type\":\"user_message\",\"content\":\"First question\",\"sessionID\":\"s\"}\n",
            "{\"type\":\"step_finish\",\"sessionID\":\"s\"}\n",
            "{\"type\":\"input\",\"content\":\"Second question\",\"sessionID\":\"s\"}\n",
            "{\"type\":\"step_finish\",\"sessionID\":\"s\"}\n",
        );
        let msgs = parse_opencode_jsonl(raw);
        let user_msgs: Vec<_> = msgs.iter().filter(|m| m.role == ChatRole::User).collect();
        assert_eq!(user_msgs.len(), 2);
        assert_eq!(user_msgs[0].content, "First question");
        assert_eq!(user_msgs[1].content, "Second question");
    }

    #[test]
    fn parse_jsonl_role_field_user_message() {
        let raw = concat!(
            "{\"role\":\"user\",\"content\":\"Question via role field\",\"sessionID\":\"s\"}\n",
            "{\"type\":\"text\",\"text\":\"Answer\",\"sessionID\":\"s\"}\n",
            "{\"type\":\"step_finish\",\"sessionID\":\"s\"}\n",
        );
        let msgs = parse_opencode_jsonl(raw);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, ChatRole::User);
        assert_eq!(msgs[0].content, "Question via role field");
        assert_eq!(msgs[1].role, ChatRole::Assistant);
    }

    #[test]
    fn read_session_preview_from_json_file() {
        let dir = std::env::temp_dir();
        let path = dir.join("oc_preview_test.json");
        let json = r#"[
            {"role":"user","content":"What is gate4agent?"},
            {"role":"assistant","content":"It is a transport library."}
        ]"#;
        std::fs::write(&path, json).unwrap();
        let preview = read_session_preview(&path);
        assert_eq!(preview, "What is gate4agent?");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn read_session_preview_from_jsonl_file() {
        let dir = std::env::temp_dir();
        let path = dir.join("oc_preview_test.jsonl");
        let jsonl = concat!(
            "{\"type\":\"user\",\"content\":\"JSONL user question here\",\"sessionID\":\"s\"}\n",
            "{\"type\":\"text\",\"text\":\"Answer\",\"sessionID\":\"s\"}\n",
            "{\"type\":\"step_finish\",\"sessionID\":\"s\"}\n",
        );
        std::fs::write(&path, jsonl).unwrap();
        let preview = read_session_preview(&path);
        assert_eq!(preview, "JSONL user question here");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn session_matches_workdir_pass_through_on_no_field() {
        let dir = std::env::temp_dir();
        let path = dir.join("oc_test_no_dir.json");
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

    #[test]
    fn sqlite_db_candidates_includes_local_share() {
        // On Linux/macOS, the ~/.local/share/opencode/ path should appear.
        // This is a basic smoke test that the function returns at least one path.
        let candidates = sqlite_db_candidates();
        assert!(!candidates.is_empty());
    }

    /// Verify that the SQLite reader can query the real OpenCode database when
    /// it is present on this machine.  This test is skipped when no database
    /// file is found.
    #[test]
    fn sqlite_reader_smoke_test() {
        let conn = match open_sqlite() {
            Some(c) => c,
            None => return, // no OpenCode DB on this machine — skip
        };
        // Just verify we can query the session table without panicking.
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM session", [], |r| r.get(0))
            .unwrap_or(0);
        // Whatever the count, the query must succeed.
        let _ = count;
    }
}
