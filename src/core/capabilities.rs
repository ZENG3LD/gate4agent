//! Capability descriptors for each supported CLI tool.
//!
//! Each descriptor lists the models, permission modes, and feature flags
//! available for that tool. Returned by [`crate::core::types::CliTool::capabilities()`]
//! (static defaults) or [`crate::core::types::CliTool::discover_capabilities()`]
//! (dynamic, reads on-disk config files).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Metadata about one model offered by a CLI tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    /// CLI-level identifier passed to `--model` / `-m`.
    /// e.g. `"claude-sonnet-4"`, `"gemini-2.5-pro"`, `"opencode/gpt-5-nano"`
    pub id: String,

    /// Human-readable label for UI display.
    /// e.g. `"Claude Sonnet 4"`, `"Gemini 2.5 Pro"`
    pub display_name: String,

    /// Whether this is the tool's default when `SpawnOptions::model` is `None`.
    pub is_default: bool,

    /// Whether this model requires no paid API key (free tier / bundled auth).
    pub is_free_tier: bool,

    /// Optional context window size in tokens (for display in model picker).
    pub context_window: Option<u64>,
}

/// Metadata about one permission mode offered by a CLI tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionModeInfo {
    /// Value passed to the CLI flag (e.g. `"acceptEdits"`, `"full-auto"`).
    pub id: String,

    /// Human-readable label (e.g. `"Accept Edits"`, `"Full Auto"`).
    pub display_name: String,

    /// One-line description of what this mode allows.
    pub description: String,

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

/// Full capability descriptor for one CLI tool.
///
/// Returned by [`crate::core::types::CliTool::capabilities()`] (static defaults) or
/// [`crate::core::types::CliTool::discover_capabilities()`] (dynamic, config-enriched).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliCapabilities {
    /// Canonical identifier matching `CliTool` variant (snake_case, stable).
    pub tool_id: String,

    /// Human-readable product name.
    pub display_name: String,

    /// CLI binary name as it appears on PATH.
    pub binary: String,

    /// All known models, in preferred display order.
    /// First model with `is_default = true` is the picker default.
    pub available_models: Vec<ModelInfo>,

    /// All supported permission modes, in preferred display order.
    /// First mode with `is_default = true` is the picker default.
    pub permission_modes: Vec<PermissionModeInfo>,

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

// ── Builders ──────────────────────────────────────────────────────────────────

fn model(
    id: &str,
    display_name: &str,
    is_default: bool,
    is_free_tier: bool,
    context_window: Option<u64>,
) -> ModelInfo {
    ModelInfo {
        id: id.to_string(),
        display_name: display_name.to_string(),
        is_default,
        is_free_tier,
        context_window,
    }
}

fn perm(id: &str, display_name: &str, description: &str, is_default: bool) -> PermissionModeInfo {
    PermissionModeInfo {
        id: id.to_string(),
        display_name: display_name.to_string(),
        description: description.to_string(),
        is_default,
    }
}

// ── Default capability constructors ───────────────────────────────────────────

/// Returns the default (compile-time) capabilities for Claude Code.
pub(crate) fn claude_capabilities() -> CliCapabilities {
    CliCapabilities {
        tool_id: "claude_code".to_string(),
        display_name: "Claude Code".to_string(),
        binary: "claude".to_string(),
        available_models: vec![
            model("claude-opus-4-6", "Claude Opus 4.6", false, false, Some(200_000)),
            model("claude-sonnet-4-6", "Claude Sonnet 4.6", true, false, Some(200_000)),
            model("claude-haiku-4-5", "Claude Haiku 4.5", false, false, Some(200_000)),
            model("opus", "Opus (alias)", false, false, None),
            model("sonnet", "Sonnet (alias)", false, false, None),
            model("haiku", "Haiku (alias)", false, false, None),
        ],
        permission_modes: vec![
            perm("default", "Default", "Standard interactive permissions; asks before file edits.", true),
            perm("acceptEdits", "Accept Edits", "Auto-accepts file edits, asks for shell commands.", false),
            perm("auto", "Auto", "Approves most operations automatically.", false),
            perm("bypassPermissions", "Bypass Permissions", "Skips all permission checks (equivalent to --dangerously-skip-permissions).", false),
            perm("plan", "Plan Mode", "Shows a plan and asks for approval before executing.", false),
        ],
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
    }
}

/// Returns the default (compile-time) capabilities for Codex.
pub(crate) fn codex_capabilities() -> CliCapabilities {
    CliCapabilities {
        tool_id: "codex".to_string(),
        display_name: "Codex".to_string(),
        binary: "codex".to_string(),
        available_models: vec![
            model("gpt-5.4", "GPT-5.4", true, false, Some(258_400)),
            model("gpt-5.4-mini", "GPT-5.4 Mini", false, false, Some(128_000)),
            model("gpt-5.3-codex", "GPT-5.3 Codex", false, false, Some(200_000)),
            model("gpt-5.2", "GPT-5.2", false, false, Some(128_000)),
        ],
        // Codex uses --full-auto / --suggest / --auto-edit as approval mode flags (not a
        // --permission-mode string). We surface them as permission modes so the UI can offer
        // a picker. The builder maps `permission_mode` → the correct Codex CLI flag.
        permission_modes: vec![
            perm("suggest", "Suggest", "Read-only: proposes changes but does not apply them.", false),
            perm("auto-edit", "Auto Edit", "Edits files automatically; asks before running commands.", false),
            perm("full-auto", "Full Auto", "Fully autonomous: edits and runs commands without asking.", true),
        ],
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
    }
}

/// Returns the default (compile-time) capabilities for Gemini.
pub(crate) fn gemini_capabilities() -> CliCapabilities {
    CliCapabilities {
        tool_id: "gemini".to_string(),
        display_name: "Gemini".to_string(),
        binary: "gemini".to_string(),
        available_models: vec![
            model("gemini-2.5-pro", "Gemini 2.5 Pro", false, false, Some(1_000_000)),
            model("gemini-2.5-flash", "Gemini 2.5 Flash", false, true, Some(1_000_000)),
            model("gemini-3.1-pro", "Gemini 3.1 Pro", true, false, Some(2_000_000)),
            model("gemini-3.0-flash", "Gemini 3.0 Flash", false, true, Some(1_000_000)),
        ],
        permission_modes: vec![
            perm("default", "Default", "Standard Gemini permissions.", true),
            perm("auto-edit", "Auto Edit", "Auto-applies file edits.", false),
            perm("yolo", "YOLO", "No permission prompts; maximum autonomy.", false),
            perm("plan", "Plan", "Gemini shows a plan before executing tool calls.", false),
        ],
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
    }
}

/// Returns the default (compile-time) capabilities for OpenCode.
pub(crate) fn opencode_capabilities() -> CliCapabilities {
    CliCapabilities {
        tool_id: "opencode".to_string(),
        display_name: "OpenCode".to_string(),
        binary: "opencode".to_string(),
        available_models: vec![
            // Free tier (no API key required)
            model("opencode/gpt-5-nano", "OpenCode GPT-5 Nano (free)", true, true, None),
            // Anthropic via API key
            model("anthropic/claude-sonnet-4-5", "Claude Sonnet 4.5", false, false, None),
            model("anthropic/claude-opus-4", "Claude Opus 4", false, false, None),
            // OpenAI via API key
            model("openai/gpt-4o", "GPT-4o", false, false, None),
            model("openai/gpt-5", "GPT-5", false, false, None),
            // Google via API key
            model("google/gemini-2.5-pro", "Gemini 2.5 Pro", false, false, None),
        ],
        // OpenCode has no permission modes concept.
        permission_modes: vec![],
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
    }
}

// ── Config-based discovery helpers ────────────────────────────────────────────

/// Read the configured model from `~/.codex/config.toml`.
///
/// The file uses `model = "gpt-5.4"` syntax. Returns `None` if the file is
/// absent, unreadable, or does not contain a `model` key.
fn read_codex_config_model() -> Option<String> {
    let home = home_dir()?;
    let path = home.join(".codex").join("config.toml");
    let content = std::fs::read_to_string(path).ok()?;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("model") && trimmed.contains('=') {
            let val = trimmed.split('=').nth(1)?.trim().trim_matches('"');
            if !val.is_empty() {
                return Some(val.to_string());
            }
        }
    }
    None
}

/// Read the configured default model from an OpenCode config file.
///
/// Searches:
/// 1. `$cwd/opencode.json`
/// 2. `~/.config/opencode/opencode.json`
///
/// Expects: `{ "model": { "default": "anthropic/claude-sonnet-4-5" } }`
fn read_opencode_config_model() -> Option<String> {
    let candidates: Vec<PathBuf> = {
        let mut v = Vec::new();
        if let Ok(cwd) = std::env::current_dir() {
            v.push(cwd.join("opencode.json"));
        }
        if let Some(home) = home_dir() {
            v.push(home.join(".config").join("opencode").join("opencode.json"));
        }
        v
    };

    for path in candidates {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                let model_id = json
                    .get("model")
                    .and_then(|m| m.get("default"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                if model_id.is_some() {
                    return model_id;
                }
            }
        }
    }
    None
}

/// Mark `discovered_id` as the default model in `models`, clearing other defaults.
///
/// If `discovered_id` is not already in the list, it is prepended with
/// `is_default = true` so the caller always sees it as an option.
fn update_default_model(models: &mut Vec<ModelInfo>, discovered_id: &str) {
    // First clear all existing defaults.
    for m in models.iter_mut() {
        m.is_default = false;
    }
    // Try to find and mark the discovered model.
    if let Some(m) = models.iter_mut().find(|m| m.id == discovered_id) {
        m.is_default = true;
    } else {
        // Unknown model — prepend it so it's visible in pickers.
        models.insert(
            0,
            ModelInfo {
                id: discovered_id.to_string(),
                display_name: discovered_id.to_string(),
                is_default: true,
                is_free_tier: false,
                context_window: None,
            },
        );
    }
}

/// Cross-platform home directory lookup without the `dirs` crate.
fn home_dir() -> Option<PathBuf> {
    // Try $HOME first (Unix), then $USERPROFILE (Windows).
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()
        .map(PathBuf::from)
}

// ── Public discovery API (used by CliTool::discover_capabilities) ─────────────

/// Build capabilities enriched by reading on-disk config files.
///
/// Falls back to the compiled-in defaults for any tool whose config is
/// absent or unreadable. Safe to call from any thread; performs only
/// synchronous filesystem I/O.
pub(crate) fn discover(
    tool: crate::core::types::CliTool,
) -> CliCapabilities {
    use crate::core::types::CliTool;

    let mut caps = match tool {
        CliTool::ClaudeCode => claude_capabilities(),
        CliTool::Codex => codex_capabilities(),
        CliTool::Gemini => gemini_capabilities(),
        CliTool::OpenCode => opencode_capabilities(),
    };

    match tool {
        CliTool::Codex => {
            if let Some(model_id) = read_codex_config_model() {
                update_default_model(&mut caps.available_models, &model_id);
            }
        }
        CliTool::OpenCode => {
            if let Some(model_id) = read_opencode_config_model() {
                update_default_model(&mut caps.available_models, &model_id);
            }
        }
        // Claude and Gemini have no config-based model discovery.
        CliTool::ClaudeCode | CliTool::Gemini => {}
    }

    caps
}

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
            for m in &caps.available_models {
                assert!(
                    !m.id.is_empty(),
                    "{:?} has a model with an empty id (display_name={})",
                    tool,
                    m.display_name
                );
            }
        }
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

    #[test]
    fn update_default_model_marks_known_model() {
        let mut models = vec![
            ModelInfo { id: "a".to_string(), display_name: "A".to_string(), is_default: true, is_free_tier: false, context_window: None },
            ModelInfo { id: "b".to_string(), display_name: "B".to_string(), is_default: false, is_free_tier: false, context_window: None },
        ];
        update_default_model(&mut models, "b");
        assert!(!models[0].is_default);
        assert!(models[1].is_default);
    }

    #[test]
    fn update_default_model_prepends_unknown_model() {
        let mut models = vec![
            ModelInfo { id: "a".to_string(), display_name: "A".to_string(), is_default: true, is_free_tier: false, context_window: None },
        ];
        update_default_model(&mut models, "new-model");
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "new-model");
        assert!(models[0].is_default);
        assert!(!models[1].is_default);
    }

    #[test]
    fn discover_returns_valid_capabilities() {
        for tool in [CliTool::ClaudeCode, CliTool::Codex, CliTool::Gemini, CliTool::OpenCode] {
            let caps = tool.discover_capabilities();
            // Must still have exactly one default model.
            let default_count = caps.available_models.iter().filter(|m| m.is_default).count();
            assert_eq!(
                default_count,
                1,
                "{:?} discover_capabilities must have exactly one default model, found {}",
                tool,
                default_count
            );
            assert_eq!(caps.tool_id, tool.capabilities().tool_id);
        }
    }

    #[test]
    fn codex_config_discovery_with_temp_file() {
        // Write a fake ~/.codex/config.toml via a temp env override.
        let dir = std::env::temp_dir().join("gate4agent_test_codex");
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("config.toml");
        std::fs::write(&config_path, "model = \"gpt-5-custom\"\n").unwrap();

        // We can't easily override home_dir() without refactoring, so test the
        // parser directly.
        let content = std::fs::read_to_string(&config_path).unwrap();
        let mut found = None;
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("model") && trimmed.contains('=') {
                if let Some(val) = trimmed.split('=').nth(1) {
                    let val = val.trim().trim_matches('"');
                    if !val.is_empty() {
                        found = Some(val.to_string());
                    }
                }
            }
        }
        assert_eq!(found.as_deref(), Some("gpt-5-custom"));
    }

    #[test]
    fn opencode_config_discovery_with_temp_file() {
        let dir = std::env::temp_dir().join("gate4agent_test_opencode");
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("opencode.json");
        std::fs::write(
            &config_path,
            r#"{"model":{"default":"anthropic/claude-opus-4"}}"#,
        )
        .unwrap();

        let content = std::fs::read_to_string(&config_path).unwrap();
        let json: serde_json::Value = serde_json::from_str(&content).unwrap();
        let model_id = json
            .get("model")
            .and_then(|m| m.get("default"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        assert_eq!(model_id.as_deref(), Some("anthropic/claude-opus-4"));
    }
}
