//! Pipe-mode Claude Code bindings: NDJSON parser + spawn builder.

use super::traits::{CliEvent, NdjsonParser};
use crate::utils::truncate_str;
use crate::transport::SpawnOptions;

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

        let v: serde_json::Value = match serde_json::from_str(line) {
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
                                    .unwrap_or(serde_json::Value::Null);
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

/// Pipe-mode spawn builder for Claude Code.
///
/// Argv produced (default, no permission_mode):
///   `claude -p --output-format stream-json --verbose --dangerously-skip-permissions`
///   `[--append-system-prompt "<text>"] [--resume <id> | --continue] [--model <m>]`
///   `[--allowedTools <tools>] [--permission-mode <mode>] [--mcp-config <path>]`
///   `[--max-turns <N>] [<extra>...]`
///
/// When `permission_mode` is set, `--dangerously-skip-permissions` is omitted and
/// `--permission-mode <value>` is added instead.
///
/// The initial prompt is **not** included in argv — it is written to stdin by
/// the caller (`pipe/process.rs`) after spawn.
pub struct ClaudePipeBuilder;

impl super::traits::CliCommandBuilder for ClaudePipeBuilder {
    fn build_command(&self, opts: &SpawnOptions) -> std::process::Command {
        let mut cmd = std::process::Command::new("claude");
        cmd.arg("-p");
        cmd.arg("--output-format");
        cmd.arg("stream-json");
        cmd.arg("--verbose");

        // --dangerously-skip-permissions is the default, but omitted when the
        // caller explicitly sets permission_mode (they conflict).
        if opts.permission_mode.is_none() {
            cmd.arg("--dangerously-skip-permissions");
        }

        if let Some(ref system_prompt) = opts.append_system_prompt {
            cmd.arg("--append-system-prompt");
            cmd.arg(system_prompt);
        }

        if let Some(ref session_id) = opts.resume_session_id {
            cmd.arg("--resume");
            cmd.arg(session_id);
        } else if opts.continue_last {
            cmd.arg("--continue");
        }

        if let Some(ref model) = opts.model {
            cmd.arg("--model");
            cmd.arg(model);
        }

        if !opts.allowed_tools.is_empty() {
            cmd.arg("--allowedTools");
            cmd.arg(opts.allowed_tools.join(","));
        }

        if let Some(ref mode) = opts.permission_mode {
            cmd.arg("--permission-mode");
            cmd.arg(mode);
        }

        if let Some(ref mcp_config) = opts.mcp_config {
            cmd.arg("--mcp-config");
            cmd.arg(mcp_config);
        }

        if let Some(max_turns) = opts.max_turns {
            cmd.arg("--max-turns");
            cmd.arg(max_turns.to_string());
        }

        for arg in &opts.extra_args {
            cmd.arg(arg);
        }
        // No prompt in argv — delivered via stdin after spawn.
        cmd
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
    fn claude_assistant_text_block() {
        let mut parser = ClaudeNdjsonParser::new();
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hello"}]}}"#;
        let events = parser.parse_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::AssistantText { text, is_delta } => {
                assert_eq!(text, "hello");
                assert!(!*is_delta, "assistant message blocks are not deltas");
            }
            other => panic!("expected AssistantText, got: {other:?}"),
        }
    }

    #[test]
    fn claude_assistant_tool_use() {
        let mut parser = ClaudeNdjsonParser::new();
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"tool_1","name":"Read","input":{"path":"foo.rs"}}]}}"#;
        let events = parser.parse_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::ToolCallStart { id, name, input } => {
                assert_eq!(id, "tool_1");
                assert_eq!(name, "Read");
                assert_eq!(input.get("path").and_then(|v| v.as_str()), Some("foo.rs"));
            }
            other => panic!("expected ToolCallStart, got: {other:?}"),
        }
    }

    #[test]
    fn claude_assistant_thinking() {
        let mut parser = ClaudeNdjsonParser::new();
        let line = r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"let me think..."}]}}"#;
        let events = parser.parse_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::Thinking { text } => {
                assert_eq!(text, "let me think...");
            }
            other => panic!("expected Thinking, got: {other:?}"),
        }
    }

    #[test]
    fn claude_assistant_usage() {
        let mut parser = ClaudeNdjsonParser::new();
        let line = r#"{"type":"assistant","message":{"usage":{"input_tokens":100,"output_tokens":50}}}"#;
        let events = parser.parse_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::TurnComplete { input_tokens, output_tokens } => {
                assert_eq!(*input_tokens, 100);
                assert_eq!(*output_tokens, 50);
            }
            other => panic!("expected TurnComplete, got: {other:?}"),
        }
    }

    #[test]
    fn claude_user_tool_result() {
        let mut parser = ClaudeNdjsonParser::new();
        let line = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tool_1","content":"file contents here"}]}}"#;
        let events = parser.parse_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::ToolCallResult { id, output, is_error, duration_ms } => {
                assert_eq!(id, "tool_1");
                assert_eq!(output, "file contents here");
                assert!(!*is_error, "tool_result without is_error field defaults to false");
                assert!(duration_ms.is_none());
            }
            other => panic!("expected ToolCallResult, got: {other:?}"),
        }
    }

    #[test]
    fn claude_result_event() {
        let mut parser = ClaudeNdjsonParser::new();
        let line = r#"{"type":"result","result":"session complete","total_cost_usd":0.05,"duration_ms":5000,"session_id":"abc-123"}"#;
        let events = parser.parse_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::SessionEnd { result, cost_usd, is_error } => {
                assert_eq!(result, "session complete");
                assert_eq!(*cost_usd, Some(0.05));
                assert!(!*is_error, "result without is_error defaults to false");
            }
            other => panic!("expected SessionEnd, got: {other:?}"),
        }
    }

    #[test]
    fn claude_session_id_persists() {
        let mut parser = ClaudeNdjsonParser::new();
        assert!(parser.session_id().is_none(), "session_id must be None before system event");
        let line = r#"{"type":"system","session_id":"ses-persist-42","model":"claude-sonnet-4","tools":[]}"#;
        parser.parse_line(line);
        assert_eq!(parser.session_id(), Some("ses-persist-42"));
    }

    #[test]
    fn claude_assistant_mixed_content_blocks() {
        // Multiple content blocks in one message → multiple events
        let mut parser = ClaudeNdjsonParser::new();
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Here is the result:"},{"type":"tool_use","id":"t2","name":"Bash","input":{"command":"ls"}}]}}"#;
        let events = parser.parse_line(line);
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], CliEvent::AssistantText { text, .. } if text == "Here is the result:"));
        assert!(matches!(&events[1], CliEvent::ToolCallStart { name, .. } if name == "Bash"));
    }

    #[test]
    fn claude_assistant_usage_with_content_both_emitted() {
        // Message with both content blocks AND usage → content events + TurnComplete
        let mut parser = ClaudeNdjsonParser::new();
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"done"}],"usage":{"input_tokens":10,"output_tokens":5}}}"#;
        let events = parser.parse_line(line);
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], CliEvent::AssistantText { text, .. } if text == "done"));
        assert!(matches!(&events[1], CliEvent::TurnComplete { input_tokens: 10, output_tokens: 5 }));
    }
}
