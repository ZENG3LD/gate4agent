//! Shared JSON-RPC 2.0 primitives used by the ACP transport.
//!
//! This module provides the low-level building blocks for bidirectional
//! JSON-RPC 2.0 communication:
//!
//! - **Message types** (`message.rs`): [`RpcRequest`], [`RpcResponse`],
//!   [`RpcNotification`], [`RpcError`], [`RpcId`], [`classify_line`]
//! - **Pending requests** (`pending.rs`): [`PendingRequests`] — a map of
//!   in-flight host → agent requests awaiting responses.
//! - **Host handler** (`handler.rs`): [`HostHandler`] trait for agent → host
//!   request dispatch, plus [`MethodRouter`] and [`RejectAllHandler`].
//! - **ID generator** (`id.rs`): [`IdGen`] — monotonic integer ID generator.
//!
//! These primitives are consumed by [`crate::acp`]. They are not a transport
//! on their own — see [`crate::acp::AcpSession`] for the full ACP transport.

pub mod handler;
pub mod id;
pub mod message;
pub mod pending;

pub use handler::{HostHandler, MethodRouter, RejectAllHandler};
pub use id::IdGen;
pub use message::{
    classify_line, IncomingMessage, RpcError, RpcId, RpcNotification, RpcRequest, RpcResponse,
};
pub use pending::PendingRequests;
