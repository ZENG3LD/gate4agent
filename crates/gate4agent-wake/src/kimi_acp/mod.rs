//! Kimi ACP host: a persistent `kimi acp` stdio process per m4a session.
//!
//! Behind feature `wake-kimi`. Permission requests are never auto-approved.

#[cfg(feature = "wake-kimi")]
mod acp;
#[cfg(feature = "wake-kimi")]
mod host;
#[cfg(feature = "wake-kimi")]
mod job_object;

#[cfg(feature = "wake-kimi")]
pub use host::{KimiAcpAdapter, KimiAcpConfig, KIMI_ACP_ENV, KIMI_BIN_ENV};

/// Stubs when `wake-kimi` is off.
#[cfg(not(feature = "wake-kimi"))]
mod stub {
    use crate::{ProviderKind, ProviderSession, SessionKind, WakeAdapter, WakeError, WakeLetter, WakeOutcome};

    pub const KIMI_ACP_ENV: &str = "M4A_KIMI_ACP";
    pub const KIMI_BIN_ENV: &str = "M4A_KIMI_BIN";

    #[derive(Clone, Debug)]
    pub struct KimiAcpConfig { pub bin: String }
    impl KimiAcpConfig {
        pub fn from_env() -> Option<Self> { None }
    }

    pub struct KimiAcpAdapter { _n: () }
    impl KimiAcpAdapter {
        pub fn new(_: Option<KimiAcpConfig>) -> Self { Self { _n: () } }
    }
    impl WakeAdapter for KimiAcpAdapter {
        fn kind(&self) -> SessionKind { SessionKind::local(ProviderKind::KimiCode) }
        fn probe(&self, _: &ProviderSession) -> Result<(), WakeError> {
            Err(WakeError::Unavailable("this build was made without feature wake-kimi".into()))
        }
        fn wake(&mut self, s: &ProviderSession, _: &WakeLetter<'_>) -> Result<WakeOutcome, WakeError> {
            self.probe(s)?; unreachable!()
        }
    }
}
#[cfg(not(feature = "wake-kimi"))]
pub use stub::{KimiAcpAdapter, KimiAcpConfig, KIMI_ACP_ENV, KIMI_BIN_ENV};
