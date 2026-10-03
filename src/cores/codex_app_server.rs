//! Codex app-server JSON-RPC codec and a one-thread stdio client.
//!
//! Wire, from the public app-server README (`openai/codex`,
//! `codex-rs/app-server/README.md`): JSON-RPC 2.0 with the `"jsonrpc":"2.0"`
//! header omitted, one object per stdout line. Handshake is `initialize`,
//! then an `initialized` notification, then `thread/start`. A turn is
//! `turn/start`. This file does not spawn `npx` or an ACP adapter.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

use super::stdio_child::StdioChild;
use super::CoreError;

/// Argv after the `codex` binary. The core never inserts `npx`.
pub const CODEX_APP_SERVER_ARGS: &[&str] = &["app-server"];

/// JSON-RPC id. App-server examples use integers; strings are accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum CodexId {
    Number(i64),
    String(String),
}

/// `initialize.params.clientInfo`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexClientInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub version: String,
}

/// `initialize` params. Capabilities are omitted unless the caller sets them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexInitializeParams {
    pub client_info: CodexClientInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<CodexInitializeCapabilities>,
}

/// Subset of `initialize.params.capabilities` this slice actually sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexInitializeCapabilities {
    pub experimental_api: bool,
}

/// `thread/start` params this slice sends. Sandbox and permissions are not
/// combined; neither is sent unless the caller sets a field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CodexThreadStartParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
}

/// One user input item on `turn/start`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type")]
pub enum CodexUserInput {
    #[serde(rename = "text")]
    Text { text: String },
}

/// `turn/start` params. `threadId` and `input` are the required fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexTurnStartParams {
    pub thread_id: String,
    pub input: Vec<CodexUserInput>,
}

/// One parsed app-server line. This is the event; nothing here is a screen.
#[derive(Debug, Clone, PartialEq)]
pub enum CodexMessage {
    Request {
        id: CodexId,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    Response {
        id: CodexId,
        result: Value,
    },
    Error {
        id: Option<CodexId>,
        code: i64,
        message: String,
    },
}

/// `codex` on `PATH`, if it is a regular file. Does not look for `npx`.
pub fn codex_binary() -> Option<PathBuf> {
    executable_on_path("codex")
}

fn executable_on_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

/// Encode `initialize`. The `"jsonrpc"` header is intentionally absent.
pub fn encode_initialize(id: CodexId, params: &CodexInitializeParams) -> Result<String, CoreError> {
    encode_request("initialize", id, params)
}

/// Encode the post-initialize notification. No id, so it is not a request.
pub fn encode_initialized() -> Result<String, CoreError> {
    to_line(&json!({"method": "initialized"}))
}

/// Encode `thread/start`.
pub fn encode_thread_start(
    id: CodexId,
    params: &CodexThreadStartParams,
) -> Result<String, CoreError> {
    encode_request("thread/start", id, params)
}

/// Encode `turn/start`.
pub fn encode_turn_start(id: CodexId, params: &CodexTurnStartParams) -> Result<String, CoreError> {
    encode_request("turn/start", id, params)
}

/// Parse one JSON-RPC line. Blank lines and non-JSON (including terminal
/// text) are errors. `"jsonrpc"` may be absent, which is the app-server wire.
pub fn parse_codex_line(line: &str) -> Result<CodexMessage, CoreError> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Err(CoreError::BadLine("empty".into()));
    }
    let value: Value =
        serde_json::from_str(trimmed).map_err(|err| CoreError::BadLine(err.to_string()))?;
    let obj = value
        .as_object()
        .ok_or_else(|| CoreError::BadLine("expected a JSON object".into()))?;
    let id = match obj.get("id") {
        Some(raw) => Some(parse_id(raw)?),
        None => None,
    };
    let method = obj.get("method").and_then(Value::as_str).map(str::to_owned);
    let params = obj.get("params").cloned().unwrap_or(Value::Null);
    if let Some(method) = method {
        return match id {
            Some(id) => Ok(CodexMessage::Request { id, method, params }),
            None => Ok(CodexMessage::Notification { method, params }),
        };
    }
    if obj.get("error").is_some() {
        let error = &obj["error"];
        let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("rpc error")
            .to_owned();
        return Ok(CodexMessage::Error { id, code, message });
    }
    if let Some(id) = id {
        let result = obj.get("result").cloned().unwrap_or(Value::Null);
        return Ok(CodexMessage::Response { id, result });
    }
    Err(CoreError::BadLine(
        "object is neither a request, notification, nor response".into(),
    ))
}

/// Stdio client for `codex app-server`.
///
/// [`Self::spawn`] starts the real binary when it is on `PATH`. The live
/// handshake sends `initialize`, reads that response, sends `initialized`,
/// sends `thread/start`, and returns the next JSON-RPC line.
pub struct CodexAppServer {
    io: StdioChild,
}

impl CodexAppServer {
    /// Spawn `codex app-server` with piped stdio. No PTY.
    pub fn spawn() -> Result<Self, CoreError> {
        let program = codex_binary().ok_or(CoreError::CodexNotInstalled)?;
        Self::spawn_program(&program)
    }

    /// Spawn `program app-server`. Tests pass a stand-in; production uses
    /// [`Self::spawn`], which only looks up `codex`.
    pub fn spawn_program(program: &Path) -> Result<Self, CoreError> {
        let mut command = Command::new(program);
        command.args(CODEX_APP_SERVER_ARGS);
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

    pub fn send_initialize(
        &mut self,
        id: CodexId,
        params: &CodexInitializeParams,
    ) -> Result<(), CoreError> {
        self.io.write_line(&encode_initialize(id, params)?)
    }

    pub fn send_initialized(&mut self) -> Result<(), CoreError> {
        self.io.write_line(&encode_initialized()?)
    }

    pub fn send_thread_start(
        &mut self,
        id: CodexId,
        params: &CodexThreadStartParams,
    ) -> Result<(), CoreError> {
        self.io.write_line(&encode_thread_start(id, params)?)
    }

    pub fn send_turn_start(
        &mut self,
        id: CodexId,
        params: &CodexTurnStartParams,
    ) -> Result<(), CoreError> {
        self.io.write_line(&encode_turn_start(id, params)?)
    }

    /// Parse the next stdout line.
    pub fn read_message(&mut self) -> Result<CodexMessage, CoreError> {
        let line = self.io.read_raw_line()?;
        parse_codex_line(&line).map_err(|err| match err {
            CoreError::BadLine(detail) => {
                let tail = self.io.stderr_tail();
                if tail.is_empty() {
                    CoreError::BadLine(detail)
                } else {
                    CoreError::BadLine(format!("{detail}; stderr: {tail}"))
                }
            }
            other => other,
        })
    }

    /// Read until a response or error with `id`, skipping notifications.
    pub fn read_response(
        &mut self,
        id: &CodexId,
        max_lines: usize,
    ) -> Result<CodexMessage, CoreError> {
        for _ in 0..max_lines {
            match self.read_message()? {
                CodexMessage::Notification { .. } => continue,
                CodexMessage::Response { id: got, result } if &got == id => {
                    return Ok(CodexMessage::Response { id: got, result });
                }
                CodexMessage::Error {
                    id: got,
                    code,
                    message,
                } if got.as_ref() == Some(id) => {
                    return Err(CoreError::Rpc { code, message });
                }
                other => {
                    return Err(CoreError::Unexpected(format!("{other:?}")));
                }
            }
        }
        Err(CoreError::Unexpected(format!(
            "no response for id {id:?} within {max_lines} lines"
        )))
    }

    /// `initialize` + `initialized` + `thread/start`, then one parsed line.
    ///
    /// The initialize response is consumed (the server rejects later methods
    /// before it). The returned line is whatever the server writes next
    /// after `thread/start` — the response or a notification such as
    /// `thread/started`.
    pub fn initialize_and_start_thread(&mut self, cwd: &Path) -> Result<CodexMessage, CoreError> {
        let init_id = CodexId::Number(0);
        self.send_initialize(init_id.clone(), &gate4agent_initialize_params())?;
        self.read_response(&init_id, 8)?;
        self.send_initialized()?;
        let thread_id = CodexId::Number(1);
        self.send_thread_start(
            thread_id,
            &CodexThreadStartParams {
                cwd: Some(cwd.display().to_string()),
                ..CodexThreadStartParams::default()
            },
        )?;
        self.read_message()
    }
}

fn gate4agent_initialize_params() -> CodexInitializeParams {
    CodexInitializeParams {
        client_info: CodexClientInfo {
            name: "gate4agent".into(),
            title: Some("gate4agent".into()),
            version: env!("CARGO_PKG_VERSION").into(),
        },
        capabilities: None,
    }
}

fn encode_request(method: &str, id: CodexId, params: &impl Serialize) -> Result<String, CoreError> {
    #[derive(Serialize)]
    struct Out<'a, P: Serialize> {
        method: &'a str,
        id: CodexId,
        params: &'a P,
    }
    to_line(&Out { method, id, params })
}

fn to_line(value: &impl Serialize) -> Result<String, CoreError> {
    serde_json::to_string(value).map_err(|err| CoreError::BadLine(err.to_string()))
}

fn parse_id(value: &Value) -> Result<CodexId, CoreError> {
    match value {
        Value::Number(number) => number
            .as_i64()
            .map(CodexId::Number)
            .ok_or_else(|| CoreError::BadLine(format!("id is not an i64: {number}"))),
        Value::String(text) => Ok(CodexId::String(text.clone())),
        other => Err(CoreError::BadLine(format!(
            "id is not a string or number: {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minified from openai/codex `codex-rs/app-server/README.md` (the VS Code
    /// extension example). The header is omitted on the wire.
    const INITIALIZE_LINE: &str = r#"{"method":"initialize","id":0,"params":{"clientInfo":{"name":"codex_vscode","title":"Codex VS Code Extension","version":"0.1.0"}}}"#;

    /// Result fields the same README says `initialize` returns: userAgent,
    /// codexHome, platformFamily, platformOs. Not a packet capture.
    const INITIALIZE_RESULT_LINE: &str = r#"{"id":0,"result":{"userAgent":"codex-vscode/0.1.0","codexHome":"/Users/me/.codex","platformFamily":"unix","platformOs":"linux"}}"#;

    /// Minified response example from that README's `thread/start` section.
    const THREAD_START_RESULT_LINE: &str = r#"{"id":10,"result":{"thread":{"id":"thr_123","preview":"","modelProvider":"openai","createdAt":1730910000}}}"#;

    /// Minified required fields from that README's `turn/start` example.
    const TURN_START_LINE: &str = r#"{"method":"turn/start","id":30,"params":{"threadId":"thr_123","input":[{"type":"text","text":"Run tests"}]}}"#;

    /// Minified `turn/start` result example from that README.
    const TURN_RESULT_LINE: &str = r#"{"id":30,"result":{"turn":{"id":"turn_456","status":"inProgress","items":[],"error":null}}}"#;

    #[test]
    fn encode_initialize_matches_readme_example_and_omits_jsonrpc() {
        let line = encode_initialize(
            CodexId::Number(0),
            &CodexInitializeParams {
                client_info: CodexClientInfo {
                    name: "codex_vscode".into(),
                    title: Some("Codex VS Code Extension".into()),
                    version: "0.1.0".into(),
                },
                capabilities: None,
            },
        )
        .unwrap();
        assert_eq!(line, INITIALIZE_LINE);
        assert!(!line.contains("jsonrpc"));
        assert!(!line.contains("npx"));
        assert!(!line.contains("agentclientprotocol"));
    }

    #[test]
    fn encode_thread_and_turn_match_readme_examples() {
        let thread = encode_thread_start(
            CodexId::Number(10),
            &CodexThreadStartParams {
                model: Some("gpt-5.1-codex".into()),
                cwd: Some("/Users/me/project".into()),
                approval_policy: Some("never".into()),
                sandbox: Some("workspaceWrite".into()),
            },
        )
        .unwrap();
        assert_eq!(
            thread,
            r#"{"method":"thread/start","id":10,"params":{"model":"gpt-5.1-codex","cwd":"/Users/me/project","approvalPolicy":"never","sandbox":"workspaceWrite"}}"#
        );

        let turn = encode_turn_start(
            CodexId::Number(30),
            &CodexTurnStartParams {
                thread_id: "thr_123".into(),
                input: vec![CodexUserInput::Text {
                    text: "Run tests".into(),
                }],
            },
        )
        .unwrap();
        assert_eq!(turn, TURN_START_LINE);
    }

    #[test]
    fn parse_readme_lines() {
        let init = parse_codex_line(INITIALIZE_LINE).unwrap();
        match init {
            CodexMessage::Request { id, method, params } => {
                assert_eq!(id, CodexId::Number(0));
                assert_eq!(method, "initialize");
                assert_eq!(params["clientInfo"]["name"], "codex_vscode");
            }
            other => panic!("{other:?}"),
        }

        let result = parse_codex_line(INITIALIZE_RESULT_LINE).unwrap();
        match result {
            CodexMessage::Response { id, result } => {
                assert_eq!(id, CodexId::Number(0));
                assert_eq!(result["platformOs"], "linux");
                assert_eq!(result["codexHome"], "/Users/me/.codex");
            }
            other => panic!("{other:?}"),
        }

        let thread = parse_codex_line(THREAD_START_RESULT_LINE).unwrap();
        match thread {
            CodexMessage::Response { result, .. } => {
                assert_eq!(result["thread"]["id"], "thr_123");
            }
            other => panic!("{other:?}"),
        }

        let started =
            parse_codex_line(r#"{"method":"thread/started","params":{"thread":{"id":"thr_123"}}}"#)
                .unwrap();
        match started {
            CodexMessage::Notification { method, params } => {
                assert_eq!(method, "thread/started");
                assert_eq!(params["thread"]["id"], "thr_123");
            }
            other => panic!("{other:?}"),
        }

        let turn = parse_codex_line(TURN_RESULT_LINE).unwrap();
        match turn {
            CodexMessage::Response { result, .. } => {
                assert_eq!(result["turn"]["id"], "turn_456");
                assert_eq!(result["turn"]["status"], "inProgress");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn screen_text_is_not_a_wire_line() {
        let err = parse_codex_line("\u{1b}[32mcodex>\u{1b}[0m").unwrap_err();
        assert!(matches!(err, CoreError::BadLine(_)));
    }

    #[test]
    fn app_server_args_are_not_the_acp_adapter() {
        assert_eq!(CODEX_APP_SERVER_ARGS, &["app-server"]);
    }

    #[test]
    fn live_app_server_only_when_codex_is_installed() {
        if codex_binary().is_none() {
            match CodexAppServer::spawn() {
                Err(CoreError::CodexNotInstalled) => {}
                Err(other) => panic!("expected CodexNotInstalled, got {other}"),
                Ok(_) => panic!("spawn succeeded without a codex binary"),
            }
            return;
        }
        let mut client = CodexAppServer::spawn().unwrap();
        client.set_read_timeout(Duration::from_secs(3));
        let message = client
            .initialize_and_start_thread(Path::new("."))
            .expect("codex app-server initialize + thread/start");
        assert!(matches!(
            message,
            CodexMessage::Response { .. } | CodexMessage::Notification { .. }
        ));
    }

    #[test]
    fn stand_in_initialize_and_thread_start_parses_one_line() {
        // Not the Codex binary. Proves spawn, initialize, thread/start, and
        // one parsed line when `codex` itself is absent.
        let script = r#"
import json, sys
def read():
    line = sys.stdin.readline()
    assert line, "eof"
    return json.loads(line)
init = read()
assert init["method"] == "initialize", init
assert "jsonrpc" not in init
assert init["params"]["clientInfo"]["name"] == "gate4agent"
sys.stdout.write(json.dumps({"id": init["id"], "result": {"userAgent": "codex/test", "codexHome": "/tmp/codex", "platformFamily": "unix", "platformOs": "linux"}}) + "\n")
sys.stdout.flush()
note = read()
assert note["method"] == "initialized", note
assert "id" not in note
start = read()
assert start["method"] == "thread/start", start
assert start["params"]["cwd"] == "/workspace"
sys.stdout.write(json.dumps({"id": start["id"], "result": {"thread": {"id": "thr_test", "preview": "", "modelProvider": "openai", "createdAt": 1}}}) + "\n")
sys.stdout.flush()
"#;
        let mut command = Command::new("python3");
        command.arg("-c").arg(script);
        let mut client = CodexAppServer::from_command(command).unwrap();
        client.set_read_timeout(Duration::from_secs(2));
        let message = client
            .initialize_and_start_thread(Path::new("/workspace"))
            .unwrap();
        match message {
            CodexMessage::Response { id, result } => {
                assert_eq!(id, CodexId::Number(1));
                assert_eq!(result["thread"]["id"], "thr_test");
            }
            other => panic!("{other:?}"),
        }
    }
}
