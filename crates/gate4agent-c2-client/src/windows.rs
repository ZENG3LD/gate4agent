use gate4agent_c2_protocol::{
    c2_auth_transcript, C2AuthDirection, C2ClientAuthentication, C2ClientFrame, C2ClientHello,
    C2Hello, C2RelayFailure, C2RequestEnvelope, C2RequestId, C2ServerFrame, NodeRequest,
    NodeRoute, RoutedNodeEvent, RoutedNodeRequest, RoutedNodeResponse, C2_CONTROL_PROTOCOL_VERSION,
    MAX_C2_AUTH_FRAME_BYTES, MAX_C2_CLIENT_FRAME_BYTES, MAX_C2_HELLO_FRAME_BYTES, MAX_C2_SERVER_FRAME_BYTES,
};
use gate4agent_node_protocol::{
    read_json_frame_limited_body_timeout, write_json_frame_limited, FrameError,
};
use gate4agent_node_wire::{local_hmac_sha256, proofs_match, random_nonce};
use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{sleep, timeout};

const CONNECT_RETRIES: usize = 100;
const CONNECT_RETRY_DELAY: Duration = Duration::from_millis(20);
const AUTH_DEADLINE: Duration = Duration::from_secs(5);
const HELLO_DEADLINE: Duration = Duration::from_secs(10);
const FRAME_BODY_DEADLINE: Duration = Duration::from_secs(5);
const COMMAND_CAPACITY: usize = 64;
const INBOUND_CAPACITY: usize = 2;
const WRITER_CAPACITY: usize = 64;
const EVENT_CAPACITY: usize = 2;

#[derive(Clone)]
pub struct C2ControlHandle {
    commands: mpsc::Sender<ControlCommand>,
    hello: Arc<C2Hello>,
}

impl C2ControlHandle {
    pub fn hello(&self) -> &C2Hello { &self.hello }

    pub async fn request(
        &self,
        route: NodeRoute,
        request: NodeRequest,
    ) -> Result<RoutedNodeResponse, C2ControlError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.commands.send(ControlCommand { route, request, reply: reply_tx })
            .await.map_err(|_| C2ControlError::Closed)?;
        reply_rx.await.map_err(|_| C2ControlError::Closed)?
    }
}

pub struct C2EventReceiver {
    events: mpsc::Receiver<RoutedNodeEvent>,
}

impl C2EventReceiver {
    pub async fn recv(&mut self) -> Option<RoutedNodeEvent> { self.events.recv().await }
}

struct ControlCommand {
    route: NodeRoute,
    request: NodeRequest,
    reply: oneshot::Sender<Result<RoutedNodeResponse, C2ControlError>>,
}

enum OwnerInput {
    Frame(C2ServerFrame),
    Closed,
}

pub async fn connect_local(
    endpoint: &str,
    token: &str,
) -> Result<(C2ControlHandle, C2EventReceiver), C2ControlError> {
    validate_endpoint(endpoint)?;
    validate_token(token)?;
    let mut pipe = connect_pipe(endpoint).await?;
    let client_nonce = random_nonce().map_err(C2ControlError::Authentication)?;
    timeout(AUTH_DEADLINE, write_json_frame_limited(
        &mut pipe,
        &C2ClientFrame::Hello(C2ClientHello::new(client_nonce)),
        MAX_C2_AUTH_FRAME_BYTES,
    )).await.map_err(|_| C2ControlError::AuthenticationTimedOut)??;
    let challenge = timeout(AUTH_DEADLINE, read_server_frame(&mut pipe, MAX_C2_AUTH_FRAME_BYTES))
        .await.map_err(|_| C2ControlError::AuthenticationTimedOut)??;
    let C2ServerFrame::Challenge(challenge) = challenge else {
        return Err(C2ControlError::Protocol("C2 did not return an authentication challenge".to_owned()));
    };
    if challenge.protocol_version != C2_CONTROL_PROTOCOL_VERSION {
        return Err(C2ControlError::Protocol("C2 control protocol version mismatch".to_owned()));
    }
    let expected_server = c2_proof(token, C2AuthDirection::Server, &client_nonce, &challenge.server_nonce)?;
    if !proofs_match(&challenge.server_proof, &expected_server) {
        return Err(C2ControlError::Authentication("C2 server proof mismatch".to_owned()));
    }
    let client_proof = c2_proof(token, C2AuthDirection::Client, &client_nonce, &challenge.server_nonce)?;
    timeout(AUTH_DEADLINE, write_json_frame_limited(
        &mut pipe,
        &C2ClientFrame::Authenticate(C2ClientAuthentication { client_proof }),
        MAX_C2_AUTH_FRAME_BYTES,
    )).await.map_err(|_| C2ControlError::AuthenticationTimedOut)??;
    let hello = timeout(HELLO_DEADLINE, read_server_frame(&mut pipe, MAX_C2_HELLO_FRAME_BYTES))
        .await.map_err(|_| C2ControlError::AuthenticationTimedOut)??;
    let hello = match hello {
        C2ServerFrame::Hello(hello) if hello.protocol_version == C2_CONTROL_PROTOCOL_VERSION => hello,
        C2ServerFrame::Rejected(failure) =>
            return Err(C2ControlError::Relay(failure)),
        C2ServerFrame::Hello(_) =>
            return Err(C2ControlError::Protocol("C2 control protocol version mismatch".to_owned())),
        _ => return Err(C2ControlError::Protocol("C2 did not return hello".to_owned())),
    };

    let (reader, writer) = tokio::io::split(pipe);
    let (commands_tx, commands_rx) = mpsc::channel(COMMAND_CAPACITY);
    let (events_tx, events_rx) = mpsc::channel(EVENT_CAPACITY);
    let (writer_tx, writer_rx) = mpsc::channel(WRITER_CAPACITY);
    let (owner_tx, owner_rx) = mpsc::channel(INBOUND_CAPACITY);
    let reader_task = tokio::spawn(control_reader(reader, owner_tx.clone()));
    let writer_task = tokio::spawn(control_writer(writer, writer_rx, owner_tx));
    tokio::spawn(async move {
        control_owner(commands_rx, events_tx, writer_tx, owner_rx).await;
        reader_task.abort();
        writer_task.abort();
    });
    Ok((C2ControlHandle { commands: commands_tx, hello: Arc::new(hello) }, C2EventReceiver { events: events_rx }))
}

async fn read_server_frame(
    pipe: &mut NamedPipeClient,
    limit: usize,
) -> Result<C2ServerFrame, C2ControlError> {
    Ok(read_json_frame_limited_body_timeout(pipe, limit, FRAME_BODY_DEADLINE).await?)
}

async fn control_reader<R>(mut reader: R, owner: mpsc::Sender<OwnerInput>)
where
    R: tokio::io::AsyncRead + Unpin,
{
    loop {
        match read_json_frame_limited_body_timeout(
            &mut reader,
            MAX_C2_SERVER_FRAME_BYTES,
            FRAME_BODY_DEADLINE,
        ).await {
            Ok(frame) => if owner.send(OwnerInput::Frame(frame)).await.is_err() { return; },
            Err(_) => { let _ = owner.send(OwnerInput::Closed).await; return; }
        }
    }
}

async fn control_writer<W>(
    mut writer: W,
    mut frames: mpsc::Receiver<C2ClientFrame>,
    owner: mpsc::Sender<OwnerInput>,
) where
    W: tokio::io::AsyncWrite + Unpin,
{
    while let Some(frame) = frames.recv().await {
        if !matches!(timeout(FRAME_BODY_DEADLINE, write_json_frame_limited(
            &mut writer,
            &frame,
            MAX_C2_CLIENT_FRAME_BYTES,
        )).await, Ok(Ok(()))) {
            break;
        }
    }
    let _ = owner.send(OwnerInput::Closed).await;
}

async fn control_owner(
    mut commands: mpsc::Receiver<ControlCommand>,
    events: mpsc::Sender<RoutedNodeEvent>,
    writer: mpsc::Sender<C2ClientFrame>,
    mut incoming: mpsc::Receiver<OwnerInput>,
) {
    let mut next_request_id = 1_u64;
    let mut pending = BTreeMap::new();
    loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else { break; };
                let request_id = C2RequestId(next_request_id);
                let Some(next) = next_request_id.checked_add(1) else {
                    let _ = command.reply.send(Err(C2ControlError::RequestIdExhausted));
                    break;
                };
                next_request_id = next;
                let frame = C2ClientFrame::Request(C2RequestEnvelope {
                    request_id,
                    request: RoutedNodeRequest { route: command.route, request: command.request },
                });
                pending.insert(request_id, command.reply);
                if writer.send(frame).await.is_err() { break; }
            }
            input = incoming.recv() => {
                match input {
                    Some(OwnerInput::Frame(C2ServerFrame::Reply(reply))) => {
                        let Some(waiter) = pending.remove(&reply.request_id) else { break; };
                        let _ = waiter.send(reply.result.map_err(C2ControlError::Relay));
                    }
                    Some(OwnerInput::Frame(C2ServerFrame::Event(event))) => {
                        if events.try_send(event).is_err() { break; }
                    }
                    Some(OwnerInput::Frame(C2ServerFrame::Challenge(_) | C2ServerFrame::Hello(_) | C2ServerFrame::Rejected(_)))
                        | Some(OwnerInput::Closed) | None => break,
                }
            }
        }
    }
    for (_, waiter) in pending { let _ = waiter.send(Err(C2ControlError::Closed)); }
}

fn c2_proof(
    token: &str,
    direction: C2AuthDirection,
    client_nonce: &[u8; 32],
    server_nonce: &[u8; 32],
) -> Result<[u8; 32], C2ControlError> {
    local_hmac_sha256(token.as_bytes(), &c2_auth_transcript(direction, client_nonce, server_nonce))
        .map_err(C2ControlError::Authentication)
}

async fn connect_pipe(endpoint: &str) -> io::Result<NamedPipeClient> {
    let mut last_error = None;
    for _ in 0..CONNECT_RETRIES {
        match ClientOptions::new().open(endpoint) {
            Ok(pipe) => return Ok(pipe),
            Err(error) if matches!(error.kind(), io::ErrorKind::NotFound) || error.raw_os_error() == Some(231) => {
                last_error = Some(error);
                sleep(CONNECT_RETRY_DELAY).await;
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_error.unwrap_or_else(|| io::Error::new(io::ErrorKind::NotFound, "C2 control pipe unavailable")))
}

fn validate_endpoint(endpoint: &str) -> Result<(), C2ControlError> {
    if !endpoint.starts_with(r"\\.\pipe\") || endpoint.len() <= r"\\.\pipe\".len() || endpoint.len() > 1024 {
        return Err(C2ControlError::InvalidEndpoint);
    }
    Ok(())
}

fn validate_token(token: &str) -> Result<(), C2ControlError> {
    if token.is_empty() || token.len() > 4096 || !token.bytes().all(|byte| matches!(byte, 0x21..=0x7e)) {
        return Err(C2ControlError::InvalidToken);
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum C2ControlError {
    #[error("C2 control endpoint is not a bounded local named pipe")]
    InvalidEndpoint,
    #[error("C2 token must contain 1..=4096 visible ASCII bytes without whitespace")]
    InvalidToken,
    #[error("C2 control I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error("C2 authentication timed out")]
    AuthenticationTimedOut,
    #[error("C2 authentication failed: {0}")]
    Authentication(String),
    #[error("C2 control protocol failed: {0}")]
    Protocol(String),
    #[error("C2 relay rejected request: {0:?}")]
    Relay(C2RelayFailure),
    #[error("C2 control connection closed")]
    Closed,
    #[error("C2 request ID space exhausted")]
    RequestIdExhausted,
}

#[cfg(test)]
mod tests {
    use super::*;
    use gate4agent_c2_protocol::{NodeCursor, NodeEvent, NodeId};
    use gate4agent_node_protocol::NodeIncarnationId;

    fn event(sequence: u64) -> OwnerInput {
        OwnerInput::Frame(C2ServerFrame::Event(RoutedNodeEvent {
            node_id: NodeId::new("node-a").unwrap(),
            cursor: NodeCursor {
                incarnation_id: NodeIncarnationId::from_bytes([7; 16]),
                sequence,
            },
            event: NodeEvent::ResyncRequired { oldest_available_sequence: sequence },
        }))
    }

    #[tokio::test]
    async fn slow_event_consumer_closes_control_owner_without_silent_drop() {
        let (commands_tx, commands_rx) = mpsc::channel(1);
        let (events_tx, _events_rx) = mpsc::channel(EVENT_CAPACITY);
        let (writer_tx, _writer_rx) = mpsc::channel(1);
        let (incoming_tx, incoming_rx) = mpsc::channel(4);
        let owner = tokio::spawn(control_owner(commands_rx, events_tx, writer_tx, incoming_rx));

        incoming_tx.send(event(1)).await.unwrap();
        incoming_tx.send(event(2)).await.unwrap();
        incoming_tx.send(event(3)).await.unwrap();
        timeout(Duration::from_secs(1), owner).await.unwrap().unwrap();
        assert!(commands_tx.is_closed());
    }
}
