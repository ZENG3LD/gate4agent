//! PTY wrapper for cross-platform terminal emulation.

use gate4agent_pty::{native_pty_system, Child, ChildKiller, CommandBuilder, MasterPty, PtySize};
use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::TryRecvError;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use thiserror::Error;

use crate::agent::{AgentId, AgentSpec, LaunchPlan};
use crate::child_environment::platform_minimal_child_environment;
#[cfg(test)]
use crate::child_environment::{
    is_platform_child_environment_key, platform_minimal_child_environment_from,
};
use crate::core::types::CliTool;
use crate::pty::os_process::{
    observe_pty_foreground, PtyForegroundObservation, PtyProcessProbeError,
};

pub const PTY_OUTPUT_HIGH_WATER_BYTES: usize = 256 * 1024;
pub const PTY_OUTPUT_LOW_WATER_BYTES: usize = 32 * 1024;
const PTY_TERM: &str = "xterm-256color";

/// Start each PTY child from a small machine-local runtime environment.
/// Provider credentials, control-plane tokens, SSH agents, proxy credentials,
/// and unrelated daemon state are not inherited implicitly. A launch plan may
/// still set or remove any variable explicitly after this baseline is built.
#[cfg(test)]
fn isolate_pty_child_environment_from<I, K, V>(command: &mut CommandBuilder, inherited: I)
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    command.env_clear();
    for (key, value) in platform_minimal_child_environment_from(inherited) {
        command.env(key, value);
    }
    command.env("TERM", PTY_TERM);
}

fn isolate_pty_child_environment(command: &mut CommandBuilder) {
    command.env_clear();
    for (key, value) in platform_minimal_child_environment() {
        command.env(key, value);
    }
    command.env("TERM", PTY_TERM);
}

/// Module-internal PTY errors. Converted to `AgentError` at the `PtySession` boundary.
#[derive(Error, Debug)]
pub enum PtyError {
    #[error("Failed to create PTY: {0}")]
    CreateFailed(String),
    #[error("Failed to spawn process: {0}")]
    SpawnFailed(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("PTY error: {0}")]
    Pty(String),
    #[error("Windows command wrapper argument {index} contains shell metacharacters")]
    UnsafeWindowsCommandArgument { index: usize },
    #[error("Windows PTY working directory cannot be a UNC path: {path}")]
    UnsupportedWindowsUncWorkingDirectory { path: String },
    #[error("PTY OS reader thread did not join within {timeout_ms}ms")]
    ReaderJoinTimedOut { timeout_ms: u64 },
    #[error("PTY OS reader thread panicked")]
    ReaderPanicked,
}

/// PTY wrapper for managing terminal processes.
pub struct PtyWrapper {
    /// Master PTY for I/O.
    master: Option<Box<dyn MasterPty + Send>>,
    /// Child process.
    child: Box<dyn Child + Send + Sync>,
    root_pid: Option<u32>,
    /// Writer for sending input.
    writer: Option<Box<dyn Write + Send>>,
    /// Receiver for output.
    output_rx: PtyReadReceiver,
    reader_thread: Option<thread::JoinHandle<()>>,
    /// Stable agent identity. Legacy callers can also recover `CliTool`.
    agent_id: AgentId,
    legacy_tool: Option<CliTool>,
    exit_code: Option<u32>,
}

pub(crate) enum PtyReadEvent {
    Output(Vec<u8>),
    Eof,
    Error(String),
}

struct PtyReadQueueState {
    events: VecDeque<PtyReadEvent>,
    queued_bytes: usize,
    backpressured: bool,
    producer_closed: bool,
    consumer_closed: bool,
}

struct PtyReadQueue {
    state: Mutex<PtyReadQueueState>,
    changed: Condvar,
}

struct PtyReadSender {
    queue: Arc<PtyReadQueue>,
}

struct PtyReadReceiver {
    queue: Arc<PtyReadQueue>,
}

fn pty_read_queue() -> (PtyReadSender, PtyReadReceiver) {
    let queue = Arc::new(PtyReadQueue {
        state: Mutex::new(PtyReadQueueState {
            events: VecDeque::new(),
            queued_bytes: 0,
            backpressured: false,
            producer_closed: false,
            consumer_closed: false,
        }),
        changed: Condvar::new(),
    });
    (
        PtyReadSender {
            queue: queue.clone(),
        },
        PtyReadReceiver { queue },
    )
}

impl PtyReadSender {
    fn send(&self, event: PtyReadEvent) -> Result<(), ()> {
        let event_bytes = match &event {
            PtyReadEvent::Output(data) => data.len(),
            PtyReadEvent::Eof | PtyReadEvent::Error(_) => 0,
        };
        let mut state = self.queue.state.lock().map_err(|_| ())?;
        while event_bytes > 0
            && (state.backpressured
                || state.queued_bytes.saturating_add(event_bytes) > PTY_OUTPUT_HIGH_WATER_BYTES)
            && !state.consumer_closed
        {
            state.backpressured = true;
            state = self.queue.changed.wait(state).map_err(|_| ())?;
        }
        if state.consumer_closed {
            return Err(());
        }
        state.queued_bytes = state.queued_bytes.saturating_add(event_bytes);
        state.events.push_back(event);
        self.queue.changed.notify_all();
        Ok(())
    }
}

impl Drop for PtyReadSender {
    fn drop(&mut self) {
        if let Ok(mut state) = self.queue.state.lock() {
            state.producer_closed = true;
            self.queue.changed.notify_all();
        }
    }
}

impl PtyReadReceiver {
    fn try_recv(&self) -> Result<PtyReadEvent, TryRecvError> {
        let mut state = self
            .queue
            .state
            .lock()
            .map_err(|_| TryRecvError::Disconnected)?;
        let Some(event) = state.events.pop_front() else {
            return if state.producer_closed {
                Err(TryRecvError::Disconnected)
            } else {
                Err(TryRecvError::Empty)
            };
        };
        if let PtyReadEvent::Output(data) = &event {
            state.queued_bytes = state.queued_bytes.saturating_sub(data.len());
            if state.backpressured && state.queued_bytes <= PTY_OUTPUT_LOW_WATER_BYTES {
                state.backpressured = false;
                self.queue.changed.notify_all();
            }
        }
        Ok(event)
    }

    /// Atomically stop the producer only when every event accepted so far has
    /// been consumed. This closes the race between an empty poll and exit
    /// publication without discarding an already queued output chunk.
    fn close_if_empty(&self) -> bool {
        let Ok(mut state) = self.queue.state.lock() else {
            return false;
        };
        if !state.events.is_empty() {
            return false;
        }
        state.consumer_closed = true;
        state.backpressured = false;
        self.queue.changed.notify_all();
        true
    }
}

impl Drop for PtyReadReceiver {
    fn drop(&mut self) {
        if let Ok(mut state) = self.queue.state.lock() {
            state.consumer_closed = true;
            self.queue.changed.notify_all();
        }
    }
}

impl PtyWrapper {
    /// Create a new PTY wrapper with configurable size.
    pub fn new(
        tool: CliTool,
        working_dir: &std::path::Path,
        rows: u16,
        cols: u16,
    ) -> Result<Self, PtyError> {
        Self::new_with_env(tool, working_dir, &[], rows, cols)
    }

    /// Create a new PTY wrapper at the standard 24x80 size.
    ///
    /// Convenience constructor for interactive TUI use where the exact size doesn't matter.
    pub fn new_compact(tool: CliTool, working_dir: &std::path::Path) -> Result<Self, PtyError> {
        Self::new(tool, working_dir, 24, 80)
    }

    /// Create a new PTY wrapper with custom environment variables and size.
    pub fn new_with_env(
        tool: CliTool,
        working_dir: &std::path::Path,
        env_vars: &[(String, String)],
        rows: u16,
        cols: u16,
    ) -> Result<Self, PtyError> {
        let cmd = Self::build_command(tool, working_dir, env_vars)?;
        Self::spawn_command(AgentId::from(tool), Some(tool), cmd, rows, cols)
    }

    /// Spawn an arbitrary registered agent from a shell-free launch plan.
    pub fn from_launch_plan(
        plan: LaunchPlan,
        legacy_tool: Option<CliTool>,
        rows: u16,
        cols: u16,
    ) -> Result<Self, PtyError> {
        let agent_id = plan.agent_id.clone();
        let command = Self::build_launch_plan_command(&plan)?;
        Self::spawn_command(agent_id, legacy_tool, command, rows, cols)
    }

    fn spawn_command(
        agent_id: AgentId,
        legacy_tool: Option<CliTool>,
        cmd: CommandBuilder,
        rows: u16,
        cols: u16,
    ) -> Result<Self, PtyError> {
        let pty_system = native_pty_system();

        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| PtyError::CreateFailed(e.to_string()))?;

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| PtyError::SpawnFailed(e.to_string()))?;

        let root_pid = child.process_id();

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| PtyError::Pty(e.to_string()))?;

        let writer = pair
            .master
            .take_writer()
            .map_err(|e| PtyError::Pty(e.to_string()))?;

        // Spawn reader thread. Byte-based high/low watermarks keep memory
        // bounded independently of read() chunking and avoid rapid pause/resume
        // oscillation while the consumer catches up.
        let (tx, rx) = pty_read_queue();
        let reader_thread = thread::spawn(move || {
            Self::reader_thread(reader, tx);
        });

        Ok(Self {
            master: Some(pair.master),
            child,
            root_pid,
            writer: Some(writer),
            output_rx: rx,
            reader_thread: Some(reader_thread),
            agent_id,
            legacy_tool,
            exit_code: None,
        })
    }

    fn build_command(
        tool: CliTool,
        working_dir: &std::path::Path,
        env_vars: &[(String, String)],
    ) -> Result<CommandBuilder, PtyError> {
        let mut cmd = if cfg!(windows) {
            let tool_name = match tool {
                CliTool::ClaudeCode => "claude",
                CliTool::Codex => "codex",
                CliTool::KimiCode => "kimi",
                CliTool::Gemini => "gemini",
                // OpenCode PTY integration will be added once
                // its invocation shape is confirmed via live capture.
                CliTool::OpenCode => "opencode",
            };
            let mut c = CommandBuilder::new("cmd");
            c.args(["/Q", "/K", tool_name]);
            c
        } else {
            // On Unix, use the CLI directly
            match tool {
                CliTool::ClaudeCode => CommandBuilder::new("claude"),
                CliTool::Codex => CommandBuilder::new("codex"),
                CliTool::KimiCode => CommandBuilder::new("kimi"),
                CliTool::Gemini => CommandBuilder::new("gemini"),
                // OpenCode PTY integration will be added once
                // its invocation shape is confirmed via live capture.
                CliTool::OpenCode => CommandBuilder::new("opencode"),
            }
        };

        cmd.cwd(validated_windows_child_working_directory(working_dir)?);

        isolate_pty_child_environment(&mut cmd);

        // Add custom env vars (these override inherited values if keys match)
        for (key, value) in env_vars {
            cmd.env(key, value);
        }

        Ok(cmd)
    }

    fn build_launch_plan_command(plan: &LaunchPlan) -> Result<CommandBuilder, PtyError> {
        let mut command = if cfg!(windows) {
            if let Some(wrapper) = windows_command_wrapper(&plan.program) {
                for (index, argument) in plan.args.iter().enumerate() {
                    if !is_safe_windows_wrapper_argument(argument) {
                        return Err(PtyError::UnsafeWindowsCommandArgument { index });
                    }
                }
                let mut command = CommandBuilder::new("cmd.exe");
                command.args(["/D", "/Q", windows_wrapper_mode(&plan.agent_id)]);
                command.arg(wrapper);
                command.args(&plan.args);
                command
            } else {
                let mut argv = Vec::with_capacity(plan.args.len() + 1);
                argv.push(plan.program.clone());
                argv.extend(plan.args.iter().cloned());
                CommandBuilder::from_argv(argv)
            }
        } else {
            let mut argv = Vec::with_capacity(plan.args.len() + 1);
            argv.push(plan.program.clone());
            argv.extend(plan.args.iter().cloned());
            CommandBuilder::from_argv(argv)
        };

        command.cwd(validated_windows_child_working_directory(&plan.working_dir)?);
        isolate_pty_child_environment(&mut command);
        for mutation in &plan.env {
            if let Some(value) = &mutation.value {
                command.env(&mutation.key, value);
            } else {
                command.env_remove(&mutation.key);
            }
        }
        Ok(command)
    }

    fn reader_thread(reader: Box<dyn std::io::Read + Send>, tx: PtyReadSender) {
        use std::io::Read;

        let mut reader = reader;
        let mut buffer = [0u8; 4096];

        loop {
            match reader.read(&mut buffer) {
                Ok(0) => {
                    let _ = tx.send(PtyReadEvent::Eof);
                    break;
                }
                Ok(n) => {
                    // Send raw bytes — never lose UTF-8 to from_utf8_lossy.
                    // vt100::Parser accepts &[u8] and handles multi-byte UTF-8
                    // correctly even if a sequence is split across read() calls.
                    if tx.send(PtyReadEvent::Output(buffer[..n].to_vec())).is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) => {
                    let _ = tx.send(PtyReadEvent::Error(error.to_string()));
                    break;
                }
            }
        }
    }

    /// Send input to the terminal.
    pub fn write(&mut self, data: &str) -> Result<(), PtyError> {
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| PtyError::Pty("PTY writer is closed".to_owned()))?;
        writer.write_all(data.as_bytes())?;
        writer.flush()?;
        Ok(())
    }

    /// Write raw bytes to the terminal.
    pub fn write_bytes(&mut self, data: &[u8]) -> Result<(), PtyError> {
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| PtyError::Pty("PTY writer is closed".to_owned()))?;
        writer.write_all(data)?;
        writer.flush()?;
        Ok(())
    }

    /// Send a line of input (with newline).
    pub fn writeln(&mut self, data: &str) -> Result<(), PtyError> {
        self.write(&format!("{}\n", data))
    }

    /// Try to receive output bytes (non-blocking).
    pub fn try_recv(&self) -> Option<Vec<u8>> {
        match self.output_rx.try_recv().ok()? {
            PtyReadEvent::Output(data) => Some(data),
            PtyReadEvent::Eof | PtyReadEvent::Error(_) => None,
        }
    }

    /// Try to receive output while preserving the distinction between an empty
    /// queue and the reader reaching EOF.
    pub(crate) fn try_recv_result(&self) -> Result<PtyReadEvent, TryRecvError> {
        self.output_rx.try_recv()
    }

    pub(crate) fn close_output_if_empty(&self) -> bool {
        self.output_rx.close_if_empty()
    }

    /// Resize the terminal.
    pub fn resize(&self, rows: u16, cols: u16) -> Result<(), PtyError> {
        self.master
            .as_ref()
            .ok_or_else(|| PtyError::Pty("PTY master is closed".to_owned()))?
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| PtyError::Pty(e.to_string()))
    }

    /// Close ConPTY/POSIX master handles and join the OS reader thread.
    /// This is separate from the Tokio reader task, which consumes the bounded
    /// queue and must finish first.
    pub(crate) fn close_and_join_reader(&mut self, timeout: Duration) -> Result<(), PtyError> {
        self.writer.take();
        self.master.take();
        let Some(handle) = self.reader_thread.take() else {
            return Ok(());
        };
        let deadline = Instant::now() + timeout;
        while !handle.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        if !handle.is_finished() {
            self.reader_thread = Some(handle);
            return Err(PtyError::ReaderJoinTimedOut {
                timeout_ms: timeout.as_millis().min(u128::from(u64::MAX)) as u64,
            });
        }
        handle.join().map_err(|_| PtyError::ReaderPanicked)
    }

    /// Check if the process is still running.
    pub fn is_running(&mut self) -> bool {
        self.try_exit_code().is_none()
    }

    /// Kill the process.
    pub fn kill(&mut self) -> Result<(), PtyError> {
        match self.child.kill() {
            Ok(()) => Ok(()),
            #[cfg(windows)]
            Err(error) if error.raw_os_error() == Some(0) => Ok(()),
            Err(error) => Err(PtyError::Pty(error.to_string())),
        }
    }

    /// Wait for the process to exit.
    pub fn wait(&mut self) -> Option<u32> {
        if let Some(code) = self.exit_code {
            return Some(code);
        }
        self.exit_code = self.child.wait().ok().map(|status| status.exit_code());
        self.exit_code
    }

    /// Poll and retain the exit code so later output-drain logic cannot lose it.
    pub(crate) fn try_exit_code(&mut self) -> Option<u32> {
        if self.exit_code.is_none() {
            self.exit_code = self
                .child
                .try_wait()
                .ok()
                .flatten()
                .map(|status| status.exit_code());
        }
        self.exit_code
    }

    pub fn agent_id(&self) -> &AgentId {
        &self.agent_id
    }

    pub fn root_pid(&self) -> Option<u32> {
        self.root_pid
    }

    pub(crate) fn observe_foreground(
        &self,
        spec: &AgentSpec,
    ) -> Result<PtyForegroundObservation, PtyProcessProbeError> {
        #[cfg(unix)]
        let foreground_pgid = self
            .master
            .as_ref()
            .and_then(|master| master.process_group_leader())
            .and_then(|pid| u32::try_from(pid).ok());
        #[cfg(not(unix))]
        let foreground_pgid = None;

        observe_pty_foreground(self.root_pid, foreground_pgid, spec)
    }

    /// Clone a termination-only handle that does not contend with PTY I/O.
    pub(crate) fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        self.child.clone_killer()
    }

    /// Get the legacy tool when this PTY was created through the four-tool API.
    pub fn tool(&self) -> Option<CliTool> {
        self.legacy_tool
    }
}

impl Drop for PtyWrapper {
    fn drop(&mut self) {
        let _ = self.close_and_join_reader(Duration::from_millis(250));
    }
}

fn windows_command_wrapper(program: &OsStr) -> Option<OsString> {
    if !cfg!(windows) {
        return None;
    }
    let path = Path::new(program);
    let extension = path
        .extension()
        .and_then(OsStr::to_str)
        .map(str::to_ascii_lowercase);
    if matches!(extension.as_deref(), Some("cmd" | "bat")) {
        return Some(program.to_owned());
    }
    if extension.is_some() || path.components().count() > 1 {
        return None;
    }
    let wrapper_name = format!("{}.cmd", program.to_string_lossy());
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|directory| directory.join(&wrapper_name))
            .find(|candidate| candidate.is_file())
            .map(PathBuf::into_os_string)
    })
}

fn windows_wrapper_mode(_agent_id: &AgentId) -> &'static str {
    "/C"
}

fn windows_child_working_directory(path: &Path) -> PathBuf {
    if !cfg!(windows) {
        return path.to_owned();
    }
    let value = path.as_os_str().to_string_lossy();
    if let Some(rest) = value.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = value.strip_prefix(r"\\?\") {
        let bytes = rest.as_bytes();
        if bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'\\' | b'/')
        {
            return PathBuf::from(rest);
        }
    }
    path.to_owned()
}

fn validated_windows_child_working_directory(path: &Path) -> Result<PathBuf, PtyError> {
    let normalized = windows_child_working_directory(path);
    if cfg!(windows) && normalized.as_os_str().to_string_lossy().starts_with(r"\\") {
        return Err(PtyError::UnsupportedWindowsUncWorkingDirectory {
            path: normalized.to_string_lossy().into_owned(),
        });
    }
    Ok(normalized)
}

fn is_safe_windows_wrapper_argument(argument: &OsStr) -> bool {
    let value = argument.to_string_lossy();
    !value.chars().any(|character| {
        matches!(
            character,
            '\0' | '\r' | '\n' | '"' | '%' | '!' | '^' | '&' | '|' | '<' | '>' | '(' | ')'
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHILD_ENV_BOOTSTRAP_KEY: &str = "GATE4AGENT_PTY_CHILD_ENV_BOOTSTRAP";
    const CHILD_ENV_BOOTSTRAP_REPORT_KEY: &str = "GATE4AGENT_PTY_CHILD_ENV_BOOTSTRAP_REPORT";
    const CHILD_ENV_REPORT_KEY: &str = "GATE4AGENT_PTY_CHILD_ENV_REPORT";
    const CHILD_ENV_EXPECTED_HOME_KEY: &str = "GATE4AGENT_PTY_CHILD_ENV_EXPECTED_HOME";
    const CHILD_ENV_EXPECTED_CONFIG_KEY: &str = "GATE4AGENT_PTY_CHILD_ENV_EXPECTED_CONFIG";
    const CHILD_ENV_EXPLICIT_KEY: &str = "GATE4AGENT_PTY_CHILD_ENV_EXPLICIT";
    const CHILD_ENV_SECRET_KEY: &str = "GATE4AGENT_PTY_CHILD_ENV_FAKE_JWT";

    struct PtyChildEnvTestRoot(PathBuf);

    impl Drop for PtyChildEnvTestRoot {
        fn drop(&mut self) {
            if self
                .0
                .file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| name.starts_with("gate4agent-pty-child-env-"))
            {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
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

    #[test]
    fn isolated_environment_keeps_only_machine_runtime_and_terminal_keys() {
        let mut command = CommandBuilder::new("fixture-program");
        command.env(CHILD_ENV_SECRET_KEY, "ambient-secret-that-must-be-cleared");

        #[cfg(windows)]
        let inherited = vec![
            (OsString::from("SystemDrive"), OsString::from(r"C:")),
            (OsString::from("Path"), OsString::from(r"C:\runtime\bin")),
            (OsString::from("HOME"), OsString::from(r"C:\Users\fixture")),
            (OsString::from("USERPROFILE"), OsString::from(r"C:\Users\fixture")),
            (OsString::from("APPDATA"), OsString::from(r"C:\Users\fixture\AppData\Roaming")),
            (OsString::from("OPENAI_API_KEY"), OsString::from("fake-api-key")),
            (OsString::from("HTTPS_PROXY"), OsString::from("http://fake-auth@proxy.invalid")),
        ];
        #[cfg(not(windows))]
        let inherited = vec![
            (OsString::from("PATH"), OsString::from("/usr/local/bin:/usr/bin:/bin")),
            (OsString::from("HOME"), OsString::from("/home/fixture")),
            (OsString::from("XDG_CONFIG_HOME"), OsString::from("/home/fixture/.config")),
            (OsString::from("LC_MESSAGES"), OsString::from("C.UTF-8")),
            (OsString::from("OPENAI_API_KEY"), OsString::from("fake-api-key")),
            (OsString::from("HTTPS_PROXY"), OsString::from("http://fake-auth@proxy.invalid")),
        ];

        isolate_pty_child_environment_from(&mut command, inherited);

        assert!(command.get_env(CHILD_ENV_SECRET_KEY).is_none());
        assert!(command.get_env("OPENAI_API_KEY").is_none());
        assert!(command.get_env("HTTPS_PROXY").is_none());
        assert!(command.get_env("PATH").is_some());
        assert!(command.get_env("HOME").is_some());
        assert!(command.get_env(child_home_key()).is_some());
        assert!(command.get_env(child_config_key()).is_some());
        #[cfg(windows)]
        {
            assert_eq!(command.get_env("SystemDrive"), Some(OsStr::new(r"C:")));
            assert!(command.iter_full_env_as_str().all(|(_, value)| {
                !value
                    .to_ascii_lowercase()
                    .contains("%systemdrive%")
            }));
        }
        #[cfg(not(windows))]
        assert_eq!(command.get_env("LC_MESSAGES"), Some(OsStr::new("C.UTF-8")));
        assert_eq!(command.get_env("TERM"), Some(OsStr::new(PTY_TERM)));
    }

    #[test]
    fn launch_plan_environment_mutations_are_authoritative_after_isolation() {
        let current_exe = std::env::current_exe().unwrap();
        let working_dir = std::env::current_dir().unwrap();
        let removed_key = child_removed_key();
        let plan = LaunchPlan {
            agent_id: AgentId::new("codex").unwrap(),
            program: current_exe.as_os_str().to_owned(),
            args: vec![OsString::from("--list")],
            working_dir: working_dir.clone(),
            env: vec![
                crate::agent::EnvMutation {
                    key: OsString::from(CHILD_ENV_EXPLICIT_KEY),
                    value: Some(OsString::from("explicit-value")),
                },
                crate::agent::EnvMutation {
                    key: OsString::from(removed_key),
                    value: Some(OsString::from("remove-this-value")),
                },
                crate::agent::EnvMutation {
                    key: OsString::from(removed_key),
                    value: None,
                },
            ],
            followup_prompt: None,
            followup_draft: None,
            applied_session_options: None,
        };

        let command = PtyWrapper::build_launch_plan_command(&plan).unwrap();

        assert_eq!(command.get_argv().first(), Some(&plan.program));
        assert_eq!(command.get_cwd(), Some(&working_dir.into_os_string()));
        assert_eq!(
            command.get_env(CHILD_ENV_EXPLICIT_KEY),
            Some(OsStr::new("explicit-value"))
        );
        assert!(command.get_env(removed_key).is_none());
    }

    #[test]
    fn legacy_tool_command_keeps_runtime_lookup_and_explicit_environment() {
        let working_dir = std::env::current_dir().unwrap();
        let command = PtyWrapper::build_command(
            CliTool::Codex,
            &working_dir,
            &[(
                CHILD_ENV_EXPLICIT_KEY.to_owned(),
                "legacy-explicit-value".to_owned(),
            )],
        )
        .unwrap();

        #[cfg(windows)]
        assert_eq!(
            command.get_argv(),
            &vec![
                OsString::from("cmd"),
                OsString::from("/Q"),
                OsString::from("/K"),
                OsString::from("codex"),
            ]
        );
        #[cfg(not(windows))]
        assert_eq!(command.get_argv(), &vec![OsString::from("codex")]);
        assert_eq!(command.get_cwd(), Some(&working_dir.into_os_string()));
        assert_eq!(command.get_env("PATH"), std::env::var_os("PATH").as_deref());
        assert_eq!(
            command.get_env(CHILD_ENV_EXPLICIT_KEY),
            Some(OsStr::new("legacy-explicit-value"))
        );
        assert_eq!(command.get_env("TERM"), Some(OsStr::new(PTY_TERM)));
    }

    #[test]
    fn pty_child_environment_fixture() {
        let Some(report_path) = std::env::var_os(CHILD_ENV_REPORT_KEY) else {
            return;
        };
        let expected_home = std::env::var_os(CHILD_ENV_EXPECTED_HOME_KEY).unwrap();
        let expected_config = std::env::var_os(CHILD_ENV_EXPECTED_CONFIG_KEY).unwrap();
        let checks = vec![
            std::env::var_os(CHILD_ENV_SECRET_KEY).is_none(),
            std::env::var_os("ANTHROPIC_API_KEY").is_none(),
            std::env::var_os("OPENAI_API_KEY").is_none(),
            std::env::var_os("SSH_AUTH_SOCK").is_none(),
            std::env::var_os("HTTPS_PROXY").is_none(),
            std::env::var_os(CHILD_ENV_BOOTSTRAP_KEY).is_none(),
            std::env::var_os(child_home_key()).as_ref() == Some(&expected_home),
            std::env::var_os("HOME").as_ref() == Some(&expected_home),
            std::env::var_os(child_config_key()).as_ref() == Some(&expected_config),
            std::env::var_os(CHILD_ENV_EXPLICIT_KEY).as_deref()
                == Some(OsStr::new("explicit-value")),
            std::env::var_os(child_removed_key()).is_none(),
            std::env::var_os("TERM").as_deref() == Some(OsStr::new(PTY_TERM)),
        ];
        #[cfg(windows)]
        let checks = {
            let mut checks = checks;
            let system_drive = std::env::var_os("SystemDrive");
            let retained_paths_are_expanded = std::env::vars_os()
                .filter(|(key, _)| is_platform_child_environment_key(key))
                .all(|(_, value)| {
                    !value
                        .to_string_lossy()
                        .to_ascii_lowercase()
                        .contains("%systemdrive%")
                });
            if system_drive.is_none() {
                std::fs::create_dir_all(
                    std::env::current_dir()
                        .unwrap()
                        .join("%SystemDrive%")
                        .join("ProgramData"),
                )
                .unwrap();
            }
            checks.extend([
                system_drive.is_some(),
                retained_paths_are_expanded,
                !std::env::current_dir()
                    .unwrap()
                    .join("%SystemDrive%")
                    .exists(),
            ]);
            checks
        };
        let report = checks
            .iter()
            .map(|passed| if *passed { "true" } else { "false" })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(report_path, report).unwrap();
    }

    #[test]
    fn isolated_pty_child_receives_no_ambient_control_plane_credentials() {
        if std::env::var_os(CHILD_ENV_BOOTSTRAP_KEY).is_some() {
            run_isolated_pty_child_environment_bootstrap();
            return;
        }

        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = PtyChildEnvTestRoot(std::env::temp_dir().join(format!(
            "gate4agent-pty-child-env-{}-{nonce}",
            std::process::id(),
        )));
        let home = root.0.join("home");
        let config = root.0.join("config");
        let report = root.0.join("report.txt");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&config).unwrap();

        let mut bootstrap = std::process::Command::new(std::env::current_exe().unwrap());
        bootstrap.env_clear();
        for (key, value) in std::env::vars_os() {
            if is_platform_child_environment_key(&key) {
                bootstrap.env(key, value);
            }
        }
        bootstrap
            .args([
                "--exact",
                "pty::wrapper::tests::isolated_pty_child_receives_no_ambient_control_plane_credentials",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_ENV_BOOTSTRAP_KEY, "1")
            .env(CHILD_ENV_BOOTSTRAP_REPORT_KEY, &report)
            .env("HOME", &home)
            .env(child_home_key(), &home)
            .env(child_config_key(), &config)
            .env(child_removed_key(), "ambient-value-to-remove")
            .env(CHILD_ENV_SECRET_KEY, "fake-jwt-that-must-not-cross-the-pty")
            .env("ANTHROPIC_API_KEY", "fake-anthropic-key")
            .env("OPENAI_API_KEY", "fake-openai-key")
            .env("SSH_AUTH_SOCK", root.0.join("fake-ssh-agent.sock"))
            .env("HTTPS_PROXY", "http://fake-auth@proxy.invalid");
        let output = bootstrap.output().unwrap();
        assert!(
            output.status.success(),
            "isolated PTY bootstrap failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let checks = std::fs::read_to_string(&report)
            .expect("PTY child did not write its environment report");
        #[cfg(windows)]
        assert_eq!(checks.lines().count(), 15);
        #[cfg(not(windows))]
        assert_eq!(checks.lines().count(), 12);
        assert!(checks.lines().all(|line| line == "true"));
        #[cfg(windows)]
        assert!(!home.join("%SystemDrive%").exists());
    }

    fn run_isolated_pty_child_environment_bootstrap() {
        let report_path = std::env::var_os(CHILD_ENV_BOOTSTRAP_REPORT_KEY).unwrap();
        let expected_home = std::env::var_os(child_home_key()).unwrap();
        let expected_config = std::env::var_os(child_config_key()).unwrap();
        let working_dir = PathBuf::from(&expected_home);
        let plan = LaunchPlan {
            agent_id: AgentId::new("codex").unwrap(),
            program: std::env::current_exe().unwrap().into_os_string(),
            args: vec![
                OsString::from("--exact"),
                OsString::from("pty::wrapper::tests::pty_child_environment_fixture"),
                OsString::from("--nocapture"),
                OsString::from("--test-threads=1"),
            ],
            working_dir,
            env: vec![
                crate::agent::EnvMutation {
                    key: OsString::from(CHILD_ENV_REPORT_KEY),
                    value: Some(report_path),
                },
                crate::agent::EnvMutation {
                    key: OsString::from(CHILD_ENV_EXPECTED_HOME_KEY),
                    value: Some(expected_home),
                },
                crate::agent::EnvMutation {
                    key: OsString::from(CHILD_ENV_EXPECTED_CONFIG_KEY),
                    value: Some(expected_config),
                },
                crate::agent::EnvMutation {
                    key: OsString::from(CHILD_ENV_EXPLICIT_KEY),
                    value: Some(OsString::from("explicit-value")),
                },
                crate::agent::EnvMutation {
                    key: OsString::from(child_removed_key()),
                    value: None,
                },
            ],
            followup_prompt: None,
            followup_draft: None,
            applied_session_options: None,
        };
        let mut wrapper = PtyWrapper::from_launch_plan(plan, None, 24, 80).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let exit_code = loop {
            if let Some(exit_code) = wrapper.try_exit_code() {
                break exit_code;
            }
            if Instant::now() >= deadline {
                let _ = wrapper.kill();
                panic!("isolated PTY child did not exit");
            }
            thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(exit_code, 0);
        wrapper
            .close_and_join_reader(Duration::from_secs(2))
            .unwrap();
    }

    #[cfg(windows)]
    struct WindowsWrapperTestRoot(PathBuf);

    #[cfg(windows)]
    impl Drop for WindowsWrapperTestRoot {
        fn drop(&mut self) {
            if self
                .0
                .file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| name.starts_with("gate4agent-wrapper-reap-"))
            {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn windows_command_wrappers_exit_with_the_vendor_cli() {
        assert_eq!(windows_wrapper_mode(&AgentId::new("kimi").unwrap()), "/C");
        assert_eq!(windows_wrapper_mode(&AgentId::new("claude").unwrap()), "/C");
        assert_eq!(windows_wrapper_mode(&AgentId::new("codex").unwrap()), "/C");
    }

    #[cfg(windows)]
    #[test]
    fn windows_c_wrapper_exits_after_and_reaps_its_fixture_descendant() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = WindowsWrapperTestRoot(std::env::temp_dir().join(format!(
            "gate4agent-wrapper-reap-{}-{nonce}",
            std::process::id(),
        )));
        std::fs::create_dir_all(&root.0).unwrap();
        let child_pid_file = root.0.join("child.pid");
        let fixture = root.0.join("fixture.cmd");
        std::fs::write(
            &fixture,
            b"@echo off\r\npowershell.exe -NoLogo -NoProfile -NonInteractive -Command \"[IO.File]::WriteAllText($env:GATE4AGENT_WRAPPER_CHILD_PID_FILE, [string]$PID); [Console]::ReadLine() | Out-Null\"\r\nexit /b %errorlevel%\r\n",
        )
        .unwrap();
        let plan = LaunchPlan {
            agent_id: AgentId::new("kimi").unwrap(),
            program: fixture.into_os_string(),
            args: Vec::new(),
            working_dir: root.0.clone(),
            env: vec![crate::agent::EnvMutation {
                key: OsString::from("GATE4AGENT_WRAPPER_CHILD_PID_FILE"),
                value: Some(child_pid_file.as_os_str().to_owned()),
            }],
            followup_prompt: None,
            followup_draft: None,
            applied_session_options: None,
        };
        let mut wrapper = PtyWrapper::from_launch_plan(plan, None, 24, 80).unwrap();
        let root_pid = wrapper.root_pid().unwrap();

        let pid_deadline = Instant::now() + Duration::from_secs(3);
        while !child_pid_file.is_file() && Instant::now() < pid_deadline {
            thread::sleep(Duration::from_millis(20));
        }
        let child_pid: u32 = std::fs::read_to_string(&child_pid_file)
            .expect("fixture descendant did not publish its PID")
            .trim()
            .parse()
            .unwrap();
        let running_rows = crate::pty::os_process::query_process_tree_rows().unwrap();
        let root_row = running_rows
            .iter()
            .find(|row| row.pid == root_pid)
            .expect("cmd /C root was not observable while its descendant was running")
            .clone();
        let child_row = running_rows
            .iter()
            .find(|row| row.pid == child_pid)
            .expect("fixture descendant was not observable")
            .clone();
        assert!(crate::pty::os_process::collect_descendants(&running_rows, root_pid)
            .iter()
            .any(|(row, _)| row.pid == child_pid));

        wrapper.write_bytes(b"\r").unwrap();

        let exit_deadline = Instant::now() + Duration::from_secs(8);
        let exit_code = loop {
            if let Some(exit_code) = wrapper.try_exit_code() {
                break exit_code;
            }
            if Instant::now() >= exit_deadline {
                let _ = wrapper.kill();
                panic!("cmd wrapper did not exit after its fixture descendant");
            }
            thread::sleep(Duration::from_millis(20));
        };
        let mut fixture_output = Vec::new();
        while let Some(chunk) = wrapper.try_recv() {
            fixture_output.extend(chunk);
        }
        assert_eq!(
            exit_code,
            0,
            "wrapper fixture failed: {}",
            String::from_utf8_lossy(&fixture_output)
        );
        wrapper
            .close_and_join_reader(Duration::from_secs(2))
            .unwrap();

        let reaped_deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let rows = crate::pty::os_process::query_process_tree_rows().unwrap();
            let root_alive = rows.iter().any(|row| {
                row.pid == root_row.pid && row.started_at == root_row.started_at
            });
            let child_alive = rows.iter().any(|row| {
                row.pid == child_row.pid && row.started_at == child_row.started_at
            });
            if !root_alive && !child_alive {
                break;
            }
            if Instant::now() >= reaped_deadline {
                panic!(
                    "cmd /C wrapper process tree survived: root_alive={root_alive} child_alive={child_alive}"
                );
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_cloned_conpty_killer_reports_success_and_reaps_wrapper() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = WindowsWrapperTestRoot(std::env::temp_dir().join(format!(
            "gate4agent-wrapper-reap-{}-{nonce}",
            std::process::id(),
        )));
        std::fs::create_dir_all(&root.0).unwrap();
        let fixture = root.0.join("fixture.cmd");
        std::fs::write(
            &fixture,
            b"@echo off\r\npause >nul\r\n",
        )
        .unwrap();
        let plan = LaunchPlan {
            agent_id: AgentId::new("codex").unwrap(),
            program: fixture.into_os_string(),
            args: Vec::new(),
            working_dir: root.0.clone(),
            env: Vec::new(),
            followup_prompt: None,
            followup_draft: None,
            applied_session_options: None,
        };
        let mut wrapper = PtyWrapper::from_launch_plan(plan, None, 24, 80).unwrap();
        let mut killer = wrapper.clone_killer();

        killer
            .kill()
            .expect("successful TerminateProcess was reported as an error");

        let exit_deadline = Instant::now() + Duration::from_secs(3);
        while wrapper.try_exit_code().is_none() {
            assert!(
                Instant::now() < exit_deadline,
                "ConPTY wrapper did not expose its forced exit"
            );
            thread::sleep(Duration::from_millis(20));
        }
        wrapper
            .close_and_join_reader(Duration::from_secs(2))
            .unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn child_working_directory_removes_cmd_incompatible_verbatim_prefixes() {
        assert_eq!(
            windows_child_working_directory(Path::new(r"\\?\C:\repo\workspace")),
            PathBuf::from(r"C:\repo\workspace")
        );
        assert_eq!(
            windows_child_working_directory(Path::new(r"\\?\UNC\server\share\workspace")),
            PathBuf::from(r"\\server\share\workspace")
        );
        assert_eq!(
            windows_child_working_directory(Path::new(r"C:\repo\workspace")),
            PathBuf::from(r"C:\repo\workspace")
        );
    }

    #[cfg(windows)]
    #[test]
    fn child_working_directory_rejects_unc_before_spawn() {
        assert!(matches!(
            validated_windows_child_working_directory(Path::new(
                r"\\?\UNC\server\share\workspace"
            )),
            Err(PtyError::UnsupportedWindowsUncWorkingDirectory { .. })
        ));
        assert!(matches!(
            validated_windows_child_working_directory(Path::new(r"\\server\share\workspace")),
            Err(PtyError::UnsupportedWindowsUncWorkingDirectory { .. })
        ));
    }

    #[test]
    fn output_queue_resumes_only_after_low_water() {
        let (sender, receiver) = pty_read_queue();
        sender
            .send(PtyReadEvent::Output(vec![0; PTY_OUTPUT_HIGH_WATER_BYTES]))
            .unwrap();

        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let producer = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            sender.send(PtyReadEvent::Output(vec![1])).unwrap();
        });
        started_rx.recv().unwrap();
        for _ in 0..100 {
            if receiver.queue.state.lock().unwrap().backpressured {
                break;
            }
            std::thread::yield_now();
        }
        assert!(receiver.queue.state.lock().unwrap().backpressured);

        assert!(matches!(receiver.try_recv(), Ok(PtyReadEvent::Output(_))));
        producer.join().unwrap();
        assert!(matches!(receiver.try_recv(), Ok(PtyReadEvent::Output(data)) if data == vec![1]));
    }
}
