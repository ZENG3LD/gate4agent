use crate::{
    builtin_adapter_registry, AcpTransportSpec, AdapterBinding, AdapterFamily,
    AgentAdapterCapabilities, AgentCapabilities, AgentCommandMode, AgentId, AgentReadinessSpec,
    AgentRegistry, AgentSpec, AgentTransportCapabilities, DetectionSpec, DraftReadySignal,
    InitialPromptMode, LaunchSpec, NativeDraftMode, PipePromptDelivery, PipeProtocol,
    PipeTransportSpec, ProcessMatcher, PromptSpec, SpecVerification,
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
            "Kimi Code",
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
    value.launch.fixed_args.extend([
        "-c".to_owned(),
        "windows_wsl_setup_acknowledged=true".to_owned(),
    ]);
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
    let adapter_id = if matches!(id, "claude" | "openclaude") {
        "claude-code"
    } else {
        id
    };
    let transport_adapter_id = match id {
        "claude" | "codex" | "gemini" | "opencode" => Some(adapter_id),
        _ => None,
    };
    let one_shot_adapter_id = matches!(
        id,
        "claude"
            | "codex"
            | "opencode"
            | "pi"
            | "amp"
            | "cursor"
            | "kimi"
            | "copilot"
            | "antigravity"
    )
    .then_some(id);
    let pipe = one_shot_adapter_id
        .and_then(|adapter| binding(AdapterFamily::OneShot, adapter))
        .map(|adapter| PipeTransportSpec {
            adapter,
            protocol: PipeProtocol::OneShotText,
            launch_override: None,
            prompt_delivery: PipePromptDelivery::None,
        })
        .or_else(|| {
            transport_adapter_id
                .and_then(|adapter| binding(AdapterFamily::Pipe, adapter))
                .map(|adapter| PipeTransportSpec {
                    adapter,
                    protocol: PipeProtocol::SemanticNdjson,
                    launch_override: None,
                    prompt_delivery: PipePromptDelivery::None,
                })
        });
    AgentCapabilities {
        agent_commands: matches!(id, "claude" | "codex" | "gemini" | "cursor")
            .then_some(AgentCommandMode::SlashLine),
        transports: AgentTransportCapabilities {
            pty: true,
            pty_adapter: transport_adapter_id
                .and_then(|adapter| binding(AdapterFamily::PtySemantic, adapter)),
            pipe,
            // The legacy Claude/Codex ACP adapters use npm packages that may
            // be downloaded at launch. They are deliberately not enabled by
            // the catalog-backed control plane.
            acp: transport_adapter_id
                .and_then(|adapter| binding(AdapterFamily::Acp, adapter))
                .map(|adapter| AcpTransportSpec {
                    adapter,
                    launch_override: None,
                }),
        },
        adapters: AgentAdapterCapabilities {
            hook: binding(AdapterFamily::Hook, adapter_id),
            managed_hook: binding(AdapterFamily::ManagedHook, id),
            one_shot: one_shot_adapter_id
                .and_then(|adapter| binding(AdapterFamily::OneShot, adapter)),
            history: binding(AdapterFamily::History, adapter_id),
            resume: binding(AdapterFamily::Resume, adapter_id),
            session_options: binding(AdapterFamily::SessionOptions, adapter_id),
            capability_probe: binding(AdapterFamily::CapabilityProbe, adapter_id),
        },
    }
}

fn binding(family: AdapterFamily, id: &str) -> Option<AdapterBinding> {
    builtin_adapter_registry().binding(family, id).cloned()
}

fn readiness(id: &str) -> AgentReadinessSpec {
    AgentReadinessSpec {
        followup_requires_terminal: matches!(id, "claude" | "codex" | "kimi" | "qwen-code"),
        draft_signal: match id {
            "codex" => DraftReadySignal::CodexComposerPrompt,
            "kimi" => DraftReadySignal::BracketedPaste,
            "claude" => DraftReadySignal::ClaudeComposerPrompt,
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
    fn current_four_backend_pty_launch_contracts_are_explicit() {
        let registry = builtin_registry();
        for id in ["claude", "codex", "kimi", "qwen-code"] {
            assert!(registry.get_by_id(id).is_some(), "missing target backend {id}");
        }
        let codex = registry.get_by_id("codex").unwrap();
        assert_eq!(
            codex.launch.fixed_args,
            ["-c", "windows_wsl_setup_acknowledged=true"]
        );
        assert_eq!(
            registry.get_by_id("claude").unwrap().readiness.draft_signal,
            DraftReadySignal::ClaudeComposerPrompt
        );
        assert_eq!(
            codex.readiness.draft_signal,
            DraftReadySignal::CodexComposerPrompt
        );
        assert_eq!(
            registry.get_by_id("kimi").unwrap().readiness.draft_signal,
            DraftReadySignal::BracketedPaste
        );
        let qwen = registry.get_by_id("qwen-code").unwrap();
        assert_eq!(qwen.prompt.initial, InitialPromptMode::AfterReady);
        assert_eq!(
            qwen.readiness.draft_signal,
            DraftReadySignal::QuietAfterBracketedPaste
        );
    }

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
        assert_eq!(
            registry
                .get_by_id("gemini")
                .unwrap()
                .capabilities
                .transports
                .pipe
                .as_ref()
                .unwrap()
                .protocol,
            PipeProtocol::SemanticNdjson
        );
        for id in [
            "claude",
            "codex",
            "opencode",
            "pi",
            "amp",
            "cursor",
            "kimi",
            "copilot",
            "antigravity",
        ] {
            let spec = registry.get_by_id(id).unwrap();
            let pipe = spec.capabilities.transports.pipe.as_ref().unwrap();
            assert_eq!(pipe.protocol, PipeProtocol::OneShotText, "{id}");
            assert_eq!(pipe.adapter.id.as_str(), id);
        }
        for id in [
            "claude",
            "codex",
            "opencode",
            "pi",
            "amp",
            "cursor",
            "kimi",
            "copilot",
            "antigravity",
        ] {
            let binding = registry
                .get_by_id(id)
                .unwrap()
                .capabilities
                .adapters
                .one_shot
                .as_ref()
                .unwrap_or_else(|| panic!("missing OneShot adapter for {id}"));
            assert_eq!(binding.id.as_str(), id);
            let expected_revision = match id {
                "claude" => gate4agent_adapters::CLAUDE_CODE_INLINE_REVISION,
                "codex" => gate4agent_adapters::CODEX_CLI_INLINE_REVISION,
                "kimi" => gate4agent_adapters::KIMI_CODE_INLINE_REVISION,
                _ => gate4agent_adapters::ONE_SHOT_REVISION,
            };
            assert_eq!(binding.revision, expected_revision);
        }
        let qwen = registry.get_by_id("qwen-code").unwrap();
        assert!(qwen.capabilities.transports.pipe.is_none());
        assert!(qwen.capabilities.adapters.one_shot.is_none());
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

        for id in ["grok", "kimi", "copilot", "droid", "cursor"] {
            let adapters = &registry.get_by_id(id).unwrap().capabilities.adapters;
            assert!(adapters.hook.is_some(), "missing hook adapter for {id}");
            assert!(
                adapters.history.is_some(),
                "missing history adapter for {id}"
            );
        }
        for id in [
            "claude",
            "openclaude",
            "codex",
            "gemini",
            "antigravity",
            "amp",
            "cursor",
            "droid",
            "command-code",
            "grok",
            "copilot",
            "hermes",
            "devin",
            "kimi",
        ] {
            let binding = registry
                .get_by_id(id)
                .unwrap()
                .capabilities
                .adapters
                .managed_hook
                .as_ref()
                .unwrap_or_else(|| panic!("missing managed Hook adapter for {id}"));
            assert_eq!(binding.id.as_str(), id);
            assert_eq!(binding.revision, gate4agent_adapters::MANAGED_HOOK_REVISION);
        }
        let openclaude = &registry
            .get_by_id("openclaude")
            .unwrap()
            .capabilities
            .adapters;
        assert_eq!(openclaude.hook.as_ref().unwrap().id.as_str(), "claude-code");
        for id in [
            "claude",
            "codex",
            "gemini",
            "antigravity",
            "opencode",
            "pi",
            "mimo-code",
            "droid",
            "grok",
            "devin",
        ] {
            assert!(
                registry
                    .get_by_id(id)
                    .unwrap()
                    .capabilities
                    .adapters
                    .resume
                    .is_some(),
                "missing resume adapter for {id}"
            );
        }
        for id in [
            "kimi",
            "copilot",
            "cursor",
            "qwen-code",
            "omp",
            "amp",
            "command-code",
            "hermes",
        ] {
            assert!(
                registry
                    .get_by_id(id)
                    .unwrap()
                    .capabilities
                    .adapters
                    .resume
                    .is_none(),
                "unexpected live resume adapter for {id}"
            );
        }

        for id in ["claude", "codex", "gemini", "cursor"] {
            let binding = registry
                .get_by_id(id)
                .unwrap()
                .capabilities
                .adapters
                .session_options
                .as_ref()
                .unwrap_or_else(|| panic!("missing session-option adapter for {id}"));
            assert_eq!(
                binding.revision,
                gate4agent_adapters::SESSION_OPTION_CATALOG_REVISION
            );
        }
        for id in ["opencode", "grok", "kimi", "qwen-code"] {
            assert!(
                registry
                    .get_by_id(id)
                    .unwrap()
                    .capabilities
                    .adapters
                    .session_options
                    .is_none(),
                "unexpected session-option adapter for {id}"
            );
        }

        let qwen = registry.get_by_id("qwen-code").unwrap();
        assert_eq!(qwen.prompt.initial, InitialPromptMode::AfterReady);
        assert!(qwen.capabilities.adapters.hook.is_none());
        assert!(qwen.capabilities.adapters.history.is_none());
        assert!(qwen.capabilities.transports.pty_adapter.is_none());
    }
}
