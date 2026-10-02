use crate::{
    resolve_session_option_launch_for, AgentId, AgentSpec, ApprovalLevel, InitialPromptMode,
    NativeDraftMode, RuntimePlatform, SessionOptionCatalogError, SessionOptionSelection,
};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::PathBuf;
use thiserror::Error;

pub const MAX_LAUNCH_PROMPT_BYTES: usize = 16 * 1024 * 1024;
pub const WINDOWS_INLINE_LAUNCH_MAX_CHARS: usize = 24_000;

#[derive(Clone, Eq, PartialEq)]
pub struct EnvMutation {
    pub key: OsString,
    /// `None` removes the variable from the child environment.
    pub value: Option<OsString>,
}

impl fmt::Debug for EnvMutation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EnvMutation")
            .field("key", &self.key)
            .field("action", &if self.value.is_some() { "set" } else { "remove" })
            .finish()
    }
}

/// Generic host-only description of one stdio MCP server a spawn should
/// expose to its agent: an ACP-transport agent gets `name`/`program`/`args`/
/// `env` translated into one `session/new.mcpServers` stdio entry; a
/// PTY-transport child instead gets `env` installed into its own OS
/// environment (whatever that child needs to reach or relaunch the same
/// server is `env`'s job to carry -- this type never interprets a key or a
/// value). Naming, endpoint/token shape, and any trace opt-in are entirely
/// the caller's concern; this crate and every other gate4agent-owned crate
/// never grow harness- or task-specific vocabulary of their own.
///
/// Lives here, one level below both `gate4agent-runtime-native` (which
/// resolves it) and `gate4agent-shell-native` (whose ACP branch reads it):
/// the dependency between those two crates runs one way only
/// (`gate4agent-shell-native` cannot depend on `gate4agent-runtime-native`),
/// so a type shared by both has to live below the pair, not in either one.
/// It is carried as a plain in-process value the whole way -- never a wire
/// or protocol type.
#[derive(Clone)]
pub struct McpServerSpec {
    name: String,
    program: OsString,
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
}

impl McpServerSpec {
    pub fn new(
        name: impl Into<String>,
        program: OsString,
        args: Vec<OsString>,
        env: Vec<(OsString, OsString)>,
    ) -> Result<Self, McpServerSpecError> {
        let name = name.into();
        if name.is_empty() {
            return Err(McpServerSpecError::EmptyName);
        }
        if program.is_empty() {
            return Err(McpServerSpecError::EmptyProgram);
        }
        Ok(Self {
            name,
            program,
            args,
            env,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn program(&self) -> &OsStr {
        &self.program
    }

    pub fn args(&self) -> &[OsString] {
        &self.args
    }

    pub fn env(&self) -> &[(OsString, OsString)] {
        &self.env
    }
}

impl fmt::Debug for McpServerSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpServerSpec")
            .field("name", &self.name)
            .field(
                "env_keys",
                &self.env.iter().map(|(key, _)| key).collect::<Vec<_>>(),
            )
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum McpServerSpecError {
    #[error("MCP server spec name must not be empty")]
    EmptyName,
    #[error("MCP server spec program must not be empty")]
    EmptyProgram,
}

#[derive(Clone, Eq, PartialEq)]
pub struct LaunchRequest {
    pub working_dir: PathBuf,
    pub prompt: Option<String>,
    pub extra_args: Vec<OsString>,
    pub env: Vec<EnvMutation>,
    pub platform: RuntimePlatform,
    pub session_options: Option<SessionOptionSelection>,
}

impl fmt::Debug for LaunchRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LaunchRequest")
            .field("working_dir", &self.working_dir)
            .field("has_prompt", &self.prompt.is_some())
            .field("extra_args_len", &self.extra_args.len())
            .field("env", &self.env)
            .field("platform", &self.platform)
            .field("has_session_options", &self.session_options.is_some())
            .finish()
    }
}

impl Default for LaunchRequest {
    fn default() -> Self {
        Self {
            working_dir: PathBuf::new(),
            prompt: None,
            extra_args: Vec::new(),
            env: Vec::new(),
            platform: RuntimePlatform::current(),
            session_options: None,
        }
    }
}

/// A vendor ACP session-mode id -- the `id` a provider's `session/new`/
/// `session/load` handshake result names under `modes.availableModes`
/// (`gate4agent`'s `src/acp/protocol.rs::SessionMode`), and the exact value
/// `session/set_mode`'s own `modeId` parameter takes to select it
/// (`SessionSetModeParams`). A newtype over the raw wire string, kept
/// distinct from [`ApprovalLevelResolution::Supported`]'s own `args` field
/// even on the one row (`claude`) where the two happen to share a spelling
/// (`bypassPermissions`/`acceptEdits`/`plan`): argv and an ACP mode id are
/// two different mechanisms applied by two different transports (PTY vs.
/// ACP), and this type exists so a caller that only checked the string can
/// never silently substitute one for the other.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModeId(String);

impl ModeId {
    /// Construct a mode id from its raw wire string.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The raw wire string, e.g. as sent in `session/set_mode`'s `modeId`
    /// field or compared against an agent's own announced
    /// `modes.availableModes[].id`.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ModeId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The result of resolving a provider + [`ApprovalLevel`] to concrete launch
/// behaviour.
///
/// This exists because a level's *name* and the vendor mode it launches can
/// drift apart: `claude`'s `ReadOnly` used to launch `--permission-mode
/// default`, which is claude's own **interactive** mode -- it asks about
/// everything, exactly the opposite of what the name promises. Deferral
/// logic that matched on the level's name rather than on this fact got that
/// backwards: the one level whose vendor mode actually asks was the level
/// where asking was assumed impossible. `asks_for_permission` is the fix --
/// declared beside each row's flag, from that vendor mode's own documented
/// behaviour, so nothing downstream has to (mis)infer it from a name again.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApprovalLevelResolution {
    /// This provider has a verified vendor mode for this level.
    Supported {
        /// The CLI flag(s) that select the vendor mode, lifted verbatim from
        /// vendor documentation where one exists. Applied only to the PTY
        /// transport (via [`approval_level_args`]) -- the ACP transport has
        /// its own, separate mechanism: `acp_mode_id`, below.
        args: Vec<String>,
        /// Whether that vendor mode's own documented behaviour has the CLI
        /// raise a permission question on the wire at all -- never inferred
        /// from the level's name.
        asks_for_permission: bool,
        /// The ACP `session/set_mode` mode id that applies this row's level
        /// over the ACP transport, when one is sourced from vendor
        /// documentation or a live capture -- never guessed from `args`.
        /// `None` means this catalog has no confirmed ACP mode id for this
        /// provider x level yet: an honest gap, not an invented one, exactly
        /// like an empty `args` row (see this function's own doc comment for
        /// which rows are `None` and why). The ACP transport (`gate4agent`'s
        /// `src/acp`, `gate4agent-shell-native`) must treat `None` here the
        /// same as `Unsupported` below for any level but
        /// [`ApprovalLevel::Unmanaged`]: with no known id, `session/
        /// set_mode` has nothing to call, so no ACP mechanism exists to
        /// enforce the request at all -- see `Unmanaged`'s own row for the
        /// one case where `None` means something else.
        acp_mode_id: Option<ModeId>,
    },
    /// This provider has no verified mode for the requested level. The ACP
    /// transport (the only caller that applies this table -- see
    /// `approval_level_args`'s doc comment) must refuse the spawn by name
    /// here rather than silently falling back to the vendor's own default:
    /// an unrequested default is a wider authority than what was asked for,
    /// most acute for `ReadOnly` on a provider with no read-only mode at
    /// all -- substituting an "ask" mode for it would be exactly the
    /// name-vs-behaviour drift this type exists to remove.
    Unsupported,
}

/// Resolve a provider + [`ApprovalLevel`] to its vendor mode, using only
/// vendor-documented behaviour -- never the level's own name. Each row below
/// carries a confidence note; several are marked UNCONFIRMED deliberately,
/// per the "an honest unconfirmed beats a guessed bool" rule this table
/// exists to enforce.
///
/// Since 2026-09-02 each `Supported` row also carries `acp_mode_id:
/// Option<ModeId>` -- the `session/set_mode` mode id that applies this SAME
/// level over the ACP transport, sourced independently of `args`. This
/// exists because `args` turned out to be fiction for ACP: measured live,
/// both running ACP adapters (`claude`, `codex`) spawn with no `--permission-
/// mode` flag reaching them at all, because `npx`-wrapped specs never
/// receive one (`applicable_approval_args`, `gate4agent`'s
/// `src/acp/spawn.rs`) -- the adapter simply starts in its own default,
/// confirmed on the wire as `mode:Mode="auto"`, which approves itself. `args`
/// keeps meaning exactly what it always meant for the PTY transport; for ACP
/// the level lives in `acp_mode_id` instead, and only where one is actually
/// sourced -- `None` is left rather than guessed, the same honesty bar
/// `args` already holds itself to.
///
/// - `claude` and `codex` have a verified flag and verified `asks_for_permission`
///   for all three non-`Unmanaged` levels.
/// - `grok` (xAI Grok Build) has Ask (default), Auto, and Always-approve
///   modes, but no documented read-only mode. Confirmed 2026-10-02 against
///   xAI agent-mode + permissions user guides: `FullAuto` uses
///   `--always-approve` (alias `--yolo`; Claude-compat
///   `--permission-mode bypassPermissions` is also accepted) as an *agent*
///   option between `agent` and `stdio`; `Moderate` uses
///   `--permission-mode auto`; `ReadOnly` is `Unsupported` -- refused, not
///   silently downgraded to Ask. ACP `_meta.yoloMode` / `autoMode` on
///   `session/new` are documented peer levers (not yet wired into
///   `AcpSessionOptions`).
/// - `kimi` (Kimi Code) confirms `--yolo` as a real flag (from its own
///   release notes: rejected only when combined with `--prompt`, i.e. it is
///   otherwise accepted); its "skip everything" semantics are inferred from
///   the universal industry meaning of "yolo mode" rather than read directly
///   from a Kimi Code permissions doc, so treat that inference as
///   HIGH-CONFIDENCE but UNCONFIRMED-at-the-source. Kimi's release notes
///   also name `--auto` and `--plan` flags, but only as flags rejected
///   together with `--prompt` -- there is no confirmation either applies to
///   (or has the same meaning under) the `kimi acp` subcommand this project
///   actually spawns, so neither is wired into `args` here. All three
///   managed levels carry `acp_mode_id: None` now: measured 2026-09-05,
///   `kimi.exe 0.29.0`'s `session/new` result (the native binary at
///   `%USERPROFILE%\.kimi-code\bin\kimi.exe`, spawned directly by `kimi acp`
///   since `1fb14ca`) carries `configOptions` (a `model` select) and no
///   `modes` field at all -- there is no ACP mode mechanism to source an id
///   from any more. The 2026-09-02 measurement that put `yolo`/`auto`/`plan`
///   here was a different build: the npm shell shim running under WSL
///   interop, not this native binary.
/// - `Unmanaged` always resolves `Supported` with no flag and
///   `asks_for_permission: false`, for every agent ID including one this
///   catalog does not recognize: it imposes nothing by definition, so it can
///   never be refused, and it decides `session/request_permission`
///   immediately (auto-approve) rather than parking it -- some providers'
///   ACP clients won't wait out a deferred decision.
/// - An agent ID this catalog does not recognize resolves `Unsupported` at
///   every other level: this function never guesses at an unknown
///   provider's flag surface, and refusing is honest where the old
///   behaviour (silently launching with no flag at all) was not.
///
/// Station-profile note (docs only): this table is the **sandbox /
/// approval** axis of `{providerHome, cwd, sandbox, network,
/// browserProfile?}`. It does **not** encode OS Landlock/Seatbelt/Windows
/// token choice, Codex `networkAccess` (catalog `provider_native` Moderate
/// `-c` overlay), Claude Bash `sandbox.network.*` via ProviderHome `--settings` overlay (permits→allowedDomains; PTY/non-Win), Grok `--sandbox`
/// profiles, or a dig2browser profile id — see hatchery-websession-docs
/// `research/station-profile-and-os-sandbox-matrix-2026-10-02.md`,
/// `research/claude-kimi-network-argv-vs-station-catalog-2026-10-02.md`,
/// and `research/grok-linux-child-network-vs-station-catalog-2026-10-02.md`
/// (Claude native Windows = no vendor Bash sandbox; Claude network is
/// settings-shaped not ApprovalLevel; Kimi has no first-party OS-sandbox /
/// network matrix; Grok child-network is profile/Linux-only — inventing
/// Claude/Kimi/Grok `provider_native` network keys is refused).
pub fn approval_level_resolution(agent_id: &AgentId, level: ApprovalLevel) -> ApprovalLevelResolution {
    use ApprovalLevelResolution::{Supported, Unsupported};
    match (agent_id.as_str(), level) {
        // claude -- code.claude.com/docs/en/cli-reference `--permission-mode`.
        ("claude", ApprovalLevel::FullAuto) => Supported {
            args: vec!["--permission-mode".to_owned(), "bypassPermissions".to_owned()],
            // bypassPermissions: every tool call is auto-approved. Never asks.
            asks_for_permission: false,
            // claude-agent-acp exposes the CLI's own permission modes as ACP
            // session modes under the identical spelling (CHANGELOG 0.71.0
            // "align Claude modes...", 0.67.0 "expose permission mode
            // kinds", 0.25.0 "Add auto permission mode support"); measured
            // live, the handshake's default `currentModeId` is the bare
            // string `"auto"` (`mode:Mode="auto"`), confirming the id space
            // is the raw mode string, not a separate ACP-only vocabulary.
            acp_mode_id: Some(ModeId::new("bypassPermissions")),
        },
        ("claude", ApprovalLevel::Moderate) => Supported {
            args: vec!["--permission-mode".to_owned(), "acceptEdits".to_owned()],
            // acceptEdits: file edits are auto-approved, everything else
            // (Bash, other tools) still raises a permission question.
            asks_for_permission: true,
            acp_mode_id: Some(ModeId::new("acceptEdits")), // see FullAuto's comment above.
        },
        ("claude", ApprovalLevel::ReadOnly) => Supported {
            // `--permission-mode plan` remains the PTY-transport (CLI)
            // vendor-documented flag for this level; it is `args`'s own
            // mechanism and untouched by the ACP finding below.
            args: vec!["--permission-mode".to_owned(), "plan".to_owned()],
            // measured 2026-09-02, session/new of claude-agent-acp, see
            // docs/gate4agent/audits/gate4agent-acp-slice1-proof-2026-09-02.md
            // Measured live over `session/set_mode`, `plan` neither refuses
            // nor asks: a write inside the working directory completed
            // silently and the file was created -- the opposite of the
            // vendor-doc-sourced "Plan mode refuses edits" claim this row
            // used to carry. claude's only mode where both an in-cwd and an
            // out-of-cwd write raised `session/request_permission` is
            // `default`, so `ReadOnly` moves there: `may_ask: true`,
            // boundary "any write", not just "outside the workspace".
            asks_for_permission: true,
            acp_mode_id: Some(ModeId::new("default")),
        },

        // codex -- OpenAI Codex CLI `--sandbox` / `--ask-for-approval`.
        ("codex", ApprovalLevel::FullAuto) => Supported {
            args: vec!["--dangerously-bypass-approvals-and-sandbox".to_owned()],
            // measured 2026-09-02, session/new of codex-acp, see
            // docs/gate4agent/audits/gate4agent-acp-slice1-proof-2026-09-02.md
            // codex-acp announces this level's ACP mode as
            // `agent-full-access` ("Full access"); measured live a write
            // completed silently and reported success -- never asks, same
            // as the PTY bypass flag above.
            asks_for_permission: false,
            acp_mode_id: Some(ModeId::new("agent-full-access")),
        },
        ("codex", ApprovalLevel::Moderate) => Supported {
            args: vec![
                "--sandbox".to_owned(),
                "workspace-write".to_owned(),
                "--ask-for-approval".to_owned(),
                "on-request".to_owned(),
            ],
            // measured 2026-09-02, session/new of codex-acp, see
            // docs/gate4agent/audits/gate4agent-acp-slice1-proof-2026-09-02.md
            // codex-acp announces this level's ACP mode as `agent`
            // ("Approve for me"); measured live it wrote and reported
            // success with no `session/request_permission` at all -- unlike
            // the PTY `on-request` flag above (which still escalates on a
            // command outside the sandbox), the ACP mode itself never asks.
            asks_for_permission: false,
            acp_mode_id: Some(ModeId::new("agent")),
        },
        ("codex", ApprovalLevel::ReadOnly) => Supported {
            args: vec![
                "--sandbox".to_owned(),
                "read-only".to_owned(),
                "--ask-for-approval".to_owned(),
                "never".to_owned(),
            ],
            // measured 2026-09-02, session/new of codex-acp, see
            // docs/gate4agent/audits/gate4agent-acp-slice1-proof-2026-09-02.md
            // codex-acp announces this level's ACP mode as `read-only`
            // ("Ask for approval"); measured live a write inside the
            // working directory completed silently, one outside it raised
            // `session/request_permission` -- boundary "outside the
            // workspace", `may_ask: true`. The one row (with claude's own
            // `default`) whose vendor mode asks rather than refuses.
            asks_for_permission: true,
            acp_mode_id: Some(ModeId::new("read-only")),
        },

        // grok (xAI Grok Build) -- see this function's own doc comment for
        // the three documented modes and per-row confidence.
        ("grok", ApprovalLevel::FullAuto) => Supported {
            // Confirmed 2026-10-02 (xAI agent-mode + permissions guides):
            // `grok agent --always-approve stdio` (alias `--yolo`). Claude-
            // compatible `--permission-mode bypassPermissions` is accepted
            // as the same mode, but `--always-approve` is the product name
            // the agent-mode examples use. Spawn inserts these between
            // `agent` and `stdio` (`src/acp/spawn.rs`).
            args: vec!["--always-approve".to_owned()],
            asks_for_permission: false,
            // No sourced ACP `modes.availableModes[].id` yet (slice-1 skipped
            // grok). `_meta.yoloMode: true` on `session/new` is a documented
            // peer lever; not wired into `AcpSessionOptions` this pass.
            acp_mode_id: None,
        },
        ("grok", ApprovalLevel::Moderate) => Supported {
            // Confirmed 2026-10-02: `--permission-mode auto` (classifier
            // auto-approves safer tools; others may still prompt / block in
            // non-interactive sessions). Same agent-option position as
            // FullAuto. `_meta.autoMode: true` is the ACP peer lever.
            args: vec!["--permission-mode".to_owned(), "auto".to_owned()],
            asks_for_permission: true,
            acp_mode_id: None,
        },
        // No documented read-only mode exists for Grok Build at all (Ask /
        // Auto / Always-approve, none of them a restriction) -- refuse by
        // name rather than silently substitute "Ask", which would launch at
        // a wider authority than requested.
        ("grok", ApprovalLevel::ReadOnly) => Unsupported,

        // kimi (Kimi Code) -- see this function's own doc comment.
        ("kimi", ApprovalLevel::FullAuto) => Supported {
            // `--auto`, not `--yolo`. Both are real top-level options, read
            // from `kimi --help` 2026-09-09, and they differ exactly where
            // this level cares: `-y, --yolo` is "Auto-approve regular tool
            // calls; the agent MAY STILL ASK QUESTIONS", while `--auto` is
            // "Start in auto permission mode: fully autonomous, the agent
            // WILL NOT ASK QUESTIONS". Measured live under `--yolo`, kimi
            // still answered every `g4a_*` MCP call "rejected at the approval
            // prompt" without ever asking the host -- the question it was
            // still allowed to raise. `--auto` is what `FullAuto` means.
            // Both are top-level, so they precede the `acp` subcommand (see
            // `direct_command`, `src/acp/spawn.rs`); `kimi acp` itself takes
            // only `--login`/`--help`.
            args: vec!["--auto".to_owned()],
            asks_for_permission: false,
            // measured 2026-09-05: kimi.exe 0.29.0 session/new offers no
            // modes; the 2026-09-02 yolo/auto/plan measurement was the npm
            // shim under WSL interop
            acp_mode_id: None,
        },
        ("kimi", ApprovalLevel::Moderate) => Supported {
            // `--auto` is named in Kimi's own release notes but only as a
            // flag rejected together with `--prompt`; whether it applies to
            // (or means the same thing under) the `kimi acp` subcommand this
            // project spawns is UNCONFIRMED, so it is not wired into `args`
            // here -- falls back to no flag, same as `Unmanaged`.
            args: Vec::new(),
            asks_for_permission: false,
            // measured 2026-09-05: kimi.exe 0.29.0 session/new offers no
            // modes; the 2026-09-02 yolo/auto/plan measurement was the npm
            // shim under WSL interop
            acp_mode_id: None,
        },
        // `--plan`'s applicability to the PTY `kimi acp` subcommand remains
        // UNCONFIRMED, so `args` stays empty here, same as `Moderate` above.
        ("kimi", ApprovalLevel::ReadOnly) => Supported {
            args: Vec::new(),
            asks_for_permission: false,
            // measured 2026-09-05: kimi.exe 0.29.0 session/new offers no
            // modes; the 2026-09-02 yolo/auto/plan measurement was the npm
            // shim under WSL interop
            acp_mode_id: None,
        },

        // Impose nothing, for every agent ID: never refused, and the
        // vendor's own default is unpredictable by definition -- assume it
        // can ask. No ACP mode id either: imposing nothing means there is
        // nothing for `session/set_mode` to apply, so the ACP transport
        // (`required_acp_mode`, `gate4agent-shell-native/src/lib.rs`) must
        // read `acp_mode_id: None` here as "apply nothing", never as "no
        // mechanism exists" -- the one row where those two readings differ.
        (_, ApprovalLevel::Unmanaged) => Supported {
            args: Vec::new(),
            // Unmanaged decides `session/request_permission` immediately
            // (auto-approve) rather than parking it: some providers' ACP
            // clients won't wait out a deferred decision (kimi reports the
            // call as refused), so this level must never defer.
            asks_for_permission: false,
            acp_mode_id: None,
        },
        // An agent ID this mapping does not carry verified data for at all.
        _ => Unsupported,
    }
}

/// Map an approval level to the provider CLI flags that implement it, for
/// the given agent.
///
/// This is the single source of truth for the level -> argv mapping; nothing
/// else in this codebase should hardcode one of these flags. A thin
/// argv-only accessor over [`approval_level_resolution`], reading only its
/// `args` field -- never its `acp_mode_id`, which has no argv shape at all.
/// Where a provider has no verified flag for a level --
/// including a level `approval_level_resolution` refuses outright -- this
/// returns an empty `Vec`, the same as `ApprovalLevel::Unmanaged`, rather
/// than inventing one.
///
/// `plan_launch` (the PTY launch planner in this module) never calls this: a
/// PTY is opened by a human, who picks their own agent's permission flags,
/// so this crate does not impose one. Nothing in this crate calls it for ACP
/// either any more: that transport used to be this function's one real
/// caller, but argv turned out to be fiction for it (see
/// `approval_level_resolution`'s own doc comment) -- it now applies a level
/// exclusively through `session/set_mode`, reading `acp_mode_id` off
/// `approval_level_resolution` directly instead of calling this function at
/// all. This function is kept for whatever future PTY-adjacent caller wants
/// the plain argv table without the rest of the resolution -- today, nothing
/// in this workspace calls it.
pub fn approval_level_args(agent_id: &AgentId, level: ApprovalLevel) -> Vec<String> {
    match approval_level_resolution(agent_id, level) {
        ApprovalLevelResolution::Supported { args, .. } => args,
        ApprovalLevelResolution::Unsupported => Vec::new(),
    }
}

/// Shell-free executable plan for an interactive agent CLI.
#[derive(Clone, Eq, PartialEq)]
pub struct LaunchPlan {
    pub agent_id: AgentId,
    pub program: OsString,
    pub args: Vec<OsString>,
    pub working_dir: PathBuf,
    pub env: Vec<EnvMutation>,
    /// Prompt that must be delivered only after the readiness policy succeeds.
    pub followup_prompt: Option<String>,
    /// Reviewable draft that must be inserted after draft readiness succeeds.
    pub followup_draft: Option<String>,
    /// Options actually applied by generated launch arguments after accounting
    /// for later caller-provided overrides.
    pub applied_session_options: Option<SessionOptionSelection>,
}

impl fmt::Debug for LaunchPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LaunchPlan")
            .field("agent_id", &self.agent_id)
            .field("program", &self.program)
            .field("args_len", &self.args.len())
            .field("working_dir", &self.working_dir)
            .field("env", &self.env)
            .field("has_followup_prompt", &self.followup_prompt.is_some())
            .field("has_followup_draft", &self.followup_draft.is_some())
            .field(
                "has_applied_session_options",
                &self.applied_session_options.is_some(),
            )
            .finish()
    }
}

pub fn plan_launch(
    spec: &AgentSpec,
    request: LaunchRequest,
) -> Result<LaunchPlan, LaunchPlanError> {
    if !spec.supports_platform(request.platform) {
        return Err(LaunchPlanError::UnsupportedPlatform {
            agent: spec.id.clone(),
            platform: request.platform,
        });
    }

    let prompt = request.prompt.filter(|prompt| !prompt.is_empty());
    if let Some(prompt) = &prompt {
        if prompt.len() > MAX_LAUNCH_PROMPT_BYTES {
            return Err(LaunchPlanError::PromptTooLarge {
                bytes: prompt.len(),
                max: MAX_LAUNCH_PROMPT_BYTES,
            });
        }
    }

    let trailing_agent_args = request
        .extra_args
        .iter()
        .map(|value| value.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let resolved_session_options = request
        .session_options
        .as_ref()
        .map(|selection| resolve_session_option_launch_for(spec, selection, &trailing_agent_args))
        .transpose()
        .map_err(LaunchPlanError::SessionOptions)?;
    let mut args: Vec<OsString> = spec.launch.fixed_args.iter().map(OsString::from).collect();
    if let Some(resolved) = &resolved_session_options {
        args.extend(resolved.args.iter().map(OsString::from));
    }
    args.extend(request.extra_args);
    let mut followup_prompt = None;

    if let Some(prompt) = prompt {
        match &spec.prompt.initial {
            InitialPromptMode::None => {
                return Err(LaunchPlanError::PromptUnsupported(spec.id.clone()));
            }
            InitialPromptMode::Positional { option_terminator } => {
                let prompt_arg_start = args.len();
                if *option_terminator {
                    args.push(OsString::from("--"));
                }
                args.push(OsString::from(&prompt));
                if request.platform == RuntimePlatform::Windows
                    && (windows_wrapper_unsafe_text(&prompt)
                        || estimated_windows_launch_chars_for(
                            OsStr::new(&spec.launch.program),
                            &args,
                            &request.env,
                        ) > WINDOWS_INLINE_LAUNCH_MAX_CHARS)
                {
                    args.truncate(prompt_arg_start);
                    followup_prompt = Some(prompt);
                }
            }
            InitialPromptMode::Flag { flag } | InitialPromptMode::InteractiveFlag { flag } => {
                args.push(OsString::from(flag));
                args.push(OsString::from(prompt));
            }
            InitialPromptMode::AgentNativeQuery => {
                return Err(LaunchPlanError::NativePlannerRequired(spec.id.clone()));
            }
            InitialPromptMode::AfterReady => {
                followup_prompt = Some(prompt);
            }
        }
    }

    let plan = LaunchPlan {
        agent_id: spec.id.clone(),
        program: OsString::from(&spec.launch.program),
        args,
        working_dir: request.working_dir,
        env: request.env,
        followup_prompt,
        followup_draft: None,
        applied_session_options: resolved_session_options.and_then(|resolved| resolved.applied),
    };
    validate_platform_budget(&plan, request.platform)?;
    Ok(plan)
}

/// Plan a reviewable initial draft without accidentally submitting it as a task.
pub fn plan_draft_launch(
    spec: &AgentSpec,
    request: LaunchRequest,
    draft: String,
) -> Result<LaunchPlan, LaunchPlanError> {
    if request
        .prompt
        .as_ref()
        .is_some_and(|prompt| !prompt.is_empty())
    {
        return Err(LaunchPlanError::ConflictingPromptAndDraft);
    }
    if draft.len() > MAX_LAUNCH_PROMPT_BYTES {
        return Err(LaunchPlanError::PromptTooLarge {
            bytes: draft.len(),
            max: MAX_LAUNCH_PROMPT_BYTES,
        });
    }

    let platform = request.platform;
    let mut plan = plan_launch(spec, request)?;
    if draft.is_empty() {
        return Ok(plan);
    }

    match &spec.prompt.native_draft {
        Some(NativeDraftMode::Flag { flag }) => {
            plan.args.push(OsString::from(flag));
            plan.args.push(OsString::from(&draft));
            if platform == RuntimePlatform::Windows
                && (windows_wrapper_unsafe_text(&draft)
                    || estimated_windows_launch_chars(&plan) > WINDOWS_INLINE_LAUNCH_MAX_CHARS)
            {
                plan.args.pop();
                plan.args.pop();
                plan.followup_draft = Some(draft);
            }
        }
        None => plan.followup_draft = Some(draft),
    }
    validate_platform_budget(&plan, platform)?;
    Ok(plan)
}

fn validate_platform_budget(
    plan: &LaunchPlan,
    platform: RuntimePlatform,
) -> Result<(), LaunchPlanError> {
    if platform != RuntimePlatform::Windows {
        return Ok(());
    }
    let chars = estimated_windows_launch_chars(plan);
    if chars > WINDOWS_INLINE_LAUNCH_MAX_CHARS {
        return Err(LaunchPlanError::WindowsInlineLaunchTooLarge {
            chars,
            max: WINDOWS_INLINE_LAUNCH_MAX_CHARS,
        });
    }
    Ok(())
}

fn estimated_windows_launch_chars(plan: &LaunchPlan) -> usize {
    estimated_windows_launch_chars_for(&plan.program, &plan.args, &plan.env)
}

fn estimated_windows_launch_chars_for(
    program: &OsStr,
    args: &[OsString],
    env: &[EnvMutation],
) -> usize {
    let argv_chars = std::iter::once(program)
        .chain(args.iter().map(OsString::as_os_str))
        .map(|value| value.to_string_lossy().chars().count().saturating_add(1))
        .sum::<usize>();
    let env_chars = env
        .iter()
        .map(|mutation| {
            mutation.key.to_string_lossy().chars().count()
                + mutation
                    .value
                    .as_ref()
                    .map(|value| value.to_string_lossy().chars().count())
                    .unwrap_or_default()
                + 2
        })
        .sum::<usize>();
    argv_chars.saturating_add(env_chars)
}

fn windows_wrapper_unsafe_text(value: &str) -> bool {
    value.chars().any(|character| {
        matches!(
            character,
            '\0' | '\r' | '\n' | '"' | '%' | '!' | '^' | '&' | '|' | '<' | '>' | '(' | ')'
        )
    })
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum LaunchPlanError {
    #[error("agent '{agent}' does not support runtime platform {platform:?}")]
    UnsupportedPlatform {
        agent: AgentId,
        platform: RuntimePlatform,
    },
    #[error("agent '{0}' does not accept an initial prompt")]
    PromptUnsupported(AgentId),
    #[error("agent '{0}' requires a provider-native startup query planner")]
    NativePlannerRequired(AgentId),
    #[error("prompt is {bytes} bytes; the launch limit is {max} bytes")]
    PromptTooLarge { bytes: usize, max: usize },
    #[error("launch request cannot contain both an auto-submitted prompt and a reviewable draft")]
    ConflictingPromptAndDraft,
    #[error("Windows inline launch is {chars} characters; the safe limit is {max}")]
    WindowsInlineLaunchTooLarge { chars: usize, max: usize },
    #[error(transparent)]
    SessionOptions(#[from] SessionOptionCatalogError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin_registry;

    fn request(prompt: &str) -> LaunchRequest {
        LaunchRequest {
            prompt: Some(prompt.to_owned()),
            ..LaunchRequest::default()
        }
    }

    fn args_as_strings(plan: &LaunchPlan) -> Vec<String> {
        plan.args
            .iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn environment_and_launch_debug_preserve_names_and_actions_without_values() {
        let spec = builtin_registry().get_by_id("codex").unwrap();
        let mut request = request("prompt-value-must-be-redacted");
        request.extra_args = vec![OsString::from("extra-arg-value-must-be-redacted")];
        request.env = vec![
            EnvMutation {
                key: OsString::from("EXPLICIT_SECRET"),
                value: Some(OsString::from("environment-value-must-be-redacted")),
            },
            EnvMutation {
                key: OsString::from("AMBIENT_SECRET"),
                value: None,
            },
        ];
        let request_debug = format!("{request:?}");
        assert!(request_debug.contains("EXPLICIT_SECRET"));
        assert!(request_debug.contains("AMBIENT_SECRET"));
        assert!(request_debug.contains("action: \"set\""));
        assert!(request_debug.contains("action: \"remove\""));
        assert!(request_debug.contains("has_prompt: true"));
        assert!(request_debug.contains("extra_args_len: 1"));
        assert!(!request_debug.contains("environment-value-must-be-redacted"));
        assert!(!request_debug.contains("prompt-value-must-be-redacted"));
        assert!(!request_debug.contains("extra-arg-value-must-be-redacted"));
        let plan = plan_launch(spec, request).unwrap();

        let debug = format!("{plan:?}");
        assert!(debug.contains("EXPLICIT_SECRET"));
        assert!(debug.contains("AMBIENT_SECRET"));
        assert!(debug.contains("action: \"set\""));
        assert!(debug.contains("action: \"remove\""));
        assert!(!debug.contains("environment-value-must-be-redacted"));
        assert!(!debug.contains("prompt-value-must-be-redacted"));
    }

    #[test]
    fn grok_terminates_options_before_a_positional_prompt() {
        let spec = builtin_registry().get_by_id("grok").unwrap();
        let plan = plan_launch(spec, request("--version")).unwrap();
        assert_eq!(args_as_strings(&plan), ["--", "--version"]);
        assert!(plan.followup_prompt.is_none());
    }

    #[test]
    fn flag_initial_prompt_mode_delivers_without_shell_quoting() {
        let mut spec = builtin_registry().get_by_id("claude").unwrap().clone();
        spec.prompt.initial = InitialPromptMode::Flag {
            flag: "--prompt".to_owned(),
        };
        let prompt = "fix 'quotes'\nand Unicode: Привет";
        let plan = plan_launch(&spec, request(prompt)).unwrap();
        assert_eq!(args_as_strings(&plan), ["--prompt", prompt]);
    }

    #[test]
    fn kimi_defers_prompt_until_readiness() {
        let spec = builtin_registry().get_by_id("kimi").unwrap();
        let plan = plan_launch(spec, request("inspect the repository")).unwrap();
        assert!(plan.args.is_empty());
        assert_eq!(
            plan.followup_prompt.as_deref(),
            Some("inspect the repository")
        );
    }

    #[test]
    fn unsafe_windows_positional_initial_prompt_falls_back_to_post_ready_paste() {
        let prompt = "quote \" percent % ampersand & pipe | caret ^ and Unicode: Привет";
        for id in ["claude", "codex"] {
            let spec = builtin_registry().get_by_id(id).unwrap();
            let plan = plan_launch(
                spec,
                LaunchRequest {
                    platform: RuntimePlatform::Windows,
                    ..request(prompt)
                },
            )
            .unwrap();
            assert!(!args_as_strings(&plan).iter().any(|arg| arg == prompt), "{id}");
            assert_eq!(plan.followup_prompt.as_deref(), Some(prompt), "{id}");
        }
    }

    #[test]
    fn oversized_windows_positional_initial_prompt_falls_back_to_post_ready_paste() {
        let prompt = "x".repeat(WINDOWS_INLINE_LAUNCH_MAX_CHARS);
        for id in ["claude", "codex"] {
            let spec = builtin_registry().get_by_id(id).unwrap();
            let plan = plan_launch(
                spec,
                LaunchRequest {
                    platform: RuntimePlatform::Windows,
                    ..request(&prompt)
                },
            )
            .unwrap();
            assert!(!args_as_strings(&plan).iter().any(|arg| arg == &prompt), "{id}");
            assert_eq!(plan.followup_prompt.as_deref(), Some(prompt.as_str()), "{id}");
        }
    }

    #[test]
    fn claude_uses_native_prefill_for_a_reviewable_draft() {
        let spec = builtin_registry().get_by_id("claude").unwrap();
        let plan = plan_draft_launch(
            spec,
            LaunchRequest {
                platform: RuntimePlatform::Linux,
                ..LaunchRequest::default()
            },
            "review before submit".to_owned(),
        )
        .unwrap();
        assert_eq!(
            args_as_strings(&plan),
            ["--prefill", "review before submit"]
        );
        assert!(plan.followup_draft.is_none());
    }

    #[test]
    fn agents_without_native_prefill_defer_the_draft() {
        let spec = builtin_registry().get_by_id("kimi").unwrap();
        let plan = plan_draft_launch(
            spec,
            LaunchRequest {
                platform: RuntimePlatform::Linux,
                ..LaunchRequest::default()
            },
            "review before submit".to_owned(),
        )
        .unwrap();
        assert_eq!(plan.followup_draft.as_deref(), Some("review before submit"));
        assert!(plan.followup_prompt.is_none());
    }

    #[test]
    fn unsafe_windows_wrapper_draft_falls_back_to_post_ready_paste() {
        let spec = builtin_registry().get_by_id("claude").unwrap();
        let plan = plan_draft_launch(
            spec,
            LaunchRequest {
                platform: RuntimePlatform::Windows,
                ..LaunchRequest::default()
            },
            "inspect & explain".to_owned(),
        )
        .unwrap();
        assert!(plan.args.is_empty());
        assert_eq!(plan.followup_draft.as_deref(), Some("inspect & explain"));
    }

    #[test]
    fn session_options_precede_user_args_and_record_only_effective_values() {
        let spec = builtin_registry().get_by_id("claude").unwrap();
        let plan = plan_launch(
            spec,
            LaunchRequest {
                session_options: Some(
                    SessionOptionSelection::new("opus")
                        .with_value("effort", "xhigh")
                        .with_value("fastMode", true),
                ),
                extra_args: vec!["--model".into(), "haiku".into()],
                platform: RuntimePlatform::Linux,
                ..LaunchRequest::default()
            },
        )
        .unwrap();
        assert_eq!(
            args_as_strings(&plan),
            ["--model", "opus", "--effort", "xhigh", "--model", "haiku"]
        );
        assert!(plan.applied_session_options.is_none());
    }

    #[test]
    fn explicit_claude_options_are_composed_but_untouched_launches_stay_vanilla() {
        let spec = builtin_registry().get_by_id("claude").unwrap();
        let vanilla = plan_launch(
            spec,
            LaunchRequest {
                platform: RuntimePlatform::Linux,
                // "Vanilla" here means "no session options and no
                // operator-provided extra args".
                ..LaunchRequest::default()
            },
        )
        .unwrap();
        assert!(vanilla.args.is_empty());
        assert!(vanilla.applied_session_options.is_none());

        let selected = SessionOptionSelection::new("opus").with_value("effort", "xhigh");
        let plan = plan_launch(
            spec,
            LaunchRequest {
                session_options: Some(selected.clone()),
                platform: RuntimePlatform::Linux,
                ..LaunchRequest::default()
            },
        )
        .unwrap();
        assert_eq!(args_as_strings(&plan), ["--model", "opus", "--effort", "xhigh"]);
        assert_eq!(plan.applied_session_options, Some(selected));
    }

    #[test]
    fn launch_only_agents_cannot_borrow_another_provider_option_catalog() {
        let spec = builtin_registry().get_by_id("kimi").unwrap();
        assert!(matches!(
            plan_launch(
                spec,
                LaunchRequest {
                    session_options: Some(SessionOptionSelection::new("opus")),
                    platform: RuntimePlatform::Linux,
                    ..LaunchRequest::default()
                },
            ),
            Err(LaunchPlanError::SessionOptions(
                SessionOptionCatalogError::UnsupportedAgent(_)
            ))
        ));
    }

    #[test]
    fn approval_level_defaults_to_full_auto() {
        assert_eq!(ApprovalLevel::default(), ApprovalLevel::FullAuto);
    }

    #[test]
    fn approval_level_args_match_the_verified_provider_flag_table() {
        let claude = AgentId::new("claude").unwrap();
        assert_eq!(
            approval_level_args(&claude, ApprovalLevel::FullAuto),
            ["--permission-mode", "bypassPermissions"]
        );
        assert_eq!(
            approval_level_args(&claude, ApprovalLevel::Moderate),
            ["--permission-mode", "acceptEdits"]
        );
        assert_eq!(
            approval_level_args(&claude, ApprovalLevel::ReadOnly),
            ["--permission-mode", "plan"]
        );
        assert!(approval_level_args(&claude, ApprovalLevel::Unmanaged).is_empty());

        let codex = AgentId::new("codex").unwrap();
        assert_eq!(
            approval_level_args(&codex, ApprovalLevel::FullAuto),
            ["--dangerously-bypass-approvals-and-sandbox"]
        );
        assert_eq!(
            approval_level_args(&codex, ApprovalLevel::Moderate),
            [
                "--sandbox",
                "workspace-write",
                "--ask-for-approval",
                "on-request"
            ]
        );
        assert_eq!(
            approval_level_args(&codex, ApprovalLevel::ReadOnly),
            ["--sandbox", "read-only", "--ask-for-approval", "never"]
        );
        assert!(approval_level_args(&codex, ApprovalLevel::Unmanaged).is_empty());

        let grok = AgentId::new("grok").unwrap();
        assert_eq!(
            approval_level_args(&grok, ApprovalLevel::FullAuto),
            ["--always-approve"]
        );
        assert_eq!(
            approval_level_args(&grok, ApprovalLevel::Moderate),
            ["--permission-mode", "auto"]
        );
        assert!(approval_level_args(&grok, ApprovalLevel::Unmanaged).is_empty());

        let kimi = AgentId::new("kimi").unwrap();
        assert_eq!(approval_level_args(&kimi, ApprovalLevel::FullAuto), ["--auto"]);
        assert!(approval_level_args(&kimi, ApprovalLevel::Unmanaged).is_empty());
    }

    /// `grok` and `kimi` have exactly one verified flag each (`FullAuto`).
    /// Neither `Moderate` nor `ReadOnly` may fabricate a flag for either --
    /// at the argv-only layer both produce exactly the same empty result as
    /// `Unmanaged`. `grok`'s `ReadOnly` additionally resolves `Unsupported`
    /// one layer up (see
    /// `approval_level_resolution_matches_the_verified_provider_table` and
    /// `unsupported_resolution_still_yields_empty_argv_not_a_fabricated_flag`);
    /// `kimi`'s `Moderate` and `ReadOnly` resolve `Supported` with no
    /// `acp_mode_id` (measured 2026-09-05: `kimi.exe` 0.29.0 offers no ACP
    /// modes at all) and still no confirmed CLI flag -- this test only
    /// covers the argv-only accessor, which has no channel to carry either
    /// an ACP mode id or a refusal.
    #[test]
    fn grok_and_kimi_moderate_never_invents_a_flag_and_matches_unmanaged() {
        // grok's `Moderate` is deliberately absent from this loop: it now
        // carries `--permission-mode auto`, confirmed against xAI's own
        // documentation on 2026-09-02. This test guards against INVENTING a
        // flag, not against having one -- and the empty argv it used to
        // assert was itself the defect, since no flag reaches grok's default
        // "Ask", not "Auto", so a Moderate session ran narrower than it
        // asked for. `approval_level_resolution_matches_the_verified_
        // provider_table` pins the new row.
        for (id, levels) in [
            ("grok", &[ApprovalLevel::ReadOnly][..]),
            ("kimi", &[ApprovalLevel::Moderate, ApprovalLevel::ReadOnly][..]),
        ] {
            let agent = AgentId::new(id).unwrap();
            for level in levels.iter().copied() {
                let produced = approval_level_args(&agent, level);
                assert!(produced.is_empty(), "{id} at {level:?} must not fabricate a flag");
                assert_eq!(
                    produced,
                    approval_level_args(&agent, ApprovalLevel::Unmanaged),
                    "{id} at {level:?} must match the Unmanaged (no-op) result exactly"
                );
            }
        }
    }

    #[test]
    fn unmanaged_never_adds_a_flag_for_any_known_provider() {
        for id in ["claude", "codex", "grok", "kimi"] {
            let agent = AgentId::new(id).unwrap();
            assert!(
                approval_level_args(&agent, ApprovalLevel::Unmanaged).is_empty(),
                "{id}"
            );
        }
    }

    /// Item 3's table: one row per provider x level, asserting the expected
    /// flag(s), the expected `asks_for_permission`, AND (since 2026-09-02)
    /// the expected `acp_mode_id`, so a wrong claim about vendor behaviour
    /// fails loudly here instead of surfacing live as a missing
    /// `host-request-observed` or a session silently parked in the vendor's
    /// own default mode.
    #[test]
    fn approval_level_resolution_matches_the_verified_provider_table() {
        fn supported(
            args: &[&str],
            asks_for_permission: bool,
            acp_mode_id: Option<&str>,
        ) -> ApprovalLevelResolution {
            ApprovalLevelResolution::Supported {
                args: args.iter().map(|value| (*value).to_owned()).collect(),
                asks_for_permission,
                acp_mode_id: acp_mode_id.map(ModeId::new),
            }
        }

        let claude = AgentId::new("claude").unwrap();
        assert_eq!(
            approval_level_resolution(&claude, ApprovalLevel::FullAuto),
            supported(&["--permission-mode", "bypassPermissions"], false, Some("bypassPermissions"))
        );
        assert_eq!(
            approval_level_resolution(&claude, ApprovalLevel::Moderate),
            supported(&["--permission-mode", "acceptEdits"], true, Some("acceptEdits"))
        );
        assert_eq!(
            approval_level_resolution(&claude, ApprovalLevel::ReadOnly),
            supported(&["--permission-mode", "plan"], true, Some("default"))
        );
        assert_eq!(
            approval_level_resolution(&claude, ApprovalLevel::Unmanaged),
            supported(&[], false, None)
        );

        let codex = AgentId::new("codex").unwrap();
        assert_eq!(
            approval_level_resolution(&codex, ApprovalLevel::FullAuto),
            supported(
                &["--dangerously-bypass-approvals-and-sandbox"],
                false,
                Some("agent-full-access")
            )
        );
        assert_eq!(
            approval_level_resolution(&codex, ApprovalLevel::Moderate),
            supported(
                &["--sandbox", "workspace-write", "--ask-for-approval", "on-request"],
                false,
                Some("agent")
            )
        );
        assert_eq!(
            approval_level_resolution(&codex, ApprovalLevel::ReadOnly),
            supported(
                &["--sandbox", "read-only", "--ask-for-approval", "never"],
                true,
                Some("read-only")
            )
        );
        assert_eq!(
            approval_level_resolution(&codex, ApprovalLevel::Unmanaged),
            supported(&[], false, None)
        );

        let grok = AgentId::new("grok").unwrap();
        assert_eq!(
            approval_level_resolution(&grok, ApprovalLevel::FullAuto),
            supported(&["--always-approve"], false, None)
        );
        assert_eq!(
            approval_level_resolution(&grok, ApprovalLevel::Moderate),
            supported(&["--permission-mode", "auto"], true, None)
        );
        assert_eq!(
            approval_level_resolution(&grok, ApprovalLevel::ReadOnly),
            ApprovalLevelResolution::Unsupported
        );
        assert_eq!(
            approval_level_resolution(&grok, ApprovalLevel::Unmanaged),
            supported(&[], false, None)
        );

        // measured 2026-09-05: kimi.exe 0.29.0 session/new offers no modes;
        // the 2026-09-02 yolo/auto/plan measurement was the npm shim under
        // WSL interop.
        let kimi = AgentId::new("kimi").unwrap();
        assert_eq!(
            approval_level_resolution(&kimi, ApprovalLevel::FullAuto),
            supported(&["--auto"], false, None)
        );
        assert_eq!(
            approval_level_resolution(&kimi, ApprovalLevel::Moderate),
            supported(&[], false, None)
        );
        assert_eq!(
            approval_level_resolution(&kimi, ApprovalLevel::ReadOnly),
            supported(&[], false, None)
        );
        assert_eq!(
            approval_level_resolution(&kimi, ApprovalLevel::Unmanaged),
            supported(&[], false, None)
        );

        let unknown = AgentId::new("some-future-provider").unwrap();
        for level in [ApprovalLevel::FullAuto, ApprovalLevel::Moderate, ApprovalLevel::ReadOnly] {
            assert_eq!(
                approval_level_resolution(&unknown, level),
                ApprovalLevelResolution::Unsupported,
                "{level:?}"
            );
        }
        assert_eq!(
            approval_level_resolution(&unknown, ApprovalLevel::Unmanaged),
            supported(&[], false, None)
        );
    }

    /// The regression guard named in Item 3: every level but `Unmanaged`
    /// must either resolve a concrete, non-`"auto"` ACP mode id or refuse
    /// outright -- never silently `None`, which is how a session ends up
    /// parked in whatever mode the agent announces on its own (measured
    /// live as `mode:Mode="auto"`, which approves itself). This is the
    /// catalog-layer half of the guard; `gate4agent-shell-native`'s own
    /// `required_acp_mode_never_silently_permits_auto_except_for_unmanaged`
    /// asserts the same property at the ACP spawn-site layer that actually
    /// consumes this table.
    #[test]
    fn acp_mode_id_is_never_silently_absent_for_a_managed_level() {
        for id in ["claude", "codex", "grok", "kimi", "some-future-provider"] {
            let agent = AgentId::new(id).unwrap();
            for level in [ApprovalLevel::FullAuto, ApprovalLevel::Moderate, ApprovalLevel::ReadOnly] {
                match approval_level_resolution(&agent, level) {
                    // Every sourced mode id must be a concrete, non-`"auto"`
                    // string -- kimi no longer has a row that qualifies for
                    // an exception here: measured 2026-09-05, `kimi.exe`
                    // 0.29.0 offers no ACP modes at all, so every one of its
                    // managed levels now falls into the `None` arm below.
                    ApprovalLevelResolution::Supported { acp_mode_id: Some(mode_id), .. } => {
                        assert_ne!(
                            mode_id.as_str(),
                            "auto",
                            "{id} at {level:?} must never silently resolve the literal vendor default 'auto'"
                        );
                    }
                    ApprovalLevelResolution::Supported { acp_mode_id: None, .. } => {} // refused one layer up (`required_acp_mode`)
                    ApprovalLevelResolution::Unsupported => {}
                }
            }
            assert_eq!(
                approval_level_resolution(&agent, ApprovalLevel::Unmanaged),
                ApprovalLevelResolution::Supported {
                    args: Vec::new(),
                    asks_for_permission: false,
                    acp_mode_id: None,
                },
                "{id}: Unmanaged is the one level allowed to carry no ACP mode id"
            );
        }
    }

    /// A level `approval_level_resolution` refuses (`Unsupported`) must
    /// still surface as an empty `Vec` from the argv-only accessor, exactly
    /// like `Unmanaged` -- `approval_level_args` has no channel to carry a
    /// refusal, which is precisely why the ACP transport must call
    /// `approval_level_resolution` directly and refuse the spawn itself
    /// rather than trusting this function's empty result to mean "safe to
    /// launch with no flag". `kimi` is not one of this test's providers:
    /// its `ReadOnly` still resolves `Supported` (with `acp_mode_id: None`
    /// since 2026-09-05, see `approval_level_resolution`'s own doc comment),
    /// not `Unsupported` -- `grok` is the one provider left with no
    /// read-only mode of any kind.
    #[test]
    fn unsupported_resolution_still_yields_empty_argv_not_a_fabricated_flag() {
        let grok = AgentId::new("grok").unwrap();
        assert!(approval_level_args(&grok, ApprovalLevel::ReadOnly).is_empty());
        assert!(matches!(
            approval_level_resolution(&grok, ApprovalLevel::ReadOnly),
            ApprovalLevelResolution::Unsupported
        ));
    }

    #[test]
    fn unrecognized_agent_gets_no_approval_flag_at_any_level() {
        let unknown = AgentId::new("some-future-provider").unwrap();
        for level in [
            ApprovalLevel::FullAuto,
            ApprovalLevel::Moderate,
            ApprovalLevel::ReadOnly,
            ApprovalLevel::Unmanaged,
        ] {
            assert!(approval_level_args(&unknown, level).is_empty(), "{level:?}");
        }
    }

    /// A PTY is opened by a human, who picks their own agent's permission
    /// flags -- `plan_launch` (the PTY launch planner) must never inject one
    /// of `approval_level_args`'s flags on its own, for any provider,
    /// regardless of that provider's own verified `FullAuto` flag. This is
    /// the flip side of `approval_level_args_match_the_verified_provider_flag_table`:
    /// the table exists and is correct, but this planner is no longer one of
    /// its callers -- see `plan_launch`'s own doc comment on
    /// `approval_level_args` for who is (the ACP transport).
    #[test]
    fn plan_launch_never_injects_an_approval_flag_pty_has_no_approval_axis() {
        for id in ["claude", "codex", "grok", "kimi"] {
            let spec = builtin_registry().get_by_id(id).unwrap();
            let plan = plan_launch(
                spec,
                LaunchRequest {
                    platform: RuntimePlatform::Linux,
                    ..LaunchRequest::default()
                },
            )
            .unwrap();
            let full_auto_flag_tokens = approval_level_args(&spec.id, ApprovalLevel::FullAuto);
            for token in &full_auto_flag_tokens {
                assert!(
                    !args_as_strings(&plan).contains(token),
                    "{id}: plan_launch must never inject its own FullAuto flag token {token:?}"
                );
            }
        }
    }

}
