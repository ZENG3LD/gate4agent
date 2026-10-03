//! Claude Agent SDK `query()` stream-json codec.
//!
//! The official SDK has no ACP session. It spawns the `claude` CLI with
//! `--output-format stream-json --input-format stream-json --verbose` and
//! exchanges one JSON object per line. Control initialize is a
//! `control_request` whose `request.subtype` is `initialize` (see
//! `anthropics/claude-agent-sdk-python` `tests/test_streaming_client.py` and
//! `tests/test_close_cancellation.py`). This file does not spawn `npx` or
//! `@agentclientprotocol/claude-agent-acp`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use super::stdio_child::StdioChild;
use super::CoreError;

/// Argv the Agent SDK uses for streaming `query()`. No ACP adapter.
pub const CLAUDE_QUERY_ARGS: &[&str] = &[
    "--output-format",
    "stream-json",
    "--input-format",
    "stream-json",
    "--verbose",
];

/// One decoded stdout event from `query()`.
#[derive(Debug, Clone, PartialEq)]
pub enum ClaudeQueryEvent {
    /// `{"type":"system","subtype":"init",...}`.
    SessionInit { session_id: String },
    /// Assistant text blocks, in order. Tool-use blocks are not turned into UI.
    AssistantText { text: Vec<String> },
    /// `{"type":"result",...}` — the SDK's end-of-turn message.
    Result {
        subtype: Option<String>,
        session_id: Option<String>,
    },
    /// SDK -> CLI `control_request`, or a request the CLI echoed.
    ControlRequest { request_id: String, subtype: String },
    /// CLI -> SDK `control_response`.
    ControlResponse { request_id: String, subtype: String },
    /// Echo of a user message.
    User,
    /// Some other `type` on the stream. The raw object is kept by the caller
    /// only when they parse with [`parse_claude_line`]; this variant names it.
    Other { type_name: String },
}

/// `claude` on `PATH`, if it is a regular file. Does not look for `npx`.
pub fn claude_binary() -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join("claude"))
        .find(|path| path.is_file())
}

/// Encode the SDK initialize control request.
///
/// Field order matches the SDK tests: `type`, `request_id`, then `request`.
/// `serde_json::json!` sorts keys, so this is a struct on purpose.
pub fn encode_control_initialize(request_id: &str) -> Result<String, CoreError> {
    #[derive(Serialize)]
    struct Request<'a> {
        subtype: &'a str,
    }
    #[derive(Serialize)]
    struct Line<'a> {
        #[serde(rename = "type")]
        kind: &'a str,
        request_id: &'a str,
        request: Request<'a>,
    }
    to_line(&Line {
        kind: "control_request",
        request_id,
        request: Request {
            subtype: "initialize",
        },
    })
}

/// Encode one user turn the way streaming `query()` writes stdin.
pub fn encode_user_message(text: &str) -> Result<String, CoreError> {
    #[derive(Serialize)]
    struct TextBlock<'a> {
        #[serde(rename = "type")]
        kind: &'a str,
        text: &'a str,
    }
    #[derive(Serialize)]
    struct Message<'a> {
        role: &'a str,
        content: Vec<TextBlock<'a>>,
    }
    #[derive(Serialize)]
    struct Line<'a> {
        #[serde(rename = "type")]
        kind: &'a str,
        session_id: &'a str,
        message: Message<'a>,
        parent_tool_use_id: Option<&'a str>,
    }
    to_line(&Line {
        kind: "user",
        session_id: "",
        message: Message {
            role: "user",
            content: vec![TextBlock { kind: "text", text }],
        },
        parent_tool_use_id: None,
    })
}

/// Parse one stream-json line into an event.
pub fn parse_claude_line(line: &str) -> Result<ClaudeQueryEvent, CoreError> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Err(CoreError::BadLine("empty".into()));
    }
    if !trimmed.starts_with('{') {
        return Err(CoreError::BadLine(format!(
            "not a json object: {}",
            truncate(trimmed)
        )));
    }
    let value: Value =
        serde_json::from_str(trimmed).map_err(|err| CoreError::BadLine(err.to_string()))?;
    let obj = value
        .as_object()
        .ok_or_else(|| CoreError::BadLine("expected a JSON object".into()))?;
    let type_name = obj
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    match type_name.as_str() {
        "system" if obj.get("subtype").and_then(Value::as_str) == Some("init") => {
            let session_id = obj
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            Ok(ClaudeQueryEvent::SessionInit { session_id })
        }
        "assistant" => Ok(ClaudeQueryEvent::AssistantText {
            text: assistant_text(&value),
        }),
        "result" => Ok(ClaudeQueryEvent::Result {
            subtype: obj
                .get("subtype")
                .and_then(Value::as_str)
                .map(str::to_owned),
            session_id: obj
                .get("session_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
        }),
        "control_request" => Ok(ClaudeQueryEvent::ControlRequest {
            request_id: request_id_of(&value).unwrap_or_default(),
            subtype: nested_subtype(&value, "request"),
        }),
        "control_response" => Ok(ClaudeQueryEvent::ControlResponse {
            request_id: request_id_of(&value).unwrap_or_default(),
            subtype: nested_subtype(&value, "response"),
        }),
        "user" => Ok(ClaudeQueryEvent::User),
        other => Ok(ClaudeQueryEvent::Other {
            type_name: other.to_owned(),
        }),
    }
}

/// Stdio client for `claude` in `query()` streaming mode.
///
/// [`Self::spawn`] is the live path and returns [`CoreError::ClaudeNotInstalled`]
/// when the CLI is absent. This slice does not start a live Claude process
/// unless that binary is on `PATH`.
pub struct ClaudeQueryClient {
    io: StdioChild,
}

impl ClaudeQueryClient {
    /// Spawn `claude` with [`CLAUDE_QUERY_ARGS`]. No PTY, no SDK npm package.
    pub fn spawn() -> Result<Self, CoreError> {
        let program = claude_binary().ok_or(CoreError::ClaudeNotInstalled)?;
        Self::spawn_program(&program)
    }

    pub fn spawn_program(program: &Path) -> Result<Self, CoreError> {
        let mut command = Command::new(program);
        command.args(CLAUDE_QUERY_ARGS);
        Self::from_command(command)
    }

    pub fn from_command(command: Command) -> Result<Self, CoreError> {
        Ok(Self {
            io: StdioChild::spawn(command)?,
        })
    }

    pub fn set_read_timeout(&mut self, timeout: Duration) {
        self.io.set_read_timeout(timeout);
    }

    pub fn send_initialize(&mut self, request_id: &str) -> Result<(), CoreError> {
        self.io.write_line(&encode_control_initialize(request_id)?)
    }

    pub fn send_user_message(&mut self, text: &str) -> Result<(), CoreError> {
        self.io.write_line(&encode_user_message(text)?)
    }

    /// Parse the next stdout event.
    pub fn read_event(&mut self) -> Result<ClaudeQueryEvent, CoreError> {
        let line = self.io.read_raw_line()?;
        parse_claude_line(&line)
    }
}

fn assistant_text(value: &Value) -> Vec<String> {
    let Some(blocks) = value.pointer("/message/content").and_then(Value::as_array) else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}

fn request_id_of(value: &Value) -> Option<String> {
    value
        .get("request_id")
        .and_then(Value::as_str)
        .or_else(|| value.pointer("/request/request_id").and_then(Value::as_str))
        .or_else(|| {
            value
                .pointer("/response/request_id")
                .and_then(Value::as_str)
        })
        .map(str::to_owned)
}

fn nested_subtype(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(|nested| nested.get("subtype"))
        .and_then(Value::as_str)
        .or_else(|| value.get("subtype").and_then(Value::as_str))
        .unwrap_or("")
        .to_owned()
}

fn to_line(value: &impl serde::Serialize) -> Result<String, CoreError> {
    serde_json::to_string(value).map_err(|err| CoreError::BadLine(err.to_string()))
}

fn truncate(text: &str) -> String {
    text.chars().take(80).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fields from `anthropics/claude-agent-sdk-python`
    /// `tests/test_close_cancellation.py` (`FAKE_CLI`, commit 7968c40c).
    const SYSTEM_INIT_LINE: &str = r#"{"type":"system","subtype":"init","session_id":"s","model":"m","cwd":".","tools":[],"mcp_servers":[],"permissionMode":"default","apiKeySource":"none"}"#;

    /// Control response the same fixture prints for a `control_request`.
    const CONTROL_RESPONSE_LINE: &str = r#"{"type":"control_response","response":{"subtype":"success","request_id":"1","response":{}}}"#;

    /// Stream-json assistant shape: `type=assistant`, `message.content[]`
    /// text blocks. Public CLI protocol shape, not a packet capture.
    const ASSISTANT_LINE: &str = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Paris is the capital of France."}]},"session_id":"abc123"}"#;

    /// Stream-json result shape from the same public protocol.
    const RESULT_LINE: &str =
        r#"{"type":"result","subtype":"success","total_cost_usd":0.001,"session_id":"abc123"}"#;

    #[test]
    fn encode_initialize_is_query_control_not_acp() {
        let line = encode_control_initialize("req_1").unwrap();
        assert_eq!(
            line,
            r#"{"type":"control_request","request_id":"req_1","request":{"subtype":"initialize"}}"#
        );
        assert!(!line.contains("jsonrpc"));
        assert!(!line.contains("npx"));
        assert!(!line.contains("agentclientprotocol"));
        assert!(!line.contains("protocolVersion"));
        let event = parse_claude_line(&line).unwrap();
        assert_eq!(
            event,
            ClaudeQueryEvent::ControlRequest {
                request_id: "req_1".into(),
                subtype: "initialize".into(),
            }
        );
    }

    #[test]
    fn parse_sdk_fixture_lines() {
        let init = parse_claude_line(SYSTEM_INIT_LINE).unwrap();
        assert_eq!(
            init,
            ClaudeQueryEvent::SessionInit {
                session_id: "s".into(),
            }
        );
        let control = parse_claude_line(CONTROL_RESPONSE_LINE).unwrap();
        assert_eq!(
            control,
            ClaudeQueryEvent::ControlResponse {
                request_id: "1".into(),
                subtype: "success".into(),
            }
        );
        let assistant = parse_claude_line(ASSISTANT_LINE).unwrap();
        assert_eq!(
            assistant,
            ClaudeQueryEvent::AssistantText {
                text: vec!["Paris is the capital of France.".into()],
            }
        );
        let result = parse_claude_line(RESULT_LINE).unwrap();
        assert_eq!(
            result,
            ClaudeQueryEvent::Result {
                subtype: Some("success".into()),
                session_id: Some("abc123".into()),
            }
        );
    }

    #[test]
    fn user_message_round_trips_as_user_event() {
        let line = encode_user_message("Run tests").unwrap();
        assert_eq!(parse_claude_line(&line).unwrap(), ClaudeQueryEvent::User);
        assert!(line.contains("\"text\":\"Run tests\""));
    }

    #[test]
    fn non_json_cli_noise_is_not_an_event() {
        let err = parse_claude_line("[SandboxDebug] seccomp filtering not available").unwrap_err();
        assert!(matches!(err, CoreError::BadLine(_)));
    }

    #[test]
    fn query_args_do_not_shell_out_to_the_acp_adapter() {
        assert_eq!(
            CLAUDE_QUERY_ARGS,
            &[
                "--output-format",
                "stream-json",
                "--input-format",
                "stream-json",
                "--verbose",
            ]
        );
    }

    #[test]
    fn stand_in_writes_initialize_and_parses_one_event() {
        // Not the Claude CLI. The live `claude` spawn is a separate test and
        // does not run when the binary is absent.
        let script = r#"
import json, sys
line = sys.stdin.readline()
msg = json.loads(line)
assert msg["type"] == "control_request", msg
assert msg["request"]["subtype"] == "initialize"
sys.stdout.write(json.dumps({
    "type": "control_response",
    "response": {
        "subtype": "success",
        "request_id": msg["request_id"],
        "response": {},
    },
}) + "\n")
sys.stdout.flush()
"#;
        let mut command = Command::new("python3");
        command.arg("-c").arg(script);
        let mut client = ClaudeQueryClient::from_command(command).unwrap();
        client.set_read_timeout(Duration::from_secs(2));
        client.send_initialize("req_1").unwrap();
        let event = client.read_event().unwrap();
        assert_eq!(
            event,
            ClaudeQueryEvent::ControlResponse {
                request_id: "req_1".into(),
                subtype: "success".into(),
            }
        );
    }

    #[test]
    fn live_claude_spawn_only_when_the_cli_exists() {
        if claude_binary().is_none() {
            match ClaudeQueryClient::spawn() {
                Err(CoreError::ClaudeNotInstalled) => {}
                Err(other) => panic!("expected ClaudeNotInstalled, got {other}"),
                Ok(_) => panic!("spawn succeeded without a claude binary"),
            }
            return;
        }
        let mut client = ClaudeQueryClient::spawn().unwrap();
        client.set_read_timeout(Duration::from_secs(3));
        client.send_initialize("req_live").unwrap();
        let event = client.read_event().unwrap();
        assert!(matches!(
            event,
            ClaudeQueryEvent::SessionInit { .. }
                | ClaudeQueryEvent::ControlResponse { .. }
                | ClaudeQueryEvent::Other { .. }
        ));
    }
}
