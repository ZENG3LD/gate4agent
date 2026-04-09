//! Claude Code CLI bindings - commands, responses, and parsing.
//!
//! This module provides comprehensive bindings for Claude Code CLI,
//! including all slash commands, keyboard shortcuts, and response parsing.




use regex::Regex;
use std::collections::HashMap;
use std::io;
use std::sync::OnceLock;

use super::traits::{
    CliCommandBuilder, MessageClass, MessageMetadata, OutputParser, ParsedMessage, PromptSubmitter,
    StartupAction,
};
use crate::transport::SpawnOptions;
use crate::core::types::CliTool;

/// Claude Code slash commands.
///
/// These are the interactive commands available in Claude Code REPL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaudeCommand {
    // Core commands
    Help,
    Clear,
    Compact { instructions: Option<String> },
    Exit,

    // Context & Memory
    Context,
    Cost,
    Memory,
    Usage,
    Add { file: String },
    Diff,
    Review,

    // Configuration
    Config,
    Init,
    Model { model: Option<String> },
    Permissions,
    Vim,
    Theme,

    // Session Management
    Rename { name: String },
    Resume { session: Option<String> },
    Rewind,
    Export { filename: Option<String> },
    Teleport,

    // Workflow
    Plan,
    Todos,
    Tasks,
    Stats,

    // System
    Doctor,
    Bug,
    Status,
    Statusline,

    // Extensions
    Agents,
    Mcp { subcommand: Option<McpSubcommand> },
    Plugin { subcommand: Option<PluginSubcommand> },
    Hooks,
}

/// MCP (Model Context Protocol) subcommands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpSubcommand {
    Enable { server: String },
    Disable { server: String },
    Add { server: String },
    Remove { server: String },
    List,
    Show { server: String },
    Login { server: String },
    Logout { server: String },
}

/// Plugin management subcommands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginSubcommand {
    Enable { plugin: String, marketplace: Option<String> },
    Disable { plugin: String, marketplace: Option<String> },
    Install { plugin: String, marketplace: Option<String> },
    Uninstall { plugin: String },
    List,
}

/// Permission modes in Claude Code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionMode {
    /// Standard mode with prompts for all operations.
    Normal,
    /// Auto-approve all permissions.
    AutoAccept,
    /// Read-only research and planning mode.
    Plan,
}

/// Keyboard shortcuts and control sequences.
///
/// These represent control sequences that can be sent to Claude Code via PTY.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeControl {
    /// Cancel current operation (Ctrl+C).
    Cancel,
    /// Exit session (Ctrl+D).
    Exit,
    /// Open external editor (Ctrl+G).
    OpenEditor,
    /// Clear terminal screen (Ctrl+L).
    ClearScreen,
    /// Toggle verbose output (Ctrl+O).
    ToggleVerbose,
    /// Search command history (Ctrl+R).
    SearchHistory,
    /// Paste image from clipboard (Ctrl+V).
    PasteImage,
    /// Background running tasks (Ctrl+B).
    Background,
    /// Toggle permission mode (Shift+Tab).
    ToggleMode,
    /// Switch model (Alt+P).
    SwitchModel,
    /// Toggle extended thinking (Alt+T).
    ToggleThinking,
    /// Rewind conversation (Esc+Esc).
    RewindEsc,

    // Text editing controls
    /// Jump to beginning of line (Ctrl+A).
    JumpLineStart,
    /// Jump to end of line (Ctrl+E).
    JumpLineEnd,
    /// Delete to end of line (Ctrl+K).
    DeleteToLineEnd,
    /// Delete entire line (Ctrl+U).
    DeleteLine,
    /// Delete previous word (Ctrl+W).
    DeletePrevWord,
    /// Paste deleted text (Ctrl+Y).
    PasteDeleted,
    /// Cycle paste history (Alt+Y).
    CyclePasteHistory,
    /// Move cursor back one word (Alt+B).
    WordBack,
    /// Move cursor forward one word (Alt+F).
    WordForward,
    /// Multiline input (Ctrl+J).
    MultilineInput,
}

impl ClaudeCommand {
    /// Convert command to string to send via PTY.
    ///
    /// Returns the command string with newline, ready to be written to PTY.
    pub fn to_pty_input(&self) -> String {
        let cmd = match self {
            // Core
            Self::Help => "/help".to_string(),
            Self::Clear => "/clear".to_string(),
            Self::Compact { instructions } => {
                if let Some(instr) = instructions {
                    format!("/compact {}", instr)
                } else {
                    "/compact".to_string()
                }
            }
            Self::Exit => "/exit".to_string(),

            // Context & Memory
            Self::Context => "/context".to_string(),
            Self::Cost => "/cost".to_string(),
            Self::Memory => "/memory".to_string(),
            Self::Usage => "/usage".to_string(),
            Self::Add { file } => format!("/add {}", file),
            Self::Diff => "/diff".to_string(),
            Self::Review => "/review".to_string(),

            // Configuration
            Self::Config => "/config".to_string(),
            Self::Init => "/init".to_string(),
            Self::Model { model } => {
                if let Some(m) = model {
                    format!("/model {}", m)
                } else {
                    "/model".to_string()
                }
            }
            Self::Permissions => "/permissions".to_string(),
            Self::Vim => "/vim".to_string(),
            Self::Theme => "/theme".to_string(),

            // Session
            Self::Rename { name } => format!("/rename {}", name),
            Self::Resume { session } => {
                if let Some(s) = session {
                    format!("/resume {}", s)
                } else {
                    "/resume".to_string()
                }
            }
            Self::Rewind => "/rewind".to_string(),
            Self::Export { filename } => {
                if let Some(f) = filename {
                    format!("/export {}", f)
                } else {
                    "/export".to_string()
                }
            }
            Self::Teleport => "/teleport".to_string(),

            // Workflow
            Self::Plan => "/plan".to_string(),
            Self::Todos => "/todos".to_string(),
            Self::Tasks => "/tasks".to_string(),
            Self::Stats => "/stats".to_string(),

            // System
            Self::Doctor => "/doctor".to_string(),
            Self::Bug => "/bug".to_string(),
            Self::Status => "/status".to_string(),
            Self::Statusline => "/statusline".to_string(),

            // Extensions
            Self::Agents => "/agents".to_string(),
            Self::Mcp { subcommand } => {
                if let Some(sub) = subcommand {
                    match sub {
                        McpSubcommand::Enable { server } => format!("/mcp enable {}", server),
                        McpSubcommand::Disable { server } => format!("/mcp disable {}", server),
                        McpSubcommand::Add { server } => format!("/mcp add {}", server),
                        McpSubcommand::Remove { server } => format!("/mcp remove {}", server),
                        McpSubcommand::List => "/mcp list".to_string(),
                        McpSubcommand::Show { server } => format!("/mcp show {}", server),
                        McpSubcommand::Login { server } => format!("/mcp login {}", server),
                        McpSubcommand::Logout { server } => format!("/mcp logout {}", server),
                    }
                } else {
                    "/mcp".to_string()
                }
            }
            Self::Plugin { subcommand } => {
                if let Some(sub) = subcommand {
                    match sub {
                        PluginSubcommand::Enable { plugin, marketplace } => {
                            if let Some(m) = marketplace {
                                format!("/plugin enable {}@{}", plugin, m)
                            } else {
                                format!("/plugin enable {}", plugin)
                            }
                        }
                        PluginSubcommand::Disable { plugin, marketplace } => {
                            if let Some(m) = marketplace {
                                format!("/plugin disable {}@{}", plugin, m)
                            } else {
                                format!("/plugin disable {}", plugin)
                            }
                        }
                        PluginSubcommand::Install { plugin, marketplace } => {
                            if let Some(m) = marketplace {
                                format!("/plugin install {}@{}", plugin, m)
                            } else {
                                format!("/plugin install {}", plugin)
                            }
                        }
                        PluginSubcommand::Uninstall { plugin } => format!("/plugin uninstall {}", plugin),
                        PluginSubcommand::List => "/plugin list".to_string(),
                    }
                } else {
                    "/plugin".to_string()
                }
            }
            Self::Hooks => "/hooks".to_string(),
        };

        format!("{}\n", cmd)
    }
}

impl ClaudeControl {
    /// Convert control sequence to bytes to send via PTY.
    ///
    /// Returns the raw bytes representing the control sequence.
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            Self::Cancel => vec![0x03],              // Ctrl+C
            Self::Exit => vec![0x04],                // Ctrl+D
            Self::OpenEditor => vec![0x07],          // Ctrl+G
            Self::ClearScreen => vec![0x0C],         // Ctrl+L
            Self::ToggleVerbose => vec![0x0F],       // Ctrl+O
            Self::SearchHistory => vec![0x12],       // Ctrl+R
            Self::PasteImage => vec![0x16],          // Ctrl+V
            Self::Background => vec![0x02],          // Ctrl+B
            Self::ToggleMode => vec![0x1B, 0x5B, 0x5A], // Shift+Tab (ESC [ Z)
            Self::SwitchModel => vec![0x1B, 0x70],   // Alt+P (ESC p)
            Self::ToggleThinking => vec![0x1B, 0x74], // Alt+T (ESC t)
            Self::RewindEsc => vec![0x1B, 0x1B],     // Esc+Esc

            // Text editing controls
            Self::JumpLineStart => vec![0x01],       // Ctrl+A
            Self::JumpLineEnd => vec![0x05],         // Ctrl+E
            Self::DeleteToLineEnd => vec![0x0B],     // Ctrl+K
            Self::DeleteLine => vec![0x15],          // Ctrl+U
            Self::DeletePrevWord => vec![0x17],      // Ctrl+W
            Self::PasteDeleted => vec![0x19],        // Ctrl+Y
            Self::CyclePasteHistory => vec![0x1B, 0x79], // Alt+Y (ESC y)
            Self::WordBack => vec![0x1B, 0x62],      // Alt+B (ESC b)
            Self::WordForward => vec![0x1B, 0x66],   // Alt+F (ESC f)
            Self::MultilineInput => vec![0x0A],      // Ctrl+J (LF)
        }
    }
}

/// Bash mode command (prefixed with !).
///
/// Execute shell commands directly without Claude interpretation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BashCommand(pub String);

impl BashCommand {
    /// Convert to PTY input format.
    ///
    /// Returns the command prefixed with ! and terminated with newline.
    pub fn to_pty_input(&self) -> String {
        format!("!{}\n", self.0)
    }
}

/// File reference with @ prefix.
///
/// Reference files and directories using @ prefix with autocomplete support.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileReference(pub String);

impl FileReference {
    /// Convert to PTY input format.
    ///
    /// Returns the file path prefixed with @.
    pub fn to_pty_input(&self) -> String {
        format!("@{}", self.0)
    }
}

/// Parsed responses from Claude Code CLI.
#[derive(Debug, Clone, PartialEq)]
pub enum ClaudeResponse {
    /// Rate limit exceeded with retry information.
    RateLimitExceeded {
        retry_after_seconds: Option<u64>,
        message: String,
    },

    /// Context limit reached.
    ContextLimitReached,

    /// Token usage information.
    TokenUsage {
        used: u64,
        total: u64,
        remaining: u64,
    },

    /// Status information from /status or statusline.
    StatusInfo {
        model: Option<String>,
        permission_mode: Option<PermissionMode>,
        context_percent: Option<f32>,
    },

    /// Session information.
    SessionInfo {
        id: String,
        name: Option<String>,
    },

    /// Cost information from /cost command.
    CostInfo {
        input_tokens: u64,
        output_tokens: u64,
        total_cost_usd: Option<f64>,
    },

    /// Context usage breakdown from /context command.
    ContextUsage {
        used_tokens: u64,
        total_tokens: u64,
        percent_used: f32,
    },

    /// Permission mode indicator detected.
    PermissionModeChange {
        mode: PermissionMode,
    },

    /// Background task information from /tasks.
    BackgroundTask {
        task_id: String,
        command: String,
        status: TaskStatus,
    },

    /// Model information from /model output.
    ModelInfo {
        current_model: String,
        available_models: Vec<String>,
    },

    /// Doctor health check results.
    DoctorResults {
        status: HealthStatus,
        issues: Vec<String>,
    },

    /// Stats output (usage, history, streaks).
    StatsInfo {
        timeframe: String,
        usage_data: HashMap<String, u64>,
    },

    /// Plugin information.
    PluginInfo {
        name: String,
        enabled: bool,
        marketplace: Option<String>,
    },

    /// Agent information.
    AgentInfo {
        name: String,
        description: Option<String>,
        model: Option<String>,
    },

    /// Unknown/raw output that couldn't be parsed.
    Raw(String),
}

/// Background task status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Running,
    Completed,
    Failed,
}

/// Health check status from /doctor command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthStatus {
    Healthy,
    Warning,
    Error,
}

/// Parser for Claude Code CLI output.
///
/// Maintains a buffer and parses known patterns from Claude CLI output.
pub struct ClaudeOutputParser {
    buffer: String,
    token_usage_regex: OnceLock<Regex>,
    rate_limit_regex: OnceLock<Regex>,
    context_limit_regex: OnceLock<Regex>,
    retry_after_regex: OnceLock<Regex>,
    permission_mode_regex: OnceLock<Regex>,
    /// Accumulates partial AI response text during streaming.
    pending_response: String,
    /// Set to `true` when a prompt marker appears, indicating the response is complete.
    response_complete: bool,
}

impl ClaudeOutputParser {
    /// Create a new parser.
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
            token_usage_regex: OnceLock::new(),
            rate_limit_regex: OnceLock::new(),
            context_limit_regex: OnceLock::new(),
            retry_after_regex: OnceLock::new(),
            permission_mode_regex: OnceLock::new(),
            pending_response: String::new(),
            response_complete: false,
        }
    }

    /// Feed new data into the parser.
    ///
    /// Appends data to the internal buffer for parsing.
    pub fn feed(&mut self, data: &str) {
        self.buffer.push_str(data);
    }

    /// Parse all available Claude-specific responses from the buffer.
    ///
    /// Returns a vector of parsed responses and clears processed data from buffer.
    /// Note: For the unified trait interface, use `OutputParser::parse()` which
    /// returns `Vec<ParsedMessage>` instead.
    pub fn parse_native(&mut self) -> Vec<ClaudeResponse> {
        self.parse_claude_responses()
    }

    /// Parse status line from welcome screen or /usage output.
    /// Patterns:
    /// - "Opus 4.5 · Claude Max · email@example.com's Organization"
    /// - "Sonnet 3.5 · Claude Pro"
    fn parse_status_line(text: &str) -> Option<ClaudeResponse> {
        // Look for model names
        let model = if text.contains("Opus 4.5") {
            Some("Opus 4.5".to_string())
        } else if text.contains("Sonnet 4.5") {
            Some("Sonnet 4.5".to_string())
        } else if text.contains("Sonnet 3.5") {
            Some("Sonnet 3.5".to_string())
        } else if text.contains("Haiku") {
            Some("Haiku".to_string())
        } else {
            None
        };

        // Look for plan information (Claude Max, Claude Pro, Free)
        // This gives us some usage context even without exact token counts
        if model.is_some() || text.contains("Claude Max") || text.contains("Claude Pro") {
            Some(ClaudeResponse::StatusInfo {
                model,
                permission_mode: None,
                context_percent: None,
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

    /// Returns the currently accumulated partial response text.
    pub fn pending_response(&self) -> &str {
        &self.pending_response
    }

    /// Returns true if the last response is complete.
    pub fn is_response_complete(&self) -> bool {
        self.response_complete
    }
}

impl Default for ClaudeOutputParser {
    fn default() -> Self {
        Self::new()
    }
}

// ===========================================================================
// OutputParser trait implementation for ClaudeOutputParser
// ===========================================================================

impl OutputParser for ClaudeOutputParser {
    fn feed(&mut self, data: &str) {
        self.buffer.push_str(data);
    }

    fn parse(&mut self) -> Vec<ParsedMessage> {
        let claude_responses = self.parse_claude_responses();
        let tool = CliTool::ClaudeCode;

        let mut messages: Vec<ParsedMessage> = Vec::new();
        self.response_complete = false;

        for resp in claude_responses {
            let (class, content) = match resp {
                ClaudeResponse::RateLimitExceeded { message, .. } => {
                    (MessageClass::Error, message)
                }
                ClaudeResponse::ContextLimitReached => {
                    (MessageClass::Error, "Context limit reached".to_string())
                }
                ClaudeResponse::TokenUsage { used, total, remaining } => (
                    MessageClass::InfoMessage,
                    format!("Token usage: {}/{} ({} remaining)", used, total, remaining),
                ),
                ClaudeResponse::StatusInfo { model, permission_mode, context_percent } => {
                    let mut parts = Vec::new();
                    if let Some(m) = model {
                        parts.push(m);
                    }
                    if let Some(pm) = permission_mode {
                        parts.push(format!("{:?}", pm));
                    }
                    if let Some(cp) = context_percent {
                        parts.push(format!("{:.1}% context", cp));
                    }
                    (MessageClass::InfoMessage, parts.join(" · "))
                }
                ClaudeResponse::PermissionModeChange { mode } => (
                    MessageClass::InfoMessage,
                    format!("Permission mode: {:?}", mode),
                ),
                ClaudeResponse::Raw(text) => {
                    let class = self.classify(&text);
                    (class, text)
                }
                other => (MessageClass::InfoMessage, format!("{:?}", other)),
            };

            match class {
                MessageClass::AiResponse => {
                    self.pending_response.push_str(&content);
                    messages.push(ParsedMessage {
                        class: MessageClass::AiResponse,
                        content,
                        metadata: MessageMetadata {
                            tool,
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
                                tool,
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
                            tool,
                            ..Default::default()
                        },
                    });
                }
                _ => {
                    messages.push(ParsedMessage {
                        class,
                        content,
                        metadata: MessageMetadata {
                            tool,
                            ..Default::default()
                        },
                    });
                }
            }
        }

        messages
    }

    fn extract_ai_text(&self, raw_cleaned: &str) -> String {
        // Look for the response marker "●" which indicates AI's actual response.
        if let Some(marker_pos) = raw_cleaned.find('●') {
            let from_marker = &raw_cleaned[marker_pos + '●'.len_utf8()..];

            // The AI response is usually in the first line after "●".
            let first_line = from_marker.lines().next().unwrap_or("").trim();

            if !first_line.is_empty()
                && !first_line.starts_with('❯')
                && !first_line.contains("Inferring")
                && !first_line.contains("Germinating")
                && !first_line.contains("Julienning")
            {
                // Clean the first line: keep alphanumeric, whitespace, punctuation, and Cyrillic.
                let cleaned = first_line
                    .chars()
                    .filter(|c| {
                        c.is_alphanumeric()
                            || c.is_whitespace()
                            || ".,!?-\u{2014}:;()[]{}«»\"'".contains(*c)
                            || (*c >= '\u{0400}' && *c <= '\u{04FF}')
                    })
                    .collect::<String>();

                return cleaned.trim().to_string();
            }

            // Multi-line collection: gather lines until a UI marker.
            let mut result = String::new();
            for line in from_marker.lines() {
                let trimmed = line.trim();

                // Stop at UI markers.
                if trimmed.starts_with('❯')
                    || trimmed.starts_with("0;✳")
                    || trimmed.starts_with('✻')
                    || trimmed.starts_with('✽')
                    || trimmed.starts_with('✶')
                    || trimmed.starts_with('✢')
                    || trimmed.contains("Inferring")
                    || trimmed.contains("Germinating")
                    || trimmed.contains("Julienning")
                {
                    break;
                }

                // Skip UI noise.
                if trimmed.is_empty()
                    || trimmed.contains("ctrl+g")
                    || trimmed.contains("shortcuts")
                    || trimmed.starts_with('─')
                    || trimmed.starts_with('│')
                {
                    continue;
                }

                if !result.is_empty() {
                    result.push(' ');
                }
                result.push_str(trimmed);
            }

            return result.trim().to_string();
        }

        // Fallback: line-by-line approach.
        let mut ai_lines = Vec::new();
        let mut in_response = false;

        for line in raw_cleaned.lines() {
            let trimmed = line.trim();

            if trimmed.starts_with('●') {
                in_response = true;
                let text = trimmed.trim_start_matches('●').trim();
                if !text.is_empty() {
                    ai_lines.push(text.to_string());
                }
                continue;
            }

            if in_response && (trimmed.starts_with('❯') || trimmed.contains("0;✳")) {
                break;
            }

            if in_response && !trimmed.is_empty() {
                ai_lines.push(trimmed.to_string());
            }
        }

        ai_lines.join(" ").trim().to_string()
    }

    fn classify(&self, text: &str) -> MessageClass {
        // Contains ● → AiResponse
        if text.contains('●') {
            return MessageClass::AiResponse;
        }

        // Contains ❯ → PromptReady
        if text.contains('❯') {
            return MessageClass::PromptReady;
        }

        // Thinking indicators
        if text.contains("Germinating")
            || text.contains("Inferring")
            || text.contains("Julienning")
            || text.contains("Thinking")
        {
            return MessageClass::ThinkingIndicator;
        }

        // Errors
        if text.contains("Rate limit") || text.contains("Context limit") {
            return MessageClass::Error;
        }

        // Box-drawing UI elements
        if text.contains('╭')
            || text.contains('╰')
            || text.contains('│')
            || text.contains('─')
        {
            return MessageClass::UiElement;
        }

        // UI shortcut hints
        if text.contains("ctrl+g") || text.contains("shortcuts") || text.contains("? for") {
            return MessageClass::UiElement;
        }

        // Info messages (update/install notices)
        if text.contains("native installer")
            || text.contains("Update")
            || text.contains("switched from npm")
        {
            return MessageClass::InfoMessage;
        }

        // Tool approval prompts
        if text.contains("approve") || text.contains("Allow") {
            return MessageClass::ToolApproval;
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
        CliTool::ClaudeCode
    }
}

impl ClaudeOutputParser {
    /// Parse buffer into Claude-specific response types.
    ///
    /// This is the original parse logic, renamed to avoid conflict with the
    /// `OutputParser` trait method which returns `Vec<ParsedMessage>`.
    pub fn parse_claude_responses(&mut self) -> Vec<ClaudeResponse> {
        let mut responses = Vec::new();

        let token_regex = self.token_usage_regex.get_or_init(|| {
            Regex::new(r"<system_warning>Token usage: (\d+)/(\d+); (\d+) remaining</system_warning>")
                .unwrap()
        });

        if let Some(caps) = token_regex.captures(&self.buffer) {
            if let (Some(used), Some(total), Some(remaining)) = (
                caps.get(1).and_then(|m| m.as_str().parse().ok()),
                caps.get(2).and_then(|m| m.as_str().parse().ok()),
                caps.get(3).and_then(|m| m.as_str().parse().ok()),
            ) {
                responses.push(ClaudeResponse::TokenUsage {
                    used,
                    total,
                    remaining,
                });
            }
        }

        if self.buffer.contains("Opus")
            || self.buffer.contains("Claude Max")
            || self.buffer.contains("Claude Pro")
        {
            if let Some(response) = Self::parse_status_line(&self.buffer) {
                responses.push(response);
            }
        }

        let context_limit_regex = self.context_limit_regex.get_or_init(|| {
            Regex::new(r"Context limit reached.*?/compact or /clear").unwrap()
        });

        if context_limit_regex.is_match(&self.buffer) {
            responses.push(ClaudeResponse::ContextLimitReached);
        }

        let rate_limit_regex = self.rate_limit_regex.get_or_init(|| {
            Regex::new(r"Rate limit exceeded").unwrap()
        });

        if rate_limit_regex.is_match(&self.buffer) {
            let retry_regex = self.retry_after_regex.get_or_init(|| {
                Regex::new(r"retry-after:\s*(\d+)").unwrap()
            });

            let retry_after = retry_regex
                .captures(&self.buffer)
                .and_then(|caps| caps.get(1))
                .and_then(|m| m.as_str().parse().ok());

            responses.push(ClaudeResponse::RateLimitExceeded {
                retry_after_seconds: retry_after,
                message: "Rate limit exceeded".to_string(),
            });
        }

        let permission_regex = self.permission_mode_regex.get_or_init(|| {
            Regex::new(r"(⏵⏵ accept edits on|⏸ plan mode on)").unwrap()
        });

        if let Some(caps) = permission_regex.captures(&self.buffer) {
            if let Some(indicator) = caps.get(1) {
                let mode = match indicator.as_str() {
                    "⏵⏵ accept edits on" => PermissionMode::AutoAccept,
                    "⏸ plan mode on" => PermissionMode::Plan,
                    _ => PermissionMode::Normal,
                };

                responses.push(ClaudeResponse::PermissionModeChange { mode });
            }
        }

        if !responses.is_empty() {
            self.buffer.clear();
        }

        responses
    }
}

// ===========================================================================
// PromptSubmitter implementation for Claude Code
// ===========================================================================

/// Submitter for sending prompts and commands to Claude Code via PTY.
pub struct ClaudePromptSubmitter;

impl ClaudePromptSubmitter {
    /// Create a new prompt submitter for Claude Code.
    pub fn new() -> Self {
        Self
    }
}

impl Default for ClaudePromptSubmitter {
    fn default() -> Self {
        Self::new()
    }
}

impl PromptSubmitter for ClaudePromptSubmitter {
    fn send_prompt(&self, writer: &mut dyn io::Write, prompt: &str) -> io::Result<()> {
        // Ctrl+U to clear any existing input, then type prompt, then Enter.
        writer.write_all(b"\x15")?;
        writer.write_all(prompt.as_bytes())?;
        writer.write_all(b"\r")?;
        writer.flush()
    }

    fn send_command(&self, writer: &mut dyn io::Write, command: &str) -> io::Result<()> {
        // Ctrl+U to clear line, then slash command, then Enter.
        writer.write_all(b"\x15")?;
        writer.write_all(command.as_bytes())?;
        writer.write_all(b"\r")?;
        writer.flush()
    }

    fn send_control(&self, writer: &mut dyn io::Write, bytes: &[u8]) -> io::Result<()> {
        writer.write_all(bytes)?;
        writer.flush()
    }

    fn handle_startup(&self, output: &str) -> StartupAction {
        // Handle "trust this folder" safety check dialog
        if output.contains("trust this folder") || output.contains("safety check") {
            return StartupAction::SendInput("\r".to_string());
        }

        if output.contains('❯') {
            StartupAction::Ready
        } else {
            StartupAction::Waiting
        }
    }

    fn tool(&self) -> CliTool {
        CliTool::ClaudeCode
    }

    fn requires_char_by_char(&self) -> bool {
        true
    }
}

/// Top-level CLI commands (not slash commands).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaudeTopLevelCommand {
    /// Start interactive REPL.
    Interactive,
    /// Update Claude Code to latest version.
    Update,
    /// Run health check diagnostics.
    Doctor,
    /// Plugin management.
    Plugin {
        action: PluginAction,
    },
    /// MCP server management.
    Mcp {
        action: McpAction,
    },
}

/// Plugin actions for top-level command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginAction {
    Install { plugin: String, marketplace: Option<String> },
    Uninstall { plugin: String },
    MarketplaceAdd { url: String },
    MarketplaceList,
    MarketplaceUpdate { name: String },
    MarketplaceRemove { name: String },
}

/// MCP actions for top-level command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpAction {
    Configure,
    Enable { server: String },
    Disable { server: String },
    Add { server: String },
    Remove { server: String },
    List,
}

impl ClaudeTopLevelCommand {
    /// Convert to command string.
    pub fn to_command_string(&self) -> String {
        match self {
            Self::Interactive => "claude".to_string(),
            Self::Update => "claude update".to_string(),
            Self::Doctor => "claude doctor".to_string(),
            Self::Plugin { action } => match action {
                PluginAction::Install { plugin, marketplace } => {
                    if let Some(m) = marketplace {
                        format!("claude plugin install {}@{}", plugin, m)
                    } else {
                        format!("claude plugin install {}", plugin)
                    }
                }
                PluginAction::Uninstall { plugin } => {
                    format!("claude plugin uninstall {}", plugin)
                }
                PluginAction::MarketplaceAdd { url } => {
                    format!("claude plugin marketplace add {}", url)
                }
                PluginAction::MarketplaceList => "claude plugin marketplace list".to_string(),
                PluginAction::MarketplaceUpdate { name } => {
                    format!("claude plugin marketplace update {}", name)
                }
                PluginAction::MarketplaceRemove { name } => {
                    format!("claude plugin marketplace remove {}", name)
                }
            },
            Self::Mcp { action } => match action {
                McpAction::Configure => "claude mcp".to_string(),
                McpAction::Enable { server } => format!("claude mcp enable {}", server),
                McpAction::Disable { server } => format!("claude mcp disable {}", server),
                McpAction::Add { server } => format!("claude mcp add {}", server),
                McpAction::Remove { server } => format!("claude mcp remove {}", server),
                McpAction::List => "claude mcp list".to_string(),
            },
        }
    }
}

/// Builder for constructing complex Claude CLI command strings.
#[derive(Debug, Default)]
pub struct ClaudeCommandBuilder {
    flags: HashMap<String, Option<String>>,
    prompt: Option<String>,
}

impl ClaudeCommandBuilder {
    /// Create a new command builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the model to use.
    pub fn model(mut self, model: &str) -> Self {
        self.flags.insert("--model".to_string(), Some(model.to_string()));
        self
    }

    /// Enable continue mode.
    pub fn continue_mode(mut self) -> Self {
        self.flags.insert("-c".to_string(), None);
        self
    }

    /// Resume a specific session.
    pub fn resume(mut self, session: &str) -> Self {
        self.flags.insert("-r".to_string(), Some(session.to_string()));
        self
    }

    /// Use print mode (headless).
    pub fn print_mode(mut self) -> Self {
        self.flags.insert("-p".to_string(), None);
        self
    }

    /// Set permission mode.
    pub fn permission_mode(mut self, mode: PermissionMode) -> Self {
        let mode_str = match mode {
            PermissionMode::Normal => "normal",
            PermissionMode::AutoAccept => "auto-accept",
            PermissionMode::Plan => "plan",
        };
        self.flags.insert("--permission-mode".to_string(), Some(mode_str.to_string()));
        self
    }

    /// Add custom system prompt.
    pub fn system_prompt(mut self, prompt: &str) -> Self {
        self.flags.insert("--system-prompt".to_string(), Some(prompt.to_string()));
        self
    }

    /// Append to system prompt.
    pub fn append_system_prompt(mut self, prompt: &str) -> Self {
        self.flags.insert("--append-system-prompt".to_string(), Some(prompt.to_string()));
        self
    }

    /// Add working directory.
    pub fn working_dir(mut self, dir: &str) -> Self {
        self.flags.insert("--add-dir".to_string(), Some(dir.to_string()));
        self
    }

    /// Set the initial prompt/query.
    pub fn prompt(mut self, prompt: &str) -> Self {
        self.prompt = Some(prompt.to_string());
        self
    }

    /// Build the command string.
    pub fn build(&self) -> String {
        let mut cmd = vec!["claude".to_string()];

        // Add flags
        for (flag, value) in &self.flags {
            cmd.push(flag.clone());
            if let Some(val) = value {
                cmd.push(format!("\"{}\"", val.replace('"', "\\\"")));
            }
        }

        // Add prompt if any
        if let Some(prompt) = &self.prompt {
            cmd.push(format!("\"{}\"", prompt.replace('"', "\\\"")));
        }

        cmd.join(" ")
    }
}

/// Pipe-mode spawn builder for Claude Code.
///
/// Implements `CliCommandBuilder` for use in `pipe/process.rs` dispatch.
///
/// Argv produced (all platforms):
///   `claude -p --output-format stream-json --verbose --dangerously-skip-permissions`
///   `[--append-system-prompt "<text>"] [--resume <id>] [--model <m>] [<extra>...]`
///
/// The initial prompt is **not** included in argv — it is written to stdin by
/// the caller (`pipe/process.rs`) after spawn. Claude `-p` reads stdin until EOF.
pub struct ClaudePipeBuilder;

impl CliCommandBuilder for ClaudePipeBuilder {
    fn build_command(&self, opts: &SpawnOptions) -> std::process::Command {
        let mut cmd = std::process::Command::new("claude");
        cmd.arg("-p");
        cmd.arg("--output-format");
        cmd.arg("stream-json");
        cmd.arg("--verbose");
        cmd.arg("--dangerously-skip-permissions");

        if let Some(ref system_prompt) = opts.append_system_prompt {
            cmd.arg("--append-system-prompt");
            cmd.arg(system_prompt);
        }
        if let Some(ref session_id) = opts.resume_session_id {
            cmd.arg("--resume");
            cmd.arg(session_id);
        }
        if let Some(ref model) = opts.model {
            cmd.arg("--model");
            cmd.arg(model);
        }
        for arg in &opts.extra_args {
            cmd.arg(arg);
        }
        // No prompt in argv — delivered via stdin after spawn.
        cmd
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_command_to_pty_input() {
        assert_eq!(ClaudeCommand::Help.to_pty_input(), "/help\n");
        assert_eq!(ClaudeCommand::Clear.to_pty_input(), "/clear\n");
        assert_eq!(
            ClaudeCommand::Compact { instructions: None }.to_pty_input(),
            "/compact\n"
        );
        assert_eq!(
            ClaudeCommand::Compact {
                instructions: Some("Focus on auth".to_string())
            }
            .to_pty_input(),
            "/compact Focus on auth\n"
        );
        assert_eq!(
            ClaudeCommand::Rename {
                name: "test-session".to_string()
            }
            .to_pty_input(),
            "/rename test-session\n"
        );
    }

    #[test]
    fn test_control_to_bytes() {
        assert_eq!(ClaudeControl::Cancel.to_bytes(), vec![0x03]);
        assert_eq!(ClaudeControl::Exit.to_bytes(), vec![0x04]);
        assert_eq!(ClaudeControl::ClearScreen.to_bytes(), vec![0x0C]);
    }

    #[test]
    fn test_parser_token_usage() {
        let mut parser = ClaudeOutputParser::new();
        parser.feed("<system_warning>Token usage: 48278/200000; 151722 remaining</system_warning>");

        let responses = parser.parse_native();
        assert_eq!(responses.len(), 1);

        match &responses[0] {
            ClaudeResponse::TokenUsage {
                used,
                total,
                remaining,
            } => {
                assert_eq!(*used, 48278);
                assert_eq!(*total, 200000);
                assert_eq!(*remaining, 151722);
            }
            _ => panic!("Expected TokenUsage response"),
        }
    }

    #[test]
    fn test_parser_context_limit() {
        let mut parser = ClaudeOutputParser::new();
        parser.feed("Context limit reached · /compact or /clear to continue");

        let responses = parser.parse_native();
        assert_eq!(responses.len(), 1);
        assert!(matches!(responses[0], ClaudeResponse::ContextLimitReached));
    }

    #[test]
    fn test_parser_rate_limit() {
        let mut parser = ClaudeOutputParser::new();
        parser.feed("Rate limit exceeded\nretry-after: 300");

        let responses = parser.parse_native();
        assert_eq!(responses.len(), 1);

        match &responses[0] {
            ClaudeResponse::RateLimitExceeded {
                retry_after_seconds,
                ..
            } => {
                assert_eq!(*retry_after_seconds, Some(300));
            }
            _ => panic!("Expected RateLimitExceeded response"),
        }
    }

    #[test]
    fn test_parser_permission_mode() {
        let mut parser = ClaudeOutputParser::new();
        parser.feed("⏵⏵ accept edits on");

        let responses = parser.parse_native();
        assert_eq!(responses.len(), 1);

        match &responses[0] {
            ClaudeResponse::PermissionModeChange { mode } => {
                assert_eq!(*mode, PermissionMode::AutoAccept);
            }
            _ => panic!("Expected PermissionModeChange response"),
        }
    }

    #[test]
    fn test_command_builder() {
        let cmd = ClaudeCommandBuilder::new()
            .model("opus")
            .continue_mode()
            .prompt("test query")
            .build();

        assert!(cmd.contains("claude"));
        assert!(cmd.contains("--model"));
        assert!(cmd.contains("opus"));
        assert!(cmd.contains("-c"));
        assert!(cmd.contains("test query"));
    }

    #[test]
    fn test_command_builder_permission_mode() {
        let cmd = ClaudeCommandBuilder::new()
            .permission_mode(PermissionMode::Plan)
            .build();

        assert!(cmd.contains("--permission-mode"));
        assert!(cmd.contains("plan"));
    }

    #[test]
    fn test_bash_command() {
        let cmd = BashCommand("npm test".to_string());
        assert_eq!(cmd.to_pty_input(), "!npm test\n");
    }

    #[test]
    fn test_file_reference() {
        let file = FileReference("./src/main.rs".to_string());
        assert_eq!(file.to_pty_input(), "@./src/main.rs");
    }

    #[test]
    fn test_new_slash_commands() {
        assert_eq!(
            ClaudeCommand::Add { file: "test.rs".to_string() }.to_pty_input(),
            "/add test.rs\n"
        );
        assert_eq!(ClaudeCommand::Diff.to_pty_input(), "/diff\n");
        assert_eq!(ClaudeCommand::Review.to_pty_input(), "/review\n");
    }

    #[test]
    fn test_mcp_subcommands() {
        assert_eq!(
            ClaudeCommand::Mcp {
                subcommand: Some(McpSubcommand::Add {
                    server: "github".to_string()
                })
            }
            .to_pty_input(),
            "/mcp add github\n"
        );
        assert_eq!(
            ClaudeCommand::Mcp {
                subcommand: Some(McpSubcommand::List)
            }
            .to_pty_input(),
            "/mcp list\n"
        );
    }

    #[test]
    fn test_plugin_subcommands() {
        assert_eq!(
            ClaudeCommand::Plugin {
                subcommand: Some(PluginSubcommand::Enable {
                    plugin: "my-plugin".to_string(),
                    marketplace: Some("official".to_string())
                })
            }
            .to_pty_input(),
            "/plugin enable my-plugin@official\n"
        );
        assert_eq!(
            ClaudeCommand::Plugin {
                subcommand: Some(PluginSubcommand::List)
            }
            .to_pty_input(),
            "/plugin list\n"
        );
    }

    #[test]
    fn test_text_editing_controls() {
        assert_eq!(ClaudeControl::JumpLineStart.to_bytes(), vec![0x01]);
        assert_eq!(ClaudeControl::JumpLineEnd.to_bytes(), vec![0x05]);
        assert_eq!(ClaudeControl::DeleteToLineEnd.to_bytes(), vec![0x0B]);
        assert_eq!(ClaudeControl::WordBack.to_bytes(), vec![0x1B, 0x62]);
    }

    #[test]
    fn test_top_level_commands() {
        assert_eq!(
            ClaudeTopLevelCommand::Update.to_command_string(),
            "claude update"
        );
        assert_eq!(
            ClaudeTopLevelCommand::Doctor.to_command_string(),
            "claude doctor"
        );
        assert_eq!(
            ClaudeTopLevelCommand::Plugin {
                action: PluginAction::Install {
                    plugin: "test".to_string(),
                    marketplace: Some("official".to_string())
                }
            }
            .to_command_string(),
            "claude plugin install test@official"
        );
    }
}
