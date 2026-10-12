//! Hook doorbells: the session's own hook process picks the letter up.
//!
//! Every provider CLI runs hooks inside the open session. `m4a-inbox` is
//! the hook command; this adapter only queues the letter in the session's
//! inbox when such a hook is installed (armed) or running (live):
//!
//! | Flavor | Hook | Turn starts | Consumer name |
//! |---|---|---|---|
//! | `claude-rewake` | Claude `Stop`/`SessionStart` with `asyncRewake: true` running `m4a-inbox wait` | immediately, idle session (exit 2 wakes Claude) | live `claude-rewake` |
//! | `claude-stop` | Claude `Stop` running `m4a-inbox drain --format claude-stop` | at the end of the current turn (`decision: block`) | armed `stop-claude-stop` |
//! | `codex-stop` | Codex `Stop` hook, same JSON | end of current turn | armed `stop-codex-stop` |
//! | `kimi-stop` | Kimi `[[hooks]] event = "Stop"`, exit 2 + stderr | end of current turn | armed `stop-kimi-stop` |
//! | `cursor-stop` | Cursor `stop` hook, `followup_message` | end of current turn (auto-submitted) | armed `stop-cursor-stop` |
//! | `grok-stop` | Grok Build `Stop` hook (Claude-compatible JSON) | end of current turn | armed `stop-grok-stop` |
//!
//! A Stop-hook doorbell cannot start a turn in an idle session; it is the
//! fallback after the in-session primary. The letter stays durable in the
//! inbox either way.

use std::path::PathBuf;

use super::inbox;
use super::{
    ProviderKind, ProviderSession, SessionKind, WakeAdapter, WakeError, WakeLetter, WakeOutcome,
};

/// Which hook drains the inbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookFlavor {
    /// Claude `asyncRewake` waiter (`m4a-inbox wait`): wakes an idle session.
    ClaudeRewake,
    /// Claude `Stop` hook, `{"decision":"block","reason":...}`.
    ClaudeStop,
    /// Codex `Stop` hook, same JSON as Claude.
    CodexStop,
    /// Kimi `Stop` hook, exit 2 with the prompt on stderr.
    KimiStop,
    /// Cursor `stop` hook, `{"followup_message":...}`.
    CursorStop,
    /// Grok Build `Stop` hook (Claude-compatible).
    GrokStop,
}

impl HookFlavor {
    /// Every flavor.
    pub const ALL: [HookFlavor; 6] = [
        HookFlavor::ClaudeRewake,
        HookFlavor::ClaudeStop,
        HookFlavor::CodexStop,
        HookFlavor::KimiStop,
        HookFlavor::CursorStop,
        HookFlavor::GrokStop,
    ];

    /// `m4a-inbox --format` value.
    pub fn id(self) -> &'static str {
        match self {
            HookFlavor::ClaudeRewake => "claude-rewake",
            HookFlavor::ClaudeStop => "claude-stop",
            HookFlavor::CodexStop => "codex-stop",
            HookFlavor::KimiStop => "kimi-stop",
            HookFlavor::CursorStop => "cursor-stop",
            HookFlavor::GrokStop => "grok-stop",
        }
    }

    /// Parses [`Self::id`].
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|flavor| flavor.id() == value)
    }

    /// Provider whose hook this is.
    pub fn provider(self) -> ProviderKind {
        match self {
            HookFlavor::ClaudeRewake | HookFlavor::ClaudeStop => ProviderKind::ClaudeCode,
            HookFlavor::CodexStop => ProviderKind::Codex,
            HookFlavor::KimiStop => ProviderKind::KimiCode,
            HookFlavor::CursorStop => ProviderKind::Cursor,
            HookFlavor::GrokStop => ProviderKind::Grok,
        }
    }

    /// Whether the hook wakes an idle session (true) or only continues the
    /// current turn (false).
    pub fn wakes_idle(self) -> bool {
        matches!(self, HookFlavor::ClaudeRewake)
    }

    /// `.live/` name of the consumer.
    pub fn consumer(self) -> String {
        match self {
            HookFlavor::ClaudeRewake => inbox::consumer::CLAUDE_REWAKE.to_string(),
            other => inbox::consumer::stop(other.id()),
        }
    }

    /// Whether that consumer is present for `dir`.
    pub fn present(self, dir: &std::path::Path) -> bool {
        if self.wakes_idle() {
            inbox::is_live(dir, &self.consumer())
        } else {
            inbox::is_armed(dir, &self.consumer())
        }
    }

    /// What the hook prints for drained `prompts`: `(stdout, stderr, exit)`.
    /// `None` when there is nothing to say (exit 0, no output).
    pub fn render(self, prompts: &[String]) -> Option<(String, String, i32)> {
        if prompts.is_empty() {
            return None;
        }
        let text = prompts.join("\n\n");
        Some(match self {
            HookFlavor::ClaudeRewake | HookFlavor::KimiStop => (String::new(), text, 2),
            HookFlavor::ClaudeStop | HookFlavor::CodexStop | HookFlavor::GrokStop => (
                serde_json::json!({"decision": "block", "reason": text}).to_string(),
                String::new(),
                0,
            ),
            HookFlavor::CursorStop => (
                serde_json::json!({"followup_message": text}).to_string(),
                String::new(),
                0,
            ),
        })
    }
}

/// Queues a letter for a hook that is installed in the open session.
pub struct InboxHookAdapter {
    kind: SessionKind,
    flavor: HookFlavor,
    inbox_dir: Option<PathBuf>,
}

impl InboxHookAdapter {
    /// Adapter for `flavor` on `kind`, draining `inbox_dir`.
    pub fn new(kind: SessionKind, flavor: HookFlavor, inbox_dir: Option<PathBuf>) -> Self {
        Self {
            kind,
            flavor,
            inbox_dir,
        }
    }

    /// The hook flavor.
    pub fn flavor(&self) -> HookFlavor {
        self.flavor
    }
}

impl WakeAdapter for InboxHookAdapter {
    fn kind(&self) -> SessionKind {
        self.kind
    }

    fn probe(&self, _session: &ProviderSession) -> Result<(), WakeError> {
        let Some(dir) = self.inbox_dir.as_deref() else {
            return Err(WakeError::Unavailable("inbox dir unset".into()));
        };
        if self.flavor.present(dir) {
            Ok(())
        } else {
            Err(WakeError::Unavailable(format!(
                "{} hook not installed in the session",
                self.flavor.id()
            )))
        }
    }

    fn wake(
        &mut self,
        session: &ProviderSession,
        letter: &WakeLetter<'_>,
    ) -> Result<WakeOutcome, WakeError> {
        self.probe(session)?;
        let dir = self.inbox_dir.as_deref().expect("probed");
        inbox::write_letter(dir, session, letter).map(WakeOutcome::Queued)
    }
}

/// Last link of every local chain: keep the letter durable in the inbox
/// even when no consumer is present, so the next hook run or channel
/// start delivers it. Reported as [`WakeOutcome::Queued`].
pub struct InboxQueueAdapter {
    kind: SessionKind,
    inbox_dir: Option<PathBuf>,
}

impl InboxQueueAdapter {
    /// Queue into `inbox_dir`.
    pub fn new(kind: SessionKind, inbox_dir: Option<PathBuf>) -> Self {
        Self { kind, inbox_dir }
    }
}

impl WakeAdapter for InboxQueueAdapter {
    fn kind(&self) -> SessionKind {
        self.kind
    }

    fn probe(&self, _session: &ProviderSession) -> Result<(), WakeError> {
        match self.inbox_dir {
            Some(_) => Ok(()),
            None => Err(WakeError::Unavailable("inbox dir unset".into())),
        }
    }

    fn wake(
        &mut self,
        session: &ProviderSession,
        letter: &WakeLetter<'_>,
    ) -> Result<WakeOutcome, WakeError> {
        self.probe(session)?;
        let dir = self.inbox_dir.as_deref().expect("probed");
        inbox::write_letter(dir, session, letter).map(WakeOutcome::Queued)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{letter, session};

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "m4a-hook-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn flavors_round_trip_and_render_vendor_shapes() {
        for flavor in HookFlavor::ALL {
            assert_eq!(HookFlavor::parse(flavor.id()), Some(flavor));
            assert!(flavor.render(&[]).is_none());
        }
        let prompts = vec!["a".to_string(), "b".to_string()];
        let (out, err, code) = HookFlavor::ClaudeStop.render(&prompts).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["decision"], "block");
        assert_eq!(v["reason"], "a\n\nb");
        assert!(err.is_empty() && code == 0);
        let (out, _, _) = HookFlavor::CursorStop.render(&prompts).unwrap();
        assert!(out.contains("followup_message"));
        let (out, err, code) = HookFlavor::KimiStop.render(&prompts).unwrap();
        assert!(out.is_empty() && err == "a\n\nb" && code == 2);
        let (_, _, code) = HookFlavor::ClaudeRewake.render(&prompts).unwrap();
        assert_eq!(code, 2);
    }

    #[test]
    fn hook_adapter_needs_its_consumer_then_queues() {
        let dir = temp_dir("q");
        let kind = SessionKind::local(ProviderKind::Codex);
        let s = session(kind);
        let mut adapter = InboxHookAdapter::new(kind, HookFlavor::CodexStop, Some(dir.clone()));
        assert!(adapter.wake(&s, &letter("x")).is_err());
        inbox::arm(&dir, &HookFlavor::CodexStop.consumer()).unwrap();
        assert!(matches!(
            adapter.wake(&s, &letter("x")).unwrap(),
            WakeOutcome::Queued(_)
        ));
        assert_eq!(inbox::pending(&dir), 1);
        let mut rewake = InboxHookAdapter::new(kind, HookFlavor::ClaudeRewake, Some(dir.clone()));
        assert!(rewake.wake(&s, &letter("y")).is_err());
        {
            let _live =
                inbox::Presence::announce(&dir, &HookFlavor::ClaudeRewake.consumer()).unwrap();
            assert!(rewake.probe(&s).is_ok());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
