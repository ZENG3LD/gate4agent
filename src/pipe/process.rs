//! Pipe-based process wrapper for headless CLI tools.
//!
//! Unlike PtyWrapper (which uses a PTY for interactive TUI tools),
//! PipeProcess uses stdin/stdout pipes for headless NDJSON-streaming tools.

use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use crate::cli::factory::cli_builder;
use crate::transport::SpawnOptions;
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
        }
    }

    /// Build the spawn `Command` by delegating to the per-CLI builder.
    ///
    /// On Unix: returns the `Command` from the per-CLI builder directly.
    ///
    /// On Windows: the per-CLI builder returns a bare argv `Command`; this
    /// function converts it to a `cmd /C <shell_string>` command. The shell
    /// string is built by quoting each argument with Windows `"..."` quoting,
    /// which handles spaces and special characters correctly for `cmd.exe`.
    ///
    /// Note: the prompt is included in argv for Codex and Gemini even on
    /// Windows — the shell-quoting in `argv_to_windows_shell_string` handles
    /// escaping. Claude's prompt is NOT in argv (delivered via stdin instead).
    fn build_command_with_options(tool: CliTool, opts: &SpawnOptions) -> Command {
        let builder = cli_builder(tool);
        let inner_cmd = builder.build_command(opts);

        if cfg!(windows) {
            let shell_str = Self::argv_to_windows_shell_string(&inner_cmd);
            let mut cmd = Command::new("cmd");
            cmd.args(["/C", &shell_str]);
            cmd
        } else {
            inner_cmd
        }
    }

    /// Convert a `Command`'s program + args into a Windows shell string for `cmd /C`.
    ///
    /// Each token is wrapped in double quotes with internal double-quotes escaped
    /// as `\"`. This matches the quoting the old `format!("... \"{}\"", ...)` code
    /// used for Codex and Gemini prompts, and is safe for `cmd.exe` argument passing.
    fn argv_to_windows_shell_string(cmd: &Command) -> String {
        let program = cmd.get_program().to_string_lossy();
        let mut parts = vec![Self::win_quote(&program)];
        for arg in cmd.get_args() {
            let s = arg.to_string_lossy();
            parts.push(Self::win_quote(&s));
        }
        parts.join(" ")
    }

    /// Wrap a single token in Windows double-quote quoting.
    ///
    /// If the token contains no spaces, quotes, or shell-special characters,
    /// the raw token is returned unchanged (avoids spurious quoting noise in
    /// simple cases like flag names). Otherwise the token is wrapped in `"..."`
    /// with internal `"` escaped as `\"`.
    fn win_quote(s: &str) -> String {
        let needs_quoting = s.is_empty()
            || s.contains(' ')
            || s.contains('"')
            || s.contains('&')
            || s.contains('|')
            || s.contains('<')
            || s.contains('>');
        if needs_quoting {
            format!("\"{}\"", s.replace('"', "\\\""))
        } else {
            s.to_string()
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
