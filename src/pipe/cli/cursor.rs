//! Pipe-mode Cursor Agent bindings: NDJSON parser + spawn builder.
//!
//! NOTE: Cursor CLI is closed-source and proprietary. All stream-json field
//! names and event shapes below are sourced from:
//!   - Official docs: https://cursor.com/docs/cli/reference/output-format
//!   - Community stream analysis: https://tarq.net/posts/cursor-agent-stream-format/
//!   - Windows port reverse-engineering: https://github.com/gitcnd/cursor-agent-cli-windows
//!
//! Fields marked `// UNVERIFIED` come from community analysis only and may
//! change in future Cursor releases without notice.

// NOTE: closed-source, flags from community docs

use super::traits::{CliEvent, NdjsonParser};
use crate::utils::truncate_str;
use crate::transport::SpawnOptions;

/// Cursor Agent stream-json parser.
///
/// Handles the NDJSON event types emitted by `cursor-agent --output-format stream-json`:
///   - `system`    → [`CliEvent::SessionStart`]
///   - `assistant` → [`CliEvent::AssistantText`]
///   - `tool_call` (subtype `started`) → [`CliEvent::ToolCallStart`]
///   - `tool_call` (subtype `completed`) → [`CliEvent::ToolCallResult`]
///   - `user`      → ignored (echoes the prompt, no actionable data)
///   - `result`    → [`CliEvent::SessionEnd`]
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
            // Session initialisation — emitted once at stream start.
            // Schema per official docs + community analysis:
            //   { "type": "system", "subtype": "init", "session_id": "...",
            //     "cwd": "/path", "model": "claude-3-5-sonnet" }
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
                // Cursor does not list available tools in the system event.
                // UNVERIFIED: tool list might be added in future versions.
                self.session_id = Some(sid.clone());
                events.push(CliEvent::SessionStart {
                    session_id: sid,
                    model,
                    tools: vec![],
                });
            }

            // Assistant text response.
            // Schema per official docs:
            //   { "type": "assistant",
            //     "message": { "role": "assistant",
            //                  "content": [{"type": "text", "text": "..."}] },
            //     "session_id": "..." }
            Some("assistant") => {
                if let Some(content) =
                    v.pointer("/message/content").and_then(|c| c.as_array())
                {
                    for block in content {
                        if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                            if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                                // Cursor delivers complete messages, not streaming deltas.
                                events.push(CliEvent::AssistantText {
                                    text: text.to_string(),
                                    is_delta: false,
                                });
                            }
                        }
                    }
                }
            }

            // Tool invocation event — two subtypes: "started" and "completed".
            // Schema per community analysis (UNVERIFIED — closed-source CLI):
            //   started:   { "type": "tool_call", "subtype": "started",
            //                "tool": "shellToolCall", "session_id": "..." }
            //   completed: { "type": "tool_call", "subtype": "completed",
            //                "tool": "shellToolCall", "session_id": "...",
            //                "duration_ms": 1234 }    // UNVERIFIED
            //
            // Known tool type strings (from community stream captures):
            //   shellToolCall, readToolCall, editToolCall, writeToolCall,
            //   deleteToolCall, grepToolCall, lsToolCall, globToolCall,
            //   todoToolCall, updateTodosToolCall
            Some("tool_call") => {
                let subtype = v
                    .get("subtype")
                    .and_then(|s| s.as_str())
                    .unwrap_or("");
                // UNVERIFIED: Cursor does not expose a stable tool call ID in the stream.
                // We synthesise one from the tool name to allow ToolCallResult to
                // reference the same logical call.
                let tool_name = v
                    .get("tool")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();

                match subtype {
                    "started" => {
                        events.push(CliEvent::ToolCallStart {
                            // UNVERIFIED: no stable id field in community docs;
                            // use tool name as synthetic id for pairing.
                            id: tool_name.clone(),
                            name: tool_name,
                            input: serde_json::Value::Null, // UNVERIFIED: parameters not in spec
                        });
                    }
                    "completed" => {
                        let duration_ms = v
                            .get("duration_ms") // UNVERIFIED field name
                            .and_then(|d| d.as_u64());
                        events.push(CliEvent::ToolCallResult {
                            id: tool_name,
                            output: String::new(), // UNVERIFIED: output not in spec
                            is_error: false,       // UNVERIFIED: no error field in spec
                            duration_ms,
                        });
                    }
                    _ => {}
                }
            }

            // User event — echoes the prompt back to the consumer.
            // Schema per official docs:
            //   { "type": "user",
            //     "message": { "role": "user",
            //                  "content": [{"type": "text", "text": "..."}] },
            //     "session_id": "..." }
            // No actionable data — ignored.
            Some("user") => {}

            // Terminal event — stream ends after this line.
            // Schema per official docs + community analysis:
            //   { "type": "result", "subtype": "success" | "error" | "cancelled",
            //     "duration_ms": 12453, "session_id": "..." }
            // UNVERIFIED: "cancelled" subtype inferred from parallel CLI behaviour.
            Some("result") => {
                let subtype = v
                    .get("subtype")
                    .and_then(|s| s.as_str())
                    .unwrap_or("success");
                let is_error = subtype == "error";
                events.push(CliEvent::SessionEnd {
                    result: subtype.to_string(),
                    cost_usd: None, // Cursor does not expose cost in stream events.
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

/// Pipe-mode spawn builder for Cursor Agent.
///
/// NOTE: closed-source, flags from community docs and official reference at
/// https://cursor.com/docs/cli/reference/output-format
///
/// Argv produced (fresh session):
/// ```text
/// cursor-agent -p --output-format stream-json [--model <m>] [<extra>...] "<prompt>"
/// ```
///
/// Argv produced (resumed session):
/// ```text
/// cursor-agent -p --output-format stream-json [--model <m>] --resume <id> [<extra>...] "<prompt>"
/// ```
///
/// No stdin support: Cursor CLI reads the prompt from argv only. Large prompts
/// via shell substitution may cause parsing issues with special characters.
pub struct CursorPipeBuilder;

impl super::traits::CliCommandBuilder for CursorPipeBuilder {
    fn build_command(&self, opts: &SpawnOptions) -> std::process::Command {
        let mut cmd = std::process::Command::new("cursor-agent");
        cmd.arg("-p");
        cmd.arg("--output-format");
        cmd.arg("stream-json");

        if let Some(ref model) = opts.model {
            cmd.arg("--model");
            cmd.arg(model);
        }
        if let Some(ref session_id) = opts.resume_session_id {
            cmd.arg("--resume");
            cmd.arg(session_id);
        }
        for arg in &opts.extra_args {
            cmd.arg(arg);
        }
        // Prompt as final positional arg (docs-canonical for -p mode).
        cmd.arg(&opts.prompt);
        cmd
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_parses_system_init() {
        let mut parser = CursorNdjsonParser::new();

        let line = r#"{"type":"system","subtype":"init","session_id":"cursor_ses_abc","cwd":"/home/user/project","model":"claude-3-5-sonnet"}"#;
        let events = parser.parse_line(line);

        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::SessionStart { session_id, model, tools } => {
                assert_eq!(session_id, "cursor_ses_abc");
                assert_eq!(model, "claude-3-5-sonnet");
                assert!(tools.is_empty());
            }
            other => panic!("expected SessionStart, got {:?}", other),
        }
        assert_eq!(parser.session_id(), Some("cursor_ses_abc"));
    }

    #[test]
    fn cursor_parses_assistant_text() {
        let mut parser = CursorNdjsonParser::new();

        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Hello from Cursor!"}]},"session_id":"cursor_ses_abc"}"#;
        let events = parser.parse_line(line);

        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::AssistantText { text, is_delta } => {
                assert_eq!(text, "Hello from Cursor!");
                assert!(!is_delta);
            }
            other => panic!("expected AssistantText, got {:?}", other),
        }
    }

    #[test]
    fn cursor_parses_tool_call_started() {
        let mut parser = CursorNdjsonParser::new();

        let line = r#"{"type":"tool_call","subtype":"started","tool":"shellToolCall","session_id":"cursor_ses_abc"}"#;
        let events = parser.parse_line(line);

        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::ToolCallStart { id, name, .. } => {
                assert_eq!(id, "shellToolCall");
                assert_eq!(name, "shellToolCall");
            }
            other => panic!("expected ToolCallStart, got {:?}", other),
        }
    }

    #[test]
    fn cursor_parses_tool_call_completed() {
        let mut parser = CursorNdjsonParser::new();

        let line = r#"{"type":"tool_call","subtype":"completed","tool":"shellToolCall","duration_ms":1234,"session_id":"cursor_ses_abc"}"#;
        let events = parser.parse_line(line);

        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::ToolCallResult { id, duration_ms, is_error, .. } => {
                assert_eq!(id, "shellToolCall");
                assert_eq!(*duration_ms, Some(1234));
                assert!(!is_error);
            }
            other => panic!("expected ToolCallResult, got {:?}", other),
        }
    }

    #[test]
    fn cursor_parses_user_event_as_no_events() {
        let mut parser = CursorNdjsonParser::new();

        let line = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"find and fix the memory leak"}]},"session_id":"cursor_ses_abc"}"#;
        let events = parser.parse_line(line);

        // user event is ignored — it just echoes the prompt back
        assert_eq!(events.len(), 0);
    }

    #[test]
    fn cursor_parses_result_success() {
        let mut parser = CursorNdjsonParser::new();

        let line = r#"{"type":"result","subtype":"success","duration_ms":12453,"session_id":"cursor_ses_abc"}"#;
        let events = parser.parse_line(line);

        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::SessionEnd { is_error, cost_usd, result } => {
                assert!(!is_error);
                assert!(cost_usd.is_none());
                assert_eq!(result, "success");
            }
            other => panic!("expected SessionEnd, got {:?}", other),
        }
    }

    #[test]
    fn cursor_parses_result_error() {
        let mut parser = CursorNdjsonParser::new();

        let line = r#"{"type":"result","subtype":"error","duration_ms":500,"session_id":"cursor_ses_abc"}"#;
        let events = parser.parse_line(line);

        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::SessionEnd { is_error, .. } => {
                assert!(is_error);
            }
            other => panic!("expected SessionEnd, got {:?}", other),
        }
    }

    #[test]
    fn cursor_parses_full_session() {
        let mut parser = CursorNdjsonParser::new();

        let lines = [
            r#"{"type":"system","subtype":"init","session_id":"ses_xyz","cwd":"/repo","model":"gpt-4o"}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"analyze this CI failure"}]},"session_id":"ses_xyz"}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"I found the issue in..."}]},"session_id":"ses_xyz"}"#,
            r#"{"type":"tool_call","subtype":"started","tool":"shellToolCall","session_id":"ses_xyz"}"#,
            r#"{"type":"tool_call","subtype":"completed","tool":"shellToolCall","duration_ms":300,"session_id":"ses_xyz"}"#,
            r#"{"type":"result","subtype":"success","duration_ms":5000,"session_id":"ses_xyz"}"#,
        ];

        let all_events: Vec<_> = lines.iter().flat_map(|l| parser.parse_line(l)).collect();

        // system(1) + user(0) + assistant(1) + started(1) + completed(1) + result(1) = 5
        assert_eq!(all_events.len(), 5);
        assert!(matches!(all_events[0], CliEvent::SessionStart { .. }));
        assert!(matches!(all_events[1], CliEvent::AssistantText { .. }));
        assert!(matches!(all_events[2], CliEvent::ToolCallStart { .. }));
        assert!(matches!(all_events[3], CliEvent::ToolCallResult { .. }));
        assert!(matches!(all_events[4], CliEvent::SessionEnd { .. }));

        assert_eq!(parser.session_id(), Some("ses_xyz"));
    }
}
