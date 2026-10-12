//! Codex CLI: start a turn in a running thread through `codex app-server`.
//!
//! The operator runs one app-server (`codex app-server --listen unix://` or
//! `--listen ws://127.0.0.1:PORT`) and attaches the interactive TUI to it
//! with `codex --remote <same endpoint>`. This adapter is a second client on
//! that server. Wire (verified against codex-cli 0.160.1 on the box):
//! one JSON-RPC message per WebSocket text frame, no `"jsonrpc"` member;
//! `initialize` → `initialized` → `turn/start {threadId, input}`. A thread
//! the server has not loaded answers `thread not found`; the adapter then
//! sends `thread/resume {threadId}` and retries `turn/start` once.
//! `turn/start` on a thread with an active turn is accepted by the server.
//! The other client (the TUI) sees `turn/started` and the user item.
//!
//! `unix://` uses a Unix domain socket and is unix-only here; Windows uses
//! `ws://127.0.0.1:PORT`. Non-loopback `ws://` is refused.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tungstenite::{Message, WebSocket};

use super::{
    wake_prompt, ProviderKind, ProviderSession, SessionKind, WakeAdapter, WakeError, WakeLetter,
    WakeOutcome,
};

/// Env var naming the app-server endpoint (`unix:///path` or `ws://127.0.0.1:PORT`).
pub const CODEX_APP_SERVER_ENV: &str = "M4A_CODEX_APP_SERVER";

pub mod desktop;

const RPC_TIMEOUT: Duration = Duration::from_secs(20);

/// Where the shared app-server listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexEndpoint {
    /// The running Desktop owner's IPC router, never a new app-server.
    Desktop(PathBuf),
    /// `unix:///abs/path.sock` (WebSocket over a Unix socket).
    Unix(PathBuf),
    /// `ws://127.0.0.1:PORT` (loopback only).
    Ws(String),
}

impl CodexEndpoint {
    /// Parses `unix:///abs/path` or a loopback `ws://` URL. A bare
    /// `unix://` (Codex default socket) is not resolved here; pass the path.
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        if value == "desktop" {
            return desktop::default_endpoint().map(CodexEndpoint::Desktop);
        }
        if let Some(path) = value.strip_prefix("desktop://") {
            return (!path.is_empty()).then(|| CodexEndpoint::Desktop(path.into()));
        }
        if let Some(path) = value.strip_prefix("unix://") {
            return (!path.is_empty()).then(|| CodexEndpoint::Unix(PathBuf::from(path)));
        }
        let rest = value.strip_prefix("ws://")?;
        let host = rest.split(['/', '?']).next().unwrap_or("");
        let host = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
        matches!(host, "127.0.0.1" | "localhost" | "[::1]")
            .then(|| CodexEndpoint::Ws(value.to_string()))
    }

    /// Reads [`CODEX_APP_SERVER_ENV`].
    pub fn from_env() -> Option<Self> {
        std::env::var(CODEX_APP_SERVER_ENV)
            .ok()
            .and_then(|value| Self::parse(&value))
    }
}

/// Second client on the operator's Codex app-server.
pub struct CodexAppServerAdapter {
    endpoint: Option<CodexEndpoint>,
}

impl CodexAppServerAdapter {
    /// Adapter for `endpoint` (`None`: every wake is `Unavailable`).
    pub fn new(endpoint: Option<CodexEndpoint>) -> Self {
        Self { endpoint }
    }
}

impl WakeAdapter for CodexAppServerAdapter {
    fn kind(&self) -> SessionKind {
        SessionKind::local(ProviderKind::Codex)
    }

    fn probe(&self, session: &ProviderSession) -> Result<(), WakeError> {
        match &self.endpoint {
            None => Err(WakeError::Unavailable(
                "codex app-server endpoint unset".into(),
            )),
            Some(CodexEndpoint::Desktop(path)) => desktop::probe(path, &session.session_id),
            Some(CodexEndpoint::Unix(path)) if !path.exists() => Err(WakeError::Unavailable(
                "codex app-server socket missing".into(),
            )),
            Some(_) => Ok(()),
        }
    }

    fn wake(
        &mut self,
        session: &ProviderSession,
        letter: &WakeLetter<'_>,
    ) -> Result<WakeOutcome, WakeError> {
        self.probe(session)?;
        let text = wake_prompt(session, letter);
        match self.endpoint.clone() {
            Some(CodexEndpoint::Desktop(path)) => desktop::wake(&path, &session.session_id, &text),
            Some(CodexEndpoint::Ws(url)) => {
                let (mut ws, _) = tungstenite::connect(url.as_str())
                    .map_err(|err| WakeError::Unavailable(format!("codex ws: {err}")))?;
                if let tungstenite::stream::MaybeTlsStream::Plain(stream) = ws.get_mut() {
                    stream.set_read_timeout(Some(RPC_TIMEOUT))
                        .map_err(|_| WakeError::Transport("codex read timeout setup failed".into()))?;
                    stream.set_write_timeout(Some(RPC_TIMEOUT))
                        .map_err(|_| WakeError::Transport("codex write timeout setup failed".into()))?;
                }
                start_turn(&mut ws, &session.session_id, &text)
            }
            #[cfg(unix)]
            Some(CodexEndpoint::Unix(path)) => {
                let stream = std::os::unix::net::UnixStream::connect(&path)
                    .map_err(|err| WakeError::Unavailable(format!("codex unix: {}", err.kind())))?;
                stream
                    .set_read_timeout(Some(RPC_TIMEOUT))
                    .map_err(|err| WakeError::Transport(err.to_string()))?;
                let (mut ws, _) = tungstenite::client("ws://localhost/", stream)
                    .map_err(|err| WakeError::Unavailable(format!("codex handshake: {err}")))?;
                start_turn(&mut ws, &session.session_id, &text)
            }
            #[cfg(not(unix))]
            Some(CodexEndpoint::Unix(_)) => Err(WakeError::Unavailable(
                "unix:// app-server is unix-only here; use ws://127.0.0.1:PORT".into(),
            )),
            None => Err(WakeError::Unavailable(
                "codex app-server endpoint unset".into(),
            )),
        }
    }
}

/// `initialize` request (exact shape verified on 0.160.1).
pub fn initialize_request(id: i64) -> Value {
    json!({
        "id": id,
        "method": "initialize",
        "params": {"clientInfo": {"name": "mail4agent", "version": env!("CARGO_PKG_VERSION")}},
    })
}

/// `turn/start` request with one text input.
pub fn turn_start_request(id: i64, thread_id: &str, text: &str) -> Value {
    json!({
        "id": id,
        "method": "turn/start",
        "params": {"threadId": thread_id, "input": [{"type": "text", "text": text}]},
    })
}

/// `thread/resume` request.
pub fn thread_resume_request(id: i64, thread_id: &str) -> Value {
    json!({"id": id, "method": "thread/resume", "params": {"threadId": thread_id}})
}

/// Runs the handshake and one `turn/start` (with one resume retry) on an
/// open WebSocket. Generic over the stream so tests use a fake server.
pub fn start_turn<S: Read + Write>(
    ws: &mut WebSocket<S>,
    thread_id: &str,
    text: &str,
) -> Result<WakeOutcome, WakeError> {
    call(ws, initialize_request(0))?
        .map_err(|msg| WakeError::Transport(format!("initialize: {msg}")))?;
    send(ws, json!({"method": "initialized"}))?;
    match call(ws, turn_start_request(1, thread_id, text))? {
        Ok(_) => Ok(WakeOutcome::Delivered),
        Err(msg) if msg.contains("not found") || msg.contains("not loaded") => {
            call(ws, thread_resume_request(2, thread_id))?
                .map_err(|msg| WakeError::Transport(format!("thread/resume: {msg}")))?;
            call(ws, turn_start_request(3, thread_id, text))?
                .map(|_| WakeOutcome::Delivered)
                .map_err(|msg| WakeError::Transport(format!("turn/start: {msg}")))
        }
        Err(msg) => Err(WakeError::Transport(format!("turn/start: {msg}"))),
    }
}

fn send<S: Read + Write>(ws: &mut WebSocket<S>, value: Value) -> Result<(), WakeError> {
    ws.send(Message::text(value.to_string()))
        .map_err(|err| WakeError::Transport(format!("codex send: {err}")))
}

/// Sends `request` and reads until the response with the same id.
/// Notifications and server requests in between are skipped (approvals
/// stay with the TUI). Inner `Err` is the server's error message.
fn call<S: Read + Write>(
    ws: &mut WebSocket<S>,
    request: Value,
) -> Result<Result<Value, String>, WakeError> {
    let id = request["id"].clone();
    send(ws, request)?;
    let deadline = Instant::now() + RPC_TIMEOUT;
    while Instant::now() < deadline {
        let message = ws
            .read()
            .map_err(|err| WakeError::Transport(format!("codex read: {err}")))?;
        let Message::Text(text) = message else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(text.as_str()) else {
            continue;
        };
        if value.get("method").is_some() || value.get("id") != Some(&id) {
            continue;
        }
        if let Some(error) = value.get("error") {
            let msg = error["message"].as_str().unwrap_or("error").to_string();
            return Ok(Err(msg));
        }
        return Ok(Ok(value.get("result").cloned().unwrap_or(Value::Null)));
    }
    Err(WakeError::Transport("codex rpc timed out".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{letter, session};
    use std::net::TcpListener;

    fn fake_server(loaded: bool) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("ws://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept(stream).unwrap();
            let mut seen = Vec::new();
            let mut loaded = loaded;
            loop {
                let Ok(Message::Text(text)) = ws.read() else {
                    break;
                };
                let v: Value = serde_json::from_str(text.as_str()).unwrap();
                let method = v["method"].as_str().unwrap_or("").to_string();
                seen.push(method.clone());
                assert!(v.get("jsonrpc").is_none());
                let reply = match method.as_str() {
                    "initialize" => json!({"id": v["id"], "result": {"userAgent": "fake"}}),
                    "initialized" => continue,
                    "thread/resume" => {
                        loaded = true;
                        json!({"id": v["id"], "result": {"thread": {"id": "t"}}})
                    }
                    "turn/start" if loaded => {
                        assert_eq!(v["params"]["input"][0]["type"], "text");
                        ws.send(Message::text(
                            json!({"method": "turn/started", "params": {}}).to_string(),
                        ))
                        .unwrap();
                        json!({"id": v["id"], "result": {"turn": {"id": "u", "status": "inProgress"}}})
                    }
                    "turn/start" => {
                        json!({"id": v["id"], "error": {"code": -32600, "message": "thread not found: t"}})
                    }
                    _ => json!({"id": v["id"], "error": {"code": -32601, "message": "nope"}}),
                };
                ws.send(Message::text(reply.to_string())).unwrap();
                if seen.iter().filter(|m| *m == "turn/start").count() >= 1
                    && loaded
                    && method == "turn/start"
                {
                    break;
                }
            }
            seen
        });
        (endpoint, handle)
    }

    #[test]
    fn loaded_thread_gets_one_turn_start() {
        let (endpoint, server) = fake_server(true);
        let mut adapter = CodexAppServerAdapter::new(CodexEndpoint::parse(&endpoint));
        let s = session(SessionKind::local(ProviderKind::Codex));
        assert_eq!(
            adapter.wake(&s, &letter("hi")).unwrap(),
            WakeOutcome::Delivered
        );
        assert_eq!(
            server.join().unwrap(),
            ["initialize", "initialized", "turn/start"]
        );
    }

    #[test]
    fn unloaded_thread_is_resumed_then_started() {
        let (endpoint, server) = fake_server(false);
        let mut adapter = CodexAppServerAdapter::new(CodexEndpoint::parse(&endpoint));
        let s = session(SessionKind::local(ProviderKind::Codex));
        assert_eq!(
            adapter.wake(&s, &letter("hi")).unwrap(),
            WakeOutcome::Delivered
        );
        assert_eq!(
            server.join().unwrap(),
            [
                "initialize",
                "initialized",
                "turn/start",
                "thread/resume",
                "turn/start"
            ]
        );
    }

    #[test]
    fn endpoint_parse_is_loopback_only() {
        assert_eq!(
            CodexEndpoint::parse("unix:///tmp/a.sock"),
            Some(CodexEndpoint::Unix("/tmp/a.sock".into()))
        );
        assert!(CodexEndpoint::parse("ws://127.0.0.1:4500").is_some());
        assert!(CodexEndpoint::parse("ws://10.0.0.5:4500").is_none());
        assert!(CodexEndpoint::parse("unix://").is_none());
    }

    /// Live: `M4A_LIVE_CODEX_APP_SERVER=unix:///…` and `M4A_LIVE_CODEX_THREAD=<id>`.
    #[test]
    #[ignore]
    fn live_turn_start() {
        let endpoint = std::env::var("M4A_LIVE_CODEX_APP_SERVER").unwrap();
        let thread = std::env::var("M4A_LIVE_CODEX_THREAD").unwrap();
        let mut adapter = CodexAppServerAdapter::new(CodexEndpoint::parse(&endpoint));
        let mut s = session(SessionKind::local(ProviderKind::Codex));
        s.session_id = thread;
        assert_eq!(
            adapter.wake(&s, &letter("live probe")).unwrap(),
            WakeOutcome::Delivered
        );
    }
}
