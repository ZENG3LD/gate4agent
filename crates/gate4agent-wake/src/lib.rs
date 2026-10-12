//! gate4agent-wake: the deliver-turn API -- put one letter into an agent session as a turn,
//! over structured channels only. No PTY, no raw keystrokes, no screen read: this crate must
//! never depend on the terminal crates (gate4agent-pty, -shell-*, -runtime-native, -node-core).
//! It has no permission model of its own; the host (hatchery) decides who may deliver.
//! Adapters never answer a permission request; the modal stays with the session's client.
//!
//! (Adapters moved here from mail4agent, 2026-10-13; the grok leader ACP client and the routine
//! POST followed on `feat/deliver-adapters`, so the messenger links no session channel.)
//!
//! Layer rule: mail4agent (m4a) is a pure messenger -- it hands a letter over (`M4A_LETTER_HOOK`)
//! and keeps it in its durable inbox. The host (the hatchery node) decides with [`hosted`] and
//! delivers through the adapters here.
//!
//! Original description (mail4agent):
//!
//! The clients (push link, decrypt, sealed store, `m4a-send` socket) are
//! provider-agnostic. The only provider-specific step is *wake*: putting one
//! decrypted room text in front of an agent session so it takes a turn.
//! That step is the [`WakeAdapter`] trait. A session is addressed by a
//! [`SessionKind`]: the vendor ([`ProviderKind`]) and the [`Surface`] it runs
//! on (vendor-hosted web/cloud, or a CLI on the user's machine). Replies
//! never go through the adapter: every agent answers with the shell command
//! `m4a-send --as <own-nick> --to <peer-nick> '<text>'`.
//!
//! Each session gets an ordered [`WakeChain`] ([`chain::plan_chain`],
//! order in [`chain::mechanisms`]): a turn in the client that already has
//! the session open first, then hooks inside that session, then the
//! durable inbox, and a new-process resume only for a headless session.
//!
//! | Kind | Primary (in-session) | Fallbacks | Last resort (headless only) |
//! | --- | --- | --- | --- |
//! | Grok CLI | [`GrokLeaderAdapter`] ACP on `leader.sock` | Stop hook, inbox | `grok --resume -p` |
//! | Codex CLI | [`codex::CodexAppServerAdapter`] `turn/start` | Stop hook, inbox | `codex exec resume` |
//! | Kimi Code CLI | [`kimi::KimiServerAdapter`] `kimi web` prompts | Stop hook, inbox | `kimi -S -p` |
//! | Claude Code CLI | [`ClaudeChannelAdapter`] channel MCP; `asyncRewake` waiter | Stop hook, inbox | `claude --resume -p` |
//! | Cursor CLI | none for an idle chat | `stop` hook `followup_message`, inbox | `agent --resume -p` |
//! | Grok Bot / Cursor web | [`RoutineWebhookAdapter`] | - | - |
//! | Claude Code web | `asyncRewake` waiter in the cloud session | Stop hook, inbox | [`ClaudeRoutineFireAdapter`] (new session) |
//! | Codex cloud | - | Stop hook, inbox | `codex cloud exec` (new task) |
//! | Kimi web, Grok web | none documented ([`NoInboundAdapter`]) | - | - |
//!
//! Core four: Grok, Kimi Code, Claude Code, Codex. Cursor CLI is optional.
//! Design: the project documentation.
//! Only [`spawn::ResumeSpawnAdapter`] starts a provider process, and only
//! for a headless session. No adapter answers a permission modal or logs
//! the plaintext or a credential.

pub mod chain;
pub mod hosted;
#[cfg(feature = "wake-claude")]
pub mod claude_channel;
#[cfg(not(feature = "wake-claude"))]
pub use off::claude_channel;
#[cfg(feature = "wake-codex")]
pub mod codex;
#[cfg(not(feature = "wake-codex"))]
pub use off::codex;
pub mod hook;
pub mod inbox;
#[cfg(feature = "wake-kimi")]
pub mod kimi;
#[cfg(not(feature = "wake-kimi"))]
pub use off::kimi;
pub mod registry;
#[cfg(feature = "wake-grok")]
pub mod grok_leader;
#[cfg(feature = "wake-routine")]
pub mod routine;
#[cfg(not(all(feature = "wake-claude", feature = "wake-codex", feature = "wake-kimi", feature = "wake-spawn")))]
mod off;
#[cfg(feature = "wake-spawn")]
pub mod spawn;
#[cfg(not(feature = "wake-spawn"))]
pub use off::spawn;

use std::fmt;
use std::path::{Path, PathBuf};

/// `M4A_LEADER_SOCK`: the grok leader socket.
pub const LEADER_SOCK_ENV: &str = "M4A_LEADER_SOCK";

/// Command a woken bot runs to answer, as shown in the wake's `reply`.
pub const SEND_COMMAND: &str = "m4a-send";

/// The `reply` hint for a wake from `from_nick` to `to`.
pub fn reply_hint(to: &str, from_nick: &str) -> String {
    format!("{SEND_COMMAND} --as {to} --to {from_nick} '<your reply>'")
}


pub use chain::{plan_chain, ChainError, HostEnv, Mechanism, Tier, WakeChain, WebVendor};
pub use claude_channel::ClaudeChannelAdapter;
pub use codex::{CodexAppServerAdapter, CodexEndpoint};
pub use hook::{HookFlavor, InboxHookAdapter, InboxQueueAdapter};
pub use inbox::{InboxEntry, InboxLetter};
pub use kimi::KimiServerAdapter;
pub use registry::SessionRecord;
pub use spawn::ResumeSpawnAdapter;

/// Environment variable naming the provider of a local session.
pub const PROVIDER_ENV: &str = "M4A_PROVIDER";

/// Environment variable for a per-session inbox directory used by
/// queue-style adapters (Claude channel server, hook doorbells).
pub const INBOX_DIR_ENV: &str = "M4A_INBOX_DIR";

/// Agent vendors the stack drives. Ids match the gate4agent catalog /
/// adapter ids (`grok`, `kimi`, `claude`, `codex`, `cursor`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderKind {
    /// xAI Grok.
    Grok,
    /// Kimi Code.
    KimiCode,
    /// Claude Code.
    ClaudeCode,
    /// OpenAI Codex.
    Codex,
    /// Cursor: the vendor-hosted web agents (Grok Bot boxes) and the
    /// optional Linux-only `cursor-agent` CLI.
    Cursor,
}

impl ProviderKind {
    /// The core four, Grok first.
    pub const CORE: [ProviderKind; 4] = [
        ProviderKind::Grok,
        ProviderKind::KimiCode,
        ProviderKind::ClaudeCode,
        ProviderKind::Codex,
    ];

    /// Every provider, including the optional Cursor CLI.
    pub const ALL: [ProviderKind; 5] = [
        ProviderKind::Grok,
        ProviderKind::KimiCode,
        ProviderKind::ClaudeCode,
        ProviderKind::Codex,
        ProviderKind::Cursor,
    ];

    /// Stable id (gate4agent catalog spelling).
    pub fn id(self) -> &'static str {
        match self {
            ProviderKind::Grok => "grok",
            ProviderKind::KimiCode => "kimi",
            ProviderKind::ClaudeCode => "claude",
            ProviderKind::Codex => "codex",
            ProviderKind::Cursor => "cursor",
        }
    }

    /// `true` for the optional fifth CLI provider.
    pub fn is_optional(self) -> bool {
        matches!(self, ProviderKind::Cursor)
    }

    /// Parses an id or a common alias. Case-insensitive.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "grok" => Some(ProviderKind::Grok),
            "kimi" | "kimi-code" | "kimicode" => Some(ProviderKind::KimiCode),
            "claude" | "claude-code" | "claudecode" => Some(ProviderKind::ClaudeCode),
            "codex" => Some(ProviderKind::Codex),
            "cursor" | "cursor-agent" => Some(ProviderKind::Cursor),
            _ => None,
        }
    }

    /// Reads [`PROVIDER_ENV`]. Unset means Grok.
    pub fn from_env() -> Result<Self, WakeError> {
        match std::env::var(PROVIDER_ENV) {
            Ok(value) if !value.trim().is_empty() => {
                Self::parse(&value).ok_or(WakeError::UnknownProvider)
            }
            _ => Ok(ProviderKind::Grok),
        }
    }
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

/// Where a session runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Surface {
    /// Vendor-hosted web / cloud session (`m4a-web-client`).
    Web,
    /// CLI session on the user's machine (local client).
    Local,
}

/// Vendor + surface: what picks the adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionKind {
    /// Vendor.
    pub provider: ProviderKind,
    /// Web or local.
    pub surface: Surface,
}

impl SessionKind {
    /// Local CLI session of `provider`.
    pub fn local(provider: ProviderKind) -> Self {
        Self {
            provider,
            surface: Surface::Local,
        }
    }

    /// Vendor-hosted web session of `provider`.
    pub fn web(provider: ProviderKind) -> Self {
        Self {
            provider,
            surface: Surface::Web,
        }
    }
}

impl fmt::Display for SessionKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let surface = match self.surface {
            Surface::Web => "web",
            Surface::Local => "local",
        };
        write!(f, "{}/{}", self.provider, surface)
    }
}

/// One session a client wakes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderSession {
    /// Vendor + surface.
    pub kind: SessionKind,
    /// Vendor-native session / thread / chat id. Never logged or committed.
    pub session_id: String,
    /// mail4agent nick of this session (slug of the display name).
    pub nick: String,
    /// Working directory the session was started in (local only).
    pub cwd: Option<PathBuf>,
    /// No client holds the session open. Only then may a chain fall back
    /// to spawning a new process that resumes it.
    pub headless: bool,
}

/// One decrypted room text to put in front of the session.
#[derive(Debug, Clone, Copy)]
pub struct WakeLetter<'a> {
    /// Plaintext body.
    pub body: &'a str,
    /// Sender nick.
    pub from_nick: &'a str,
    /// Matrix event id (dedupe key).
    pub event_id: &'a str,
    /// Room id, if known.
    pub room: Option<&'a str>,
}

/// What a wake did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WakeOutcome {
    /// The session accepted the prompt now (a turn started or was queued
    /// by the vendor runtime).
    Delivered,
    /// Stored for the session to pick up (channel server / hook). The path
    /// is the inbox entry written.
    Queued(PathBuf),
}

/// Why a wake did not happen. Never carries the plaintext or a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WakeError {
    /// [`PROVIDER_ENV`] named something unknown.
    UnknownProvider,
    /// The adapter is a skeleton on this branch.
    NotImplemented(SessionKind),
    /// The vendor exposes no inbound trigger into an existing session.
    NoInboundTrigger(SessionKind),
    /// The endpoint (socket, server, inbox, routine) is absent or unset.
    Unavailable(String),
    /// The endpoint answered but the wake failed.
    Transport(String),
}

impl fmt::Display for WakeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WakeError::UnknownProvider => write!(f, "unknown provider in {PROVIDER_ENV}"),
            WakeError::NotImplemented(kind) => {
                write!(f, "wake adapter for {kind} is not implemented")
            }
            WakeError::NoInboundTrigger(kind) => {
                write!(
                    f,
                    "{kind} exposes no inbound trigger into a running session"
                )
            }
            WakeError::Unavailable(why) => write!(f, "wake endpoint unavailable: {why}"),
            WakeError::Transport(why) => write!(f, "wake failed: {why}"),
        }
    }
}

impl std::error::Error for WakeError {}

/// Vendor/surface-specific wake. One instance per session.
///
/// The caller keeps the per-`event_id` sent set. Implementations must not
/// spawn a provider, must not answer permission prompts, and must not log
/// `letter.body` or a credential.
pub trait WakeAdapter: Send {
    /// Session kind this adapter wakes.
    fn kind(&self) -> SessionKind;

    /// Cheap readiness check (socket exists, server configured, routine
    /// key present). Called before register and on every full drive.
    fn probe(&self, session: &ProviderSession) -> Result<(), WakeError>;

    /// Puts one letter in front of the session.
    fn wake(
        &mut self,
        session: &ProviderSession,
        letter: &WakeLetter<'_>,
    ) -> Result<WakeOutcome, WakeError>;
}

/// The prompt text every local adapter injects. Same shape for all
/// providers so agents learn one protocol
/// (`docs/mail4agent/agent-mail-protocol.md`). Web routines get the JSON
/// object instead, whose `reply` field carries the same command.
pub fn wake_prompt(session: &ProviderSession, letter: &WakeLetter<'_>) -> String {
    let reply = reply_hint(&session.nick, letter.from_nick);
    format!(
        "[mail4agent] Letter from {from} to {to} (event {event}).\n\
         Answer every direct letter with {cmd}; at minimum \"принято\" plus what you will do and when.\n\
         Reply: {reply}\n\
         ---\n\
         {body}",
        from = letter.from_nick,
        to = session.nick,
        event = letter.event_id,
        cmd = SEND_COMMAND,
        reply = reply,
        body = letter.body,
    )
}

/// Endpoints the host hands to [`adapter_for`]. Every field is optional;
/// an adapter whose endpoint is missing reports [`WakeError::Unavailable`].
/// Credentials come from the host keychain / environment, never a literal.
#[derive(Default, Clone)]
pub struct AdapterConfig {
    /// Grok `leader.sock` (`M4A_LEADER_SOCK`).
    pub leader_sock: Option<PathBuf>,
    /// Inbox directory for queue-style adapters (`M4A_INBOX_DIR`).
    pub inbox_dir: Option<PathBuf>,
    /// Codex app-server endpoint (`M4A_CODEX_APP_SERVER`).
    pub codex: Option<CodexEndpoint>,
    /// Kimi local server origin (`M4A_KIMI_SERVER_URL`).
    pub kimi_url: Option<String>,
    /// Kimi local server bearer (`M4A_KIMI_SERVER_TOKEN`), in memory only.
    pub kimi_bearer: Option<String>,
    /// Web routine URL of the bot (`M4A_ROUTINE_URL`), in memory only.
    pub routine_url: Option<String>,
    /// Web routine key (`M4A_ROUTINE_BEARER`), in memory only.
    pub routine_bearer: Option<String>,
    /// Claude routine `/fire` URL (`M4A_CLAUDE_ROUTINE_FIRE_URL`), in memory only.
    pub claude_fire_url: Option<String>,
    /// Claude routine token (`M4A_CLAUDE_ROUTINE_TOKEN`), in memory only.
    pub claude_fire_bearer: Option<String>,
    /// Codex cloud environment id for `codex cloud exec --env`.
    pub codex_cloud_env: Option<String>,
    /// Override of the provider binary for resume spawns (tests).
    pub spawn_program: Option<PathBuf>,
}

/// Claude routine fire URL env.
pub const CLAUDE_FIRE_URL_ENV: &str = "M4A_CLAUDE_ROUTINE_FIRE_URL";
/// Claude routine token env.
pub const CLAUDE_FIRE_TOKEN_ENV: &str = "M4A_CLAUDE_ROUTINE_TOKEN";

impl AdapterConfig {
    /// Endpoints from the environment for one session. `inbox_dir` is the
    /// session's inbox (see [`registry::inbox_dir`]) unless
    /// [`INBOX_DIR_ENV`] overrides it. A Kimi server is discovered from
    /// `~/.kimi-code` when its env is unset. Routine URL/bearer are not
    /// read here: the web client passes its own.
    pub fn from_env(inbox_dir: Option<PathBuf>) -> Self {
        let get = |key: &str| std::env::var(key).ok().filter(|value| !value.is_empty());
        let mut kimi = KimiServerAdapter::from_env();
        if !kimi.is_configured() {
            if let Some(found) = kimi::discover_local_server(None) {
                kimi = found;
            }
        }
        let (kimi_url, kimi_bearer) = kimi.into_parts();
        Self {
            leader_sock: get(crate::LEADER_SOCK_ENV).map(PathBuf::from),
            inbox_dir: get(INBOX_DIR_ENV).map(PathBuf::from).or(inbox_dir),
            codex: CodexEndpoint::from_env(),
            kimi_url,
            kimi_bearer,
            routine_url: None,
            routine_bearer: None,
            claude_fire_url: get(CLAUDE_FIRE_URL_ENV),
            claude_fire_bearer: get(CLAUDE_FIRE_TOKEN_ENV),
            codex_cloud_env: get(spawn::CODEX_CLOUD_ENV_ENV),
            spawn_program: None,
        }
    }
}

impl fmt::Debug for AdapterConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never print endpoints that may embed keys, or bearers.
        f.debug_struct("AdapterConfig")
            .field("leader_sock", &self.leader_sock.is_some())
            .field("inbox_dir", &self.inbox_dir.is_some())
            .field("codex", &self.codex.is_some())
            .field("kimi_url", &self.kimi_url.is_some())
            .field("routine_url", &self.routine_url.is_some())
            .field("claude_fire_url", &self.claude_fire_url.is_some())
            .field("codex_cloud_env", &self.codex_cloud_env.is_some())
            .finish()
    }
}

/// Builds the adapter for `kind` from `config`.
pub fn adapter_for(kind: SessionKind, config: &AdapterConfig) -> Box<dyn WakeAdapter> {
    match (kind.surface, kind.provider) {
        (Surface::Local, ProviderKind::Grok) => Box::new(GrokLeaderAdapter {
            leader_sock: config.leader_sock.clone(),
        }),
        (Surface::Local, ProviderKind::Codex) => {
            Box::new(CodexAppServerAdapter::new(config.codex.clone()))
        }
        (Surface::Local, ProviderKind::KimiCode) => Box::new(KimiServerAdapter::new(
            config.kimi_url.clone(),
            config.kimi_bearer.clone(),
        )),
        (Surface::Local, ProviderKind::ClaudeCode) => {
            Box::new(ClaudeChannelAdapter::new(config.inbox_dir.clone()))
        }
        (Surface::Local, ProviderKind::Cursor) => Box::new(CursorAgentAdapter {
            inbox_dir: config.inbox_dir.clone(),
        }),
        (Surface::Web, ProviderKind::Cursor) => Box::new(RoutineWebhookAdapter {
            provider: ProviderKind::Cursor,
            url: config.routine_url.clone(),
            bearer: config.routine_bearer.clone(),
        }),
        (Surface::Web, ProviderKind::ClaudeCode) => Box::new(ClaudeRoutineFireAdapter {
            url: config.claude_fire_url.clone(),
            bearer: config.claude_fire_bearer.clone(),
        }),
        (Surface::Web, ProviderKind::Codex) => Box::new(CodexCloudAdapter),
        // A web session whose host handed a routine URL is woken through it; without one
        // there is no inbound path.
        (Surface::Web, provider @ (ProviderKind::KimiCode | ProviderKind::Grok)) => {
            if config.routine_url.as_deref().is_some_and(|u| !u.is_empty()) {
                Box::new(RoutineWebhookAdapter {
                    provider,
                    url: config.routine_url.clone(),
                    bearer: config.routine_bearer.clone(),
                })
            } else {
                Box::new(NoInboundAdapter { provider })
            }
        }
    }
}

// ---------------------------------------------------------------- local --

/// Grok CLI: ACP `session/prompt` on an already-running `leader.sock`.

pub struct GrokLeaderAdapter {
    /// `M4A_LEADER_SOCK`.
    pub leader_sock: Option<PathBuf>,
}

impl WakeAdapter for GrokLeaderAdapter {
    fn kind(&self) -> SessionKind {
        SessionKind::local(ProviderKind::Grok)
    }

    fn probe(&self, _session: &ProviderSession) -> Result<(), WakeError> {
        match self.leader_sock.as_deref() {
            Some(path) if path.exists() => Ok(()),
            Some(_) => Err(WakeError::Unavailable("leader socket missing".into())),
            None => Err(WakeError::Unavailable("leader socket unset".into())),
        }
    }

    fn wake(
        &mut self,
        session: &ProviderSession,
        letter: &WakeLetter<'_>,
    ) -> Result<WakeOutcome, WakeError> {
        let _ = (&session, &letter);
        #[cfg(not(feature = "wake-grok"))]
        return Err(WakeError::Unavailable("this build was made without feature wake-grok".into()));
        #[cfg(feature = "wake-grok")]
        self.probe(session)?;
        #[cfg(feature = "wake-grok")]
        let Some(sock) = self.leader_sock.as_deref() else {
            return Err(WakeError::Unavailable("leader socket unset".into()));
        };
        #[cfg(feature = "wake-grok")]
        let cwd = session_cwd(session)?;
        #[cfg(feature = "wake-grok")]
        {
            grok_leader::wake_decrypted_room_blocking(sock, &session.session_id, &cwd, &wake_prompt(session, letter))
                .map(|()| WakeOutcome::Delivered)
                .map_err(|e| WakeError::Transport(e.to_string()))
        }
    }
}

/// Cursor CLI: the session's own `stop` hook (`m4a-inbox drain --format
/// cursor-stop`) returns `followup_message`, which Cursor auto-submits as
/// the next user turn. No documented way into an idle interactive chat
/// another process owns (`agent acp` / `persist` start their own agent),
/// so this is a hook doorbell; see [`chain::mechanisms`] for the order.
pub struct CursorAgentAdapter {
    /// `M4A_INBOX_DIR`.
    pub inbox_dir: Option<PathBuf>,
}

impl WakeAdapter for CursorAgentAdapter {
    fn kind(&self) -> SessionKind {
        SessionKind::local(ProviderKind::Cursor)
    }

    fn probe(&self, session: &ProviderSession) -> Result<(), WakeError> {
        InboxHookAdapter::new(self.kind(), HookFlavor::CursorStop, self.inbox_dir.clone())
            .probe(session)
    }

    fn wake(
        &mut self,
        session: &ProviderSession,
        letter: &WakeLetter<'_>,
    ) -> Result<WakeOutcome, WakeError> {
        if !cfg!(feature = "wake-cursor") {
            return Err(WakeError::Unavailable("this build was made without feature wake-cursor".into()));
        }
        InboxHookAdapter::new(self.kind(), HookFlavor::CursorStop, self.inbox_dir.clone())
            .wake(session, letter)
    }
}

// ------------------------------------------------------------------ web --

/// Vendor-hosted web session woken by its own webhook routine (Cursor /
/// Grok Bot boxes). Same JSON object `m4a-web-client` posts today
/// (`body`, `from`, `from_nick`, `to`, `event_id`, `room`, `reply`).
pub struct RoutineWebhookAdapter {
    /// Vendor of the web session.
    pub provider: ProviderKind,
    /// Routine URL. In memory only.
    pub url: Option<String>,
    /// Routine key. In memory only.
    pub bearer: Option<String>,
}

impl WakeAdapter for RoutineWebhookAdapter {
    fn kind(&self) -> SessionKind {
        SessionKind::web(self.provider)
    }

    fn probe(&self, _session: &ProviderSession) -> Result<(), WakeError> {
        match self.url.as_deref() {
            Some(url) if !url.is_empty() => Ok(()),
            _ => Err(WakeError::Unavailable("routine url unset".into())),
        }
    }

    fn wake(
        &mut self,
        session: &ProviderSession,
        letter: &WakeLetter<'_>,
    ) -> Result<WakeOutcome, WakeError> {
        if !cfg!(feature = "wake-routine") {
            return Err(WakeError::Unavailable("this build was made without feature wake-routine".into()));
        }
        self.probe(session)?;
        let url = self.url.as_deref().unwrap_or_default();
        let reply = reply_hint(&session.nick, letter.from_nick);
        let mut body = serde_json::Map::new();
        for (k, v) in [
            ("body", Some(letter.body)),
            ("from", Some(letter.from_nick)),
            ("event_id", Some(letter.event_id)),
            ("room", letter.room),
            ("from_nick", Some(letter.from_nick)),
            ("to", Some(session.nick.as_str())),
            ("reply", Some(reply.as_str())),
        ] {
            if let Some(v) = v.map(str::trim).filter(|v| !v.is_empty() || k == "body") {
                body.insert(k.into(), serde_json::Value::String(v.into()));
            }
        }
        #[cfg(not(feature = "wake-routine"))]
        {
            let _ = (url, body);
            return Err(WakeError::Unavailable("this build was made without feature wake-routine".into()));
        }
        #[cfg(feature = "wake-routine")]
        routine::post_routine_json(url, &serde_json::Value::Object(body), self.bearer.as_deref())
            .map(|()| WakeOutcome::Delivered)
            .map_err(|e| WakeError::Transport(e.to_string()))
    }
}

/// Claude Code on the web, LAST resort: routine `/fire`
/// (`POST https://api.anthropic.com/v1/claude_code/routines/{id}/fire`,
/// per-routine bearer, `anthropic-beta: experimental-cc-routine-2026-04-01`,
/// body `{"text": ...}`). Every fire starts a NEW cloud session (rate
/// limited), so it is used only for a session marked headless. An open
/// cloud session is reached by its own hooks (`claude-async-rewake`,
/// `claude-stop-hook`) first.
pub struct ClaudeRoutineFireAdapter {
    /// Fire URL. In memory only.
    pub url: Option<String>,
    /// Routine token. In memory only.
    pub bearer: Option<String>,
}

/// `anthropic-beta` value for routine fire (override `M4A_CLAUDE_ROUTINE_BETA`).
pub const CLAUDE_ROUTINE_BETA: &str = "experimental-cc-routine-2026-04-01";

impl WakeAdapter for ClaudeRoutineFireAdapter {
    fn kind(&self) -> SessionKind {
        SessionKind::web(ProviderKind::ClaudeCode)
    }

    fn probe(&self, session: &ProviderSession) -> Result<(), WakeError> {
        if self.url.as_deref().is_none_or(str::is_empty) {
            return Err(WakeError::Unavailable(
                "claude routine fire url unset".into(),
            ));
        }
        if self.bearer.as_deref().is_none_or(str::is_empty) {
            return Err(WakeError::Unavailable("claude routine token unset".into()));
        }
        if !session.headless {
            return Err(WakeError::Unavailable(
                "session is open; routine fire would start a new session".into(),
            ));
        }
        Ok(())
    }

    fn wake(
        &mut self,
        session: &ProviderSession,
        letter: &WakeLetter<'_>,
    ) -> Result<WakeOutcome, WakeError> {
        if !cfg!(feature = "wake-claude") {
            return Err(WakeError::Unavailable("this build was made without feature wake-claude".into()));
        }
        self.probe(session)?;
        let url = self.url.as_deref().unwrap_or_default();
        let token = self.bearer.as_deref().unwrap_or_default();
        let parsed = reqwest::Url::parse(url)
            .ok()
            .filter(|u| u.scheme() == "https" || is_loopback_http(u))
            .ok_or_else(|| WakeError::Unavailable("claude routine fire url invalid".into()))?;
        let beta = std::env::var("M4A_CLAUDE_ROUTINE_BETA")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| CLAUDE_ROUTINE_BETA.to_string());
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .map_err(|err| WakeError::Transport(err.without_url().to_string()))?;
        let response = client
            .post(parsed)
            .bearer_auth(token)
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", beta)
            .json(&serde_json::json!({"text": wake_prompt(session, letter)}))
            .send()
            .map_err(|err| WakeError::Transport(err.without_url().to_string()))?;
        if response.status().is_success() {
            Ok(WakeOutcome::Delivered)
        } else {
            Err(WakeError::Transport(format!(
                "routine fire status {}",
                response.status().as_u16()
            )))
        }
    }
}

fn is_loopback_http(url: &reqwest::Url) -> bool {
    url.scheme() == "http"
        && matches!(
            url.host_str(),
            Some("127.0.0.1" | "localhost" | "[::1]" | "::1")
        )
}

/// Codex cloud (stub, vendor-side fix).
///
/// TODO(codex-cloud): `codex cloud exec --env <ENV_ID> <prompt>` creates a
/// NEW task; no documented follow-up into an existing cloud task.
pub struct CodexCloudAdapter;

/// Vendor web surface with no documented inbound trigger (Kimi web, Grok web).
pub struct NoInboundAdapter {
    /// Vendor.
    pub provider: ProviderKind,
}

macro_rules! refuse_adapter {
    ($ty:ty, $kind:expr, $err:ident) => {
        impl WakeAdapter for $ty {
            fn kind(&self) -> SessionKind {
                $kind(self)
            }

            fn probe(&self, _session: &ProviderSession) -> Result<(), WakeError> {
                Err(WakeError::$err(self.kind()))
            }

            fn wake(
                &mut self,
                _session: &ProviderSession,
                _letter: &WakeLetter<'_>,
            ) -> Result<WakeOutcome, WakeError> {
                Err(WakeError::$err(self.kind()))
            }
        }
    };
}

refuse_adapter!(
    CodexCloudAdapter,
    |_: &CodexCloudAdapter| SessionKind::web(ProviderKind::Codex),
    NotImplemented
);
refuse_adapter!(
    NoInboundAdapter,
    |adapter: &NoInboundAdapter| SessionKind::web(adapter.provider),
    NoInboundTrigger
);

#[cfg_attr(not(feature = "wake-grok"), allow(dead_code))]
pub(crate) fn session_cwd(session: &ProviderSession) -> Result<String, WakeError> {
    session
        .cwd
        .as_deref()
        .map(Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok())
        .map(|path| path.display().to_string())
        .filter(|cwd| !cwd.is_empty())
        .ok_or_else(|| WakeError::Unavailable("session cwd is empty".into()))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn session(kind: SessionKind) -> ProviderSession {
        ProviderSession {
            kind,
            session_id: "s-1".into(),
            nick: "alice".into(),
            cwd: Some(PathBuf::from("/tmp")),
            headless: false,
        }
    }

    pub(crate) fn letter<'a>(body: &'a str) -> WakeLetter<'a> {
        WakeLetter {
            body,
            from_nick: "carol",
            event_id: "$ev1",
            room: None,
        }
    }

    #[test]
    fn web_grok_uses_the_routine_only_when_one_is_configured() {
        let kind = SessionKind::web(ProviderKind::Grok);
        let none = adapter_for(kind, &AdapterConfig::default());
        assert!(none.probe(&session(kind)).is_err());
        let cfg = AdapterConfig { routine_url: Some("http://127.0.0.1:9/hook".into()), ..Default::default() };
        let routine = adapter_for(kind, &cfg);
        assert!(routine.probe(&session(kind)).is_ok());
    }

    #[test]
    fn ids_round_trip() {
        for kind in ProviderKind::ALL {
            assert_eq!(ProviderKind::parse(kind.id()), Some(kind));
        }
        assert_eq!(
            ProviderKind::parse("Claude-Code"),
            Some(ProviderKind::ClaudeCode)
        );
        assert_eq!(ProviderKind::parse("gemini"), None);
        assert!(ProviderKind::CORE.iter().all(|kind| !kind.is_optional()));
        assert!(ProviderKind::Cursor.is_optional());
        assert_eq!(
            SessionKind::web(ProviderKind::Codex).to_string(),
            "codex/web"
        );
    }

    #[test]
    fn every_kind_gets_an_adapter_of_that_kind() {
        let config = AdapterConfig::default();
        for provider in ProviderKind::ALL {
            for kind in [SessionKind::local(provider), SessionKind::web(provider)] {
                assert_eq!(adapter_for(kind, &config).kind(), kind);
            }
        }
    }

    #[test]
    fn stubs_and_unconfigured_adapters_refuse_without_side_effects() {
        let config = AdapterConfig::default();
        let cases = [
            (SessionKind::local(ProviderKind::Cursor), "unavailable"),
            (SessionKind::web(ProviderKind::ClaudeCode), "unavailable"),
            (SessionKind::web(ProviderKind::Codex), "not implemented"),
            (SessionKind::web(ProviderKind::KimiCode), "no inbound"),
            (SessionKind::web(ProviderKind::Grok), "no inbound"),
            (SessionKind::web(ProviderKind::Cursor), "unavailable"),
            (SessionKind::local(ProviderKind::Grok), "unavailable"),
            (SessionKind::local(ProviderKind::Codex), "unavailable"),
            (SessionKind::local(ProviderKind::KimiCode), "unavailable"),
            (SessionKind::local(ProviderKind::ClaudeCode), "unavailable"),
        ];
        for (kind, expect) in cases {
            let mut adapter = adapter_for(kind, &config);
            let s = session(kind);
            let err = adapter.wake(&s, &letter("x")).unwrap_err().to_string();
            assert!(err.contains(expect), "{kind}: {err}");
        }
    }

    #[test]
    fn prompt_carries_reply_command_and_body() {
        let text = wake_prompt(
            &session(SessionKind::local(ProviderKind::Codex)),
            &letter("ping"),
        );
        assert!(text.contains("m4a-send --as alice --to carol"));
        assert!(text.ends_with("ping"));
    }

    #[test]
    fn config_debug_hides_values() {
        let config = AdapterConfig {
            kimi_bearer: Some("k-secret".into()),
            routine_url: Some("https://example.invalid/hook".into()),
            ..AdapterConfig::default()
        };
        let shown = format!("{config:?}");
        assert!(!shown.contains("secret") && !shown.contains("example"));
    }
}

#[cfg(test)]
mod g4a_alignment_tests {
    use super::ProviderKind;

    /// hatchery-node maps g4a ids (claude, codex, grok, kimi) to these names before
    /// `m4a-agent account provision --provider ...`; every one must parse, and g4a's own ids too.
    #[test]
    fn names_hatchery_node_sends_all_parse() {
        for (sent, kind) in [
            ("claude-code", ProviderKind::ClaudeCode),
            ("codex", ProviderKind::Codex),
            ("grok", ProviderKind::Grok),
            ("kimi-code", ProviderKind::KimiCode),
            ("claude", ProviderKind::ClaudeCode),
            ("kimi", ProviderKind::KimiCode),
        ] {
            assert_eq!(ProviderKind::parse(sent), Some(kind), "{sent}");
        }
        assert_eq!(ProviderKind::parse("custom"), None);
    }
}
