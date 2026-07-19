//! Bounded native process authority for provider-owned one-shot plans.

use gate4agent::AgentEvent;
use gate4agent_adapters::{
    resolve_one_shot_plan, OneShotAdapterError, OneShotPlan, ONE_SHOT_OUTPUT_MAX_BYTES,
    ONE_SHOT_TIMEOUT_SECONDS,
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
        Self::spawn_with_config(
            spec,
            binding,
            prompt,
            selection,
            working_directory,
            NativeOneShotConfig::default(),
        )
        .await
    }

    pub async fn spawn_with_config(
        spec: &AgentSpec,
        binding: &AdapterBinding,
        prompt: &str,
        selection: Option<&SessionOptionSelection>,
        working_directory: &Path,
        mut config: NativeOneShotConfig,
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
        let mut plan = resolve_one_shot_plan(&binding.id, &spec.launch, prompt, selection)?;
        if let Some(launch) = &pipe.launch_override {
            plan.program = launch.program.clone();
            plan.args = launch.fixed_args.clone();
            if plan.stdin_payload.is_none() {
                plan.args.push(prompt.to_owned());
            }
        }
        let mut command = native_command(&plan.program, &plan.args)?;
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
            command.args(["/D", "/S", "/C", &resolved]).args(args);
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use gate4agent_catalog::builtin_registry;
    use gate4agent_types::{PipeProtocol, SpecVerification};
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

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
