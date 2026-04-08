//! Transport layer — thin dispatch router for spawning CLI agent processes.
//!
//! Two transport modes exist:
//! - **Pipe**: NDJSON-streaming subprocesses (Claude, Codex, Gemini, Cursor, OpenCode)
//!   Entry point: `PipeSession::spawn` or `TransportSession::spawn` (routes to PipeSession)
//! - **PTY**: pseudo-terminal screen-scraping — use `pty::PtySession` directly
//!
//! `TransportSession` is a thin wrapper over `PipeSession` providing the same API.

mod options;
pub mod session;

pub use options::SpawnOptions;
pub use session::TransportSession;
