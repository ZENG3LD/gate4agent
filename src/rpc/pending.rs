//! Thread-safe registry of in-flight host → agent requests.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;
use serde_json::Value;

use super::message::{RpcError, RpcId};

/// Result type for a pending host → agent request.
pub type RpcResult = Result<Value, RpcError>;

/// Thread-safe registry of in-flight requests sent by the host to the agent.
///
/// When the host sends a request with ID N, it registers a oneshot sender here.
/// When the agent's response for ID N arrives, the reader loop looks it up and
/// delivers the result, waking the waiting `rpc_call` future.
///
/// `PendingRequests` is `Clone` (wraps `Arc`). One clone lives in the reader
/// loop; one stays in `RpcSession` for the `rpc_call` method.
#[derive(Clone, Default)]
pub struct PendingRequests {
    inner: Arc<Mutex<HashMap<RpcId, oneshot::Sender<RpcResult>>>>,
}

impl PendingRequests {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new pending request. Returns the receiver to await.
    pub fn register(&self, id: RpcId) -> oneshot::Receiver<RpcResult> {
        let (tx, rx) = oneshot::channel();
        if let Ok(mut guard) = self.inner.lock() {
            guard.insert(id, tx);
        }
        rx
    }

    /// Deliver a response (called from reader loop).
    ///
    /// Returns `Ok(())` if a waiter was found and notified.
    /// Returns `Err(result)` if no waiter was registered for `id` (stale or
    /// unsolicited response).
    pub fn resolve(&self, id: RpcId, result: RpcResult) -> Result<(), RpcResult> {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return Err(result),
        };
        match guard.remove(&id) {
            Some(tx) => {
                let _ = tx.send(result);
                Ok(())
            }
            None => Err(result),
        }
    }

    /// Cancel all pending requests with an internal error.
    ///
    /// Called on session shutdown to wake all waiting `rpc_call` futures.
    pub fn cancel_all(&self, reason: &str) {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        for (_id, tx) in guard.drain() {
            let _ = tx.send(Err(RpcError::internal(reason)));
        }
    }

    /// Number of currently pending requests.
    pub fn len(&self) -> usize {
        self.inner.lock().map(|g| g.len()).unwrap_or(0)
    }

    /// Returns true if there are no pending requests.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn register_and_resolve_success() {
        let pending = PendingRequests::new();
        let rx = pending.register(RpcId::Number(1));
        pending.resolve(RpcId::Number(1), Ok(json!({"ok": true}))).unwrap();
        let result = rx.await.unwrap();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn resolve_unknown_id_returns_err() {
        let pending = PendingRequests::new();
        let result = pending.resolve(RpcId::Number(999), Ok(json!(null)));
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn cancel_all_wakes_waiters() {
        let pending = PendingRequests::new();
        let rx1 = pending.register(RpcId::Number(1));
        let rx2 = pending.register(RpcId::Number(2));
        pending.cancel_all("session closed");
        let r1 = rx1.await.unwrap();
        let r2 = rx2.await.unwrap();
        assert!(r1.is_err());
        assert!(r2.is_err());
        assert_eq!(r1.unwrap_err().code, RpcError::INTERNAL_ERROR);
    }

    #[tokio::test]
    async fn two_concurrent_waiters_resolve_independently() {
        let pending = PendingRequests::new();
        let rx1 = pending.register(RpcId::Number(1));
        let rx2 = pending.register(RpcId::Number(2));

        pending
            .resolve(RpcId::Number(2), Ok(json!({"which": "two"})))
            .unwrap();
        pending
            .resolve(RpcId::Number(1), Ok(json!({"which": "one"})))
            .unwrap();

        let r1 = rx1.await.unwrap().unwrap();
        let r2 = rx2.await.unwrap().unwrap();
        assert_eq!(r1["which"], "one");
        assert_eq!(r2["which"], "two");
    }
}
