use gate4agent_catalog::AgentRegistry;
use gate4agent_handle::{ToolClientDispatchError, ToolCompletionDelivery};
use gate4agent_runtime_native::{
    NativeProviderExecutor, NativeProviderExit, NativeProviderOperation,
    NativeProviderControlError, NativeProviderOperationError, NativeProviderResultPoll,
    NativeRuntime, NativeRuntimeConfig, ProviderOperationKey, ProviderStopCause,
    ProviderSupervisorState,
};
use gate4agent_testkit::{interactive_agent_spec, CONTROL_FIXTURE_ID};
use gate4agent_tool_engine::{
    CancellationDisposition, CapabilityClass, CapabilityDescriptor, CapabilityObservation,
    CapabilityOwner, CapabilityProviderDescriptor, CapabilityRequestId, CapabilityRequestInput,
    CapabilityResult, CapabilityResultDelivery, CapabilityResultMetadata,
    CapabilityTerminalOutcome, GrantMode, InvocationCancelReason, PolicyGrant, PolicyKey,
    ResourceScopeId, ToolActorId, ToolAuthorityCommand, ToolCapabilityId, ToolProviderId,
};
use gate4agent_types::{
    AgentId, AgentInstanceId, CommandEnvelope, CommandId, ControlCommand, ProviderRuntimePolicy,
    SessionGeneration, SessionStatus, StartRequest, TerminalSize, TransportKind,
    CONTROL_PROTOCOL_VERSION,
};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

const FIXTURE_TIMEOUT: Duration = Duration::from_secs(15);
const RECEIPT_DRAIN_REQUESTS: u64 = 10;
const CANCEL_BACKLOG_CLIENTS: u64 = 3;
const CANCEL_BACKLOG_REQUESTS_PER_CLIENT: u64 = 32;
const CANCEL_BACKLOG_REQUESTS: u64 =
    CANCEL_BACKLOG_CLIENTS * CANCEL_BACKLOG_REQUESTS_PER_CLIENT;

#[derive(Default)]
struct ProcessEvidence {
    starts: usize,
    last_pid: Option<u32>,
    operations: Vec<ProviderOperationKey>,
    stop_requests: usize,
    force_stop_requests: usize,
    reaped: usize,
    last_status: Option<ExitStatus>,
}

struct ControlledProviderExecutor {
    evidence: Arc<Mutex<ProcessEvidence>>,
    successful_starts_remaining: u64,
    success_release: Arc<AtomicBool>,
}

struct ControlledProviderOperation {
    evidence: Arc<Mutex<ProcessEvidence>>,
    child: Option<Child>,
    complete_successfully: bool,
    success_release: Arc<AtomicBool>,
    result_delivered: bool,
    stop_requested: bool,
    force_stop_requested: bool,
    defer_first_wait_after_force_stop: bool,
}

impl NativeProviderExecutor for ControlledProviderExecutor {
    fn start(
        &mut self,
        invocation: &gate4agent_handle::ProviderInvocation,
    ) -> Result<Box<dyn NativeProviderOperation>, gate4agent_tool_engine::ToolFailure> {
        let complete_successfully = self.successful_starts_remaining != 0;
        self.successful_starts_remaining = self.successful_starts_remaining.saturating_sub(1);
        let mut child = spawn_provider_child(complete_successfully);
        let pid = child.id();
        if !complete_successfully {
            assert!(
                child
                    .try_wait()
                    .expect("probe controlled provider child")
                    .is_none(),
                "controlled provider child must be alive after spawn"
            );
        }
        let mut evidence = lock(&self.evidence);
        evidence.starts += 1;
        evidence.last_pid = Some(pid);
        evidence.operations.push(ProviderOperationKey {
            binding_id: invocation.binding_id(),
            operation_id: invocation.operation_id(),
            request_key: invocation.request_key().clone(),
        });
        drop(evidence);
        Ok(Box::new(ControlledProviderOperation {
            evidence: Arc::clone(&self.evidence),
            child: Some(child),
            complete_successfully,
            success_release: Arc::clone(&self.success_release),
            result_delivered: false,
            stop_requested: false,
            force_stop_requested: false,
            defer_first_wait_after_force_stop: false,
        }))
    }
}

impl NativeProviderOperation for ControlledProviderOperation {
    fn request_stop(&mut self) -> Result<(), NativeProviderOperationError> {
        if self.stop_requested {
            return Ok(());
        }
        self.stop_requested = true;
        lock(&self.evidence).stop_requests += 1;
        Ok(())
    }

    fn request_force_stop(&mut self) -> Result<(), NativeProviderOperationError> {
        if self.force_stop_requested {
            return Ok(());
        }
        self.force_stop_requested = true;
        self.defer_first_wait_after_force_stop = true;
        lock(&self.evidence).force_stop_requests += 1;
        self.child
            .as_mut()
            .ok_or_else(|| NativeProviderOperationError::new("fixture-child-missing"))?
            .kill()
            .map_err(|_| NativeProviderOperationError::new("fixture-kill-failed"))
    }

    fn try_wait(&mut self) -> Result<Option<NativeProviderExit>, NativeProviderOperationError> {
        if self.force_stop_requested && self.defer_first_wait_after_force_stop {
            self.defer_first_wait_after_force_stop = false;
            return Ok(None);
        }
        let status = self
            .child
            .as_mut()
            .ok_or_else(|| NativeProviderOperationError::new("fixture-child-missing"))?
            .try_wait()
            .map_err(|_| NativeProviderOperationError::new("fixture-wait-failed"))?;
        let Some(status) = status else {
            return Ok(None);
        };
        self.child.take();
        let mut evidence = lock(&self.evidence);
        evidence.reaped += 1;
        evidence.last_status = Some(status);
        Ok(Some(NativeProviderExit {
            success: status.success(),
            code: status.code(),
        }))
    }

    fn try_poll_result(
        &mut self,
    ) -> Result<NativeProviderResultPoll, NativeProviderOperationError> {
        if !self.complete_successfully {
            return Ok(NativeProviderResultPoll::Pending);
        }
        if !self.success_release.load(Ordering::Acquire) {
            return Ok(NativeProviderResultPoll::Pending);
        }
        if self.result_delivered {
            return Ok(NativeProviderResultPoll::Closed);
        }
        self.result_delivered = true;
        Ok(NativeProviderResultPoll::Ready(success_observation()))
    }
}

impl Drop for ControlledProviderOperation {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            if let Ok(status) = child.wait() {
                let mut evidence = lock(&self.evidence);
                evidence.reaped += 1;
                evidence.last_status = Some(status);
            }
        }
    }
}

#[derive(Default)]
struct CancelPressureEvidence {
    starts: usize,
    stop_requests: usize,
    reaped: usize,
}

struct ImmediateStopExecutor {
    evidence: Arc<Mutex<CancelPressureEvidence>>,
}

struct ImmediateStopOperation {
    evidence: Arc<Mutex<CancelPressureEvidence>>,
    stopped: bool,
    reaped: bool,
}

impl NativeProviderExecutor for ImmediateStopExecutor {
    fn start(
        &mut self,
        _invocation: &gate4agent_handle::ProviderInvocation,
    ) -> Result<Box<dyn NativeProviderOperation>, gate4agent_tool_engine::ToolFailure> {
        lock(&self.evidence).starts += 1;
        Ok(Box::new(ImmediateStopOperation {
            evidence: Arc::clone(&self.evidence),
            stopped: false,
            reaped: false,
        }))
    }
}

impl NativeProviderOperation for ImmediateStopOperation {
    fn request_stop(&mut self) -> Result<(), NativeProviderOperationError> {
        if !self.stopped {
            self.stopped = true;
            lock(&self.evidence).stop_requests += 1;
        }
        Ok(())
    }

    fn request_force_stop(&mut self) -> Result<(), NativeProviderOperationError> {
        self.request_stop()
    }

    fn try_wait(&mut self) -> Result<Option<NativeProviderExit>, NativeProviderOperationError> {
        if !self.stopped || self.reaped {
            return Ok(None);
        }
        self.reaped = true;
        lock(&self.evidence).reaped += 1;
        Ok(Some(NativeProviderExit {
            success: false,
            code: None,
        }))
    }

    fn try_poll_result(
        &mut self,
    ) -> Result<NativeProviderResultPoll, NativeProviderOperationError> {
        Ok(NativeProviderResultPoll::Pending)
    }
}

#[tokio::test]
async fn native_provider_supervisor_reaps_cancelled_child_before_detach_and_higher_rebind() {
    let provider = provider_descriptor();
    let provider_id = provider.id.clone();
    let catalog = AgentRegistry::new([interactive_agent_spec()]).expect("fixture registry");
    let (handle, mut runtime) = NativeRuntime::new_with_tool_providers(
        catalog,
        NativeRuntimeConfig {
            provider_stop_grace_ms: 25,
            provider_shutdown_timeout_ms: 2_000,
            worker_poll_interval_ms: 5,
            ..NativeRuntimeConfig::default()
        },
        [provider],
    )
    .expect("native runtime with provider");
    let authority = runtime.tool_authority();
    let client = authority
        .bind_client(ToolActorId::new("agent.physical-e2e").unwrap())
        .expect("bind tool client");
    let completions = client.subscribe_completions(32);
    let instance_id = AgentInstanceId(4_401);

    handle
        .dispatch(command(
            1,
            ControlCommand::Register {
                instance_id,
                agent_id: AgentId::new(CONTROL_FIXTURE_ID).unwrap(),
                transport: TransportKind::Pty,
            },
        ))
        .expect("register controlled agent");
    handle
        .dispatch(command(
            2,
            ControlCommand::Start {
                instance_id,
                runtime_policy: ProviderRuntimePolicy::raw_pty(),
                request: StartRequest {
                    working_directory: std::env::current_dir()
                        .expect("current directory")
                        .to_string_lossy()
                        .into_owned(),
                    terminal_size: TerminalSize {
                        rows: 24,
                        columns: 80,
                    },
                    initial_prompt: None,
                    session_options: None,
                },
            },
        ))
        .expect("start controlled agent");
    drive_until(&mut runtime, |_| {
        handle.snapshot().sessions.first().is_some_and(|session| {
            session.status == SessionStatus::Running
                && session
                    .terminal_frame
                    .as_ref()
                    .is_some_and(|frame| frame.contents.contains("fixture-ready>"))
        })
    })
    .await;
    let generation = handle.snapshot().sessions[0].generation;

    let first_evidence = Arc::new(Mutex::new(ProcessEvidence::default()));
    let first_binding = runtime
        .install_native_provider(
            &provider_id,
            8,
            Box::new(ControlledProviderExecutor {
                evidence: Arc::clone(&first_evidence),
                successful_starts_remaining: 0,
                success_release: Arc::new(AtomicBool::new(true)),
            }),
        )
        .expect("install first provider supervisor");
    runtime.tick().await;
    let first_supervisor = runtime
        .native_provider_snapshot(&provider_id)
        .expect("first supervisor");
    assert_eq!(first_supervisor.provider_id, provider_id);
    assert_eq!(first_supervisor.binding_id, first_binding);
    assert_eq!(first_supervisor.state, ProviderSupervisorState::Running);

    let first_grant = grant(&client, instance_id, generation, GrantMode::Allow);
    authority
        .dispatch(ToolAuthorityCommand::SetGrant {
            grant: first_grant.clone(),
        })
        .expect("set first grant");
    runtime.tick().await;
    client
        .dispatch(request(1, instance_id, generation))
        .expect("dispatch first provider request");
    runtime.tick().await;
    let first_pid = lock(&first_evidence)
        .last_pid
        .expect("first controlled provider PID");
    assert_eq!(lock(&first_evidence).starts, 1);
    let first_operation = lock(&first_evidence).operations[0].clone();
    assert_eq!(first_operation.binding_id, first_binding);
    assert_eq!(first_operation.request_key.local_id, CapabilityRequestId(1));

    authority
        .dispatch(ToolAuthorityCommand::RevokeGrant {
            key: first_grant.key,
        })
        .expect("revoke first grant");
    runtime.tick().await;
    let ToolCompletionDelivery::Completion(completion) = completions
        .try_recv()
        .expect("grant-revoked terminal completion")
    else {
        panic!("expected terminal completion");
    };
    assert_eq!(completion.provider_id, provider_id);
    assert_eq!(completion.operation_id, Some(first_operation.operation_id));
    assert_eq!(completion.request_key, first_operation.request_key);
    assert!(matches!(
        &completion.outcome,
        CapabilityTerminalOutcome::GrantRevoked {
            cancellation: CancellationDisposition::CancelQueuedUnconfirmed,
        }
    ));
    assert_eq!(lock(&first_evidence).stop_requests, 1);
    assert_eq!(lock(&first_evidence).force_stop_requests, 0);
    assert_eq!(lock(&first_evidence).reaped, 0);

    runtime
        .retire_native_provider(&provider_id)
        .expect("begin first provider retirement");
    assert!(matches!(
        runtime.install_native_provider(
            &provider_id,
            8,
            Box::new(ControlledProviderExecutor {
                evidence: Arc::new(Mutex::new(ProcessEvidence::default())),
                successful_starts_remaining: 0,
                success_release: Arc::new(AtomicBool::new(true)),
            }),
        ),
        Err(NativeProviderControlError::AlreadyInstalled {
            state: ProviderSupervisorState::Draining,
        })
    ));
    runtime.tick().await;
    assert!(client.snapshot().available_providers.is_empty());
    assert_eq!(lock(&first_evidence).reaped, 0);
    let draining = runtime
        .native_provider_snapshot(&provider_id)
        .expect("draining first supervisor");
    assert_eq!(draining.state, ProviderSupervisorState::Draining);
    assert!(matches!(
        client.dispatch(request(99, instance_id, generation)),
        Err(ToolClientDispatchError::ProviderUnavailable { .. })
    ));

    let first_ack = tokio::time::timeout(FIXTURE_TIMEOUT, async {
        loop {
            runtime.tick().await;
            if let Some(ack) = runtime
                .drain_provider_exit_acks(8)
                .into_iter()
                .find(|ack| ack.operation.binding_id == first_binding)
            {
                break ack;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("physical exit acknowledgement timeout");
    assert_eq!(first_ack.provider_id, provider_id);
    assert_eq!(first_ack.binding_id, first_binding);
    assert_eq!(first_ack.operation, first_operation);
    assert_eq!(lock(&first_evidence).reaped, 1);
    assert_eq!(
        first_ack.stop_cause,
        Some(ProviderStopCause::Cancellation {
            reason: InvocationCancelReason::GrantRevoked,
        })
    );
    assert!(first_ack.stop_signal_attempted);
    assert!(first_ack.stop_signalled);
    assert!(first_ack.force_stop_attempted);
    assert!(first_ack.force_stop_signalled);
    assert_eq!(lock(&first_evidence).force_stop_requests, 1);
    assert!(matches!(
        runtime.install_native_provider(
            &provider_id,
            8,
            Box::new(ControlledProviderExecutor {
                evidence: Arc::new(Mutex::new(ProcessEvidence::default())),
                successful_starts_remaining: 0,
                success_release: Arc::new(AtomicBool::new(true)),
            }),
        ),
        Err(NativeProviderControlError::AlreadyInstalled {
            state: ProviderSupervisorState::Closing,
        })
    ));

    runtime.tick().await;
    assert_eq!(
        runtime
            .native_provider_snapshot(&provider_id)
            .expect("closed first supervisor")
            .state,
        ProviderSupervisorState::Closed
    );

    let second_evidence = Arc::new(Mutex::new(ProcessEvidence::default()));
    let success_release = Arc::new(AtomicBool::new(false));
    let second_binding = runtime
        .install_native_provider(
            &provider_id,
            8,
            Box::new(ControlledProviderExecutor {
                evidence: Arc::clone(&second_evidence),
                successful_starts_remaining: RECEIPT_DRAIN_REQUESTS,
                success_release: Arc::clone(&success_release),
            }),
        )
        .expect("install second provider supervisor");
    assert!(second_binding.0 > first_binding.0);
    runtime.tick().await;
    let second_supervisor = runtime
        .native_provider_snapshot(&provider_id)
        .expect("second supervisor");
    assert_eq!(second_supervisor.provider_id, provider_id);
    assert_eq!(second_supervisor.binding_id, second_binding);
    assert_eq!(second_supervisor.state, ProviderSupervisorState::Running);

    let second_grant = grant(&client, instance_id, generation, GrantMode::Allow);
    authority
        .dispatch(ToolAuthorityCommand::SetGrant {
            grant: second_grant.clone(),
        })
        .expect("set second grant");
    runtime.tick().await;

    for offset in 0..RECEIPT_DRAIN_REQUESTS {
        let local_id = 100 + offset;
        client
            .dispatch(request(local_id, instance_id, generation))
            .expect("dispatch completing provider request");
        runtime.tick().await;
    }
    assert_eq!(lock(&second_evidence).starts, RECEIPT_DRAIN_REQUESTS as usize);
    drive_until(&mut runtime, |_| {
        lock(&second_evidence).reaped == RECEIPT_DRAIN_REQUESTS as usize
    })
    .await;
    assert_eq!(lock(&second_evidence).reaped, RECEIPT_DRAIN_REQUESTS as usize);
    assert_eq!(
        runtime
            .native_provider_snapshot(&provider_id)
            .expect("provider operations waiting on release")
            .operations
            .len(),
        RECEIPT_DRAIN_REQUESTS as usize
    );

    success_release.store(true, Ordering::Release);
    runtime.tick().await;
    assert_eq!(
        runtime
            .native_provider_snapshot(&provider_id)
            .expect("provider completions pending submission")
            .operations
            .len(),
        RECEIPT_DRAIN_REQUESTS as usize
    );

    let (successful_completions, successful_acks) =
        tokio::time::timeout(FIXTURE_TIMEOUT, async {
            let mut successful_completions = Vec::new();
            let mut successful_acks = Vec::new();
            loop {
                runtime.tick().await;
                successful_acks.extend(runtime.drain_provider_exit_acks(32));
                while let Ok(delivery) = completions.try_recv() {
                    match delivery {
                        ToolCompletionDelivery::Completion(completion) => {
                            successful_completions.push(completion);
                        }
                        ToolCompletionDelivery::SourceGap(gap) => {
                            panic!("unexpected tool completion gap: {gap:?}");
                        }
                    }
                }
                if successful_completions.len() == RECEIPT_DRAIN_REQUESTS as usize
                    && successful_acks.len() == RECEIPT_DRAIN_REQUESTS as usize
                {
                    break (successful_completions, successful_acks);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("concurrent receipt pressure timeout");

    for offset in 0..RECEIPT_DRAIN_REQUESTS {
        let local_id = 100 + offset;
        let operation = lock(&second_evidence)
            .operations
            .iter()
            .find(|operation| operation.request_key.local_id == CapabilityRequestId(local_id))
            .expect("successful invocation provenance")
            .clone();
        let completion = successful_completions
            .iter()
            .find(|completion| completion.request_key.local_id == CapabilityRequestId(local_id))
            .expect("successful terminal completion");
        assert_eq!(operation.binding_id, second_binding);
        assert_eq!(completion.provider_id, provider_id);
        assert_eq!(completion.operation_id, Some(operation.operation_id));
        assert_eq!(
            &completion.outcome,
            &CapabilityTerminalOutcome::Succeeded {
                result: success_result(),
            }
        );
        let ack = successful_acks
            .iter()
            .find(|ack| ack.operation == operation)
            .expect("successful physical exit acknowledgement");
        assert_eq!(ack.provider_id, provider_id);
        assert_eq!(ack.binding_id, second_binding);
        assert_eq!(ack.stop_cause, None);
        assert!(!ack.stop_signal_attempted);
        assert!(!ack.stop_signalled);
        assert!(!ack.force_stop_attempted);
        assert!(!ack.force_stop_signalled);
    }
    runtime.tick().await;
    assert!(runtime.drain_provider_faults(32).is_empty());
    let snapshot = runtime
        .native_provider_snapshot(&provider_id)
        .expect("running supervisor after receipt pressure");
    assert_eq!(snapshot.provider_id, provider_id);
    assert_eq!(snapshot.binding_id, second_binding);
    assert_eq!(snapshot.state, ProviderSupervisorState::Running);
    assert!(snapshot.operations.is_empty());

    client
        .dispatch(request(2, instance_id, generation))
        .expect("dispatch second provider request");
    runtime.tick().await;
    let second_operation_snapshot = runtime
        .native_provider_snapshot(&provider_id)
        .expect("second supervisor")
        .operations
        .into_iter()
        .next()
        .expect("second supervised operation");
    let second_operation = lock(&second_evidence)
        .operations
        .last()
        .expect("second cancellation invocation provenance")
        .clone();
    assert_eq!(second_operation_snapshot.operation, second_operation);
    assert_eq!(second_operation.binding_id, second_binding);
    assert_eq!(second_operation.request_key.local_id, CapabilityRequestId(2));
    assert_eq!(
        lock(&second_evidence).starts,
        RECEIPT_DRAIN_REQUESTS as usize + 1
    );

    authority
        .dispatch(ToolAuthorityCommand::RevokeGrant {
            key: second_grant.key,
        })
        .expect("revoke second grant");
    runtime.tick().await;
    let ToolCompletionDelivery::Completion(completion) = completions
        .try_recv()
        .expect("second grant-revoked terminal completion")
    else {
        panic!("expected second terminal completion");
    };
    assert_eq!(completion.provider_id, provider_id);
    assert_eq!(completion.operation_id, Some(second_operation.operation_id));
    assert_eq!(completion.request_key, second_operation.request_key);
    assert!(matches!(
        &completion.outcome,
        CapabilityTerminalOutcome::GrantRevoked {
            cancellation: CancellationDisposition::CancelQueuedUnconfirmed,
        }
    ));
    assert_eq!(lock(&second_evidence).stop_requests, 1);
    assert_eq!(lock(&second_evidence).force_stop_requests, 0);

    assert!(!runtime.native_provider_shutdown_complete());
    runtime
        .shutdown_native_providers()
        .await
        .expect("coordinated native provider shutdown");
    let second_ack = runtime
        .drain_provider_exit_acks(8)
        .into_iter()
        .find(|ack| ack.operation == second_operation)
        .expect("second physical exit acknowledgement");
    assert_eq!(second_ack.provider_id, provider_id);
    assert_eq!(second_ack.binding_id, second_binding);
    assert_eq!(second_ack.operation, second_operation);
    assert_eq!(
        second_ack.stop_cause,
        Some(ProviderStopCause::Cancellation {
            reason: InvocationCancelReason::GrantRevoked,
        })
    );
    assert!(second_ack.stop_signal_attempted);
    assert!(second_ack.stop_signalled);
    assert!(second_ack.force_stop_attempted);
    assert!(second_ack.force_stop_signalled);
    assert_eq!(lock(&second_evidence).force_stop_requests, 1);

    assert_eq!(
        runtime
            .native_provider_snapshot(&provider_id)
            .expect("coordinated closed supervisor")
            .state,
        ProviderSupervisorState::Closed
    );
    assert!(client.snapshot().available_providers.is_empty());
    assert!(runtime.drain_provider_faults(8).is_empty());
    assert_eq!(
        lock(&second_evidence).reaped,
        RECEIPT_DRAIN_REQUESTS as usize + 1
    );

    handle
        .dispatch(command(
            3,
            ControlCommand::Stop {
                instance_id,
                force: true,
            },
        ))
        .expect("stop controlled agent");
    drive_until(&mut runtime, |_| {
        handle.snapshot().sessions.first().is_some_and(|session| {
            matches!(session.status, SessionStatus::Exited { .. })
        })
    })
    .await;

    eprintln!(
        "provider_pid={first_pid} physical_exit_reaped={} receipt_cycles={} first_binding={} second_binding={} coordinated_shutdown={}",
        lock(&first_evidence).reaped,
        RECEIPT_DRAIN_REQUESTS,
        first_binding.0,
        second_binding.0,
        runtime.native_provider_shutdown_complete(),
    );
}

#[tokio::test]
async fn coordinated_shutdown_drains_exact_cancel_beyond_one_work_quantum() {
    let provider = provider_descriptor();
    let provider_id = provider.id.clone();
    let catalog = AgentRegistry::new([interactive_agent_spec()]).expect("fixture registry");
    let (handle, mut runtime) = NativeRuntime::new_with_tool_providers(
        catalog,
        NativeRuntimeConfig {
            command_capacity: 256,
            max_commands_per_tick: 256,
            provider_stop_grace_ms: 25,
            provider_shutdown_timeout_ms: 2_000,
            worker_poll_interval_ms: 5,
            ..NativeRuntimeConfig::default()
        },
        [provider],
    )
    .expect("native runtime with pressure provider");
    let authority = runtime.tool_authority();
    let clients = [
        "agent.cancel-pressure-0",
        "agent.cancel-pressure-1",
        "agent.cancel-pressure-2",
    ]
    .into_iter()
    .map(|actor| {
        authority
            .bind_client(ToolActorId::new(actor).unwrap())
            .expect("bind pressure client")
    })
    .collect::<Vec<_>>();
    let instance_id = AgentInstanceId(4_402);

    handle
        .dispatch(command(
            11,
            ControlCommand::Register {
                instance_id,
                agent_id: AgentId::new(CONTROL_FIXTURE_ID).unwrap(),
                transport: TransportKind::Pty,
            },
        ))
        .expect("register pressure agent");
    handle
        .dispatch(command(
            12,
            ControlCommand::Start {
                instance_id,
                runtime_policy: ProviderRuntimePolicy::raw_pty(),
                request: StartRequest {
                    working_directory: std::env::current_dir()
                        .expect("current directory")
                        .to_string_lossy()
                        .into_owned(),
                    terminal_size: TerminalSize {
                        rows: 24,
                        columns: 80,
                    },
                    initial_prompt: None,
                    session_options: None,
                },
            },
        ))
        .expect("start pressure agent");
    drive_until(&mut runtime, |_| {
        handle.snapshot().sessions.first().is_some_and(|session| {
            session.status == SessionStatus::Running
                && session
                    .terminal_frame
                    .as_ref()
                    .is_some_and(|frame| frame.contents.contains("fixture-ready>"))
        })
    })
    .await;
    let generation = handle.snapshot().sessions[0].generation;

    let evidence = Arc::new(Mutex::new(CancelPressureEvidence::default()));
    let binding = runtime
        .install_native_provider(
            &provider_id,
            128,
            Box::new(ImmediateStopExecutor {
                evidence: Arc::clone(&evidence),
            }),
        )
        .expect("install pressure provider");
    runtime.tick().await;
    let pressure_grants = clients
        .iter()
        .map(|client| grant(client, instance_id, generation, GrantMode::Allow))
        .collect::<Vec<_>>();
    for pressure_grant in &pressure_grants {
        authority
            .dispatch(ToolAuthorityCommand::SetGrant {
                grant: pressure_grant.clone(),
            })
            .expect("set pressure grant");
    }
    runtime.tick().await;

    for client in &clients {
        for offset in 0..CANCEL_BACKLOG_REQUESTS_PER_CLIENT {
            client
                .dispatch(request(1_000 + offset, instance_id, generation))
                .expect("dispatch pressure request");
        }
    }
    runtime.tick().await;
    runtime.tick().await;
    assert_eq!(lock(&evidence).starts, CANCEL_BACKLOG_REQUESTS as usize);

    for pressure_grant in pressure_grants {
        authority
            .dispatch(ToolAuthorityCommand::RevokeGrant {
                key: pressure_grant.key,
            })
            .expect("revoke pressure grant");
    }
    runtime.tick().await;
    assert_eq!(lock(&evidence).stop_requests, CANCEL_BACKLOG_REQUESTS as usize);
    assert_eq!(lock(&evidence).reaped, CANCEL_BACKLOG_REQUESTS as usize);
    let snapshot = runtime
        .native_provider_snapshot(&provider_id)
        .expect("pressure supervisor after first cancel quantum");
    assert_eq!(snapshot.binding_id, binding);
    assert_eq!(
        snapshot.operations.len(),
        (CANCEL_BACKLOG_REQUESTS - 64) as usize
    );

    runtime
        .shutdown_native_providers()
        .await
        .expect("pressure provider coordinated shutdown");
    let acks = runtime.drain_provider_exit_acks(128);
    assert_eq!(acks.len(), CANCEL_BACKLOG_REQUESTS as usize);
    assert!(acks.iter().all(|ack| {
        ack.binding_id == binding
            && ack.stop_cause
                == Some(ProviderStopCause::Cancellation {
                    reason: InvocationCancelReason::GrantRevoked,
                })
    }));
    assert!(runtime.drain_provider_faults(128).is_empty());
    assert!(runtime.native_provider_shutdown_complete());

    handle
        .dispatch(command(
            13,
            ControlCommand::Stop {
                instance_id,
                force: true,
            },
        ))
        .expect("stop pressure agent");
    drive_until(&mut runtime, |_| {
        handle.snapshot().sessions.first().is_some_and(|session| {
            matches!(session.status, SessionStatus::Exited { .. })
        })
    })
    .await;
}

async fn drive_until(
    runtime: &mut NativeRuntime,
    mut predicate: impl FnMut(&NativeRuntime) -> bool,
) {
    tokio::time::timeout(FIXTURE_TIMEOUT, async {
        loop {
            runtime.tick().await;
            if predicate(runtime) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("controlled native provider E2E timeout");
}

fn provider_descriptor() -> CapabilityProviderDescriptor {
    CapabilityProviderDescriptor {
        id: provider_id(),
        owner: CapabilityOwner::Gate,
        capabilities: vec![CapabilityDescriptor::new(
            capability_id(),
            CapabilityClass::Browser,
            "Return controlled page metadata",
        )
        .unwrap()],
    }
}

fn provider_id() -> ToolProviderId {
    ToolProviderId::new("gate.browser.physical-e2e").unwrap()
}

fn capability_id() -> ToolCapabilityId {
    ToolCapabilityId::new("browser.snapshot").unwrap()
}

fn resource_scope() -> ResourceScopeId {
    ResourceScopeId::new("page.active").unwrap()
}

fn success_result() -> CapabilityResult {
    CapabilityResult {
        metadata: CapabilityResultMetadata {
            byte_len: 2,
            media_type: Some("application/json".to_owned()),
            truncated: false,
            redacted_summary: Some("controlled provider result".to_owned()),
        },
        delivery: CapabilityResultDelivery::Inline {
            bytes: b"{}".to_vec(),
        },
    }
}

fn success_observation() -> CapabilityObservation {
    CapabilityObservation::Succeeded {
        result: success_result(),
    }
}

fn grant(
    client: &gate4agent_handle::ToolClientHandle,
    instance_id: AgentInstanceId,
    generation: SessionGeneration,
    mode: GrantMode,
) -> PolicyGrant {
    PolicyGrant {
        key: PolicyKey {
            consumer_id: client.consumer_id().clone(),
            actor_id: client.actor_id().clone(),
            instance_id,
            generation,
            provider_id: provider_id(),
            capability_id: capability_id(),
            resource_scope_id: resource_scope(),
        },
        mode,
    }
}

fn request(
    local_id: u64,
    instance_id: AgentInstanceId,
    generation: SessionGeneration,
) -> CapabilityRequestInput {
    CapabilityRequestInput {
        local_id: CapabilityRequestId(local_id),
        instance_id,
        generation,
        provider_id: provider_id(),
        capability_id: capability_id(),
        resource_scope_id: resource_scope(),
        approval_summary: "Read controlled page metadata".to_owned(),
        deadline_tick: 10_000,
        payload: br#"{"scope":"active-page"}"#.to_vec(),
    }
}

fn command(id: u64, command: ControlCommand) -> CommandEnvelope {
    CommandEnvelope {
        protocol_version: CONTROL_PROTOCOL_VERSION,
        id: CommandId(id),
        command,
    }
}

fn spawn_provider_child(exit_successfully: bool) -> Child {
    #[cfg(windows)]
    let mut command = {
        let mut command = Command::new("powershell.exe");
        command.args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            if exit_successfully {
                "exit 0"
            } else {
                "Start-Sleep -Seconds 120"
            },
        ]);
        command
    };

    #[cfg(not(windows))]
    let mut command = {
        let mut command = if exit_successfully {
            Command::new("true")
        } else {
            let mut command = Command::new("sleep");
            command.arg("120");
            command
        };
        command
    };

    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn controlled provider child")
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
