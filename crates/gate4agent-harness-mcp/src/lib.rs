//! MCP stdio adapter for the grant-filtered harness read API.

use std::{io::{BufRead, Write}, net::SocketAddr};

use gate4agent_harness_client::{
    HarnessMailAddressV1, HarnessMailMessageId, HarnessMailRefV1, HarnessOperationId,
    HarnessReadClient,
    HarnessReadClientError, HarnessReadCredential, HarnessReadHostErrorV1, HarnessReadResponseV1,
    HarnessRevision, HarnessRunFinishOutcomeV1, HarnessRunId, HarnessRunLifecycleV1,
    HarnessSelectorV1, HarnessTaskId, HarnessTaskStateV1,
    SessionContextV1, HARNESS_MAIL_REFS_MAX, HARNESS_READ_REQUEST_MAX_BYTES,
    HARNESS_READ_RESPONSE_MAX_BYTES,
    HARNESS_READ_TOOL_IDS, HARNESS_WRITE_TOOL_IDS,
};
use gate4agent_node_protocol::HarnessReadRequestV1;
use gate4agent_node_wire::{LocalSessionHarnessMcpClient, LocalSessionHarnessMcpError};
use serde::Deserialize;
use serde_json::{json, Value};
use thiserror::Error;

pub const MCP_PROTOCOL_VERSION_CURRENT: &str = "2025-11-25";
pub const MCP_PROTOCOL_VERSION_COMPATIBLE: &str = "2025-06-18";
pub const HARNESS_MCP_ENDPOINT_ENV: &str = "GATE4AGENT_HARNESS_READ_ENDPOINT";
pub const HARNESS_MCP_CREDENTIAL_ENV: &str = "GATE4AGENT_HARNESS_READ_CREDENTIAL";
/// Names the file the helper appends a raw JSON-RPC stdio trace to, when
/// set. Debugging-only: absent by default, and its absence changes nothing
/// about the stdio loop's behaviour. See [`HarnessMcpStdioTrace`].
pub const HARNESS_MCP_TRACE_ENV: &str = "G4A_HARNESS_MCP_TRACE";

const JSONRPC_VERSION: &str = "2.0";
const SERVER_NAME: &str = "gate4agent-harness-mcp";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_LIMIT: u16 = 64;

pub trait HarnessMcpBackend {
    fn context_get(&self) -> Result<SessionContextV1, HarnessMcpBackendError>;
    fn call(&self, call: HarnessMcpToolCall) -> Result<HarnessReadResponseV1, HarnessMcpBackendError>;
}

impl HarnessMcpBackend for HarnessReadClient {
    fn context_get(&self) -> Result<SessionContextV1, HarnessMcpBackendError> {
        HarnessReadClient::context_get(self).map_err(HarnessMcpBackendError::from)
    }

    fn call(&self, call: HarnessMcpToolCall) -> Result<HarnessReadResponseV1, HarnessMcpBackendError> {
        let response = match call {
            HarnessMcpToolCall::ContextGet => HarnessReadResponseV1::Context(self.context_get()?),
            HarnessMcpToolCall::MonitorGet { run_id } => {
                HarnessReadResponseV1::Monitor(self.monitor_get(run_id).map_err(HarnessMcpBackendError::from)?)
            }
            HarnessMcpToolCall::TimelineRead { run_id, after_sequence, limit } => {
                HarnessReadResponseV1::Timeline(self.timeline_read(run_id, after_sequence, limit).map_err(HarnessMcpBackendError::from)?)
            }
            HarnessMcpToolCall::TasksList { after_task_id, state, limit } => {
                HarnessReadResponseV1::Tasks(self.tasks_list(after_task_id, state, limit).map_err(HarnessMcpBackendError::from)?)
            }
            HarnessMcpToolCall::TaskGet { task_id } => {
                HarnessReadResponseV1::Task(self.task_get(task_id).map_err(HarnessMcpBackendError::from)?)
            }
            HarnessMcpToolCall::RunsList { task_id, after_run_id, lifecycle, limit } => {
                HarnessReadResponseV1::Runs(self.runs_list(task_id, after_run_id, lifecycle, limit).map_err(HarnessMcpBackendError::from)?)
            }
            HarnessMcpToolCall::RunGet { run_id } => {
                HarnessReadResponseV1::Run(self.run_get(run_id).map_err(HarnessMcpBackendError::from)?)
            }
            HarnessMcpToolCall::OperationGet { operation_id } => {
                HarnessReadResponseV1::Operation(self.operation_get(operation_id).map_err(HarnessMcpBackendError::from)?)
            }
            HarnessMcpToolCall::MailSend { to, subject, body, reply_to, refs } => {
                HarnessReadResponseV1::MailSend(
                    self.mail_send(to, subject, body, reply_to, refs).map_err(HarnessMcpBackendError::from)?,
                )
            }
            HarnessMcpToolCall::MailInbox { since_unix_ms, limit } => {
                HarnessReadResponseV1::MailInbox(
                    self.mail_inbox(since_unix_ms, limit).map_err(HarnessMcpBackendError::from)?,
                )
            }
            HarnessMcpToolCall::MailAck { message_id } => {
                HarnessReadResponseV1::MailAck(self.mail_ack(message_id).map_err(HarnessMcpBackendError::from)?)
            }
            HarnessMcpToolCall::MailFetch { message_id, ref_index } => {
                HarnessReadResponseV1::MailFetch(
                    self.mail_fetch(message_id, ref_index).map_err(HarnessMcpBackendError::from)?,
                )
            }
            HarnessMcpToolCall::TaskCreate { title, body, parent_task_id } => {
                HarnessReadResponseV1::TaskCreate(
                    self.task_create(title, body, parent_task_id).map_err(HarnessMcpBackendError::from)?,
                )
            }
            HarnessMcpToolCall::TaskMove { task_id, expected_revision, to } => {
                HarnessReadResponseV1::TaskMove(
                    self.task_move(task_id, expected_revision, to).map_err(HarnessMcpBackendError::from)?,
                )
            }
            HarnessMcpToolCall::RunFinish { outcome, summary } => {
                HarnessReadResponseV1::RunFinish(
                    self.run_finish(outcome, summary).map_err(HarnessMcpBackendError::from)?,
                )
            }
        };
        Ok(response)
    }
}

impl HarnessMcpBackend for LocalSessionHarnessMcpClient {
    fn context_get(&self) -> Result<SessionContextV1, HarnessMcpBackendError> {
        self.send(HarnessReadRequestV1::ContextGet)
            .map_err(HarnessMcpBackendError::from)
            .and_then(|response| match response {
                HarnessReadResponseV1::Context(context) => Ok(context),
                _ => Err(HarnessMcpBackendError::Unavailable),
            })
    }

    fn call(&self, call: HarnessMcpToolCall) -> Result<HarnessReadResponseV1, HarnessMcpBackendError> {
        self.send(tool_call_request(call)).map_err(HarnessMcpBackendError::from)
    }
}

fn tool_call_request(call: HarnessMcpToolCall) -> HarnessReadRequestV1 {
    match call {
        HarnessMcpToolCall::ContextGet => HarnessReadRequestV1::ContextGet,
        HarnessMcpToolCall::MonitorGet { run_id } => HarnessReadRequestV1::MonitorGet { run_id },
        HarnessMcpToolCall::TimelineRead { run_id, after_sequence, limit } => {
            HarnessReadRequestV1::TimelineRead { run_id, after_sequence, limit }
        }
        HarnessMcpToolCall::TasksList { after_task_id, state, limit } => {
            HarnessReadRequestV1::TasksList { after_task_id, state, limit }
        }
        HarnessMcpToolCall::TaskGet { task_id } => HarnessReadRequestV1::TaskGet { task_id },
        HarnessMcpToolCall::RunsList { task_id, after_run_id, lifecycle, limit } => {
            HarnessReadRequestV1::RunsList { task_id, after_run_id, lifecycle, limit }
        }
        HarnessMcpToolCall::RunGet { run_id } => HarnessReadRequestV1::RunGet { run_id },
        HarnessMcpToolCall::OperationGet { operation_id } => {
            HarnessReadRequestV1::OperationGet { operation_id }
        }
        HarnessMcpToolCall::MailSend { to, subject, body, reply_to, refs } => {
            HarnessReadRequestV1::MailSend { to, subject, body, reply_to, refs }
        }
        HarnessMcpToolCall::MailInbox { since_unix_ms, limit } => {
            HarnessReadRequestV1::MailInbox { since_unix_ms, limit }
        }
        HarnessMcpToolCall::MailAck { message_id } => HarnessReadRequestV1::MailAck { message_id },
        HarnessMcpToolCall::MailFetch { message_id, ref_index } => {
            HarnessReadRequestV1::MailFetch { message_id, ref_index }
        }
        HarnessMcpToolCall::TaskCreate { title, body, parent_task_id } => {
            HarnessReadRequestV1::TaskCreate { title, body, parent_task_id }
        }
        HarnessMcpToolCall::TaskMove { task_id, expected_revision, to } => {
            HarnessReadRequestV1::TaskMove { task_id, expected_revision, to }
        }
        HarnessMcpToolCall::RunFinish { outcome, summary } => {
            HarnessReadRequestV1::RunFinish { outcome, summary }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HarnessMcpToolCall {
    ContextGet,
    MonitorGet { run_id: Option<HarnessRunId> },
    TimelineRead { run_id: Option<HarnessRunId>, after_sequence: Option<u64>, limit: u16 },
    TasksList { after_task_id: Option<HarnessTaskId>, state: Option<HarnessTaskStateV1>, limit: u16 },
    TaskGet { task_id: HarnessTaskId },
    RunsList {
        task_id: Option<HarnessTaskId>,
        after_run_id: Option<HarnessRunId>,
        lifecycle: Option<HarnessRunLifecycleV1>,
        limit: u16,
    },
    RunGet { run_id: HarnessRunId },
    OperationGet { operation_id: HarnessOperationId },
    MailSend {
        to: HarnessMailAddressV1,
        subject: String,
        body: String,
        reply_to: Option<HarnessMailMessageId>,
        refs: Vec<HarnessMailRefV1>,
    },
    MailInbox { since_unix_ms: Option<u64>, limit: u16 },
    MailAck { message_id: HarnessMailMessageId },
    MailFetch { message_id: HarnessMailMessageId, ref_index: u16 },
    TaskCreate {
        title: String,
        body: String,
        parent_task_id: Option<HarnessTaskId>,
    },
    TaskMove {
        task_id: HarnessTaskId,
        expected_revision: HarnessRevision,
        to: HarnessTaskStateV1,
    },
    RunFinish {
        outcome: HarnessRunFinishOutcomeV1,
        summary: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HarnessMcpBackendError {
    NotFoundOrDenied,
    Unauthorized,
    InvalidRequest,
    Unavailable,
}

impl From<HarnessReadClientError> for HarnessMcpBackendError {
    fn from(error: HarnessReadClientError) -> Self {
        match error {
            HarnessReadClientError::Host(HarnessReadHostErrorV1::NotFoundOrDenied) => Self::NotFoundOrDenied,
            HarnessReadClientError::Host(HarnessReadHostErrorV1::Unauthorized) => Self::Unauthorized,
            HarnessReadClientError::Host(HarnessReadHostErrorV1::InvalidRequest)
            | HarnessReadClientError::Api(_) => Self::InvalidRequest,
            _ => Self::Unavailable,
        }
    }
}

impl From<LocalSessionHarnessMcpError> for HarnessMcpBackendError {
    fn from(error: LocalSessionHarnessMcpError) -> Self {
        match error {
            LocalSessionHarnessMcpError::Unauthorized
            | LocalSessionHarnessMcpError::Host(HarnessReadHostErrorV1::Unauthorized) => {
                Self::Unauthorized
            }
            LocalSessionHarnessMcpError::InvalidRequest
            | LocalSessionHarnessMcpError::Host(HarnessReadHostErrorV1::InvalidRequest) => {
                Self::InvalidRequest
            }
            LocalSessionHarnessMcpError::Host(HarnessReadHostErrorV1::NotFoundOrDenied) => {
                Self::NotFoundOrDenied
            }
            _ => Self::Unavailable,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum McpState { AwaitInitialize, AwaitInitialized, Ready }

pub struct HarnessMcpServer<B> {
    backend: B,
    state: McpState,
}

impl<B: HarnessMcpBackend> HarnessMcpServer<B> {
    pub fn new(backend: B) -> Self { Self { backend, state: McpState::AwaitInitialize } }

    pub fn handle_line(&mut self, line: &[u8]) -> Option<Vec<u8>> {
        let response = self.handle_line_inner(line);
        response.map(|value| {
            let mut encoded = serde_json::to_vec(&value)
                .unwrap_or_else(|_| br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32603,"message":"internal error"}}"#.to_vec());
            if encoded.len() >= HARNESS_READ_RESPONSE_MAX_BYTES {
                encoded = br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32603,"message":"internal error"}}"#.to_vec();
            }
            encoded.push(b'\n');
            encoded
        })
    }

    fn handle_line_inner(&mut self, line: &[u8]) -> Option<Value> {
        if line.len() > HARNESS_READ_REQUEST_MAX_BYTES {
            return Some(rpc_error(Value::Null, -32600, "invalid request"));
        }
        let value: Value = match serde_json::from_slice(line) {
            Ok(value) => value,
            Err(_) => return Some(rpc_error(Value::Null, -32700, "parse error")),
        };
        if value.is_array() {
            return Some(rpc_error(Value::Null, -32600, "invalid request"));
        }
        let id_present = value.as_object().is_some_and(|object| object.contains_key("id"));
        let request: RpcRequest = match serde_json::from_value(value) {
            Ok(request) => request,
            Err(_) => return Some(rpc_error(Value::Null, -32600, "invalid request")),
        };
        if request.jsonrpc != JSONRPC_VERSION
            || id_present && request.id.is_none()
            || !valid_id(&request.id)
        {
            return Some(rpc_error(request.id.unwrap_or(Value::Null), -32600, "invalid request"));
        }
        let id = request.id.clone().unwrap_or(Value::Null);
        let is_notification = !id_present;
        if is_notification {
            if request.method == "notifications/initialized"
                && self.state == McpState::AwaitInitialized
                && metadata_only_params(request.params.as_ref())
            {
                self.state = McpState::Ready;
            }
            return None;
        }
        match request.method.as_str() {
            "initialize" => Some(self.initialize(id, request.params)),
            "ping" if self.state != McpState::AwaitInitialize && metadata_only_params(request.params.as_ref()) => {
                Some(rpc_result(id, json!({})))
            }
            "tools/list" => Some(self.tools_list(id, request.params)),
            "tools/call" => Some(self.tools_call(id, request.params)),
            _ => Some(rpc_error(id, -32601, "method not found")),
        }
    }

    fn initialize(&mut self, id: Value, params: Option<Value>) -> Value {
        if self.state != McpState::AwaitInitialize || id.is_null() {
            return rpc_error(id, -32600, "invalid request");
        }
        let params: InitializeParams = match params.and_then(|value| serde_json::from_value(value).ok()) {
            Some(params) => params,
            None => return rpc_error(id, -32602, "invalid params"),
        };
        if !matches!(params.protocol_version.as_str(), MCP_PROTOCOL_VERSION_CURRENT | MCP_PROTOCOL_VERSION_COMPATIBLE)
            || params.client_info.name.is_empty()
            || params.client_info.name.len() > 128
            || params.client_info.version.is_empty()
            || params.client_info.version.len() > 64
            || !params.capabilities.is_object()
            || params.meta.as_ref().is_some_and(|meta| !meta.is_object())
            || params.client_info.title.as_ref().is_some_and(|title| title.is_empty() || title.len() > 128)
            || params.client_info.description.as_ref().is_some_and(|description| description.len() > 1_024)
            || params.client_info.website_url.as_ref().is_some_and(|url| url.is_empty() || url.len() > 2_048)
            || params.client_info.icons.len() > 8
        {
            return rpc_error(id, -32602, "invalid params");
        }
        self.state = McpState::AwaitInitialized;
        rpc_result(id, json!({
            "protocolVersion": params.protocol_version,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION }
        }))
    }

    fn tools_list(&self, id: Value, params: Option<Value>) -> Value {
        if self.state != McpState::Ready {
            return rpc_error(id, -32002, "server not initialized");
        }
        let list_params: ListToolsParams = match params {
            None | Some(Value::Null) => ListToolsParams { cursor: None, meta: None },
            Some(value) => match serde_json::from_value(value) {
                Ok(params) => params,
                Err(_) => return rpc_error(id, -32602, "invalid params"),
            },
        };
        if list_params.cursor.is_some()
            || list_params.meta.as_ref().is_some_and(|meta| !meta.is_object())
        {
            return rpc_error(id, -32602, "invalid params");
        }
        let context = match self.backend.context_get() {
            Ok(context) => context,
            Err(_) => return rpc_error(id, -32603, "harness read unavailable"),
        };
        if context.validate().is_err() {
            return rpc_error(id, -32603, "harness read unavailable");
        }
        let tools = tool_definitions()
            .into_iter()
            .filter(|tool| context.allowed_tool_ids.iter().any(|allowed| allowed == tool["name"].as_str().unwrap_or_default()))
            .collect::<Vec<_>>();
        rpc_result(id, json!({ "tools": tools }))
    }

    fn tools_call(&self, id: Value, params: Option<Value>) -> Value {
        if self.state != McpState::Ready {
            return rpc_error(id, -32002, "server not initialized");
        }
        let params: ToolCallParams = match params.and_then(|value| serde_json::from_value(value).ok()) {
            Some(params) => params,
            None => return rpc_error(id, -32602, "invalid params"),
        };
        if params.meta.as_ref().is_some_and(|meta| !meta.is_object()) {
            return rpc_error(id, -32602, "invalid params");
        }
        let context = match self.backend.context_get() {
            Ok(context) => context,
            Err(_) => return rpc_error(id, -32603, "harness read unavailable"),
        };
        if context.validate().is_err() {
            return rpc_error(id, -32603, "harness read unavailable");
        }
        let name = strip_server_prefix(&params.name);
        if !context.allowed_tool_ids.iter().any(|allowed| allowed == name)
            || !(HARNESS_READ_TOOL_IDS.contains(&name) || HARNESS_WRITE_TOOL_IDS.contains(&name))
        {
            return rpc_error(id, -32601, "method not found");
        }
        let call = match parse_tool_call(name, params.arguments) {
            Ok(call) => call,
            Err(_) => return rpc_error(id, -32602, "invalid params"),
        };
        match self.backend.call(call) {
            Ok(response) => {
                if response.validate().is_err() {
                    return tool_error(id, "harness read unavailable");
                }
                let text = serde_json::to_string(&response).unwrap_or_else(|_| "null".to_owned());
                rpc_result(id, json!({ "content": [{ "type": "text", "text": text }], "isError": false }))
            }
            Err(HarnessMcpBackendError::NotFoundOrDenied) => tool_error(id, "not found or denied"),
            Err(HarnessMcpBackendError::InvalidRequest) => tool_error(id, "invalid request"),
            Err(HarnessMcpBackendError::Unauthorized | HarnessMcpBackendError::Unavailable) => {
                tool_error(id, "harness read unavailable")
            }
        }
    }
}

pub fn run_stdio<B: HarnessMcpBackend>(
    backend: B,
    reader: &mut impl BufRead,
    writer: &mut impl Write,
) -> Result<(), HarnessMcpIoError> {
    run_stdio_traced(backend, reader, writer, None)
}

/// A raw stdio observer for [`run_stdio_traced`]. Each method is handed
/// exactly the bytes the loop already read from, or is about to write to,
/// the peer -- the hook can observe the wire, never change it. Built for
/// [`HarnessMcpStdioTrace`]; nothing about `handle_line`'s behaviour,
/// timing, or the set of harness calls it makes changes when a trace is
/// attached.
pub trait HarnessMcpTrace {
    /// The raw inbound line, without its trailing newline, right after it is
    /// read off the peer and before it is handed to the server.
    fn on_inbound(&mut self, line: &[u8]);
    /// The raw outbound line, exactly as [`HarnessMcpServer::handle_line`]
    /// encoded it (its trailing newline included), right before it is
    /// written back to the peer.
    fn on_outbound(&mut self, line: &[u8]);
    /// Called once, when the peer closes its side of stdin.
    fn on_eof(&mut self);
}

/// Same loop as [`run_stdio`], plus an optional raw trace hook. `run_stdio`
/// is exactly this function called with `None` -- passing a hook can only
/// add observations, never alter what gets read, handled, or written.
pub fn run_stdio_traced<B: HarnessMcpBackend>(
    backend: B,
    reader: &mut impl BufRead,
    writer: &mut impl Write,
    mut trace: Option<&mut dyn HarnessMcpTrace>,
) -> Result<(), HarnessMcpIoError> {
    let mut server = HarnessMcpServer::new(backend);
    loop {
        let line = read_bounded_line(reader, HARNESS_READ_REQUEST_MAX_BYTES)?;
        let Some(line) = line else {
            if let Some(trace) = trace.as_deref_mut() { trace.on_eof(); }
            return Ok(());
        };
        if let Some(trace) = trace.as_deref_mut() { trace.on_inbound(&line); }
        if let Some(response) = server.handle_line(&line) {
            if let Some(trace) = trace.as_deref_mut() { trace.on_outbound(&response); }
            writer.write_all(&response).map_err(|_| HarnessMcpIoError::Output)?;
            writer.flush().map_err(|_| HarnessMcpIoError::Output)?;
        }
    }
}

/// An env-gated raw JSON-RPC stdio trace, appended to a file named by
/// [`HARNESS_MCP_TRACE_ENV`]. Every record is one line: a millisecond
/// Unix timestamp, a one-character marker (`<` inbound, `>` outbound, `#`
/// lifecycle), and a body. A traced line is bounded to 64 KiB (truncated
/// with `…`) and has any object field whose name looks like a credential
/// (contains `credential` or `token`, case-insensitively) replaced with
/// `<redacted>` -- this is a debugging artefact, and it must never carry
/// the reservation credential or session token that reach this helper only
/// through its own environment, never over stdio.
///
/// A write failure (a full disk, a file removed out from under the helper,
/// ...) is logged to stderr exactly once and then silently swallowed on
/// every later call: tracing is diagnostic, and must never be a reason the
/// stdio loop itself breaks.
pub struct HarnessMcpStdioTrace {
    file: std::fs::File,
    write_failed: bool,
}

const HARNESS_MCP_TRACE_LINE_MAX_BYTES: usize = 64 * 1024;
const HARNESS_MCP_TRACE_ELLIPSIS: &str = "\u{2026}";

impl HarnessMcpStdioTrace {
    /// Opens the file named by `HARNESS_MCP_TRACE_ENV`, if the variable is
    /// set, and appends a `# start` record naming `argv` and the names
    /// (never the values) of every `GATE4AGENT_*` environment variable.
    /// Returns `None` -- after logging once to stderr -- when the variable
    /// is unset or the file cannot be opened; the parent directory is never
    /// created.
    pub fn open_from_env(argv: &[std::ffi::OsString]) -> Option<Self> {
        let path = std::env::var_os(HARNESS_MCP_TRACE_ENV)?;
        let file = match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            Ok(file) => file,
            Err(error) => {
                eprintln!("gate4agent-harness-mcp: trace file unavailable: {error}");
                return None;
            }
        };
        let mut trace = Self { file, write_failed: false };
        trace.write_start(argv);
        Some(trace)
    }

    fn write_start(&mut self, argv: &[std::ffi::OsString]) {
        let pid = std::process::id();
        let argv_display = argv
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let mut env_keys = std::env::vars_os()
            .filter_map(|(key, _)| key.into_string().ok())
            .filter(|key| key.starts_with("GATE4AGENT_"))
            .collect::<Vec<_>>();
        env_keys.sort();
        let body = format!("start pid={pid} argv={argv_display:?} env_keys={env_keys:?}");
        self.write_record('#', body.as_bytes());
    }

    fn write_record(&mut self, marker: char, body: &[u8]) {
        let timestamp = unix_millis();
        let mut record = format!("{timestamp} {marker} ").into_bytes();
        record.extend_from_slice(body);
        record.push(b'\n');
        let outcome = self.file.write_all(&record).and_then(|_| self.file.flush());
        if outcome.is_err() && !self.write_failed {
            self.write_failed = true;
            eprintln!("gate4agent-harness-mcp: trace write failed");
        }
    }
}

impl HarnessMcpTrace for HarnessMcpStdioTrace {
    fn on_inbound(&mut self, line: &[u8]) {
        let body = prepare_trace_body(line);
        self.write_record('<', &body);
    }

    fn on_outbound(&mut self, line: &[u8]) {
        let body = prepare_trace_body(line);
        self.write_record('>', &body);
    }

    fn on_eof(&mut self) {
        self.write_record('#', b"eof");
    }
}

fn unix_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

fn prepare_trace_body(line: &[u8]) -> Vec<u8> {
    bound_trace_line(redact_credentials(strip_trailing_newline(line)))
}

fn strip_trailing_newline(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    if end > 0 && line[end - 1] == b'\n' { end -= 1; }
    if end > 0 && line[end - 1] == b'\r' { end -= 1; }
    &line[..end]
}

fn bound_trace_line(mut line: Vec<u8>) -> Vec<u8> {
    if line.len() > HARNESS_MCP_TRACE_LINE_MAX_BYTES {
        let ellipsis = HARNESS_MCP_TRACE_ELLIPSIS.as_bytes();
        let keep = HARNESS_MCP_TRACE_LINE_MAX_BYTES.saturating_sub(ellipsis.len());
        line.truncate(keep);
        line.extend_from_slice(ellipsis);
    }
    line
}

/// Best-effort credential redaction for one traced line. The primary path
/// parses the line as JSON (every line this helper reads or writes over
/// MCP is JSON-RPC) and blanks the value of any object field whose name
/// contains `credential` or `token`. A line that fails to parse (a
/// malformed probe, say) falls back to a coarse textual check: if it even
/// mentions one of those words, the whole line is replaced rather than
/// risk carrying an un-redacted value this fallback has no parse tree to
/// safely locate.
fn redact_credentials(line: &[u8]) -> Vec<u8> {
    match serde_json::from_slice::<Value>(line) {
        Ok(mut value) => {
            redact_credentials_in_place(&mut value);
            serde_json::to_vec(&value).unwrap_or_else(|_| b"<redacted: re-encode failed>".to_vec())
        }
        Err(_) => {
            let lowered = String::from_utf8_lossy(line).to_ascii_lowercase();
            if lowered.contains("credential") || lowered.contains("token") {
                b"<redacted: unparseable line named a credential-like field>".to_vec()
            } else {
                line.to_vec()
            }
        }
    }
}

fn redact_credentials_in_place(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, entry) in map.iter_mut() {
                let lowered_key = key.to_ascii_lowercase();
                if lowered_key.contains("credential") || lowered_key.contains("token") {
                    *entry = Value::String("<redacted>".to_owned());
                } else {
                    redact_credentials_in_place(entry);
                }
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() { redact_credentials_in_place(item); }
        }
        _ => {}
    }
}

pub fn client_from_env() -> Result<HarnessReadClient, HarnessMcpStartupError> {
    if std::env::var_os(HARNESS_SESSION_PROXY_ENDPOINT_ENV).is_some()
        || std::env::var_os(HARNESS_SESSION_PROXY_TOKEN_ENV).is_some()
    {
        return Err(HarnessMcpStartupError::Configuration);
    }
    let endpoint = std::env::var(HARNESS_MCP_ENDPOINT_ENV).map_err(|_| HarnessMcpStartupError::Configuration)?;
    let endpoint: SocketAddr = endpoint.parse().map_err(|_| HarnessMcpStartupError::Configuration)?;
    let credential = std::env::var(HARNESS_MCP_CREDENTIAL_ENV).map_err(|_| HarnessMcpStartupError::Configuration)?;
    let credential = HarnessReadCredential::parse(credential).map_err(|_| HarnessMcpStartupError::Configuration)?;
    HarnessReadClient::new(endpoint, credential).map_err(|_| HarnessMcpStartupError::Configuration)
}

pub const HARNESS_SESSION_PROXY_ENDPOINT_ENV: &str =
    "GATE4AGENT_HARNESS_SESSION_ENDPOINT";
pub const HARNESS_SESSION_PROXY_TOKEN_ENV: &str =
    "GATE4AGENT_HARNESS_SESSION_TOKEN";

pub fn session_proxy_client_from_env(
) -> Result<LocalSessionHarnessMcpClient, HarnessMcpStartupError> {
    proxy_client_from_values(
        std::env::var_os(HARNESS_SESSION_PROXY_ENDPOINT_ENV),
        std::env::var(HARNESS_SESSION_PROXY_TOKEN_ENV).ok(),
        std::env::var_os(HARNESS_MCP_ENDPOINT_ENV).is_some()
            || std::env::var_os(HARNESS_MCP_CREDENTIAL_ENV).is_some(),
    )
}

fn proxy_client_from_values(
    endpoint: Option<std::ffi::OsString>,
    token: Option<String>,
    legacy_h2_present: bool,
) -> Result<LocalSessionHarnessMcpClient, HarnessMcpStartupError> {
    if legacy_h2_present {
        return Err(HarnessMcpStartupError::Configuration);
    }
    let endpoint = endpoint.filter(|value| !value.is_empty())
        .ok_or(HarnessMcpStartupError::Configuration)?;
    let token = token.ok_or(HarnessMcpStartupError::Configuration)?;
    let token = gate4agent_node_protocol::HarnessMcpLocalToken::new(token)
        .map_err(|_| HarnessMcpStartupError::Configuration)?;
    LocalSessionHarnessMcpClient::new(
        std::path::PathBuf::from(endpoint),
        token,
    ).map_err(|_| HarnessMcpStartupError::Configuration)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RpcRequest {
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    params: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InitializeParams {
    protocol_version: String,
    capabilities: Value,
    client_info: ClientInfo,
    #[serde(rename = "_meta")]
    meta: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ClientInfo {
    name: String,
    title: Option<String>,
    description: Option<String>,
    version: String,
    website_url: Option<String>,
    #[serde(default)]
    icons: Vec<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolCallParams {
    name: String,
    #[serde(default = "empty_object")]
    arguments: Value,
    #[serde(rename = "_meta")]
    meta: Option<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListToolsParams {
    cursor: Option<String>,
    #[serde(rename = "_meta")]
    meta: Option<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyArgs {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MonitorArgs { run_id: Option<String> }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TimelineArgs { run_id: Option<String>, after_sequence: Option<u64>, #[serde(default = "default_limit")] limit: u16 }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TasksListArgs { after_task_id: Option<String>, state: Option<HarnessTaskStateV1>, #[serde(default = "default_limit")] limit: u16 }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskGetArgs { task_id: String }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunsListArgs {
    task_id: Option<String>,
    after_run_id: Option<String>,
    lifecycle: Option<HarnessRunLifecycleV1>,
    #[serde(default = "default_limit")]
    limit: u16,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunGetArgs { run_id: String }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OperationGetArgs { operation_id: String }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MailAddressArgs { session: Option<String>, task: Option<String> }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MailSendArgs {
    to: MailAddressArgs,
    subject: String,
    body: String,
    reply_to: Option<String>,
    #[serde(default)]
    refs: Vec<HarnessMailRefV1>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MailInboxArgs { since_unix_ms: Option<u64>, #[serde(default = "default_limit")] limit: u16 }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MailAckArgs { message_id: String }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MailFetchArgs { message_id: String, ref_index: u16 }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskCreateArgs {
    title: String,
    body: String,
    parent_task_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskMoveArgs {
    task_id: String,
    expected_revision: u64,
    to: HarnessTaskStateV1,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunFinishArgs {
    outcome: HarnessRunFinishOutcomeV1,
    summary: Option<String>,
}

fn parse_mail_address(args: MailAddressArgs) -> Result<HarnessMailAddressV1, ()> {
    match (args.session, args.task) {
        (Some(record_id), None) => Ok(HarnessMailAddressV1::Session {
            record_id: HarnessSelectorV1::new(record_id).map_err(|_| ())?,
        }),
        (None, Some(task_id)) => Ok(HarnessMailAddressV1::Task {
            task_id: HarnessTaskId::new(task_id).map_err(|_| ())?,
        }),
        _ => Err(()),
    }
}

/// Strip a leading MCP server-qualifier off a `tools/call` name, so a
/// server-qualified id resolves to the bare id `tools/list` actually
/// advertises and this module matches against everywhere else.
///
/// We advertise the MCP server as `"gate4agent"` and bare tool ids (e.g.
/// `g4a_context_get`). codex, kimi, and claude forward that bare id
/// unchanged. grok's third-party CLI instead forwards a server-qualified
/// name -- `gate4agent__g4a_context_get`, or `mcp__gate4agent__g4a_context_get`
/// -- which a strict-equality match against the bare id would reject with
/// "method not found". The longer prefix is checked first so a name that
/// happens to carry both is not left half-stripped; a name with neither
/// prefix (the bare-id case) is returned unchanged.
fn strip_server_prefix(name: &str) -> &str {
    const MCP_QUALIFIED_PREFIX: &str = "mcp__gate4agent__";
    const SERVER_QUALIFIED_PREFIX: &str = "gate4agent__";
    name.strip_prefix(MCP_QUALIFIED_PREFIX)
        .or_else(|| name.strip_prefix(SERVER_QUALIFIED_PREFIX))
        .unwrap_or(name)
}

fn parse_tool_call(name: &str, arguments: Value) -> Result<HarnessMcpToolCall, ()> {
    match name {
        "g4a_context_get" => {
            serde_json::from_value::<EmptyArgs>(arguments).map_err(|_| ())?;
            Ok(HarnessMcpToolCall::ContextGet)
        }
        "g4a_monitor_get" => {
            let args: MonitorArgs = serde_json::from_value(arguments).map_err(|_| ())?;
            Ok(HarnessMcpToolCall::MonitorGet { run_id: parse_optional(args.run_id, HarnessRunId::new)? })
        }
        "g4a_timeline_read" => {
            let args: TimelineArgs = serde_json::from_value(arguments).map_err(|_| ())?;
            if !(1..=128).contains(&args.limit) || args.after_sequence == Some(0) { return Err(()); }
            Ok(HarnessMcpToolCall::TimelineRead {
                run_id: parse_optional(args.run_id, HarnessRunId::new)?,
                after_sequence: args.after_sequence,
                limit: args.limit,
            })
        }
        "g4a_tasks_list" => {
            let args: TasksListArgs = serde_json::from_value(arguments).map_err(|_| ())?;
            if !(1..=64).contains(&args.limit) { return Err(()); }
            Ok(HarnessMcpToolCall::TasksList {
                after_task_id: parse_optional(args.after_task_id, HarnessTaskId::new)?,
                state: args.state,
                limit: args.limit,
            })
        }
        "g4a_tasks_get" => {
            let args: TaskGetArgs = serde_json::from_value(arguments).map_err(|_| ())?;
            Ok(HarnessMcpToolCall::TaskGet { task_id: HarnessTaskId::new(args.task_id).map_err(|_| ())? })
        }
        "g4a_runs_list" => {
            let args: RunsListArgs = serde_json::from_value(arguments).map_err(|_| ())?;
            if !(1..=64).contains(&args.limit) { return Err(()); }
            Ok(HarnessMcpToolCall::RunsList {
                task_id: parse_optional(args.task_id, HarnessTaskId::new)?,
                after_run_id: parse_optional(args.after_run_id, HarnessRunId::new)?,
                lifecycle: args.lifecycle,
                limit: args.limit,
            })
        }
        "g4a_runs_get" => {
            let args: RunGetArgs = serde_json::from_value(arguments).map_err(|_| ())?;
            Ok(HarnessMcpToolCall::RunGet { run_id: HarnessRunId::new(args.run_id).map_err(|_| ())? })
        }
        "g4a_operation_get" => {
            let args: OperationGetArgs = serde_json::from_value(arguments).map_err(|_| ())?;
            Ok(HarnessMcpToolCall::OperationGet { operation_id: HarnessOperationId::new(args.operation_id).map_err(|_| ())? })
        }
        "g4a_mail_send" => {
            let args: MailSendArgs = serde_json::from_value(arguments).map_err(|_| ())?;
            if args.refs.len() > HARNESS_MAIL_REFS_MAX { return Err(()); }
            Ok(HarnessMcpToolCall::MailSend {
                to: parse_mail_address(args.to)?,
                subject: args.subject,
                body: args.body,
                reply_to: parse_optional(args.reply_to, HarnessMailMessageId::new)?,
                refs: args.refs,
            })
        }
        "g4a_mail_inbox" => {
            let args: MailInboxArgs = serde_json::from_value(arguments).map_err(|_| ())?;
            if !(1..=64).contains(&args.limit) { return Err(()); }
            Ok(HarnessMcpToolCall::MailInbox { since_unix_ms: args.since_unix_ms, limit: args.limit })
        }
        "g4a_mail_ack" => {
            let args: MailAckArgs = serde_json::from_value(arguments).map_err(|_| ())?;
            Ok(HarnessMcpToolCall::MailAck { message_id: HarnessMailMessageId::new(args.message_id).map_err(|_| ())? })
        }
        "g4a_mail_fetch" => {
            let args: MailFetchArgs = serde_json::from_value(arguments).map_err(|_| ())?;
            if usize::from(args.ref_index) >= HARNESS_MAIL_REFS_MAX { return Err(()); }
            Ok(HarnessMcpToolCall::MailFetch {
                message_id: HarnessMailMessageId::new(args.message_id).map_err(|_| ())?,
                ref_index: args.ref_index,
            })
        }
        "g4a_task_create" => {
            let args: TaskCreateArgs = serde_json::from_value(arguments).map_err(|_| ())?;
            Ok(HarnessMcpToolCall::TaskCreate {
                title: args.title,
                body: args.body,
                parent_task_id: parse_optional(args.parent_task_id, HarnessTaskId::new)?,
            })
        }
        "g4a_task_move" => {
            let args: TaskMoveArgs = serde_json::from_value(arguments).map_err(|_| ())?;
            Ok(HarnessMcpToolCall::TaskMove {
                task_id: HarnessTaskId::new(args.task_id).map_err(|_| ())?,
                expected_revision: HarnessRevision::new(args.expected_revision).map_err(|_| ())?,
                to: args.to,
            })
        }
        "g4a_run_finish" => {
            let args: RunFinishArgs = serde_json::from_value(arguments).map_err(|_| ())?;
            Ok(HarnessMcpToolCall::RunFinish { outcome: args.outcome, summary: args.summary })
        }
        _ => Err(()),
    }
}

fn parse_optional<T>(value: Option<String>, parse: impl FnOnce(String) -> Result<T, gate4agent_harness_client::HarnessValidationError>) -> Result<Option<T>, ()> {
    value.map(|value| parse(value).map_err(|_| ())).transpose()
}

fn tool_definitions() -> Vec<Value> {
    vec![
        tool("g4a_context_get", "Read the effective harness grant context.", object_schema(vec![], vec![])),
        tool("g4a_monitor_get", "Read the redacted monitoring projection.", object_schema(vec![("run_id", string_schema())], vec![])),
        tool("g4a_timeline_read", "Read a bounded redacted activity timeline.", object_schema(vec![("run_id", string_schema()), ("after_sequence", integer_schema(1, u64::MAX)), ("limit", integer_schema(1, 128))], vec![])),
        tool("g4a_tasks_list", "List visible harness tasks.", object_schema(vec![("after_task_id", string_schema()), ("state", enum_schema(&["backlog","ready","running","waiting","review","done","failed","cancelled"])), ("limit", integer_schema(1, 64))], vec![])),
        tool("g4a_tasks_get", "Read one visible harness task.", object_schema(vec![("task_id", string_schema())], vec!["task_id"])),
        tool("g4a_runs_list", "List visible harness runs.", object_schema(vec![("task_id", string_schema()), ("after_run_id", string_schema()), ("lifecycle", enum_schema(&["requested","preparing","dispatching","outcome-unknown","running","waiting","completed","failed","cancelled"])), ("limit", integer_schema(1, 64))], vec![])),
        tool("g4a_runs_get", "Read one visible harness run.", object_schema(vec![("run_id", string_schema())], vec!["run_id"])),
        tool("g4a_operation_get", "Read one visible harness operation.", object_schema(vec![("operation_id", string_schema())], vec!["operation_id"])),
        // Ordered to match `HARNESS_WRITE_TOOL_IDS` (alphabetical), the same
        // way the eight reads above are ordered to match `HARNESS_READ_TOOL_IDS`
        // -- `all_eight_tool_schemas_are_stable_and_closed` zips both.
        tool_with_hints(
            "g4a_mail_ack",
            "Acknowledge one mail message. Idempotent: acking an already-acked message succeeds again.",
            object_schema(vec![("message_id", string_schema())], vec!["message_id"]),
            false,
            true,
        ),
        tool_with_hints(
            "g4a_mail_fetch",
            "Dereference one ref attached to a mail message already in this session's inbox. Authorized by message membership, not by this grant's own lineage. A Run ref returns a monitor projection plus a timeline page; a Result ref returns the redacted run record; a ContextPack ref returns its digest, id, byte_len, and its bytes as lower-hex (bytes_hex, exactly byte_len * 2 hex characters); a WorkspacePath ref returns its workspace, path, byte_len, bare-hex sha256, and its UTF-8 text verbatim; anything else refuses by name.",
            object_schema(vec![
                ("message_id", string_schema()),
                ("ref_index", integer_schema(0, HARNESS_MAIL_REFS_MAX as u64 - 1)),
            ], vec!["message_id", "ref_index"]),
            true,
            true,
        ),
        tool_with_hints(
            "g4a_mail_inbox",
            "Read waiting mail addressed to this session or to a task it can see.",
            object_schema(vec![
                ("since_unix_ms", integer_schema(1, u64::MAX)),
                ("limit", integer_schema(1, 64)),
            ], vec![]),
            true,
            true,
        ),
        tool_with_hints(
            "g4a_mail_send",
            "Send mail to a session record or a task's forum. Returns the queued message id or the engine's named refusal.",
            object_schema(vec![
                ("to", mail_address_schema()),
                ("subject", title_schema()),
                ("body", body_schema()),
                ("reply_to", string_schema()),
                ("refs", mail_refs_schema()),
            ], vec!["to", "subject", "body"]),
            false,
            false,
        ),
        tool_with_hints(
            "g4a_run_finish",
            "Report this session's OWN run finished -- there is no run-id argument, only the caller's own run. `done` moves the run to Completed and its task to Review (never Done -- an operator must still review it); `failed` moves both to Failed and records a retryable failure. Not idempotent: a second call against an already-finished run is refused by name as already-finished, not repeated as a no-op. `summary`, when given, is posted as mail to the task's own forum; a mail failure never undoes the finish.",
            object_schema(vec![
                ("outcome", enum_schema(&["done", "failed"])),
                ("summary", body_schema()),
            ], vec!["outcome"]),
            false,
            false,
        ),
        tool_with_hints(
            "g4a_task_create",
            "Create a task under this session's own task, or under one of its strict descendants (parent_task_id, defaults to this session's own task). Refuses by name if the parent is outside this session's own subtree or already terminal.",
            object_schema(vec![
                ("title", title_schema()),
                ("body", body_schema()),
                ("parent_task_id", string_schema()),
            ], vec!["title", "body"]),
            false,
            false,
        ),
        tool_with_hints(
            "g4a_task_move",
            "Move a task that is a strict descendant of this session's own task to a new state (never this session's own task -- refused by name as task-is-own). Not idempotent: replaying the same call after it already moved the task is refused by name as a revision conflict, not repeated as a no-op.",
            object_schema(vec![
                ("task_id", string_schema()),
                ("expected_revision", integer_schema(1, u64::MAX)),
                ("to", enum_schema(&["backlog","ready","running","waiting","review","done","failed","cancelled"])),
            ], vec!["task_id", "expected_revision", "to"]),
            false,
            false,
        ),
    ]
}

fn tool(name: &str, description: &str, input_schema: Value) -> Value {
    tool_with_hints(name, description, input_schema, true, true)
}

fn tool_with_hints(
    name: &str,
    description: &str,
    input_schema: Value,
    read_only: bool,
    idempotent: bool,
) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": input_schema,
        "annotations": {
            "readOnlyHint": read_only,
            "destructiveHint": false,
            "idempotentHint": idempotent,
            "openWorldHint": false
        }
    })
}

fn object_schema(properties: Vec<(&str, Value)>, required: Vec<&str>) -> Value {
    let properties = properties.into_iter().map(|(name, schema)| (name.to_owned(), schema)).collect::<serde_json::Map<_, _>>();
    json!({ "type": "object", "properties": properties, "required": required, "additionalProperties": false })
}

fn string_schema() -> Value { json!({ "type": "string", "minLength": 1, "maxLength": 128 }) }
fn integer_schema(minimum: u64, maximum: u64) -> Value { json!({ "type": "integer", "minimum": minimum, "maximum": maximum }) }
fn enum_schema(values: &[&str]) -> Value { json!({ "type": "string", "enum": values }) }
fn title_schema() -> Value { json!({ "type": "string", "minLength": 1, "maxLength": 256 }) }
fn body_schema() -> Value { json!({ "type": "string", "maxLength": 8_192 }) }
fn mail_address_schema() -> Value {
    json!({
        "type": "object",
        "properties": { "session": string_schema(), "task": string_schema() },
        "additionalProperties": false
    })
}
/// A ref field's own text can run longer than `string_schema`'s 128-byte
/// tool-argument convention (a workspace-relative `path`, in particular).
/// This is advisory only -- `HarnessMailRefV1::validate()` (harness-protocol)
/// is the authoritative bound each field is actually checked against; this
/// schema only needs to not reject something that would otherwise validate.
fn mail_ref_text_schema() -> Value { json!({ "type": "string", "minLength": 1, "maxLength": 2_048 }) }

/// One `HarnessMailRefV1`, deserialized straight off the protocol's own
/// `#[serde(tag = "kind")]` shape (D3): four fixed kinds, so a `oneOf` over
/// their exact shapes is closed and stable, unlike `mail_address_schema`'s
/// deliberately simplified two-key surface (that address has only two
/// shapes and no internal tag to begin with).
fn mail_ref_schema() -> Value {
    json!({
        "type": "object",
        "oneOf": [
            {
                "properties": { "kind": { "const": "run" }, "run_id": mail_ref_text_schema() },
                "required": ["kind", "run_id"],
                "additionalProperties": false,
            },
            {
                "properties": { "kind": { "const": "context-pack" }, "digest": mail_ref_text_schema() },
                "required": ["kind", "digest"],
                "additionalProperties": false,
            },
            {
                "properties": { "kind": { "const": "result" }, "result_ref": mail_ref_text_schema() },
                "required": ["kind", "result_ref"],
                "additionalProperties": false,
            },
            {
                "properties": {
                    "kind": { "const": "workspace-path" },
                    "workspace": mail_ref_text_schema(),
                    "path": mail_ref_text_schema(),
                    "sha256": mail_ref_text_schema(),
                },
                "required": ["kind", "workspace", "path", "sha256"],
                "additionalProperties": false,
            },
        ],
    })
}
fn mail_refs_schema() -> Value {
    json!({ "type": "array", "items": mail_ref_schema(), "maxItems": HARNESS_MAIL_REFS_MAX })
}
fn empty_object() -> Value { json!({}) }
fn default_limit() -> u16 { DEFAULT_LIMIT }

fn metadata_only_params(params: Option<&Value>) -> bool {
    match params {
        None | Some(Value::Null) => true,
        Some(Value::Object(object)) if object.is_empty() => true,
        Some(Value::Object(object)) if object.len() == 1 => {
            object.get("_meta").is_some_and(Value::is_object)
        }
        _ => false,
    }
}

fn valid_id(id: &Option<Value>) -> bool {
    id.as_ref().map_or(true, |id| id.is_string() || id.as_i64().is_some() || id.as_u64().is_some())
}

fn rpc_result(id: Value, result: Value) -> Value { json!({ "jsonrpc": JSONRPC_VERSION, "id": id, "result": result }) }
fn rpc_error(id: Value, code: i64, message: &str) -> Value { json!({ "jsonrpc": JSONRPC_VERSION, "id": id, "error": { "code": code, "message": message } }) }
fn tool_error(id: Value, message: &str) -> Value {
    rpc_result(id, json!({ "content": [{ "type": "text", "text": message }], "isError": true }))
}

fn read_bounded_line(reader: &mut impl BufRead, maximum: usize) -> Result<Option<Vec<u8>>, HarnessMcpIoError> {
    let mut line = Vec::with_capacity(4096);
    loop {
        let available = reader.fill_buf().map_err(|_| HarnessMcpIoError::Input)?;
        if available.is_empty() {
            return if line.is_empty() { Ok(None) } else { Err(HarnessMcpIoError::Input) };
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |position| position + 1);
        if line.len().saturating_add(consumed) > maximum {
            return Err(HarnessMcpIoError::InputTooLarge);
        }
        line.extend_from_slice(&available[..consumed]);
        reader.consume(consumed);
        if newline.is_some() {
            line.pop();
            if line.last() == Some(&b'\r') { line.pop(); }
            return Ok(Some(line));
        }
    }
}

#[derive(Debug, Error)]
pub enum HarnessMcpIoError {
    #[error("MCP input failed")]
    Input,
    #[error("MCP input exceeded the bound")]
    InputTooLarge,
    #[error("MCP output failed")]
    Output,
}

#[derive(Debug, Error)]
pub enum HarnessMcpStartupError {
    #[error("MCP configuration is unavailable")]
    Configuration,
}

#[cfg(test)]
mod tests {
    use super::*;
    use gate4agent_harness_client::{
        CallerRunV1, HarnessExecutionModeV1, HarnessMonitoringVisibilityV1,
        HarnessReadPermissionsV1, HarnessResultDispositionV1, HarnessRevision,
        HarnessEntityReadScopeV1, RedactedBindingStateV1, RedactedRunIntentV1,
        RedactedRunV1, RedactedTaskV1, RedactedWorktreeIntentV1, TaskCreatorCategoryV1,
    };
    use std::cell::{Cell, RefCell};

    struct FixtureBackend {
        allowed: RefCell<Vec<String>>,
        calls: Cell<u32>,
        call_error: Cell<Option<HarnessMcpBackendError>>,
        tasks_enabled: Cell<bool>,
        own_task_present: Cell<bool>,
        mail_enabled: Cell<bool>,
    }

    /// The calling run's own task, returned only when `own_task_present` is set.
    fn sample_redacted_task() -> RedactedTaskV1 {
        RedactedTaskV1 {
            task_id: HarnessTaskId::new("htask_000000000000000000000001").unwrap(),
            revision: HarnessRevision::new(1).unwrap(),
            title: "fixture task".to_owned(),
            body: "fixture body".to_owned(),
            creator: TaskCreatorCategoryV1::User,
            parent_task_id: None,
            dependency_ids: Vec::new(),
            state: HarnessTaskStateV1::Running,
            run_ids: Vec::new(),
            references_redacted: false,
            result_refs: Vec::new(),
            artifact_refs: Vec::new(),
            created_at_unix_ms: 10,
            updated_at_unix_ms: 10,
        }
    }

    /// A prior run of `sample_redacted_task`, exercising the `sibling_runs`
    /// pass-through end to end over the MCP wire.
    fn sample_sibling_run() -> RedactedRunV1 {
        RedactedRunV1 {
            run_id: HarnessRunId::new("hrun_000000000000000000000002").unwrap(),
            revision: HarnessRevision::new(1).unwrap(),
            parent_run_id: None,
            task_id: Some(HarnessTaskId::new("htask_000000000000000000000001").unwrap()),
            operation_id: None,
            intent: RedactedRunIntentV1 {
                mode: HarnessExecutionModeV1::Pty,
                worktree: RedactedWorktreeIntentV1::Existing,
                has_delivery_bundle: false,
                has_continuation: false,
            },
            lifecycle: HarnessRunLifecycleV1::Completed,
            binding: RedactedBindingStateV1::ManagedDormant,
            result_disposition: Some(HarnessResultDispositionV1::Succeeded),
            failure_category: None,
            context_pack: None,
            git_facts: None,
            references_redacted: false,
            created_at_unix_ms: 10,
            updated_at_unix_ms: 10,
        }
    }

    impl FixtureBackend {
        fn context(&self) -> SessionContextV1 {
            let task = self.own_task_present.get().then(sample_redacted_task);
            let sibling_runs = if task.is_some() { vec![sample_sibling_run()] } else { Vec::new() };
            SessionContextV1 {
                grant_id: gate4agent_harness_client::SessionGrantId::new(
                    "hgrant_000000000000000000000001",
                )
                .unwrap(),
                grant_revision: HarnessRevision::new(1).unwrap(),
                actor_run: CallerRunV1 {
                    run_id: HarnessRunId::new("hrun_000000000000000000000001").unwrap(),
                    task_id: task.as_ref().map(|task| task.task_id.clone()),
                    parent_run_id: None,
                    lifecycle: HarnessRunLifecycleV1::Running,
                    references_redacted: true,
                },
                task,
                sibling_runs,
                read_permissions: HarnessReadPermissionsV1 {
                    tasks: if self.tasks_enabled.get() {
                        HarnessEntityReadScopeV1::SelfOnly
                    } else {
                        HarnessEntityReadScopeV1::None
                    },
                    runs: HarnessEntityReadScopeV1::None,
                    operations: HarnessEntityReadScopeV1::None,
                },
                monitoring_visibility: HarnessMonitoringVisibilityV1::None,
                maximum_child_count: 0,
                maximum_child_depth: 0,
                mail: self.mail_enabled.get(),
                task_create: false,
                task_mutate: false,
                allowed_tool_ids: self.allowed.borrow().clone(),
                history_message_count: None,
                completed_turn_count: None,
                total_tokens: None,
            }
        }
    }

    impl HarnessMcpBackend for FixtureBackend {
        fn context_get(&self) -> Result<SessionContextV1, HarnessMcpBackendError> {
            Ok(self.context())
        }

        fn call(&self, call: HarnessMcpToolCall) -> Result<HarnessReadResponseV1, HarnessMcpBackendError> {
            self.calls.set(self.calls.get() + 1);
            if let Some(error) = self.call_error.get() { return Err(error); }
            match call {
                HarnessMcpToolCall::ContextGet => Ok(HarnessReadResponseV1::Context(self.context())),
                _ => Err(HarnessMcpBackendError::NotFoundOrDenied),
            }
        }
    }

    fn fixture() -> FixtureBackend {
        FixtureBackend {
            // `g4a_run_finish` is unconditional (never gated by a grant
            // permission, see `HARNESS_WRITE_TOOL_IDS`'s own doc comment),
            // so it belongs in every fixture's `allowed_tool_ids` the same
            // way `g4a_context_get` already does -- `SessionContextV1::
            // validate` requires this list to equal exactly what
            // `expected_allowed_tool_ids` derives.
            allowed: RefCell::new(vec![
                "g4a_context_get".to_owned(),
                "g4a_run_finish".to_owned(),
                "g4a_tasks_get".to_owned(),
                "g4a_tasks_list".to_owned(),
            ]),
            calls: Cell::new(0),
            call_error: Cell::new(None),
            tasks_enabled: Cell::new(true),
            own_task_present: Cell::new(false),
            mail_enabled: Cell::new(false),
        }
    }

    fn request(server: &mut HarnessMcpServer<FixtureBackend>, value: Value) -> Value {
        let encoded = serde_json::to_vec(&value).unwrap();
        let response = server.handle_line(&encoded).expect("response");
        serde_json::from_slice(response.strip_suffix(b"\n").unwrap()).unwrap()
    }

    #[test]
    fn proxy_env_mode_is_bearerless_and_rejects_mixed_h2() {
        let endpoint = Some(std::ffi::OsString::from(r"\\.\pipe\gate4agent-hmcp-fixture"));
        let token = Some(format!("g4ah3_{}", "a".repeat(64)));
        assert!(proxy_client_from_values(endpoint.clone(), token.clone(), false).is_ok());
        assert!(matches!(
            proxy_client_from_values(endpoint, token, true),
            Err(HarnessMcpStartupError::Configuration),
        ));
    }

    fn initialize(server: &mut HarnessMcpServer<FixtureBackend>) {
        let response = request(server, json!({
            "jsonrpc":"2.0",
            "id":1,
            "method":"initialize",
            "params":{
                "protocolVersion":MCP_PROTOCOL_VERSION_CURRENT,
                "capabilities":{},
                "clientInfo":{"name":"fixture","version":"1"}
            }
        }));
        assert_eq!(response["result"]["protocolVersion"], MCP_PROTOCOL_VERSION_CURRENT);
        assert_eq!(response["result"]["capabilities"]["tools"]["listChanged"], false);
        let notification = serde_json::to_vec(&json!({
            "jsonrpc":"2.0",
            "method":"notifications/initialized",
            "params":{}
        })).unwrap();
        assert!(server.handle_line(&notification).is_none());
    }

    #[test]
    fn lifecycle_and_tools_list_are_grant_filtered_with_closed_schemas() {
        let mut server = HarnessMcpServer::new(fixture());
        initialize(&mut server);
        let listed = request(&mut server, json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}));
        let tools = listed["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 4);
        assert_eq!(tools[0]["name"], "g4a_context_get");
        assert_eq!(tools[1]["name"], "g4a_tasks_list");
        assert_eq!(tools[2]["name"], "g4a_tasks_get");
        // `g4a_run_finish` is unconditional, so it survives even the
        // narrowed re-list below -- last here because `tool_definitions()`
        // enumerates every read before any write, and `g4a_run_finish` is
        // the only write this narrowed grant still carries.
        assert_eq!(tools[3]["name"], "g4a_run_finish");
        assert!(tools.iter().all(|tool| tool["inputSchema"]["additionalProperties"] == false));

        server.backend.allowed.replace(vec!["g4a_context_get".to_owned(), "g4a_run_finish".to_owned()]);
        server.backend.tasks_enabled.set(false);
        let relisted = request(&mut server, json!({"jsonrpc":"2.0","id":3,"method":"tools/list"}));
        let relisted_tools = relisted["result"]["tools"].as_array().unwrap();
        assert_eq!(relisted_tools.len(), 2);
        assert_eq!(relisted_tools[0]["name"], "g4a_context_get");
        assert_eq!(relisted_tools[1]["name"], "g4a_run_finish");
    }

    #[test]
    fn hidden_tools_are_method_not_found_and_object_denial_is_generic() {
        let mut server = HarnessMcpServer::new(fixture());
        initialize(&mut server);
        let hidden = request(&mut server, json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"g4a_monitor_get","arguments":{}}
        }));
        assert_eq!(hidden["error"]["code"], -32601);
        assert_eq!(server.backend.calls.get(), 0);

        server.backend.call_error.set(Some(HarnessMcpBackendError::NotFoundOrDenied));
        let denied = request(&mut server, json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"g4a_tasks_get","arguments":{"task_id":"htask_000000000000000000000099"}}
        }));
        assert_eq!(denied["result"]["isError"], true);
        assert_eq!(denied["result"]["content"][0]["text"], "not found or denied");
        assert!(!denied.to_string().contains("000000000099"));
    }

    #[test]
    fn unsupported_versions_batches_and_non_mcp_methods_fail_closed() {
        let mut server = HarnessMcpServer::new(fixture());
        let version = request(&mut server, json!({
            "jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}}
        }));
        assert_eq!(version["error"]["code"], -32602);

        let batch = request(&mut server, json!([]));
        assert_eq!(batch["error"]["code"], -32600);

        let mut server = HarnessMcpServer::new(fixture());
        initialize(&mut server);
        let resources = request(&mut server, json!({"jsonrpc":"2.0","id":4,"method":"resources/list"}));
        assert_eq!(resources["error"]["code"], -32601);
    }

    #[test]
    fn notifications_are_silent_ping_works_and_stdio_is_json_lines_only() {
        let mut server = HarnessMcpServer::new(fixture());
        let initialize_notification = serde_json::to_vec(&json!({
            "jsonrpc":"2.0","method":"initialize",
            "params":{"protocolVersion":MCP_PROTOCOL_VERSION_CURRENT,"capabilities":{},"clientInfo":{"name":"fixture","version":"1"}}
        })).unwrap();
        assert!(server.handle_line(&initialize_notification).is_none());
        let unknown_notification = serde_json::to_vec(&json!({"jsonrpc":"2.0","method":"notifications/unknown"})).unwrap();
        assert!(server.handle_line(&unknown_notification).is_none());

        initialize(&mut server);
        let ping = request(&mut server, json!({"jsonrpc":"2.0","id":9,"method":"ping","params":{}}));
        assert_eq!(ping["result"], json!({}));

        let input = [
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":MCP_PROTOCOL_VERSION_COMPATIBLE,"capabilities":{},"clientInfo":{"name":"fixture","title":"Fixture","version":"1","websiteUrl":"https://example.invalid","icons":[]},"_meta":{}}}),
            json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
            json!({"jsonrpc":"2.0","id":2,"method":"ping"}),
        ]
        .into_iter()
        .map(|value| serde_json::to_string(&value).unwrap())
        .collect::<Vec<_>>()
        .join("\n") + "\n";
        let mut reader = std::io::BufReader::new(input.as_bytes());
        let mut output = Vec::new();
        run_stdio(fixture(), &mut reader, &mut output).expect("stdio");
        let output = String::from_utf8(output).unwrap();
        let lines = output.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|line| serde_json::from_str::<Value>(line).is_ok()));
    }

    #[test]
    fn all_tool_schemas_are_stable_and_closed() {
        let tools = tool_definitions();
        assert_eq!(tools.len(), HARNESS_READ_TOOL_IDS.len() + HARNESS_WRITE_TOOL_IDS.len());
        let expected_names = HARNESS_READ_TOOL_IDS.iter().chain(HARNESS_WRITE_TOOL_IDS.iter());
        for (tool, expected_name) in tools.iter().zip(expected_names) {
            assert_eq!(tool["name"], *expected_name);
            assert_eq!(tool["inputSchema"]["type"], "object");
            assert_eq!(tool["inputSchema"]["additionalProperties"], false);
        }
    }

    /// D3/Slice C: `g4a_mail_fetch` parses a valid call, refuses an
    /// out-of-range `ref_index` before ever reaching the backend, and
    /// `g4a_mail_send` carries `refs` end to end (parse -> tool call ->
    /// wire request), all bounded by `HARNESS_MAIL_REFS_MAX`.
    #[test]
    fn mail_fetch_parses_and_rejects_out_of_range_index_before_the_backend() {
        let message_id = format!("hmail_{}", "1".repeat(24));
        let call = parse_tool_call(
            "g4a_mail_fetch",
            json!({ "message_id": message_id, "ref_index": 0 }),
        ).unwrap();
        assert_eq!(
            call,
            HarnessMcpToolCall::MailFetch {
                message_id: HarnessMailMessageId::new(format!("hmail_{}", "1".repeat(24))).unwrap(),
                ref_index: 0,
            },
        );
        assert!(parse_tool_call(
            "g4a_mail_fetch",
            json!({ "message_id": format!("hmail_{}", "1".repeat(24)), "ref_index": HARNESS_MAIL_REFS_MAX as u64 }),
        ).is_err());

        let run_id = format!("hrun_{}", "2".repeat(24));
        let send_call = parse_tool_call(
            "g4a_mail_send",
            json!({
                "to": { "session": "record-b" },
                "subject": "status",
                "body": "handing off",
                "refs": [{ "kind": "run", "run_id": run_id }],
            }),
        ).unwrap();
        match send_call {
            HarnessMcpToolCall::MailSend { refs, .. } => assert_eq!(refs.len(), 1),
            other => panic!("expected MailSend, got {other:?}"),
        }

        let too_many_refs = (0..HARNESS_MAIL_REFS_MAX + 1)
            .map(|_| json!({ "kind": "run", "run_id": format!("hrun_{}", "2".repeat(24)) }))
            .collect::<Vec<_>>();
        assert!(parse_tool_call(
            "g4a_mail_send",
            json!({
                "to": { "session": "record-b" },
                "subject": "status",
                "body": "handing off",
                "refs": too_many_refs,
            }),
        ).is_err());
    }

    /// S10: `g4a_run_finish` is listed with a closed schema naming `outcome`
    /// as the only required argument, its arguments parse into
    /// `HarnessMcpToolCall::RunFinish` with `summary` defaulting to `None`
    /// when omitted, and a bad `outcome` value is rejected at parse time
    /// (invalid params) before it ever reaches the backend.
    #[test]
    fn run_finish_tool_is_listed_parses_and_rejects_a_bad_outcome_at_parse_time() {
        let mut server = HarnessMcpServer::new(fixture());
        initialize(&mut server);
        let listed = request(&mut server, json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}));
        let tools = listed["result"]["tools"].as_array().unwrap();
        let run_finish = tools.iter()
            .find(|tool| tool["name"] == "g4a_run_finish")
            .expect("g4a_run_finish is listed");
        assert_eq!(run_finish["inputSchema"]["required"], json!(["outcome"]));
        assert_eq!(run_finish["inputSchema"]["properties"]["outcome"]["enum"], json!(["done", "failed"]));

        let call = parse_tool_call(
            "g4a_run_finish",
            json!({ "outcome": "done", "summary": "handed off cleanly" }),
        ).unwrap();
        assert_eq!(call, HarnessMcpToolCall::RunFinish {
            outcome: HarnessRunFinishOutcomeV1::Done,
            summary: Some("handed off cleanly".to_owned()),
        });

        let without_summary = parse_tool_call("g4a_run_finish", json!({ "outcome": "failed" })).unwrap();
        assert_eq!(without_summary, HarnessMcpToolCall::RunFinish {
            outcome: HarnessRunFinishOutcomeV1::Failed,
            summary: None,
        });

        assert!(parse_tool_call("g4a_run_finish", json!({ "outcome": "in-progress" })).is_err());

        let rejected = request(&mut server, json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"g4a_run_finish","arguments":{"outcome":"in-progress"}}
        }));
        assert_eq!(rejected["error"]["code"], -32602);
        assert_eq!(server.backend.calls.get(), 0);
    }

    /// D7: "not an authority channel, by construction and by test." No
    /// catalog id resembles `ResolveInteraction` (the sole approval-answering
    /// verb, operator-wire only) and `tools/call` refuses that exact name by
    /// name (-32601), the same closed-catalog path `hidden_tools_are_method_
    /// not_found_and_object_denial_is_generic` above already exercises for a
    /// legitimate-but-ungranted tool id.
    #[test]
    fn d7_no_tool_reaches_resolve_interaction_and_write_tool_ids_are_exactly_mail_task_and_run() {
        assert_eq!(HARNESS_WRITE_TOOL_IDS, [
            "g4a_mail_ack", "g4a_mail_fetch", "g4a_mail_inbox", "g4a_mail_send",
            "g4a_run_finish", "g4a_task_create", "g4a_task_move",
        ]);
        assert!(HARNESS_READ_TOOL_IDS.iter().chain(HARNESS_WRITE_TOOL_IDS.iter()).all(|id| {
            !id.contains("resolve_interaction") && !id.contains("resolve-interaction")
        }));

        let mut server = HarnessMcpServer::new(fixture());
        initialize(&mut server);
        let refused = request(&mut server, json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"g4a_resolve_interaction","arguments":{}}
        }));
        assert_eq!(refused["error"]["code"], -32601);
        assert_eq!(server.backend.calls.get(), 0);
    }

    #[test]
    fn strip_server_prefix_strips_the_longer_prefix_first_and_leaves_bare_names_alone() {
        assert_eq!(strip_server_prefix("mcp__gate4agent__g4a_mail_inbox"), "g4a_mail_inbox");
        assert_eq!(strip_server_prefix("gate4agent__g4a_context_get"), "g4a_context_get");
        assert_eq!(strip_server_prefix("g4a_context_get"), "g4a_context_get");
        assert_eq!(strip_server_prefix("foo__g4a_context_get"), "foo__g4a_context_get");
    }

    /// grok's third-party MCP client forwards a server-qualified tool name
    /// instead of the bare id `tools/list` advertises. `tools_call` must
    /// strip a leading `gate4agent__` or `mcp__gate4agent__` before matching
    /// against `allowed_tool_ids`/`HARNESS_READ_TOOL_IDS`/
    /// `HARNESS_WRITE_TOOL_IDS` and before `parse_tool_call`, so both
    /// prefixed spellings resolve exactly like the bare id -- while a bare
    /// id (codex/kimi/claude), an unrecognized prefix, and an unknown tool
    /// id all still behave exactly as before the shim.
    #[test]
    fn server_qualified_tool_names_resolve_to_the_bare_id() {
        let mut server = HarnessMcpServer::new(fixture());
        initialize(&mut server);
        // Turn on the mail grant (and off the unrelated tasks grant) so
        // `g4a_mail_inbox` joins `allowed_tool_ids` -- `SessionContextV1::
        // validate` requires that list to equal exactly the ids the
        // context's own flags derive, so it must carry every mail id once
        // `mail` is on, not just the one this test calls.
        server.backend.tasks_enabled.set(false);
        server.backend.mail_enabled.set(true);
        server.backend.allowed.replace(vec![
            "g4a_context_get".to_owned(),
            "g4a_mail_ack".to_owned(),
            "g4a_mail_fetch".to_owned(),
            "g4a_mail_inbox".to_owned(),
            "g4a_mail_send".to_owned(),
            "g4a_run_finish".to_owned(),
        ]);

        // A bare id is unaffected -- codex/kimi/claude's own shape.
        let bare = request(&mut server, json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"g4a_context_get","arguments":{}}
        }));
        assert_eq!(bare["result"]["isError"], false);

        // `gate4agent__g4a_context_get` resolves to `g4a_context_get` and is
        // served identically to the bare call above.
        let server_qualified = request(&mut server, json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"gate4agent__g4a_context_get","arguments":{}}
        }));
        assert_eq!(server_qualified["result"]["isError"], false);
        let text = server_qualified["result"]["content"][0]["text"].as_str().unwrap();
        let response: Value = serde_json::from_str(text).unwrap();
        assert_eq!(response["kind"], "context");

        // `mcp__gate4agent__g4a_mail_inbox` resolves to `g4a_mail_inbox` --
        // it clears the allow-list and tool-id checks and reaches the
        // backend (this fixture doesn't implement the call, so it comes
        // back as a business-level refusal, never a protocol-level
        // "method not found").
        let mcp_qualified = request(&mut server, json!({
            "jsonrpc":"2.0","id":4,"method":"tools/call",
            "params":{"name":"mcp__gate4agent__g4a_mail_inbox","arguments":{}}
        }));
        assert!(mcp_qualified.get("error").is_none());
        assert_eq!(mcp_qualified["result"]["isError"], true);
        assert_eq!(mcp_qualified["result"]["content"][0]["text"], "not found or denied");

        // An unrecognized prefix does not get stripped -- refused by name,
        // same as any other tool id absent from both allow-lists.
        let unknown_prefix = request(&mut server, json!({
            "jsonrpc":"2.0","id":5,"method":"tools/call",
            "params":{"name":"foo__g4a_context_get","arguments":{}}
        }));
        assert_eq!(unknown_prefix["error"]["code"], -32601);

        // A bare but unknown tool id is refused after stripping, same as
        // ever -- the shim never grants an id it wouldn't otherwise serve.
        let bogus = request(&mut server, json!({
            "jsonrpc":"2.0","id":6,"method":"tools/call",
            "params":{"name":"g4a_bogus","arguments":{}}
        }));
        assert_eq!(bogus["error"]["code"], -32601);
    }

    #[test]
    fn null_id_is_invalid_request_and_not_silent() {
        let mut server = HarnessMcpServer::new(fixture());
        let encoded = serde_json::to_vec(&json!({
            "jsonrpc":"2.0",
            "id":null,
            "method":"initialize",
            "params":{"protocolVersion":MCP_PROTOCOL_VERSION_CURRENT,"capabilities":{},"clientInfo":{"name":"fixture","version":"1"}}
        })).unwrap();
        let response = server.handle_line(&encoded).expect("explicit null id must respond");
        let response: Value = serde_json::from_slice(response.strip_suffix(b"\n").unwrap()).unwrap();
        assert_eq!(response["error"]["code"], -32600);
        assert!(server.state == McpState::AwaitInitialize);
    }

    #[test]
    fn metadata_objects_are_accepted_and_non_objects_do_not_mutate_state_or_call_backend() {
        let mut invalid = HarnessMcpServer::new(fixture());
        let _ = request(&mut invalid, json!({
            "jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocolVersion":MCP_PROTOCOL_VERSION_CURRENT,"capabilities":{},"clientInfo":{"name":"fixture","version":"1"}}
        }));
        let bad_initialized = serde_json::to_vec(&json!({
            "jsonrpc":"2.0","method":"notifications/initialized","params":{"_meta":"not-an-object"}
        })).unwrap();
        assert!(invalid.handle_line(&bad_initialized).is_none());
        assert!(invalid.state == McpState::AwaitInitialized);
        assert_eq!(invalid.backend.calls.get(), 0);

        let mut server = HarnessMcpServer::new(fixture());
        let _ = request(&mut server, json!({
            "jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocolVersion":MCP_PROTOCOL_VERSION_CURRENT,"capabilities":{},"clientInfo":{"name":"fixture","description":"fixture client","version":"1"}}
        }));
        let initialized = serde_json::to_vec(&json!({
            "jsonrpc":"2.0","method":"notifications/initialized","params":{"_meta":{}}
        })).unwrap();
        assert!(server.handle_line(&initialized).is_none());
        assert!(server.state == McpState::Ready);

        let ping = request(&mut server, json!({"jsonrpc":"2.0","id":2,"method":"ping","params":{"_meta":{}}}));
        assert_eq!(ping["result"], json!({}));
        let listed = request(&mut server, json!({"jsonrpc":"2.0","id":3,"method":"tools/list","params":{"_meta":{}}}));
        assert!(listed["result"]["tools"].is_array());
        let called = request(&mut server, json!({
            "jsonrpc":"2.0","id":4,"method":"tools/call",
            "params":{"name":"g4a_context_get","arguments":{},"_meta":{}}
        }));
        assert_eq!(called["result"]["isError"], false);
        assert_eq!(server.backend.calls.get(), 1);

        let rejected = request(&mut server, json!({
            "jsonrpc":"2.0","id":5,"method":"tools/call",
            "params":{"name":"g4a_context_get","arguments":{},"_meta":"not-an-object"}
        }));
        assert_eq!(rejected["error"]["code"], -32602);
        assert_eq!(server.backend.calls.get(), 1);
        assert!(server.state == McpState::Ready);
    }

    #[test]
    fn context_get_result_carries_own_task_and_sibling_runs_over_the_wire() {
        let mut server = HarnessMcpServer::new(fixture());
        initialize(&mut server);
        server.backend.own_task_present.set(true);

        let called = request(&mut server, json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"g4a_context_get","arguments":{}}
        }));
        assert_eq!(called["result"]["isError"], false);
        let text = called["result"]["content"][0]["text"].as_str().unwrap();
        let response: Value = serde_json::from_str(text).unwrap();
        assert_eq!(response["kind"], "context");
        let context = &response["value"];
        assert_eq!(context["actor_run"]["task_id"], "htask_000000000000000000000001");
        assert_eq!(context["task"]["task_id"], "htask_000000000000000000000001");
        assert_eq!(context["task"]["title"], "fixture task");
        assert_eq!(context["task"]["body"], "fixture body");
        let siblings = context["sibling_runs"].as_array().unwrap();
        assert_eq!(siblings.len(), 1);
        assert_eq!(siblings[0]["run_id"], "hrun_000000000000000000000002");
        assert_eq!(siblings[0]["lifecycle"], "completed");
        assert_eq!(siblings[0]["result_disposition"], "succeeded");

        // Without an own task, both fields stay empty -- no leakage by default.
        server.backend.own_task_present.set(false);
        let called = request(&mut server, json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"g4a_context_get","arguments":{}}
        }));
        let text = called["result"]["content"][0]["text"].as_str().unwrap();
        let response: Value = serde_json::from_str(text).unwrap();
        assert!(response["value"]["task"].is_null());
        assert_eq!(response["value"]["sibling_runs"], json!([]));
    }

    // -- G4A_HARNESS_MCP_TRACE ------------------------------------------

    static TRACE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Restores whatever `HARNESS_MCP_TRACE_ENV` held before the test (or
    /// clears it) on drop, the same `EnvironmentGuard` shape already used
    /// by this workspace's other env-mutating tests.
    struct TraceEnvGuard {
        previous: Option<std::ffi::OsString>,
    }

    impl TraceEnvGuard {
        fn set(path: &std::path::Path) -> Self {
            let previous = std::env::var_os(HARNESS_MCP_TRACE_ENV);
            std::env::set_var(HARNESS_MCP_TRACE_ENV, path);
            Self { previous }
        }
    }

    impl Drop for TraceEnvGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(previous) => std::env::set_var(HARNESS_MCP_TRACE_ENV, previous),
                None => std::env::remove_var(HARNESS_MCP_TRACE_ENV),
            }
        }
    }

    fn unique_temp_trace_path(label: &str) -> std::path::PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "g4a-harness-mcp-trace-{label}-{}-{n}.log",
            std::process::id()
        ))
    }

    #[test]
    fn stdio_trace_is_silent_without_the_env_var() {
        let _guard = TRACE_ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        std::env::remove_var(HARNESS_MCP_TRACE_ENV);
        let path = unique_temp_trace_path("absent");
        let _ = std::fs::remove_file(&path);

        let trace = HarnessMcpStdioTrace::open_from_env(&[std::ffi::OsString::from("gate4agent-harness-mcp")]);
        assert!(trace.is_none());
        assert!(!path.exists());
    }

    #[test]
    fn stdio_trace_records_wire_lines_and_redacts_credential_like_fields() {
        let _guard = TRACE_ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let path = unique_temp_trace_path("gated");
        let _ = std::fs::remove_file(&path);
        let _env = TraceEnvGuard::set(&path);

        let argv = vec![
            std::ffi::OsString::from("gate4agent-harness-mcp"),
            std::ffi::OsString::from("--session-proxy"),
        ];
        let mut trace = HarnessMcpStdioTrace::open_from_env(&argv).expect("trace file must open");

        let secret = "do-not-leak-this-token-value";
        let input = [
            json!({
                "jsonrpc":"2.0","id":1,"method":"initialize",
                "params":{
                    "protocolVersion": MCP_PROTOCOL_VERSION_CURRENT,
                    "capabilities":{},
                    "clientInfo":{"name":"fixture","version":"1"},
                    "_meta":{"token": secret}
                }
            }),
            json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
        ]
        .into_iter()
        .map(|value| serde_json::to_string(&value).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
            + "\n";
        let mut reader = std::io::BufReader::new(input.as_bytes());
        let mut output = Vec::new();
        run_stdio_traced(fixture(), &mut reader, &mut output, Some(&mut trace)).expect("stdio");
        drop(trace);

        let contents = std::fs::read_to_string(&path).expect("trace file readable");
        let lines = contents.lines().collect::<Vec<_>>();

        assert!(lines[0].contains("# start pid="));
        assert!(lines[0].contains("argv=[\"gate4agent-harness-mcp\", \"--session-proxy\"]"));

        let inbound = lines.iter().filter(|line| line.contains(" < ")).count();
        let outbound = lines.iter().filter(|line| line.contains(" > ")).count();
        assert_eq!(inbound, 3, "all three request lines were read: {lines:?}");
        assert_eq!(outbound, 2, "only the two requests (not the notification) get a reply: {lines:?}");

        assert!(!contents.contains(secret), "the traced file must never carry the credential-like value");
        assert!(contents.contains("\"token\":\"<redacted>\""), "the field must be redacted in place: {contents}");
        assert!(lines.last().is_some_and(|line| line.ends_with(" # eof")), "the last record marks stdin closing: {lines:?}");

        let _ = std::fs::remove_file(&path);
    }
}
