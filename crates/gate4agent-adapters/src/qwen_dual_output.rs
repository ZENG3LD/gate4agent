use gate4agent_types::{
    ProviderEvent, ProviderInteractionKind, ProviderInteractionOutcome, TokenUsage,
};
use serde_json::Value;
use std::collections::BTreeSet;

pub const QWEN_DUAL_OUTPUT_REVISION: &str = "qwen-code-dual-output/v1";
pub const QWEN_DUAL_OUTPUT_MAX_LINE_BYTES: usize = 1_048_576;
const QWEN_DUAL_OUTPUT_MAX_TRACKED_TOOLS: usize = 1_024;
const QWEN_DUAL_OUTPUT_MAX_PENDING_INTERACTIONS: usize = 1_024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QwenDualOutputLine {
    Events(Vec<ProviderEvent>),
    Ignored,
    Gap,
}

#[derive(Debug, Default)]
pub struct QwenDualOutputParser {
    handshake_seen: bool,
    clean_end_seen: bool,
    turn_active: bool,
    started_tools: BTreeSet<String>,
    completed_tools: BTreeSet<String>,
    pending_interactions: BTreeSet<String>,
}

impl QwenDualOutputParser {
    pub fn parse_line(&mut self, line: &[u8]) -> QwenDualOutputLine {
        if line.is_empty() || line.iter().all(u8::is_ascii_whitespace) {
            return QwenDualOutputLine::Ignored;
        }
        if line.len() > QWEN_DUAL_OUTPUT_MAX_LINE_BYTES {
            return QwenDualOutputLine::Gap;
        }
        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            return QwenDualOutputLine::Gap;
        };
        let Some(object) = value.as_object() else {
            return QwenDualOutputLine::Gap;
        };
        let Some(kind) = object.get("type").and_then(Value::as_str) else {
            return QwenDualOutputLine::Gap;
        };
        let subtype = object.get("subtype").and_then(Value::as_str);

        if !self.handshake_seen {
            if kind != "system" || subtype != Some("session_start") {
                return QwenDualOutputLine::Gap;
            }
            if !valid_handshake(object.get("data")) {
                return QwenDualOutputLine::Gap;
            }
            self.handshake_seen = true;
            return QwenDualOutputLine::Events(vec![ProviderEvent::Ready]);
        }
        if self.clean_end_seen {
            return QwenDualOutputLine::Gap;
        }
        if kind == "system" && subtype == Some("session_start") {
            return QwenDualOutputLine::Gap;
        }

        let parsed = match kind {
            "system" if subtype == Some("session_end") => {
                self.clean_end_seen = true;
                self.pending_interactions.clear();
                Some(vec![ProviderEvent::SessionEnded {
                    result: "session ended".to_owned(),
                    cost_usd: None,
                    is_error: false,
                }])
            }
            "stream_event" => self.parse_stream_event(object.get("event")),
            "assistant" => self.parse_assistant(object.get("message")),
            "user" => self.parse_user(object.get("message")),
            "result" => self.parse_result(object),
            "control_request" => self.parse_control_request(object),
            "control_response" => self.parse_control_response(object),
            _ => Some(Vec::new()),
        };
        let Some(events) = parsed else {
            return QwenDualOutputLine::Gap;
        };
        if events.iter().any(|event| event.validate_ingress().is_err()) {
            return QwenDualOutputLine::Gap;
        }
        if events.is_empty() {
            QwenDualOutputLine::Ignored
        } else {
            QwenDualOutputLine::Events(events)
        }
    }

    pub fn handshake_seen(&self) -> bool {
        self.handshake_seen
    }

    pub fn clean_end_seen(&self) -> bool {
        self.clean_end_seen
    }

    fn parse_stream_event(&mut self, event: Option<&Value>) -> Option<Vec<ProviderEvent>> {
        let event = event?.as_object()?;
        match event.get("type")?.as_str()? {
            "message_start" => Some(self.start_turn()),
            "content_block_start" => {
                let block = event.get("content_block")?.as_object()?;
                if block.get("type")?.as_str()? != "tool_use" {
                    return Some(Vec::new());
                }
                self.tool_started(block)
            }
            "content_block_delta" | "content_block_stop" | "message_stop" => Some(Vec::new()),
            _ => Some(Vec::new()),
        }
    }

    fn parse_assistant(&mut self, message: Option<&Value>) -> Option<Vec<ProviderEvent>> {
        let message = message?.as_object()?;
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            return None;
        }
        let mut events = self.start_turn();
        if let Some(content) = message.get("content").and_then(Value::as_array) {
            for block in content {
                let Some(block) = block.as_object() else {
                    return None;
                };
                if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                    events.extend(self.tool_started(block)?);
                }
            }
        }
        Some(events)
    }

    fn parse_user(&mut self, message: Option<&Value>) -> Option<Vec<ProviderEvent>> {
        let message = message?.as_object()?;
        if message.get("role").and_then(Value::as_str) != Some("user") {
            return None;
        }
        let content = message.get("content")?;
        if content.is_string() {
            return Some(self.start_turn());
        }
        let content = content.as_array()?;
        let mut events = Vec::new();
        let mut contains_prompt = false;
        for block in content {
            let block = block.as_object()?;
            if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                let id = block.get("tool_use_id")?.as_str()?.to_owned();
                if !self.completed_tools.contains(&id)
                    && self.completed_tools.len() == QWEN_DUAL_OUTPUT_MAX_TRACKED_TOOLS
                {
                    return None;
                }
                if self.completed_tools.insert(id.clone()) {
                    events.push(ProviderEvent::ToolCompleted {
                        id,
                        output: String::new(),
                        is_error: block.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                        duration_ms: block.get("duration_ms").and_then(Value::as_u64),
                        agent_id: None,
                    });
                }
            } else {
                contains_prompt = true;
            }
        }
        if contains_prompt {
            let mut started = self.start_turn();
            started.extend(events);
            Some(started)
        } else {
            Some(events)
        }
    }

    fn parse_result(
        &mut self,
        object: &serde_json::Map<String, Value>,
    ) -> Option<Vec<ProviderEvent>> {
        let subtype = object.get("subtype")?.as_str()?;
        if !matches!(subtype, "success" | "error_max_turns" | "error_during_execution") {
            return None;
        }
        let usage = parse_usage(object.get("usage")?)?;
        let mut events = self.start_turn();
        events.push(ProviderEvent::TurnCompleted {
            usage,
            is_cumulative: false,
        });
        self.turn_active = false;
        self.started_tools.clear();
        self.completed_tools.clear();
        self.pending_interactions.clear();
        Some(events)
    }

    fn parse_control_request(
        &mut self,
        object: &serde_json::Map<String, Value>,
    ) -> Option<Vec<ProviderEvent>> {
        let request_id = object.get("request_id")?.as_str()?.to_owned();
        let request = object.get("request")?.as_object()?;
        if request.get("subtype")?.as_str()? != "can_use_tool" {
            return Some(Vec::new());
        }
        request.get("tool_use_id")?.as_str()?;
        if self.pending_interactions.contains(&request_id)
            || self.pending_interactions.len() == QWEN_DUAL_OUTPUT_MAX_PENDING_INTERACTIONS
        {
            return None;
        }
        self.pending_interactions.insert(request_id.clone());
        let tool_name = tool_class(request.get("tool_name")?.as_str()?);
        Some(vec![ProviderEvent::InteractionRequested {
            request_id: Some(request_id),
            interaction_kind: ProviderInteractionKind::Approval,
            tool_name,
            prompt: String::new(),
            agent_id: None,
        }])
    }

    fn parse_control_response(
        &mut self,
        object: &serde_json::Map<String, Value>,
    ) -> Option<Vec<ProviderEvent>> {
        let response = object.get("response")?.as_object()?;
        match response.get("subtype")?.as_str()? {
            "success" => {
                let request_id = response.get("request_id")?.as_str()?.to_owned();
                let allowed = response
                    .get("response")?
                    .as_object()?
                    .get("allowed")?
                    .as_bool()?;
                if !self.pending_interactions.remove(&request_id) {
                    return None;
                }
                Some(vec![ProviderEvent::InteractionResolved {
                    request_id,
                    outcome: if allowed {
                        ProviderInteractionOutcome::Approved
                    } else {
                        ProviderInteractionOutcome::Denied
                    },
                }])
            }
            "error" => Some(Vec::new()),
            _ => None,
        }
    }

    fn tool_started(
        &mut self,
        block: &serde_json::Map<String, Value>,
    ) -> Option<Vec<ProviderEvent>> {
        let id = block.get("id")?.as_str()?.to_owned();
        let name = block.get("name")?.as_str()?.to_owned();
        if !self.started_tools.contains(&id)
            && self.started_tools.len() == QWEN_DUAL_OUTPUT_MAX_TRACKED_TOOLS
        {
            return None;
        }
        if !self.started_tools.insert(id.clone()) {
            return Some(Vec::new());
        }
        let mut events = self.start_turn();
        events.push(ProviderEvent::ToolStarted {
            id,
            name,
            input_json: String::new(),
            agent_id: None,
        });
        Some(events)
    }

    fn start_turn(&mut self) -> Vec<ProviderEvent> {
        if self.turn_active {
            Vec::new()
        } else {
            self.turn_active = true;
            vec![ProviderEvent::TurnStarted { prompt: None }]
        }
    }
}

fn valid_handshake(data: Option<&Value>) -> bool {
    let Some(data) = data else {
        return true;
    };
    let Some(data) = data.as_object() else {
        return false;
    };
    if data
        .get("protocol_version")
        .is_some_and(|value| value.as_u64().is_none())
    {
        return false;
    }
    if let Some(events) = data.get("supported_events") {
        let Some(events) = events.as_array() else {
            return false;
        };
        if events.iter().any(|event| event.as_str().is_none()) {
            return false;
        }
    }
    true
}

fn parse_usage(value: &Value) -> Option<TokenUsage> {
    let usage = value.as_object()?;
    Some(TokenUsage {
        input_tokens: usage.get("input_tokens")?.as_u64()?,
        output_tokens: usage.get("output_tokens")?.as_u64()?,
        cache_read_tokens: usage
            .get("cache_read_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_write_tokens: usage
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        reasoning_tokens: 0,
        context_window: None,
    })
}

fn tool_class(name: &str) -> String {
    let normalized = name.to_ascii_lowercase();
    if ["shell", "bash", "powershell", "terminal", "exec", "command"]
        .iter()
        .any(|term| normalized.contains(term))
    {
        "Shell"
    } else if ["read", "view", "open", "get"]
        .iter()
        .any(|term| normalized.contains(term))
    {
        "Read"
    } else if ["write", "create", "save", "edit", "patch", "replace"]
        .iter()
        .any(|term| normalized.contains(term))
    {
        "Edit"
    } else if ["search", "find", "grep", "query"]
        .iter()
        .any(|term| normalized.contains(term))
    {
        "Search"
    } else if ["browser", "web", "http", "fetch"]
        .iter()
        .any(|term| normalized.contains(term))
    {
        "Browse"
    } else {
        "Tool"
    }
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(parser: &mut QwenDualOutputParser, line: &str) -> Vec<ProviderEvent> {
        match parser.parse_line(line.as_bytes()) {
            QwenDualOutputLine::Events(events) => events,
            other => panic!("expected events, got {other:?}"),
        }
    }

    #[test]
    fn official_lifecycle_tool_control_and_usage_are_private() {
        let mut parser = QwenDualOutputParser::default();
        assert_eq!(
            parse(
                &mut parser,
                r#"{"type":"system","subtype":"session_start","session_id":"private-session","data":{"session_id":"private-session","cwd":"C:\\private","protocol_version":2,"supported_events":["control_request"]}}"#,
            ),
            vec![ProviderEvent::Ready]
        );
        let started = parse(
            &mut parser,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"tool-1","name":"run_shell_command","input":{"command":"private command"}}]}}"#,
        );
        assert_eq!(started.len(), 2);
        assert_eq!(started[0], ProviderEvent::TurnStarted { prompt: None });
        assert!(matches!(
            &started[1],
            ProviderEvent::ToolStarted { id, name, input_json, agent_id: None }
                if id == "tool-1" && name == "run_shell_command" && input_json.is_empty()
        ));
        let attention = parse(
            &mut parser,
            r#"{"type":"control_request","request_id":"approval-1","request":{"subtype":"can_use_tool","tool_name":"run_shell_command","tool_use_id":"tool-1","input":{"command":"private command"}}}"#,
        );
        assert!(matches!(
            &attention[0],
            ProviderEvent::InteractionRequested { request_id: Some(id), tool_name, prompt, .. }
                if id == "approval-1" && tool_name == "Shell" && prompt.is_empty()
        ));
        let resolved = parse(
            &mut parser,
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"approval-1","response":{"allowed":true}}}"#,
        );
        assert_eq!(
            resolved,
            vec![ProviderEvent::InteractionResolved {
                request_id: "approval-1".to_owned(),
                outcome: ProviderInteractionOutcome::Approved,
            }]
        );
        let completed = parse(
            &mut parser,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tool-1","content":"private output","is_error":false,"duration_ms":17}]}}"#,
        );
        assert!(matches!(
            &completed[0],
            ProviderEvent::ToolCompleted { id, output, is_error: false, duration_ms: Some(17), .. }
                if id == "tool-1" && output.is_empty()
        ));
        let turn = parse(
            &mut parser,
            r#"{"type":"result","subtype":"success","result":"private transcript","usage":{"input_tokens":11,"output_tokens":13,"cache_read_input_tokens":17,"cache_creation_input_tokens":19}}"#,
        );
        assert_eq!(
            turn,
            vec![ProviderEvent::TurnCompleted {
                usage: TokenUsage {
                    input_tokens: 11,
                    output_tokens: 13,
                    cache_read_tokens: 17,
                    cache_write_tokens: 19,
                    reasoning_tokens: 0,
                    context_window: None,
                },
                is_cumulative: false,
            }]
        );
        let ended = parse(
            &mut parser,
            r#"{"type":"system","subtype":"session_end","data":{"session_id":"private-session"}}"#,
        );
        assert_eq!(
            ended,
            vec![ProviderEvent::SessionEnded {
                result: "session ended".to_owned(),
                cost_usd: None,
                is_error: false,
            }]
        );
        let encoded =
            serde_json::to_string(&(started, attention, resolved, completed, turn, ended)).unwrap();
        for private in ["private-session", "C:\\private", "private command", "private output", "private transcript"] {
            assert!(!encoded.contains(private));
        }
    }

    #[test]
    fn control_response_resolves_only_matching_pending_request_with_exact_outcome() {
        let mut parser = QwenDualOutputParser::default();
        parse(
            &mut parser,
            r#"{"type":"system","subtype":"session_start","data":{"protocol_version":2,"supported_events":["control_request","control_response"]}}"#,
        );
        parse(
            &mut parser,
            r#"{"type":"control_request","request_id":"approval-denied","request":{"subtype":"can_use_tool","tool_name":"write_file","tool_use_id":"tool-2","input":{"path":"private"}}}"#,
        );
        assert_eq!(
            parse(
                &mut parser,
                r#"{"type":"control_response","response":{"subtype":"success","request_id":"approval-denied","response":{"allowed":false}}}"#,
            ),
            vec![ProviderEvent::InteractionResolved {
                request_id: "approval-denied".to_owned(),
                outcome: ProviderInteractionOutcome::Denied,
            }]
        );
        assert_eq!(
            parser.parse_line(
                br#"{"type":"control_response","response":{"subtype":"success","request_id":"approval-denied","response":{"allowed":false}}}"#,
            ),
            QwenDualOutputLine::Gap
        );
        assert_eq!(
            parser.parse_line(
                br#"{"type":"control_response","response":{"subtype":"success","request_id":"orphan","response":{"allowed":true}}}"#,
            ),
            QwenDualOutputLine::Gap
        );
        assert_eq!(
            parser.parse_line(
                br#"{"type":"control_response","response":{"subtype":"error","request_id":"orphan","error":"unknown request_id"}}"#,
            ),
            QwenDualOutputLine::Ignored
        );
    }

    #[test]
    fn handshake_is_required_and_malformed_or_oversized_lines_emit_only_gap() {
        let mut parser = QwenDualOutputParser::default();
        assert_eq!(
            parser.parse_line(br#"{"type":"result","subtype":"success","usage":{"input_tokens":1,"output_tokens":1}}"#),
            QwenDualOutputLine::Gap
        );
        assert_eq!(parser.parse_line(b"not-json"), QwenDualOutputLine::Gap);
        assert_eq!(
            parser.parse_line(&vec![b'x'; QWEN_DUAL_OUTPUT_MAX_LINE_BYTES + 1]),
            QwenDualOutputLine::Gap
        );
        assert!(!parser.handshake_seen());
    }
}
