//! Wake for sessions a host owns (the hatchery node): the host knows which sessions are live
//! (it spawned them and assigned each its m4a account, nick = key = role); g4a decides what a
//! letter does and frames the prompt; the host executes the decision on its own runtime
//! (submit a prompt into the live PTY/ACP session, or spawn one). No I/O here, so the decision
//! is the same for every host and testable without one.

use crate::{wake_prompt, ProviderKind, ProviderSession, SessionKind, WakeLetter};

/// What the host knows about the session that owns `nick`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostedTarget {
    pub nick: String,
    pub provider: ProviderKind,
    /// A process for this session is running now and can take a prompt.
    pub live: bool,
    /// The host may start a new session for this nick when none is live.
    pub spawn_allowed: bool,
}

/// The host's view of its sessions.
pub trait HostedSessions {
    fn lookup(&self, nick: &str) -> Option<HostedTarget>;
}

/// What the host should do with one letter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostedWake {
    /// Submit `prompt` as the next turn of the live session.
    Submit { nick: String, prompt: String },
    /// No live session: spawn one for `provider` with `prompt` as its first turn.
    Spawn { nick: String, provider: ProviderKind, prompt: String },
    /// Nothing to do here (unknown nick, or not live and spawn not allowed). The letter stays
    /// in m4a's durable inbox; the reason is safe to log (no letter text).
    Refuse(String),
}

/// Decides the wake for a letter addressed to `nick`.
pub fn plan_hosted_wake(sessions: &dyn HostedSessions, nick: &str, letter: &WakeLetter<'_>) -> HostedWake {
    let Some(target) = sessions.lookup(nick) else {
        return HostedWake::Refuse(format!("{nick}: no session of this host"));
    };
    let session = ProviderSession {
        kind: SessionKind::local(target.provider),
        session_id: String::new(),
        nick: target.nick.clone(),
        cwd: None,
        headless: !target.live,
    };
    let prompt = wake_prompt(&session, letter);
    if target.live {
        HostedWake::Submit { nick: target.nick, prompt }
    } else if target.spawn_allowed {
        HostedWake::Spawn { nick: target.nick, provider: target.provider, prompt }
    } else {
        HostedWake::Refuse(format!("{nick}: not live and spawn is off"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake(Vec<HostedTarget>);
    impl HostedSessions for Fake {
        fn lookup(&self, nick: &str) -> Option<HostedTarget> {
            self.0.iter().find(|t| t.nick == nick).cloned()
        }
    }
    fn t(nick: &str, live: bool, spawn: bool) -> HostedTarget {
        HostedTarget { nick: nick.into(), provider: ProviderKind::Codex, live, spawn_allowed: spawn }
    }
    fn letter() -> WakeLetter<'static> {
        WakeLetter { body: "ping", from_nick: "hatcheryweb", event_id: "$e1", room: None, reaction: None }
    }

    #[test]
    fn live_session_gets_a_framed_turn() {
        let f = Fake(vec![t("codex-1", true, false)]);
        match plan_hosted_wake(&f, "codex-1", &letter()) {
            HostedWake::Submit { nick, prompt } => {
                assert_eq!(nick, "codex-1");
                assert!(prompt.ends_with("ping"));
                assert!(prompt.contains("m4a-send --as codex-1"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn offline_session_spawns_only_when_allowed_and_unknown_is_refused() {
        let f = Fake(vec![t("a", false, true), t("b", false, false)]);
        assert!(matches!(plan_hosted_wake(&f, "a", &letter()), HostedWake::Spawn { provider: ProviderKind::Codex, .. }));
        assert!(matches!(plan_hosted_wake(&f, "b", &letter()), HostedWake::Refuse(_)));
        match plan_hosted_wake(&f, "zzz", &letter()) {
            HostedWake::Refuse(why) => assert!(!why.contains("ping")),
            other => panic!("{other:?}"),
        }
    }
}
