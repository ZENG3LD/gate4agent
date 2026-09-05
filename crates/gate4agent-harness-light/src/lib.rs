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
//! exactly which operator requests this crate serves, relays, or
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
mod terminal;
mod util;

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use gate4agent_c2_client::{C2ReconnectingEventReceiver, C2ReconnectingHandle};
use gate4agent_c2_protocol::C2Topology;
use gate4agent_harness_api::{
    HarnessOperatorApiError, HarnessOperatorCredential, HarnessOperatorEnvelopeV1,
    HarnessOperatorEventV1, HarnessOperatorHostErrorV1, HarnessOperatorReplyV1,
    HarnessOperatorRequestV1, HarnessRuntimeNodeInventoryV1,
};
use gate4agent_harness_service::runtime::{
    read_single_frame_detecting_operator, run_operator_event_subscription, write_operator_reply,
    HarnessRuntimeError, OperatorRequestLogIdentity, SubscriberRegistry,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, watch, Semaphore};
use tokio::task::JoinHandle;
use tokio::time::{interval_at, timeout, Instant, MissedTickBehavior};

pub use error::HarnessLightError;

/// Per-connection outer deadline: classify, authorize, dispatch, and reply,
/// all in one bound -- mirrors `gate4agent-harness-service::runtime`'s own
/// `HOST_CONNECTION_DEADLINE` role for the same one-shot request/reply
/// framing.
const LIGHT_CONNECTION_DEADLINE: Duration = Duration::from_secs(45);

/// Capacity of the `LightCommand` channel every accepted connection and
/// every detached C2-event/topology-change task shares a sender clone of
/// (`LightState::commands`). `Shutdown`/`Subscribe` are rare and always sent
/// after `run_light_host`'s own task is already running and draining this
/// channel (a connection can only be accepted, and a caller can only hold a
/// `HarnessLightRunning` to call `shutdown()`, once `start_harness_light` has
/// spawned it) -- but `InventoryChanged`/`InventoryRemoved` (see
/// `LightCommand`'s own doc comment) is `try_send`d, including from the
/// *initial* roster sweep (`inventory::sweep_online_nodes`, called before
/// this loop task is spawned at all, see `start_harness_light`), so this
/// must stay generously above any realistic fleet size purely so that sweep
/// can never observe a full channel. Mirrors
/// `gate4agent-harness-service::runtime`'s own `HOST_SUBSCRIBER_QUEUE_CAPACITY`
/// value for the same "generous, not tuned to a measured load" reasoning.
const LIGHT_COMMAND_CAPACITY: usize = 256;

/// Own dedicated pool for subscribed connections, mirroring
/// `gate4agent-harness-service::runtime`'s own `HOST_SUBSCRIBER_LIMIT` (a
/// plain literal, not promoted `pub` -- same no-kernel-dependency
/// duplication rationale `crate::relay::SESSION_SPAWN_DEADLINE_MS` already
/// documents for this crate). This crate has no ordinary-connection
/// admission control at all otherwise -- every accepted connection just
/// spawns unconditionally -- so this is the first, and only, connection cap
/// it enforces: an open-ended, long-lived subscription must not be free to
/// accumulate without bound the way a one-shot request/reply connection
/// already self-bounds (it always ends within `LIGHT_CONNECTION_DEADLINE`).
const LIGHT_SUBSCRIBER_LIMIT: usize = 8;

/// Mirrors `gate4agent-harness-service::runtime`'s own
/// `HOST_SUBSCRIBER_QUEUE_CAPACITY` -- same rationale as
/// `LIGHT_SUBSCRIBER_LIMIT` above.
const LIGHT_SUBSCRIBER_QUEUE_CAPACITY: usize = 256;

/// Mirrors `gate4agent-harness-service::runtime`'s own
/// `HOST_SUBSCRIBER_KEEPALIVE_INTERVAL` -- see that constant's own doc
/// comment for the full reasoning (this crate reuses `SubscriberRegistry`
/// verbatim, so it inherits the exact same "only reaps on a failed write"
/// property and the exact same leak). Same duplication rationale as
/// `LIGHT_SUBSCRIBER_LIMIT` above: a plain literal here, not shared, since
/// this crate has no dependency on the full harness's own kernel-bound
/// constants.
const LIGHT_SUBSCRIBER_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// Shared, cloneable state every accepted connection dispatches against:
/// the live C2 control handle (session-verb relay, route resolution), the
/// maintained runtime-inventory roster, and the one credential this process
/// minted at start.
///
/// `control` is a `C2ReconnectingHandle`, not a raw `C2ControlHandle`: the
/// physical connection to `c2_endpoint` can die and be re-established any
/// number of times over this process's lifetime without ever handing out a
/// new value here -- a background supervisor keeps the handle's `watch`
/// cells pointed at whichever connection is live, so a c2 relay restart
/// never permanently breaks this crate the way the bare `connect_local` it
/// replaced would have (see `crate::c2::resolve_exact_route`'s doc comment
/// for the specific failure this fixes).
pub(crate) struct LightState {
    control: C2ReconnectingHandle,
    inventory: inventory::SharedInventory,
    /// The maintained ring of live terminal frames, kept in lockstep with
    /// `inventory` on every C2 event/topology change -- see `crate::terminal`'s
    /// module doc comment for why this reuses
    /// `gate4agent_harness_service::terminal::TerminalBufferRegistry` verbatim
    /// rather than a light-local reimplementation.
    terminal: terminal::SharedTerminalRegistry,
    credential_authority: credential::LightCredentialAuthority,
    /// Serializes every `NodeRequest::Snapshot` this process issues -- see
    /// `crate::c2::fetch_snapshot_serialized`'s doc comment for why.
    /// `Arc`-wrapped so `crate::relay`'s detached post-stop reap task (which
    /// outlives the request that spawned it, and so cannot borrow from this
    /// `LightState`) can clone a handle to the very same gate instead of
    /// serializing against a private one of its own.
    snapshot_gate: Arc<tokio::sync::Mutex<()>>,
    /// Back-channel into `run_light_host`'s own select loop -- the loop
    /// alone owns the `SubscriberRegistry` (the same drain doctrine
    /// `run_light_host`'s own doc comment already establishes for
    /// `inventory`/`terminal`: state mutation may run detached, but every
    /// emit happens on this one task). Every accepted connection and every
    /// detached C2-event/topology-change task holds a clone of this same
    /// sender (`mpsc::Sender` clones cheaply -- no further `Arc` wrapper
    /// needed, unlike `snapshot_gate` above), used to register a new
    /// subscription (`LightCommand::Subscribe`, blocking -- dropping this
    /// one must never happen silently) and to report a roster change for the
    /// loop to diff-emit (`LightCommand::InventoryChanged`/`InventoryRemoved`,
    /// `try_send` -- never blocking its sender, see `LightCommand`'s own doc
    /// comment for why that specifically matters here).
    commands: mpsc::Sender<LightCommand>,
    /// Admission control for `SubscribeEvents` connections specifically --
    /// see `LIGHT_SUBSCRIBER_LIMIT`'s own doc comment.
    subscriber_connections: Arc<Semaphore>,
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

/// Every message a detached task (a per-connection handler, or the C2-
/// event/topology-change tasks `run_light_host` spawns) can send back into
/// the select loop it cannot mutate directly. `Shutdown` predates A3;
/// `Subscribe`/`InventoryChanged`/`InventoryRemoved` are A3's own bridge
/// into the loop-owned `SubscriberRegistry` -- see `LightState::commands`'s
/// doc comment for the drain-doctrine rationale, and
/// `gate4agent-harness-service::runtime`'s own `HostCommand::Subscribe` for
/// the full harness's exact counterpart to the first.
pub(crate) enum LightCommand {
    Shutdown(oneshot::Sender<()>),
    /// Registers a new event subscriber. Fire-and-forget, same shape as
    /// `gate4agent-harness-service::runtime`'s own `HostCommand::Subscribe`:
    /// no ack -- the connection task already holds the paired
    /// `mpsc::Receiver`, and the loop's own `SnapshotBaseline` push (sent to
    /// `sender` the moment this arm runs) is itself the observable proof of
    /// successful registration. Always sent via a blocking `.send().await`
    /// (`lib.rs`'s `handle_connection`): dropping this one silently would
    /// leave the client hanging forever waiting for a baseline that never
    /// arrives, so unlike the two variants below it must never be lossy.
    Subscribe {
        sender: mpsc::Sender<HarnessOperatorEventV1>,
        identity: OperatorRequestLogIdentity,
    },
    /// A node's runtime-inventory entry was newly inserted or changed --
    /// carries the already-diffed (`crate::inventory::emits_inventory_changed`),
    /// already-cloned new value, computed off the loop by whichever detached
    /// task called `crate::inventory::refresh_route` (the background C2-
    /// event handler, a topology reconcile's re-sweep, or an eager post-
    /// mutation refresh in `crate::relay`). Sent via `try_send`, never
    /// awaited -- see `refresh_route`'s own doc comment for why this
    /// specific command must never block its sender (including the initial
    /// roster sweep, which runs before this loop task has even been spawned
    /// to start draining it).
    InventoryChanged(HarnessRuntimeNodeInventoryV1),
    /// A node left the maintained roster entirely (offline, its incarnation
    /// moved on, or it left the fleet) -- one command per node id
    /// `crate::inventory::reconcile_topology`'s own diffed removal set
    /// (`stale_node_ids`) actually dropped. Same `try_send`, never-block
    /// rationale as `InventoryChanged` above.
    InventoryRemoved(String),
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
        gate4agent_c2_client::connect_local_reconnecting(c2_endpoint, c2_token).await
            .map_err(HarnessLightError::C2Connect)?;
    let topology = control.subscribe_topology();

    let (credential_authority, operator_credential) = credential::LightCredentialAuthority::mint()?;

    // Built before `LightState` (which now holds a clone of `commands` --
    // see that field's own doc comment): the initial sweep below runs
    // through the very same `refresh_route` path every later roster change
    // does, so it needs a live sender to `try_send` against too, even though
    // nothing drains this channel until `run_light_host` is spawned further
    // down (`LIGHT_COMMAND_CAPACITY`'s doc comment is exactly why that is
    // safe: `try_send` never blocks, and the sweep can never fill it).
    let (commands, command_rx) = mpsc::channel(LIGHT_COMMAND_CAPACITY);
    let inventory = inventory::new_shared();
    let terminal = terminal::new_shared();
    let state = LightState {
        control,
        inventory,
        terminal,
        credential_authority,
        snapshot_gate: Arc::new(tokio::sync::Mutex::new(())),
        commands: commands.clone(),
        subscriber_connections: Arc::new(Semaphore::new(LIGHT_SUBSCRIBER_LIMIT)),
    };
    inventory::sweep_online_nodes(&state).await;
    let state = Arc::new(state);

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.map_err(HarnessLightError::Bind)?;
    let operator_endpoint = listener.local_addr().map_err(HarnessLightError::Bind)?;

    let host_task = tokio::spawn(run_light_host(listener, state, events, topology, command_rx));

    tracing::info!(operator_endpoint = %operator_endpoint, "harness-light: operator host started");
    Ok(HarnessLightRunning { operator_endpoint, operator_credential, commands, host_task })
}

/// The light harness's single background task: accepts operator connections
/// (spawning a detached handler per connection, matching
/// `gate4agent-harness-service::runtime`'s own per-connection task shape),
/// applies live C2 events and topology changes to the runtime-inventory
/// roster AND the terminal ring (`crate::terminal`), and stops on
/// `LightCommand::Shutdown`.
///
/// Also the sole owner of the `SubscriberRegistry` (A3): every
/// `subscribers.insert`/`send_to`/`emit`/`recover_lagged_with` call happens
/// on this one task, driven either by a `LightCommand` drained here
/// (`Subscribe`/`InventoryChanged`/`InventoryRemoved`, sent by a detached
/// connection/event/topology task that cannot touch `subscribers` itself)
/// or by the unconditional `recover_lagged_with` pass at the bottom of every
/// loop iteration -- the same "state mutation may run detached, emits never
/// do" drain doctrine already established for `inventory`/`terminal` above,
/// extended to cover the registry too.
async fn run_light_host(
    listener: TcpListener,
    state: Arc<LightState>,
    mut events: C2ReconnectingEventReceiver,
    mut topology: watch::Receiver<Arc<C2Topology>>,
    mut commands: mpsc::Receiver<LightCommand>,
) {
    let mut subscribers = SubscriberRegistry::default();
    // See `LIGHT_SUBSCRIBER_KEEPALIVE_INTERVAL`'s own doc comment: this
    // registry only ever discovers a dead subscriber on a failed write, so a
    // periodic tick is what makes that discovery happen even when nothing
    // real ever changes.
    let mut subscriber_keepalive = interval_at(
        Instant::now() + LIGHT_SUBSCRIBER_KEEPALIVE_INTERVAL,
        LIGHT_SUBSCRIBER_KEEPALIVE_INTERVAL,
    );
    subscriber_keepalive.set_missed_tick_behavior(MissedTickBehavior::Skip);
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
    // `inventory::handle_event`/`reconcile_topology` each end in a
    // `NodeRequest::Snapshot` round trip serialized behind
    // `LightState::snapshot_gate` (see that field's doc comment), which can
    // legitimately take a while under load or contention. Awaiting that
    // inline here would mean this one loop iteration's `select!` branch does
    // not resolve until that whole round trip settles -- and since this same
    // loop is what drains the *next* event off `events` and accepts the
    // *next* operator connection, one slow refresh would stall every other
    // event and every new connection behind it. Draining stays cheap and
    // constant-time; processing runs independently and concurrently, one
    // task per event/topology change, naturally serialized against each
    // other only by the shared `snapshot_gate` they already both go through.
    // `terminal::handle_event`/`reconcile_topology` ride the very same
    // detached task, called right after their `inventory` counterpart: they
    // never touch `snapshot_gate` or C2 at all (a pure in-memory ring
    // ingest/prune), so folding them in adds no new blocking point -- see
    // `crate::terminal`'s own module doc comment for why this is one shared
    // task rather than a second spawn per event.
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
                    Some(LightCommand::Subscribe { sender, identity }) => {
                        tracing::info!("harness-light: operator event subscriber registered");
                        let id = subscribers.insert(sender, identity);
                        // Tasks/runs are always empty in light mode (no task
                        // kernel, by canon); nodes come from the same shared,
                        // lock-guarded roster `RuntimeInventoryList` itself
                        // reads (`crate::inventory::list`), snapshotted fresh
                        // right here rather than cached loop-side the way the
                        // full harness's own `HarnessRuntimeInventoryCache`
                        // is, since that cache is not reused here (see
                        // `crate::inventory`'s own module doc comment).
                        let nodes = state.inventory.read().await.values().cloned().collect::<Vec<_>>();
                        subscribers.send_to(id, |sequence| HarnessOperatorEventV1::SnapshotBaseline {
                            sequence, tasks: Vec::new(), runs: Vec::new(), nodes,
                        });
                    }
                    Some(LightCommand::InventoryChanged(node)) => {
                        subscribers.emit(|sequence| HarnessOperatorEventV1::RuntimeInventoryChanged {
                            sequence, node: node.clone(),
                        });
                    }
                    Some(LightCommand::InventoryRemoved(node_id)) => {
                        subscribers.emit(|sequence| HarnessOperatorEventV1::RuntimeInventoryRemoved {
                            sequence, node_id: node_id.clone(),
                        });
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
                        tokio::spawn(async move {
                            inventory::handle_event(&state, &event).await;
                            terminal::handle_event(&state.terminal, &event).await;
                        });
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
                    tokio::spawn(async move {
                        inventory::reconcile_topology(&state, &current).await;
                        terminal::reconcile_topology(&state.terminal, &current).await;
                    });
                } else {
                    topology_open = false;
                    tracing::warn!("harness-light: c2 topology watch closed");
                }
            }
            _ = subscriber_keepalive.tick(), if !subscribers.is_empty() => {
                // Same `Ping` shape and the same reasoning as
                // `gate4agent-harness-service::runtime`'s own
                // `emit_subscriber_keepalive`: `emit`'s existing `Closed`
                // handling reaps a dead peer and logs it; this call site
                // adds no logging of its own.
                subscribers.emit(|sequence| HarnessOperatorEventV1::Ping { sequence });
            }
        }
        // Runs once per select-loop pass, exactly mirroring
        // `gate4agent-harness-service::runtime`'s own placement of
        // `subscribers.recover_lagged(...)` right after its own `select!`
        // block: `needs_recovery`'s cheap check means the `.read().await`
        // lock acquisition below only actually happens on a pass where at
        // least one subscriber is lagged, not on every idle iteration.
        if subscribers.needs_recovery() {
            let nodes = state.inventory.read().await.values().cloned().collect::<Vec<_>>();
            subscribers.recover_lagged_with(|| (Vec::new(), Vec::new(), nodes));
        }
    }
    tracing::info!("harness-light: operator host stopped");
}

/// Handles exactly one operator connection: read one frame, classify,
/// authorize, dispatch, reply. Mirrors
/// `gate4agent-harness-service::runtime::handle_connection`'s operator
/// branch, minus everything specific to that function's other frame family
/// (the legacy read wire) and its cancel-signal plumbing (node-workspace/
/// session-spawn/session-control/etc cancellation is out of scope for this
/// crate's direct-relay model, see `crate::relay`'s own module doc comment).
///
/// `SubscribeEvents` (A3) is the one request that branches out of the
/// ordinary one-shot reply path entirely, exactly mirroring the full
/// harness's own `handle_connection`: it registers via
/// `LightCommand::Subscribe` and returns immediately; the mandatory first
/// `SnapshotBaseline` arrives moments later through the paired channel,
/// forwarded by the promoted `run_operator_event_subscription` once this
/// whole `timeout(...)` block resolves. Recorded into the outer
/// `subscription` local (captured by mutable reference) rather than
/// returned directly, so the connection's whole subsequent forwarding-loop
/// lifetime runs outside `LIGHT_CONNECTION_DEADLINE`, not bounded by it.
async fn handle_connection(mut stream: TcpStream, state: Arc<LightState>) {
    let mut subscription = None;
    let outcome = timeout(LIGHT_CONNECTION_DEADLINE, async {
        let mut operator_frame = false;
        let frame = read_single_frame_detecting_operator(&mut stream, &mut operator_frame).await?;
        let envelope: HarnessOperatorEnvelopeV1 = serde_json::from_slice(&frame)
            .map_err(|_| HarnessRuntimeError::InvalidFrame)?;
        if let Err(error) = envelope.validate() {
            if let HarnessOperatorApiError::BuildStampMismatch { expected, received } = error {
                tracing::warn!(
                    expected = %expected,
                    received = %received,
                    "harness-light: operator build stamp mismatch: rebuild and restart \
                     the out-of-date side",
                );
                write_operator_reply(
                    &mut stream,
                    HarnessOperatorReplyV1::Error {
                        error: HarnessOperatorHostErrorV1::BuildStampMismatch { expected, received },
                    },
                ).await?;
            }
            return Err(HarnessRuntimeError::InvalidFrame);
        }

        if !state.credential_authority.verify(&envelope.credential) {
            tracing::warn!("harness-light: operator request rejected: unauthorized");
            return write_operator_reply(
                &mut stream,
                HarnessOperatorReplyV1::Error { error: HarnessOperatorHostErrorV1::Unauthorized },
            ).await;
        }

        if matches!(envelope.request, HarnessOperatorRequestV1::SubscribeEvents {}) {
            let Ok(subscriber_permit) = state.subscriber_connections.clone().try_acquire_owned()
            else {
                tracing::info!(
                    limit = LIGHT_SUBSCRIBER_LIMIT,
                    "harness-light: operator event subscribe rejected: subscriber limit reached",
                );
                return write_operator_reply(
                    &mut stream,
                    HarnessOperatorReplyV1::Error { error: HarnessOperatorHostErrorV1::Busy },
                ).await;
            };
            let identity = OperatorRequestLogIdentity::describe(&envelope.request);
            let (sender, receiver) = mpsc::channel(LIGHT_SUBSCRIBER_QUEUE_CAPACITY);
            state.commands.send(LightCommand::Subscribe { sender, identity }).await
                .map_err(|_| HarnessRuntimeError::HostStopped)?;
            subscription = Some((receiver, subscriber_permit));
            return Ok(());
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
        Ok(Ok(())) => {
            if let Some((receiver, subscriber_permit)) = subscription {
                let _ = run_operator_event_subscription(stream, receiver, subscriber_permit).await;
            }
        }
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
