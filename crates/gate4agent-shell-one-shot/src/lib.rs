//! Bounded native process authority for provider-owned one-shot plans.

use gate4agent::agent::EnvMutation;
use gate4agent::child_environment::platform_minimal_child_environment;
use gate4agent::AgentEvent;
use gate4agent_adapters::{
    resolve_one_shot_plan_with_persistence, OneShotAdapterError, OneShotPlan,
    OneShotSessionPersistence, ONE_SHOT_OUTPUT_MAX_BYTES, ONE_SHOT_TIMEOUT_SECONDS,
};
use gate4agent_types::{
    AdapterBinding, AgentSpec, SessionOptionSelection, PROVIDER_EVENT_TEXT_MAX_BYTES,
};
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;
use tokio::time::{sleep_until, timeout, Instant};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeOneShotConfig {
    pub timeout: Duration,
    pub output_max_bytes: usize,
}

impl Default for NativeOneShotConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(ONE_SHOT_TIMEOUT_SECONDS),
            output_max_bytes: ONE_SHOT_OUTPUT_MAX_BYTES,
        }
    }
}

pub struct NativeOneShotSession {
    process_id: u32,
    events: broadcast::Sender<AgentEvent>,
    initial_events: Mutex<Option<broadcast::Receiver<AgentEvent>>>,
    cancel: watch::Sender<bool>,
    reader_task: JoinHandle<()>,
}

struct RunningProcess {
    child: tokio::process::Child,
    stdin: Option<ChildStdin>,
    stdout: tokio::process::ChildStdout,
    stderr: tokio::process::ChildStderr,
    process_id: u32,
}

impl NativeOneShotSession {
    pub async fn spawn(
        spec: &AgentSpec,
        binding: &AdapterBinding,
        prompt: &str,
        selection: Option<&SessionOptionSelection>,
        working_directory: &Path,
    ) -> Result<Self, NativeOneShotError> {
        Self::spawn_with_environment(
            spec,
            binding,
            prompt,
            selection,
            working_directory,
            &[],
        )
        .await
    }

    /// Spawn a one-shot child with shell-owned mutations applied after the
    /// platform-minimal inherited environment.
    pub async fn spawn_with_environment(
        spec: &AgentSpec,
        binding: &AdapterBinding,
        prompt: &str,
        selection: Option<&SessionOptionSelection>,
        working_directory: &Path,
        environment: &[EnvMutation],
    ) -> Result<Self, NativeOneShotError> {
        Self::spawn_with_environment_and_persistence(
            spec,
            binding,
            prompt,
            selection,
            working_directory,
            environment,
            OneShotSessionPersistence::Ephemeral,
        )
        .await
    }

    /// Spawn a one-shot child with an explicit provider session-persistence
    /// policy and shell-owned environment mutations.
    pub async fn spawn_with_environment_and_persistence(
        spec: &AgentSpec,
        binding: &AdapterBinding,
        prompt: &str,
        selection: Option<&SessionOptionSelection>,
        working_directory: &Path,
        environment: &[EnvMutation],
        persistence: OneShotSessionPersistence,
    ) -> Result<Self, NativeOneShotError> {
        Self::spawn_with_environment_and_config(
            spec,
            binding,
            prompt,
            selection,
            working_directory,
            environment,
            NativeOneShotConfig::default(),
            persistence,
        )
        .await
    }

    pub async fn spawn_with_config(
        spec: &AgentSpec,
        binding: &AdapterBinding,
        prompt: &str,
        selection: Option<&SessionOptionSelection>,
        working_directory: &Path,
        config: NativeOneShotConfig,
    ) -> Result<Self, NativeOneShotError> {
        Self::spawn_with_environment_and_config(
            spec,
            binding,
            prompt,
            selection,
            working_directory,
            &[],
            config,
            OneShotSessionPersistence::Ephemeral,
        )
        .await
    }

    async fn spawn_with_environment_and_config(
        spec: &AgentSpec,
        binding: &AdapterBinding,
        prompt: &str,
        selection: Option<&SessionOptionSelection>,
        working_directory: &Path,
        environment: &[EnvMutation],
        mut config: NativeOneShotConfig,
        persistence: OneShotSessionPersistence,
    ) -> Result<Self, NativeOneShotError> {
        config.output_max_bytes = config.output_max_bytes.clamp(1, ONE_SHOT_OUTPUT_MAX_BYTES);
        let pipe = spec
            .capabilities
            .transports
            .pipe
            .as_ref()
            .ok_or(NativeOneShotError::UndeclaredTransport)?;
        if &pipe.adapter != binding || spec.capabilities.adapters.one_shot.as_ref() != Some(binding)
        {
            return Err(NativeOneShotError::BindingMismatch);
        }
        let mut plan = resolve_one_shot_plan_with_persistence(
            &binding.id,
            &spec.launch,
            prompt,
            selection,
            persistence,
        )?;
        if let Some(launch) = &pipe.launch_override {
            if persistence == OneShotSessionPersistence::Persist {
                return Err(NativeOneShotError::PersistentLaunchOverrideUnsupported);
            }
            plan.program = launch.program.clone();
            plan.args = launch.fixed_args.clone();
            if plan.stdin_payload.is_none() {
                plan.args.push(prompt.to_owned());
            }
        }
        let mut command = native_command(&plan.program, &plan.args)?;
        command.env_clear();
        command.envs(platform_minimal_child_environment());
        for mutation in environment {
            if let Some(value) = &mutation.value {
                command.env(&mutation.key, value);
            } else {
                command.env_remove(&mutation.key);
            }
        }
        command
            .current_dir(working_directory)
            .stdin(if plan.stdin_payload.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        configure_process_group(&mut command);
        let mut child = command
            .spawn()
            .map_err(|_| NativeOneShotError::SpawnUnavailable)?;
        let process_id = child.id().ok_or(NativeOneShotError::SpawnUnavailable)?;
        let stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or(NativeOneShotError::SpawnUnavailable)?;
        let stderr = child
            .stderr
            .take()
            .ok_or(NativeOneShotError::SpawnUnavailable)?;
        let (events, initial_events) = broadcast::channel(64);
        let event_tx = events.clone();
        let (cancel, cancel_rx) = watch::channel(false);
        let reader_task = tokio::spawn(run_process(
            RunningProcess {
                child,
                stdin,
                stdout,
                stderr,
                process_id,
            },
            plan,
            config,
            cancel_rx,
            event_tx,
        ));
        Ok(Self {
            process_id,
            events,
            initial_events: Mutex::new(Some(initial_events)),
            cancel,
            reader_task,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.initial_events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            .unwrap_or_else(|| self.events.subscribe())
    }

    pub fn process_id(&self) -> Option<u32> {
        Some(self.process_id)
    }

    pub fn reader_finished(&self) -> bool {
        self.reader_task.is_finished()
    }

    pub async fn kill(&mut self) -> Result<(), NativeOneShotError> {
        let _ = self.cancel.send(true);
        match timeout(Duration::from_secs(3), &mut self.reader_task).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(NativeOneShotError::ExecutorUnavailable),
            Err(_) => {
                terminate_process_tree(self.process_id).await;
                self.reader_task.abort();
                Err(NativeOneShotError::ExecutorUnavailable)
            }
        }
    }
}

impl Drop for NativeOneShotSession {
    fn drop(&mut self) {
        let _ = self.cancel.send(true);
    }
}

async fn run_process(
    process: RunningProcess,
    plan: OneShotPlan,
    config: NativeOneShotConfig,
    mut cancel: watch::Receiver<bool>,
    events: broadcast::Sender<AgentEvent>,
) {
    let RunningProcess {
        mut child,
        stdin,
        stdout,
        stderr,
        process_id,
    } = process;
    let total = Arc::new(AtomicUsize::new(0));
    let stdout_total = Arc::clone(&total);
    let stderr_total = total;
    let stdin_payload = plan.stdin_payload;
    let execution = async {
        let wait = async {
            child
                .wait()
                .await
                .map_err(|_| OneShotFailure::ExecutorUnavailable)
        };
        let read_stdout = read_bounded(stdout, stdout_total, config.output_max_bytes);
        let read_stderr = read_bounded(stderr, stderr_total, config.output_max_bytes);
        let write_stdin = write_prompt(stdin, stdin_payload);
        tokio::try_join!(wait, read_stdout, read_stderr, write_stdin)
            .map(|(status, stdout, stderr, ())| (status, stdout, stderr))
    };
    tokio::pin!(execution);
    let deadline = Instant::now() + config.timeout;
    let outcome = tokio::select! {
        result = &mut execution => match result {
            Ok((status, stdout, stderr)) => completed_outcome(status, stdout, stderr),
            Err(failure) => {
                terminate_process_tree(process_id).await;
                OneShotOutcome::Failed { failure, exit_code: None }
            }
        },
        _ = sleep_until(deadline) => {
            terminate_process_tree(process_id).await;
            OneShotOutcome::Failed { failure: OneShotFailure::TimedOut, exit_code: None }
        },
        changed = cancel.changed() => {
            if changed.is_err() || *cancel.borrow() {
                terminate_process_tree(process_id).await;
            }
            OneShotOutcome::Canceled
        }
    };
    publish_outcome(outcome, &events);
}

async fn write_prompt(
    stdin: Option<ChildStdin>,
    payload: Option<String>,
) -> Result<(), OneShotFailure> {
    let (Some(mut stdin), Some(payload)) = (stdin, payload) else {
        return Ok(());
    };
    stdin
        .write_all(payload.as_bytes())
        .await
        .map_err(|_| OneShotFailure::ExecutorUnavailable)?;
    stdin
        .shutdown()
        .await
        .map_err(|_| OneShotFailure::ExecutorUnavailable)
}

async fn read_bounded(
    mut reader: impl AsyncRead + Unpin,
    total: Arc<AtomicUsize>,
    limit: usize,
) -> Result<Vec<u8>, OneShotFailure> {
    let mut output = Vec::new();
    let mut chunk = [0_u8; 8 * 1024];
    loop {
        let read = reader
            .read(&mut chunk)
            .await
            .map_err(|_| OneShotFailure::ExecutorUnavailable)?;
        if read == 0 {
            return Ok(output);
        }
        let previous = total.fetch_add(read, Ordering::AcqRel);
        if previous > limit || read > limit.saturating_sub(previous) {
            return Err(OneShotFailure::OutputLimitExceeded);
        }
        output.extend_from_slice(&chunk[..read]);
    }
}

fn completed_outcome(status: ExitStatus, stdout: Vec<u8>, _stderr: Vec<u8>) -> OneShotOutcome {
    let exit_code = status.code();
    if !status.success() {
        return OneShotOutcome::Failed {
            failure: OneShotFailure::NonZeroExit,
            exit_code,
        };
    }
    let text = String::from_utf8_lossy(&stdout).replace("\r\n", "\n");
    let text = text.trim().to_owned();
    if text.is_empty() {
        OneShotOutcome::Failed {
            failure: OneShotFailure::EmptyOutput,
            exit_code,
        }
    } else {
        OneShotOutcome::Succeeded { text, exit_code }
    }
}

fn publish_outcome(outcome: OneShotOutcome, events: &broadcast::Sender<AgentEvent>) {
    match outcome {
        OneShotOutcome::Succeeded { text, exit_code } => {
            for chunk in utf8_chunks(&text, PROVIDER_EVENT_TEXT_MAX_BYTES) {
                let _ = events.send(AgentEvent::Text {
                    text: chunk.to_owned(),
                    is_delta: true,
                });
            }
            let _ = events.send(AgentEvent::TurnComplete {
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                reasoning_tokens: 0,
                context_window: None,
                is_cumulative: false,
            });
            let _ = events.send(AgentEvent::SessionEnd {
                result: "one-shot completed".to_owned(),
                cost_usd: None,
                is_error: false,
            });
            let _ = events.send(AgentEvent::Exited {
                code: exit_code.unwrap_or(0),
            });
        }
        OneShotOutcome::Failed { failure, exit_code } => {
            let message = failure.message().to_owned();
            let _ = events.send(AgentEvent::Error {
                message: message.clone(),
            });
            let _ = events.send(AgentEvent::SessionEnd {
                result: message,
                cost_usd: None,
                is_error: true,
            });
            let _ = events.send(AgentEvent::Exited {
                code: exit_code.unwrap_or_else(|| failure.synthetic_exit_code()),
            });
        }
        OneShotOutcome::Canceled => {
            let _ = events.send(AgentEvent::Exited { code: 130 });
        }
    }
}

fn utf8_chunks(mut text: &str, max_bytes: usize) -> Vec<&str> {
    let mut chunks = Vec::new();
    while text.len() > max_bytes {
        let mut split = max_bytes;
        while !text.is_char_boundary(split) {
            split -= 1;
        }
        chunks.push(&text[..split]);
        text = &text[split..];
    }
    if !text.is_empty() {
        chunks.push(text);
    }
    chunks
}

enum OneShotOutcome {
    Succeeded {
        text: String,
        exit_code: Option<i32>,
    },
    Failed {
        failure: OneShotFailure,
        exit_code: Option<i32>,
    },
    Canceled,
}

#[derive(Clone, Copy)]
enum OneShotFailure {
    TimedOut,
    OutputLimitExceeded,
    NonZeroExit,
    EmptyOutput,
    ExecutorUnavailable,
}

impl OneShotFailure {
    fn message(self) -> &'static str {
        match self {
            Self::TimedOut => "one-shot process timed out",
            Self::OutputLimitExceeded => "one-shot process exceeded the combined output limit",
            Self::NonZeroExit => "one-shot process returned a non-zero exit status",
            Self::EmptyOutput => "one-shot process returned an empty result",
            Self::ExecutorUnavailable => "one-shot process executor became unavailable",
        }
    }

    fn synthetic_exit_code(self) -> i32 {
        match self {
            Self::TimedOut => 124,
            Self::OutputLimitExceeded => 125,
            Self::NonZeroExit | Self::EmptyOutput | Self::ExecutorUnavailable => 1,
        }
    }
}

fn native_command(program: &str, args: &[String]) -> Result<Command, NativeOneShotError> {
    #[cfg(windows)]
    {
        let resolved = resolve_windows_command(program);
        if is_windows_batch(&resolved) {
            if let Some(shim) = resolve_windows_npm_shim(&resolved) {
                let mut command = Command::new(shim.program);
                if let Some(script) = shim.script {
                    command.arg(script);
                }
                command.args(args);
                return Ok(command);
            }
            if std::iter::once(resolved.as_str())
                .chain(args.iter().map(String::as_str))
                .any(has_unsafe_windows_batch_syntax)
            {
                return Err(NativeOneShotError::UnsafeWindowsBatchArguments);
            }
            let command_path = std::env::var_os("ComSpec")
                .map(std::path::PathBuf::from)
                .or_else(|| {
                    std::env::var_os("SystemRoot")
                        .map(|root| std::path::PathBuf::from(root).join("System32/cmd.exe"))
                })
                .unwrap_or_else(|| std::path::PathBuf::from("cmd.exe"));
            let mut command = Command::new(command_path);
            // `cmd /C <absolute-batch-path> ...` truncates a separately quoted
            // path at the first space under `/S` (for example `C:\Users\VA
            // PC\...\codex.cmd` becomes `C:\Users\VA`). Prefixing the reviewed
            // batch invocation with `CALL` keeps the quoted path as one command
            // token while preserving separate CreateProcess arguments.
            command
                .args(["/D", "/S", "/C", "CALL", &resolved])
                .args(args);
            return Ok(command);
        }
        let mut command = Command::new(resolved);
        command.args(args);
        Ok(command)
    }
    #[cfg(not(windows))]
    {
        let mut command = Command::new(program);
        command.args(args);
        Ok(command)
    }
}

#[cfg(windows)]
fn resolve_windows_command(program: &str) -> String {
    if program.contains(['/', '\\']) || Path::new(program).extension().is_some() {
        return program.to_owned();
    }
    let Some(path) = std::env::var_os("PATH") else {
        return program.to_owned();
    };
    for directory in std::env::split_paths(&path) {
        for name in [
            format!("{program}.cmd"),
            format!("{program}.exe"),
            format!("{program}.bat"),
            program.to_owned(),
        ] {
            let candidate = directory.join(name);
            if candidate.is_file() {
                return candidate.to_string_lossy().into_owned();
            }
        }
    }
    program.to_owned()
}

#[cfg(windows)]
fn is_windows_batch(program: &str) -> bool {
    Path::new(program)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| matches!(extension.to_ascii_lowercase().as_str(), "cmd" | "bat"))
}

#[cfg(windows)]
struct WindowsNpmShim {
    program: std::path::PathBuf,
    script: Option<std::path::PathBuf>,
}

#[cfg(windows)]
fn resolve_windows_npm_shim(program: &str) -> Option<WindowsNpmShim> {
    const NPM_SHIM_MAX_BYTES: u64 = 64 * 1024;

    let shim_path = Path::new(program);
    if std::fs::metadata(shim_path).ok()?.len() > NPM_SHIM_MAX_BYTES {
        return None;
    }
    let contents = std::fs::read_to_string(shim_path).ok()?;
    let has_npm_preamble = contents
        .lines()
        .any(|line| line.trim().eq_ignore_ascii_case("SET dp0=%~dp0"))
        && contents
            .lines()
            .any(|line| line.trim().eq_ignore_ascii_case("CALL :find_dp0"));
    if !has_npm_preamble {
        return None;
    }
    let target = contents.lines().find_map(|line| {
        if !line.trim_end().ends_with("%*") {
            return None;
        }
        let target = quoted_windows_segments(line).into_iter().find(|segment| {
            segment
                .get(..6)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("%dp0%\\"))
        })?;
        let quoted_target = format!("\"{target}\"");
        let target_start = line.find(&quoted_target)?;
        if line[target_start + quoted_target.len()..].trim() != "%*" {
            return None;
        }
        let extension = Path::new(target)
            .extension()
            .and_then(|value| value.to_str())?;
        let prefix = line[..target_start].trim();
        let canonical = if extension.eq_ignore_ascii_case("exe") {
            prefix.is_empty()
        } else if matches!(extension.to_ascii_lowercase().as_str(), "js" | "cjs" | "mjs") {
            prefix.eq_ignore_ascii_case(
                "endLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & \"%_prog%\"",
            )
        } else {
            false
        };
        canonical.then(|| target.to_owned())
    })?;
    let relative = Path::new(&target[6..]);
    if relative.components().any(|component| {
        !matches!(component, std::path::Component::Normal(_))
    }) {
        return None;
    }
    let shim_directory = shim_path.parent()?;
    let target_path = shim_directory.join(relative);
    if !target_path.is_file() {
        return None;
    }
    let extension = target_path
        .extension()
        .and_then(|value| value.to_str())?
        .to_ascii_lowercase();
    match extension.as_str() {
        "exe" => Some(WindowsNpmShim {
            program: target_path,
            script: None,
        }),
        "js" | "cjs" | "mjs" => {
            let bundled_node = shim_directory.join("node.exe");
            Some(WindowsNpmShim {
                program: if bundled_node.is_file() {
                    bundled_node
                } else {
                    std::path::PathBuf::from("node.exe")
                },
                script: Some(target_path),
            })
        }
        _ => None,
    }
}

#[cfg(windows)]
fn quoted_windows_segments(line: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut start = None;
    for (index, character) in line.char_indices() {
        if character != '"' {
            continue;
        }
        if let Some(opening) = start.take() {
            segments.push(&line[opening..index]);
        } else {
            start = Some(index + 1);
        }
    }
    segments
}

#[cfg(windows)]
fn has_unsafe_windows_batch_syntax(value: &str) -> bool {
    value.chars().any(|character| {
        matches!(
            character,
            '&' | '|' | '<' | '>' | '^' | '"' | '%' | '!' | '\r' | '\n'
        )
    })
}

fn configure_process_group(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command
            .as_std_mut()
            .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    }
}

async fn terminate_process_tree(process_id: u32) {
    #[cfg(windows)]
    let mut command = {
        let mut command = Command::new("taskkill.exe");
        command.args(["/PID", &process_id.to_string(), "/T", "/F"]);
        command
    };
    #[cfg(unix)]
    let mut command = {
        let mut command = Command::new("kill");
        command.args(["-KILL", "--", &format!("-{process_id}")]);
        command
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let _ = timeout(Duration::from_secs(2), command.status()).await;
}

#[derive(Debug, Error)]
pub enum NativeOneShotError {
    #[error(transparent)]
    Adapter(#[from] OneShotAdapterError),
    #[error("agent does not declare a one-shot Pipe transport")]
    UndeclaredTransport,
    #[error("one-shot transport and capability bindings do not match")]
    BindingMismatch,
    #[error("one-shot process could not be spawned")]
    SpawnUnavailable,
    #[error("one-shot Windows batch command contains unsafe arguments")]
    UnsafeWindowsBatchArguments,
    #[error("one-shot process executor became unavailable")]
    ExecutorUnavailable,
    #[error("persistent one-shot sessions do not allow a launch override")]
    PersistentLaunchOverrideUnsupported,
}

#[cfg(test)]
mod tests {
    use super::*;
    use gate4agent_catalog::builtin_registry;
    use gate4agent_types::{PipeProtocol, SpecVerification};
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    const CHILD_ENV_BOOTSTRAP_KEY: &str = "GATE4AGENT_ONE_SHOT_ENV_BOOTSTRAP";
    const CHILD_ENV_FIXTURE_KEY: &str = "GATE4AGENT_ONE_SHOT_ENV_FIXTURE";
    const CHILD_ENV_EXPECTED_HOME_KEY: &str = "GATE4AGENT_ONE_SHOT_EXPECTED_HOME";
    const CHILD_ENV_EXPECTED_CONFIG_KEY: &str = "GATE4AGENT_ONE_SHOT_EXPECTED_CONFIG";
    const CHILD_ENV_EXPLICIT_KEY: &str = "GATE4AGENT_ONE_SHOT_EXPLICIT";
    const CHILD_ENV_SENTINEL_KEY: &str = "GATE4AGENT_ONE_SHOT_FAKE_JWT";
    const CHILD_ENV_SENTINEL_VALUE: &str = "fake-jwt-that-must-not-cross-one-shot";

    fn fixture_dir(name: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "gate4agent-one-shot-{name}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        path
    }

    fn fixture_spec(script: &str) -> AgentSpec {
        let mut spec = builtin_registry().get_by_id("claude").unwrap().clone();
        #[cfg(windows)]
        {
            spec.launch.program = "powershell.exe".to_owned();
            spec.launch.fixed_args = vec![
                "-NoProfile".to_owned(),
                "-NonInteractive".to_owned(),
                "-Command".to_owned(),
                script.to_owned(),
            ];
        }
        #[cfg(not(windows))]
        {
            spec.launch.program = "sh".to_owned();
            spec.launch.fixed_args = vec!["-c".to_owned(), script.to_owned()];
        }
        let binding = spec.capabilities.adapters.one_shot.clone().unwrap();
        let pipe = spec.capabilities.transports.pipe.as_mut().unwrap();
        pipe.adapter = binding;
        pipe.protocol = PipeProtocol::OneShotText;
        pipe.launch_override = Some(spec.launch.clone());
        spec.verification = SpecVerification::Gate4AgentVerified;
        spec
    }

    #[cfg(windows)]
    fn child_home_key() -> &'static str {
        "USERPROFILE"
    }

    #[cfg(not(windows))]
    fn child_home_key() -> &'static str {
        "HOME"
    }

    #[cfg(windows)]
    fn child_config_key() -> &'static str {
        "APPDATA"
    }

    #[cfg(not(windows))]
    fn child_config_key() -> &'static str {
        "XDG_CONFIG_HOME"
    }

    #[cfg(windows)]
    fn child_removed_key() -> &'static str {
        "LOCALAPPDATA"
    }

    #[cfg(not(windows))]
    fn child_removed_key() -> &'static str {
        "LOGNAME"
    }

    async fn collect(session: &NativeOneShotSession) -> Vec<AgentEvent> {
        let mut receiver = session.subscribe();
        let mut events = Vec::new();
        timeout(Duration::from_secs(5), async {
            loop {
                let event = receiver.recv().await.unwrap();
                let exited = matches!(event, AgentEvent::Exited { .. });
                events.push(event);
                if exited {
                    break;
                }
            }
        })
        .await
        .unwrap();
        events
    }

    #[test]
    fn one_shot_child_environment_fixture() {
        if std::env::var_os(CHILD_ENV_FIXTURE_KEY).is_none() {
            return;
        }
        let expected_home = std::env::var_os(CHILD_ENV_EXPECTED_HOME_KEY).unwrap();
        let expected_config = std::env::var_os(CHILD_ENV_EXPECTED_CONFIG_KEY).unwrap();
        let mut checks = vec![
            ("ambient_custom_absent", std::env::var_os(CHILD_ENV_SENTINEL_KEY).is_none()),
            ("ambient_api_absent", std::env::var_os("OPENAI_API_KEY").is_none()),
            ("ambient_jwt_absent", std::env::var_os("GATE4AGENT_FAKE_JWT").is_none()),
            ("ambient_proxy_absent", std::env::var_os("HTTPS_PROXY").is_none()),
            ("ambient_ssh_absent", std::env::var_os("SSH_AUTH_SOCK").is_none()),
            ("bootstrap_absent", std::env::var_os(CHILD_ENV_BOOTSTRAP_KEY).is_none()),
            ("path_present", std::env::var_os("PATH").is_some()),
            (
                "home_present",
                std::env::var_os(child_home_key()).as_ref() == Some(&expected_home),
            ),
            (
                "config_present",
                std::env::var_os(child_config_key()).as_ref() == Some(&expected_config),
            ),
            (
                "explicit_present",
                std::env::var_os(CHILD_ENV_EXPLICIT_KEY).as_deref()
                    == Some(OsStr::new("explicit-value")),
            ),
            ("explicit_remove", std::env::var_os(child_removed_key()).is_none()),
        ];
        #[cfg(windows)]
        checks.extend([
            ("system_drive_present", std::env::var_os("SystemDrive").is_some()),
            ("system_root_present", std::env::var_os("SystemRoot").is_some()),
            ("windir_present", std::env::var_os("WINDIR").is_some()),
            ("comspec_present", std::env::var_os("ComSpec").is_some()),
        ]);
        for (name, passed) in checks {
            println!("{name}={passed}");
        }
    }

    #[tokio::test]
    async fn isolated_one_shot_child_receives_baseline_and_explicit_environment_only() {
        if std::env::var_os(CHILD_ENV_BOOTSTRAP_KEY).is_some() {
            run_isolated_one_shot_environment_bootstrap().await;
            return;
        }

        let directory = fixture_dir("environment-bootstrap");
        let home = directory.join("home");
        let config = directory.join("config");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&config).unwrap();
        let mut bootstrap = std::process::Command::new(std::env::current_exe().unwrap());
        bootstrap.env_clear();
        bootstrap.envs(platform_minimal_child_environment());
        bootstrap
            .args([
                "--exact",
                "tests::isolated_one_shot_child_receives_baseline_and_explicit_environment_only",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_ENV_BOOTSTRAP_KEY, "true")
            .env("HOME", &home)
            .env(child_home_key(), &home)
            .env(child_config_key(), &config)
            .env(child_removed_key(), "ambient-remove-me")
            .env(CHILD_ENV_SENTINEL_KEY, CHILD_ENV_SENTINEL_VALUE)
            .env("OPENAI_API_KEY", "fake-api-key")
            .env("GATE4AGENT_FAKE_JWT", "fake-control-jwt")
            .env("HTTPS_PROXY", "http://fake-auth@proxy.invalid")
            .env("SSH_AUTH_SOCK", directory.join("fake-ssh-agent.sock"));
        let output = bootstrap.output().unwrap();
        assert!(
            output.status.success(),
            "isolated OneShot bootstrap failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        fs::remove_dir_all(directory).unwrap();
    }

    async fn run_isolated_one_shot_environment_bootstrap() {
        let expected_home = std::env::var_os(child_home_key()).unwrap();
        let expected_config = std::env::var_os(child_config_key()).unwrap();
        let working_directory = std::path::PathBuf::from(&expected_home);
        let mut spec = builtin_registry().get_by_id("claude").unwrap().clone();
        spec.launch.program = std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        spec.launch.fixed_args = vec![
            "--exact".to_owned(),
            "tests::one_shot_child_environment_fixture".to_owned(),
            "--nocapture".to_owned(),
            "--test-threads=1".to_owned(),
        ];
        let binding = spec.capabilities.adapters.one_shot.clone().unwrap();
        let pipe = spec.capabilities.transports.pipe.as_mut().unwrap();
        pipe.adapter = binding.clone();
        pipe.protocol = PipeProtocol::OneShotText;
        pipe.launch_override = Some(spec.launch.clone());
        spec.verification = SpecVerification::Gate4AgentVerified;
        let environment = vec![
            EnvMutation {
                key: OsString::from(CHILD_ENV_FIXTURE_KEY),
                value: Some(OsString::from("true")),
            },
            EnvMutation {
                key: OsString::from(CHILD_ENV_EXPECTED_HOME_KEY),
                value: Some(expected_home),
            },
            EnvMutation {
                key: OsString::from(CHILD_ENV_EXPECTED_CONFIG_KEY),
                value: Some(expected_config),
            },
            EnvMutation {
                key: OsString::from(CHILD_ENV_EXPLICIT_KEY),
                value: Some(OsString::from("explicit-value")),
            },
            EnvMutation {
                key: OsString::from(child_removed_key()),
                value: None,
            },
        ];
        let mut session = NativeOneShotSession::spawn_with_environment(
            &spec,
            &binding,
            "fixture prompt",
            None,
            &working_directory,
            &environment,
        )
        .await
        .unwrap();
        let events = collect(&session).await;
        let output = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        for name in [
            "ambient_custom_absent",
            "ambient_api_absent",
            "ambient_jwt_absent",
            "ambient_proxy_absent",
            "ambient_ssh_absent",
            "bootstrap_absent",
            "path_present",
            "home_present",
            "config_present",
            "explicit_present",
            "explicit_remove",
        ] {
            assert!(output.contains(&format!("{name}=true")), "output: {output}");
        }
        #[cfg(windows)]
        for name in [
            "system_drive_present",
            "system_root_present",
            "windir_present",
            "comspec_present",
        ] {
            assert!(output.contains(&format!("{name}=true")), "output: {output}");
        }
        assert!(!output.contains(CHILD_ENV_SENTINEL_VALUE));
        session.kill().await.unwrap();
    }

    #[tokio::test]
    async fn executes_exact_plan_with_stdin_and_publishes_bounded_semantic_events() {
        #[cfg(windows)]
        let script = "$prompt=[Console]::In.ReadToEnd(); [Console]::Write('answer:' + $prompt)";
        #[cfg(not(windows))]
        let script = "prompt=$(cat); printf 'answer:%s' \"$prompt\"";
        let directory = fixture_dir("success");
        let spec = fixture_spec(script);
        let binding = spec.capabilities.adapters.one_shot.as_ref().unwrap();
        let mut session =
            NativeOneShotSession::spawn(&spec, binding, "fixture prompt", None, &directory)
                .await
                .unwrap();
        let events = collect(&session).await;
        assert!(
            events.iter().any(|event| matches!(
                event,
                AgentEvent::Text { text, .. } if text == "answer:fixture prompt"
            )),
            "events: {events:?}"
        );
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::SessionEnd {
                is_error: false,
                ..
            }
        )));
        session.kill().await.unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn persistent_session_with_launch_override_fails_closed_before_spawn() {
        let mut spec = builtin_registry().get_by_id("codex").unwrap().clone();
        let binding = spec.capabilities.adapters.one_shot.clone().unwrap();
        let pipe = spec.capabilities.transports.pipe.as_mut().unwrap();
        pipe.adapter = binding.clone();
        pipe.protocol = PipeProtocol::OneShotText;
        pipe.launch_override = Some(spec.launch.clone());

        assert!(matches!(
            NativeOneShotSession::spawn_with_environment_and_persistence(
                &spec,
                &binding,
                "prompt",
                None,
                Path::new("."),
                &[],
                OneShotSessionPersistence::Persist,
            )
            .await,
            Err(NativeOneShotError::PersistentLaunchOverrideUnsupported)
        ));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn executes_batch_path_with_spaces() {
        let directory = fixture_dir("batch path");
        let script = directory.join("fixture agent.cmd");
        fs::write(
            &script,
            "@echo off\r\nmore > nul\r\necho batch-path-ok\r\n",
        )
        .unwrap();
        let mut spec = builtin_registry().get_by_id("claude").unwrap().clone();
        spec.launch.program = script.to_string_lossy().into_owned();
        spec.launch.fixed_args.clear();
        let binding = spec.capabilities.adapters.one_shot.clone().unwrap();
        let pipe = spec.capabilities.transports.pipe.as_mut().unwrap();
        pipe.adapter = binding.clone();
        pipe.protocol = PipeProtocol::OneShotText;
        pipe.launch_override = Some(spec.launch.clone());
        spec.verification = SpecVerification::Gate4AgentVerified;

        let mut session =
            NativeOneShotSession::spawn(&spec, &binding, "fixture prompt", None, &directory)
                .await
                .unwrap();
        let events = collect(&session).await;
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::Text { text, .. } if text == "batch-path-ok"
        )));
        session.kill().await.unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn npm_shim_uses_direct_node_process_for_multiline_code_prompt() {
        let directory = fixture_dir("npm shim");
        let entrypoint = directory.join("node_modules/vendor/agent/dist/main.mjs");
        fs::create_dir_all(entrypoint.parent().unwrap()).unwrap();
        fs::write(&entrypoint, "").unwrap();
        let shim = directory.join("agent.cmd");
        fs::write(
            &shim,
            "@ECHO off\r\nGOTO start\r\n:find_dp0\r\nSET dp0=%~dp0\r\nEXIT /b\r\n:start\r\nSETLOCAL\r\nCALL :find_dp0\r\nendLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & \"%_prog%\" \"%dp0%\\node_modules\\vendor\\agent\\dist\\main.mjs\" %*\r\n",
        )
        .unwrap();
        let prompt = "Review:\r\n\"quoted\" 100% ! & | < > ^ Привет";
        let command = native_command(
            shim.to_string_lossy().as_ref(),
            &["-p".to_owned(), prompt.to_owned()],
        )
        .unwrap();
        let command = command.as_std();
        assert_eq!(command.get_program(), "node.exe");
        let args = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(std::path::PathBuf::from(&args[0]), entrypoint);
        assert_eq!(args[1], "-p");
        assert_eq!(args[2], prompt);
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn non_npm_batch_keeps_metacharacter_rejection() {
        let directory = fixture_dir("plain batch");
        let script = directory.join("agent.cmd");
        fs::write(&script, "@ECHO off\r\necho %*\r\n").unwrap();
        let result = native_command(
            script.to_string_lossy().as_ref(),
            &["unsafe & argument".to_owned()],
        );
        assert!(matches!(
            result,
            Err(NativeOneShotError::UnsafeWindowsBatchArguments)
        ));
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn modified_npm_shim_with_required_args_fails_closed() {
        let directory = fixture_dir("modified npm shim");
        let executable = directory.join("node_modules/vendor/agent/agent.exe");
        fs::create_dir_all(executable.parent().unwrap()).unwrap();
        fs::write(&executable, "").unwrap();
        let shim = directory.join("agent.cmd");
        fs::write(
            &shim,
            "@ECHO off\r\nGOTO start\r\n:find_dp0\r\nSET dp0=%~dp0\r\nEXIT /b\r\n:start\r\nSETLOCAL\r\nCALL :find_dp0\r\n\"%dp0%\\node_modules\\vendor\\agent\\agent.exe\" --sandbox %*\r\n",
        )
        .unwrap();
        let result = native_command(
            shim.to_string_lossy().as_ref(),
            &["unsafe & argument".to_owned()],
        );
        assert!(matches!(
            result,
            Err(NativeOneShotError::UnsafeWindowsBatchArguments)
        ));
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn combined_output_limit_terminates_the_process() {
        #[cfg(windows)]
        let script = "$null=[Console]::In.ReadToEnd(); [Console]::Write('x' * 4096)";
        #[cfg(not(windows))]
        let script = "cat >/dev/null; head -c 4096 /dev/zero | tr '\\0' x";
        let directory = fixture_dir("output-limit");
        let spec = fixture_spec(script);
        let binding = spec.capabilities.adapters.one_shot.as_ref().unwrap();
        let mut session = NativeOneShotSession::spawn_with_config(
            &spec,
            binding,
            "prompt",
            None,
            &directory,
            NativeOneShotConfig {
                timeout: Duration::from_secs(5),
                output_max_bytes: 128,
            },
        )
        .await
        .unwrap();
        let events = collect(&session).await;
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::Error { message } if message.contains("output limit")
        )));
        session.kill().await.unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn timeout_terminates_the_process_tree() {
        let directory = fixture_dir("timeout-tree");
        let marker = directory.join("descendant-alive.txt");
        #[cfg(windows)]
        let script = format!(
            "$null=[Console]::In.ReadToEnd(); $child=\"Start-Sleep -Milliseconds 700; Set-Content -LiteralPath '{}' -Value alive\"; Start-Process powershell.exe -WindowStyle Hidden -ArgumentList '-NoProfile','-NonInteractive','-Command',$child; Start-Sleep -Seconds 10",
            marker.display().to_string().replace('\'', "''")
        );
        #[cfg(not(windows))]
        let script = format!(
            "cat >/dev/null; (sleep 1; printf alive > '{}') & sleep 10",
            marker.display().to_string().replace('\'', "'\\''")
        );
        let spec = fixture_spec(&script);
        let binding = spec.capabilities.adapters.one_shot.as_ref().unwrap();
        let mut session = NativeOneShotSession::spawn_with_config(
            &spec,
            binding,
            "prompt",
            None,
            &directory,
            NativeOneShotConfig {
                timeout: Duration::from_millis(100),
                output_max_bytes: 1_024,
            },
        )
        .await
        .unwrap();
        let events = collect(&session).await;
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::Error { message } if message.contains("timed out")
        )));
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        assert!(!marker.exists());
        session.kill().await.unwrap();
        fs::remove_dir_all(directory).unwrap();
    }
}
