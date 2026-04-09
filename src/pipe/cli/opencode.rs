//! Pipe-mode OpenCode bindings: NDJSON parser + spawn builder.

use super::traits::{CliEvent, NdjsonParser};
use crate::utils::truncate_str;
use crate::transport::SpawnOptions;

/// OpenCode NDJSON parser.
///
/// Parses output from: `opencode --format json "<prompt>"`
///
/// Event types: "text", "tool_use", "step_start", "step_finish", "reasoning", "error"
///
/// OpenCode has NO init/session_start event — `session_id` is tracked from any
/// line that contains a `sessionID` field.
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
        // OpenCode emits "sessionID" (camelCase) per source; accept both forms.
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
                let input = v.get("input").cloned().unwrap_or(serde_json::Value::Null);
                events.push(CliEvent::ToolCallStart { id, name, input });
            }
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
            Some("reasoning") => {
                // Emitted only when `--thinking` flag is passed to opencode.
                let text = v
                    .get("text")
                    .and_then(|s| s.as_str())
                    .or_else(|| v.get("content").and_then(|s| s.as_str()))
                    .unwrap_or("")
                    .to_string();
                if !text.is_empty() {
                    events.push(CliEvent::Thinking { text });
                }
            }
            Some("error") => {
                let message = v
                    .get("message")
                    .and_then(|s| s.as_str())
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

    #[test]
    fn opencode_parses_minimal_session() {
        let mut parser = OpenCodeNdjsonParser::new();

        let line1 = r#"{"session_id":"ses_test999","type":"text","text":"Hello from OpenCode"}"#;
        let line2 = r#"{"type":"step_finish","id":"step_1","output":"ls output here","is_error":false}"#;

        let ev1 = parser.parse_line(line1);
        let ev2 = parser.parse_line(line2);

        assert_eq!(ev1.len(), 1);
        match &ev1[0] {
            CliEvent::AssistantText { text, is_delta } => {
                assert_eq!(text, "Hello from OpenCode");
                assert!(!is_delta);
            }
            other => panic!("expected AssistantText, got {:?}", other),
        }

        assert_eq!(ev2.len(), 1);
        match &ev2[0] {
            CliEvent::ToolCallResult { id, output, is_error, .. } => {
                assert_eq!(id, "step_1");
                assert_eq!(output, "ls output here");
                assert!(!is_error);
            }
            other => panic!("expected ToolCallResult, got {:?}", other),
        }
    }

    #[test]
    fn opencode_parses_tool_use_alias() {
        let mut parser = OpenCodeNdjsonParser::new();
        let line = r#"{"type":"tool_use","id":"step_2","tool_name":"bash","input":{"cmd":"ls"}}"#;
        let events = parser.parse_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::ToolCallStart { id, name, .. } => {
                assert_eq!(id, "step_2");
                assert_eq!(name, "bash");
            }
            other => panic!("expected ToolCallStart, got {:?}", other),
        }
    }

    #[test]
    fn opencode_session_id_tracked_camel_case() {
        // OpenCode emits "sessionID" (camelCase) per source code.
        let mut parser = OpenCodeNdjsonParser::new();
        assert!(parser.session_id().is_none());
        let line = r#"{"sessionID":"ses_test123","type":"text","text":"hi"}"#;
        parser.parse_line(line);
        assert_eq!(parser.session_id(), Some("ses_test123"));
    }

    #[test]
    fn opencode_session_id_tracked_snake_case_fallback() {
        // Accept legacy snake_case form as well.
        let mut parser = OpenCodeNdjsonParser::new();
        assert!(parser.session_id().is_none());
        let line = r#"{"session_id":"ses_test456","type":"text","text":"hi"}"#;
        parser.parse_line(line);
        assert_eq!(parser.session_id(), Some("ses_test456"));
    }

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

    #[test]
    fn opencode_error_emits_cli_error() {
        let mut parser = OpenCodeNdjsonParser::new();
        let line = r#"{"type":"error","message":"model not available"}"#;
        let events = parser.parse_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::Error { message } => {
                assert_eq!(message, "model not available");
            }
            other => panic!("expected CliEvent::Error, got {:?}", other),
        }
    }

    #[test]
    fn opencode_reasoning_emits_thinking() {
        let mut parser = OpenCodeNdjsonParser::new();
        let line = r#"{"type":"reasoning","text":"Let me think about this carefully..."}"#;
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
        let line = r#"{"type":"reasoning","text":""}"#;
        let events = parser.parse_line(line);
        assert!(events.is_empty(), "empty reasoning text should produce no events");
    }
}
