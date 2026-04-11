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

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use serde::Deserialize;

static PROJECTS_DIR_CACHE: Mutex<Option<HashMap<PathBuf, Option<PathBuf>>>> = Mutex::new(None);

use crate::pty::snapshot::{ChatMessage, ChatRole};
use super::{HistoryReader, SessionMeta, SessionUsage};

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

    fn load_session_with_usage(
        &self,
        workdir: &Path,
        session_id: &str,
    ) -> (Vec<ChatMessage>, SessionUsage) {
        let projects_dir = match find_projects_dir(workdir) {
            Some(p) => p,
            None => return (Vec::new(), SessionUsage::default()),
        };
        let path = projects_dir.join(format!("{}.jsonl", session_id));
        let raw = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => return (Vec::new(), SessionUsage::default()),
        };
        let mut messages = Vec::new();
        let mut usage = SessionUsage::default();
        for line in raw.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(evt) = serde_json::from_str::<JsonlEvent>(line) {
                // Extract usage from assistant messages before consuming the event.
                if let Some(ref msg) = evt.message {
                    if msg.role.as_deref() == Some("assistant") {
                        if let Some(ref u) = msg.usage {
                            // input_tokens: replace (last turn's value reflects cumulative context)
                            if let Some(v) = u.input_tokens {
                                usage.input_tokens = v;
                            }
                            // output_tokens: accumulate across turns
                            if let Some(v) = u.output_tokens {
                                usage.output_tokens = usage.output_tokens.saturating_add(v);
                            }
                            // cache fields: replace (last turn is most recent)
                            if let Some(v) = u.cache_read_input_tokens {
                                usage.cache_read_tokens = v;
                            }
                            if let Some(v) = u.cache_creation_input_tokens {
                                usage.cache_write_tokens = v;
                            }
                        }
                    }
                }
                if let Some(msg) = evt.into_chat_message() {
                    messages.push(msg);
                }
            }
        }
        (messages, usage)
    }
}

fn find_projects_dir(workdir: &Path) -> Option<PathBuf> {
    // Cached lookup — avoid disk + log spam on per-frame calls.
    {
        let mut guard = PROJECTS_DIR_CACHE.lock().ok()?;
        let map = guard.get_or_insert_with(HashMap::new);
        if let Some(cached) = map.get(workdir) {
            return cached.clone();
        }
    }
    let result = find_projects_dir_uncached(workdir);
    if let Ok(mut guard) = PROJECTS_DIR_CACHE.lock() {
        if let Some(map) = guard.as_mut() {
            map.insert(workdir.to_path_buf(), result.clone());
        }
    }
    result
}

fn find_projects_dir_uncached(workdir: &Path) -> Option<PathBuf> {
    // Claude Code stores sessions at ~/.claude/projects/{mangled-cwd}/, NOT inside the cwd.
    // Claude's mangling: replace each non-alphanumeric char with '-'. NO leading dash.
    let home = home_dir()?;
    let base = home.join(".claude").join("projects");
    if !base.exists() {
        eprintln!("[gate4agent::history] base not found: {}", base.display());
        return None;
    }
    let mangled = mangle_cwd(workdir);
    eprintln!("[gate4agent::history] looking for cwd={} mangled={}", workdir.display(), mangled);
    let exact = base.join(&mangled);
    if exact.is_dir() {
        eprintln!("[gate4agent::history]   exact match: {}", exact.display());
        return Some(exact);
    }
    // Strict fallback: only accept exact name match (case-insensitive). No substring matching —
    // a shorter cwd's directory must NOT match a longer cwd.
    let subdirs: Vec<PathBuf> = match fs::read_dir(&base) {
        Ok(rd) => rd.flatten().filter(|e| e.path().is_dir()).map(|e| e.path()).collect(),
        Err(_) => return None,
    };
    let mangled_lower = mangled.to_lowercase();
    let result = subdirs.into_iter().find(|p| {
        p.file_name()
            .and_then(|s| s.to_str())
            .map(|n| n.to_lowercase() == mangled_lower)
            .unwrap_or(false)
    });
    if let Some(ref p) = result {
        eprintln!("[gate4agent::history]   exact (ci) match: {}", p.display());
    } else {
        eprintln!("[gate4agent::history]   no match in {}", base.display());
    }
    result
}

/// Clear the projects-dir cache. Call after a new session is created so we re-scan.
pub fn invalidate_projects_dir_cache() {
    if let Ok(mut guard) = PROJECTS_DIR_CACHE.lock() {
        *guard = None;
    }
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        if let Some(p) = std::env::var_os("USERPROFILE") {
            return Some(PathBuf::from(p));
        }
    }
    std::env::var_os("HOME").map(PathBuf::from)
}

fn mangle_cwd(path: &Path) -> String {
    // Claude's scheme: replace each non-alphanumeric char with '-'. No leading dash.
    let s = path.to_string_lossy();
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    out
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

#[derive(Deserialize, Default)]
struct JsonlUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct JsonlMessage {
    role: Option<String>,
    content: Option<serde_json::Value>,
    usage: Option<JsonlUsage>,
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
