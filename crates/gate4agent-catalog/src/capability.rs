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

    #[test]
    fn cursor_probe_uses_structured_launch_override_and_adapter_owned_suffix() {
        let mut cursor = builtin_registry().get_by_id("cursor").unwrap().clone();
        cursor.launch.program = "npx".to_owned();
        cursor.launch.fixed_args = vec!["cursor-agent".to_owned()];
        assert_eq!(
            resolve_capability_probe_for(&cursor).unwrap(),
            ResolvedCapabilityProbePlan {
                program: "npx".to_owned(),
                args: vec!["cursor-agent".to_owned(), "--list-models".to_owned()],
            }
        );
    }

    #[test]
    fn undeclared_and_stale_probe_bindings_fail_closed() {
        let registry = builtin_registry();
        assert!(matches!(
            resolve_capability_probe_for(registry.get_by_id("gemini").unwrap()),
            Err(CapabilityProbeCatalogError::UnsupportedAgent(_))
        ));

        let mut cursor = registry.get_by_id("cursor").unwrap().clone();
        cursor
            .capabilities
            .adapters
            .capability_probe
            .as_mut()
            .unwrap()
            .revision = "stale".to_owned();
        assert!(matches!(
            resolve_capability_probe_for(&cursor),
            Err(CapabilityProbeCatalogError::UnavailableBinding { .. })
        ));
    }
}
