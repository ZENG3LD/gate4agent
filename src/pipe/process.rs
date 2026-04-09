//! Pipe-based process wrapper for headless CLI tools.
//!
//! Unlike PtyWrapper (which uses a PTY for interactive TUI tools),
//! PipeProcess uses stdin/stdout pipes for headless NDJSON-streaming tools.

use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use crate::pipe::cli::cli_builder;
use crate::transport::SpawnOptions;
use crate::core::types::CliTool;

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
    ///
    /// The initial prompt is written to stdin (not as a CLI argument) to avoid
    /// Windows `cmd /C` mangling of Unicode, spaces, and special characters.
    /// Note: Codex and Gemini receive the prompt as a CLI argument because they
    /// do not read stdin; for those tools the `cmd /C` shell string includes the
    /// properly-escaped prompt.
    pub fn new_with_options(
        tool: CliTool,
        working_dir: &std::path::Path,
        initial_prompt: &str,
        options: PipeProcessOptions,
    ) -> Result<Self, std::io::Error> {
        let spawn_opts = Self::pipe_options_to_spawn_opts(tool, initial_prompt, &options, working_dir);
        let mut cmd = Self::build_command_with_options(tool, &spawn_opts);
        cmd.current_dir(working_dir);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::null());

        let mut child = cmd.spawn()?;

        let stdin = child.stdin.take();

        // Write prompt via stdin instead of CLI argument (avoids cmd.exe mangling).
        // Claude `-p` reads stdin until EOF, so we must drop (close) stdin after writing.
        // For Codex and Gemini the prompt is already in the argv; stdin is closed immediately.
        if let Some(mut s) = stdin {
            if tool == CliTool::ClaudeCode {
                s.write_all(initial_prompt.as_bytes())?;
                s.flush()?;
            }
            drop(s); // close stdin → Claude sees EOF → starts processing
        }

        // stdin is now closed; set to None
        let stdin: Option<std::process::ChildStdin> = None;

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

    /// Convert legacy `PipeProcessOptions` to the new `SpawnOptions`.
    fn pipe_options_to_spawn_opts(
        _tool: CliTool,
        prompt: &str,
        options: &PipeProcessOptions,
        working_dir: &std::path::Path,
    ) -> SpawnOptions {
        SpawnOptions {
            working_dir: working_dir.to_path_buf(),
            prompt: prompt.to_string(),
            resume_session_id: options.claude.resume_session_id.clone(),
            model: options.claude.model.clone(),
            append_system_prompt: options.claude.append_system_prompt.clone(),
            extra_args: options.extra_args.clone(),
            env_vars: Vec::new(),
            ..Default::default()
        }
    }

    /// Build the spawn `Command` by delegating to the per-CLI builder.
    ///
    /// On Unix: returns the `Command` from the per-CLI builder directly.
    ///
    /// On Windows: npm-installed CLIs get `cmd /C <program>.cmd <args...>`.
    /// Non-npm tools (bash scripts, native binaries) get `bash -c '...'`.
    /// Each arg is passed individually (not joined into a shell string)
    /// so Windows CreateProcess handles quoting correctly for prompts
    /// that contain spaces and special characters.
    fn build_command_with_options(tool: CliTool, opts: &SpawnOptions) -> Command {
        let builder = cli_builder(tool);
        let inner_cmd = builder.build_command(opts);

        if cfg!(windows) {
            // On Windows, npm-installed CLIs have `.cmd` batch wrappers that
            // cmd.exe can invoke. Non-npm tools (e.g. cursor-agent) may be
            // bash scripts or native binaries with no `.cmd` wrapper.
            //
            // Strategy: check if `<program>.cmd` exists on PATH. If yes,
            // use `cmd /C <program>.cmd <args...>`. If not, invoke the
            // program directly (works for .exe binaries and bash scripts
            // when running under MSYS2/Git Bash).
            //
            // Each arg is passed as a separate element (NOT joined into a
            // shell string) so Windows CreateProcess handles quoting correctly.
            let program = inner_cmd.get_program().to_string_lossy().to_string();

            if program.ends_with(".cmd") || program.ends_with(".exe") {
                // Already has extension — use cmd /C directly.
                let mut cmd = Command::new("cmd");
                cmd.arg("/C");
                cmd.arg(&program);
                for arg in inner_cmd.get_args() {
                    cmd.arg(arg);
                }
                cmd
            } else {
                // Check if a .cmd wrapper exists on PATH.
                let cmd_name = format!("{}.cmd", program);
                let has_cmd = std::env::var_os("PATH")
                    .map(|path| {
                        std::env::split_paths(&path)
                            .any(|dir| dir.join(&cmd_name).is_file())
                    })
                    .unwrap_or(false);

                if has_cmd {
                    let mut cmd = Command::new("cmd");
                    cmd.arg("/C");
                    cmd.arg(&cmd_name);
                    for arg in inner_cmd.get_args() {
                        cmd.arg(arg);
                    }
                    cmd
                } else {
                    // No .cmd wrapper — may be a bash script or native .exe.
                    // Windows CreateProcess can't run extensionless scripts,
                    // so we invoke via `bash -c "program arg1 arg2 ..."`.
                    let mut cmd = Command::new("bash");
                    cmd.arg("-c");
                    let mut shell_str = Self::shell_quote(&program);
                    for arg in inner_cmd.get_args() {
                        let s = arg.to_string_lossy();
                        shell_str.push(' ');
                        shell_str.push_str(&Self::shell_quote(&s));
                    }
                    cmd.arg(&shell_str);
                    cmd
                }
            }
        } else {
            inner_cmd
        }
    }

    /// Single-quote a token for POSIX shell (`bash -c`).
    fn shell_quote(s: &str) -> String {
        if s.is_empty() {
            return "''".to_string();
        }
        // If it contains no special chars, return as-is.
        if s.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '/') {
            return s.to_string();
        }
        // Wrap in single quotes, escaping embedded single quotes.
        format!("'{}'", s.replace('\'', "'\\''"))
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

    /// Wait for the process to exit and return its exit status.
    ///
    /// Called by the reader loop after `is_running()` returns false to collect
    /// the exit code for `SessionEnd` synthesis.
    pub fn wait(&mut self) -> Result<Option<std::process::ExitStatus>, std::io::Error> {
        self.child.try_wait()
    }

    /// Get the CLI tool type.
    pub fn tool(&self) -> CliTool {
        self.tool
    }
}
