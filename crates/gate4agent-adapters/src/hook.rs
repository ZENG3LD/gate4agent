use gate4agent_types::{
    AdapterId, ProviderEvent, ProviderEventValidationError, ProviderInteractionKind,
    ProviderSessionIdentity, ProviderSessionKey, TokenUsage, PROVIDER_SESSION_LOCATOR_MAX_BYTES,
};
use serde_json::{Map, Value};
use thiserror::Error;

pub const HOOK_EVENT_NAME_MAX_BYTES: usize = 128;
pub const HOOK_PAYLOAD_MAX_BYTES: usize = 1_048_576;
pub const HOOK_TEXT_MAX_CHARS: usize = 65_536;
pub const MIMO_CODE_HOOK_TEXT_MAX_CHARS: usize = 8_000;

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
        "mimo-code" => normalize_mimo_code_family(event_name, record),
        "pi" | "omp" => normalize_pi_family(event_name, record),
        "amp" => normalize_amp(event_name, record),
        "grok" => normalize_grok(event_name, record),
        "kimi" => normalize_kimi(event_name, record),
        "cursor" => normalize_cursor(event_name, record),
        id => Err(HookAdapterError::UnsupportedAdapter(id.to_owned())),
    }?;
    if !events
        .iter()
        .any(|event| matches!(event, ProviderEvent::SessionIdentityObserved { .. }))
    {
        if let Some(identity) = provider_session_identity(adapter_id, record) {
            let position = events
                .iter()
                .rposition(|event| matches!(event, ProviderEvent::SessionStarted { .. }))
                .map_or(0, |index| index + 1);
            events.insert(
                position,
                ProviderEvent::SessionIdentityObserved { identity },
            );
        }
    }
    for event in &events {
        event.validate_ingress()?;
    }
    Ok(events)
}

fn provider_session_identity(
    adapter_id: &AdapterId,
    payload: &Map<String, Value>,
) -> Option<ProviderSessionIdentity> {
    let (key, keys): (ProviderSessionKey, &[&str]) = match adapter_id.as_str() {
        "claude-code" | "codex" | "kimi" | "pi" => {
            (ProviderSessionKey::SessionId, &["session_id"])
        }
        "mimo-code" => (ProviderSessionKey::SessionId, &["sessionID"]),
        "grok" => (ProviderSessionKey::SessionId, &["sessionId", "session_id"]),
        _ => return None,
    };
    let id = string(payload, keys).and_then(normalize_provider_session_id)?;
    let transcript_path = match adapter_id.as_str() {
        "claude-code" | "codex" => string(payload, &["transcript_path", "transcriptPath"])
            .and_then(normalize_provider_transcript_path),
        "pi" => string(payload, &["session_file"]).and_then(normalize_provider_transcript_path),
        _ => None,
    };
    if adapter_id.as_str() == "pi" && transcript_path.is_none() {
        return None;
    }
    Some(ProviderSessionIdentity {
        key,
        id,
        transcript_path,
    })
}

fn normalize_mimo_code_family(
    event_name: &str,
    payload: &Map<String, Value>,
) -> Result<Vec<ProviderEvent>, HookAdapterError> {
    match event_name {
        "SessionBusy" => Ok(vec![ProviderEvent::WorkingObserved]),
        "SessionIdle" => Ok(vec![ProviderEvent::TurnCompleted {
            usage: TokenUsage::default(),
            is_cumulative: false,
        }]),
        "MessagePart" => {
            let role = string(payload, &["role"]);
            let text = string(payload, &["text"]).map(bounded_mimo_code_text);
            match (role.as_deref(), text) {
                (Some("user"), Some(prompt)) => Ok(vec![ProviderEvent::TurnStarted {
                    prompt: Some(prompt),
                }]),
                (Some("assistant"), Some(text)) => Ok(vec![
                    ProviderEvent::WorkingObserved,
                    ProviderEvent::Text {
                        text,
                        is_delta: false,
                    },
                ]),
                _ => Ok(vec![ProviderEvent::WorkingObserved]),
            }
        }
        "PermissionRequest" => Ok(vec![mimo_code_interaction_event(
            payload,
            ProviderInteractionKind::Approval,
        )]),
        "AskUserQuestion" => Ok(vec![mimo_code_interaction_event(
            payload,
            ProviderInteractionKind::Question,
        )]),
        _ => Ok(Vec::new()),
    }
}

fn mimo_code_interaction_event(
    payload: &Map<String, Value>,
    interaction_kind: ProviderInteractionKind,
) -> ProviderEvent {
    let prompt_source = first_value(payload, &["tool_input", "toolInput"])
        .cloned()
        .unwrap_or_else(|| Value::Object(payload.clone()));
    let tool_name = match interaction_kind {
        ProviderInteractionKind::Approval => {
            string(payload, &["permission", "tool_name", "toolName"])
                .unwrap_or_else(|| "approval".to_owned())
        }
        ProviderInteractionKind::Question => "AskUserQuestion".to_owned(),
    };
    ProviderEvent::InteractionRequested {
        request_id: explicit_tool_id(payload).map(bounded_string),
        interaction_kind,
        tool_name: bounded_string(tool_name),
        // The hook payload carries no title or structured option list the
        // way an ACP `session/request_permission` call does -- this source
        // has none, not a dropped one.
        title: None,
        prompt: input_json(Some(&prompt_source)),
        options: Vec::new(),
        agent_id: None,
    }
}

fn normalize_pi_family(
    event_name: &str,
    payload: &Map<String, Value>,
) -> Result<Vec<ProviderEvent>, HookAdapterError> {
    match event_name {
        "session_start" => Ok(Vec::new()),
        "before_agent_start" => Ok(vec![turn_started(payload)]),
        "agent_start" => Ok(vec![ProviderEvent::WorkingObserved]),
        "tool_call" | "tool_execution_start" => Ok(vec![tool_started(payload)]),
        "tool_execution_end" => Ok(vec![tool_completed(payload, false)]),
        "message_end" => {
            let mut events = vec![ProviderEvent::WorkingObserved];
            if string(payload, &["role"]).as_deref() == Some("assistant") {
                if let Some(text) = string(payload, &["text"]) {
                    events.push(ProviderEvent::Text {
                        text: bounded_string(text),
                        is_delta: false,
                    });
                }
            }
            Ok(events)
        }
        "agent_end" => Ok(vec![ProviderEvent::TurnCompleted {
            usage: TokenUsage::default(),
            is_cumulative: false,
        }]),
        _ => Ok(Vec::new()),
    }
}

fn normalize_amp(
    event_name: &str,
    payload: &Map<String, Value>,
) -> Result<Vec<ProviderEvent>, HookAdapterError> {
    match event_name {
        "session.start" => Ok(Vec::new()),
        "agent.start" => Ok(vec![ProviderEvent::TurnStarted {
            prompt: string(payload, &["prompt", "user_prompt", "userPrompt", "message"])
                .map(bounded_string),
        }]),
        "tool.call" => Ok(vec![tool_started(payload)]),
        "tool.result" => {
            let is_error = first_value(payload, &["error"]).is_some()
                || string(payload, &["status"])
                    .is_some_and(|status| matches!(status.as_str(), "error" | "failed"));
            Ok(vec![
                tool_completed(payload, is_error),
                ProviderEvent::WorkingObserved,
            ])
        }
        "agent.end" if string(payload, &["status"]).as_deref() == Some("cancelled") => {
            Ok(vec![ProviderEvent::TurnInterrupted])
        }
        "agent.end" => Ok(vec![ProviderEvent::TurnCompleted {
            usage: TokenUsage::default(),
            is_cumulative: false,
        }]),
        _ => Ok(Vec::new()),
    }
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
        "Stop" if bool_value(payload, &["is_interrupt"]) == Some(true) => {
            Ok(turn_interrupted(payload))
        }
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
        "notification" if is_routine_grok_permission_notification(payload) => Ok(Vec::new()),
        "notification" if is_grok_permission_message(string(payload, &["message"]).as_deref()) => {
            Ok(vec![interaction_event(
                payload,
                ProviderInteractionKind::Approval,
            )])
        }
        "notification" if is_idle_message(string(payload, &["message"]).as_deref()) => {
            Ok(vec![ProviderEvent::TurnInterrupted])
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
        "Stop" if bool_value(payload, &["is_interrupt"]) == Some(true) => {
            Ok(turn_interrupted(payload))
        }
        "Stop" | "StopFailure" => Ok(turn_completed(payload)),
        _ => Ok(Vec::new()),
    }
}

fn normalize_cursor(
    event_name: &str,
    payload: &Map<String, Value>,
) -> Result<Vec<ProviderEvent>, HookAdapterError> {
    match event_name {
        "sessionStart" => {
            let mut events = session_started(payload, &["session_id", "sessionId"]);
            events.push(ProviderEvent::WorkingObserved);
            Ok(events)
        }
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
        "afterAgentResponse" => {
            let mut events = vec![ProviderEvent::WorkingObserved];
            if let Some(text) = string(payload, &["text"]) {
                events.push(ProviderEvent::Text {
                    text: bounded_string(text),
                    is_delta: false,
                });
            }
            Ok(events)
        }
        "stop"
            if string(payload, &["status"])
                .is_some_and(|status| status.as_str() != "completed") =>
        {
            Ok(turn_interrupted(payload))
        }
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
            "output",
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
        // The hook payload carries no title or structured option list the
        // way an ACP `session/request_permission` call does -- this source
        // has none, not a dropped one.
        title: None,
        prompt,
        options: Vec::new(),
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
            "assistant_response",
            "response_text",
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

fn turn_interrupted(payload: &Map<String, Value>) -> Vec<ProviderEvent> {
    let mut events = turn_completed(payload);
    if let Some(last) = events.last_mut() {
        *last = ProviderEvent::TurnInterrupted;
    }
    events
}

fn tool_name(payload: &Map<String, Value>) -> Option<String> {
    string(
        payload,
        &["toolName", "tool_name", "tool", "name", "tool_display_name"],
    )
    .or_else(|| {
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

fn bool_value(payload: &Map<String, Value>, keys: &[&str]) -> Option<bool> {
    first_value(payload, keys).and_then(Value::as_bool)
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

fn is_ask_user_question(value: Option<&str>) -> bool {
    normalized_tool_name(value) == "askuserquestion"
}

fn normalized_tool_name(value: Option<&str>) -> String {
    value
        .unwrap_or_default()
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn is_grok_permission_message(message: Option<&str>) -> bool {
    message.is_some_and(|message| {
        let lower = message.to_ascii_lowercase();
        [
            "permission",
            "approval",
            "approve",
            "allow",
            "confirm",
            "needs your",
            "requires your",
            "feedback",
            "clarify",
            "question",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
    })
}

fn is_routine_grok_permission_notification(payload: &Map<String, Value>) -> bool {
    let notification_type = string(payload, &["notificationType", "notification_type", "type"])
        .map(|value| snake_event_name(&value));
    let message = string(payload, &["message"]);
    let level = string(payload, &["level"]);
    notification_type.as_deref() == Some("permission_prompt")
        && message.is_some_and(|message| {
            message
                .trim()
                .eq_ignore_ascii_case("tool permission requested")
        })
        && level.is_none_or(|level| level.trim().eq_ignore_ascii_case("info"))
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

fn bounded_mimo_code_text(value: String) -> String {
    value.chars().take(MIMO_CODE_HOOK_TEXT_MAX_CHARS).collect()
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

fn normalize_provider_transcript_path(value: String) -> Option<String> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > PROVIDER_SESSION_LOCATOR_MAX_BYTES
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
    fn grok_suppresses_routine_permission_chatter_but_keeps_feedback_boundaries() {
        let routine = normalize_hook_event(
            &id("grok"),
            "Notification",
            &json!({
                "notificationType": "permission_prompt",
                "message": "Tool permission requested",
                "level": "info"
            }),
        )
        .unwrap();
        assert!(routine.is_empty());

        let feedback = normalize_hook_event(
            &id("grok"),
            "Notification",
            &json!({"message": "Grok needs your feedback to proceed"}),
        )
        .unwrap();
        assert!(matches!(
            feedback.as_slice(),
            [ProviderEvent::InteractionRequested {
                interaction_kind: ProviderInteractionKind::Approval,
                prompt,
                ..
            }] if prompt.contains("feedback")
        ));
    }

    #[test]
    fn pinned_interrupt_markers_do_not_count_as_completed_turns() {
        for (adapter, event_name, payload) in [
            (
                "claude-code",
                "Stop",
                json!({"is_interrupt": true, "last_assistant_message": "cancelled"}),
            ),
            (
                "kimi",
                "Stop",
                json!({"is_interrupt": true, "last_assistant_message": "cancelled"}),
            ),
            (
                "cursor",
                "stop",
                json!({"status": "aborted", "last_assistant_message": "cancelled"}),
            ),
        ] {
            let events = normalize_hook_event(&id(adapter), event_name, &payload).unwrap();
            assert!(matches!(
                events.last(),
                Some(ProviderEvent::TurnInterrupted)
            ));
            assert!(!events
                .iter()
                .any(|event| matches!(event, ProviderEvent::TurnCompleted { .. })));
        }
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
    fn idle_notifications_terminate_incomplete_provider_turns() {
        for (adapter, message) in [("grok", "Type your message")] {
            assert_eq!(
                normalize_hook_event(&id(adapter), "Notification", &json!({"message": message}),)
                    .unwrap(),
                vec![ProviderEvent::TurnInterrupted]
            );
        }
    }

    #[test]
    fn malformed_or_oversized_envelopes_are_rejected() {
        assert_eq!(
            normalize_hook_event(&id("grok"), "Stop", &Value::Null),
            Err(HookAdapterError::PayloadMustBeObject)
        );
        assert_eq!(
            normalize_hook_event(
                &id("grok"),
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
    fn claude_normalizes_subagent_lifecycle_identity() {
        let adapter = AdapterId::new("claude-code").unwrap();
        let started = normalize_hook_event(
            &adapter,
            "SubagentStart",
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
            &json!({
                "session_id": "codex-session-1",
                "transcript_path": "C:/sessions/codex-rollout-1.jsonl",
                "prompt": "resume work"
            }),
        )
        .unwrap();
        assert!(matches!(
            started.as_slice(),
            [
                ProviderEvent::SessionStarted { session_id, .. },
                ProviderEvent::SessionIdentityObserved { identity },
                ProviderEvent::TurnStarted { prompt }
            ] if session_id == "codex-session-1"
                && identity.key == ProviderSessionKey::SessionId
                && identity.id == "codex-session-1"
                && identity.transcript_path.as_deref()
                    == Some("C:/sessions/codex-rollout-1.jsonl")
                && prompt.as_deref() == Some("resume work")
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
    fn mimo_code_maps_status_messages_and_human_boundaries() {
        let adapter = id("mimo-code");
        let user = normalize_hook_event(
            &adapter,
            "MessagePart",
            &json!({
                "sessionID": "mimo-session-1",
                "messageID": "message-user-1",
                "role": "user",
                "text": "ship the fix"
            }),
        )
        .unwrap();
        assert!(matches!(
            user.as_slice(),
            [
                ProviderEvent::SessionIdentityObserved { identity },
                ProviderEvent::TurnStarted { prompt }
            ] if identity.key == ProviderSessionKey::SessionId
                && identity.id == "mimo-session-1"
                && prompt.as_deref() == Some("ship the fix")
        ));

        let assistant = normalize_hook_event(
            &adapter,
            "MessagePart",
            &json!({
                "sessionID": "mimo-session-1",
                "messageID": "message-assistant-1",
                "role": "assistant",
                "text": "x".repeat(MIMO_CODE_HOOK_TEXT_MAX_CHARS + 100)
            }),
        )
        .unwrap();
        assert!(matches!(
            assistant.as_slice(),
            [
                ProviderEvent::SessionIdentityObserved { .. },
                ProviderEvent::WorkingObserved,
                ProviderEvent::Text { text, is_delta: false }
            ] if text.chars().count() == MIMO_CODE_HOOK_TEXT_MAX_CHARS
        ));

        let approval = normalize_hook_event(
            &adapter,
            "PermissionRequest",
            &json!({
                "id": "permission-1",
                "sessionID": "mimo-session-1",
                "permission": "bash",
                "patterns": ["git push"]
            }),
        )
        .unwrap();
        assert!(matches!(
            approval.as_slice(),
            [
                ProviderEvent::SessionIdentityObserved { .. },
                ProviderEvent::InteractionRequested {
                    request_id: Some(request_id),
                    interaction_kind: ProviderInteractionKind::Approval,
                    tool_name,
                    prompt,
                    ..
                }
            ] if request_id == "permission-1"
                && tool_name == "bash"
                && prompt.contains("git push")
        ));

        let question = normalize_hook_event(
            &adapter,
            "AskUserQuestion",
            &json!({
                "id": "question-1",
                "sessionID": "mimo-session-1",
                "questions": [{"question": "Deploy?", "options": ["yes", "no"]}]
            }),
        )
        .unwrap();
        assert!(matches!(
            question.as_slice(),
            [
                ProviderEvent::SessionIdentityObserved { .. },
                ProviderEvent::InteractionRequested {
                    interaction_kind: ProviderInteractionKind::Question,
                    tool_name,
                    prompt,
                    ..
                }
            ] if tool_name == "AskUserQuestion" && prompt.contains("Deploy?")
        ));
    }

    #[test]
    fn mimo_code_keeps_an_independent_hook_contract() {
        let adapter = id("mimo-code");
        let busy = normalize_hook_event(
            &adapter,
            "SessionBusy",
            &json!({"sessionID": "mimo-session-1"}),
        )
        .unwrap();
        assert!(matches!(
            busy.as_slice(),
            [
                ProviderEvent::SessionIdentityObserved { identity },
                ProviderEvent::WorkingObserved
            ] if identity.key == ProviderSessionKey::SessionId
                && identity.id == "mimo-session-1"
        ));

        let idle = normalize_hook_event(
            &adapter,
            "SessionIdle",
            &json!({"sessionID": "mimo-session-1"}),
        )
        .unwrap();
        assert!(matches!(
            idle.as_slice(),
            [
                ProviderEvent::SessionIdentityObserved { .. },
                ProviderEvent::TurnCompleted { .. }
            ]
        ));
    }

    #[test]
    fn pi_maps_native_turn_tool_message_and_completion_events() {
        let adapter = id("pi");
        let session_start = normalize_hook_event(
            &adapter,
            "session_start",
            &json!({
                "session_id": "pi-session-1",
                "session_file": "/tmp/pi-session-1.jsonl"
            }),
        )
        .unwrap();
        assert_eq!(
            session_start,
            vec![ProviderEvent::SessionIdentityObserved {
                identity: ProviderSessionIdentity {
                    key: ProviderSessionKey::SessionId,
                    id: "pi-session-1".to_owned(),
                    transcript_path: Some("/tmp/pi-session-1.jsonl".to_owned()),
                },
            }]
        );

        assert!(normalize_hook_event(
            &adapter,
            "session_start",
            &json!({"session_id": "pi-session-without-file"}),
        )
        .unwrap()
        .is_empty());

        let started = normalize_hook_event(
            &adapter,
            "before_agent_start",
            &json!({"prompt": "resume this task"}),
        )
        .unwrap();
        assert!(matches!(
            started.as_slice(),
            [ProviderEvent::TurnStarted { prompt }]
                if prompt.as_deref() == Some("resume this task")
        ));

        for event_name in ["tool_call", "tool_execution_start"] {
            let tool = normalize_hook_event(
                &adapter,
                event_name,
                &json!({"tool_name": "bash", "tool_input": {"command": "cargo test"}}),
            )
            .unwrap();
            assert!(matches!(
                tool.as_slice(),
                [ProviderEvent::ToolStarted { name, input_json, .. }]
                    if name == "bash" && input_json.contains("cargo test")
            ));
        }

        let completed = normalize_hook_event(
            &adapter,
            "tool_execution_end",
            &json!({"tool_name": "bash"}),
        )
        .unwrap();
        assert!(matches!(
            completed.as_slice(),
            [ProviderEvent::ToolCompleted { id, is_error: false, .. }] if id == "bash"
        ));

        let message = normalize_hook_event(
            &adapter,
            "message_end",
            &json!({"role": "assistant", "text": "Done"}),
        )
        .unwrap();
        assert!(matches!(
            message.as_slice(),
            [
                ProviderEvent::WorkingObserved,
                ProviderEvent::Text { text, is_delta: false }
            ] if text == "Done"
        ));

        let ended = normalize_hook_event(&adapter, "agent_end", &json!({})).unwrap();
        assert!(matches!(
            ended.as_slice(),
            [ProviderEvent::TurnCompleted { .. }]
        ));
        assert!(
            normalize_hook_event(&adapter, "session_shutdown", &json!({}))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn omp_keeps_an_independent_pi_family_contract_without_resume_identity() {
        let adapter = id("omp");
        let started = normalize_hook_event(
            &adapter,
            "before_agent_start",
            &json!({"prompt": "wire omp status", "session_id": "not-owned-by-omp"}),
        )
        .unwrap();
        assert!(matches!(
            started.as_slice(),
            [ProviderEvent::TurnStarted { prompt }]
                if prompt.as_deref() == Some("wire omp status")
        ));

        let progress = normalize_hook_event(&adapter, "agent_start", &json!({})).unwrap();
        assert_eq!(progress, vec![ProviderEvent::WorkingObserved]);
    }

    #[test]
    fn amp_maps_thread_lifecycle_and_cancelled_end_without_resume_claim() {
        let adapter = id("amp");
        let started = normalize_hook_event(
            &adapter,
            "agent.start",
            &json!({"threadId": "thread-1", "id": "agent-1", "message": "fix tests"}),
        )
        .unwrap();
        assert!(matches!(
            started.as_slice(),
            [ProviderEvent::TurnStarted { prompt }] if prompt.as_deref() == Some("fix tests")
        ));

        let tool = normalize_hook_event(
            &adapter,
            "tool.call",
            &json!({
                "threadId": "thread-1",
                "toolUseId": "tool-1",
                "tool": "bash",
                "input": {"command": "cargo check"}
            }),
        )
        .unwrap();
        assert!(matches!(
            tool.as_slice(),
            [ProviderEvent::ToolStarted { id, name, input_json, .. }]
                if id == "tool-1" && name == "bash" && input_json.contains("cargo check")
        ));

        let result = normalize_hook_event(
            &adapter,
            "tool.result",
            &json!({
                "threadId": "thread-1",
                "toolUseId": "tool-1",
                "tool": "bash",
                "status": "error",
                "error": "exit 1",
                "output": "failed"
            }),
        )
        .unwrap();
        assert!(matches!(
            result.as_slice(),
            [
                ProviderEvent::ToolCompleted { id, output, is_error: true, .. },
                ProviderEvent::WorkingObserved
            ] if id == "tool-1" && output == "failed"
        ));

        let cancelled = normalize_hook_event(
            &adapter,
            "agent.end",
            &json!({"threadId": "thread-1", "status": "cancelled"}),
        )
        .unwrap();
        assert_eq!(cancelled, vec![ProviderEvent::TurnInterrupted]);

        assert!(
            normalize_hook_event(&adapter, "session.start", &json!({"threadId": "thread-2"}))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn claude_question_and_lifecycle_are_independent_canonical_events() {
        let adapter = id("claude-code");
        let session = normalize_hook_event(
            &adapter,
            "SessionStart",
            &json!({
                "session_id": "claude-session-1",
                "transcript_path": "C:/sessions/claude-rollout-1.jsonl"
            }),
        )
        .unwrap();
        assert!(matches!(
            session.as_slice(),
            [
                ProviderEvent::SessionStarted { session_id, .. },
                ProviderEvent::SessionIdentityObserved { identity },
            ] if session_id == "claude-session-1"
                && identity.key == ProviderSessionKey::SessionId
                && identity.id == "claude-session-1"
                && identity.transcript_path.as_deref()
                    == Some("C:/sessions/claude-rollout-1.jsonl")
        ));
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
        let [ProviderEvent::WorkingObserved, ProviderEvent::Text { text, .. }] = events.as_slice()
        else {
            panic!("expected working and text events");
        };
        assert_eq!(text.chars().count(), HOOK_TEXT_MAX_CHARS);
        assert!(text.starts_with("привет"));
    }
}
