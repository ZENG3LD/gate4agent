//! Shell-neutral authority ports for provider history and resume preparation.
//!
//! Role: port contracts between pure provider adapters and effect-owning shells.
//! Owns: no runtime or domain state.
//! Exports: validated history discovery/load and resume authorization contracts.
//! Imports: pure gate4agent types and adapters only.
//! Forbidden: async runtimes, locks, channels, filesystem, database, process,
//! network, credentials, and product presentation policy.

mod history;
mod resume;

use gate4agent_types::{AdapterBinding, AdapterFamily, AgentId, AgentSpec};
use thiserror::Error;

pub use history::{
    discover_history, load_history_session, HistoryAuthority, HistoryCandidate, HistoryCandidateId,
    HistoryDiscoveryRequest, HistoryLoadRequest, HistoryPortError, HISTORY_CANDIDATE_ID_MAX_BYTES,
    HISTORY_DISCOVERY_LIMIT_MAX,
};
pub use resume::{
    prepare_resume, PreparedResume, ResumeAuthority, ResumeAuthorityDecision, ResumeOutcome,
    ResumePortError, ResumeRequest, RESUME_DENIAL_REASON_MAX_BYTES,
};

fn declared_binding(
    spec: &AgentSpec,
    family: AdapterFamily,
) -> Result<AdapterBinding, ProviderPortValidationError> {
    let binding = match family {
        AdapterFamily::History => spec.capabilities.adapters.history.as_ref(),
        AdapterFamily::Resume => spec.capabilities.adapters.resume.as_ref(),
        _ => None,
    }
    .ok_or_else(|| ProviderPortValidationError::UnsupportedFamily {
        agent_id: spec.id.clone(),
        family,
    })?;
    binding
        .validate()
        .map_err(|error| ProviderPortValidationError::InvalidBinding {
            agent_id: spec.id.clone(),
            family,
            message: error.to_string(),
        })?;
    Ok(binding.clone())
}

fn validate_working_directory(
    working_directory: Option<String>,
) -> Result<Option<String>, ProviderPortValidationError> {
    let Some(value) = working_directory else {
        return Ok(None);
    };
    if value.trim().is_empty()
        || value.len() > gate4agent_types::WORKING_DIRECTORY_MAX_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(ProviderPortValidationError::InvalidWorkingDirectory);
    }
    Ok(Some(value))
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ProviderPortValidationError {
    #[error("agent {agent_id} does not declare a {family:?} adapter")]
    UnsupportedFamily {
        agent_id: AgentId,
        family: AdapterFamily,
    },
    #[error("agent {agent_id} declares an invalid {family:?} adapter: {message}")]
    InvalidBinding {
        agent_id: AgentId,
        family: AdapterFamily,
        message: String,
    },
    #[error("working directory is empty, too large, or contains control characters")]
    InvalidWorkingDirectory,
    #[error("history discovery limit must be between 1 and {max}")]
    InvalidHistoryLimit { max: u16 },
    #[error("history candidate ID must be a bounded opaque ASCII token, not a host path")]
    InvalidHistoryCandidateId,
    #[error("history session ID hint is empty, unsafe, or too large")]
    InvalidHistorySessionId,
    #[error("history candidate belongs to another agent or adapter binding")]
    HistoryCandidateSourceMismatch,
    #[error("resume session ID is empty, unsafe, or too large")]
    InvalidResumeSessionId,
}
