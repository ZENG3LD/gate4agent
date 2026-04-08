//! NDJSON stream parser for OpenCode CLI.
//!
//! Parses output from: `opencode run --format json "<prompt>"`
//!
//! OpenCode v1.4.0+ emits NDJSON with a 5-event schema distinct from Claude/Cursor.
//! Each line is a JSON object with a `type` discriminant.
//!
//! # Source
//! - https://opencode.ai/docs/cli/ — run command documentation
//! - https://deepwiki.com/sst/opencode/6.1-command-line-interface-(cli)
//! - https://github.com/opencode-ai/opencode
//!
//! # Event schema (from docs)
//!
//! | `type` value   | gate4agent event        | Key fields |
//! |----------------|-------------------------|------------|
//! | `step_start`   | `ToolCallStart`         | `id`, `tool_name`, `input` |
//! | `tool_use`     | `ToolCallStart` (alias) | `id`, `tool_name`, `input` |
//! | `text`         | `AssistantText`         | `text` or `content` |
//! | `step_finish`  | `ToolCallResult`        | `id`, `output`, `is_error` |
//! | `error`        | `AssistantText` (surfaced) | `message` |
//!
//! # Session ID
//! Lines that include a `session_id` field (typically early in the stream,
//! before or alongside the first event) are tracked internally.
//! Session IDs use the `ses_XXXX` prefix per OpenCode conventions.
//!
//! # Field name assumptions
//! Field names taken from OpenCode docs. If real output differs (e.g., `step_id`
//! vs `id`), a future patch will reconcile. Fields noted as assumed are marked
//! with "assumed from docs" inline.

use serde_json::Value;

use crate::ndjson::traits::{CliEvent, NdjsonParser};
use crate::utils::truncate_str;

/// OpenCode NDJSON parser.
///
/// Parses output from: `opencode run --format json "<prompt>"`
///
/// OpenCode v1.4.0+ emits NDJSON with a 5-event schema distinct from Claude/Cursor.
/// Each line is a JSON object with a `type` discriminant.
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

#[cfg(test)]
mod tests {
    use super::*;

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
