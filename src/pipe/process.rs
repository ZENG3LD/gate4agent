//! Pipe-based process wrapper for headless CLI tools.
//!
//! Unlike PtyWrapper (which uses a PTY for interactive TUI tools),
//! PipeProcess uses stdin/stdout pipes for headless NDJSON-streaming tools.

use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use crate::types::CliTool;

/// Claude Code-specific options for pipe mode spawning.
#[derive(Debug, Clone, Default)]
pub struct ClaudeOptions {
    /// Content to append to the system prompt via --append-system-prompt.
    /// Goes into the system prompt and is NEVER compressed or ignored — highest priority.
    pub append_system_prompt: Option<String>,
    /// Resume an existing session via --resume <session-id>.
    pub resume_session_id: Option<String>,
    /// Model override via --model.
    pub model: Option<String>,
}

/// Options for PipeProcess spawning.
///
/// Extra args are passed directly to the CLI command after the standard flags.
/// Claude-specific options live in the `claude` sub-struct to make them self-documenting.
#[derive(Debug, Clone, Default)]
pub struct PipeProcessOptions {
    /// Extra CLI arguments appended after standard flags.
    pub extra_args: Vec<String>,
    /// Claude Code-specific options (ignored for Codex/Gemini).
    pub claude: ClaudeOptions,
}

/// A pipe-based process for headless CLI tool execution.
pub struct PipeProcess {
    child: Child,
    stdin: Option<std::process::ChildStdin>,
    output_rx: Receiver<String>,
    tool: CliTool,
}

impl PipeProcess {
    /// Spawn a headless CLI process with stdin/stdout pipes.
    ///
    /// Each tool is launched with its headless NDJSON flags:
    /// - Claude: `claude -p --output-format stream-json --verbose`
    /// - Codex: `codex exec --json`
    /// - Gemini: `gemini --output-format stream-json -p`
    pub fn new(
        tool: CliTool,
        working_dir: &std::path::Path,
        initial_prompt: &str,
    ) -> Result<Self, std::io::Error> {
        Self::new_with_options(
            tool,
            working_dir,
            initial_prompt,
            PipeProcessOptions::default(),
        )
    }

    /// Spawn a headless CLI process with stdin/stdout pipes and custom options.
    pub fn new_with_options(
        tool: CliTool,
        working_dir: &std::path::Path,
        initial_prompt: &str,
        options: PipeProcessOptions,
    ) -> Result<Self, std::io::Error> {
        let mut cmd = Self::build_command_with_options(tool, initial_prompt, &options);
        cmd.current_dir(working_dir);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::null());

        let mut child = cmd.spawn()?;

        let stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "no stdout"))?;

        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            Self::reader_thread(stdout, tx);
        });

        Ok(Self {
            child,
            stdin,
            output_rx: rx,
            tool,
        })
    }

    fn build_command_with_options(
        tool: CliTool,
        prompt: &str,
        options: &PipeProcessOptions,
    ) -> Command {
        if cfg!(windows) {
            let tool_cmd = match tool {
                CliTool::ClaudeCode => {
                    let mut cmd_str = String::from(
                        "claude -p --output-format stream-json --verbose --dangerously-skip-permissions",
                    );

                    if let Some(ref system_prompt) = options.claude.append_system_prompt {
                        cmd_str.push_str(" --append-system-prompt \"");
                        cmd_str.push_str(&system_prompt.replace('"', "\\\""));
                        cmd_str.push('"');
                    }
                    if let Some(ref session_id) = options.claude.resume_session_id {
                        cmd_str.push_str(" --resume ");
                        cmd_str.push_str(session_id);
                    }
                    if let Some(ref model) = options.claude.model {
                        cmd_str.push_str(" --model ");
                        cmd_str.push_str(model);
                    }

                    for arg in &options.extra_args {
                        cmd_str.push(' ');
                        cmd_str.push_str(arg);
                    }

                    cmd_str.push_str(" \"");
                    cmd_str.push_str(&prompt.replace('"', "\\\""));
                    cmd_str.push('"');
                    cmd_str
                }
                CliTool::Codex => {
                    format!("codex exec --json \"{}\"", prompt.replace('"', "\\\""))
                }
                CliTool::Gemini => {
                    format!(
                        "gemini --output-format stream-json -p \"{}\" --yolo",
                        prompt.replace('"', "\\\"")
                    )
                }
            };
            let mut cmd = Command::new("cmd");
            cmd.args(["/C", &tool_cmd]);
            cmd
        } else {
            match tool {
                CliTool::ClaudeCode => {
                    let mut cmd = Command::new("claude");
                    cmd.arg("-p");
                    cmd.arg("--output-format");
                    cmd.arg("stream-json");
                    cmd.arg("--verbose");
                    cmd.arg("--dangerously-skip-permissions");

                    if let Some(ref system_prompt) = options.claude.append_system_prompt {
                        cmd.arg("--append-system-prompt");
                        cmd.arg(system_prompt);
                    }
                    if let Some(ref session_id) = options.claude.resume_session_id {
                        cmd.arg("--resume");
                        cmd.arg(session_id);
                    }
                    if let Some(ref model) = options.claude.model {
                        cmd.arg("--model");
                        cmd.arg(model);
                    }

                    for arg in &options.extra_args {
                        cmd.arg(arg);
                    }

                    cmd.arg(prompt);
                    cmd
                }
                CliTool::Codex => {
                    let mut cmd = Command::new("codex");
                    cmd.args(["exec", "--json", prompt]);
                    cmd
                }
                CliTool::Gemini => {
                    let mut cmd = Command::new("gemini");
                    cmd.args(["--output-format", "stream-json", "-p", prompt, "--yolo"]);
                    cmd
                }
            }
        }
    }

    fn reader_thread(stdout: std::process::ChildStdout, tx: Sender<String>) {
        use std::io::{BufRead, BufReader};

        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(line) => {
                    if tx.send(format!("{}\n", line)).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    }

    /// Try to receive output (non-blocking).
    pub fn try_recv(&self) -> Option<String> {
        self.output_rx.try_recv().ok()
    }

    /// Write input to the process stdin.
    pub fn write(&mut self, data: &str) -> Result<(), std::io::Error> {
        if let Some(stdin) = &mut self.stdin {
            stdin.write_all(data.as_bytes())?;
            stdin.flush()?;
        }
        Ok(())
    }

    /// Check if the process is still running.
    pub fn is_running(&mut self) -> bool {
        self.child.try_wait().ok().flatten().is_none()
    }

    /// Kill the process.
    pub fn kill(&mut self) -> Result<(), std::io::Error> {
        self.child.kill()
    }

    /// Get the CLI tool type.
    pub fn tool(&self) -> CliTool {
        self.tool
    }
}
