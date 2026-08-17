//! MCP stdio adapter for the grant-filtered harness read API.

use std::{io::{BufRead, Write}, net::SocketAddr};

use gate4agent_harness_client::{
    HarnessOperationId, HarnessReadClient, HarnessReadClientError, HarnessReadCredential,
    HarnessReadHostErrorV1, HarnessReadResponseV1, HarnessRunId, HarnessRunLifecycleV1,
    HarnessTaskId, HarnessTaskStateV1, SessionContextV1, HARNESS_READ_REQUEST_MAX_BYTES,
    HARNESS_READ_RESPONSE_MAX_BYTES, HARNESS_READ_TOOL_IDS,
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
        if !context.allowed_tool_ids.iter().any(|allowed| allowed == &params.name)
            || !HARNESS_READ_TOOL_IDS.contains(&params.name.as_str())
        {
            return rpc_error(id, -32601, "method not found");
        }
        let call = match parse_tool_call(&params.name, params.arguments) {
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
    let mut server = HarnessMcpServer::new(backend);
    loop {
        let line = read_bounded_line(reader, HARNESS_READ_REQUEST_MAX_BYTES)?;
        let Some(line) = line else { return Ok(()); };
        if let Some(response) = server.handle_line(&line) {
            writer.write_all(&response).map_err(|_| HarnessMcpIoError::Output)?;
            writer.flush().map_err(|_| HarnessMcpIoError::Output)?;
        }
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
    ]
}

fn tool(name: &str, description: &str, input_schema: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": input_schema,
        "annotations": {
            "readOnlyHint": true,
            "destructiveHint": false,
            "idempotentHint": true,
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
            allowed: RefCell::new(vec![
                "g4a_context_get".to_owned(),
                "g4a_tasks_get".to_owned(),
                "g4a_tasks_list".to_owned(),
            ]),
            calls: Cell::new(0),
            call_error: Cell::new(None),
            tasks_enabled: Cell::new(true),
            own_task_present: Cell::new(false),
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
        assert_eq!(tools.len(), 3);
        assert_eq!(tools[0]["name"], "g4a_context_get");
        assert_eq!(tools[1]["name"], "g4a_tasks_list");
        assert_eq!(tools[2]["name"], "g4a_tasks_get");
        assert!(tools.iter().all(|tool| tool["inputSchema"]["additionalProperties"] == false));

        server.backend.allowed.replace(vec!["g4a_context_get".to_owned()]);
        server.backend.tasks_enabled.set(false);
        let relisted = request(&mut server, json!({"jsonrpc":"2.0","id":3,"method":"tools/list"}));
        assert_eq!(relisted["result"]["tools"].as_array().unwrap().len(), 1);
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
    fn all_eight_tool_schemas_are_stable_and_closed() {
        let tools = tool_definitions();
        assert_eq!(tools.len(), HARNESS_READ_TOOL_IDS.len());
        for (tool, expected_name) in tools.iter().zip(HARNESS_READ_TOOL_IDS) {
            assert_eq!(tool["name"], expected_name);
            assert_eq!(tool["inputSchema"]["type"], "object");
            assert_eq!(tool["inputSchema"]["additionalProperties"], false);
        }
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
}
