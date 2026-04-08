//! NDJSON stream parsers for Claude Code, Codex, and Gemini CLI.
//!
//! Each AI CLI tool supports a headless mode that outputs newline-delimited JSON.
//! This module parses those streams into a unified CliEvent type.

use serde_json::Value;

use super::traits::{CliEvent, NdjsonParser};
use crate::types::CliTool;
use crate::utils::truncate_str;

/// Claude Code stream-json parser.
///
/// Expects output from: `claude -p "prompt" --output-format stream-json --verbose`
///
/// Event types: "system" (init), "assistant" (text/tool_use), "user" (tool_result), "result" (final)
pub struct ClaudeNdjsonParser {
    session_id: Option<String>,
}

impl ClaudeNdjsonParser {
    pub fn new() -> Self {
        Self { session_id: None }
    }
}

impl Default for ClaudeNdjsonParser {
    fn default() -> Self {
        Self::new()
    }
}

impl NdjsonParser for ClaudeNdjsonParser {
    fn parse_line(&mut self, line: &str) -> Vec<CliEvent> {
        let line = line.trim();
        if line.is_empty() {
            return vec![];
        }

        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                return vec![CliEvent::Error {
                    message: format!("invalid JSON: {}", truncate_str(line, 100)),
                }]
            }
        };

        let mut events = Vec::new();

        match v.get("type").and_then(|t| t.as_str()) {
            Some("system") => {
                let sid = v
                    .get("session_id")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let model = v
                    .get("model")
                    .and_then(|s| s.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                let tools = v
                    .get("tools")
                    .and_then(|t| t.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                self.session_id = Some(sid.clone());
                events.push(CliEvent::SessionStart {
                    session_id: sid,
                    model,
                    tools,
                });
            }
            Some("assistant") => {
                if let Some(content) =
                    v.pointer("/message/content").and_then(|c| c.as_array())
                {
                    for block in content {
                        match block.get("type").and_then(|t| t.as_str()) {
                            Some("text") => {
                                if let Some(text) =
                                    block.get("text").and_then(|t| t.as_str())
                                {
                                    events.push(CliEvent::AssistantText {
                                        text: text.to_string(),
                                        is_delta: false,
                                    });
                                }
                            }
                            Some("tool_use") => {
                                let id = block
                                    .get("id")
                                    .and_then(|s| s.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let name = block
                                    .get("name")
                                    .and_then(|s| s.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let input = block
                                    .get("input")
                                    .cloned()
                                    .unwrap_or(Value::Null);
                                events.push(CliEvent::ToolCallStart { id, name, input });
                            }
                            Some("thinking") => {
                                if let Some(text) =
                                    block.get("thinking").and_then(|t| t.as_str())
                                {
                                    events.push(CliEvent::Thinking {
                                        text: text.to_string(),
                                    });
                                }
                            }
                            _ => {}
                        }
                    }
                }
                if let Some(usage) = v.pointer("/message/usage") {
                    let input = usage
                        .get("input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let output = usage
                        .get("output_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    if input > 0 || output > 0 {
                        events.push(CliEvent::TurnComplete {
                            input_tokens: input,
                            output_tokens: output,
                        });
                    }
                }
            }
            Some("user") => {
                if let Some(content) =
                    v.pointer("/message/content").and_then(|c| c.as_array())
                {
                    for block in content {
                        if block.get("type").and_then(|t| t.as_str())
                            == Some("tool_result")
                        {
                            let id = block
                                .get("tool_use_id")
                                .and_then(|s| s.as_str())
                                .unwrap_or("")
                                .to_string();
                            let output = block
                                .get("content")
                                .and_then(|s| s.as_str())
                                .unwrap_or("")
                                .to_string();
                            let is_error = block
                                .get("is_error")
                                .and_then(|b| b.as_bool())
                                .unwrap_or(false);
                            let duration_ms = v
                                .pointer("/tool_use_result/durationMs")
                                .and_then(|d| d.as_u64());
                            events.push(CliEvent::ToolCallResult {
                                id,
                                output,
                                is_error,
                                duration_ms,
                            });
                        }
                    }
                }
            }
            Some("result") => {
                let result_text = v
                    .get("result")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let cost = v.get("total_cost_usd").and_then(|c| c.as_f64());
                let is_error = v
                    .get("is_error")
                    .and_then(|b| b.as_bool())
                    .unwrap_or(false);
                events.push(CliEvent::SessionEnd {
                    result: result_text,
                    cost_usd: cost,
                    is_error,
                });
            }
            Some("stream_event") => {
                if let Some(delta_text) = v.pointer("/event/delta/text") {
                    if let Some(text) = delta_text.as_str() {
                        events.push(CliEvent::AssistantText {
                            text: text.to_string(),
                            is_delta: true,
                        });
                    }
                }
            }
            _ => {}
        }

        events
    }

    fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }
}

/// Codex CLI JSONL parser.
///
/// Expects output from: `codex exec --json "prompt"`
///
/// Event types: "thread.started", "turn.started", "turn.completed", "item.started", "item.completed"
pub struct CodexNdjsonParser {
    thread_id: Option<String>,
}

impl CodexNdjsonParser {
    pub fn new() -> Self {
        Self { thread_id: None }
    }
}

impl Default for CodexNdjsonParser {
    fn default() -> Self {
        Self::new()
    }
}

impl NdjsonParser for CodexNdjsonParser {
    fn parse_line(&mut self, line: &str) -> Vec<CliEvent> {
        let line = line.trim();
        if line.is_empty() {
            return vec![];
        }

        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                return vec![CliEvent::Error {
                    message: format!("invalid JSON: {}", truncate_str(line, 100)),
                }]
            }
        };

        let mut events = Vec::new();

        match v.get("type").and_then(|t| t.as_str()) {
            Some("thread.started") => {
                let tid = v
                    .get("thread_id")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                self.thread_id = Some(tid.clone());
                events.push(CliEvent::SessionStart {
                    session_id: tid,
                    model: "codex".to_string(),
                    tools: vec![],
                });
            }
            Some("turn.completed") => {
                if let Some(usage) = v.get("usage") {
                    let input = usage
                        .get("input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let output = usage
                        .get("output_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    events.push(CliEvent::TurnComplete {
                        input_tokens: input,
                        output_tokens: output,
                    });
                }
            }
            Some("turn.failed") => {
                let msg = v
                    .get("error")
                    .and_then(|e| e.as_str())
                    .unwrap_or("turn failed")
                    .to_string();
                events.push(CliEvent::Error { message: msg });
            }
            Some("item.started") => {
                if let Some(item) = v.get("item") {
                    match item.get("type").and_then(|t| t.as_str()) {
                        Some("command_execution") => {
                            let id = item
                                .get("id")
                                .and_then(|s| s.as_str())
                                .unwrap_or("")
                                .to_string();
                            let cmd = item
                                .get("command")
                                .and_then(|s| s.as_str())
                                .unwrap_or("")
                                .to_string();
                            events.push(CliEvent::ToolCallStart {
                                id,
                                name: "Bash".to_string(),
                                input: serde_json::json!({"command": cmd}),
                            });
                        }
                        // Both "agent_message" and "assistant_message" are valid names
                        // depending on the Codex version. Treat them as aliases.
                        Some("agent_message") | Some("assistant_message") => {
                            // Message starting — will get content in item.completed
                        }
                        Some("file_change") => {
                            let id = item
                                .get("id")
                                .and_then(|s| s.as_str())
                                .unwrap_or("")
                                .to_string();
                            events.push(CliEvent::ToolCallStart {
                                id,
                                name: "FileChange".to_string(),
                                input: item.clone(),
                            });
                        }
                        Some("reasoning") => {
                            if let Some(text) =
                                item.get("text").and_then(|s| s.as_str())
                            {
                                events.push(CliEvent::Thinking {
                                    text: text.to_string(),
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
            Some("item.completed") => {
                if let Some(item) = v.get("item") {
                    match item.get("type").and_then(|t| t.as_str()) {
                        Some("command_execution") => {
                            let id = item
                                .get("id")
                                .and_then(|s| s.as_str())
                                .unwrap_or("")
                                .to_string();
                            // [BUGFIX] Codex emits "aggregated_output", not "output".
                            // Reading "output" always returned an empty string for all
                            // shell command results in pipe mode.
                            let output = item
                                .get("aggregated_output")
                                .and_then(|s| s.as_str())
                                .unwrap_or("")
                                .to_string();
                            let status =
                                item.get("status").and_then(|s| s.as_str()).unwrap_or("");
                            events.push(CliEvent::ToolCallResult {
                                id,
                                output,
                                is_error: status == "failed",
                                duration_ms: None,
                            });
                        }
                        // Both "agent_message" and "assistant_message" are valid names
                        // depending on the Codex version. Treat them as aliases.
                        Some("agent_message") | Some("assistant_message") => {
                            let text = item
                                .get("text")
                                .and_then(|s| s.as_str())
                                .unwrap_or("")
                                .to_string();
                            if !text.is_empty() {
                                events.push(CliEvent::AssistantText {
                                    text,
                                    is_delta: false,
                                });
                            }
                        }
                        Some("file_change") => {
                            let id = item
                                .get("id")
                                .and_then(|s| s.as_str())
                                .unwrap_or("")
                                .to_string();
                            events.push(CliEvent::ToolCallResult {
                                id,
                                output: "file changed".to_string(),
                                is_error: false,
                                duration_ms: None,
                            });
                        }
                        _ => {}
                    }
                }
            }
            Some("error") => {
                let msg = v
                    .get("message")
                    .and_then(|s| s.as_str())
                    .or_else(|| v.get("error").and_then(|s| s.as_str()))
                    .unwrap_or("unknown error")
                    .to_string();
                events.push(CliEvent::Error { message: msg });
            }
            _ => {}
        }

        events
    }

    fn session_id(&self) -> Option<&str> {
        self.thread_id.as_deref()
    }
}

/// Gemini CLI stream-json parser.
///
/// Expects output from: `gemini --output-format stream-json --prompt "prompt"`
///
/// Event types: "init", "message", "tool_use", "tool_result", "result"
pub struct GeminiNdjsonParser {
    session_id: Option<String>,
}

impl GeminiNdjsonParser {
    pub fn new() -> Self {
        Self { session_id: None }
    }
}

impl Default for GeminiNdjsonParser {
    fn default() -> Self {
        Self::new()
    }
}

impl NdjsonParser for GeminiNdjsonParser {
    fn parse_line(&mut self, line: &str) -> Vec<CliEvent> {
        let line = line.trim();
        if line.is_empty() {
            return vec![];
        }

        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                return vec![CliEvent::Error {
                    message: format!("invalid JSON: {}", truncate_str(line, 100)),
                }]
            }
        };

        let mut events = Vec::new();

        match v.get("type").and_then(|t| t.as_str()) {
            Some("init") => {
                let sid = v
                    .get("session_id")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let model = v
                    .get("model")
                    .and_then(|s| s.as_str())
                    .unwrap_or("gemini")
                    .to_string();
                self.session_id = Some(sid.clone());
                events.push(CliEvent::SessionStart {
                    session_id: sid,
                    model,
                    tools: vec![],
                });
            }
            Some("message") => {
                let role = v
                    .get("role")
                    .and_then(|s| s.as_str())
                    .unwrap_or("");
                let content = v
                    .get("content")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let is_delta = v
                    .get("delta")
                    .and_then(|b| b.as_bool())
                    .unwrap_or(false);
                if role == "assistant" && !content.is_empty() {
                    events.push(CliEvent::AssistantText { text: content, is_delta });
                }
            }
            Some("tool_use") => {
                let id = v
                    .get("tool_id")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = v
                    .get("tool_name")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let params = v.get("parameters").cloned().unwrap_or(Value::Null);
                events.push(CliEvent::ToolCallStart { id, name, input: params });
            }
            Some("tool_result") => {
                let id = v
                    .get("tool_id")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let output = v
                    .get("output")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let status = v
                    .get("status")
                    .and_then(|s| s.as_str())
                    .unwrap_or("success");
                events.push(CliEvent::ToolCallResult {
                    id,
                    output,
                    is_error: status != "success",
                    duration_ms: None,
                });
            }
            Some("error") => {
                let msg = v
                    .get("message")
                    .and_then(|s| s.as_str())
                    .unwrap_or("unknown error")
                    .to_string();
                events.push(CliEvent::Error { message: msg });
            }
            Some("result") => {
                let status = v
                    .get("status")
                    .and_then(|s| s.as_str())
                    .unwrap_or("success");
                let is_error = status != "success";
                if let Some(stats) = v.get("stats") {
                    let input = stats
                        .get("input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let output = stats
                        .get("output_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    if input > 0 || output > 0 {
                        events.push(CliEvent::TurnComplete {
                            input_tokens: input,
                            output_tokens: output,
                        });
                    }
                }
                events.push(CliEvent::SessionEnd {
                    result: String::new(),
                    cost_usd: None,
                    is_error,
                });
            }
            _ => {}
        }

        events
    }

    fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }
}

/// Cursor Agent stream-json parser.
///
/// Cursor Agent's `--output-format stream-json` is documented as Claude-compatible:
/// it emits the same 5 event types (system/init, assistant, user, tool_use, result).
///
/// # Source
/// - https://cursor.com/docs/cli/headless — "stream-json format mirrors Claude Code"
/// - https://cursor.com/blog/cli (January 2026 announcement)
///
/// This parser is a copy of `ClaudeNdjsonParser` with Cursor-specific naming.
/// If a future live capture reveals divergence in field names or event shapes,
/// a targeted patch should be applied here without touching the Claude parser.
///
/// # Assumed identical to Claude per docs
/// All field paths (`type`, `session_id`, `model`, `tools`, `message/content`,
/// `tool_use_id`, `result`, `total_cost_usd`, `is_error`) are assumed identical
/// to the Claude stream-json schema. Divergences will be reconciled in a future
/// patch once live capture data is available.
pub struct CursorNdjsonParser {
    session_id: Option<String>,
}

impl CursorNdjsonParser {
    pub fn new() -> Self {
        Self { session_id: None }
    }
}

impl Default for CursorNdjsonParser {
    fn default() -> Self {
        Self::new()
    }
}

impl NdjsonParser for CursorNdjsonParser {
    fn parse_line(&mut self, line: &str) -> Vec<CliEvent> {
        let line = line.trim();
        if line.is_empty() {
            return vec![];
        }

        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                return vec![CliEvent::Error {
                    message: format!("invalid JSON: {}", truncate_str(line, 100)),
                }]
            }
        };

        let mut events = Vec::new();

        // Field names assumed identical to Claude stream-json per Cursor docs.
        // Source: https://cursor.com/docs/cli/headless
        match v.get("type").and_then(|t| t.as_str()) {
            Some("system") => {
                // session_id field: same as Claude "system" init event.
                let sid = v
                    .get("session_id")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let model = v
                    .get("model")
                    .and_then(|s| s.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                let tools = v
                    .get("tools")
                    .and_then(|t| t.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                self.session_id = Some(sid.clone());
                events.push(CliEvent::SessionStart {
                    session_id: sid,
                    model,
                    tools,
                });
            }
            Some("assistant") => {
                if let Some(content) =
                    v.pointer("/message/content").and_then(|c| c.as_array())
                {
                    for block in content {
                        match block.get("type").and_then(|t| t.as_str()) {
                            Some("text") => {
                                if let Some(text) =
                                    block.get("text").and_then(|t| t.as_str())
                                {
                                    events.push(CliEvent::AssistantText {
                                        text: text.to_string(),
                                        is_delta: false,
                                    });
                                }
                            }
                            Some("tool_use") => {
                                let id = block
                                    .get("id")
                                    .and_then(|s| s.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let name = block
                                    .get("name")
                                    .and_then(|s| s.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let input = block
                                    .get("input")
                                    .cloned()
                                    .unwrap_or(Value::Null);
                                events.push(CliEvent::ToolCallStart { id, name, input });
                            }
                            Some("thinking") => {
                                if let Some(text) =
                                    block.get("thinking").and_then(|t| t.as_str())
                                {
                                    events.push(CliEvent::Thinking {
                                        text: text.to_string(),
                                    });
                                }
                            }
                            _ => {}
                        }
                    }
                }
                if let Some(usage) = v.pointer("/message/usage") {
                    let input = usage
                        .get("input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let output = usage
                        .get("output_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    if input > 0 || output > 0 {
                        events.push(CliEvent::TurnComplete {
                            input_tokens: input,
                            output_tokens: output,
                        });
                    }
                }
            }
            Some("user") => {
                if let Some(content) =
                    v.pointer("/message/content").and_then(|c| c.as_array())
                {
                    for block in content {
                        if block.get("type").and_then(|t| t.as_str())
                            == Some("tool_result")
                        {
                            let id = block
                                .get("tool_use_id")
                                .and_then(|s| s.as_str())
                                .unwrap_or("")
                                .to_string();
                            let output = block
                                .get("content")
                                .and_then(|s| s.as_str())
                                .unwrap_or("")
                                .to_string();
                            let is_error = block
                                .get("is_error")
                                .and_then(|b| b.as_bool())
                                .unwrap_or(false);
                            let duration_ms = v
                                .pointer("/tool_use_result/durationMs")
                                .and_then(|d| d.as_u64());
                            events.push(CliEvent::ToolCallResult {
                                id,
                                output,
                                is_error,
                                duration_ms,
                            });
                        }
                    }
                }
            }
            Some("result") => {
                let result_text = v
                    .get("result")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let cost = v.get("total_cost_usd").and_then(|c| c.as_f64());
                let is_error = v
                    .get("is_error")
                    .and_then(|b| b.as_bool())
                    .unwrap_or(false);
                events.push(CliEvent::SessionEnd {
                    result: result_text,
                    cost_usd: cost,
                    is_error,
                });
            }
            Some("stream_event") => {
                if let Some(delta_text) = v.pointer("/event/delta/text") {
                    if let Some(text) = delta_text.as_str() {
                        events.push(CliEvent::AssistantText {
                            text: text.to_string(),
                            is_delta: true,
                        });
                    }
                }
            }
            _ => {}
        }

        events
    }

    fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }
}

/// OpenCode NDJSON parser.
///
/// Parses output from: `opencode run --format json "<prompt>"`
///
/// OpenCode v1.4.0+ emits NDJSON with a 5-event schema distinct from Claude/Cursor.
/// Each line is a JSON object with a `type` discriminant.
///
/// # Source
/// - https://opencode.ai/docs/cli/ — run command documentation
/// - https://deepwiki.com/sst/opencode/6.1-command-line-interface-(cli)
/// - https://github.com/opencode-ai/opencode
///
/// # Event schema (from docs)
///
/// | `type` value   | gate4agent event        | Key fields |
/// |----------------|-------------------------|------------|
/// | `step_start`   | `ToolCallStart`         | `id`, `tool_name`, `input` |
/// | `tool_use`     | `ToolCallStart` (alias) | `id`, `tool_name`, `input` |
/// | `text`         | `AssistantText`         | `text` or `content` |
/// | `step_finish`  | `ToolCallResult`        | `id`, `output`, `is_error` |
/// | `error`        | `AssistantText` (surfaced) | `message` |
///
/// # Session ID
/// Lines that include a `session_id` field (typically early in the stream,
/// before or alongside the first event) are tracked internally.
/// Session IDs use the `ses_XXXX` prefix per OpenCode conventions.
///
/// # Field name assumptions
/// Field names taken from OpenCode docs. If real output differs (e.g., `step_id`
/// vs `id`), a future patch will reconcile. Fields noted as assumed are marked
/// with "assumed from docs" inline.
pub struct OpenCodeNdjsonParser {
    session_id: Option<String>,
}

impl OpenCodeNdjsonParser {
    pub fn new() -> Self {
        Self { session_id: None }
    }
}

impl Default for OpenCodeNdjsonParser {
    fn default() -> Self {
        Self::new()
    }
}

impl NdjsonParser for OpenCodeNdjsonParser {
    fn parse_line(&mut self, line: &str) -> Vec<CliEvent> {
        let line = line.trim();
        if line.is_empty() {
            return vec![];
        }

        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                return vec![CliEvent::Error {
                    message: format!("invalid JSON: {}", truncate_str(line, 100)),
                }]
            }
        };

        // Track session_id whenever it appears in any line.
        // Source: https://opencode.ai/docs/cli/ — session_id emitted early in the stream.
        // Assumed field name: "session_id" (same convention as Claude/Cursor).
        if self.session_id.is_none() {
            if let Some(sid) = v.get("session_id").and_then(|s| s.as_str()) {
                self.session_id = Some(sid.to_string());
            }
        }

        let mut events = Vec::new();

        match v.get("type").and_then(|t| t.as_str()) {
            // step_start: a tool call is beginning.
            // Source: OpenCode docs, run command NDJSON schema.
            // Field names assumed from docs: "id", "tool_name", "input".
            Some("step_start") | Some("tool_use") => {
                let id = v
                    .get("id")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = v
                    .get("tool_name")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let input = v.get("input").cloned().unwrap_or(Value::Null);
                events.push(CliEvent::ToolCallStart { id, name, input });
            }
            // text: assistant text output.
            // Source: OpenCode docs.
            // Field name: "text" or "content" — use whichever is present.
            // "text" is the primary field; "content" is a fallback alias seen in
            // some versions per docs. Assumed from docs.
            Some("text") => {
                let text = v
                    .get("text")
                    .and_then(|s| s.as_str())
                    .or_else(|| v.get("content").and_then(|s| s.as_str()))
                    .unwrap_or("")
                    .to_string();
                if !text.is_empty() {
                    events.push(CliEvent::AssistantText {
                        text,
                        is_delta: false,
                    });
                }
            }
            // step_finish: a tool call completed.
            // Source: OpenCode docs.
            // Field names assumed from docs: "id", "output", "is_error".
            Some("step_finish") => {
                let id = v
                    .get("id")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let output = v
                    .get("output")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let is_error = v
                    .get("is_error")
                    .and_then(|b| b.as_bool())
                    .unwrap_or(false);
                events.push(CliEvent::ToolCallResult {
                    id,
                    output,
                    is_error,
                    duration_ms: None,
                });
            }
            // error: fatal error event.
            // Source: OpenCode docs.
            // Field name: "message". Surfaced as AssistantText with "[error] " prefix
            // so it is visible to consumers without a dedicated error routing path.
            // A proper CliEvent::Error variant could be used here in a future patch.
            Some("error") => {
                let message = v
                    .get("message")
                    .and_then(|s| s.as_str())
                    .unwrap_or("unknown error")
                    .to_string();
                events.push(CliEvent::AssistantText {
                    text: format!("[error] {}", message),
                    is_delta: false,
                });
            }
            // Unknown types are skipped silently.
            _ => {}
        }

        events
    }

    fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }
}

/// Create NDJSON parser for the given tool.
///
/// OpenClaw parser is not yet implemented (Phase 4).
/// It falls back to the Claude parser as a structural stub — will be replaced
/// with a proper parser in Phase 4 after daemon output capture.
pub fn create_ndjson_parser(tool: CliTool) -> Box<dyn NdjsonParser> {
    match tool {
        CliTool::ClaudeCode => Box::new(ClaudeNdjsonParser::new()),
        CliTool::Codex => Box::new(CodexNdjsonParser::new()),
        CliTool::Gemini => Box::new(GeminiNdjsonParser::new()),
        CliTool::Cursor => Box::new(CursorNdjsonParser::new()),
        CliTool::OpenCode => Box::new(OpenCodeNdjsonParser::new()),
        // Phase 4: OpenClaw parser will be implemented after daemon output capture.
        // Claude parser used as a structural stub until then.
        CliTool::OpenClaw => Box::new(ClaudeNdjsonParser::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_parses_system_event() {
        let mut parser = ClaudeNdjsonParser::new();
        let line = r#"{"type":"system","session_id":"abc123","model":"claude-opus-4","tools":["Bash","Read"]}"#;
        let events = parser.parse_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::SessionStart { session_id, model, tools } => {
                assert_eq!(session_id, "abc123");
                assert_eq!(model, "claude-opus-4");
                assert_eq!(tools, &["Bash", "Read"]);
            }
            _ => panic!("expected SessionStart"),
        }
    }

    #[test]
    fn claude_parses_stream_delta() {
        let mut parser = ClaudeNdjsonParser::new();
        let line = r#"{"type":"stream_event","event":{"delta":{"text":"Hello "}}}"#;
        let events = parser.parse_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::AssistantText { text, is_delta } => {
                assert_eq!(text, "Hello ");
                assert!(*is_delta);
            }
            _ => panic!("expected AssistantText"),
        }
    }

    #[test]
    fn malformed_json_returns_error_not_panic() {
        let mut parser = ClaudeNdjsonParser::new();
        let events = parser.parse_line("this is not json {{{");
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], CliEvent::Error { .. }));
    }

    #[test]
    fn empty_line_returns_no_events() {
        let mut parser = ClaudeNdjsonParser::new();
        assert!(parser.parse_line("").is_empty());
        assert!(parser.parse_line("   ").is_empty());
    }

    #[test]
    fn codex_parses_thread_started() {
        let mut parser = CodexNdjsonParser::new();
        let line = r#"{"type":"thread.started","thread_id":"thread_xyz"}"#;
        let events = parser.parse_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::SessionStart { session_id, .. } => {
                assert_eq!(session_id, "thread_xyz");
            }
            _ => panic!("expected SessionStart"),
        }
    }

    /// Regression test: Codex emits "aggregated_output" for command_execution results,
    /// NOT "output". Reading the wrong field returns an empty string — this test guards
    /// against regressing to that behavior.
    #[test]
    fn codex_aggregated_output_regression() {
        let mut parser = CodexNdjsonParser::new();
        let line = r#"{"type":"item.completed","item":{"id":"item_1","type":"command_execution","command":"bash -lc 'ls -la'","aggregated_output":"total 48\ndrwxr-xr-x 12 user user 4096 Apr  9 12:00 .","exit_code":0,"status":"completed"}}"#;
        let events = parser.parse_line(line);
        assert_eq!(events.len(), 1, "expected exactly one ToolCallResult event");
        match &events[0] {
            CliEvent::ToolCallResult { output, is_error, .. } => {
                assert!(
                    !output.is_empty(),
                    "aggregated_output must not be empty — parser may be reading the wrong field"
                );
                assert!(output.contains("total 48"), "expected ls output in aggregated_output");
                assert!(!is_error, "status=completed should not be an error");
            }
            other => panic!("expected ToolCallResult, got {:?}", other),
        }
    }

    /// Test that "assistant_message" is accepted as an alias for "agent_message"
    /// in item.completed. Some Codex versions use one or the other.
    #[test]
    fn codex_assistant_message_alias_completed() {
        let mut parser = CodexNdjsonParser::new();
        let line = r#"{"type":"item.completed","item":{"id":"item_3","type":"assistant_message","text":"Here is what I found: some result"}}"#;
        let events = parser.parse_line(line);
        assert_eq!(events.len(), 1, "expected exactly one AssistantText event");
        match &events[0] {
            CliEvent::AssistantText { text, is_delta } => {
                assert_eq!(text, "Here is what I found: some result");
                assert!(!is_delta, "completed message should not be a delta");
            }
            other => panic!("expected AssistantText, got {:?}", other),
        }
    }

    /// Test that "assistant_message" in item.started is silently consumed (no events),
    /// same as "agent_message" — the content arrives in item.completed.
    #[test]
    fn codex_assistant_message_alias_started() {
        let mut parser = CodexNdjsonParser::new();
        let line = r#"{"type":"item.started","item":{"id":"item_3","type":"assistant_message","status":"in_progress"}}"#;
        let events = parser.parse_line(line);
        assert!(
            events.is_empty(),
            "item.started for assistant_message should produce no events (content comes in item.completed)"
        );
    }

    /// Test that "agent_message" still works (alias must not break the original).
    #[test]
    fn codex_agent_message_original_still_works() {
        let mut parser = CodexNdjsonParser::new();
        let line = r#"{"type":"item.completed","item":{"id":"item_2","type":"agent_message","text":"Original agent_message format still works"}}"#;
        let events = parser.parse_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::AssistantText { text, .. } => {
                assert_eq!(text, "Original agent_message format still works");
            }
            other => panic!("expected AssistantText, got {:?}", other),
        }
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Cursor fixture tests
    //
    // Hand-authored NDJSON based on Cursor docs (https://cursor.com/docs/cli/headless).
    // The schema is documented as Claude-compatible, so we use Claude-shaped events.
    // ─────────────────────────────────────────────────────────────────────────

    /// Feed a 3-line Cursor NDJSON fixture (system init + assistant message + result)
    /// through CursorNdjsonParser and assert SessionStart → Text → SessionEnd event order.
    #[test]
    fn cursor_parses_hello_session() {
        let mut parser = CursorNdjsonParser::new();

        // Line 1: system/init — same shape as Claude per docs.
        let line1 = r#"{"type":"system","session_id":"cursor_ses_abc","model":"cursor-small","tools":[]}"#;
        // Line 2: assistant message with text content.
        let line2 = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hello from Cursor!"}],"usage":{"input_tokens":0,"output_tokens":0}}}"#;
        // Line 3: result — same shape as Claude per docs.
        let line3 = r#"{"type":"result","result":"success","is_error":false,"total_cost_usd":0.001}"#;

        let ev1 = parser.parse_line(line1);
        let ev2 = parser.parse_line(line2);
        let ev3 = parser.parse_line(line3);

        assert_eq!(ev1.len(), 1, "line1 should produce exactly one event");
        match &ev1[0] {
            CliEvent::SessionStart { session_id, model, .. } => {
                assert_eq!(session_id, "cursor_ses_abc");
                assert_eq!(model, "cursor-small");
            }
            other => panic!("expected SessionStart, got {:?}", other),
        }

        assert_eq!(ev2.len(), 1, "line2 should produce exactly one AssistantText event");
        match &ev2[0] {
            CliEvent::AssistantText { text, is_delta } => {
                assert_eq!(text, "Hello from Cursor!");
                assert!(!is_delta);
            }
            other => panic!("expected AssistantText, got {:?}", other),
        }

        assert_eq!(ev3.len(), 1, "line3 should produce exactly one SessionEnd event");
        match &ev3[0] {
            CliEvent::SessionEnd { is_error, cost_usd, .. } => {
                assert!(!is_error);
                assert!(cost_usd.is_some());
            }
            other => panic!("expected SessionEnd, got {:?}", other),
        }

        // session_id should be tracked after parsing line1
        assert_eq!(parser.session_id(), Some("cursor_ses_abc"));
    }

    // ─────────────────────────────────────────────────────────────────────────
    // OpenCode fixture tests
    //
    // Hand-authored NDJSON based on OpenCode docs (https://opencode.ai/docs/cli/).
    // Field names are assumed from docs; reconcile with live capture in future.
    // ─────────────────────────────────────────────────────────────────────────

    /// Feed a minimal OpenCode session (session_id line + text event + step_finish)
    /// and assert correct event sequence.
    #[test]
    fn opencode_parses_minimal_session() {
        let mut parser = OpenCodeNdjsonParser::new();

        // A line that includes session_id (may appear with or without a type field).
        let line1 = r#"{"session_id":"ses_test999","type":"text","text":"Hello from OpenCode"}"#;
        // step_finish for a previously started tool.
        let line2 = r#"{"type":"step_finish","id":"step_1","output":"ls output here","is_error":false}"#;

        let ev1 = parser.parse_line(line1);
        let ev2 = parser.parse_line(line2);

        assert_eq!(ev1.len(), 1, "text line should produce one AssistantText event");
        match &ev1[0] {
            CliEvent::AssistantText { text, is_delta } => {
                assert_eq!(text, "Hello from OpenCode");
                assert!(!is_delta);
            }
            other => panic!("expected AssistantText, got {:?}", other),
        }

        assert_eq!(ev2.len(), 1, "step_finish should produce one ToolCallResult event");
        match &ev2[0] {
            CliEvent::ToolCallResult { id, output, is_error, .. } => {
                assert_eq!(id, "step_1");
                assert_eq!(output, "ls output here");
                assert!(!is_error);
            }
            other => panic!("expected ToolCallResult, got {:?}", other),
        }
    }

    /// "tool_use" type is an alias for "step_start" — both must produce ToolCallStart.
    #[test]
    fn opencode_parses_tool_use_alias() {
        let mut parser = OpenCodeNdjsonParser::new();

        let line = r#"{"type":"tool_use","id":"step_2","tool_name":"bash","input":{"cmd":"ls"}}"#;
        let events = parser.parse_line(line);

        assert_eq!(events.len(), 1, "tool_use should produce one ToolCallStart event");
        match &events[0] {
            CliEvent::ToolCallStart { id, name, .. } => {
                assert_eq!(id, "step_2");
                assert_eq!(name, "bash");
            }
            other => panic!("expected ToolCallStart, got {:?}", other),
        }
    }

    /// After feeding a line containing `session_id`, `parser.session_id()` must return it.
    #[test]
    fn opencode_session_id_tracked() {
        let mut parser = OpenCodeNdjsonParser::new();

        assert!(parser.session_id().is_none(), "no session_id before any input");

        let line = r#"{"session_id":"ses_test123","type":"text","text":"hi"}"#;
        parser.parse_line(line);

        assert_eq!(
            parser.session_id(),
            Some("ses_test123"),
            "session_id must be tracked from the first line that contains it"
        );
    }

    /// step_start type also produces ToolCallStart (same dispatch as tool_use).
    #[test]
    fn opencode_parses_step_start() {
        let mut parser = OpenCodeNdjsonParser::new();

        let line = r#"{"type":"step_start","id":"step_3","tool_name":"read_file","input":{"path":"/tmp/test"}}"#;
        let events = parser.parse_line(line);

        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::ToolCallStart { id, name, .. } => {
                assert_eq!(id, "step_3");
                assert_eq!(name, "read_file");
            }
            other => panic!("expected ToolCallStart, got {:?}", other),
        }
    }

    /// error type is surfaced as AssistantText with "[error] " prefix.
    #[test]
    fn opencode_error_surfaced_as_text() {
        let mut parser = OpenCodeNdjsonParser::new();

        let line = r#"{"type":"error","message":"model not available"}"#;
        let events = parser.parse_line(line);

        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::AssistantText { text, .. } => {
                assert!(text.starts_with("[error] "), "error must be prefixed with [error]");
                assert!(text.contains("model not available"));
            }
            other => panic!("expected AssistantText with error prefix, got {:?}", other),
        }
    }
}
