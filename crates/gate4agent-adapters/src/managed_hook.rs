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

const fn direct(name: &'static str, passes_event_name: bool) -> ManagedHookEventSpec {
    ManagedHookEventSpec {
        name,
        shape: ManagedHookEventShape::DirectCommand {
            timeout: MANAGED_HOOK_TIMEOUT_SECONDS,
        },
        passes_event_name,
    }
}

const CLAUDE_EVENTS: &[ManagedHookEventSpec] = &[
    // The only event here that fires without the agent doing any work, and
    // the reason it matters: every other event on this list needs a real
    // turn, a tool call or a subagent to exist first, so a freshly spawned
    // session that is merely SITTING there produces nothing at all. That is
    // exactly the condition the node most needs a provider-authored answer
    // for -- "did the agent actually come up?" -- and without this event we
    // could observe the channel for Claude only by spending a model turn.
    // Grok's contract carries it and Grok is the one provider whose channel
    // has been proven end to end; Codex carries it too. Claude supports it
    // (the operator's own hand-written hook sits on this event in the same
    // settings file) and we simply never asked for it.
    nested("SessionStart", None),
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

const KIMI_EVENTS: &[ManagedHookEventSpec] = &[
    // Read off Kimi's own shipped bundle rather than assumed: its dist
    // carries SessionStart, SessionEnd and Notification alongside the
    // events we already request, at the same frequency, so the contract we
    // were writing was simply short. SessionStart is the one that matters
    // most -- it is the only event here that fires without the agent doing
    // any work, which is what lets the node learn "the agent came up"
    // without spending a model turn to find out.
    direct("SessionStart", false),
    direct("SessionEnd", false),
    direct("Notification", false),
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
    fn inventory_matches_the_fleet_managed_controls() {
        let actual = managed_hook_specs()
            .iter()
            .map(|spec| spec.target)
            .collect::<BTreeSet<_>>();
        let expected = ["claude", "codex", "grok", "kimi"]
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
}
