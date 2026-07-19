use crate::{builtin_adapter_registry, MANAGED_HOOK_REVISION};
use gate4agent_types::{AdapterBinding, AdapterFamily};
use thiserror::Error;

pub const MANAGED_HOOK_TIMEOUT_SECONDS: u64 = 10;
pub const MANAGED_HOOK_TIMEOUT_MILLISECONDS: u64 = MANAGED_HOOK_TIMEOUT_SECONDS * 1_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedHookConfigLocation {
    HomeRelative(&'static str),
    EnvironmentHome {
        variable: &'static str,
        fallback: &'static str,
        suffix: &'static str,
    },
    AppDataOrHome {
        app_data_suffix: &'static str,
        home_fallback: &'static str,
    },
    RuntimeDataRelative(&'static str),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedHookConfigKind {
    JsonHooks {
        container: &'static str,
        require_version_one: bool,
    },
    AmpPlugin,
    HermesPlugin,
    KimiToml,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedHookEventShape {
    NestedCommand {
        matcher: Option<&'static str>,
        timeout: u64,
    },
    DirectCommand {
        timeout: u64,
    },
    CopilotCommand {
        timeout_seconds: u64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ManagedHookEventSpec {
    pub name: &'static str,
    pub shape: ManagedHookEventShape,
    pub passes_event_name: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ManagedHookAdapterSpec {
    pub target: &'static str,
    pub source_adapter: &'static str,
    pub config_location: ManagedHookConfigLocation,
    pub config_kind: ManagedHookConfigKind,
    pub script_stem: &'static str,
    pub events: &'static [ManagedHookEventSpec],
}

const fn nested(name: &'static str, matcher: Option<&'static str>) -> ManagedHookEventSpec {
    ManagedHookEventSpec {
        name,
        shape: ManagedHookEventShape::NestedCommand {
            matcher,
            timeout: MANAGED_HOOK_TIMEOUT_SECONDS,
        },
        passes_event_name: false,
    }
}

const fn nested_ms(name: &'static str) -> ManagedHookEventSpec {
    ManagedHookEventSpec {
        name,
        shape: ManagedHookEventShape::NestedCommand {
            matcher: None,
            timeout: MANAGED_HOOK_TIMEOUT_MILLISECONDS,
        },
        passes_event_name: false,
    }
}

const fn direct(name: &'static str, passes_event_name: bool) -> ManagedHookEventSpec {
    ManagedHookEventSpec {
        name,
        shape: ManagedHookEventShape::DirectCommand {
            timeout: MANAGED_HOOK_TIMEOUT_SECONDS,
        },
        passes_event_name,
    }
}

const fn copilot(name: &'static str) -> ManagedHookEventSpec {
    ManagedHookEventSpec {
        name,
        shape: ManagedHookEventShape::CopilotCommand { timeout_seconds: 5 },
        passes_event_name: true,
    }
}

const CLAUDE_EVENTS: &[ManagedHookEventSpec] = &[
    nested("UserPromptSubmit", None),
    nested("Stop", None),
    nested("StopFailure", None),
    nested("SubagentStart", None),
    nested("SubagentStop", None),
    nested("TeammateIdle", None),
    nested("PreToolUse", Some("*")),
    nested("PostToolUse", Some("*")),
    nested("PostToolUseFailure", Some("*")),
    nested("PermissionRequest", Some("*")),
];

const CODEX_EVENTS: &[ManagedHookEventSpec] = &[
    nested("SessionStart", None),
    nested("UserPromptSubmit", None),
    nested("PreToolUse", None),
    nested("PermissionRequest", None),
    nested("PostToolUse", None),
    nested("Stop", None),
];

const GEMINI_EVENTS: &[ManagedHookEventSpec] = &[
    nested_ms("BeforeAgent"),
    nested_ms("AfterAgent"),
    nested_ms("AfterTool"),
    nested_ms("BeforeTool"),
];

const ANTIGRAVITY_EVENTS: &[ManagedHookEventSpec] = &[
    direct("PreInvocation", true),
    direct("PostInvocation", true),
    direct("Stop", true),
    ManagedHookEventSpec {
        name: "PostToolUse",
        shape: ManagedHookEventShape::NestedCommand {
            matcher: Some("*"),
            timeout: MANAGED_HOOK_TIMEOUT_SECONDS,
        },
        passes_event_name: true,
    },
];

const AMP_EVENTS: &[ManagedHookEventSpec] = &[
    direct("session.start", true),
    direct("agent.start", true),
    direct("tool.call", true),
    direct("tool.result", true),
    direct("agent.end", true),
];

const CURSOR_EVENTS: &[ManagedHookEventSpec] = &[
    direct("beforeSubmitPrompt", false),
    direct("stop", false),
    direct("preToolUse", false),
    direct("postToolUse", false),
    direct("postToolUseFailure", false),
    direct("beforeShellExecution", false),
    direct("beforeMCPExecution", false),
    direct("afterAgentResponse", false),
];

const DROID_EVENTS: &[ManagedHookEventSpec] = &[
    nested("SessionStart", None),
    nested("UserPromptSubmit", None),
    nested("Stop", None),
    nested("SubagentStop", None),
    nested("PreToolUse", Some("*")),
    nested("PostToolUse", Some("*")),
    nested("PermissionRequest", Some("*")),
    nested("Notification", None),
];

const COMMAND_CODE_EVENTS: &[ManagedHookEventSpec] = &[
    nested("PreToolUse", Some(".*")),
    nested("PostToolUse", Some(".*")),
    nested("Stop", None),
];

const GROK_EVENTS: &[ManagedHookEventSpec] = &[
    nested("SessionStart", None),
    nested("UserPromptSubmit", None),
    nested("Stop", None),
    nested("StopFailure", None),
    nested("SessionEnd", None),
    nested("PreToolUse", Some(".*")),
    nested("PostToolUse", Some(".*")),
    nested("PostToolUseFailure", Some(".*")),
    nested("Notification", None),
];

const COPILOT_EVENTS: &[ManagedHookEventSpec] = &[
    copilot("SessionStart"),
    copilot("SessionEnd"),
    copilot("UserPromptSubmit"),
    copilot("PreToolUse"),
    copilot("PostToolUse"),
    copilot("PostToolUseFailure"),
    copilot("subagentStart"),
    copilot("SubagentStop"),
    copilot("PreCompact"),
    copilot("Stop"),
    copilot("ErrorOccurred"),
    copilot("PermissionRequest"),
    copilot("Notification"),
];

const HERMES_EVENTS: &[ManagedHookEventSpec] = &[
    direct("on_session_start", true),
    direct("pre_llm_call", true),
    direct("post_llm_call", true),
    direct("pre_tool_call", true),
    direct("post_tool_call", true),
    direct("pre_approval_request", true),
    direct("post_approval_response", true),
    direct("on_session_end", true),
    direct("on_session_finalize", true),
    direct("on_session_reset", true),
];

const DEVIN_EVENTS: &[ManagedHookEventSpec] = &[
    nested("SessionStart", None),
    nested("UserPromptSubmit", None),
    nested("Stop", None),
    nested("PostCompaction", None),
    nested("SessionEnd", None),
    nested("PreToolUse", None),
    nested("PostToolUse", None),
    nested("PermissionRequest", None),
];

const KIMI_EVENTS: &[ManagedHookEventSpec] = &[
    direct("UserPromptSubmit", false),
    direct("PreToolUse", false),
    direct("PostToolUse", false),
    direct("PostToolUseFailure", false),
    direct("PermissionRequest", false),
    direct("Stop", false),
    direct("StopFailure", false),
];

const SPECS: &[ManagedHookAdapterSpec] = &[
    json(
        "claude",
        "claude-code",
        ".claude/settings.json",
        "claude-hook",
        CLAUDE_EVENTS,
        false,
    ),
    json(
        "openclaude",
        "claude-code",
        ".openclaude/settings.json",
        "openclaude-hook",
        CLAUDE_EVENTS,
        false,
    ),
    ManagedHookAdapterSpec {
        target: "codex",
        source_adapter: "codex",
        // Gate4Agent does not own a shadow Codex home. The explicit manager
        // edits the provider's normal hooks.json while preserving its login.
        config_location: ManagedHookConfigLocation::HomeRelative(".codex/hooks.json"),
        config_kind: ManagedHookConfigKind::JsonHooks {
            container: "hooks",
            require_version_one: false,
        },
        script_stem: "codex-hook",
        events: CODEX_EVENTS,
    },
    json(
        "gemini",
        "gemini",
        ".gemini/settings.json",
        "gemini-hook",
        GEMINI_EVENTS,
        false,
    ),
    ManagedHookAdapterSpec {
        target: "antigravity",
        source_adapter: "antigravity",
        config_location: ManagedHookConfigLocation::HomeRelative(".gemini/config/hooks.json"),
        config_kind: ManagedHookConfigKind::JsonHooks {
            container: "orca-status",
            require_version_one: false,
        },
        script_stem: "antigravity-hook",
        events: ANTIGRAVITY_EVENTS,
    },
    ManagedHookAdapterSpec {
        target: "amp",
        source_adapter: "amp",
        config_location: ManagedHookConfigLocation::HomeRelative(
            ".config/amp/plugins/gate4agent-agent-status.ts",
        ),
        config_kind: ManagedHookConfigKind::AmpPlugin,
        script_stem: "amp-plugin",
        events: AMP_EVENTS,
    },
    json(
        "cursor",
        "cursor",
        ".cursor/hooks.json",
        "cursor-hook",
        CURSOR_EVENTS,
        true,
    ),
    json(
        "droid",
        "droid",
        ".factory/settings.json",
        "droid-hook",
        DROID_EVENTS,
        false,
    ),
    json(
        "command-code",
        "command-code",
        ".commandcode/settings.json",
        "command-code-hook",
        COMMAND_CODE_EVENTS,
        false,
    ),
    ManagedHookAdapterSpec {
        target: "grok",
        source_adapter: "grok",
        config_location: ManagedHookConfigLocation::EnvironmentHome {
            variable: "GROK_HOME",
            fallback: ".grok",
            suffix: "hooks/gate4agent-status.json",
        },
        config_kind: ManagedHookConfigKind::JsonHooks {
            container: "hooks",
            require_version_one: false,
        },
        script_stem: "grok-hook",
        events: GROK_EVENTS,
    },
    ManagedHookAdapterSpec {
        target: "copilot",
        source_adapter: "copilot",
        config_location: ManagedHookConfigLocation::EnvironmentHome {
            variable: "COPILOT_HOME",
            fallback: ".copilot",
            suffix: "hooks/gate4agent.json",
        },
        config_kind: ManagedHookConfigKind::JsonHooks {
            container: "hooks",
            require_version_one: true,
        },
        script_stem: "copilot-hook",
        events: COPILOT_EVENTS,
    },
    ManagedHookAdapterSpec {
        target: "hermes",
        source_adapter: "hermes",
        config_location: ManagedHookConfigLocation::EnvironmentHome {
            variable: "HERMES_HOME",
            fallback: ".hermes",
            suffix: "config.yaml",
        },
        config_kind: ManagedHookConfigKind::HermesPlugin,
        script_stem: "hermes-plugin",
        events: HERMES_EVENTS,
    },
    ManagedHookAdapterSpec {
        target: "devin",
        source_adapter: "devin",
        config_location: ManagedHookConfigLocation::AppDataOrHome {
            app_data_suffix: "devin/config.json",
            home_fallback: ".config/devin/config.json",
        },
        config_kind: ManagedHookConfigKind::JsonHooks {
            container: "hooks",
            require_version_one: false,
        },
        script_stem: "devin-hook",
        events: DEVIN_EVENTS,
    },
    ManagedHookAdapterSpec {
        target: "kimi",
        source_adapter: "kimi",
        config_location: ManagedHookConfigLocation::EnvironmentHome {
            variable: "KIMI_CODE_HOME",
            fallback: ".kimi-code",
            suffix: "config.toml",
        },
        config_kind: ManagedHookConfigKind::KimiToml,
        script_stem: "kimi-hook",
        events: KIMI_EVENTS,
    },
];

const fn json(
    target: &'static str,
    source_adapter: &'static str,
    path: &'static str,
    script_stem: &'static str,
    events: &'static [ManagedHookEventSpec],
    require_version_one: bool,
) -> ManagedHookAdapterSpec {
    ManagedHookAdapterSpec {
        target,
        source_adapter,
        config_location: ManagedHookConfigLocation::HomeRelative(path),
        config_kind: ManagedHookConfigKind::JsonHooks {
            container: "hooks",
            require_version_one,
        },
        script_stem,
        events,
    }
}

pub fn managed_hook_specs() -> &'static [ManagedHookAdapterSpec] {
    SPECS
}

pub fn managed_hook_spec(
    binding: &AdapterBinding,
) -> Result<&'static ManagedHookAdapterSpec, ManagedHookAdapterError> {
    if binding.revision != MANAGED_HOOK_REVISION {
        return Err(ManagedHookAdapterError::RevisionMismatch {
            requested: binding.revision.clone(),
        });
    }
    let registered = builtin_adapter_registry()
        .binding(AdapterFamily::ManagedHook, binding.id.as_str())
        .is_some_and(|registered| registered == binding);
    if !registered {
        return Err(ManagedHookAdapterError::UnsupportedTarget(
            binding.id.as_str().to_owned(),
        ));
    }
    SPECS
        .iter()
        .find(|spec| spec.target == binding.id.as_str())
        .ok_or_else(|| ManagedHookAdapterError::UnsupportedTarget(binding.id.as_str().to_owned()))
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ManagedHookAdapterError {
    #[error("managed Hook adapter target is unsupported: {0}")]
    UnsupportedTarget(String),
    #[error("managed Hook adapter revision mismatch: requested {requested}")]
    RevisionMismatch { requested: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn inventory_matches_pinned_orca_managed_controls() {
        let actual = managed_hook_specs()
            .iter()
            .map(|spec| spec.target)
            .collect::<BTreeSet<_>>();
        let expected = [
            "amp",
            "antigravity",
            "claude",
            "codex",
            "command-code",
            "copilot",
            "cursor",
            "devin",
            "droid",
            "gemini",
            "grok",
            "hermes",
            "kimi",
            "openclaude",
        ]
        .into_iter()
        .collect::<BTreeSet<_>>();
        assert_eq!(actual, expected);
    }

    #[test]
    fn registry_and_specs_are_revision_exact() {
        for spec in managed_hook_specs() {
            let binding = builtin_adapter_registry()
                .binding(AdapterFamily::ManagedHook, spec.target)
                .unwrap();
            assert_eq!(managed_hook_spec(binding).unwrap(), spec);
        }
    }

    #[test]
    fn antigravity_omits_permission_deciding_pre_tool_hook() {
        let antigravity = SPECS
            .iter()
            .find(|spec| spec.target == "antigravity")
            .unwrap();
        assert!(!antigravity
            .events
            .iter()
            .any(|event| event.name == "PreToolUse"));
        assert!(antigravity
            .events
            .iter()
            .any(|event| event.name == "PostToolUse"));
    }
}
