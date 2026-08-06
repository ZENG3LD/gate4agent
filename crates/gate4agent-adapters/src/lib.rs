//! Pure provider adapter contracts and implementations.
//!
//! Role: adapter shell logic without OS authority.
//! Owns: revisioned adapter definitions and pure provider parsing/building.
//! Exports: adapter registry and family-specific pure adapters.
//! Forbidden: async, locks, channels, process, filesystem, database, network,
//! credentials, and product presentation policy.

mod capability;
mod history;
mod hook;
mod hook_session;
mod managed_hook;
mod one_shot;
mod resume;
mod session_options;

use gate4agent_types::{AdapterBinding, AdapterFamily, AdapterId, AdapterVerification, AgentId};
use std::collections::BTreeMap;
use std::sync::OnceLock;
use thiserror::Error;

pub use capability::{
    capability_probe_plan, parse_capability_models, CapabilityProbeAdapterError,
    CapabilityProbePlan, CAPABILITY_PROBE_OUTPUT_MAX_BYTES, CAPABILITY_PROBE_REVISION,
};

pub use history::{
    history_source_variants, parse_history, HistoryAdapterError, HistoryDocument, HistoryMessage,
    HistoryRole, HistorySession, HistorySourceLayout, HistorySourceVariant,
    HISTORY_DOCUMENT_MAX_BYTES, HISTORY_MESSAGE_MAX_CHARS, HISTORY_METADATA_MAX_BYTES,
    HISTORY_STORED_MESSAGES_MAX,
};
pub use hook::{
    normalize_hook_event, HookAdapterError, HOOK_EVENT_NAME_MAX_BYTES, HOOK_PAYLOAD_MAX_BYTES,
    HOOK_TEXT_MAX_CHARS, OPENCODE_HOOK_TEXT_MAX_CHARS,
};
pub use hook_session::{
    HookEventDisposition, HookEventEnvelope, HookReduction, HookSessionReducer,
    HookSessionReducerError, HookSubagentSeed, HOOK_EVENT_ID_MAX_BYTES, HOOK_SEEN_EVENT_IDS_MAX,
};
pub use managed_hook::{
    managed_hook_spec, managed_hook_specs, ManagedHookAdapterError, ManagedHookAdapterSpec,
    ManagedHookConfigKind, ManagedHookConfigLocation, ManagedHookEventShape, ManagedHookEventSpec,
    MANAGED_HOOK_TIMEOUT_MILLISECONDS, MANAGED_HOOK_TIMEOUT_SECONDS,
};
pub use one_shot::{
    one_shot_spec, one_shot_specs, resolve_one_shot_plan, OneShotAdapterError, OneShotAdapterSpec,
    OneShotModelSource, OneShotModelSpec, OneShotPlan, OneShotPromptDelivery, OneShotThinkingLevel,
    CLAUDE_CODE_INLINE_REVISION, CODEX_CLI_INLINE_REVISION, KIMI_CODE_INLINE_REVISION,
    ONE_SHOT_OUTPUT_MAX_BYTES, ONE_SHOT_REVISION, ONE_SHOT_THINKING_OPTION_ID,
    ONE_SHOT_TIMEOUT_SECONDS, QWEN_CODE_INLINE_REVISION,
};
pub use resume::{
    build_resume_plan, build_resume_plan_for_identity, ResumeAdapterError, ResumePlan,
    RESUME_SESSION_ID_MAX_BYTES,
};
pub use session_options::{
    merge_session_option_models, parse_session_option_models, plan_mid_session_action,
    plan_mid_session_option, resolve_session_option_launch, session_option_catalog,
    AgentSessionOptionCatalog, ResolvedSessionOptionLaunch, SessionOption,
    SessionOptionAdapterError, SessionOptionApply, SessionOptionArgumentOverride,
    SessionOptionCategory, SessionOptionChoice, SessionOptionInteractionDetection,
    SessionOptionKind, SessionOptionLaunchApplication, SessionOptionMidSessionApplication,
    SessionOptionMidSessionPlan, SessionOptionModel, SessionOptionModelListSpec,
    SESSION_OPTION_CATALOG_REVISION, SESSION_OPTION_MODELS_MAX,
    SESSION_OPTION_MODEL_LIST_MAX_BYTES,
};

pub const BUILTIN_ADAPTER_REVISION: &str = "gate4agent-adapter/v1";
pub const MANAGED_HOOK_REVISION: &str = "gate4agent-managed-hooks/orca-d8629c4/v1";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdapterDescriptor {
    pub family: AdapterFamily,
    pub binding: AdapterBinding,
    pub agents: Vec<AgentId>,
}

/// Validated registry keyed by adapter family and implementation identity.
///
/// The family is part of the key: one stable adapter ID may intentionally
/// implement several independent families without turning them into a blanket
/// provider-support claim.
#[derive(Clone, Debug, Default)]
pub struct AdapterRegistry {
    descriptors: BTreeMap<(AdapterFamily, AdapterId), AdapterDescriptor>,
}

impl AdapterRegistry {
    pub fn new(
        descriptors: impl IntoIterator<Item = AdapterDescriptor>,
    ) -> Result<Self, AdapterRegistryError> {
        let mut registry = Self::default();
        for descriptor in descriptors {
            registry.insert(descriptor)?;
        }
        Ok(registry)
    }

    pub fn insert(&mut self, descriptor: AdapterDescriptor) -> Result<(), AdapterRegistryError> {
        descriptor
            .binding
            .validate()
            .map_err(|error| AdapterRegistryError::InvalidBinding {
                family: descriptor.family,
                adapter_id: descriptor.binding.id.clone(),
                message: error.to_string(),
            })?;
        if descriptor.agents.is_empty() {
            return Err(AdapterRegistryError::MissingAgents {
                family: descriptor.family,
                adapter_id: descriptor.binding.id,
            });
        }
        let key = (descriptor.family, descriptor.binding.id.clone());
        if self.descriptors.contains_key(&key) {
            return Err(AdapterRegistryError::Duplicate {
                family: key.0,
                adapter_id: key.1,
            });
        }
        self.descriptors.insert(key, descriptor);
        Ok(())
    }

    pub fn get(&self, family: AdapterFamily, id: &AdapterId) -> Option<&AdapterDescriptor> {
        self.descriptors.get(&(family, id.clone()))
    }

    pub fn binding(&self, family: AdapterFamily, id: &str) -> Option<&AdapterBinding> {
        self.descriptors
            .get(&(family, AdapterId::new(id).ok()?))
            .map(|descriptor| &descriptor.binding)
    }

    pub fn supports(&self, family: AdapterFamily, binding: &AdapterBinding) -> bool {
        self.get(family, &binding.id)
            .is_some_and(|descriptor| descriptor.binding.revision == binding.revision)
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &AdapterDescriptor> {
        self.descriptors.values()
    }
}

/// Runtime implementations keyed by the same revisioned family binding used
/// by the declarative adapter registry.
///
/// `T` is intentionally generic: native shells may store process/parser
/// implementations while WASM consumers may store pure client handlers. The
/// registry itself performs no I/O and does not prescribe an execution model.
#[derive(Clone, Debug, Default)]
pub struct AdapterRuntimeRegistry<T> {
    runtimes: BTreeMap<(AdapterFamily, AdapterId), AdapterRuntime<T>>,
}

#[derive(Clone, Debug)]
struct AdapterRuntime<T> {
    binding: AdapterBinding,
    implementation: T,
}

impl<T> AdapterRuntimeRegistry<T> {
    pub fn insert(
        &mut self,
        family: AdapterFamily,
        binding: AdapterBinding,
        implementation: T,
    ) -> Result<(), AdapterRuntimeRegistryError> {
        binding
            .validate()
            .map_err(|error| AdapterRuntimeRegistryError::InvalidBinding {
                family,
                adapter_id: binding.id.clone(),
                message: error.to_string(),
            })?;
        let key = (family, binding.id.clone());
        if self.runtimes.contains_key(&key) {
            return Err(AdapterRuntimeRegistryError::Duplicate {
                family,
                adapter_id: binding.id,
            });
        }
        self.runtimes.insert(
            key,
            AdapterRuntime {
                binding,
                implementation,
            },
        );
        Ok(())
    }

    pub fn resolve(
        &self,
        family: AdapterFamily,
        binding: &AdapterBinding,
    ) -> Result<&T, AdapterRuntimeRegistryError> {
        let Some(runtime) = self.runtimes.get(&(family, binding.id.clone())) else {
            return Err(AdapterRuntimeRegistryError::Unavailable {
                family,
                adapter_id: binding.id.clone(),
                revision: binding.revision.clone(),
            });
        };
        if runtime.binding.revision != binding.revision {
            return Err(AdapterRuntimeRegistryError::RevisionMismatch {
                family,
                adapter_id: binding.id.clone(),
                requested: binding.revision.clone(),
                available: runtime.binding.revision.clone(),
            });
        }
        Ok(&runtime.implementation)
    }

    pub fn len(&self) -> usize {
        self.runtimes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.runtimes.is_empty()
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum AdapterRegistryError {
    #[error("duplicate {family:?} adapter ID: {adapter_id}")]
    Duplicate {
        family: AdapterFamily,
        adapter_id: AdapterId,
    },
    #[error("{family:?} adapter {adapter_id} has no bound agents")]
    MissingAgents {
        family: AdapterFamily,
        adapter_id: AdapterId,
    },
    #[error("invalid {family:?} adapter {adapter_id}: {message}")]
    InvalidBinding {
        family: AdapterFamily,
        adapter_id: AdapterId,
        message: String,
    },
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum AdapterRuntimeRegistryError {
    #[error("duplicate runtime for {family:?} adapter {adapter_id}")]
    Duplicate {
        family: AdapterFamily,
        adapter_id: AdapterId,
    },
    #[error("invalid runtime binding for {family:?} adapter {adapter_id}: {message}")]
    InvalidBinding {
        family: AdapterFamily,
        adapter_id: AdapterId,
        message: String,
    },
    #[error("runtime unavailable for {family:?} adapter {adapter_id} at revision {revision}")]
    Unavailable {
        family: AdapterFamily,
        adapter_id: AdapterId,
        revision: String,
    },
    #[error(
        "runtime revision mismatch for {family:?} adapter {adapter_id}: requested {requested}, available {available}"
    )]
    RevisionMismatch {
        family: AdapterFamily,
        adapter_id: AdapterId,
        requested: String,
        available: String,
    },
}

pub fn builtin_adapter_registry() -> &'static AdapterRegistry {
    static REGISTRY: OnceLock<AdapterRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        AdapterRegistry::new(builtin_descriptors())
            .expect("built-in provider adapter registry must be valid")
    })
}

fn builtin_descriptors() -> Vec<AdapterDescriptor> {
    let mut descriptors = Vec::new();
    for id in ["claude-code", "codex", "gemini", "opencode"] {
        descriptors.push(descriptor(AdapterFamily::PtySemantic, id));
    }
    for id in ["claude-code", "codex", "gemini", "opencode", "kimi"] {
        descriptors.push(descriptor(AdapterFamily::Pipe, id));
    }
    for id in [
        "claude",
        "codex",
        "opencode",
        "pi",
        "amp",
        "cursor",
        "kimi",
        "qwen-code",
        "copilot",
        "antigravity",
    ] {
        let (revision, verification) = match id {
            "claude" => (CLAUDE_CODE_INLINE_REVISION, AdapterVerification::VendorCanary),
            "codex" => (CODEX_CLI_INLINE_REVISION, AdapterVerification::VendorCanary),
            "kimi" => (KIMI_CODE_INLINE_REVISION, AdapterVerification::VendorCanary),
            "qwen-code" => (QWEN_CODE_INLINE_REVISION, AdapterVerification::Reference),
            _ => (ONE_SHOT_REVISION, AdapterVerification::SyntheticFixture),
        };
        descriptors.push(descriptor_with_revision_and_verification(
            AdapterFamily::OneShot,
            id,
            revision,
            verification,
        ));
    }
    for id in ["gemini", "opencode"] {
        descriptors.push(descriptor(AdapterFamily::Acp, id));
    }
    descriptors.push(descriptor(AdapterFamily::Hook, "claude-code"));
    descriptors.push(descriptor(AdapterFamily::Hook, "codex"));
    descriptors.push(descriptor(AdapterFamily::Hook, "gemini"));
    descriptors.push(descriptor(AdapterFamily::Hook, "opencode"));
    descriptors.push(descriptor(AdapterFamily::Hook, "mimo-code"));
    descriptors.push(descriptor(AdapterFamily::Hook, "pi"));
    descriptors.push(descriptor(AdapterFamily::Hook, "omp"));
    descriptors.push(descriptor(AdapterFamily::Hook, "antigravity"));
    descriptors.push(descriptor(AdapterFamily::Hook, "amp"));
    descriptors.push(descriptor(AdapterFamily::Hook, "command-code"));
    descriptors.push(descriptor(AdapterFamily::Hook, "hermes"));
    descriptors.push(descriptor(AdapterFamily::Hook, "devin"));
    for id in ["grok", "kimi", "copilot", "droid", "cursor"] {
        descriptors.push(descriptor(AdapterFamily::Hook, id));
        descriptors.push(descriptor(AdapterFamily::History, id));
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
        descriptors.push(descriptor_with_revision(
            AdapterFamily::ManagedHook,
            id,
            MANAGED_HOOK_REVISION,
        ));
    }
    for id in [
        "claude-code",
        "codex",
        "gemini",
        "antigravity",
        "opencode",
        "hermes",
        "rovo",
        "openclaw",
        "pi",
        "omp",
        "devin",
    ] {
        descriptors.push(descriptor(AdapterFamily::History, id));
    }
    for id in [
        "claude-code",
        "codex",
        "gemini",
        "antigravity",
        "opencode",
        "pi",
        "mimo-code",
        "grok",
        "droid",
        "devin",
        "kimi",
    ] {
        descriptors.push(descriptor(AdapterFamily::Resume, id));
    }
    for id in ["claude-code", "codex", "gemini", "cursor"] {
        descriptors.push(descriptor_with_revision(
            AdapterFamily::SessionOptions,
            id,
            SESSION_OPTION_CATALOG_REVISION,
        ));
    }
    descriptors.push(descriptor_with_revision(
        AdapterFamily::CapabilityProbe,
        "cursor",
        CAPABILITY_PROBE_REVISION,
    ));
    descriptors
}

fn descriptor(family: AdapterFamily, id: &str) -> AdapterDescriptor {
    descriptor_with_revision(family, id, BUILTIN_ADAPTER_REVISION)
}

fn descriptor_with_revision(family: AdapterFamily, id: &str, revision: &str) -> AdapterDescriptor {
    descriptor_with_revision_and_verification(
        family,
        id,
        revision,
        AdapterVerification::SyntheticFixture,
    )
}

fn descriptor_with_revision_and_verification(
    family: AdapterFamily,
    id: &str,
    revision: &str,
    verification: AdapterVerification,
) -> AdapterDescriptor {
    let agent_ids = if family == AdapterFamily::Hook && id == "claude-code" {
        vec!["claude", "openclaude"]
    } else {
        vec![if id == "claude-code" { "claude" } else { id }]
    };
    AdapterDescriptor {
        family,
        binding: AdapterBinding::new(
            AdapterId::new(id).expect("hardcoded adapter ID"),
            revision,
            verification,
        )
        .expect("hardcoded adapter binding"),
        agents: agent_ids
            .into_iter()
            .map(|agent_id| AgentId::new(agent_id).expect("hardcoded agent ID"))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn family_is_part_of_the_registry_key() {
        let registry = builtin_adapter_registry();
        let id = AdapterId::new("gemini").unwrap();
        assert!(registry.get(AdapterFamily::PtySemantic, &id).is_some());
        assert!(registry.get(AdapterFamily::Pipe, &id).is_some());
        assert!(registry.get(AdapterFamily::Acp, &id).is_some());
        assert!(registry.get(AdapterFamily::Hook, &id).is_some());
        assert!(registry.get(AdapterFamily::History, &id).is_some());
        let claude = AdapterId::new("claude-code").unwrap();
        assert!(registry.get(AdapterFamily::Hook, &claude).is_some());
        assert!(registry.get(AdapterFamily::History, &claude).is_some());
        for id in [
            "codex",
            "opencode",
            "pi",
            "omp",
            "antigravity",
            "hermes",
            "devin",
        ] {
            let id = AdapterId::new(id).unwrap();
            assert!(registry.get(AdapterFamily::Hook, &id).is_some());
            assert!(registry.get(AdapterFamily::History, &id).is_some());
        }
        for id in ["mimo-code", "amp", "command-code"] {
            let id = AdapterId::new(id).unwrap();
            assert!(registry.get(AdapterFamily::Hook, &id).is_some());
            assert!(registry.get(AdapterFamily::History, &id).is_none());
        }
        for id in ["rovo", "openclaw"] {
            let id = AdapterId::new(id).unwrap();
            assert!(registry.get(AdapterFamily::Hook, &id).is_none());
            assert!(registry.get(AdapterFamily::History, &id).is_some());
        }
    }

    #[test]
    fn history_registry_matches_the_pinned_orca_source_inventory() {
        let actual = builtin_adapter_registry()
            .iter()
            .filter(|descriptor| descriptor.family == AdapterFamily::History)
            .map(|descriptor| descriptor.binding.id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        let expected = [
            "antigravity",
            "claude-code",
            "codex",
            "copilot",
            "cursor",
            "devin",
            "droid",
            "gemini",
            "grok",
            "hermes",
            "kimi",
            "omp",
            "opencode",
            "openclaw",
            "pi",
            "rovo",
        ]
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(actual, expected);
    }

    #[test]
    fn hook_registry_matches_the_pinned_orca_source_inventory() {
        let actual = builtin_adapter_registry()
            .iter()
            .filter(|descriptor| descriptor.family == AdapterFamily::Hook)
            .map(|descriptor| descriptor.binding.id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        let expected = [
            "amp",
            "antigravity",
            "claude-code",
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
            "mimo-code",
            "omp",
            "opencode",
            "pi",
        ]
        .into_iter()
        .collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn managed_hook_registry_matches_the_pinned_orca_control_inventory() {
        let actual = builtin_adapter_registry()
            .iter()
            .filter(|descriptor| descriptor.family == AdapterFamily::ManagedHook)
            .map(|descriptor| {
                (
                    descriptor.binding.id.as_str(),
                    descriptor.binding.revision.as_str(),
                )
            })
            .collect::<std::collections::BTreeSet<_>>();
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
        .map(|id| (id, MANAGED_HOOK_REVISION))
        .collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn one_shot_registry_tracks_live_and_reference_contract_revisions() {
        let actual = builtin_adapter_registry()
            .iter()
            .filter(|descriptor| descriptor.family == AdapterFamily::OneShot)
            .map(|descriptor| {
                (
                    descriptor.binding.id.as_str(),
                    descriptor.binding.revision.as_str(),
                )
            })
            .collect::<std::collections::BTreeSet<_>>();
        let expected = [
            ("amp", ONE_SHOT_REVISION),
            ("antigravity", ONE_SHOT_REVISION),
            ("claude", CLAUDE_CODE_INLINE_REVISION),
            ("codex", CODEX_CLI_INLINE_REVISION),
            ("copilot", ONE_SHOT_REVISION),
            ("cursor", ONE_SHOT_REVISION),
            ("kimi", KIMI_CODE_INLINE_REVISION),
            ("opencode", ONE_SHOT_REVISION),
            ("pi", ONE_SHOT_REVISION),
            ("qwen-code", QWEN_CODE_INLINE_REVISION),
        ]
        .into_iter()
        .collect();
        assert_eq!(actual, expected);
        for id in ["claude", "codex", "kimi"] {
            assert_eq!(
                builtin_adapter_registry()
                    .binding(AdapterFamily::OneShot, id)
                    .unwrap()
                    .verification,
                AdapterVerification::VendorCanary,
                "{id}"
            );
        }
        assert_eq!(
            builtin_adapter_registry()
                .binding(AdapterFamily::OneShot, "qwen-code")
                .unwrap()
                .verification,
            AdapterVerification::Reference
        );
    }

    #[test]
    fn resume_registry_matches_the_supported_live_inventory() {
        let actual = builtin_adapter_registry()
            .iter()
            .filter(|descriptor| descriptor.family == AdapterFamily::Resume)
            .map(|descriptor| descriptor.binding.id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        let expected = [
            "antigravity",
            "claude-code",
            "codex",
            "devin",
            "droid",
            "gemini",
            "grok",
            "kimi",
            "mimo-code",
            "opencode",
            "pi",
        ]
        .into_iter()
        .collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn session_option_registry_matches_the_pinned_orca_catalog_inventory() {
        let actual = builtin_adapter_registry()
            .iter()
            .filter(|descriptor| descriptor.family == AdapterFamily::SessionOptions)
            .map(|descriptor| {
                (
                    descriptor.binding.id.as_str(),
                    descriptor.binding.revision.as_str(),
                )
            })
            .collect::<std::collections::BTreeSet<_>>();
        let expected = ["claude-code", "codex", "cursor", "gemini"]
            .into_iter()
            .map(|id| (id, SESSION_OPTION_CATALOG_REVISION))
            .collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn capability_probe_registry_matches_the_pinned_orca_executed_inventory() {
        let actual = builtin_adapter_registry()
            .iter()
            .filter(|descriptor| descriptor.family == AdapterFamily::CapabilityProbe)
            .map(|descriptor| {
                (
                    descriptor.binding.id.as_str(),
                    descriptor.binding.revision.as_str(),
                )
            })
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            actual,
            [("cursor", CAPABILITY_PROBE_REVISION)]
                .into_iter()
                .collect()
        );
    }

    #[test]
    fn binding_revision_must_match_the_registered_implementation() {
        let registry = builtin_adapter_registry();
        let binding = AdapterBinding::new(
            AdapterId::new("codex").unwrap(),
            "other-revision",
            AdapterVerification::Reference,
        )
        .unwrap();
        assert!(!registry.supports(AdapterFamily::Pipe, &binding));
    }

    #[test]
    fn runtime_resolution_is_family_and_revision_exact() {
        let binding = builtin_adapter_registry()
            .binding(AdapterFamily::Pipe, "codex")
            .unwrap()
            .clone();
        let mut registry = AdapterRuntimeRegistry::default();
        registry
            .insert(AdapterFamily::Pipe, binding.clone(), "pipe-runtime")
            .unwrap();

        assert_eq!(
            registry.resolve(AdapterFamily::Pipe, &binding).unwrap(),
            &"pipe-runtime"
        );
        assert!(matches!(
            registry.resolve(AdapterFamily::PtySemantic, &binding),
            Err(AdapterRuntimeRegistryError::Unavailable { .. })
        ));

        let other_revision =
            AdapterBinding::new(binding.id, "other-revision", AdapterVerification::Reference)
                .unwrap();
        assert!(matches!(
            registry.resolve(AdapterFamily::Pipe, &other_revision),
            Err(AdapterRuntimeRegistryError::RevisionMismatch { .. })
        ));
    }
}
