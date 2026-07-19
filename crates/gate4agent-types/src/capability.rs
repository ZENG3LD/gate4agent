use crate::{OperationId, SessionGeneration, WORKING_DIRECTORY_MAX_BYTES};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const CAPABILITY_MODELS_MAX: usize = 512;
pub const CAPABILITY_MODEL_ID_MAX_BYTES: usize = 512;
pub const CAPABILITY_MODEL_LABEL_MAX_BYTES: usize = 1_024;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CapabilityProbeRequest {
    pub working_directory: String,
}

impl CapabilityProbeRequest {
    pub fn validate(&self) -> Result<(), CapabilityValidationError> {
        if self.working_directory.is_empty()
            || self.working_directory.len() > WORKING_DIRECTORY_MAX_BYTES
            || self.working_directory.contains('\0')
        {
            return Err(CapabilityValidationError::InvalidWorkingDirectory);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CapabilityModelSummary {
    pub id: String,
    pub label: String,
}

impl CapabilityModelSummary {
    pub fn validate(&self) -> Result<(), CapabilityValidationError> {
        if self.id.is_empty()
            || self.id.len() > CAPABILITY_MODEL_ID_MAX_BYTES
            || self.id.chars().any(char::is_whitespace)
            || self.id.chars().any(char::is_control)
        {
            return Err(CapabilityValidationError::InvalidModelId);
        }
        if self.label.trim().is_empty()
            || self.label.len() > CAPABILITY_MODEL_LABEL_MAX_BYTES
            || self.label.chars().any(char::is_control)
        {
            return Err(CapabilityValidationError::InvalidModelLabel);
        }
        Ok(())
    }
}

pub fn validate_capability_models(
    models: &[CapabilityModelSummary],
) -> Result<(), CapabilityValidationError> {
    if models.len() > CAPABILITY_MODELS_MAX {
        return Err(CapabilityValidationError::TooManyModels);
    }
    let mut seen = std::collections::BTreeSet::new();
    for model in models {
        model.validate()?;
        if !seen.insert(model.id.as_str()) {
            return Err(CapabilityValidationError::DuplicateModelId);
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum CapabilityProbeFailure {
    ExecutorUnavailable,
    SpawnUnavailable,
    TimedOut,
    OutputLimitExceeded,
    NonZeroExit { exit_code: Option<i32> },
    AuthorityRejected,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PendingCapabilityProbe {
    pub operation_id: OperationId,
    pub generation: SessionGeneration,
    pub request: CapabilityProbeRequest,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct CapabilitySnapshot {
    pub pending: Option<PendingCapabilityProbe>,
    pub settled: bool,
    pub session_option_models: Vec<CapabilityModelSummary>,
    pub last_failure: Option<CapabilityProbeFailure>,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum CapabilityValidationError {
    #[error("capability probe working directory is empty, too large, or contains a NUL byte")]
    InvalidWorkingDirectory,
    #[error("capability model ID is empty, too large, or contains whitespace or controls")]
    InvalidModelId,
    #[error("capability model label is empty, too large, or contains controls")]
    InvalidModelLabel,
    #[error("capability model result exceeds its count bound")]
    TooManyModels,
    #[error("capability model result contains duplicate IDs")]
    DuplicateModelId,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_models_are_bounded_and_unique() {
        let valid = vec![CapabilityModelSummary {
            id: "gpt-5.3-codex".to_owned(),
            label: "GPT-5.3 Codex".to_owned(),
        }];
        assert!(validate_capability_models(&valid).is_ok());

        let duplicate = vec![valid[0].clone(), valid[0].clone()];
        assert_eq!(
            validate_capability_models(&duplicate),
            Err(CapabilityValidationError::DuplicateModelId)
        );
    }
}
