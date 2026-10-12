//! Last resort: start a NEW provider process that resumes the session for
//! one turn. Only for a session marked headless (no client holds it open).
//! A session that has a live client is never spawned into: two writers on
//! one transcript is how sessions get corrupted.
//!
//! | Kind | Command (prompt never in argv when the CLI reads stdin) |
//! |---|---|
//! | grok/local | `grok --resume <id> -p <prompt>` |
//! | codex/local | `codex exec resume <id> -` (prompt on stdin) |
//! | kimi_code/local | `kimi -S <id> -p <prompt>` |
//! | claude_code/local | `claude --resume <id> -p` (prompt on stdin) |
//! | cursor/local | `agent --resume <id> -p <prompt>` |
//! | codex/web | `codex cloud exec --env <env> <prompt>` (new cloud task) |
//!
//! The binary is `M4A_<PROVIDER>_BIN` or the vendor name on `PATH`.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use super::{
    wake_prompt, ProviderKind, ProviderSession, SessionKind, Surface, WakeAdapter, WakeError,
    WakeLetter, WakeOutcome,
};

/// Env var naming the cloud environment for `codex cloud exec --env`.
pub const CODEX_CLOUD_ENV_ENV: &str = "M4A_CODEX_CLOUD_ENV";

/// Env override for a provider binary (`M4A_CODEX_BIN`, ...).
pub fn bin_env(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Grok => "M4A_GROK_BIN",
        ProviderKind::KimiCode => "M4A_KIMI_BIN",
        ProviderKind::ClaudeCode => "M4A_CLAUDE_BIN",
        ProviderKind::Codex => "M4A_CODEX_BIN",
        ProviderKind::Cursor => "M4A_CURSOR_AGENT_BIN",
    }
}

/// Default program name on `PATH`.
pub fn default_program(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Grok => "grok",
        ProviderKind::KimiCode => "kimi",
        ProviderKind::ClaudeCode => "claude",
        ProviderKind::Codex => "codex",
        ProviderKind::Cursor => "agent",
    }
}

/// Argv (after the program) and the stdin payload for one resume turn.
pub fn resume_command(
    kind: SessionKind,
    session_id: &str,
    prompt: &str,
    cloud_env: Option<&str>,
) -> Result<(Vec<String>, Option<String>), WakeError> {
    let id = session_id.to_string();
    let p = prompt.to_string();
    Ok(match (kind.surface, kind.provider) {
        (Surface::Local, ProviderKind::Grok) => (vec!["--resume".into(), id, "-p".into(), p], None),
        (Surface::Local, ProviderKind::Codex) => (
            vec!["exec".into(), "resume".into(), id, "-".into()],
            Some(p),
        ),
        (Surface::Local, ProviderKind::KimiCode) => (vec!["-S".into(), id, "-p".into(), p], None),
        (Surface::Local, ProviderKind::ClaudeCode) => {
            (vec!["--resume".into(), id, "-p".into()], Some(p))
        }
        (Surface::Local, ProviderKind::Cursor) => {
            (vec!["--resume".into(), id, "-p".into(), p], None)
        }
        (Surface::Web, ProviderKind::Codex) => {
            let Some(env) = cloud_env.filter(|env| !env.is_empty()) else {
                return Err(WakeError::Unavailable(format!(
                    "{CODEX_CLOUD_ENV_ENV} unset"
                )));
            };
            (
                vec![
                    "cloud".into(),
                    "exec".into(),
                    "--env".into(),
                    env.to_string(),
                    p,
                ],
                None,
            )
        }
        _ => return Err(WakeError::NoInboundTrigger(kind)),
    })
}

/// Spawns one resume turn for a headless session. Does not wait for it.
pub struct ResumeSpawnAdapter {
    kind: SessionKind,
    program: Option<PathBuf>,
    cloud_env: Option<String>,
}

impl ResumeSpawnAdapter {
    /// Adapter for `kind`. `program` overrides the binary.
    pub fn new(kind: SessionKind, program: Option<PathBuf>, cloud_env: Option<String>) -> Self {
        Self {
            kind,
            program,
            cloud_env,
        }
    }

    /// Program from `M4A_<PROVIDER>_BIN` or `PATH`.
    pub fn from_env(kind: SessionKind) -> Self {
        let program = std::env::var_os(bin_env(kind.provider))
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let cloud_env = std::env::var(CODEX_CLOUD_ENV_ENV)
            .ok()
            .filter(|v| !v.is_empty());
        Self::new(kind, program, cloud_env)
    }

    fn program(&self) -> PathBuf {
        self.program
            .clone()
            .unwrap_or_else(|| PathBuf::from(default_program(self.kind.provider)))
    }
}

impl WakeAdapter for ResumeSpawnAdapter {
    fn kind(&self) -> SessionKind {
        self.kind
    }

    fn probe(&self, session: &ProviderSession) -> Result<(), WakeError> {
        if !session.headless {
            return Err(WakeError::Unavailable(
                "session has a live client; resume spawn is headless-only".into(),
            ));
        }
        resume_command(
            self.kind,
            &session.session_id,
            "",
            self.cloud_env.as_deref(),
        )
        .map(|_| ())
    }

    fn wake(
        &mut self,
        session: &ProviderSession,
        letter: &WakeLetter<'_>,
    ) -> Result<WakeOutcome, WakeError> {
        self.probe(session)?;
        let prompt = wake_prompt(session, letter);
        let (args, stdin) = resume_command(
            self.kind,
            &session.session_id,
            &prompt,
            self.cloud_env.as_deref(),
        )?;
        let mut command = Command::new(self.program());
        command
            .args(&args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(cwd) = session.cwd.as_deref().filter(|cwd| cwd.is_dir()) {
            command.current_dir(cwd);
        }
        let mut child = command
            .spawn()
            .map_err(|err| WakeError::Unavailable(format!("spawn: {}", err.kind())))?;
        if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
            pipe.write_all(text.as_bytes())
                .map_err(|err| WakeError::Transport(format!("spawn stdin: {}", err.kind())))?;
        }
        // Reap in the background; the turn runs on its own.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(WakeOutcome::Delivered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{letter, session};

    #[test]
    fn commands_keep_prompt_off_argv_where_the_cli_reads_stdin() {
        let (args, stdin) =
            resume_command(SessionKind::local(ProviderKind::Codex), "t1", "hi", None).unwrap();
        assert_eq!(args, ["exec", "resume", "t1", "-"]);
        assert_eq!(stdin.as_deref(), Some("hi"));
        let (args, stdin) = resume_command(
            SessionKind::local(ProviderKind::ClaudeCode),
            "c1",
            "hi",
            None,
        )
        .unwrap();
        assert!(!args.iter().any(|a| a == "hi") && stdin.is_some());
        assert!(resume_command(SessionKind::web(ProviderKind::Codex), "x", "hi", None).is_err());
        assert!(resume_command(SessionKind::web(ProviderKind::KimiCode), "x", "hi", None).is_err());
    }

    #[test]
    fn never_spawns_into_a_session_with_a_live_client() {
        let kind = SessionKind::local(ProviderKind::KimiCode);
        let mut adapter = ResumeSpawnAdapter::new(kind, Some("/bin/false".into()), None);
        let s = session(kind);
        assert!(!s.headless);
        let err = adapter.wake(&s, &letter("x")).unwrap_err().to_string();
        assert!(err.contains("headless-only"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn headless_session_gets_one_resume_process() {
        let dir = std::env::temp_dir().join(format!("m4a-spawn-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let out = dir.join("argv");
        let script = dir.join("fake-codex");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"$@\" > {0}.tmp\ncat >> {0}.tmp\nmv {0}.tmp {0}\n",
                out.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let kind = SessionKind::local(ProviderKind::Codex);
        let mut adapter = ResumeSpawnAdapter::new(kind, Some(script), None);
        let mut s = session(kind);
        s.headless = true;
        assert_eq!(
            adapter.wake(&s, &letter("ping")).unwrap(),
            WakeOutcome::Delivered
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !out.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let text = std::fs::read_to_string(&out).unwrap();
        assert!(text.starts_with("exec resume s-1 -"));
        assert!(text.trim_end().ends_with("ping"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
