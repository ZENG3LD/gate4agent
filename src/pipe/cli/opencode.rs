//! Pipe-mode OpenCode bindings: NDJSON parser + spawn builder.

use super::traits::{CliEvent, NdjsonParser};
use crate::utils::truncate_str;
use crate::transport::SpawnOptions;

/// OpenCode NDJSON parser.
///
/// Parses output from: `opencode run --format json "<prompt>"`
///
/// OpenCode v1.4.0+ emits NDJSON with a 5-event schema distinct from Claude/Cursor.
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

        // Track session_id whenever it appears in any line.
        if self.session_id.is_none() {
            if let Some(sid) = v.get("session_id").and_then(|s| s.as_str()) {
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
/// Argv produced (fresh session):
/// ```text
/// opencode run --format json [<extra>...] "<prompt>"
/// ```
///
/// Argv produced (resumed session):
/// ```text
/// opencode run --format json --session <ses_XXXX> [<extra>...] "<prompt>"
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
    fn opencode_session_id_tracked() {
        let mut parser = OpenCodeNdjsonParser::new();
        assert!(parser.session_id().is_none());
        let line = r#"{"session_id":"ses_test123","type":"text","text":"hi"}"#;
        parser.parse_line(line);
        assert_eq!(parser.session_id(), Some("ses_test123"));
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
    fn opencode_error_surfaced_as_text() {
        let mut parser = OpenCodeNdjsonParser::new();
        let line = r#"{"type":"error","message":"model not available"}"#;
        let events = parser.parse_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::AssistantText { text, .. } => {
                assert!(text.starts_with("[error] "));
                assert!(text.contains("model not available"));
            }
            other => panic!("expected AssistantText with error prefix, got {:?}", other),
        }
    }
}
