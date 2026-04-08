//! Google Gemini CLI bindings - commands, responses, and parsing.
//!
//! This module provides comprehensive bindings for Google's Gemini CLI,
//! including:
//! - Interactive slash commands
//! - CLI flags and top-level commands
//! - Control sequences and keyboard shortcuts
//! - Response parsing (rate limits, token usage, status info)
//! - Extension, MCP, and Skills management
//! - Session and checkpoint management

pub mod parser;
pub use parser::GeminiNdjsonParser;

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io;
use std::sync::OnceLock;

use crate::cli::traits::{
    CliCommandBuilder, MessageClass, MessageMetadata, OutputParser, ParsedMessage, PromptSubmitter,
    StartupAction,
};
use crate::parser::VteParser;
use crate::transport::SpawnOptions;
use crate::types::CliTool;

/// Gemini interactive slash commands.
///
/// These are the interactive commands available in Gemini CLI REPL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GeminiCommand {
    // Help & Information
    Help,
    About,
    Stats,
    Privacy,

    // Chat Management
    Chat { subcommand: Option<ChatSubcommand> },

    // Session Management
    Resume,
    Clear,
    Compress,

    // Memory & Context
    Memory { subcommand: Option<MemorySubcommand> },

    // Workspace Management
    Directory { subcommand: Option<DirectorySubcommand> },

    // MCP Server Management
    Mcp { subcommand: Option<McpSubcommand> },

    // Tools & Extensions
    Tools { subcommand: Option<ToolsSubcommand> },
    Extensions,

    // Agent Skills (Experimental)
    Skills { subcommand: Option<SkillsSubcommand> },

    // Checkpointing & Restoration
    Restore { tool_call_id: Option<String> },

    // Configuration & Settings
    Settings,
    Theme,
    Auth,
    Model,
    Editor,

    // Utilities
    Copy,
    Bug { text: Option<String> },
    Init,
    Vim,
    Quit,
    Exit,
}

/// Chat management subcommands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatSubcommand {
    /// Save current conversation with tag
    Save { tag: String },
    /// Resume previously saved conversation
    Resume { tag: String },
    /// List available checkpoints
    List,
    /// Delete saved checkpoint
    Delete { tag: String },
    /// Export conversation to file (Markdown or JSON)
    Share { filename: String },
}

/// Memory system subcommands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemorySubcommand {
    /// Display full concatenated memory content
    Show,
    /// Add text to memory (appends to ~/.gemini/GEMINI.md)
    Add { text: String },
    /// Reload hierarchical memory from all GEMINI.md files
    Refresh,
    /// List paths of GEMINI.md files in use
    List,
    /// Remove memory entry (not in official docs, but logical)
    Remove { text: String },
}

/// Workspace directory subcommands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectorySubcommand {
    /// Add directories to workspace
    Add { paths: Vec<String> },
    /// Display all added directories
    Show,
}

/// MCP server management subcommands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpSubcommand {
    /// List configured MCP servers and tools (default)
    List,
    /// List servers and tools with descriptions
    Desc,
    /// List servers, tools, descriptions, and schemas
    Schema,
    /// Initiate OAuth flow for server
    Auth { server_name: Option<String> },
    /// Restart all MCP servers
    Refresh,
}

/// Tools display subcommands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolsSubcommand {
    /// Show detailed descriptions
    Desc,
    /// Show tool names only (no descriptions)
    NoDesc,
}

/// Agent Skills subcommands (experimental).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillsSubcommand {
    /// List all discovered skills and status
    List,
    /// Enable specific skill
    Enable { name: String },
    /// Disable specific skill
    Disable { name: String },
    /// Refresh skill discovery
    Reload,
}

/// Approval modes for Gemini CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApprovalMode {
    /// Default mode - prompts for approval on each tool call
    Default,
    /// Auto-approve edit tools (replace, write_file) only
    AutoEdit,
    /// Auto-approve all tool calls (YOLO mode)
    Yolo,
    /// Planning mode with preview
    Plan,
}

impl std::fmt::Display for ApprovalMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApprovalMode::Default => write!(f, "default"),
            ApprovalMode::AutoEdit => write!(f, "auto_edit"),
            ApprovalMode::Yolo => write!(f, "yolo"),
            ApprovalMode::Plan => write!(f, "plan"),
        }
    }
}

/// Keyboard shortcuts and control sequences.
///
/// These represent control sequences that can be sent to Gemini CLI via PTY.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeminiControl {
    // Basic Controls
    /// Confirm selection or choice (Enter)
    Confirm,
    /// Dismiss dialogs or cancel focus (Esc)
    Dismiss,
    /// Cancel request or quit when input empty (Ctrl+C)
    Cancel,
    /// Exit CLI when input buffer empty (Ctrl+D)
    Exit,
    /// Clear input or browse previous interactions (Double Esc)
    DoubleEsc,

    // Cursor Movement
    /// Move cursor to line start (Ctrl+A / Home)
    LineStart,
    /// Move cursor to line end (Ctrl+E / End)
    LineEnd,
    /// Move cursor up one line (Up Arrow)
    CursorUp,
    /// Move cursor down one line (Down Arrow)
    CursorDown,
    /// Move cursor left one character (Left Arrow / Ctrl+B)
    CursorLeft,
    /// Move cursor right one character (Right Arrow / Ctrl+F)
    CursorRight,
    /// Move cursor left one word (Ctrl+Left / Alt+B)
    WordLeft,
    /// Move cursor right one word (Ctrl+Right / Alt+F)
    WordRight,

    // Editing
    /// Delete from cursor to line end (Ctrl+K)
    DeleteToLineEnd,
    /// Delete from cursor to line start (Ctrl+U)
    DeleteToLineStart,
    /// Delete previous word (Ctrl+Backspace / Alt+Backspace / Ctrl+W)
    DeletePrevWord,
    /// Delete next word (Ctrl+Delete / Alt+Delete)
    DeleteNextWord,
    /// Delete character to the left (Backspace / Ctrl+H)
    Backspace,
    /// Delete character to the right (Delete / Ctrl+D)
    DeleteChar,
    /// Undo most recent edit (Ctrl+Z)
    Undo,
    /// Redo most recent undone edit (Ctrl+Shift+Z)
    Redo,

    // Scrolling
    /// Scroll content up (Shift+Up)
    ScrollUp,
    /// Scroll content down (Shift+Down)
    ScrollDown,
    /// Scroll to top (Ctrl+Home / Shift+Home)
    ScrollTop,
    /// Scroll to bottom (Ctrl+End / Shift+End)
    ScrollBottom,
    /// Scroll up one page (Page Up)
    PageUp,
    /// Scroll down one page (Page Down)
    PageDown,

    // History & Search
    /// Show previous history entry (Ctrl+P)
    PrevHistory,
    /// Show next history entry (Ctrl+N)
    NextHistory,
    /// Start reverse search through history (Ctrl+R)
    ReverseSearch,

    // Text Input
    /// Insert newline without submitting (Ctrl+Enter / Cmd+Enter / Alt+Enter / Shift+Enter / Ctrl+J)
    InsertNewline,
    /// Open prompt in external editor (Ctrl+X / Meta+Enter)
    ExternalEditor,
    /// Paste from clipboard (Ctrl+V / Cmd+V / Alt+V)
    Paste,

    // App Controls
    /// Clear terminal screen and redraw UI (Ctrl+L)
    ClearScreen,
    /// Toggle YOLO mode (Ctrl+Y)
    ToggleYolo,
    /// Cycle through approval modes (Shift+Tab)
    CycleApprovalMode,
    /// Toggle full TODO list / Toggle MCP tool descriptions (Ctrl+T)
    ToggleTodo,
    /// Toggle copy mode or expand response (Ctrl+S)
    ToggleCopyMode,
    /// Show IDE context details (Ctrl+G)
    ShowIdeContext,
    /// Toggle Markdown rendering (Alt+M)
    ToggleMarkdown,
    /// Toggle detailed error information (F12)
    ToggleErrorDetails,
    /// Restart application (R key)
    Restart,
    /// Focus shell input from Gemini input (Tab)
    FocusShell,
    /// Enter/exit shell mode (! on empty prompt)
    ToggleShellMode,
}

/// Special input symbols.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GeminiSpecialInput {
    /// File/directory injection: @<path>
    FileReference(String),
    /// Shell command execution: !<command>
    ShellCommand(String),
}

impl GeminiCommand {
    /// Convert command to string to send via PTY.
    pub fn to_pty_input(&self) -> String {
        let cmd = match self {
            // Help & Information
            Self::Help => "/help".to_string(),
            Self::About => "/about".to_string(),
            Self::Stats => "/stats".to_string(),
            Self::Privacy => "/privacy".to_string(),

            // Chat Management
            Self::Chat { subcommand } => {
                if let Some(sub) = subcommand {
                    match sub {
                        ChatSubcommand::Save { tag } => format!("/chat save {}", tag),
                        ChatSubcommand::Resume { tag } => format!("/chat resume {}", tag),
                        ChatSubcommand::List => "/chat list".to_string(),
                        ChatSubcommand::Delete { tag } => format!("/chat delete {}", tag),
                        ChatSubcommand::Share { filename } => format!("/chat share {}", filename),
                    }
                } else {
                    "/chat".to_string()
                }
            }

            // Session Management
            Self::Resume => "/resume".to_string(),
            Self::Clear => "/clear".to_string(),
            Self::Compress => "/compress".to_string(),

            // Memory & Context
            Self::Memory { subcommand } => {
                if let Some(sub) = subcommand {
                    match sub {
                        MemorySubcommand::Show => "/memory show".to_string(),
                        MemorySubcommand::Add { text } => format!("/memory add {}", text),
                        MemorySubcommand::Refresh => "/memory refresh".to_string(),
                        MemorySubcommand::List => "/memory list".to_string(),
                        MemorySubcommand::Remove { text } => format!("/memory remove {}", text),
                    }
                } else {
                    "/memory".to_string()
                }
            }

            // Workspace Management
            Self::Directory { subcommand } => {
                if let Some(sub) = subcommand {
                    match sub {
                        DirectorySubcommand::Add { paths } => {
                            format!("/directory add {}", paths.join(","))
                        }
                        DirectorySubcommand::Show => "/directory show".to_string(),
                    }
                } else {
                    "/directory".to_string()
                }
            }

            // MCP Server Management
            Self::Mcp { subcommand } => {
                if let Some(sub) = subcommand {
                    match sub {
                        McpSubcommand::List => "/mcp list".to_string(),
                        McpSubcommand::Desc => "/mcp desc".to_string(),
                        McpSubcommand::Schema => "/mcp schema".to_string(),
                        McpSubcommand::Auth { server_name } => {
                            if let Some(name) = server_name {
                                format!("/mcp auth {}", name)
                            } else {
                                "/mcp auth".to_string()
                            }
                        }
                        McpSubcommand::Refresh => "/mcp refresh".to_string(),
                    }
                } else {
                    "/mcp".to_string()
                }
            }

            // Tools & Extensions
            Self::Tools { subcommand } => {
                if let Some(sub) = subcommand {
                    match sub {
                        ToolsSubcommand::Desc => "/tools desc".to_string(),
                        ToolsSubcommand::NoDesc => "/tools nodesc".to_string(),
                    }
                } else {
                    "/tools".to_string()
                }
            }
            Self::Extensions => "/extensions".to_string(),

            // Agent Skills
            Self::Skills { subcommand } => {
                if let Some(sub) = subcommand {
                    match sub {
                        SkillsSubcommand::List => "/skills list".to_string(),
                        SkillsSubcommand::Enable { name } => format!("/skills enable {}", name),
                        SkillsSubcommand::Disable { name } => format!("/skills disable {}", name),
                        SkillsSubcommand::Reload => "/skills reload".to_string(),
                    }
                } else {
                    "/skills".to_string()
                }
            }

            // Checkpointing
            Self::Restore { tool_call_id } => {
                if let Some(id) = tool_call_id {
                    format!("/restore {}", id)
                } else {
                    "/restore".to_string()
                }
            }

            // Configuration
            Self::Settings => "/settings".to_string(),
            Self::Theme => "/theme".to_string(),
            Self::Auth => "/auth".to_string(),
            Self::Model => "/model".to_string(),
            Self::Editor => "/editor".to_string(),

            // Utilities
            Self::Copy => "/copy".to_string(),
            Self::Bug { text } => {
                if let Some(t) = text {
                    format!("/bug {}", t)
                } else {
                    "/bug".to_string()
                }
            }
            Self::Init => "/init".to_string(),
            Self::Vim => "/vim".to_string(),
            Self::Quit => "/quit".to_string(),
            Self::Exit => "/exit".to_string(),
        };

        format!("{}\n", cmd)
    }
}

impl GeminiControl {
    /// Convert control sequence to bytes to send via PTY.
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            // Basic Controls
            Self::Confirm => vec![0x0D],                    // Enter
            Self::Dismiss => vec![0x1B],                    // Esc
            Self::Cancel => vec![0x03],                     // Ctrl+C
            Self::Exit => vec![0x04],                       // Ctrl+D
            Self::DoubleEsc => vec![0x1B, 0x1B],           // Esc Esc

            // Cursor Movement
            Self::LineStart => vec![0x01],                  // Ctrl+A
            Self::LineEnd => vec![0x05],                    // Ctrl+E
            Self::CursorUp => vec![0x1B, 0x5B, 0x41],      // Up Arrow (ESC [ A)
            Self::CursorDown => vec![0x1B, 0x5B, 0x42],    // Down Arrow (ESC [ B)
            Self::CursorLeft => vec![0x1B, 0x5B, 0x44],    // Left Arrow (ESC [ D)
            Self::CursorRight => vec![0x1B, 0x5B, 0x43],   // Right Arrow (ESC [ C)
            Self::WordLeft => vec![0x1B, 0x62],            // Alt+B (ESC b)
            Self::WordRight => vec![0x1B, 0x66],           // Alt+F (ESC f)

            // Editing
            Self::DeleteToLineEnd => vec![0x0B],           // Ctrl+K
            Self::DeleteToLineStart => vec![0x15],         // Ctrl+U
            Self::DeletePrevWord => vec![0x17],            // Ctrl+W
            Self::DeleteNextWord => vec![0x1B, 0x64],      // Alt+D (ESC d)
            Self::Backspace => vec![0x08],                 // Ctrl+H / Backspace
            Self::DeleteChar => vec![0x7F],                // Delete
            Self::Undo => vec![0x1A],                      // Ctrl+Z
            Self::Redo => vec![0x1B, 0x5B, 0x31, 0x3B, 0x32, 0x5A], // Ctrl+Shift+Z

            // Scrolling
            Self::ScrollUp => vec![0x1B, 0x5B, 0x31, 0x3B, 0x32, 0x41], // Shift+Up
            Self::ScrollDown => vec![0x1B, 0x5B, 0x31, 0x3B, 0x32, 0x42], // Shift+Down
            Self::ScrollTop => vec![0x1B, 0x5B, 0x31, 0x7E], // Home
            Self::ScrollBottom => vec![0x1B, 0x5B, 0x34, 0x7E], // End
            Self::PageUp => vec![0x1B, 0x5B, 0x35, 0x7E],  // Page Up
            Self::PageDown => vec![0x1B, 0x5B, 0x36, 0x7E], // Page Down

            // History & Search
            Self::PrevHistory => vec![0x10],               // Ctrl+P
            Self::NextHistory => vec![0x0E],               // Ctrl+N
            Self::ReverseSearch => vec![0x12],             // Ctrl+R

            // Text Input
            Self::InsertNewline => vec![0x0A],             // Ctrl+J (LF)
            Self::ExternalEditor => vec![0x18],            // Ctrl+X
            Self::Paste => vec![0x16],                     // Ctrl+V

            // App Controls
            Self::ClearScreen => vec![0x0C],               // Ctrl+L
            Self::ToggleYolo => vec![0x19],                // Ctrl+Y
            Self::CycleApprovalMode => vec![0x1B, 0x5B, 0x5A], // Shift+Tab (ESC [ Z)
            Self::ToggleTodo => vec![0x14],                // Ctrl+T
            Self::ToggleCopyMode => vec![0x13],            // Ctrl+S
            Self::ShowIdeContext => vec![0x07],            // Ctrl+G
            Self::ToggleMarkdown => vec![0x1B, 0x6D],      // Alt+M (ESC m)
            Self::ToggleErrorDetails => vec![0x1B, 0x5B, 0x32, 0x34, 0x7E], // F12
            Self::Restart => vec![0x52],                   // R key
            Self::FocusShell => vec![0x09],                // Tab
            Self::ToggleShellMode => b"!\n".to_vec(),      // ! + Enter
        }
    }
}

impl GeminiSpecialInput {
    /// Convert special input to PTY format.
    pub fn to_pty_input(&self) -> String {
        match self {
            Self::FileReference(path) => format!("@{}", path),
            Self::ShellCommand(cmd) => format!("!{}\n", cmd),
        }
    }
}

/// Parsed responses from Gemini CLI.
#[derive(Debug, Clone, PartialEq)]
pub enum GeminiResponse {
    /// Welcome screen information
    WelcomeScreen {
        model: String,
        version: String,
    },

    /// Token usage information (API key auth only)
    TokenUsage {
        used: u64,
        remaining: u64,
        cached_savings: Option<u64>,
    },

    /// Rate limit hit
    RateLimitHit {
        retry_at: Option<String>,
        message: String,
    },

    /// Approval mode changed
    ApprovalModeChanged {
        mode: ApprovalMode,
    },

    /// Memory information
    MemoryInfo {
        entries: Vec<String>,
        file_paths: Vec<String>,
    },

    /// MCP server list
    McpServerList {
        servers: Vec<McpServer>,
    },

    /// Skills list
    SkillsList {
        skills: Vec<Skill>,
    },

    /// Status information from /stats
    StatusInfo {
        session_duration: Option<String>,
        model: Option<String>,
        token_usage: Option<u64>,
        cached_tokens: Option<u64>,
    },

    /// Checkpoint information from /restore
    CheckpointInfo {
        checkpoints: Vec<Checkpoint>,
    },

    /// Session list
    SessionList {
        sessions: Vec<Session>,
    },

    /// Extension list
    ExtensionList {
        extensions: Vec<Extension>,
    },

    /// Model selection menu
    ModelMenu {
        current_model: String,
        available_models: Vec<String>,
    },

    /// Update available
    UpdateAvailable {
        current_version: String,
        new_version: String,
    },

    /// Context percentage indicator
    ContextUsage {
        percent_used: f32,
        tokens_used: Option<u64>,
        tokens_total: Option<u64>,
    },

    /// Tool execution approval request
    ToolApprovalRequest {
        tool_name: String,
        description: Option<String>,
    },

    /// Working indicator
    Working {
        message: String,
    },

    /// Unknown/raw output
    Raw(String),
}

/// MCP server information.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServer {
    pub name: String,
    pub status: McpServerStatus,
    pub tools: Vec<String>,
    pub description: Option<String>,
}

/// MCP server connection status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpServerStatus {
    Connected,
    Disconnected,
    Error,
}

/// Skill information (experimental feature).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub enabled: bool,
    pub description: Option<String>,
}

/// Checkpoint information.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    pub id: String,
    pub timestamp: String,
    pub filename: String,
    pub tool_name: String,
}

/// Session information.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    pub index: Option<usize>,
    pub created_at: Option<String>,
    pub message_count: Option<u64>,
}

/// Extension information.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extension {
    pub name: String,
    pub version: Option<String>,
    pub enabled: bool,
    pub source: Option<String>,
}

/// Parser for Gemini CLI output.
pub struct GeminiOutputParser {
    buffer: String,
    /// Accumulates partial AI response text during streaming.
    pending_response: String,
    /// Set to `true` when a prompt marker appears, indicating the response is complete.
    response_complete: bool,
}

impl GeminiOutputParser {
    /// Create a new parser.
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
            pending_response: String::new(),
            response_complete: false,
        }
    }

    /// Returns the currently accumulated partial response text.
    pub fn pending_response(&self) -> &str {
        &self.pending_response
    }

    /// Returns true if the last response is complete.
    pub fn is_response_complete(&self) -> bool {
        self.response_complete
    }

    /// Feed data into the parser.
    pub fn feed(&mut self, data: &str) {
        self.buffer.push_str(data);
    }

    /// Parse buffered data and return detected responses.
    /// Clears the buffer after parsing.
    pub fn parse(&mut self) -> Vec<GeminiResponse> {
        let responses = Self::parse_text(&self.buffer);
        self.buffer.clear();
        responses
    }

    /// Parse text without buffering.
    pub fn parse_text(text: &str) -> Vec<GeminiResponse> {
        let mut responses = Vec::new();

        // Welcome screen
        if let Some(response) = Self::parse_welcome_screen(text) {
            responses.push(response);
        }

        // Token usage
        if let Some(response) = Self::parse_token_usage(text) {
            responses.push(response);
        }

        // Rate limit
        if let Some(response) = Self::parse_rate_limit(text) {
            responses.push(response);
        }

        // Approval mode changed
        if let Some(response) = Self::parse_approval_mode(text) {
            responses.push(response);
        }

        // Memory info
        if let Some(response) = Self::parse_memory_info(text) {
            responses.push(response);
        }

        // MCP server list
        if let Some(response) = Self::parse_mcp_list(text) {
            responses.push(response);
        }

        // Skills list
        if let Some(response) = Self::parse_skills_list(text) {
            responses.push(response);
        }

        // Status info
        if let Some(response) = Self::parse_status_info(text) {
            responses.push(response);
        }

        // Checkpoint info
        if let Some(response) = Self::parse_checkpoint_info(text) {
            responses.push(response);
        }

        // Session list
        if let Some(response) = Self::parse_session_list(text) {
            responses.push(response);
        }

        // Extension list
        if let Some(response) = Self::parse_extension_list(text) {
            responses.push(response);
        }

        // Model menu
        if let Some(response) = Self::parse_model_menu(text) {
            responses.push(response);
        }

        // Update available
        if let Some(response) = Self::parse_update_available(text) {
            responses.push(response);
        }

        // Context usage
        if let Some(response) = Self::parse_context_usage(text) {
            responses.push(response);
        }

        // Tool approval request
        if let Some(response) = Self::parse_tool_approval(text) {
            responses.push(response);
        }

        // Working indicator
        if let Some(response) = Self::parse_working_indicator(text) {
            responses.push(response);
        }

        // If nothing specific was parsed, return raw
        if responses.is_empty() && !text.trim().is_empty() {
            responses.push(GeminiResponse::Raw(text.to_string()));
        }

        responses
    }

    /// Parse welcome screen.
    /// Patterns:
    /// - "Gemini CLI (v0.23.0)"
    /// - "Model: gemini-2.5-pro"
    fn parse_welcome_screen(text: &str) -> Option<GeminiResponse> {
        static WELCOME_RE: OnceLock<Regex> = OnceLock::new();
        let re = WELCOME_RE.get_or_init(|| {
            Regex::new(r"Gemini CLI \(v([\d.]+)\)").unwrap()
        });

        let version = re.captures(text)
            .and_then(|cap| cap.get(1))
            .map(|m| m.as_str().to_string())?;

        // Extract model
        static MODEL_RE: OnceLock<Regex> = OnceLock::new();
        let model_re = MODEL_RE.get_or_init(|| {
            Regex::new(r"Model:\s*(\S+)").unwrap()
        });

        let model = model_re.captures(text)
            .and_then(|cap| cap.get(1))
            .map(|m| m.as_str().to_string())
            .or_else(|| {
                // Fallback: look for common model names
                if text.contains("gemini-2.5-pro") {
                    Some("gemini-2.5-pro".to_string())
                } else if text.contains("gemini-2.5-flash") {
                    Some("gemini-2.5-flash".to_string())
                } else if text.contains("gemini-2.0-flash") {
                    Some("gemini-2.0-flash".to_string())
                } else {
                    None
                }
            })?;

        Some(GeminiResponse::WelcomeScreen { model, version })
    }

    /// Parse token usage from /stats output.
    /// Patterns (API key auth only):
    /// - "Token usage: 12,345 used, 87,655 remaining"
    /// - "Cached token savings: 5,000"
    fn parse_token_usage(text: &str) -> Option<GeminiResponse> {
        static USAGE_RE: OnceLock<Regex> = OnceLock::new();
        let re = USAGE_RE.get_or_init(|| {
            Regex::new(r"Token usage:\s*([\d,]+)\s*used,\s*([\d,]+)\s*remaining").unwrap()
        });

        let caps = re.captures(text)?;
        let used = caps.get(1)?
            .as_str()
            .replace(",", "")
            .parse::<u64>()
            .ok()?;
        let remaining = caps.get(2)?
            .as_str()
            .replace(",", "")
            .parse::<u64>()
            .ok()?;

        // Check for cached token savings
        static CACHED_RE: OnceLock<Regex> = OnceLock::new();
        let cached_re = CACHED_RE.get_or_init(|| {
            Regex::new(r"Cached token savings:\s*([\d,]+)").unwrap()
        });

        let cached_savings = cached_re.captures(text)
            .and_then(|cap| cap.get(1))
            .and_then(|m| m.as_str().replace(",", "").parse::<u64>().ok());

        Some(GeminiResponse::TokenUsage {
            used,
            remaining,
            cached_savings,
        })
    }

    /// Parse rate limit message.
    /// Patterns:
    /// - "Rate limit exceeded"
    /// - "Try again at 2026-01-26 15:30:00 UTC"
    fn parse_rate_limit(text: &str) -> Option<GeminiResponse> {
        static RATE_LIMIT_RE: OnceLock<Regex> = OnceLock::new();
        let re = RATE_LIMIT_RE.get_or_init(|| {
            Regex::new(r"(?i)rate limit|quota exceeded").unwrap()
        });

        if !re.is_match(text) {
            return None;
        }

        // Extract retry time
        static RETRY_RE: OnceLock<Regex> = OnceLock::new();
        let retry_re = RETRY_RE.get_or_init(|| {
            Regex::new(r"(?i)try again (?:at|in)\s+([^\n]+)").unwrap()
        });

        let retry_at = retry_re.captures(text)
            .and_then(|cap| cap.get(1))
            .map(|m| m.as_str().trim().to_string());

        Some(GeminiResponse::RateLimitHit {
            retry_at,
            message: text.to_string(),
        })
    }

    /// Parse approval mode change.
    /// Patterns:
    /// - "Approval mode: default"
    /// - "Approval mode: auto_edit"
    /// - "Approval mode: yolo"
    /// - "Approval mode: plan"
    fn parse_approval_mode(text: &str) -> Option<GeminiResponse> {
        static APPROVAL_RE: OnceLock<Regex> = OnceLock::new();
        let re = APPROVAL_RE.get_or_init(|| {
            Regex::new(r"Approval mode:\s*(\w+)").unwrap()
        });

        let mode_str = re.captures(text)
            .and_then(|cap| cap.get(1))
            .map(|m| m.as_str())?;

        let mode = match mode_str.to_lowercase().as_str() {
            "default" => ApprovalMode::Default,
            "auto_edit" | "autoedit" => ApprovalMode::AutoEdit,
            "yolo" => ApprovalMode::Yolo,
            "plan" => ApprovalMode::Plan,
            _ => return None,
        };

        Some(GeminiResponse::ApprovalModeChanged { mode })
    }

    /// Parse memory information from /memory show or /memory list.
    /// Patterns:
    /// - File paths: "~/.gemini/GEMINI.md"
    /// - Entry content (markdown)
    fn parse_memory_info(text: &str) -> Option<GeminiResponse> {
        // Check if this is memory-related output
        if !text.contains("GEMINI.md") && !text.contains("/memory") {
            return None;
        }

        let mut file_paths = Vec::new();
        let mut entries = Vec::new();

        // Extract file paths
        static PATH_RE: OnceLock<Regex> = OnceLock::new();
        let path_re = PATH_RE.get_or_init(|| {
            Regex::new(r"([~/.][\w/.-]*GEMINI\.md)").unwrap()
        });

        for cap in path_re.captures_iter(text) {
            if let Some(path) = cap.get(1) {
                file_paths.push(path.as_str().to_string());
            }
        }

        // If we found paths, this is memory info
        if !file_paths.is_empty() {
            // Try to extract content entries (lines starting with #, -, or bullet points)
            for line in text.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with('#') || trimmed.starts_with('-') || trimmed.starts_with('*') {
                    entries.push(trimmed.to_string());
                }
            }

            Some(GeminiResponse::MemoryInfo {
                entries,
                file_paths,
            })
        } else {
            None
        }
    }

    /// Parse MCP server list from /mcp or /mcp list.
    /// Patterns:
    /// - Server name with status
    /// - Available tools
    fn parse_mcp_list(text: &str) -> Option<GeminiResponse> {
        // Check if this is MCP server list output
        if !text.contains("MCP") && !text.contains("server") {
            return None;
        }

        // This is a simplified parser - in reality, you'd need to parse
        // the actual table format from Gemini CLI output
        let mut servers = Vec::new();

        // Look for server entries (example pattern, adjust based on actual output)
        static SERVER_RE: OnceLock<Regex> = OnceLock::new();
        let server_re = SERVER_RE.get_or_init(|| {
            Regex::new(r"(\w+)\s+(connected|disconnected|error)").unwrap()
        });

        for cap in server_re.captures_iter(text) {
            let name = cap.get(1).map(|m| m.as_str().to_string()).unwrap_or_default();
            let status_str = cap.get(2).map(|m| m.as_str()).unwrap_or("");

            let status = match status_str.to_lowercase().as_str() {
                "connected" => McpServerStatus::Connected,
                "disconnected" => McpServerStatus::Disconnected,
                "error" => McpServerStatus::Error,
                _ => McpServerStatus::Disconnected,
            };

            servers.push(McpServer {
                name,
                status,
                tools: Vec::new(), // Would need more parsing to extract tools
                description: None,
            });
        }

        if servers.is_empty() {
            None
        } else {
            Some(GeminiResponse::McpServerList { servers })
        }
    }

    /// Parse skills list from /skills list.
    fn parse_skills_list(text: &str) -> Option<GeminiResponse> {
        // Check if this is skills list output
        if !text.contains("skill") && !text.contains("Skills") {
            return None;
        }

        let mut skills = Vec::new();

        // Look for skill entries
        static SKILL_RE: OnceLock<Regex> = OnceLock::new();
        let skill_re = SKILL_RE.get_or_init(|| {
            Regex::new(r"(\w[\w-]+)\s+(enabled|disabled)").unwrap()
        });

        for cap in skill_re.captures_iter(text) {
            let name = cap.get(1).map(|m| m.as_str().to_string()).unwrap_or_default();
            let enabled = cap.get(2).map(|m| m.as_str() == "enabled").unwrap_or(false);

            skills.push(Skill {
                name,
                enabled,
                description: None,
            });
        }

        if skills.is_empty() {
            None
        } else {
            Some(GeminiResponse::SkillsList { skills })
        }
    }

    /// Parse status information from /stats.
    fn parse_status_info(text: &str) -> Option<GeminiResponse> {
        // Look for session duration, token usage, etc.
        static DURATION_RE: OnceLock<Regex> = OnceLock::new();
        let duration_re = DURATION_RE.get_or_init(|| {
            Regex::new(r"Session duration:\s*([^\n]+)").unwrap()
        });

        let session_duration = duration_re.captures(text)
            .and_then(|cap| cap.get(1))
            .map(|m| m.as_str().trim().to_string());

        static MODEL_RE: OnceLock<Regex> = OnceLock::new();
        let model_re = MODEL_RE.get_or_init(|| {
            Regex::new(r"Model:\s*(\S+)").unwrap()
        });

        let model = model_re.captures(text)
            .and_then(|cap| cap.get(1))
            .map(|m| m.as_str().to_string());

        static TOKEN_RE: OnceLock<Regex> = OnceLock::new();
        let token_re = TOKEN_RE.get_or_init(|| {
            Regex::new(r"Total tokens:\s*([\d,]+)").unwrap()
        });

        let token_usage = token_re.captures(text)
            .and_then(|cap| cap.get(1))
            .and_then(|m| m.as_str().replace(",", "").parse::<u64>().ok());

        // Only return if we found at least one piece of info
        if session_duration.is_some() || model.is_some() || token_usage.is_some() {
            Some(GeminiResponse::StatusInfo {
                session_duration,
                model,
                token_usage,
                cached_tokens: None,
            })
        } else {
            None
        }
    }

    /// Parse checkpoint list from /restore.
    fn parse_checkpoint_info(text: &str) -> Option<GeminiResponse> {
        // Check if this is restore/checkpoint output
        if !text.contains("checkpoint") && !text.contains("restore") {
            return None;
        }

        let mut checkpoints = Vec::new();

        // Parse checkpoint entries
        // Format: "2025-06-22T10-00-00_000Z-my-file.txt-write_file"
        static CHECKPOINT_RE: OnceLock<Regex> = OnceLock::new();
        let checkpoint_re = CHECKPOINT_RE.get_or_init(|| {
            Regex::new(r"(\d{4}-\d{2}-\d{2}T\d{2}-\d{2}-\d{2}_\d{3}Z)-(.+?)-(\w+)").unwrap()
        });

        for cap in checkpoint_re.captures_iter(text) {
            let timestamp = cap.get(1).map(|m| m.as_str().to_string()).unwrap_or_default();
            let filename = cap.get(2).map(|m| m.as_str().to_string()).unwrap_or_default();
            let tool_name = cap.get(3).map(|m| m.as_str().to_string()).unwrap_or_default();
            let id = cap.get(0).map(|m| m.as_str().to_string()).unwrap_or_default();

            checkpoints.push(Checkpoint {
                id,
                timestamp,
                filename,
                tool_name,
            });
        }

        if checkpoints.is_empty() {
            None
        } else {
            Some(GeminiResponse::CheckpointInfo { checkpoints })
        }
    }

    /// Parse session list from --list-sessions.
    fn parse_session_list(text: &str) -> Option<GeminiResponse> {
        // Check if this is session list output
        if !text.contains("session") && !text.contains("Session") {
            return None;
        }

        let mut sessions = Vec::new();

        // Parse session entries
        static SESSION_RE: OnceLock<Regex> = OnceLock::new();
        let session_re = SESSION_RE.get_or_init(|| {
            Regex::new(r"(\d+)\.\s+([a-f0-9-]+)\s+(.+?)(?:\n|$)").unwrap()
        });

        for cap in session_re.captures_iter(text) {
            let index = cap.get(1).and_then(|m| m.as_str().parse::<usize>().ok());
            let id = cap.get(2).map(|m| m.as_str().to_string()).unwrap_or_default();
            let details = cap.get(3).map(|m| m.as_str()).unwrap_or("");

            // Try to extract creation time and message count from details
            let created_at = if details.contains("created") {
                Some(details.to_string())
            } else {
                None
            };

            sessions.push(Session {
                id,
                index,
                created_at,
                message_count: None,
            });
        }

        if sessions.is_empty() {
            None
        } else {
            Some(GeminiResponse::SessionList { sessions })
        }
    }

    /// Parse extension list from /extensions.
    fn parse_extension_list(text: &str) -> Option<GeminiResponse> {
        // Check if this is extension list output
        if !text.contains("extension") && !text.contains("Extension") {
            return None;
        }

        let mut extensions = Vec::new();

        // Parse extension entries
        static EXT_RE: OnceLock<Regex> = OnceLock::new();
        let ext_re = EXT_RE.get_or_init(|| {
            Regex::new(r"([\w-]+)(?:\s+v?([\d.]+))?\s+(enabled|disabled)").unwrap()
        });

        for cap in ext_re.captures_iter(text) {
            let name = cap.get(1).map(|m| m.as_str().to_string()).unwrap_or_default();
            let version = cap.get(2).map(|m| m.as_str().to_string());
            let enabled = cap.get(3).map(|m| m.as_str() == "enabled").unwrap_or(false);

            extensions.push(Extension {
                name,
                version,
                enabled,
                source: None,
            });
        }

        if extensions.is_empty() {
            None
        } else {
            Some(GeminiResponse::ExtensionList { extensions })
        }
    }

    /// Parse model selection menu from /model.
    fn parse_model_menu(text: &str) -> Option<GeminiResponse> {
        // Check if this is model selection output
        if !text.contains("model") || !text.contains("select") {
            return None;
        }

        static CURRENT_RE: OnceLock<Regex> = OnceLock::new();
        let current_re = CURRENT_RE.get_or_init(|| {
            Regex::new(r"current:\s*(\S+)").unwrap()
        });

        let current_model = current_re.captures(text)
            .and_then(|cap| cap.get(1))
            .map(|m| m.as_str().to_string())?;

        // Extract available models
        let mut available_models = Vec::new();
        static MODEL_RE: OnceLock<Regex> = OnceLock::new();
        let model_re = MODEL_RE.get_or_init(|| {
            Regex::new(r"gemini-[\d.]+-[\w-]+").unwrap()
        });

        for cap in model_re.captures_iter(text) {
            if let Some(model) = cap.get(0) {
                let model_name = model.as_str().to_string();
                if !available_models.contains(&model_name) {
                    available_models.push(model_name);
                }
            }
        }

        Some(GeminiResponse::ModelMenu {
            current_model,
            available_models,
        })
    }

    /// Parse update available notification.
    fn parse_update_available(text: &str) -> Option<GeminiResponse> {
        static UPDATE_RE: OnceLock<Regex> = OnceLock::new();
        let re = UPDATE_RE.get_or_init(|| {
            Regex::new(r"Update available.*?([\d.]+)\s*->\s*([\d.]+)").unwrap()
        });

        re.captures(text).map(|cap| {
            GeminiResponse::UpdateAvailable {
                current_version: cap.get(1).unwrap().as_str().to_string(),
                new_version: cap.get(2).unwrap().as_str().to_string(),
            }
        })
    }

    /// Parse context usage information.
    fn parse_context_usage(text: &str) -> Option<GeminiResponse> {
        static CONTEXT_RE: OnceLock<Regex> = OnceLock::new();
        let re = CONTEXT_RE.get_or_init(|| {
            Regex::new(r"Context:\s*([\d.]+)%|(\d+)%\s*used").unwrap()
        });

        re.captures(text).and_then(|cap| {
            let percent_used = cap.get(1)
                .or_else(|| cap.get(2))
                .and_then(|m| m.as_str().parse::<f32>().ok())?;

            Some(GeminiResponse::ContextUsage {
                percent_used,
                tokens_used: None,
                tokens_total: None,
            })
        })
    }

    /// Parse tool approval request.
    fn parse_tool_approval(text: &str) -> Option<GeminiResponse> {
        // Check for tool approval patterns
        if !text.contains("approve") && !text.contains("Allow") {
            return None;
        }

        static TOOL_RE: OnceLock<Regex> = OnceLock::new();
        let tool_re = TOOL_RE.get_or_init(|| {
            Regex::new(r"(?:approve|allow)\s+(\w+)").unwrap()
        });

        tool_re.captures(text).map(|cap| {
            let tool_name = cap.get(1).unwrap().as_str().to_string();

            GeminiResponse::ToolApprovalRequest {
                tool_name,
                description: None,
            }
        })
    }

    /// Parse working indicator.
    fn parse_working_indicator(text: &str) -> Option<GeminiResponse> {
        if text.contains("Working") || text.contains("Processing") || text.contains("...") {
            Some(GeminiResponse::Working {
                message: text.to_string(),
            })
        } else {
            None
        }
    }

    /// Get the current buffer contents (for debugging).
    pub fn buffer(&self) -> &str {
        &self.buffer
    }

    /// Clear the buffer.
    pub fn clear(&mut self) {
        self.buffer.clear();
    }
}

impl Default for GeminiOutputParser {
    fn default() -> Self {
        Self::new()
    }
}

/// Top-level CLI commands (not slash commands).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GeminiTopLevelCommand {
    /// Start interactive REPL
    Interactive,

    /// Non-interactive mode with prompt
    Prompt { text: String },

    /// Start interactive session with initial prompt
    PromptInteractive { text: String },

    /// Resume session
    Resume { id: Option<String> },

    /// List all sessions
    ListSessions,

    /// Delete session
    DeleteSession { id: String },

    /// Extension management
    Extensions { action: ExtensionAction },

    /// MCP server management
    Mcp { action: McpTopLevelAction },

    /// Skills management
    Skills { action: SkillsAction },

    /// Display version
    Version,

    /// Display help
    Help,
}

/// Extension actions for top-level command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtensionAction {
    Install { source: String, ref_name: Option<String>, auto_update: bool, pre_release: bool },
    Uninstall { name: String },
    List,
    Update { name: Option<String> },
    Enable { name: String, scope: Option<String> },
    Disable { name: String, scope: Option<String> },
    New { path: String, template: Option<String> },
    Link { path: String },
}

/// MCP actions for top-level command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpTopLevelAction {
    Add { name: String, command_or_url: String, args: Vec<String>, transport: Option<String>, headers: Vec<String> },
    List,
    Remove { name: String },
    Enable { name: String },
    Disable { name: String },
}

/// Skills actions for top-level command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillsAction {
    Install { source: String, scope: Option<String> },
    Uninstall { name: String },
    Enable { name: String },
    Disable { name: String },
}

impl GeminiTopLevelCommand {
    /// Convert to command string.
    pub fn to_command_string(&self) -> String {
        match self {
            Self::Interactive => "gemini".to_string(),
            Self::Prompt { text } => format!("gemini -p \"{}\"", escape_quotes(text)),
            Self::PromptInteractive { text } => format!("gemini -i \"{}\"", escape_quotes(text)),
            Self::Resume { id } => {
                if let Some(session_id) = id {
                    format!("gemini --resume {}", session_id)
                } else {
                    "gemini --resume".to_string()
                }
            }
            Self::ListSessions => "gemini --list-sessions".to_string(),
            Self::DeleteSession { id } => format!("gemini --delete-session {}", id),
            Self::Extensions { action } => match action {
                ExtensionAction::Install { source, ref_name, auto_update, pre_release } => {
                    let mut cmd = format!("gemini extensions install {}", source);
                    if let Some(r) = ref_name {
                        cmd.push_str(&format!(" --ref {}", r));
                    }
                    if *auto_update {
                        cmd.push_str(" --auto-update");
                    }
                    if *pre_release {
                        cmd.push_str(" --pre-release");
                    }
                    cmd
                }
                ExtensionAction::Uninstall { name } => {
                    format!("gemini extensions uninstall {}", name)
                }
                ExtensionAction::List => "gemini extensions list".to_string(),
                ExtensionAction::Update { name } => {
                    if let Some(n) = name {
                        format!("gemini extensions update {}", n)
                    } else {
                        "gemini extensions update --all".to_string()
                    }
                }
                ExtensionAction::Enable { name, scope } => {
                    let mut cmd = format!("gemini extensions enable {}", name);
                    if let Some(s) = scope {
                        cmd.push_str(&format!(" --scope {}", s));
                    }
                    cmd
                }
                ExtensionAction::Disable { name, scope } => {
                    let mut cmd = format!("gemini extensions disable {}", name);
                    if let Some(s) = scope {
                        cmd.push_str(&format!(" --scope {}", s));
                    }
                    cmd
                }
                ExtensionAction::New { path, template } => {
                    let mut cmd = format!("gemini extensions new {}", path);
                    if let Some(t) = template {
                        cmd.push_str(&format!(" {}", t));
                    }
                    cmd
                }
                ExtensionAction::Link { path } => {
                    format!("gemini extensions link {}", path)
                }
            },
            Self::Mcp { action } => match action {
                McpTopLevelAction::Add { name, command_or_url, args, transport, headers } => {
                    let mut cmd = format!("gemini mcp add {}", name);
                    if let Some(t) = transport {
                        cmd.push_str(&format!(" --transport {}", t));
                    }
                    for header in headers {
                        cmd.push_str(&format!(" --header \"{}\"", escape_quotes(header)));
                    }
                    cmd.push_str(&format!(" {}", command_or_url));
                    for arg in args {
                        cmd.push_str(&format!(" {}", arg));
                    }
                    cmd
                }
                McpTopLevelAction::List => "gemini mcp list".to_string(),
                McpTopLevelAction::Remove { name } => format!("gemini mcp remove {}", name),
                McpTopLevelAction::Enable { name } => format!("gemini mcp enable {}", name),
                McpTopLevelAction::Disable { name } => format!("gemini mcp disable {}", name),
            },
            Self::Skills { action } => match action {
                SkillsAction::Install { source, scope } => {
                    let mut cmd = format!("gemini skills install {}", source);
                    if let Some(s) = scope {
                        cmd.push_str(&format!(" --scope {}", s));
                    }
                    cmd
                }
                SkillsAction::Uninstall { name } => {
                    format!("gemini skills uninstall {}", name)
                }
                SkillsAction::Enable { name } => format!("gemini skills enable {}", name),
                SkillsAction::Disable { name } => format!("gemini skills disable {}", name),
            },
            Self::Version => "gemini --version".to_string(),
            Self::Help => "gemini --help".to_string(),
        }
    }
}

/// Builder for constructing complex Gemini CLI command strings.
#[derive(Debug, Default)]
pub struct GeminiCommandBuilder {
    flags: HashMap<String, Option<String>>,
    prompt: Option<String>,
}

impl GeminiCommandBuilder {
    /// Create a new command builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the model to use.
    pub fn model(mut self, model: &str) -> Self {
        self.flags.insert("--model".to_string(), Some(model.to_string()));
        self
    }

    /// Set output format (text, json, stream-json).
    pub fn output_format(mut self, format: &str) -> Self {
        self.flags.insert("--output-format".to_string(), Some(format.to_string()));
        self
    }

    /// Enable screen reader mode.
    pub fn screen_reader(mut self) -> Self {
        self.flags.insert("--screen-reader".to_string(), None);
        self
    }

    /// Enable debug mode.
    pub fn debug(mut self) -> Self {
        self.flags.insert("--debug".to_string(), None);
        self
    }

    /// Enable YOLO mode.
    pub fn yolo(mut self) -> Self {
        self.flags.insert("--yolo".to_string(), None);
        self
    }

    /// Set approval mode.
    pub fn approval_mode(mut self, mode: ApprovalMode) -> Self {
        self.flags.insert("--approval-mode".to_string(), Some(mode.to_string()));
        self
    }

    /// Set allowed tools (comma-separated).
    pub fn allowed_tools(mut self, tools: &str) -> Self {
        self.flags.insert("--allowed-tools".to_string(), Some(tools.to_string()));
        self
    }

    /// Enable sandbox mode.
    pub fn sandbox(mut self) -> Self {
        self.flags.insert("--sandbox".to_string(), None);
        self
    }

    /// Include additional directories.
    pub fn include_directories(mut self, dirs: &str) -> Self {
        self.flags.insert("--include-directories".to_string(), Some(dirs.to_string()));
        self
    }

    /// Resume a session.
    pub fn resume(mut self, session: &str) -> Self {
        self.flags.insert("--resume".to_string(), Some(session.to_string()));
        self
    }

    /// Specify extensions to use.
    pub fn extensions(mut self, extensions: &str) -> Self {
        self.flags.insert("--extensions".to_string(), Some(extensions.to_string()));
        self
    }

    /// Set allowed MCP server names.
    pub fn allowed_mcp_servers(mut self, servers: &str) -> Self {
        self.flags.insert("--allowed-mcp-server-names".to_string(), Some(servers.to_string()));
        self
    }

    /// Set the initial prompt.
    pub fn prompt(mut self, prompt: &str) -> Self {
        self.prompt = Some(prompt.to_string());
        self
    }

    /// Build the command string.
    pub fn build(&self) -> String {
        let mut cmd = vec!["gemini".to_string()];

        // Add flags
        for (flag, value) in &self.flags {
            cmd.push(flag.clone());
            if let Some(val) = value {
                cmd.push(format!("\"{}\"", escape_quotes(val)));
            }
        }

        // Add prompt if using -p flag
        if let Some(prompt) = &self.prompt {
            cmd.push("-p".to_string());
            cmd.push(format!("\"{}\"", escape_quotes(prompt)));
        }

        cmd.join(" ")
    }
}

/// Escape quotes in a string for shell command.
fn escape_quotes(s: &str) -> String {
    s.replace('"', "\\\"")
}

// ---------------------------------------------------------------------------
// OutputParser trait implementation
// ---------------------------------------------------------------------------

impl OutputParser for GeminiOutputParser {
    fn feed(&mut self, data: &str) {
        self.buffer.push_str(data);
    }

    fn parse(&mut self) -> Vec<ParsedMessage> {
        let raw = std::mem::take(&mut self.buffer);
        let responses = Self::parse_text(&raw);
        self.response_complete = false;

        let mut messages = Vec::new();

        for resp in responses {
            let content = match &resp {
                GeminiResponse::Raw(s) => s.clone(),
                GeminiResponse::Working { message } => message.clone(),
                GeminiResponse::RateLimitHit { message, .. } => message.clone(),
                GeminiResponse::WelcomeScreen { model, version } => {
                    format!("Gemini CLI (v{}) Model: {}", version, model)
                }
                GeminiResponse::ApprovalModeChanged { mode } => {
                    format!("Approval mode: {}", mode)
                }
                GeminiResponse::ToolApprovalRequest { tool_name, .. } => {
                    format!("Approve {}", tool_name)
                }
                other => format!("{:?}", other),
            };

            let class = self.classify(&content);

            match class {
                MessageClass::AiResponse => {
                    self.pending_response.push_str(&content);
                    messages.push(ParsedMessage {
                        class: MessageClass::AiResponse,
                        content,
                        metadata: MessageMetadata {
                            tool: CliTool::Gemini,
                            is_partial: true,
                            ..Default::default()
                        },
                    });
                }
                MessageClass::PromptReady => {
                    // Emit the final accumulated response if any
                    if !self.pending_response.is_empty() {
                        let final_response = std::mem::take(&mut self.pending_response);
                        messages.push(ParsedMessage {
                            class: MessageClass::AiResponse,
                            content: final_response,
                            metadata: MessageMetadata {
                                tool: CliTool::Gemini,
                                is_partial: false,
                                ..Default::default()
                            },
                        });
                    }
                    self.response_complete = true;
                    // Also emit the PromptReady message itself
                    messages.push(ParsedMessage {
                        class: MessageClass::PromptReady,
                        content,
                        metadata: MessageMetadata {
                            tool: CliTool::Gemini,
                            ..Default::default()
                        },
                    });
                }
                _ => {
                    messages.push(ParsedMessage {
                        class,
                        content,
                        metadata: MessageMetadata {
                            tool: CliTool::Gemini,
                            ..Default::default()
                        },
                    });
                }
            }
        }

        messages
    }

    fn extract_ai_text(&self, raw_cleaned: &str) -> String {
        // Strip ANSI/VTE codes using the VTE parser
        let mut parser = VteParser::new();
        let cleaned = parser.parse(raw_cleaned);

        let mut result_lines = Vec::new();
        let mut in_ai_response = false;

        for line in cleaned.lines() {
            let trimmed = line.trim();

            // Skip empty lines at the start
            if !in_ai_response && trimmed.is_empty() {
                continue;
            }

            // Skip UI noise
            if trimmed.contains("███")
                || trimmed.contains("Tips for getting started")
                || trimmed.contains("GEMINI.md")
                || trimmed.contains("no sandbox")
                || trimmed.contains("sandbox")
                || trimmed.contains("Auto (Gemini")
                || trimmed.starts_with('┌')
                || trimmed.starts_with('└')
                || trimmed.starts_with('│')
                || trimmed.starts_with('─')
                || trimmed.starts_with('╭')
                || trimmed.starts_with('╰')
                || trimmed.starts_with('╯')
                || trimmed.starts_with('╮')
                || trimmed.contains("ctrl+")
                || trimmed.contains("shortcuts")
                || trimmed.contains("Ask questions")
                || trimmed.contains("Be specific")
                || trimmed.contains("help for more")
                || trimmed.contains("Initializing...")
                || trimmed.contains("Queued")
                || trimmed.contains("Navigate your prompt")
                || trimmed.contains("esc to cancel")
                || trimmed.contains("Up and Down arrows")
                || (trimmed.contains("press") && trimmed.contains("to edit"))
            {
                continue;
            }

            // Stop at prompt marker
            if trimmed.starts_with("> Type your message") {
                break;
            }

            // Skip thinking/processing indicators but mark we're in response mode
            if trimmed.contains("Thinking")
                || trimmed.contains("thinking")
                || trimmed.contains("Answering in Kind")
                || trimmed.contains("Crafting a Reply")
                || trimmed.contains("Composing")
            {
                in_ai_response = true;
                continue;
            }

            // Collect actual response content
            if !trimmed.is_empty() {
                let cleaned_line: String = trimmed
                    .chars()
                    .filter(|c| {
                        c.is_alphanumeric()
                            || c.is_whitespace()
                            || ".,!?-\u{2014}:;()[]{}<<>>\"'".contains(*c)
                            || (*c >= '\u{0400}' && *c <= '\u{04FF}') // Cyrillic
                    })
                    .collect();

                let final_line = cleaned_line.trim();
                if !final_line.is_empty()
                    && !final_line.starts_with("0s")
                    && !final_line.starts_with("1s")
                    && !final_line.ends_with("s)")
                {
                    in_ai_response = true;
                    result_lines.push(final_line.to_string());
                }
            }
        }

        let joined = result_lines.join(" ").trim().to_string();

        // Try to find where the actual response starts (after the prompt question mark)
        if let Some(question_pos) = joined.find('?') {
            let after_prompt = joined[question_pos + 1..].trim();
            if !after_prompt.is_empty() {
                return after_prompt.to_string();
            }
        }

        joined
    }

    fn classify(&self, text: &str) -> MessageClass {
        // ThinkingIndicator
        if text.contains("Working") || text.contains("Processing") || text.contains("Thinking") {
            return MessageClass::ThinkingIndicator;
        }

        // ToolApproval
        if text.contains("approve") || text.contains("Allow") {
            return MessageClass::ToolApproval;
        }

        // Error (rate limit / quota)
        if text.contains("rate limit") || text.contains("quota exceeded") {
            return MessageClass::Error;
        }

        // InfoMessage (approval mode info)
        if text.contains("Approval mode:") {
            return MessageClass::InfoMessage;
        }

        // UiElement: ASCII art progress bars
        if text.contains("███") {
            return MessageClass::UiElement;
        }

        // UiElement: box-drawing characters
        if text.contains('╭')
            || text.contains('╰')
            || text.contains('│')
            || text.contains('─')
        {
            return MessageClass::UiElement;
        }

        // UiElement: tips, sandbox, GEMINI.md noise
        if text.contains("Tips for getting started")
            || text.contains("no sandbox")
            || text.contains("GEMINI.md")
        {
            return MessageClass::UiElement;
        }

        // PromptReady
        if text.contains("Type your message") {
            return MessageClass::PromptReady;
        }

        MessageClass::Raw
    }

    fn buffer(&self) -> &str {
        &self.buffer
    }

    fn clear(&mut self) {
        self.buffer.clear();
    }

    fn tool(&self) -> CliTool {
        CliTool::Gemini
    }
}

// ---------------------------------------------------------------------------
// GeminiPromptSubmitter
// ---------------------------------------------------------------------------

/// Submits prompts and commands to the Gemini CLI via PTY.
pub struct GeminiPromptSubmitter;

impl GeminiPromptSubmitter {
    /// Create a new submitter instance.
    pub fn new() -> Self {
        Self
    }
}

impl Default for GeminiPromptSubmitter {
    fn default() -> Self {
        Self::new()
    }
}

impl PromptSubmitter for GeminiPromptSubmitter {
    /// Send a prompt: text + Tab (focus submit) + Enter (submit).
    fn send_prompt(&self, writer: &mut dyn io::Write, prompt: &str) -> io::Result<()> {
        writer.write_all(prompt.as_bytes())?;
        writer.write_all(b"\t")?;
        writer.write_all(b"\r")?;
        writer.flush()
    }

    /// Send a slash-command: text + Enter.
    fn send_command(&self, writer: &mut dyn io::Write, command: &str) -> io::Result<()> {
        writer.write_all(command.as_bytes())?;
        writer.write_all(b"\r")?;
        writer.flush()
    }

    /// Send raw control bytes.
    fn send_control(&self, writer: &mut dyn io::Write, bytes: &[u8]) -> io::Result<()> {
        writer.write_all(bytes)?;
        writer.flush()
    }

    /// Detect whether the CLI is ready for input.
    fn handle_startup(&self, output: &str) -> StartupAction {
        if output.contains("Type your message") {
            StartupAction::Ready
        } else {
            StartupAction::Waiting
        }
    }

    fn tool(&self) -> CliTool {
        CliTool::Gemini
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_command_to_pty_input() {
        assert_eq!(GeminiCommand::Help.to_pty_input(), "/help\n");
        assert_eq!(GeminiCommand::Stats.to_pty_input(), "/stats\n");
        assert_eq!(GeminiCommand::Clear.to_pty_input(), "/clear\n");
    }

    #[test]
    fn test_memory_commands() {
        assert_eq!(
            GeminiCommand::Memory { subcommand: Some(MemorySubcommand::Show) }.to_pty_input(),
            "/memory show\n"
        );
        assert_eq!(
            GeminiCommand::Memory {
                subcommand: Some(MemorySubcommand::Add {
                    text: "test".to_string()
                })
            }
            .to_pty_input(),
            "/memory add test\n"
        );
    }

    #[test]
    fn test_chat_commands() {
        assert_eq!(
            GeminiCommand::Chat {
                subcommand: Some(ChatSubcommand::Save {
                    tag: "checkpoint1".to_string()
                })
            }
            .to_pty_input(),
            "/chat save checkpoint1\n"
        );
        assert_eq!(
            GeminiCommand::Chat {
                subcommand: Some(ChatSubcommand::List)
            }
            .to_pty_input(),
            "/chat list\n"
        );
    }

    #[test]
    fn test_mcp_commands() {
        assert_eq!(
            GeminiCommand::Mcp { subcommand: Some(McpSubcommand::List) }.to_pty_input(),
            "/mcp list\n"
        );
        assert_eq!(
            GeminiCommand::Mcp {
                subcommand: Some(McpSubcommand::Auth {
                    server_name: Some("github".to_string())
                })
            }
            .to_pty_input(),
            "/mcp auth github\n"
        );
    }

    #[test]
    fn test_skills_commands() {
        assert_eq!(
            GeminiCommand::Skills { subcommand: Some(SkillsSubcommand::List) }.to_pty_input(),
            "/skills list\n"
        );
        assert_eq!(
            GeminiCommand::Skills {
                subcommand: Some(SkillsSubcommand::Enable {
                    name: "skill-creator".to_string()
                })
            }
            .to_pty_input(),
            "/skills enable skill-creator\n"
        );
    }

    #[test]
    fn test_control_to_bytes() {
        assert_eq!(GeminiControl::Cancel.to_bytes(), vec![0x03]);
        assert_eq!(GeminiControl::Exit.to_bytes(), vec![0x04]);
        assert_eq!(GeminiControl::ClearScreen.to_bytes(), vec![0x0C]);
        assert_eq!(GeminiControl::ToggleYolo.to_bytes(), vec![0x19]);
    }

    #[test]
    fn test_special_input() {
        assert_eq!(
            GeminiSpecialInput::FileReference("src/main.rs".to_string()).to_pty_input(),
            "@src/main.rs"
        );
        assert_eq!(
            GeminiSpecialInput::ShellCommand("git status".to_string()).to_pty_input(),
            "!git status\n"
        );
    }

    #[test]
    fn test_parse_welcome_screen() {
        let text = "Gemini CLI (v0.23.0)\nModel: gemini-2.5-pro";
        let response = GeminiOutputParser::parse_welcome_screen(text).unwrap();

        match response {
            GeminiResponse::WelcomeScreen { model, version } => {
                assert_eq!(version, "0.23.0");
                assert_eq!(model, "gemini-2.5-pro");
            }
            _ => panic!("Expected WelcomeScreen"),
        }
    }

    #[test]
    fn test_parse_token_usage() {
        let text = "Token usage: 12,345 used, 87,655 remaining\nCached token savings: 5,000";
        let response = GeminiOutputParser::parse_token_usage(text).unwrap();

        match response {
            GeminiResponse::TokenUsage { used, remaining, cached_savings } => {
                assert_eq!(used, 12345);
                assert_eq!(remaining, 87655);
                assert_eq!(cached_savings, Some(5000));
            }
            _ => panic!("Expected TokenUsage"),
        }
    }

    #[test]
    fn test_parse_rate_limit() {
        let text = "Rate limit exceeded. Try again at 2026-01-26 15:30:00 UTC";
        let response = GeminiOutputParser::parse_rate_limit(text).unwrap();

        match response {
            GeminiResponse::RateLimitHit { retry_at, .. } => {
                assert_eq!(retry_at, Some("2026-01-26 15:30:00 UTC".to_string()));
            }
            _ => panic!("Expected RateLimitHit"),
        }
    }

    #[test]
    fn test_parse_approval_mode() {
        let text = "Approval mode: auto_edit";
        let response = GeminiOutputParser::parse_approval_mode(text).unwrap();

        match response {
            GeminiResponse::ApprovalModeChanged { mode } => {
                assert_eq!(mode, ApprovalMode::AutoEdit);
            }
            _ => panic!("Expected ApprovalModeChanged"),
        }
    }

    #[test]
    fn test_top_level_commands() {
        assert_eq!(
            GeminiTopLevelCommand::Interactive.to_command_string(),
            "gemini"
        );
        assert_eq!(
            GeminiTopLevelCommand::Prompt {
                text: "test query".to_string()
            }
            .to_command_string(),
            "gemini -p \"test query\""
        );
        assert_eq!(
            GeminiTopLevelCommand::ListSessions.to_command_string(),
            "gemini --list-sessions"
        );
    }

    #[test]
    fn test_command_builder() {
        let cmd = GeminiCommandBuilder::new()
            .model("gemini-2.5-flash")
            .yolo()
            .prompt("test query")
            .build();

        assert!(cmd.contains("gemini"));
        assert!(cmd.contains("--model"));
        assert!(cmd.contains("gemini-2.5-flash"));
        assert!(cmd.contains("--yolo"));
        assert!(cmd.contains("-p"));
        assert!(cmd.contains("test query"));
    }

    #[test]
    fn test_approval_mode_display() {
        assert_eq!(ApprovalMode::Default.to_string(), "default");
        assert_eq!(ApprovalMode::AutoEdit.to_string(), "auto_edit");
        assert_eq!(ApprovalMode::Yolo.to_string(), "yolo");
        assert_eq!(ApprovalMode::Plan.to_string(), "plan");
    }

    #[test]
    fn test_extension_commands() {
        let cmd = GeminiTopLevelCommand::Extensions {
            action: ExtensionAction::Install {
                source: "https://github.com/user/ext".to_string(),
                ref_name: Some("v1.0.0".to_string()),
                auto_update: true,
                pre_release: false,
            },
        };

        let cmd_str = cmd.to_command_string();
        assert!(cmd_str.contains("gemini extensions install"));
        assert!(cmd_str.contains("https://github.com/user/ext"));
        assert!(cmd_str.contains("--ref v1.0.0"));
        assert!(cmd_str.contains("--auto-update"));
    }

    #[test]
    fn test_mcp_top_level_commands() {
        let cmd = GeminiTopLevelCommand::Mcp {
            action: McpTopLevelAction::Add {
                name: "my-server".to_string(),
                command_or_url: "/path/to/server".to_string(),
                args: vec!["arg1".to_string(), "arg2".to_string()],
                transport: None,
                headers: vec![],
            },
        };

        let cmd_str = cmd.to_command_string();
        assert!(cmd_str.contains("gemini mcp add my-server"));
        assert!(cmd_str.contains("/path/to/server"));
        assert!(cmd_str.contains("arg1"));
        assert!(cmd_str.contains("arg2"));
    }

    #[test]
    fn test_parse_memory_info() {
        let text = "~/.gemini/GEMINI.md\n.gemini/GEMINI.md\n# Instructions\n- Use Rust\n- Follow best practices";
        let response = GeminiOutputParser::parse_memory_info(text).unwrap();

        match response {
            GeminiResponse::MemoryInfo { entries, file_paths } => {
                assert_eq!(file_paths.len(), 2);
                assert!(file_paths.contains(&"~/.gemini/GEMINI.md".to_string()));
                assert!(entries.len() > 0);
            }
            _ => panic!("Expected MemoryInfo"),
        }
    }

    #[test]
    fn test_parse_update_available() {
        let text = "Update available: 0.23.0 -> 0.24.0";
        let response = GeminiOutputParser::parse_update_available(text).unwrap();

        match response {
            GeminiResponse::UpdateAvailable { current_version, new_version } => {
                assert_eq!(current_version, "0.23.0");
                assert_eq!(new_version, "0.24.0");
            }
            _ => panic!("Expected UpdateAvailable"),
        }
    }

    #[test]
    fn test_directory_commands() {
        assert_eq!(
            GeminiCommand::Directory {
                subcommand: Some(DirectorySubcommand::Add {
                    paths: vec!["../lib".to_string(), "../docs".to_string()]
                })
            }
            .to_pty_input(),
            "/directory add ../lib,../docs\n"
        );
        assert_eq!(
            GeminiCommand::Directory {
                subcommand: Some(DirectorySubcommand::Show)
            }
            .to_pty_input(),
            "/directory show\n"
        );
    }

    #[test]
    fn test_restore_command() {
        assert_eq!(
            GeminiCommand::Restore { tool_call_id: None }.to_pty_input(),
            "/restore\n"
        );
        assert_eq!(
            GeminiCommand::Restore {
                tool_call_id: Some("2025-06-22T10-00-00_000Z-file.txt-write_file".to_string())
            }
            .to_pty_input(),
            "/restore 2025-06-22T10-00-00_000Z-file.txt-write_file\n"
        );
    }
}

/// Pipe-mode spawn builder for Gemini CLI.
///
/// Implements `CliCommandBuilder` for use in `pipe/process.rs` dispatch.
///
/// Argv produced:
///   `gemini --output-format stream-json -p <prompt>`
///
/// Note: `--verbose` is intentionally omitted — it is not required for
/// `--output-format stream-json` and only adds stderr noise (Gemini CLI v0.36.0).
///
/// Resume: Gemini CLI does not support a `--resume` flag in pipe (`-p`) mode.
/// `resume_session_id` is ignored. If Gemini adds resume support in a future
/// version, this builder will need updating (Phase 3 live capture will confirm).
pub struct GeminiPipeBuilder;

impl CliCommandBuilder for GeminiPipeBuilder {
    fn build_command(&self, opts: &SpawnOptions) -> std::process::Command {
        let mut cmd = std::process::Command::new("gemini");
        // Note: --verbose intentionally omitted (see doc comment above).
        cmd.arg("--output-format");
        cmd.arg("stream-json");
        cmd.arg("-p");

        for arg in &opts.extra_args {
            cmd.arg(arg);
        }

        // Prompt follows -p as the final positional argument.
        cmd.arg(&opts.prompt);
        cmd
    }
}
