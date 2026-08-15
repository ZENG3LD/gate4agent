use crate::{
    metadata_limit, sqlite, transcript_limit, CandidateLocator, CandidateRecord,
    ComponentSignature, LoadedDocument, NativeHistoryError, NativeHistoryLimits,
    HISTORY_AUXILIARY_INDEX_MAX_BYTES,
};
use gate4agent_adapters::{HistoryDocument, HistorySourceLayout};
use serde_json::{Map, Value};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

const HISTORY_LOCATOR_PREFIX_MAX_BYTES: usize = 64 * 1024;
const HISTORY_PREVIEW_TAIL_MAX_BYTES: usize = 512 * 1024;

pub(crate) fn load_preview_document(
    record: &CandidateRecord,
    limits: NativeHistoryLimits,
) -> Result<(HistoryDocument, bool), NativeHistoryError> {
    let CandidateLocator::File {
        root,
        primary,
        layout,
    } = &record.key.locator
    else {
        return load_document(record, limits).map(|loaded| (loaded.document, false));
    };
    let adapter = record.key.source.binding.id.as_str();
    let mut signatures = Vec::new();
    let mut document = HistoryDocument {
        session_id_hint: record.session_id_hint.clone(),
        metadata_json: None,
        transcript: String::new(),
    };
    let mut source_truncated = false;
    match (adapter, layout) {
        ("claude-code" | "qwen-code", HistorySourceLayout::SingleNdjson) => {
            let source_generation = required_file_signature(primary)?;
            let locator = load_ndjson_locator_document(
                adapter,
                root,
                primary,
                &record.session_id_hint,
            )?;
            let (tail, truncated) = read_tail(root, primary, HISTORY_PREVIEW_TAIL_MAX_BYTES)?;
            document.transcript = format!(
                "{}\n{}",
                locator.transcript.trim_end(),
                tail.trim_start(),
            );
            source_truncated = truncated;
            ensure_generation(primary, &source_generation)?;
        }
        ("codex", HistorySourceLayout::NdjsonWithOptionalIndex) => {
            let source_generation = required_file_signature(primary)?;
            let locator = load_ndjson_locator_document(
                adapter,
                root,
                primary,
                &record.session_id_hint,
            )?;
            let (tail, truncated) = read_tail(root, primary, HISTORY_PREVIEW_TAIL_MAX_BYTES)?;
            document.transcript = format!(
                "{}\n{}",
                locator.transcript.trim_end(),
                tail.trim_start(),
            );
            source_truncated = truncated;
            document.metadata_json = load_codex_index(
                root,
                &document.transcript,
                &mut signatures,
            )?;
            ensure_generation(primary, &source_generation)?;
        }
        ("grok", HistorySourceLayout::SummaryJsonWithSiblingNdjson) => {
            document.metadata_json = Some(read_required(
                root,
                primary,
                primary_limit(*layout),
                &mut signatures,
            )?);
            let sibling = primary.with_file_name("chat_history.jsonl");
            if sibling.is_file() {
                (document.transcript, source_truncated) =
                    read_tail(root, &sibling, HISTORY_PREVIEW_TAIL_MAX_BYTES)?;
            }
        }
        ("kimi", HistorySourceLayout::StateJsonWithIndexAndSiblingNdjson) => {
            let state_content = read_required(
                root,
                primary,
                primary_limit(*layout),
                &mut signatures,
            )?;
            let metadata = load_kimi_locator_metadata(
                root,
                state_content,
                &record.session_id_hint,
                &mut signatures,
            )?;
            let state = serde_json::from_str::<Value>(&metadata)
                .ok()
                .and_then(|value| value.as_object().cloned())
                .ok_or(NativeHistoryError::ReadFailed)?;
            let primary_agent = kimi_primary_agent_id(&state);
            if !safe_component(&primary_agent) {
                return Err(NativeHistoryError::SourceChanged);
            }
            let session_dir = primary.parent().ok_or(NativeHistoryError::SourceChanged)?;
            let wire_path = session_dir
                .join("agents")
                .join(primary_agent)
                .join("wire.jsonl");
            if wire_path.is_file() {
                (document.transcript, source_truncated) = read_tail(
                    root,
                    &wire_path,
                    HISTORY_PREVIEW_TAIL_MAX_BYTES,
                )?;
            }
            document.metadata_json = Some(metadata);
        }
        _ => return load_document(record, limits).map(|loaded| (loaded.document, false)),
    }
    Ok((document, source_truncated))
}

pub(crate) fn load_locator_document(
    record: &CandidateRecord,
    limits: NativeHistoryLimits,
) -> Result<HistoryDocument, NativeHistoryError> {
    match &record.key.locator {
        CandidateLocator::File {
            root,
            primary,
            layout,
        } if matches!(
            layout,
            HistorySourceLayout::SingleJson
                | HistorySourceLayout::SessionJsonWithSiblingMessageJson
        ) => load_document(record, limits).map(|loaded| loaded.document),
        CandidateLocator::File {
            root,
            primary,
            layout,
        } => load_file_locator(
            record.key.source.binding.id.as_str(),
            root,
            primary,
            *layout,
            &record.session_id_hint,
        ),
        CandidateLocator::Sqlite { .. } => {
            load_document(record, limits).map(|loaded| loaded.document)
        }
    }
}

fn load_file_locator(
    adapter: &str,
    root: &Path,
    primary: &Path,
    layout: HistorySourceLayout,
    session_id_hint: &str,
) -> Result<HistoryDocument, NativeHistoryError> {
    let mut signatures = Vec::new();
    let mut document = HistoryDocument {
        session_id_hint: session_id_hint.to_owned(),
        metadata_json: None,
        transcript: String::new(),
    };
    match layout {
        HistorySourceLayout::SingleNdjson
        | HistorySourceLayout::NdjsonWithOptionalIndex
        | HistorySourceLayout::JsonOrNdjson => {
            document = load_ndjson_locator_document(
                adapter,
                root,
                primary,
                session_id_hint,
            )?;
        }
        HistorySourceLayout::SummaryJsonWithSiblingNdjson
        | HistorySourceLayout::MetadataJsonWithSiblingJson => {
            document.metadata_json = Some(read_required(
                root,
                primary,
                primary_limit(layout),
                &mut signatures,
            )?);
        }
        HistorySourceLayout::StateJsonWithIndexAndSiblingNdjson => {
            let state_content = read_required(
                root,
                primary,
                primary_limit(layout),
                &mut signatures,
            )?;
            document.metadata_json = Some(load_kimi_locator_metadata(
                root,
                state_content,
                session_id_hint,
                &mut signatures,
            )?);
        }
        HistorySourceLayout::SingleJson
        | HistorySourceLayout::SessionJsonWithSiblingMessageJson
        | HistorySourceLayout::ReadOnlySqliteProjection => {
            return Err(NativeHistoryError::SourceChanged);
        }
    }
    Ok(document)
}

fn load_ndjson_locator_document(
    adapter: &str,
    root: &Path,
    primary: &Path,
    session_id_hint: &str,
) -> Result<HistoryDocument, NativeHistoryError> {
    let source_generation = required_file_signature(primary)?;
    let prefix = read_raw_prefix(root, primary, HISTORY_LOCATOR_PREFIX_MAX_BYTES)?;
    let id_key = if adapter == "codex" { "id" } else { "sessionId" };
    let mut cwd = extract_json_string_field(&prefix, "cwd");
    let mut session_id = extract_json_string_field(&prefix, id_key);
    if cwd.is_none() || session_id.is_none() {
        let tail = read_raw_tail(root, primary, HISTORY_LOCATOR_PREFIX_MAX_BYTES)?;
        cwd = cwd.or_else(|| extract_json_string_field(&tail, "cwd"));
        session_id = session_id.or_else(|| extract_json_string_field(&tail, id_key));
    }
    ensure_generation(primary, &source_generation)?;
    let session_id = session_id.unwrap_or_else(|| session_id_hint.to_owned());
    let record = match adapter {
        "codex" => serde_json::json!({
            "type": "session_meta",
            "payload": { "id": session_id, "cwd": cwd },
        }),
        "qwen-code" => serde_json::json!({
            "uuid": "gate4agent-locator",
            "parentUuid": null,
            "sessionId": session_id,
            "type": "user",
            "provenance": "system",
            "cwd": cwd,
        }),
        _ => serde_json::json!({
            "type": "progress",
            "sessionId": session_id,
            "cwd": cwd,
        }),
    };
    Ok(HistoryDocument {
        session_id_hint: session_id_hint.to_owned(),
        metadata_json: None,
        transcript: record.to_string(),
    })
}

fn extract_json_string_field(content: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let mut rest = content;
    while let Some(position) = rest.find(&needle) {
        let after_key = &rest[position + needle.len()..];
        let value = after_key.trim_start().strip_prefix(':')?.trim_start();
        if !value.starts_with('"') {
            rest = after_key;
            continue;
        }
        let mut escaped = false;
        for (index, character) in value[1..].char_indices() {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                return serde_json::from_str::<String>(&value[..index + 2]).ok();
            }
        }
        return None;
    }
    None
}

fn load_kimi_locator_metadata(
    sessions_root: &Path,
    state_content: String,
    session_id: &str,
    signatures: &mut Vec<ComponentSignature>,
) -> Result<String, NativeHistoryError> {
    let mut state = serde_json::from_str::<Value>(&state_content)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .ok_or(NativeHistoryError::ReadFailed)?;
    let home = sessions_root
        .parent()
        .ok_or(NativeHistoryError::SourceChanged)?;
    let index_path = home.join("session_index.jsonl");
    if let Some(index) = read_optional(
        home,
        &index_path,
        HISTORY_AUXILIARY_INDEX_MAX_BYTES,
        signatures,
    )? {
        if let Some(work_dir) = index
            .lines()
            .filter_map(json_object)
            .filter_map(|record| {
                let id = record.get("sessionId")?.as_str()?.trim();
                if id != session_id {
                    return None;
                }
                Some(record.get("workDir")?.as_str()?.trim().to_owned())
            })
            .rfind(|work_dir| !work_dir.is_empty())
        {
            state.insert("cwd".to_owned(), Value::String(work_dir));
        }
    }
    Ok(Value::Object(state).to_string())
}

fn read_raw_prefix(
    authorized_root: &Path,
    path: &Path,
    max: usize,
) -> Result<String, NativeHistoryError> {
    validate_root(authorized_root)?;
    let metadata = fs::symlink_metadata(path).map_err(|_| NativeHistoryError::SourceChanged)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(NativeHistoryError::SourceChanged);
    }
    let canonical = fs::canonicalize(path).map_err(|_| NativeHistoryError::SourceChanged)?;
    if !canonical.starts_with(authorized_root) {
        return Err(NativeHistoryError::SourceChanged);
    }
    let file = File::open(&canonical).map_err(|_| NativeHistoryError::ReadFailed)?;
    let mut bytes = Vec::with_capacity(max.min(metadata.len() as usize));
    file.take((max as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| NativeHistoryError::ReadFailed)?;
    bytes.truncate(max);
    String::from_utf8(bytes).map_err(|_| NativeHistoryError::InvalidUtf8)
}

fn read_raw_tail(
    authorized_root: &Path,
    path: &Path,
    max: usize,
) -> Result<String, NativeHistoryError> {
    validate_root(authorized_root)?;
    let before = required_file_signature(path)?;
    let canonical = fs::canonicalize(path).map_err(|_| NativeHistoryError::SourceChanged)?;
    if !canonical.starts_with(authorized_root) {
        return Err(NativeHistoryError::SourceChanged);
    }
    let mut file = File::open(&canonical).map_err(|_| NativeHistoryError::ReadFailed)?;
    let len = file
        .metadata()
        .map_err(|_| NativeHistoryError::ReadFailed)?
        .len();
    let start = len.saturating_sub(max as u64);
    file.seek(SeekFrom::Start(start))
        .map_err(|_| NativeHistoryError::ReadFailed)?;
    let mut bytes = Vec::with_capacity((len - start) as usize);
    file.read_to_end(&mut bytes)
        .map_err(|_| NativeHistoryError::ReadFailed)?;
    ensure_generation(path, &before)?;
    String::from_utf8(bytes).map_err(|_| NativeHistoryError::InvalidUtf8)
}

fn required_file_signature(path: &Path) -> Result<ComponentSignature, NativeHistoryError> {
    let signature = component_signature(path)?;
    if !signature.present || signature.directory {
        return Err(NativeHistoryError::SourceChanged);
    }
    Ok(signature)
}

fn ensure_generation(
    path: &Path,
    expected: &ComponentSignature,
) -> Result<(), NativeHistoryError> {
    if component_signature(path)? != *expected {
        return Err(NativeHistoryError::SourceChanged);
    }
    Ok(())
}

fn read_tail(
    authorized_root: &Path,
    path: &Path,
    max: usize,
) -> Result<(String, bool), NativeHistoryError> {
    validate_root(authorized_root)?;
    let before = required_file_signature(path)?;
    let canonical = fs::canonicalize(path).map_err(|_| NativeHistoryError::SourceChanged)?;
    if !canonical.starts_with(authorized_root) {
        return Err(NativeHistoryError::SourceChanged);
    }
    let mut file = File::open(&canonical).map_err(|_| NativeHistoryError::ReadFailed)?;
    let len = file
        .metadata()
        .map_err(|_| NativeHistoryError::ReadFailed)?
        .len();
    let start = len.saturating_sub(max as u64);
    file.seek(SeekFrom::Start(start))
        .map_err(|_| NativeHistoryError::ReadFailed)?;
    let mut bytes = Vec::with_capacity((len - start) as usize);
    file.read_to_end(&mut bytes)
        .map_err(|_| NativeHistoryError::ReadFailed)?;
    if start > 0 {
        let Some(first_newline) = bytes.iter().position(|byte| *byte == b'\n') else {
            return Err(NativeHistoryError::ReadFailed);
        };
        bytes.drain(..=first_newline);
    }
    ensure_generation(path, &before)?;
    let content = String::from_utf8(bytes).map_err(|_| NativeHistoryError::InvalidUtf8)?;
    Ok((content, start > 0))
}

pub(crate) fn load_document(
    record: &CandidateRecord,
    limits: NativeHistoryLimits,
) -> Result<LoadedDocument, NativeHistoryError> {
    match &record.key.locator {
        CandidateLocator::File {
            root,
            primary,
            layout,
        } => load_file_layout(root, primary, *layout, &record.session_id_hint, limits),
        CandidateLocator::Sqlite {
            database,
            session_id,
        } => {
            validate_database(database)?;
            let document = sqlite::load_session(database, session_id)?;
            let signatures = sqlite_component_paths(database)
                .into_iter()
                .map(|path| component_signature(&path))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(LoadedDocument {
                document,
                signatures,
            })
        }
    }
}

pub(crate) fn signatures_are_current(signatures: &[ComponentSignature]) -> bool {
    signatures.iter().all(|expected| {
        component_signature(&expected.path).is_ok_and(|current| current == *expected)
    })
}

pub(crate) fn validated_resume_file(
    authorized_root: &Path,
    path: &Path,
) -> Result<PathBuf, NativeHistoryError> {
    validate_root(authorized_root)?;
    let metadata = fs::symlink_metadata(path).map_err(|_| NativeHistoryError::SourceChanged)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(NativeHistoryError::SourceChanged);
    }
    let canonical = fs::canonicalize(path).map_err(|_| NativeHistoryError::SourceChanged)?;
    if !canonical.starts_with(authorized_root) {
        return Err(NativeHistoryError::SourceChanged);
    }
    Ok(canonical)
}

fn load_file_layout(
    root: &Path,
    primary: &Path,
    layout: HistorySourceLayout,
    session_id_hint: &str,
    limits: NativeHistoryLimits,
) -> Result<LoadedDocument, NativeHistoryError> {
    validate_root(root)?;
    let mut signatures = Vec::new();
    let primary_content = read_required(root, primary, primary_limit(layout), &mut signatures)?;
    let mut document = HistoryDocument {
        session_id_hint: session_id_hint.to_owned(),
        metadata_json: None,
        transcript: String::new(),
    };
    match layout {
        HistorySourceLayout::SingleNdjson
        | HistorySourceLayout::SingleJson
        | HistorySourceLayout::JsonOrNdjson => {
            document.transcript = primary_content;
        }
        HistorySourceLayout::NdjsonWithOptionalIndex => {
            document.transcript = primary_content;
            document.metadata_json = load_codex_index(root, &document.transcript, &mut signatures)?;
        }
        HistorySourceLayout::SummaryJsonWithSiblingNdjson => {
            document.metadata_json = Some(primary_content);
            let sibling = primary.with_file_name("chat_history.jsonl");
            document.transcript =
                read_optional(root, &sibling, transcript_limit(), &mut signatures)?
                    .unwrap_or_default();
        }
        HistorySourceLayout::MetadataJsonWithSiblingJson => {
            document.metadata_json = Some(primary_content);
            let sibling = primary.with_file_name("session_context.json");
            document.transcript =
                read_optional(root, &sibling, transcript_limit(), &mut signatures)?
                    .unwrap_or_else(|| "{}".to_owned());
        }
        HistorySourceLayout::SessionJsonWithSiblingMessageJson => {
            document.metadata_json = Some(primary_content);
            document.transcript = load_opencode_messages(
                root,
                primary,
                document.metadata_json.as_deref().unwrap_or("{}"),
                session_id_hint,
                limits,
                &mut signatures,
            )?;
        }
        HistorySourceLayout::StateJsonWithIndexAndSiblingNdjson => {
            let (metadata, transcript) = load_kimi(
                root,
                primary,
                primary_content,
                session_id_hint,
                &mut signatures,
            )?;
            document.metadata_json = Some(metadata);
            document.transcript = transcript;
        }
        HistorySourceLayout::ReadOnlySqliteProjection => {
            return Err(NativeHistoryError::SourceChanged);
        }
    }
    Ok(LoadedDocument {
        document,
        signatures,
    })
}

fn load_codex_index(
    sessions_root: &Path,
    transcript: &str,
    signatures: &mut Vec<ComponentSignature>,
) -> Result<Option<String>, NativeHistoryError> {
    let session_id = transcript.lines().find_map(|line| {
        let value = serde_json::from_str::<Value>(line).ok()?;
        if value.get("type")?.as_str()? != "session_meta" {
            return None;
        }
        Some(value.get("payload")?.get("id")?.as_str()?.trim().to_owned())
    });
    let Some(session_id) = session_id.filter(|id| !id.is_empty()) else {
        return Ok(None);
    };
    let home = sessions_root
        .parent()
        .ok_or(NativeHistoryError::SourceChanged)?;
    let index_path = home.join("session_index.jsonl");
    let Some(index) = read_optional(
        home,
        &index_path,
        HISTORY_AUXILIARY_INDEX_MAX_BYTES,
        signatures,
    )?
    else {
        return Ok(None);
    };
    let title = index
        .lines()
        .filter_map(json_object)
        .filter_map(|record| {
            let id = record.get("id")?.as_str()?.trim();
            if id != session_id {
                return None;
            }
            Some(record.get("thread_name")?.as_str()?.trim().to_owned())
        })
        .rfind(|title| !title.is_empty());
    Ok(title.map(|title| serde_json::json!({ "indexed_title": title }).to_string()))
}

fn load_opencode_messages(
    storage_root: &Path,
    primary: &Path,
    metadata: &str,
    fallback_session_id: &str,
    limits: NativeHistoryLimits,
    signatures: &mut Vec<ComponentSignature>,
) -> Result<String, NativeHistoryError> {
    let session_id = serde_json::from_str::<Value>(metadata)
        .ok()
        .and_then(|value| value.get("id")?.as_str().map(str::trim).map(str::to_owned))
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| fallback_session_id.to_owned());
    if !safe_component(&session_id) {
        return Err(NativeHistoryError::SourceChanged);
    }
    if !primary.starts_with(storage_root.join("session")) {
        return Err(NativeHistoryError::SourceChanged);
    }
    let message_dir = storage_root.join("message").join(&session_id);
    let directory_before = component_signature(&message_dir)?;
    if !directory_before.present {
        signatures.push(directory_before);
        return Ok(String::new());
    }
    validate_directory(storage_root, &message_dir)?;
    let entries = fs::read_dir(&message_dir).map_err(|_| NativeHistoryError::ReadFailed)?;
    let mut files = entries
        .flatten()
        .filter_map(|entry| {
            let file_type = entry.file_type().ok()?;
            let path = entry.path();
            (file_type.is_file()
                && !file_type.is_symlink()
                && path.extension().and_then(|value| value.to_str()) == Some("json"))
            .then_some(path)
        })
        .collect::<Vec<_>>();
    files.sort();
    files.truncate(limits.max_sibling_files);
    let mut transcript = String::new();
    for path in files {
        let remaining = transcript_limit().saturating_sub(transcript.len());
        if remaining == 0 {
            return Err(NativeHistoryError::FileTooLarge {
                max: transcript_limit(),
            });
        }
        let content = read_required(storage_root, &path, remaining, signatures)?;
        if !transcript.is_empty() {
            transcript.push('\n');
        }
        transcript.push_str(content.trim());
        if transcript.len() > transcript_limit() {
            return Err(NativeHistoryError::FileTooLarge {
                max: transcript_limit(),
            });
        }
    }
    let directory_after = component_signature(&message_dir)?;
    if directory_before != directory_after {
        return Err(NativeHistoryError::SourceChanged);
    }
    signatures.push(directory_after);
    Ok(transcript)
}

fn load_kimi(
    sessions_root: &Path,
    primary: &Path,
    state_content: String,
    session_id: &str,
    signatures: &mut Vec<ComponentSignature>,
) -> Result<(String, String), NativeHistoryError> {
    let mut state = serde_json::from_str::<Value>(&state_content)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .ok_or(NativeHistoryError::ReadFailed)?;
    let home = sessions_root
        .parent()
        .ok_or(NativeHistoryError::SourceChanged)?;
    let index_path = home.join("session_index.jsonl");
    if let Some(index) = read_optional(
        home,
        &index_path,
        HISTORY_AUXILIARY_INDEX_MAX_BYTES,
        signatures,
    )? {
        if let Some(work_dir) = index
            .lines()
            .filter_map(json_object)
            .filter_map(|record| {
                let id = record.get("sessionId")?.as_str()?.trim();
                if id != session_id {
                    return None;
                }
                Some(record.get("workDir")?.as_str()?.trim().to_owned())
            })
            .rfind(|work_dir| !work_dir.is_empty())
        {
            state.insert("cwd".to_owned(), Value::String(work_dir));
        }
    }
    let primary_agent = kimi_primary_agent_id(&state);
    if !safe_component(&primary_agent) {
        return Err(NativeHistoryError::SourceChanged);
    }
    let session_dir = primary.parent().ok_or(NativeHistoryError::SourceChanged)?;
    let wire_path = session_dir
        .join("agents")
        .join(primary_agent)
        .join("wire.jsonl");
    let transcript = read_optional(sessions_root, &wire_path, transcript_limit(), signatures)?
        .unwrap_or_default();
    Ok((Value::Object(state).to_string(), transcript))
}

fn kimi_primary_agent_id(state: &Map<String, Value>) -> String {
    state
        .get("agents")
        .and_then(Value::as_object)
        .and_then(|agents| {
            agents.iter().find_map(|(id, value)| {
                let record = value.as_object()?;
                (record.get("type").and_then(Value::as_str) == Some("main")
                    && record.get("parentAgentId").is_none_or(Value::is_null))
                .then(|| id.clone())
            })
        })
        .unwrap_or_else(|| "main".to_owned())
}

fn read_required(
    authorized_root: &Path,
    path: &Path,
    max: usize,
    signatures: &mut Vec<ComponentSignature>,
) -> Result<String, NativeHistoryError> {
    read_component(authorized_root, path, max, true, signatures)?
        .ok_or(NativeHistoryError::SourceChanged)
}

fn read_optional(
    authorized_root: &Path,
    path: &Path,
    max: usize,
    signatures: &mut Vec<ComponentSignature>,
) -> Result<Option<String>, NativeHistoryError> {
    read_component(authorized_root, path, max, false, signatures)
}

fn read_component(
    authorized_root: &Path,
    path: &Path,
    max: usize,
    required: bool,
    signatures: &mut Vec<ComponentSignature>,
) -> Result<Option<String>, NativeHistoryError> {
    validate_root(authorized_root)?;
    let before = component_signature(path)?;
    if !before.present {
        signatures.push(before);
        return if required {
            Err(NativeHistoryError::SourceChanged)
        } else {
            Ok(None)
        };
    }
    if before.directory {
        return Err(NativeHistoryError::SourceChanged);
    }
    let metadata = fs::symlink_metadata(path).map_err(|_| NativeHistoryError::SourceChanged)?;
    if metadata.file_type().is_symlink() {
        return Err(NativeHistoryError::SourceChanged);
    }
    let canonical = fs::canonicalize(path).map_err(|_| NativeHistoryError::SourceChanged)?;
    if !canonical.starts_with(authorized_root) {
        return Err(NativeHistoryError::SourceChanged);
    }
    if metadata.len() > u64::try_from(max).unwrap_or(u64::MAX) {
        return Err(NativeHistoryError::FileTooLarge { max });
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0).min(max));
    File::open(&canonical)
        .map_err(|_| NativeHistoryError::ReadFailed)?
        .take(u64::try_from(max).unwrap_or(u64::MAX).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| NativeHistoryError::ReadFailed)?;
    if bytes.len() > max {
        return Err(NativeHistoryError::FileTooLarge { max });
    }
    let after = component_signature(&canonical)?;
    if before != after {
        return Err(NativeHistoryError::SourceChanged);
    }
    signatures.push(after);
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| NativeHistoryError::InvalidUtf8)
}

fn validate_root(root: &Path) -> Result<(), NativeHistoryError> {
    let metadata = fs::symlink_metadata(root).map_err(|_| NativeHistoryError::SourceChanged)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(NativeHistoryError::SourceChanged);
    }
    let canonical = fs::canonicalize(root).map_err(|_| NativeHistoryError::SourceChanged)?;
    if canonical != root {
        return Err(NativeHistoryError::SourceChanged);
    }
    Ok(())
}

fn validate_directory(root: &Path, path: &Path) -> Result<(), NativeHistoryError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| NativeHistoryError::SourceChanged)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(NativeHistoryError::SourceChanged);
    }
    let canonical = fs::canonicalize(path).map_err(|_| NativeHistoryError::SourceChanged)?;
    if !canonical.starts_with(root) {
        return Err(NativeHistoryError::SourceChanged);
    }
    Ok(())
}

fn validate_database(database: &Path) -> Result<(), NativeHistoryError> {
    let metadata = fs::symlink_metadata(database).map_err(|_| NativeHistoryError::SourceChanged)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(NativeHistoryError::SourceChanged);
    }
    if fs::canonicalize(database).map_err(|_| NativeHistoryError::SourceChanged)? != database {
        return Err(NativeHistoryError::SourceChanged);
    }
    Ok(())
}

fn component_signature(path: &Path) -> Result<ComponentSignature, NativeHistoryError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ComponentSignature {
                path: path.to_owned(),
                present: false,
                directory: false,
                len: 0,
                modified_nanos: 0,
            });
        }
        Err(_) => return Err(NativeHistoryError::ReadFailed),
    };
    if metadata.file_type().is_symlink() {
        return Err(NativeHistoryError::SourceChanged);
    }
    let modified_nanos = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    Ok(ComponentSignature {
        path: path.to_owned(),
        present: true,
        directory: metadata.is_dir(),
        len: metadata.len(),
        modified_nanos,
    })
}

fn sqlite_component_paths(database: &Path) -> [PathBuf; 3] {
    let mut wal = database.as_os_str().to_owned();
    wal.push("-wal");
    let mut shm = database.as_os_str().to_owned();
    shm.push("-shm");
    [database.to_owned(), PathBuf::from(wal), PathBuf::from(shm)]
}

fn primary_limit(layout: HistorySourceLayout) -> usize {
    match layout {
        HistorySourceLayout::SummaryJsonWithSiblingNdjson
        | HistorySourceLayout::MetadataJsonWithSiblingJson
        | HistorySourceLayout::SessionJsonWithSiblingMessageJson
        | HistorySourceLayout::StateJsonWithIndexAndSiblingNdjson => metadata_limit(),
        _ => transcript_limit(),
    }
}

fn safe_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn json_object(line: &str) -> Option<Map<String, Value>> {
    serde_json::from_str::<Value>(line.trim())
        .ok()
        .and_then(|value| value.as_object().cloned())
}
