use crate::{
    builtin_adapter_registry, AdapterBinding, AdapterFamily, AdapterRegistry, AgentId, AgentSpec,
    InitialPromptMode, ProcessMatcher, RuntimePlatform,
};
use gate4agent_types::normalize_executable_name;
use std::collections::{HashMap, HashSet};
use thiserror::Error;

/// Validated collection of built-in and consumer-provided agent specs.
#[derive(Clone, Debug, Default)]
pub struct AgentRegistry {
    specs: Vec<AgentSpec>,
    id_index: HashMap<AgentId, usize>,
    exact_command_index: HashMap<String, usize>,
    windows_command_index: HashMap<String, usize>,
}

impl AgentRegistry {
    pub fn new(specs: impl IntoIterator<Item = AgentSpec>) -> Result<Self, RegistryError> {
        Self::new_with_adapters(specs, builtin_adapter_registry())
    }

    /// Builds a catalog against a consumer-extended adapter registry.
    pub fn new_with_adapters(
        specs: impl IntoIterator<Item = AgentSpec>,
        adapters: &AdapterRegistry,
    ) -> Result<Self, RegistryError> {
        let mut registry = Self::default();
        for spec in specs {
            registry.insert_with_adapters(spec, adapters)?;
        }
        Ok(registry)
    }

    pub fn insert(&mut self, spec: AgentSpec) -> Result<(), RegistryError> {
        self.insert_with_adapters(spec, builtin_adapter_registry())
    }

    /// Inserts a specification validated against a consumer adapter registry.
    pub fn insert_with_adapters(
        &mut self,
        spec: AgentSpec,
        adapters: &AdapterRegistry,
    ) -> Result<(), RegistryError> {
        validate_spec(&spec, adapters)?;
        if self.id_index.contains_key(&spec.id) {
            return Err(RegistryError::DuplicateAgentId(spec.id));
        }

        let mut exact_commands = HashSet::new();
        let mut windows_commands = HashSet::new();
        for command in std::iter::once(&spec.detection.command).chain(&spec.detection.aliases) {
            exact_commands.insert(command.clone());
            let normalized = normalize_executable_name(command, RuntimePlatform::Windows);
            windows_commands.insert(normalized.clone());
            if let Some(existing_index) = self.windows_command_index.get(&normalized) {
                return Err(RegistryError::AmbiguousCommand {
                    command: normalized,
                    first: self.specs[*existing_index].id.clone(),
                    second: spec.id.clone(),
                });
            }
        }

        let index = self.specs.len();
        self.id_index.insert(spec.id.clone(), index);
        for command in exact_commands {
            self.exact_command_index.insert(command, index);
        }
        for command in windows_commands {
            self.windows_command_index.insert(command, index);
        }
        self.specs.push(spec);
        Ok(())
    }

    pub fn get(&self, id: &AgentId) -> Option<&AgentSpec> {
        self.id_index.get(id).map(|index| &self.specs[*index])
    }

    pub fn get_by_id(&self, id: &str) -> Option<&AgentSpec> {
        self.id_index.get(id).map(|index| &self.specs[*index])
    }

    /// Resolves a process or PATH entry to an agent spec.
    ///
    /// Windows script/binary suffixes and a full path are normalized before
    /// lookup. The result is filtered by the requested runtime platform.
    pub fn find_by_command(
        &self,
        command_or_path: &str,
        platform: RuntimePlatform,
    ) -> Option<&AgentSpec> {
        let normalized = normalize_executable_name(command_or_path, platform);
        let index = match platform {
            RuntimePlatform::Windows => self.windows_command_index.get(&normalized),
            RuntimePlatform::MacOs | RuntimePlatform::Linux | RuntimePlatform::Wsl => {
                self.exact_command_index.get(&normalized)
            }
        }?;
        let spec = &self.specs[*index];
        spec.supports_platform(platform).then_some(spec)
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &AgentSpec> {
        self.specs.iter()
    }

    pub fn len(&self) -> usize {
        self.specs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.specs.is_empty()
    }
}

fn validate_spec(spec: &AgentSpec, adapters: &AdapterRegistry) -> Result<(), RegistryError> {
    if spec.revision.trim().is_empty()
        || spec.revision.len() > 256
        || spec.revision.chars().any(char::is_control)
    {
        return Err(RegistryError::InvalidRevision {
            agent: spec.id.clone(),
            revision: spec.revision.clone(),
        });
    }
    if spec.display_name.trim().is_empty() {
        return Err(RegistryError::EmptyDisplayName(spec.id.clone()));
    }
    validate_executable_name(&spec.id, "detection command", &spec.detection.command)?;
    for alias in &spec.detection.aliases {
        validate_executable_name(&spec.id, "detection alias", alias)?;
    }
    for required in &spec.detection.required_commands {
        validate_executable_name(&spec.id, "required command", required)?;
    }
    validate_launch(&spec.id, &spec.launch)?;
    validate_adapter_bindings(spec, adapters)?;
    if let Some(launch) = spec
        .capabilities
        .transports
        .pipe
        .as_ref()
        .and_then(|transport| transport.launch_override.as_ref())
    {
        validate_launch(&spec.id, launch)?;
    }
    if let Some(launch) = spec
        .capabilities
        .transports
        .acp
        .as_ref()
        .and_then(|transport| transport.launch_override.as_ref())
    {
        validate_launch(&spec.id, launch)?;
    }
    if spec.expected_processes.is_empty() {
        return Err(RegistryError::MissingExpectedProcess(spec.id.clone()));
    }
    for matcher in &spec.expected_processes {
        let value = match matcher {
            ProcessMatcher::Exact { name } => name,
            ProcessMatcher::Prefix { prefix } => prefix,
        };
        if value.is_empty()
            || value.contains('\0')
            || value.chars().any(char::is_whitespace)
            || value.contains('/')
            || value.contains('\\')
        {
            return Err(RegistryError::InvalidProcessMatcher {
                agent: spec.id.clone(),
                value: value.clone(),
            });
        }
    }
    match &spec.prompt.initial {
        InitialPromptMode::Flag { flag } | InitialPromptMode::InteractiveFlag { flag }
            if !flag.starts_with('-') || flag.contains('\0') =>
        {
            Err(RegistryError::InvalidPromptFlag {
                agent: spec.id.clone(),
                flag: flag.clone(),
            })
        }
        _ => Ok(()),
    }
}

fn validate_adapter_bindings(
    spec: &AgentSpec,
    adapters: &AdapterRegistry,
) -> Result<(), RegistryError> {
    let transports = &spec.capabilities.transports;
    for (family, binding) in [
        (AdapterFamily::PtySemantic, transports.pty_adapter.as_ref()),
        (
            AdapterFamily::Pipe,
            transports.pipe.as_ref().map(|value| &value.adapter),
        ),
        (
            AdapterFamily::Acp,
            transports.acp.as_ref().map(|value| &value.adapter),
        ),
        (
            AdapterFamily::Hook,
            spec.capabilities.adapters.hook.as_ref(),
        ),
        (
            AdapterFamily::History,
            spec.capabilities.adapters.history.as_ref(),
        ),
        (
            AdapterFamily::Resume,
            spec.capabilities.adapters.resume.as_ref(),
        ),
        (
            AdapterFamily::SessionOptions,
            spec.capabilities.adapters.session_options.as_ref(),
        ),
        (
            AdapterFamily::CapabilityProbe,
            spec.capabilities.adapters.capability_probe.as_ref(),
        ),
    ] {
        if let Some(binding) = binding {
            validate_adapter_binding(&spec.id, family, binding, adapters)?;
        }
    }
    Ok(())
}

fn validate_adapter_binding(
    agent: &AgentId,
    family: AdapterFamily,
    binding: &AdapterBinding,
    adapters: &AdapterRegistry,
) -> Result<(), RegistryError> {
    binding
        .validate()
        .map_err(|error| RegistryError::InvalidAdapterBinding {
            agent: agent.clone(),
            family,
            adapter_id: binding.id.to_string(),
            message: error.to_string(),
        })?;
    if !adapters.supports(family, binding) {
        return Err(RegistryError::UnsupportedAdapterBinding {
            agent: agent.clone(),
            family,
            adapter_id: binding.id.to_string(),
            revision: binding.revision.clone(),
        });
    }
    Ok(())
}

fn validate_executable_name(
    agent: &AgentId,
    field: &'static str,
    value: &str,
) -> Result<(), RegistryError> {
    if value.is_empty()
        || value.contains('\0')
        || value.chars().any(char::is_whitespace)
        || value.contains('/')
        || value.contains('\\')
    {
        return Err(RegistryError::InvalidExecutableName {
            agent: agent.clone(),
            field,
            value: value.to_owned(),
        });
    }
    Ok(())
}

fn validate_program(agent: &AgentId, value: &str) -> Result<(), RegistryError> {
    if value.trim().is_empty() || value.contains('\0') {
        return Err(RegistryError::InvalidProgram {
            agent: agent.clone(),
            value: value.to_owned(),
        });
    }
    Ok(())
}

fn validate_launch(agent: &AgentId, launch: &crate::LaunchSpec) -> Result<(), RegistryError> {
    validate_program(agent, &launch.program)?;
    for argument in &launch.fixed_args {
        if argument.contains('\0') {
            return Err(RegistryError::NulByte {
                agent: agent.clone(),
                field: "fixed argument",
            });
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum RegistryError {
    #[error("duplicate agent ID: {0}")]
    DuplicateAgentId(AgentId),
    #[error("command '{command}' is ambiguous between agents '{first}' and '{second}'")]
    AmbiguousCommand {
        command: String,
        first: AgentId,
        second: AgentId,
    },
    #[error("agent '{0}' has an empty display name")]
    EmptyDisplayName(AgentId),
    #[error("agent '{agent}' has invalid specification revision '{revision}'")]
    InvalidRevision { agent: AgentId, revision: String },
    #[error("agent '{agent}' has invalid {field} '{value}'")]
    InvalidExecutableName {
        agent: AgentId,
        field: &'static str,
        value: String,
    },
    #[error("agent '{agent}' has invalid launch program '{value}'")]
    InvalidProgram { agent: AgentId, value: String },
    #[error("agent '{0}' has no expected process matcher")]
    MissingExpectedProcess(AgentId),
    #[error("agent '{agent}' has invalid process matcher '{value}'")]
    InvalidProcessMatcher { agent: AgentId, value: String },
    #[error("agent '{agent}' has invalid prompt flag '{flag}'")]
    InvalidPromptFlag { agent: AgentId, flag: String },
    #[error("agent '{agent}' contains a NUL byte in {field}")]
    NulByte { agent: AgentId, field: &'static str },
    #[error("agent '{agent}' has invalid {family:?} adapter '{adapter_id}': {message}")]
    InvalidAdapterBinding {
        agent: AgentId,
        family: AdapterFamily,
        adapter_id: String,
        message: String,
    },
    #[error(
        "agent '{agent}' requires unavailable {family:?} adapter '{adapter_id}' at revision '{revision}'"
    )]
    UnsupportedAdapterBinding {
        agent: AgentId,
        family: AdapterFamily,
        adapter_id: String,
        revision: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AdapterDescriptor, AdapterId, AdapterVerification, DetectionSpec, LaunchSpec,
        ProcessMatcher, PromptSpec, SpecVerification,
    };

    fn spec(id: &str, command: &str) -> AgentSpec {
        AgentSpec {
            id: AgentId::new(id).unwrap(),
            revision: "test:1".to_owned(),
            display_name: id.to_owned(),
            detection: DetectionSpec {
                command: command.to_owned(),
                aliases: Vec::new(),
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
                initial: InitialPromptMode::AfterReady,
                native_draft: None,
            },
            readiness: crate::AgentReadinessSpec::default(),
            capabilities: crate::AgentCapabilities::default(),
            verification: SpecVerification::Reference,
        }
    }

    #[test]
    fn resolves_windows_wrappers_and_paths() {
        let registry = AgentRegistry::new([spec("qwen-code", "qwen")]).unwrap();
        let found = registry
            .find_by_command(r"C:\Users\dev\bin\QWEN.CMD", RuntimePlatform::Windows)
            .unwrap();
        assert_eq!(found.id.as_str(), "qwen-code");
    }

    #[test]
    fn unix_matching_remains_case_sensitive() {
        let registry = AgentRegistry::new([spec("qwen-code", "qwen")]).unwrap();
        assert!(registry
            .find_by_command("QWEN", RuntimePlatform::Linux)
            .is_none());
        assert!(registry
            .find_by_command("qwen", RuntimePlatform::Linux)
            .is_some());
    }

    #[test]
    fn rejects_alias_collisions() {
        let mut second = spec("second", "second");
        second.detection.aliases.push("first.cmd".to_owned());
        let error = AgentRegistry::new([spec("first", "first"), second]).unwrap_err();
        assert!(matches!(error, RegistryError::AmbiguousCommand { .. }));
    }

    #[test]
    fn consumer_specs_can_use_an_extended_adapter_registry() {
        let binding = AdapterBinding::new(
            AdapterId::new("custom-hook").unwrap(),
            "custom-hook/v1",
            AdapterVerification::SyntheticFixture,
        )
        .unwrap();
        let adapters = AdapterRegistry::new([AdapterDescriptor {
            family: AdapterFamily::Hook,
            binding: binding.clone(),
            agents: vec![AgentId::new("custom").unwrap()],
        }])
        .unwrap();
        let mut custom = spec("custom", "custom");
        custom.capabilities.adapters.hook = Some(binding);

        assert!(matches!(
            AgentRegistry::new([custom.clone()]),
            Err(RegistryError::UnsupportedAdapterBinding { .. })
        ));
        AgentRegistry::new_with_adapters([custom], &adapters).unwrap();
    }
}
