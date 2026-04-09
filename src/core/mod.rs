//! Core shared types and errors for gate4agent.

pub mod types;
pub mod error;

pub use types::{AgentEvent, CliTool, RateLimitInfo, RateLimitType, SessionConfig};
pub use error::AgentError;
