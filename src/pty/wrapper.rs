//! PTY wrapper for cross-platform terminal emulation.

use portable_pty::{native_pty_system, Child, ChildKiller, CommandBuilder, MasterPty, PtySize};
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
use crate::core::types::CliTool;
use crate::pty::os_process::{
    observe_pty_foreground, PtyForegroundObservation, PtyProcessProbeError,
};

pub const PTY_OUTPUT_HIGH_WATER_BYTES: usize = 256 * 1024;
pub const PTY_OUTPUT_LOW_WATER_BYTES: usize = 32 * 1024;

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

        // Note: CommandBuilder::new() already inherits ALL current process
        // environment variables via get_base_env(), so we don't need to
        // manually pass through HOME, PATH, APPDATA, etc.

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

fn windows_wrapper_mode(agent_id: &AgentId) -> &'static str {
    if agent_id.as_str() == "kimi" {
        "/K"
    } else {
        "/C"
    }
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

    #[test]
    fn kimi_keeps_the_windows_command_wrapper_as_the_process_tree_anchor() {
        assert_eq!(windows_wrapper_mode(&AgentId::new("kimi").unwrap()), "/K");
        assert_eq!(windows_wrapper_mode(&AgentId::new("claude").unwrap()), "/C");
        assert_eq!(windows_wrapper_mode(&AgentId::new("codex").unwrap()), "/C");
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
