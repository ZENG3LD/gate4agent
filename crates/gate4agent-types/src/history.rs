use crate::{OperationId, WORKING_DIRECTORY_MAX_BYTES};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const HISTORY_DISCOVERY_LIMIT_MAX: u16 = 256;
pub const HISTORY_CANDIDATE_ID_MAX_BYTES: usize = 1_024;
pub const HISTORY_SESSION_ID_MAX_BYTES: usize = 512;
pub const HISTORY_TITLE_MAX_BYTES: usize = 512;
pub const HISTORY_MODEL_MAX_BYTES: usize = 512;
pub const HISTORY_MESSAGES_MAX: usize = 256;
pub const HISTORY_MESSAGE_MAX_BYTES: usize = 16_384;
pub const HISTORY_ERROR_MAX_BYTES: usize = 4_096;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HistoryQuery {
    pub working_directory: Option<String>,
    pub limit: u16,
}

impl HistoryQuery {
    pub fn validate(&self) -> Result<(), HistoryValidationError> {
        if !(1..=HISTORY_DISCOVERY_LIMIT_MAX).contains(&self.limit) {
            return Err(HistoryValidationError::InvalidLimit);
        }
        if let Some(working_directory) = &self.working_directory {
            if working_directory.trim().is_empty()
                || working_directory.len() > WORKING_DIRECTORY_MAX_BYTES
                || working_directory.chars().any(char::is_control)
            {
                return Err(HistoryValidationError::InvalidWorkingDirectory);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HistoryCandidateSummary {
    pub id: String,
    pub session_id_hint: String,
    pub modified_at_unix_ms: Option<u64>,
}

impl HistoryCandidateSummary {
    pub fn validate(&self) -> Result<(), HistoryValidationError> {
        validate_candidate_id(&self.id)?;
        validate_required_identifier(
            &self.session_id_hint,
            HISTORY_SESSION_ID_MAX_BYTES,
            "session id",
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HistoryMessageRole {
    User,
    Assistant,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HistoryMessageRecord {
    pub role: HistoryMessageRole,
    pub text: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HistorySessionRecord {
    pub session_id: String,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub message_count: u64,
    pub total_tokens: u64,
    pub messages: Vec<HistoryMessageRecord>,
}

impl HistorySessionRecord {
    pub fn validate(&self) -> Result<(), HistoryValidationError> {
        validate_required_identifier(&self.session_id, HISTORY_SESSION_ID_MAX_BYTES, "session id")?;
        validate_optional_text(&self.title, HISTORY_TITLE_MAX_BYTES, "title")?;
        validate_optional_identifier(&self.cwd, WORKING_DIRECTORY_MAX_BYTES, "working directory")?;
        validate_optional_identifier(&self.model, HISTORY_MODEL_MAX_BYTES, "model")?;
        if self.messages.len() > HISTORY_MESSAGES_MAX {
            return Err(HistoryValidationError::TooManyMessages);
        }
        for message in &self.messages {
            validate_text(&message.text, HISTORY_MESSAGE_MAX_BYTES, "message")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum HistoryOperation {
    Discover { query: HistoryQuery },
    Load { candidate_id: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PendingHistoryOperation {
    pub operation_id: OperationId,
    pub operation: HistoryOperation,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct HistorySnapshot {
    pub pending: Option<PendingHistoryOperation>,
    pub candidates: Vec<HistoryCandidateSummary>,
    pub loaded: Option<HistorySessionRecord>,
    pub last_error: Option<String>,
}

impl HistorySnapshot {
    pub fn candidate(&self, candidate_id: &str) -> Option<&HistoryCandidateSummary> {
        self.candidates
            .iter()
            .find(|candidate| candidate.id == candidate_id)
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum HistoryValidationError {
    #[error("history discovery limit is outside the supported bounded range")]
    InvalidLimit,
    #[error("history working directory is empty, too large, or contains controls")]
    InvalidWorkingDirectory,
    #[error("history candidate ID is not a bounded opaque ASCII token")]
    InvalidCandidateId,
    #[error("history {field} is empty, too large, or contains controls")]
    InvalidField { field: &'static str },
    #[error("history result contains too many retained messages")]
    TooManyMessages,
}

pub fn validate_candidate_id(value: &str) -> Result<(), HistoryValidationError> {
    if value.trim().is_empty()
        || value.len() > HISTORY_CANDIDATE_ID_MAX_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(HistoryValidationError::InvalidCandidateId);
    }
    Ok(())
}

pub fn validate_history_error(message: &str) -> Result<(), HistoryValidationError> {
    validate_required_text(message, HISTORY_ERROR_MAX_BYTES, "error")
}

fn validate_optional_text(
    value: &Option<String>,
    max: usize,
    field: &'static str,
) -> Result<(), HistoryValidationError> {
    if let Some(value) = value {
        validate_text(value, max, field)?;
    }
    Ok(())
}

fn validate_optional_identifier(
    value: &Option<String>,
    max: usize,
    field: &'static str,
) -> Result<(), HistoryValidationError> {
    if let Some(value) = value {
        validate_required_identifier(value, max, field)?;
    }
    Ok(())
}

fn validate_required_identifier(
    value: &str,
    max: usize,
    field: &'static str,
) -> Result<(), HistoryValidationError> {
    if value.trim().is_empty() || value.len() > max || value.chars().any(char::is_control) {
        return Err(HistoryValidationError::InvalidField { field });
    }
    Ok(())
}

fn validate_required_text(
    value: &str,
    max: usize,
    field: &'static str,
) -> Result<(), HistoryValidationError> {
    if value.trim().is_empty() {
        return Err(HistoryValidationError::InvalidField { field });
    }
    validate_text(value, max, field)
}

fn validate_text(
    value: &str,
    max: usize,
    field: &'static str,
) -> Result<(), HistoryValidationError> {
    if value.len() > max
        || value
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(HistoryValidationError::InvalidField { field });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_candidate_and_result_bounds_are_explicit() {
        assert_eq!(
            HistoryQuery {
                working_directory: None,
                limit: 0,
            }
            .validate(),
            Err(HistoryValidationError::InvalidLimit)
        );
        assert_eq!(
            validate_candidate_id(r"C:\history\session.jsonl"),
            Err(HistoryValidationError::InvalidCandidateId)
        );
        let record = HistorySessionRecord {
            session_id: "session-1".to_owned(),
            title: None,
            cwd: None,
            model: None,
            message_count: 0,
            total_tokens: 0,
            messages: Vec::new(),
        };
        assert_eq!(record.validate(), Ok(()));

        let mut record_with_control = record;
        record_with_control.session_id = "session\n2".to_owned();
        assert_eq!(
            record_with_control.validate(),
            Err(HistoryValidationError::InvalidField {
                field: "session id"
            })
        );
    }
}
