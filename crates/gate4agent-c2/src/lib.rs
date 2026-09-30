//! Headless observe-only aggregation daemon for local Gate4Agent nodes.

#[cfg(any(windows, unix))]
mod runtime;

#[cfg(any(windows, unix))]
pub use runtime::{
    default_c2_control_endpoint, C2Config, C2ConfigError, C2Error, C2NodeConfig, C2Running,
    C2ShutdownHandle, C2Timings, DEFAULT_C2_CONTROL_ENDPOINT,
};

pub use gate4agent_c2_protocol as protocol;
