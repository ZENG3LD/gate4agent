use crate::{builtin_adapter_registry, AgentSpec};
use gate4agent_adapters::{
    parse_session_option_models, plan_mid_session_action, plan_mid_session_option,
    resolve_session_option_launch, session_option_catalog, AgentSessionOptionCatalog,
    ResolvedSessionOptionLaunch, SessionOptionAdapterError, SessionOptionMidSessionPlan,
    SessionOptionModel,
};
use gate4agent_types::{
    AdapterBinding, AdapterFamily, AgentCommand, AgentId, SessionOptionSelection,
    SessionOptionValue,
};
use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionOptionControlPlan {
    pub semantics: SessionOptionMidSessionPlan,
    pub command: Option<AgentCommand>,
}

pub fn session_option_catalog_for(
    spec: &AgentSpec,
) -> Result<AgentSessionOptionCatalog, SessionOptionCatalogError> {
    let binding = session_option_binding(spec)?;
    session_option_catalog(&binding.id).map_err(SessionOptionCatalogError::Adapter)
}

pub fn resolve_session_option_launch_for(
    spec: &AgentSpec,
    selection: &SessionOptionSelection,
    trailing_agent_args: &[String],
) -> Result<ResolvedSessionOptionLaunch, SessionOptionCatalogError> {
    let binding = session_option_binding(spec)?;
    resolve_session_option_launch(&binding.id, selection, trailing_agent_args)
        .map_err(SessionOptionCatalogError::Adapter)
}

pub fn plan_mid_session_option_for(
    spec: &AgentSpec,
    current: &SessionOptionSelection,
    option_id: &str,
    target: SessionOptionValue,
) -> Result<SessionOptionMidSessionPlan, SessionOptionCatalogError> {
    let binding = session_option_binding(spec)?;
    plan_mid_session_option(&binding.id, current, option_id, target)
        .map_err(SessionOptionCatalogError::Adapter)
}

pub fn plan_mid_session_action_for(
    spec: &AgentSpec,
    current: &SessionOptionSelection,
    option_id: &str,
) -> Result<SessionOptionMidSessionPlan, SessionOptionCatalogError> {
    let binding = session_option_binding(spec)?;
    plan_mid_session_action(&binding.id, current, option_id)
        .map_err(SessionOptionCatalogError::Adapter)
}

pub fn plan_mid_session_control_for(
    spec: &AgentSpec,
    current: &SessionOptionSelection,
    option_id: &str,
    target: SessionOptionValue,
) -> Result<SessionOptionControlPlan, SessionOptionCatalogError> {
    let semantics = plan_mid_session_option_for(spec, current, option_id, target)?;
    bind_control_plan(spec, semantics)
}

pub fn plan_mid_session_action_control_for(
    spec: &AgentSpec,
    current: &SessionOptionSelection,
    option_id: &str,
) -> Result<SessionOptionControlPlan, SessionOptionCatalogError> {
    let semantics = plan_mid_session_action_for(spec, current, option_id)?;
    bind_control_plan(spec, semantics)
}

pub fn parse_session_option_models_for(
    spec: &AgentSpec,
    stdout: &str,
) -> Result<Vec<SessionOptionModel>, SessionOptionCatalogError> {
    let binding = session_option_binding(spec)?;
    parse_session_option_models(&binding.id, stdout).map_err(SessionOptionCatalogError::Adapter)
}

fn session_option_binding(spec: &AgentSpec) -> Result<&AdapterBinding, SessionOptionCatalogError> {
    let binding = spec
        .capabilities
        .adapters
        .session_options
        .as_ref()
        .ok_or_else(|| SessionOptionCatalogError::UnsupportedAgent(spec.id.clone()))?;
    if !builtin_adapter_registry().supports(AdapterFamily::SessionOptions, binding) {
        return Err(SessionOptionCatalogError::UnavailableBinding {
            agent_id: spec.id.clone(),
            adapter_id: binding.id.as_str().to_owned(),
            revision: binding.revision.clone(),
        });
    }
    Ok(binding)
}

fn bind_control_plan(
    spec: &AgentSpec,
    semantics: SessionOptionMidSessionPlan,
) -> Result<SessionOptionControlPlan, SessionOptionCatalogError> {
    let command_text = match &semantics {
        SessionOptionMidSessionPlan::Noop => None,
        SessionOptionMidSessionPlan::Command { command, .. }
        | SessionOptionMidSessionPlan::AgentPicker { command } => Some(command),
    };
    let command = command_text
        .map(|command| {
            let mut segments = command.split_ascii_whitespace();
            let name = segments
                .next()
                .and_then(|name| name.strip_prefix('/'))
                .filter(|name| !name.is_empty())
                .ok_or(SessionOptionCatalogError::InvalidGeneratedCommand)?;
            Ok(AgentCommand {
                agent_id: spec.id.clone(),
                name: name.to_owned(),
                arguments: segments.map(str::to_owned).collect(),
            })
        })
        .transpose()?;
    Ok(SessionOptionControlPlan { semantics, command })
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SessionOptionCatalogError {
    #[error("agent {0} does not declare session options")]
    UnsupportedAgent(AgentId),
    #[error(
        "agent {agent_id} declares unavailable session-option adapter {adapter_id} at revision {revision}"
    )]
    UnavailableBinding {
        agent_id: AgentId,
        adapter_id: String,
        revision: String,
    },
    #[error("session-option adapter rejected the request: {0}")]
    Adapter(#[source] SessionOptionAdapterError),
    #[error("session-option adapter produced an invalid provider-native command")]
    InvalidGeneratedCommand,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin_registry;

    #[test]
    fn spec_binding_is_required_and_revision_exact() {
        let registry = builtin_registry();
        let claude = registry.get_by_id("claude").unwrap();
        assert_eq!(
            session_option_catalog_for(claude)
                .unwrap()
                .adapter_id
                .as_str(),
            "claude-code"
        );
        let opencode = registry.get_by_id("opencode").unwrap();
        assert!(matches!(
            session_option_catalog_for(opencode),
            Err(SessionOptionCatalogError::UnsupportedAgent(_))
        ));

        let mut stale = claude.clone();
        stale
            .capabilities
            .adapters
            .session_options
            .as_mut()
            .unwrap()
            .revision = "stale-revision".to_owned();
        assert!(matches!(
            session_option_catalog_for(&stale),
            Err(SessionOptionCatalogError::UnavailableBinding { .. })
        ));
    }

    #[test]
    fn spec_bound_mid_session_plans_never_fall_back_to_another_provider() {
        let registry = builtin_registry();
        let cursor = registry.get_by_id("cursor").unwrap();
        let current = SessionOptionSelection::new("gpt-5.3-codex")
            .with_value("effort", "high")
            .with_value("fastMode", false);
        assert!(matches!(
            plan_mid_session_option_for(cursor, &current, "fastMode", true.into()).unwrap(),
            SessionOptionMidSessionPlan::Command { command, .. }
                if command == "/model gpt-5.3-codex-high-fast"
        ));
        let control =
            plan_mid_session_control_for(cursor, &current, "fastMode", true.into()).unwrap();
        assert_eq!(
            control.command,
            Some(AgentCommand {
                agent_id: cursor.id.clone(),
                name: "model".to_owned(),
                arguments: vec!["gpt-5.3-codex-high-fast".to_owned()],
            })
        );
        gate4agent_types::prepare_agent_command(control.command.unwrap(), &cursor.id).unwrap();
    }
}
