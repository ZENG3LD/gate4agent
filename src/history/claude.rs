//! Claude Code session reader.
//!
//! Claude Code stores sessions at:
//!   {workdir}/.claude/projects/{mangled-cwd}/{session-uuid}.jsonl
//!
//! `mangled-cwd` is the absolute workdir path with `\` and `/` and `:` replaced by `-`,
//! and a leading `-` prefix. Example: `c:\Users\VA PC\foo` -> `-c--Users-VA-PC-foo`.
//!
//! Each .jsonl line is one event with `type` field: "user", "assistant", "summary", etc.
//! User messages contain `message.content` (string or array of {type:"text", text}).
//! Assistant messages contain `message.content` (array of {type:"text", text} or {type:"tool_use", ...}).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Deserialize;

use crate::snapshot::{ChatMessage, ChatRole};
use super::{HistoryReader, SessionMeta};

pub struct ClaudeHistoryReader;

impl HistoryReader for ClaudeHistoryReader {
    fn list_sessions(&self, workdir: &Path) -> Vec<SessionMeta> {
        let projects_dir = match find_projects_dir(workdir) {
            Some(p) => p,
            None => return Vec::new(),
        };
        let entries = match fs::read_dir(&projects_dir) {
            Ok(e) => e,
            Err(_) => return Vec::new(),
        };
        let mut metas: Vec<SessionMeta> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            let id = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
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
            let preview = first_user_text(&path).unwrap_or_default();
            metas.push(SessionMeta { id, timestamp: ts, preview });
        }
        metas.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        metas
    }

    fn load_session(&self, workdir: &Path, session_id: &str) -> Vec<ChatMessage> {
        let projects_dir = match find_projects_dir(workdir) {
            Some(p) => p,
            None => return Vec::new(),
        };
        let path = projects_dir.join(format!("{}.jsonl", session_id));
        let raw = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::new();
        for line in raw.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(evt) = serde_json::from_str::<JsonlEvent>(line) {
                if let Some(msg) = evt.into_chat_message() {
                    out.push(msg);
                }
            }
        }
        out
    }
}

fn find_projects_dir(workdir: &Path) -> Option<PathBuf> {
    let base = workdir.join(".claude").join("projects");
    if !base.exists() {
        return None;
    }
    // Claude mangles the cwd path. The exact mangling is: replace each non-alphanumeric
    // character with `-`, prefix with `-`. We don't try to predict the exact name —
    // instead we list subdirs of `.claude/projects` and pick the one with our mangled cwd.
    // Easiest: if there's only one subdir, use it. Otherwise pick by best match.
    let mut subdirs: Vec<PathBuf> = match fs::read_dir(&base) {
        Ok(rd) => rd.flatten().filter(|e| e.path().is_dir()).map(|e| e.path()).collect(),
        Err(_) => return None,
    };
    if subdirs.is_empty() {
        return None;
    }
    if subdirs.len() == 1 {
        return Some(subdirs.remove(0));
    }
    // Multiple — pick the one whose name contains the workdir basename
    let basename = workdir.file_name().and_then(|s| s.to_str()).unwrap_or("");
    subdirs
        .iter()
        .find(|p| {
            p.file_name()
                .and_then(|s| s.to_str())
                .map(|n| n.contains(basename))
                .unwrap_or(false)
        })
        .cloned()
        .or_else(|| subdirs.into_iter().next())
}

fn first_user_text(path: &Path) -> Option<String> {
    let raw = fs::read_to_string(path).ok()?;
    for line in raw.lines() {
        if let Ok(evt) = serde_json::from_str::<JsonlEvent>(line) {
            if matches!(evt.event_type.as_deref(), Some("user")) {
                if let Some(msg) = evt.into_chat_message() {
                    let mut s = msg.content;
                    if s.len() > 80 { s.truncate(80); s.push_str("..."); }
                    return Some(s);
                }
            }
        }
    }
    None
}

#[derive(Deserialize)]
struct JsonlEvent {
    #[serde(rename = "type")]
    event_type: Option<String>,
    message: Option<JsonlMessage>,
}

#[derive(Deserialize)]
struct JsonlMessage {
    role: Option<String>,
    content: Option<serde_json::Value>,
}

impl JsonlEvent {
    fn into_chat_message(self) -> Option<ChatMessage> {
        let msg = self.message?;
        let role_str = msg.role.as_deref().unwrap_or("");
        let role = match role_str {
            "user" => ChatRole::User,
            "assistant" => ChatRole::Assistant,
            _ => return None,
        };
        let content = extract_text(msg.content?)?;
        if content.is_empty() {
            return None;
        }
        Some(ChatMessage { role, content, tool_name: None })
    }
}

fn extract_text(v: serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s),
        serde_json::Value::Array(arr) => {
            let mut out = String::new();
            for item in arr {
                if let Some(obj) = item.as_object() {
                    if obj.get("type").and_then(|t| t.as_str()) == Some("text") {
                        if let Some(t) = obj.get("text").and_then(|x| x.as_str()) {
                            if !out.is_empty() { out.push('\n'); }
                            out.push_str(t);
                        }
                    }
                }
            }
            if out.is_empty() { None } else { Some(out) }
        }
        _ => None,
    }
}
