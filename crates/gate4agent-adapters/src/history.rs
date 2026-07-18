use gate4agent_types::AdapterId;
use serde_json::{Map, Value};
use thiserror::Error;

pub const HISTORY_METADATA_MAX_BYTES: usize = 1_048_576;
pub const HISTORY_DOCUMENT_MAX_BYTES: usize = 8_388_608;
pub const HISTORY_STORED_MESSAGES_MAX: usize = 256;
pub const HISTORY_MESSAGE_MAX_CHARS: usize = 4_096;
const HISTORY_TITLE_MAX_CHARS: usize = 96;

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
    pub total_tokens: u64,
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

pub fn parse_history(
    adapter_id: &AdapterId,
    document: &HistoryDocument,
) -> Result<HistorySession, HistoryAdapterError> {
    validate_document(document)?;
    match adapter_id.as_str() {
        "grok" => parse_grok(document),
        "kimi" => parse_kimi(document),
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
                    session.total_tokens = session.total_tokens.saturating_add(sum_named_numbers(
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
                    .and_then(|value| string(value, &["trustedFolder", "cwd"]))
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
                    session.total_tokens = session
                        .total_tokens
                        .saturating_add(u64_value(data, &["currentTokens"]).unwrap_or(0));
                    if let Some(metrics) = data.get("modelMetrics") {
                        session.total_tokens = session
                            .total_tokens
                            .saturating_add(sum_numeric_leaves(metrics));
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
                    session.total_tokens = session
                        .total_tokens
                        .saturating_add(sum_numeric_leaves(usage));
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

struct SessionBuilder {
    session_id: String,
    title: Option<String>,
    cwd: Option<String>,
    model: Option<String>,
    message_count: u64,
    total_tokens: u64,
    messages: Vec<HistoryMessage>,
}

impl SessionBuilder {
    fn new(session_id: String) -> Self {
        Self {
            session_id,
            title: None,
            cwd: None,
            model: None,
            message_count: 0,
            total_tokens: 0,
            messages: Vec::new(),
        }
    }

    fn push(&mut self, role: HistoryRole, text: Option<String>) {
        self.message_count = self.message_count.saturating_add(1);
        let text = text.and_then(normalize_message);
        if role == HistoryRole::User && self.title.is_none() {
            self.title = text.clone().and_then(normalize_title);
        }
        if self.messages.len() < HISTORY_STORED_MESSAGES_MAX {
            if let Some(text) = text {
                self.messages.push(HistoryMessage { role, text });
            }
        }
    }

    fn finish(self) -> HistorySession {
        HistorySession {
            session_id: self.session_id,
            title: self.title,
            cwd: self.cwd,
            model: self.model,
            message_count: self.message_count,
            total_tokens: self.total_tokens,
            messages: self.messages,
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

fn sum_named_numbers(record: &Map<String, Value>, keys: &[&str]) -> u64 {
    keys.iter()
        .filter_map(|key| u64_value(record, &[*key]))
        .fold(0, u64::saturating_add)
}

fn sum_numeric_leaves(value: &Value) -> u64 {
    match value {
        Value::Number(number) => number.as_u64().unwrap_or(0),
        Value::Array(values) => values
            .iter()
            .map(sum_numeric_leaves)
            .fold(0, u64::saturating_add),
        Value::Object(values) => values
            .values()
            .map(sum_numeric_leaves)
            .fold(0, u64::saturating_add),
        _ => 0,
    }
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

fn content_text(value: &Value) -> Option<String> {
    content_text_at_depth(value, 0).and_then(normalize_message)
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
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    (!normalized.is_empty()).then(|| normalized.chars().take(HISTORY_MESSAGE_MAX_CHARS).collect())
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
    #[error("history session ID is empty, unsafe, or too large")]
    InvalidSessionId,
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
}
