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
pub const NATIVE_SESSION_CATALOG_LIMIT_MAX: u16 = 64;
pub const NATIVE_SESSION_PREVIEW_MESSAGE_LIMIT_MAX: u16 = 24;
pub const NATIVE_SESSION_PREVIEW_TEXT_MAX_BYTES: usize = 4_096;
pub const NATIVE_SESSION_EXTERNAL_GROUP_ID_MAX_BYTES: usize = 128;
pub const NATIVE_SESSION_EXTERNAL_GROUP_LABEL_MAX_BYTES: usize = 256;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NativeSessionCatalogScope {
    Workspace,
    Unregistered,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct NativeSessionExternalGroup {
    pub group_id: String,
    pub kind: NativeSessionExternalGroupKind,
    pub display_name: String,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NativeSessionExternalGroupKind {
    Project,
    Global,
}

impl NativeSessionExternalGroup {
    pub fn validate(&self) -> Result<(), HistoryValidationError> {
        if self.group_id.trim().is_empty()
            || self.group_id.len() > NATIVE_SESSION_EXTERNAL_GROUP_ID_MAX_BYTES
            || !self
                .group_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(HistoryValidationError::InvalidField {
                field: "native session external group id",
            });
        }
        validate_required_identifier(
            &self.display_name,
            NATIVE_SESSION_EXTERNAL_GROUP_LABEL_MAX_BYTES,
            "native session external group display name",
        )?;
        if self.display_name == "."
            || self.display_name == ".."
            || self.display_name.contains('/')
            || self.display_name.contains('\\')
            || self.display_name.contains(':')
        {
            return Err(HistoryValidationError::InvalidField {
                field: "native session external group display name",
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeSessionCatalogEntry {
    pub selection_id: String,
    pub session_id: String,
    pub title: Option<String>,
    pub modified_at_unix_ms: Option<u64>,
    pub model: Option<String>,
    pub message_count: u64,
    pub completed_turn_count: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NativeSessionCatalogWindow {
    Recent,
    Older,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NativeSessionCatalogSummary {
    pub catalog_revision: u64,
    pub recent_cutoff_unix_ms: u64,
    pub recent_total_count: u32,
    pub older_total_count: u32,
    pub recent_next_after_selection_id: Option<String>,
    pub recent_has_more: bool,
}

impl NativeSessionCatalogSummary {
    pub fn validate(&self) -> Result<(), HistoryValidationError> {
        validate_catalog_cursor_presence(
            &self.recent_next_after_selection_id,
            self.recent_has_more,
        )
    }

    pub fn validate_initial_entries(
        &self,
        entry_count: usize,
    ) -> Result<(), HistoryValidationError> {
        self.validate()?;
        let entry_count = u32::try_from(entry_count).map_err(|_| {
            HistoryValidationError::InvalidField {
                field: "catalog summary",
            }
        })?;
        if self.recent_total_count < entry_count
            || self.recent_has_more != (self.recent_total_count > entry_count)
        {
            return Err(HistoryValidationError::InvalidField {
                field: "catalog summary",
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeSessionCatalogPage {
    pub window: NativeSessionCatalogWindow,
    pub revision: u64,
    pub entries: Vec<NativeSessionCatalogEntry>,
    pub next_after_selection_id: Option<String>,
    pub remaining_count: u32,
    pub has_more: bool,
}

impl NativeSessionCatalogPage {
    pub fn validate(&self) -> Result<(), HistoryValidationError> {
        if self.entries.len() > usize::from(NATIVE_SESSION_CATALOG_LIMIT_MAX) {
            return Err(HistoryValidationError::TooManyMessages);
        }
        for (index, entry) in self.entries.iter().enumerate() {
            entry.validate()?;
            if self.entries[..index]
                .iter()
                .any(|existing| existing.session_id == entry.session_id)
            {
                return Err(HistoryValidationError::InvalidField {
                    field: "catalog entries",
                });
            }
        }
        validate_catalog_cursor(
            &self.next_after_selection_id,
            self.has_more,
            self.remaining_count,
        )
    }
}

fn validate_catalog_cursor(
    cursor: &Option<String>,
    has_more: bool,
    remaining_count: u32,
) -> Result<(), HistoryValidationError> {
    validate_catalog_cursor_presence(cursor, has_more)?;
    if has_more != (remaining_count > 0) {
        return Err(HistoryValidationError::InvalidField {
            field: "catalog cursor",
        });
    }
    Ok(())
}

fn validate_catalog_cursor_presence(
    cursor: &Option<String>,
    has_more: bool,
) -> Result<(), HistoryValidationError> {
    if let Some(cursor) = cursor {
        validate_candidate_id(cursor)?;
    }
    if has_more != cursor.is_some() {
        return Err(HistoryValidationError::InvalidField {
            field: "catalog cursor",
        });
    }
    Ok(())
}

impl NativeSessionCatalogEntry {
    pub fn validate(&self) -> Result<(), HistoryValidationError> {
        validate_candidate_id(&self.selection_id)?;
        validate_required_identifier(&self.session_id, HISTORY_SESSION_ID_MAX_BYTES, "session id")?;
        validate_optional_single_line_text(&self.title, HISTORY_TITLE_MAX_BYTES, "title")?;
        validate_optional_identifier(&self.model, HISTORY_MODEL_MAX_BYTES, "model")
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NativeSessionPreviewMessage {
    pub role: HistoryMessageRole,
    pub text: String,
}

impl NativeSessionPreviewMessage {
    pub fn validate(&self) -> Result<(), HistoryValidationError> {
        validate_text(&self.text, NATIVE_SESSION_PREVIEW_TEXT_MAX_BYTES, "preview message")
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NativeSessionPreview {
    pub session_id: String,
    pub title: Option<String>,
    pub modified_at_unix_ms: Option<u64>,
    pub model: Option<String>,
    pub message_count: u64,
    #[serde(default)]
    pub message_count_exact: bool,
    pub completed_turn_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    pub truncated: bool,
    pub messages: Vec<NativeSessionPreviewMessage>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionRecordPreview {
    pub title: Option<String>,
    pub modified_at_unix_ms: Option<u64>,
    pub model: Option<String>,
    pub message_count: u64,
    #[serde(default)]
    pub message_count_exact: bool,
    pub completed_turn_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    pub truncated: bool,
    pub messages: Vec<NativeSessionPreviewMessage>,
}

impl SessionRecordPreview {
    pub fn validate(&self) -> Result<(), HistoryValidationError> {
        validate_optional_single_line_text(&self.title, HISTORY_TITLE_MAX_BYTES, "title")?;
        validate_optional_identifier(&self.model, HISTORY_MODEL_MAX_BYTES, "model")?;
        if self.messages.len() > usize::from(NATIVE_SESSION_PREVIEW_MESSAGE_LIMIT_MAX) {
            return Err(HistoryValidationError::TooManyMessages);
        }
        for message in &self.messages {
            message.validate()?;
        }
        Ok(())
    }
}

impl From<NativeSessionPreview> for SessionRecordPreview {
    fn from(preview: NativeSessionPreview) -> Self {
        Self {
            title: preview.title,
            modified_at_unix_ms: preview.modified_at_unix_ms,
            model: preview.model,
            message_count: preview.message_count,
            message_count_exact: preview.message_count_exact,
            completed_turn_count: preview.completed_turn_count,
            total_tokens: preview.total_tokens,
            truncated: preview.truncated,
            messages: preview.messages,
        }
    }
}

impl NativeSessionPreview {
    pub fn validate(&self) -> Result<(), HistoryValidationError> {
        validate_required_identifier(&self.session_id, HISTORY_SESSION_ID_MAX_BYTES, "session id")?;
        validate_optional_single_line_text(&self.title, HISTORY_TITLE_MAX_BYTES, "title")?;
        validate_optional_identifier(&self.model, HISTORY_MODEL_MAX_BYTES, "model")?;
        if self.messages.len() > usize::from(NATIVE_SESSION_PREVIEW_MESSAGE_LIMIT_MAX) {
            return Err(HistoryValidationError::TooManyMessages);
        }
        for message in &self.messages {
            message.validate()?;
        }
        Ok(())
    }
}

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_turn_count: Option<u64>,
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
    pub loaded_candidate_id: Option<String>,
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

pub fn validate_native_session_id(value: &str) -> Result<(), HistoryValidationError> {
    validate_required_identifier(value, HISTORY_SESSION_ID_MAX_BYTES, "session id")
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

fn validate_optional_single_line_text(
    value: &Option<String>,
    max: usize,
    field: &'static str,
) -> Result<(), HistoryValidationError> {
    if value
        .as_ref()
        .is_some_and(|value| value.len() > max || value.chars().any(char::is_control))
    {
        return Err(HistoryValidationError::InvalidField { field });
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
            completed_turn_count: None,
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

    #[test]
    fn completed_turn_count_is_backward_compatible_optional_metadata() {
        let legacy = serde_json::from_str::<HistorySessionRecord>(
            r#"{"session_id":"session-1","title":null,"cwd":null,"model":null,"message_count":2,"total_tokens":3,"messages":[]}"#,
        )
        .unwrap();
        assert_eq!(legacy.completed_turn_count, None);
        assert!(serde_json::to_value(&legacy)
            .unwrap()
            .get("completed_turn_count")
            .is_none());

        let current = HistorySessionRecord {
            completed_turn_count: Some(1),
            ..legacy
        };
        assert_eq!(
            serde_json::to_value(&current).unwrap()["completed_turn_count"],
            1
        );
    }

    #[test]
    fn native_session_catalog_entry_is_bounded_metadata_only() {
        let entry = NativeSessionCatalogEntry {
            selection_id: "hist_selection_1".to_owned(),
            session_id: "session-1".to_owned(),
            title: Some("Review".to_owned()),
            modified_at_unix_ms: Some(7),
            model: Some("model-1".to_owned()),
            message_count: 4,
            completed_turn_count: Some(2),
        };
        assert_eq!(entry.validate(), Ok(()));
        let NativeSessionCatalogEntry {
            selection_id: _,
            session_id: _,
            title: _,
            modified_at_unix_ms: _,
            model: _,
            message_count: _,
            completed_turn_count: _,
        } = &entry;
        let injected = NativeSessionCatalogEntry {
            title: Some("safe\nunsafe".to_owned()),
            ..entry
        };
        assert_eq!(
            injected.validate(),
            Err(HistoryValidationError::InvalidField { field: "title" })
        );
    }

    #[test]
    fn preview_token_totals_are_optional_and_backward_compatible() {
        let legacy_native = serde_json::from_str::<NativeSessionPreview>(
            r#"{"session_id":"session-1","title":null,"modified_at_unix_ms":null,"model":null,"message_count":0,"message_count_exact":true,"completed_turn_count":null,"truncated":false,"messages":[]}"#,
        )
        .unwrap();
        assert_eq!(legacy_native.total_tokens, None);
        assert!(serde_json::to_value(&legacy_native)
            .unwrap()
            .get("total_tokens")
            .is_none());

        let legacy_record = serde_json::from_str::<SessionRecordPreview>(
            r#"{"title":null,"modified_at_unix_ms":null,"model":null,"message_count":0,"message_count_exact":true,"completed_turn_count":null,"truncated":false,"messages":[]}"#,
        )
        .unwrap();
        assert_eq!(legacy_record.total_tokens, None);

        let observed_zero = NativeSessionPreview {
            total_tokens: Some(0),
            ..legacy_native
        };
        assert_eq!(
            serde_json::to_value(&observed_zero).unwrap()["total_tokens"],
            0
        );
        assert_eq!(
            SessionRecordPreview::from(observed_zero).total_tokens,
            Some(0)
        );
    }
}
