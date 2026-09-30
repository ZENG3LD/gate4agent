//! The sanitized control-event detail a C2 relays to a client that negotiated
//! `C2_CONTROL_DETAIL_CAPABILITY`.
//!
//! The coarse `C2ControlEventKind` tag says WHICH control event happened and
//! nothing else. A client that derives its own telemetry (tool calls, token
//! usage, plans, lifecycle) needs the facts behind the tag, in
//! `gate4agent-types`' own vocabulary. This module produces exactly those, and
//! clears every field telemetry does not need: provider session identities and
//! transcript paths, provider request ids, tool inputs and outputs, prompts,
//! free-text results, raw provider payloads. What remains is the skeleton of
//! the event -- enough to count, classify and correlate it, never enough to
//! read what the session said or did.

use gate4agent_types::{ControlEvent, ControlEventKind, ProviderEvent, ProviderSessionIdentity};

/// Message a `ProviderEvent::Error` keeps when the node's own engine refused
/// provider events -- the one error category a telemetry client tells apart.
const PROVIDER_EVENTS_REJECTED: &str = "provider events rejected";
/// Message every other `ProviderEvent::Error` is reduced to.
const PROVIDER_ERROR: &str = "provider error";

/// The telemetry view of `event`, or `None` when no telemetry derives from it
/// (control-plane bookkeeping such as requests, completions and probes).
///
/// Every provider event keeps a skeleton -- content-bearing kinds (`Text`,
/// `UserMessage`, catalogs) keep their kind and lose their content -- because
/// the receipt of an event is itself telemetry: a source declares what it can
/// report with its first event, whatever that event is.
pub fn control_telemetry_detail(event: &ControlEvent) -> Option<ControlEvent> {
    let kind = match &event.event {
        ControlEventKind::Running { .. } => ControlEventKind::Running { process_id: None },
        ControlEventKind::Exited { exit_code, forced } => ControlEventKind::Exited {
            exit_code: *exit_code,
            forced: *forced,
        },
        ControlEventKind::Failed { .. } => ControlEventKind::Failed { message: String::new() },
        ControlEventKind::Removed => ControlEventKind::Removed,
        ControlEventKind::InteractionResolved { interaction_id, outcome } => {
            ControlEventKind::InteractionResolved {
                interaction_id: interaction_id.clone(),
                outcome: *outcome,
            }
        }
        ControlEventKind::ProviderGap { sequence, source, source_sequence, missed } => {
            ControlEventKind::ProviderGap {
                sequence: *sequence,
                source: source.clone(),
                source_sequence: *source_sequence,
                missed: *missed,
            }
        }
        ControlEventKind::ProviderEvent { sequence, source, source_sequence, event: provider } => {
            ControlEventKind::ProviderEvent {
                sequence: *sequence,
                source: source.clone(),
                source_sequence: *source_sequence,
                event: telemetry_provider_event(provider),
            }
        }
        _ => return None,
    };
    Some(ControlEvent {
        sequence: event.sequence,
        command_id: event.command_id,
        instance_id: event.instance_id,
        generation: event.generation,
        event: kind,
    })
}

fn telemetry_provider_event(event: &ProviderEvent) -> ProviderEvent {
    match event {
        ProviderEvent::SessionStarted { .. } => ProviderEvent::SessionStarted {
            session_id: String::new(),
            model: String::new(),
            tools: Vec::new(),
        },
        ProviderEvent::TurnStarted { .. } => ProviderEvent::TurnStarted { prompt: None },
        ProviderEvent::WorkingObserved => ProviderEvent::WorkingObserved,
        ProviderEvent::Thinking { .. } => ProviderEvent::Thinking { text: String::new() },
        ProviderEvent::ToolStarted { id, name, .. } => ProviderEvent::ToolStarted {
            id: id.clone(),
            name: name.clone(),
            input_json: String::new(),
            agent_id: None,
        },
        ProviderEvent::ToolCompleted { id, is_error, duration_ms, non_execution_kind, .. } => {
            ProviderEvent::ToolCompleted {
                id: id.clone(),
                output: String::new(),
                is_error: *is_error,
                duration_ms: *duration_ms,
                agent_id: None,
                non_execution_kind: non_execution_kind.clone(),
            }
        }
        ProviderEvent::TurnCompleted { usage, is_cumulative } => ProviderEvent::TurnCompleted {
            usage: usage.clone(),
            is_cumulative: *is_cumulative,
        },
        ProviderEvent::ContextWindowUsage { usage } => {
            ProviderEvent::ContextWindowUsage { usage: usage.clone() }
        }
        ProviderEvent::TurnInterrupted => ProviderEvent::TurnInterrupted,
        ProviderEvent::SessionEnded { is_error, .. } => ProviderEvent::SessionEnded {
            result: String::new(),
            cost_usd: None,
            is_error: *is_error,
            stop_reason: None,
        },
        ProviderEvent::Error { message } => ProviderEvent::Error {
            message: if message.trim().to_ascii_lowercase().starts_with(PROVIDER_EVENTS_REJECTED) {
                PROVIDER_EVENTS_REJECTED.to_owned()
            } else {
                PROVIDER_ERROR.to_owned()
            },
        },
        ProviderEvent::Ready => ProviderEvent::Ready,
        ProviderEvent::InteractionRequested { interaction_kind, tool_name, .. } => {
            ProviderEvent::InteractionRequested {
                request_id: None,
                interaction_kind: *interaction_kind,
                tool_name: tool_name.clone(),
                title: None,
                prompt: String::new(),
                options: Vec::new(),
                agent_id: None,
            }
        }
        ProviderEvent::SubagentStarted { agent_id, agent_type, .. } => {
            ProviderEvent::SubagentStarted {
                agent_id: agent_id.clone(),
                agent_type: agent_type.clone(),
                description: None,
            }
        }
        ProviderEvent::SubagentStopped { agent_id } => {
            ProviderEvent::SubagentStopped { agent_id: agent_id.clone() }
        }
        ProviderEvent::RateLimited { limit_type, .. } => ProviderEvent::RateLimited {
            limit_type: limit_type.clone(),
            resets_at: None,
            usage_percent: None,
            raw_message: String::new(),
        },
        ProviderEvent::HostRequestObserved { method, decision, outcome, .. } => {
            ProviderEvent::HostRequestObserved {
                method: method.clone(),
                params_json: String::new(),
                decision: decision.clone(),
                outcome: outcome.clone(),
                reason: None,
            }
        }
        ProviderEvent::UnrecognizedNotification { method, .. } => {
            ProviderEvent::UnrecognizedNotification {
                method: method.clone(),
                payload_json: String::new(),
            }
        }
        ProviderEvent::UsageUpdated { used_tokens, context_window, .. } => {
            ProviderEvent::UsageUpdated {
                used_tokens: *used_tokens,
                context_window: *context_window,
                cost_amount: None,
                cost_currency: None,
            }
        }
        ProviderEvent::Plan { steps } => ProviderEvent::Plan { steps: steps.clone() },
        ProviderEvent::SessionIdentityObserved { identity } => {
            ProviderEvent::SessionIdentityObserved {
                identity: ProviderSessionIdentity {
                    key: identity.key.clone(),
                    id: String::new(),
                    transcript_path: None,
                },
            }
        }
        ProviderEvent::Text { is_delta, .. } => {
            ProviderEvent::Text { text: String::new(), is_delta: *is_delta }
        }
        ProviderEvent::InteractionResolved { outcome, .. } => {
            ProviderEvent::InteractionResolved {
                request_id: String::new(),
                outcome: outcome.clone(),
            }
        }
        ProviderEvent::UserMessage { is_delta, .. } => {
            ProviderEvent::UserMessage { text: String::new(), is_delta: *is_delta }
        }
        ProviderEvent::AvailableCommandsUpdated { .. } => {
            ProviderEvent::AvailableCommandsUpdated { commands: Vec::new() }
        }
        ProviderEvent::ModeChanged { .. } => ProviderEvent::ModeChanged {
            mode_id: String::new(),
            available: Vec::new(),
        },
        ProviderEvent::SessionInfoUpdated { .. } => {
            ProviderEvent::SessionInfoUpdated { title: None }
        }
        ProviderEvent::ConfigOptionsUpdated { .. } => {
            ProviderEvent::ConfigOptionsUpdated { options: Vec::new() }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gate4agent_types::{
        AdapterBinding, AdapterFamily, AdapterId, AdapterVerification, AgentInstanceId,
        ProviderSessionIdentity, ProviderSessionKey, ProviderSource, SessionGeneration,
    };

    fn control(event: ControlEventKind) -> ControlEvent {
        ControlEvent {
            sequence: 4,
            command_id: None,
            instance_id: AgentInstanceId(7),
            generation: SessionGeneration(3),
            event,
        }
    }

    fn provider_source() -> ProviderSource {
        ProviderSource {
            family: AdapterFamily::Acp,
            binding: AdapterBinding::new(
                AdapterId::new("claude-code").unwrap(),
                "fixture/v1",
                AdapterVerification::SyntheticFixture,
            )
            .unwrap(),
        }
    }

    fn provider(event: ProviderEvent) -> ControlEvent {
        control(ControlEventKind::ProviderEvent {
            sequence: 8,
            source: provider_source(),
            source_sequence: 9,
            event,
        })
    }

    fn detail_json(event: ProviderEvent) -> String {
        serde_json::to_string(&control_telemetry_detail(&provider(event)).unwrap()).unwrap()
    }

    #[test]
    fn content_and_identity_kinds_keep_only_their_kind() {
        for (event, private) in [
            (
                ProviderEvent::Text { text: "secret words".to_owned(), is_delta: true },
                "secret words",
            ),
            (
                ProviderEvent::UserMessage { text: "secret words".to_owned(), is_delta: false },
                "secret words",
            ),
            (
                ProviderEvent::SessionIdentityObserved {
                    identity: ProviderSessionIdentity {
                        key: ProviderSessionKey::ConversationId,
                        id: "private-provider-id".to_owned(),
                        transcript_path: Some(r"C:\private\transcript.jsonl".to_owned()),
                    },
                },
                "private",
            ),
            (
                ProviderEvent::SessionInfoUpdated { title: Some("secret title".to_owned()) },
                "secret title",
            ),
            (
                ProviderEvent::InteractionResolved {
                    request_id: "private-request".to_owned(),
                    outcome: gate4agent_types::ProviderInteractionOutcome::Denied,
                },
                "private-request",
            ),
        ] {
            let json = detail_json(event);
            assert!(json.contains("\"kind\""));
            assert!(!json.contains(private), "{private} leaked: {json}");
        }
        assert_eq!(
            control_telemetry_detail(&control(ControlEventKind::Registered)),
            None,
        );
    }

    #[test]
    fn tool_and_turn_events_keep_their_skeleton_and_lose_their_content() {
        let started = detail_json(ProviderEvent::ToolStarted {
            id: "toolu_1".to_owned(),
            name: "Bash".to_owned(),
            input_json: r#"{"command":"cat private.txt"}"#.to_owned(),
            agent_id: Some("private-agent".to_owned()),
        });
        assert!(started.contains("toolu_1"));
        assert!(started.contains("Bash"));
        assert!(!started.contains("private.txt"));
        assert!(!started.contains("private-agent"));

        let completed = detail_json(ProviderEvent::ToolCompleted {
            id: "toolu_1".to_owned(),
            output: "contents of private.txt".to_owned(),
            is_error: true,
            duration_ms: Some(12),
            agent_id: None,
            non_execution_kind: Some("permission-rule".to_owned()),
        });
        assert!(completed.contains("permission-rule"));
        assert!(completed.contains("\"duration_ms\":12"));
        assert!(!completed.contains("private.txt"));

        let ended = detail_json(ProviderEvent::SessionEnded {
            result: "private result".to_owned(),
            cost_usd: Some("1.50".to_owned()),
            is_error: false,
            stop_reason: None,
        });
        assert!(!ended.contains("private result"));
        assert!(!ended.contains("1.50"));
    }

    #[test]
    fn prompts_options_request_ids_and_payloads_are_cleared() {
        let interaction = detail_json(ProviderEvent::InteractionRequested {
            request_id: Some("private-request-id".to_owned()),
            interaction_kind: gate4agent_types::ProviderInteractionKind::Approval,
            tool_name: "Bash".to_owned(),
            title: Some("private title".to_owned()),
            prompt: "private prompt".to_owned(),
            options: Vec::new(),
            agent_id: Some("private-agent".to_owned()),
        });
        assert!(interaction.contains("Bash"));
        for private in ["private-request-id", "private title", "private prompt", "private-agent"] {
            assert!(!interaction.contains(private), "{private}");
        }
        let unrecognized = detail_json(ProviderEvent::UnrecognizedNotification {
            method: "vendor/thing".to_owned(),
            payload_json: r#"{"secret":true}"#.to_owned(),
        });
        assert!(unrecognized.contains("vendor/thing"));
        assert!(!unrecognized.contains("secret"));
    }

    #[test]
    fn error_messages_reduce_to_their_category() {
        let rejected = detail_json(ProviderEvent::Error {
            message: "Provider events rejected: capability withheld, 3 dropped at C:\\private"
                .to_owned(),
        });
        assert!(rejected.contains("provider events rejected"));
        assert!(!rejected.contains("private"));
        let other = detail_json(ProviderEvent::Error {
            message: "boom at C:\\private".to_owned(),
        });
        assert!(other.contains("provider error"));
        assert!(!other.contains("private"));
    }

    #[test]
    fn lifecycle_events_keep_exit_facts_and_lose_messages() {
        let exited = control_telemetry_detail(&control(ControlEventKind::Exited {
            exit_code: Some(3),
            forced: true,
        }))
        .unwrap();
        assert_eq!(
            exited.event,
            ControlEventKind::Exited { exit_code: Some(3), forced: true },
        );
        let failed = control_telemetry_detail(&control(ControlEventKind::Failed {
            message: "private failure".to_owned(),
        }))
        .unwrap();
        assert_eq!(failed.event, ControlEventKind::Failed { message: String::new() });
        let running = control_telemetry_detail(&control(ControlEventKind::Running {
            process_id: Some(4242),
        }))
        .unwrap();
        assert_eq!(running.event, ControlEventKind::Running { process_id: None });
    }
}
