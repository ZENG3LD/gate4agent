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
//! **Old layout**: flat `rollout-<timestamp>-<uuid>.json` files (pre-v0.8, from 2025).
//! These have no `cwd` field and cannot be scoped to a project, so they are skipped
//! entirely when a workdir filter is active.

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
/// by `workdir` via the `session_meta` header.
///
/// Old-layout `.json` files (pre-v0.8, from 2025) contain no `cwd` field and
/// cannot be scoped to a project, so they are skipped entirely.
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
                // Exclude zombie sessions — Codex created the file but no real
                // user interaction happened (preview would be empty).
                if paths_match(&cwd, workdir) && !preview.is_empty() {
                    out.push(SessionMeta {
                        id: session_id,
                        timestamp: ts,
                        preview,
                    });
                }
            }
        }
        // Old-layout `.json` files (pre-v0.8) have no `cwd` field and cannot be
        // scoped to a project — skip them entirely.
    }
}

/// Read a `.jsonl` file and extract session id, cwd, and the first real user
/// message preview.
///
/// Reads the `session_meta` first line for `id` and `cwd`, then scans forward
/// to find the first genuine user message (skipping Codex-injected system
/// context).  Falls back to `first_message` in the metadata if present and no
/// real user message is found.
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
    // Try `first_message` from meta as a fallback (often absent in real sessions).
    let meta_preview: String = payload
        .get("first_message")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .chars()
        .take(80)
        .collect();

    // Scan the remaining lines to find the first genuine user message.
    //
    // Two formats exist:
    // - New (Codex v0.118+): `event_msg` with `payload.type == "user_message"` and
    //   `payload.message` holding the raw text typed by the user.
    // - Old (Codex <v0.118): `response_item` with `payload.type == "message"` and
    //   `payload.role == "user"`, filtered via `is_injected_system_content`.
    let mut preview = String::new();
    let mut scan_line = String::new();
    loop {
        scan_line.clear();
        match reader.read_line(&mut scan_line) {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(_) => break,
        }
        let trimmed = scan_line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let lv: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let record_type = lv.get("type").and_then(|t| t.as_str()).unwrap_or("");

        // New format: event_msg with payload.type == "user_message".
        if record_type == "event_msg" {
            let lp = match lv.get("payload") {
                Some(p) => p,
                None => continue,
            };
            if lp.get("type").and_then(|t| t.as_str()) != Some("user_message") {
                continue;
            }
            let text = lp
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if !text.is_empty() {
                preview = text.chars().take(80).collect();
                break;
            }
            continue;
        }

        // Old format: response_item with payload.role == "user".
        if record_type != "response_item" {
            continue;
        }
        let lp = match lv.get("payload") {
            Some(p) => p,
            None => continue,
        };
        if lp.get("type").and_then(|t| t.as_str()) != Some("message") {
            continue;
        }
        if lp.get("role").and_then(|r| r.as_str()) != Some("user") {
            continue;
        }
        let text = extract_content(lp.get("content").unwrap_or(&serde_json::Value::Null));
        if is_injected_system_content(&text) {
            continue;
        }
        if !text.is_empty() {
            preview = text.chars().take(80).collect();
            break;
        }
    }

    // Fall back to first_message from meta when no real user message was found.
    if preview.is_empty() {
        preview = meta_preview;
    }

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
/// Two record types carry message content:
/// - `event_msg` with `payload.type == "user_message"`: canonical user input
///   (Codex v0.93+). The text is in `payload.message`.
/// - `response_item` with `payload.role == "assistant"`: agent responses.
///
/// `response_item` entries with `role == "user"` are skipped entirely — Codex
/// v0.93+ echoes every user message as a `response_item` in addition to the
/// `event_msg`, causing duplicates. The `event_msg` is the canonical source.
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

        // New format: real user message via event_msg.
        if record_type == "event_msg" {
            let payload = match v.get("payload") {
                Some(p) => p,
                None => continue,
            };
            if payload.get("type").and_then(|t| t.as_str()) != Some("user_message") {
                continue;
            }
            let text = payload
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if !text.is_empty() {
                out.push(ChatMessage { role: ChatRole::User, content: text, tool_name: None });
            }
            continue;
        }

        // response_item: only emit assistant messages.
        // User entries are skipped — Codex v0.93+ duplicates them from event_msg.
        if record_type != "response_item" {
            continue;
        }
        let payload = match v.get("payload") {
            Some(p) => p,
            None => continue,
        };
        // Skip user role — canonical source is event_msg (avoids duplicates).
        if payload.get("role").and_then(|r| r.as_str()) == Some("user") {
            continue;
        }
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
    // Skip Codex-injected system context that appears as "user" role messages.
    if role == ChatRole::User && is_injected_system_content(&content) {
        return None;
    }
    Some(ChatMessage { role, content, tool_name: None })
}

/// Detect whether a "user" role message is actually system context injected by
/// Codex (environment info, AGENTS.md, IDE context) rather than a real user
/// message.
///
/// Heuristics:
/// - Starts with `<` and is longer than 500 chars → XML environment block.
/// - Starts with `# ` → markdown section header (AGENTS.md, IDE open tabs, etc.).
/// - Starts with `## ` → same as above.
/// - Contains `<environment_context>` or `<INSTRUCTIONS>` → XML injected blocks.
fn is_injected_system_content(text: &str) -> bool {
    let trimmed = text.trim_start();
    if trimmed.is_empty() {
        return false;
    }
    // XML blocks injected by Codex are long and start with `<`.
    if trimmed.starts_with('<') && text.len() > 500 {
        return true;
    }
    // Shorter XML that is clearly a system tag.
    if trimmed.contains("<environment_context>") || trimmed.contains("<INSTRUCTIONS>") {
        return true;
    }
    // Markdown headers used for AGENTS.md, IDE context, open tabs, etc.
    // Real short user messages rarely start with `# ` or `## `.
    if (trimmed.starts_with("# ") || trimmed.starts_with("## ")) && text.len() > 200 {
        return true;
    }
    false
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
        // User message comes via event_msg (canonical since v0.93+).
        // The response_item with role=user is the duplicate echo — it must be dropped.
        let raw = concat!(
            r#"{"type":"session_meta","payload":{"id":"abc","cwd":"/p","first_message":"Hi"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"Hello","images":[]}}"#,
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

    #[test]
    fn injected_xml_context_is_filtered() {
        // The environment_context block (>500 chars, starts with `<`) should be dropped.
        let env_block = format!(
            "<environment_context><cwd>c:\\\\users\\\\me</cwd>{}</environment_context>",
            "x".repeat(600),
        );
        assert!(is_injected_system_content(&env_block));
        // A short XML snippet is NOT filtered (could be user pasting XML).
        assert!(!is_injected_system_content("<tag>short</tag>"));
        // Markdown header with body > 200 chars should be filtered.
        let md_header = format!("# AGENTS.md instructions\n\n{}", "y".repeat(300));
        assert!(is_injected_system_content(&md_header));
        // A short markdown heading is NOT filtered.
        assert!(!is_injected_system_content("# Short heading"));
        // Normal user text is NOT filtered.
        assert!(!is_injected_system_content("Fix the bug in auth.rs"));
    }

    #[test]
    fn parse_codex_jsonl_skips_injected_context() {
        // Injected system context arrives as response_item with role=user — dropped
        // by the user-role skip rule. Real user input arrives via event_msg only.
        let env_block = format!(
            "<environment_context><cwd>/home/user</cwd>{}</environment_context>",
            "x".repeat(600),
        );
        let raw = format!(
            "{}\n{}\n{}\n",
            r#"{"type":"session_meta","payload":{"id":"abc","cwd":"/p"}}"#,
            serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text": env_block}]}}).to_string(),
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"Real user message","images":[]}}"#,
        );
        let msgs = parse_codex_jsonl(&raw);
        // Only the real user message from event_msg should appear.
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "Real user message");
    }

    #[test]
    fn read_jsonl_meta_extracts_event_msg_user_message() {
        // New Codex format (v0.118+): real user input comes via event_msg.
        let jsonl = concat!(
            r#"{"timestamp":"2026-04-10T10:00:00Z","type":"session_meta","payload":{"id":"evt-test","cwd":"/home/user/project"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"<permissions instructions>..."}]}}"#,
            "\n",
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"# AGENTS.md instructions\\n\\n<INSTRUCTIONS>...</INSTRUCTIONS>\"}]}}",
            "\n",
            r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"abc"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"Fix the memory leak in allocator.rs","images":[]}}"#,
            "\n",
        );
        let dir = std::env::temp_dir();
        let path = dir.join("test_codex_event_msg.jsonl");
        std::fs::write(&path, jsonl).unwrap();
        let result = read_jsonl_meta(&path);
        assert!(result.is_some());
        let (id, cwd, preview) = result.unwrap();
        assert_eq!(id, "evt-test");
        assert_eq!(cwd, "/home/user/project");
        assert_eq!(preview, "Fix the memory leak in allocator.rs");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn parse_codex_jsonl_extracts_event_msg_user_messages() {
        // New format: event_msg carries real user input; response_item carries assistant.
        let raw = concat!(
            r#"{"type":"session_meta","payload":{"id":"abc","cwd":"/p"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"t1"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"Hello from event_msg","images":[]}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":"Hi there!"}}"#,
            "\n",
        );
        let msgs = parse_codex_jsonl(raw);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, ChatRole::User);
        assert_eq!(msgs[0].content, "Hello from event_msg");
        assert_eq!(msgs[1].role, ChatRole::Assistant);
        assert_eq!(msgs[1].content, "Hi there!");
    }

    #[test]
    fn read_jsonl_meta_falls_back_to_scanning_when_no_first_message() {
        let env_block = format!(
            "<environment_context><cwd>/home/user</cwd>{}</environment_context>",
            "x".repeat(600),
        );
        let jsonl = format!(
            "{}\n{}\n{}\n",
            r#"{"type":"session_meta","payload":{"id":"scan-test","cwd":"/home/user/project"}}"#,
            serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text": env_block}]}}).to_string(),
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Actual question from the user"}]}}"#,
        );
        let dir = std::env::temp_dir();
        let path = dir.join("test_codex_scan_preview.jsonl");
        std::fs::write(&path, &jsonl).unwrap();
        let result = read_jsonl_meta(&path);
        assert!(result.is_some());
        let (id, _cwd, preview) = result.unwrap();
        assert_eq!(id, "scan-test");
        assert_eq!(preview, "Actual question from the user");
        std::fs::remove_file(&path).ok();
    }
}
