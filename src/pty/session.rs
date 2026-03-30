//! Async PTY session with tokio broadcast fan-out.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::cli::factory::{create_pipeline, create_submitter};
use crate::cli::traits::{MessageClass, StartupAction};
use crate::detection::RateLimitDetector;
use crate::error::AgentError;
use crate::parser::VteParser;
use crate::types::{AgentEvent, CliTool, SessionConfig};

use super::wrapper::{PtyError, PtyWrapper};

/// Conversion from the internal PtyError to the public AgentError.
impl From<PtyError> for AgentError {
    fn from(e: PtyError) -> Self {
        match e {
            PtyError::CreateFailed(s) => AgentError::PtyCreate(s),
            PtyError::SpawnFailed(s) => AgentError::PtySpawn(s),
            PtyError::Io(e) => AgentError::PtyIo { source: e },
            PtyError::Pty(s) => AgentError::Pty(s),
        }
    }
}

/// Opaque write handle for sending input to a PTY.
pub struct PtyWriteHandle {
    inner: Arc<Mutex<PtyWrapper>>,
}

impl PtyWriteHandle {
    /// Write raw bytes to the PTY.
    pub fn write(&self, data: &str) -> Result<(), AgentError> {
        let mut pty = self
            .inner
            .lock()
            .map_err(|_| AgentError::Pty("PTY mutex poisoned".into()))?;
        pty.write(data).map_err(|e| AgentError::Pty(e.to_string()))
    }

    /// Resize the PTY.
    pub fn resize(&self, rows: u16, cols: u16) -> Result<(), AgentError> {
        let pty = self
            .inner
            .lock()
            .map_err(|_| AgentError::Pty("PTY mutex poisoned".into()))?;
        pty.resize(rows, cols).map_err(AgentError::from)
    }
}

/// Async PTY session. Spawns a CLI tool in a real PTY and broadcasts `AgentEvent`
/// to all subscribers via a tokio broadcast channel.
///
/// The reader loop runs on a blocking thread (via `spawn_blocking`) because PTY
/// I/O is inherently blocking. The loop bridges to async consumers via the
/// broadcast channel.
///
/// # Broadcast semantics
///
/// `tokio::sync::broadcast` drops events for lagged receivers — if a subscriber
/// falls behind, it will skip events. This is acceptable for streaming agent output
/// (e.g., TUI rendering can re-render from the PTY screen buffer). If you need
/// guaranteed delivery, wrap the receiver in a buffering layer.
pub struct PtySession {
    session_id: String,
    tx: broadcast::Sender<AgentEvent>,
    pty: Arc<Mutex<PtyWrapper>>,
    reader_task: JoinHandle<()>,
}

impl PtySession {
    /// Spawn a CLI tool in a PTY and start broadcasting events.
    ///
    /// Uses a 24x80 terminal size (compact). To control size, use `spawn_with_size`.
    pub async fn spawn(config: SessionConfig) -> Result<Self, AgentError> {
        Self::spawn_with_size(config, 24, 80).await
    }

    /// Spawn a CLI tool in a PTY with a specific terminal size.
    pub async fn spawn_with_size(
        config: SessionConfig,
        rows: u16,
        cols: u16,
    ) -> Result<Self, AgentError> {
        let tool = config.tool;
        let session_id = uuid_v4();

        // Create PTY (blocking, must be done before entering the async task)
        let pty = PtyWrapper::new_with_env(
            tool,
            &config.working_dir,
            &config.env_vars,
            rows,
            cols,
        )?;
        let pty = Arc::new(Mutex::new(pty));

        let (tx, _) = broadcast::channel::<AgentEvent>(256);

        // Broadcast Started event
        let _ = tx.send(AgentEvent::Started {
            session_id: session_id.clone(),
        });

        // Clone for the reader task
        let pty_clone = pty.clone();
        let tx_clone = tx.clone();
        let sid_clone = session_id.clone();

        let reader_task = tokio::task::spawn_blocking(move || {
            reader_loop(pty_clone, tx_clone, tool, sid_clone);
        });

        Ok(Self {
            session_id,
            tx,
            pty,
            reader_task,
        })
    }

    /// Subscribe to receive all future `AgentEvent` values from this session.
    ///
    /// Note: events that occurred before subscribing will not be received.
    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.tx.subscribe()
    }

    /// Get the write handle for sending input to the PTY.
    pub fn write_handle(&self) -> PtyWriteHandle {
        PtyWriteHandle {
            inner: self.pty.clone(),
        }
    }

    /// Send raw bytes to the PTY.
    pub async fn write(&self, data: &str) -> Result<(), AgentError> {
        let data = data.to_owned();
        let pty = self.pty.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = pty
                .lock()
                .map_err(|_| AgentError::Pty("PTY mutex poisoned".into()))?;
            guard.write(&data).map_err(|e| AgentError::Pty(e.to_string()))
        })
        .await
        .map_err(|_| AgentError::Pty("spawn_blocking panicked".into()))?
    }

    /// Send a prompt char-by-char (required for Ink-based TUI tools like Claude Code).
    ///
    /// Sends each character with a small delay to avoid overwhelming the TUI's raw-mode
    /// input processing. Ends with a carriage return.
    pub async fn send_prompt(&self, prompt: &str) -> Result<(), AgentError> {
        for ch in prompt.chars() {
            let s = ch.to_string();
            self.write(&s).await?;
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        self.write("\r").await
    }

    /// Resize the PTY.
    pub async fn resize(&self, rows: u16, cols: u16) -> Result<(), AgentError> {
        let pty = self.pty.clone();
        tokio::task::spawn_blocking(move || {
            let guard = pty
                .lock()
                .map_err(|_| AgentError::Pty("PTY mutex poisoned".into()))?;
            guard.resize(rows, cols).map_err(AgentError::from)
        })
        .await
        .map_err(|_| AgentError::Pty("spawn_blocking panicked".into()))?
    }

    /// Session ID assigned at spawn time.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Abort the reader task and kill the child process.
    pub async fn kill(&self) -> Result<(), AgentError> {
        self.reader_task.abort();
        let pty = self.pty.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = pty
                .lock()
                .map_err(|_| AgentError::Pty("PTY mutex poisoned".into()))?;
            guard.kill().map_err(AgentError::from)
        })
        .await
        .map_err(|_| AgentError::Pty("spawn_blocking panicked".into()))?
    }
}

// ---------------------------------------------------------------------------
// Reader loop (runs on blocking thread via spawn_blocking)
// ---------------------------------------------------------------------------

fn reader_loop(
    pty: Arc<Mutex<PtyWrapper>>,
    tx: broadcast::Sender<AgentEvent>,
    tool: CliTool,
    _session_id: String,
) {
    let mut vte_parser = VteParser::new();
    let rate_limit_detector = RateLimitDetector::new_for_tool(tool);
    let mut pipeline = create_pipeline(tool);
    let submitter = create_submitter(tool);
    let mut startup_done = false;

    loop {
        // Try to receive output from PTY (non-blocking)
        let raw = {
            match pty.lock() {
                Ok(guard) => guard.try_recv(),
                Err(_) => break, // Mutex poisoned, exit
            }
        };

        let raw = match raw {
            Some(r) => r,
            None => {
                // Nothing available, check if process is still running
                let still_running = pty
                    .lock()
                    .map(|mut g| g.is_running())
                    .unwrap_or(false);
                if !still_running {
                    let code = pty
                        .lock()
                        .ok()
                        .and_then(|mut g| g.wait())
                        .unwrap_or(0);
                    let _ = tx.send(AgentEvent::Exited { code: code as i32 });
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
        };

        // Broadcast raw PTY bytes (for vt100 screen emulation by consumers)
        let _ = tx.send(AgentEvent::PtyRaw { data: raw.clone() });

        // Strip ANSI for text analysis
        let cleaned = vte_parser.parse(&raw);

        // Rate limit detection
        if let Some(rl_info) = rate_limit_detector.detect(&cleaned) {
            let _ = tx.send(AgentEvent::RateLimit(rl_info));
        }

        // Classification pipeline
        let messages = pipeline.process(&raw);
        for msg in messages {
            match msg.class {
                MessageClass::PromptReady => {
                    let _ = tx.send(AgentEvent::PtyReady);
                }
                MessageClass::ToolApproval => {
                    let tool_name = msg
                        .metadata
                        .tool_name
                        .clone()
                        .unwrap_or_else(|| "unknown".into());
                    let _ = tx.send(AgentEvent::PtyToolApproval {
                        tool_name,
                        description: None,
                    });
                }
                _ => {}
            }
            let _ = tx.send(AgentEvent::PtyParsed(msg));
        }

        // Startup sequence handling
        if !startup_done {
            let action = submitter.handle_startup(&cleaned);
            match action {
                StartupAction::Ready => {
                    startup_done = true;
                }
                StartupAction::SendInput(input) => {
                    if let Ok(mut guard) = pty.lock() {
                        let _ = guard.write(&input);
                    }
                }
                StartupAction::Waiting => {}
            }
        }
    }
}

/// Generate a simple UUID-like session ID.
fn uuid_v4() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("pty-{:x}", t)
}
