//! Pipe-mode Gemini bindings: NDJSON parser + spawn builder.

use super::traits::{CliEvent, NdjsonParser};
use crate::transport::SpawnOptions;

/// Gemini CLI stream-json parser.
///
/// Expects output from: `gemini --output-format stream-json --prompt "prompt"`
///
/// Event types: "init", "message", "tool_use", "tool_result", "error", "result"
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

        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                // Skip non-JSON lines silently (startup banners, auth notices).
                // These are not real errors — just pre-NDJSON output from the CLI.
                // Real Gemini errors arrive as JSON objects with `"type": "error"`.
                return vec![];
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
                let params = v.get("parameters").cloned().unwrap_or(serde_json::Value::Null);
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

/// Pipe-mode spawn builder for Gemini CLI.
///
/// Argv produced (fresh session):
/// ```text
/// gemini --output-format stream-json [--sandbox] -p [<extra>...] "<prompt>"
/// ```
///
/// Argv produced (resumed session):
/// ```text
/// gemini --output-format stream-json --resume <id> [--sandbox] -p [<extra>...] "<prompt>"
/// ```
///
/// Note: `--verbose` is intentionally omitted — it is not required for
/// `--output-format stream-json` and only adds stderr noise.
///
/// Resume: `--resume latest` or `--resume <index>` (from `--list-sessions`).
/// Source: `packages/cli/src/config/config.ts` — `--resume` / `-r` flag.
///
/// `continue_last` is NOT supported by Gemini — it has no `--continue` flag.
/// Use `resume_session_id = Some("latest".to_string())` instead.
pub struct GeminiPipeBuilder;

impl super::traits::CliCommandBuilder for GeminiPipeBuilder {
    fn build_command(&self, opts: &SpawnOptions) -> std::process::Command {
        let mut cmd = std::process::Command::new("gemini");
        cmd.arg("--output-format");
        cmd.arg("stream-json");

        if let Some(ref session_id) = opts.resume_session_id {
            cmd.arg("--resume");
            cmd.arg(session_id);
        }

        if opts.sandbox {
            cmd.arg("--sandbox");
        }

        for arg in &opts.extra_args {
            cmd.arg(arg);
        }

        // -p takes the prompt as its value (not as a separate positional arg).
        // `gemini -p "prompt text"` — confirmed from `gemini --help`.
        cmd.arg("-p");
        cmd.arg(&opts.prompt);
        cmd
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parser() -> GeminiNdjsonParser {
        GeminiNdjsonParser::new()
    }

    #[test]
    fn non_json_lines_are_silently_skipped() {
        let mut p = parser();
        // Startup banner — must return empty vec, NOT an error event.
        let events = p.parse_line("Gemini CLI v1.2.3 — Initializing...");
        assert!(events.is_empty(), "expected no events for banner line, got: {events:?}");
    }

    #[test]
    fn auth_notice_is_silently_skipped() {
        let mut p = parser();
        let events = p.parse_line("Authenticating with Google... done.");
        assert!(events.is_empty(), "expected no events for auth notice, got: {events:?}");
    }

    #[test]
    fn empty_line_is_silently_skipped() {
        let mut p = parser();
        assert!(p.parse_line("").is_empty());
        assert!(p.parse_line("   ").is_empty());
    }

    #[test]
    fn real_json_error_is_preserved() {
        let mut p = parser();
        let line = r#"{"type":"error","message":"quota exceeded"}"#;
        let events = p.parse_line(line);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], CliEvent::Error { message } if message == "quota exceeded"));
    }

    #[test]
    fn valid_message_event_is_parsed() {
        let mut p = parser();
        let line = r#"{"type":"message","role":"assistant","content":"Hello","delta":false}"#;
        let events = p.parse_line(line);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], CliEvent::AssistantText { text, .. } if text == "Hello"));
    }

    #[test]
    fn gemini_init_event() {
        let mut p = parser();
        let line = r#"{"type":"init","timestamp":"2026-01-01T00:00:00Z","session_id":"ses-abc123","model":"gemini-3-flash-preview"}"#;
        let events = p.parse_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::SessionStart { session_id, model, tools } => {
                assert_eq!(session_id, "ses-abc123");
                assert_eq!(model, "gemini-3-flash-preview");
                assert!(tools.is_empty());
            }
            other => panic!("expected SessionStart, got: {other:?}"),
        }
    }

    #[test]
    fn gemini_assistant_message_delta() {
        let mut p = parser();
        let line = r#"{"type":"message","timestamp":"2026-01-01T00:00:00Z","role":"assistant","content":"hello","delta":true}"#;
        let events = p.parse_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::AssistantText { text, is_delta } => {
                assert_eq!(text, "hello");
                assert!(*is_delta, "expected is_delta=true");
            }
            other => panic!("expected AssistantText, got: {other:?}"),
        }
    }

    #[test]
    fn gemini_assistant_message_full() {
        let mut p = parser();
        // delta field absent → is_delta defaults to false
        let line = r#"{"type":"message","timestamp":"2026-01-01T00:00:00Z","role":"assistant","content":"full response"}"#;
        let events = p.parse_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::AssistantText { text, is_delta } => {
                assert_eq!(text, "full response");
                assert!(!*is_delta, "expected is_delta=false when delta field is absent");
            }
            other => panic!("expected AssistantText, got: {other:?}"),
        }
    }

    #[test]
    fn gemini_assistant_message_delta_false() {
        let mut p = parser();
        let line = r#"{"type":"message","timestamp":"2026-01-01T00:00:00Z","role":"assistant","content":"complete","delta":false}"#;
        let events = p.parse_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::AssistantText { text, is_delta } => {
                assert_eq!(text, "complete");
                assert!(!*is_delta, "expected is_delta=false when delta=false");
            }
            other => panic!("expected AssistantText, got: {other:?}"),
        }
    }

    #[test]
    fn gemini_user_message_ignored() {
        let mut p = parser();
        let line = r#"{"type":"message","timestamp":"2026-01-01T00:00:00Z","role":"user","content":"prompt text"}"#;
        let events = p.parse_line(line);
        assert!(events.is_empty(), "user messages must not generate events, got: {events:?}");
    }

    #[test]
    fn gemini_tool_use() {
        let mut p = parser();
        // Parser reads: tool_id, tool_name, parameters
        let line = r#"{"type":"tool_use","timestamp":"2026-01-01T00:00:00Z","tool_id":"call-1","tool_name":"edit_file","parameters":{"path":"foo.rs"}}"#;
        let events = p.parse_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::ToolCallStart { id, name, input } => {
                assert_eq!(id, "call-1");
                assert_eq!(name, "edit_file");
                assert!(input.get("path").is_some(), "input must contain path field");
            }
            other => panic!("expected ToolCallStart, got: {other:?}"),
        }
    }

    #[test]
    fn gemini_tool_result_success() {
        let mut p = parser();
        let line = r#"{"type":"tool_result","timestamp":"2026-01-01T00:00:00Z","tool_id":"call-1","tool_name":"edit_file","output":"done","status":"success"}"#;
        let events = p.parse_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::ToolCallResult { id, output, is_error, .. } => {
                assert_eq!(id, "call-1");
                assert_eq!(output, "done");
                assert!(!*is_error, "expected is_error=false for status=success");
            }
            other => panic!("expected ToolCallResult, got: {other:?}"),
        }
    }

    #[test]
    fn gemini_tool_result_failed() {
        let mut p = parser();
        let line = r#"{"type":"tool_result","timestamp":"2026-01-01T00:00:00Z","tool_id":"call-2","tool_name":"bad_tool","output":"failed","status":"failed"}"#;
        let events = p.parse_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::ToolCallResult { id, output, is_error, .. } => {
                assert_eq!(id, "call-2");
                assert_eq!(output, "failed");
                assert!(*is_error, "expected is_error=true for status=failed");
            }
            other => panic!("expected ToolCallResult, got: {other:?}"),
        }
    }

    #[test]
    fn gemini_error_event() {
        let mut p = parser();
        let line = r#"{"type":"error","timestamp":"2026-01-01T00:00:00Z","message":"something went wrong"}"#;
        let events = p.parse_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::Error { message } => {
                assert_eq!(message, "something went wrong");
            }
            other => panic!("expected Error, got: {other:?}"),
        }
    }

    #[test]
    fn gemini_result_success() {
        let mut p = parser();
        let line = r#"{"type":"result","timestamp":"2026-01-01T00:00:00Z","status":"success","stats":{"total_tokens":100,"input_tokens":80,"output_tokens":20}}"#;
        let events = p.parse_line(line);
        // Expect TurnComplete + SessionEnd
        assert_eq!(events.len(), 2, "result with stats must emit TurnComplete + SessionEnd");
        match &events[0] {
            CliEvent::TurnComplete { input_tokens, output_tokens } => {
                assert_eq!(*input_tokens, 80);
                assert_eq!(*output_tokens, 20);
            }
            other => panic!("expected TurnComplete first, got: {other:?}"),
        }
        match &events[1] {
            CliEvent::SessionEnd { is_error, .. } => {
                assert!(!*is_error, "expected is_error=false for status=success");
            }
            other => panic!("expected SessionEnd second, got: {other:?}"),
        }
    }

    #[test]
    fn gemini_result_error() {
        let mut p = parser();
        let line = r#"{"type":"result","timestamp":"2026-01-01T00:00:00Z","status":"error","error":"API failure"}"#;
        let events = p.parse_line(line);
        // No stats → only SessionEnd
        assert_eq!(events.len(), 1, "result without stats must emit only SessionEnd");
        match &events[0] {
            CliEvent::SessionEnd { is_error, .. } => {
                assert!(*is_error, "expected is_error=true for status=error");
            }
            other => panic!("expected SessionEnd, got: {other:?}"),
        }
    }

    #[test]
    fn gemini_session_id_tracked() {
        let mut p = parser();
        assert!(p.session_id().is_none(), "session_id must be None before init");
        let line = r#"{"type":"init","timestamp":"2026-01-01T00:00:00Z","session_id":"ses-xyz789","model":"gemini-3-flash-preview"}"#;
        p.parse_line(line);
        assert_eq!(p.session_id(), Some("ses-xyz789"));
    }

    #[test]
    fn gemini_malformed_json() {
        let mut p = parser();
        let events = p.parse_line("{not valid json{{");
        assert!(events.is_empty(), "malformed JSON must be silently skipped, got: {events:?}");
    }

    #[test]
    fn gemini_non_json_banner() {
        let mut p = parser();
        let events = p.parse_line("Welcome to Gemini CLI! Type your prompt below.");
        assert!(events.is_empty(), "banner lines must be silently skipped, got: {events:?}");
    }

    #[test]
    fn gemini_empty_content_assistant_message_ignored() {
        let mut p = parser();
        // Empty content string should not produce AssistantText
        let line = r#"{"type":"message","role":"assistant","content":"","delta":true}"#;
        let events = p.parse_line(line);
        assert!(events.is_empty(), "empty content must not produce events, got: {events:?}");
    }
}
