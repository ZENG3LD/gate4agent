//! PTY transport — pseudo-terminal screen-scraping for CLI agents.

pub mod wrapper;
pub mod session;
pub mod vte;
pub mod screen;
pub mod snapshot;
pub mod rate_limit;
pub mod cli;

pub use wrapper::{PtyError, PtyWrapper};
pub use session::{PtySession, PtyWriteHandle};
pub use snapshot::{
    AgentCli, AgentRenderSnapshot, AgentSnapshotMode, BuddyArt, ChatMessage, ChatRole,
    LiveStatus, TermCell, TermGrid,
};
pub use rate_limit::RateLimitDetector;
pub use vte::VteParser;
pub use screen::{ScreenParser, analyze_screen, RowKind, ScreenAnalysis, UiState};
