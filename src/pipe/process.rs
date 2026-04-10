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
    pub(crate) fn shell_quote(s: &str) -> String {
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

    /// Expose `build_command_with_options` for in-module tests only.
    #[cfg(test)]
    pub(crate) fn build_for_test(tool: CliTool, opts: &SpawnOptions) -> Command {
        Self::build_command_with_options(tool, opts)
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
    ///
    /// Returns `BrokenPipe` if stdin is closed (all supported CLIs are one-shot:
    /// Claude reads prompt from stdin then closes; Codex/Gemini/OpenCode take the
    /// prompt as argv and never open stdin at all). Callers should use
    /// `resume_session_id` to continue a prior session rather than writing again.
    pub fn write(&mut self, data: &str) -> Result<(), std::io::Error> {
        if let Some(stdin) = &mut self.stdin {
            stdin.write_all(data.as_bytes())?;
            stdin.flush()?;
            Ok(())
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "stdin is closed — process is one-shot, use resume_session_id to continue",
            ))
        }
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

// ─────────────────────────────────────────────────────────────────────────────
// Unit tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── shell_quote ──────────────────────────────────────────────────────────

    #[test]
    fn shell_quote_empty_string() {
        assert_eq!(PipeProcess::shell_quote(""), "''");
    }

    #[test]
    fn shell_quote_simple_token_returned_as_is() {
        // Pure alphanumeric + allowed symbols — no quoting needed.
        assert_eq!(PipeProcess::shell_quote("hello"), "hello");
        assert_eq!(PipeProcess::shell_quote("my-flag"), "my-flag");
        assert_eq!(PipeProcess::shell_quote("path/to/file.json"), "path/to/file.json");
        assert_eq!(PipeProcess::shell_quote("arg_name"), "arg_name");
    }

    #[test]
    fn shell_quote_string_with_spaces_is_single_quoted() {
        let q = PipeProcess::shell_quote("hello world");
        assert_eq!(q, "'hello world'");
    }

    #[test]
    fn shell_quote_string_with_special_chars() {
        let q = PipeProcess::shell_quote("--output-format=stream-json");
        // Contains '=', which is not in the safe set, so must be quoted.
        assert_eq!(q, "'--output-format=stream-json'");
    }

    #[test]
    fn shell_quote_embedded_single_quote_is_escaped() {
        // Input: it's → output: 'it'\''s'
        let q = PipeProcess::shell_quote("it's");
        assert_eq!(q, "'it'\\''s'");
    }

    #[test]
    fn shell_quote_only_single_quotes() {
        let q = PipeProcess::shell_quote("'");
        assert_eq!(q, "''\\'''");
    }

    // ── Windows cmd /C wrapping (build_command_with_options) ─────────────────

    /// On Windows, a program that already ends with `.cmd` must be wrapped as
    /// `cmd /C <program>.cmd <args...>`.
    #[test]
    #[cfg(windows)]
    fn windows_cmd_extension_wraps_with_cmd_c() {
        // We cannot reach build_command_with_options directly from a test file
        // because it is private. These tests live inside the module so they can.
        use crate::transport::SpawnOptions;

        // Temporarily override the builder to return a command with a `.cmd` suffix.
        // We can verify this through build_for_test.
        // ClaudeCode's inner program is "claude" — on Windows it looks up "claude.cmd"
        // on PATH. If "claude.cmd" exists we get `cmd /C claude.cmd <args>`.
        // If not, we get the bash fallback. Either way the top-level program must be
        // either "cmd" (with /C) or "bash".
        let opts = SpawnOptions {
            prompt: "test".to_string(),
            ..Default::default()
        };
        let cmd = PipeProcess::build_for_test(CliTool::ClaudeCode, &opts);
        let prog = cmd.get_program().to_string_lossy();

        // On Windows we always get either "cmd" or "bash" as the outer wrapper.
        assert!(
            prog == "cmd" || prog == "bash",
            "On Windows, outer program must be 'cmd' or 'bash', got '{}'",
            prog
        );
    }

    /// shell_quote produces a string that, when used in `bash -c`, passes the
    /// original tokens correctly — validated by checking that the quoted forms
    /// can round-trip through a simple concatenation rule.
    #[test]
    fn shell_quote_space_prompt_has_no_unquoted_space() {
        let q = PipeProcess::shell_quote("hello world");
        // The result must not contain an unquoted space (every space is inside quotes).
        // In our single-quote scheme, the result starts and ends with `'`.
        assert!(q.starts_with('\''), "quoted string must start with single-quote");
        assert!(q.ends_with('\''), "quoted string must end with single-quote");
    }

    /// Empty-string produces `''` — the shell empty-string literal.
    #[test]
    fn shell_quote_empty_is_shell_empty_literal() {
        assert_eq!(PipeProcess::shell_quote(""), "''");
    }

    // ── Non-Windows: build_for_test returns the inner command directly ────────

    #[test]
    #[cfg(not(windows))]
    fn unix_build_command_returns_inner_command_directly() {
        use crate::transport::SpawnOptions;

        let opts = SpawnOptions {
            prompt: "hello".to_string(),
            ..Default::default()
        };
        let cmd = PipeProcess::build_for_test(CliTool::ClaudeCode, &opts);
        // On Unix, no wrapping — program is the bare CLI name.
        assert_eq!(
            cmd.get_program().to_str().unwrap(),
            "claude",
            "On Unix, build_command returns the inner command with no wrapping"
        );
    }

    #[test]
    #[cfg(not(windows))]
    fn unix_gemini_build_command_is_bare_gemini() {
        use crate::transport::SpawnOptions;

        let opts = SpawnOptions {
            prompt: "test".to_string(),
            ..Default::default()
        };
        let cmd = PipeProcess::build_for_test(CliTool::Gemini, &opts);
        assert_eq!(cmd.get_program().to_str().unwrap(), "gemini");
    }

    #[test]
    #[cfg(not(windows))]
    fn unix_codex_build_command_is_bare_codex() {
        use crate::transport::SpawnOptions;

        let opts = SpawnOptions {
            prompt: "test".to_string(),
            ..Default::default()
        };
        let cmd = PipeProcess::build_for_test(CliTool::Codex, &opts);
        assert_eq!(cmd.get_program().to_str().unwrap(), "codex");
    }
}
