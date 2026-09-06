//! Real subprocess-backed terminal sessions for ACP `terminal/*` methods.
//!
//! ACP terminal semantics are create-once, poll-many: the agent creates a
//! terminal bound to a command via `terminal/create`, then independently
//! polls accumulated output (`terminal/output`), blocks for completion
//! (`terminal/wait_for_exit`), or tears it down (`terminal/kill`,
//! `terminal/release`) — none of which are the JSON-RPC turn that created
//! it. This mirrors the background-reader-thread pattern `AcpProcess`
//! (`spawn.rs`) already uses for the ACP subprocess itself: spawn, then a
//! background OS thread drains the pipe into shared state while the caller
//! polls it independently. The one difference is what gets accumulated —
//! `AcpProcess` splits NDJSON lines for its own JSON-RPC framing, a
//! terminal command has no such framing and just accumulates raw bytes.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use super::protocol::{EnvVariable, TerminalCreateParams, TerminalExitStatus, TerminalOutputResult};

/// Cap on retained combined stdout+stderr per terminal when the agent does
/// not send `outputByteLimit` — an ACP agent that never calls
/// `terminal/release` must not be able to grow host memory without bound.
const DEFAULT_OUTPUT_BYTE_LIMIT: usize = 1024 * 1024;

/// A single live (or exited-but-not-yet-released) terminal.
struct TerminalHandle {
    child: Mutex<Child>,
    output: Arc<Mutex<Vec<u8>>>,
    truncated: Arc<AtomicBool>,
    /// Cached exit status once `wait_for_exit` (or a completed `try_wait`
    /// probe from `output`) has observed the process end. `None` means
    /// still running as far as this handle has observed.
    exit_status: Mutex<Option<TerminalExitStatus>>,
}

/// Table of live terminals for one ACP host session, keyed by an
/// opaque, host-generated `terminalId`.
pub(crate) struct TerminalStore {
    sessions: Mutex<HashMap<String, TerminalHandle>>,
    next_id: AtomicU64,
}

impl TerminalStore {
    pub(crate) fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    /// Spawn `params.command` and register it under a new terminal id.
    ///
    /// `params.cwd`, when present, is used as-is (the ACP spec documents it
    /// as an absolute path); otherwise the command inherits `working_dir`
    /// (the ACP session's own working directory).
    pub(crate) fn create(
        &self,
        working_dir: &Path,
        params: &TerminalCreateParams,
    ) -> Result<String, String> {
        if params.command.is_empty() {
            return Err("terminal/create requires a non-empty command".to_string());
        }

        let mut cmd = Command::new(&params.command);
        crate::utils::hide_console_window(&mut cmd);
        cmd.args(&params.args);
        for EnvVariable { name, value } in &params.env {
            cmd.env(name, value);
        }
        let cwd: PathBuf = params
            .cwd
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| working_dir.to_path_buf());
        cmd.current_dir(&cwd);
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child = cmd.spawn().map_err(|e| e.to_string())?;
        let stdout = child.stdout.take().ok_or("no stdout pipe")?;
        let stderr = child.stderr.take().ok_or("no stderr pipe")?;

        let limit = params
            .output_byte_limit
            .and_then(|limit| usize::try_from(limit).ok())
            .filter(|limit| *limit > 0)
            .unwrap_or(DEFAULT_OUTPUT_BYTE_LIMIT);

        let output = Arc::new(Mutex::new(Vec::new()));
        let truncated = Arc::new(AtomicBool::new(false));
        spawn_output_drain(stdout, Arc::clone(&output), Arc::clone(&truncated), limit);
        spawn_output_drain(stderr, Arc::clone(&output), Arc::clone(&truncated), limit);

        let id = format!("term-{:x}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let handle = TerminalHandle {
            child: Mutex::new(child),
            output,
            truncated,
            exit_status: Mutex::new(None),
        };
        self.sessions
            .lock()
            .map_err(|_| "terminal table mutex poisoned".to_string())?
            .insert(id.clone(), handle);
        Ok(id)
    }

    /// Non-blocking snapshot of accumulated output. Also probes (without
    /// blocking) whether the process has exited since the last call.
    pub(crate) fn output(&self, terminal_id: &str) -> Result<TerminalOutputResult, String> {
        let sessions = self
            .sessions
            .lock()
            .map_err(|_| "terminal table mutex poisoned".to_string())?;
        let handle = sessions
            .get(terminal_id)
            .ok_or_else(|| format!("unknown terminal: {}", terminal_id))?;

        let exit_status = self.probe_exit(handle)?;
        let output = handle
            .output
            .lock()
            .map_err(|_| "terminal output mutex poisoned".to_string())?;
        Ok(TerminalOutputResult {
            output: String::from_utf8_lossy(&output).into_owned(),
            truncated: handle.truncated.load(Ordering::Acquire),
            exit_status,
        })
    }

    /// Block until the command exits, then return its exit status.
    pub(crate) fn wait_for_exit(&self, terminal_id: &str) -> Result<TerminalExitStatus, String> {
        let status = {
            let sessions = self
                .sessions
                .lock()
                .map_err(|_| "terminal table mutex poisoned".to_string())?;
            let handle = sessions
                .get(terminal_id)
                .ok_or_else(|| format!("unknown terminal: {}", terminal_id))?;
            if let Some(status) = handle
                .exit_status
                .lock()
                .map_err(|_| "terminal exit-status mutex poisoned".to_string())?
                .clone()
            {
                return Ok(status);
            }
            let mut child = handle
                .child
                .lock()
                .map_err(|_| "terminal child mutex poisoned".to_string())?;
            let exit = child.wait().map_err(|e| e.to_string())?;
            let status = exit_status_from(&exit);
            *handle
                .exit_status
                .lock()
                .map_err(|_| "terminal exit-status mutex poisoned".to_string())? =
                Some(status.clone());
            status
        };
        Ok(status)
    }

    /// Kill the command if it is still running. Not an error if it has
    /// already exited.
    pub(crate) fn kill(&self, terminal_id: &str) -> Result<(), String> {
        let sessions = self
            .sessions
            .lock()
            .map_err(|_| "terminal table mutex poisoned".to_string())?;
        let handle = sessions
            .get(terminal_id)
            .ok_or_else(|| format!("unknown terminal: {}", terminal_id))?;
        let mut child = handle
            .child
            .lock()
            .map_err(|_| "terminal child mutex poisoned".to_string())?;
        match child.try_wait() {
            Ok(Some(_)) => Ok(()),
            Ok(None) => child.kill().map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        }
    }

    /// Kill the command (if still running) and forget the terminal.
    pub(crate) fn release(&self, terminal_id: &str) -> Result<(), String> {
        let handle = self
            .sessions
            .lock()
            .map_err(|_| "terminal table mutex poisoned".to_string())?
            .remove(terminal_id)
            .ok_or_else(|| format!("unknown terminal: {}", terminal_id))?;
        let mut child = handle
            .child
            .lock()
            .map_err(|_| "terminal child mutex poisoned".to_string())?;
        if matches!(child.try_wait(), Ok(None)) {
            let _ = child.kill();
        }
        Ok(())
    }

    /// Non-blocking exit probe shared by [`output`](Self::output); caches a
    /// completed status onto the handle so a later `wait_for_exit` returns
    /// immediately instead of re-waiting a reaped child.
    fn probe_exit(&self, handle: &TerminalHandle) -> Result<Option<TerminalExitStatus>, String> {
        {
            let cached = handle
                .exit_status
                .lock()
                .map_err(|_| "terminal exit-status mutex poisoned".to_string())?;
            if cached.is_some() {
                return Ok(cached.clone());
            }
        }
        let mut child = handle
            .child
            .lock()
            .map_err(|_| "terminal child mutex poisoned".to_string())?;
        let Some(exit) = child.try_wait().map_err(|e| e.to_string())? else {
            return Ok(None);
        };
        let status = exit_status_from(&exit);
        *handle
            .exit_status
            .lock()
            .map_err(|_| "terminal exit-status mutex poisoned".to_string())? = Some(status.clone());
        Ok(Some(status))
    }
}

fn exit_status_from(status: &std::process::ExitStatus) -> TerminalExitStatus {
    TerminalExitStatus {
        exit_code: status.code(),
        signal: unix_signal(status),
    }
}

#[cfg(unix)]
fn unix_signal(status: &std::process::ExitStatus) -> Option<String> {
    use std::os::unix::process::ExitStatusExt;
    status.signal().map(|signal| signal.to_string())
}

#[cfg(not(unix))]
fn unix_signal(_status: &std::process::ExitStatus) -> Option<String> {
    None
}

/// Drain a pipe into `output` (shared with the sibling stdout/stderr
/// thread) up to `limit` combined bytes, then stop reading — the process
/// itself is left running; only further capture is abandoned. Sets
/// `truncated` once the cap is hit.
fn spawn_output_drain(
    mut reader: impl Read + Send + 'static,
    output: Arc<Mutex<Vec<u8>>>,
    truncated: Arc<AtomicBool>,
    limit: usize,
) {
    thread::spawn(move || {
        let mut chunk = [0_u8; 8 * 1024];
        loop {
            let read = match reader.read(&mut chunk) {
                Ok(0) => return,
                Ok(n) => n,
                Err(_) => return,
            };
            let Ok(mut buffer) = output.lock() else { return };
            if buffer.len() >= limit {
                truncated.store(true, Ordering::Release);
                return;
            }
            let take = read.min(limit - buffer.len());
            buffer.extend_from_slice(&chunk[..take]);
            if take < read {
                truncated.store(true, Ordering::Release);
                return;
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn echo_command() -> TerminalCreateParams {
        #[cfg(windows)]
        {
            TerminalCreateParams {
                session_id: "s1".to_owned(),
                command: "cmd".to_owned(),
                args: vec!["/C".to_owned(), "echo hello".to_owned()],
                env: vec![],
                cwd: None,
                output_byte_limit: None,
            }
        }
        #[cfg(not(windows))]
        {
            TerminalCreateParams {
                session_id: "s1".to_owned(),
                command: "sh".to_owned(),
                args: vec!["-c".to_owned(), "echo hello".to_owned()],
                env: vec![],
                cwd: None,
                output_byte_limit: None,
            }
        }
    }

    #[test]
    fn full_lifecycle_create_output_wait_release() {
        let store = TerminalStore::new();
        let working_dir = std::env::temp_dir();
        let id = store.create(&working_dir, &echo_command()).expect("spawn should succeed");

        let exit = store.wait_for_exit(&id).expect("wait should succeed");
        assert_eq!(exit.exit_code, Some(0));

        let output = store.output(&id).expect("output should succeed");
        assert!(output.output.contains("hello"), "output was: {:?}", output.output);
        assert!(!output.truncated);
        assert_eq!(output.exit_status.and_then(|s| s.exit_code), Some(0));

        store.release(&id).expect("release should succeed");
        assert!(store.output(&id).is_err(), "terminal must be gone after release");
    }

    #[test]
    fn unknown_terminal_id_errors_on_every_operation() {
        let store = TerminalStore::new();
        assert!(store.output("nope").is_err());
        assert!(store.wait_for_exit("nope").is_err());
        assert!(store.kill("nope").is_err());
        assert!(store.release("nope").is_err());
    }

    #[test]
    fn empty_command_is_rejected_before_spawning() {
        let store = TerminalStore::new();
        let working_dir = std::env::temp_dir();
        let params = TerminalCreateParams {
            session_id: "s1".to_owned(),
            command: String::new(),
            args: vec![],
            env: vec![],
            cwd: None,
            output_byte_limit: None,
        };
        assert!(store.create(&working_dir, &params).is_err());
    }

    #[test]
    fn kill_stops_a_long_running_command() {
        let store = TerminalStore::new();
        let working_dir = std::env::temp_dir();
        #[cfg(windows)]
        let params = TerminalCreateParams {
            session_id: "s1".to_owned(),
            command: "cmd".to_owned(),
            args: vec!["/C".to_owned(), "ping -n 30 127.0.0.1 >nul".to_owned()],
            env: vec![],
            cwd: None,
            output_byte_limit: None,
        };
        #[cfg(not(windows))]
        let params = TerminalCreateParams {
            session_id: "s1".to_owned(),
            command: "sleep".to_owned(),
            args: vec!["30".to_owned()],
            env: vec![],
            cwd: None,
            output_byte_limit: None,
        };
        let id = store.create(&working_dir, &params).expect("spawn should succeed");
        store.kill(&id).expect("kill should succeed");
        let exit = store.wait_for_exit(&id).expect("wait after kill should still succeed");
        assert_ne!(exit.exit_code, Some(0));
        store.release(&id).expect("release should succeed");
    }
}
