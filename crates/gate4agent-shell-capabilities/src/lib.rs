//! Native, bounded execution authority for pure capability-probe plans.

use gate4agent_catalog::{
    parse_capability_models_for, resolve_capability_probe_for, AgentSpec,
    CAPABILITY_PROBE_OUTPUT_MAX_BYTES,
};
use gate4agent_types::{CapabilityModelSummary, CapabilityProbeFailure, CapabilityProbeRequest};
use std::collections::HashMap;
use std::future::pending;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::task::JoinHandle;
use tokio::time::{sleep_until, timeout, Instant};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeCapabilityProbeConfig {
    pub timeout: Duration,
    pub output_max_bytes: usize,
}

impl Default for NativeCapabilityProbeConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(60),
            output_max_bytes: CAPABILITY_PROBE_OUTPUT_MAX_BYTES,
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ProbeCacheKey {
    agent_id: String,
    program: String,
    args: Vec<String>,
}

pub struct NativeCapabilityProbeAuthority {
    config: NativeCapabilityProbeConfig,
    cache: HashMap<ProbeCacheKey, Result<Vec<CapabilityModelSummary>, CapabilityProbeFailure>>,
}

impl Default for NativeCapabilityProbeAuthority {
    fn default() -> Self {
        Self::new(NativeCapabilityProbeConfig::default())
    }
}

impl NativeCapabilityProbeAuthority {
    pub fn new(mut config: NativeCapabilityProbeConfig) -> Self {
        config.output_max_bytes = config
            .output_max_bytes
            .clamp(1, CAPABILITY_PROBE_OUTPUT_MAX_BYTES);
        Self {
            config,
            cache: HashMap::new(),
        }
    }

    /// Runs at most once for an agent launch contract in this native host.
    /// Both success and failure are cached to preserve Orca's once-per-host
    /// fallback semantics.
    pub async fn probe(
        &mut self,
        spec: &AgentSpec,
        working_directory: &str,
    ) -> Result<Vec<CapabilityModelSummary>, CapabilityProbeFailure> {
        CapabilityProbeRequest {
            working_directory: working_directory.to_owned(),
        }
        .validate()
        .map_err(|_| CapabilityProbeFailure::AuthorityRejected)?;
        let plan = resolve_capability_probe_for(spec)
            .map_err(|_| CapabilityProbeFailure::AuthorityRejected)?;
        let key = ProbeCacheKey {
            agent_id: spec.id.to_string(),
            program: plan.program.clone(),
            args: plan.args.clone(),
        };
        if let Some(cached) = self.cache.get(&key) {
            return cached.clone();
        }
        let result = execute_probe(
            spec,
            &plan.program,
            &plan.args,
            working_directory,
            self.config,
        )
        .await;
        self.cache.insert(key, result.clone());
        result
    }
}

async fn execute_probe(
    spec: &AgentSpec,
    program: &str,
    args: &[String],
    working_directory: &str,
    config: NativeCapabilityProbeConfig,
) -> Result<Vec<CapabilityModelSummary>, CapabilityProbeFailure> {
    let mut command = native_command(program, args);
    command
        .current_dir(working_directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    configure_process_group(&mut command);
    let mut child = command
        .spawn()
        .map_err(|_| CapabilityProbeFailure::SpawnUnavailable)?;
    let process_id = child.id().ok_or(CapabilityProbeFailure::SpawnUnavailable)?;
    let stdout = child
        .stdout
        .take()
        .ok_or(CapabilityProbeFailure::SpawnUnavailable)?;
    let stderr = child
        .stderr
        .take()
        .ok_or(CapabilityProbeFailure::SpawnUnavailable)?;
    let total = Arc::new(AtomicUsize::new(0));
    let mut stdout_task = Some(tokio::spawn(read_bounded(
        stdout,
        Arc::clone(&total),
        config.output_max_bytes,
    )));
    let mut stderr_task = Some(tokio::spawn(read_bounded(
        stderr,
        total,
        config.output_max_bytes,
    )));
    let mut wait_task = Some(tokio::spawn(async move { child.wait().await }));
    let deadline = Instant::now() + config.timeout;
    let mut stdout_bytes = None;
    let mut stderr_bytes = None;
    let mut status = None;

    while stdout_bytes.is_none() || stderr_bytes.is_none() || status.is_none() {
        tokio::select! {
            _ = sleep_until(deadline) => {
                terminate_process_tree(process_id).await;
                abort_tasks(&mut wait_task, &mut stdout_task, &mut stderr_task);
                return Err(CapabilityProbeFailure::TimedOut);
            }
            result = join_optional(&mut stdout_task), if stdout_task.is_some() => {
                stdout_task = None;
                stdout_bytes = Some(resolve_reader(result, process_id, &mut wait_task, &mut stderr_task).await?);
            }
            result = join_optional(&mut stderr_task), if stderr_task.is_some() => {
                stderr_task = None;
                stderr_bytes = Some(resolve_reader(result, process_id, &mut wait_task, &mut stdout_task).await?);
            }
            result = join_optional(&mut wait_task), if wait_task.is_some() => {
                wait_task = None;
                status = Some(result
                    .map_err(|_| CapabilityProbeFailure::ExecutorUnavailable)?
                    .map_err(|_| CapabilityProbeFailure::SpawnUnavailable)?);
            }
        }
    }

    let status = status.expect("wait task completed");
    if !status.success() {
        return Err(CapabilityProbeFailure::NonZeroExit {
            exit_code: status.code(),
        });
    }
    let stdout = String::from_utf8_lossy(stdout_bytes.as_deref().unwrap_or_default());
    parse_capability_models_for(spec, &stdout)
        .map_err(|_| CapabilityProbeFailure::AuthorityRejected)
}

async fn join_optional<T>(task: &mut Option<JoinHandle<T>>) -> Result<T, tokio::task::JoinError> {
    match task.as_mut() {
        Some(task) => task.await,
        None => pending().await,
    }
}

async fn resolve_reader(
    result: Result<Result<Vec<u8>, CapabilityProbeFailure>, tokio::task::JoinError>,
    process_id: u32,
    wait_task: &mut Option<JoinHandle<std::io::Result<std::process::ExitStatus>>>,
    peer_task: &mut Option<JoinHandle<Result<Vec<u8>, CapabilityProbeFailure>>>,
) -> Result<Vec<u8>, CapabilityProbeFailure> {
    match result {
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(failure)) => {
            terminate_process_tree(process_id).await;
            if let Some(task) = wait_task.take() {
                task.abort();
            }
            if let Some(task) = peer_task.take() {
                task.abort();
            }
            Err(failure)
        }
        Err(_) => {
            terminate_process_tree(process_id).await;
            if let Some(task) = wait_task.take() {
                task.abort();
            }
            if let Some(task) = peer_task.take() {
                task.abort();
            }
            Err(CapabilityProbeFailure::ExecutorUnavailable)
        }
    }
}

async fn read_bounded(
    mut reader: impl AsyncRead + Unpin,
    total: Arc<AtomicUsize>,
    limit: usize,
) -> Result<Vec<u8>, CapabilityProbeFailure> {
    let mut output = Vec::new();
    let mut chunk = [0_u8; 8 * 1024];
    loop {
        let read = reader
            .read(&mut chunk)
            .await
            .map_err(|_| CapabilityProbeFailure::ExecutorUnavailable)?;
        if read == 0 {
            return Ok(output);
        }
        let previous = total.fetch_add(read, Ordering::AcqRel);
        if previous > limit || read > limit.saturating_sub(previous) {
            return Err(CapabilityProbeFailure::OutputLimitExceeded);
        }
        output.extend_from_slice(&chunk[..read]);
    }
}

fn abort_tasks(
    wait_task: &mut Option<JoinHandle<std::io::Result<std::process::ExitStatus>>>,
    stdout_task: &mut Option<JoinHandle<Result<Vec<u8>, CapabilityProbeFailure>>>,
    stderr_task: &mut Option<JoinHandle<Result<Vec<u8>, CapabilityProbeFailure>>>,
) {
    for task in [
        wait_task.take().map(|task| task.abort_handle()),
        stdout_task.take().map(|task| task.abort_handle()),
        stderr_task.take().map(|task| task.abort_handle()),
    ]
    .into_iter()
    .flatten()
    {
        task.abort();
    }
}

fn native_command(program: &str, args: &[String]) -> Command {
    #[cfg(windows)]
    if program.ends_with(".cmd") || program.ends_with(".bat") {
        let mut command = Command::new("cmd.exe");
        command.args(["/D", "/S", "/C", program]).args(args);
        return command;
    }
    let mut command = Command::new(program);
    command.args(args);
    command
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

#[cfg(test)]
mod tests {
    use super::*;
    use gate4agent_catalog::builtin_registry;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn fixture_dir(name: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "gate4agent-capability-{name}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        path
    }

    #[tokio::test]
    async fn structured_override_is_executed_once_and_cached_per_host() {
        let directory = fixture_dir("cache");
        let counter = directory.join("count.txt");
        let script_path = directory.join(if cfg!(windows) {
            "probe.cmd"
        } else {
            "probe.sh"
        });
        #[cfg(windows)]
        let script = format!(
            "@echo off\r\n>>\"{}\" echo x\r\necho auto - Auto ^(default^)\r\necho gpt-5.3-codex - GPT-5.3 Codex\r\n",
            counter.display()
        );
        #[cfg(not(windows))]
        let script = format!(
            "printf x >> '{}'; printf 'auto - Auto (default)\\ngpt-5.3-codex - GPT-5.3 Codex\\n'",
            counter.display().to_string().replace('\'', "'\\''")
        );
        fs::write(&script_path, script).unwrap();

        let mut spec = builtin_registry().get_by_id("cursor").unwrap().clone();
        #[cfg(windows)]
        {
            spec.launch.program = script_path.display().to_string();
            spec.launch.fixed_args = Vec::new();
        }
        #[cfg(not(windows))]
        {
            spec.launch.program = "sh".to_owned();
            spec.launch.fixed_args = vec![script_path.display().to_string()];
        }
        let mut authority = NativeCapabilityProbeAuthority::default();
        let first = authority
            .probe(&spec, directory.to_str().unwrap())
            .await
            .unwrap();
        let second = authority
            .probe(&spec, directory.to_str().unwrap())
            .await
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(first.len(), 2);
        assert_eq!(fs::read_to_string(&counter).unwrap().lines().count(), 1);
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn combined_output_limit_fails_typed() {
        let directory = fixture_dir("limit");
        let script_path = directory.join(if cfg!(windows) {
            "probe.ps1"
        } else {
            "probe.sh"
        });
        #[cfg(windows)]
        let script = "[Console]::Write('auto - A label that exceeds the configured limit')";
        #[cfg(not(windows))]
        let script = "printf 'auto - A label that exceeds the configured limit'";
        fs::write(&script_path, script).unwrap();
        let mut spec = builtin_registry().get_by_id("cursor").unwrap().clone();
        #[cfg(windows)]
        {
            spec.launch.program = "powershell.exe".to_owned();
            spec.launch.fixed_args = vec![
                "-NoProfile".to_owned(),
                "-NonInteractive".to_owned(),
                "-File".to_owned(),
                script_path.display().to_string(),
            ];
        }
        #[cfg(not(windows))]
        {
            spec.launch.program = "sh".to_owned();
            spec.launch.fixed_args = vec![script_path.display().to_string()];
        }
        let mut authority = NativeCapabilityProbeAuthority::new(NativeCapabilityProbeConfig {
            timeout: Duration::from_secs(5),
            output_max_bytes: 16,
        });
        assert_eq!(
            authority.probe(&spec, directory.to_str().unwrap()).await,
            Err(CapabilityProbeFailure::OutputLimitExceeded)
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn timeout_fails_typed_and_terminates_the_probe_process() {
        let directory = fixture_dir("timeout");
        let script_path = directory.join(if cfg!(windows) {
            "probe.cmd"
        } else {
            "probe.sh"
        });
        #[cfg(windows)]
        let script = "@echo off\r\ntimeout /T 10 /NOBREAK >NUL\r\n";
        #[cfg(not(windows))]
        let script = "sleep 10";
        fs::write(&script_path, script).unwrap();
        let mut spec = builtin_registry().get_by_id("cursor").unwrap().clone();
        #[cfg(windows)]
        {
            spec.launch.program = script_path.display().to_string();
            spec.launch.fixed_args = Vec::new();
        }
        #[cfg(not(windows))]
        {
            spec.launch.program = "sh".to_owned();
            spec.launch.fixed_args = vec![script_path.display().to_string()];
        }
        let mut authority = NativeCapabilityProbeAuthority::new(NativeCapabilityProbeConfig {
            timeout: Duration::from_millis(50),
            output_max_bytes: 1_024,
        });
        assert_eq!(
            authority.probe(&spec, directory.to_str().unwrap()).await,
            Err(CapabilityProbeFailure::TimedOut)
        );
        fs::remove_dir_all(directory).unwrap();
    }
}
