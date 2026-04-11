//! OpenAI Codex CLI bindings - commands, responses, and parsing.
//!
//! This module provides comprehensive bindings for the OpenAI Codex CLI,
//! including:
//! - Slash commands and special inputs
//! - Control sequences
//! - Response parsing (rate limits, token usage, status info)
//! - ANSI escape code stripping




use regex::Regex;
use serde::{Deserialize, Serialize};
use std::io;
use std::sync::OnceLock;

use super::traits::{
    CliCommandBuilder, MessageClass, MessageMetadata, OutputParser, ParsedMessage, PromptSubmitter,
    StartupAction,
};
use crate::transport::SpawnOptions;
use crate::core::types::CliTool;

/// Codex slash commands that can be sent via PTY.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexCommand {
    // Core commands
    /// Set approval mode (auto, readonly, full)
    Approvals { mode: Option<ApprovalMode> },
    /// Summarize conversation to free tokens
    Compact,
    /// Show Git diff
    Diff,
    /// Review working tree
    Review,
    /// Display session configuration and token usage
    Status,
    /// Exit the CLI
    Exit,
    /// Quit the CLI (alias for Exit)
    Quit,

    // Session management
    /// Choose active model
    Model { model: Option<String> },
    /// Attach a file to conversation
    Mention { file: String },
    /// Start new conversation
    New,
    /// Resume saved conversation
    Resume,
    /// Fork conversation
    Fork,

    // System
    /// List MCP tools
    Mcp,
    /// Generate AGENTS.md scaffold
    Init,
    /// Sign out
    Logout,
    /// Send logs to maintainers
    Feedback,

    // Custom prompts
    /// Custom prompt with optional arguments
    Custom { name: String, args: Vec<String> },
}

/// Approval mode for Codex CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApprovalMode {
    /// Read Only (Suggest mode) - safest, requires approval for everything
    ReadOnly,
    /// Auto (Agent mode) - allows local edits with approval for external calls
    Auto,
    /// Full Auto mode - most autonomous, sandboxed
    Full,
}

impl std::fmt::Display for ApprovalMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApprovalMode::ReadOnly => write!(f, "Read Only"),
            ApprovalMode::Auto => write!(f, "Auto"),
            ApprovalMode::Full => write!(f, "Full Access"),
        }
    }
}

/// Special input symbols that can be used in Codex CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexSpecialInput {
    /// File search: @path
    FileSearch(String),
    /// Shell command: !command
    ShellCommand(String),
    /// Skill invocation: $skill
    SkillInvoke(String),
}

/// Control sequences for Codex CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexControl {
    /// Cancel current operation (Ctrl+C)
    Cancel,
    /// Open external editor (Ctrl+G)
    OpenEditor,
    /// Fork transcript (Esc+Esc)
    ForkTranscript,
    /// Choose environment (Ctrl+O)
    ChooseEnv,
}

impl CodexCommand {
    /// Convert command to string to send via PTY.
    pub fn to_pty_input(&self) -> String {
        match self {
            CodexCommand::Approvals { mode } => match mode {
                Some(ApprovalMode::ReadOnly) => "/approvals readonly\n".to_string(),
                Some(ApprovalMode::Auto) => "/approvals auto\n".to_string(),
                Some(ApprovalMode::Full) => "/approvals full\n".to_string(),
                None => "/approvals\n".to_string(),
            },
            CodexCommand::Compact => "/compact\n".to_string(),
            CodexCommand::Diff => "/diff\n".to_string(),
            CodexCommand::Review => "/review\n".to_string(),
            CodexCommand::Status => "/status\n".to_string(),
            CodexCommand::Exit => "/exit\n".to_string(),
            CodexCommand::Quit => "/quit\n".to_string(),
            CodexCommand::Model { model } => match model {
                Some(m) => format!("/model {}\n", m),
                None => "/model\n".to_string(),
            },
            CodexCommand::Mention { file } => format!("/mention {}\n", file),
            CodexCommand::New => "/new\n".to_string(),
            CodexCommand::Resume => "/resume\n".to_string(),
            CodexCommand::Fork => "/fork\n".to_string(),
            CodexCommand::Mcp => "/mcp\n".to_string(),
            CodexCommand::Init => "/init\n".to_string(),
            CodexCommand::Logout => "/logout\n".to_string(),
            CodexCommand::Feedback => "/feedback\n".to_string(),
            CodexCommand::Custom { name, args } => {
                if args.is_empty() {
                    format!("/{}\n", name)
                } else {
                    format!("/{} {}\n", name, args.join(" "))
                }
            }
        }
    }
}

impl CodexSpecialInput {
    /// Convert special input to string to send via PTY.
    pub fn to_pty_input(&self) -> String {
        match self {
            CodexSpecialInput::FileSearch(path) => format!("@{}\n", path),
            CodexSpecialInput::ShellCommand(cmd) => format!("!{}\n", cmd),
            CodexSpecialInput::SkillInvoke(skill) => format!("${}\n", skill),
        }
    }
}

impl CodexControl {
    /// Convert control sequence to bytes to send via PTY.
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            CodexControl::Cancel => vec![0x03], // Ctrl+C
            CodexControl::OpenEditor => vec![0x07], // Ctrl+G
            CodexControl::ForkTranscript => vec![0x1B, 0x1B], // Esc+Esc
            CodexControl::ChooseEnv => vec![0x0F], // Ctrl+O
        }
    }
}

/// Single model menu item.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelMenuItem {
    pub number: u8,
    pub name: String,
    pub is_current: bool,
    pub description: String,
}

/// Parsed responses from Codex CLI.
#[derive(Debug, Clone, PartialEq)]
pub enum CodexResponse {
    /// Rate limit hit - CRITICAL pattern from PTY test
    RateLimitHit {
        message: String,
        reset_time: Option<String>, // "Jan 23rd, 2026 4:06 AM"
        upgrade_url: Option<String>,
    },

    /// Token usage from /status
    TokenUsage {
        input: u64,
        output: u64,
        total: u64,
        context_percent_left: Option<f32>,
        context_used: Option<u64>,
        context_total: Option<u64>,
    },

    /// Status information
    StatusInfo {
        model: String,
        version: Option<String>,
        approval_mode: Option<ApprovalMode>,
    },

    /// Welcome screen info (seen in PTY test)
    WelcomeScreen {
        version: String,
        model: String,
        context_percent: f32,
    },

    /// Context percentage indicator
    ContextLeft {
        percent: f32,
    },

    /// Model selection menu displayed
    ModelMenu {
        models: Vec<ModelMenuItem>,
        current_index: usize,
    },

    /// Update available
    UpdateAvailable {
        current_version: String,
        new_version: String,
    },

    /// Model switch suggestion
    ModelSwitchSuggestion {
        suggested_model: String,
        reason: String,
    },

    /// Working indicator
    Working { elapsed_seconds: u32 },

    /// Approval mode changed
    ApprovalModeChanged { mode: ApprovalMode },

    /// Unknown/raw output
    Raw(String),
}

/// Parser for Codex CLI output.
pub struct CodexOutputParser {
    buffer: String,
    /// Accumulates partial AI response text during streaming.
    pending_response: String,
    /// Set to `true` when a prompt marker appears, indicating the response is complete.
    response_complete: bool,
}

impl CodexOutputParser {
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
    pub fn parse(&mut self) -> Vec<CodexResponse> {
        let cleaned = strip_ansi_codes(&self.buffer);
        let responses = Self::parse_text(&cleaned);
        self.buffer.clear();
        responses
    }

    /// Parse text without buffering.
    pub fn parse_text(text: &str) -> Vec<CodexResponse> {
        let mut responses = Vec::new();

        // Rate limit patterns
        if let Some(response) = Self::parse_rate_limit(text) {
            responses.push(response);
        }

        // Welcome screen
        if let Some(response) = Self::parse_welcome_screen(text) {
            responses.push(response);
        }

        // Token usage (before status info, as it's more specific)
        if let Some(response) = Self::parse_token_usage(text) {
            responses.push(response);
        }

        // Status info (fallback for model info without full token usage)
        if let Some(response) = Self::parse_status_info(text) {
            // Only add if we don't already have a welcome screen or token usage
            if !responses.iter().any(|r| {
                matches!(r, CodexResponse::WelcomeScreen { .. } | CodexResponse::TokenUsage { .. })
            }) {
                responses.push(response);
            }
        }

        // Context left indicator (standalone, not part of welcome screen)
        if let Some(response) = Self::parse_context_left(text) {
            // Only add if we didn't already parse a welcome screen
            if !responses.iter().any(|r| matches!(r, CodexResponse::WelcomeScreen { .. })) {
                responses.push(response);
            }
        }

        // Model menu
        if let Some(response) = Self::parse_model_menu(text) {
            responses.push(response);
        }

        // Update available
        if let Some(response) = Self::parse_update_available(text) {
            responses.push(response);
        }

        // Model switch suggestion
        if let Some(response) = Self::parse_model_switch_suggestion(text) {
            responses.push(response);
        }

        // Working indicator
        if let Some(response) = Self::parse_working_indicator(text) {
            responses.push(response);
        }

        // Approval mode changed
        if let Some(response) = Self::parse_approval_mode_changed(text) {
            responses.push(response);
        }

        // If nothing specific was parsed, return raw
        if responses.is_empty() && !text.trim().is_empty() {
            responses.push(CodexResponse::Raw(text.to_string()));
        }

        responses
    }

    /// Parse rate limit hit message.
    /// Patterns:
    /// - "■ You've hit your usage limit. Upgrade to Pro (https://openai.com/chatgpt/pricing)"
    /// - "try again at Jan 23rd, 2026 4:06 AM"
    /// - "You've reached your 5-hour message limit. Try again in 3h 42m."
    fn parse_rate_limit(text: &str) -> Option<CodexResponse> {
        static RATE_LIMIT_RE: OnceLock<Regex> = OnceLock::new();
        let re = RATE_LIMIT_RE.get_or_init(|| {
            Regex::new(r"(?i)(hit|reached).*usage limit|rate limit").unwrap()
        });

        if !re.is_match(text) {
            return None;
        }

        // Extract upgrade URL
        static URL_RE: OnceLock<Regex> = OnceLock::new();
        let url_re = URL_RE.get_or_init(|| {
            Regex::new(r"https?://[^\s)]+").unwrap()
        });
        let upgrade_url = url_re.find(text).map(|m| m.as_str().to_string());

        // Extract reset time
        // Pattern: "try again at Jan 23rd, 2026 4:06 AM"
        // Pattern: "Try again in 3h 42m"
        static RESET_TIME_RE: OnceLock<Regex> = OnceLock::new();
        let reset_time_re = RESET_TIME_RE.get_or_init(|| {
            Regex::new(r"try again (?:at|in) ([^.\n]+)").unwrap()
        });
        let reset_time = reset_time_re
            .captures(text)
            .and_then(|cap| cap.get(1))
            .map(|m| m.as_str().trim().to_string());

        Some(CodexResponse::RateLimitHit {
            message: text.to_string(),
            reset_time,
            upgrade_url,
        })
    }

    /// Parse welcome screen.
    /// Pattern:
    /// - "OpenAI Codex (v0.85.0)"
    /// - "model: gpt-5.2-codex"
    /// - "100% context left"
    fn parse_welcome_screen(text: &str) -> Option<CodexResponse> {
        static WELCOME_RE: OnceLock<Regex> = OnceLock::new();
        let re = WELCOME_RE.get_or_init(|| {
            Regex::new(r"OpenAI Codex \(v([\d.]+)\)").unwrap()
        });

        let version = re.captures(text).and_then(|cap| {
            cap.get(1).map(|m| m.as_str().to_string())
        })?;

        // Extract model - try both "model:" and standalone model name patterns
        static MODEL_RE: OnceLock<Regex> = OnceLock::new();
        let model_re = MODEL_RE.get_or_init(|| {
            Regex::new(r"model:\s*(\S+)").unwrap()
        });
        let model = model_re
            .captures(text)
            .and_then(|cap| cap.get(1))
            .map(|m| m.as_str().to_string())
            .or_else(|| {
                // Fallback: look for common model names
                if text.contains("gpt-5.2-codex") {
                    Some("gpt-5.2-codex".to_string())
                } else if text.contains("gpt-5.1-codex") {
                    Some("gpt-5.1-codex".to_string())
                } else {
                    None
                }
            })?;

        // Extract context percentage
        static CONTEXT_RE: OnceLock<Regex> = OnceLock::new();
        let context_re = CONTEXT_RE.get_or_init(|| {
            Regex::new(r"(\d+)%\s*context left").unwrap()
        });
        let context_percent = context_re
            .captures(text)
            .and_then(|cap| cap.get(1))
            .and_then(|m| m.as_str().parse::<f32>().ok())
            .unwrap_or(100.0);

        Some(CodexResponse::WelcomeScreen {
            version,
            model,
            context_percent,
        })
    }

    /// Parse status info from output that contains model information.
    /// This is a fallback when /status doesn't show full token usage.
    /// Patterns:
    /// - "model:     gpt-5.2-codex   /model to change"
    /// - "directory: ~\CODING\..."
    fn parse_status_info(text: &str) -> Option<CodexResponse> {
        // Look for model line in status-like output
        static MODEL_LINE_RE: OnceLock<Regex> = OnceLock::new();
        let re = MODEL_LINE_RE.get_or_init(|| {
            Regex::new(r"model:\s*(\S+)").unwrap()
        });

        let model = re
            .captures(text)
            .and_then(|cap| cap.get(1))
            .map(|m| m.as_str().to_string())?;

        Some(CodexResponse::StatusInfo {
            model,
            version: None,
            approval_mode: None,
        })
    }

    /// Parse token usage from /status output.
    /// Patterns:
    /// - "Token usage: 7.49K total (7.38K input + 105 output)"
    /// - "Context window: 100% left (7.49K used / 272K)"
    /// - "Token usage: total=7,485 input=7,380 output=105"
    fn parse_token_usage(text: &str) -> Option<CodexResponse> {
        // Pattern 1: "7.49K total (7.38K input + 105 output)"
        static USAGE_RE1: OnceLock<Regex> = OnceLock::new();
        let re1 = USAGE_RE1.get_or_init(|| {
            Regex::new(r"Token usage:\s*([\d.]+)([KM]?)\s*total\s*\(([\d.]+)([KM]?)\s*input\s*\+\s*([\d.]+)([KM]?)\s*output\)").unwrap()
        });

        if let Some(cap) = re1.captures(text) {
            let total = parse_number_with_suffix(
                cap.get(1).unwrap().as_str(),
                cap.get(2).map(|m| m.as_str()),
            );
            let input = parse_number_with_suffix(
                cap.get(3).unwrap().as_str(),
                cap.get(4).map(|m| m.as_str()),
            );
            let output = parse_number_with_suffix(
                cap.get(5).unwrap().as_str(),
                cap.get(6).map(|m| m.as_str()),
            );

            // Parse context window
            static CONTEXT_RE: OnceLock<Regex> = OnceLock::new();
            let context_re = CONTEXT_RE.get_or_init(|| {
                Regex::new(r"Context window:\s*(\d+)%\s*left\s*\(([\d.]+)([KM]?)\s*used\s*/\s*([\d.]+)([KM]?)\)").unwrap()
            });

            let (context_percent_left, context_used, context_total) =
                if let Some(ctx_cap) = context_re.captures(text) {
                    let percent = ctx_cap.get(1).unwrap().as_str().parse::<f32>().ok();
                    let used = parse_number_with_suffix(
                        ctx_cap.get(2).unwrap().as_str(),
                        ctx_cap.get(3).map(|m| m.as_str()),
                    );
                    let total = parse_number_with_suffix(
                        ctx_cap.get(4).unwrap().as_str(),
                        ctx_cap.get(5).map(|m| m.as_str()),
                    );
                    (percent, Some(used), Some(total))
                } else {
                    (None, None, None)
                };

            return Some(CodexResponse::TokenUsage {
                input,
                output,
                total,
                context_percent_left,
                context_used,
                context_total,
            });
        }

        // Pattern 2: "total=7,485 input=7,380 output=105"
        static USAGE_RE2: OnceLock<Regex> = OnceLock::new();
        let re2 = USAGE_RE2.get_or_init(|| {
            Regex::new(r"Token usage:\s*total=([\d,]+)\s*input=([\d,]+)\s*output=([\d,]+)").unwrap()
        });

        if let Some(cap) = re2.captures(text) {
            let total = cap
                .get(1)
                .unwrap()
                .as_str()
                .replace(",", "")
                .parse::<u64>()
                .unwrap_or(0);
            let input = cap
                .get(2)
                .unwrap()
                .as_str()
                .replace(",", "")
                .parse::<u64>()
                .unwrap_or(0);
            let output = cap
                .get(3)
                .unwrap()
                .as_str()
                .replace(",", "")
                .parse::<u64>()
                .unwrap_or(0);

            return Some(CodexResponse::TokenUsage {
                input,
                output,
                total,
                context_percent_left: None,
                context_used: None,
                context_total: None,
            });
        }

        None
    }

    /// Parse update available message.
    /// Pattern: "✨ Update available! 0.85.0 -> 0.87.0"
    fn parse_update_available(text: &str) -> Option<CodexResponse> {
        static UPDATE_RE: OnceLock<Regex> = OnceLock::new();
        let re = UPDATE_RE.get_or_init(|| {
            Regex::new(r"Update available!\s*([\d.]+)\s*->\s*([\d.]+)").unwrap()
        });

        re.captures(text).map(|cap| CodexResponse::UpdateAvailable {
            current_version: cap.get(1).unwrap().as_str().to_string(),
            new_version: cap.get(2).unwrap().as_str().to_string(),
        })
    }

    /// Parse model switch suggestion.
    /// Pattern:
    /// - "Approaching rate limits"
    /// - "Switch to gpt-5.1-codex-mini for lower credit usage?"
    fn parse_model_switch_suggestion(text: &str) -> Option<CodexResponse> {
        static SWITCH_RE: OnceLock<Regex> = OnceLock::new();
        let re = SWITCH_RE.get_or_init(|| {
            Regex::new(r"Switch to\s+(\S+)\s+for\s+(.+?)\?").unwrap()
        });

        re.captures(text).map(|cap| {
            CodexResponse::ModelSwitchSuggestion {
                suggested_model: cap.get(1).unwrap().as_str().to_string(),
                reason: cap.get(2).unwrap().as_str().to_string(),
            }
        })
    }

    /// Parse working indicator.
    /// Pattern: "• Working (0s • esc to interrupt)"
    fn parse_working_indicator(text: &str) -> Option<CodexResponse> {
        static WORKING_RE: OnceLock<Regex> = OnceLock::new();
        let re = WORKING_RE.get_or_init(|| {
            Regex::new(r"Working\s*\((\d+)s").unwrap()
        });

        re.captures(text).map(|cap| CodexResponse::Working {
            elapsed_seconds: cap
                .get(1)
                .unwrap()
                .as_str()
                .parse::<u32>()
                .unwrap_or(0),
        })
    }

    /// Parse approval mode changed message.
    fn parse_approval_mode_changed(text: &str) -> Option<CodexResponse> {
        let lower = text.to_lowercase();

        if lower.contains("approval") && lower.contains("mode") {
            if lower.contains("read") && lower.contains("only") {
                return Some(CodexResponse::ApprovalModeChanged {
                    mode: ApprovalMode::ReadOnly,
                });
            } else if lower.contains("auto") {
                return Some(CodexResponse::ApprovalModeChanged {
                    mode: ApprovalMode::Auto,
                });
            } else if lower.contains("full") {
                return Some(CodexResponse::ApprovalModeChanged {
                    mode: ApprovalMode::Full,
                });
            }
        }

        None
    }

    /// Parse context left indicator.
    /// Patterns:
    /// - "100% context left"
    /// - "100% context left · ? for shortcuts"
    /// - "75% context left"
    fn parse_context_left(text: &str) -> Option<CodexResponse> {
        static CONTEXT_LEFT_RE: OnceLock<Regex> = OnceLock::new();
        let re = CONTEXT_LEFT_RE.get_or_init(|| {
            Regex::new(r"(\d+(?:\.\d+)?)%\s*context left").unwrap()
        });

        re.captures(text)
            .and_then(|cap| cap.get(1))
            .and_then(|m| m.as_str().parse::<f32>().ok())
            .map(|percent| CodexResponse::ContextLeft { percent })
    }

    /// Parse model selection menu.
    /// Pattern:
    /// ```text
    /// Select Model and Effort
    /// › 1. gpt-5.2-codex (current)  Latest frontier agentic coding model.
    ///   2. gpt-5.1-codex-max        Codex-optimized flagship for deep and fast reasoning.
    ///   3. gpt-5.1-codex-mini       Optimized for codex. Cheaper, faster, but less capable.
    /// ```
    fn parse_model_menu(text: &str) -> Option<CodexResponse> {
        // Check if this is a model menu
        if !text.contains("Select Model and Effort") {
            return None;
        }

        let mut models = Vec::new();
        let mut current_index = 0;

        // Match model menu items
        static MODEL_ITEM_RE: OnceLock<Regex> = OnceLock::new();
        let re = MODEL_ITEM_RE.get_or_init(|| {
            // Match both current and non-current items
            // Format: [›/space] number. model-name [(current)]  description
            Regex::new(r"[›\s]\s*(\d+)\.\s+([\w.-]+)\s*(?:\(current\))?\s+(.+?)(?:\n|$)").unwrap()
        });

        for (idx, cap) in re.captures_iter(text).enumerate() {
            let number = cap.get(1)
                .and_then(|m| m.as_str().parse::<u8>().ok())
                .unwrap_or(0);
            let name = cap.get(2).map(|m| m.as_str().to_string()).unwrap_or_default();
            let description = cap.get(3).map(|m| m.as_str().trim().to_string()).unwrap_or_default();

            // Check if this is the current model
            let is_current = text.lines()
                .find(|line| line.contains(&name) && line.contains("(current)"))
                .is_some();

            // Check if this line starts with '›' (current selection in menu)
            let line_text = cap.get(0).map(|m| m.as_str()).unwrap_or("");
            if line_text.trim_start().starts_with('›') {
                current_index = idx;
            }

            models.push(ModelMenuItem {
                number,
                name,
                is_current,
                description,
            });
        }

        if models.is_empty() {
            None
        } else {
            Some(CodexResponse::ModelMenu {
                models,
                current_index,
            })
        }
    }
}

impl Default for CodexOutputParser {
    fn default() -> Self {
        Self::new()
    }
}

/// Strip leading spinner fragment residue from text.
///
/// The "Working" spinner can leave partial fragments like "rk", "ng", "ki", "in"
/// glued to the beginning of the real AI response text.
fn strip_spinner_prefix(text: &str) -> String {
    const SPINNER_FRAGMENTS: &[&str] = &[
        "Wo", "or", "rk", "ki", "in", "ng",
        "Wor", "ork", "rki", "kin", "ing",
        "Work", "orki", "rkin", "king",
    ];
    let mut s = text.to_string();
    let mut changed = true;
    while changed {
        changed = false;
        let trimmed = s.trim_start();
        for frag in SPINNER_FRAGMENTS {
            if trimmed.starts_with(frag) {
                let after = &trimmed[frag.len()..];
                if after.starts_with(' ') || after.starts_with('\u{a0}') {
                    s = after.trim_start().to_string();
                    changed = true;
                    break;
                }
            }
        }
    }
    s
}

impl OutputParser for CodexOutputParser {
    fn feed(&mut self, data: &str) {
        self.buffer.push_str(data);
    }

    fn parse(&mut self) -> Vec<ParsedMessage> {
        let cleaned = strip_ansi_codes(&self.buffer);
        self.buffer.clear();
        self.response_complete = false;

        let mut messages = Vec::new();

        for line in cleaned.lines().filter(|line| !line.trim().is_empty()) {
            let class = self.classify(line);
            let content = line.to_string();

            match class {
                MessageClass::AiResponse => {
                    self.pending_response.push_str(&content);
                    messages.push(ParsedMessage {
                        class: MessageClass::AiResponse,
                        content,
                        metadata: MessageMetadata {
                            tool: CliTool::Codex,
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
                                tool: CliTool::Codex,
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
                            tool: CliTool::Codex,
                            ..Default::default()
                        },
                    });
                }
                _ => {
                    messages.push(ParsedMessage {
                        class,
                        content,
                        metadata: MessageMetadata {
                            tool: CliTool::Codex,
                            ..Default::default()
                        },
                    });
                }
            }
        }

        messages
    }

    fn extract_ai_text(&self, raw_cleaned: &str) -> String {
        let lines: Vec<&str> = raw_cleaned
            .lines()
            .filter(|line| {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    return false;
                }
                // Skip Working spinner and interrupt hint
                if trimmed.contains("Working") || trimmed.contains("esc to interrupt") {
                    return false;
                }
                // Skip spinner fragments (very short lines)
                if trimmed.len() <= 3 {
                    return false;
                }
                // Skip bare bullet markers
                if trimmed == "\u{25e6}" || trimmed == "\u{2022}" {
                    return false;
                }
                // Skip box-drawing characters
                if trimmed.starts_with('\u{2502}')
                    || trimmed.starts_with('\u{256d}')
                    || trimmed.starts_with('\u{2570}')
                    || trimmed.starts_with('\u{256e}')
                    || trimmed.starts_with('\u{256f}')
                    || trimmed.starts_with('\u{251c}')
                    || trimmed.starts_with('\u{2524}')
                    || trimmed.contains("\u{2500}\u{2500}\u{2500}")
                    || trimmed.starts_with('\u{2500}')
                    || trimmed.starts_with('\u{250c}')
                    || trimmed.starts_with('\u{2514}')
                    || trimmed.starts_with('\u{2510}')
                    || trimmed.starts_with('\u{2518}')
                {
                    return false;
                }
                // Skip UI elements
                if trimmed.contains("Tip:")
                    || trimmed.contains("https://")
                    || trimmed.contains("/status")
                    || trimmed.contains("/model")
                    || trimmed.contains("context left")
                    || trimmed.contains("shortcuts")
                    || trimmed.contains("ctrl+")
                {
                    return false;
                }
                // Skip update banner
                if trimmed.contains("Update available")
                    || trimmed.contains("npm install")
                    || trimmed.contains("OpenAI Codex")
                    || trimmed.contains("release notes")
                {
                    return false;
                }
                // Skip model/directory info lines
                if trimmed.contains("model:") || trimmed.contains("directory:") {
                    return false;
                }
                // Skip prompt marker line
                if trimmed.starts_with('\u{203a}') {
                    return false;
                }
                true
            })
            .collect();

        // Look for bullet marker lines
        let mut result = String::new();
        for line in &lines {
            let trimmed = line.trim();
            if let Some(pos) = trimmed.find('\u{2022}') {
                let after = &trimmed[pos + '\u{2022}'.len_utf8()..];
                let text = after.trim();
                if !text.is_empty() && !text.contains("Working") {
                    if !result.is_empty() {
                        result.push(' ');
                    }
                    result.push_str(text);
                }
            }
        }

        let result = strip_spinner_prefix(result.trim());

        if result.is_empty() {
            let fallback = lines.join(" ").trim().to_string();
            strip_spinner_prefix(&fallback)
        } else {
            result
        }
    }

    fn classify(&self, text: &str) -> MessageClass {
        let trimmed = text.trim();

        // AI response marker
        if trimmed.contains('\u{2022}') {
            return MessageClass::AiResponse;
        }

        // Prompt ready marker
        if trimmed.contains('\u{203a}') {
            return MessageClass::PromptReady;
        }

        // Thinking/working indicator
        if trimmed.contains("Working") || trimmed.contains("esc to interrupt") {
            return MessageClass::ThinkingIndicator;
        }

        // Menu (update available)
        if trimmed.contains("Update available") || trimmed.contains("Update now") {
            return MessageClass::Menu;
        }

        // Error (rate limit)
        if trimmed.contains("hit your usage limit") || trimmed.contains("rate limit") {
            return MessageClass::Error;
        }

        // Box-drawing / UI elements
        if trimmed.starts_with('\u{256d}')
            || trimmed.starts_with('\u{2570}')
            || trimmed.starts_with('\u{2502}')
            || trimmed.starts_with('\u{2500}')
            || trimmed.contains("\u{2500}\u{2500}\u{2500}")
        {
            return MessageClass::UiElement;
        }

        // Tips and info UI
        if trimmed.contains("Tip:")
            || trimmed.contains("100% context left")
            || trimmed.contains("shortcuts")
        {
            return MessageClass::UiElement;
        }

        // Info messages
        if trimmed.contains("model:") || trimmed.contains("directory:") {
            return MessageClass::InfoMessage;
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
        CliTool::Codex
    }
}

/// Submitter for sending prompts to Codex CLI via PTY.
pub struct CodexPromptSubmitter;

impl CodexPromptSubmitter {
    /// Create a new prompt submitter for Codex.
    pub fn new() -> Self {
        Self
    }
}

impl Default for CodexPromptSubmitter {
    fn default() -> Self {
        Self::new()
    }
}

impl PromptSubmitter for CodexPromptSubmitter {
    fn send_prompt(&self, writer: &mut dyn io::Write, prompt: &str) -> io::Result<()> {
        // Ctrl+U to clear the line, then type prompt, then Enter
        writer.write_all(b"\x15")?;
        writer.write_all(prompt.as_bytes())?;
        writer.write_all(b"\r")?;
        writer.flush()
    }

    fn send_command(&self, writer: &mut dyn io::Write, command: &str) -> io::Result<()> {
        // Ctrl+U to clear the line, then type command, then Enter
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
        if output.contains("Update available") {
            return StartupAction::SendInput("2\n".to_string());
        }
        if output.contains('\u{203a}') {
            return StartupAction::Ready;
        }
        StartupAction::Waiting
    }

    fn tool(&self) -> CliTool {
        CliTool::Codex
    }

    fn requires_char_by_char(&self) -> bool {
        true
    }
}

/// Strip ANSI escape codes from text.
pub fn strip_ansi_codes(text: &str) -> String {
    static ANSI_RE: OnceLock<Regex> = OnceLock::new();
    let re = ANSI_RE.get_or_init(|| {
        Regex::new(r"\x1B(?:[@-Z\\-_]|\[[0-?]*[ -/]*[@-~])").unwrap()
    });
    re.replace_all(text, "").to_string()
}

/// Parse a number with optional K/M suffix.
fn parse_number_with_suffix(num_str: &str, suffix: Option<&str>) -> u64 {
    let num = num_str.replace(",", "").parse::<f64>().unwrap_or(0.0);
    let multiplier = match suffix {
        Some("K") | Some("k") => 1000.0,
        Some("M") | Some("m") => 1_000_000.0,
        _ => 1.0,
    };
    (num * multiplier) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_command_to_pty_input() {
        assert_eq!(
            CodexCommand::Status.to_pty_input(),
            "/status\n"
        );
        assert_eq!(
            CodexCommand::Approvals {
                mode: Some(ApprovalMode::Auto)
            }
            .to_pty_input(),
            "/approvals auto\n"
        );
        assert_eq!(
            CodexCommand::Model {
                model: Some("gpt-5.2-codex".to_string())
            }
            .to_pty_input(),
            "/model gpt-5.2-codex\n"
        );
    }

    #[test]
    fn test_special_input_to_pty_input() {
        assert_eq!(
            CodexSpecialInput::FileSearch("src/main.rs".to_string()).to_pty_input(),
            "@src/main.rs\n"
        );
        assert_eq!(
            CodexSpecialInput::ShellCommand("ls -la".to_string()).to_pty_input(),
            "!ls -la\n"
        );
        assert_eq!(
            CodexSpecialInput::SkillInvoke("skill-creator".to_string()).to_pty_input(),
            "$skill-creator\n"
        );
    }

    #[test]
    fn test_control_to_bytes() {
        assert_eq!(CodexControl::Cancel.to_bytes(), vec![0x03]);
        assert_eq!(CodexControl::OpenEditor.to_bytes(), vec![0x07]);
        assert_eq!(CodexControl::ForkTranscript.to_bytes(), vec![0x1B, 0x1B]);
    }

    #[test]
    fn test_parse_rate_limit() {
        let text = "■ You've hit your usage limit. Upgrade to Pro (https://openai.com/chatgpt/pricing), or try again at Jan 23rd, 2026 4:06 AM";
        let response = CodexOutputParser::parse_rate_limit(text).unwrap();

        match response {
            CodexResponse::RateLimitHit {
                message,
                reset_time,
                upgrade_url,
            } => {
                assert!(message.contains("hit your usage limit"));
                assert_eq!(reset_time, Some("Jan 23rd, 2026 4:06 AM".to_string()));
                assert_eq!(
                    upgrade_url,
                    Some("https://openai.com/chatgpt/pricing".to_string())
                );
            }
            _ => panic!("Expected RateLimitHit"),
        }
    }

    #[test]
    fn test_parse_welcome_screen() {
        let text = "OpenAI Codex (v0.85.0)\nmodel: gpt-5.2-codex\n100% context left";
        let response = CodexOutputParser::parse_welcome_screen(text).unwrap();

        match response {
            CodexResponse::WelcomeScreen {
                version,
                model,
                context_percent,
            } => {
                assert_eq!(version, "0.85.0");
                assert_eq!(model, "gpt-5.2-codex");
                assert_eq!(context_percent, 100.0);
            }
            _ => panic!("Expected WelcomeScreen"),
        }
    }

    #[test]
    fn test_parse_token_usage() {
        let text = "Token usage: 7.49K total (7.38K input + 105 output)\nContext window: 100% left (7.49K used / 272K)";
        let response = CodexOutputParser::parse_token_usage(text).unwrap();

        match response {
            CodexResponse::TokenUsage {
                input,
                output,
                total,
                context_percent_left,
                context_used,
                context_total,
            } => {
                assert_eq!(total, 7490);
                assert_eq!(input, 7380);
                assert_eq!(output, 105);
                assert_eq!(context_percent_left, Some(100.0));
                assert_eq!(context_used, Some(7490));
                assert_eq!(context_total, Some(272000));
            }
            _ => panic!("Expected TokenUsage"),
        }
    }

    #[test]
    fn test_parse_update_available() {
        let text = "✨ Update available! 0.85.0 -> 0.87.0";
        let response = CodexOutputParser::parse_update_available(text).unwrap();

        match response {
            CodexResponse::UpdateAvailable {
                current_version,
                new_version,
            } => {
                assert_eq!(current_version, "0.85.0");
                assert_eq!(new_version, "0.87.0");
            }
            _ => panic!("Expected UpdateAvailable"),
        }
    }

    #[test]
    fn test_parse_working_indicator() {
        let text = "• Working (0s • esc to interrupt)";
        let response = CodexOutputParser::parse_working_indicator(text).unwrap();

        match response {
            CodexResponse::Working { elapsed_seconds } => {
                assert_eq!(elapsed_seconds, 0);
            }
            _ => panic!("Expected Working"),
        }
    }

    #[test]
    fn test_strip_ansi_codes() {
        let text = "\x1B[1;32mGreen\x1B[0m Normal";
        let stripped = strip_ansi_codes(text);
        assert_eq!(stripped, "Green Normal");
    }

    #[test]
    fn test_parse_number_with_suffix() {
        assert_eq!(parse_number_with_suffix("7.49", Some("K")), 7490);
        assert_eq!(parse_number_with_suffix("272", Some("K")), 272000);
        assert_eq!(parse_number_with_suffix("1.5", Some("M")), 1500000);
        assert_eq!(parse_number_with_suffix("105", None), 105);
    }

    #[test]
    fn test_parse_context_left() {
        // Simple format
        let text = "100% context left";
        let response = CodexOutputParser::parse_context_left(text).unwrap();
        match response {
            CodexResponse::ContextLeft { percent } => {
                assert_eq!(percent, 100.0);
            }
            _ => panic!("Expected ContextLeft"),
        }

        // With additional text
        let text = "100% context left · ? for shortcuts";
        let response = CodexOutputParser::parse_context_left(text).unwrap();
        match response {
            CodexResponse::ContextLeft { percent } => {
                assert_eq!(percent, 100.0);
            }
            _ => panic!("Expected ContextLeft"),
        }

        // Partial context
        let text = "75% context left";
        let response = CodexOutputParser::parse_context_left(text).unwrap();
        match response {
            CodexResponse::ContextLeft { percent } => {
                assert_eq!(percent, 75.0);
            }
            _ => panic!("Expected ContextLeft"),
        }
    }

    #[test]
    fn test_parse_model_menu() {
        let text = r#"Select Model and Effort
› 1. gpt-5.2-codex (current)  Latest frontier agentic coding model.
  2. gpt-5.1-codex-max        Codex-optimized flagship for deep and fast reasoning.
  3. gpt-5.1-codex-mini       Optimized for codex. Cheaper, faster, but less capable.
  4. gpt-5.2                   Latest frontier model..."#;

        let response = CodexOutputParser::parse_model_menu(text).unwrap();

        match response {
            CodexResponse::ModelMenu { models, current_index } => {
                assert_eq!(models.len(), 4);
                assert_eq!(current_index, 0); // First item is selected with ›

                // Check first model
                assert_eq!(models[0].number, 1);
                assert_eq!(models[0].name, "gpt-5.2-codex");
                assert!(models[0].is_current);
                assert!(models[0].description.contains("Latest frontier"));

                // Check second model
                assert_eq!(models[1].number, 2);
                assert_eq!(models[1].name, "gpt-5.1-codex-max");
                assert!(!models[1].is_current);
                assert!(models[1].description.contains("Codex-optimized"));

                // Check third model
                assert_eq!(models[2].number, 3);
                assert_eq!(models[2].name, "gpt-5.1-codex-mini");
                assert!(!models[2].is_current);
            }
            _ => panic!("Expected ModelMenu"),
        }
    }

    #[test]
    fn test_codex_slash_status() {
        // This test verifies that /status output is properly parsed
        let text = r#"Model: gpt-5.2-codex
Token usage: 7.49K total (7.38K input + 105 output)
Context window: 100% left (7.49K used / 272K)"#;

        let responses = CodexOutputParser::parse_text(text);

        // Should parse token usage
        assert!(responses.iter().any(|r| matches!(r, CodexResponse::TokenUsage { .. })));
    }

    #[test]
    fn test_context_left_standalone() {
        // Test that standalone context left is parsed, but not when part of welcome screen
        let text = "100% context left";
        let responses = CodexOutputParser::parse_text(text);

        // Should contain ContextLeft response
        assert_eq!(responses.len(), 1);
        match &responses[0] {
            CodexResponse::ContextLeft { percent } => {
                assert_eq!(*percent, 100.0);
            }
            _ => panic!("Expected ContextLeft, got {:?}", responses[0]),
        }

        // When part of welcome screen, should only return WelcomeScreen
        let text = r#"╭───────────────────────────────────────────────────╮
│ >_ OpenAI Codex (v0.87.0)                         │
│                                                   │
│ model:     gpt-5.2-codex   /model to change       │
│ directory: ~\CODING\ML_TRADING\nemo\…\crates\core │
╰───────────────────────────────────────────────────╯
100% context left"#;

        let responses = CodexOutputParser::parse_text(text);

        // Should contain WelcomeScreen, but not duplicate ContextLeft
        assert!(responses.iter().any(|r| matches!(r, CodexResponse::WelcomeScreen { .. })));
        assert!(!responses.iter().any(|r| matches!(r, CodexResponse::ContextLeft { .. })));
    }
}

/// Pipe-mode spawn builder for Codex.
///
/// Implements `CliCommandBuilder` for use in `pipe/process.rs` dispatch.
///
/// Argv produced (fresh session):
///   `codex exec --json --full-auto --skip-git-repo-check <prompt>`
///
/// Argv produced (resumed session):
///   `codex exec resume <session_id> --json --full-auto --skip-git-repo-check <prompt>`
///
/// Note the `exec resume <id>` sub-sub-command shape — this is why per-CLI
/// function builders are used instead of a declarative `ResumeMode` enum.
///
/// `--full-auto`: non-interactive execution mode; without this, Codex blocks
///   on interactive tool approval prompts when piped, causing the reader loop
///   to hang forever.
/// `--skip-git-repo-check`: allows spawning Codex in non-git directories
///   (chart app sessions, daemon contexts).
pub struct CodexPipeBuilder;

impl CliCommandBuilder for CodexPipeBuilder {
    fn build_command(&self, opts: &SpawnOptions) -> std::process::Command {
        let mut cmd = std::process::Command::new("codex");

        if let Some(ref session_id) = opts.resume_session_id {
            // Resume shape: `codex exec resume <id> --json --full-auto ...`
            cmd.arg("exec");
            cmd.arg("resume");
            cmd.arg(session_id);
        } else {
            // Fresh shape: `codex exec --json --full-auto ...`
            cmd.arg("exec");
        }

        cmd.arg("--json");
        // Codex approval mode is a positional flag, not --permission-mode.
        // Map SpawnOptions::permission_mode to the correct Codex CLI flag.
        match opts.permission_mode.as_deref() {
            Some("suggest") => {
                cmd.arg("--suggest");
            }
            Some("auto-edit") => {
                cmd.arg("--auto-edit");
            }
            _ => {
                cmd.arg("--full-auto");
            }
        }
        cmd.arg("--skip-git-repo-check");

        for arg in &opts.extra_args {
            cmd.arg(arg);
        }

        // Prompt is the final positional argument.
        cmd.arg(&opts.prompt);
        cmd
    }
}
