//! ACP process spawning: spawn spec table and low-level process wrapper.
//!
//! Unlike [`PipeProcess`](crate::pipe::process::PipeProcess) (which closes
//! stdin immediately after writing the initial prompt), `AcpProcess` keeps
//! stdin open for the full session lifetime — required for multi-turn
//! bidirectional JSON-RPC over stdio.

use std::collections::VecDeque;
use std::io::Write as _;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::core::types::CliTool;
use gate4agent_types::LaunchSpec;

// ---------------------------------------------------------------------------
// AcpSpawnSpec
// ---------------------------------------------------------------------------

/// Describes how to spawn a CLI tool in ACP mode.
///
/// All fields are `'static` references — no heap allocation at call time.
pub(crate) struct AcpSpawnSpec {
    /// Base program name (e.g. `"grok"`, `"npx"`).
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
pub(crate) fn acp_command(tool: CliTool) -> Result<AcpSpawnSpec, std::io::Error> {
    let spec = match tool {
        // Both adapters moved from the `@zed-industries` scope to
        // `@agentclientprotocol`, and the abandoned packages are still
        // published -- 0.16.2 against 0.71.0 for Claude, 0.16.0 against
        // 1.8.0 for Codex -- so pointing at the old scope installs
        // something that runs, lags badly, and never says why. Verified
        // against the registry rather than taken from a summary: the old
        // names carry deprecation notices naming these as replacements.
        CliTool::ClaudeCode => AcpSpawnSpec {
            program: "npx",
            args: &["-y", "@agentclientprotocol/claude-agent-acp"],
            npm_tool: true,
        },
        CliTool::Codex => AcpSpawnSpec {
            program: "npx",
            args: &["-y", "@agentclientprotocol/codex-acp"],
            npm_tool: true,
        },
        CliTool::Grok => AcpSpawnSpec {
            program: "grok",
            args: &["agent", "stdio"],
            npm_tool: false,
        },
        CliTool::KimiCode => AcpSpawnSpec {
            program: "kimi",
            args: &["acp"],
            npm_tool: false,
        },
    };
    Ok(spec)
}

/// Argv approval flags reaching an ACP-spawned process, for ANY spec --
/// always empty, unconditionally.
///
/// This used to depend on `spec.npm_tool`: `grok` and `kimi` (`npm_tool:
/// false`) spawn the vendor's own binary directly, so `approval_args` reached
/// their real argv, while `claude` and `codex` (`npm_tool: true`) spawn an
/// `npx`-installed adapter-wrapper package whose trailing-argv forwarding was
/// never verified, so they got an empty slice instead. That split is retired
/// now that ACP has exactly one mechanism for applying an approval level,
/// full stop: `session/set_mode`, driven from
/// `gate4agent_catalog::approval_level_resolution`'s own `acp_mode_id`
/// column (`ModeId`) by the caller that spawns a session
/// (`gate4agent-shell-native`'s `apply_acp_approval_mode`), never by argv.
/// Measured live, the wrapper-fronted providers (`claude`, `codex`) never
/// received a flag either way and came up in the agent's own default mode
/// regardless (`mode:Mode="auto"`) -- proof that the argv mechanism was
/// fiction for ACP even where a spec's `npm_tool` check let it through, not
/// just where it blocked it. `grok` and `kimi` spawning their own binary
/// changes nothing about that: "the binary is the vendor's own" was never
/// the reason argv reached a session mode over ACP, only the reason it
/// reached that binary's OWN argv parser, which is a different question
/// from whether the resulting session actually landed in the requested
/// mode.
///
/// Kept as a real function (not simply inlined at the one call site,
/// `AcpProcess::spawn`) so the intent -- and the reason it changed -- has
/// somewhere to live, and so a caller that later has a real, argv-shaped use
/// for `AcpSpawnSpec`/`approval_args` again finds the seam already there.
pub(crate) fn applicable_approval_args<'a>(
    _spec: &AcpSpawnSpec,
    _approval_args: &'a [String],
) -> &'a [String] {
    &[]
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
    /// Bounded ring buffer of the most recent stderr lines. Populated by a
    /// background reader thread; read out via [`stderr_tail`](Self::stderr_tail)
    /// to enrich handshake-failure diagnostics when the process exits before
    /// producing any valid JSON-RPC line on stdout (e.g. an unauthenticated
    /// CLI that prints a login prompt and exits).
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
}

/// Maximum number of trailing stderr lines retained per ACP process.
const ACP_STDERR_TAIL_MAX_LINES: usize = 20;

impl AcpProcess {
    /// Spawn the CLI tool in ACP mode.
    ///
    /// Builds the appropriate `Command` (with Windows `cmd /C` wrapping for
    /// npm-installed tools), sets `stdin`/`stdout` to piped, and starts a
    /// background reader thread on stdout.
    ///
    /// `approval_args` is accepted for signature compatibility with
    /// callers that still resolve `gate4agent_catalog::approval_level_args`
    /// for other purposes, but `applicable_approval_args` now discards it
    /// unconditionally: ACP applies an approval level exclusively through
    /// `session/set_mode`, never argv (see `applicable_approval_args`'s own
    /// doc comment).
    pub(crate) fn spawn(
        tool: CliTool,
        working_dir: &std::path::Path,
        env_vars: &[(String, String)],
        approval_args: &[String],
    ) -> Result<Self, std::io::Error> {
        let spec = acp_command(tool)?;
        let extra_args = applicable_approval_args(&spec, approval_args);
        let mut cmd = build_command(&spec, extra_args);

        for (key, value) in env_vars {
            cmd.env(key, value);
        }
        cmd.current_dir(working_dir);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child = cmd.spawn()?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "no stdin pipe"))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "no stdout pipe"))?;

        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "no stderr pipe"))?;

        let (tx, rx) = mpsc::channel::<String>();
        thread::spawn(move || reader_thread(stdout, tx));

        let stderr_tail = Arc::new(Mutex::new(VecDeque::with_capacity(ACP_STDERR_TAIL_MAX_LINES)));
        let stderr_tail_writer = Arc::clone(&stderr_tail);
        thread::spawn(move || stderr_reader_thread(stderr, stderr_tail_writer));

        Ok(Self {
            child,
            stdin,
            output_rx: rx,
            stderr_tail,
        })
    }

    pub(crate) fn spawn_with_launch(
        working_dir: &std::path::Path,
        env_vars: &[(String, String)],
        launch: &LaunchSpec,
    ) -> Result<Self, std::io::Error> {
        let mut cmd = Command::new(&launch.program);
        cmd.args(&launch.fixed_args);
        for (key, value) in env_vars {
            cmd.env(key, value);
        }
        cmd.current_dir(working_dir);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        let mut child = cmd.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "no stdin pipe"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "no stdout pipe"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "no stderr pipe"))?;
        let (tx, rx) = mpsc::channel::<String>();
        thread::spawn(move || reader_thread(stdout, tx));

        let stderr_tail = Arc::new(Mutex::new(VecDeque::with_capacity(ACP_STDERR_TAIL_MAX_LINES)));
        let stderr_tail_writer = Arc::clone(&stderr_tail);
        thread::spawn(move || stderr_reader_thread(stderr, stderr_tail_writer));

        Ok(Self {
            child,
            stdin,
            output_rx: rx,
            stderr_tail,
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

    pub(crate) fn process_id(&self) -> u32 {
        self.child.id()
    }

    /// Snapshot of the most recent stderr lines, oldest first.
    ///
    /// Used to enrich handshake-failure diagnostics: a process that exits
    /// before writing any valid JSON-RPC line to stdout (e.g. because the
    /// vendor CLI is not authenticated) otherwise surfaces only a generic
    /// "session closed" error with no indication of the underlying cause.
    pub(crate) fn stderr_tail(&self) -> Vec<String> {
        self.stderr_tail
            .lock()
            .map(|guard| guard.iter().cloned().collect())
            .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Build the OS-appropriate Command
// ---------------------------------------------------------------------------

/// Build a `Command` using the same Windows `cmd /C` wrapping logic as
/// `PipeProcess::build_command_with_options`.
///
/// `extra_args` are appended after `spec.args` -- callers are expected to
/// have already applied `applicable_approval_args` (an empty slice is a
/// no-op here either way).
fn build_command(spec: &AcpSpawnSpec, extra_args: &[String]) -> Command {
    if cfg!(windows) {
        build_command_windows(spec, extra_args)
    } else {
        build_command_unix(spec, extra_args)
    }
}

fn build_command_unix(spec: &AcpSpawnSpec, extra_args: &[String]) -> Command {
    let mut cmd = Command::new(spec.program);
    for arg in spec.args {
        cmd.arg(arg);
    }
    for arg in extra_args {
        cmd.arg(arg);
    }
    cmd
}

fn build_command_windows(spec: &AcpSpawnSpec, extra_args: &[String]) -> Command {
    // npm-installed tools always have a `.cmd` wrapper on Windows.
    if spec.npm_tool {
        return windows_cmd_wrapper(&format!("{}.cmd", spec.program), spec.args, extra_args);
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
        windows_cmd_wrapper(&cmd_name, spec.args, extra_args)
    } else {
        // No `.cmd` wrapper — invoke via `bash -c "program arg1 arg2 ..."`.
        windows_bash_fallback(program, spec.args, extra_args)
    }
}

/// `cmd /C <cmd_name> <args...> <extra_args...>` -- each argument is its own
/// `Command::arg`, so no shell-quoting is needed here.
fn windows_cmd_wrapper(cmd_name: &str, args: &[&str], extra_args: &[String]) -> Command {
    let mut cmd = Command::new("cmd");
    cmd.arg("/C").arg(cmd_name);
    for arg in args {
        cmd.arg(arg);
    }
    for arg in extra_args {
        cmd.arg(arg);
    }
    cmd
}

/// `bash -c "program arg1 arg2 ... extra1 extra2 ..."` -- unlike
/// `windows_cmd_wrapper`, every argument (including `extra_args`) has to be
/// folded into the SAME single shell-quoted string, because that string is
/// itself the only argument `bash -c` receives; appending `extra_args` as
/// further `Command::arg` calls after this one would hand them to `bash` as
/// its own positional parameters (`$0`, `$1`, ...) rather than to `program`.
fn windows_bash_fallback(program: &str, args: &[&str], extra_args: &[String]) -> Command {
    let mut cmd = Command::new("bash");
    cmd.arg("-c");
    let mut shell_str = shell_quote(program);
    for arg in args {
        shell_str.push(' ');
        shell_str.push_str(&shell_quote(arg));
    }
    for arg in extra_args {
        shell_str.push(' ');
        shell_str.push_str(&shell_quote(arg));
    }
    cmd.arg(&shell_str);
    cmd
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

/// Reads stderr lines into a bounded ring buffer, oldest lines dropped first.
///
/// Runs for the lifetime of the child process's stderr pipe; exits when the
/// pipe closes (process exit) or a read error occurs.
fn stderr_reader_thread(stderr: std::process::ChildStderr, tail: Arc<Mutex<VecDeque<String>>>) {
    use std::io::{BufRead, BufReader};

    let reader = BufReader::new(stderr);
    for line in reader.lines() {
        let Ok(line) = line else { break };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(mut buffer) = tail.lock() {
            if buffer.len() >= ACP_STDERR_TAIL_MAX_LINES {
                buffer.pop_front();
            }
            buffer.push_back(trimmed.to_owned());
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
    fn acp_command_claude_code() {
        let spec = acp_command(CliTool::ClaudeCode).unwrap();
        assert_eq!(spec.program, "npx");
        assert!(spec.npm_tool);
        assert!(spec.args.contains(&"@agentclientprotocol/claude-agent-acp"));
    }

    #[test]
    fn acp_command_codex() {
        let spec = acp_command(CliTool::Codex).unwrap();
        assert_eq!(spec.program, "npx");
        assert!(spec.npm_tool);
        assert!(spec.args.contains(&"@agentclientprotocol/codex-acp"));
    }

    #[test]
    fn acp_command_grok() {
        let spec = acp_command(CliTool::Grok).unwrap();
        assert_eq!(spec.program, "grok");
        assert_eq!(spec.args, &["agent", "stdio"]);
        assert!(!spec.npm_tool);
    }

    #[test]
    fn acp_command_kimi_is_native() {
        let spec = acp_command(CliTool::KimiCode).unwrap();
        assert_eq!(spec.program, "kimi");
        assert_eq!(spec.args, &["acp"]);
        assert!(!spec.npm_tool);
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

    // -----------------------------------------------------------------------
    // applicable_approval_args -- always empty, for every spec, over ACP
    // -----------------------------------------------------------------------

    #[test]
    fn direct_binary_specs_no_longer_receive_approval_args_over_acp_argv() {
        // `grok`/`kimi` spawn the vendor's own binary directly -- this used
        // to mean `approval_args` reached its real argv unchanged. That
        // mechanism is retired: ACP applies a level exclusively through
        // `session/set_mode` now, for every provider, so even a
        // non-adapter-wrapped spec gets nothing here any more.
        for tool in [CliTool::Grok, CliTool::KimiCode] {
            let spec = acp_command(tool).unwrap();
            assert!(!spec.npm_tool, "{tool}");
            let args = vec!["--some-flag".to_owned(), "value".to_owned()];
            assert!(
                applicable_approval_args(&spec, &args).is_empty(),
                "{tool}: a direct-binary spec must not receive an approval flag over ACP argv any more"
            );
        }
    }

    #[test]
    fn adapter_wrapped_specs_never_receive_approval_args() {
        for tool in [CliTool::ClaudeCode, CliTool::Codex] {
            let spec = acp_command(tool).unwrap();
            assert!(spec.npm_tool, "{tool}");
            let args = vec!["--some-flag".to_owned(), "value".to_owned()];
            assert!(
                applicable_approval_args(&spec, &args).is_empty(),
                "{tool}: adapter-wrapped spec must never receive an approval flag"
            );
        }
    }

    /// End-to-end table: `gate4agent_catalog::approval_level_args`'s own
    /// verified flag table, filtered through `applicable_approval_args`, for
    /// every provider at every `ApprovalLevel` -- exactly the composition
    /// `AcpProcess::spawn` performs. The catalog's table itself still has
    /// real, non-empty entries (e.g. claude `FullAuto` ->
    /// `["--permission-mode", "bypassPermissions"]`); this proves none of
    /// them ever reach ACP argv regardless, for any provider or level --
    /// the level is applied through `session/set_mode` instead
    /// (`gate4agent-shell-native`'s `apply_acp_approval_mode`), one
    /// mechanism per transport.
    #[test]
    fn approval_flags_never_reach_acp_argv_for_any_provider_at_any_level() {
        use gate4agent_catalog::{approval_level_args, AgentId};
        use gate4agent_types::ApprovalLevel;

        let providers: [(&str, CliTool); 4] = [
            ("claude", CliTool::ClaudeCode),
            ("codex", CliTool::Codex),
            ("grok", CliTool::Grok),
            ("kimi", CliTool::KimiCode),
        ];
        let levels = [
            ApprovalLevel::FullAuto,
            ApprovalLevel::Moderate,
            ApprovalLevel::ReadOnly,
            ApprovalLevel::Unmanaged,
        ];
        for (provider_id, tool) in providers {
            let agent = AgentId::new(provider_id).unwrap();
            let spec = acp_command(tool).unwrap();
            for level in levels {
                let table_args = approval_level_args(&agent, level);
                let applied = applicable_approval_args(&spec, &table_args);
                assert!(
                    applied.is_empty(),
                    "{provider_id} at {level:?} must never see an ACP argv approval flag, \
                     even though the catalog's own table has one: {table_args:?}"
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // build_command / windows_cmd_wrapper / windows_bash_fallback
    // -----------------------------------------------------------------------

    fn command_args(cmd: &Command) -> Vec<String> {
        cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn build_command_unix_appends_extra_args_after_spec_args() {
        let spec = acp_command(CliTool::Grok).unwrap();
        let extra = vec!["--permission-mode".to_owned(), "bypassPermissions".to_owned()];
        let cmd = build_command_unix(&spec, &extra);
        assert_eq!(cmd.get_program(), std::ffi::OsStr::new("grok"));
        assert_eq!(
            command_args(&cmd),
            ["agent", "stdio", "--permission-mode", "bypassPermissions"]
        );
    }

    #[test]
    fn build_command_unix_with_no_extra_args_is_unchanged() {
        let spec = acp_command(CliTool::KimiCode).unwrap();
        let cmd = build_command_unix(&spec, &[]);
        assert_eq!(command_args(&cmd), ["acp"]);
    }

    #[test]
    fn windows_cmd_wrapper_appends_extra_args_after_spec_args() {
        let extra = vec!["--yolo".to_owned()];
        let cmd = windows_cmd_wrapper("kimi.cmd", &["acp"], &extra);
        assert_eq!(cmd.get_program(), std::ffi::OsStr::new("cmd"));
        assert_eq!(command_args(&cmd), ["/C", "kimi.cmd", "acp", "--yolo"]);
    }

    #[test]
    fn windows_cmd_wrapper_for_an_npm_tool_ignores_an_empty_extra_args() {
        let cmd = windows_cmd_wrapper(
            "npx.cmd",
            &["-y", "@agentclientprotocol/claude-agent-acp"],
            &[],
        );
        assert_eq!(cmd.get_program(), std::ffi::OsStr::new("cmd"));
        assert_eq!(
            command_args(&cmd),
            ["/C", "npx.cmd", "-y", "@agentclientprotocol/claude-agent-acp"]
        );
    }

    /// The fallback path folds every argument -- including `extra_args` --
    /// into the SAME single shell string, because that string is the only
    /// argument `bash -c` itself receives. This is the regression the
    /// `windows_bash_fallback` split guards: appending `extra_args` as
    /// further `Command::arg` calls after building this string (instead of
    /// folding them in) would hand them to `bash` as `$0`/`$1`/... rather
    /// than to `program`.
    #[test]
    fn windows_bash_fallback_folds_extra_args_into_the_single_shell_string() {
        let extra = vec!["--permission-mode".to_owned(), "bypassPermissions".to_owned()];
        let cmd = windows_bash_fallback("grok", &["agent", "stdio"], &extra);
        assert_eq!(cmd.get_program(), std::ffi::OsStr::new("bash"));
        assert_eq!(
            command_args(&cmd),
            ["-c", "grok agent stdio --permission-mode bypassPermissions"]
        );
    }

    #[test]
    fn windows_bash_fallback_quotes_an_extra_arg_that_needs_it() {
        let extra = vec!["hello world".to_owned()];
        let cmd = windows_bash_fallback("kimi", &["acp"], &extra);
        assert_eq!(command_args(&cmd), ["-c", "kimi acp 'hello world'"]);
    }
}
