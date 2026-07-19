//! Pure provider adapter contracts and implementations.
//!
//! Role: adapter shell logic without OS authority.
//! Owns: revisioned adapter definitions and pure provider parsing/building.
//! Exports: adapter registry and family-specific pure adapters.
//! Forbidden: async, locks, channels, process, filesystem, database, network,
//! credentials, and product presentation policy.

mod history;
mod hook;
mod hook_session;
mod resume;

use gate4agent_types::{AdapterBinding, AdapterFamily, AdapterId, AdapterVerification, AgentId};
use std::collections::BTreeMap;
use std::sync::OnceLock;
use thiserror::Error;

pub use history::{
    parse_history, HistoryAdapterError, HistoryDocument, HistoryMessage, HistoryRole,
    HistorySession, HISTORY_DOCUMENT_MAX_BYTES, HISTORY_MESSAGE_MAX_CHARS,
    HISTORY_METADATA_MAX_BYTES, HISTORY_STORED_MESSAGES_MAX,
};
pub use hook::{
    normalize_hook_event, HookAdapterError, HOOK_EVENT_NAME_MAX_BYTES, HOOK_PAYLOAD_MAX_BYTES,
    HOOK_TEXT_MAX_CHARS, OPENCODE_HOOK_TEXT_MAX_CHARS,
};
pub use hook_session::{
    HookEventDisposition, HookEventEnvelope, HookReduction, HookSessionReducer,
    HookSessionReducerError, HookSubagentSeed, HOOK_EVENT_ID_MAX_BYTES, HOOK_SEEN_EVENT_IDS_MAX,
};
pub use resume::{build_resume_plan, ResumeAdapterError, ResumePlan, RESUME_SESSION_ID_MAX_BYTES};

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
    for id in ["claude-code", "codex", "gemini", "opencode"] {
        descriptors.push(descriptor(AdapterFamily::PtySemantic, id));
        descriptors.push(descriptor(AdapterFamily::Pipe, id));
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
    for id in ["grok", "kimi", "copilot", "droid", "cursor"] {
        descriptors.push(descriptor(AdapterFamily::Hook, id));
        descriptors.push(descriptor(AdapterFamily::History, id));
    }
    for id in [
        "claude-code",
        "codex",
        "gemini",
        "opencode",
        "grok",
        "droid",
    ] {
        descriptors.push(descriptor(AdapterFamily::Resume, id));
    }
    descriptors
}

fn descriptor(family: AdapterFamily, id: &str) -> AdapterDescriptor {
    let agent_id = if id == "claude-code" { "claude" } else { id };
    AdapterDescriptor {
        family,
        binding: AdapterBinding::new(
            AdapterId::new(id).expect("hardcoded adapter ID"),
            BUILTIN_ADAPTER_REVISION,
            AdapterVerification::SyntheticFixture,
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
        let id = AdapterId::new("gemini").unwrap();
        assert!(registry.get(AdapterFamily::PtySemantic, &id).is_some());
        assert!(registry.get(AdapterFamily::Pipe, &id).is_some());
        assert!(registry.get(AdapterFamily::Acp, &id).is_some());
        assert!(registry.get(AdapterFamily::Hook, &id).is_some());
        assert!(registry.get(AdapterFamily::History, &id).is_none());
        let claude = AdapterId::new("claude-code").unwrap();
        assert!(registry.get(AdapterFamily::Hook, &claude).is_some());
        assert!(registry.get(AdapterFamily::History, &claude).is_none());
        for id in ["codex", "gemini", "opencode", "mimo-code", "pi", "omp"] {
            let id = AdapterId::new(id).unwrap();
            assert!(registry.get(AdapterFamily::Hook, &id).is_some());
            assert!(registry.get(AdapterFamily::History, &id).is_none());
        }
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
