use crate::{
    validate_candidate_id, HistoryValidationError, OperationId, ProviderSessionIdentity,
    ProviderSessionKey, TerminalSize, WORKING_DIRECTORY_MAX_BYTES,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const RESUME_ERROR_MAX_BYTES: usize = 4_096;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ResumeLaunchRequest {
    pub working_directory: String,
    pub terminal_size: TerminalSize,
}

impl ResumeLaunchRequest {
    pub fn validate(&self) -> Result<(), ResumeValidationError> {
        if self.working_directory.trim().is_empty()
            || self.working_directory.len() > WORKING_DIRECTORY_MAX_BYTES
            || self.working_directory.chars().any(char::is_control)
        {
            return Err(ResumeValidationError::InvalidWorkingDirectory);
        }
        if !self.terminal_size.is_valid() {
            return Err(ResumeValidationError::InvalidTerminalSize);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ResumeTarget {
    CurrentProvider,
    HistoryCandidate { candidate_id: String },
}

impl ResumeTarget {
    pub fn validate(&self) -> Result<(), ResumeValidationError> {
        match self {
            Self::CurrentProvider => Ok(()),
            Self::HistoryCandidate { candidate_id } => validate_candidate_id(candidate_id)
                .map_err(ResumeValidationError::InvalidHistoryCandidate),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ResumeAuthorityTarget {
    ProviderSession { identity: ProviderSessionIdentity },
    HistoryCandidate { candidate_id: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResumePhase {
    Authorizing,
    Spawning,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PendingResumeOperation {
    pub operation_id: OperationId,
    pub target: ResumeTarget,
    pub request: ResumeLaunchRequest,
    pub phase: ResumePhase,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ResumeSessionSummary {
    pub key: ProviderSessionKey,
    pub id: String,
}

impl From<&ProviderSessionIdentity> for ResumeSessionSummary {
    fn from(identity: &ProviderSessionIdentity) -> Self {
        Self {
            key: identity.key,
            id: identity.id.clone(),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ResumeSnapshot {
    pub pending: Option<PendingResumeOperation>,
    pub last_session: Option<ResumeSessionSummary>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ResumeValidationError {
    #[error("resume working directory is empty, too large, or contains controls")]
    InvalidWorkingDirectory,
    #[error("resume terminal size is outside the supported bounded range")]
    InvalidTerminalSize,
    #[error("resume history candidate is invalid: {0}")]
    InvalidHistoryCandidate(HistoryValidationError),
    #[error("resume error is empty, too large, or contains unsafe controls")]
    InvalidError,
}

pub fn validate_resume_error(message: &str) -> Result<(), ResumeValidationError> {
    if message.trim().is_empty()
        || message.len() > RESUME_ERROR_MAX_BYTES
        || message
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(ResumeValidationError::InvalidError);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_requests_and_public_summaries_are_bounded() {
        assert_eq!(
            ResumeLaunchRequest {
                working_directory: "C:/repo".to_owned(),
                terminal_size: TerminalSize {
                    rows: 24,
                    columns: 80,
                },
            }
            .validate(),
            Ok(())
        );
        assert!(ResumeTarget::HistoryCandidate {
            candidate_id: r"C:\sessions\one.jsonl".to_owned(),
        }
        .validate()
        .is_err());
        assert!(validate_resume_error("authorization failed").is_ok());
        assert!(validate_resume_error("bad\0error").is_err());
    }
}
