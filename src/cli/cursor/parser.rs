//! NDJSON stream parser for Cursor Agent CLI.
//!
//! Cursor Agent's `--output-format stream-json` is documented as Claude-compatible:
//! it emits the same 5 event types (system/init, assistant, user, tool_use, result).
//!
//! # Source
//! - https://cursor.com/docs/cli/headless — "stream-json format mirrors Claude Code"
//! - https://cursor.com/blog/cli (January 2026 announcement)
//!
//! This parser is a copy of `ClaudeNdjsonParser` with Cursor-specific naming.
//! If a future live capture reveals divergence in field names or event shapes,
//! a targeted patch should be applied here without touching the Claude parser.
//!
//! # Assumed identical to Claude per docs
//! All field paths (`type`, `session_id`, `model`, `tools`, `message/content`,
//! `tool_use_id`, `result`, `total_cost_usd`, `is_error`) are assumed identical
//! to the Claude stream-json schema. Divergences will be reconciled in a future
//! patch once live capture data is available.

use serde_json::Value;

use crate::ndjson::traits::{CliEvent, NdjsonParser};
use crate::utils::truncate_str;

/// Cursor Agent stream-json parser.
///
/// Cursor Agent's `--output-format stream-json` is documented as Claude-compatible:
/// it emits the same 5 event types (system/init, assistant, user, tool_use, result).
///
/// # Source
/// - https://cursor.com/docs/cli/headless — "stream-json format mirrors Claude Code"
/// - https://cursor.com/blog/cli (January 2026 announcement)
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
