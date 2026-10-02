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

/// The gate4agent provider catalog.
///
/// Exactly the fleet this project runs: Claude Code, Codex, Grok, and Kimi
/// Code. This catalog is the single source of truth for provider identity —
/// it does not carry reference-only or aspirational entries for CLIs the
/// project does not actually spawn. `SpecVerification::Reference`
/// intentionally prevents a launch shape derived from Orca from being
/// confused with live vendor verification.
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
        grok_spec(),
        kimi_spec(),
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

fn kimi_spec() -> AgentSpec {
    let mut value = spec(
        "kimi",
        "Kimi Code",
        "kimi",
        &[],
        InitialPromptMode::AfterReady,
    );
    // macOS renders Kimi Code's own PTY foreground process as `kimi-code`,
    // not the `kimi` launch command -- observed live on a clean macOS
    // arm64 stand with no vendor login. Without this second matcher,
    // `is_expected_agent_process` reported the confirmed Kimi session as
    // foreign, which `classify_pty_screen_state` reads as `NotAgent` and
    // that outranks even an already-recognized `OperatorGate`.
    value.expected_processes.push(ProcessMatcher::Exact {
        name: "kimi-code".to_owned(),
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
    let adapter_id = if id == "claude" { "claude-code" } else { id };
    let transport_adapter_id = match id {
        "claude" | "codex" | "kimi" | "grok" => Some(adapter_id),
        _ => None,
    };
    let one_shot_adapter_id = matches!(id, "claude" | "codex" | "kimi").then_some(id);
    let structured_pipe_adapter_id =
        matches!(id, "claude" | "codex" | "kimi").then_some(adapter_id);
    let pipe = structured_pipe_adapter_id
        .and_then(|adapter| binding(AdapterFamily::Pipe, adapter))
        .map(|adapter| PipeTransportSpec {
            adapter,
            protocol: PipeProtocol::StructuredJsonl,
            launch_override: None,
            prompt_delivery: PipePromptDelivery::None,
        })
        .or_else(|| {
            one_shot_adapter_id
                .and_then(|adapter| binding(AdapterFamily::OneShot, adapter))
                .map(|adapter| PipeTransportSpec {
                    adapter,
                    protocol: PipeProtocol::OneShotText,
                    launch_override: None,
                    prompt_delivery: PipePromptDelivery::None,
                })
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
        agent_commands: matches!(id, "claude" | "codex").then_some(AgentCommandMode::SlashLine),
        transports: AgentTransportCapabilities {
            // Grok is ACP-native (`grok agent stdio`); catalog forbids
            // TransportKind::Pty as a control driver. PTY remains an
            // operator attach/replay concern outside this flag — see
            // hatchery research agent-harness-isolation-and-vendor-cli-recon-2026-10-02.
            pty: id != "grok",
            pty_adapter: transport_adapter_id
                .and_then(|adapter| binding(AdapterFamily::PtySemantic, adapter)),
            pipe,
            // Claude/Codex ACP use npm adapter packages; Grok/Kimi are native.
            // Catalog admits ACP for all four fleet providers.
            // Codex first-party `codex app-server` is NOT an AcpTransportSpec /
            // launch_override here — parallel channel, inventory only
            // (hatchery research codex-app-server-vs-acp-adapter-2026-10-02).
            acp: transport_adapter_id
                .and_then(|adapter| binding(AdapterFamily::Acp, adapter))
                .map(|adapter| AcpTransportSpec {
                    adapter,
                    launch_override: None,
                }),
        },
        adapters: AgentAdapterCapabilities {
            // No provider in the current fleet declares a PTY sidecar.
            pty_sidecar: None,
            // Lifecycle hooks are retired (owner ruling 2026-09-25): no
            // provider declares a Hook or ManagedHook adapter anymore. The
            // fields stay on `AgentAdapterCapabilities` because they are
            // wire-visible (see `gate4agent-types::spec::AgentAdapterCapabilities`);
            // only the catalog stops producing a value for them.
            hook: None,
            managed_hook: None,
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
        followup_requires_terminal: matches!(id, "claude" | "codex" | "kimi"),
        draft_signal: match id {
            "codex" => DraftReadySignal::CodexComposerPrompt,
            "kimi" => DraftReadySignal::BracketedPaste,
            // Claude 2.1.224 on Windows enables Win32 input plus focus
            // reporting instead of bracketed paste. The scanner accepts that
            // full bootstrap or the older bracketed-paste contract.
            "claude" => DraftReadySignal::CursorAfterBracketedPaste,
            _ => DraftReadySignal::QuietAfterBracketedPaste,
        },
        ..AgentReadinessSpec::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_three_backend_pty_launch_contracts_are_explicit() {
        let registry = builtin_registry();
        for id in ["claude", "codex", "kimi"] {
            assert!(registry.get_by_id(id).is_some(), "missing target backend {id}");
        }
        let codex = registry.get_by_id("codex").unwrap();
        assert_eq!(
            codex.launch.fixed_args,
            ["-c", "windows_wsl_setup_acknowledged=true"]
        );
        assert_eq!(
            registry.get_by_id("claude").unwrap().readiness.draft_signal,
            DraftReadySignal::CursorAfterBracketedPaste
        );
        assert_eq!(
            codex.readiness.draft_signal,
            DraftReadySignal::CodexComposerPrompt
        );
        assert_eq!(
            registry.get_by_id("kimi").unwrap().readiness.draft_signal,
            DraftReadySignal::BracketedPaste
        );
    }

    /// macOS renders Kimi Code's own PTY foreground process as `kimi-code`,
    /// not the `kimi` launch command -- the spec must match both, the same
    /// way `grok`/`codex` already carry a second matcher for their own
    /// versioned/suffixed process names.
    #[test]
    fn kimi_spec_matches_both_the_launch_command_and_its_macos_process_name() {
        let kimi = builtin_registry()
            .get_by_id("kimi")
            .expect("kimi spec must exist");
        assert_eq!(
            kimi.expected_processes,
            vec![
                ProcessMatcher::Exact {
                    name: "kimi".to_owned()
                },
                ProcessMatcher::Exact {
                    name: "kimi-code".to_owned()
                },
            ],
        );
    }

    #[test]
    fn fleet_catalog_is_stable_and_unique() {
        let registry = builtin_registry();
        let ids: Vec<_> = registry.iter().map(|spec| spec.id.as_str()).collect();
        assert_eq!(ids.len(), 4);
        for required in ["claude", "codex", "grok", "kimi"] {
            assert!(ids.contains(&required), "missing built-in agent {required}");
        }
        assert!(!ids.contains(&"claude-agent-teams"));

        for id in ["claude", "codex", "kimi"] {
            let transports = &registry.get_by_id(id).unwrap().capabilities.transports;
            assert!(transports.pty_adapter.is_some(), "{id}");
            assert!(transports.pipe.is_some(), "{id}");
        }
        for id in ["claude", "codex", "kimi"] {
            let spec = registry.get_by_id(id).unwrap();
            let pipe = spec.capabilities.transports.pipe.as_ref().unwrap();
            assert_eq!(pipe.protocol, PipeProtocol::StructuredJsonl, "{id}");
            assert_eq!(
                pipe.adapter.id.as_str(),
                if id == "claude" { "claude-code" } else { id }
            );
        }
        for id in ["claude", "codex", "kimi"] {
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
        // No provider in the current fleet declares a PTY sidecar.
        assert!(registry
            .iter()
            .all(|spec| spec.capabilities.adapters.pty_sidecar.is_none()));

        // ACP is wired for all four fleet providers.
        for id in ["claude", "codex", "grok", "kimi"] {
            assert!(
                registry
                    .get_by_id(id)
                    .unwrap()
                    .capabilities
                    .transports
                    .acp
                    .is_some(),
                "missing ACP transport for {id}"
            );
        }
        // Grok is ACP-native: no raw PTY driver flag, no PTY semantic /
        // pipe adapters. PTY attach/replay is outside this catalog surface.
        let grok_t = &registry.get_by_id("grok").unwrap().capabilities.transports;
        assert!(!grok_t.pty, "grok must not advertise TransportKind::Pty");
        assert!(grok_t.pty_adapter.is_none());
        assert!(grok_t.pipe.is_none());
        assert!(grok_t.acp.is_some());

        for id in ["claude", "codex", "grok", "kimi"] {
            let adapters = &registry.get_by_id(id).unwrap().capabilities.adapters;
            // Lifecycle hooks are retired: no provider declares a Hook or
            // ManagedHook adapter anymore.
            assert!(adapters.hook.is_none(), "unexpected hook adapter for {id}");
            assert!(
                adapters.managed_hook.is_none(),
                "unexpected managed Hook adapter for {id}"
            );
            assert!(
                adapters.history.is_some(),
                "missing history adapter for {id}"
            );
            assert!(
                adapters.resume.is_some(),
                "missing resume adapter for {id}"
            );
        }

        for id in ["claude", "codex"] {
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
        for id in ["grok", "kimi"] {
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
    }
}
