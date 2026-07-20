use gate4agent_handle::{
    bounded_control_plane, ToolAuthorityError, ToolClientDispatchError, ToolCompletionDelivery,
};
use gate4agent_kernel::{BackendIngress, Gate4AgentKernel};
use gate4agent_tool_protocol::{
    CapabilityClass, CapabilityDescriptor, CapabilityEffect, CapabilityObservation,
    CapabilityObservationEnvelope, CapabilityOwner, CapabilityProviderDescriptor,
    CapabilityRequestId, CapabilityRequestInput, CapabilityResult, CapabilityResultDelivery,
    CapabilityResultMetadata, CapabilityTerminalOutcome, GrantMode, PolicyGrant, PolicyKey,
    ResourceScopeId, ToolActorId, ToolAuthorityCommand, ToolCapabilityId, ToolProviderId,
    CAPABILITY_PROTOCOL_VERSION,
};
use gate4agent_types::{
    AgentId, AgentInstanceId, CommandEnvelope, CommandId, ControlCommand, ControlObservation,
    ObservationEnvelope, SessionGeneration, StartRequest, TerminalSize, TransportKind,
    CONTROL_PROTOCOL_VERSION,
};
use std::sync::mpsc::TryRecvError;

fn instance() -> AgentInstanceId {
    AgentInstanceId(71)
}

fn provider_id() -> ToolProviderId {
    ToolProviderId::new("gate.browser.test").unwrap()
}

fn capability_id() -> ToolCapabilityId {
    ToolCapabilityId::new("browser.snapshot").unwrap()
}

fn resource_scope() -> ResourceScopeId {
    ResourceScopeId::new("page.active").unwrap()
}

fn provider() -> CapabilityProviderDescriptor {
    CapabilityProviderDescriptor {
        id: provider_id(),
        owner: CapabilityOwner::Gate,
        capabilities: vec![CapabilityDescriptor::new(
            capability_id(),
            CapabilityClass::Browser,
            "Return active page metadata",
        )
        .unwrap()],
    }
}

fn command(id: u64, command: ControlCommand) -> CommandEnvelope {
    CommandEnvelope {
        protocol_version: CONTROL_PROTOCOL_VERSION,
        id: CommandId(id),
        command,
    }
}

fn request(local_id: u64, generation: SessionGeneration) -> CapabilityRequestInput {
    CapabilityRequestInput {
        local_id: CapabilityRequestId(local_id),
        instance_id: instance(),
        generation,
        provider_id: provider_id(),
        capability_id: capability_id(),
        resource_scope_id: resource_scope(),
        approval_summary: "Read active page metadata".to_owned(),
        deadline_tick: 100,
        payload: br#"{"scope":"active-page"}"#.to_vec(),
    }
}

fn grant(
    client: &gate4agent_handle::ToolClientHandle,
    generation: SessionGeneration,
    mode: GrantMode,
) -> PolicyGrant {
    PolicyGrant {
        key: PolicyKey {
            consumer_id: client.consumer_id().clone(),
            actor_id: client.actor_id().clone(),
            instance_id: instance(),
            generation,
            provider_id: provider_id(),
            capability_id: capability_id(),
            resource_scope_id: resource_scope(),
        },
        mode,
    }
}

fn start_running(
    kernel: &mut Gate4AgentKernel,
    gate: &gate4agent_handle::Gate4AgentHandle,
    port: &gate4agent_handle::ControlPlaneKernelPort,
) -> SessionGeneration {
    gate.dispatch(command(
        1,
        ControlCommand::Register {
            instance_id: instance(),
            agent_id: AgentId::new("claude").unwrap(),
            transport: TransportKind::Pty,
        },
    ))
    .unwrap();
    gate.dispatch(command(
        2,
        ControlCommand::Start {
            instance_id: instance(),
            request: StartRequest {
                working_directory: ".".to_owned(),
                terminal_size: TerminalSize {
                    rows: 24,
                    columns: 80,
                },
                initial_prompt: None,
                session_options: None,
            },
        },
    ))
    .unwrap();
    let starting = kernel.step_control_plane(port.drain_ingress(8), [], []);
    let spawn = starting.effects[0].clone();
    port.publish_step(&starting);

    let running = kernel.step_control_plane(
        [],
        [ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: Some(spawn.operation_id),
            instance_id: spawn.instance_id,
            generation: spawn.generation,
            observation: ControlObservation::Spawned {
                process_id: Some(9001),
            },
        }],
        [],
    );
    port.publish_step(&running);
    running.backend_snapshot.control.sessions[0].generation
}

#[test]
fn control_plane_e2e_routes_fake_provider_result_only_to_exact_client() {
    let (gate, authority, port) = bounded_control_plane(16);
    let client = authority
        .bind_client(ToolActorId::new("agent.primary").unwrap())
        .unwrap();
    let other = authority
        .bind_client(ToolActorId::new("agent.other").unwrap())
        .unwrap();
    let request_outcomes = client.subscribe_request_outcomes(4);
    let completions = client.subscribe_completions(4);
    let other_completions = other.subscribe_completions(4);
    let authority_outcomes = authority.subscribe_outcomes(4);
    let mut kernel = Gate4AgentKernel::with_tool_providers(
        gate4agent_catalog::builtin_registry().clone(),
        [provider()],
    )
    .unwrap();
    let generation = start_running(&mut kernel, &gate, &port);

    authority
        .dispatch(ToolAuthorityCommand::SetGrant {
            grant: grant(&client, generation, GrantMode::Allow),
        })
        .unwrap();
    let granted = kernel.step_control_plane(port.drain_ingress(8), [], []);
    port.publish_step(&granted);
    assert!(authority_outcomes.try_recv().unwrap().result.is_ok());

    let request_key = client.dispatch(request(1, generation)).unwrap();
    let dispatched = kernel.step_control_plane(port.drain_ingress(8), [], []);
    assert_eq!(dispatched.tool_effects.len(), 1);
    let effect = dispatched.tool_effects[0].clone();
    assert!(matches!(effect.effect, CapabilityEffect::Invoke { .. }));
    port.publish_step(&dispatched);
    let published_snapshot = client.snapshot();
    assert_eq!(
        published_snapshot.backend_revision,
        dispatched.backend_snapshot.revision
    );
    assert_eq!(
        published_snapshot.protocol_version,
        CAPABILITY_PROTOCOL_VERSION
    );
    let accepted = request_outcomes.try_recv().unwrap();
    assert_eq!(accepted.request_key, request_key);
    let accepted_sequence = accepted.accepted_sequence.unwrap();

    let completed = kernel.step_control_plane(
        [],
        [],
        [CapabilityObservationEnvelope {
            protocol_version: CAPABILITY_PROTOCOL_VERSION,
            operation_id: effect.operation_id,
            request_key: effect.request_key.clone(),
            instance_id: effect.instance_id,
            generation: effect.generation,
            provider_id: effect.provider_id.clone(),
            observation: CapabilityObservation::Succeeded {
                result: CapabilityResult {
                    metadata: CapabilityResultMetadata {
                        byte_len: 2,
                        media_type: Some("application/json".to_owned()),
                        truncated: false,
                        redacted_summary: Some("page snapshot captured".to_owned()),
                    },
                    delivery: CapabilityResultDelivery::Inline {
                        bytes: b"{}".to_vec(),
                    },
                },
            },
        }],
    );
    port.publish_step(&completed);
    let ToolCompletionDelivery::Completion(completion) = completions.try_recv().unwrap() else {
        panic!("expected exact completion");
    };
    assert_eq!(completion.request_key, request_key);
    assert_eq!(completion.accepted_sequence, accepted_sequence);
    assert!(matches!(
        completion.outcome,
        CapabilityTerminalOutcome::Succeeded { .. }
    ));
    assert_eq!(other_completions.try_recv(), Err(TryRecvError::Empty));
}

#[test]
fn scoped_clients_cannot_observe_each_others_outcomes_or_snapshots() {
    let (_gate, authority, port) = bounded_control_plane(8);
    let first = authority
        .bind_client(ToolActorId::new("agent.first").unwrap())
        .unwrap();
    let second = authority
        .bind_client(ToolActorId::new("agent.second").unwrap())
        .unwrap();
    let first_outcomes = first.subscribe_request_outcomes(2);
    let second_outcomes = second.subscribe_request_outcomes(2);
    let first_completions = first.subscribe_completions(2);
    let second_completions = second.subscribe_completions(2);
    let first_key = first.dispatch(request(1, SessionGeneration(0))).unwrap();
    let second_key = second.dispatch(request(1, SessionGeneration(0))).unwrap();

    let mut kernel = Gate4AgentKernel::default();
    let step = kernel.step_control_plane(port.drain_ingress(8), [], []);
    port.publish_step(&step);

    assert_eq!(first_outcomes.try_recv().unwrap().request_key, first_key);
    assert_eq!(second_outcomes.try_recv().unwrap().request_key, second_key);
    let ToolCompletionDelivery::Completion(first_completion) =
        first_completions.try_recv().unwrap()
    else {
        panic!("expected first completion");
    };
    let ToolCompletionDelivery::Completion(second_completion) =
        second_completions.try_recv().unwrap()
    else {
        panic!("expected second completion");
    };
    assert_eq!(first_completion.request_key, first_key);
    assert_eq!(second_completion.request_key, second_key);
    assert_eq!(first.snapshot().requests[0].key, first_key);
    assert_eq!(second.snapshot().requests[0].key, second_key);
    assert!(first.snapshot().grants.is_empty());
    assert!(second.snapshot().grants.is_empty());
}

#[test]
fn full_close_retry_does_not_block_authority_and_enqueues_close_once() {
    let (_gate, authority, port) = bounded_control_plane(1);
    let client = authority
        .bind_client(ToolActorId::new("agent.close").unwrap())
        .unwrap();
    let clone = client.clone();
    client.dispatch(request(1, SessionGeneration(0))).unwrap();

    assert_eq!(
        authority.close_client(&client),
        Err(ToolAuthorityError::Full)
    );
    assert_eq!(
        clone.dispatch(request(2, SessionGeneration(0))),
        Err(ToolClientDispatchError::Inactive)
    );
    let first = port.drain_ingress(1);
    assert!(matches!(first[0], BackendIngress::ToolRequest(_)));

    assert_eq!(
        authority
            .dispatch(ToolAuthorityCommand::SetGrant {
                grant: grant(&client, SessionGeneration(0), GrantMode::Allow),
            })
            .unwrap(),
        1
    );
    assert!(matches!(
        &port.drain_ingress(1)[0],
        BackendIngress::ToolAuthority(envelope) if envelope.sequence == 1
    ));

    assert_eq!(authority.close_client(&clone).unwrap(), 2);
    assert_eq!(authority.close_client(&client).unwrap(), 2);
    let close = port.drain_ingress(1);
    assert!(matches!(
        &close[0],
        BackendIngress::ToolAuthority(envelope)
            if envelope.sequence == 2
                && matches!(envelope.command, ToolAuthorityCommand::CloseClient { .. })
    ));
    assert!(port.drain_ingress(1).is_empty());
}

#[test]
fn client_closed_completion_is_delivered_before_subscription_disconnect() {
    let (gate, authority, port) = bounded_control_plane(16);
    let client = authority
        .bind_client(ToolActorId::new("agent.close-order").unwrap())
        .unwrap();
    let clone = client.clone();
    let completion_subscription = client.subscribe_completions(4);
    let authority_subscription = authority.subscribe_outcomes(4);
    let mut kernel = Gate4AgentKernel::with_tool_providers(
        gate4agent_catalog::builtin_registry().clone(),
        [provider()],
    )
    .unwrap();
    let generation = start_running(&mut kernel, &gate, &port);

    authority
        .dispatch(ToolAuthorityCommand::SetGrant {
            grant: grant(&client, generation, GrantMode::RequireApproval),
        })
        .unwrap();
    let granted = kernel.step_control_plane(port.drain_ingress(8), [], []);
    port.publish_step(&granted);
    authority_subscription.try_recv().unwrap();

    client.dispatch(request(9, generation)).unwrap();
    let awaiting = kernel.step_control_plane(port.drain_ingress(8), [], []);
    assert!(awaiting.tool_effects.is_empty());
    port.publish_step(&awaiting);

    authority.close_client(&client).unwrap();
    assert_eq!(
        clone.dispatch(request(10, generation)),
        Err(ToolClientDispatchError::Inactive)
    );
    let closed = kernel.step_control_plane(port.drain_ingress(8), [], []);
    let report = port.publish_step(&closed);
    assert_eq!(report.closed_clients, 1);
    assert!(matches!(
        authority_subscription.try_recv().unwrap().result,
        Ok(gate4agent_tool_protocol::ToolAuthorityOutcome::ClientClosed { .. })
    ));
    let ToolCompletionDelivery::Completion(completion) =
        completion_subscription.try_recv().unwrap()
    else {
        panic!("expected ClientClosed completion");
    };
    assert!(matches!(
        completion.outcome,
        CapabilityTerminalOutcome::ClientClosed { .. }
    ));
    assert_eq!(
        completion_subscription.try_recv(),
        Err(TryRecvError::Disconnected)
    );
}

#[test]
fn slow_scoped_subscribers_disconnect_and_exhaustion_gap_is_not_repeated() {
    let (_gate, authority, port) = bounded_control_plane(8);
    let client = authority
        .bind_client(ToolActorId::new("agent.slow").unwrap())
        .unwrap();
    let outcomes = client.subscribe_request_outcomes(1);
    let completions = client.subscribe_completions(1);
    client.dispatch(request(1, SessionGeneration(0))).unwrap();
    client.dispatch(request(2, SessionGeneration(0))).unwrap();

    let mut kernel = Gate4AgentKernel::default();
    let mut step = kernel.step_control_plane(port.drain_ingress(8), [], []);
    let report = port.publish_step(&step);
    assert_eq!(report.request_outcomes.disconnected_slow, 1);
    assert_eq!(report.completions.disconnected_slow, 1);
    outcomes.try_recv().unwrap();
    assert_eq!(outcomes.try_recv(), Err(TryRecvError::Disconnected));
    completions.try_recv().unwrap();
    assert_eq!(completions.try_recv(), Err(TryRecvError::Disconnected));

    let gap_subscription = client.subscribe_completions(2);
    let retained_completion = step.tool_completions.completions[0].clone();
    step.ingress_outcomes.clear();
    step.events.clear();
    step.tool_completions.completions = vec![retained_completion];
    step.tool_completions.dropped_since_last_drain = 1;
    step.tool_completions.total_dropped = 1;
    step.tool_completions.sequence_exhausted = true;
    port.publish_step(&step);
    assert!(matches!(
        gap_subscription.try_recv().unwrap(),
        ToolCompletionDelivery::Completion(_)
    ));
    assert!(matches!(
        gap_subscription.try_recv().unwrap(),
        ToolCompletionDelivery::SourceGap(gap)
            if gap.dropped_since_last_drain == 1 && gap.sequence_exhausted
    ));

    step.tool_completions.completions.clear();
    step.tool_completions.dropped_since_last_drain = 0;
    port.publish_step(&step);
    assert_eq!(gap_subscription.try_recv(), Err(TryRecvError::Empty));
}
