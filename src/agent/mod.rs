//! Extensible agent identity, launch specifications, and registry.
//!
//! This module is the compatibility seam between gate4agent's original closed
//! four-tool API and a broader CLI grounding layer. It plans argv and deferred
//! prompt delivery only; transport ownership and provider-specific parsers stay
//! in their existing modules during the migration.

mod process;
mod readiness;

pub use gate4agent_catalog::{
    builtin_registry, builtin_specs, merge_session_option_models, parse_session_option_models_for,
    plan_draft_launch, plan_launch, plan_mid_session_action_control_for,
    plan_mid_session_control_for, resolve_session_option_launch_for, session_option_catalog_for,
    AgentRegistry, AgentSessionOptionCatalog, EnvMutation, LaunchPlan, LaunchPlanError,
    LaunchRequest, RegistryError, ResolvedSessionOptionLaunch, SessionOptionCatalogError,
    SessionOptionControlPlan, SessionOptionMidSessionPlan, SessionOptionModel,
    MAX_LAUNCH_PROMPT_BYTES, ORCA_REFERENCE_REVISION, SESSION_OPTION_CATALOG_REVISION,
    WINDOWS_INLINE_LAUNCH_MAX_CHARS,
};
pub use gate4agent_types::{
    prepare_agent_command, prepare_input, prepare_input_with_limits, prepare_shell_command,
    sanitize_prompt_text, AgentCommand, InputAction, InputPrepareError, PreparedInput,
    PreparedInputKind, PreparedWrite, PreparedWriteKind, PromptFraming, PromptPayload,
    ShellCommand, TerminalControl, TerminalText, BRACKETED_PASTE_END, BRACKETED_PASTE_START,
    TERMINAL_INPUT_CHUNK_MAX_BYTES, TERMINAL_INPUT_MAX_BYTES, TERMINAL_SUBMIT_DELAY_MS,
    TERMINAL_WRITE_DELAY_MAX_MS,
};
pub use gate4agent_types::{
    AgentCapabilities, AgentCommandMode, AgentId, AgentIdError, AgentReadinessSpec, AgentSpec,
    DetectionSpec, DraftReadySignal, InitialPromptMode, LaunchSpec, NativeDraftMode,
    ProcessMatcher, PromptSpec, RuntimePlatform, SessionOptionSelection, SessionOptionValue,
    SpecVerification,
};
pub use process::{
    is_agent_foreground_wrapper, is_expected_agent_command_line, is_expected_agent_process,
    recognize_agent_process, recognize_agent_process_from_command_line, tokenize_command_line,
    ProcessRecognitionOptions, RecognitionPath, RecognizedAgentProcess,
};
pub use readiness::{
    ForegroundObservation, ReadinessIntent, ReadinessPermit, ReadinessStatus, ReadinessTracker,
    ReadyReason,
};

use crate::core::types::CliTool;
use thiserror::Error;

impl From<CliTool> for AgentId {
    fn from(tool: CliTool) -> Self {
        let id = match tool {
            CliTool::ClaudeCode => "claude",
            CliTool::Codex => "codex",
            CliTool::Gemini => "gemini",
            CliTool::OpenCode => "opencode",
        };
        AgentId::new(id).expect("legacy CLI tool IDs are valid")
    }
}

impl TryFrom<&AgentId> for CliTool {
    type Error = LegacyCliToolError;

    fn try_from(id: &AgentId) -> Result<Self, Self::Error> {
        match id.as_str() {
            "claude" => Ok(Self::ClaudeCode),
            "codex" => Ok(Self::Codex),
            "gemini" => Ok(Self::Gemini),
            "opencode" => Ok(Self::OpenCode),
            _ => Err(LegacyCliToolError(id.clone())),
        }
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("agent '{0}' has no legacy CliTool variant")]
pub struct LegacyCliToolError(pub AgentId);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_mapping_is_explicit_and_one_way_for_new_agents() {
        assert_eq!(AgentId::from(CliTool::ClaudeCode).as_str(), "claude");
        assert_eq!(
            CliTool::try_from(&AgentId::new("opencode").unwrap()).unwrap(),
            CliTool::OpenCode
        );
        assert!(CliTool::try_from(&AgentId::new("grok").unwrap()).is_err());
    }
}
