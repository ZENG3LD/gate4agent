//! Host-side handler trait for agent → host JSON-RPC requests.

use serde_json::Value;

use super::message::RpcError;

/// Handler for agent → host JSON-RPC requests.
///
/// Implement this trait to handle requests from the agent
/// (e.g. `fs/read_text_file`, `terminal/execute`, `permission/request`).
///
/// The handler is called **synchronously** from the reader loop's blocking
/// thread. It must not block for longer than a few milliseconds. For async
/// or heavy I/O, spawn a separate thread and use `std::sync::mpsc` internally.
///
/// The host MUST respond to every request. Failure to return causes the agent
/// to block waiting for a response.
pub trait HostHandler: Send + Sync {
    /// Handle a JSON-RPC request from the agent.
    ///
    /// `method` is the full method string (e.g. `"fs/read_text_file"`).
    /// `params` is the raw params Value (may be null, object, or array per
    /// the JSON-RPC 2.0 spec).
    ///
    /// Return `Ok(Value)` to send a successful response.
    /// Return `Err(RpcError)` to send an error response.
    fn handle(&self, method: &str, params: Option<Value>) -> Result<Value, RpcError>;
}

/// Default handler that rejects all requests with `METHOD_NOT_FOUND` (-32601).
///
/// Use this when JSON-RPC mode is enabled but the caller doesn't need to
/// handle any agent → host requests (e.g. notification-only or host-driven
/// agents).
pub struct RejectAllHandler;

impl HostHandler for RejectAllHandler {
    fn handle(&self, method: &str, _params: Option<Value>) -> Result<Value, RpcError> {
        Err(RpcError::method_not_found(method))
    }
}

/// A composable handler that routes requests by method name.
///
/// Register per-method handlers with [`MethodRouter::on`]. Unregistered
/// methods fall through to a fallback handler (defaults to
/// [`RejectAllHandler`]).
///
/// Dispatch is O(N) linear scan — acceptable for the typical 5–10 registered
/// methods per session.
pub struct MethodRouter {
    routes: Vec<(String, Box<dyn Fn(Option<Value>) -> Result<Value, RpcError> + Send + Sync>)>,
    fallback: Box<dyn HostHandler>,
}

impl MethodRouter {
    /// Create a new router with [`RejectAllHandler`] as the fallback.
    pub fn new() -> Self {
        Self {
            routes: Vec::new(),
            fallback: Box::new(RejectAllHandler),
        }
    }

    /// Register a sync handler for a specific method.
    ///
    /// The closure receives the raw params Value and returns the result.
    pub fn on(
        mut self,
        method: impl Into<String>,
        f: impl Fn(Option<Value>) -> Result<Value, RpcError> + Send + Sync + 'static,
    ) -> Self {
        self.routes.push((method.into(), Box::new(f)));
        self
    }

    /// Set the fallback handler for unregistered methods.
    pub fn fallback(mut self, handler: impl HostHandler + 'static) -> Self {
        self.fallback = Box::new(handler);
        self
    }
}

impl Default for MethodRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl HostHandler for MethodRouter {
    fn handle(&self, method: &str, params: Option<Value>) -> Result<Value, RpcError> {
        for (route_method, handler) in &self.routes {
            if route_method == method {
                return handler(params);
            }
        }
        self.fallback.handle(method, params)
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reject_all_returns_method_not_found() {
        let h = RejectAllHandler;
        let err = h.handle("fs/read_text_file", None).unwrap_err();
        assert_eq!(err.code, RpcError::METHOD_NOT_FOUND);
        assert!(err.message.contains("fs/read_text_file"));
    }

    #[test]
    fn method_router_dispatches_registered() {
        let router = MethodRouter::new().on("ping", |_params| Ok(json!({"pong": true})));
        let result = router.handle("ping", None).unwrap();
        assert_eq!(result["pong"], true);
    }

    #[test]
    fn method_router_falls_back_on_unknown() {
        let router = MethodRouter::new().on("ping", |_| Ok(json!(null)));
        let err = router.handle("unknown/method", None).unwrap_err();
        assert_eq!(err.code, RpcError::METHOD_NOT_FOUND);
    }

    #[test]
    fn method_router_custom_fallback() {
        struct AllowAll;
        impl HostHandler for AllowAll {
            fn handle(&self, _m: &str, _p: Option<Value>) -> Result<Value, RpcError> {
                Ok(json!({"allowed": true}))
            }
        }

        let router = MethodRouter::new()
            .on("known", |_| Ok(json!({"known": true})))
            .fallback(AllowAll);

        let known = router.handle("known", None).unwrap();
        assert_eq!(known["known"], true);

        let unknown = router.handle("anything/else", None).unwrap();
        assert_eq!(unknown["allowed"], true);
    }

    #[test]
    fn method_router_passes_params_to_handler() {
        let router =
            MethodRouter::new().on("echo", |params| Ok(params.unwrap_or(json!(null))));
        let params = json!({"key": "value"});
        let result = router.handle("echo", Some(params.clone())).unwrap();
        assert_eq!(result, params);
    }
}
