//! Core shared types and errors for gate4agent.

pub mod capabilities;
pub mod types;
pub mod error;
pub mod image;

pub use capabilities::{CliCapabilities, CliFeatures, ModelInfo, PermissionModeInfo};
pub use types::{AgentEvent, CliTool, RateLimitInfo, RateLimitType, SessionConfig};
pub use error::AgentError;
pub use image::image_to_prompt_reference;
