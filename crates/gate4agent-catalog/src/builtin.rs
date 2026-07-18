use crate::{
    AcpTransportSpec, AgentCapabilities, AgentCommandMode, AgentId, AgentReadinessSpec,
    AgentRegistry, AgentSpec, AgentTransportCapabilities, DetectionSpec, DraftReadySignal,
    InitialPromptMode, LaunchSpec, NativeDraftMode, PipePromptDelivery, PipeTransportSpec,
    ProcessMatcher, PromptSpec, ProviderAdapter, SpecVerification,
};
use std::sync::OnceLock;

pub const ORCA_REFERENCE_REVISION: &str = "d8629c41c832436463d5f0b4e4deb95f867fdc42";

pub fn builtin_registry() -> &'static AgentRegistry {
    static REGISTRY: OnceLock<AgentRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        AgentRegistry::new(builtin_specs()).unwrap_or_else(|error| {
            panic!(
                "invalid built-in agent registry extracted from Orca {ORCA_REFERENCE_REVISION}: {error}"
            )
        })
    })
}

/// Portable external-agent catalog for the registry migration.
///
/// The first four entries preserve gate4agent identity while the additional
/// entries establish first-class launch grounding for Orca's pinned external
/// CLI catalog. Orca-owned `claude-agent-teams` is intentionally excluded.
/// `SpecVerification::Reference` intentionally prevents a launch shape derived
/// from Orca from being confused with live vendor verification.
pub fn builtin_specs() -> Vec<AgentSpec> {
    vec![
        with_native_draft_flag(
            spec(
                "claude",
                "Claude Code",
                "claude",
                &[],
                InitialPromptMode::Positional {
                    option_terminator: false,
                },
            ),
            "--prefill",
        ),
        codex_spec(),
        spec(
            "gemini",
            "Gemini CLI",
            "gemini",
            &[],
            InitialPromptMode::InteractiveFlag {
                flag: "--prompt-interactive".to_owned(),
            },
        ),
        spec(
            "opencode",
            "OpenCode",
            "opencode",
            &[],
            InitialPromptMode::Flag {
                flag: "--prompt".to_owned(),
            },
        ),
        grok_spec(),
        spec(
            "kimi",
            "Kimi CLI",
            "kimi",
            &[],
            InitialPromptMode::AfterReady,
        ),
        spec(
            "qwen-code",
            "Qwen Code",
            "qwen",
            &[],
            InitialPromptMode::AfterReady,
        ),
        spec(
            "copilot",
            "GitHub Copilot CLI",
            "copilot",
            &[],
            InitialPromptMode::InteractiveFlag {
                flag: "-i".to_owned(),
            },
        ),
        with_native_draft_flag(
            spec(
                "openclaude",
                "OpenClaude",
                "openclaude",
                &[],
                InitialPromptMode::Positional {
                    option_terminator: false,
                },
            ),
            "--prefill",
        ),
        spec(
            "autohand",
            "Autohand Code",
            "autohand",
            &[],
            InitialPromptMode::AfterReady,
        ),
        spec(
            "mimo-code",
            "MiMo Code",
            "mimo",
            &[],
            InitialPromptMode::Flag {
                flag: "--prompt".to_owned(),
            },
        ),
        spec(
            "pi",
            "Pi",
            "pi",
            &[],
            InitialPromptMode::Positional {
                option_terminator: false,
            },
        ),
        spec(
            "omp",
            "oh-my-pi",
            "omp",
            &[],
            InitialPromptMode::Positional {
                option_terminator: false,
            },
        ),
        spec(
            "antigravity",
            "Google Antigravity CLI",
            "agy",
            &[],
            InitialPromptMode::InteractiveFlag {
                flag: "--prompt-interactive".to_owned(),
            },
        ),
        after_ready("aider", "Aider", "aider", &[]),
        after_ready("goose", "Goose", "goose", &[]),
        after_ready("amp", "Amp", "amp", &[]),
        after_ready("kilo", "Kilocode", "kilo", &[]),
        with_fixed_args(
            after_ready("kiro", "Kiro", "kiro-cli", &[]),
            &["chat", "--tui"],
        ),
        after_ready("crush", "Charm Crush", "crush", &[]),
        after_ready("aug", "Augment Code", "auggie", &[]),
        after_ready("cline", "Cline", "cline", &[]),
        after_ready("codebuff", "Codebuff", "codebuff", &[]),
        spec(
            "command-code",
            "Command Code",
            "command-code",
            &[],
            InitialPromptMode::Positional {
                option_terminator: false,
            },
        ),
        after_ready("continue", "Continue CLI", "cn", &[]),
        spec(
            "cursor",
            "Cursor Agent",
            "cursor-agent",
            &[],
            InitialPromptMode::Positional {
                option_terminator: false,
            },
        ),
        spec(
            "droid",
            "Factory Droid",
            "droid",
            &[],
            InitialPromptMode::Positional {
                option_terminator: false,
            },
        ),
        after_ready("mistral-vibe", "Mistral Vibe", "vibe", &["mistral-vibe"]),
        after_ready("rovo", "Rovo Dev", "rovo", &[]),
        with_fixed_args(
            spec(
                "hermes",
                "Hermes Agent",
                "hermes",
                &[],
                InitialPromptMode::AgentNativeQuery,
            ),
            &["--tui"],
        ),
        after_ready("openclaw", "OpenClaw", "openclaw", &[]),
        after_ready("devin", "Devin CLI", "devin", &[]),
        after_ready("ante", "Ante", "ante", &[]),
    ]
}

fn grok_spec() -> AgentSpec {
    let mut value = spec(
        "grok",
        "xAI Grok CLI",
        "grok",
        &[],
        InitialPromptMode::Positional {
            option_terminator: true,
        },
    );
    value.expected_processes.push(ProcessMatcher::Prefix {
        prefix: "grok-".to_owned(),
    });
    value
}

fn codex_spec() -> AgentSpec {
    let mut value = spec(
        "codex",
        "OpenAI Codex",
        "codex",
        &[],
        InitialPromptMode::Positional {
            option_terminator: false,
        },
    );
    value.expected_processes.push(ProcessMatcher::Prefix {
        prefix: "codex-".to_owned(),
    });
    value
}

fn after_ready(id: &str, display_name: &str, command: &str, aliases: &[&str]) -> AgentSpec {
    spec(
        id,
        display_name,
        command,
        aliases,
        InitialPromptMode::AfterReady,
    )
}

fn with_fixed_args(mut spec: AgentSpec, fixed_args: &[&str]) -> AgentSpec {
    spec.launch.fixed_args = fixed_args
        .iter()
        .map(|argument| (*argument).to_owned())
        .collect();
    spec
}

fn with_native_draft_flag(mut spec: AgentSpec, flag: &str) -> AgentSpec {
    spec.prompt.native_draft = Some(NativeDraftMode::Flag {
        flag: flag.to_owned(),
    });
    spec
}

fn spec(
    id: &str,
    display_name: &str,
    command: &str,
    aliases: &[&str],
    initial: InitialPromptMode,
) -> AgentSpec {
    AgentSpec {
        id: AgentId::new(id).expect("hardcoded agent ID must be valid"),
        revision: format!("orca:{ORCA_REFERENCE_REVISION}"),
        display_name: display_name.to_owned(),
        detection: DetectionSpec {
            command: command.to_owned(),
            aliases: aliases.iter().map(|alias| (*alias).to_owned()).collect(),
            required_commands: Vec::new(),
            unsupported_platforms: Vec::new(),
        },
        launch: LaunchSpec {
            program: command.to_owned(),
            fixed_args: Vec::new(),
        },
        expected_processes: vec![ProcessMatcher::Exact {
            name: command.to_owned(),
        }],
        prompt: PromptSpec {
            initial,
            native_draft: None,
        },
        readiness: readiness(id),
        capabilities: capabilities(id),
        verification: SpecVerification::Reference,
    }
}

fn capabilities(id: &str) -> AgentCapabilities {
    let adapter = match id {
        "claude" => Some(ProviderAdapter::ClaudeCode),
        "codex" => Some(ProviderAdapter::Codex),
        "gemini" => Some(ProviderAdapter::Gemini),
        "opencode" => Some(ProviderAdapter::OpenCode),
        _ => None,
    };
    AgentCapabilities {
        agent_commands: matches!(id, "claude" | "codex" | "gemini")
            .then_some(AgentCommandMode::SlashLine),
        transports: AgentTransportCapabilities {
            pty: true,
            pty_adapter: adapter,
            pipe: adapter.map(|adapter| PipeTransportSpec {
                adapter,
                launch_override: None,
                prompt_delivery: PipePromptDelivery::None,
            }),
            // The legacy Claude/Codex ACP adapters use npm packages that may
            // be downloaded at launch. They are deliberately not enabled by
            // the catalog-backed control plane.
            acp: match adapter {
                Some(adapter @ (ProviderAdapter::Gemini | ProviderAdapter::OpenCode)) => {
                    Some(AcpTransportSpec {
                        adapter,
                        launch_override: None,
                    })
                }
                _ => None,
            },
        },
    }
}

fn readiness(id: &str) -> AgentReadinessSpec {
    AgentReadinessSpec {
        draft_signal: match id {
            "codex" => DraftReadySignal::CodexComposerPrompt,
            "opencode" | "mimo-code" => DraftReadySignal::CursorAfterBracketedPaste,
            _ => DraftReadySignal::QuietAfterBracketedPaste,
        },
        ..AgentReadinessSpec::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portable_reference_catalog_is_stable_and_unique() {
        let registry = builtin_registry();
        let ids: Vec<_> = registry.iter().map(|spec| spec.id.as_str()).collect();
        assert_eq!(ids.len(), 33);
        for required in [
            "claude",
            "codex",
            "gemini",
            "opencode",
            "grok",
            "kimi",
            "qwen-code",
            "copilot",
            "kiro",
            "mistral-vibe",
            "hermes",
        ] {
            assert!(ids.contains(&required), "missing built-in agent {required}");
        }
        assert!(!ids.contains(&"claude-agent-teams"));

        for id in ["claude", "codex", "gemini", "opencode"] {
            let transports = &registry.get_by_id(id).unwrap().capabilities.transports;
            assert!(transports.pty_adapter.is_some());
            assert!(transports.pipe.is_some());
        }
        for id in ["gemini", "opencode"] {
            assert!(registry
                .get_by_id(id)
                .unwrap()
                .capabilities
                .transports
                .acp
                .is_some());
        }
        for id in ["claude", "codex", "grok"] {
            assert!(registry
                .get_by_id(id)
                .unwrap()
                .capabilities
                .transports
                .acp
                .is_none());
        }
        assert!(registry
            .get_by_id("grok")
            .unwrap()
            .capabilities
            .transports
            .pty_adapter
            .is_none());
    }
}
