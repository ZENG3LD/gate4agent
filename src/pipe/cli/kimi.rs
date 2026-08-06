//! Kimi Code pipe bindings for the current `stream-json` wire format.

use super::traits::{CliEvent, NdjsonParser};
use crate::transport::SpawnOptions;
use crate::utils::truncate_str;
use serde_json::Value;

/// Parser for `kimi -p <prompt> --output-format stream-json`.
///
/// Kimi emits OpenAI-shaped assistant/tool messages and announces the durable
/// provider session ID at the end of a successful run through a structured
/// `session.resume_hint` meta record.
pub struct KimiNdjsonParser {
    session_id: Option<String>,
}

impl KimiNdjsonParser {
    pub fn new() -> Self {
        Self { session_id: None }
    }

    fn parse_assistant(&self, value: &Value) -> Vec<CliEvent> {
        let mut events = Vec::new();
        match value.get("content") {
            Some(Value::String(text)) if !text.is_empty() => {
                events.push(CliEvent::AssistantText {
                    text: text.clone(),
                    is_delta: false,
                });
            }
            Some(Value::Array(blocks)) => {
                for block in blocks {
                    let block_type = block.get("type").and_then(Value::as_str);
                    let text = block.get("text").and_then(Value::as_str);
                    match (block_type, text) {
                        (Some("thinking" | "think"), Some(text)) => {
                            events.push(CliEvent::Thinking {
                                text: text.to_owned(),
                            });
                        }
                        (Some("text"), Some(text)) if !text.is_empty() => {
                            events.push(CliEvent::AssistantText {
                                text: text.to_owned(),
                                is_delta: false,
                            });
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }

        if let Some(tool_calls) = value.get("tool_calls").and_then(Value::as_array) {
            for tool_call in tool_calls {
                let id = tool_call
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let name = tool_call
                    .pointer("/function/name")
                    .and_then(Value::as_str)
                    .or_else(|| tool_call.get("name").and_then(Value::as_str))
                    .unwrap_or_default()
                    .to_owned();
                let input = tool_call
                    .pointer("/function/arguments")
                    .or_else(|| tool_call.get("arguments"))
                    .map(parse_tool_arguments)
                    .unwrap_or(Value::Null);
                events.push(CliEvent::ToolCallStart { id, name, input });
            }
        }
        events
    }

    fn parse_tool(&self, value: &Value) -> Vec<CliEvent> {
        let id = value
            .get("tool_call_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let output = match value.get("content") {
            Some(Value::String(content)) => content.clone(),
            Some(content) => content.to_string(),
            None => String::new(),
        };
        let is_error = value
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let duration_ms = value.get("duration_ms").and_then(Value::as_u64);
        vec![CliEvent::ToolCallResult {
            id,
            output,
            is_error,
            duration_ms,
        }]
    }

    fn parse_meta(&mut self, value: &Value) -> Vec<CliEvent> {
        let event_type = value.get("type").and_then(Value::as_str).unwrap_or_default();
        if event_type == "session.resume_hint" {
            let Some(session_id) = value.get("session_id").and_then(Value::as_str) else {
                return vec![CliEvent::Error {
                    message: "Kimi session.resume_hint omitted session_id".to_owned(),
                }];
            };
            self.session_id = Some(session_id.to_owned());
            let model = value
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned();
            let tools = value
                .get("tools")
                .and_then(Value::as_array)
                .map(|tools| {
                    tools
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            return vec![CliEvent::SessionStart {
                session_id: session_id.to_owned(),
                model,
                tools,
            }];
        }

        if event_type.contains("retry") {
            let reason = value
                .get("reason")
                .or_else(|| value.get("message"))
                .or_else(|| value.get("error"))
                .map(value_as_text)
                .unwrap_or_else(|| "provider requested retry".to_owned());
            let attempt = value.get("attempt").and_then(Value::as_u64);
            let max_attempts = value
                .get("max_attempts")
                .or_else(|| value.get("max_retries"))
                .and_then(Value::as_u64);
            let delay_ms = value
                .get("delay_ms")
                .or_else(|| value.get("retry_after_ms"))
                .and_then(Value::as_u64);
            return vec![CliEvent::Error {
                message: format!(
                    "Kimi retry: {reason}; attempt={}; max_attempts={}; delay_ms={}",
                    optional_number(attempt),
                    optional_number(max_attempts),
                    optional_number(delay_ms),
                ),
            }];
        }

        Vec::new()
    }
}

impl Default for KimiNdjsonParser {
    fn default() -> Self {
        Self::new()
    }
}

impl NdjsonParser for KimiNdjsonParser {
    fn parse_line(&mut self, line: &str) -> Vec<CliEvent> {
        let line = line.trim();
        if line.is_empty() {
            return Vec::new();
        }
        let value: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(_) => {
                return vec![CliEvent::Error {
                    message: format!("invalid JSON: {}", truncate_str(line, 100)),
                }];
            }
        };
        match value.get("role").and_then(Value::as_str) {
            Some("assistant") => self.parse_assistant(&value),
            Some("tool") => self.parse_tool(&value),
            Some("meta") => self.parse_meta(&value),
            _ => Vec::new(),
        }
    }

    fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }
}

fn parse_tool_arguments(value: &Value) -> Value {
    match value {
        Value::String(arguments) => {
            serde_json::from_str(arguments).unwrap_or_else(|_| Value::String(arguments.clone()))
        }
        value => value.clone(),
    }
}

fn value_as_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        value => value.to_string(),
    }
}

fn optional_number(value: Option<u64>) -> String {
    value.map_or_else(|| "unknown".to_owned(), |value| value.to_string())
}

/// Kimi Code structured one-turn builder.
///
/// `-p` already selects Kimi's non-interactive auto permission mode. Kimi
/// rejects `--yolo`, `--auto`, and `--plan` when combined with `-p`, so this
/// builder intentionally never injects those interactive permission flags.
pub struct KimiPipeBuilder;

impl super::traits::CliCommandBuilder for KimiPipeBuilder {
    fn build_command(&self, opts: &SpawnOptions) -> std::process::Command {
        let mut command = std::process::Command::new("kimi");
        if let Some(session_id) = &opts.resume_session_id {
            command.arg("-r");
            command.arg(session_id);
        } else if opts.continue_last {
            command.arg("-c");
        }
        if let Some(model) = &opts.model {
            command.arg("-m");
            command.arg(model);
        }
        command.arg("-p");
        command.arg(&opts.prompt);
        command.arg("--output-format");
        command.arg("stream-json");
        command.args(&opts.extra_args);
        command
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipe::cli::traits::CliCommandBuilder;

    fn args(command: &std::process::Command) -> Vec<String> {
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn parses_current_assistant_and_resume_hint_records() {
        let mut parser = KimiNdjsonParser::new();
        let assistant = parser.parse_line(r#"{"role":"assistant","content":"MARKER"}"#);
        assert!(matches!(
            assistant.as_slice(),
            [CliEvent::AssistantText { text, is_delta: false }] if text == "MARKER"
        ));

        let identity = parser.parse_line(
            r#"{"role":"meta","type":"session.resume_hint","session_id":"session_UUID"}"#,
        );
        assert!(matches!(
            identity.as_slice(),
            [CliEvent::SessionStart { session_id, .. }] if session_id == "session_UUID"
        ));
        assert_eq!(parser.detected_session_id(), Some("session_UUID"));
    }

    #[test]
    fn parses_tool_call_arguments_and_tool_result() {
        let mut parser = KimiNdjsonParser::new();
        let calls = parser.parse_line(
            r#"{"role":"assistant","content":"Checking","tool_calls":[{"type":"function","id":"tc_1","function":{"name":"Shell","arguments":"{\"command\":\"pwd\"}"}}]}"#,
        );
        assert_eq!(calls.len(), 2);
        assert!(matches!(
            &calls[1],
            CliEvent::ToolCallStart { id, name, input }
                if id == "tc_1" && name == "Shell" && input["command"] == "pwd"
        ));

        let results = parser.parse_line(
            r#"{"role":"tool","tool_call_id":"tc_1","content":"C:\\repo","is_error":false,"duration_ms":12}"#,
        );
        assert!(matches!(
            results.as_slice(),
            [CliEvent::ToolCallResult { id, output, is_error: false, duration_ms: Some(12) }]
                if id == "tc_1" && output == "C:\\repo"
        ));
    }

    #[test]
    fn surfaces_retry_meta_without_ending_the_session() {
        let mut parser = KimiNdjsonParser::new();
        let events = parser.parse_line(
            r#"{"role":"meta","type":"provider.retry","reason":"overloaded","attempt":2,"max_attempts":10,"delay_ms":1500}"#,
        );
        assert!(matches!(
            events.as_slice(),
            [CliEvent::Error { message }]
                if message.contains("overloaded") && message.contains("attempt=2")
        ));
    }

    #[test]
    fn builds_fresh_and_resumed_stream_json_commands() {
        let fresh = KimiPipeBuilder.build_command(&SpawnOptions {
            prompt: "fresh prompt".to_owned(),
            ..SpawnOptions::default()
        });
        assert_eq!(
            args(&fresh),
            ["-p", "fresh prompt", "--output-format", "stream-json"]
        );

        let resumed = KimiPipeBuilder.build_command(&SpawnOptions {
            prompt: "follow up".to_owned(),
            resume_session_id: Some("session_UUID".to_owned()),
            model: Some("kimi-code/kimi-for-coding".to_owned()),
            permission_mode: Some("plan".to_owned()),
            ..SpawnOptions::default()
        });
        assert_eq!(
            args(&resumed),
            [
                "-r",
                "session_UUID",
                "-m",
                "kimi-code/kimi-for-coding",
                "-p",
                "follow up",
                "--output-format",
                "stream-json",
            ]
        );
    }
}
