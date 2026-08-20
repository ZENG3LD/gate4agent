//! `gate4agent-harness-light`: a stateless, in-process light harness.
//!
//! Implements the SAME operator wire `gate4agent-harness-service` serves
//! (newline-delimited JSON request/reply frames over loopback TCP, the
//! `g4aho_` operator credential, `HarnessOperatorRequestV1`/
//! `HarnessOperatorReplyV1`), so an ordinary `HarnessOperatorClient`
//! (`gate4agent-harness-client`) connects to either one identically. Unlike
//! the full harness, there is no SQLite task kernel and no persistence: this
//! is meant to be hosted in-process inside a light client binary (e.g.
//! `gate4agent-tui-light`), talking directly to C2 instead of going through
//! a durable single-writer authority. See `crate::dispatch`'s module doc for
//! exactly which operator requests this A1 slice serves, relays, or
//! typed-rejects.
//!
//! ```no_run
//! # async fn example() -> Result<(), gate4agent_harness_light::HarnessLightError> {
//! let running = gate4agent_harness_light::start_harness_light(
//!     r"\\.\pipe\gate4agent-c2",
//!     "c2-token",
//! ).await?;
//! let _endpoint = running.operator_endpoint();
//! let _credential = running.operator_credential();
//! running.shutdown().await?;
//! # Ok(())
//! # }
//! ```

mod c2;
mod credential;
mod dispatch;
mod error;
mod inventory;
mod relay;
mod util;

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use gate4agent_c2_client::{C2ControlHandle, C2EventReceiver};
use gate4agent_c2_protocol::C2Topology;
use gate4agent_harness_api::{
    HarnessOperatorCredential, HarnessOperatorEnvelopeV1, HarnessOperatorHostErrorV1,
    HarnessOperatorReplyV1,
};
use gate4agent_harness_service::runtime::{
    read_single_frame_detecting_operator, write_operator_reply, HarnessRuntimeError,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::timeout;

pub use error::HarnessLightError;

/// Per-connection outer deadline: classify, authorize, dispatch, and reply,
/// all in one bound -- mirrors `gate4agent-harness-service::runtime`'s own
/// `HOST_CONNECTION_DEADLINE` role for the same one-shot request/reply
/// framing.
const LIGHT_CONNECTION_DEADLINE: Duration = Duration::from_secs(45);

/// Shared, cloneable state every accepted connection dispatches against:
/// the live C2 control handle (session-verb relay, route resolution), the
/// maintained runtime-inventory roster, and the one credential this process
/// minted at start.
pub(crate) struct LightState {
    control: C2ControlHandle,
    inventory: inventory::SharedInventory,
    credential_authority: credential::LightCredentialAuthority,
    /// Serializes every `NodeRequest::Snapshot` this process issues -- see
    /// `crate::c2::fetch_snapshot_serialized`'s doc comment for why.
    /// `Arc`-wrapped so `crate::relay`'s detached post-stop reap task (which
    /// outlives the request that spawned it, and so cannot borrow from this
    /// `LightState`) can clone a handle to the very same gate instead of
    /// serializing against a private one of its own.
    snapshot_gate: Arc<tokio::sync::Mutex<()>>,
}

/// A running light harness: an accepted-connections operator host plus the
/// background task keeping its runtime inventory current from C2.
pub struct HarnessLightRunning {
    operator_endpoint: SocketAddr,
    operator_credential: HarnessOperatorCredential,
    commands: mpsc::Sender<LightCommand>,
    host_task: JoinHandle<()>,
}

impl HarnessLightRunning {
    /// The loopback, ephemeral-port address the operator wire listens on.
    pub fn operator_endpoint(&self) -> SocketAddr {
        self.operator_endpoint
    }

    /// The `g4aho_` operator credential minted for this process at start.
    /// An ordinary `HarnessOperatorClient::new(operator_endpoint(),
    /// operator_credential())` connects exactly as it would to the full
    /// harness.
    pub fn operator_credential(&self) -> HarnessOperatorCredential {
        self.operator_credential.clone()
    }

    /// Stops accepting new operator connections and ends the background
    /// inventory-maintenance task. Connections already in flight are left
    /// to finish on their own (each is already bounded by
    /// `LIGHT_CONNECTION_DEADLINE`); this does not wait for them.
    pub async fn shutdown(self) -> Result<(), HarnessLightError> {
        let (ack_tx, ack_rx) = oneshot::channel();
        if self.commands.send(LightCommand::Shutdown(ack_tx)).await.is_ok() {
            let _ = ack_rx.await;
        }
        self.host_task.await?;
        Ok(())
    }
}

pub(crate) enum LightCommand {
    Shutdown(oneshot::Sender<()>),
}

/// Starts a light harness: connects to C2 as the operator, mints a fresh
/// in-process operator credential, performs one synchronous initial sweep of
/// the live runtime inventory (so the very first `RuntimeInventoryList` an
/// early caller sees already reflects whatever nodes are online), then
/// starts accepting operator connections on an ephemeral loopback port.
pub async fn start_harness_light(
    c2_endpoint: &str,
    c2_token: &str,
) -> Result<HarnessLightRunning, HarnessLightError> {
    let (control, events) =
        gate4agent_c2_client::connect_local(c2_endpoint, c2_token).await
            .map_err(HarnessLightError::C2Connect)?;
    let topology = control.subscribe_topology();

    let (credential_authority, operator_credential) = credential::LightCredentialAuthority::mint()?;

    let inventory = inventory::new_shared();
    let state = LightState {
        control,
        inventory,
        credential_authority,
        snapshot_gate: Arc::new(tokio::sync::Mutex::new(())),
    };
    inventory::sweep_online_nodes(&state).await;
    let state = Arc::new(state);

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.map_err(HarnessLightError::Bind)?;
    let operator_endpoint = listener.local_addr().map_err(HarnessLightError::Bind)?;

    let (commands, command_rx) = mpsc::channel(4);
    let host_task = tokio::spawn(run_light_host(listener, state, events, topology, command_rx));

    tracing::info!(operator_endpoint = %operator_endpoint, "harness-light: operator host started");
    Ok(HarnessLightRunning { operator_endpoint, operator_credential, commands, host_task })
}

/// The light harness's single background task: accepts operator connections
/// (spawning a detached handler per connection, matching
/// `gate4agent-harness-service::runtime`'s own per-connection task shape),
/// applies live C2 events and topology changes to the runtime-inventory
/// roster, and stops on `LightCommand::Shutdown`.
async fn run_light_host(
    listener: TcpListener,
    state: Arc<LightState>,
    mut events: C2EventReceiver,
    mut topology: watch::Receiver<Arc<C2Topology>>,
    mut commands: mpsc::Receiver<LightCommand>,
) {
    // Both `events.recv()` and `topology.changed()` resolve immediately,
    // forever, once their sender side has closed (a dead C2 connection) --
    // without these guards `select!` would busy-poll that branch on every
    // loop iteration with no yield point, pinning a worker thread at 100%
    // CPU and starving the `accept`/`commands` branches instead of just
    // going quiet. Each closes independently and only once; `commands`/
    // `listener.accept()` keep working regardless (an operator connection
    // that lands after C2 is gone still gets a reply -- a typed relay
    // failure per request -- rather than the whole host wedging).
    //
    // Both the per-event and per-topology-change handling are spawned as
    // their own detached tasks rather than awaited inline in this loop:
    // `handle_event`/`reconcile_topology` each end in a `NodeRequest::
    // Snapshot` round trip serialized behind `LightState::snapshot_gate`
    // (see that field's doc comment), which can legitimately take a while
    // under load or contention. Awaiting that inline here would mean this
    // one loop iteration's `select!` branch does not resolve until that
    // whole round trip settles -- and since this same loop is what drains
    // the *next* event off `events` and accepts the *next* operator
    // connection, one slow refresh would stall every other event and every
    // new connection behind it. Draining stays cheap and constant-time;
    // processing runs independently and concurrently, one task per event/
    // topology change, naturally serialized against each other only by the
    // shared `snapshot_gate` they already both go through.
    let mut events_open = true;
    let mut topology_open = true;
    loop {
        tokio::select! {
            command = commands.recv() => {
                match command {
                    Some(LightCommand::Shutdown(ack)) => {
                        let _ = ack.send(());
                        break;
                    }
                    None => break,
                }
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _peer)) => {
                        let state = Arc::clone(&state);
                        tokio::spawn(async move { handle_connection(stream, state).await; });
                    }
                    Err(error) => {
                        tracing::warn!(error = %error, "harness-light: operator accept failed");
                    }
                }
            }
            event = events.recv(), if events_open => {
                match event {
                    Some(event) => {
                        let state = Arc::clone(&state);
                        tokio::spawn(async move { inventory::handle_event(&state, &event).await; });
                    }
                    None => {
                        events_open = false;
                        tracing::warn!("harness-light: c2 event stream closed");
                    }
                }
            }
            changed = topology.changed(), if topology_open => {
                if changed.is_ok() {
                    let current = topology.borrow().clone();
                    let state = Arc::clone(&state);
                    tokio::spawn(async move { inventory::reconcile_topology(&state, &current).await; });
                } else {
                    topology_open = false;
                    tracing::warn!("harness-light: c2 topology watch closed");
                }
            }
        }
    }
    tracing::info!("harness-light: operator host stopped");
}

/// Handles exactly one operator connection: read one frame, classify,
/// authorize, dispatch, reply. Mirrors
/// `gate4agent-harness-service::runtime::handle_connection`'s operator
/// branch, minus everything specific to that function's other frame family
/// (the legacy read wire) and its `SubscribeEvents`/cancel-signal plumbing
/// (light mode has no push-subscription surface in A1 -- `SubscribeEvents`
/// gets the ordinary one-shot `Unsupported` reply like any other
/// unimplemented verb, see `crate::dispatch`).
async fn handle_connection(mut stream: TcpStream, state: Arc<LightState>) {
    let outcome = timeout(LIGHT_CONNECTION_DEADLINE, async {
        let mut operator_frame = false;
        let frame = read_single_frame_detecting_operator(&mut stream, &mut operator_frame).await?;
        let envelope: HarnessOperatorEnvelopeV1 = serde_json::from_slice(&frame)
            .map_err(|_| HarnessRuntimeError::InvalidFrame)?;
        envelope.validate().map_err(|_| HarnessRuntimeError::InvalidFrame)?;

        if !state.credential_authority.verify(&envelope.credential) {
            tracing::warn!("harness-light: operator request rejected: unauthorized");
            return write_operator_reply(
                &mut stream,
                HarnessOperatorReplyV1::Error { error: HarnessOperatorHostErrorV1::Unauthorized },
            ).await;
        }

        let reply = dispatch::handle_request(&state, envelope.request).await;
        match write_operator_reply(&mut stream, reply).await {
            Err(HarnessRuntimeError::ResponseTooLarge) => write_operator_reply(
                &mut stream,
                HarnessOperatorReplyV1::Error { error: HarnessOperatorHostErrorV1::TooLarge },
            ).await,
            result => result,
        }
    }).await;

    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::warn!(error = %error, "harness-light: operator connection failed");
        }
        Err(_) => {
            tracing::warn!("harness-light: operator connection exceeded its deadline");
            let _ = write_operator_reply(
                &mut stream,
                HarnessOperatorReplyV1::Error { error: HarnessOperatorHostErrorV1::Deadline },
            ).await;
        }
    }
}
