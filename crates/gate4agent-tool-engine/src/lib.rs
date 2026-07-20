//! Deterministic policy and lifecycle engine for Gate-owned tool capabilities.
//!
//! This crate owns no executor. It releases an invocation only as a typed
//! effect after an exact grant (and, when configured, an approval). Provider
//! shells return correlated observations through the same generation fence.

mod engine;

pub use engine::{ToolEngine, ToolEngineError};
pub use gate4agent_tool_protocol::*;
