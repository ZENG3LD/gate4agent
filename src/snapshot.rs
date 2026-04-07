//! Snapshot types for agent rendering — no OS handles, safe to clone and send to UI.

use serde::{Deserialize, Serialize};

/// Which AI CLI agent to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentCli {
    Claude,
    Codex,
    Gemini,
}

impl AgentCli {
    /// Lowercase name used for file paths and CLI invocation.
    pub fn as_str(self) -> &'static str {
        match self {
            AgentCli::Claude => "claude",
            AgentCli::Codex => "codex",
            AgentCli::Gemini => "gemini",
        }
    }

    /// Human-readable label for this CLI.
    pub fn label(self) -> &'static str {
        match self {
            AgentCli::Claude => "Claude",
            AgentCli::Codex => "Codex",
            AgentCli::Gemini => "Gemini",
        }
    }

    /// Returns the next CLI in cycle order.
    pub fn cycle(self) -> Self {
        match self {
            AgentCli::Claude => AgentCli::Codex,
            AgentCli::Codex => AgentCli::Gemini,
            AgentCli::Gemini => AgentCli::Claude,
        }
    }
}

/// A single terminal cell with character and colors.
#[derive(Clone, Debug)]
pub struct TermCell {
    pub ch: String,
    pub fg: [u8; 3],
    pub bg: [u8; 3],
    pub bold: bool,
}

impl Default for TermCell {
    fn default() -> Self {
        Self {
            ch: " ".to_string(),
            fg: [204, 204, 204],
            bg: [0, 0, 0],
            bold: false,
        }
    }
}

/// Terminal grid — rows x cols of cells.
#[derive(Clone, Debug)]
pub struct TermGrid {
    pub cells: Vec<Vec<TermCell>>,
    pub cols: u16,
    pub rows: u16,
    pub cursor_row: u16,
    pub cursor_col: u16,
}

impl TermGrid {
    pub fn empty(cols: u16, rows: u16) -> Self {
        Self {
            cells: vec![vec![TermCell::default(); cols as usize]; rows as usize],
            cols,
            rows,
            cursor_row: 0,
            cursor_col: 0,
        }
    }
}

/// Chat message role.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ChatRole {
    User,
    Assistant,
    Tool,
    Thinking,
    Error,
}

/// A single chat message.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
    pub tool_name: Option<String>,
}

/// The rendering mode of the agent panel.
#[derive(Clone, Debug)]
pub enum AgentSnapshotMode {
    Pty(TermGrid),
    Chat(Vec<ChatMessage>),
    Idle,
}

/// Snapshot of agent state for rendering — no OS handles.
#[derive(Clone, Debug)]
pub struct AgentRenderSnapshot {
    pub mode: AgentSnapshotMode,
    pub session_active: bool,
}
