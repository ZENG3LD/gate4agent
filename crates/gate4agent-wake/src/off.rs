//! Stand-ins for wake adapters whose feature is off (`wake-codex`, `wake-kimi`, `wake-claude`,
//! `wake-spawn`). The real modules are not compiled; these keep the chain planner and the adapter
//! table type-correct, and every attempt reports why nothing was delivered.

#![allow(dead_code)]

use std::path::PathBuf;

use super::{ProviderKind, ProviderSession, SessionKind, WakeAdapter, WakeError, WakeLetter, WakeOutcome};

macro_rules! off_adapter {
    ($ty:ident, $kind:expr, $feature:literal) => {
        pub struct $ty;
        impl WakeAdapter for $ty {
            fn kind(&self) -> SessionKind {
                $kind
            }
            fn probe(&self, _: &ProviderSession) -> Result<(), WakeError> {
                Err(WakeError::Unavailable(concat!("this build was made without feature ", $feature).into()))
            }
            fn wake(&mut self, s: &ProviderSession, _: &WakeLetter<'_>) -> Result<WakeOutcome, WakeError> {
                self.probe(s).map(|()| WakeOutcome::Delivered)
            }
        }
    };
}

pub mod codex {
    use super::*;
    #[derive(Clone, Debug)]
    pub struct CodexEndpoint;
    impl CodexEndpoint {
        pub fn parse(_: &str) -> Option<Self> {
            None
        }
        pub fn from_env() -> Option<Self> {
            None
        }
    }
    off_adapter!(CodexAppServerAdapter, SessionKind::local(ProviderKind::Codex), "wake-codex");
    impl CodexAppServerAdapter {
        pub fn new(_: Option<CodexEndpoint>) -> Self {
            Self
        }
    }
}

pub mod kimi {
    use super::*;
    off_adapter!(KimiServerAdapter, SessionKind::local(ProviderKind::KimiCode), "wake-kimi");
    impl KimiServerAdapter {
        pub fn new(_: Option<String>, _: Option<String>) -> Self {
            Self
        }
        pub fn from_env() -> Self {
            Self
        }
        pub fn is_configured(&self) -> bool {
            false
        }
        pub fn into_parts(self) -> (Option<String>, Option<String>) {
            (None, None)
        }
    }
    pub fn discover_local_server(_: Option<&std::path::Path>) -> Option<KimiServerAdapter> {
        None
    }
}

pub mod claude_channel {
    use super::*;
    off_adapter!(ClaudeChannelAdapter, SessionKind::local(ProviderKind::ClaudeCode), "wake-claude");
    impl ClaudeChannelAdapter {
        pub fn new(_: Option<PathBuf>) -> Self {
            Self
        }
    }
}

pub mod spawn {
    use super::*;
    pub const CODEX_CLOUD_ENV_ENV: &str = "M4A_CODEX_CLOUD_ENV";
    pub fn bin_env(_: ProviderKind) -> &'static str {
        "M4A_SPAWN_PROGRAM_UNAVAILABLE"
    }
    off_adapter!(ResumeSpawnAdapter, SessionKind::local(ProviderKind::Grok), "wake-spawn");
    impl ResumeSpawnAdapter {
        pub fn new(_: SessionKind, _: Option<PathBuf>, _: Option<String>) -> Self {
            Self
        }
        pub fn from_env(_: SessionKind) -> Self {
            Self
        }
    }
}
