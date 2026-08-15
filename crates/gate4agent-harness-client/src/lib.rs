//! Typed localhost client for the harness read host.
//!
//! The client implements the wire contract as one newline-terminated JSON
//! request per loopback connection, followed by `Shutdown::Write`; the host
//! replies only after that EOF request boundary.

use std::{
    io::{BufRead, BufReader, Write},
    net::{Shutdown, SocketAddr, TcpStream},
    time::Duration,
};

pub use gate4agent_harness_api::*;
use thiserror::Error;

pub const HARNESS_READ_DEADLINE: Duration = Duration::from_secs(3);
pub const HARNESS_OPERATOR_DEADLINE: Duration = Duration::from_secs(3);
pub const HARNESS_NATIVE_HISTORY_DEADLINE: Duration = Duration::from_secs(42);

#[derive(Clone, Debug)]
pub struct HarnessReadClient {
    endpoint: SocketAddr,
    credential: HarnessReadCredential,
    deadline: Duration,
}

impl HarnessReadClient {
    pub fn new(
        endpoint: SocketAddr,
        credential: HarnessReadCredential,
    ) -> Result<Self, HarnessReadClientError> {
        if !endpoint.ip().is_loopback() || endpoint.port() == 0 {
            return Err(HarnessReadClientError::NonLoopbackEndpoint);
        }
        Ok(Self { endpoint, credential, deadline: HARNESS_READ_DEADLINE })
    }

    pub fn context_get(&self) -> Result<SessionContextV1, HarnessReadClientError> {
        match self.send(HarnessReadRequestV1::ContextGet)? {
            HarnessReadResponseV1::Context(value) => Ok(value),
            _ => Err(HarnessReadClientError::UnexpectedResponse),
        }
    }

    pub fn monitor_get(
        &self,
        run_id: Option<HarnessRunId>,
    ) -> Result<SessionMonitorV1, HarnessReadClientError> {
        match self.send(HarnessReadRequestV1::MonitorGet { run_id })? {
            HarnessReadResponseV1::Monitor(value) => Ok(value),
            _ => Err(HarnessReadClientError::UnexpectedResponse),
        }
    }

    pub fn timeline_read(
        &self,
        run_id: Option<HarnessRunId>,
        after_sequence: Option<u64>,
        limit: u16,
    ) -> Result<TimelinePageV1, HarnessReadClientError> {
        match self.send(HarnessReadRequestV1::TimelineRead { run_id, after_sequence, limit })? {
            HarnessReadResponseV1::Timeline(value) => Ok(value),
            _ => Err(HarnessReadClientError::UnexpectedResponse),
        }
    }

    pub fn tasks_list(
        &self,
        after_task_id: Option<HarnessTaskId>,
        state: Option<HarnessTaskStateV1>,
        limit: u16,
    ) -> Result<TaskPageV1, HarnessReadClientError> {
        match self.send(HarnessReadRequestV1::TasksList { after_task_id, state, limit })? {
            HarnessReadResponseV1::Tasks(value) => Ok(value),
            _ => Err(HarnessReadClientError::UnexpectedResponse),
        }
    }

    pub fn task_get(&self, task_id: HarnessTaskId) -> Result<RedactedTaskV1, HarnessReadClientError> {
        match self.send(HarnessReadRequestV1::TaskGet { task_id })? {
            HarnessReadResponseV1::Task(value) => Ok(value),
            _ => Err(HarnessReadClientError::UnexpectedResponse),
        }
    }

    pub fn runs_list(
        &self,
        task_id: Option<HarnessTaskId>,
        after_run_id: Option<HarnessRunId>,
        lifecycle: Option<HarnessRunLifecycleV1>,
        limit: u16,
    ) -> Result<RunPageV1, HarnessReadClientError> {
        match self.send(HarnessReadRequestV1::RunsList { task_id, after_run_id, lifecycle, limit })? {
            HarnessReadResponseV1::Runs(value) => Ok(value),
            _ => Err(HarnessReadClientError::UnexpectedResponse),
        }
    }

    pub fn run_get(&self, run_id: HarnessRunId) -> Result<RedactedRunV1, HarnessReadClientError> {
        match self.send(HarnessReadRequestV1::RunGet { run_id })? {
            HarnessReadResponseV1::Run(value) => Ok(value),
            _ => Err(HarnessReadClientError::UnexpectedResponse),
        }
    }

    pub fn operation_get(
        &self,
        operation_id: HarnessOperationId,
    ) -> Result<RedactedOperationV1, HarnessReadClientError> {
        match self.send(HarnessReadRequestV1::OperationGet { operation_id })? {
            HarnessReadResponseV1::Operation(value) => Ok(value),
            _ => Err(HarnessReadClientError::UnexpectedResponse),
        }
    }

    fn send(&self, request: HarnessReadRequestV1) -> Result<HarnessReadResponseV1, HarnessReadClientError> {
        request.validate()?;
        let envelope = HarnessReadEnvelopeV1 {
            version: HARNESS_READ_WIRE_VERSION_V1,
            credential: self.credential.clone(),
            request,
        };
        envelope.validate()?;
        let mut encoded = serde_json::to_vec(&envelope).map_err(|_| HarnessReadClientError::Encoding)?;
        if encoded.len() >= HARNESS_READ_REQUEST_MAX_BYTES {
            return Err(HarnessReadClientError::RequestTooLarge);
        }
        encoded.push(b'\n');

        let mut stream = TcpStream::connect_timeout(&self.endpoint, self.deadline)
            .map_err(map_connect_error)?;
        stream.set_read_timeout(Some(self.deadline)).map_err(|_| HarnessReadClientError::Transport)?;
        stream.set_write_timeout(Some(self.deadline)).map_err(|_| HarnessReadClientError::Transport)?;
        stream.write_all(&encoded).map_err(map_io_error)?;
        stream.shutdown(Shutdown::Write).map_err(|_| HarnessReadClientError::Transport)?;

        let mut reader = BufReader::new(stream);
        let response = read_bounded_line(&mut reader, HARNESS_READ_RESPONSE_MAX_BYTES)?;
        let reply: HarnessReadReplyV1 =
            serde_json::from_slice(&response).map_err(|_| HarnessReadClientError::InvalidResponse)?;
        reply.validate()?;
        match reply {
            HarnessReadReplyV1::Ok { response } => Ok(response),
            HarnessReadReplyV1::Error { error } => Err(HarnessReadClientError::Host(error)),
        }
    }
}

#[derive(Clone, Debug)]
pub struct HarnessOperatorClient {
    endpoint: SocketAddr,
    credential: HarnessOperatorCredential,
    deadline: Duration,
}

impl HarnessOperatorClient {
    pub fn new(
        endpoint: SocketAddr,
        credential: HarnessOperatorCredential,
    ) -> Result<Self, HarnessOperatorClientError> {
        if !endpoint.ip().is_loopback() || endpoint.port() == 0 {
            return Err(HarnessOperatorClientError::NonLoopbackEndpoint);
        }
        Ok(Self { endpoint, credential, deadline: HARNESS_OPERATOR_DEADLINE })
    }

    pub fn monitor_get(
        &self,
        run_id: HarnessRunId,
    ) -> Result<SessionMonitorV1, HarnessOperatorClientError> {
        match self.send(HarnessOperatorRequestV1::MonitorGet { run_id })? {
            HarnessOperatorResponseV1::Monitor(value) => Ok(value),
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    pub fn timeline_read(
        &self,
        run_id: HarnessRunId,
        after_sequence: Option<u64>,
        limit: u16,
    ) -> Result<TimelinePageV1, HarnessOperatorClientError> {
        match self.send(HarnessOperatorRequestV1::TimelineRead {
            run_id,
            after_sequence,
            limit,
        })? {
            HarnessOperatorResponseV1::Timeline(value) => Ok(value),
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    pub fn tasks_list(
        &self,
        after_task_id: Option<HarnessTaskId>,
        state: Option<HarnessTaskStateV1>,
        limit: u16,
    ) -> Result<TaskPageV1, HarnessOperatorClientError> {
        match self.send(HarnessOperatorRequestV1::TasksList {
            after_task_id,
            state,
            limit,
        })? {
            HarnessOperatorResponseV1::Tasks(value) => Ok(value),
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    pub fn task_get(
        &self,
        task_id: HarnessTaskId,
    ) -> Result<RedactedTaskV1, HarnessOperatorClientError> {
        match self.send(HarnessOperatorRequestV1::TaskGet { task_id })? {
            HarnessOperatorResponseV1::Task(value) => Ok(value),
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    pub fn runs_list(
        &self,
        task_id: Option<HarnessTaskId>,
        after_run_id: Option<HarnessRunId>,
        lifecycle: Option<HarnessRunLifecycleV1>,
        limit: u16,
    ) -> Result<RunPageV1, HarnessOperatorClientError> {
        match self.send(HarnessOperatorRequestV1::RunsList {
            task_id,
            after_run_id,
            lifecycle,
            limit,
        })? {
            HarnessOperatorResponseV1::Runs(value) => Ok(value),
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    pub fn run_get(
        &self,
        run_id: HarnessRunId,
    ) -> Result<RedactedRunV1, HarnessOperatorClientError> {
        match self.send(HarnessOperatorRequestV1::RunGet { run_id })? {
            HarnessOperatorResponseV1::Run(value) => Ok(value),
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    pub fn runtime_inventory_list(
        &self,
        after_node_id: Option<String>,
        limit: u16,
    ) -> Result<HarnessRuntimeInventoryPageV1, HarnessOperatorClientError> {
        match self.send(HarnessOperatorRequestV1::RuntimeInventoryList {
            after_node_id,
            limit,
        })? {
            HarnessOperatorResponseV1::RuntimeInventory(value) => Ok(value),
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    pub fn catalog_native_sessions(
        &self,
        route: HarnessNativeSessionRouteV1,
        limit: u16,
    ) -> Result<HarnessNativeSessionsCatalogedV1, HarnessOperatorClientError> {
        match self.send(HarnessOperatorRequestV1::CatalogNativeSessions { route, limit })? {
            HarnessOperatorResponseV1::NativeSessionsCataloged(value) => Ok(value),
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    pub fn page_native_sessions(
        &self,
        route: HarnessNativeSessionRouteV1,
        window: HarnessNativeSessionCatalogWindowV1,
        catalog_revision: u64,
        recent_cutoff_unix_ms: u64,
        after_selection_id: Option<String>,
        limit: u16,
    ) -> Result<HarnessNativeSessionsPagedV1, HarnessOperatorClientError> {
        match self.send(HarnessOperatorRequestV1::PageNativeSessions {
            route,
            window,
            catalog_revision,
            recent_cutoff_unix_ms,
            after_selection_id,
            limit,
        })? {
            HarnessOperatorResponseV1::NativeSessionsPaged(value) => Ok(value),
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    pub fn preview_native_session(
        &self,
        selection: HarnessNativeSessionSelectionV1,
        message_limit: u16,
    ) -> Result<HarnessNativeSessionPreviewedV1, HarnessOperatorClientError> {
        match self.send(HarnessOperatorRequestV1::PreviewNativeSession {
            selection,
            message_limit,
        })? {
            HarnessOperatorResponseV1::NativeSessionPreviewed(value) => Ok(value),
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    pub fn submit_intent(
        &self,
        intent: HarnessOperatorIntentV1,
    ) -> Result<HarnessOperatorResponseV1, HarnessOperatorClientError> {
        self.send(HarnessOperatorRequestV1::SubmitIntent { intent })
    }

    pub fn create_task(
        &self,
        request: HarnessCreateTaskRequestV1,
    ) -> Result<HarnessOperatorMutationOutcomeV1, HarnessOperatorClientError> {
        self.send_mutation(HarnessOperatorRequestV1::CreateTask { request })
    }

    pub fn replace_task(
        &self,
        request: HarnessReplaceTaskRequestV1,
    ) -> Result<HarnessOperatorMutationOutcomeV1, HarnessOperatorClientError> {
        self.send_mutation(HarnessOperatorRequestV1::ReplaceTask { request })
    }

    pub fn move_task(
        &self,
        request: HarnessMoveTaskRequestV1,
    ) -> Result<HarnessOperatorMutationOutcomeV1, HarnessOperatorClientError> {
        self.send_mutation(HarnessOperatorRequestV1::MoveTask { request })
    }

    pub fn cancel_task(
        &self,
        request: HarnessCancelTaskRequestV1,
    ) -> Result<HarnessOperatorMutationOutcomeV1, HarnessOperatorClientError> {
        self.send_mutation(HarnessOperatorRequestV1::CancelTask { request })
    }

    pub fn retry_task(
        &self,
        request: HarnessRetryTaskRequestV1,
    ) -> Result<HarnessOperatorMutationOutcomeV1, HarnessOperatorClientError> {
        self.send_mutation(HarnessOperatorRequestV1::RetryTask { request })
    }

    pub fn schedule_next(
        &self,
        request: HarnessScheduleNextRequestV1,
    ) -> Result<HarnessScheduleOutcomeV1, HarnessOperatorClientError> {
        match self.send(HarnessOperatorRequestV1::ScheduleNext { request })? {
            HarnessOperatorResponseV1::Schedule(value) => Ok(value),
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    fn send_mutation(
        &self,
        request: HarnessOperatorRequestV1,
    ) -> Result<HarnessOperatorMutationOutcomeV1, HarnessOperatorClientError> {
        match self.send(request)? {
            HarnessOperatorResponseV1::Mutation(value) => Ok(value),
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    fn send(
        &self,
        request: HarnessOperatorRequestV1,
    ) -> Result<HarnessOperatorResponseV1, HarnessOperatorClientError> {
        let response_deadline = if matches!(
            &request,
            HarnessOperatorRequestV1::CatalogNativeSessions { .. }
                | HarnessOperatorRequestV1::PageNativeSessions { .. }
                | HarnessOperatorRequestV1::PreviewNativeSession { .. }
        ) {
            HARNESS_NATIVE_HISTORY_DEADLINE
        } else {
            self.deadline
        };
        request.validate()?;
        let version = if request.requires_v3() {
            HARNESS_OPERATOR_WIRE_VERSION_V3
        } else {
            HARNESS_OPERATOR_WIRE_VERSION_V2
        };
        let envelope = HarnessOperatorEnvelopeV1 {
            version,
            credential: self.credential.clone(),
            request,
        };
        envelope.validate()?;
        let mut encoded = serde_json::to_vec(&envelope)
            .map_err(|_| HarnessOperatorClientError::Encoding)?;
        if encoded.len() >= HARNESS_OPERATOR_REQUEST_MAX_BYTES {
            return Err(HarnessOperatorClientError::RequestTooLarge);
        }
        encoded.push(b'\n');

        let mut stream = TcpStream::connect_timeout(&self.endpoint, self.deadline)
            .map_err(map_operator_connect_error)?;
        stream.set_read_timeout(Some(response_deadline))
            .map_err(|_| HarnessOperatorClientError::Transport)?;
        stream.set_write_timeout(Some(self.deadline))
            .map_err(|_| HarnessOperatorClientError::Transport)?;
        stream.write_all(&encoded).map_err(map_operator_io_error)?;
        stream.shutdown(Shutdown::Write)
            .map_err(|_| HarnessOperatorClientError::Transport)?;

        let mut reader = BufReader::new(stream);
        let response = read_operator_bounded_line(
            &mut reader,
            HARNESS_OPERATOR_RESPONSE_MAX_BYTES,
        )?;
        let reply: HarnessOperatorReplyV1 = serde_json::from_slice(&response)
            .map_err(|_| HarnessOperatorClientError::InvalidResponse)?;
        reply.validate()?;
        match reply {
            HarnessOperatorReplyV1::Ok { response } => Ok(response),
            HarnessOperatorReplyV1::Error { error } => {
                Err(HarnessOperatorClientError::Host(error))
            }
        }
    }
}

fn read_operator_bounded_line(
    reader: &mut impl BufRead,
    max_bytes: usize,
) -> Result<Vec<u8>, HarnessOperatorClientError> {
    let mut response = Vec::with_capacity(4096);
    loop {
        let available = reader.fill_buf().map_err(map_operator_io_error)?;
        if available.is_empty() {
            return Err(if response.is_empty() {
                HarnessOperatorClientError::ConnectionClosed
            } else {
                HarnessOperatorClientError::IncompleteResponse
            });
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |position| position + 1);
        if response.len().saturating_add(consumed) > max_bytes {
            return Err(HarnessOperatorClientError::ResponseTooLarge);
        }
        response.extend_from_slice(&available[..consumed]);
        reader.consume(consumed);
        if newline.is_some() { break; }
    }
    response.pop();
    if response.last() == Some(&b'\r') { response.pop(); }
    if response.is_empty() {
        return Err(HarnessOperatorClientError::InvalidResponse);
    }
    Ok(response)
}

fn map_operator_connect_error(error: std::io::Error) -> HarnessOperatorClientError {
    if matches!(error.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) {
        HarnessOperatorClientError::Deadline
    } else {
        HarnessOperatorClientError::Unavailable
    }
}

fn map_operator_io_error(error: std::io::Error) -> HarnessOperatorClientError {
    if matches!(error.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) {
        HarnessOperatorClientError::Deadline
    } else {
        HarnessOperatorClientError::Transport
    }
}

#[derive(Debug, Error)]
pub enum HarnessOperatorClientError {
    #[error("harness operator endpoint must be a concrete loopback address")]
    NonLoopbackEndpoint,
    #[error("harness operator request is invalid")]
    Api(#[from] HarnessOperatorApiError),
    #[error("harness operator request encoding failed")]
    Encoding,
    #[error("harness operator request is too large")]
    RequestTooLarge,
    #[error("harness operator response is too large")]
    ResponseTooLarge,
    #[error("harness operator response is incomplete")]
    IncompleteResponse,
    #[error("harness operator response is invalid")]
    InvalidResponse,
    #[error("harness operator response type is unexpected")]
    UnexpectedResponse,
    #[error("harness operator host closed the connection")]
    ConnectionClosed,
    #[error("harness operator deadline exceeded")]
    Deadline,
    #[error("harness operator host is unavailable")]
    Unavailable,
    #[error("harness operator transport failed")]
    Transport,
    #[error("harness operator host rejected the request: {0:?}")]
    Host(HarnessOperatorHostErrorV1),
}

fn read_bounded_line(
    reader: &mut impl BufRead,
    max_bytes: usize,
) -> Result<Vec<u8>, HarnessReadClientError> {
    let mut response = Vec::with_capacity(4096);
    loop {
        let available = reader.fill_buf().map_err(map_io_error)?;
        if available.is_empty() {
            return Err(if response.is_empty() {
                HarnessReadClientError::ConnectionClosed
            } else {
                HarnessReadClientError::IncompleteResponse
            });
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |position| position + 1);
        if response.len().saturating_add(consumed) > max_bytes {
            return Err(HarnessReadClientError::ResponseTooLarge);
        }
        response.extend_from_slice(&available[..consumed]);
        reader.consume(consumed);
        if newline.is_some() { break; }
    }
    response.pop();
    if response.last() == Some(&b'\r') { response.pop(); }
    if response.is_empty() {
        return Err(HarnessReadClientError::InvalidResponse);
    }
    Ok(response)
}

fn map_connect_error(error: std::io::Error) -> HarnessReadClientError {
    if matches!(error.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) {
        HarnessReadClientError::Deadline
    } else {
        HarnessReadClientError::Unavailable
    }
}

fn map_io_error(error: std::io::Error) -> HarnessReadClientError {
    if matches!(error.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) {
        HarnessReadClientError::Deadline
    } else {
        HarnessReadClientError::Transport
    }
}

#[derive(Debug, Error)]
pub enum HarnessReadClientError {
    #[error("harness read endpoint must be a concrete loopback address")]
    NonLoopbackEndpoint,
    #[error("harness read request is invalid")]
    Api(#[from] HarnessReadApiError),
    #[error("harness read request encoding failed")]
    Encoding,
    #[error("harness read request is too large")]
    RequestTooLarge,
    #[error("harness read response is too large")]
    ResponseTooLarge,
    #[error("harness read response is incomplete")]
    IncompleteResponse,
    #[error("harness read response is invalid")]
    InvalidResponse,
    #[error("harness read response type is unexpected")]
    UnexpectedResponse,
    #[error("harness read host closed the connection")]
    ConnectionClosed,
    #[error("harness read deadline exceeded")]
    Deadline,
    #[error("harness read host is unavailable")]
    Unavailable,
    #[error("harness read transport failed")]
    Transport,
    #[error("harness read host rejected the request: {0:?}")]
    Host(HarnessReadHostErrorV1),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Read, net::TcpListener, thread};

    fn credential() -> HarnessReadCredential {
        HarnessReadCredential::parse(format!("g4ah2_aa.{}", "0".repeat(64))).expect("credential")
    }

    fn operator_credential() -> HarnessOperatorCredential {
        HarnessOperatorCredential::parse(format!("g4aho_{}", "a".repeat(64)))
            .expect("operator credential")
    }

    #[test]
    fn client_uses_one_bounded_ndjson_request_per_loopback_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let endpoint = listener.local_addr().expect("address");
        let host = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = String::new();
            stream.read_to_string(&mut request).expect("request");
            assert_eq!(request.matches('\n').count(), 1);
            let envelope: HarnessReadEnvelopeV1 =
                serde_json::from_str(request.trim_end()).expect("envelope");
            assert!(matches!(envelope.request, HarnessReadRequestV1::ContextGet));
            let reply = HarnessReadReplyV1::Error { error: HarnessReadHostErrorV1::NotFoundOrDenied };
            let mut encoded = serde_json::to_vec(&reply).expect("reply");
            encoded.push(b'\n');
            stream.write_all(&encoded).expect("write reply");
        });
        let client = HarnessReadClient::new(endpoint, credential()).expect("client");
        assert!(matches!(client.context_get(), Err(HarnessReadClientError::Host(HarnessReadHostErrorV1::NotFoundOrDenied))));
        host.join().expect("host");
    }

    #[test]
    fn client_rejects_non_loopback_and_oversized_frames_without_secret_errors() {
        let secret = credential();
        let error = HarnessReadClient::new("192.0.2.1:1".parse().expect("address"), secret.clone())
            .expect_err("non-loopback must fail");
        assert!(!error.to_string().contains(secret.expose()));

        let oversized = vec![b'x'; HARNESS_READ_RESPONSE_MAX_BYTES + 1];
        let mut reader = BufReader::new(oversized.as_slice());
        assert!(matches!(read_bounded_line(&mut reader, HARNESS_READ_RESPONSE_MAX_BYTES), Err(HarnessReadClientError::ResponseTooLarge)));
    }

    #[test]
    fn operator_client_uses_the_same_bounded_socket_with_a_distinct_envelope() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let endpoint = listener.local_addr().expect("address");
        let host = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = String::new();
            stream.read_to_string(&mut request).expect("request");
            let envelope: HarnessOperatorEnvelopeV1 =
                serde_json::from_str(request.trim_end()).expect("operator envelope");
            assert!(matches!(
                envelope.request,
                HarnessOperatorRequestV1::TasksList { limit: 1, .. },
            ));
            let reply = HarnessOperatorReplyV1::Error {
                error: HarnessOperatorHostErrorV1::Conflict,
            };
            let mut encoded = serde_json::to_vec(&reply).expect("reply");
            encoded.push(b'\n');
            stream.write_all(&encoded).expect("write reply");
        });
        let client = HarnessOperatorClient::new(endpoint, operator_credential())
            .expect("operator client");
        assert!(matches!(
            client.tasks_list(None, None, 1),
            Err(HarnessOperatorClientError::Host(
                HarnessOperatorHostErrorV1::Conflict,
            )),
        ));
        host.join().expect("host");
    }

    #[test]
    fn operator_client_submits_v3_user_intent_without_durable_authority() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let endpoint = listener.local_addr().expect("address");
        let host = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = String::new();
            stream.read_to_string(&mut request).expect("request");
            assert!(!request.contains("authority"));
            assert!(!request.contains("operation_id"));
            assert!(!request.contains("idempotency_ref"));
            assert!(!request.contains("\"task_id\":"));
            let envelope: HarnessOperatorEnvelopeV1 =
                serde_json::from_str(request.trim_end()).expect("operator envelope");
            assert_eq!(envelope.version, HARNESS_OPERATOR_WIRE_VERSION_V3);
            assert!(matches!(
                envelope.request,
                HarnessOperatorRequestV1::SubmitIntent {
                    intent: HarnessOperatorIntentV1 {
                        action: HarnessOperatorActionV1::CreateTask { .. },
                        ..
                    },
                },
            ));
            let reply = HarnessOperatorReplyV1::Ok {
                response: HarnessOperatorResponseV1::Mutation(
                    HarnessOperatorMutationOutcomeV1::Applied,
                ),
            };
            let mut encoded = serde_json::to_vec(&reply).expect("reply");
            encoded.push(b'\n');
            stream.write_all(&encoded).expect("write reply");
        });
        let client = HarnessOperatorClient::new(endpoint, operator_credential())
            .expect("operator client");
        let response = client.submit_intent(HarnessOperatorIntentV1 {
            request_ref: HarnessOperatorRequestRefV1::new(format!(
                "hireq_{}",
                "1".repeat(24),
            )).unwrap(),
            submitted_at_unix_ms: 10,
            action: HarnessOperatorActionV1::CreateTask {
                title: "Harness-owned identity".to_owned(),
                body: "Typed user intent".to_owned(),
                parent_task_id: None,
                dependencies: Vec::new(),
                initial_state: HarnessTaskStateV1::Backlog,
            },
        }).unwrap();
        assert_eq!(
            response,
            HarnessOperatorResponseV1::Mutation(HarnessOperatorMutationOutcomeV1::Applied),
        );
        host.join().expect("host");
    }

    #[test]
    fn operator_client_native_history_uses_v3_exact_route_and_extended_read_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let endpoint = listener.local_addr().expect("address");
        let route = HarnessNativeSessionRouteV1 {
            node_id: "node-a".to_owned(),
            incarnation_id: "1".repeat(32),
            scope: HarnessNativeSessionCatalogScopeV1::Workspace,
            workspace_id: Some("workspace-a".to_owned()),
            provider: "codex".to_owned(),
        };
        let echoed_route = route.clone();
        let host = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = String::new();
            stream.read_to_string(&mut request).expect("request");
            for forbidden in ["provider_identity", "session_id", "terminal", "cwd"] {
                assert!(!request.contains(forbidden));
            }
            let envelope: HarnessOperatorEnvelopeV1 =
                serde_json::from_str(request.trim_end()).expect("operator envelope");
            assert_eq!(envelope.version, HARNESS_OPERATOR_WIRE_VERSION_V3);
            assert!(matches!(
                envelope.request,
                HarnessOperatorRequestV1::CatalogNativeSessions {
                    route: ref actual,
                    limit: 16,
                } if actual == &echoed_route,
            ));
            let reply = HarnessOperatorReplyV1::Ok {
                response: HarnessOperatorResponseV1::NativeSessionsCataloged(
                    HarnessNativeSessionsCatalogedV1 {
                        route: echoed_route,
                        entries: Vec::new(),
                        summary: None,
                    },
                ),
            };
            let mut encoded = serde_json::to_vec(&reply).expect("reply");
            encoded.push(b'\n');
            stream.write_all(&encoded).expect("write reply");
        });
        assert!(HARNESS_NATIVE_HISTORY_DEADLINE > Duration::from_secs(35));
        let client = HarnessOperatorClient::new(endpoint, operator_credential())
            .expect("operator client");
        let response = client.catalog_native_sessions(route.clone(), 16).unwrap();
        assert_eq!(response.route, route);
        assert!(response.entries.is_empty());
        host.join().expect("host");
    }

    #[test]
    fn operator_client_frame_bounds_and_errors_never_expose_secret() {
        let secret = operator_credential();
        let error = HarnessOperatorClient::new(
            "192.0.2.1:1".parse().expect("address"),
            secret.clone(),
        ).expect_err("non-loopback must fail");
        assert!(!error.to_string().contains(secret.expose()));
        let oversized = vec![b'x'; HARNESS_OPERATOR_RESPONSE_MAX_BYTES + 1];
        let mut reader = BufReader::new(oversized.as_slice());
        assert!(matches!(
            read_operator_bounded_line(&mut reader, HARNESS_OPERATOR_RESPONSE_MAX_BYTES),
            Err(HarnessOperatorClientError::ResponseTooLarge),
        ));
    }

}
