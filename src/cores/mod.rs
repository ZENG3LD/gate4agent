//! Deep connector cores. Events in, events out. Not a terminal UI.
//!
//! Owner decision 2026-10-04: PTY is the shallow path (do not send prompts
//! into a PTY and do not parse the screen). This module is the deep path,
//! and it speaks each vendor's own wire:
//!
//! - Codex: `codex app-server` JSON-RPC over stdio (`initialize`,
//!   `initialized`, `thread/start`, `turn/start`). Never
//!   `npx @agentclientprotocol/codex-acp`.
//! - Claude: the Agent SDK `query()` stream-json protocol against the
//!   `claude` CLI. The official SDK has no ACP. Never
//!   `npx @agentclientprotocol/claude-agent-acp`.
//! - Kimi (`kimi acp`) and Grok (`grok agent stdio`) stay on the native ACP
//!   spawn in [`crate::acp`]. This module does not replace those.
//!
//! The first slice is the wire only: spawn, initialize, one thread start,
//! one parsed line. It does not drive the session UI.

mod claude_query;
mod codex_app_server;
mod stdio_child;

pub use claude_query::{
    claude_binary, encode_control_initialize, encode_user_message, parse_claude_line,
    ClaudeQueryClient, ClaudeQueryEvent, CLAUDE_QUERY_ARGS,
};
pub use codex_app_server::{
    codex_binary, encode_initialize, encode_initialized, encode_thread_start, encode_turn_start,
    parse_codex_line, CodexAppServer, CodexClientInfo, CodexId, CodexInitializeParams,
    CodexMessage, CodexThreadStartParams, CodexTurnStartParams, CodexUserInput,
    CODEX_APP_SERVER_ARGS,
};

use std::io;
use std::time::Duration;

/// Failure while encoding, parsing, or driving one stdio core.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// `codex` was not on `PATH`, so the live app-server was not started.
    #[error("codex binary not on PATH; live app-server spawn was not run")]
    CodexNotInstalled,
    /// `claude` was not on `PATH`, so the live query() CLI was not started.
    #[error("claude binary not on PATH; live query spawn was not run")]
    ClaudeNotInstalled,
    /// A read produced no bytes before the child closed stdout.
    #[error("child stdout closed")]
    Eof,
    /// No JSON line arrived before the read deadline.
    #[error("timed out after {0:?} waiting for a wire line")]
    Timeout(Duration),
    /// The line was empty, not JSON, or not a wire object.
    #[error("bad wire line: {0}")]
    BadLine(String),
    /// A line parsed, but it was not the message this step was waiting for.
    #[error("unexpected wire message: {0}")]
    Unexpected(String),
    /// The peer returned a JSON-RPC error object.
    #[error("rpc error {code}: {message}")]
    Rpc { code: i64, message: String },
    /// Spawning or writing the child failed.
    #[error("stdio: {0}")]
    Io(#[from] io::Error),
}
