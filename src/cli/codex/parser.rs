//! NDJSON stream parser for OpenAI Codex CLI.
//!
//! Expects output from: `codex exec --json "prompt"`
//!
//! Event types: "thread.started", "turn.started", "turn.completed", "item.started",
//! "item.completed", "turn.failed", "error"

use serde_json::Value;

use crate::ndjson::traits::{CliEvent, NdjsonParser};
use crate::utils::truncate_str;

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

#[cfg(test)]
mod tests {
    use super::*;

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
