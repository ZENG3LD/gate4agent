//! PTY transport — pseudo-terminal screen-scraping for CLI agents.

pub mod wrapper;
pub mod session;
pub mod event;
pub mod os_process;
pub mod process_tree;
pub mod restore;
pub mod vte;
pub mod screen;
pub mod snapshot;
pub mod rate_limit;
pub mod cli;

pub use wrapper::{PtyError, PtyWrapper};
pub use wrapper::{PTY_OUTPUT_HIGH_WATER_BYTES, PTY_OUTPUT_LOW_WATER_BYTES};
pub use session::{PtySession, PtyShutdownOutcome, PtyWriteHandle, PTY_SHUTDOWN_TIMEOUT_MS};
pub use event::{
    PtyAttachError, PtyAttachment, PtyEvent, PtyEventEnvelope, PtyEventReceiver, PtyEventRecvError,
    PtyGapReason, PtyReplayCursor, PtySignal, PtySignalOutcome, PtySize, PtyTerminalSnapshot,
    DEFAULT_PTY_REPLAY_BYTES, PTY_PROVIDER_PROTOCOL_REVISION,
};
pub use os_process::{PtyForegroundObservation, PtyForegroundSource, PtyProcessProbeError};
pub use process_tree::{
    PtyTreeTerminationError, PtyTreeTerminationReport, PTY_DESCENDANT_GRACE,
};
pub use restore::{
    PtyColdRestoreCheckpoint, PtyColdRestoreError, PTY_COLD_RESTORE_FORMAT_REVISION,
    PTY_COLD_RESTORE_MAX_BYTES, PTY_COLD_RESTORE_MAX_TAIL_EVENTS,
};
pub use snapshot::{
    AgentCli, AgentRenderSnapshot, AgentSnapshotMode, BuddyArt, ChatMessage, ChatRole,
    LiveStatus, TermCell, TermGrid,
};
pub use rate_limit::RateLimitDetector;
pub use vte::VteParser;
pub use screen::{ScreenParser, analyze_screen, RowKind, ScreenAnalysis, UiState};
