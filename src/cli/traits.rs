//! Unified traits and types for CLI output parsing and prompt submission.

use crate::types::CliTool;
use std::io;

/// Classification of a message received from a CLI tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageClass {
    /// AI's actual response text.
    AiResponse,
    /// Informational message (update warnings, tips, install notices).
    InfoMessage,
    /// Error from the CLI (rate limit, API error, permission denied, context limit).
    Error,
    /// UI element (box-drawing, status bar, ASCII art, spinners).
    UiElement,
    /// Echo of the user's own input.
    UserEcho,
    /// Thinking/processing indicator.
    ThinkingIndicator,
    /// Tool/permission approval prompt.
    ToolApproval,
    /// Interactive menu (model selection, update menu).
    Menu,
    /// Prompt marker indicating CLI is ready for input.
    PromptReady,
    /// Unclassified raw output.
    Raw,
}

/// Metadata about a parsed message.
#[derive(Debug, Clone, Default)]
pub struct MessageMetadata {
    /// The CLI tool that produced this message. Non-optional: parsers always know their tool.
    pub tool: CliTool,
    pub turn: Option<u32>,
    pub is_partial: bool,
    pub tool_name: Option<String>,
    pub elapsed_secs: Option<u32>,
}

impl MessageMetadata {
    /// Create metadata with a given tool and all other fields defaulted.
    pub fn for_tool(tool: CliTool) -> Self {
        Self {
            tool,
            turn: None,
            is_partial: false,
            tool_name: None,
            elapsed_secs: None,
        }
    }
}

/// A fully classified message from CLI output.
#[derive(Debug, Clone)]
pub struct ParsedMessage {
    pub class: MessageClass,
    pub content: String,
    pub metadata: MessageMetadata,
}

/// Trait that all CLI output parsers must implement.
pub trait OutputParser: Send {
    fn feed(&mut self, data: &str);
    fn parse(&mut self) -> Vec<ParsedMessage>;
    fn extract_ai_text(&self, raw_cleaned: &str) -> String;
    fn classify(&self, text: &str) -> MessageClass;
    fn buffer(&self) -> &str;
    fn clear(&mut self);
    fn tool(&self) -> CliTool;
}

/// Trait for submitting prompts to a CLI tool via PTY.
pub trait PromptSubmitter: Send {
    fn send_prompt(&self, writer: &mut dyn io::Write, prompt: &str) -> io::Result<()>;
    fn send_command(&self, writer: &mut dyn io::Write, command: &str) -> io::Result<()>;
    fn send_control(&self, writer: &mut dyn io::Write, bytes: &[u8]) -> io::Result<()>;
    fn handle_startup(&self, output: &str) -> StartupAction;
    fn tool(&self) -> CliTool;

    /// Whether this CLI tool requires character-by-character input.
    /// Ink-based TUI apps (Claude, Codex) need this because they read stdin in raw mode.
    fn requires_char_by_char(&self) -> bool {
        false // default: send all at once
    }
}

/// Action to take during startup handling.
#[derive(Debug, Clone)]
pub enum StartupAction {
    Ready,
    SendInput(String),
    Waiting,
}
