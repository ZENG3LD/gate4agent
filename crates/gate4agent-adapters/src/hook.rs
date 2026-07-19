use gate4agent_types::{
    AdapterId, ProviderEvent, ProviderEventValidationError, ProviderInteractionKind, TokenUsage,
};
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

    let mut events = match adapter_id.as_str() {
        "claude-code" => normalize_claude(event_name, record),
        "codex" => normalize_codex(event_name, record),
        "gemini" => normalize_gemini(event_name, record),
        "grok" => normalize_grok(event_name, record),
        "kimi" => normalize_kimi(event_name, record),
        "copilot" => normalize_copilot(event_name, record),
        "droid" => normalize_droid(event_name, record),
        "cursor" => normalize_cursor(event_name, record),
        id => Err(HookAdapterError::UnsupportedAdapter(id.to_owned())),
    }?;
    if !events.iter().any(|event| {
        matches!(
            event,
            ProviderEvent::SessionStarted { .. } | ProviderEvent::SessionIdentityObserved { .. }
        )
    }) {
        if let Some(session_id) = provider_session_id(adapter_id, record) {
            events.insert(0, ProviderEvent::SessionIdentityObserved { session_id });
        }
    }
    for event in &events {
        event.validate_ingress()?;
    }
    Ok(events)
}

fn provider_session_id(adapter_id: &AdapterId, payload: &Map<String, Value>) -> Option<String> {
    let keys: &[&str] = match adapter_id.as_str() {
        "claude-code" | "codex" | "gemini" | "droid" | "kimi" => &["session_id"],
        "grok" => &["sessionId", "session_id"],
        _ => return None,
    };
    string(payload, keys).and_then(normalize_provider_session_id)
}

fn normalize_codex(
    event_name: &str,
    payload: &Map<String, Value>,
) -> Result<Vec<ProviderEvent>, HookAdapterError> {
    match event_name {
        "SessionStart" => {
            let mut events = session_started(payload, &["session_id"]);
            events.push(turn_started(payload));
            Ok(events)
        }
        "UserPromptSubmit" => Ok(vec![turn_started(payload)]),
        "PreToolUse" => Ok(vec![tool_started(payload)]),
        "PermissionRequest" if is_ask_user_question(tool_name(payload).as_deref()) => {
            Ok(vec![interaction_event(
                payload,
                ProviderInteractionKind::Question,
            )])
        }
        "PermissionRequest" => Ok(vec![interaction_event(
            payload,
            ProviderInteractionKind::Approval,
        )]),
        "PostToolUse" => Ok(vec![tool_completed(payload, false)]),
        "Stop" => Ok(turn_completed(payload)),
        _ => Ok(Vec::new()),
    }
}

fn normalize_gemini(
    event_name: &str,
    payload: &Map<String, Value>,
) -> Result<Vec<ProviderEvent>, HookAdapterError> {
    match event_name {
        "BeforeAgent" => Ok(vec![turn_started(payload)]),
        "BeforeTool" | "PreToolUse" => Ok(vec![tool_started(payload)]),
        "AfterTool" | "PostToolUse" => Ok(vec![tool_completed(payload, false)]),
        "AfterAgent" => {
            let mut events = string(payload, &["prompt_response"])
                .map(|text| {
                    vec![ProviderEvent::Text {
                        text: bounded_string(text),
                        is_delta: false,
                    }]
                })
                .unwrap_or_default();
            events.push(ProviderEvent::TurnCompleted {
                usage: TokenUsage::default(),
                is_cumulative: false,
            });
            Ok(events)
        }
        _ => Ok(Vec::new()),
    }
}

fn normalize_claude(
    event_name: &str,
    payload: &Map<String, Value>,
) -> Result<Vec<ProviderEvent>, HookAdapterError> {
    match event_name {
        "SessionStart" => Ok(session_started(payload, &["session_id"])),
        "UserPromptSubmit" => Ok(vec![turn_started(payload)]),
        "PreToolUse" if is_ask_user_question(tool_name(payload).as_deref()) => {
            Ok(vec![interaction_event(
                payload,
                ProviderInteractionKind::Question,
            )])
        }
        "PreToolUse" => Ok(vec![tool_started(payload)]),
        "PostToolUse" => Ok(vec![tool_completed(payload, false)]),
        "PostToolUseFailure" => Ok(vec![tool_completed(payload, true)]),
        "PermissionRequest" => Ok(vec![interaction_event(
            payload,
            ProviderInteractionKind::Approval,
        )]),
        "Stop" | "StopFailure" => Ok(turn_completed(payload)),
        "SubagentStart" => Ok(subagent_started(payload)),
        "SubagentStop" => Ok(subagent_stopped(payload)),
        "TeammateIdle" => Ok(Vec::new()),
        _ => Ok(Vec::new()),
    }
}

fn normalize_grok(
    event_name: &str,
    payload: &Map<String, Value>,
) -> Result<Vec<ProviderEvent>, HookAdapterError> {
    let event = snake_event_name(event_name);
    match event.as_str() {
        "session_start" => Ok(session_started(payload, &["sessionId", "session_id"])),
        "subagent_start" => Ok(subagent_started(payload)),
        "subagent_stop" => Ok(subagent_stopped(payload)),
        "user_prompt_submit" => Ok(vec![turn_started(payload)]),
        "pre_tool_use" if is_ask_user_question(tool_name(payload).as_deref()) => {
            Ok(vec![interaction_event(
                payload,
                ProviderInteractionKind::Question,
            )])
        }
        "pre_tool_use" => Ok(vec![tool_started(payload)]),
        "post_tool_use" => Ok(vec![tool_completed(payload, false)]),
        "post_tool_use_failure" => Ok(vec![tool_completed(payload, true)]),
        "stop" | "stop_failure" | "session_end" => Ok(turn_completed(payload)),
        "notification" if is_permission_message(string(payload, &["message"]).as_deref()) => {
            Ok(vec![interaction_event(
                payload,
                ProviderInteractionKind::Approval,
            )])
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
        "SubagentStart" => Ok(subagent_started(payload)),
        "SubagentStop" => Ok(subagent_stopped(payload)),
        "UserPromptSubmit" => Ok(vec![turn_started(payload)]),
        "PreToolUse" if is_ask_user_question(tool_name(payload).as_deref()) => {
            Ok(vec![interaction_event(
                payload,
                ProviderInteractionKind::Question,
            )])
        }
        "PreToolUse" => Ok(vec![tool_started(payload)]),
        "PostToolUse" => Ok(vec![tool_completed(payload, false)]),
        "PostToolUseFailure" => Ok(vec![tool_completed(payload, true)]),
        "PermissionRequest" => Ok(vec![interaction_event(
            payload,
            ProviderInteractionKind::Approval,
        )]),
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
        "SubagentStart" => Ok(subagent_started(payload)),
        "SubagentStop" => Ok(subagent_stopped(payload)),
        "UserPromptSubmit" => Ok(vec![turn_started(payload)]),
        "PreToolUse" | "PermissionRequest" if is_ask_user(tool_name(payload).as_deref()) => {
            Ok(vec![interaction_event(
                payload,
                ProviderInteractionKind::Question,
            )])
        }
        "PreToolUse" | "PermissionRequest" => Ok(vec![tool_started(payload)]),
        "PostToolUse" => Ok(vec![tool_completed(payload, false)]),
        "PostToolUseFailure" => Ok(vec![tool_completed(payload, true)]),
        "Notification" if is_blocking_copilot_notification(payload) => Ok(vec![interaction_event(
            payload,
            ProviderInteractionKind::Approval,
        )]),
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
        "SubagentStart" => Ok(subagent_started(payload)),
        "SubagentStop" => Ok(subagent_stopped(payload)),
        "UserPromptSubmit" => Ok(vec![turn_started(payload)]),
        "PreToolUse" if is_ask_user(tool_name(payload).as_deref()) => Ok(vec![interaction_event(
            payload,
            ProviderInteractionKind::Question,
        )]),
        "PreToolUse" if is_high_risk(payload) => Ok(vec![interaction_event(
            payload,
            ProviderInteractionKind::Approval,
        )]),
        "PreToolUse" => Ok(vec![tool_started(payload)]),
        "PostToolUse" => Ok(vec![tool_completed(payload, false)]),
        "PermissionRequest" => Ok(vec![interaction_event(
            payload,
            ProviderInteractionKind::Approval,
        )]),
        "Notification" if is_permission_message(string(payload, &["message"]).as_deref()) => {
            Ok(vec![interaction_event(
                payload,
                ProviderInteractionKind::Approval,
            )])
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
        "subagentStart" => Ok(subagent_started(payload)),
        "subagentStop" => Ok(subagent_stopped(payload)),
        "beforeSubmitPrompt" => Ok(vec![turn_started(payload)]),
        "preToolUse" => Ok(vec![tool_started(payload)]),
        "postToolUse" => Ok(vec![tool_completed(payload, false)]),
        "postToolUseFailure" => Ok(vec![tool_completed(payload, true)]),
        "beforeShellExecution" => Ok(vec![ProviderEvent::ToolStarted {
            id: tool_id(payload, "Shell"),
            name: "Shell".to_owned(),
            input_json: input_json(payload.get("command")),
            agent_id: provider_agent_id(payload),
        }]),
        "beforeMCPExecution" => {
            let name = tool_name(payload).unwrap_or_else(|| "MCP".to_owned());
            Ok(vec![ProviderEvent::ToolStarted {
                id: tool_id(payload, &name),
                name,
                input_json: input_json(first_value(payload, &["tool_input", "command", "url"])),
                agent_id: provider_agent_id(payload),
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

fn subagent_started(payload: &Map<String, Value>) -> Vec<ProviderEvent> {
    string(payload, &["agent_id", "agentId"])
        .map(|agent_id| {
            vec![ProviderEvent::SubagentStarted {
                agent_id: bounded_string(agent_id),
                agent_type: string(payload, &["agent_type", "agentType"]).map(bounded_string),
                description: string(payload, &["description", "prompt"]).map(bounded_string),
            }]
        })
        .unwrap_or_default()
}

fn subagent_stopped(payload: &Map<String, Value>) -> Vec<ProviderEvent> {
    string(payload, &["agent_id", "agentId"])
        .map(|agent_id| {
            vec![ProviderEvent::SubagentStopped {
                agent_id: bounded_string(agent_id),
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
            &[
                "toolInput",
                "tool_input",
                "toolArgs",
                "args",
                "input",
                "arguments",
            ],
        )),
        agent_id: provider_agent_id(payload),
    }
}

fn turn_started(payload: &Map<String, Value>) -> ProviderEvent {
    ProviderEvent::TurnStarted {
        prompt: string(
            payload,
            &[
                "prompt",
                "user_prompt",
                "userPrompt",
                "user_message",
                "initial_prompt",
                "initialPrompt",
            ],
        )
        .map(bounded_string),
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
        agent_id: provider_agent_id(payload),
    }
}

fn interaction_event(
    payload: &Map<String, Value>,
    interaction_kind: ProviderInteractionKind,
) -> ProviderEvent {
    let tool_name = tool_name(payload).unwrap_or_else(|| match interaction_kind {
        ProviderInteractionKind::Approval => "approval".to_owned(),
        ProviderInteractionKind::Question => "question".to_owned(),
    });
    let prompt = if interaction_kind == ProviderInteractionKind::Question {
        input_json(first_value(
            payload,
            &["toolInput", "tool_input", "toolArgs", "input", "arguments"],
        ))
    } else {
        string(
            payload,
            &["description", "message", "body", "text", "title"],
        )
        .map(bounded_string)
        .unwrap_or_else(|| {
            input_json(first_value(
                payload,
                &["toolInput", "tool_input", "toolArgs", "input", "arguments"],
            ))
        })
    };
    ProviderEvent::InteractionRequested {
        request_id: explicit_tool_id(payload).map(bounded_string),
        interaction_kind,
        tool_name,
        prompt,
        agent_id: provider_agent_id(payload),
    }
}

fn provider_agent_id(payload: &Map<String, Value>) -> Option<String> {
    string(payload, &["agent_id", "agentId"]).map(bounded_string)
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
    bounded_string(explicit_tool_id(payload).unwrap_or_else(|| fallback.to_owned()))
}

fn explicit_tool_id(payload: &Map<String, Value>) -> Option<String> {
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
        "subagentStart" => "SubagentStart",
        "subagentStop" => "SubagentStop",
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
    #[error(transparent)]
    InvalidCanonicalEvent(#[from] ProviderEventValidationError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn id(value: &str) -> AdapterId {
        AdapterId::new(value).unwrap()
    }

    #[test]
    fn kimi_ask_user_pre_tool_is_a_structured_question_boundary() {
        let events = normalize_hook_event(
            &id("kimi"),
            "PreToolUse",
            &json!({
                "tool_name": "AskUserQuestion",
                "tool_use_id": "question-k1",
                "tool_input": {"question": "Continue?"}
            }),
        )
        .unwrap();
        assert!(matches!(
            events.as_slice(),
            [ProviderEvent::InteractionRequested {
                interaction_kind: ProviderInteractionKind::Question,
                request_id: Some(request_id),
                tool_name,
                prompt,
                ..
            }] if request_id == "question-k1"
                && tool_name == "AskUserQuestion"
                && prompt.contains("Continue?")
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
            [ProviderEvent::InteractionRequested {
                interaction_kind: ProviderInteractionKind::Question,
                ..
            }]
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
            [ProviderEvent::InteractionRequested {
                interaction_kind: ProviderInteractionKind::Approval,
                tool_name,
                ..
            }] if tool_name == "shell"
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
    fn copilot_normalizes_subagent_lifecycle_identity() {
        let adapter = AdapterId::new("copilot").unwrap();
        let started = normalize_hook_event(
            &adapter,
            "subagentStart",
            &serde_json::json!({
                "agentId": "child-c1",
                "agentType": "reviewer",
                "description": "review changes"
            }),
        )
        .unwrap();
        assert!(matches!(
            started.as_slice(),
            [ProviderEvent::SubagentStarted {
                agent_id,
                agent_type: Some(agent_type),
                description: Some(description),
            }] if agent_id == "child-c1" && agent_type == "reviewer" && description == "review changes"
        ));

        let stopped = normalize_hook_event(
            &adapter,
            "SubagentStop",
            &serde_json::json!({"agent_id": "child-c1"}),
        )
        .unwrap();
        assert!(matches!(
            stopped.as_slice(),
            [ProviderEvent::SubagentStopped { agent_id }] if agent_id == "child-c1"
        ));
    }

    #[test]
    fn codex_has_independent_session_permission_and_stop_contracts() {
        let adapter = id("codex");
        let started = normalize_hook_event(
            &adapter,
            "SessionStart",
            &json!({"session_id": "codex-session-1", "prompt": "resume work"}),
        )
        .unwrap();
        assert!(matches!(
            started.as_slice(),
            [
                ProviderEvent::SessionStarted { session_id, .. },
                ProviderEvent::TurnStarted { prompt }
            ] if session_id == "codex-session-1" && prompt.as_deref() == Some("resume work")
        ));

        let approval = normalize_hook_event(
            &adapter,
            "PermissionRequest",
            &json!({
                "tool_name": "shell",
                "tool_use_id": "codex-tool-1",
                "input": {"command": "git push --force"}
            }),
        )
        .unwrap();
        assert!(matches!(
            approval.as_slice(),
            [ProviderEvent::InteractionRequested {
                request_id: Some(request_id),
                interaction_kind: ProviderInteractionKind::Approval,
                tool_name,
                ..
            }] if request_id == "codex-tool-1" && tool_name == "shell"
        ));

        let question = normalize_hook_event(
            &adapter,
            "PermissionRequest",
            &json!({
                "tool_name": "AskUserQuestion",
                "tool_use_id": "codex-question-1",
                "input": {"questions": [{"question": "Choose", "options": ["a", "b"]}]}
            }),
        )
        .unwrap();
        assert!(matches!(
            question.as_slice(),
            [ProviderEvent::InteractionRequested {
                interaction_kind: ProviderInteractionKind::Question,
                prompt,
                ..
            }] if prompt.contains("Choose")
        ));

        let stopped =
            normalize_hook_event(&adapter, "Stop", &json!({"last_assistant_message": "done"}))
                .unwrap();
        assert!(matches!(
            stopped.as_slice(),
            [ProviderEvent::Text { text, .. }, ProviderEvent::TurnCompleted { .. }]
                if text == "done"
        ));
    }

    #[test]
    fn gemini_uses_native_before_after_agent_and_tool_events() {
        let adapter = id("gemini");
        let started = normalize_hook_event(
            &adapter,
            "BeforeAgent",
            &json!({"session_id": "gemini-session-1", "prompt": "inspect repository"}),
        )
        .unwrap();
        assert!(matches!(
            started.as_slice(),
            [
                ProviderEvent::SessionIdentityObserved { session_id },
                ProviderEvent::TurnStarted { prompt }
            ] if session_id == "gemini-session-1"
                && prompt.as_deref() == Some("inspect repository")
        ));

        let tool = normalize_hook_event(
            &adapter,
            "BeforeTool",
            &json!({
                "tool_name": "read_file",
                "tool_call_id": "g-tool-1",
                "args": {"path": "Cargo.toml"}
            }),
        )
        .unwrap();
        assert!(matches!(
            tool.as_slice(),
            [ProviderEvent::ToolStarted { id, name, input_json, .. }]
                if id == "g-tool-1" && name == "read_file" && input_json.contains("Cargo.toml")
        ));

        let completed = normalize_hook_event(
            &adapter,
            "AfterAgent",
            &json!({"prompt_response": "Repository inspected"}),
        )
        .unwrap();
        assert!(matches!(
            completed.as_slice(),
            [ProviderEvent::Text { text, .. }, ProviderEvent::TurnCompleted { .. }]
                if text == "Repository inspected"
        ));
        assert!(
            normalize_hook_event(&adapter, "PermissionRequest", &json!({}))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn claude_question_and_lifecycle_are_independent_canonical_events() {
        let adapter = id("claude-code");
        let question = normalize_hook_event(
            &adapter,
            "PreToolUse",
            &json!({
                "tool_name": "AskUserQuestion",
                "tool_use_id": "q1",
                "tool_input": {"question": "Continue?"},
                "agent_id": "a1"
            }),
        )
        .unwrap();
        assert!(matches!(
            question.as_slice(),
            [ProviderEvent::InteractionRequested {
                request_id: Some(request_id),
                interaction_kind: ProviderInteractionKind::Question,
                agent_id: Some(agent_id),
                ..
            }] if request_id == "q1" && agent_id == "a1"
        ));
        let started = normalize_hook_event(
            &adapter,
            "SubagentStart",
            &json!({"agent_id": "a1", "agent_type": "reviewer"}),
        )
        .unwrap();
        assert!(matches!(
            started.as_slice(),
            [ProviderEvent::SubagentStarted { agent_id, .. }] if agent_id == "a1"
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
