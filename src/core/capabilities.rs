//! Static capability descriptors for each supported CLI tool.
//!
//! Each descriptor lists the models, permission modes, and feature flags
//! available for that tool. Returned by [`crate::core::types::CliTool::capabilities()`].
//!
//! The data is entirely compile-time — no heap allocations occur at call sites.

use serde::{Deserialize, Serialize};

/// Metadata about one model offered by a CLI tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    /// CLI-level identifier passed to `--model` / `-m`.
    /// e.g. `"claude-sonnet-4"`, `"gemini-2.5-pro"`, `"opencode/gpt-5-nano"`
    pub id: &'static str,

    /// Human-readable label for UI display.
    /// e.g. `"Claude Sonnet 4"`, `"Gemini 2.5 Pro"`
    pub display_name: &'static str,

    /// Whether this is the tool's default when `SpawnOptions::model` is `None`.
    pub is_default: bool,

    /// Whether this model requires no paid API key (free tier / bundled auth).
    pub is_free_tier: bool,

    /// Optional context window size in tokens (for display in model picker).
    pub context_window: Option<u32>,
}

/// Metadata about one permission mode offered by a CLI tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionModeInfo {
    /// Value passed to the CLI flag (e.g. `"acceptEdits"`, `"full-auto"`).
    pub id: &'static str,

    /// Human-readable label (e.g. `"Accept Edits"`, `"Full Auto"`).
    pub display_name: &'static str,

    /// One-line description of what this mode allows.
    pub description: &'static str,

    /// Whether this mode is the default when `SpawnOptions::permission_mode` is `None`.
    pub is_default: bool,
}

/// Feature flags describing what a CLI tool supports.
/// All fields default to `false`; set only what the tool actually supports.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct CliFeatures {
    /// Supports extended thinking / reasoning blocks.
    pub thinking: bool,

    /// Supports an effort/budget slider (low / medium / high / max).
    pub effort_control: bool,

    /// Supports MCP server configuration (`--mcp-config`).
    pub mcp: bool,

    /// Supports resuming a previous session by ID.
    pub resume: bool,

    /// Supports `--continue` / `continue_last` to resume most recent session.
    pub continue_last: bool,

    /// Supports restricting which tools the agent may use (`--allowedTools`).
    pub allowed_tools_filter: bool,

    /// Supports injecting extra system-prompt text (`--append-system-prompt`).
    pub system_prompt_injection: bool,

    /// Supports capping the number of agentic turns (`--max-turns`).
    pub max_turns: bool,

    /// Supports a sandbox / isolation mode for tool execution.
    pub sandbox_mode: bool,

    /// Supports IDE context (reads open files / workspace from editor integration).
    pub ide_context: bool,

    /// Supports a "plan mode" that shows a plan before executing.
    pub plan_mode: bool,

    /// Supports a speed/performance toggle (fast vs. quality).
    pub speed_toggle: bool,

    /// Supports multi-provider routing (provider/model syntax).
    pub multi_provider: bool,
}

/// Full static capability descriptor for one CLI tool.
/// Returned by [`crate::core::types::CliTool::capabilities()`].
///
/// `Deserialize` is intentionally not derived: `available_models` and
/// `permission_modes` are `&'static [T]` references to compile-time constants
/// and cannot be deserialized from dynamic data. Use `Serialize` to send
/// capability metadata over JSON; receive it as an opaque JSON value if needed.
#[derive(Debug, Clone, Serialize)]
pub struct CliCapabilities {
    /// Canonical identifier matching `CliTool` variant (snake_case, stable).
    pub tool_id: &'static str,

    /// Human-readable product name.
    pub display_name: &'static str,

    /// CLI binary name as it appears on PATH.
    pub binary: &'static str,

    /// All known models, in preferred display order.
    /// First model with `is_default = true` is the picker default.
    pub available_models: &'static [ModelInfo],

    /// All supported permission modes, in preferred display order.
    /// First mode with `is_default = true` is the picker default.
    pub permission_modes: &'static [PermissionModeInfo],

    /// Feature flag set for this tool.
    pub features: CliFeatures,
}

impl CliCapabilities {
    /// Returns the default model for this tool (first with `is_default = true`),
    /// or the first model in the list if none is marked default.
    pub fn default_model(&self) -> Option<&ModelInfo> {
        self.available_models
            .iter()
            .find(|m| m.is_default)
            .or_else(|| self.available_models.first())
    }

    /// Returns the default permission mode, or `None` if the tool has no modes.
    pub fn default_permission_mode(&self) -> Option<&PermissionModeInfo> {
        self.permission_modes
            .iter()
            .find(|p| p.is_default)
            .or_else(|| self.permission_modes.first())
    }
}

// ── Claude Code ──────────────────────────────────────────────────────────────

static CLAUDE_MODELS: &[ModelInfo] = &[
    ModelInfo {
        id: "claude-opus-4",
        display_name: "Claude Opus 4",
        is_default: false,
        is_free_tier: false,
        context_window: Some(200_000),
    },
    ModelInfo {
        id: "claude-opus-4-1m",
        display_name: "Claude Opus 4 (1M ctx)",
        is_default: false,
        is_free_tier: false,
        context_window: Some(1_000_000),
    },
    ModelInfo {
        id: "claude-sonnet-4",
        display_name: "Claude Sonnet 4",
        is_default: true,
        is_free_tier: false,
        context_window: Some(200_000),
    },
    ModelInfo {
        id: "claude-sonnet-4-1m",
        display_name: "Claude Sonnet 4 (1M ctx)",
        is_default: false,
        is_free_tier: false,
        context_window: Some(1_000_000),
    },
    ModelInfo {
        id: "claude-haiku-4",
        display_name: "Claude Haiku 4",
        is_default: false,
        is_free_tier: false,
        context_window: Some(200_000),
    },
    ModelInfo {
        id: "opus",
        display_name: "Opus (alias)",
        is_default: false,
        is_free_tier: false,
        context_window: None,
    },
    ModelInfo {
        id: "sonnet",
        display_name: "Sonnet (alias)",
        is_default: false,
        is_free_tier: false,
        context_window: None,
    },
    ModelInfo {
        id: "haiku",
        display_name: "Haiku (alias)",
        is_default: false,
        is_free_tier: false,
        context_window: None,
    },
];

static CLAUDE_PERMISSION_MODES: &[PermissionModeInfo] = &[
    PermissionModeInfo {
        id: "default",
        display_name: "Default",
        description: "Standard interactive permissions; asks before file edits.",
        is_default: true,
    },
    PermissionModeInfo {
        id: "acceptEdits",
        display_name: "Accept Edits",
        description: "Auto-accepts file edits, asks for shell commands.",
        is_default: false,
    },
    PermissionModeInfo {
        id: "auto",
        display_name: "Auto",
        description: "Approves most operations automatically.",
        is_default: false,
    },
    PermissionModeInfo {
        id: "bypassPermissions",
        display_name: "Bypass Permissions",
        description: "Skips all permission checks (equivalent to --dangerously-skip-permissions).",
        is_default: false,
    },
    PermissionModeInfo {
        id: "plan",
        display_name: "Plan Mode",
        description: "Shows a plan and asks for approval before executing.",
        is_default: false,
    },
];

pub(crate) static CLAUDE_CAPABILITIES: CliCapabilities = CliCapabilities {
    tool_id: "claude_code",
    display_name: "Claude Code",
    binary: "claude",
    available_models: CLAUDE_MODELS,
    permission_modes: CLAUDE_PERMISSION_MODES,
    features: CliFeatures {
        thinking: true,
        effort_control: true,
        mcp: true,
        resume: true,
        continue_last: true,
        allowed_tools_filter: true,
        system_prompt_injection: true,
        max_turns: true,
        sandbox_mode: false,
        ide_context: false,
        plan_mode: true,
        speed_toggle: false,
        multi_provider: false,
    },
};

// ── Codex ─────────────────────────────────────────────────────────────────────

static CODEX_MODELS: &[ModelInfo] = &[
    ModelInfo {
        id: "gpt-5.4",
        display_name: "GPT-5.4",
        is_default: true,
        is_free_tier: false,
        context_window: None,
    },
    ModelInfo {
        id: "gpt-5.4-mini",
        display_name: "GPT-5.4 Mini",
        is_default: false,
        is_free_tier: false,
        context_window: None,
    },
    ModelInfo {
        id: "gpt-5.3-codex",
        display_name: "GPT-5.3 Codex",
        is_default: false,
        is_free_tier: false,
        context_window: None,
    },
    ModelInfo {
        id: "gpt-5.2",
        display_name: "GPT-5.2",
        is_default: false,
        is_free_tier: false,
        context_window: None,
    },
];

// Codex uses --full-auto / --suggest / --auto-edit as approval mode flags (not a --permission-mode
// string). We surface them as permission modes so the UI can offer a picker. The builder maps
// `permission_mode` → the correct Codex CLI flag.
static CODEX_PERMISSION_MODES: &[PermissionModeInfo] = &[
    PermissionModeInfo {
        id: "suggest",
        display_name: "Suggest",
        description: "Read-only: proposes changes but does not apply them.",
        is_default: false,
    },
    PermissionModeInfo {
        id: "auto-edit",
        display_name: "Auto Edit",
        description: "Edits files automatically; asks before running commands.",
        is_default: false,
    },
    PermissionModeInfo {
        id: "full-auto",
        display_name: "Full Auto",
        description: "Fully autonomous: edits and runs commands without asking.",
        is_default: true,
    },
];

pub(crate) static CODEX_CAPABILITIES: CliCapabilities = CliCapabilities {
    tool_id: "codex",
    display_name: "Codex",
    binary: "codex",
    available_models: CODEX_MODELS,
    permission_modes: CODEX_PERMISSION_MODES,
    features: CliFeatures {
        thinking: false,
        effort_control: false,
        mcp: true,
        resume: true,
        continue_last: true,
        allowed_tools_filter: false,
        system_prompt_injection: false,
        max_turns: false,
        sandbox_mode: false,
        ide_context: true,
        plan_mode: true,
        speed_toggle: true,
        multi_provider: false,
    },
};

// ── Gemini ────────────────────────────────────────────────────────────────────

static GEMINI_MODELS: &[ModelInfo] = &[
    ModelInfo {
        id: "gemini-2.5-pro",
        display_name: "Gemini 2.5 Pro",
        is_default: false,
        is_free_tier: false,
        context_window: Some(1_000_000),
    },
    ModelInfo {
        id: "gemini-2.5-flash",
        display_name: "Gemini 2.5 Flash",
        is_default: false,
        is_free_tier: true,
        context_window: Some(1_000_000),
    },
    ModelInfo {
        id: "gemini-3.1-pro",
        display_name: "Gemini 3.1 Pro",
        is_default: true,
        is_free_tier: false,
        context_window: Some(2_000_000),
    },
    ModelInfo {
        id: "gemini-3.0-flash",
        display_name: "Gemini 3.0 Flash",
        is_default: false,
        is_free_tier: true,
        context_window: Some(1_000_000),
    },
];

static GEMINI_PERMISSION_MODES: &[PermissionModeInfo] = &[
    PermissionModeInfo {
        id: "default",
        display_name: "Default",
        description: "Standard Gemini permissions.",
        is_default: true,
    },
    PermissionModeInfo {
        id: "auto-edit",
        display_name: "Auto Edit",
        description: "Auto-applies file edits.",
        is_default: false,
    },
    PermissionModeInfo {
        id: "yolo",
        display_name: "YOLO",
        description: "No permission prompts; maximum autonomy.",
        is_default: false,
    },
    PermissionModeInfo {
        id: "plan",
        display_name: "Plan",
        description: "Gemini shows a plan before executing tool calls.",
        is_default: false,
    },
];

pub(crate) static GEMINI_CAPABILITIES: CliCapabilities = CliCapabilities {
    tool_id: "gemini",
    display_name: "Gemini",
    binary: "gemini",
    available_models: GEMINI_MODELS,
    permission_modes: GEMINI_PERMISSION_MODES,
    features: CliFeatures {
        thinking: false,
        effort_control: false,
        mcp: true,
        resume: true,
        continue_last: false, // Gemini has no --continue; use resume_session_id="latest"
        allowed_tools_filter: false,
        system_prompt_injection: false,
        max_turns: false,
        sandbox_mode: true,
        ide_context: false,
        plan_mode: true,
        speed_toggle: false,
        multi_provider: false,
    },
};

// ── OpenCode ──────────────────────────────────────────────────────────────────

static OPENCODE_MODELS: &[ModelInfo] = &[
    // Free tier (no API key required)
    ModelInfo {
        id: "opencode/gpt-5-nano",
        display_name: "OpenCode GPT-5 Nano (free)",
        is_default: true,
        is_free_tier: true,
        context_window: None,
    },
    // Anthropic via API key
    ModelInfo {
        id: "anthropic/claude-sonnet-4-5",
        display_name: "Claude Sonnet 4.5",
        is_default: false,
        is_free_tier: false,
        context_window: None,
    },
    ModelInfo {
        id: "anthropic/claude-opus-4",
        display_name: "Claude Opus 4",
        is_default: false,
        is_free_tier: false,
        context_window: None,
    },
    // OpenAI via API key
    ModelInfo {
        id: "openai/gpt-4o",
        display_name: "GPT-4o",
        is_default: false,
        is_free_tier: false,
        context_window: None,
    },
    ModelInfo {
        id: "openai/gpt-5",
        display_name: "GPT-5",
        is_default: false,
        is_free_tier: false,
        context_window: None,
    },
    // Google via API key
    ModelInfo {
        id: "google/gemini-2.5-pro",
        display_name: "Gemini 2.5 Pro",
        is_default: false,
        is_free_tier: false,
        context_window: None,
    },
];

// OpenCode has no permission modes concept.
static OPENCODE_PERMISSION_MODES: &[PermissionModeInfo] = &[];

pub(crate) static OPENCODE_CAPABILITIES: CliCapabilities = CliCapabilities {
    tool_id: "opencode",
    display_name: "OpenCode",
    binary: "opencode",
    available_models: OPENCODE_MODELS,
    permission_modes: OPENCODE_PERMISSION_MODES,
    features: CliFeatures {
        thinking: true, // depends on provider/model; surfaced when provider supports it
        effort_control: false,
        mcp: true,
        resume: true,
        continue_last: true,
        allowed_tools_filter: false,
        system_prompt_injection: false,
        max_turns: false,
        sandbox_mode: false,
        ide_context: false,
        plan_mode: false,
        speed_toggle: false,
        multi_provider: true,
    },
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::CliTool;

    #[test]
    fn each_tool_has_exactly_one_default_model() {
        for tool in [CliTool::ClaudeCode, CliTool::Codex, CliTool::Gemini, CliTool::OpenCode] {
            let caps = tool.capabilities();
            let default_count = caps.available_models.iter().filter(|m| m.is_default).count();
            assert_eq!(
                default_count,
                1,
                "{:?} must have exactly one default model, found {}",
                tool,
                default_count
            );
        }
    }

    #[test]
    fn each_tool_with_modes_has_exactly_one_default_mode() {
        for tool in [CliTool::ClaudeCode, CliTool::Codex, CliTool::Gemini] {
            let caps = tool.capabilities();
            let default_count = caps.permission_modes.iter().filter(|p| p.is_default).count();
            assert_eq!(
                default_count,
                1,
                "{:?} must have exactly one default permission mode, found {}",
                tool,
                default_count
            );
        }
        // OpenCode has zero permission modes — that is valid.
        assert!(CliTool::OpenCode.capabilities().permission_modes.is_empty());
    }

    #[test]
    fn capabilities_returns_correct_tool_id() {
        assert_eq!(CliTool::ClaudeCode.capabilities().tool_id, "claude_code");
        assert_eq!(CliTool::Codex.capabilities().tool_id, "codex");
        assert_eq!(CliTool::Gemini.capabilities().tool_id, "gemini");
        assert_eq!(CliTool::OpenCode.capabilities().tool_id, "opencode");
    }

    #[test]
    fn model_ids_are_nonempty() {
        for tool in [CliTool::ClaudeCode, CliTool::Codex, CliTool::Gemini, CliTool::OpenCode] {
            let caps = tool.capabilities();
            for model in caps.available_models {
                assert!(
                    !model.id.is_empty(),
                    "{:?} has a model with an empty id (display_name={})",
                    tool,
                    model.display_name
                );
            }
        }
    }

    #[test]
    fn static_refs_are_identity() {
        // Confirms capabilities() returns a pointer to a static — no heap allocation.
        let p1 = CliTool::ClaudeCode.capabilities() as *const CliCapabilities;
        let p2 = CliTool::ClaudeCode.capabilities() as *const CliCapabilities;
        assert_eq!(p1, p2, "capabilities() must return a pointer to the same static");
    }

    #[test]
    fn claude_has_five_permission_modes() {
        assert_eq!(CliTool::ClaudeCode.capabilities().permission_modes.len(), 5);
    }

    #[test]
    fn codex_has_three_permission_modes() {
        assert_eq!(CliTool::Codex.capabilities().permission_modes.len(), 3);
    }

    #[test]
    fn gemini_has_four_permission_modes() {
        assert_eq!(CliTool::Gemini.capabilities().permission_modes.len(), 4);
    }

    #[test]
    fn opencode_has_zero_permission_modes() {
        assert_eq!(CliTool::OpenCode.capabilities().permission_modes.len(), 0);
    }

    #[test]
    fn claude_features_correct() {
        let f = CliTool::ClaudeCode.capabilities().features;
        assert!(f.thinking);
        assert!(f.effort_control);
        assert!(f.plan_mode);
        assert!(f.mcp);
        assert!(f.resume);
        assert!(!f.multi_provider);
        assert!(!f.sandbox_mode);
    }

    #[test]
    fn codex_features_correct() {
        let f = CliTool::Codex.capabilities().features;
        assert!(f.ide_context);
        assert!(f.plan_mode);
        assert!(f.resume);
        assert!(!f.thinking);
        assert!(!f.multi_provider);
    }

    #[test]
    fn gemini_features_correct() {
        let f = CliTool::Gemini.capabilities().features;
        assert!(f.sandbox_mode);
        assert!(f.plan_mode);
        assert!(f.resume);
        assert!(!f.continue_last);
        assert!(!f.thinking);
    }

    #[test]
    fn opencode_features_correct() {
        let f = CliTool::OpenCode.capabilities().features;
        assert!(f.multi_provider);
        assert!(f.resume);
        assert!(f.thinking);
        assert!(!f.plan_mode);
        assert!(!f.sandbox_mode);
    }

    #[test]
    fn default_model_helper_works() {
        for tool in [CliTool::ClaudeCode, CliTool::Codex, CliTool::Gemini, CliTool::OpenCode] {
            let caps = tool.capabilities();
            let default_model = caps.default_model();
            assert!(
                default_model.is_some(),
                "{:?} default_model() must return Some",
                tool
            );
            assert!(
                default_model.unwrap().is_default,
                "{:?} default_model() must return a model with is_default=true",
                tool
            );
        }
    }
}
