use crate::{builtin_adapter_registry, AgentSpec};
use gate4agent_adapters::{
    capability_probe_plan, parse_capability_models, CapabilityProbeAdapterError,
};
use gate4agent_types::{AdapterBinding, AdapterFamily, AgentId, CapabilityModelSummary};
use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedCapabilityProbePlan {
    pub program: String,
    pub args: Vec<String>,
}

pub fn resolve_capability_probe_for(
    spec: &AgentSpec,
) -> Result<ResolvedCapabilityProbePlan, CapabilityProbeCatalogError> {
    let probe = capability_probe_binding(spec)?;
    let session_options = spec
        .capabilities
        .adapters
        .session_options
        .as_ref()
        .ok_or_else(|| CapabilityProbeCatalogError::MissingSessionOptions(spec.id.clone()))?;
    if session_options.id != probe.id {
        return Err(CapabilityProbeCatalogError::MismatchedAdapters {
            agent_id: spec.id.clone(),
            capability_probe_id: probe.id.to_string(),
            session_options_id: session_options.id.to_string(),
        });
    }
    let adapter = capability_probe_plan(&probe.id).map_err(CapabilityProbeCatalogError::Adapter)?;
    let mut args = spec.launch.fixed_args.clone();
    args.extend(adapter.args);
    Ok(ResolvedCapabilityProbePlan {
        // LaunchSpec is the structured command-override boundary. Browser
        // commands never provide a program or arguments for this operation.
        program: spec.launch.program.clone(),
        args,
    })
}

pub fn parse_capability_models_for(
    spec: &AgentSpec,
    stdout: &str,
) -> Result<Vec<CapabilityModelSummary>, CapabilityProbeCatalogError> {
    let binding = capability_probe_binding(spec)?;
    parse_capability_models(&binding.id, stdout).map_err(CapabilityProbeCatalogError::Adapter)
}

fn capability_probe_binding(
    spec: &AgentSpec,
) -> Result<&AdapterBinding, CapabilityProbeCatalogError> {
    let binding = spec
        .capabilities
        .adapters
        .capability_probe
        .as_ref()
        .ok_or_else(|| CapabilityProbeCatalogError::UnsupportedAgent(spec.id.clone()))?;
    if !builtin_adapter_registry().supports(AdapterFamily::CapabilityProbe, binding) {
        return Err(CapabilityProbeCatalogError::UnavailableBinding {
            agent_id: spec.id.clone(),
            adapter_id: binding.id.to_string(),
            revision: binding.revision.clone(),
        });
    }
    Ok(binding)
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum CapabilityProbeCatalogError {
    #[error("agent {0} does not declare a capability probe")]
    UnsupportedAgent(AgentId),
    #[error("agent {0} declares a capability probe without session options")]
    MissingSessionOptions(AgentId),
    #[error(
        "agent {agent_id} capability-probe adapter {capability_probe_id} does not match session-options adapter {session_options_id}"
    )]
    MismatchedAdapters {
        agent_id: AgentId,
        capability_probe_id: String,
        session_options_id: String,
    },
    #[error(
        "agent {agent_id} declares unavailable capability-probe adapter {adapter_id} at revision {revision}"
    )]
    UnavailableBinding {
        agent_id: AgentId,
        adapter_id: String,
        revision: String,
    },
    #[error("capability-probe adapter rejected the request: {0}")]
    Adapter(#[source] CapabilityProbeAdapterError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin_registry;
    use gate4agent_types::{AdapterId, AdapterVerification};

    #[test]
    fn undeclared_probe_binding_fails_closed() {
        let registry = builtin_registry();
        // No provider in the current fleet declares a capability probe.
        for id in ["claude", "codex", "grok", "kimi"] {
            assert!(matches!(
                resolve_capability_probe_for(registry.get_by_id(id).unwrap()),
                Err(CapabilityProbeCatalogError::UnsupportedAgent(_))
            ));
        }
    }

    #[test]
    fn stale_probe_binding_fails_closed() {
        let mut claude = builtin_registry().get_by_id("claude").unwrap().clone();
        claude.capabilities.adapters.capability_probe = Some(
            AdapterBinding::new(
                AdapterId::new("claude-code").unwrap(),
                "stale",
                AdapterVerification::SyntheticFixture,
            )
            .unwrap(),
        );
        assert!(matches!(
            resolve_capability_probe_for(&claude),
            Err(CapabilityProbeCatalogError::UnavailableBinding { .. })
        ));
    }
}
