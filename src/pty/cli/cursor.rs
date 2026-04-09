//! Cursor Agent PTY stubs.
//!
//! Cursor Agent is pipe-only — it has no PTY OutputParser or PromptSubmitter.
//! The PTY factory falls back to the Claude parser/submitter for Cursor.
//! This module exists as a placeholder for future PTY support.
