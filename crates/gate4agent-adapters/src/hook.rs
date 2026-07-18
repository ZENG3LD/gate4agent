use gate4agent_types::{AdapterId, ProviderEvent, TokenUsage};
use serde_json::{Map, Value};
use thiserror::Error;

pub const HOOK_EVENT_NAME_MAX_BYTES: usize = 128;
pub const HOOK_PAYLOAD_MAX_BYTES: usize = 1_048_576;
pub const HOOK_TEXT_MAX_CHARS: usize = 65_536;

/// Converts a provider hook payload into transport-neutral provider events.
///
/// The function is deliberately stateless and performs no hook installation or
/// network I/O. Callers own authentication, ordering, and tool-call correlation.
pub fn normalize_hook_event(
    adapter_id: &AdapterId,
    event_name: &str,
    payload: &Value,
) -> Result<Vec<ProviderEvent>, HookAdapterError> {
    if event_name.is_empty()
        || event_name.len() > HOOK_EVENT_NAME_MAX_BYTES
        || event_name.chars().any(char::is_control)
    {
        return Err(HookAdapterError::InvalidEventName);
    }
    let record = payload
        .as_object()
        .ok_or(HookAdapterError::PayloadMustBeObject)?;
    if serde_json::to_vec(payload)
        .map_err(|_| HookAdapterError::PayloadTooLarge)?
        .len()
        > HOOK_PAYLOAD_MAX_BYTES
    {
        return Err(HookAdapterError::PayloadTooLarge);
    }

    match adapter_id.as_str() {
        "grok" => normalize_grok(event_name, record),
        "kimi" => normalize_kimi(event_name, record),
        "copilot" => normalize_copilot(event_name, record),
        "droid" => normalize_droid(event_name, record),
        "cursor" => normalize_cursor(event_name, record),
        id => Err(HookAdapterError::UnsupportedAdapter(id.to_owned())),
    }
}

fn normalize_grok(
    event_name: &str,
    payload: &Map<String, Value>,
) -> Result<Vec<ProviderEvent>, HookAdapterError> {
    let event = snake_event_name(event_name);
    match event.as_str() {
        "session_start" => Ok(session_started(payload, &["sessionId", "session_id"])),
        "pre_tool_use" if is_ask_user_question(tool_name(payload).as_deref()) => {
            Ok(vec![approval_event(payload)])
        }
        "pre_tool_use" => Ok(vec![tool_started(payload)]),
        "post_tool_use" => Ok(vec![tool_completed(payload, false)]),
        "post_tool_use_failure" => Ok(vec![tool_completed(payload, true)]),
        "stop" | "stop_failure" | "session_end" => Ok(turn_completed(payload)),
        "notification" if is_permission_message(string(payload, &["message"]).as_deref()) => {
            Ok(vec![approval_event(payload)])
        }
        "notification" if is_idle_message(string(payload, &["message"]).as_deref()) => {
            Ok(vec![ProviderEvent::Ready])
        }
        _ => Ok(Vec::new()),
    }
}

fn normalize_kimi(
    event_name: &str,
    payload: &Map<String, Value>,
) -> Result<Vec<ProviderEvent>, HookAdapterError> {
    match event_name {
        "SessionStart" => Ok(session_started(payload, &["session_id"])),
        "PreToolUse" if is_ask_user_question(tool_name(payload).as_deref()) => {
            Ok(vec![approval_event(payload)])
        }
        "PreToolUse" => Ok(vec![tool_started(payload)]),
        "PostToolUse" => Ok(vec![tool_completed(payload, false)]),
        "PostToolUseFailure" => Ok(vec![tool_completed(payload, true)]),
        "PermissionRequest" => Ok(vec![approval_event(payload)]),
        "Stop" | "StopFailure" => Ok(turn_completed(payload)),
        _ => Ok(Vec::new()),
    }
}

fn normalize_copilot(
    event_name: &str,
    payload: &Map<String, Value>,
) -> Result<Vec<ProviderEvent>, HookAdapterError> {
    let event = copilot_event_name(event_name);
    match event.as_str() {
        "SessionStart" => Ok(session_started_from_copilot(payload)),
        "PreToolUse" | "PermissionRequest" if is_ask_user(tool_name(payload).as_deref()) => {
            Ok(vec![approval_event(payload)])
        }
        "PreToolUse" | "PermissionRequest" => Ok(vec![tool_started(payload)]),
        "PostToolUse" => Ok(vec![tool_completed(payload, false)]),
        "PostToolUseFailure" => Ok(vec![tool_completed(payload, true)]),
        "Notification" if is_blocking_copilot_notification(payload) => {
            Ok(vec![approval_event(payload)])
        }
        "ErrorOccurred" if payload.get("recoverable") != Some(&Value::Bool(true)) => {
            Ok(vec![ProviderEvent::Error {
                message: bounded_string(
                    string(
                        payload,
                        &["error_message", "errorMessage", "error", "message"],
                    )
                    .unwrap_or_else(|| "Copilot hook reported an error".to_owned()),
                ),
            }])
        }
        "Stop" | "SessionEnd" => Ok(turn_completed(payload)),
        _ => Ok(Vec::new()),
    }
}

fn normalize_droid(
    event_name: &str,
    payload: &Map<String, Value>,
) -> Result<Vec<ProviderEvent>, HookAdapterError> {
    match event_name {
        "SessionStart" => Ok(session_started(payload, &["session_id"])),
        "PreToolUse" if is_ask_user(tool_name(payload).as_deref()) || is_high_risk(payload) => {
            Ok(vec![approval_event(payload)])
        }
        "PreToolUse" => Ok(vec![tool_started(payload)]),
        "PostToolUse" => Ok(vec![tool_completed(payload, false)]),
        "PermissionRequest" => Ok(vec![approval_event(payload)]),
        "Notification" if is_permission_message(string(payload, &["message"]).as_deref()) => {
            Ok(vec![approval_event(payload)])
        }
        "Notification" if is_idle_message(string(payload, &["message"]).as_deref()) => {
            Ok(vec![ProviderEvent::Ready])
        }
        "Stop" => Ok(turn_completed(payload)),
        _ => Ok(Vec::new()),
    }
}

fn normalize_cursor(
    event_name: &str,
    payload: &Map<String, Value>,
) -> Result<Vec<ProviderEvent>, HookAdapterError> {
    match event_name {
        "sessionStart" => Ok(session_started(payload, &["session_id", "sessionId"])),
        "preToolUse" => Ok(vec![tool_started(payload)]),
        "postToolUse" => Ok(vec![tool_completed(payload, false)]),
        "postToolUseFailure" => Ok(vec![tool_completed(payload, true)]),
        "beforeShellExecution" => Ok(vec![ProviderEvent::ToolStarted {
            id: tool_id(payload, "Shell"),
            name: "Shell".to_owned(),
            input_json: input_json(payload.get("command")),
        }]),
        "beforeMCPExecution" => {
            let name = tool_name(payload).unwrap_or_else(|| "MCP".to_owned());
            Ok(vec![ProviderEvent::ToolStarted {
                id: tool_id(payload, &name),
                name,
                input_json: input_json(first_value(payload, &["tool_input", "command", "url"])),
            }])
        }
        "afterAgentResponse" => Ok(string(payload, &["text"])
            .map(|text| {
                vec![ProviderEvent::Text {
                    text: bounded_string(text),
                    is_delta: false,
                }]
            })
            .unwrap_or_default()),
        "stop" | "sessionEnd" => Ok(turn_completed(payload)),
        _ => Ok(Vec::new()),
    }
}

fn session_started(payload: &Map<String, Value>, keys: &[&str]) -> Vec<ProviderEvent> {
    string(payload, keys)
        .and_then(normalize_provider_session_id)
        .map(|session_id| {
            vec![ProviderEvent::SessionStarted {
                session_id,
                model: bounded_string(string(payload, &["model", "model_id"]).unwrap_or_default()),
                tools: Vec::new(),
            }]
        })
        .unwrap_or_default()
}

fn session_started_from_copilot(payload: &Map<String, Value>) -> Vec<ProviderEvent> {
    let session_id = string(payload, &["sessionId", "session_id"]).or_else(|| {
        payload
            .get("data")
            .and_then(Value::as_object)
            .and_then(|data| string(data, &["sessionId", "session_id"]))
    });
    session_id
        .and_then(normalize_provider_session_id)
        .map(|session_id| {
            vec![ProviderEvent::SessionStarted {
                session_id,
                model: String::new(),
                tools: Vec::new(),
            }]
        })
        .unwrap_or_default()
}

fn tool_started(payload: &Map<String, Value>) -> ProviderEvent {
    let name = tool_name(payload).unwrap_or_else(|| "unknown".to_owned());
    ProviderEvent::ToolStarted {
        id: tool_id(payload, &name),
        name,
        input_json: input_json(first_value(
            payload,
            &["toolInput", "tool_input", "toolArgs", "input", "arguments"],
        )),
    }
}

fn tool_completed(payload: &Map<String, Value>, is_error: bool) -> ProviderEvent {
    let name = tool_name(payload).unwrap_or_else(|| "unknown".to_owned());
    let output = first_value(
        payload,
        &[
            "toolResponse",
            "tool_response",
            "toolResult",
            "tool_result",
            "toolOutput",
            "tool_output",
            "error",
            "message",
        ],
    )
    .map(value_text)
    .unwrap_or_default();
    ProviderEvent::ToolCompleted {
        id: tool_id(payload, &name),
        output: bounded_string(output),
        is_error,
        duration_ms: first_value(payload, &["duration_ms", "durationMs"]).and_then(Value::as_u64),
    }
}

fn approval_event(payload: &Map<String, Value>) -> ProviderEvent {
    ProviderEvent::ApprovalRequested {
        tool_name: tool_name(payload).unwrap_or_else(|| "approval".to_owned()),
        description: string(
            payload,
            &["description", "message", "body", "text", "title"],
        )
        .map(bounded_string),
    }
}

fn turn_completed(payload: &Map<String, Value>) -> Vec<ProviderEvent> {
    let mut events = Vec::new();
    if let Some(text) = string(
        payload,
        &[
            "lastAssistantMessage",
            "last_assistant_message",
            "finalText",
            "message",
        ],
    ) {
        events.push(ProviderEvent::Text {
            text: bounded_string(text),
            is_delta: false,
        });
    }
    events.push(ProviderEvent::TurnCompleted {
        usage: TokenUsage::default(),
        is_cumulative: false,
    });
    events
}

fn tool_name(payload: &Map<String, Value>) -> Option<String> {
    string(payload, &["toolName", "tool_name", "name"]).or_else(|| {
        payload
            .get("toolCall")
            .and_then(Value::as_object)
            .and_then(|call| string(call, &["name", "toolName", "tool_name"]))
    })
}

fn tool_id(payload: &Map<String, Value>, fallback: &str) -> String {
    bounded_string(
        string(
            payload,
            &[
                "tool_use_id",
                "toolUseId",
                "tool_call_id",
                "toolCallId",
                "id",
            ],
        )
        .unwrap_or_else(|| fallback.to_owned()),
    )
}

fn first_value<'a>(payload: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|key| payload.get(*key))
}

fn string(payload: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    first_value(payload, keys)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn input_json(value: Option<&Value>) -> String {
    let value = value.unwrap_or(&Value::Null);
    bounded_string(match value {
        Value::String(text) => text.clone(),
        _ => serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned()),
    })
}

fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        _ => serde_json::to_string(value).unwrap_or_default(),
    }
}

fn snake_event_name(value: &str) -> String {
    let mut normalized = String::with_capacity(value.len());
    let mut previous_lower_or_digit = false;
    for character in value.trim().chars() {
        if character.is_ascii_uppercase() {
            if previous_lower_or_digit {
                normalized.push('_');
            }
            normalized.push(character.to_ascii_lowercase());
            previous_lower_or_digit = false;
        } else if character == '-' || character.is_ascii_whitespace() {
            if !normalized.ends_with('_') {
                normalized.push('_');
            }
            previous_lower_or_digit = false;
        } else {
            normalized.push(character.to_ascii_lowercase());
            previous_lower_or_digit = character.is_ascii_lowercase() || character.is_ascii_digit();
        }
    }
    normalized
}

fn copilot_event_name(value: &str) -> String {
    match value {
        "sessionStart" => "SessionStart",
        "sessionEnd" => "SessionEnd",
        "userPromptSubmitted" | "userPromptSubmit" => "UserPromptSubmit",
        "preToolUse" => "PreToolUse",
        "postToolUse" => "PostToolUse",
        "postToolUseFailure" => "PostToolUseFailure",
        "agentStop" | "stop" => "Stop",
        "errorOccurred" => "ErrorOccurred",
        "permissionRequest" => "PermissionRequest",
        "notification" => "Notification",
        other => other,
    }
    .to_owned()
}

fn is_ask_user_question(value: Option<&str>) -> bool {
    normalized_tool_name(value) == "askuserquestion"
}

fn is_ask_user(value: Option<&str>) -> bool {
    normalized_tool_name(value) == "askuser"
}

fn normalized_tool_name(value: Option<&str>) -> String {
    value
        .unwrap_or_default()
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn is_high_risk(payload: &Map<String, Value>) -> bool {
    string(payload, &["riskLevel", "risk_level"])
        .or_else(|| {
            first_value(payload, &["tool_input", "input", "arguments"])
                .and_then(Value::as_object)
                .and_then(|input| string(input, &["riskLevel", "risk_level"]))
        })
        .is_some_and(|risk| risk.eq_ignore_ascii_case("high"))
}

fn is_blocking_copilot_notification(payload: &Map<String, Value>) -> bool {
    string(payload, &["notification_type", "notificationType"])
        .is_some_and(|kind| matches!(kind.as_str(), "permission_prompt" | "elicitation_dialog"))
}

fn is_permission_message(message: Option<&str>) -> bool {
    message.is_some_and(|message| {
        let lower = message.to_ascii_lowercase();
        lower.contains("permission") || lower.contains("approve") || lower.contains("approval")
    })
}

fn is_idle_message(message: Option<&str>) -> bool {
    message.is_some_and(|message| {
        let lower = message.to_ascii_lowercase();
        lower.contains("waiting for your input")
            || lower.contains("waiting for input")
            || lower.contains("type your message")
            || lower.contains("enter send")
            || lower.contains("shift-tab normal")
            || lower.contains("ask a side question")
    })
}

fn bounded_string(value: String) -> String {
    value.chars().take(HOOK_TEXT_MAX_CHARS).collect()
}

fn normalize_provider_session_id(value: String) -> Option<String> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 512
        || value.starts_with('-')
        || value.chars().any(char::is_control)
    {
        return None;
    }
    Some(value.to_owned())
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum HookAdapterError {
    #[error("hook event name is empty, unsafe, or too large")]
    InvalidEventName,
    #[error("hook payload must be a JSON object")]
    PayloadMustBeObject,
    #[error("hook payload exceeds the supported bound")]
    PayloadTooLarge,
    #[error("hook adapter is unavailable for {0}")]
    UnsupportedAdapter(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn id(value: &str) -> AdapterId {
        AdapterId::new(value).unwrap()
    }

    #[test]
    fn kimi_ask_user_pre_tool_is_an_approval_boundary() {
        let events = normalize_hook_event(
            &id("kimi"),
            "PreToolUse",
            &json!({
                "tool_name": "AskUserQuestion",
                "tool_input": {"question": "Continue?"}
            }),
        )
        .unwrap();
        assert!(matches!(
            events.as_slice(),
            [ProviderEvent::ApprovalRequested { tool_name, .. }] if tool_name == "AskUserQuestion"
        ));
    }

    #[test]
    fn grok_accepts_camel_case_hook_names() {
        let events = normalize_hook_event(
            &id("grok"),
            "PostToolUseFailure",
            &json!({"toolName": "shell", "toolResponse": "denied", "id": "t1"}),
        )
        .unwrap();
        assert!(matches!(
            events.as_slice(),
            [ProviderEvent::ToolCompleted { id, is_error: true, .. }] if id == "t1"
        ));
    }

    #[test]
    fn copilot_generic_permission_is_progress_but_ask_user_blocks() {
        let generic = normalize_hook_event(
            &id("copilot"),
            "permissionRequest",
            &json!({"tool_name": "shell", "tool_input": {"command": "pwd"}}),
        )
        .unwrap();
        assert!(matches!(
            generic.as_slice(),
            [ProviderEvent::ToolStarted { .. }]
        ));

        let blocked = normalize_hook_event(
            &id("copilot"),
            "permissionRequest",
            &json!({"tool_name": "ask_user", "tool_input": {"question": "Choose"}}),
        )
        .unwrap();
        assert!(matches!(
            blocked.as_slice(),
            [ProviderEvent::ApprovalRequested { .. }]
        ));
    }

    #[test]
    fn cursor_shell_gate_is_tool_progress_not_approval() {
        let events = normalize_hook_event(
            &id("cursor"),
            "beforeShellExecution",
            &json!({"command": "cargo check"}),
        )
        .unwrap();
        assert!(matches!(
            events.as_slice(),
            [ProviderEvent::ToolStarted { name, .. }] if name == "Shell"
        ));
    }

    #[test]
    fn droid_high_risk_pre_tool_is_an_approval_boundary() {
        let events = normalize_hook_event(
            &id("droid"),
            "PreToolUse",
            &json!({
                "tool_name": "shell",
                "tool_input": {"command": "deploy", "riskLevel": "high"}
            }),
        )
        .unwrap();
        assert!(matches!(
            events.as_slice(),
            [ProviderEvent::ApprovalRequested { tool_name, .. }] if tool_name == "shell"
        ));
    }

    #[test]
    fn malformed_or_oversized_envelopes_are_rejected() {
        assert_eq!(
            normalize_hook_event(&id("droid"), "Stop", &Value::Null),
            Err(HookAdapterError::PayloadMustBeObject)
        );
        assert_eq!(
            normalize_hook_event(
                &id("droid"),
                &"x".repeat(HOOK_EVENT_NAME_MAX_BYTES + 1),
                &json!({})
            ),
            Err(HookAdapterError::InvalidEventName)
        );
    }

    #[test]
    fn unknown_events_are_ignored_and_normalization_is_deterministic() {
        let payload = json!({"future_field": {"nested": true}});
        let first = normalize_hook_event(&id("grok"), "futureEvent", &payload).unwrap();
        let second = normalize_hook_event(&id("grok"), "futureEvent", &payload).unwrap();
        assert!(first.is_empty());
        assert_eq!(first, second);
        assert!(matches!(
            normalize_hook_event(&id("future-provider"), "Stop", &json!({})),
            Err(HookAdapterError::UnsupportedAdapter(_))
        ));
    }

    #[test]
    fn text_is_utf8_safe_and_bounded_by_characters() {
        let text = format!("привет{}", "界".repeat(HOOK_TEXT_MAX_CHARS));
        let events =
            normalize_hook_event(&id("cursor"), "afterAgentResponse", &json!({"text": text}))
                .unwrap();
        let [ProviderEvent::Text { text, .. }] = events.as_slice() else {
            panic!("expected one text event");
        };
        assert_eq!(text.chars().count(), HOOK_TEXT_MAX_CHARS);
        assert!(text.starts_with("привет"));
    }
}
