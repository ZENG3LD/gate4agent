//! Pipe-mode OpenCode bindings: NDJSON parser + spawn builder.
//!
//! ## Real OpenCode NDJSON format (from `opencode run --format json`)
//!
//! Each line is a JSON object with at minimum `type`, `timestamp`, and `sessionID`.
//! The payload lives inside a `part` object — NOT at the top level.
//!
//! ### Event types
//!
//! **`text`** — assistant text content:
//! ```json
//! {"type":"text","sessionID":"ses_...","part":{"type":"text","text":"Hello!",...}}
//! ```
//! Text is at `part.text`.
//!
//! **`tool_use`** — tool call with result bundled in the same event:
//! ```json
//! {"type":"tool_use","sessionID":"ses_...","part":{"type":"tool","callID":"chatcmpl-tool-...","tool":"read","state":{"status":"completed","input":{...},"output":"<file>...</file>",...}}}
//! ```
//! Tool name: `part.tool`. Call ID: `part.callID`. Input: `part.state.input`. Output: `part.state.output`.
//! When `part.state.status == "completed"` the result is included; emit both `ToolCallStart` + `ToolCallResult`.
//!
//! **`step_start`** — turn boundary marker, carries no tool information:
//! ```json
//! {"type":"step_start","sessionID":"ses_...","part":{"type":"step-start","snapshot":"..."}}
//! ```
//! Ignored — produces no events.
//!
//! **`step_finish`** — turn end with token usage:
//! ```json
//! {"type":"step_finish","sessionID":"ses_...","part":{"type":"step-finish","reason":"stop","cost":0,"tokens":{"input":14290,"output":6,"reasoning":4,...}}}
//! ```
//! Emits `TurnComplete` when token counts are present.
//!
//! **`reasoning`** — thinking content (only when model supports it):
//! ```json
//! {"type":"reasoning","sessionID":"ses_...","part":{"type":"reasoning","text":"..."}}
//! ```
//! Text is at `part.text`.
//!
//! **`error`** — API or runtime error:
//! ```json
//! {"type":"error","sessionID":"ses_...","error":{"name":"APIError","data":{"message":"Incorrect API key...","statusCode":401}}}
//! ```
//! Message is at `error.data.message`, fallback to `error.message`, then top-level `message`.
//!
//! **Session ID**: OpenCode has NO init/session_start event. `sessionID` (camelCase) is emitted
//! on every line and tracked from the first line that contains it.

use super::traits::{CliEvent, NdjsonParser};
use crate::utils::truncate_str;
use crate::transport::SpawnOptions;

/// OpenCode NDJSON parser.
///
/// Parses output from: `opencode run --format json "<prompt>"`
///
/// Payload lives inside the `part` object on each line. See module-level docs
/// for the full format description.
///
/// `session_id` is tracked from any line that contains a `sessionID` field.
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

        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                return vec![CliEvent::Error {
                    message: format!("invalid JSON: {}", truncate_str(line, 100)),
                }]
            }
        };

        // Track session ID whenever it appears in any line.
        // OpenCode emits "sessionID" (camelCase); accept legacy "session_id" as fallback.
        if self.session_id.is_none() {
            let sid = v
                .get("sessionID")
                .or_else(|| v.get("session_id"))
                .and_then(|s| s.as_str());
            if let Some(sid) = sid {
                self.session_id = Some(sid.to_string());
            }
        }

        let mut events = Vec::new();

        match v.get("type").and_then(|t| t.as_str()) {
            Some("tool_use") => {
                // Real format: part.callID, part.tool, part.state.input, part.state.output
                let call_id = v
                    .pointer("/part/callID")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = v
                    .pointer("/part/tool")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let input = v
                    .pointer("/part/state/input")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                events.push(CliEvent::ToolCallStart {
                    id: call_id.clone(),
                    name,
                    input,
                });

                // OpenCode includes the tool result in the same event when status == "completed".
                let status = v
                    .pointer("/part/state/status")
                    .and_then(|s| s.as_str())
                    .unwrap_or("pending");
                if status == "completed" {
                    let output = v
                        .pointer("/part/state/output")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string();
                    events.push(CliEvent::ToolCallResult {
                        id: call_id,
                        output,
                        is_error: false,
                        duration_ms: None,
                    });
                }
            }
            Some("step_start") => {
                // step_start is a turn boundary marker, not a tool call.
                // It carries no tool name or input — just a snapshot hash.
                // Intentionally ignored.
            }
            Some("text") => {
                // Real format: part.text, fallback to top-level text/content.
                let text = v
                    .pointer("/part/text")
                    .and_then(|s| s.as_str())
                    .or_else(|| v.get("text").and_then(|s| s.as_str()))
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
            Some("step_finish") => {
                // Extract token counts from part.tokens.
                let input_tokens = v
                    .pointer("/part/tokens/input")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let output_tokens = v
                    .pointer("/part/tokens/output")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                if input_tokens > 0 || output_tokens > 0 {
                    events.push(CliEvent::TurnComplete {
                        input_tokens,
                        output_tokens,
                    });
                }
            }
            Some("reasoning") => {
                // Emitted only when the model supports reasoning/thinking.
                // Real format: part.text, fallback to top-level text/content.
                let text = v
                    .pointer("/part/text")
                    .and_then(|s| s.as_str())
                    .or_else(|| v.get("text").and_then(|s| s.as_str()))
                    .or_else(|| v.get("content").and_then(|s| s.as_str()))
                    .unwrap_or("")
                    .to_string();
                if !text.is_empty() {
                    events.push(CliEvent::Thinking { text });
                }
            }
            Some("error") => {
                // Real format: error.data.message, fallback to error.message, then top-level message.
                let message = v
                    .pointer("/error/data/message")
                    .and_then(|s| s.as_str())
                    .or_else(|| v.pointer("/error/message").and_then(|s| s.as_str()))
                    .or_else(|| v.get("message").and_then(|s| s.as_str()))
                    .unwrap_or("unknown error")
                    .to_string();
                events.push(CliEvent::Error { message });
            }
            _ => {}
        }

        events
    }

    fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }
}

/// Pipe-mode spawn builder for OpenCode.
///
/// The `run` subcommand is required for headless pipe mode.
/// Without it, `opencode` launches the TUI.
///
/// Argv produced (fresh session):
/// ```text
/// opencode run --format json [<extra>...] "<prompt>"
/// ```
///
/// Argv produced (resumed session — specific ID):
/// ```text
/// opencode run --format json --session <ses_XXXX> [<extra>...] "<prompt>"
/// ```
///
/// Argv produced (resumed session — last session):
/// ```text
/// opencode run --format json --continue [<extra>...] "<prompt>"
/// ```
pub struct OpenCodePipeBuilder;

impl super::traits::CliCommandBuilder for OpenCodePipeBuilder {
    fn build_command(&self, opts: &SpawnOptions) -> std::process::Command {
        let mut cmd = std::process::Command::new("opencode");
        cmd.arg("run");
        cmd.arg("--format");
        cmd.arg("json");

        if let Some(ref session_id) = opts.resume_session_id {
            cmd.arg("--session");
            cmd.arg(session_id);
        } else if opts.continue_last {
            cmd.arg("--continue");
        }

        if let Some(ref model) = opts.model {
            cmd.arg("-m");
            cmd.arg(model);
        }

        for arg in &opts.extra_args {
            cmd.arg(arg);
        }
        // Prompt as final positional arg.
        cmd.arg(&opts.prompt);
        cmd
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Real format tests (matching actual OpenCode output) ──────────────────

    #[test]
    fn opencode_parses_real_text_event() {
        let mut parser = OpenCodeNdjsonParser::new();
        let line = r#"{"type":"text","timestamp":1775737274160,"sessionID":"ses_28dcfca7effeG4iMaRZrD8C8x6","part":{"id":"prt_d72307f22001","sessionID":"ses_28dcfca7effeG4iMaRZrD8C8x6","messageID":"msg_d7230374c001","type":"text","text":"Hello! 👋","time":{"start":1775737274149,"end":1775737274149}}}"#;
        let events = parser.parse_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::AssistantText { text, is_delta } => {
                assert_eq!(text, "Hello! 👋");
                assert!(!is_delta);
            }
            other => panic!("expected AssistantText, got {:?}", other),
        }
        assert_eq!(parser.session_id(), Some("ses_28dcfca7effeG4iMaRZrD8C8x6"));
    }

    #[test]
    fn opencode_parses_real_tool_use() {
        let mut parser = OpenCodeNdjsonParser::new();
        let line = r#"{"type":"tool_use","timestamp":1775737305647,"sessionID":"ses_abc","part":{"id":"prt_xyz","type":"tool","callID":"chatcmpl-tool-bdc397ac90703079","tool":"read","state":{"status":"completed","input":{"filePath":"C:\\README.md"},"output":"<file>contents here</file>","title":"README.md","time":{"start":1775737305648,"end":1775737305700}}}}"#;
        let events = parser.parse_line(line);
        assert_eq!(events.len(), 2, "completed tool_use must emit ToolCallStart + ToolCallResult");

        match &events[0] {
            CliEvent::ToolCallStart { id, name, input } => {
                assert_eq!(id, "chatcmpl-tool-bdc397ac90703079");
                assert_eq!(name, "read");
                assert_eq!(
                    input.pointer("/filePath").and_then(|v| v.as_str()),
                    Some("C:\\README.md")
                );
            }
            other => panic!("expected ToolCallStart, got {:?}", other),
        }

        match &events[1] {
            CliEvent::ToolCallResult { id, output, is_error, .. } => {
                assert_eq!(id, "chatcmpl-tool-bdc397ac90703079");
                assert_eq!(output, "<file>contents here</file>");
                assert!(!is_error);
            }
            other => panic!("expected ToolCallResult, got {:?}", other),
        }
    }

    #[test]
    fn opencode_parses_real_tool_use_pending_no_result() {
        let mut parser = OpenCodeNdjsonParser::new();
        let line = r#"{"type":"tool_use","sessionID":"ses_abc","part":{"type":"tool","callID":"call-123","tool":"bash","state":{"status":"pending","input":{"command":"ls"}}}}"#;
        let events = parser.parse_line(line);
        assert_eq!(events.len(), 1, "pending tool_use emits only ToolCallStart");
        match &events[0] {
            CliEvent::ToolCallStart { id, name, .. } => {
                assert_eq!(id, "call-123");
                assert_eq!(name, "bash");
            }
            other => panic!("expected ToolCallStart, got {:?}", other),
        }
    }

    #[test]
    fn opencode_parses_real_step_finish_with_tokens() {
        let mut parser = OpenCodeNdjsonParser::new();
        let line = r#"{"type":"step_finish","timestamp":1775737275618,"sessionID":"ses_abc","part":{"type":"step-finish","reason":"stop","snapshot":"abc","cost":0,"tokens":{"input":14290,"output":6,"reasoning":4,"cache":{"read":0,"write":0}}}}"#;
        let events = parser.parse_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::TurnComplete { input_tokens, output_tokens } => {
                assert_eq!(*input_tokens, 14290);
                assert_eq!(*output_tokens, 6);
            }
            other => panic!("expected TurnComplete, got {:?}", other),
        }
    }

    #[test]
    fn opencode_step_finish_zero_tokens_emits_nothing() {
        let mut parser = OpenCodeNdjsonParser::new();
        // When both token counts are 0 (or absent), no event is emitted.
        let line = r#"{"type":"step_finish","sessionID":"ses_abc","part":{"type":"step-finish","reason":"tool-calls","cost":0,"tokens":{"input":0,"output":0}}}"#;
        let events = parser.parse_line(line);
        assert!(events.is_empty(), "zero-token step_finish should produce no events");
    }

    #[test]
    fn opencode_parses_real_error() {
        let mut parser = OpenCodeNdjsonParser::new();
        let line = r#"{"type":"error","timestamp":1775737195012,"sessionID":"ses_28dd0bb31ffe","error":{"name":"APIError","data":{"message":"Incorrect API key provided: sk-or-v1...d6a3.","statusCode":401,"isRetryable":false}}}"#;
        let events = parser.parse_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::Error { message } => {
                assert_eq!(message, "Incorrect API key provided: sk-or-v1...d6a3.");
            }
            other => panic!("expected CliEvent::Error, got {:?}", other),
        }
    }

    #[test]
    fn opencode_error_fallback_to_error_message() {
        let mut parser = OpenCodeNdjsonParser::new();
        let line = r#"{"type":"error","error":{"message":"network timeout"}}"#;
        let events = parser.parse_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::Error { message } => assert_eq!(message, "network timeout"),
            other => panic!("expected CliEvent::Error, got {:?}", other),
        }
    }

    #[test]
    fn opencode_error_fallback_to_toplevel_message() {
        let mut parser = OpenCodeNdjsonParser::new();
        let line = r#"{"type":"error","message":"model not available"}"#;
        let events = parser.parse_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::Error { message } => assert_eq!(message, "model not available"),
            other => panic!("expected CliEvent::Error, got {:?}", other),
        }
    }

    #[test]
    fn opencode_step_start_is_ignored() {
        let mut parser = OpenCodeNdjsonParser::new();
        let line = r#"{"type":"step_start","timestamp":1775737274135,"sessionID":"ses_28dcfca7effeG4iMaRZrD8C8x6","part":{"id":"prt_d72307f0a001","sessionID":"ses_28dcfca7effeG4iMaRZrD8C8x6","messageID":"msg_d7230374c001","type":"step-start","snapshot":"11f897c48dde50396cfdadda13159d56b138e9af"}}"#;
        let events = parser.parse_line(line);
        assert!(events.is_empty(), "step_start should produce no events, got {:?}", events);
    }

    #[test]
    fn opencode_session_id_from_any_line() {
        // sessionID is tracked from whatever the first line is — step_start in practice.
        let mut parser = OpenCodeNdjsonParser::new();
        assert!(parser.session_id().is_none());

        let step_start = r#"{"type":"step_start","timestamp":123,"sessionID":"ses_realworld123","part":{"type":"step-start","snapshot":"abc"}}"#;
        let events = parser.parse_line(step_start);
        assert!(events.is_empty());
        assert_eq!(parser.session_id(), Some("ses_realworld123"));
    }

    #[test]
    fn opencode_session_id_tracked_camel_case() {
        let mut parser = OpenCodeNdjsonParser::new();
        assert!(parser.session_id().is_none());
        let line = r#"{"sessionID":"ses_test123","type":"text","part":{"type":"text","text":"hi"}}"#;
        parser.parse_line(line);
        assert_eq!(parser.session_id(), Some("ses_test123"));
    }

    #[test]
    fn opencode_session_id_tracked_snake_case_fallback() {
        // Accept legacy snake_case form as well.
        let mut parser = OpenCodeNdjsonParser::new();
        assert!(parser.session_id().is_none());
        let line = r#"{"session_id":"ses_test456","type":"text","part":{"type":"text","text":"hi"}}"#;
        parser.parse_line(line);
        assert_eq!(parser.session_id(), Some("ses_test456"));
    }

    #[test]
    fn opencode_reasoning_emits_thinking_from_part() {
        let mut parser = OpenCodeNdjsonParser::new();
        let line = r#"{"type":"reasoning","sessionID":"ses_abc","part":{"type":"reasoning","text":"Let me think about this carefully..."}}"#;
        let events = parser.parse_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::Thinking { text } => {
                assert_eq!(text, "Let me think about this carefully...");
            }
            other => panic!("expected CliEvent::Thinking, got {:?}", other),
        }
    }

    #[test]
    fn opencode_reasoning_fallback_to_toplevel_text() {
        // Legacy / fallback path: top-level text field.
        let mut parser = OpenCodeNdjsonParser::new();
        let line = r#"{"type":"reasoning","text":"top-level reasoning text"}"#;
        let events = parser.parse_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::Thinking { text } => {
                assert_eq!(text, "top-level reasoning text");
            }
            other => panic!("expected CliEvent::Thinking, got {:?}", other),
        }
    }

    #[test]
    fn opencode_reasoning_via_content_field() {
        let mut parser = OpenCodeNdjsonParser::new();
        let line = r#"{"type":"reasoning","content":"Alternative content field for reasoning"}"#;
        let events = parser.parse_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::Thinking { text } => {
                assert_eq!(text, "Alternative content field for reasoning");
            }
            other => panic!("expected CliEvent::Thinking, got {:?}", other),
        }
    }

    #[test]
    fn opencode_reasoning_empty_text_ignored() {
        let mut parser = OpenCodeNdjsonParser::new();
        let line = r#"{"type":"reasoning","part":{"type":"reasoning","text":""}}"#;
        let events = parser.parse_line(line);
        assert!(events.is_empty(), "empty reasoning text should produce no events");
    }

    #[test]
    fn opencode_text_empty_part_text_ignored() {
        let mut parser = OpenCodeNdjsonParser::new();
        let line = r#"{"type":"text","sessionID":"ses_abc","part":{"type":"text","text":""}}"#;
        let events = parser.parse_line(line);
        assert!(events.is_empty(), "empty text event should produce no events");
    }

    #[test]
    fn opencode_invalid_json_emits_error() {
        let mut parser = OpenCodeNdjsonParser::new();
        let events = parser.parse_line("not json at all {{{");
        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::Error { message } => assert!(message.starts_with("invalid JSON")),
            other => panic!("expected CliEvent::Error, got {:?}", other),
        }
    }

    #[test]
    fn opencode_empty_line_produces_no_events() {
        let mut parser = OpenCodeNdjsonParser::new();
        assert!(parser.parse_line("").is_empty());
        assert!(parser.parse_line("   ").is_empty());
    }
}
