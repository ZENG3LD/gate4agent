use gate4agent_types::AdapterId;
use serde_json::{Map, Value};
use std::collections::{HashSet, VecDeque};
use thiserror::Error;

pub const HISTORY_METADATA_MAX_BYTES: usize = 1_048_576;
pub const HISTORY_DOCUMENT_MAX_BYTES: usize = 8_388_608;
pub const HISTORY_STORED_MESSAGES_MAX: usize = 256;
pub const HISTORY_MESSAGE_MAX_CHARS: usize = 4_096;
const HISTORY_TITLE_MAX_CHARS: usize = 96;
const HISTORY_PROVIDER_MESSAGE_ID_MAX_BYTES: usize = 512;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HistoryDocument {
    /// Required fallback because the pure adapter has no filesystem path from
    /// which to infer the provider session ID.
    pub session_id_hint: String,
    /// Summary/state JSON for providers that split metadata and transcript.
    pub metadata_json: Option<String>,
    /// Provider transcript content, usually NDJSON.
    pub transcript: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistorySession {
    pub session_id: String,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub message_count: u64,
    pub completed_turn_count: Option<u64>,
    pub total_tokens: u64,
    pub total_tokens_observed: bool,
    pub messages: Vec<HistoryMessage>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryMessage {
    pub role: HistoryRole,
    pub text: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HistoryRole {
    User,
    Assistant,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum HistorySourceLayout {
    SingleNdjson,
    SingleJson,
    JsonOrNdjson,
    NdjsonWithOptionalIndex,
    SummaryJsonWithSiblingNdjson,
    MetadataJsonWithSiblingJson,
    SessionJsonWithSiblingMessageJson,
    ReadOnlySqliteProjection,
    StateJsonWithIndexAndSiblingNdjson,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HistorySourceVariant {
    pub layout: HistorySourceLayout,
    pub requires_sibling_reads: bool,
    pub requires_auxiliary_index: bool,
    pub requires_readonly_database: bool,
}

/// Declares the provider-owned source layouts an effect-owning history shell
/// may project into [`HistoryDocument`]. The pure adapter never opens paths or
/// databases; callers must bound and authorize every candidate and related
/// read before supplying metadata/transcript content.
pub fn history_source_variants(
    adapter_id: &AdapterId,
) -> Result<&'static [HistorySourceVariant], HistoryAdapterError> {
    const NDJSON: HistorySourceVariant = HistorySourceVariant {
        layout: HistorySourceLayout::SingleNdjson,
        requires_sibling_reads: false,
        requires_auxiliary_index: false,
        requires_readonly_database: false,
    };
    const JSON: HistorySourceVariant = HistorySourceVariant {
        layout: HistorySourceLayout::SingleJson,
        requires_sibling_reads: false,
        requires_auxiliary_index: false,
        requires_readonly_database: false,
    };
    const JSON_OR_NDJSON: HistorySourceVariant = HistorySourceVariant {
        layout: HistorySourceLayout::JsonOrNdjson,
        requires_sibling_reads: false,
        requires_auxiliary_index: false,
        requires_readonly_database: false,
    };
    const CODEX: HistorySourceVariant = HistorySourceVariant {
        layout: HistorySourceLayout::NdjsonWithOptionalIndex,
        requires_sibling_reads: false,
        requires_auxiliary_index: true,
        requires_readonly_database: false,
    };
    const GROK: HistorySourceVariant = HistorySourceVariant {
        layout: HistorySourceLayout::SummaryJsonWithSiblingNdjson,
        requires_sibling_reads: true,
        requires_auxiliary_index: false,
        requires_readonly_database: false,
    };
    const ROVO: HistorySourceVariant = HistorySourceVariant {
        layout: HistorySourceLayout::MetadataJsonWithSiblingJson,
        requires_sibling_reads: true,
        requires_auxiliary_index: false,
        requires_readonly_database: false,
    };
    const OPENCODE_FILES: HistorySourceVariant = HistorySourceVariant {
        layout: HistorySourceLayout::SessionJsonWithSiblingMessageJson,
        requires_sibling_reads: true,
        requires_auxiliary_index: false,
        requires_readonly_database: false,
    };
    const OPENCODE_SQLITE: HistorySourceVariant = HistorySourceVariant {
        layout: HistorySourceLayout::ReadOnlySqliteProjection,
        requires_sibling_reads: false,
        requires_auxiliary_index: false,
        requires_readonly_database: true,
    };
    const KIMI: HistorySourceVariant = HistorySourceVariant {
        layout: HistorySourceLayout::StateJsonWithIndexAndSiblingNdjson,
        requires_sibling_reads: true,
        requires_auxiliary_index: true,
        requires_readonly_database: false,
    };

    match adapter_id.as_str() {
        "claude-code" | "copilot" | "cursor" | "openclaw" | "pi" | "omp" | "droid"
        | "antigravity" | "qwen-code" => Ok(&[NDJSON]),
        "codex" => Ok(&[CODEX]),
        "gemini" => Ok(&[JSON_OR_NDJSON]),
        "hermes" | "devin" => Ok(&[JSON]),
        "grok" => Ok(&[GROK]),
        "rovo" => Ok(&[ROVO]),
        "opencode" => Ok(&[OPENCODE_FILES, OPENCODE_SQLITE]),
        "kimi" => Ok(&[KIMI]),
        id => Err(HistoryAdapterError::UnsupportedAdapter(id.to_owned())),
    }
}

pub fn parse_history(
    adapter_id: &AdapterId,
    document: &HistoryDocument,
) -> Result<HistorySession, HistoryAdapterError> {
    validate_document(document)?;
    match adapter_id.as_str() {
        "claude-code" => parse_claude(document),
        "codex" => parse_codex(document),
        "gemini" => parse_gemini(document),
        "antigravity" => parse_antigravity(document),
        "opencode" => parse_opencode(document),
        "hermes" => parse_hermes(document),
        "rovo" => parse_rovo(document),
        "openclaw" | "pi" | "omp" => parse_message_graph(document),
        "devin" => parse_devin(document),
        "grok" => parse_grok(document),
        "kimi" => parse_kimi(document),
        "qwen-code" => parse_qwen(document),
        "copilot" => parse_copilot(document),
        "droid" => parse_droid(document),
        "cursor" => parse_cursor(document),
        id => Err(HistoryAdapterError::UnsupportedAdapter(id.to_owned())),
    }
}

fn validate_document(document: &HistoryDocument) -> Result<(), HistoryAdapterError> {
    if document.transcript.len() > HISTORY_DOCUMENT_MAX_BYTES {
        return Err(HistoryAdapterError::TranscriptTooLarge);
    }
    if document
        .metadata_json
        .as_ref()
        .is_some_and(|metadata| metadata.len() > HISTORY_METADATA_MAX_BYTES)
    {
        return Err(HistoryAdapterError::MetadataTooLarge);
    }
    validate_session_id(&document.session_id_hint)
}

fn parse_claude(document: &HistoryDocument) -> Result<HistorySession, HistoryAdapterError> {
    let mut session = SessionBuilder::new(document.session_id_hint.trim().to_owned());
    let mut completed_turn_ids = HashSet::new();
    let mut custom_title = None;
    let mut generated_title = None;
    let mut first_user_title = None;
    let mut meta_title = None;

    for record in ndjson_records(&document.transcript) {
        if let Some(id) = string(&record, &["sessionId"]) {
            session.session_id = select_session_id(Some(id), &session.session_id)?;
        }
        session.cwd = string(&record, &["cwd"]).or(session.cwd);
        match record.get("type").and_then(Value::as_str) {
            Some("custom-title") => {
                custom_title = string(&record, &["customTitle"]).and_then(normalize_title);
            }
            Some("ai-title") => {
                if let Some(title) = string(&record, &["aiTitle"]).and_then(normalize_title) {
                    generated_title = Some(title);
                }
            }
            Some("agent-name") if generated_title.is_none() => {
                meta_title = meta_title
                    .or_else(|| string(&record, &["agentName"]).and_then(normalize_title));
            }
            Some("user") => {
                let message = record.get("message").and_then(Value::as_object);
                let is_meta = record.get("isMeta").and_then(Value::as_bool) == Some(true);
                let text = message
                    .and_then(|message| message.get("content"))
                    .and_then(claude_visible_message_text);
                let is_harness_injected = text
                    .as_deref()
                    .is_some_and(is_known_harness_injected_user_turn);
                if !is_meta && !is_harness_injected {
                    first_user_title = first_user_title
                        .or_else(|| text.clone().and_then(normalize_title));
                    session.push(HistoryRole::User, text);
                }
            }
            Some("assistant") => {
                let message = record.get("message").and_then(Value::as_object);
                if let Some(message) = message {
                    if let Some(message_id) = claude_completed_turn_message_id(&record, message) {
                        completed_turn_ids.insert(message_id);
                    }
                }
                session.model = message
                    .and_then(|message| string(message, &["model"]))
                    .or(session.model);
                if let Some(usage) = message.and_then(|message| message.get("usage")) {
                    session.add_total_tokens(claude_usage_total(usage));
                }
                session.push(
                    HistoryRole::Assistant,
                    message
                        .and_then(|message| message.get("content"))
                        .and_then(claude_visible_message_text),
                );
            }
            _ => {}
        }
    }
    session.title = custom_title
        .or(generated_title)
        .or(first_user_title)
        .or(meta_title);
    session.completed_turn_count = Some(completed_turn_ids.len() as u64);
    Ok(session.finish())
}

fn parse_codex(document: &HistoryDocument) -> Result<HistorySession, HistoryAdapterError> {
    let mut session = SessionBuilder::new(document.session_id_hint.trim().to_owned());
    let mut saw_session_meta = false;
    let mut metadata_title = None;
    let mut user_title = None;
    let mut previous_usage = None;

    for record in ndjson_records(&document.transcript) {
        let payload = record.get("payload").and_then(Value::as_object);
        match (record.get("type").and_then(Value::as_str), payload) {
            (Some("session_meta"), Some(payload)) => {
                if is_codex_worker_session(payload) {
                    return Err(HistoryAdapterError::ExcludedProviderSession);
                }
                saw_session_meta = true;
                if let Some(id) = string(payload, &["id"]) {
                    session.session_id = select_session_id(Some(id), &session.session_id)?;
                }
                metadata_title = string(payload, &["title", "thread_name", "threadName"])
                    .and_then(normalize_title)
                    .or(metadata_title);
                session.cwd = string(payload, &["cwd"]).or(session.cwd);
            }
            (Some("turn_context"), Some(payload)) => {
                session.cwd = string(payload, &["cwd"]).or(session.cwd);
                session.model = model_from_nested_record(payload).or(session.model);
            }
            (Some("response_item"), Some(payload))
                if payload.get("type").and_then(Value::as_str) == Some("message") =>
            {
                let role = role_from_value(payload.get("role"));
                if let Some(role) = role {
                    let text = payload.get("content").and_then(content_text);
                    if role == HistoryRole::User {
                        user_title = user_title.or_else(|| text.clone().and_then(normalize_title));
                    }
                    session.push(role, text);
                } else {
                    session.message_count = session.message_count.saturating_add(1);
                }
            }
            (Some("event_msg"), Some(payload)) => {
                match payload.get("type").and_then(Value::as_str) {
                    Some("user_message") => {
                        let text = payload.get("message").and_then(content_text);
                        user_title = user_title.or_else(|| text.clone().and_then(normalize_title));
                        session.push(HistoryRole::User, text);
                    }
                    Some("agent_message") => session.push(
                        HistoryRole::Assistant,
                        payload.get("message").and_then(content_text),
                    ),
                    Some("token_count") => {
                        if let Some(info) = payload.get("info").and_then(Value::as_object) {
                            let total = info
                                .get("total_token_usage")
                                .and_then(normalize_codex_usage);
                            let last = info.get("last_token_usage").and_then(normalize_codex_usage);
                            if let Some(total) = total {
                                session.total_tokens_observed = true;
                                session.total_tokens = session.total_tokens.saturating_add(
                                    total.total_tokens.saturating_sub(
                                        previous_usage
                                            .map(|usage: CodexUsage| usage.total_tokens)
                                            .unwrap_or(0),
                                    ),
                                );
                                previous_usage = Some(total);
                            } else if let Some(last) = last {
                                session.total_tokens_observed = true;
                                session.total_tokens =
                                    session.total_tokens.saturating_add(last.total_tokens);
                                previous_usage = Some(previous_usage.unwrap_or_default().add(last));
                            }
                        }
                        session.model = model_from_nested_record(payload).or(session.model);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    let indexed_title = if saw_session_meta {
        document
            .metadata_json
            .as_deref()
            .and_then(|metadata| serde_json::from_str::<Value>(metadata).ok())
            .and_then(|value| value.as_object().cloned())
            .and_then(|metadata| string(&metadata, &["indexed_title", "title"]))
            .and_then(normalize_title)
    } else {
        None
    };
    session.title = metadata_title.or(indexed_title).or(user_title);
    Ok(session.finish())
}

fn parse_gemini(document: &HistoryDocument) -> Result<HistorySession, HistoryAdapterError> {
    let trimmed = document.transcript.trim();
    let legacy = serde_json::from_str::<Value>(trimmed)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .filter(|record| record.get("messages").is_some());
    let mut session = SessionBuilder::new(document.session_id_hint.trim().to_owned());
    if let Some(record) = legacy {
        if let Some(id) = string(&record, &["sessionId"]) {
            session.session_id = select_session_id(Some(id), &session.session_id)?;
        }
        if let Some(messages) = record.get("messages").and_then(Value::as_array) {
            for message in messages {
                if let Some(message) = message.as_object() {
                    consume_gemini_message(&mut session, message);
                }
            }
        }
    } else {
        for record in ndjson_records(&document.transcript) {
            if record.get("$set").and_then(Value::as_object).is_some() {
                continue;
            }
            if let Some(id) = string(&record, &["sessionId"]) {
                session.session_id = select_session_id(Some(id), &session.session_id)?;
            }
            consume_gemini_message(&mut session, &record);
        }
    }
    Ok(session.finish())
}

fn consume_gemini_message(session: &mut SessionBuilder, record: &Map<String, Value>) {
    match record.get("type").and_then(Value::as_str) {
        Some("user") => session.push(
            HistoryRole::User,
            record.get("content").and_then(content_text),
        ),
        Some("gemini") => {
            session.model = string(record, &["model"]).or(session.model.take());
            if let Some(tokens) = record.get("tokens") {
                session.add_total_tokens(token_total(tokens));
            }
            session.push(
                HistoryRole::Assistant,
                record.get("content").and_then(content_text),
            );
        }
        _ => {}
    }
}

fn parse_antigravity(document: &HistoryDocument) -> Result<HistorySession, HistoryAdapterError> {
    let mut session = SessionBuilder::new(document.session_id_hint.trim().to_owned());
    for record in ndjson_records(&document.transcript) {
        let source = string(&record, &["source"]);
        let kind = string(&record, &["type"]);
        let content = string(&record, &["content"]);
        if matches!(source.as_deref(), Some("USER_EXPLICIT" | "USER"))
            && matches!(kind.as_deref(), Some("USER_INPUT" | "REQUEST"))
        {
            session.push(
                HistoryRole::User,
                content.and_then(extract_antigravity_user_request),
            );
        } else if source.as_deref() == Some("MODEL") && kind.as_deref() == Some("PLANNER_RESPONSE")
        {
            session.push(HistoryRole::Assistant, content);
        }
    }
    Ok(session.finish())
}

fn parse_grok(document: &HistoryDocument) -> Result<HistorySession, HistoryAdapterError> {
    let metadata = metadata_object(document)?;
    let info = metadata.get("info").and_then(Value::as_object);
    let mut session = SessionBuilder::new(select_session_id(
        info.and_then(|value| string(value, &["id"])),
        &document.session_id_hint,
    )?);
    session.cwd = info.and_then(|value| string(value, &["cwd"]));
    session.title =
        string(&metadata, &["generated_title", "session_summary"]).and_then(normalize_title);
    session.model = string(&metadata, &["current_model_id"]);
    let declared_count = u64_value(&metadata, &["num_chat_messages"])
        .filter(|count| *count > 0)
        .or_else(|| u64_value(&metadata, &["num_messages"]));

    for record in ndjson_records(&document.transcript) {
        let Some(role) = role_from_value(record.get("type")) else {
            continue;
        };
        let text = record.get("content").and_then(grok_content_text);
        session.push(role, text);
    }
    if let Some(declared_count) = declared_count {
        session.message_count = declared_count;
    }
    Ok(session.finish())
}

fn parse_kimi(document: &HistoryDocument) -> Result<HistorySession, HistoryAdapterError> {
    let metadata = metadata_object(document)?;
    let mut session = SessionBuilder::new(document.session_id_hint.trim().to_owned());
    session.title = string(&metadata, &["title"]).and_then(normalize_title);
    session.cwd = string(&metadata, &["cwd", "workDir"]);
    let fallback_title = string(&metadata, &["lastPrompt"]).and_then(normalize_title);
    let mut assistant_parts = Vec::new();

    for record in ndjson_records(&document.transcript) {
        match record.get("type").and_then(Value::as_str) {
            Some("config.update") => {
                session.model = string(&record, &["modelAlias"]).or(session.model);
            }
            Some("usage.record")
                if record.get("usageScope").and_then(Value::as_str) != Some("session") =>
            {
                session.model = string(&record, &["model"]).or(session.model);
                if let Some(usage) = record.get("usage").and_then(Value::as_object) {
                    session.add_total_tokens(sum_named_numbers(
                        usage,
                        &[
                            "inputOther",
                            "output",
                            "inputCacheRead",
                            "inputCacheCreation",
                        ],
                    ));
                }
            }
            Some("context.append_message") => {
                let Some(message) = record.get("message").and_then(Value::as_object) else {
                    continue;
                };
                let is_real_user = message.get("role").and_then(Value::as_str) == Some("user")
                    && message
                        .get("origin")
                        .and_then(Value::as_object)
                        .and_then(|origin| origin.get("kind"))
                        .and_then(Value::as_str)
                        == Some("user");
                if is_real_user {
                    session.push(
                        HistoryRole::User,
                        message.get("content").and_then(content_text),
                    );
                }
            }
            Some("context.append_loop_event") => {
                let Some(event) = record.get("event").and_then(Value::as_object) else {
                    continue;
                };
                match event.get("type").and_then(Value::as_str) {
                    Some("content.part") => {
                        let part = event.get("part").and_then(Value::as_object);
                        if part
                            .and_then(|value| value.get("type"))
                            .and_then(Value::as_str)
                            == Some("text")
                        {
                            if let Some(text) = part
                                .and_then(|value| value.get("text"))
                                .and_then(Value::as_str)
                            {
                                assistant_parts.push(text.to_owned());
                            }
                        }
                    }
                    Some("step.end") => flush_assistant(&mut session, &mut assistant_parts),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    flush_assistant(&mut session, &mut assistant_parts);
    session.title = session.title.or(fallback_title);
    Ok(session.finish())
}

fn parse_qwen(document: &HistoryDocument) -> Result<HistorySession, HistoryAdapterError> {
    let records = qwen_active_records(&document.transcript);
    let mut session = SessionBuilder::new(document.session_id_hint.trim().to_owned());
    let mut custom_title = None;

    for record in records {
        if let Some(id) = string(&record, &["sessionId"]) {
            session.session_id = select_session_id(Some(id), &session.session_id)?;
        }
        session.cwd = string(&record, &["cwd"]).or(session.cwd);
        match (
            record.get("type").and_then(Value::as_str),
            record.get("provenance").and_then(Value::as_str),
        ) {
            (Some("user"), Some("real_user"))
                if matches!(
                    record.get("subtype").and_then(Value::as_str),
                    None | Some("mid_turn_user_message")
                ) =>
            {
                let text = qwen_message_text(&record).filter(|text| {
                    !is_known_harness_injected_user_turn(text)
                });
                session.push(HistoryRole::User, text);
            }
            (Some("assistant"), Some("assistant_output"))
                if record.get("subtype").is_none() =>
            {
                session.model = string(&record, &["model"]).or(session.model);
                if let Some(usage) = record.get("usageMetadata") {
                    session.add_total_tokens(qwen_usage_total(usage));
                }
                session.push(HistoryRole::Assistant, qwen_message_text(&record));
            }
            (Some("system"), _)
                if record.get("subtype").and_then(Value::as_str)
                    == Some("custom_title") =>
            {
                custom_title = record
                    .get("systemPayload")
                    .and_then(Value::as_object)
                    .and_then(|payload| string(payload, &["customTitle"]))
                    .and_then(normalize_title)
                    .or(custom_title);
            }
            _ => {}
        }
    }
    session.title = custom_title.or(session.title);
    Ok(session.finish())
}

fn parse_copilot(document: &HistoryDocument) -> Result<HistorySession, HistoryAdapterError> {
    let mut session = SessionBuilder::new(document.session_id_hint.trim().to_owned());
    for record in ndjson_records(&document.transcript) {
        let data = record.get("data").and_then(Value::as_object);
        match record.get("type").and_then(Value::as_str) {
            Some("session.start") => {
                if let Some(id) = data.and_then(|value| string(value, &["sessionId"])) {
                    session.session_id = select_session_id(Some(id), &session.session_id)?;
                }
            }
            Some("session.model_change") => {
                session.model = data
                    .and_then(|value| string(value, &["newModel"]))
                    .or(session.model);
            }
            Some("session.info") => {
                session.cwd = data
                    .and_then(|value| {
                        string(value, &["trustedFolder", "cwd"]).or_else(|| {
                            string(value, &["message"]).and_then(copilot_trusted_folder)
                        })
                    })
                    .or(session.cwd);
            }
            Some("user.message") => session.push(
                HistoryRole::User,
                data.and_then(|value| string(value, &["transformedContent", "content"])),
            ),
            Some("assistant.message") => session.push(
                HistoryRole::Assistant,
                data.and_then(|value| string(value, &["content"])),
            ),
            Some("session.shutdown") => {
                session.model = data
                    .and_then(|value| string(value, &["currentModel"]))
                    .or(session.model);
                if let Some(data) = data {
                    session.add_total_tokens(u64_value(data, &["currentTokens"]));
                    if let Some(metrics) = data.get("modelMetrics") {
                        session.add_total_tokens(copilot_model_metrics_total(metrics));
                    }
                }
            }
            _ => {}
        }
    }
    Ok(session.finish())
}

fn parse_droid(document: &HistoryDocument) -> Result<HistorySession, HistoryAdapterError> {
    let mut session = SessionBuilder::new(document.session_id_hint.trim().to_owned());
    for record in ndjson_records(&document.transcript) {
        if let Some(id) = string(&record, &["session_id", "sessionId"]) {
            session.session_id = select_session_id(Some(id), &session.session_id)?;
        }
        match record.get("type").and_then(Value::as_str) {
            Some("session_start") => {
                if let Some(id) = string(&record, &["id"]) {
                    session.session_id = select_session_id(Some(id), &session.session_id)?;
                }
                session.title = string(&record, &["title"]).and_then(normalize_title);
                session.cwd = string(&record, &["cwd"]).or(session.cwd);
            }
            Some("system") => {
                session.cwd = string(&record, &["cwd"]).or(session.cwd);
                session.model = string(&record, &["model"]).or(session.model);
            }
            Some("message") => {
                let nested = record.get("message").and_then(Value::as_object);
                let role = role_from_value(
                    record
                        .get("role")
                        .or_else(|| nested.and_then(|message| message.get("role"))),
                );
                if let Some(role) = role {
                    let text = string(&record, &["text"]).or_else(|| {
                        nested
                            .and_then(|message| message.get("content"))
                            .and_then(content_text)
                    });
                    session.push(role, text);
                }
            }
            Some("completion") => {
                session.push(HistoryRole::Assistant, string(&record, &["finalText"]));
                if let Some(usage) = record.get("usage") {
                    session.add_total_tokens(token_total(usage));
                }
            }
            _ => {}
        }
    }
    Ok(session.finish())
}

fn parse_cursor(document: &HistoryDocument) -> Result<HistorySession, HistoryAdapterError> {
    let mut session = SessionBuilder::new(document.session_id_hint.trim().to_owned());
    for record in ndjson_records(&document.transcript) {
        let Some(role) = role_from_value(record.get("role")) else {
            continue;
        };
        let text = record
            .get("message")
            .and_then(|message| {
                message
                    .as_object()
                    .and_then(|value| value.get("content"))
                    .or(Some(message))
            })
            .or_else(|| record.get("content"))
            .and_then(content_text);
        session.push(role, text);
    }
    Ok(session.finish())
}

fn parse_opencode(document: &HistoryDocument) -> Result<HistorySession, HistoryAdapterError> {
    let metadata = metadata_object(document)?;
    let mut session = SessionBuilder::new(select_session_id(
        string(&metadata, &["id"]),
        &document.session_id_hint,
    )?);
    session.title = string(&metadata, &["title"]).and_then(normalize_title);
    session.cwd = string(&metadata, &["directory"]);
    session.model = opencode_model(&metadata);
    session.replace_total_tokens(sum_named_numbers(
        &metadata,
        &[
            "tokens_input",
            "tokens_output",
            "tokens_reasoning",
            "tokens_cache_read",
        ],
    ));
    let declared_count = u64_value(&metadata, &["message_count"]);

    for record in ndjson_records(&document.transcript) {
        let Some(role) = role_from_value(record.get("role")) else {
            continue;
        };
        if role == HistoryRole::User && session.title.is_none() {
            session.title = string(&record, &["summary_title", "summary_body"])
                .or_else(|| {
                    record
                        .get("summary")
                        .and_then(Value::as_object)
                        .and_then(|summary| string(summary, &["title", "body"]))
                })
                .and_then(normalize_title);
        }
        let text = record
            .get("part_data")
            .and_then(Value::as_str)
            .and_then(json_string_field_text)
            .or_else(|| record.get("content").and_then(content_text))
            .or_else(|| {
                record
                    .get("summary")
                    .and_then(Value::as_object)
                    .and_then(|summary| string(summary, &["body", "title"]))
            });
        session.push(role, text);
        session.model = opencode_model(&record).or(session.model);
        if let Some(tokens) = record.get("tokens") {
            session.add_total_tokens(token_total(tokens));
        }
    }
    if let Some(declared_count) = declared_count {
        session.message_count = session.message_count.max(declared_count);
    }
    Ok(session.finish())
}

fn parse_hermes(document: &HistoryDocument) -> Result<HistorySession, HistoryAdapterError> {
    let record = transcript_object(document, false)?;
    let mut session = SessionBuilder::new(select_session_id(
        string(&record, &["session_id"]),
        &document.session_id_hint,
    )?);
    session.model = string(&record, &["model"]);
    session.cwd = string(&record, &["cwd"]);
    if let Some(messages) = record.get("messages").and_then(Value::as_array) {
        for message in messages {
            let Some(message) = message.as_object() else {
                continue;
            };
            let Some(role) = role_from_value(message.get("role")) else {
                continue;
            };
            session.push(role, message.get("content").and_then(content_text));
        }
    }
    if session.message_count == 0 {
        session.message_count = u64_value(&record, &["message_count"]).unwrap_or(0);
    }
    Ok(session.finish())
}

fn parse_rovo(document: &HistoryDocument) -> Result<HistorySession, HistoryAdapterError> {
    let metadata = metadata_object(document)?;
    let mut session = SessionBuilder::new(document.session_id_hint.trim().to_owned());
    session.title = string(&metadata, &["title", "name", "summary"]).and_then(normalize_title);
    session.cwd = string(
        &metadata,
        &[
            "workspace_path",
            "workspacePath",
            "workspace",
            "cwd",
            "working_directory",
            "workingDirectory",
            "project_path",
            "projectPath",
        ],
    );
    let context = transcript_object(document, true)?;
    if let Some(messages) = context.get("messages").and_then(Value::as_array) {
        for message in messages {
            let Some(message) = message.as_object() else {
                continue;
            };
            let Some(role) = role_from_value(message.get("role")) else {
                continue;
            };
            session.push(role, message.get("content").and_then(content_text));
        }
    }
    if let Some(history) = context.get("message_history").and_then(Value::as_array) {
        for entry in history {
            let Some(entry) = entry.as_object() else {
                continue;
            };
            let role = role_from_value(entry.get("role")).or_else(|| {
                match string(entry, &["kind"]).as_deref() {
                    Some("request") => Some(HistoryRole::User),
                    Some("response") => Some(HistoryRole::Assistant),
                    _ => None,
                }
            });
            let Some(role) = role else {
                continue;
            };
            if let Some(text) = rovo_parts_text(entry.get("parts"), role) {
                session.push(role, Some(text));
            }
        }
    }
    Ok(session.finish())
}

fn parse_message_graph(document: &HistoryDocument) -> Result<HistorySession, HistoryAdapterError> {
    let mut session = SessionBuilder::new(document.session_id_hint.trim().to_owned());
    for record in ndjson_records(&document.transcript) {
        match record.get("type").and_then(Value::as_str) {
            Some("session") => {
                if let Some(id) = string(&record, &["id"]) {
                    session.session_id = select_session_id(Some(id), &session.session_id)?;
                }
                session.cwd = string(&record, &["cwd"]).or(session.cwd);
            }
            Some("model_change") => {
                session.model = string(&record, &["modelId", "model"]).or(session.model);
            }
            Some("message") => {
                let Some(message) = record.get("message").and_then(Value::as_object) else {
                    continue;
                };
                let Some(role) = role_from_value(message.get("role")) else {
                    continue;
                };
                if role == HistoryRole::Assistant {
                    session.model = string(message, &["model"]).or(session.model);
                    if let Some(usage) = message.get("usage") {
                        session.add_total_tokens(token_total(usage));
                    }
                }
                session.push(role, message.get("content").and_then(content_text));
            }
            _ => {}
        }
    }
    Ok(session.finish())
}

fn parse_devin(document: &HistoryDocument) -> Result<HistorySession, HistoryAdapterError> {
    let record = transcript_object(document, false)?;
    let mut session = SessionBuilder::new(select_session_id(
        string(&record, &["session_id", "sessionId"]),
        &document.session_id_hint,
    )?);
    let agent = record.get("agent").and_then(Value::as_object);
    session.model = agent
        .and_then(|agent| string(agent, &["model_name", "model"]))
        .or_else(|| string(&record, &["generation_model"]));
    session.cwd = string(&record, &["working_directory"]);
    if let Some(steps) = record.get("steps").and_then(Value::as_array) {
        for step in steps {
            let Some(step) = step.as_object() else {
                continue;
            };
            let metadata = step.get("metadata").and_then(Value::as_object);
            let metrics = metadata
                .and_then(|metadata| metadata.get("metrics"))
                .and_then(Value::as_object);
            session.model = metadata
                .and_then(|metadata| string(metadata, &["generation_model"]))
                .or_else(|| metrics.and_then(|metrics| string(metrics, &["generation_model"])))
                .or(session.model);
            session.add_total_tokens(devin_step_token_total(metadata, metrics));
            let is_user = metadata
                .and_then(|metadata| metadata.get("is_user_input"))
                .and_then(Value::as_bool)
                == Some(true);
            let is_assistant = string(step, &["role"]).as_deref() == Some("assistant")
                || step.get("tool_calls").is_some();
            let role = if is_user {
                Some(HistoryRole::User)
            } else if is_assistant {
                Some(HistoryRole::Assistant)
            } else {
                None
            };
            let Some(role) = role else {
                continue;
            };
            let text = step
                .get("message")
                .and_then(Value::as_object)
                .and_then(|message| message.get("content"))
                .and_then(content_text)
                .or_else(|| string(step, &["text"]))
                .or_else(|| step.get("content").and_then(content_text));
            session.push(role, text);
        }
    }
    Ok(session.finish())
}

struct SessionBuilder {
    session_id: String,
    title: Option<String>,
    cwd: Option<String>,
    model: Option<String>,
    message_count: u64,
    completed_turn_count: Option<u64>,
    total_tokens: u64,
    total_tokens_observed: bool,
    messages: VecDeque<HistoryMessage>,
}

impl SessionBuilder {
    fn new(session_id: String) -> Self {
        Self {
            session_id,
            title: None,
            cwd: None,
            model: None,
            message_count: 0,
            completed_turn_count: None,
            total_tokens: 0,
            total_tokens_observed: false,
            messages: VecDeque::new(),
        }
    }

    fn add_total_tokens(&mut self, observed: Option<u64>) {
        let Some(total) = observed else { return; };
        self.total_tokens_observed = true;
        self.total_tokens = self.total_tokens.saturating_add(total);
    }

    fn replace_total_tokens(&mut self, observed: Option<u64>) {
        let Some(total) = observed else { return; };
        self.total_tokens_observed = true;
        self.total_tokens = total;
    }

    fn push(&mut self, role: HistoryRole, text: Option<String>) {
        self.message_count = self.message_count.saturating_add(1);
        let text = text.and_then(normalize_message);
        if role == HistoryRole::User && self.title.is_none() {
            self.title = text.clone().and_then(normalize_title);
        }
        if let Some(text) = text {
            if self.messages.len() == HISTORY_STORED_MESSAGES_MAX {
                self.messages.pop_front();
            }
            self.messages.push_back(HistoryMessage { role, text });
        }
    }

    fn finish(self) -> HistorySession {
        HistorySession {
            session_id: self.session_id,
            title: self.title,
            cwd: self.cwd,
            model: self.model,
            message_count: self.message_count,
            completed_turn_count: self.completed_turn_count,
            total_tokens: self.total_tokens,
            total_tokens_observed: self.total_tokens_observed,
            messages: self.messages.into_iter().collect(),
        }
    }
}

fn metadata_object(document: &HistoryDocument) -> Result<Map<String, Value>, HistoryAdapterError> {
    let metadata = document
        .metadata_json
        .as_deref()
        .ok_or(HistoryAdapterError::MissingMetadata)?;
    serde_json::from_str::<Value>(metadata)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .ok_or(HistoryAdapterError::InvalidMetadata)
}

fn transcript_object(
    document: &HistoryDocument,
    empty_is_object: bool,
) -> Result<Map<String, Value>, HistoryAdapterError> {
    if empty_is_object && document.transcript.trim().is_empty() {
        return Ok(Map::new());
    }
    serde_json::from_str::<Value>(&document.transcript)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .ok_or(HistoryAdapterError::InvalidTranscript)
}

fn ndjson_records(content: &str) -> impl Iterator<Item = Map<String, Value>> + '_ {
    content.lines().filter_map(|line| {
        serde_json::from_str::<Value>(line.trim())
            .ok()
            .and_then(|value| value.as_object().cloned())
    })
}

fn select_session_id(
    candidate: Option<String>,
    fallback: &str,
) -> Result<String, HistoryAdapterError> {
    let value = candidate
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| fallback.trim());
    validate_session_id(value)?;
    Ok(value.to_owned())
}

fn validate_session_id(value: &str) -> Result<(), HistoryAdapterError> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 512
        || value.starts_with('-')
        || value.chars().any(char::is_control)
    {
        return Err(HistoryAdapterError::InvalidSessionId);
    }
    Ok(())
}

fn role_from_value(value: Option<&Value>) -> Option<HistoryRole> {
    match value.and_then(Value::as_str) {
        Some("user") => Some(HistoryRole::User),
        Some("assistant") => Some(HistoryRole::Assistant),
        _ => None,
    }
}

fn string(record: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        record
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    })
}

fn u64_value(record: &Map<String, Value>, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|key| {
        record.get(*key).and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_i64().and_then(|value| u64::try_from(value).ok()))
        })
    })
}

fn sum_named_numbers(record: &Map<String, Value>, keys: &[&str]) -> Option<u64> {
    let mut observed = false;
    let total = keys
        .iter()
        .filter_map(|key| {
            let value = u64_value(record, &[*key]);
            observed |= value.is_some();
            value
        })
        .fold(0, u64::saturating_add);
    observed.then_some(total)
}

fn claude_usage_total(value: &Value) -> Option<u64> {
    let Some(usage) = value.as_object() else {
        return None;
    };
    sum_named_numbers(
        usage,
        &[
            "input_tokens",
            "output_tokens",
            "cache_read_input_tokens",
            "cache_creation_input_tokens",
        ],
    )
}

fn claude_completed_turn_message_id(
    record: &Map<String, Value>,
    message: &Map<String, Value>,
) -> Option<String> {
    if record.get("isMeta").and_then(Value::as_bool) == Some(true)
        || message.get("stop_reason").and_then(Value::as_str) != Some("end_turn")
    {
        return None;
    }
    let message_id = message.get("id")?.as_str()?.trim();
    if message_id.is_empty()
        || message_id.len() > HISTORY_PROVIDER_MESSAGE_ID_MAX_BYTES
        || !message_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return None;
    }
    let has_visible_text = message
        .get("content")
        .and_then(Value::as_array)
        .is_some_and(|content| {
            content.iter().any(|block| {
                let Some(block) = block.as_object() else {
                    return false;
                };
                block.get("type").and_then(Value::as_str) == Some("text")
                    && block
                        .get("text")
                        .and_then(Value::as_str)
                        .and_then(|text| normalize_message(text.to_owned()))
                        .is_some()
            })
        });
    has_visible_text.then(|| message_id.to_owned())
}

fn copilot_trusted_folder(message: String) -> Option<String> {
    message
        .strip_prefix("Folder ")
        .and_then(|value| value.strip_suffix(" has been added to trusted folders."))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn copilot_model_metrics_total(value: &Value) -> Option<u64> {
    let mut observed = false;
    let total = value
        .as_object()
        .into_iter()
        .flat_map(|metrics| metrics.values())
        .filter_map(Value::as_object)
        .filter_map(|metric| metric.get("usage"))
        .filter_map(|usage| {
            let total = token_total(usage);
            observed |= total.is_some();
            total
        })
        .fold(0, u64::saturating_add);
    observed.then_some(total)
}

#[derive(Clone, Copy, Debug, Default)]
struct CodexUsage {
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
}

impl CodexUsage {
    fn add(self, other: Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            total_tokens: self.total_tokens.saturating_add(other.total_tokens),
        }
    }
}

fn normalize_codex_usage(value: &Value) -> Option<CodexUsage> {
    let usage = value.as_object()?;
    if u64_value(usage, &["input_tokens", "output_tokens", "total_tokens"]).is_none() {
        return None;
    }
    let input_tokens = u64_value(usage, &["input_tokens"]).unwrap_or(0);
    let output_tokens = u64_value(usage, &["output_tokens"]).unwrap_or(0);
    Some(CodexUsage {
        input_tokens,
        output_tokens,
        total_tokens: u64_value(usage, &["total_tokens"])
            .filter(|value| *value > 0)
            .unwrap_or_else(|| input_tokens.saturating_add(output_tokens)),
    })
}

fn model_from_nested_record(record: &Map<String, Value>) -> Option<String> {
    string(record, &["model", "model_name"])
        .or_else(|| {
            record
                .get("metadata")
                .and_then(Value::as_object)
                .and_then(|metadata| string(metadata, &["model"]))
        })
        .or_else(|| {
            record
                .get("info")
                .and_then(Value::as_object)
                .and_then(|info| string(info, &["model"]))
        })
}

fn is_codex_worker_session(payload: &Map<String, Value>) -> bool {
    if let Some(source) = string(payload, &["thread_source", "threadSource"]) {
        return !source.eq_ignore_ascii_case("user");
    }
    payload
        .get("source")
        .and_then(Value::as_object)
        .and_then(|source| source.get("subagent"))
        .and_then(Value::as_object)
        .is_some()
}

fn is_known_harness_injected_user_turn(text: &str) -> bool {
    let normalized = text.trim().to_ascii_lowercase();
    let tag = normalized
        .strip_prefix('<')
        .and_then(|value| value.split([' ', '>']).next());
    let known_tag = tag.is_some_and(|tag| {
        matches!(
            tag,
            "agent-message"
                | "bash-input"
                | "bash-stderr"
                | "bash-stdout"
                | "command-args"
                | "command-message"
                | "command-name"
                | "cross-session-message"
                | "fork-boilerplate"
                | "local-command-caveat"
                | "local-command-stderr"
                | "local-command-stdout"
                | "mcp-polling-update"
                | "mcp-resource-update"
                | "system-reminder"
                | "task-notification"
                | "teammate-message"
                | "user-memory-input"
                | "user-prompt-submit-hook"
        )
    });
    known_tag
        || [
            "<channel source=",
            "[request interrupted",
            "a message arrived from ",
            "another claude session sent a message",
            "no response requested.",
            "caveat: the messages below were generated by the user while running local commands",
            "this session is being continued from a previous conversation",
        ]
        .iter()
        .any(|prefix| normalized.starts_with(prefix))
}

fn extract_antigravity_user_request(content: String) -> Option<String> {
    const OPEN: &str = "<USER_REQUEST>";
    const CLOSE: &str = "</USER_REQUEST>";
    let Some(start) = content.find(OPEN) else {
        return normalize_message(content);
    };
    let body_start = start + OPEN.len();
    let body_end = content[body_start..]
        .find(CLOSE)
        .map(|offset| body_start + offset)
        .unwrap_or(content.len());
    normalize_message(content[body_start..body_end].to_owned())
}

fn token_total(value: &Value) -> Option<u64> {
    let Some(usage) = value.as_object() else {
        return None;
    };
    if let Some(total) = u64_value(usage, &["total", "totalTokens", "total_tokens"])
    {
        return Some(total);
    }
    sum_named_numbers(
        usage,
        &[
            "input",
            "inputTokens",
            "input_tokens",
            "output",
            "outputTokens",
            "output_tokens",
            "cacheRead",
            "cacheReadTokens",
            "cache_read_input_tokens",
            "cacheWrite",
            "cacheWriteTokens",
            "cache_creation_input_tokens",
            "cached",
            "cachedInputTokens",
            "cached_input_tokens",
            "reasoning",
            "reasoningOutputTokens",
            "reasoning_output_tokens",
        ],
    )
}

fn opencode_model(record: &Map<String, Value>) -> Option<String> {
    if let Some(model) = record.get("model").and_then(Value::as_object) {
        return string(model, &["id", "modelID"]);
    }
    if let Some(model_json) = string(record, &["model_json"]) {
        if let Some(model) = serde_json::from_str::<Value>(&model_json)
            .ok()
            .and_then(|value| value.as_object().cloned())
            .and_then(|model| string(&model, &["id", "modelID"]))
        {
            return Some(model);
        }
    }
    string(record, &["modelID"])
}

fn json_string_field_text(value: &str) -> Option<String> {
    serde_json::from_str::<Value>(value)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .and_then(|record| string(&record, &["text"]))
        .and_then(normalize_message)
}

fn rovo_parts_text(value: Option<&Value>, role: HistoryRole) -> Option<String> {
    let parts = value.and_then(Value::as_array)?;
    let mut text = Vec::new();
    for part in parts {
        let Some(part) = part.as_object() else {
            continue;
        };
        let kind = string(part, &["part_kind"]);
        let accepted = match role {
            HistoryRole::User => matches!(kind.as_deref(), Some("user-prompt" | "text")),
            HistoryRole::Assistant => kind.as_deref() == Some("text"),
        };
        if accepted {
            if let Some(value) = string(part, &["content", "text"]) {
                text.push(value);
            }
        }
    }
    normalize_message(text.join(" "))
}

fn devin_step_token_total(
    metadata: Option<&Map<String, Value>>,
    metrics: Option<&Map<String, Value>>,
) -> Option<u64> {
    let mut observed = false;
    let total = [
        &["total_input_tokens", "input_tokens"][..],
        &["output_tokens"][..],
        &["cache_read_tokens", "cache_read_input_tokens"][..],
        &["cache_creation_tokens", "cache_creation_input_tokens"][..],
    ]
    .iter()
    .map(|keys| {
        [metadata, metrics]
            .into_iter()
            .flatten()
            .find_map(|source| u64_value(source, keys))
            .inspect(|_| observed = true)
            .unwrap_or(0)
    })
    .fold(0, u64::saturating_add);
    observed.then_some(total)
}

fn grok_content_text(value: &Value) -> Option<String> {
    let text = content_text(value)?;
    let lower = text.to_ascii_lowercase();
    let opener = "<user_query>";
    let closer = "</user_query>";
    let Some(start) = lower.find(opener).map(|index| index + opener.len()) else {
        return Some(text);
    };
    let Some(end) = lower[start..].find(closer).map(|index| start + index) else {
        return Some(text);
    };
    normalize_message(text[start..end].to_owned()).or(Some(text))
}

fn qwen_active_records(content: &str) -> Vec<Map<String, Value>> {
    let records = ndjson_records(content).collect::<Vec<_>>();
    let by_uuid = records
        .iter()
        .enumerate()
        .filter_map(|(index, record)| {
            string(record, &["uuid"]).map(|uuid| (uuid, index))
        })
        .collect::<std::collections::HashMap<_, _>>();
    let Some(mut current) = records
        .iter()
        .rev()
        .find_map(|record| string(record, &["uuid"]))
    else {
        return Vec::new();
    };
    let mut active = std::collections::HashSet::new();
    while let Some(index) = by_uuid.get(&current).copied() {
        if !active.insert(index) {
            break;
        }
        let Some(parent) = records[index]
            .get("parentUuid")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|parent| !parent.is_empty())
        else {
            break;
        };
        current = parent.to_owned();
    }
    records
        .into_iter()
        .enumerate()
        .filter_map(|(index, record)| active.contains(&index).then_some(record))
        .collect()
}

fn qwen_message_text(record: &Map<String, Value>) -> Option<String> {
    let parts = record
        .get("message")
        .and_then(Value::as_object)?
        .get("parts")
        .and_then(Value::as_array)?;
    let text = parts
        .iter()
        .filter_map(Value::as_object)
        .filter(|part| part.get("thought").and_then(Value::as_bool) != Some(true))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join(" ");
    normalize_message(text)
}

fn qwen_usage_total(value: &Value) -> Option<u64> {
    let Some(usage) = value.as_object() else {
        return None;
    };
    u64_value(usage, &["totalTokenCount"])
        .or_else(|| {
            sum_named_numbers(usage, &["promptTokenCount", "candidatesTokenCount"])
        })
}

fn content_text(value: &Value) -> Option<String> {
    content_text_at_depth(value, 0).and_then(normalize_message)
}

fn claude_visible_message_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => normalize_message(text.clone()),
        Value::Array(parts) => {
            let visible = parts
                .iter()
                .filter_map(Value::as_object)
                .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(" ");
            normalize_message(visible)
        }
        _ => None,
    }
}

fn content_text_at_depth(value: &Value, depth: usize) -> Option<String> {
    if depth > 4 {
        return None;
    }
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Array(values) => {
            let parts = values
                .iter()
                .filter_map(|value| content_text_at_depth(value, depth + 1))
                .collect::<Vec<_>>();
            (!parts.is_empty()).then(|| parts.join(" "))
        }
        Value::Object(record) => ["text", "content", "message"].iter().find_map(|key| {
            record
                .get(*key)
                .and_then(|value| content_text_at_depth(value, depth + 1))
        }),
        _ => None,
    }
}

fn flush_assistant(session: &mut SessionBuilder, parts: &mut Vec<String>) {
    if parts.is_empty() {
        return;
    }
    let text = parts.join("");
    parts.clear();
    session.push(HistoryRole::Assistant, Some(text));
}

fn normalize_message(value: String) -> Option<String> {
    let visible = strip_hidden_context_blocks(value);
    let normalized = visible.split_whitespace().collect::<Vec<_>>().join(" ");
    let lower = normalized.to_ascii_lowercase();
    if lower.starts_with("# agents.md instructions") || lower.starts_with("<instructions>") {
        return None;
    }
    (!normalized.is_empty()).then(|| normalized.chars().take(HISTORY_MESSAGE_MAX_CHARS).collect())
}

fn strip_hidden_context_blocks(mut value: String) -> String {
    const HIDDEN: [(&str, &str); 3] = [
        ("system-reminder", "</system-reminder>"),
        ("codex_internal_context", "</codex_internal_context>"),
        ("goal_context", "</goal_context>"),
    ];
    loop {
        let lower = value.to_ascii_lowercase();
        let next = HIDDEN
            .iter()
            .filter_map(|(name, close)| find_open_tag(&lower, name).map(|start| (start, *close)))
            .min_by_key(|(start, _)| *start);
        let Some((start, close)) = next else {
            return value;
        };
        let Some(open_end) = lower[start..].find('>').map(|offset| start + offset + 1) else {
            value.truncate(start);
            return value;
        };
        let Some(close_start) = lower[open_end..]
            .find(close)
            .map(|offset| open_end + offset)
        else {
            value.truncate(start);
            return value;
        };
        value.replace_range(start..close_start + close.len(), " ");
    }
}

fn find_open_tag(value: &str, name: &str) -> Option<usize> {
    let needle = format!("<{name}");
    let mut from = 0;
    while let Some(offset) = value[from..].find(&needle) {
        let start = from + offset;
        let boundary = value.as_bytes().get(start + needle.len()).copied();
        if boundary.is_none_or(|byte| byte == b'>' || byte.is_ascii_whitespace()) {
            return Some(start);
        }
        from = start + needle.len();
    }
    None
}

fn normalize_title(value: String) -> Option<String> {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    (!normalized.is_empty()).then(|| normalized.chars().take(HISTORY_TITLE_MAX_CHARS).collect())
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum HistoryAdapterError {
    #[error("history metadata is required for this provider")]
    MissingMetadata,
    #[error("history metadata is not a JSON object")]
    InvalidMetadata,
    #[error("history metadata exceeds the supported bound")]
    MetadataTooLarge,
    #[error("history transcript exceeds the supported bound")]
    TranscriptTooLarge,
    #[error("history transcript is not the required JSON object")]
    InvalidTranscript,
    #[error("history session ID is empty, unsafe, or too large")]
    InvalidSessionId,
    #[error("provider history belongs to an internal worker session")]
    ExcludedProviderSession,
    #[error("history adapter is unavailable for {0}")]
    UnsupportedAdapter(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(value: &str) -> AdapterId {
        AdapterId::new(value).unwrap()
    }

    #[test]
    fn parses_grok_summary_and_user_query_envelope() {
        let session = parse_history(
            &id("grok"),
            &HistoryDocument {
                session_id_hint: "fallback".to_owned(),
                metadata_json: Some(
                    r#"{"info":{"id":"g1","cwd":"/repo"},"current_model_id":"grok-4","num_chat_messages":2}"#
                        .to_owned(),
                ),
                transcript: concat!(
                    r#"{"type":"user","content":"prefix <user_query>fix tests</user_query> suffix"}"#,
                    "\n",
                    r#"{"type":"assistant","content":"done"}"#
                )
                .to_owned(),
            },
        )
        .unwrap();
        assert_eq!(session.session_id, "g1");
        assert_eq!(session.title.as_deref(), Some("fix tests"));
        assert_eq!(session.model.as_deref(), Some("grok-4"));
        assert_eq!(session.message_count, 2);
    }

    #[test]
    fn parses_claude_titles_usage_and_injected_user_turns() {
        let transcript = [
            r#"{"type":"user","sessionId":"claude-1","cwd":"/repo","isMeta":true,"message":{"content":"<system-reminder>internal</system-reminder>"}}"#,
            r#"{"type":"user","sessionId":"claude-1","message":{"content":"fix the tests"}}"#,
            r#"{"type":"assistant","sessionId":"claude-1","message":{"model":"claude-sonnet","content":[{"type":"text","text":"done"}],"usage":{"input_tokens":2,"output_tokens":3,"cache_read_input_tokens":4,"cache_creation_input_tokens":5}}}"#,
            r#"{"type":"ai-title","sessionId":"claude-1","aiTitle":"Generated title"}"#,
            r#"{"type":"ai-title","sessionId":"claude-1","aiTitle":"   "}"#,
        ]
        .join("\n");
        let session = parse_history(
            &id("claude-code"),
            &HistoryDocument {
                session_id_hint: "fallback".to_owned(),
                metadata_json: None,
                transcript,
            },
        )
        .unwrap();
        assert_eq!(session.session_id, "claude-1");
        assert_eq!(session.title.as_deref(), Some("Generated title"));
        assert_eq!(session.cwd.as_deref(), Some("/repo"));
        assert_eq!(session.model.as_deref(), Some("claude-sonnet"));
        assert_eq!(session.total_tokens, 14);
        assert!(session.total_tokens_observed);
        assert_eq!(session.message_count, 2);
        assert_eq!(session.completed_turn_count, Some(0));
    }

    #[test]
    fn history_metrics_distinguish_unknown_token_total_from_zero() {
        let without_usage = parse_history(
            &id("claude-code"),
            &HistoryDocument {
                session_id_hint: "without-usage".to_owned(),
                metadata_json: None,
                transcript: r#"{"type":"assistant","message":{"content":"done"}}"#.to_owned(),
            },
        )
        .unwrap();
        assert_eq!(without_usage.total_tokens, 0);
        assert!(!without_usage.total_tokens_observed);

        let observed_zero = parse_history(
            &id("claude-code"),
            &HistoryDocument {
                session_id_hint: "observed-zero".to_owned(),
                metadata_json: None,
                transcript: r#"{"type":"assistant","message":{"content":"done","usage":{"input_tokens":0,"output_tokens":0}}}"#.to_owned(),
            },
        )
        .unwrap();
        assert_eq!(observed_zero.total_tokens, 0);
        assert!(observed_zero.total_tokens_observed);
    }

    #[test]
    fn claude_history_excludes_tool_results_tools_and_thinking_from_messages() {
        let transcript = [
            r#"{"type":"user","sessionId":"claude-visible","cwd":"/repo","isMeta":true,"message":{"content":"META_TEXT_SENTINEL"}}"#,
            r#"{"type":"user","sessionId":"claude-visible","cwd":"/repo","message":{"content":[{"type":"tool_result","content":"TOOL_OUTPUT_SENTINEL"},{"type":"text","text":"visible question"}]}}"#,
            r#"{"type":"assistant","sessionId":"claude-visible","message":{"content":[{"type":"thinking","thinking":"THINKING_SENTINEL"},{"type":"tool_use","name":"read","input":{"path":"TOOL_INPUT_SENTINEL"}},{"type":"text","text":"visible answer"}]}}"#,
        ]
        .join("\n");
        let session = parse_history(
            &id("claude-code"),
            &HistoryDocument {
                session_id_hint: "fallback".to_owned(),
                metadata_json: None,
                transcript,
            },
        )
        .unwrap();

        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].text, "visible question");
        assert_eq!(session.messages[1].text, "visible answer");
        let projected = session
            .messages
            .iter()
            .map(|message| message.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        for sentinel in [
            "TOOL_OUTPUT_SENTINEL",
            "THINKING_SENTINEL",
            "TOOL_INPUT_SENTINEL",
            "META_TEXT_SENTINEL",
        ] {
            assert!(!projected.contains(sentinel));
        }
    }

    #[test]
    fn claude_history_excludes_meta_and_harness_user_turns_from_title_and_messages() {
        let transcript = [
            r#"{"type":"user","sessionId":"claude-private","cwd":"/repo","isMeta":true,"message":{"content":"META_TITLE_SENTINEL"}}"#,
            r#"{"type":"user","sessionId":"claude-private","message":{"content":"This session is being continued from a previous conversation HARNESS_TITLE_SENTINEL"}}"#,
            r#"{"type":"assistant","sessionId":"claude-private","message":{"content":[{"type":"text","text":"visible answer"}]}}"#,
        ]
        .join("\n");
        let session = parse_history(
            &id("claude-code"),
            &HistoryDocument {
                session_id_hint: "fallback".to_owned(),
                metadata_json: None,
                transcript,
            },
        )
        .unwrap();

        assert_eq!(session.title, None);
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].text, "visible answer");
        let projected = format!(
            "{} {}",
            session.title.as_deref().unwrap_or_default(),
            session
                .messages
                .iter()
                .map(|message| message.text.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        );
        for sentinel in ["META_TITLE_SENTINEL", "HARNESS_TITLE_SENTINEL"] {
            assert!(!projected.contains(sentinel));
        }
    }

    #[test]
    fn claude_completed_turn_count_requires_distinct_valid_end_turn_text_messages() {
        let oversized_id_record = serde_json::json!({
            "type": "assistant",
            "message": {
                "id": "x".repeat(HISTORY_PROVIDER_MESSAGE_ID_MAX_BYTES + 1),
                "stop_reason": "end_turn",
                "content": [{"type": "text", "text": "oversized id"}],
            },
        })
        .to_string();
        let transcript = [[
            r#"{"type":"assistant","message":{"id":"msg-1","stop_reason":"end_turn","content":[{"type":"text","text":"first answer"}]}}"#,
            r#"{"type":"assistant","message":{"id":"msg-1","stop_reason":"end_turn","content":[{"type":"text","text":"duplicate answer"}]}}"#,
            r#"{"type":"assistant","message":{"id":"msg_2","stop_reason":"end_turn","content":[{"type":"thinking","thinking":"private"},{"type":"tool_use","id":"tool-1","name":"read"},{"type":"text","text":"second answer"}]}}"#,
            r#"{"type":"assistant","message":{"id":"msg-tool","stop_reason":"end_turn","content":[{"type":"tool_use","id":"tool-2","name":"write"}]}}"#,
            r#"{"type":"assistant","message":{"id":"msg-thinking","stop_reason":"end_turn","content":[{"type":"thinking","thinking":"private"}]}}"#,
            r#"{"type":"assistant","message":{"id":"msg-hidden","stop_reason":"end_turn","content":[{"type":"text","text":"<system-reminder>internal</system-reminder>"}]}}"#,
            r#"{"type":"assistant","message":{"id":"msg-not-finished","stop_reason":"tool_use","content":[{"type":"text","text":"not finished"}]}}"#,
            r#"{"type":"user","message":{"id":"msg-user","stop_reason":"end_turn","content":[{"type":"text","text":"user text"}]}}"#,
            r#"{"type":"assistant","isMeta":true,"message":{"id":"msg-meta","stop_reason":"end_turn","content":[{"type":"text","text":"metadata"}]}}"#,
            r#"{"type":"assistant","message":{"id":"bad/id","stop_reason":"end_turn","content":[{"type":"text","text":"invalid id"}]}}"#,
            r#"{"type":"assistant","message":{"id":42,"stop_reason":"end_turn","content":[{"type":"text","text":"non-string id"}]}}"#,
        ]
        .join("\n"), oversized_id_record]
        .join("\n");

        let session = parse_history(
            &id("claude-code"),
            &HistoryDocument {
                session_id_hint: "claude-completed-turns".to_owned(),
                metadata_json: None,
                transcript,
            },
        )
        .unwrap();

        assert_eq!(session.completed_turn_count, Some(2));
    }

    #[test]
    fn parses_codex_cumulative_usage_and_excludes_worker_transcripts() {
        let transcript = [
            r#"{"type":"session_meta","payload":{"id":"codex-1","thread_source":"user","cwd":"/repo"}}"#,
            r#"{"type":"turn_context","payload":{"model":"gpt-5","cwd":"/repo/new"}}"#,
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"fix tests"}}"#,
            r#"{"type":"event_msg","payload":{"type":"agent_message","message":"working"}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"developer","content":"internal"}}"#,
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":6,"output_tokens":4,"total_tokens":10}}}}"#,
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":9,"output_tokens":6,"total_tokens":15}}}}"#,
        ]
        .join("\n");
        let session = parse_history(
            &id("codex"),
            &HistoryDocument {
                session_id_hint: "fallback".to_owned(),
                metadata_json: Some(r#"{"indexed_title":"Indexed title"}"#.to_owned()),
                transcript,
            },
        )
        .unwrap();
        assert_eq!(session.session_id, "codex-1");
        assert_eq!(session.title.as_deref(), Some("Indexed title"));
        assert_eq!(session.cwd.as_deref(), Some("/repo/new"));
        assert_eq!(session.model.as_deref(), Some("gpt-5"));
        assert_eq!(session.total_tokens, 15);
        assert_eq!(session.message_count, 3);
        assert_eq!(session.completed_turn_count, None);

        let worker = parse_history(
            &id("codex"),
            &HistoryDocument {
                session_id_hint: "worker".to_owned(),
                metadata_json: None,
                transcript: r#"{"type":"session_meta","payload":{"thread_source":"subagent"}}"#
                    .to_owned(),
            },
        );
        assert_eq!(worker, Err(HistoryAdapterError::ExcludedProviderSession));
    }

    #[test]
    fn parses_gemini_json_and_jsonl_session_formats() {
        let legacy = parse_history(
            &id("gemini"),
            &HistoryDocument {
                session_id_hint: "fallback".to_owned(),
                metadata_json: None,
                transcript: r#"{"sessionId":"gemini-json","messages":[{"type":"user","content":"question"},{"type":"gemini","content":"answer","model":"gemini-3","tokens":{"input":2,"output":3}}]}"#.to_owned(),
            },
        )
        .unwrap();
        assert_eq!(legacy.session_id, "gemini-json");
        assert_eq!(legacy.total_tokens, 5);

        let jsonl = parse_history(
            &id("gemini"),
            &HistoryDocument {
                session_id_hint: "fallback".to_owned(),
                metadata_json: None,
                transcript: [
                    r#"{"sessionId":"gemini-jsonl","type":"user","content":"question"}"#,
                    r#"{"type":"gemini","content":"answer","model":"gemini-3","tokens":{"totalTokens":7}}"#,
                ]
                .join("\n"),
            },
        )
        .unwrap();
        assert_eq!(jsonl.session_id, "gemini-jsonl");
        assert_eq!(jsonl.message_count, 2);
        assert_eq!(jsonl.total_tokens, 7);
    }

    #[test]
    fn parses_antigravity_user_envelopes_and_planner_responses() {
        let session = parse_history(
            &id("antigravity"),
            &HistoryDocument {
                session_id_hint: "conversation-1".to_owned(),
                metadata_json: None,
                transcript: [
                    r#"{"source":"USER_EXPLICIT","type":"USER_INPUT","content":"prefix<USER_REQUEST>ship it</USER_REQUEST>suffix"}"#,
                    r#"{"source":"MODEL","type":"PLANNER_RESPONSE","content":"done"}"#,
                    r#"{"source":"MODEL","type":"TOOL_RESPONSE","content":"hidden"}"#,
                ]
                .join("\n"),
            },
        )
        .unwrap();
        assert_eq!(session.title.as_deref(), Some("ship it"));
        assert_eq!(session.message_count, 2);
        assert_eq!(session.messages[1].text, "done");
    }

    #[test]
    fn parses_kimi_wire_chunks_and_excludes_injections() {
        let transcript = [
            r#"{"type":"config.update","modelAlias":"kimi-k2"}"#,
            r#"{"type":"context.append_message","message":{"role":"user","origin":{"kind":"injection"},"content":"ignore"}}"#,
            r#"{"type":"context.append_message","message":{"role":"user","origin":{"kind":"user"},"content":"hello"}}"#,
            r#"{"type":"context.append_loop_event","event":{"type":"content.part","part":{"type":"text","text":"world"}}}"#,
            r#"{"type":"context.append_loop_event","event":{"type":"step.end"}}"#,
            r#"{"type":"usage.record","usage":{"inputOther":2,"output":3,"inputCacheRead":4,"inputCacheCreation":5}}"#,
        ]
        .join("\n");
        let session = parse_history(
            &id("kimi"),
            &HistoryDocument {
                session_id_hint: "session_1".to_owned(),
                metadata_json: Some(r#"{"lastPrompt":"fallback"}"#.to_owned()),
                transcript,
            },
        )
        .unwrap();
        assert_eq!(session.message_count, 2);
        assert_eq!(session.total_tokens, 14);
        assert_eq!(session.model.as_deref(), Some("kimi-k2"));
    }

    #[test]
    fn parses_qwen_active_real_user_and_assistant_output_chain() {
        let transcript = [
            r#"{"uuid":"u1","parentUuid":null,"sessionId":"11111111-1111-4111-8111-111111111111","timestamp":"2026-08-10T00:00:00Z","type":"user","provenance":"real_user","cwd":"C:/repo","version":"0.21.6","gitBranch":"main","message":{"role":"user","parts":[{"text":"ship the adapter"}]}}"#,
            r#"{"uuid":"ignored-user","parentUuid":"u1","sessionId":"11111111-1111-4111-8111-111111111111","type":"user","provenance":"system","cwd":"C:/repo","message":{"role":"user","parts":[{"text":"internal notification"}]}}"#,
            r#"{"uuid":"notification","parentUuid":"ignored-user","sessionId":"11111111-1111-4111-8111-111111111111","type":"user","subtype":"notification","provenance":"system","cwd":"C:/repo","message":{"role":"user","parts":[{"text":"background"}]}}"#,
            r#"{"uuid":"a1","parentUuid":"notification","sessionId":"11111111-1111-4111-8111-111111111111","type":"assistant","provenance":"assistant_output","cwd":"C:/repo","model":"qwen3-coder","usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":4,"totalTokenCount":7},"message":{"role":"model","parts":[{"text":"private chain of thought","thought":true},{"text":"adapter ready"},{"functionCall":{"name":"hidden_tool"}}]}}"#,
            r#"{"uuid":"stale","parentUuid":"u1","sessionId":"11111111-1111-4111-8111-111111111111","type":"user","provenance":"real_user","cwd":"C:/repo","message":{"role":"user","parts":[{"text":"stale branch"}]}}"#,
            r#"{"uuid":"injected","parentUuid":"a1","sessionId":"11111111-1111-4111-8111-111111111111","type":"user","provenance":"real_user","cwd":"C:/repo","message":{"role":"user","parts":[{"text":"<teammate-message>hidden harness</teammate-message>"}]}}"#,
            r#"{"uuid":"title","parentUuid":"injected","sessionId":"11111111-1111-4111-8111-111111111111","type":"system","subtype":"custom_title","cwd":"C:/repo","version":"0.21.6","systemPayload":{"customTitle":"Qwen context pack","titleSource":"manual"}}"#,
        ]
        .join("\n");
        let session = parse_history(
            &id("qwen-code"),
            &HistoryDocument {
                session_id_hint: "fallback".to_owned(),
                metadata_json: None,
                transcript,
            },
        )
        .unwrap();

        assert_eq!(session.session_id, "11111111-1111-4111-8111-111111111111");
        assert_eq!(session.title.as_deref(), Some("Qwen context pack"));
        assert_eq!(session.cwd.as_deref(), Some("C:/repo"));
        assert_eq!(session.model.as_deref(), Some("qwen3-coder"));
        assert_eq!(session.total_tokens, 7);
        assert_eq!(session.message_count, 3);
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].text, "ship the adapter");
        assert_eq!(session.messages[1].text, "adapter ready");
        assert!(!session.messages.iter().any(|message| {
            message.text.contains("private chain of thought")
        }));
    }

    #[test]
    fn parses_copilot_droid_and_cursor_ndjson_shapes() {
        let fixtures = [
            (
                "copilot",
                concat!(
                    r#"{"type":"session.start","data":{"sessionId":"c1"}}"#,
                    "\n",
                    r#"{"type":"user.message","data":{"content":"question"}}"#,
                    "\n",
                    r#"{"type":"assistant.message","data":{"content":"answer"}}"#
                ),
                "c1",
            ),
            (
                "droid",
                concat!(
                    r#"{"type":"session_start","id":"d1","cwd":"/repo"}"#,
                    "\n",
                    r#"{"type":"message","role":"user","text":"question"}"#,
                    "\n",
                    r#"{"type":"completion","finalText":"answer","usage":{"input":1,"output":2}}"#
                ),
                "d1",
            ),
            (
                "cursor",
                concat!(
                    r#"{"role":"user","message":{"content":"question"}}"#,
                    "\n",
                    r#"{"role":"assistant","content":"answer"}"#
                ),
                "hint",
            ),
        ];
        for (adapter, transcript, expected_id) in fixtures {
            let session = parse_history(
                &id(adapter),
                &HistoryDocument {
                    session_id_hint: "hint".to_owned(),
                    metadata_json: None,
                    transcript: transcript.to_owned(),
                },
            )
            .unwrap();
            assert_eq!(session.session_id, expected_id);
            assert_eq!(session.message_count, 2);
            assert_eq!(session.title.as_deref(), Some("question"));
        }
    }

    #[test]
    fn copilot_history_extracts_trusted_folder_and_only_usage_metrics() {
        let session = parse_history(
            &id("copilot"),
            &HistoryDocument {
                session_id_hint: "copilot-1".to_owned(),
                metadata_json: None,
                transcript: [
                    r#"{"type":"session.info","data":{"message":"Folder /repo has been added to trusted folders."}}"#,
                    r#"{"type":"session.shutdown","data":{"currentTokens":5,"modelMetrics":{"model-a":{"usage":{"total":7},"latency":999},"unrelated":42}}}"#,
                ]
                .join("\n"),
            },
        )
        .unwrap();
        assert_eq!(session.cwd.as_deref(), Some("/repo"));
        assert_eq!(session.total_tokens, 12);
    }

    #[test]
    fn parses_opencode_legacy_and_sqlite_authority_shapes() {
        let session = parse_history(
            &id("opencode"),
            &HistoryDocument {
                session_id_hint: "fallback".to_owned(),
                metadata_json: Some(
                    r#"{"id":"ses_1","title":"OpenCode title","directory":"/repo","model_json":"{\"id\":\"glm-5.2\"}","tokens_input":5,"tokens_output":3,"tokens_reasoning":2,"message_count":3}"#
                        .to_owned(),
                ),
                transcript: [
                    r#"{"role":"user","part_data":"{\"type\":\"text\",\"text\":\"Plan the work\"}","summary_title":"fallback title"}"#,
                    r#"{"role":"assistant","content":[{"type":"text","text":"Done"}],"tokens":{"input":1,"output":2}}"#,
                ]
                .join("\n"),
            },
        )
        .unwrap();
        assert_eq!(session.session_id, "ses_1");
        assert_eq!(session.title.as_deref(), Some("OpenCode title"));
        assert_eq!(session.cwd.as_deref(), Some("/repo"));
        assert_eq!(session.model.as_deref(), Some("glm-5.2"));
        assert_eq!(session.message_count, 3);
        assert_eq!(session.total_tokens, 13);
        assert_eq!(session.messages.len(), 2);
    }

    #[test]
    fn parses_pi_omp_and_openclaw_message_graphs() {
        let transcript = [
            r#"{"type":"session","id":"graph-1","cwd":"/repo"}"#,
            r#"{"type":"model_change","modelId":"model-a"}"#,
            r#"{"type":"message","message":{"role":"user","content":"question"}}"#,
            r#"{"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"answer"}],"model":"model-b","usage":{"input":2,"output":3,"cacheRead":4}}}"#,
        ]
        .join("\n");
        for adapter in ["pi", "omp", "openclaw"] {
            let session = parse_history(
                &id(adapter),
                &HistoryDocument {
                    session_id_hint: "fallback".to_owned(),
                    metadata_json: None,
                    transcript: transcript.clone(),
                },
            )
            .unwrap();
            assert_eq!(session.session_id, "graph-1");
            assert_eq!(session.cwd.as_deref(), Some("/repo"));
            assert_eq!(session.model.as_deref(), Some("model-b"));
            assert_eq!(session.total_tokens, 9);
            assert_eq!(session.message_count, 2);
        }
    }

    #[test]
    fn parses_hermes_rovo_and_devin_object_contracts() {
        let hermes = parse_history(
            &id("hermes"),
            &HistoryDocument {
                session_id_hint: "fallback".to_owned(),
                metadata_json: None,
                transcript: r#"{"session_id":"hermes-1","model":"qwen","cwd":"/repo","messages":[{"role":"user","content":"question"},{"role":"assistant","content":"answer"}]}"#.to_owned(),
            },
        )
        .unwrap();
        assert_eq!(hermes.session_id, "hermes-1");
        assert_eq!(hermes.message_count, 2);

        let rovo = parse_history(
            &id("rovo"),
            &HistoryDocument {
                session_id_hint: "rovo-dir-1".to_owned(),
                metadata_json: Some(
                    r#"{"title":"Rovo task","workspace_path":"/repo"}"#.to_owned(),
                ),
                transcript: r#"{"messages":[{"role":"user","content":"question"}],"message_history":[{"kind":"response","parts":[{"part_kind":"text","content":"answer"},{"part_kind":"tool","content":"hidden"}]}]}"#.to_owned(),
            },
        )
        .unwrap();
        assert_eq!(rovo.session_id, "rovo-dir-1");
        assert_eq!(rovo.title.as_deref(), Some("Rovo task"));
        assert_eq!(rovo.message_count, 2);
        assert_eq!(rovo.messages[1].text, "answer");

        let devin = parse_history(
            &id("devin"),
            &HistoryDocument {
                session_id_hint: "fallback".to_owned(),
                metadata_json: None,
                transcript: r#"{"session_id":"devin-1","working_directory":"/repo","agent":{"model_name":"sonnet"},"steps":[{"metadata":{"is_user_input":true,"total_input_tokens":2},"message":{"content":"question"}},{"role":"assistant","metadata":{"metrics":{"output_tokens":3,"cache_read_tokens":4}},"content":"answer"}]}"#.to_owned(),
            },
        )
        .unwrap();
        assert_eq!(devin.session_id, "devin-1");
        assert_eq!(devin.model.as_deref(), Some("sonnet"));
        assert_eq!(devin.total_tokens, 9);
        assert_eq!(devin.message_count, 2);
    }

    #[test]
    fn malformed_lines_are_skipped_but_bounds_are_enforced() {
        let session = parse_history(
            &id("cursor"),
            &HistoryDocument {
                session_id_hint: "c1".to_owned(),
                metadata_json: None,
                transcript: "not-json\n{\"role\":\"user\",\"content\":\"ok\"}".to_owned(),
            },
        )
        .unwrap();
        assert_eq!(session.message_count, 1);

        let oversized = HistoryDocument {
            session_id_hint: "c1".to_owned(),
            metadata_json: None,
            transcript: "x".repeat(HISTORY_DOCUMENT_MAX_BYTES + 1),
        };
        assert_eq!(
            parse_history(&id("cursor"), &oversized),
            Err(HistoryAdapterError::TranscriptTooLarge)
        );
    }

    #[test]
    fn retained_history_is_utf8_safe_and_bounded_without_losing_total_count() {
        let long_text = format!("привет{}", "界".repeat(HISTORY_MESSAGE_MAX_CHARS));
        let transcript = (0..=HISTORY_STORED_MESSAGES_MAX)
            .map(|index| {
                serde_json::json!({
                    "role": if index % 2 == 0 { "user" } else { "assistant" },
                    "content": long_text
                })
                .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let session = parse_history(
            &id("cursor"),
            &HistoryDocument {
                session_id_hint: "c1".to_owned(),
                metadata_json: None,
                transcript,
            },
        )
        .unwrap();
        assert_eq!(
            session.message_count,
            u64::try_from(HISTORY_STORED_MESSAGES_MAX + 1).unwrap()
        );
        assert_eq!(session.messages.len(), HISTORY_STORED_MESSAGES_MAX);
        assert_eq!(
            session.messages[0].text.chars().count(),
            HISTORY_MESSAGE_MAX_CHARS
        );
        assert!(session.messages[0].text.starts_with("привет"));
    }

    #[test]
    fn retained_history_keeps_the_most_recent_tail_in_chronological_order() {
        let transcript = (0..HISTORY_STORED_MESSAGES_MAX + 2)
            .map(|index| {
                serde_json::json!({
                    "role": if index % 2 == 0 { "user" } else { "assistant" },
                    "content": format!("message-{index}")
                })
                .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let session = parse_history(
            &id("cursor"),
            &HistoryDocument {
                session_id_hint: "cursor-tail".to_owned(),
                metadata_json: None,
                transcript,
            },
        )
        .unwrap();

        assert_eq!(
            session.message_count,
            u64::try_from(HISTORY_STORED_MESSAGES_MAX + 2).unwrap()
        );
        assert_eq!(session.messages.len(), HISTORY_STORED_MESSAGES_MAX);
        assert_eq!(session.messages[0].text, "message-2");
        assert_eq!(
            session.messages.last().unwrap().text,
            format!("message-{}", HISTORY_STORED_MESSAGES_MAX + 1)
        );
    }

    #[test]
    fn split_metadata_contracts_reject_missing_or_invalid_json() {
        let missing = HistoryDocument {
            session_id_hint: "g1".to_owned(),
            metadata_json: None,
            transcript: String::new(),
        };
        assert_eq!(
            parse_history(&id("grok"), &missing),
            Err(HistoryAdapterError::MissingMetadata)
        );

        let invalid = HistoryDocument {
            session_id_hint: "k1".to_owned(),
            metadata_json: Some("[]".to_owned()),
            transcript: String::new(),
        };
        assert_eq!(
            parse_history(&id("kimi"), &invalid),
            Err(HistoryAdapterError::InvalidMetadata)
        );
    }

    #[test]
    fn source_contracts_keep_related_reads_in_the_effect_owning_shell() {
        let opencode = history_source_variants(&id("opencode")).unwrap();
        assert_eq!(opencode.len(), 2);
        assert!(opencode.iter().any(|variant| {
            variant.layout == HistorySourceLayout::ReadOnlySqliteProjection
                && variant.requires_readonly_database
        }));
        assert!(opencode.iter().any(|variant| {
            variant.layout == HistorySourceLayout::SessionJsonWithSiblingMessageJson
                && variant.requires_sibling_reads
        }));

        let codex = history_source_variants(&id("codex")).unwrap();
        assert_eq!(codex.len(), 1);
        assert!(codex[0].requires_auxiliary_index);
        assert!(matches!(
            history_source_variants(&id("unknown")),
            Err(HistoryAdapterError::UnsupportedAdapter(_))
        ));
    }

    #[test]
    fn hidden_harness_context_is_not_retained_as_history_text() {
        let session = parse_history(
            &id("cursor"),
            &HistoryDocument {
                session_id_hint: "cursor-1".to_owned(),
                metadata_json: None,
                transcript: [
                    r#"{"role":"user","content":"<system-reminder secret=\"x\">internal</system-reminder> real request"}"#,
                    r#"{"role":"assistant","content":"<goal_context>private</goal_context>answer"}"#,
                ]
                .join("\n"),
            },
        )
        .unwrap();
        assert_eq!(session.messages[0].text, "real request");
        assert_eq!(session.messages[1].text, "answer");
    }
}
