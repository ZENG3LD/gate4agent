#[cfg(unix)]
use std::collections::BTreeSet;
#[cfg(unix)]
use std::ffi::OsString;
#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::Command;
#[cfg(unix)]
use std::sync::mpsc::TryRecvError;
#[cfg(unix)]
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use gate4agent_catalog::{builtin_registry, AgentRegistry};
#[cfg(unix)]
use gate4agent_runtime_native::{NativeRuntime, NativeRuntimeConfig};
#[cfg(unix)]
use gate4agent_types::{
    AgentId, AgentInstanceId, CommandEnvelope, CommandId, ControlCommand, ControlEvent,
    ControlEventKind, InputAction, PreparedInputKind, ProviderRuntimePolicy, SessionStatus,
    StartRequest, TerminalControl, TerminalSize, TransportKind, CONTROL_PROTOCOL_VERSION,
};

#[cfg(unix)]
const LIVE_CANARY_ENV: &str = "GATE4AGENT_VENDOR_PTY_SCREEN_CANARY";
#[cfg(unix)]
const LIVE_AGENT_ENV: &str = "GATE4AGENT_VENDOR_PTY_SCREEN_AGENT";
#[cfg(unix)]
const LIVE_LAUNCHER_ENV: &str = "GATE4AGENT_VENDOR_PTY_SCREEN_LAUNCHER";
#[cfg(unix)]
const LIVE_VERSION_ENV: &str = "GATE4AGENT_VENDOR_PTY_SCREEN_VERSION";
#[cfg(unix)]
const START_TIMEOUT: Duration = Duration::from_secs(45);
#[cfg(unix)]
const OPERATION_TIMEOUT: Duration = Duration::from_secs(15);
#[cfg(unix)]
const STOP_TIMEOUT: Duration = Duration::from_secs(30);

#[cfg(unix)]
struct LiveConfig {
    version: String,
    launcher: PathBuf,
}

#[cfg(unix)]
struct PrivateEnvironment {
    saved: Option<Vec<(OsString, OsString)>>,
    root: PathBuf,
    workspace: PathBuf,
}

#[cfg(unix)]
impl PrivateEnvironment {
    fn new() -> Result<Self, &'static str> {
        use std::os::unix::fs::DirBuilderExt;

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| "private vendor PTY clock is unavailable")?
            .as_nanos();
        let mut root = None;
        for attempt in 0..32_u8 {
            let candidate = std::env::temp_dir().join(format!(
                "g4a-vpty-{}-{nonce}-{attempt}",
                std::process::id(),
            ));
            let result = fs::DirBuilder::new().mode(0o700).create(&candidate);
            match result {
                Ok(()) => {
                    root = Some(candidate);
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_) => return Err("private vendor PTY directory could not be created"),
            }
        }
        let root = root.ok_or("private vendor PTY directory allocation was exhausted")?;
        let home = root.join("home");
        let temporary = root.join("tmp");
        let xdg_config = root.join("xdg-config");
        let xdg_cache = root.join("xdg-cache");
        let workspace = root.join("workspace");
        for directory in [&home, &temporary, &xdg_config, &xdg_cache, &workspace] {
            if fs::DirBuilder::new()
                .mode(0o700)
                .create(directory)
                .is_err()
            {
                let _ = fs::remove_dir_all(&root);
                return Err("private vendor PTY subdirectory could not be created");
            }
        }

        let saved = std::env::vars_os().collect::<Vec<_>>();
        for (name, _) in &saved {
            std::env::remove_var(name);
        }
        std::env::set_var("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
        std::env::set_var("HOME", &home);
        std::env::set_var("TMPDIR", &temporary);
        std::env::set_var("XDG_CONFIG_HOME", &xdg_config);
        std::env::set_var("XDG_CACHE_HOME", &xdg_cache);
        std::env::set_var("TERM", "xterm-256color");
        std::env::set_var("LANG", "C");
        std::env::set_var("DISABLE_AUTOUPDATER", "1");
        Ok(Self {
            saved: Some(saved),
            root,
            workspace,
        })
    }

    fn workspace(&self) -> &Path {
        &self.workspace
    }

    fn restore(&mut self) {
        let Some(saved) = self.saved.take() else {
            return;
        };
        let current_names = std::env::vars_os()
            .map(|(name, _)| name)
            .collect::<Vec<_>>();
        for name in current_names {
            std::env::remove_var(name);
        }
        for (name, value) in saved {
            std::env::set_var(name, value);
        }
    }

    fn close(mut self) -> Result<(), &'static str> {
        self.restore();
        fs::remove_dir_all(&self.root)
            .map_err(|_| "private vendor PTY directory cleanup failed")?;
        self.root.clear();
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for PrivateEnvironment {
    fn drop(&mut self) {
        self.restore();
        if !self.root.as_os_str().is_empty() {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProcessRow {
    process_id: u32,
    parent_process_id: u32,
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ScreenEvidence {
    bytes_nonzero: bool,
    screen_nonempty: bool,
    root_process_id: u32,
}

fn validate_lab_values(
    expected_agent: &str,
    canary: &str,
    selected_agent: &str,
    version: &str,
) -> Result<(), &'static str> {
    if canary != "1" {
        return Err("vendor PTY screen execution is not explicitly enabled");
    }
    if !matches!(expected_agent, "claude" | "codex" | "kimi") {
        return Err("vendor PTY screen test requested an unsupported agent");
    }
    if selected_agent != expected_agent {
        return Err("vendor PTY screen agent does not match the selected test");
    }
    if version.is_empty()
        || version.len() > 128
        || !version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'-'))
    {
        return Err("vendor PTY screen version is missing or unsafe for sanitized evidence");
    }
    Ok(())
}

#[cfg(unix)]
fn required_utf8_environment(name: &str) -> Result<String, &'static str> {
    let value = std::env::var_os(name).ok_or("vendor PTY screen environment is incomplete")?;
    value
        .into_string()
        .map_err(|_| "vendor PTY screen environment must be Unicode")
}

#[cfg(unix)]
fn live_config(expected_agent: &str) -> Result<LiveConfig, &'static str> {
    let canary = required_utf8_environment(LIVE_CANARY_ENV)?;
    let selected_agent = required_utf8_environment(LIVE_AGENT_ENV)?;
    let version = required_utf8_environment(LIVE_VERSION_ENV)?;
    validate_lab_values(expected_agent, &canary, &selected_agent, &version)?;

    let launcher = std::env::var_os(LIVE_LAUNCHER_ENV)
        .map(PathBuf::from)
        .ok_or("vendor PTY screen environment is incomplete")?;
    if !launcher.is_absolute() {
        return Err("vendor PTY screen launcher must be an absolute path");
    }
    let launcher = launcher
        .canonicalize()
        .map_err(|_| "vendor PTY screen launcher cannot be canonicalized")?;
    let metadata = launcher
        .metadata()
        .map_err(|_| "vendor PTY screen launcher metadata is unavailable")?;
    if !metadata.is_file() {
        return Err("vendor PTY screen launcher must be a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err("vendor PTY screen launcher is not executable");
        }
    }
    if launcher.to_str().is_none() {
        return Err("vendor PTY screen launcher path must be Unicode");
    }
    Ok(LiveConfig { version, launcher })
}

#[cfg(unix)]
fn command(id: u64, command: ControlCommand) -> CommandEnvelope {
    CommandEnvelope {
        protocol_version: CONTROL_PROTOCOL_VERSION,
        id: CommandId(id),
        command,
    }
}

#[cfg(unix)]
fn runtime_for(
    agent_id: &str,
    launcher: &Path,
) -> (gate4agent_handle::Gate4AgentHandle, NativeRuntime) {
    let mut spec = builtin_registry()
        .get_by_id(agent_id)
        .unwrap_or_else(|| panic!("missing built-in agent spec for {agent_id}"))
        .clone();
    spec.launch.program = launcher
        .to_str()
        .expect("validated vendor PTY screen launcher path")
        .to_owned();
    NativeRuntime::new(
        AgentRegistry::new([spec]).expect("single-agent vendor PTY screen registry"),
        NativeRuntimeConfig {
            worker_poll_interval_ms: 5,
            ..NativeRuntimeConfig::default()
        },
    )
}

#[cfg(unix)]
fn drain_events(
    subscription: &gate4agent_handle::EventSubscription,
    events: &mut Vec<ControlEvent>,
) {
    loop {
        match subscription.try_recv() {
            Ok(event) => events.push(event),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
        }
    }
}

#[cfg(unix)]
fn terminal_status(status: &SessionStatus) -> &'static str {
    match status {
        SessionStatus::Registered => "registered",
        SessionStatus::Starting => "starting",
        SessionStatus::Running => "running",
        SessionStatus::Stopping => "stopping",
        SessionStatus::Exited { .. } => "exited",
        SessionStatus::Failed { .. } => "failed",
    }
}

#[cfg(unix)]
fn parse_process_table(text: &str) -> Result<Vec<ProcessRow>, &'static str> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let mut fields = line.split_whitespace();
            let process_id = fields
                .next()
                .ok_or("process table row has no process ID")?
                .parse::<u32>()
                .map_err(|_| "process table has an invalid process ID")?;
            let parent_process_id = fields
                .next()
                .ok_or("process table row has no parent process ID")?
                .parse::<u32>()
                .map_err(|_| "process table has an invalid parent process ID")?;
            if fields.next().is_some() {
                return Err("process table row has unexpected fields");
            }
            Ok(ProcessRow {
                process_id,
                parent_process_id,
            })
        })
        .collect()
}

#[cfg(unix)]
fn process_table() -> Result<Vec<ProcessRow>, &'static str> {
    let output = Command::new("/bin/ps")
        .args(["-axo", "pid=,ppid="])
        .output()
        .map_err(|_| "process table command could not start")?;
    if !output.status.success() {
        return Err("process table command failed");
    }
    let text = std::str::from_utf8(&output.stdout)
        .map_err(|_| "process table command returned non-UTF-8 output")?;
    parse_process_table(text)
}

#[cfg(unix)]
fn process_tree(rows: &[ProcessRow], root_process_id: u32) -> BTreeSet<u32> {
    let mut tree = BTreeSet::from([root_process_id]);
    loop {
        let previous_len = tree.len();
        for row in rows {
            if tree.contains(&row.parent_process_id) {
                tree.insert(row.process_id);
            }
        }
        if tree.len() == previous_len {
            return tree;
        }
    }
}

#[cfg(unix)]
fn retain_observed_process_tree(
    root_process_id: u32,
    observed_processes: &mut BTreeSet<u32>,
) -> Result<(), &'static str> {
    observed_processes.extend(process_tree(&process_table()?, root_process_id));
    Ok(())
}

#[cfg(unix)]
async fn wait_for_screen(
    runtime: &mut NativeRuntime,
    handle: &gate4agent_handle::Gate4AgentHandle,
    subscription: &gate4agent_handle::EventSubscription,
    events: &mut Vec<ControlEvent>,
    observed_processes: &mut BTreeSet<u32>,
) -> Result<ScreenEvidence, String> {
    tokio::time::timeout(START_TIMEOUT, async {
        loop {
            runtime.tick().await;
            drain_events(subscription, events);
            let snapshot = handle.snapshot();
            let session = snapshot
                .sessions
                .first()
                .ok_or_else(|| "vendor PTY screen session snapshot is missing".to_owned())?;
            if let Some(root_process_id) = session.process_id {
                retain_observed_process_tree(root_process_id, observed_processes)
                    .map_err(str::to_owned)?;
            }
            if let (Some(root_process_id), Some(frame)) = (session.process_id, &session.terminal_frame)
            {
                let bytes_nonzero = !frame.formatted.is_empty()
                    || frame
                        .scrollback_formatted
                        .iter()
                        .any(|line| !line.is_empty());
                let screen_nonempty = frame.contents.chars().any(|character| !character.is_whitespace());
                if bytes_nonzero && screen_nonempty && session.status == SessionStatus::Running {
                    return Ok(ScreenEvidence {
                        bytes_nonzero,
                        screen_nonempty,
                        root_process_id,
                    });
                }
            }
            if matches!(session.status, SessionStatus::Failed { .. } | SessionStatus::Exited { .. })
            {
                return Err(format!(
                    "vendor PTY stopped before a renderable screen; status={}",
                    terminal_status(&session.status),
                ));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .map_err(|_| "vendor PTY screen startup timed out".to_owned())?
}

#[cfg(unix)]
async fn wait_for_resize(
    runtime: &mut NativeRuntime,
    handle: &gate4agent_handle::Gate4AgentHandle,
    subscription: &gate4agent_handle::EventSubscription,
    events: &mut Vec<ControlEvent>,
    event_start: usize,
    size: TerminalSize,
) -> Result<(), String> {
    tokio::time::timeout(OPERATION_TIMEOUT, async {
        loop {
            runtime.tick().await;
            drain_events(subscription, events);
            if events[event_start..].iter().any(|event| {
                matches!(event.event, ControlEventKind::Resized { size: actual } if actual == size)
            }) {
                return Ok(());
            }
            if events[event_start..]
                .iter()
                .any(|event| matches!(event.event, ControlEventKind::ResizeFailed { .. }))
            {
                return Err("vendor PTY resize failed".to_owned());
            }
            if handle.snapshot().sessions.first().is_some_and(|session| {
                matches!(session.status, SessionStatus::Failed { .. } | SessionStatus::Exited { .. })
            }) {
                return Err("vendor PTY stopped before resize completed".to_owned());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .map_err(|_| "vendor PTY resize timed out".to_owned())?
}

#[cfg(unix)]
async fn wait_for_interrupt(
    runtime: &mut NativeRuntime,
    handle: &gate4agent_handle::Gate4AgentHandle,
    subscription: &gate4agent_handle::EventSubscription,
    events: &mut Vec<ControlEvent>,
    event_start: usize,
) -> Result<(), String> {
    tokio::time::timeout(OPERATION_TIMEOUT, async {
        loop {
            runtime.tick().await;
            drain_events(subscription, events);
            if events[event_start..].iter().any(|event| {
                matches!(
                    event.event,
                    ControlEventKind::InputCompleted {
                        input_kind: PreparedInputKind::TerminalControl,
                    }
                )
            }) {
                return Ok(());
            }
            if events[event_start..].iter().any(|event| {
                matches!(
                    event.event,
                    ControlEventKind::InputFailed {
                        input_kind: PreparedInputKind::TerminalControl,
                        ..
                    }
                )
            }) {
                return Err("vendor PTY interrupt delivery failed".to_owned());
            }
            if handle.snapshot().sessions.first().is_some_and(|session| {
                matches!(session.status, SessionStatus::Failed { .. } | SessionStatus::Exited { .. })
            }) {
                return Err("vendor PTY stopped before interrupt delivery completed".to_owned());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .map_err(|_| "vendor PTY interrupt delivery timed out".to_owned())?
}

#[cfg(unix)]
async fn stop_session(
    runtime: &mut NativeRuntime,
    handle: &gate4agent_handle::Gate4AgentHandle,
    subscription: &gate4agent_handle::EventSubscription,
    events: &mut Vec<ControlEvent>,
    instance_id: AgentInstanceId,
) -> Result<(), String> {
    runtime.tick().await;
    drain_events(subscription, events);
    if runtime.active_native_sessions() != 0 {
        handle
            .dispatch(command(
                5,
                ControlCommand::Stop {
                    instance_id,
                    force: true,
                },
            ))
            .map_err(|_| "vendor PTY force-stop dispatch failed".to_owned())?;
    }
    tokio::time::timeout(STOP_TIMEOUT, async {
        loop {
            runtime.tick().await;
            drain_events(subscription, events);
            if runtime.active_native_sessions() == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .map_err(|_| "vendor PTY force-stop timed out".to_owned())?;
    Ok(())
}

#[cfg(unix)]
async fn wait_for_process_tree_reaped(observed_processes: &BTreeSet<u32>) -> Result<(), String> {
    tokio::time::timeout(STOP_TIMEOUT, async {
        loop {
            let live = process_table()
                .map_err(str::to_owned)?
                .into_iter()
                .map(|row| row.process_id)
                .collect::<BTreeSet<_>>();
            if observed_processes.is_disjoint(&live) {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .map_err(|_| "vendor PTY root or descendant process remained after stop".to_owned())?
}

#[cfg(unix)]
async fn run_live_vendor_screen(agent_id: &str, instance_id: u64) {
    let config = live_config(agent_id).unwrap_or_else(|error| panic!("{error}"));
    let private_environment = PrivateEnvironment::new()
        .unwrap_or_else(|error| panic!("{error}"));
    let working_directory = private_environment
        .workspace()
        .to_str()
        .expect("private vendor PTY workspace path must be Unicode")
        .to_owned();
    let (handle, mut runtime) = runtime_for(agent_id, &config.launcher);
    let subscription = handle.subscribe(256);
    let instance_id = AgentInstanceId(instance_id);
    let initial_size = TerminalSize {
        rows: 30,
        columns: 120,
    };
    handle
        .dispatch(command(
            1,
            ControlCommand::Register {
                instance_id,
                agent_id: AgentId::new(agent_id).expect("valid live vendor agent ID"),
                transport: TransportKind::Pty,
            },
        ))
        .expect("register live vendor PTY screen session");
    handle
        .dispatch(command(
            2,
            ControlCommand::Start {
                instance_id,
                runtime_policy: ProviderRuntimePolicy::raw_pty(),
                request: StartRequest {
                    working_directory,
                    terminal_size: initial_size,
                    initial_prompt: None,
                    session_options: None,
                },
            },
        ))
        .expect("start live vendor PTY screen session");

    let mut events = Vec::new();
    let mut observed_processes = BTreeSet::new();
    let scenario = async {
        let evidence = wait_for_screen(
            &mut runtime,
            &handle,
            &subscription,
            &mut events,
            &mut observed_processes,
        )
        .await?;
        retain_observed_process_tree(evidence.root_process_id, &mut observed_processes)
            .map_err(str::to_owned)?;

        let resized = TerminalSize {
            rows: 41,
            columns: 137,
        };
        let resize_event_start = events.len();
        handle
            .dispatch(command(
                3,
                ControlCommand::Resize {
                    instance_id,
                    size: resized,
                },
            ))
            .map_err(|_| "vendor PTY resize dispatch failed".to_owned())?;
        wait_for_resize(
            &mut runtime,
            &handle,
            &subscription,
            &mut events,
            resize_event_start,
            resized,
        )
        .await?;
        retain_observed_process_tree(evidence.root_process_id, &mut observed_processes)
            .map_err(str::to_owned)?;

        let interrupt_event_start = events.len();
        handle
            .dispatch(command(
                4,
                ControlCommand::SendInput {
                    instance_id,
                    action: InputAction::TerminalControl(TerminalControl::Interrupt),
                },
            ))
            .map_err(|_| "vendor PTY interrupt dispatch failed".to_owned())?;
        wait_for_interrupt(
            &mut runtime,
            &handle,
            &subscription,
            &mut events,
            interrupt_event_start,
        )
        .await?;
        let _ = retain_observed_process_tree(evidence.root_process_id, &mut observed_processes);
        Ok::<ScreenEvidence, String>(evidence)
    }
    .await;
    let cleanup = stop_session(
        &mut runtime,
        &handle,
        &subscription,
        &mut events,
        instance_id,
    )
    .await;
    let reaped = wait_for_process_tree_reaped(&observed_processes).await;
    let evidence = scenario.unwrap_or_else(|error| {
        panic!(
            "{error}; cleanup={} reaped={}",
            if cleanup.is_ok() { "ok" } else { "failed" },
            if reaped.is_ok() { "true" } else { "false" },
        )
    });
    cleanup.expect("clean up live vendor PTY screen session");
    reaped.expect("verify live vendor PTY root and descendant teardown");

    assert!(evidence.bytes_nonzero);
    assert!(evidence.screen_nonempty);
    assert!(!observed_processes.is_empty());
    assert_eq!(runtime.active_native_sessions(), 0);
    drop(runtime);
    drop(subscription);
    drop(handle);
    private_environment
        .close()
        .expect("clean up private vendor PTY environment");
    println!(
        "vendor_pty_screen agent={agent_id} version={} bytes_nonzero=true screen_nonempty=true resize=true interrupt=true reaped=true active_sessions=0",
        config.version,
    );
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "requires an exact isolated Claude launcher; authentication is not required"]
async fn unix_live_claude_real_pty_screen_without_login() {
    run_live_vendor_screen("claude", 30_001).await;
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "requires an exact isolated Codex launcher; authentication is not required"]
async fn unix_live_codex_real_pty_screen_without_login() {
    run_live_vendor_screen("codex", 30_002).await;
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "requires an exact isolated Kimi launcher; authentication is not required"]
async fn unix_live_kimi_real_pty_screen_without_login() {
    run_live_vendor_screen("kimi", 30_003).await;
}

#[test]
fn live_vendor_screen_environment_contract_fails_closed() {
    assert_eq!(
        validate_lab_values("claude", "1", "claude", "1.2.3"),
        Ok(())
    );
    assert_eq!(
        validate_lab_values("claude", "", "claude", "1.2.3"),
        Err("vendor PTY screen execution is not explicitly enabled")
    );
    assert_eq!(
        validate_lab_values("claude", "1", "codex", "1.2.3"),
        Err("vendor PTY screen agent does not match the selected test")
    );
    assert_eq!(
        validate_lab_values("claude", "1", "claude", "1.2.3 secret"),
        Err("vendor PTY screen version is missing or unsafe for sanitized evidence")
    );
}

#[cfg(unix)]
#[test]
fn process_tree_collects_all_recursive_descendants() {
    let rows = [
        ProcessRow {
            process_id: 11,
            parent_process_id: 1,
        },
        ProcessRow {
            process_id: 12,
            parent_process_id: 11,
        },
        ProcessRow {
            process_id: 13,
            parent_process_id: 12,
        },
        ProcessRow {
            process_id: 99,
            parent_process_id: 1,
        },
    ];
    assert_eq!(process_tree(&rows, 11), BTreeSet::from([11, 12, 13]));
}
