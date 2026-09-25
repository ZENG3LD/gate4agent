//! Pure provider adapter contracts and implementations.
//!
//! Role: adapter shell logic without OS authority.
//! Owns: revisioned adapter definitions and pure provider parsing/building.
//! Exports: adapter registry and family-specific pure adapters.
//! Forbidden: async, locks, channels, process, filesystem, database, network,
//! credentials, and product presentation policy.

mod capability;
mod history;
mod one_shot;
mod pty_identity;
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
pub use one_shot::{
    one_shot_spec, one_shot_specs, resolve_one_shot_plan,
    resolve_one_shot_plan_with_persistence, OneShotAdapterError, OneShotAdapterSpec,
    OneShotModelSource, OneShotModelSpec, OneShotPlan, OneShotPromptDelivery,
    OneShotSessionPersistence, OneShotThinkingLevel, CLAUDE_CODE_INLINE_REVISION,
    CODEX_CLI_INLINE_REVISION, KIMI_CODE_INLINE_REVISION, ONE_SHOT_OUTPUT_MAX_BYTES,
    ONE_SHOT_REVISION, ONE_SHOT_THINKING_OPTION_ID, ONE_SHOT_TIMEOUT_SECONDS,
};
pub use pty_identity::{
    CodexPtySessionIdentityExtractor, KimiPtySessionIdentityExtractor,
    KIMI_PTY_SESSION_ID_MAX_BYTES,
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
    for id in ["claude-code", "codex", "kimi"] {
        descriptors.push(descriptor(AdapterFamily::PtySemantic, id));
    }
    for id in ["claude-code", "codex", "kimi"] {
        descriptors.push(descriptor(AdapterFamily::Pipe, id));
    }
    for id in ["claude", "codex", "kimi"] {
        let (revision, verification) = match id {
            "claude" => (CLAUDE_CODE_INLINE_REVISION, AdapterVerification::VendorCanary),
            "codex" => (CODEX_CLI_INLINE_REVISION, AdapterVerification::VendorCanary),
            "kimi" => (KIMI_CODE_INLINE_REVISION, AdapterVerification::VendorCanary),
            _ => (ONE_SHOT_REVISION, AdapterVerification::SyntheticFixture),
        };
        descriptors.push(descriptor_with_revision_and_verification(
            AdapterFamily::OneShot,
            id,
            revision,
            verification,
        ));
    }
    for id in ["claude-code", "codex", "grok", "kimi"] {
        descriptors.push(descriptor(AdapterFamily::Acp, id));
    }
    for id in ["claude-code", "codex", "grok", "kimi"] {
        descriptors.push(descriptor(AdapterFamily::History, id));
        descriptors.push(descriptor(AdapterFamily::Resume, id));
    }
    for id in ["claude-code", "codex"] {
        descriptors.push(descriptor_with_revision(
            AdapterFamily::SessionOptions,
            id,
            SESSION_OPTION_CATALOG_REVISION,
        ));
    }
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
    let agent_id = if id == "claude-code" { "claude" } else { id };
    AdapterDescriptor {
        family,
        binding: AdapterBinding::new(
            AdapterId::new(id).expect("hardcoded adapter ID"),
            revision,
            verification,
        )
        .expect("hardcoded adapter binding"),
        agents: vec![AgentId::new(agent_id).expect("hardcoded agent ID")],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn family_is_part_of_the_registry_key() {
        let registry = builtin_adapter_registry();
        let claude = AdapterId::new("claude-code").unwrap();
        assert!(registry.get(AdapterFamily::PtySemantic, &claude).is_some());
        assert!(registry.get(AdapterFamily::Pipe, &claude).is_some());
        assert!(registry.get(AdapterFamily::Acp, &claude).is_some());
        assert!(registry.get(AdapterFamily::History, &claude).is_some());
        for id in ["codex", "kimi"] {
            let id = AdapterId::new(id).unwrap();
            assert!(registry.get(AdapterFamily::PtySemantic, &id).is_some());
            assert!(registry.get(AdapterFamily::Pipe, &id).is_some());
            assert!(registry.get(AdapterFamily::Acp, &id).is_some());
            assert!(registry.get(AdapterFamily::History, &id).is_some());
        }
        let grok = AdapterId::new("grok").unwrap();
        assert!(registry.get(AdapterFamily::PtySemantic, &grok).is_none());
        assert!(registry.get(AdapterFamily::Pipe, &grok).is_none());
        assert!(registry.get(AdapterFamily::Acp, &grok).is_some());
        assert!(registry.get(AdapterFamily::History, &grok).is_some());
    }

    /// Lifecycle hooks are retired (owner ruling 2026-09-25): sessions are
    /// observed through ACP where a provider has it, never through a global
    /// hook install. Neither `Hook` nor `ManagedHook` is registered anymore.
    #[test]
    fn hook_families_are_retired_and_unregistered() {
        let registry = builtin_adapter_registry();
        assert_eq!(
            registry
                .iter()
                .filter(|descriptor| descriptor.family == AdapterFamily::Hook
                    || descriptor.family == AdapterFamily::ManagedHook)
                .count(),
            0
        );
    }

    #[test]
    fn history_registry_matches_the_fleet_source_inventory() {
        let actual = builtin_adapter_registry()
            .iter()
            .filter(|descriptor| descriptor.family == AdapterFamily::History)
            .map(|descriptor| descriptor.binding.id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        let expected = ["claude-code", "codex", "grok", "kimi"]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();
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
            ("claude", CLAUDE_CODE_INLINE_REVISION),
            ("codex", CODEX_CLI_INLINE_REVISION),
            ("kimi", KIMI_CODE_INLINE_REVISION),
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
    }

    #[test]
    fn resume_registry_matches_the_supported_live_inventory() {
        let actual = builtin_adapter_registry()
            .iter()
            .filter(|descriptor| descriptor.family == AdapterFamily::Resume)
            .map(|descriptor| descriptor.binding.id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        let expected = ["claude-code", "codex", "grok", "kimi"].into_iter().collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn session_option_registry_matches_the_fleet_catalog_inventory() {
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
        let expected = ["claude-code", "codex"]
            .into_iter()
            .map(|id| (id, SESSION_OPTION_CATALOG_REVISION))
            .collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn capability_probe_registry_is_empty_for_the_current_fleet() {
        let actual = builtin_adapter_registry()
            .iter()
            .filter(|descriptor| descriptor.family == AdapterFamily::CapabilityProbe)
            .count();
        assert_eq!(actual, 0);
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
