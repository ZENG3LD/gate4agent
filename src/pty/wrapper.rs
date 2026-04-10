//! PTY wrapper for cross-platform terminal emulation.

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::io::Write;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use thiserror::Error;

use crate::core::types::CliTool;

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
}

/// PTY wrapper for managing terminal processes.
pub struct PtyWrapper {
    /// Master PTY for I/O.
    master: Box<dyn MasterPty + Send>,
    /// Child process.
    child: Box<dyn Child + Send + Sync>,
    /// Writer for sending input.
    writer: Box<dyn Write + Send>,
    /// Receiver for output.
    output_rx: Receiver<Vec<u8>>,
    /// CLI tool being wrapped.
    tool: CliTool,
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
        let pty_system = native_pty_system();

        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| PtyError::CreateFailed(e.to_string()))?;

        let cmd = Self::build_command(tool, working_dir, env_vars);

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| PtyError::SpawnFailed(e.to_string()))?;

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| PtyError::Pty(e.to_string()))?;

        let writer = pair
            .master
            .take_writer()
            .map_err(|e| PtyError::Pty(e.to_string()))?;

        // Spawn reader thread
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            Self::reader_thread(reader, tx);
        });

        Ok(Self {
            master: pair.master,
            child,
            writer,
            output_rx: rx,
            tool,
        })
    }

    fn build_command(
        tool: CliTool,
        working_dir: &std::path::Path,
        env_vars: &[(String, String)],
    ) -> CommandBuilder {
        let mut cmd = if cfg!(windows) {
            let tool_name = match tool {
                CliTool::ClaudeCode => "claude",
                CliTool::Codex => "codex",
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
                CliTool::Gemini => CommandBuilder::new("gemini"),
                // OpenCode PTY integration will be added once
                // its invocation shape is confirmed via live capture.
                CliTool::OpenCode => CommandBuilder::new("opencode"),
            }
        };

        cmd.cwd(working_dir);

        // Note: CommandBuilder::new() already inherits ALL current process
        // environment variables via get_base_env(), so we don't need to
        // manually pass through HOME, PATH, APPDATA, etc.

        // Add custom env vars (these override inherited values if keys match)
        for (key, value) in env_vars {
            cmd.env(key, value);
        }

        cmd
    }

    fn reader_thread(reader: Box<dyn std::io::Read + Send>, tx: Sender<Vec<u8>>) {
        use std::io::Read;

        let mut reader = reader;
        let mut buffer = [0u8; 4096];

        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break, // EOF
                Ok(n) => {
                    // Send raw bytes — never lose UTF-8 to from_utf8_lossy.
                    // vt100::Parser accepts &[u8] and handles multi-byte UTF-8
                    // correctly even if a sequence is split across read() calls.
                    if tx.send(buffer[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    }

    /// Send input to the terminal.
    pub fn write(&mut self, data: &str) -> Result<(), PtyError> {
        self.writer.write_all(data.as_bytes())?;
        self.writer.flush()?;
        Ok(())
    }

    /// Write raw bytes to the terminal.
    pub fn write_bytes(&mut self, data: &[u8]) -> Result<(), PtyError> {
        self.writer.write_all(data)?;
        self.writer.flush()?;
        Ok(())
    }

    /// Send a line of input (with newline).
    pub fn writeln(&mut self, data: &str) -> Result<(), PtyError> {
        self.write(&format!("{}\n", data))
    }

    /// Try to receive output bytes (non-blocking).
    pub fn try_recv(&self) -> Option<Vec<u8>> {
        self.output_rx.try_recv().ok()
    }

    /// Resize the terminal.
    pub fn resize(&self, rows: u16, cols: u16) -> Result<(), PtyError> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| PtyError::Pty(e.to_string()))
    }

    /// Check if the process is still running.
    pub fn is_running(&mut self) -> bool {
        self.child.try_wait().ok().flatten().is_none()
    }

    /// Kill the process.
    pub fn kill(&mut self) -> Result<(), PtyError> {
        self.child.kill().map_err(|e| PtyError::Pty(e.to_string()))
    }

    /// Wait for the process to exit.
    pub fn wait(&mut self) -> Option<u32> {
        self.child.wait().ok().map(|s| s.exit_code())
    }

    /// Get the CLI tool.
    pub fn tool(&self) -> CliTool {
        self.tool
    }
}
