//! Ordered wake chains and host/session detection.
//!
//! A [`WakeChain`] tries mechanisms in order until one delivers or queues:
//! in-session trigger first (a turn in the client that already has the
//! session open), then hooks inside that session, then a durable inbox
//! queue, and a new-process resume only for a headless session.
//! [`plan_chain`] builds the chain from where this client runs
//! ([`HostEnv`]) and what the session is ([`ProviderSession`]).

use std::fmt;
use std::path::{Path, PathBuf};

use super::claude_channel::ClaudeChannelAdapter;
use super::codex::CodexAppServerAdapter;
use super::hook::{HookFlavor, InboxHookAdapter, InboxQueueAdapter};
use super::kimi::KimiServerAdapter;
use super::kimi_acp::{KimiAcpAdapter, KimiAcpConfig};
use super::spawn::ResumeSpawnAdapter;
use super::{
    AdapterConfig, ClaudeRoutineFireAdapter, GrokLeaderAdapter, NoInboundAdapter, ProviderKind,
    ProviderSession, RoutineWebhookAdapter, SessionKind, Surface, WakeAdapter, WakeError,
    WakeLetter, WakeOutcome,
};

/// Explicit surface override: `web` or `local`.
pub const SURFACE_ENV: &str = "M4A_SURFACE";
/// Explicit web vendor override: `grok-bot`, `claude-web`, `codex-cloud`, `cursor-cloud`.
pub const WEB_VENDOR_ENV: &str = "M4A_WEB_VENDOR";
/// Marks the session as headless (no client holds it open): `1`.
pub const HEADLESS_ENV: &str = "M4A_HEADLESS";

/// Where a mechanism sits in the order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Starts a turn in the client that has the session open, now.
    InSession,
    /// A hook inside the open session picks the letter up.
    Hook,
    /// Durable inbox; delivered by the next hook run / channel start.
    Queue,
    /// New process resuming the session. Headless sessions only.
    Spawn,
}

/// One row of the per-kind table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mechanism {
    /// Stable id (also the chain link id).
    pub id: &'static str,
    /// Tier.
    pub tier: Tier,
    /// Verified live on a real CLI on 2026-10-06.
    pub verified: bool,
}

const fn m(id: &'static str, tier: Tier, verified: bool) -> Mechanism {
    Mechanism { id, tier, verified }
}

/// Vendor box this client runs on, when it is not the user's machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebVendor {
    /// Grok Bot / Cursor-style agent box with a webhook routine.
    GrokBot,
    /// Claude Code on the web (cloud container).
    ClaudeWeb,
    /// Codex cloud task container.
    CodexCloud,
    /// Cursor cloud agent.
    CursorCloud,
}

impl WebVendor {
    /// Parses [`WEB_VENDOR_ENV`].
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "grok-bot" | "grokbot" => Some(Self::GrokBot),
            "claude-web" | "claude" => Some(Self::ClaudeWeb),
            "codex-cloud" | "codex" => Some(Self::CodexCloud),
            "cursor-cloud" | "cursor" => Some(Self::CursorCloud),
            _ => None,
        }
    }
}

/// Where this client runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostEnv {
    /// Web vendor host or the user's machine.
    pub surface: Surface,
    /// Which vendor box, when web.
    pub vendor: Option<WebVendor>,
    /// `std::env::consts::OS` (`linux`, `macos`, `windows`).
    pub os: &'static str,
}

impl HostEnv {
    /// Detects from the process environment and the filesystem.
    pub fn detect() -> Self {
        Self::detect_from(|key| std::env::var(key).ok(), |path| path.exists())
    }

    /// Detection with injected lookups (tests).
    ///
    /// Order: [`SURFACE_ENV`] / [`WEB_VENDOR_ENV`] overrides, then vendor
    /// markers: `CLAUDE_CODE_REMOTE=true` (Claude web container),
    /// `<home>/agent-data` (Grok Bot box). Anything else is local.
    pub fn detect_from(
        get: impl Fn(&str) -> Option<String>,
        exists: impl Fn(&Path) -> bool,
    ) -> Self {
        let os = std::env::consts::OS;
        let set = |key: &str| get(key).filter(|value| !value.trim().is_empty());
        let vendor = set(WEB_VENDOR_ENV)
            .and_then(|value| WebVendor::parse(&value))
            .or_else(|| {
                set("CLAUDE_CODE_REMOTE")
                    .filter(|value| value.eq_ignore_ascii_case("true") || value == "1")
                    .map(|_| WebVendor::ClaudeWeb)
            })
            .or_else(|| {
                let home = get("HOME").filter(|h| !h.is_empty());
                let marker = home.map(|h| Path::new(&h).join("agent-data")).unwrap_or_else(|| Path::new("agent-data").to_path_buf());
                exists(&marker).then_some(WebVendor::GrokBot)
            });
        let surface = match set(SURFACE_ENV)
            .as_deref()
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("local") => {
                return Self {
                    surface: Surface::Local,
                    vendor: None,
                    os,
                }
            }
            Some("web") => Surface::Web,
            _ if vendor.is_some() => Surface::Web,
            _ => Surface::Local,
        };
        Self {
            surface,
            vendor,
            os,
        }
    }
}

/// Provider of the session this process runs inside, from env markers
/// the CLIs export to their child processes: `M4A_PROVIDER` first, then
/// `CLAUDECODE` (Claude Code), `CODEX_THREAD_ID` (Codex), `CURSOR_AGENT`
/// (Cursor CLI). Kimi and Grok export no session marker: set
/// `M4A_PROVIDER`.
pub fn detect_provider(get: impl Fn(&str) -> Option<String>) -> Option<ProviderKind> {
    let set = |key: &str| get(key).filter(|value| !value.trim().is_empty());
    if let Some(value) = set(super::PROVIDER_ENV) {
        return ProviderKind::parse(&value);
    }
    if set("CLAUDECODE").is_some() || set("CLAUDE_CODE_REMOTE").is_some() {
        return Some(ProviderKind::ClaudeCode);
    }
    if set("CODEX_THREAD_ID").is_some() {
        return Some(ProviderKind::Codex);
    }
    if set("CURSOR_AGENT").is_some() {
        return Some(ProviderKind::Cursor);
    }
    None
}

/// Vendor session id from env, where the CLI exports one.
pub fn detect_session_id(
    provider: ProviderKind,
    get: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    let keys: &[&str] = match provider {
        ProviderKind::ClaudeCode => &[
            "CLAUDE_CODE_SESSION_ID",
            "CLAUDE_SESSION_ID",
            "CLAUDE_CODE_REMOTE_SESSION_ID",
        ],
        ProviderKind::Codex => &["CODEX_THREAD_ID", "CODEX_SESSION_ID"],
        _ => &[],
    };
    keys.iter()
        .find_map(|key| get(key).filter(|value| !value.trim().is_empty()))
}

/// The ordered mechanism table for `kind` on `vendor` (web) hosts.
/// [`plan_chain`] follows exactly this order.
pub fn mechanisms(kind: SessionKind, vendor: Option<WebVendor>) -> Vec<Mechanism> {
    use Tier::*;
    match (kind.surface, kind.provider) {
        (Surface::Local, ProviderKind::Grok) => vec![
            m("grok-leader-acp", InSession, true),
            m("grok-stop-hook", Hook, false),
            m("inbox-queue", Queue, true),
            m("grok-resume-spawn", Spawn, false),
        ],
        (Surface::Local, ProviderKind::Codex) => vec![
            m("codex-app-server-turn", InSession, true),
            m("codex-stop-hook", Hook, false),
            m("inbox-queue", Queue, true),
            m("codex-exec-resume-spawn", Spawn, false),
        ],
        (Surface::Local, ProviderKind::KimiCode) => vec![
            m("kimi-acp-host", InSession, true),
            m("kimi-server-prompt", InSession, true),
            m("kimi-stop-hook", Hook, false),
            m("inbox-queue", Queue, true),
            m("kimi-resume-spawn", Spawn, false),
        ],
        (Surface::Local, ProviderKind::ClaudeCode) => vec![
            m("claude-channel", InSession, false),
            m("claude-async-rewake", InSession, false),
            m("claude-stop-hook", Hook, false),
            m("inbox-queue", Queue, true),
            m("claude-resume-spawn", Spawn, false),
        ],
        (Surface::Local, ProviderKind::Cursor) => vec![
            m("cursor-stop-followup", Hook, false),
            m("inbox-queue", Queue, true),
            m("cursor-resume-spawn", Spawn, false),
        ],
        (Surface::Web, provider) => {
            let mut rows = Vec::new();
            if matches!(vendor, Some(WebVendor::GrokBot | WebVendor::CursorCloud))
                || provider == ProviderKind::Cursor
            {
                rows.push(m(
                    "routine-webhook",
                    InSession,
                    vendor == Some(WebVendor::GrokBot),
                ));
            }
            match provider {
                ProviderKind::ClaudeCode => {
                    rows.push(m("claude-async-rewake", InSession, false));
                    rows.push(m("claude-stop-hook", Hook, false));
                    rows.push(m("inbox-queue", Queue, true));
                    rows.push(m("claude-routine-fire", Spawn, false));
                }
                ProviderKind::Codex => {
                    rows.push(m("codex-stop-hook", Hook, false));
                    rows.push(m("inbox-queue", Queue, true));
                    rows.push(m("codex-cloud-exec", Spawn, false));
                }
                ProviderKind::Cursor => {
                    rows.push(m("cursor-stop-followup", Hook, false));
                    rows.push(m("inbox-queue", Queue, true));
                }
                ProviderKind::KimiCode | ProviderKind::Grok => {
                    if rows.is_empty() {
                        rows.push(m("no-inbound", Spawn, true));
                    }
                }
            }
            rows
        }
    }
}

/// Ordered adapters with ids.
#[derive(Default)]
pub struct WakeChain {
    links: Vec<(&'static str, Box<dyn WakeAdapter>)>,
}

/// Why every link refused. Ids and errors only (no body, no secret).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainError(pub Vec<(&'static str, WakeError)>);

impl fmt::Display for ChainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            return write!(f, "wake chain is empty");
        }
        let parts: Vec<String> = self
            .0
            .iter()
            .map(|(id, err)| format!("{id}: {err}"))
            .collect();
        write!(f, "{}", parts.join("; "))
    }
}

impl std::error::Error for ChainError {}

impl fmt::Debug for WakeChain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.ids()).finish()
    }
}

impl WakeChain {
    /// Empty chain.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a link.
    pub fn push(&mut self, id: &'static str, adapter: Box<dyn WakeAdapter>) {
        self.links.push((id, adapter));
    }

    /// Puts a link in front (an embedder's own first stop, e.g. m4a's letter hook).
    pub fn prepend(&mut self, id: &'static str, adapter: Box<dyn WakeAdapter>) {
        self.links.insert(0, (id, adapter));
    }

    /// Link ids in order.
    pub fn ids(&self) -> Vec<&'static str> {
        self.links.iter().map(|(id, _)| *id).collect()
    }

    /// Whether the chain has no link.
    pub fn is_empty(&self) -> bool {
        self.links.is_empty()
    }

    /// First link whose probe passes, without waking.
    pub fn best(&self, session: &ProviderSession) -> Option<&'static str> {
        self.links
            .iter()
            .find(|(_, adapter)| adapter.probe(session).is_ok())
            .map(|(id, _)| *id)
    }

    /// Tries each link in order. The first `Delivered` or `Queued` wins.
    pub fn wake(
        &mut self,
        session: &ProviderSession,
        letter: &WakeLetter<'_>,
    ) -> Result<(&'static str, WakeOutcome), ChainError> {
        let mut errors = Vec::new();
        for (id, adapter) in &mut self.links {
            if let Err(err) = adapter.probe(session) {
                errors.push((*id, err));
                continue;
            }
            match adapter.wake(session, letter) {
                Ok(outcome) => return Ok((*id, outcome)),
                Err(err) => errors.push((*id, err)),
            }
        }
        Err(ChainError(errors))
    }
}

/// Builds the ordered chain for `session` on `host` ([`mechanisms`] order).
pub fn plan_chain(session: &ProviderSession, host: &HostEnv, config: &AdapterConfig) -> WakeChain {
    let kind = session.kind;
    let inbox = config.inbox_dir.clone();
    let mut chain = WakeChain::new();
    for row in mechanisms(kind, host.vendor) {
        let adapter: Box<dyn WakeAdapter> = match row.id {
            "grok-leader-acp" => Box::new(GrokLeaderAdapter {
                leader_sock: config.leader_sock.clone(),
            }),
            "codex-app-server-turn" => Box::new(CodexAppServerAdapter::new(config.codex.clone())),
            "kimi-acp-host" => Box::new(KimiAcpAdapter::new(
                config.kimi_acp_bin.clone().map(|bin| KimiAcpConfig { bin }),
            )),
            "kimi-server-prompt" => Box::new(KimiServerAdapter::new(
                config.kimi_url.clone(),
                config.kimi_bearer.clone(),
            )),
            "claude-channel" => Box::new(ClaudeChannelAdapter::new(inbox.clone())),
            "routine-webhook" => Box::new(RoutineWebhookAdapter {
                provider: kind.provider,
                url: config.routine_url.clone(),
                bearer: config.routine_bearer.clone(),
            }),
            "claude-async-rewake" => hook(kind, HookFlavor::ClaudeRewake, &inbox),
            "claude-stop-hook" => hook(kind, HookFlavor::ClaudeStop, &inbox),
            "codex-stop-hook" => hook(kind, HookFlavor::CodexStop, &inbox),
            "kimi-stop-hook" => hook(kind, HookFlavor::KimiStop, &inbox),
            "cursor-stop-followup" => hook(kind, HookFlavor::CursorStop, &inbox),
            "grok-stop-hook" => hook(kind, HookFlavor::GrokStop, &inbox),
            "inbox-queue" => Box::new(InboxQueueAdapter::new(kind, inbox.clone())),
            "claude-routine-fire" => Box::new(ClaudeRoutineFireAdapter {
                url: config.claude_fire_url.clone(),
                bearer: config.claude_fire_bearer.clone(),
            }),
            "no-inbound" => Box::new(NoInboundAdapter {
                provider: kind.provider,
            }),
            id if id.ends_with("-spawn") || id == "codex-cloud-exec" => {
                let program = config.spawn_program.clone().or_else(|| {
                    std::env::var_os(super::spawn::bin_env(kind.provider))
                        .filter(|value| !value.is_empty())
                        .map(PathBuf::from)
                });
                let cloud_env = config.codex_cloud_env.clone().or_else(|| {
                    std::env::var(super::spawn::CODEX_CLOUD_ENV_ENV)
                        .ok()
                        .filter(|value| !value.is_empty())
                });
                Box::new(ResumeSpawnAdapter::new(kind, program, cloud_env))
            }
            _ => continue,
        };
        chain.push(row.id, adapter);
    }
    chain
}

/// Chain for a session the WEB machine client holds, when the session has
/// no webhook routine: only on a detected web vendor host other than the
/// Grok Bot box (which wakes through its routine), with the provider read
/// from the CLI's env markers. `None` keeps today's behaviour.
pub fn detected_web_chain(
    session_id: &str,
    nick: &str,
    store_root: &Path,
) -> Option<(ProviderSession, WakeChain)> {
    let host = HostEnv::detect();
    if host.surface != Surface::Web || host.vendor == Some(WebVendor::GrokBot) {
        return None;
    }
    let provider = detect_provider(|key| std::env::var(key).ok())?;
    let session = ProviderSession {
        kind: SessionKind::web(provider),
        session_id: session_id.to_string(),
        nick: nick.to_string(),
        cwd: std::env::current_dir().ok(),
        headless: std::env::var(HEADLESS_ENV).is_ok_and(|v| v == "1"),
    };
    let config = AdapterConfig::from_env(Some(super::registry::inbox_dir(store_root, session_id)));
    let chain = plan_chain(&session, &host, &config);
    Some((session, chain))
}

fn hook(kind: SessionKind, flavor: HookFlavor, inbox: &Option<PathBuf>) -> Box<dyn WakeAdapter> {
    Box::new(InboxHookAdapter::new(kind, flavor, inbox.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "wake-claude")]
    use crate::inbox;
    use crate::tests::{letter, session};

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn host_detection_orders_overrides_then_markers() {
        let none = |_: &Path| false;
        assert_eq!(HostEnv::detect_from(env(&[]), none).surface, Surface::Local);
        let claude = HostEnv::detect_from(env(&[("CLAUDE_CODE_REMOTE", "true")]), none);
        assert_eq!(
            (claude.surface, claude.vendor),
            (Surface::Web, Some(WebVendor::ClaudeWeb))
        );
        let bot = HostEnv::detect_from(env(&[]), |p: &Path| p.ends_with("agent-data"));
        assert_eq!(bot.vendor, Some(WebVendor::GrokBot));
        let forced = HostEnv::detect_from(env(&[(SURFACE_ENV, "local")]), |_: &Path| true);
        assert_eq!((forced.surface, forced.vendor), (Surface::Local, None));
        let codex = HostEnv::detect_from(env(&[(WEB_VENDOR_ENV, "codex-cloud")]), none);
        assert_eq!(codex.vendor, Some(WebVendor::CodexCloud));
    }

    #[test]
    fn provider_detection_uses_cli_markers() {
        assert_eq!(
            detect_provider(env(&[("CLAUDECODE", "1")])),
            Some(ProviderKind::ClaudeCode)
        );
        assert_eq!(
            detect_provider(env(&[("CODEX_THREAD_ID", "t")])),
            Some(ProviderKind::Codex)
        );
        assert_eq!(
            detect_provider(env(&[("CURSOR_AGENT", "1")])),
            Some(ProviderKind::Cursor)
        );
        assert_eq!(
            detect_provider(env(&[("M4A_PROVIDER", "kimi"), ("CLAUDECODE", "1")])),
            Some(ProviderKind::KimiCode)
        );
        assert_eq!(detect_provider(env(&[])), None);
        assert_eq!(
            detect_session_id(ProviderKind::Codex, env(&[("CODEX_THREAD_ID", "t9")])).as_deref(),
            Some("t9")
        );
    }

    #[test]
    fn every_chain_puts_in_session_first_and_spawn_last() {
        for provider in ProviderKind::ALL {
            for (kind, vendor) in [
                (SessionKind::local(provider), None),
                (SessionKind::web(provider), Some(WebVendor::GrokBot)),
                (SessionKind::web(provider), Some(WebVendor::ClaudeWeb)),
            ] {
                let rows = mechanisms(kind, vendor);
                assert!(!rows.is_empty(), "{kind}");
                let tiers: Vec<Tier> = rows.iter().map(|row| row.tier).collect();
                let rank = |t: &Tier| match t {
                    Tier::InSession => 0,
                    Tier::Hook => 1,
                    Tier::Queue => 2,
                    Tier::Spawn => 3,
                };
                assert!(
                    tiers.windows(2).all(|w| rank(&w[0]) <= rank(&w[1])),
                    "{kind}: {tiers:?}"
                );
                let host = HostEnv {
                    surface: kind.surface,
                    vendor,
                    os: "linux",
                };
                let chain = plan_chain(&session(kind), &host, &AdapterConfig::default());
                let ids: Vec<&str> = rows.iter().map(|row| row.id).collect();
                assert_eq!(chain.ids(), ids, "{kind}");
            }
        }
    }

    #[cfg(feature = "wake-claude")]
    #[test]
    fn chain_falls_through_to_durable_queue_without_spawning() {
        let dir = std::env::temp_dir().join(format!("m4a-chain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let kind = SessionKind::local(ProviderKind::ClaudeCode);
        let config = AdapterConfig {
            inbox_dir: Some(dir.clone()),
            spawn_program: Some("/bin/false".into()),
            ..AdapterConfig::default()
        };
        let host = HostEnv {
            surface: Surface::Local,
            vendor: None,
            os: "linux",
        };
        let s = session(kind);
        let mut chain = plan_chain(&s, &host, &config);
        let (id, outcome) = chain.wake(&s, &letter("hi")).unwrap();
        assert_eq!(id, "inbox-queue");
        assert!(matches!(outcome, WakeOutcome::Queued(_)));
        // With a live channel server the primary wins.
        let _live = inbox::Presence::announce(&dir, inbox::consumer::CLAUDE_CHANNEL).unwrap();
        assert_eq!(chain.best(&s), Some("claude-channel"));
        let (id, _) = chain.wake(&s, &letter("hi")).unwrap();
        assert_eq!(id, "claude-channel");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn chain_without_any_endpoint_reports_every_link() {
        let kind = SessionKind::web(ProviderKind::KimiCode);
        let host = HostEnv {
            surface: Surface::Web,
            vendor: None,
            os: "linux",
        };
        let s = session(kind);
        let mut chain = plan_chain(&s, &host, &AdapterConfig::default());
        let err = chain.wake(&s, &letter("x")).unwrap_err();
        assert_eq!(err.0.len(), 1);
        assert!(err.to_string().contains("no inbound"));
    }

    /// Live: `M4A_LIVE_CODEX_APP_SERVER` (unix://PATH or ws://127.0.0.1:PORT)
    /// and `M4A_LIVE_CODEX_THREAD` (a thread an attached client has open).
    #[test]
    #[ignore]
    fn live_codex_chain_turns_in_the_open_thread() {
        let endpoint = std::env::var("M4A_LIVE_CODEX_APP_SERVER").expect("endpoint");
        let thread = std::env::var("M4A_LIVE_CODEX_THREAD").expect("thread");
        let kind = SessionKind::local(ProviderKind::Codex);
        let mut s = session(kind);
        s.session_id = thread;
        let config = AdapterConfig {
            codex: super::super::CodexEndpoint::parse(&endpoint),
            spawn_program: Some("/bin/false".into()),
            ..AdapterConfig::default()
        };
        let host = HostEnv {
            surface: Surface::Local,
            vendor: None,
            os: std::env::consts::OS,
        };
        let mut chain = plan_chain(&s, &host, &config);
        let (id, outcome) = chain
            .wake(&s, &letter("live chain ping, reply with one word"))
            .unwrap();
        assert_eq!(
            (id, outcome),
            ("codex-app-server-turn", WakeOutcome::Delivered)
        );
    }

    /// Live: `M4A_LIVE_KIMI_URL`, `M4A_LIVE_KIMI_TOKEN`, `M4A_LIVE_KIMI_SESSION`
    /// (a session open in `kimi web`), or discovery from `~/.kimi-code`.
    #[test]
    #[ignore]
    fn live_kimi_chain_prompts_the_open_session() {
        let kind = SessionKind::local(ProviderKind::KimiCode);
        let mut s = session(kind);
        s.session_id = std::env::var("M4A_LIVE_KIMI_SESSION").expect("session");
        let mut config = AdapterConfig::from_env(None);
        if let (Ok(url), Ok(token)) = (
            std::env::var("M4A_LIVE_KIMI_URL"),
            std::env::var("M4A_LIVE_KIMI_TOKEN"),
        ) {
            config.kimi_url = Some(url);
            config.kimi_bearer = Some(token);
        }
        config.spawn_program = Some("/bin/false".into());
        let host = HostEnv {
            surface: Surface::Local,
            vendor: None,
            os: std::env::consts::OS,
        };
        let mut chain = plan_chain(&s, &host, &config);
        let (id, outcome) = chain.wake(&s, &letter("live chain ping")).unwrap();
        assert_eq!(
            (id, outcome),
            ("kimi-server-prompt", WakeOutcome::Delivered)
        );
    }
}
