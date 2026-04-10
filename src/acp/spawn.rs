//! ACP process spawning: spawn spec table and low-level process wrapper.
//!
//! Unlike [`PipeProcess`](crate::pipe::process::PipeProcess) (which closes
//! stdin immediately after writing the initial prompt), `AcpProcess` keeps
//! stdin open for the full session lifetime — required for multi-turn
//! bidirectional JSON-RPC over stdio.

use std::io::Write as _;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use crate::core::types::CliTool;

// ---------------------------------------------------------------------------
// AcpSpawnSpec
// ---------------------------------------------------------------------------

/// Describes how to spawn a CLI tool in ACP mode.
///
/// All fields are `'static` references — no heap allocation at call time.
pub(crate) struct AcpSpawnSpec {
    /// Base program name (e.g. `"gemini"`, `"npx"`).
    pub program: &'static str,
    /// Arguments passed after the program (e.g. `["--experimental-acp"]`).
    pub args: &'static [&'static str],
    /// Whether this is an npm-installed tool that needs `cmd /C` wrapping on Windows.
    pub npm_tool: bool,
}

/// Return the ACP spawn specification for a given CLI tool.
///
/// # Panics
///
/// Does not panic — all `CliTool` variants are handled.
pub(crate) fn acp_command(tool: CliTool) -> AcpSpawnSpec {
    match tool {
        CliTool::Gemini => AcpSpawnSpec {
            program: "gemini",
            args: &["--experimental-acp"],
            npm_tool: false,
        },
        CliTool::OpenCode => AcpSpawnSpec {
            program: "opencode",
            args: &["acp"],
            npm_tool: false,
        },
        CliTool::ClaudeCode => AcpSpawnSpec {
            program: "npx",
            args: &["-y", "@agentclientprotocol/claude-agent-acp"],
            npm_tool: true,
        },
        CliTool::Codex => AcpSpawnSpec {
            program: "npx",
            args: &["@zed-industries/codex-acp"],
            npm_tool: true,
        },
    }
}

// ---------------------------------------------------------------------------
// AcpProcess
// ---------------------------------------------------------------------------

/// Low-level process handle for ACP transport.
///
/// Unlike [`PipeProcess`](crate::pipe::process::PipeProcess), `AcpProcess`
/// keeps `stdin` open for the entire session lifetime so the host can send
/// multiple JSON-RPC requests without respawning.
pub(crate) struct AcpProcess {
    child: Child,
    stdin: std::process::ChildStdin,
    output_rx: Receiver<String>,
}

impl AcpProcess {
    /// Spawn the CLI tool in ACP mode.
    ///
    /// Builds the appropriate `Command` (with Windows `cmd /C` wrapping for
    /// npm-installed tools), sets `stdin`/`stdout` to piped, and starts a
    /// background reader thread on stdout.
    pub(crate) fn spawn(
        tool: CliTool,
        working_dir: &std::path::Path,
        env_vars: &[(String, String)],
    ) -> Result<Self, std::io::Error> {
        let spec = acp_command(tool);
        let mut cmd = build_command(&spec);

        for (key, value) in env_vars {
            cmd.env(key, value);
        }
        cmd.current_dir(working_dir);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::null());

        let mut child = cmd.spawn()?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "no stdin pipe"))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "no stdout pipe"))?;

        let (tx, rx) = mpsc::channel::<String>();
        thread::spawn(move || reader_thread(stdout, tx));

        Ok(Self {
            child,
            stdin,
            output_rx: rx,
        })
    }

    /// Write a line (without trailing newline) followed by `\n` to stdin.
    ///
    /// Returns `BrokenPipe` if the process has already exited and stdin is
    /// closed. The caller maps this to `AcpError::Write`.
    pub(crate) fn write_line(&mut self, line: &str) -> Result<(), std::io::Error> {
        self.stdin.write_all(line.as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        Ok(())
    }

    /// Non-blocking stdout poll. Returns `None` when no line is available.
    pub(crate) fn try_recv(&self) -> Option<String> {
        self.output_rx.try_recv().ok()
    }

    /// Returns `true` if the child process is still running.
    pub(crate) fn is_running(&mut self) -> bool {
        self.child.try_wait().ok().flatten().is_none()
    }

    /// Kill the child process.
    pub(crate) fn kill(&mut self) -> Result<(), std::io::Error> {
        self.child.kill()
    }

    /// Collect the exit code via `try_wait`. Falls back to `0` on any error
    /// or if the process has not yet exited.
    pub(crate) fn exit_code(&mut self) -> i32 {
        self.child
            .try_wait()
            .ok()
            .flatten()
            .and_then(|s| s.code())
            .unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// Build the OS-appropriate Command
// ---------------------------------------------------------------------------

/// Build a `Command` using the same Windows `cmd /C` wrapping logic as
/// `PipeProcess::build_command_with_options`.
fn build_command(spec: &AcpSpawnSpec) -> Command {
    if cfg!(windows) {
        build_command_windows(spec)
    } else {
        build_command_unix(spec)
    }
}

fn build_command_unix(spec: &AcpSpawnSpec) -> Command {
    let mut cmd = Command::new(spec.program);
    for arg in spec.args {
        cmd.arg(arg);
    }
    cmd
}

fn build_command_windows(spec: &AcpSpawnSpec) -> Command {
    // npm-installed tools always have a `.cmd` wrapper on Windows.
    if spec.npm_tool {
        let cmd_name = format!("{}.cmd", spec.program);
        let mut cmd = Command::new("cmd");
        cmd.arg("/C").arg(&cmd_name);
        for arg in spec.args {
            cmd.arg(arg);
        }
        return cmd;
    }

    let program = spec.program;

    // Check if a `.cmd` wrapper exists on PATH for non-npm tools.
    let cmd_name = format!("{}.cmd", program);
    let has_cmd = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path).any(|dir| dir.join(&cmd_name).is_file())
        })
        .unwrap_or(false);

    if has_cmd {
        let mut cmd = Command::new("cmd");
        cmd.arg("/C").arg(&cmd_name);
        for arg in spec.args {
            cmd.arg(arg);
        }
        cmd
    } else {
        // No `.cmd` wrapper — invoke via `bash -c "program arg1 arg2 ..."`.
        let mut cmd = Command::new("bash");
        cmd.arg("-c");
        let mut shell_str = shell_quote(program);
        for arg in spec.args {
            shell_str.push(' ');
            shell_str.push_str(&shell_quote(arg));
        }
        cmd.arg(&shell_str);
        cmd
    }
}

/// Single-quote a token for POSIX shell (`bash -c`).
fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    if s.chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '/')
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

// ---------------------------------------------------------------------------
// Stdout reader thread
// ---------------------------------------------------------------------------

fn reader_thread(stdout: std::process::ChildStdout, tx: Sender<String>) {
    use std::io::{BufRead, BufReader};

    let reader = BufReader::new(stdout);
    for line in reader.lines() {
        match line {
            Ok(l) => {
                if tx.send(format!("{}\n", l)).is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acp_command_gemini() {
        let spec = acp_command(CliTool::Gemini);
        assert_eq!(spec.program, "gemini");
        assert_eq!(spec.args, &["--experimental-acp"]);
        assert!(!spec.npm_tool);
    }

    #[test]
    fn acp_command_opencode() {
        let spec = acp_command(CliTool::OpenCode);
        assert_eq!(spec.program, "opencode");
        assert_eq!(spec.args, &["acp"]);
        assert!(!spec.npm_tool);
    }

    #[test]
    fn acp_command_claude_code() {
        let spec = acp_command(CliTool::ClaudeCode);
        assert_eq!(spec.program, "npx");
        assert!(spec.npm_tool);
        assert!(spec.args.contains(&"@agentclientprotocol/claude-agent-acp"));
    }

    #[test]
    fn acp_command_codex() {
        let spec = acp_command(CliTool::Codex);
        assert_eq!(spec.program, "npx");
        assert!(spec.npm_tool);
        assert!(spec.args.contains(&"@zed-industries/codex-acp"));
    }

    #[test]
    fn shell_quote_empty() {
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn shell_quote_simple() {
        assert_eq!(shell_quote("cursor-agent"), "cursor-agent");
    }

    #[test]
    fn shell_quote_with_spaces() {
        let q = shell_quote("hello world");
        assert_eq!(q, "'hello world'");
    }
}
