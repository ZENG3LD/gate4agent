use gate4agent_handle::{
    bounded_control_plane, ProviderObservationStatus, ProviderRuntimeError, ProviderRuntimeState,
    ProviderWork, ToolAuthorityError, ToolClientDispatchError, ToolCompletionDelivery,
};
use gate4agent_kernel::{BackendIngress, Gate4AgentKernel};
use gate4agent_tool_protocol::{
    CancellationDisposition, CapabilityClass, CapabilityDescriptor, CapabilityOwner,
    CapabilityProviderDescriptor, CapabilityRequestId, CapabilityRequestInput, CapabilityResult,
    CapabilityResultDelivery, CapabilityResultMetadata, CapabilityTerminalOutcome, GrantMode,
    InvocationCancelReason, ObservationIgnoredReason, PolicyGrant, PolicyKey, ResourceScopeId,
    ToolActorId, ToolAuthorityCommand, ToolCapabilityId, ToolProviderId,
    CAPABILITY_PROTOCOL_VERSION,
};
use gate4agent_types::{
    AgentId, AgentInstanceId, CommandEnvelope, CommandId, ControlCommand, ControlObservation,
    ObservationEnvelope, ProviderRuntimePolicy, SessionGeneration, StartRequest, TerminalSize,
    TransportKind, CONTROL_PROTOCOL_VERSION,
};
use std::sync::mpsc::TryRecvError;

fn instance() -> AgentInstanceId {
    AgentInstanceId(71)
}

fn provider_id() -> ToolProviderId {
    ToolProviderId::new("gate.browser.test").unwrap()
}

fn secondary_provider_id() -> ToolProviderId {
    ToolProviderId::new("gate.browser.secondary").unwrap()
}

fn capability_id() -> ToolCapabilityId {
    ToolCapabilityId::new("browser.snapshot").unwrap()
}

fn resource_scope() -> ResourceScopeId {
    ResourceScopeId::new("page.active").unwrap()
}

fn provider() -> CapabilityProviderDescriptor {
    provider_descriptor(provider_id())
}

fn secondary_provider() -> CapabilityProviderDescriptor {
    provider_descriptor(secondary_provider_id())
}

fn provider_descriptor(id: ToolProviderId) -> CapabilityProviderDescriptor {
    CapabilityProviderDescriptor {
        id,
        owner: CapabilityOwner::Gate,
        capabilities: vec![CapabilityDescriptor::new(
            capability_id(),
            CapabilityClass::Browser,
            "Return active page metadata",
        )
        .unwrap()],
    }
}

fn result() -> CapabilityResult {
    CapabilityResult {
        metadata: CapabilityResultMetadata {
            byte_len: 2,
            media_type: Some("application/json".to_owned()),
            truncated: false,
            redacted_summary: Some("page snapshot captured".to_owned()),
        },
        delivery: CapabilityResultDelivery::Inline {
            bytes: b"{}".to_vec(),
        },
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
            runtime_policy: ProviderRuntimePolicy::new(true, true, true, true, true).unwrap(),
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
    let starting = kernel.step_control_plane(port.drain_ingress(8), []);
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
    );
    port.publish_step(&running);
    running.backend_snapshot.control.sessions[0].generation
}

fn attach_provider_runtime(
    kernel: &mut Gate4AgentKernel,
    port: &gate4agent_handle::ControlPlaneKernelPort,
    authority: &gate4agent_handle::ProviderRuntimeAuthorityHandle,
    provider_id: ToolProviderId,
    effect_capacity: usize,
) -> gate4agent_handle::ProviderRuntimeHandle {
    let runtime = authority
        .bind_provider(provider_id, effect_capacity)
        .unwrap();
    let attached = kernel.step_control_plane(port.drain_ingress(16), []);
    port.publish_step(&attached);
    assert_eq!(runtime.state(), ProviderRuntimeState::Active);
    runtime
}

#[test]
fn control_plane_e2e_routes_bound_provider_result_only_to_exact_client() {
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
        [provider(), secondary_provider()],
    )
    .unwrap();
    let provider_authority = port.provider_authority();
    let provider_runtime = provider_authority.bind_provider(provider_id(), 4).unwrap();
    let secondary_runtime = provider_authority
        .bind_provider(secondary_provider_id(), 4)
        .unwrap();
    let generation = start_running(&mut kernel, &gate, &port);
    assert_eq!(provider_runtime.state(), ProviderRuntimeState::Active);
    assert_eq!(secondary_runtime.state(), ProviderRuntimeState::Active);

    authority
        .dispatch(ToolAuthorityCommand::SetGrant {
            grant: grant(&client, generation, GrantMode::Allow),
        })
        .unwrap();
    let granted = kernel.step_control_plane(port.drain_ingress(8), []);
    port.publish_step(&granted);
    assert!(authority_outcomes.try_recv().unwrap().result.is_ok());

    let request_key = client.dispatch(request(1, generation)).unwrap();
    let dispatched = kernel.step_control_plane(port.drain_ingress(8), []);
    assert_eq!(dispatched.tool_effects.len(), 1);
    assert_eq!(
        dispatched.tool_effects[0].binding_id,
        provider_runtime.binding_id()
    );
    port.publish_step(&dispatched);
    let ProviderWork::Invoke(mut invocation) = provider_runtime.try_recv().unwrap() else {
        panic!("expected provider invocation");
    };
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

    assert_eq!(
        secondary_runtime.try_succeed(&mut invocation, &result()),
        Err(ProviderRuntimeError::ForeignInvocation)
    );
    let observation_sequence = provider_runtime
        .try_succeed(&mut invocation, &result())
        .unwrap();
    let completed = kernel.step_control_plane(port.drain_ingress(8), []);
    port.publish_step(&completed);
    let observation_outcome = provider_runtime.try_recv_observation_outcome().unwrap();
    assert_eq!(observation_outcome.sequence, observation_sequence);
    assert_eq!(
        observation_outcome.status,
        ProviderObservationStatus::Applied
    );
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
    let step = kernel.step_control_plane(port.drain_ingress(8), []);
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
fn detach_fences_approval_and_issued_work_then_allows_fresh_binding() {
    let (gate, authority, port) = bounded_control_plane(16);
    let client = authority
        .bind_client(ToolActorId::new("agent.detach").unwrap())
        .unwrap();
    let completions = client.subscribe_completions(8);
    let mut kernel = Gate4AgentKernel::with_tool_providers(
        gate4agent_catalog::builtin_registry().clone(),
        [provider()],
    )
    .unwrap();
    let provider_authority = port.provider_authority();
    let first_runtime = provider_authority.bind_provider(provider_id(), 4).unwrap();
    let first_binding = first_runtime.binding_id();
    let generation = start_running(&mut kernel, &gate, &port);

    authority
        .dispatch(ToolAuthorityCommand::SetGrant {
            grant: grant(&client, generation, GrantMode::RequireApproval),
        })
        .unwrap();
    let grant_step = kernel.step_control_plane(port.drain_ingress(8), []);
    port.publish_step(&grant_step);
    client.dispatch(request(1, generation)).unwrap();
    let awaiting = kernel.step_control_plane(port.drain_ingress(8), []);
    assert!(awaiting.tool_effects.is_empty());
    port.publish_step(&awaiting);

    first_runtime.close().unwrap();
    let detached_approval = kernel.step_control_plane(port.drain_ingress(8), []);
    port.publish_step(&detached_approval);
    assert_eq!(first_runtime.state(), ProviderRuntimeState::Closed);
    let ToolCompletionDelivery::Completion(approval_completion) = completions.try_recv().unwrap()
    else {
        panic!("expected provider-detached approval completion");
    };
    assert!(matches!(
        approval_completion.outcome,
        CapabilityTerminalOutcome::ProviderDetached {
            cancellation: CancellationDisposition::NotRequired,
        }
    ));

    let second_runtime =
        attach_provider_runtime(&mut kernel, &port, &provider_authority, provider_id(), 4);
    assert_ne!(second_runtime.binding_id(), first_binding);
    authority
        .dispatch(ToolAuthorityCommand::SetGrant {
            grant: grant(&client, generation, GrantMode::Allow),
        })
        .unwrap();
    let allow_step = kernel.step_control_plane(port.drain_ingress(8), []);
    port.publish_step(&allow_step);

    client.dispatch(request(2, generation)).unwrap();
    let invoked = kernel.step_control_plane(port.drain_ingress(8), []);
    port.publish_step(&invoked);
    let ProviderWork::Invoke(mut old_invocation) = second_runtime.try_recv().unwrap() else {
        panic!("expected issued provider invocation");
    };
    let old_completion = second_runtime.completion_handle();
    let cancellation = old_invocation.cancellation_token();
    let second_binding = second_runtime.binding_id();
    second_runtime.close().unwrap();
    assert!(cancellation.is_cancelled());
    assert!(matches!(
        second_runtime.try_recv(),
        Err(TryRecvError::Disconnected)
    ));

    let detached_issued = kernel.step_control_plane(port.drain_ingress(8), []);
    port.publish_step(&detached_issued);
    let ToolCompletionDelivery::Completion(issued_completion) = completions.try_recv().unwrap()
    else {
        panic!("expected provider-detached issued completion");
    };
    assert!(matches!(
        issued_completion.outcome,
        CapabilityTerminalOutcome::ProviderDetached {
            cancellation: CancellationDisposition::ProviderDetachedUnconfirmed,
        }
    ));
    assert_eq!(
        old_completion.try_succeed(&mut old_invocation, &result()),
        Err(ProviderRuntimeError::Inactive)
    );

    let third_runtime =
        attach_provider_runtime(&mut kernel, &port, &provider_authority, provider_id(), 4);
    assert_ne!(third_runtime.binding_id(), second_binding);
    assert_eq!(
        old_completion.try_succeed(&mut old_invocation, &result()),
        Err(ProviderRuntimeError::Inactive)
    );
    assert_eq!(client.snapshot().available_providers, vec![provider_id()]);
}

#[test]
fn full_provider_work_queue_fails_closed_and_hides_availability() {
    let (gate, authority, port) = bounded_control_plane(16);
    let client = authority
        .bind_client(ToolActorId::new("agent.provider-full").unwrap())
        .unwrap();
    let completions = client.subscribe_completions(4);
    let mut kernel = Gate4AgentKernel::with_tool_providers(
        gate4agent_catalog::builtin_registry().clone(),
        [provider()],
    )
    .unwrap();
    let provider_authority = port.provider_authority();
    let runtime = provider_authority.bind_provider(provider_id(), 1).unwrap();
    let old_binding = runtime.binding_id();
    let binding_cancellation = runtime.cancellation_token();
    let generation = start_running(&mut kernel, &gate, &port);
    authority
        .dispatch(ToolAuthorityCommand::SetGrant {
            grant: grant(&client, generation, GrantMode::Allow),
        })
        .unwrap();
    let grant_step = kernel.step_control_plane(port.drain_ingress(8), []);
    port.publish_step(&grant_step);

    client.dispatch(request(1, generation)).unwrap();
    client.dispatch(request(2, generation)).unwrap();
    let invoked = kernel.step_control_plane(port.drain_ingress(8), []);
    assert_eq!(invoked.tool_effects.len(), 2);
    let report = port.publish_step(&invoked);
    assert_eq!(report.provider_effects.delivered, 1);
    assert_eq!(report.provider_effects.queue_full, 1);
    assert_eq!(runtime.state(), ProviderRuntimeState::Closing);
    assert!(binding_cancellation.is_cancelled());
    assert!(matches!(
        runtime.try_recv(),
        Err(TryRecvError::Disconnected)
    ));
    assert!(client.snapshot().available_providers.is_empty());

    let detached = kernel.step_control_plane(port.drain_ingress(8), []);
    port.publish_step(&detached);
    assert_eq!(runtime.state(), ProviderRuntimeState::Closed);
    for _ in 0..2 {
        let ToolCompletionDelivery::Completion(completion) = completions.try_recv().unwrap() else {
            panic!("expected fail-closed provider completion");
        };
        assert!(matches!(
            completion.outcome,
            CapabilityTerminalOutcome::ProviderDetached {
                cancellation: CancellationDisposition::ProviderDetachedUnconfirmed,
            }
        ));
    }

    let replacement =
        attach_provider_runtime(&mut kernel, &port, &provider_authority, provider_id(), 2);
    assert_ne!(replacement.binding_id(), old_binding);
}

#[test]
fn earlier_revoke_is_reduced_before_later_provider_success() {
    let (gate, authority, port) = bounded_control_plane(16);
    let client = authority
        .bind_client(ToolActorId::new("agent.revoke-order").unwrap())
        .unwrap();
    let completions = client.subscribe_completions(2);
    let mut kernel = Gate4AgentKernel::with_tool_providers(
        gate4agent_catalog::builtin_registry().clone(),
        [provider()],
    )
    .unwrap();
    let provider_authority = port.provider_authority();
    let runtime = provider_authority.bind_provider(provider_id(), 4).unwrap();
    let generation = start_running(&mut kernel, &gate, &port);
    let policy = grant(&client, generation, GrantMode::Allow);
    authority
        .dispatch(ToolAuthorityCommand::SetGrant {
            grant: policy.clone(),
        })
        .unwrap();
    let grant_step = kernel.step_control_plane(port.drain_ingress(8), []);
    port.publish_step(&grant_step);

    client.dispatch(request(1, generation)).unwrap();
    let invoked = kernel.step_control_plane(port.drain_ingress(8), []);
    port.publish_step(&invoked);
    let ProviderWork::Invoke(mut invocation) = runtime.try_recv().unwrap() else {
        panic!("expected provider invocation");
    };
    let invocation_cancellation = invocation.cancellation_token();

    authority
        .dispatch(ToolAuthorityCommand::RevokeGrant { key: policy.key })
        .unwrap();
    runtime.try_succeed(&mut invocation, &result()).unwrap();
    let ordered = kernel.step_control_plane(port.drain_ingress(8), []);
    port.publish_step(&ordered);
    assert!(invocation_cancellation.is_cancelled());

    let ToolCompletionDelivery::Completion(completion) = completions.try_recv().unwrap() else {
        panic!("expected revoke completion");
    };
    assert!(matches!(
        completion.outcome,
        CapabilityTerminalOutcome::GrantRevoked { .. }
    ));
    assert_eq!(
        runtime.try_recv_observation_outcome().unwrap().status,
        ProviderObservationStatus::Ignored {
            reason: ObservationIgnoredReason::RequestNotDispatched,
        }
    );
    let ProviderWork::Cancel(cancellation) = runtime.try_recv().unwrap() else {
        panic!("expected revoke cancellation");
    };
    assert_eq!(cancellation.reason(), InvocationCancelReason::GrantRevoked);
    assert_eq!(runtime.state(), ProviderRuntimeState::Active);
    assert!(!runtime.cancellation_token().is_cancelled());

    authority
        .dispatch(ToolAuthorityCommand::SetGrant {
            grant: grant(&client, generation, GrantMode::Allow),
        })
        .unwrap();
    let regranted = kernel.step_control_plane(port.drain_ingress(8), []);
    port.publish_step(&regranted);
    client.dispatch(request(2, generation)).unwrap();
    let next = kernel.step_control_plane(port.drain_ingress(8), []);
    port.publish_step(&next);
    assert!(matches!(runtime.try_recv(), Ok(ProviderWork::Invoke(_))));
}

#[test]
fn dropped_attaching_runtime_is_retired_once_and_can_rebind() {
    let (_gate, _authority, port) = bounded_control_plane(8);
    let provider_authority = port.provider_authority();
    let mut kernel = Gate4AgentKernel::with_tool_providers(
        gate4agent_catalog::builtin_registry().clone(),
        [provider()],
    )
    .unwrap();

    let runtime = provider_authority.bind_provider(provider_id(), 2).unwrap();
    let old_binding = runtime.binding_id();
    drop(runtime);
    let retired = kernel.step_control_plane(port.drain_ingress(8), []);
    assert_eq!(retired.ingress_outcomes.len(), 2);
    port.publish_step(&retired);
    assert_eq!(provider_authority.active_binding_count(), 0);

    let replacement =
        attach_provider_runtime(&mut kernel, &port, &provider_authority, provider_id(), 2);
    assert_ne!(replacement.binding_id(), old_binding);
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
fn registered_provider_without_local_runtime_is_rejected_before_ingress() {
    let (_gate, authority, port) = bounded_control_plane(8);
    let client = authority
        .bind_client(ToolActorId::new("agent.no-runtime").unwrap())
        .unwrap();
    let mut kernel = Gate4AgentKernel::with_tool_providers(
        gate4agent_catalog::builtin_registry().clone(),
        [provider()],
    )
    .unwrap();
    let initial = kernel.step_control_plane([], []);
    port.publish_step(&initial);

    assert_eq!(
        client.dispatch(request(1, SessionGeneration(0))),
        Err(ToolClientDispatchError::ProviderUnavailable {
            provider_id: provider_id(),
        })
    );
    assert!(port.drain_ingress(8).is_empty());
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
    let provider_authority = port.provider_authority();
    let _provider_runtime = provider_authority.bind_provider(provider_id(), 2).unwrap();
    let generation = start_running(&mut kernel, &gate, &port);

    authority
        .dispatch(ToolAuthorityCommand::SetGrant {
            grant: grant(&client, generation, GrantMode::RequireApproval),
        })
        .unwrap();
    let granted = kernel.step_control_plane(port.drain_ingress(8), []);
    port.publish_step(&granted);
    authority_subscription.try_recv().unwrap();

    client.dispatch(request(9, generation)).unwrap();
    let awaiting = kernel.step_control_plane(port.drain_ingress(8), []);
    assert!(awaiting.tool_effects.is_empty());
    port.publish_step(&awaiting);

    authority.close_client(&client).unwrap();
    assert_eq!(
        clone.dispatch(request(10, generation)),
        Err(ToolClientDispatchError::Inactive)
    );
    let closed = kernel.step_control_plane(port.drain_ingress(8), []);
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
    let mut step = kernel.step_control_plane(port.drain_ingress(8), []);
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
