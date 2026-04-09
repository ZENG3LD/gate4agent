//! Pipe-mode Gemini bindings: NDJSON parser + spawn builder.

use super::traits::{CliEvent, NdjsonParser};
use crate::utils::truncate_str;
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
                return vec![CliEvent::Error {
                    message: format!("invalid JSON: {}", truncate_str(line, 100)),
                }]
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
