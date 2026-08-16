use crate::protocol::{
    ContextPackLineageReceipt, GitSnapshot, ResolvedContextPackReceipt,
    SpawnContextDigest, SpawnContextId, MAX_CONTEXT_PACK_BYTES,
};
use gate4agent_types::{AgentId, HistoryMessageRecord, HistorySessionRecord};
use ring::digest::{Context, SHA256};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use thiserror::Error;

pub(crate) const MAX_CONTEXT_PACK_CATALOG_ENTRIES: usize = 128;
pub(crate) const CONTEXT_PACK_SELECTED_FILES: [&str; 4] = [
    "AGENTS.md",
    "README.md",
    "README",
    "Cargo.toml",
];
pub(crate) const MAX_CONTEXT_PACK_SELECTED_FILE_BYTES: usize = 16 * 1_024;
const CONTEXT_PACK_SCHEMA: &str = "g4a-context-pack-v1";
const CONTEXT_PACK_DIGEST_DOMAIN: &[u8] = b"g4a-context-pack-v1\0";
const CONTEXT_PACK_STATUS_MAX_ENTRIES: usize = 64;
const CONTEXT_PACK_COMMIT_MAX_ENTRIES: usize = 12;
const CONTEXT_PACK_BRANCH_MAX_BYTES: usize = 256;
const CONTEXT_PACK_PATH_MAX_BYTES: usize = 512;
const CONTEXT_PACK_COMMIT_ID_MAX_BYTES: usize = 128;
const CONTEXT_PACK_COMMIT_SUMMARY_MAX_BYTES: usize = 512;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ContextPackDocumentV1 {
    schema: String,
    source_provider: AgentId,
    source_message_count: u64,
    retained_messages: Vec<HistoryMessageRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    repository: Option<ContextPackRepositoryV1>,
    truncated: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct LegacyContextPackDocumentV1 {
    schema: String,
    source_provider: AgentId,
    history_session_id: String,
    title: Option<String>,
    model: Option<String>,
    source_message_count: u64,
    retained_messages: Vec<HistoryMessageRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    repository: Option<ContextPackRepositoryV1>,
    truncated: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ContextPackRepositoryV1 {
    is_repository: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    status: Vec<ContextPackGitStatusV1>,
    recent_commits: Vec<ContextPackGitCommitV1>,
    selected_files: Vec<ContextPackSelectedFileV1>,
    truncated: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ContextPackGitStatusV1 {
    index_status: String,
    worktree_status: String,
    path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    previous_path: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ContextPackGitCommitV1 {
    id: String,
    summary: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ContextPackSelectedFileV1 {
    path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_byte_len: Option<u32>,
    truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    skipped: Option<ContextPackSelectedFileSkipReason>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ContextPackSelectedFileSkipReason {
    NonUtf8,
    TooLarge,
    Unsafe,
    Unavailable,
    SecretLike,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ContextPackRepositoryFileSource {
    Utf8 {
        path: String,
        text: String,
        source_byte_len: u32,
    },
    Skipped {
        path: String,
        reason: ContextPackSelectedFileSkipReason,
        source_byte_len: Option<u32>,
    },
}

impl ContextPackRepositoryFileSource {
    pub(crate) fn utf8(path: impl Into<String>, text: String, source_byte_len: u32) -> Self {
        Self::Utf8 {
            path: path.into(),
            text,
            source_byte_len,
        }
    }

    pub(crate) fn skipped(
        path: impl Into<String>,
        reason: ContextPackSelectedFileSkipReason,
        source_byte_len: Option<u32>,
    ) -> Self {
        Self::Skipped {
            path: path.into(),
            reason,
            source_byte_len,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ContextPackRepository {
    document: ContextPackRepositoryV1,
}

impl ContextPackRepository {
    pub(crate) fn from_sources(
        git: &GitSnapshot,
        files: impl IntoIterator<Item = ContextPackRepositoryFileSource>,
    ) -> Self {
        let mut truncated = git.truncated || git.diagnostic.is_some();
        let branch = git.branch.as_deref().and_then(|branch| {
            let (branch, changed) = normalize_and_truncate_text(
                branch,
                CONTEXT_PACK_BRANCH_MAX_BYTES,
            );
            truncated |= changed;
            if branch.trim().is_empty()
                || branch.contains(':')
                || !repository_text_is_safe(&branch)
            {
                truncated = true;
                None
            } else {
                Some(branch)
            }
        });
        let mut status = Vec::new();
        for entry in &git.status {
            if status.len() == CONTEXT_PACK_STATUS_MAX_ENTRIES {
                truncated = true;
                break;
            }
            let Some(path) = bounded_repository_path(entry.path.as_utf8(), &mut truncated) else {
                continue;
            };
            let previous_path = entry
                .previous_path
                .as_ref()
                .and_then(|path| bounded_repository_path(path.as_utf8(), &mut truncated));
            let (index_status, index_changed) = normalize_and_truncate_text(
                &entry.index_status,
                2,
            );
            let (worktree_status, worktree_changed) = normalize_and_truncate_text(
                &entry.worktree_status,
                2,
            );
            truncated |= index_changed || worktree_changed;
            status.push(ContextPackGitStatusV1 {
                index_status,
                worktree_status,
                path,
                previous_path,
            });
        }
        let mut recent_commits = Vec::new();
        for commit in &git.recent_commits {
            if recent_commits.len() == CONTEXT_PACK_COMMIT_MAX_ENTRIES {
                truncated = true;
                break;
            }
            let (id, id_changed) = normalize_and_truncate_text(
                &commit.id,
                CONTEXT_PACK_COMMIT_ID_MAX_BYTES,
            );
            let (summary, summary_changed) = normalize_and_truncate_text(
                &commit.summary,
                CONTEXT_PACK_COMMIT_SUMMARY_MAX_BYTES,
            );
            truncated |= id_changed || summary_changed;
            if id.trim().is_empty()
                || !repository_text_is_safe(&id)
                || !repository_text_is_safe(&summary)
            {
                truncated = true;
                continue;
            }
            recent_commits.push(ContextPackGitCommitV1 { id, summary });
        }
        let mut selected_files = Vec::new();
        for source in files {
            if selected_files.len() == CONTEXT_PACK_SELECTED_FILES.len() {
                truncated = true;
                break;
            }
            let path = match &source {
                ContextPackRepositoryFileSource::Utf8 { path, .. }
                | ContextPackRepositoryFileSource::Skipped { path, .. } => path,
            };
            if !CONTEXT_PACK_SELECTED_FILES.contains(&path.as_str())
                || selected_files.iter().any(|file: &ContextPackSelectedFileV1| file.path == *path)
            {
                truncated = true;
                continue;
            }
            let file = match source {
                ContextPackRepositoryFileSource::Utf8 {
                    path,
                    text,
                    source_byte_len,
                } => {
                    if contains_obvious_secret(&text) {
                        truncated = true;
                        ContextPackSelectedFileV1 {
                            path,
                            content: None,
                            source_byte_len: Some(source_byte_len),
                            truncated: false,
                            skipped: Some(ContextPackSelectedFileSkipReason::SecretLike),
                        }
                    } else if contains_absolute_windows_host_path(&text) {
                        truncated = true;
                        ContextPackSelectedFileV1 {
                            path,
                            content: None,
                            source_byte_len: Some(source_byte_len),
                            truncated: false,
                            skipped: Some(ContextPackSelectedFileSkipReason::Unsafe),
                        }
                    } else {
                        let (content, content_changed) = normalize_and_truncate_text(
                            &text,
                            MAX_CONTEXT_PACK_SELECTED_FILE_BYTES,
                        );
                        truncated |= content_changed;
                        ContextPackSelectedFileV1 {
                            path,
                            content: Some(content),
                            source_byte_len: Some(source_byte_len),
                            truncated: content_changed,
                            skipped: None,
                        }
                    }
                }
                ContextPackRepositoryFileSource::Skipped {
                    path,
                    reason,
                    source_byte_len,
                } => {
                    if reason != ContextPackSelectedFileSkipReason::Unavailable {
                        truncated = true;
                    }
                    ContextPackSelectedFileV1 {
                        path,
                        content: None,
                        source_byte_len,
                        truncated: false,
                        skipped: Some(reason),
                    }
                }
            };
            selected_files.push(file);
        }
        Self {
            document: ContextPackRepositoryV1 {
                is_repository: git.is_repository,
                branch,
                status,
                recent_commits,
                selected_files,
                truncated,
            },
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct NodeContextPack {
    receipt: ResolvedContextPackReceipt,
    bytes: Vec<u8>,
}

impl fmt::Debug for NodeContextPack {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NodeContextPack")
            .field("receipt", &self.receipt)
            .field("bytes", &format_args!("[REDACTED; {} bytes]", self.bytes.len()))
            .finish()
    }
}

impl NodeContextPack {
    #[cfg(test)]
    pub(crate) fn export(
        lineage: ContextPackLineageReceipt,
        history: &HistorySessionRecord,
    ) -> Result<Self, ContextPackError> {
        Self::export_with_repository(lineage, history, None)
    }

    pub(crate) fn export_with_repository(
        lineage: ContextPackLineageReceipt,
        history: &HistorySessionRecord,
        repository: Option<ContextPackRepository>,
    ) -> Result<Self, ContextPackError> {
        let (history, _) = normalized_history(history);
        history.validate().map_err(|_| ContextPackError::InvalidHistory)?;
        if history.messages.is_empty()
            || history.message_count
                < u64::try_from(history.messages.len()).unwrap_or(u64::MAX)
        {
            return Err(ContextPackError::Empty);
        }

        let mut retained_messages = history.messages.clone();
        let mut truncated = history.message_count
            > u64::try_from(retained_messages.len()).unwrap_or(u64::MAX);
        let bytes = loop {
            let document = ContextPackDocumentV1 {
                schema: CONTEXT_PACK_SCHEMA.to_owned(),
                source_provider: lineage.source_provider.clone(),
                source_message_count: history.message_count,
                retained_messages: retained_messages.clone(),
                repository: repository.as_ref().map(|repository| repository.document.clone()),
                truncated,
            };
            let bytes = serde_json::to_vec(&document)
                .map_err(|_| ContextPackError::Serialization)?;
            if bytes.len() <= MAX_CONTEXT_PACK_BYTES as usize {
                break bytes;
            }
            if retained_messages.len() <= 1 {
                return Err(ContextPackError::TooLarge);
            }
            retained_messages.remove(0);
            truncated = true;
        };

        let digest = context_digest(&lineage, &bytes)?;
        let context_id = context_id(&digest)?;
        let receipt = ResolvedContextPackReceipt {
            id: context_id,
            digest,
            lineage,
            source_message_count: history.message_count,
            retained_message_count: u64::try_from(retained_messages.len())
                .map_err(|_| ContextPackError::TooLarge)?,
            byte_len: u32::try_from(bytes.len()).map_err(|_| ContextPackError::TooLarge)?,
            truncated,
        };
        Ok(Self { receipt, bytes })
    }

    pub(crate) fn from_materialized(
        receipt: ResolvedContextPackReceipt,
        bytes: Vec<u8>,
    ) -> Result<Self, ContextPackError> {
        if bytes.len() != receipt.byte_len as usize
            || bytes.is_empty()
            || bytes.len() > MAX_CONTEXT_PACK_BYTES as usize
            || context_digest(&receipt.lineage, &bytes)? != receipt.digest
            || context_id(&receipt.digest)? != receipt.id
        {
            return Err(ContextPackError::ReceiptMismatch);
        }
        if let Ok(document) = serde_json::from_slice::<ContextPackDocumentV1>(&bytes) {
            validate_materialized_document(
                &receipt,
                &document.schema,
                &document.source_provider,
                document.source_message_count,
                &document.retained_messages,
                document.repository.as_ref(),
                document.truncated,
                false,
            )?;
        } else {
            let document: LegacyContextPackDocumentV1 = serde_json::from_slice(&bytes)
                .map_err(|_| ContextPackError::Serialization)?;
            let legacy_history = HistorySessionRecord {
                session_id: document.history_session_id,
                title: document.title,
                cwd: None,
                model: document.model,
                message_count: document.source_message_count,
                completed_turn_count: None,
                total_tokens: 0,
                messages: document.retained_messages.clone(),
            };
            let (_, legacy_metadata_or_messages_changed) = normalized_history(&legacy_history);
            validate_materialized_document(
                &receipt,
                &document.schema,
                &document.source_provider,
                document.source_message_count,
                &document.retained_messages,
                document.repository.as_ref(),
                document.truncated,
                legacy_history.validate().is_err()
                    || (document.repository.is_some() && legacy_metadata_or_messages_changed),
            )?;
        }
        Ok(Self { receipt, bytes })
    }

    pub(crate) fn receipt(&self) -> &ResolvedContextPackReceipt {
        &self.receipt
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

fn validate_materialized_document(
    receipt: &ResolvedContextPackReceipt,
    schema: &str,
    source_provider: &AgentId,
    source_message_count: u64,
    retained_messages: &[HistoryMessageRecord],
    repository: Option<&ContextPackRepositoryV1>,
    truncated: bool,
    legacy_invalid: bool,
) -> Result<(), ContextPackError> {
    let history = HistorySessionRecord {
        session_id: "redacted-context-source".to_owned(),
        title: None,
        cwd: None,
        model: None,
        message_count: source_message_count,
        completed_turn_count: None,
        total_tokens: 0,
        messages: retained_messages.to_vec(),
    };
    let (_, history_changed) = normalized_history(&history);
    if schema != CONTEXT_PACK_SCHEMA
        || source_provider != &receipt.lineage.source_provider
        || source_message_count != receipt.source_message_count
        || u64::try_from(retained_messages.len()).ok() != Some(receipt.retained_message_count)
        || truncated != receipt.truncated
        || retained_messages.is_empty()
        || history.validate().is_err()
        || (repository.is_some() && history_changed)
        || repository.is_some_and(|repository| repository.validate().is_err())
        || legacy_invalid
    {
        return Err(ContextPackError::ReceiptMismatch);
    }
    Ok(())
}

impl ContextPackRepositoryV1 {
    fn validate(&self) -> Result<(), ()> {
        if self.status.len() > CONTEXT_PACK_STATUS_MAX_ENTRIES
            || self.recent_commits.len() > CONTEXT_PACK_COMMIT_MAX_ENTRIES
            || self.selected_files.len() > CONTEXT_PACK_SELECTED_FILES.len()
        {
            return Err(());
        }
        if let Some(branch) = &self.branch {
            if !text_is_normalized_and_bounded(branch, CONTEXT_PACK_BRANCH_MAX_BYTES)
                || branch.trim().is_empty()
                || branch.contains(':')
                || !repository_text_is_safe(branch)
            {
                return Err(());
            }
        }
        for entry in &self.status {
            if !text_is_normalized_and_bounded(&entry.index_status, 2)
                || !text_is_normalized_and_bounded(&entry.worktree_status, 2)
                || !repository_path_is_safe(&entry.path)
                || entry.path.len() > CONTEXT_PACK_PATH_MAX_BYTES
                || entry.previous_path.as_ref().is_some_and(|path| {
                    !repository_path_is_safe(path) || path.len() > CONTEXT_PACK_PATH_MAX_BYTES
                })
            {
                return Err(());
            }
        }
        for commit in &self.recent_commits {
            if commit.id.trim().is_empty()
                || !text_is_normalized_and_bounded(
                    &commit.id,
                    CONTEXT_PACK_COMMIT_ID_MAX_BYTES,
                )
                || !text_is_normalized_and_bounded(
                    &commit.summary,
                    CONTEXT_PACK_COMMIT_SUMMARY_MAX_BYTES,
                )
                || !repository_text_is_safe(&commit.id)
                || !repository_text_is_safe(&commit.summary)
            {
                return Err(());
            }
        }
        let mut seen = Vec::new();
        for file in &self.selected_files {
            if !CONTEXT_PACK_SELECTED_FILES.contains(&file.path.as_str())
                || seen.contains(&file.path)
                || file.content.is_some() == file.skipped.is_some()
                || file.content.as_ref().is_some_and(|content| {
                    !text_is_normalized_and_bounded(
                        content,
                        MAX_CONTEXT_PACK_SELECTED_FILE_BYTES,
                    ) || contains_obvious_secret(content)
                        || contains_absolute_windows_host_path(content)
                })
            {
                return Err(());
            }
            seen.push(file.path.clone());
        }
        Ok(())
    }
}

fn normalized_history(history: &HistorySessionRecord) -> (HistorySessionRecord, bool) {
    let (session_id, session_changed) = normalize_text(&history.session_id);
    let (title, title_changed) = normalize_optional_text(history.title.as_deref());
    let (model, model_changed) = normalize_optional_text(history.model.as_deref());
    let mut messages_changed = false;
    let messages = history
        .messages
        .iter()
        .map(|message| {
            let (text, changed) = normalize_text(&message.text);
            messages_changed |= changed;
            HistoryMessageRecord {
                role: message.role,
                text,
            }
        })
        .collect();
    (
        HistorySessionRecord {
            session_id,
            title,
            cwd: None,
            model,
            message_count: history.message_count,
            completed_turn_count: None,
            total_tokens: 0,
            messages,
        },
        session_changed
            || title_changed
            || model_changed
            || messages_changed,
    )
}

fn normalize_optional_text(value: Option<&str>) -> (Option<String>, bool) {
    let Some(value) = value else {
        return (None, false);
    };
    let (normalized, changed) = normalize_text(value);
    if normalized.trim().is_empty() {
        (None, true)
    } else {
        (Some(normalized), changed)
    }
}

fn normalize_text(value: &str) -> (String, bool) {
    let mut normalized = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    let mut changed = false;
    while let Some(character) = chars.next() {
        if character == '\u{1b}' {
            changed = true;
            match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    while let Some(sequence) = chars.next() {
                        if ('@'..='~').contains(&sequence) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(sequence) = chars.next() {
                        if sequence == '\u{7}' {
                            break;
                        }
                        if sequence == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
            continue;
        }
        if character == '\r' {
            changed = true;
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            normalized.push('\n');
            continue;
        }
        if character.is_control() && !matches!(character, '\n' | '\t') {
            changed = true;
            continue;
        }
        normalized.push(character);
    }
    if normalized == value {
        changed = false;
    }
    (normalized, changed)
}

fn normalize_and_truncate_text(value: &str, max_bytes: usize) -> (String, bool) {
    let (mut normalized, mut changed) = normalize_text(value);
    if normalized.len() > max_bytes {
        let mut end = max_bytes;
        while !normalized.is_char_boundary(end) {
            end -= 1;
        }
        normalized.truncate(end);
        changed = true;
    }
    (normalized, changed)
}

fn text_is_normalized_and_bounded(value: &str, max_bytes: usize) -> bool {
    value.len() <= max_bytes && normalize_text(value) == (value.to_owned(), false)
}

fn bounded_repository_path(value: Option<&str>, truncated: &mut bool) -> Option<String> {
    let Some(value) = value else {
        *truncated = true;
        return None;
    };
    if !repository_path_is_safe(value) {
        *truncated = true;
        return None;
    }
    let (path, changed) = normalize_and_truncate_text(value, CONTEXT_PACK_PATH_MAX_BYTES);
    *truncated |= changed;
    Some(path)
}

fn repository_path_is_safe(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.contains(':')
        && !path.chars().any(char::is_control)
        && repository_text_is_safe(path)
        && path
            .split('/')
            .all(|component| !component.is_empty() && !matches!(component, "." | ".."))
}

fn repository_text_is_safe(text: &str) -> bool {
    !contains_obvious_secret(text) && !contains_absolute_windows_host_path(text)
}

fn contains_obvious_secret(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    if lower.contains("-----begin private key-----")
        || lower.contains("-----begin rsa private key-----")
        || lower.contains("-----begin openssh private key-----")
    {
        return true;
    }
    if text.split(|character: char| character.is_whitespace() || matches!(character, '"' | '\'' | ',' | ';' | '(' | ')' | '[' | ']'))
        .any(secret_token_is_obvious)
    {
        return true;
    }
    text.lines().any(|line| {
        let Some((name, value)) = line.split_once('=').or_else(|| line.split_once(':')) else {
            return false;
        };
        let name = name
            .trim()
            .trim_start_matches("export ")
            .trim_matches(|character: char| matches!(character, '"' | '\''))
            .to_ascii_lowercase();
        let secret_name = name.ends_with("api_key")
            || name.ends_with("apikey")
            || name.ends_with("access_token")
            || name.ends_with("auth_token")
            || name.ends_with("password")
            || name.ends_with("private_key")
            || name.ends_with("client_secret");
        secret_name && secret_value_looks_live(value)
    })
}

fn contains_absolute_windows_host_path(text: &str) -> bool {
    let bytes = text.as_bytes();
    for index in 0..bytes.len() {
        if index + 2 < bytes.len()
            && bytes[index].is_ascii_alphabetic()
            && bytes[index + 1] == b':'
            && matches!(bytes[index + 2], b'\\' | b'/')
            && (index == 0 || !bytes[index - 1].is_ascii_alphanumeric())
        {
            return true;
        }
        if index + 3 < bytes.len()
            && bytes[index] == b'\\'
            && bytes[index + 1] == b'\\'
            && !matches!(bytes[index + 2], b'\\' | b'/' | b'?' | b'.')
            && !bytes[index + 2].is_ascii_whitespace()
        {
            return true;
        }
        if index + 3 < bytes.len()
            && bytes[index..index + 4] == *b"\\\\?\\"
        {
            return true;
        }
    }
    false
}

fn secret_token_is_obvious(token: &str) -> bool {
    let token = token.trim_matches(|character: char| {
        matches!(character, '.' | ',' | ':' | '=' | '"' | '\'' | '`')
    });
    (token.starts_with("sk-") && token.len() >= 16)
        || (token.starts_with("ghp_") && token.len() >= 20)
        || (token.starts_with("github_pat_") && token.len() >= 24)
        || (token.starts_with("xoxb-") && token.len() >= 20)
        || (token.starts_with("AKIA")
            && token.len() == 20
            && token.bytes().all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit()))
}

fn secret_value_looks_live(value: &str) -> bool {
    let value = value
        .trim()
        .trim_matches(|character: char| matches!(character, '"' | '\'' | '`'));
    if value.len() < 12 {
        return false;
    }
    let lower = value.to_ascii_lowercase();
    ![
        "example",
        "placeholder",
        "changeme",
        "your_",
        "your-",
        "redacted",
        "fixture",
        "dummy",
        "<",
        "${",
        "env::",
        "...",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

#[derive(Clone, Default)]
pub(crate) struct ContextPackCatalog {
    packs: BTreeMap<SpawnContextId, NodeContextPack>,
}

impl fmt::Debug for ContextPackCatalog {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ContextPackCatalog")
            .field("receipts", &self.packs.values().map(NodeContextPack::receipt).collect::<Vec<_>>())
            .finish()
    }
}

impl ContextPackCatalog {
    pub(crate) fn insert(
        &mut self,
        pack: NodeContextPack,
    ) -> Result<ResolvedContextPackReceipt, ContextPackError> {
        if let Some(existing) = self.packs.get(&pack.receipt().id) {
            if existing == &pack {
                return Ok(existing.receipt().clone());
            }
            return Err(ContextPackError::IdentityConflict);
        }
        if self.packs.len() >= MAX_CONTEXT_PACK_CATALOG_ENTRIES {
            return Err(ContextPackError::CatalogFull);
        }
        let receipt = pack.receipt().clone();
        self.packs.insert(receipt.id.clone(), pack);
        Ok(receipt)
    }

    pub(crate) fn get(&self, id: &SpawnContextId) -> Option<&NodeContextPack> {
        self.packs.get(id)
    }

    pub(crate) fn remove(&mut self, id: &SpawnContextId) -> Option<NodeContextPack> {
        self.packs.remove(id)
    }
}

fn context_digest(
    lineage: &ContextPackLineageReceipt,
    bytes: &[u8],
) -> Result<SpawnContextDigest, ContextPackError> {
    let lineage = serde_json::to_vec(lineage).map_err(|_| ContextPackError::Serialization)?;
    let mut context = Context::new(&SHA256);
    context.update(CONTEXT_PACK_DIGEST_DOMAIN);
    context.update(&lineage);
    context.update(&[0]);
    context.update(bytes);
    let hex = context
        .finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    SpawnContextDigest::new(format!("sha256:{hex}"))
        .map_err(|_| ContextPackError::Serialization)
}

fn context_id(digest: &SpawnContextDigest) -> Result<SpawnContextId, ContextPackError> {
    let hex = digest
        .as_str()
        .strip_prefix("sha256:")
        .ok_or(ContextPackError::Serialization)?;
    SpawnContextId::new(format!("ctx-{hex}"))
        .map_err(|_| ContextPackError::Serialization)
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub(crate) enum ContextPackError {
    #[error("history is invalid")]
    InvalidHistory,
    #[error("history does not contain retained messages")]
    Empty,
    #[error("context pack exceeds the bounded size")]
    TooLarge,
    #[error("context pack serialization failed")]
    Serialization,
    #[error("context pack receipt does not match materialized bytes")]
    ReceiptMismatch,
    #[error("context pack catalog is full")]
    CatalogFull,
    #[error("context pack identity conflicts with existing bytes")]
    IdentityConflict,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{
        GitCommitSummary, GitStatusEntry, NodeId, RepositoryPath, SessionAddress,
        SessionKey, WorkspaceId,
    };
    use gate4agent_types::{AgentInstanceId, HistoryMessageRole, SessionGeneration};

    fn lineage() -> ContextPackLineageReceipt {
        ContextPackLineageReceipt {
            source_node_id: NodeId::new("node-local").unwrap(),
            source_session: SessionAddress {
                workspace_id: WorkspaceId::new("source").unwrap(),
                session: SessionKey {
                    instance_id: AgentInstanceId(7),
                    generation: SessionGeneration(2),
                },
            },
            source_provider: AgentId::new("qwen-code").unwrap(),
        }
    }

    #[test]
    fn export_is_deterministic_bounded_and_roundtrips_materialized_bytes() {
        let history = HistorySessionRecord {
            session_id: "vendor-session".to_owned(),
            title: Some("review".to_owned()),
            cwd: Some(r"C:\private\repo".to_owned()),
            model: Some("qwen3-coder".to_owned()),
            message_count: 2,
            completed_turn_count: None,
            total_tokens: 19,
            messages: vec![
                HistoryMessageRecord {
                    role: HistoryMessageRole::User,
                    text: "inspect the patch".to_owned(),
                },
                HistoryMessageRecord {
                    role: HistoryMessageRole::Assistant,
                    text: "the patch is bounded".to_owned(),
                },
            ],
        };
        let first = NodeContextPack::export(lineage(), &history).unwrap();
        let second = NodeContextPack::export(lineage(), &history).unwrap();

        assert_eq!(first, second);
        assert!(!first.receipt().truncated);
        assert!(!String::from_utf8_lossy(first.bytes()).contains(r"C:\private\repo"));
        let restored = NodeContextPack::from_materialized(
            first.receipt().clone(),
            first.bytes().to_vec(),
        )
        .unwrap();
        assert_eq!(restored, first);
        let document: ContextPackDocumentV1 = serde_json::from_slice(first.bytes()).unwrap();
        assert!(document.repository.is_none());
    }

    #[test]
    fn export_allowlist_omits_provider_native_identity_model_and_host_metadata() {
        let history = HistorySessionRecord {
            session_id: "PRIVATE_NATIVE_SESSION_CANARY".to_owned(),
            title: Some("PRIVATE_TITLE_CANARY".to_owned()),
            cwd: Some(r"C:\private\PRIVATE_CWD_CANARY".to_owned()),
            model: Some("PRIVATE_MODEL_CANARY".to_owned()),
            message_count: 1,
            completed_turn_count: Some(91),
            total_tokens: 92,
            messages: vec![HistoryMessageRecord {
                role: HistoryMessageRole::Assistant,
                text: "safe semantic result".to_owned(),
            }],
        };
        let pack = NodeContextPack::export(lineage(), &history).unwrap();
        let encoded = String::from_utf8(pack.bytes().to_vec()).unwrap();
        let document: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        let keys = document
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>();
        let receipt = serde_json::to_string(pack.receipt()).unwrap();

        assert_eq!(
            keys,
            [
                "retained_messages",
                "schema",
                "source_message_count",
                "source_provider",
                "truncated",
            ]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>(),
        );
        for private in [
            "PRIVATE_NATIVE_SESSION_CANARY",
            "PRIVATE_TITLE_CANARY",
            "PRIVATE_CWD_CANARY",
            "PRIVATE_MODEL_CANARY",
        ] {
            assert!(!encoded.contains(private));
            assert!(!receipt.contains(private));
        }
        for private_key in [
            "history_session_id",
            "title",
            "model",
            "cwd",
            "completed_turn_count",
            "total_tokens",
            "provider_auth",
            "provider_home",
        ] {
            assert!(!document.as_object().unwrap().contains_key(private_key));
        }
        NodeContextPack::from_materialized(pack.receipt().clone(), pack.bytes().to_vec())
            .unwrap();
    }

    #[test]
    fn export_digest_does_not_bind_private_history_identity_or_model_metadata() {
        let first_history = HistorySessionRecord {
            session_id: "native-session-one".to_owned(),
            title: Some("private title one".to_owned()),
            cwd: Some(r"C:\private\one".to_owned()),
            model: Some("private-model-one".to_owned()),
            message_count: 1,
            completed_turn_count: Some(1),
            total_tokens: 100,
            messages: vec![HistoryMessageRecord {
                role: HistoryMessageRole::User,
                text: "bounded semantic request".to_owned(),
            }],
        };
        let second_history = HistorySessionRecord {
            session_id: "native-session-two".to_owned(),
            title: Some("private title two".to_owned()),
            cwd: Some(r"D:\private\two".to_owned()),
            model: Some("private-model-two".to_owned()),
            message_count: 1,
            completed_turn_count: Some(9),
            total_tokens: 900,
            messages: first_history.messages.clone(),
        };

        let first = NodeContextPack::export(lineage(), &first_history).unwrap();
        let second = NodeContextPack::export(lineage(), &second_history).unwrap();

        assert_eq!(first.bytes(), second.bytes());
        assert_eq!(first.receipt().digest, second.receipt().digest);
        assert_eq!(first.receipt().id, second.receipt().id);
    }

    #[test]
    fn export_drops_oldest_messages_to_the_protocol_byte_limit() {
        let messages = (0..gate4agent_types::HISTORY_MESSAGES_MAX)
            .map(|index| HistoryMessageRecord {
                role: if index % 2 == 0 {
                    HistoryMessageRole::User
                } else {
                    HistoryMessageRole::Assistant
                },
                text: format!("message-{index:03}-{}", "x".repeat(8_000)),
            })
            .collect::<Vec<_>>();
        let history = HistorySessionRecord {
            session_id: "large-session".to_owned(),
            title: None,
            cwd: None,
            model: None,
            message_count: messages.len() as u64,
            completed_turn_count: None,
            total_tokens: 0,
            messages,
        };
        let pack = NodeContextPack::export(lineage(), &history).unwrap();
        let document: ContextPackDocumentV1 = serde_json::from_slice(pack.bytes()).unwrap();

        assert!(pack.bytes().len() <= MAX_CONTEXT_PACK_BYTES as usize);
        assert!(pack.receipt().truncated);
        assert!(document.retained_messages.first().unwrap().text.starts_with("message-"));
        assert!(document.retained_messages.last().unwrap().text.starts_with("message-255"));
    }

    #[test]
    fn materialized_legacy_pack_without_repository_preserves_valid_crlf() {
        let lineage = lineage();
        let document = LegacyContextPackDocumentV1 {
            schema: CONTEXT_PACK_SCHEMA.to_owned(),
            source_provider: lineage.source_provider.clone(),
            history_session_id: "legacy-session".to_owned(),
            title: None,
            model: Some("codex".to_owned()),
            source_message_count: 1,
            retained_messages: vec![HistoryMessageRecord {
                role: HistoryMessageRole::Assistant,
                text: "legacy line one\r\nlegacy line two".to_owned(),
            }],
            repository: None,
            truncated: false,
        };
        let bytes = serde_json::to_vec(&document).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("repository"));
        let digest = context_digest(&lineage, &bytes).unwrap();
        let receipt = ResolvedContextPackReceipt {
            id: context_id(&digest).unwrap(),
            digest,
            lineage,
            source_message_count: 1,
            retained_message_count: 1,
            byte_len: bytes.len() as u32,
            truncated: false,
        };

        assert!(receipt.is_valid());
        NodeContextPack::from_materialized(receipt, bytes).unwrap();
    }

    #[test]
    fn repository_context_is_relative_bounded_and_secret_safe() {
        let history = HistorySessionRecord {
            session_id: "vendor-session".to_owned(),
            title: Some("review".to_owned()),
            cwd: Some(r"C:\private\source-root".to_owned()),
            model: Some("codex".to_owned()),
            message_count: 1,
            completed_turn_count: None,
            total_tokens: 0,
            messages: vec![HistoryMessageRecord {
                role: HistoryMessageRole::User,
                text: "explain the repository state".to_owned(),
            }],
        };
        let git = GitSnapshot {
            is_repository: true,
            branch: Some("\u{1b}[31mmain\u{1b}[0m".to_owned()),
            status: vec![GitStatusEntry {
                index_status: "M".to_owned(),
                worktree_status: " ".to_owned(),
                path: RepositoryPath::utf8("src/lib.rs".to_owned()).unwrap(),
                previous_path: None,
            }],
            recent_commits: vec![GitCommitSummary {
                id: "abc1234".to_owned(),
                summary: "add bounded context".to_owned(),
            }],
            worktrees: Vec::new(),
            managed_worktree: None,
            truncated: false,
            diagnostic: None,
        };
        let oversized_agents = format!("repository policy\n{}", "x".repeat(20_000));
        let repository = ContextPackRepository::from_sources(
            &git,
            vec![
                ContextPackRepositoryFileSource::utf8(
                    "AGENTS.md",
                    oversized_agents.clone(),
                    oversized_agents.len() as u32,
                ),
                ContextPackRepositoryFileSource::utf8(
                    "README.md",
                    "OPENAI_API_KEY=sk-live-secret-material-123456789".to_owned(),
                    49,
                ),
                ContextPackRepositoryFileSource::utf8(
                    "README",
                    r"local checkout C:\Users\owner\private-repo".to_owned(),
                    43,
                ),
                ContextPackRepositoryFileSource::skipped(
                    "Cargo.toml",
                    ContextPackSelectedFileSkipReason::NonUtf8,
                    Some(8),
                ),
            ],
        );
        let pack = NodeContextPack::export_with_repository(
            lineage(),
            &history,
            Some(repository),
        )
        .unwrap();
        let bytes = String::from_utf8(pack.bytes().to_vec()).unwrap();
        let document: ContextPackDocumentV1 = serde_json::from_str(&bytes).unwrap();
        let repository = document.repository.unwrap();

        assert!(!bytes.contains(r"C:\private\source-root"));
        assert!(!bytes.contains("sk-live-secret-material"));
        assert!(!bytes.contains(r"C:\Users\owner\private-repo"));
        assert!(!bytes.contains('\u{1b}'));
        assert_eq!(repository.branch.as_deref(), Some("main"));
        assert_eq!(repository.status[0].path, "src/lib.rs");
        assert_eq!(repository.recent_commits[0].summary, "add bounded context");
        let agents = repository
            .selected_files
            .iter()
            .find(|file| file.path == "AGENTS.md")
            .unwrap();
        assert_eq!(
            agents.content.as_ref().unwrap().len(),
            MAX_CONTEXT_PACK_SELECTED_FILE_BYTES,
        );
        assert!(agents.truncated);
        let readme = repository
            .selected_files
            .iter()
            .find(|file| file.path == "README.md")
            .unwrap();
        assert_eq!(
            readme.skipped,
            Some(ContextPackSelectedFileSkipReason::SecretLike),
        );
        assert!(readme.content.is_none());
        let readme_without_extension = repository
            .selected_files
            .iter()
            .find(|file| file.path == "README")
            .unwrap();
        assert_eq!(
            readme_without_extension.skipped,
            Some(ContextPackSelectedFileSkipReason::Unsafe),
        );
        assert!(readme_without_extension.content.is_none());
        assert!(repository.truncated);
        assert!(!pack.receipt().truncated);
        assert!(pack.receipt().is_valid());
        NodeContextPack::from_materialized(pack.receipt().clone(), pack.bytes().to_vec())
            .unwrap();
    }

    #[test]
    fn repository_git_metadata_omits_secrets_host_paths_and_colon_paths() {
        let history = HistorySessionRecord {
            session_id: "vendor-session".to_owned(),
            title: Some("review".to_owned()),
            cwd: None,
            model: Some("codex".to_owned()),
            message_count: 1,
            completed_turn_count: None,
            total_tokens: 0,
            messages: vec![HistoryMessageRecord {
                role: HistoryMessageRole::User,
                text: "explain the safe repository state".to_owned(),
            }],
        };
        let git = GitSnapshot {
            is_repository: true,
            branch: Some(r"feature/C:\Users\owner\private-repo".to_owned()),
            status: vec![
                GitStatusEntry {
                    index_status: "M".to_owned(),
                    worktree_status: " ".to_owned(),
                    path: RepositoryPath::utf8("src/lib.rs".to_owned()).unwrap(),
                    previous_path: None,
                },
                GitStatusEntry {
                    index_status: "?".to_owned(),
                    worktree_status: "?".to_owned(),
                    path: RepositoryPath::utf8(
                        "notes/OPENAI_API_KEY=sk-live-status-material-123456789".to_owned(),
                    )
                    .unwrap(),
                    previous_path: None,
                },
                GitStatusEntry {
                    index_status: "?".to_owned(),
                    worktree_status: "?".to_owned(),
                    path: RepositoryPath::utf8(r"C:/Users/owner/private-repo".to_owned())
                        .unwrap(),
                    previous_path: None,
                },
                GitStatusEntry {
                    index_status: "?".to_owned(),
                    worktree_status: "?".to_owned(),
                    path: RepositoryPath::utf8("notes:private.txt".to_owned()).unwrap(),
                    previous_path: None,
                },
                GitStatusEntry {
                    index_status: "?".to_owned(),
                    worktree_status: "?".to_owned(),
                    path: RepositoryPath::utf8(r"\\server\share\private.txt".to_owned())
                        .unwrap(),
                    previous_path: None,
                },
            ],
            recent_commits: vec![
                GitCommitSummary {
                    id: "abc1234".to_owned(),
                    summary: "add bounded context".to_owned(),
                },
                GitCommitSummary {
                    id: "def5678".to_owned(),
                    summary: "rotate OPENAI_API_KEY=sk-live-commit-material-123456789"
                        .to_owned(),
                },
                GitCommitSummary {
                    id: r"C:\private\commit-id".to_owned(),
                    summary: "unsafe id".to_owned(),
                },
                GitCommitSummary {
                    id: "fed4321".to_owned(),
                    summary: r"review C:\Users\owner\private-repo".to_owned(),
                },
            ],
            worktrees: Vec::new(),
            managed_worktree: None,
            truncated: false,
            diagnostic: None,
        };
        let repository = ContextPackRepository::from_sources(&git, Vec::new());
        let pack = NodeContextPack::export_with_repository(
            lineage(),
            &history,
            Some(repository),
        )
        .unwrap();
        let bytes = String::from_utf8(pack.bytes().to_vec()).unwrap();
        let document: ContextPackDocumentV1 = serde_json::from_str(&bytes).unwrap();
        let repository = document.repository.unwrap();

        assert!(repository.branch.is_none());
        assert_eq!(repository.status.len(), 1);
        assert_eq!(repository.status[0].path, "src/lib.rs");
        assert_eq!(repository.recent_commits.len(), 1);
        assert_eq!(repository.recent_commits[0].id, "abc1234");
        assert_eq!(repository.recent_commits[0].summary, "add bounded context");
        assert!(repository.truncated);
        assert!(!bytes.contains("sk-live-status-material"));
        assert!(!bytes.contains("sk-live-commit-material"));
        assert!(!bytes.contains(r"C:\Users\owner\private-repo"));
        assert!(!bytes.contains(r"C:/Users/owner/private-repo"));
        assert!(!bytes.contains("notes:private.txt"));
        assert!(!bytes.contains(r"\\server\share\private.txt"));
        assert!(!pack.receipt().truncated);
        assert!(pack.receipt().is_valid());
    }

    #[test]
    fn materialized_repository_rejects_sensitive_git_metadata() {
        let mut repository = ContextPackRepositoryV1 {
            is_repository: true,
            branch: Some("main".to_owned()),
            status: vec![ContextPackGitStatusV1 {
                index_status: "M".to_owned(),
                worktree_status: " ".to_owned(),
                path: "src/lib.rs".to_owned(),
                previous_path: None,
            }],
            recent_commits: vec![ContextPackGitCommitV1 {
                id: "abc1234".to_owned(),
                summary: "add bounded context".to_owned(),
            }],
            selected_files: Vec::new(),
            truncated: false,
        };
        assert_eq!(repository.validate(), Ok(()));

        repository.branch = Some(r"feature/C:\Users\owner\repo".to_owned());
        assert_eq!(repository.validate(), Err(()));
        repository.branch = Some("main".to_owned());

        repository.status[0].path = "notes:private.txt".to_owned();
        assert_eq!(repository.validate(), Err(()));
        repository.status[0].path = "src/lib.rs".to_owned();

        repository.recent_commits[0].summary =
            "rotate OPENAI_API_KEY=sk-live-commit-material-123456789".to_owned();
        assert_eq!(repository.validate(), Err(()));
        repository.recent_commits[0].summary = "add bounded context".to_owned();

        repository.recent_commits[0].id = r"\\?\C:\private\commit-id".to_owned();
        assert_eq!(repository.validate(), Err(()));
    }

    #[test]
    fn history_controls_and_ansi_are_normalized_without_losing_meaning() {
        let history = HistorySessionRecord {
            session_id: "vendor-session".to_owned(),
            title: Some("\u{1b}[32mreview\u{1b}[0m".to_owned()),
            cwd: Some(r"C:\private\repo".to_owned()),
            model: Some("codex".to_owned()),
            message_count: 1,
            completed_turn_count: None,
            total_tokens: 12,
            messages: vec![HistoryMessageRecord {
                role: HistoryMessageRole::Assistant,
                text: "\u{1b}[31msemantic result\u{1b}[0m\r\nkept\u{0}".to_owned(),
            }],
        };
        let pack = NodeContextPack::export(lineage(), &history).unwrap();
        let document: ContextPackDocumentV1 = serde_json::from_slice(pack.bytes()).unwrap();

        assert_eq!(
            document.retained_messages[0].text,
            "semantic result\nkept",
        );
        let encoded = String::from_utf8(pack.bytes().to_vec()).unwrap();
        assert!(!encoded.contains("title"));
        assert!(!encoded.contains("model"));
        assert!(!encoded.contains("history_session_id"));
        assert!(!pack.receipt().truncated);
        assert!(pack.receipt().is_valid());
        assert!(!String::from_utf8_lossy(pack.bytes()).contains(r"C:\private\repo"));
    }

    #[test]
    fn absolute_windows_host_path_detection_does_not_confuse_https_urls() {
        assert!(contains_absolute_windows_host_path(r"C:\Users\owner\repo"));
        assert!(contains_absolute_windows_host_path(r"\\server\share\repo"));
        assert!(contains_absolute_windows_host_path(r"\\?\C:\repo"));
        assert!(contains_absolute_windows_host_path("checkout D:/private/repo"));
        assert!(!contains_absolute_windows_host_path("https://example.invalid/repo"));
        assert!(!contains_absolute_windows_host_path("ordinary repository text"));
    }
}
