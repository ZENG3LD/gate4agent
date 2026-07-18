//! Bounded in-process port for gate4agent kernels and consumers.

use gate4agent_types::{CommandEnvelope, ControlEvent, ControlSnapshot};
use std::sync::mpsc::{
    sync_channel, Receiver, SyncSender, TryRecvError, TrySendError,
};
use std::sync::{Arc, Mutex, RwLock};
use thiserror::Error;

pub trait AgentPort: Send + Sync {
    fn dispatch(&self, command: CommandEnvelope) -> Result<(), PortDispatchError>;
    fn snapshot(&self) -> Arc<ControlSnapshot>;
    fn subscribe(&self, capacity: usize) -> EventSubscription;
}

#[derive(Clone)]
pub struct Gate4AgentHandle {
    command_tx: SyncSender<CommandEnvelope>,
    edge: Arc<EdgeState>,
}

pub struct KernelPort {
    command_rx: Receiver<CommandEnvelope>,
    edge: Arc<EdgeState>,
}

pub struct EventSubscription {
    receiver: Receiver<ControlEvent>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PublishReport {
    pub delivered: usize,
    pub disconnected_slow: usize,
    pub disconnected_closed: usize,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum PortDispatchError {
    #[error("gate4agent command ingress is full")]
    Full,
    #[error("gate4agent kernel ingress is disconnected")]
    Disconnected,
}

struct EdgeState {
    snapshot: RwLock<Arc<ControlSnapshot>>,
    subscribers: Mutex<Vec<SyncSender<ControlEvent>>>,
}

pub fn bounded_port(command_capacity: usize) -> (Gate4AgentHandle, KernelPort) {
    let (command_tx, command_rx) = sync_channel(command_capacity.max(1));
    let edge = Arc::new(EdgeState {
        snapshot: RwLock::new(Arc::new(ControlSnapshot::default())),
        subscribers: Mutex::new(Vec::new()),
    });
    (
        Gate4AgentHandle {
            command_tx,
            edge: Arc::clone(&edge),
        },
        KernelPort { command_rx, edge },
    )
}

impl AgentPort for Gate4AgentHandle {
    fn dispatch(&self, command: CommandEnvelope) -> Result<(), PortDispatchError> {
        self.command_tx.try_send(command).map_err(|error| match error {
            TrySendError::Full(_) => PortDispatchError::Full,
            TrySendError::Disconnected(_) => PortDispatchError::Disconnected,
        })
    }

    fn snapshot(&self) -> Arc<ControlSnapshot> {
        self.edge
            .snapshot
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn subscribe(&self, capacity: usize) -> EventSubscription {
        let (sender, receiver) = sync_channel(capacity.max(1));
        self.edge
            .subscribers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(sender);
        EventSubscription { receiver }
    }
}

impl Gate4AgentHandle {
    pub fn dispatch(&self, command: CommandEnvelope) -> Result<(), PortDispatchError> {
        AgentPort::dispatch(self, command)
    }

    pub fn snapshot(&self) -> Arc<ControlSnapshot> {
        AgentPort::snapshot(self)
    }

    pub fn subscribe(&self, capacity: usize) -> EventSubscription {
        AgentPort::subscribe(self, capacity)
    }
}

impl KernelPort {
    pub fn drain_commands(&self, limit: usize) -> Vec<CommandEnvelope> {
        let mut commands = Vec::new();
        for _ in 0..limit {
            match self.command_rx.try_recv() {
                Ok(command) => commands.push(command),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        commands
    }

    pub fn publish_snapshot(&self, snapshot: ControlSnapshot) {
        *self
            .edge
            .snapshot
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Arc::new(snapshot);
    }

    /// Slow subscribers are disconnected instead of silently losing ordered
    /// control events. Their next receive observes channel disconnection.
    pub fn publish_events(
        &self,
        events: impl IntoIterator<Item = ControlEvent>,
    ) -> PublishReport {
        let mut report = PublishReport::default();
        let mut subscribers = self
            .edge
            .subscribers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        for event in events {
            let mut index = 0;
            while index < subscribers.len() {
                match subscribers[index].try_send(event.clone()) {
                    Ok(()) => {
                        report.delivered += 1;
                        index += 1;
                    }
                    Err(TrySendError::Full(_)) => {
                        subscribers.swap_remove(index);
                        report.disconnected_slow += 1;
                    }
                    Err(TrySendError::Disconnected(_)) => {
                        subscribers.swap_remove(index);
                        report.disconnected_closed += 1;
                    }
                }
            }
        }
        report
    }
}

impl EventSubscription {
    pub fn try_recv(&self) -> Result<ControlEvent, TryRecvError> {
        self.receiver.try_recv()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gate4agent_types::{
        AgentId, AgentInstanceId, CommandId, ControlCommand, ControlEventKind,
        SessionGeneration, TransportKind, CONTROL_PROTOCOL_VERSION,
    };

    fn command(id: u64) -> CommandEnvelope {
        CommandEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            id: CommandId(id),
            command: ControlCommand::Register {
                instance_id: AgentInstanceId(id),
                agent_id: AgentId::new("claude").unwrap(),
                transport: TransportKind::Pty,
            },
        }
    }

    fn event(sequence: u64) -> ControlEvent {
        ControlEvent {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            sequence,
            command_id: None,
            instance_id: AgentInstanceId(1),
            generation: SessionGeneration(1),
            event: ControlEventKind::Registered,
        }
    }

    #[test]
    fn command_ingress_is_bounded_and_non_blocking() {
        let (handle, kernel) = bounded_port(1);
        handle.dispatch(command(1)).unwrap();
        assert_eq!(handle.dispatch(command(2)), Err(PortDispatchError::Full));
        assert_eq!(kernel.drain_commands(10), vec![command(1)]);
    }

    #[test]
    fn snapshot_publication_replaces_current_truth() {
        let (handle, kernel) = bounded_port(1);
        let mut snapshot = ControlSnapshot::default();
        snapshot.revision = 7;
        kernel.publish_snapshot(snapshot.clone());
        assert_eq!(*handle.snapshot(), snapshot);
    }

    #[test]
    fn slow_subscriber_is_disconnected_instead_of_losing_ordered_event() {
        let (handle, kernel) = bounded_port(1);
        let subscription = handle.subscribe(1);
        assert_eq!(kernel.publish_events([event(1)]).delivered, 1);

        let report = kernel.publish_events([event(2)]);
        assert_eq!(report.disconnected_slow, 1);
        assert_eq!(subscription.try_recv().unwrap(), event(1));
        assert_eq!(subscription.try_recv(), Err(TryRecvError::Disconnected));
    }

    #[test]
    fn closed_subscriber_is_pruned_explicitly() {
        let (handle, kernel) = bounded_port(1);
        let subscription = handle.subscribe(1);
        drop(subscription);

        let report = kernel.publish_events([event(1)]);
        assert_eq!(report.disconnected_closed, 1);
        assert_eq!(report.delivered, 0);
    }
}
