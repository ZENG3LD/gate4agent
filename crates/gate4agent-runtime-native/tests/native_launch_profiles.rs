use std::ffi::OsString;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gate4agent_catalog::{AgentRegistry, EnvMutation};
use gate4agent_runtime_native::{
    HookIngressConfig, NativeChildEnvironmentResolveError, NativeChildEnvironmentResolver,
    NativeInstanceLaunchOverlay, NativeLaunchEnvironmentOverlay, NativeLaunchProfile,
    NativeLaunchProfileControl, NativeLaunchProfileError, NativeLaunchProfileId, NativeRuntime,
    NativeRuntimeConfig,
    ZAI_GLM_ANTHROPIC_BASE_URL, ZAI_GLM_CLAUDE_OPTIONAL_ENV_KEYS,
    ZAI_GLM_CLAUDE_OWNED_ENV_KEYS, ZAI_GLM_CLAUDE_PROFILE, ZAI_GLM_CLAUDE_PROFILE_ID,
    ZAI_GLM_CLAUDE_PROFILE_REVISION,
    ZAI_GLM_CLAUDE_REQUIRED_ENV_KEYS,
};
use gate4agent_testkit::{
    hook_posting_agent_spec, interactive_agent_spec, one_shot_agent_spec, pipe_agent_spec,
    CONTROL_FIXTURE_ID, HOOK_POSTING_FIXTURE_ID, ONE_SHOT_FIXTURE_ID, PIPE_FIXTURE_ID,
};
use gate4agent_types::{
    AdapterFamily, AgentId, AgentInstanceId, AgentSpec, CommandEnvelope, CommandId,
    ControlCommand, ControlEventKind, ProviderEvent, ProviderRuntimePolicy, ProviderSource,
    SessionStatus, StartRequest, TerminalSize, TransportKind, CONTROL_PROTOCOL_VERSION,
};

const PROFILE_SENTINEL: &str = "GATE4AGENT_TEST_PROFILE_SENTINEL";
const REMOVE_SENTINEL: &str = "GATE4AGENT_TEST_REMOVE_SENTINEL";
const CHILD_SENTINEL: &str = "GATE4AGENT_TEST_CHILD_SENTINEL";
const OVERLAY_SENTINEL: &str = "GATE4AGENT_TEST_OVERLAY_SENTINEL";
const ZAI_TEST_TOKEN: &str = "gate4agent-zai-fixture-token";

struct EnvironmentGuard {
    values: Vec<(&'static str, Option<OsString>)>,
}

struct EmptyResolver;

impl NativeChildEnvironmentResolver for EmptyResolver {
    fn resolve_child_environment(
        &self,
    ) -> Result<Vec<EnvMutation>, NativeChildEnvironmentResolveError> {
        Ok(Vec::new())
    }
}

struct SentinelResolver {
    generation: Arc<AtomicUsize>,
    calls: Arc<AtomicUsize>,
}

struct ReservedOutputResolver;

struct ZaiGlmFixtureResolver {
    token: Option<&'static str>,
    endpoint: Option<&'static str>,
}

impl NativeChildEnvironmentResolver for ReservedOutputResolver {
    fn resolve_child_environment(
        &self,
    ) -> Result<Vec<EnvMutation>, NativeChildEnvironmentResolveError> {
        Ok(vec![EnvMutation {
            key: OsString::from("GATE4AGENT_HOOK_TOKEN"),
            value: Some(OsString::from("sentinel-not-a-token")),
        }])
    }
}

impl NativeChildEnvironmentResolver for SentinelResolver {
    fn resolve_child_environment(
        &self,
    ) -> Result<Vec<EnvMutation>, NativeChildEnvironmentResolveError> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        let generation = self.generation.load(Ordering::Acquire);
        Ok(vec![
            EnvMutation {
                key: OsString::from(PROFILE_SENTINEL),
                value: Some(OsString::from(format!("profile-value-{generation}"))),
            },
            EnvMutation {
                key: OsString::from(REMOVE_SENTINEL),
                value: None,
            },
            EnvMutation {
                key: OsString::from(CHILD_SENTINEL),
                value: Some(OsString::from("child-only")),
            },
        ])
    }
}

impl NativeChildEnvironmentResolver for ZaiGlmFixtureResolver {
    fn resolve_child_environment(
        &self,
    ) -> Result<Vec<EnvMutation>, NativeChildEnvironmentResolveError> {
        Ok(vec![
            EnvMutation {
                key: OsString::from("ANTHROPIC_AUTH_TOKEN"),
                value: self.token.map(OsString::from),
            },
            EnvMutation {
                key: OsString::from("ANTHROPIC_BASE_URL"),
                value: self.endpoint.map(OsString::from),
            },
            EnvMutation {
                key: OsString::from("API_TIMEOUT_MS"),
                value: None,
            },
            EnvMutation {
                key: OsString::from("ANTHROPIC_DEFAULT_HAIKU_MODEL"),
                value: None,
            },
            EnvMutation {
                key: OsString::from("ANTHROPIC_DEFAULT_SONNET_MODEL"),
                value: None,
            },
            EnvMutation {
                key: OsString::from("ANTHROPIC_DEFAULT_OPUS_MODEL"),
                value: None,
            },
            EnvMutation {
                key: OsString::from("CLAUDE_CODE_AUTO_COMPACT_WINDOW"),
                value: None,
            },
            EnvMutation {
                key: OsString::from("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"),
                value: None,
            },
        ])
    }
}

impl EnvironmentGuard {
    fn set(values: &[(&'static str, &'static str)]) -> Self {
        let previous = values
            .iter()
            .map(|(key, value)| {
                let previous = std::env::var_os(key);
                std::env::set_var(key, value);
                (*key, previous)
            })
            .collect();
        Self { values: previous }
    }
}

impl Drop for EnvironmentGuard {
    fn drop(&mut self) {
        for (key, value) in self.values.drain(..) {
            if let Some(value) = value {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
        }
    }
}

fn profile_id() -> NativeLaunchProfileId {
    NativeLaunchProfileId::new("isolated-sentinel").unwrap()
}

fn fixture_agent_id() -> AgentId {
    AgentId::new(HOOK_POSTING_FIXTURE_ID).unwrap()
}

fn zai_glm_fixture_spec(report_environment: bool) -> AgentSpec {
    let mut spec = interactive_agent_spec();
    spec.id = AgentId::new("claude").unwrap();
    if report_environment {
        let original_script = spec
            .launch
            .fixed_args
            .last_mut()
            .expect("fixture launch script");
        #[cfg(windows)]
        {
            *original_script = format!(
                "$optionalAbsent = [string]::IsNullOrEmpty($env:API_TIMEOUT_MS) -and [string]::IsNullOrEmpty($env:ANTHROPIC_DEFAULT_HAIKU_MODEL) -and [string]::IsNullOrEmpty($env:ANTHROPIC_DEFAULT_SONNET_MODEL) -and [string]::IsNullOrEmpty($env:ANTHROPIC_DEFAULT_OPUS_MODEL) -and [string]::IsNullOrEmpty($env:CLAUDE_CODE_AUTO_COMPACT_WINDOW) -and [string]::IsNullOrEmpty($env:CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC); [Console]::Write('zai-base=' + $env:ANTHROPIC_BASE_URL + ';token-present=' + (-not [string]::IsNullOrEmpty($env:ANTHROPIC_AUTH_TOKEN)) + ';optional-absent=' + $optionalAbsent + ';'); {original_script}"
            );
        }
        #[cfg(not(windows))]
        {
            *original_script = format!(
                "printf 'zai-base=%s;token-present=%s;optional-absent=%s;' \"$ANTHROPIC_BASE_URL\" \"$(if [ -n \"$ANTHROPIC_AUTH_TOKEN\" ]; then printf true; else printf false; fi)\" \"$(if [ -z \"$API_TIMEOUT_MS\" ] && [ -z \"$ANTHROPIC_DEFAULT_HAIKU_MODEL\" ] && [ -z \"$ANTHROPIC_DEFAULT_SONNET_MODEL\" ] && [ -z \"$ANTHROPIC_DEFAULT_OPUS_MODEL\" ] && [ -z \"$CLAUDE_CODE_AUTO_COMPACT_WINDOW\" ] && [ -z \"$CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC\" ]; then printf true; else printf false; fi)\"; {original_script}"
            );
        }
    }
    spec
}

fn command(id: u64, command: ControlCommand) -> CommandEnvelope {
    CommandEnvelope {
        protocol_version: CONTROL_PROTOCOL_VERSION,
        id: CommandId(id),
        command,
    }
}

fn register_and_start(
    handle: &gate4agent_handle::Gate4AgentHandle,
    command_id: u64,
    instance_id: AgentInstanceId,
) {
    register_and_start_agent(handle, command_id, instance_id, fixture_agent_id());
}

fn register_and_start_agent(
    handle: &gate4agent_handle::Gate4AgentHandle,
    command_id: u64,
    instance_id: AgentInstanceId,
    agent_id: AgentId,
) {
    handle
        .dispatch(command(
            command_id,
            ControlCommand::Register {
                instance_id,
                agent_id,
                transport: TransportKind::Pty,
            },
        ))
        .unwrap();
    handle
        .dispatch(command(
            command_id + 1,
            ControlCommand::Start {
                instance_id,
                runtime_policy: ProviderRuntimePolicy::raw_pty(),
                request: StartRequest {
                    working_directory: std::env::current_dir()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    terminal_size: TerminalSize {
                        rows: 24,
                        columns: 100,
                    },
                    initial_prompt: None,
                    session_options: None,
                },
            },
        ))
        .unwrap();
}

async fn rejected_zai_glm_profile_message(
    instance_id: AgentInstanceId,
    token: Option<&'static str>,
    endpoint: Option<&'static str>,
) -> String {
    let registry = AgentRegistry::new([zai_glm_fixture_spec(false)]).expect("fixture registry");
    let (handle, mut runtime) = NativeRuntime::new(registry, NativeRuntimeConfig::default());
    runtime
        .upsert_native_launch_profile(
            ZAI_GLM_CLAUDE_PROFILE
                .instantiate(Arc::new(ZaiGlmFixtureResolver { token, endpoint }))
                .unwrap(),
        )
        .unwrap();
    runtime
        .select_native_launch_profile(
            instance_id,
            NativeLaunchProfileId::new(ZAI_GLM_CLAUDE_PROFILE_ID).unwrap(),
        )
        .unwrap();
    register_and_start_agent(&handle, 40, instance_id, AgentId::new("claude").unwrap());

    drive_until(&mut runtime, |_| {
        handle.snapshot().sessions.iter().any(|session| {
            session.instance_id == instance_id
                && matches!(session.status, SessionStatus::Failed { .. })
        })
    })
    .await;
    assert_eq!(runtime.active_native_sessions(), 0);
    let snapshot = handle.snapshot();
    let debug_snapshot = format!("{snapshot:?}");
    assert!(!debug_snapshot.contains(ZAI_TEST_TOKEN));
    snapshot
        .sessions
        .iter()
        .find(|session| session.instance_id == instance_id)
        .and_then(|session| match &session.status {
            SessionStatus::Failed { message } => Some(message.clone()),
            _ => None,
        })
        .expect("Z.AI profile contract failure")
}

async fn drive_until(
    runtime: &mut NativeRuntime,
    mut predicate: impl FnMut(&NativeRuntime) -> bool,
) {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            runtime.tick().await;
            if predicate(runtime) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("native launch profile fixture timeout");
}

#[test]
fn zai_glm_claude_descriptor_has_revisioned_exact_environment_contract() {
    assert_eq!(ZAI_GLM_CLAUDE_PROFILE.id(), ZAI_GLM_CLAUDE_PROFILE_ID);
    assert_eq!(
        ZAI_GLM_CLAUDE_PROFILE.revision(),
        ZAI_GLM_CLAUDE_PROFILE_REVISION
    );
    assert_eq!(ZAI_GLM_CLAUDE_PROFILE.agent_id(), "claude");
    assert_eq!(ZAI_GLM_CLAUDE_PROFILE.transport(), TransportKind::Pty);
    assert_eq!(
        ZAI_GLM_CLAUDE_PROFILE.required_env_keys(),
        ZAI_GLM_CLAUDE_REQUIRED_ENV_KEYS
    );
    assert_eq!(
        ZAI_GLM_CLAUDE_PROFILE.optional_env_keys(),
        ZAI_GLM_CLAUDE_OPTIONAL_ENV_KEYS
    );
    assert_eq!(
        ZAI_GLM_CLAUDE_PROFILE.owned_env_keys(),
        ZAI_GLM_CLAUDE_OWNED_ENV_KEYS
    );
    let described_keys = ZAI_GLM_CLAUDE_REQUIRED_ENV_KEYS
        .iter()
        .chain(ZAI_GLM_CLAUDE_OPTIONAL_ENV_KEYS)
        .copied()
        .collect::<Vec<_>>();
    assert_eq!(described_keys, ZAI_GLM_CLAUDE_OWNED_ENV_KEYS);
    assert!(!format!("{ZAI_GLM_CLAUDE_PROFILE:?}").contains(ZAI_TEST_TOKEN));
}

#[tokio::test]
async fn zai_glm_claude_profile_enforces_required_values_before_spawn() {
    let missing_token = rejected_zai_glm_profile_message(
        AgentInstanceId(8301),
        None,
        Some(ZAI_GLM_ANTHROPIC_BASE_URL),
    )
    .await;
    assert_eq!(
        missing_token,
        NativeLaunchProfileError::RequiredEnvironmentValueMissing.to_string()
    );

    let blank_token = rejected_zai_glm_profile_message(
        AgentInstanceId(8304),
        Some(" \t "),
        Some(ZAI_GLM_ANTHROPIC_BASE_URL),
    )
    .await;
    assert_eq!(
        blank_token,
        NativeLaunchProfileError::RequiredEnvironmentValueMissing.to_string()
    );

    let wrong_endpoint = rejected_zai_glm_profile_message(
        AgentInstanceId(8302),
        Some(ZAI_TEST_TOKEN),
        Some("https://invalid.example.test/anthropic"),
    )
    .await;
    assert_eq!(
        wrong_endpoint,
        NativeLaunchProfileError::FixedEnvironmentValueMismatch.to_string()
    );
    assert!(!wrong_endpoint.contains(ZAI_TEST_TOKEN));
}

#[tokio::test]
async fn zai_glm_claude_profile_reaches_a_controlled_real_pty_without_global_config() {
    let _environment = EnvironmentGuard::set(&[
        ("API_TIMEOUT_MS", "parent-timeout"),
        ("ANTHROPIC_DEFAULT_HAIKU_MODEL", "parent-haiku"),
        ("ANTHROPIC_DEFAULT_SONNET_MODEL", "parent-sonnet"),
        ("ANTHROPIC_DEFAULT_OPUS_MODEL", "parent-opus"),
        ("CLAUDE_CODE_AUTO_COMPACT_WINDOW", "parent-window"),
        (
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
            "parent-traffic",
        ),
    ]);
    let registry =
        AgentRegistry::new([zai_glm_fixture_spec(true)]).expect("fixture registry");
    let (handle, mut runtime) = NativeRuntime::new(
        registry,
        NativeRuntimeConfig {
            worker_poll_interval_ms: 5,
            ..NativeRuntimeConfig::default()
        },
    );
    runtime
        .upsert_native_launch_profile(
            ZAI_GLM_CLAUDE_PROFILE
                .instantiate(Arc::new(ZaiGlmFixtureResolver {
                    token: Some(ZAI_TEST_TOKEN),
                    endpoint: Some(ZAI_GLM_ANTHROPIC_BASE_URL),
                }))
                .unwrap(),
        )
        .unwrap();
    let instance_id = AgentInstanceId(8303);
    runtime
        .select_native_launch_profile(
            instance_id,
            NativeLaunchProfileId::new(ZAI_GLM_CLAUDE_PROFILE_ID).unwrap(),
        )
        .unwrap();
    register_and_start_agent(&handle, 50, instance_id, AgentId::new("claude").unwrap());

    #[cfg(windows)]
    let safe_output = format!(
        "zai-base={ZAI_GLM_ANTHROPIC_BASE_URL};token-present=True;optional-absent=True;"
    );
    #[cfg(not(windows))]
    let safe_output = format!(
        "zai-base={ZAI_GLM_ANTHROPIC_BASE_URL};token-present=true;optional-absent=true;"
    );
    drive_until(&mut runtime, |_| {
        handle.snapshot().sessions.iter().any(|session| {
            session.instance_id == instance_id
                && session
                    .terminal_frame
                    .as_ref()
                    .is_some_and(|frame| frame.contents.contains(&safe_output))
        })
    })
    .await;
    let snapshot = handle.snapshot();
    assert!(!format!("{snapshot:?}").contains(ZAI_TEST_TOKEN));

    handle
        .dispatch(command(
            52,
            ControlCommand::Stop {
                instance_id,
                force: true,
            },
        ))
        .unwrap();
    drive_until(&mut runtime, |_| {
        handle.snapshot().sessions.iter().any(|session| {
            session.instance_id == instance_id
                && matches!(session.status, SessionStatus::Exited { .. })
        })
    })
    .await;
    println!(
        "zai_profile_id={} revision={} token_present=true child_endpoint=true optional_env_removed=true",
        ZAI_GLM_CLAUDE_PROFILE_ID, ZAI_GLM_CLAUDE_PROFILE_REVISION
    );
}

#[test]
fn native_launch_profile_rejects_reserved_hook_environment_collision() {
    let error = NativeLaunchProfile::new(
        profile_id(),
        fixture_agent_id(),
        TransportKind::Pty,
        vec![OsString::from("gate4agent_hook_token")],
        Arc::new(EmptyResolver),
    )
    .err()
    .expect("reserved hook environment key must be rejected");

    assert_eq!(
        error,
        NativeLaunchProfileError::ReservedHookEnvironmentKey { index: 0 }
    );

    let empty_error = NativeLaunchProfile::new(
        profile_id(),
        fixture_agent_id(),
        TransportKind::Pty,
        Vec::new(),
        Arc::new(EmptyResolver),
    )
    .err()
    .expect("empty environment ownership must be rejected");
    assert_eq!(
        empty_error,
        NativeLaunchProfileError::EmptyEnvironmentOwnership
    );
}

#[test]
fn native_launch_profile_control_clear_and_remove_semantics_are_linearizable() {
    let registry = AgentRegistry::new([hook_posting_agent_spec()]).expect("fixture registry");
    let (_, mut runtime) = NativeRuntime::new(registry, NativeRuntimeConfig::default());
    runtime
        .upsert_native_launch_profile(
            NativeLaunchProfile::new(
                profile_id(),
                fixture_agent_id(),
                TransportKind::Pty,
                vec![OsString::from(PROFILE_SENTINEL)],
                Arc::new(EmptyResolver),
            )
            .unwrap(),
        )
        .unwrap();
    let instance_id = AgentInstanceId(8199);
    let control = runtime.native_launch_profile_control();
    control
        .select_native_launch_profile(instance_id, profile_id())
        .unwrap();

    assert_eq!(
        runtime.remove_native_launch_profile(&profile_id()),
        Err(NativeLaunchProfileError::ProfileInUse)
    );
    assert!(control.clear_native_launch_profile_selection(instance_id));
    assert!(!control.clear_native_launch_profile_selection(instance_id));
    assert_eq!(runtime.remove_native_launch_profile(&profile_id()), Ok(true));
}

#[tokio::test]
async fn native_launch_profile_revalidates_resolver_output_before_spawn() {
    let registry = AgentRegistry::new([hook_posting_agent_spec()]).expect("fixture registry");
    let (handle, mut runtime) = NativeRuntime::new(registry, NativeRuntimeConfig::default());
    runtime
        .upsert_native_launch_profile(
            NativeLaunchProfile::new(
                profile_id(),
                fixture_agent_id(),
                TransportKind::Pty,
                vec![OsString::from(PROFILE_SENTINEL)],
                Arc::new(ReservedOutputResolver),
            )
            .unwrap(),
        )
        .unwrap();
    let instance_id = AgentInstanceId(8200);
    runtime
        .select_native_launch_profile(instance_id, profile_id())
        .unwrap();
    register_and_start(&handle, 20, instance_id);

    drive_until(&mut runtime, |_| {
        handle.snapshot().sessions.iter().any(|session| {
            session.instance_id == instance_id
                && matches!(session.status, SessionStatus::Failed { .. })
        })
    })
    .await;
    let failure = handle
        .snapshot()
        .sessions
        .iter()
        .find(|session| session.instance_id == instance_id)
        .and_then(|session| match &session.status {
            SessionStatus::Failed { message } => Some(message.clone()),
            _ => None,
        })
        .expect("profile validation failure");
    assert!(!failure.contains("sentinel-not-a-token"));
    assert_eq!(runtime.active_native_sessions(), 0);
    assert_eq!(runtime.active_hook_routes(), 0);
}

#[tokio::test]
async fn selected_native_launch_profile_overlays_only_future_exact_pty_spawns() {
    let _environment = EnvironmentGuard::set(&[
        (PROFILE_SENTINEL, "parent-value"),
        (REMOVE_SENTINEL, "parent-remove-value"),
    ]);
    let mut spec = hook_posting_agent_spec();
    spec.capabilities.transports.pipe = pipe_agent_spec().capabilities.transports.pipe;
    let original_script = spec
        .launch
        .fixed_args
        .last_mut()
        .expect("fixture launch script");
    #[cfg(windows)]
    {
        *original_script = format!(
            "[Console]::Write('profile=' + $env:{PROFILE_SENTINEL} + ';removed=' + [string]::IsNullOrEmpty($env:{REMOVE_SENTINEL}) + ';child=' + $env:{CHILD_SENTINEL} + ';'); {original_script}"
        );
    }
    #[cfg(not(windows))]
    {
        *original_script = format!(
            "printf 'profile=%s;removed=%s;child=%s;' \"${PROFILE_SENTINEL}\" \"$(if [ -z \"${REMOVE_SENTINEL}\" ]; then printf true; else printf false; fi)\" \"${CHILD_SENTINEL}\"; {original_script}"
        );
    }

    let registry = AgentRegistry::new([spec]).expect("fixture registry");
    let (handle, mut runtime) = NativeRuntime::new(
        registry,
        NativeRuntimeConfig {
            worker_poll_interval_ms: 5,
            ..NativeRuntimeConfig::default()
        },
    );
    let resolver_generation = Arc::new(AtomicUsize::new(1));
    let resolver_calls = Arc::new(AtomicUsize::new(0));
    runtime
        .upsert_native_launch_profile(
            NativeLaunchProfile::new(
                profile_id(),
                fixture_agent_id(),
                TransportKind::Pty,
                vec![
                    OsString::from(PROFILE_SENTINEL),
                    OsString::from(REMOVE_SENTINEL),
                    OsString::from(CHILD_SENTINEL),
                ],
                Arc::new(SentinelResolver {
                    generation: Arc::clone(&resolver_generation),
                    calls: Arc::clone(&resolver_calls),
                }),
            )
            .unwrap(),
        )
        .unwrap();
    runtime
        .start_hook_ingress(HookIngressConfig::default())
        .await
        .unwrap();
    let inherited_instance = AgentInstanceId(8101);
    runtime
        .select_native_launch_profile(inherited_instance, profile_id())
        .unwrap();
    assert!(runtime.clear_native_launch_profile_selection(inherited_instance));
    register_and_start(&handle, 1, inherited_instance);
    #[cfg(windows)]
    let inherited_output = "profile=;removed=True;child=;";
    #[cfg(not(windows))]
    let inherited_output = "profile=;removed=true;child=;";
    drive_until(&mut runtime, |_| {
        let snapshot = handle.snapshot();
        snapshot.sessions.iter().any(|session| {
            session.instance_id == inherited_instance
                && session
                    .terminal_frame
                    .as_ref()
                    .is_some_and(|frame| {
                        frame.contents.contains(inherited_output)
                    })
        })
    })
    .await;

    runtime
        .select_native_launch_profile(
            inherited_instance,
            profile_id(),
        )
        .unwrap();
    assert_eq!(resolver_calls.load(Ordering::Acquire), 0);
    assert!(handle.snapshot().sessions.iter().any(|session| {
        session.instance_id == inherited_instance
                && session
                    .terminal_frame
                    .as_ref()
                    .is_some_and(|frame| frame.contents.contains(inherited_output))
    }));

    let profiled_instance = AgentInstanceId(8102);
    runtime
        .select_native_launch_profile(
            profiled_instance,
            profile_id(),
        )
        .unwrap();
    register_and_start(&handle, 3, profiled_instance);
    runtime.tick().await;
    assert_eq!(resolver_calls.load(Ordering::Acquire), 1);
    resolver_generation.store(2, Ordering::Release);
    #[cfg(windows)]
    let first_profile_output = "profile=profile-value-1;removed=True;child=child-only;";
    #[cfg(not(windows))]
    let first_profile_output = "profile=profile-value-1;removed=true;child=child-only;";
    drive_until(&mut runtime, |_| {
        handle.snapshot().sessions.iter().any(|session| {
            session.instance_id == profiled_instance
                && session
                    .terminal_frame
                    .as_ref()
                    .is_some_and(|frame| {
                        frame.contents.contains(first_profile_output)
                    })
                && session.provider.current_prompt.is_none()
        })
    })
    .await;

    let future_instance = AgentInstanceId(8103);
    runtime
        .select_native_launch_profile(future_instance, profile_id())
        .unwrap();
    register_and_start(&handle, 5, future_instance);
    #[cfg(windows)]
    let future_profile_output = "profile=profile-value-2;removed=True;child=child-only;";
    #[cfg(not(windows))]
    let future_profile_output = "profile=profile-value-2;removed=true;child=child-only;";
    drive_until(&mut runtime, |_| {
        handle.snapshot().sessions.iter().any(|session| {
            session.instance_id == future_instance
                && session
                    .terminal_frame
                    .as_ref()
                    .is_some_and(|frame| frame.contents.contains(future_profile_output))
                && session.provider.current_prompt.is_none()
        })
    })
    .await;
    assert_eq!(resolver_calls.load(Ordering::Acquire), 2);
    assert_eq!(runtime.active_hook_routes(), 0);

    let mismatched_instance = AgentInstanceId(8104);
    runtime
        .select_native_launch_profile(mismatched_instance, profile_id())
        .unwrap();
    handle
        .dispatch(command(
            7,
            ControlCommand::Register {
                instance_id: mismatched_instance,
                agent_id: fixture_agent_id(),
                transport: TransportKind::Pipe,
            },
        ))
        .unwrap();
    handle
        .dispatch(command(
            8,
            ControlCommand::Start {
                instance_id: mismatched_instance,
                runtime_policy: ProviderRuntimePolicy::new(true, true, true, true, true).unwrap(),
                request: StartRequest {
                    working_directory: std::env::current_dir()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    terminal_size: TerminalSize {
                        rows: 24,
                        columns: 100,
                    },
                    initial_prompt: Some("fixture prompt".to_owned()),
                    session_options: None,
                },
            },
        ))
        .unwrap();
    drive_until(&mut runtime, |_| {
        handle.snapshot().sessions.iter().any(|session| {
            session.instance_id == mismatched_instance
                && matches!(session.status, SessionStatus::Failed { .. })
        })
    })
    .await;
    assert_eq!(resolver_calls.load(Ordering::Acquire), 2);

    for (command_id, instance_id) in [
        (9, inherited_instance),
        (10, profiled_instance),
        (11, future_instance),
    ] {
        handle
            .dispatch(command(
                command_id,
                ControlCommand::Stop {
                    instance_id,
                    force: true,
                },
            ))
            .unwrap();
    }
    drive_until(&mut runtime, |_| {
        handle.snapshot().sessions.iter().all(|session| {
            matches!(session.status, SessionStatus::Exited { .. })
                || (session.instance_id == mismatched_instance
                    && matches!(session.status, SessionStatus::Failed { .. }))
        })
    })
    .await;
    runtime.stop_hook_ingress().await;
    println!(
        "profile_resolutions={} default_isolated=true child_overlay=true raw_hook_route_suppressed=true transport_mismatch_failed=true",
        resolver_calls.load(Ordering::Acquire)
    );
}

#[tokio::test]
async fn native_launch_profile_control_selects_before_spawn_for_exact_pty() {
    let mut spec = interactive_agent_spec();
    let original_script = spec
        .launch
        .fixed_args
        .last_mut()
        .expect("interactive fixture script");
    #[cfg(windows)]
    {
        *original_script = format!(
            "[Console]::Write('profile=' + $env:{PROFILE_SENTINEL} + ';'); {original_script}"
        );
    }
    #[cfg(not(windows))]
    {
        *original_script =
            format!("printf 'profile=%s;' \"${PROFILE_SENTINEL}\"; {original_script}");
    }

    let (handle, mut runtime) = NativeRuntime::new(
        AgentRegistry::new([spec]).expect("fixture registry"),
        NativeRuntimeConfig {
            worker_poll_interval_ms: 5,
            ..NativeRuntimeConfig::default()
        },
    );
    let resolver_calls = Arc::new(AtomicUsize::new(0));
    runtime
        .upsert_native_launch_profile(
            NativeLaunchProfile::new(
                profile_id(),
                AgentId::new(CONTROL_FIXTURE_ID).unwrap(),
                TransportKind::Pty,
                vec![
                    OsString::from(PROFILE_SENTINEL),
                    OsString::from(REMOVE_SENTINEL),
                    OsString::from(CHILD_SENTINEL),
                ],
                Arc::new(SentinelResolver {
                    generation: Arc::new(AtomicUsize::new(1)),
                    calls: Arc::clone(&resolver_calls),
                }),
            )
            .unwrap(),
        )
        .unwrap();
    let control = runtime.native_launch_profile_control();
    let instance_id = AgentInstanceId(8400);
    control
        .select_native_launch_profile(instance_id, profile_id())
        .unwrap();
    register_and_start_agent(
        &handle,
        55,
        instance_id,
        AgentId::new(CONTROL_FIXTURE_ID).unwrap(),
    );

    drive_until(&mut runtime, |_| {
        handle.snapshot().sessions.iter().any(|session| {
            session.instance_id == instance_id
                && session.terminal_frame.as_ref().is_some_and(|frame| {
                    frame.contents.contains("profile=profile-value-1;")
                })
        })
    })
    .await;
    assert_eq!(resolver_calls.load(Ordering::Acquire), 1);

    handle
        .dispatch(command(
            57,
            ControlCommand::Stop {
                instance_id,
                force: true,
            },
        ))
        .unwrap();
    drive_until(&mut runtime, |_| {
        handle.snapshot().sessions.iter().any(|session| {
            session.instance_id == instance_id
                && matches!(session.status, SessionStatus::Exited { .. })
        })
    })
    .await;
}

#[tokio::test]
async fn native_launch_profile_control_selects_before_spawn_for_exact_one_shot_pipe() {
    let mut spec = one_shot_agent_spec();
    let launch = spec
        .capabilities
        .transports
        .pipe
        .as_mut()
        .and_then(|pipe| pipe.launch_override.as_mut())
        .expect("one-shot fixture launch override");
    let original_script = launch.fixed_args.last_mut().expect("one-shot fixture script");
    #[cfg(windows)]
    {
        *original_script = format!(
            "[Console]::Write('profile=' + $env:{PROFILE_SENTINEL} + ';'); {original_script}"
        );
    }
    #[cfg(not(windows))]
    {
        *original_script =
            format!("printf 'profile=%s;' \"${PROFILE_SENTINEL}\"; {original_script}");
    }

    let (handle, mut runtime) = NativeRuntime::new(
        AgentRegistry::new([spec]).expect("fixture registry"),
        NativeRuntimeConfig {
            worker_poll_interval_ms: 5,
            ..NativeRuntimeConfig::default()
        },
    );
    let resolver_calls = Arc::new(AtomicUsize::new(0));
    runtime
        .upsert_native_launch_profile(
            NativeLaunchProfile::new(
                profile_id(),
                AgentId::new(ONE_SHOT_FIXTURE_ID).unwrap(),
                TransportKind::Pipe,
                vec![
                    OsString::from(PROFILE_SENTINEL),
                    OsString::from(REMOVE_SENTINEL),
                    OsString::from(CHILD_SENTINEL),
                ],
                Arc::new(SentinelResolver {
                    generation: Arc::new(AtomicUsize::new(1)),
                    calls: Arc::clone(&resolver_calls),
                }),
            )
            .unwrap(),
        )
        .unwrap();
    let control = runtime.native_launch_profile_control();
    let instance_id = AgentInstanceId(8401);
    control
        .select_native_launch_profile(instance_id, profile_id())
        .unwrap();
    let subscription = handle.subscribe(32);
    handle
        .dispatch(command(
            60,
            ControlCommand::Register {
                instance_id,
                agent_id: AgentId::new(ONE_SHOT_FIXTURE_ID).unwrap(),
                transport: TransportKind::Pipe,
            },
        ))
        .unwrap();
    handle
        .dispatch(command(
            61,
            ControlCommand::Start {
                instance_id,
                runtime_policy: ProviderRuntimePolicy::new(true, true, true, true, true).unwrap(),
                request: StartRequest {
                    working_directory: std::env::current_dir()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    terminal_size: TerminalSize {
                        rows: 24,
                        columns: 80,
                    },
                    initial_prompt: Some("fixture prompt".to_owned()),
                    session_options: None,
                },
            },
        ))
        .unwrap();

    let mut events = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            runtime.tick().await;
            while let Ok(event) = subscription.try_recv() {
                events.push(event);
            }
            if handle.snapshot().sessions.iter().any(|session| {
                session.instance_id == instance_id
                    && matches!(session.status, SessionStatus::Exited { .. })
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("profiled one-shot Pipe fixture timeout");

    assert_eq!(resolver_calls.load(Ordering::Acquire), 1);
    assert!(events.iter().any(|event| matches!(
        &event.event,
        ControlEventKind::ProviderEvent {
            source: ProviderSource {
                family: AdapterFamily::OneShot,
                ..
            },
            event: ProviderEvent::Text { text, .. },
            ..
        } if text == "profile=profile-value-1;fixture-one-shot:fixture prompt"
    )));
}

#[tokio::test]
async fn selected_native_launch_profile_mismatch_fails_closed_before_resolver_or_child() {
    let (handle, mut runtime) = NativeRuntime::new(
        AgentRegistry::new([pipe_agent_spec()]).expect("fixture registry"),
        NativeRuntimeConfig::default(),
    );
    let resolver_calls = Arc::new(AtomicUsize::new(0));
    runtime
        .upsert_native_launch_profile(
            NativeLaunchProfile::new(
                profile_id(),
                AgentId::new(PIPE_FIXTURE_ID).unwrap(),
                TransportKind::Pipe,
                vec![
                    OsString::from(PROFILE_SENTINEL),
                    OsString::from(REMOVE_SENTINEL),
                    OsString::from(CHILD_SENTINEL),
                ],
                Arc::new(SentinelResolver {
                    generation: Arc::new(AtomicUsize::new(1)),
                    calls: Arc::clone(&resolver_calls),
                }),
            )
            .unwrap(),
        )
        .unwrap();
    let control = runtime.native_launch_profile_control();
    let instance_id = AgentInstanceId(8402);
    control
        .select_native_launch_profile(instance_id, profile_id())
        .unwrap();
    handle
        .dispatch(command(
            70,
            ControlCommand::Register {
                instance_id,
                agent_id: AgentId::new(PIPE_FIXTURE_ID).unwrap(),
                transport: TransportKind::Pipe,
            },
        ))
        .unwrap();
    handle
        .dispatch(command(
            71,
            ControlCommand::Start {
                instance_id,
                runtime_policy: ProviderRuntimePolicy::new(true, true, true, true, true).unwrap(),
                request: StartRequest {
                    working_directory: std::env::current_dir()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    terminal_size: TerminalSize {
                        rows: 24,
                        columns: 80,
                    },
                    initial_prompt: Some("must not spawn".to_owned()),
                    session_options: None,
                },
            },
        ))
        .unwrap();

    drive_until(&mut runtime, |_| {
        handle.snapshot().sessions.iter().any(|session| {
            session.instance_id == instance_id
                && matches!(session.status, SessionStatus::Failed { .. })
        })
    })
    .await;
    let failure = handle
        .snapshot()
        .sessions
        .iter()
        .find(|session| session.instance_id == instance_id)
        .and_then(|session| match &session.status {
            SessionStatus::Failed { message } => Some(message.clone()),
            _ => None,
        })
        .expect("structured Pipe must fail before spawn");
    assert_eq!(failure, NativeLaunchProfileError::UnsupportedTransport.to_string());
    assert_eq!(resolver_calls.load(Ordering::Acquire), 0);
    assert_eq!(runtime.active_native_sessions(), 0);
}

#[tokio::test]
async fn native_launch_profile_debug_surfaces_never_expose_resolver_values() {
    const SECRET_VALUE: &str = "resolver-secret-must-never-reach-debug";

    trait AmbiguousIfDebug<A> {
        fn marker() {}
    }

    impl<T: ?Sized> AmbiguousIfDebug<()> for T {}
    impl<T: ?Sized + std::fmt::Debug> AmbiguousIfDebug<u8> for T {}

    let _ = <NativeLaunchProfile as AmbiguousIfDebug<_>>::marker;
    let _ = <NativeLaunchProfileControl as AmbiguousIfDebug<_>>::marker;

    struct SecretDenyingResolver;

    impl NativeChildEnvironmentResolver for SecretDenyingResolver {
        fn resolve_child_environment(
            &self,
        ) -> Result<Vec<EnvMutation>, NativeChildEnvironmentResolveError> {
            assert!(!SECRET_VALUE.is_empty());
            Err(NativeChildEnvironmentResolveError::Denied)
        }
    }

    let (handle, mut runtime) = NativeRuntime::new(
        AgentRegistry::new([hook_posting_agent_spec()]).expect("fixture registry"),
        NativeRuntimeConfig::default(),
    );
    runtime
        .upsert_native_launch_profile(
            NativeLaunchProfile::new(
                profile_id(),
                fixture_agent_id(),
                TransportKind::Pty,
                vec![OsString::from(PROFILE_SENTINEL)],
                Arc::new(SecretDenyingResolver),
            )
            .unwrap(),
        )
        .unwrap();
    let instance_id = AgentInstanceId(8403);
    runtime
        .native_launch_profile_control()
        .select_native_launch_profile(instance_id, profile_id())
        .unwrap();
    register_and_start(&handle, 80, instance_id);
    drive_until(&mut runtime, |_| {
        handle.snapshot().sessions.iter().any(|session| {
            session.instance_id == instance_id
                && matches!(session.status, SessionStatus::Failed { .. })
        })
    })
    .await;

    let debug_snapshot = format!("{:?}", handle.snapshot());
    let debug_error = format!(
        "{:?}",
        NativeLaunchProfileError::Resolve(NativeChildEnvironmentResolveError::Denied)
    );
    assert!(!debug_snapshot.contains(SECRET_VALUE));
    assert!(!debug_error.contains(SECRET_VALUE));
}

#[tokio::test]
async fn native_launch_environment_overlay_reaches_exact_pty_child_only() {
    let mut spec = interactive_agent_spec();
    let original_script = spec
        .launch
        .fixed_args
        .last_mut()
        .expect("interactive fixture script");
    #[cfg(windows)]
    {
        *original_script = format!(
            "[Console]::Write('profile=' + $env:{PROFILE_SENTINEL} + ';overlay=' + $env:{OVERLAY_SENTINEL} + ';'); {original_script}"
        );
    }
    #[cfg(not(windows))]
    {
        *original_script = format!(
            "printf 'profile=%s;overlay=%s;' \"${PROFILE_SENTINEL}\" \"${OVERLAY_SENTINEL}\"; {original_script}"
        );
    }

    let (handle, mut runtime) = NativeRuntime::new(
        AgentRegistry::new([spec]).expect("fixture registry"),
        NativeRuntimeConfig {
            worker_poll_interval_ms: 5,
            ..NativeRuntimeConfig::default()
        },
    );
    let resolver_calls = Arc::new(AtomicUsize::new(0));
    runtime
        .upsert_native_launch_profile(
            NativeLaunchProfile::new(
                profile_id(),
                AgentId::new(CONTROL_FIXTURE_ID).unwrap(),
                TransportKind::Pty,
                vec![
                    OsString::from(PROFILE_SENTINEL),
                    OsString::from(REMOVE_SENTINEL),
                    OsString::from(CHILD_SENTINEL),
                ],
                Arc::new(SentinelResolver {
                    generation: Arc::new(AtomicUsize::new(1)),
                    calls: Arc::clone(&resolver_calls),
                }),
            )
            .unwrap(),
        )
        .unwrap();
    let control = runtime.native_launch_profile_control();
    let selected_instance = AgentInstanceId(8500);
    let default_instance = AgentInstanceId(8501);
    control
        .select_native_launch_profile(selected_instance, profile_id())
        .unwrap();
    let parent_value = std::env::var_os(OVERLAY_SENTINEL);
    control
        .install_native_launch_environment_overlay(
            selected_instance,
            NativeLaunchEnvironmentOverlay::new(
                AgentId::new(CONTROL_FIXTURE_ID).unwrap(),
                TransportKind::Pty,
                vec![EnvMutation {
                    key: OsString::from(OVERLAY_SENTINEL),
                    value: Some(OsString::from("overlay-child-only")),
                }],
            )
            .unwrap(),
        )
        .unwrap();
    assert_eq!(std::env::var_os(OVERLAY_SENTINEL), parent_value);

    register_and_start_agent(
        &handle,
        90,
        selected_instance,
        AgentId::new(CONTROL_FIXTURE_ID).unwrap(),
    );
    register_and_start_agent(
        &handle,
        92,
        default_instance,
        AgentId::new(CONTROL_FIXTURE_ID).unwrap(),
    );
    drive_until(&mut runtime, |_| {
        let snapshot = handle.snapshot();
        let selected_ready = snapshot.sessions.iter().any(|session| {
            session.instance_id == selected_instance
                && session.terminal_frame.as_ref().is_some_and(|frame| {
                    frame
                        .contents
                        .contains("profile=profile-value-1;overlay=overlay-child-only;")
                })
        });
        let default_ready = snapshot.sessions.iter().any(|session| {
            session.instance_id == default_instance
                && session.terminal_frame.as_ref().is_some_and(|frame| {
                    frame.contents.contains("profile=;overlay=;")
                })
        });
        selected_ready && default_ready
    })
    .await;
    assert_eq!(resolver_calls.load(Ordering::Acquire), 1);
    assert_eq!(std::env::var_os(OVERLAY_SENTINEL), parent_value);

    for (command_id, instance_id) in [(94, selected_instance), (95, default_instance)] {
        handle
            .dispatch(command(
                command_id,
                ControlCommand::Stop {
                    instance_id,
                    force: true,
                },
            ))
            .unwrap();
    }
    drive_until(&mut runtime, |_| {
        handle.snapshot().sessions.iter().all(|session| {
            matches!(session.status, SessionStatus::Exited { .. })
        })
    })
    .await;
}

#[test]
fn native_launch_environment_overlay_rejects_invalid_bindings_before_resolver_or_child() {
    let (handle, mut runtime) = NativeRuntime::new(
        AgentRegistry::new([interactive_agent_spec()]).expect("fixture registry"),
        NativeRuntimeConfig::default(),
    );
    let resolver_calls = Arc::new(AtomicUsize::new(0));
    runtime
        .upsert_native_launch_profile(
            NativeLaunchProfile::new(
                profile_id(),
                AgentId::new(CONTROL_FIXTURE_ID).unwrap(),
                TransportKind::Pty,
                vec![
                    OsString::from(PROFILE_SENTINEL),
                    OsString::from(REMOVE_SENTINEL),
                    OsString::from(CHILD_SENTINEL),
                ],
                Arc::new(SentinelResolver {
                    generation: Arc::new(AtomicUsize::new(1)),
                    calls: Arc::clone(&resolver_calls),
                }),
            )
            .unwrap(),
        )
        .unwrap();
    let control = runtime.native_launch_profile_control();
    let instance_id = AgentInstanceId(8502);
    let overlay = |agent_id: &str, key: &str| {
        NativeLaunchEnvironmentOverlay::new(
            AgentId::new(agent_id).unwrap(),
            TransportKind::Pty,
            vec![EnvMutation {
                key: OsString::from(key),
                value: Some(OsString::from("must-not-resolve")),
            }],
        )
        .unwrap()
    };

    assert_eq!(
        control
            .install_native_launch_environment_overlay(
                instance_id,
                NativeLaunchEnvironmentOverlay::new(
                    AgentId::new(CONTROL_FIXTURE_ID).unwrap(),
                    TransportKind::Pty,
                    Vec::new(),
                )
                .unwrap(),
            )
            .unwrap_err(),
        NativeLaunchProfileError::EnvironmentOverlaySelectionMissing
    );
    assert_eq!(
        control
            .install_native_launch_environment_overlay(
                instance_id,
                overlay(CONTROL_FIXTURE_ID, OVERLAY_SENTINEL),
            )
            .unwrap_err(),
        NativeLaunchProfileError::EnvironmentOverlaySelectionMissing
    );
    control
        .select_native_launch_profile(instance_id, profile_id())
        .unwrap();
    assert_eq!(
        control
            .install_native_launch_environment_overlay(
                instance_id,
                overlay(HOOK_POSTING_FIXTURE_ID, OVERLAY_SENTINEL),
            )
            .unwrap_err(),
        NativeLaunchProfileError::EnvironmentOverlayBindingMismatch
    );
    assert_eq!(
        control
            .install_native_launch_environment_overlay(
                instance_id,
                overlay(CONTROL_FIXTURE_ID, PROFILE_SENTINEL),
            )
            .unwrap_err(),
        NativeLaunchProfileError::EnvironmentOverlayKeyConflict
    );
    control
        .install_native_launch_environment_overlay(
            instance_id,
            overlay(CONTROL_FIXTURE_ID, OVERLAY_SENTINEL),
        )
        .unwrap();

    let conflicting_profile_id = NativeLaunchProfileId::new("overlay-conflict").unwrap();
    runtime
        .upsert_native_launch_profile(
            NativeLaunchProfile::new(
                conflicting_profile_id.clone(),
                AgentId::new(CONTROL_FIXTURE_ID).unwrap(),
                TransportKind::Pty,
                vec![OsString::from(OVERLAY_SENTINEL)],
                Arc::new(EmptyResolver),
            )
            .unwrap(),
        )
        .unwrap();
    assert_eq!(
        control
            .select_native_launch_profile(instance_id, conflicting_profile_id)
            .unwrap_err(),
        NativeLaunchProfileError::EnvironmentOverlayKeyConflict
    );
    assert_eq!(
        runtime
            .upsert_native_launch_profile(
                NativeLaunchProfile::new(
                    profile_id(),
                    AgentId::new(CONTROL_FIXTURE_ID).unwrap(),
                    TransportKind::Pty,
                    vec![OsString::from(OVERLAY_SENTINEL)],
                    Arc::new(EmptyResolver),
                )
                .unwrap(),
            )
            .unwrap_err(),
        NativeLaunchProfileError::EnvironmentOverlayKeyConflict
    );
    assert_eq!(resolver_calls.load(Ordering::Acquire), 0);
    assert_eq!(runtime.active_native_sessions(), 0);
    assert!(handle.snapshot().sessions.is_empty());
}

#[tokio::test]
async fn clearing_native_launch_profile_selection_discards_environment_overlay() {
    let mut spec = interactive_agent_spec();
    let original_script = spec
        .launch
        .fixed_args
        .last_mut()
        .expect("interactive fixture script");
    #[cfg(windows)]
    {
        *original_script = format!(
            "[Console]::Write('profile=' + $env:{PROFILE_SENTINEL} + ';overlay=' + $env:{OVERLAY_SENTINEL} + ';argv=' + ($args -join '|') + ';'); {original_script}"
        );
    }
    #[cfg(not(windows))]
    {
        *original_script = format!(
            "printf 'profile=%s;overlay=%s;argv=%s;' \"${PROFILE_SENTINEL}\" \"${OVERLAY_SENTINEL}\" \"$0\"; {original_script}"
        );
    }

    let (handle, mut runtime) = NativeRuntime::new(
        AgentRegistry::new([spec]).expect("fixture registry"),
        NativeRuntimeConfig {
            worker_poll_interval_ms: 5,
            ..NativeRuntimeConfig::default()
        },
    );
    let resolver_calls = Arc::new(AtomicUsize::new(0));
    runtime
        .upsert_native_launch_profile(
            NativeLaunchProfile::new(
                profile_id(),
                AgentId::new(CONTROL_FIXTURE_ID).unwrap(),
                TransportKind::Pty,
                vec![
                    OsString::from(PROFILE_SENTINEL),
                    OsString::from(REMOVE_SENTINEL),
                    OsString::from(CHILD_SENTINEL),
                ],
                Arc::new(SentinelResolver {
                    generation: Arc::new(AtomicUsize::new(2)),
                    calls: Arc::clone(&resolver_calls),
                }),
            )
            .unwrap(),
        )
        .unwrap();
    let control = runtime.native_launch_profile_control();
    let instance_id = AgentInstanceId(8503);
    control
        .select_native_launch_profile(instance_id, profile_id())
        .unwrap();
    control
        .install_native_instance_launch_overlay(
            instance_id,
            NativeInstanceLaunchOverlay::new(
                AgentId::new(CONTROL_FIXTURE_ID).unwrap(),
                TransportKind::Pty,
                vec![EnvMutation {
                    key: OsString::from(OVERLAY_SENTINEL),
                    value: Some(OsString::from("must-be-cleared")),
                }],
                vec![OsString::from("must-be-cleared-argv")],
            )
            .unwrap(),
        )
        .unwrap();
    assert!(control.clear_native_launch_profile_selection(instance_id));
    control
        .select_native_launch_profile(instance_id, profile_id())
        .unwrap();

    register_and_start_agent(
        &handle,
        100,
        instance_id,
        AgentId::new(CONTROL_FIXTURE_ID).unwrap(),
    );
    drive_until(&mut runtime, |_| {
        handle.snapshot().sessions.iter().any(|session| {
            session.instance_id == instance_id
                && session.terminal_frame.as_ref().is_some_and(|frame| {
                    frame
                        .contents
                        .contains("profile=profile-value-2;overlay=;")
                        && !frame.contents.contains("must-be-cleared-argv")
                })
        })
    })
    .await;
    assert_eq!(resolver_calls.load(Ordering::Acquire), 1);

    handle
        .dispatch(command(
            102,
            ControlCommand::Stop {
                instance_id,
                force: true,
            },
        ))
        .unwrap();
    drive_until(&mut runtime, |_| {
        handle.snapshot().sessions.iter().any(|session| {
            session.instance_id == instance_id
                && matches!(session.status, SessionStatus::Exited { .. })
        })
    })
    .await;
}

#[test]
fn native_launch_environment_overlay_debug_surfaces_never_expose_values() {
    const SECRET_VALUE: &str = "overlay-secret-must-never-reach-debug";

    trait AmbiguousIfDebug<A> {
        fn marker() {}
    }

    impl<T: ?Sized> AmbiguousIfDebug<()> for T {}
    impl<T: ?Sized + std::fmt::Debug> AmbiguousIfDebug<u8> for T {}

    let _ = <NativeLaunchEnvironmentOverlay as AmbiguousIfDebug<_>>::marker;
    let overlay = NativeLaunchEnvironmentOverlay::new(
        AgentId::new(CONTROL_FIXTURE_ID).unwrap(),
        TransportKind::Pty,
        vec![EnvMutation {
            key: OsString::from(OVERLAY_SENTINEL),
            value: Some(OsString::from(SECRET_VALUE)),
        }],
    )
    .unwrap();
    let (_handle, runtime) = NativeRuntime::new(
        AgentRegistry::new([interactive_agent_spec()]).expect("fixture registry"),
        NativeRuntimeConfig::default(),
    );
    let error = runtime
        .native_launch_profile_control()
        .install_native_launch_environment_overlay(AgentInstanceId(8504), overlay)
        .unwrap_err();
    let debug_error = format!("{error:?}");
    let debug_mutation = format!(
        "{:?}",
        EnvMutation {
            key: OsString::from(OVERLAY_SENTINEL),
            value: Some(OsString::from(SECRET_VALUE)),
        }
    );
    assert!(!debug_error.contains(SECRET_VALUE));
    assert!(!debug_mutation.contains(SECRET_VALUE));
}

#[tokio::test]
async fn bundle_only_native_instance_launch_overlay_argv_reaches_exact_pty_child_only() {
    let mut spec = interactive_agent_spec();
    let original_script = spec
        .launch
        .fixed_args
        .last()
        .expect("interactive fixture script")
        .clone();
    #[cfg(windows)]
    let _script_guard = {
        struct ScriptGuard(std::path::PathBuf);

        impl Drop for ScriptGuard {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }

        let script = format!(
            "[Console]::Write('argv=' + ($args -join '|') + ';'); {original_script}"
        );
        let directory = std::env::current_dir()
            .expect("fixture working directory")
            .join("target")
            .join("native-launch-overlay-tests");
        std::fs::create_dir_all(&directory).expect("create argv fixture directory");
        let path = directory.join(format!(
            "argv-{}-8505.ps1",
            std::process::id()
        ));
        std::fs::write(&path, script).expect("write argv fixture script");
        spec.launch.fixed_args = vec![
            "-NoLogo".to_owned(),
            "-NoProfile".to_owned(),
            "-NonInteractive".to_owned(),
            "-ExecutionPolicy".to_owned(),
            "Bypass".to_owned(),
            "-File".to_owned(),
            path.to_string_lossy().into_owned(),
        ];
        ScriptGuard(path)
    };
    #[cfg(not(windows))]
    {
        let original_script = spec
            .launch
            .fixed_args
            .last_mut()
            .expect("interactive fixture script");
        *original_script = format!(
            "printf 'argv=%s|%s;' \"$0\" \"$1\"; {original_script}"
        );
    }

    let (handle, mut runtime) = NativeRuntime::new(
        AgentRegistry::new([spec]).expect("fixture registry"),
        NativeRuntimeConfig {
            worker_poll_interval_ms: 5,
            ..NativeRuntimeConfig::default()
        },
    );
    let control = runtime.native_launch_profile_control();
    let selected_instance = AgentInstanceId(8505);
    let default_instance = AgentInstanceId(8506);
    control
        .install_native_instance_launch_overlay(
            selected_instance,
            NativeInstanceLaunchOverlay::new(
                AgentId::new(CONTROL_FIXTURE_ID).unwrap(),
                TransportKind::Pty,
                Vec::new(),
                vec![OsString::from("stale-bundle-argv")],
            )
            .unwrap(),
        )
        .unwrap();
    assert!(control.clear_native_instance_launch_overlay(selected_instance));
    assert!(!control.clear_native_instance_launch_overlay(selected_instance));
    control
        .install_native_instance_launch_overlay(
            selected_instance,
            NativeInstanceLaunchOverlay::new(
                AgentId::new(CONTROL_FIXTURE_ID).unwrap(),
                TransportKind::Pty,
                Vec::new(),
                vec![OsString::from("cleanup-only-bundle-argv")],
            )
            .unwrap(),
        )
        .unwrap();
    assert!(control.clear_native_launch_profile_selection(selected_instance));
    assert!(!control.clear_native_launch_profile_selection(selected_instance));
    control
        .install_native_instance_launch_overlay(
            selected_instance,
            NativeInstanceLaunchOverlay::new(
                AgentId::new(CONTROL_FIXTURE_ID).unwrap(),
                TransportKind::Pty,
                Vec::new(),
                vec![
                    OsString::from("--bundle-root"),
                    OsString::from("exact-private-bundle-path"),
                ],
            )
            .unwrap(),
        )
        .unwrap();

    register_and_start_agent(
        &handle,
        110,
        selected_instance,
        AgentId::new(CONTROL_FIXTURE_ID).unwrap(),
    );
    register_and_start_agent(
        &handle,
        112,
        default_instance,
        AgentId::new(CONTROL_FIXTURE_ID).unwrap(),
    );
    drive_until(&mut runtime, |_| {
        let snapshot = handle.snapshot();
        let selected_ready = snapshot.sessions.iter().any(|session| {
            session.instance_id == selected_instance
                && session.terminal_frame.as_ref().is_some_and(|frame| {
                    frame
                        .contents
                        .contains("argv=--bundle-root|exact-private-bundle-path;")
                        && !frame.contents.contains("stale-bundle-argv")
                        && !frame.contents.contains("cleanup-only-bundle-argv")
                })
        });
        let default_ready = snapshot.sessions.iter().any(|session| {
            session.instance_id == default_instance
                && session.terminal_frame.as_ref().is_some_and(|frame| {
                    frame.contents.contains("fixture-ready>")
                        && !frame.contents.contains("exact-private-bundle-path")
                })
        });
        selected_ready && default_ready
    })
    .await;

    for (command_id, instance_id) in [(114, selected_instance), (115, default_instance)] {
        handle
            .dispatch(command(
                command_id,
                ControlCommand::Stop {
                    instance_id,
                    force: true,
                },
            ))
            .unwrap();
    }
    drive_until(&mut runtime, |_| {
        handle.snapshot().sessions.iter().all(|session| {
            matches!(session.status, SessionStatus::Exited { .. })
        })
    })
    .await;
}

#[test]
fn native_instance_launch_overlay_rejects_bounds_binding_and_reserved_argv_before_resolver_or_child(
) {
    assert_eq!(
        NativeInstanceLaunchOverlay::new(
            AgentId::new(CONTROL_FIXTURE_ID).unwrap(),
            TransportKind::Pipe,
            Vec::new(),
            vec![OsString::from("--bundle-mode")],
        )
        .err()
        .expect("Pipe argv overlay must fail"),
        NativeLaunchProfileError::InstanceOverlayUnsupportedTransport
    );
    assert_eq!(
        NativeInstanceLaunchOverlay::new(
            AgentId::new(CONTROL_FIXTURE_ID).unwrap(),
            TransportKind::Pty,
            Vec::new(),
            vec![OsString::from("x"); 129],
        )
        .err()
        .expect("unbounded argv overlay must fail"),
        NativeLaunchProfileError::TooManyLaunchArguments {
            count: 129,
            max: 128,
        }
    );
    for argument in [
        "--session-id",
        "--resume",
        "--resume=session-secret",
        "--continue",
        "--print",
        "--prompt",
        "--prompt-interactive",
        "-c",
        "-p",
        "-r",
    ] {
        assert_eq!(
            NativeInstanceLaunchOverlay::new(
                AgentId::new("claude").unwrap(),
                TransportKind::Pty,
                Vec::new(),
                vec![OsString::from(argument)],
            )
            .err()
            .expect("reserved Claude argv overlay must fail"),
            NativeLaunchProfileError::ReservedClaudeLaunchArgument { index: 0 },
            "reserved Claude argument {argument} must fail closed"
        );
    }

    struct CountingEmptyResolver(Arc<AtomicUsize>);

    impl NativeChildEnvironmentResolver for CountingEmptyResolver {
        fn resolve_child_environment(
            &self,
        ) -> Result<Vec<EnvMutation>, NativeChildEnvironmentResolveError> {
            self.0.fetch_add(1, Ordering::AcqRel);
            Ok(Vec::new())
        }
    }

    let (handle, mut runtime) = NativeRuntime::new(
        AgentRegistry::new([interactive_agent_spec()]).expect("fixture registry"),
        NativeRuntimeConfig::default(),
    );
    let resolver_calls = Arc::new(AtomicUsize::new(0));
    runtime
        .upsert_native_launch_profile(
            NativeLaunchProfile::new(
                profile_id(),
                AgentId::new(CONTROL_FIXTURE_ID).unwrap(),
                TransportKind::Pty,
                vec![OsString::from(PROFILE_SENTINEL)],
                Arc::new(CountingEmptyResolver(Arc::clone(&resolver_calls))),
            )
            .unwrap(),
        )
        .unwrap();
    let control = runtime.native_launch_profile_control();
    let instance_id = AgentInstanceId(8508);
    control
        .select_native_launch_profile(instance_id, profile_id())
        .unwrap();
    assert_eq!(
        control
            .install_native_instance_launch_overlay(
                instance_id,
                NativeInstanceLaunchOverlay::new(
                    AgentId::new(HOOK_POSTING_FIXTURE_ID).unwrap(),
                    TransportKind::Pty,
                    Vec::new(),
                    vec![OsString::from("--bundle-mode")],
                )
                .unwrap(),
            )
            .unwrap_err(),
        NativeLaunchProfileError::EnvironmentOverlayBindingMismatch
    );
    assert_eq!(resolver_calls.load(Ordering::Acquire), 0);
    assert_eq!(runtime.active_native_sessions(), 0);
    assert!(handle.snapshot().sessions.is_empty());
}

#[tokio::test]
async fn bundle_only_native_instance_launch_overlay_mismatch_fails_before_child() {
    let (handle, mut runtime) = NativeRuntime::new(
        AgentRegistry::new([interactive_agent_spec()]).expect("fixture registry"),
        NativeRuntimeConfig::default(),
    );
    let instance_id = AgentInstanceId(8509);
    runtime
        .native_launch_profile_control()
        .install_native_instance_launch_overlay(
            instance_id,
            NativeInstanceLaunchOverlay::new(
                AgentId::new(HOOK_POSTING_FIXTURE_ID).unwrap(),
                TransportKind::Pty,
                Vec::new(),
                vec![
                    OsString::from("--bundle-root"),
                    OsString::from("must-not-spawn"),
                ],
            )
            .unwrap(),
        )
        .unwrap();

    register_and_start_agent(
        &handle,
        116,
        instance_id,
        AgentId::new(CONTROL_FIXTURE_ID).unwrap(),
    );
    drive_until(&mut runtime, |_| {
        handle.snapshot().sessions.iter().any(|session| {
            session.instance_id == instance_id
                && matches!(session.status, SessionStatus::Failed { .. })
        })
    })
    .await;

    let failure = handle
        .snapshot()
        .sessions
        .iter()
        .find(|session| session.instance_id == instance_id)
        .and_then(|session| match &session.status {
            SessionStatus::Failed { message } => Some(message.clone()),
            _ => None,
        })
        .expect("bundle-only binding mismatch must fail before spawn");
    assert_eq!(
        failure,
        NativeLaunchProfileError::EnvironmentOverlayBindingMismatch.to_string()
    );
    assert_eq!(runtime.active_native_sessions(), 0);
}

#[test]
fn native_instance_launch_overlay_debug_surfaces_never_expose_argv() {
    const SECRET_ARG: &str = "argv-secret-must-never-reach-debug";

    trait AmbiguousIfDebug<A> {
        fn marker() {}
    }

    impl<T: ?Sized> AmbiguousIfDebug<()> for T {}
    impl<T: ?Sized + std::fmt::Debug> AmbiguousIfDebug<u8> for T {}

    let _ = <NativeInstanceLaunchOverlay as AmbiguousIfDebug<_>>::marker;
    let overlay = NativeInstanceLaunchOverlay::new(
        AgentId::new(CONTROL_FIXTURE_ID).unwrap(),
        TransportKind::Pty,
        vec![EnvMutation {
            key: OsString::from(OVERLAY_SENTINEL),
            value: None,
        }],
        vec![OsString::from(SECRET_ARG)],
    )
    .unwrap();
    let (_handle, runtime) = NativeRuntime::new(
        AgentRegistry::new([interactive_agent_spec()]).expect("fixture registry"),
        NativeRuntimeConfig::default(),
    );
    let error = runtime
        .native_launch_profile_control()
        .install_native_instance_launch_overlay(AgentInstanceId(8507), overlay)
        .unwrap_err();
    assert!(!format!("{error:?}").contains(SECRET_ARG));
}
