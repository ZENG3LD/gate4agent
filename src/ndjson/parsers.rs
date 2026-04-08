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

/// Create NDJSON parser for the given tool.
///
/// Cursor, OpenCode, and OpenClaw parsers are not yet implemented (Phase 3/4).
/// For now they fall back to the Claude parser as a stub — they will be replaced
/// with proper parsers when their CLI output has been captured and confirmed.
pub fn create_ndjson_parser(tool: CliTool) -> Box<dyn NdjsonParser> {
    match tool {
        CliTool::ClaudeCode => Box::new(ClaudeNdjsonParser::new()),
        CliTool::Codex => Box::new(CodexNdjsonParser::new()),
        CliTool::Gemini => Box::new(GeminiNdjsonParser::new()),
        // Phase 3/4: these parsers will be implemented once real CLI output has
        // been captured and confirmed. Claude parser is used as a structural stub
        // so the factory compiles and dispatch works end-to-end.
        CliTool::Cursor | CliTool::OpenCode | CliTool::OpenClaw => {
            Box::new(ClaudeNdjsonParser::new())
        }
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
}
