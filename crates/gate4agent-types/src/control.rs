use crate::{
    AdapterBinding, AdapterFamily, AgentId, CapabilityModelSummary, CapabilityProbeFailure,
    CapabilityProbeRequest, CapabilitySnapshot, HistoryCandidateSummary, HistoryOperation,
    HistoryQuery, HistorySessionRecord, HistorySnapshot, InputAction, InputPrepareError,
    PreparedInput, PreparedInputKind, ResumeAuthorityTarget, ResumeLaunchRequest,
    ResumeSessionSummary, ResumeSnapshot, ResumeTarget, SessionOptionSelection,
};
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

pub const CONTROL_PROTOCOL_VERSION: u16 = 28;
pub const CONTROL_SESSIONS_MAX: usize = 512;
pub const CONTROL_INSTANCE_IDENTITIES_CAPACITY: u32 = 4_096;
pub const CONTROL_INSTANCE_IDENTITIES_MAX: usize = CONTROL_INSTANCE_IDENTITIES_CAPACITY as usize;
pub const TERMINAL_ROWS_MAX: u16 = 1_000;
pub const TERMINAL_COLUMNS_MAX: u16 = 1_000;
pub const WORKING_DIRECTORY_MAX_BYTES: usize = 32_768;
pub const PROVIDER_INGRESS_EVENTS_MAX: usize = 32;
pub const PROVIDER_EVENT_TEXT_MAX_BYTES: usize = 262_144;
pub const PROVIDER_EVENT_ID_MAX_BYTES: usize = 512;
pub const PROVIDER_EVENT_TOOLS_MAX: usize = 256;
pub const PROVIDER_INTERACTIONS_MAX: usize = 64;
pub const PROVIDER_INTERACTION_RESPONSE_MAX_BYTES: usize = 32_768;
pub const PROVIDER_INTERACTION_FAILURE_MAX_BYTES: usize = 4_096;
pub const PROVIDER_SUBAGENTS_MAX: usize = 64;
pub const PROVIDER_SESSION_LOCATOR_MAX_BYTES: usize = 32_768;
pub const PROVIDER_PLAN_STEPS_MAX: usize = 256;
pub const PROVIDER_AVAILABLE_COMMANDS_MAX: usize = 256;
pub const PROVIDER_CONFIG_OPTIONS_MAX: usize = 256;
pub const PROVIDER_CONFIG_OPTION_CHOICES_MAX: usize = 256;
pub const PROVIDER_MODE_CATALOG_MAX: usize = 256;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentInstanceId(pub u64);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CommandId(pub u64);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OperationId(pub u64);

#[derive(
    Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct SessionGeneration(pub u64);

/// How much autonomy a freshly spawned provider CLI process is granted at
/// launch, independent of which transport (PTY, inline/pipe, ACP) execs it --
/// these are process launch arguments, the same axis regardless of transport.
///
/// The mapping from a level to actual CLI flags is per-provider, verified
/// against vendor documentation (and, for `grok`/`kimi`, against a third-party
/// harness's observed behavior), and lives in `gate4agent_catalog` -- the
/// crate that owns launch policy -- not here; this type only names the
/// levels. Where a provider has no verified intermediate flag (`grok` and
/// `kimi` do not have one for `Moderate` or `ReadOnly` as of this writing),
/// the catalog's mapping falls back to `Unmanaged` behavior (no injected
/// flag) rather than fabricating one -- see
/// `gate4agent_catalog::approval_level_args`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalLevel {
    /// No restriction: the provider CLI is launched with whatever flag
    /// grants it full autonomy (Claude `--permission-mode bypassPermissions`,
    /// Codex `--dangerously-bypass-approvals-and-sandbox`, Grok
    /// `--permission-mode bypassPermissions`, Kimi `--yolo`). Default --
    /// restricting is opt-in, not asking a human is the norm.
    #[default]
    FullAuto,
    /// The provider's own middle ground, when one is verified (Claude
    /// `--permission-mode acceptEdits`, Codex `--sandbox workspace-write
    /// --ask-for-approval on-request`). A provider with no verified
    /// intermediate flag falls back to `Unmanaged` -- never a fabricated
    /// flag.
    Moderate,
    /// Read-only: no writes, no command execution (Claude
    /// `--permission-mode default`, Codex `--sandbox read-only
    /// --ask-for-approval never`). A provider with no verified read-only
    /// flag falls back to `Unmanaged`.
    ReadOnly,
    /// Impose nothing: launch with no approval-related flag at all and let
    /// the provider CLI use whatever it is configured with on its own.
    Unmanaged,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StartRequest {
    pub working_directory: String,
    pub terminal_size: TerminalSize,
    #[serde(default)]
    pub initial_prompt: Option<String>,
    #[serde(default)]
    pub session_options: Option<SessionOptionSelection>,
    /// Approval-level axis for this spawn; defaults to
    /// `ApprovalLevel::FullAuto` when the caller does not set it, matching
    /// `ApprovalLevel`'s own default. `#[serde(default)]` so a peer that
    /// predates this field decodes it as the same default rather than
    /// failing to deserialize.
    #[serde(default)]
    pub approval_level: ApprovalLevel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ProviderRuntimePolicy {
    pub raw_pty_lifecycle: bool,
    pub semantic_readiness: bool,
    pub structured_prompt: bool,
    pub provider_session_identity: bool,
    pub semantic_resume: bool,
    /// Whether this session's provider-event ingestion may come from a
    /// declared hook adapter -- a native hooks contract the provider CLI
    /// itself calls over the authenticated loopback ingress route, with the
    /// node's own adapter normalizing the payload before it ever reaches the
    /// engine.
    ///
    /// This is deliberately NOT `semantic_readiness`, and granting one must
    /// never imply the other. `semantic_readiness` (and the `structured_
    /// prompt`/`provider_session_identity`/`semantic_resume` capabilities
    /// chained off it) authorize INFERRING provider semantics by parsing PTY
    /// terminal text -- that inference is only sound for a CLI version this
    /// build has a verified vendor terminal contract for (see
    /// `gate4agent-runtime-native`'s `VERIFIED_PROFILES`). A hook event is
    /// not an inference: the CLI is asserting it directly over a route this
    /// node authenticated, and the node's hook adapter -- not a terminal
    /// screen scanner -- turns it into a `ProviderEvent`. None of the
    /// terminal-behaviour verification a vendor contract encodes is
    /// relevant to that trust story, so `hook_semantics` is derived purely
    /// from "does the catalog declare a hook adapter for this provider" and
    /// never from a vendor version probe. Conflating the two would let a
    /// provider that merely declares a hook adapter silently unlock
    /// PTY-parsing semantics it was never verified for, or -- the bug this
    /// field fixes -- let a verified-semantic gate silently swallow every
    /// hook event a provider with no verified profile at all (grok, codex,
    /// kimi) sends over a route that is otherwise working end to end.
    pub hook_semantics: bool,
}

impl ProviderRuntimePolicy {
    pub fn new(
        raw_pty_lifecycle: bool,
        semantic_readiness: bool,
        structured_prompt: bool,
        provider_session_identity: bool,
        semantic_resume: bool,
        hook_semantics: bool,
    ) -> Result<Self, ProviderRuntimePolicyError> {
        let policy = Self {
            raw_pty_lifecycle,
            semantic_readiness,
            structured_prompt,
            provider_session_identity,
            semantic_resume,
            hook_semantics,
        };
        policy.validate()?;
        Ok(policy)
    }

    pub const fn raw_pty() -> Self {
        Self {
            raw_pty_lifecycle: true,
            semantic_readiness: false,
            structured_prompt: false,
            provider_session_identity: false,
            semantic_resume: false,
            hook_semantics: false,
        }
    }

    /// The all-false policy: no capability admitted at all. This is the
    /// correct shape for a transport that has no PTY and whose catalog entry
    /// declares no contract this build can grant a semantic capability from
    /// -- e.g. a Pipe-transport provider with no declared Pipe contract.
    /// Unlike `raw_pty()`, this does NOT claim a PTY lifecycle exists.
    pub const fn none() -> Self {
        Self {
            raw_pty_lifecycle: false,
            semantic_readiness: false,
            structured_prompt: false,
            provider_session_identity: false,
            semantic_resume: false,
            hook_semantics: false,
        }
    }

    /// Only `semantic_resume` and `hook_semantics` require the raw PTY
    /// lifecycle. `semantic_readiness`, `structured_prompt`, and
    /// `provider_session_identity` do NOT, on their own -- an ACP transport
    /// has no PTY at all, yet `session/prompt` and `session/update` are
    /// MANDATORY surface of the ACP protocol itself, and `session/new`
    /// returns a `sessionId` under that same specification, none of it an
    /// inference this build makes by parsing PTY terminal text the way it
    /// does for a verified PTY vendor contract. Granting `semantic_readiness`/
    /// `structured_prompt`/`provider_session_identity` with
    /// `raw_pty_lifecycle: false` is therefore a legitimate policy shape (see
    /// `gate4agent_node::provider_runtime::policy_for_transport`'s
    /// `TransportKind::Acp` arm), not a defect this validation should catch.
    ///
    /// `semantic_resume`/`hook_semantics` keep the old, stricter rule: today
    /// nothing derives either of the two for a transport other than a
    /// verified PTY vendor contract -- ACP's spec gives no resume guarantee
    /// analogous to `session/new`'s `sessionId`, and the engine separately
    /// refuses ACP resume outright -- so granting one without
    /// `raw_pty_lifecycle` remains a construction defect rather than a
    /// legitimate non-PTY policy shape.
    pub fn validate(self) -> Result<(), ProviderRuntimePolicyError> {
        if (self.semantic_resume || self.hook_semantics) && !self.raw_pty_lifecycle {
            return Err(ProviderRuntimePolicyError::SemanticCapabilityRequiresRawPty);
        }
        if self.structured_prompt && !self.semantic_readiness {
            return Err(ProviderRuntimePolicyError::StructuredPromptRequiresReadiness);
        }
        if self.semantic_resume && !self.provider_session_identity {
            return Err(ProviderRuntimePolicyError::ResumeRequiresSessionIdentity);
        }
        Ok(())
    }

    pub const fn admits(self, capability: ProviderRuntimeCapability) -> bool {
        match capability {
            ProviderRuntimeCapability::RawPtyLifecycle => self.raw_pty_lifecycle,
            ProviderRuntimeCapability::SemanticReadiness => self.semantic_readiness,
            ProviderRuntimeCapability::StructuredPrompt => self.structured_prompt,
            ProviderRuntimeCapability::ProviderSessionIdentity => {
                self.provider_session_identity
            }
            ProviderRuntimeCapability::SemanticResume => self.semantic_resume,
            ProviderRuntimeCapability::HookSemantics => self.hook_semantics,
        }
    }
}

impl<'de> Deserialize<'de> for ProviderRuntimePolicy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WirePolicy {
            raw_pty_lifecycle: bool,
            semantic_readiness: bool,
            structured_prompt: bool,
            provider_session_identity: bool,
            semantic_resume: bool,
            hook_semantics: bool,
        }

        let wire = WirePolicy::deserialize(deserializer)?;
        Self::new(
            wire.raw_pty_lifecycle,
            wire.semantic_readiness,
            wire.structured_prompt,
            wire.provider_session_identity,
            wire.semantic_resume,
            wire.hook_semantics,
        )
        .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderRuntimePolicyError {
    #[error("semantic resume and hook semantics require the raw PTY lifecycle")]
    SemanticCapabilityRequiresRawPty,
    #[error("structured prompts require semantic readiness")]
    StructuredPromptRequiresReadiness,
    #[error("semantic resume requires provider session identity")]
    ResumeRequiresSessionIdentity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderRuntimeCapability {
    RawPtyLifecycle,
    SemanticReadiness,
    StructuredPrompt,
    ProviderSessionIdentity,
    SemanticResume,
    /// Admits provider-event ingestion sourced from a declared hook adapter.
    /// Independent of `SemanticReadiness` -- see `ProviderRuntimePolicy::
    /// hook_semantics` for why the two must never stand in for each other.
    HookSemantics,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TerminalSize {
    pub rows: u16,
    pub columns: u16,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TerminalMouseProtocolEncoding {
    #[default]
    Default,
    Utf8,
    Sgr,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TerminalFrame {
    pub sequence: u64,
    pub size: TerminalSize,
    pub cursor_row: u16,
    pub cursor_column: u16,
    pub contents: String,
    pub formatted: Vec<u8>,
    #[serde(default)]
    pub scrollback_formatted: Vec<Vec<u8>>,
    #[serde(default)]
    pub alternate_screen: bool,
    #[serde(default)]
    pub mouse_protocol_enabled: bool,
    #[serde(default)]
    pub mouse_protocol_encoding: TerminalMouseProtocolEncoding,
    /// Unix-epoch milliseconds when this frame's screen state was
    /// materialized -- the one instant `PtyEventPublisher::snapshot` turns
    /// live terminal state into a frame. Every hop after that (the shell's
    /// `ObservationEnvelope`, the c2 relay, the harness's terminal ring
    /// buffer, the operator wire) carries this value through unchanged; none
    /// of them may recompute, refresh, or zero it, because the whole point
    /// of the field is to let something 200ms downstream in a ring buffer
    /// still answer "how stale am I". `#[serde(default)]` so a peer that
    /// predates this field decodes it as 0 -- "age unknown" -- instead of a
    /// fabricated timestamp. Diffing this value across two hosts also mixes
    /// in their clock skew, not just transit time, so a consumer comparing
    /// it against wall-clock time must say so rather than presenting the
    /// gap as pure network/queue latency.
    #[serde(default)]
    pub produced_at_unix_ms: u64,
    /// The screen classification at the instant THIS frame's screen was
    /// materialized -- stamped once at the node from the same snapshot that
    /// produced `contents`/`formatted`, and carried through every hop
    /// unchanged like `produced_at_unix_ms`; no hop may recompute it against
    /// its own (possibly staler) copy of the screen. `#[serde(default)]` so a
    /// peer that predates this field decodes it as `Unknown` -- "not
    /// classified" -- rather than a fabricated `Ready`; defaulting to
    /// `Unknown` and not `Ready` is the entire point of that default.
    #[serde(default)]
    pub screen_state: PtyScreenState,
    /// Whether bracketed-paste mode was enabled on the PTY at the instant
    /// THIS frame's screen was materialized, read straight off the same
    /// `vt100::Screen` snapshot that produced `contents`/`formatted` --
    /// `Some(true)` means the terminal application has turned the mode on
    /// (a paste is delivered to it as one bracketed block instead of
    /// keystrokes), `Some(false)` means it explicitly has not, and `None`
    /// means this frame predates the field or the value was never sampled.
    /// `None` is not a claim that the mode is off; a caller that needs to
    /// know must treat `None` the same as `PtyScreenState::Unknown` --
    /// absence of information, not a fabricated default.
    /// `skip_serializing_if` is load-bearing the same way it is on
    /// `HarnessRuntimeTerminalFrameV1::screen_state`: the key must be
    /// ABSENT from the JSON, not `null`, so a peer that predates this field
    /// decodes it as `None` rather than a fabricated `Some(false)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bracketed_paste: Option<bool>,
}

pub const FOREGROUND_PROCESS_NAME_MAX_BYTES: usize = 512;
pub const PTY_SCREEN_GATE_NAME_MAX_BYTES: usize = 128;
/// Bound on `OperatorGateState::options` -- large enough for any list a real
/// prompt has ever been observed to render (2-4 choices), small enough that
/// a garbled or hostile screen capture cannot inflate the wire payload.
pub const OPERATOR_GATE_OPTIONS_MAX: usize = 8;
/// Per-`OperatorGateOption::text` byte bound, same scale as
/// `PTY_SCREEN_GATE_NAME_MAX_BYTES` -- an option label is a single short
/// line off the screen, never a paragraph.
pub const OPERATOR_GATE_OPTION_TEXT_MAX_BYTES: usize = 128;
/// Bound on `OperatorGateSubject::Directory`'s `path`, matching the scale of
/// other path-shaped fields carried on this wire (see `WORKING_DIRECTORY_MAX_BYTES`
/// for the same order of magnitude on a full working-directory string).
pub const OPERATOR_GATE_PATH_MAX_BYTES: usize = 32_768;

/// What TYPE of blocking question `OperatorGateState` is showing, classified
/// from the screen's own top-level phrasing (see `startup_operator_gate` in
/// `gate4agent-shell-native`, the only producer). Distinct kinds exist so a
/// consumer can react differently to "an update is running" versus "type an
/// answer" without parsing `OperatorGateSubject`/`options` first.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OperatorGateKind {
    /// Trust the current project directory before the CLI will read or run
    /// anything in it.
    WorkspaceTrust,
    /// Trust a specific set of shell hooks the CLI discovered inside an
    /// already-trusted directory. Kept distinct from `WorkspaceTrust`: that
    /// gate is about the directory as a whole, this one is about hooks the
    /// CLI found inside it -- an operator reading `kind` should be able to
    /// tell which question is being asked.
    HookTrust,
    /// Sign in, or choose how to sign in / which credential to use.
    Authentication,
    /// The CLI (or a wrapper script fronting it) is installing or updating
    /// itself and wants a relaunch, regardless of which mechanism drives the
    /// update -- an in-app updater or an external package manager.
    VendorUpdate,
    /// A first-run welcome/setup screen unrelated to trust or auth (IDE
    /// integration notice, "press enter to continue" splash, and similar).
    Onboarding,
    /// Choosing a terminal color/text style during first-run setup.
    TerminalAppearance,
    /// A stored configuration format needs to be migrated/confirmed before
    /// the CLI continues.
    ConfigurationMigration,
}

impl OperatorGateKind {
    /// Short operator-facing label, one per variant, stable in wording with
    /// what this module classified as a bare string before `OperatorGateState`
    /// existed -- existing log lines and error messages that quote this text
    /// keep reading the same.
    pub fn label(&self) -> &'static str {
        match self {
            Self::WorkspaceTrust => "workspace trust",
            Self::HookTrust => "hook trust review",
            Self::Authentication => "authentication",
            Self::VendorUpdate => "vendor update",
            Self::Onboarding => "onboarding",
            Self::TerminalAppearance => "terminal appearance setup",
            Self::ConfigurationMigration => "configuration migration",
        }
    }
}

/// WHAT entity `OperatorGateState` is gating access to -- narrower than
/// `kind` (which says what TYPE of question this is): two `WorkspaceTrust`
/// gates always share `subject: Directory`, but the `path` detail (when a
/// matcher can read one off the screen) distinguishes which directory.
/// `Unknown` is the honest reading for a `kind` whose screen text does not
/// name a concrete subject from this list (a vendor updater or onboarding
/// splash is not "about" a directory, a hook set, or an account) -- it is
/// never upgraded into a guess.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum OperatorGateSubject {
    /// A project/workspace directory. `path` is the directory the screen
    /// names, when a matcher can read one off the text; `None` means the
    /// screen's phrasing did not carry one, not that there is no directory.
    Directory { path: Option<String> },
    /// A set of shell hooks discovered inside an already-trusted directory.
    /// `count` is the number reported on screen, when readable; `None` means
    /// unreadable, never zero.
    Hooks { count: Option<u32> },
    /// MCP servers configured for the project.
    McpServers,
    /// The signed-in account/identity.
    Account,
    /// An API key/credential value.
    ApiKey,
    /// Terminal color/text-style appearance.
    Appearance,
    /// No concrete subject from this list applies to the matched `kind`.
    Unknown,
}

/// HOW `OperatorGateState` is controlled -- what a caller resolving it
/// (typically a human, occasionally a scripted answer) needs to send.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OperatorGateInput {
    /// Choices are numbered (`1.`, `2.`, ...); confirmed with Enter after
    /// selecting a number.
    NumberedList,
    /// Choices are an unnumbered list navigated with arrow keys and a
    /// cursor glyph; confirmed with Enter.
    ArrowList,
    /// A single acknowledgement -- nothing to choose between, just Enter.
    PressEnter,
    /// Free text (a pasted code, a typed value) rather than a choice from a
    /// list.
    TextEntry,
    /// The screen's input mechanism was not recognized.
    Unknown,
}

/// What choosing a given `OperatorGateOption` does, inferred from the verb
/// in its own on-screen text (see `classify_operator_gate_option_semantics`
/// in `gate4agent-shell-native`) -- never from its position or number, since
/// neither is stable across CLIs or screen wraps.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OperatorGateOptionSemantics {
    /// Grants what the gate is asking for (trust, continue, proceed).
    Accept,
    /// Refuses what the gate is asking for (don't trust, continue without
    /// trusting) while still moving past the prompt.
    Decline,
    /// Opens a closer look before deciding (review the hooks/diff) rather
    /// than accepting or declining outright.
    Inspect,
    /// Leaves the CLI entirely rather than answering the prompt.
    Exit,
    /// The option's own text used none of the recognized verbs.
    Unknown,
}

/// One choice as rendered on screen inside an `OperatorGateState`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OperatorGateOption {
    /// The option's label exactly as it appears on screen (list-marker and
    /// leading whitespace stripped, nothing else altered) -- never
    /// paraphrased, so an operator reading it sees the same words the CLI
    /// rendered.
    pub text: String,
    pub semantics: OperatorGateOptionSemantics,
    /// Whether the screen's own cursor/highlight currently sits on this
    /// option. `false` means either it is not selected or selection is not
    /// visible on this screen shape -- there is no third state, because a
    /// consumer deciding "which option is highlighted right now" only ever
    /// needs to know if THIS one is.
    pub selected: bool,
}

impl OperatorGateOption {
    fn is_valid(&self) -> bool {
        !self.text.trim().is_empty()
            && self.text.len() <= OPERATOR_GATE_OPTION_TEXT_MAX_BYTES
            && !self.text.chars().any(char::is_control)
    }
}

/// The full classification of a screen recognized as an `OperatorGate`,
/// replacing what used to be a bare label string. `kind` is always known (a
/// matcher only returns this type once it has matched a specific gate
/// phrase); `subject`, `input`, and `options` degrade independently to
/// `Unknown`/empty when the screen's specific shape was not recognized --
/// never invented from `kind` alone.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OperatorGateState {
    pub kind: OperatorGateKind,
    pub subject: OperatorGateSubject,
    pub input: OperatorGateInput,
    /// Recognized on-screen choices, in on-screen order. Empty means no
    /// option list was recognized -- NOT that the screen has no choices;
    /// see `OperatorGateInput::Unknown` for the paired "input mechanism
    /// unrecognized either" case.
    pub options: Vec<OperatorGateOption>,
}

impl OperatorGateState {
    /// The gate with nothing known past `kind` -- `subject: Unknown`,
    /// `input: Unknown`, no options. This is the honest shape for a matched
    /// `kind` whose screen a parser has not (yet) learned to read past the
    /// phrase that identified it; never upgrade an unread screen into a
    /// guessed subject or option list.
    pub fn new(kind: OperatorGateKind) -> Self {
        Self {
            kind,
            subject: OperatorGateSubject::Unknown,
            input: OperatorGateInput::Unknown,
            options: Vec::new(),
        }
    }

    /// Builder-style: attach a recognized subject.
    pub fn with_subject(mut self, subject: OperatorGateSubject) -> Self {
        self.subject = subject;
        self
    }

    /// Builder-style: attach a recognized input mechanism and its options
    /// together, since one is meaningless without the other (an `Unknown`
    /// input never carries options, and options never accompany an
    /// unrecognized input mechanism).
    pub fn with_options(mut self, input: OperatorGateInput, options: Vec<OperatorGateOption>) -> Self {
        self.input = input;
        self.options = options;
        self
    }

    /// Bounds check matching `PtyScreenState::is_valid`'s own rationale --
    /// every string this type carries travels over the wire into an
    /// operator UI and must be non-empty (where required), control-character
    /// free, and within its per-field byte cap before anything downstream
    /// trusts it.
    pub fn is_valid(&self) -> bool {
        let subject_valid = match &self.subject {
            OperatorGateSubject::Directory { path: Some(path) } => {
                !path.trim().is_empty()
                    && path.len() <= OPERATOR_GATE_PATH_MAX_BYTES
                    && !path.chars().any(char::is_control)
            }
            OperatorGateSubject::Directory { path: None }
            | OperatorGateSubject::Hooks { .. }
            | OperatorGateSubject::McpServers
            | OperatorGateSubject::Account
            | OperatorGateSubject::ApiKey
            | OperatorGateSubject::Appearance
            | OperatorGateSubject::Unknown => true,
        };
        subject_valid
            && self.options.len() <= OPERATOR_GATE_OPTIONS_MAX
            && self.options.iter().all(OperatorGateOption::is_valid)
    }
}

impl std::fmt::Display for OperatorGateState {
    /// Renders as `kind.label()` alone -- the same text this whole type
    /// replaced used to carry as its only payload -- so an existing
    /// `format!("...{gate}...")` call site keeps reading the same message
    /// after this type lands under it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.kind.label())
    }
}

impl OperatorGateState {
    /// Longer operator-facing rendering than `Display`: the kind label,
    /// plus a compact list of recognized options (currently-selected one
    /// prefixed `*`) when `options` is non-empty. `Display` stays kind-only
    /// on purpose (see its own doc comment) -- this is for a surface that
    /// can afford, and wants, the fuller picture once one was parsed.
    pub fn describe(&self) -> String {
        if self.options.is_empty() {
            return self.kind.label().to_owned();
        }
        let options = self
            .options
            .iter()
            .map(|option| {
                if option.selected {
                    format!("*{}", option.text)
                } else {
                    option.text.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("{} [{options}]", self.kind.label())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ForegroundProcessKind {
    Agent { agent_id: AgentId },
    Shell,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ForegroundProcess {
    pub root_process_id: u32,
    pub process_id: u32,
    pub process_name: String,
    pub kind: ForegroundProcessKind,
}

impl ForegroundProcess {
    pub fn is_valid_for(&self, session_agent_id: &AgentId) -> bool {
        self.root_process_id > 0
            && self.process_id > 0
            && !self.process_name.trim().is_empty()
            && self.process_name.len() <= FOREGROUND_PROCESS_NAME_MAX_BYTES
            && !self.process_name.chars().any(char::is_control)
            && match &self.kind {
                ForegroundProcessKind::Agent { agent_id } => agent_id == session_agent_id,
                ForegroundProcessKind::Shell | ForegroundProcessKind::Other => true,
            }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ForegroundAuthority {
    #[default]
    Unknown,
    Confirmed,
    Stale,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ForegroundSnapshot {
    pub authority: ForegroundAuthority,
    pub process: Option<ForegroundProcess>,
    pub stale_reason: Option<String>,
}

/// Whether the screen currently painted at a PTY looks like the agent's own
/// composer, as classified by the node from the same terminal text a human
/// would read. This is a screen-content judgement, never a process-liveness
/// one -- `SessionStatus` already answers "is something running", and a
/// consumer must not fold the two into a single "is it running" question:
/// a process can be `Running` while its screen sits on an unrelated
/// installer prompt, and that combination is exactly the case this type
/// exists to distinguish.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum PtyScreenState {
    /// No observation has been made for this generation yet, or the most
    /// recent observation attempt failed. A caller deciding whether to write
    /// to the PTY blindly must treat this exactly like `NotAgent` -- there is
    /// no "probably fine" reading of "unclassified". Never optimistic.
    #[default]
    Unknown,
    /// The PTY's foreground process is neither the agent's own binary nor a
    /// tolerated wrapper that spawns it. `observed_process` records what was
    /// actually seen there, so an operator reading this gets "an update is
    /// installing" rather than a bare timeout with no explanation.
    NotAgent { observed_process: String },
    /// The foreground process matches, but the screen itself is showing a
    /// recognized blocking pattern -- workspace trust, authentication,
    /// a vendor update, first-run onboarding. This is the state that wants a
    /// human specifically, because resolving it means typing into the pane
    /// rather than dispatching another agent turn. `gate` is a structured
    /// `OperatorGateState`, not a bare label -- what TYPE of gate
    /// (`kind`), what it is gating (`subject`), how it is answered
    /// (`input`), and which choices were read off the screen (`options`),
    /// so a consumer can close the gate or ask an operator a real question
    /// instead of only ever surfacing a tag.
    OperatorGate { gate: OperatorGateState },
    /// The foreground process matches, but the screen shows the agent came
    /// up wrong or fell over -- a crash/stack trace, an expired or rejected
    /// login, a fatal startup error. Kept distinct from `OperatorGate` on
    /// purpose: a gate is a screen a human resolves BY typing into it, while
    /// nothing typed into this screen fixes it. Collapsing the two would
    /// lose exactly the diagnosis an operator needs -- "waiting for you"
    /// versus "broken". For write-gating it behaves like every other
    /// non-`Ready` state (refused); the split buys a correct label, not
    /// different gating. `reason` is a short classifier label, the same
    /// shape `OperatorGate::gate` used to carry before it became a
    /// structured type, never raw terminal text -- nothing in this enum
    /// carries screen contents. Where a provider has a
    /// `pty_sidecar` adapter bound, structured signals (rate limits arriving
    /// as a `ProviderEvent`) remain the authority for those specific
    /// conditions; this variant is the text-derived fallback, and the only
    /// signal at all for providers that emit no structured events.
    Failing { reason: String },
    /// The foreground process matches AND no known gate or failure pattern
    /// is showing. This is explicitly NOT a claim that the agent is idle,
    /// waiting for input, or will do anything useful with a write -- it only
    /// means the screen is not known to be showing something else. Reading
    /// `Ready` as "safe to act on" beyond that is the over-read this type
    /// exists to prevent. Note the asymmetry with the other four variants:
    /// each of them fires from a single signal (process mismatch, or a
    /// recognized gate/failure pattern alone), while `Ready` requires both
    /// the process and text signals to agree -- a screen the text matcher
    /// does not recognize never reaches `Ready` on that basis alone.
    Ready,
}

impl PtyScreenState {
    /// True unless this screen was READ as an obstacle. Refuses on the three
    /// states that carry a finding -- a gate the operator must answer, a
    /// foreign process, a failure -- and admits `Ready` and `Unknown` alike.
    ///
    /// `Unknown` admits deliberately. It does not mean "an obstacle we might
    /// have missed", it means the matcher recognized nothing, and refusing on
    /// it makes ignorance indistinguishable from a finding. Every provider
    /// reaches `Ready` within a frame or two of spawn on process identity
    /// alone, long before anything is on screen, so `Unknown` is mostly just
    /// the moment before that -- and the screen is no longer where this
    /// system decides what a session is doing. ACP carries that as protocol
    /// state and never consults this predicate at all; a PTY is an operator's
    /// surface first and a control channel second.
    ///
    /// What stays refused is what was actually read: writing a task into a
    /// trust prompt or a login screen puts the text nowhere and leaves Enter
    /// to pick a menu item blind. That is a finding, and findings still
    /// count. Answering such a screen is not blocked and never was -- key
    /// injection does not come through here.
    pub fn admits_blind_write(&self) -> bool {
        !matches!(
            self,
            Self::OperatorGate { .. } | Self::NotAgent { .. } | Self::Failing { .. }
        )
    }

    /// Bounds check matching `ForegroundProcess::is_valid_for`: the carried
    /// strings must be non-empty, control-character free, and within the
    /// per-field byte caps, since both travel over the wire into an operator
    /// UI and a raw process/gate label is not something to trust unbounded.
    pub fn is_valid(&self) -> bool {
        match self {
            Self::Unknown | Self::Ready => true,
            Self::NotAgent { observed_process } => {
                !observed_process.trim().is_empty()
                    && observed_process.len() <= FOREGROUND_PROCESS_NAME_MAX_BYTES
                    && !observed_process.chars().any(char::is_control)
            }
            Self::OperatorGate { gate } => gate.is_valid(),
            Self::Failing { reason } => {
                !reason.trim().is_empty()
                    && reason.len() <= PTY_SCREEN_GATE_NAME_MAX_BYTES
                    && !reason.chars().any(char::is_control)
            }
        }
    }
}

impl TerminalSize {
    pub fn is_valid(self) -> bool {
        (1..=TERMINAL_ROWS_MAX).contains(&self.rows)
            && (1..=TERMINAL_COLUMNS_MAX).contains(&self.columns)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TransportKind {
    Pty,
    Pipe,
    Acp,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CommandEnvelope {
    pub protocol_version: u16,
    pub id: CommandId,
    pub command: ControlCommand,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ControlCommand {
    Register {
        instance_id: AgentInstanceId,
        agent_id: AgentId,
        transport: TransportKind,
    },
    Start {
        instance_id: AgentInstanceId,
        runtime_policy: ProviderRuntimePolicy,
        request: StartRequest,
    },
    Stop {
        instance_id: AgentInstanceId,
        force: bool,
    },
    SendInput {
        instance_id: AgentInstanceId,
        action: InputAction,
    },
    Resize {
        instance_id: AgentInstanceId,
        size: TerminalSize,
    },
    RefreshForeground {
        instance_id: AgentInstanceId,
    },
    ProbeCapabilities {
        instance_id: AgentInstanceId,
        request: CapabilityProbeRequest,
    },
    DiscoverHistory {
        instance_id: AgentInstanceId,
        query: HistoryQuery,
    },
    LoadHistory {
        instance_id: AgentInstanceId,
        candidate_id: String,
    },
    Resume {
        instance_id: AgentInstanceId,
        target: ResumeTarget,
        runtime_policy: ProviderRuntimePolicy,
        request: ResumeLaunchRequest,
    },
    ResolveInteraction {
        instance_id: AgentInstanceId,
        generation: SessionGeneration,
        interaction_id: ProviderInteractionId,
        response: ProviderInteractionResponse,
    },
    /// Switch the session's ACP `session/set_mode` mode. ACP-transport only;
    /// `gate4agent-engine`'s `set_session_mode` refuses any other transport.
    SetSessionMode {
        instance_id: AgentInstanceId,
        mode_id: String,
    },
    /// Set one ACP `session/set_config_option` value (model, reasoning
    /// effort, ...) -- the mechanism that supersedes session modes.
    /// `value_json` is pre-serialized JSON text, same convention as
    /// [`ProviderConfigOption::value_json`]: this crate does not depend on
    /// `serde_json`, so parsing it into a value is the shell executor's job.
    SetSessionConfigOption {
        instance_id: AgentInstanceId,
        option_id: String,
        value_json: String,
    },
    /// Switch the session's active model via a provider vendor extension
    /// (there is no `session/set_model` in the ACP spec proper). Wired end
    /// to end on the wire and through this command regardless of whether
    /// the current build shell can honor it for a given provider -- see
    /// `gate4agent-shell-native`'s `SetSessionModel` effect arm for what it
    /// actually does today.
    SetSessionModel {
        instance_id: AgentInstanceId,
        model_id: String,
    },
    IngestProvider {
        instance_id: AgentInstanceId,
        generation: SessionGeneration,
        source: ProviderSource,
        source_sequence: u64,
        events: Vec<ProviderEvent>,
    },
    Remove {
        instance_id: AgentInstanceId,
    },
}

impl ControlCommand {
    pub fn instance_id(&self) -> AgentInstanceId {
        match self {
            Self::Register { instance_id, .. }
            | Self::Start { instance_id, .. }
            | Self::Stop { instance_id, .. }
            | Self::SendInput { instance_id, .. }
            | Self::Resize { instance_id, .. }
            | Self::RefreshForeground { instance_id }
            | Self::ProbeCapabilities { instance_id, .. }
            | Self::DiscoverHistory { instance_id, .. }
            | Self::LoadHistory { instance_id, .. }
            | Self::Resume { instance_id, .. }
            | Self::ResolveInteraction { instance_id, .. }
            | Self::SetSessionMode { instance_id, .. }
            | Self::SetSessionConfigOption { instance_id, .. }
            | Self::SetSessionModel { instance_id, .. }
            | Self::IngestProvider { instance_id, .. }
            | Self::Remove { instance_id } => *instance_id,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EffectEnvelope {
    pub protocol_version: u16,
    pub operation_id: OperationId,
    pub instance_id: AgentInstanceId,
    pub generation: SessionGeneration,
    pub effect: ControlEffect,
}

/// Route proof an effect executor must obtain immediately before a PTY write.
///
/// This is carried by the effect rather than inferred by a product shell so
/// local, hosted, and future browser-facing executors enforce the same rule.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ForegroundRequirement {
    /// Explicit terminal text and controls are direct user terminal input.
    Any,
    /// Semantic input must target the session's configured agent.
    Agent { agent_id: AgentId },
    /// Intentional shell syntax may be written only while a shell owns the PTY.
    Shell,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ControlEffect {
    Spawn {
        agent_id: AgentId,
        transport: TransportKind,
        runtime_policy: ProviderRuntimePolicy,
        request: StartRequest,
    },
    Stop {
        force: bool,
    },
    WriteInput {
        input: PreparedInput,
        required_foreground: ForegroundRequirement,
    },
    SubmitPrompt {
        prompt: String,
    },
    Interrupt,
    Resize {
        size: TerminalSize,
    },
    ObserveForeground,
    ProbeCapabilities {
        agent_id: AgentId,
        request: CapabilityProbeRequest,
    },
    DiscoverHistory {
        agent_id: AgentId,
        query: HistoryQuery,
    },
    LoadHistory {
        agent_id: AgentId,
        candidate_id: String,
    },
    AuthorizeResume {
        agent_id: AgentId,
        target: ResumeAuthorityTarget,
        request: ResumeLaunchRequest,
    },
    SpawnResume {
        agent_id: AgentId,
        transport: TransportKind,
        provider_session: ProviderSessionIdentity,
        runtime_policy: ProviderRuntimePolicy,
        request: ResumeLaunchRequest,
    },
    ResolveInteraction {
        target: ProviderInteractionTarget,
        response: ProviderInteractionResponse,
    },
    SetSessionMode {
        mode_id: String,
    },
    SetSessionConfigOption {
        option_id: String,
        value_json: String,
    },
    SetSessionModel {
        model_id: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ObservationEnvelope {
    pub protocol_version: u16,
    pub operation_id: Option<OperationId>,
    pub instance_id: AgentInstanceId,
    pub generation: SessionGeneration,
    pub observation: ControlObservation,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ControlObservation {
    Spawned {
        process_id: Option<u32>,
    },
    SpawnFailed {
        message: String,
    },
    ProcessExited {
        exit_code: Option<i32>,
        final_terminal: Option<TerminalFrame>,
    },
    StopCompleted {
        forced: bool,
        exit_code: Option<i32>,
        final_terminal: Option<TerminalFrame>,
    },
    StopFailed {
        message: String,
    },
    InputCompleted,
    InputFailed {
        message: String,
    },
    ResizeCompleted {
        size: TerminalSize,
    },
    ResizeFailed {
        message: String,
    },
    ForegroundObserved {
        process: ForegroundProcess,
    },
    ForegroundFailed {
        message: String,
    },
    CapabilitiesProbed {
        session_option_models: Vec<CapabilityModelSummary>,
    },
    CapabilityProbeFailed {
        failure: CapabilityProbeFailure,
    },
    HistoryDiscovered {
        candidates: Vec<HistoryCandidateSummary>,
    },
    HistoryLoaded {
        session: HistorySessionRecord,
    },
    HistoryFailed {
        message: String,
    },
    ResumeAuthorized {
        provider_session: ProviderSessionIdentity,
    },
    ResumeDenied {
        reason: String,
    },
    ResumeFailed {
        message: String,
    },
    InteractionResolutionCompleted {
        interaction_id: ProviderInteractionId,
    },
    InteractionResolutionFailed {
        interaction_id: ProviderInteractionId,
        message: String,
    },
    /// `ControlEffect::SetSessionMode` succeeded; `mode_id` echoes back the
    /// mode the shell executor actually confirmed with the agent (Resize's
    /// `ResizeCompleted { size }` is the same convention).
    SessionModeSet {
        mode_id: String,
    },
    SessionModeSetFailed {
        message: String,
    },
    SessionConfigOptionSet {
        option_id: String,
    },
    SessionConfigOptionSetFailed {
        message: String,
    },
    SessionModelSet {
        model_id: String,
    },
    SessionModelSetFailed {
        message: String,
    },
    TerminalFrame {
        frame: TerminalFrame,
    },
    TerminalStale {
        message: String,
    },
    /// The node's merged `PtyScreenState` classification changed. Emitted
    /// only on an actual change, never per frame and never per foreground
    /// probe -- the node compares against its own last-published value
    /// before sending this, so a subscriber never has to de-duplicate.
    /// This is node-internal (shell -> engine), the same lane as
    /// `TerminalFrame`/`TerminalStale`, not a wire type: neither the node
    /// nor the c2 protocol carries `ControlObservation` across a process
    /// boundary, so this variant needs no version gate.
    ScreenState {
        state: PtyScreenState,
    },
    ProviderEvent {
        source: ProviderSource,
        sequence: u64,
        event: ProviderEvent,
    },
    ProviderGap {
        source: ProviderSource,
        source_sequence: u64,
        missed: u64,
    },
}

impl ControlObservation {
    pub fn requires_operation_id(&self) -> bool {
        !matches!(
            self,
            Self::ProcessExited { .. }
                | Self::TerminalFrame { .. }
                | Self::TerminalStale { .. }
                | Self::ScreenState { .. }
                | Self::ProviderEvent { .. }
                | Self::ProviderGap { .. }
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum SessionStatus {
    Registered,
    Starting,
    Running,
    Stopping,
    Exited { exit_code: Option<i32> },
    Failed { message: String },
}

impl SessionStatus {
    /// Whether a `Remove` command for a session in this status will be
    /// accepted rather than rejected as an invalid transition.
    ///
    /// It lives here, on the status itself, because two crates need the
    /// same answer and neither may depend on the other: `gate4agent-engine`
    /// enforces it in `Gate4AgentEngine::remove`, and `gate4agent-node`
    /// consults it in `wait_until_removed` BEFORE re-dispatching, because a
    /// rejected command is not free -- it publishes a `ControlEventKind::
    /// CommandRejected` to every subscriber and writes a WARN line. Ungated,
    /// one ordinary teardown produced 241 rejections in 1.6 seconds: the
    /// entire `Stopping` window at one known-doomed dispatch per 2ms tick.
    ///
    /// Exhaustive on purpose -- a status added to this enum without a
    /// decision here fails to compile rather than silently joining one set.
    pub fn allows_remove(&self) -> bool {
        match self {
            Self::Starting | Self::Running | Self::Stopping => false,
            Self::Registered | Self::Exited { .. } | Self::Failed { .. } => true,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub instance_id: AgentInstanceId,
    pub agent_id: AgentId,
    pub transport: TransportKind,
    pub generation: SessionGeneration,
    pub status: SessionStatus,
    pub pending_operation: Option<OperationId>,
    pub pending_input: Option<PreparedInputKind>,
    pub process_id: Option<u32>,
    pub terminal_size: Option<TerminalSize>,
    pub terminal_frame: Option<TerminalFrame>,
    pub terminal_stale: Option<String>,
    pub session_options: Option<SessionOptionSelection>,
    pub capabilities: CapabilitySnapshot,
    pub history: HistorySnapshot,
    pub resume: ResumeSnapshot,
    pub foreground: ForegroundSnapshot,
    pub provider: ProviderSnapshot,
    /// The session's CURRENT screen classification, as opposed to
    /// `terminal_frame`'s per-frame stamp -- both are read off one value
    /// computed at the node, but this one is what a consumer reads when it
    /// wants the state now without subscribing to terminal frames, which is
    /// what makes gating a dispatch possible for a caller holding only the
    /// session inventory.
    #[serde(default)]
    pub screen_state: PtyScreenState,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
    pub context_window: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContextWindowUsage {
    pub uncached_input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    pub unattributed_tokens: u64,
    pub used_tokens: u64,
    pub capacity_tokens: u64,
}

impl ContextWindowUsage {
    pub fn validate(&self) -> Result<(), ProviderEventValidationError> {
        if self.capacity_tokens == 0 {
            return Err(ProviderEventValidationError::ZeroContextWindowCapacity);
        }
        let segment_sum = self
            .uncached_input_tokens
            .checked_add(self.cache_read_tokens)
            .and_then(|sum| sum.checked_add(self.cache_write_tokens))
            .and_then(|sum| sum.checked_add(self.output_tokens))
            .and_then(|sum| sum.checked_add(self.unattributed_tokens))
            .ok_or(ProviderEventValidationError::ContextWindowSegmentsOverflow)?;
        if segment_sum != self.used_tokens {
            return Err(ProviderEventValidationError::ContextWindowSegmentsMismatch {
                segment_sum,
                used_tokens: self.used_tokens,
            });
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderInteractionKind {
    Approval,
    Question,
}

/// Which class of budget a `ProviderEvent::RateLimited` observation
/// concerns. This is the wire-typed counterpart of `gate4agent`'s own
/// (source-of-truth) `RateLimitType` -- kept as its own type here, rather
/// than imported, because this crate's contract forbids depending on
/// `gate4agent` (see this crate's `CLAUDE.md`); the conversion from the
/// detector's enum lives in the shell that already depends on both.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderRateLimitKind {
    /// Session/hourly limit (codex: the rolling 5h window).
    Session,
    /// Daily limit.
    Daily,
    /// Weekly limit.
    Weekly,
    /// Limit type could not be determined from the matched text.
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderInteractionOutcome {
    Approved,
    Answered,
    Denied,
    Interrupted,
    TurnEnded,
    Superseded,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderInteractionResponseKind {
    ApproveOnce,
    Deny,
    Answer,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ProviderInteractionResponse {
    ApproveOnce,
    Deny,
    Answer { text: String },
}

impl ProviderInteractionResponse {
    pub fn kind(&self) -> ProviderInteractionResponseKind {
        match self {
            Self::ApproveOnce => ProviderInteractionResponseKind::ApproveOnce,
            Self::Deny => ProviderInteractionResponseKind::Deny,
            Self::Answer { .. } => ProviderInteractionResponseKind::Answer,
        }
    }

    pub fn outcome(&self) -> ProviderInteractionOutcome {
        match self {
            Self::ApproveOnce => ProviderInteractionOutcome::Approved,
            Self::Deny => ProviderInteractionOutcome::Denied,
            Self::Answer { .. } => ProviderInteractionOutcome::Answered,
        }
    }

    pub fn validate_for(
        &self,
        interaction_kind: ProviderInteractionKind,
    ) -> Result<(), ProviderInteractionResponseError> {
        match (interaction_kind, self) {
            (ProviderInteractionKind::Approval, Self::ApproveOnce)
            | (ProviderInteractionKind::Approval, Self::Deny)
            | (ProviderInteractionKind::Question, Self::Deny) => Ok(()),
            (ProviderInteractionKind::Question, Self::Answer { text }) => {
                if text.trim().is_empty() {
                    return Err(ProviderInteractionResponseError::EmptyAnswer);
                }
                let has_unsafe_control = text.chars().any(|character| {
                    character.is_control() && !matches!(character, '\n' | '\r' | '\t')
                });
                if text.len() > PROVIDER_INTERACTION_RESPONSE_MAX_BYTES || has_unsafe_control {
                    return Err(ProviderInteractionResponseError::InvalidAnswer {
                        max: PROVIDER_INTERACTION_RESPONSE_MAX_BYTES,
                    });
                }
                Ok(())
            }
            (ProviderInteractionKind::Approval, Self::Answer { .. }) => {
                Err(ProviderInteractionResponseError::AnswerRequiresQuestion)
            }
            (ProviderInteractionKind::Question, Self::ApproveOnce) => {
                Err(ProviderInteractionResponseError::ApprovalRequiresApproval)
            }
        }
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ProviderInteractionResponseError {
    #[error("interaction answer is required")]
    EmptyAnswer,
    #[error("interaction answer contains controls or exceeds {max} bytes")]
    InvalidAnswer { max: usize },
    #[error("an answer response requires a question interaction")]
    AnswerRequiresQuestion,
    #[error("an approve-once response requires an approval interaction")]
    ApprovalRequiresApproval,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderSessionKey {
    SessionId,
    ConversationId,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderSessionIdentity {
    pub key: ProviderSessionKey,
    pub id: String,
    pub transcript_path: Option<String>,
}

impl ProviderSessionIdentity {
    pub fn validate(&self) -> Result<(), ProviderEventValidationError> {
        validate_required("provider session id", &self.id, PROVIDER_EVENT_ID_MAX_BYTES)?;
        if self.id.starts_with('-') {
            return Err(ProviderEventValidationError::InvalidField {
                field: "provider session id",
                max: PROVIDER_EVENT_ID_MAX_BYTES,
            });
        }
        if let Some(path) = &self.transcript_path {
            validate_required(
                "provider transcript path",
                path,
                PROVIDER_SESSION_LOCATOR_MAX_BYTES,
            )?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderSubagent {
    pub source: ProviderSource,
    pub provider_agent_id: String,
    pub agent_type: Option<String>,
    pub description: Option<String>,
}

/// Priority of a single [`ProviderPlanStep`], carried on
/// `ProviderEvent::Plan`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderPlanPriority {
    High,
    Medium,
    Low,
}

/// Status of a single [`ProviderPlanStep`], carried on
/// `ProviderEvent::Plan`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderPlanStatus {
    Pending,
    InProgress,
    Completed,
}

/// One step of the agent's execution plan (ACP transport's `plan`
/// update). `ProviderEvent::Plan` always carries the FULL plan snapshot,
/// never a delta.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderPlanStep {
    pub content: String,
    pub priority: ProviderPlanPriority,
    pub status: ProviderPlanStatus,
}

/// A single slash-style command the agent advertises (ACP transport's
/// `available_commands_update`).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderAvailableCommand {
    pub name: String,
    pub description: String,
    pub input_hint: Option<String>,
}

/// One mode the agent advertised as selectable, read from ACP's
/// `session/new` handshake result (`AcpSession::available_modes()`) and
/// carried on [`ProviderEvent::ModeChanged`] alongside the id that changed.
/// Mirrors `gate4agent-node-protocol`'s `AgentStreamNamedIdV1`
/// field-for-field; this crate does not depend on that one (see this
/// crate's own `CLAUDE.md`), so the shape is repeated rather than shared.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderModeInfo {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}

/// The kind of a [`ProviderConfigOption`] -- `select` (choose one of
/// `choices`) or `boolean` (toggle the option's current value). `Unknown`
/// is the fallback for a kind string this build does not recognize.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderConfigOptionKind {
    Select,
    Boolean,
    Unknown,
}

/// One selectable value of a `select`-kind [`ProviderConfigOption`].
/// `value_json` is the choice's value pre-serialized to JSON text (this
/// crate is a pure data contract and does not depend on `serde_json`; see
/// `ProviderConfigOption::value_json` for the same convention applied to
/// the option's own current value).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderConfigChoice {
    pub value_json: String,
    pub label: Option<String>,
}

/// One session configuration setting -- the mechanism ACP uses to change
/// model, reasoning effort, and similar settings, superseding session
/// modes. `ProviderEvent::ConfigOptionsUpdated` always carries the FULL
/// current set, never a delta.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderConfigOption {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub category: Option<String>,
    pub kind: ProviderConfigOptionKind,
    pub value_json: String,
    pub choices: Vec<ProviderConfigChoice>,
}

/// WHO decided a host request the agent sent to the ACP host -- see
/// [`ProviderEvent::HostRequestObserved`]. Mirrors `gate4agent`'s own
/// `HostDecisionAuthority` one-for-one; this crate cannot depend on
/// `gate4agent` (see this crate's `CLAUDE.md`), so the value is converted at
/// the boundary that already depends on both (`gate4agent-shell-native`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostDecisionAuthority {
    /// The dangerous-command gate forced this outcome ahead of `HostPolicy`
    /// -- `terminal/create` and `execute`-kind `session/request_permission`
    /// only. Always a denial.
    Gate,
    /// `HostPolicy` (`Yolo`/`Auto`/`ReadOnly`/`Deny`) decided the request
    /// the instant it arrived -- the default path for every request that
    /// is neither gate-blocked nor deferred.
    Policy,
    /// An operator answered a `session/request_permission` call that had
    /// been left `HostRequestDecision::Deferred`.
    Operator,
    /// A `session/request_permission` call left `HostRequestDecision::
    /// Deferred` reached its deadline with no operator answer, so
    /// `HostPolicy` -- the SAME policy that would have answered it
    /// immediately had deferral never been enabled -- decided it instead.
    /// Deliberately its own variant rather than `Policy`: folding it in
    /// would erase the fact that an operator was asked first and nobody
    /// answered in time. Equally deliberately not `Operator`: no human
    /// made this choice.
    DeadlinePolicy,
}

/// A typed answer to "what happened to this host request" -- see
/// [`ProviderEvent::HostRequestObserved`]. Mirrors `gate4agent`'s own
/// `HostRequestDecision` one-for-one; see [`HostDecisionAuthority`]'s doc
/// comment for why this crate keeps its own copy rather than importing it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum HostRequestDecision {
    /// The request was allowed. `by` is who made that call.
    Granted { by: HostDecisionAuthority },
    /// The request was refused. `by` is who made that call.
    Denied { by: HostDecisionAuthority },
    /// The request has arrived and been recorded, but nothing has decided
    /// it yet. A later `ProviderEvent::HostRequestObserved` reports the
    /// eventual `Granted`/`Denied` outcome once one exists.
    Deferred,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ProviderEvent {
    SessionStarted {
        session_id: String,
        model: String,
        tools: Vec<String>,
    },
    SessionIdentityObserved {
        identity: ProviderSessionIdentity,
    },
    TurnStarted {
        prompt: Option<String>,
    },
    WorkingObserved,
    Text {
        text: String,
        is_delta: bool,
    },
    Thinking {
        text: String,
    },
    ToolStarted {
        id: String,
        name: String,
        input_json: String,
        agent_id: Option<String>,
    },
    ToolCompleted {
        id: String,
        output: String,
        is_error: bool,
        duration_ms: Option<u64>,
        agent_id: Option<String>,
    },
    TurnCompleted {
        usage: TokenUsage,
        is_cumulative: bool,
    },
    ContextWindowUsage {
        usage: ContextWindowUsage,
    },
    TurnInterrupted,
    SessionEnded {
        result: String,
        cost_usd: Option<String>,
        is_error: bool,
    },
    Error {
        message: String,
    },
    Ready,
    InteractionRequested {
        request_id: Option<String>,
        interaction_kind: ProviderInteractionKind,
        tool_name: String,
        prompt: String,
        agent_id: Option<String>,
    },
    InteractionResolved {
        request_id: String,
        outcome: ProviderInteractionOutcome,
    },
    SubagentStarted {
        agent_id: String,
        agent_type: Option<String>,
        description: Option<String>,
    },
    SubagentStopped {
        agent_id: String,
    },
    RateLimited {
        limit_type: ProviderRateLimitKind,
        resets_at: Option<String>,
        usage_percent: Option<String>,
        raw_message: String,
    },
    /// The agent sent a JSON-RPC request to the ACP host -- `session/
    /// request_permission`, `fs/read_text_file`, `fs/write_text_file`,
    /// `terminal/create`, `terminal/output`, `terminal/wait_for_exit`,
    /// `terminal/kill`, `terminal/release`. `decision` is read off the
    /// host's own answer, not re-derived here -- see the source
    /// (`gate4agent`'s `AgentEvent::RpcIncomingRequest::decision`) for
    /// exactly how. A `session/request_permission` call the host chose to
    /// defer to an operator arrives here TWICE under different `method`/
    /// `params_json` snapshots but the SAME logical request: once as
    /// `HostRequestDecision::Deferred` when it is recorded, then again as
    /// `Granted`/`Denied` once `HostPolicy` (`Yolo`/`Auto`/`ReadOnly`/`Deny`)
    /// or an operator decides it -- see `HostRequestDecision` and
    /// `HostDecisionAuthority` for what each of the four ways a request can
    /// end up decided actually means. This event does not change what the
    /// host does; it exists purely so an operator sees the request AND the
    /// decision instead of the request silently disappearing into a
    /// refusal nobody downstream ever hears about.
    HostRequestObserved {
        method: String,
        params_json: String,
        decision: HostRequestDecision,
    },
    /// A JSON-RPC notification the reader received but could not classify
    /// into any other `ProviderEvent` -- most commonly a `session/update`
    /// whose `update` shape none of the known kinds matched, but also any
    /// other notification method this build has no mapping for. This is a
    /// raw protocol echo, NOT a normal operational event: nothing here has
    /// been validated against a known shape, so a consumer must treat
    /// `payload_json` as opaque vendor JSON, not a fact to act on.
    UnrecognizedNotification {
        method: String,
        payload_json: String,
    },
    /// Echo of a user message, replayed when resuming a loaded session
    /// (ACP transport's `user_message_chunk`).
    UserMessage {
        text: String,
        is_delta: bool,
    },
    /// The agent's full execution plan, replacing any plan reported
    /// before it (ACP transport's `plan`).
    Plan {
        steps: Vec<ProviderPlanStep>,
    },
    /// The agent's slash-command catalog changed (ACP transport's
    /// `available_commands_update`).
    AvailableCommandsUpdated {
        commands: Vec<ProviderAvailableCommand>,
    },
    /// The session's active mode changed (ACP transport's
    /// `current_mode_update`). `available` is the mode catalogue the agent
    /// returned at `session/new` (`AcpSession::available_modes()`), read
    /// back at the moment this event is minted -- `current_mode_update`
    /// itself carries only the new id, never the catalogue. ACP orders
    /// `session/new` strictly before any `session/update`, and the
    /// catalogue never changes after handshake, so by the time a
    /// `ModeChanged` can exist the same session object's catalogue is
    /// already the real one: an empty `available` here always means the
    /// agent announced zero modes, never "not read yet".
    ModeChanged {
        mode_id: String,
        available: Vec<ProviderModeInfo>,
    },
    /// Session metadata changed; only the fields that actually changed
    /// are populated (ACP transport's `session_info_update`).
    SessionInfoUpdated {
        title: Option<String>,
    },
    /// Context-window consumption and, when reported, turn cost (ACP
    /// transport's `usage_update`). `cost_amount` is a decimal string, not
    /// `f64`, for the same reason `SessionEnded::cost_usd` is -- so this
    /// type can keep deriving `Eq`.
    UsageUpdated {
        used_tokens: Option<u64>,
        context_window: Option<u64>,
        cost_amount: Option<String>,
        cost_currency: Option<String>,
    },
    /// The full current set of session configuration options (ACP
    /// transport's `config_option_update`).
    ConfigOptionsUpdated {
        options: Vec<ProviderConfigOption>,
    },
}

impl ProviderEvent {
    pub fn validate_ingress(&self) -> Result<(), ProviderEventValidationError> {
        match self {
            Self::SessionStarted {
                session_id,
                model,
                tools,
            } => {
                validate_required("session_id", session_id, PROVIDER_EVENT_ID_MAX_BYTES)?;
                validate_identifier("model", model, PROVIDER_EVENT_ID_MAX_BYTES)?;
                if tools.len() > PROVIDER_EVENT_TOOLS_MAX {
                    return Err(ProviderEventValidationError::TooManyTools {
                        count: tools.len(),
                        max: PROVIDER_EVENT_TOOLS_MAX,
                    });
                }
                for tool in tools {
                    validate_required("tool", tool, PROVIDER_EVENT_ID_MAX_BYTES)?;
                }
            }
            Self::SessionIdentityObserved { identity } => {
                identity.validate()?;
            }
            Self::TurnStarted { prompt } => {
                if let Some(prompt) = prompt {
                    validate_text("prompt", prompt, PROVIDER_EVENT_TEXT_MAX_BYTES)?;
                }
            }
            Self::Text { text, .. } | Self::Thinking { text } => {
                validate_text("text", text, PROVIDER_EVENT_TEXT_MAX_BYTES)?;
            }
            Self::ToolStarted {
                id,
                name,
                input_json,
                agent_id,
            } => {
                validate_required("tool id", id, PROVIDER_EVENT_ID_MAX_BYTES)?;
                validate_required("tool name", name, PROVIDER_EVENT_ID_MAX_BYTES)?;
                validate_text("tool input", input_json, PROVIDER_EVENT_TEXT_MAX_BYTES)?;
                validate_optional_agent_id(agent_id)?;
            }
            Self::ToolCompleted {
                id,
                output,
                agent_id,
                ..
            } => {
                validate_required("tool id", id, PROVIDER_EVENT_ID_MAX_BYTES)?;
                validate_text("tool output", output, PROVIDER_EVENT_TEXT_MAX_BYTES)?;
                validate_optional_agent_id(agent_id)?;
            }
            Self::SessionEnded {
                result, cost_usd, ..
            } => {
                validate_text("session result", result, PROVIDER_EVENT_TEXT_MAX_BYTES)?;
                if let Some(cost) = cost_usd {
                    validate_identifier("cost", cost, PROVIDER_EVENT_ID_MAX_BYTES)?;
                }
            }
            Self::Error { message } => {
                validate_required_text("error", message, PROVIDER_EVENT_TEXT_MAX_BYTES)?;
            }
            Self::InteractionRequested {
                request_id,
                interaction_kind,
                tool_name,
                prompt,
                agent_id,
            } => {
                if let Some(request_id) = request_id {
                    validate_required(
                        "interaction request id",
                        request_id,
                        PROVIDER_EVENT_ID_MAX_BYTES,
                    )?;
                }
                validate_required("interaction tool", tool_name, PROVIDER_EVENT_ID_MAX_BYTES)?;
                if *interaction_kind == ProviderInteractionKind::Question {
                    validate_required_text(
                        "interaction prompt",
                        prompt,
                        PROVIDER_EVENT_TEXT_MAX_BYTES,
                    )?;
                } else {
                    validate_text("interaction prompt", prompt, PROVIDER_EVENT_TEXT_MAX_BYTES)?;
                }
                validate_optional_agent_id(agent_id)?;
            }
            Self::InteractionResolved {
                request_id,
                outcome,
            } => {
                validate_required(
                    "interaction request id",
                    request_id,
                    PROVIDER_EVENT_ID_MAX_BYTES,
                )?;
                if !matches!(
                    outcome,
                    ProviderInteractionOutcome::Approved | ProviderInteractionOutcome::Denied
                ) {
                    return Err(
                        ProviderEventValidationError::InvalidInteractionResolutionOutcome {
                            outcome: *outcome,
                        },
                    );
                }
            }
            Self::SubagentStarted {
                agent_id,
                agent_type,
                description,
            } => {
                validate_required("subagent id", agent_id, PROVIDER_EVENT_ID_MAX_BYTES)?;
                if let Some(agent_type) = agent_type {
                    validate_identifier("subagent type", agent_type, PROVIDER_EVENT_ID_MAX_BYTES)?;
                }
                if let Some(description) = description {
                    validate_text(
                        "subagent description",
                        description,
                        PROVIDER_EVENT_TEXT_MAX_BYTES,
                    )?;
                }
            }
            Self::SubagentStopped { agent_id } => {
                validate_required("subagent id", agent_id, PROVIDER_EVENT_ID_MAX_BYTES)?;
            }
            Self::RateLimited {
                limit_type: _,
                resets_at,
                usage_percent,
                raw_message,
            } => {
                // `limit_type` is a typed enum now (`ProviderRateLimitKind`),
                // not a String -- it has no shape to validate here.
                for (field, value) in [
                    ("reset time", resets_at.as_deref()),
                    ("usage percent", usage_percent.as_deref()),
                ] {
                    if let Some(value) = value {
                        validate_identifier(field, value, PROVIDER_EVENT_ID_MAX_BYTES)?;
                    }
                }
                validate_text(
                    "rate limit message",
                    raw_message,
                    PROVIDER_EVENT_TEXT_MAX_BYTES,
                )?;
            }
            Self::HostRequestObserved {
                method,
                params_json,
                ..
            } => {
                validate_required("host request method", method, PROVIDER_EVENT_ID_MAX_BYTES)?;
                validate_text(
                    "host request params",
                    params_json,
                    PROVIDER_EVENT_TEXT_MAX_BYTES,
                )?;
            }
            Self::UnrecognizedNotification {
                method,
                payload_json,
            } => {
                validate_required(
                    "unrecognized notification method",
                    method,
                    PROVIDER_EVENT_ID_MAX_BYTES,
                )?;
                validate_text(
                    "unrecognized notification payload",
                    payload_json,
                    PROVIDER_EVENT_TEXT_MAX_BYTES,
                )?;
            }
            Self::UserMessage { text, .. } => {
                validate_text("text", text, PROVIDER_EVENT_TEXT_MAX_BYTES)?;
            }
            Self::Plan { steps } => {
                if steps.len() > PROVIDER_PLAN_STEPS_MAX {
                    return Err(ProviderEventValidationError::TooManyPlanSteps {
                        count: steps.len(),
                        max: PROVIDER_PLAN_STEPS_MAX,
                    });
                }
                for step in steps {
                    validate_required_text(
                        "plan step content",
                        &step.content,
                        PROVIDER_EVENT_TEXT_MAX_BYTES,
                    )?;
                }
            }
            Self::AvailableCommandsUpdated { commands } => {
                if commands.len() > PROVIDER_AVAILABLE_COMMANDS_MAX {
                    return Err(ProviderEventValidationError::TooManyAvailableCommands {
                        count: commands.len(),
                        max: PROVIDER_AVAILABLE_COMMANDS_MAX,
                    });
                }
                for command in commands {
                    validate_required("command name", &command.name, PROVIDER_EVENT_ID_MAX_BYTES)?;
                    validate_text(
                        "command description",
                        &command.description,
                        PROVIDER_EVENT_TEXT_MAX_BYTES,
                    )?;
                    if let Some(hint) = &command.input_hint {
                        validate_text("command input hint", hint, PROVIDER_EVENT_TEXT_MAX_BYTES)?;
                    }
                }
            }
            Self::ModeChanged { mode_id, available } => {
                validate_required("mode id", mode_id, PROVIDER_EVENT_ID_MAX_BYTES)?;
                if available.len() > PROVIDER_MODE_CATALOG_MAX {
                    return Err(ProviderEventValidationError::TooManyModes {
                        count: available.len(),
                        max: PROVIDER_MODE_CATALOG_MAX,
                    });
                }
                for mode in available {
                    validate_required("available mode id", &mode.id, PROVIDER_EVENT_ID_MAX_BYTES)?;
                    validate_required(
                        "available mode name",
                        &mode.name,
                        PROVIDER_EVENT_ID_MAX_BYTES,
                    )?;
                    if let Some(description) = &mode.description {
                        validate_text(
                            "available mode description",
                            description,
                            PROVIDER_EVENT_TEXT_MAX_BYTES,
                        )?;
                    }
                }
            }
            Self::SessionInfoUpdated { title } => {
                if let Some(title) = title {
                    validate_text("session title", title, PROVIDER_EVENT_TEXT_MAX_BYTES)?;
                }
            }
            Self::UsageUpdated {
                cost_amount,
                cost_currency,
                ..
            } => {
                if let Some(amount) = cost_amount {
                    validate_identifier("usage cost amount", amount, PROVIDER_EVENT_ID_MAX_BYTES)?;
                }
                if let Some(currency) = cost_currency {
                    validate_identifier(
                        "usage cost currency",
                        currency,
                        PROVIDER_EVENT_ID_MAX_BYTES,
                    )?;
                }
            }
            Self::ConfigOptionsUpdated { options } => {
                if options.len() > PROVIDER_CONFIG_OPTIONS_MAX {
                    return Err(ProviderEventValidationError::TooManyConfigOptions {
                        count: options.len(),
                        max: PROVIDER_CONFIG_OPTIONS_MAX,
                    });
                }
                for option in options {
                    validate_required("config option id", &option.id, PROVIDER_EVENT_ID_MAX_BYTES)?;
                    validate_required(
                        "config option name",
                        &option.name,
                        PROVIDER_EVENT_ID_MAX_BYTES,
                    )?;
                    if let Some(description) = &option.description {
                        validate_text(
                            "config option description",
                            description,
                            PROVIDER_EVENT_TEXT_MAX_BYTES,
                        )?;
                    }
                    if let Some(category) = &option.category {
                        validate_identifier(
                            "config option category",
                            category,
                            PROVIDER_EVENT_ID_MAX_BYTES,
                        )?;
                    }
                    validate_text(
                        "config option value",
                        &option.value_json,
                        PROVIDER_EVENT_TEXT_MAX_BYTES,
                    )?;
                    if option.choices.len() > PROVIDER_CONFIG_OPTION_CHOICES_MAX {
                        return Err(ProviderEventValidationError::TooManyConfigOptionChoices {
                            count: option.choices.len(),
                            max: PROVIDER_CONFIG_OPTION_CHOICES_MAX,
                        });
                    }
                    for choice in &option.choices {
                        validate_text(
                            "config option choice value",
                            &choice.value_json,
                            PROVIDER_EVENT_TEXT_MAX_BYTES,
                        )?;
                        if let Some(label) = &choice.label {
                            validate_text(
                                "config option choice label",
                                label,
                                PROVIDER_EVENT_TEXT_MAX_BYTES,
                            )?;
                        }
                    }
                }
            }
            Self::WorkingObserved
            | Self::TurnCompleted { .. }
            | Self::TurnInterrupted
            | Self::Ready => {}
            Self::ContextWindowUsage { usage } => usage.validate()?,
        }
        Ok(())
    }
}

fn validate_required(
    field: &'static str,
    value: &str,
    max: usize,
) -> Result<(), ProviderEventValidationError> {
    if value.trim().is_empty() {
        return Err(ProviderEventValidationError::Empty { field });
    }
    validate_identifier(field, value, max)
}

fn validate_required_text(
    field: &'static str,
    value: &str,
    max: usize,
) -> Result<(), ProviderEventValidationError> {
    if value.trim().is_empty() {
        return Err(ProviderEventValidationError::Empty { field });
    }
    validate_text(field, value, max)
}

fn validate_identifier(
    field: &'static str,
    value: &str,
    max: usize,
) -> Result<(), ProviderEventValidationError> {
    if value.len() > max || value.chars().any(char::is_control) {
        return Err(ProviderEventValidationError::InvalidField { field, max });
    }
    Ok(())
}

fn validate_optional_agent_id(
    agent_id: &Option<String>,
) -> Result<(), ProviderEventValidationError> {
    if let Some(agent_id) = agent_id {
        validate_required("provider agent id", agent_id, PROVIDER_EVENT_ID_MAX_BYTES)?;
    }
    Ok(())
}

fn validate_text(
    field: &'static str,
    value: &str,
    max: usize,
) -> Result<(), ProviderEventValidationError> {
    let has_unsafe_control = value
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'));
    if value.len() > max || has_unsafe_control {
        return Err(ProviderEventValidationError::InvalidField { field, max });
    }
    Ok(())
}

/// Bounds check for the `mode_id`/`option_id`/`model_id` an ACP session
/// control command (`ControlCommand::SetSessionMode`/
/// `SetSessionConfigOption`/`SetSessionModel`) carries -- the same bound as
/// any other provider-scoped id (`PROVIDER_EVENT_ID_MAX_BYTES`), required
/// and free of control characters. Mirrors what
/// `gate4agent-node-protocol`'s wire boundary already enforces
/// (`deserialize_acp_control_id`) before a command ever reaches
/// `gate4agent-engine`; re-checked here because a `ControlCommand` is
/// constructible directly (tests, other embedders), not only through that
/// one wire.
pub fn validate_session_control_id(
    field: &'static str,
    value: &str,
) -> Result<(), ProviderEventValidationError> {
    validate_required(field, value, PROVIDER_EVENT_ID_MAX_BYTES)
}

/// Bounds check for `ControlCommand::SetSessionConfigOption`'s
/// `value_json`: required, bounded the same as any other provider-scoped
/// free text (`PROVIDER_EVENT_TEXT_MAX_BYTES`), free of unsafe control
/// bytes. Mirrors `gate4agent-node-protocol`'s
/// `deserialize_acp_config_value_json` minus the JSON-parseability check --
/// this crate is a pure data contract and does not depend on `serde_json`,
/// so confirming the text actually parses is the shell executor's job
/// (`AcpSession::set_config_option` takes an already-parsed
/// `serde_json::Value`).
pub fn validate_session_config_value_json(
    value: &str,
) -> Result<(), ProviderEventValidationError> {
    validate_required_text(
        "session config option value",
        value,
        PROVIDER_EVENT_TEXT_MAX_BYTES,
    )
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ProviderEventValidationError {
    #[error("provider event field '{field}' is required")]
    Empty { field: &'static str },
    #[error("provider event field '{field}' contains controls or exceeds {max} bytes")]
    InvalidField { field: &'static str, max: usize },
    #[error("provider event tool count {count} exceeds {max}")]
    TooManyTools { count: usize, max: usize },
    #[error("provider interaction resolution outcome {outcome:?} is not exact")]
    InvalidInteractionResolutionOutcome { outcome: ProviderInteractionOutcome },
    #[error("context-window capacity must be non-zero")]
    ZeroContextWindowCapacity,
    #[error("context-window token segments overflow u64")]
    ContextWindowSegmentsOverflow,
    #[error("context-window token segments sum to {segment_sum}, not used_tokens {used_tokens}")]
    ContextWindowSegmentsMismatch { segment_sum: u64, used_tokens: u64 },
    #[error("provider plan step count {count} exceeds {max}")]
    TooManyPlanSteps { count: usize, max: usize },
    #[error("provider available command count {count} exceeds {max}")]
    TooManyAvailableCommands { count: usize, max: usize },
    #[error("provider config option count {count} exceeds {max}")]
    TooManyConfigOptions { count: usize, max: usize },
    #[error("provider config option choice count {count} exceeds {max}")]
    TooManyConfigOptionChoices { count: usize, max: usize },
    #[error("provider mode catalog count {count} exceeds {max}")]
    TooManyModes { count: usize, max: usize },
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct ProviderSource {
    pub family: AdapterFamily,
    pub binding: AdapterBinding,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderSourceCursor {
    pub source: ProviderSource,
    pub sequence: u64,
    pub gap_count: u64,
    pub stale: bool,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderInteractionId(pub u64);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderInteractionTarget {
    pub interaction_id: ProviderInteractionId,
    pub source: ProviderSource,
    pub provider_request_id: Option<String>,
    pub interaction_kind: ProviderInteractionKind,
    pub tool_name: String,
    pub agent_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ProviderInteractionStatus {
    Pending,
    Resolving {
        operation_id: OperationId,
        response_kind: ProviderInteractionResponseKind,
    },
    Resolved {
        outcome: ProviderInteractionOutcome,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderInteraction {
    pub id: ProviderInteractionId,
    pub source: ProviderSource,
    pub provider_request_id: Option<String>,
    pub interaction_kind: ProviderInteractionKind,
    pub tool_name: String,
    pub prompt: String,
    pub agent_id: Option<String>,
    pub resume_lead_activity: Option<ProviderActivity>,
    pub status: ProviderInteractionStatus,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderActivity {
    #[default]
    Idle,
    Working,
    WaitingForInput,
    Blocked,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ActiveProviderTool {
    pub id: String,
    pub name: String,
    pub input_json: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderSnapshot {
    pub sequence: u64,
    pub session: Option<ProviderSessionIdentity>,
    pub model: Option<String>,
    pub tools: Vec<String>,
    pub completed_turns: u64,
    pub usage: TokenUsage,
    pub lead_activity: ProviderActivity,
    pub activity: ProviderActivity,
    pub current_prompt: Option<String>,
    pub active_tools: Vec<ActiveProviderTool>,
    pub interactions: Vec<ProviderInteraction>,
    pub subagents: Vec<ProviderSubagent>,
    pub sources: Vec<ProviderSourceCursor>,
    pub last_event: Option<ProviderEvent>,
    pub gap_count: u64,
    pub stale: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ControlSnapshot {
    pub protocol_version: u16,
    pub revision: u64,
    pub health: ControlHealth,
    pub sessions: Vec<SessionSnapshot>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ControlHealth {
    pub operation_id_exhausted: bool,
    pub event_sequence_exhausted: bool,
    pub revision_exhausted: bool,
    pub provider_sequence_exhausted_sessions: u32,
    pub retained_instance_identities: u32,
    pub retained_instance_identity_capacity: u32,
}

impl Default for ControlHealth {
    fn default() -> Self {
        Self {
            operation_id_exhausted: false,
            event_sequence_exhausted: false,
            revision_exhausted: false,
            provider_sequence_exhausted_sessions: 0,
            retained_instance_identities: 0,
            retained_instance_identity_capacity: CONTROL_INSTANCE_IDENTITIES_CAPACITY,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ControlEvent {
    pub protocol_version: u16,
    pub sequence: u64,
    pub command_id: Option<CommandId>,
    pub instance_id: AgentInstanceId,
    pub generation: SessionGeneration,
    pub event: ControlEventKind,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ControlEventKind {
    CommandRejected {
        message: String,
    },
    Registered,
    StartRequested {
        operation_id: OperationId,
    },
    Running {
        process_id: Option<u32>,
    },
    StopRequested {
        operation_id: OperationId,
        force: bool,
    },
    InputRequested {
        operation_id: OperationId,
        input_kind: PreparedInputKind,
    },
    InputCompleted {
        input_kind: PreparedInputKind,
    },
    InputFailed {
        input_kind: PreparedInputKind,
        message: String,
    },
    ResizeRequested {
        operation_id: OperationId,
        size: TerminalSize,
    },
    Resized {
        size: TerminalSize,
    },
    ResizeFailed {
        message: String,
    },
    ForegroundRefreshRequested {
        operation_id: OperationId,
    },
    ForegroundObserved {
        process: ForegroundProcess,
    },
    ForegroundFailed {
        message: String,
    },
    CapabilityProbeRequested {
        operation_id: OperationId,
    },
    CapabilitiesProbed {
        count: usize,
    },
    CapabilityProbeFailed {
        failure: CapabilityProbeFailure,
    },
    HistoryRequested {
        operation_id: OperationId,
        operation: HistoryOperation,
    },
    HistoryDiscovered {
        count: usize,
    },
    HistoryLoaded {
        session_id: String,
    },
    HistoryFailed {
        message: String,
    },
    ResumeRequested {
        operation_id: OperationId,
        target: ResumeTarget,
    },
    ResumeAuthorized {
        session: ResumeSessionSummary,
    },
    Resumed {
        session: ResumeSessionSummary,
        process_id: Option<u32>,
    },
    ResumeDenied {
        reason: String,
    },
    ResumeFailed {
        message: String,
    },
    TerminalStale {
        message: String,
    },
    ProviderEvent {
        sequence: u64,
        source: ProviderSource,
        source_sequence: u64,
        event: ProviderEvent,
    },
    ProviderGap {
        sequence: u64,
        source: ProviderSource,
        source_sequence: u64,
        missed: u64,
    },
    InteractionRequested {
        interaction: ProviderInteraction,
    },
    InteractionResolutionRequested {
        operation_id: OperationId,
        interaction_id: ProviderInteractionId,
        response_kind: ProviderInteractionResponseKind,
    },
    InteractionResolutionFailed {
        interaction_id: ProviderInteractionId,
        message: String,
    },
    InteractionResolved {
        interaction_id: ProviderInteractionId,
        outcome: ProviderInteractionOutcome,
    },
    SessionModeSetRequested {
        operation_id: OperationId,
        mode_id: String,
    },
    SessionModeSet {
        mode_id: String,
    },
    SessionModeSetFailed {
        message: String,
    },
    SessionConfigOptionSetRequested {
        operation_id: OperationId,
        option_id: String,
    },
    SessionConfigOptionSet {
        option_id: String,
    },
    SessionConfigOptionSetFailed {
        message: String,
    },
    SessionModelSetRequested {
        operation_id: OperationId,
        model_id: String,
    },
    SessionModelSet {
        model_id: String,
    },
    SessionModelSetFailed {
        message: String,
    },
    Exited {
        exit_code: Option<i32>,
        forced: bool,
    },
    Failed {
        message: String,
    },
    Removed,
    ObservationIgnored {
        reason: ObservationIgnoredReason,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ObservationIgnoredReason {
    UnsupportedProtocolVersion,
    UnknownInstance,
    StaleGeneration,
    GenerationExhausted,
    MissingOperation,
    OperationMismatch,
    InvalidState,
    StaleTerminalFrame,
    StaleProviderEvent,
    InvalidForegroundObservation,
    InvalidCapabilityObservation,
    InvalidHistoryObservation,
    InvalidResumeObservation,
    InvalidInteractionObservation,
    ProviderRuntimePolicyDenied {
        capability: ProviderRuntimeCapability,
    },
}

#[derive(Clone, Debug, Eq, Error, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ControlError {
    #[error("control protocol version {actual} is unsupported; expected {expected}")]
    UnsupportedProtocolVersion { expected: u16, actual: u16 },
    #[error("agent instance {instance_id:?} is already registered")]
    DuplicateInstance { instance_id: AgentInstanceId },
    #[error(
        "cannot register agent instance {instance_id:?}: live session capacity {max} is exhausted"
    )]
    SessionCapacityExceeded {
        instance_id: AgentInstanceId,
        max: usize,
    },
    #[error(
        "cannot register agent instance {instance_id:?}: retained identity capacity {max} is exhausted"
    )]
    InstanceIdentityCapacityExceeded {
        instance_id: AgentInstanceId,
        max: usize,
    },
    #[error("agent instance {instance_id:?} is not registered")]
    UnknownInstance { instance_id: AgentInstanceId },
    #[error("agent instance {instance_id:?} exhausted session generation {generation:?}")]
    GenerationExhausted {
        instance_id: AgentInstanceId,
        generation: SessionGeneration,
    },
    #[error("control operation identifiers are exhausted")]
    OperationIdExhausted,
    #[error("control event sequences are exhausted")]
    EventSequenceExhausted,
    #[error("control snapshot revisions are exhausted")]
    RevisionExhausted,
    #[error(
        "agent instance {instance_id:?} generation {generation:?} exhausted provider event sequences"
    )]
    ProviderSequenceExhausted {
        instance_id: AgentInstanceId,
        generation: SessionGeneration,
    },
    #[error(
        "agent instance {instance_id:?} generation {generation:?} exhausted source sequence for {provider_source:?}"
    )]
    ProviderSourceSequenceExhausted {
        instance_id: AgentInstanceId,
        generation: SessionGeneration,
        provider_source: ProviderSource,
    },
    #[error("agent instance {instance_id:?} already has pending operation {operation_id:?}")]
    OperationPending {
        instance_id: AgentInstanceId,
        operation_id: OperationId,
    },
    #[error("agent input was rejected: {error}")]
    InputRejected { error: InputPrepareError },
    #[error("provider runtime policy is invalid: {error}")]
    InvalidProviderRuntimePolicy { error: ProviderRuntimePolicyError },
    #[error("provider runtime capability {capability:?} is not admitted")]
    ProviderRuntimePolicyDenied {
        capability: ProviderRuntimeCapability,
    },
    #[error("terminal size is outside the supported bounded range")]
    InvalidTerminalSize,
    #[error("working directory is empty, too large, or contains a NUL byte")]
    InvalidWorkingDirectory,
    #[error("pipe transport requires a non-empty initial prompt")]
    MissingInitialPrompt,
    #[error("session options are invalid: {message}")]
    InvalidSessionOptions { message: String },
    #[error("capability probe request is invalid: {message}")]
    InvalidCapabilityProbeRequest { message: String },
    #[error("capability probe operation {operation_id:?} is already pending")]
    CapabilityProbeOperationPending { operation_id: OperationId },
    #[error("capability probe already settled for this agent instance")]
    CapabilityProbeSettled,
    #[error("history request is invalid: {message}")]
    InvalidHistoryRequest { message: String },
    #[error("history operation {operation_id:?} is already pending")]
    HistoryOperationPending { operation_id: OperationId },
    #[error("history candidate is not present in the current discovery snapshot")]
    UnknownHistoryCandidate,
    #[error("resume request is invalid: {message}")]
    InvalidResumeRequest { message: String },
    #[error("resume requires a canonical provider session identity")]
    MissingProviderSession,
    #[error("resume history candidate must be the currently loaded candidate")]
    HistoryCandidateNotLoaded,
    #[error("transport {transport:?} does not support {action}")]
    UnsupportedTransportOperation {
        transport: TransportKind,
        action: String,
    },
    #[error("agent instance {instance_id:?} cannot {action} while in state {status:?}")]
    InvalidTransition {
        instance_id: AgentInstanceId,
        action: String,
        status: SessionStatus,
    },
    #[error("provider ingress generation {actual:?} is stale; expected {expected:?}")]
    StaleProviderGeneration {
        expected: SessionGeneration,
        actual: SessionGeneration,
    },
    #[error("provider ingress source sequence must be greater than the current sequence")]
    StaleProviderSequence,
    #[error("provider ingress batch must contain between 1 and {max} events")]
    InvalidProviderBatch { max: usize },
    #[error("invalid provider ingress event: {message}")]
    InvalidProviderEvent { message: String },
    #[error("provider interaction generation {actual:?} is stale; expected {expected:?}")]
    StaleProviderInteractionGeneration {
        expected: SessionGeneration,
        actual: SessionGeneration,
    },
    #[error("provider interaction {interaction_id:?} is unknown")]
    UnknownProviderInteraction {
        interaction_id: ProviderInteractionId,
    },
    #[error("provider interaction {interaction_id:?} is not pending")]
    ProviderInteractionNotPending {
        interaction_id: ProviderInteractionId,
    },
    #[error("provider interaction response is invalid: {message}")]
    InvalidProviderInteractionResponse { message: String },
    #[error("session mode request is invalid: {message}")]
    InvalidSessionModeRequest { message: String },
    #[error("session config option request is invalid: {message}")]
    InvalidSessionConfigOptionRequest { message: String },
    #[error("session model request is invalid: {message}")]
    InvalidSessionModelRequest { message: String },
}

impl Default for ControlSnapshot {
    fn default() -> Self {
        Self {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            revision: 0,
            health: ControlHealth::default(),
            sessions: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {

    /// The split `SessionStatus::allows_remove` draws, stated once so the
    /// two crates that consult it (`gate4agent-engine`'s own `remove`
    /// guard, `gate4agent-node`'s `wait_until_removed` re-dispatch gate)
    /// are pinned to the same answer.
    ///
    /// The three live statuses refuse: a session that is starting, running
    /// or stopping still owns a process. The three settled ones allow. The
    /// node's retry loop reads this BEFORE dispatching, because a rejected
    /// `Remove` fans a `CommandRejected` out to every subscriber -- 241 of
    /// them in 1.6 seconds when the loop dispatched blind.
    #[test]
    fn only_a_settled_session_admits_a_remove() {
        use crate::SessionStatus;

        for live in [
            SessionStatus::Starting,
            SessionStatus::Running,
            SessionStatus::Stopping,
        ] {
            assert!(
                !live.allows_remove(),
                "{live:?} still owns a process; a Remove for it is rejected",
            );
        }
        for settled in [
            SessionStatus::Registered,
            SessionStatus::Exited { exit_code: Some(0) },
            SessionStatus::Exited { exit_code: None },
            SessionStatus::Failed { message: "boom".to_owned() },
        ] {
            assert!(
                settled.allows_remove(),
                "{settled:?} is settled; a Remove for it must be accepted",
            );
        }
    }

    use crate::AgentId;
    use super::{
        AgentInstanceId, CapabilitySnapshot, ContextWindowUsage, ForegroundProcess,
        ForegroundProcessKind, ForegroundSnapshot, HistorySnapshot, HostDecisionAuthority,
        HostRequestDecision, OperatorGateInput,
        OperatorGateKind, OperatorGateOption, OperatorGateOptionSemantics, OperatorGateState,
        OperatorGateSubject, ProviderAvailableCommand, ProviderConfigChoice, ProviderConfigOption,
        ProviderConfigOptionKind, ProviderEvent,
        ProviderEventValidationError,
        ProviderInteractionKind, ProviderInteractionOutcome, ProviderInteractionResponse,
        ProviderInteractionResponseError, ProviderModeInfo, ProviderPlanPriority,
        ProviderPlanStatus,
        ProviderPlanStep, ProviderRuntimeCapability, ProviderRuntimePolicy,
        ProviderRuntimePolicyError,
        ProviderSessionIdentity, ProviderSessionKey, ProviderSnapshot, PtyScreenState,
        ResumeSnapshot, SessionGeneration, SessionSnapshot, SessionStatus, TerminalFrame,
        TerminalMouseProtocolEncoding, TransportKind,
        FOREGROUND_PROCESS_NAME_MAX_BYTES, OPERATOR_GATE_OPTIONS_MAX,
        OPERATOR_GATE_OPTION_TEXT_MAX_BYTES, OPERATOR_GATE_PATH_MAX_BYTES,
        PROVIDER_AVAILABLE_COMMANDS_MAX,
        PROVIDER_EVENT_ID_MAX_BYTES, PROVIDER_EVENT_TEXT_MAX_BYTES,
        PROVIDER_INTERACTION_RESPONSE_MAX_BYTES, PROVIDER_MODE_CATALOG_MAX,
        PROVIDER_PLAN_STEPS_MAX,
        PTY_SCREEN_GATE_NAME_MAX_BYTES,
    };

    /// Shared fixture: a fully-known gate (every field populated), used by
    /// every test below that needs "some real `OperatorGateState`" without
    /// re-deriving one -- one option accepted, one declined, matching the
    /// shape `parse_operator_gate_options` actually produces for a numbered
    /// list (see `gate4agent-shell-native`'s own tests for the parser
    /// itself; this crate only owns the data shape and its bounds).
    fn sample_gate() -> OperatorGateState {
        OperatorGateState::new(OperatorGateKind::HookTrust)
            .with_subject(OperatorGateSubject::Hooks { count: Some(6) })
            .with_options(
                OperatorGateInput::NumberedList,
                vec![
                    OperatorGateOption {
                        text: "Trust all and continue".to_owned(),
                        semantics: OperatorGateOptionSemantics::Accept,
                        selected: false,
                    },
                    OperatorGateOption {
                        text: "Continue without trusting".to_owned(),
                        semantics: OperatorGateOptionSemantics::Decline,
                        selected: true,
                    },
                ],
            )
    }

    #[test]
    fn context_window_usage_ingress_requires_exact_bounded_segments() {
        let event = |usage| ProviderEvent::ContextWindowUsage { usage };
        let valid = ContextWindowUsage {
            uncached_input_tokens: 70,
            cache_read_tokens: 20,
            cache_write_tokens: 0,
            output_tokens: 10,
            unattributed_tokens: 5,
            used_tokens: 105,
            capacity_tokens: 100,
        };
        assert_eq!(event(valid).validate_ingress(), Ok(()));
        assert_eq!(
            event(ContextWindowUsage { capacity_tokens: 0, ..valid }).validate_ingress(),
            Err(ProviderEventValidationError::ZeroContextWindowCapacity)
        );
        assert_eq!(
            event(ContextWindowUsage { used_tokens: 104, ..valid }).validate_ingress(),
            Err(ProviderEventValidationError::ContextWindowSegmentsMismatch {
                segment_sum: 105,
                used_tokens: 104,
            })
        );
        assert_eq!(
            event(ContextWindowUsage {
                uncached_input_tokens: u64::MAX,
                cache_read_tokens: 1,
                cache_write_tokens: 0,
                output_tokens: 0,
                unattributed_tokens: 0,
                used_tokens: u64::MAX,
                capacity_tokens: 1,
            })
            .validate_ingress(),
            Err(ProviderEventValidationError::ContextWindowSegmentsOverflow)
        );
    }

    #[test]
    fn provider_runtime_policy_enforces_semantic_invariants() {
        let raw = ProviderRuntimePolicy::raw_pty();
        assert!(raw.admits(ProviderRuntimeCapability::RawPtyLifecycle));
        assert!(!raw.admits(ProviderRuntimeCapability::SemanticReadiness));
        assert!(!raw.admits(ProviderRuntimeCapability::HookSemantics));
        assert_eq!(raw.validate(), Ok(()));

        let none = ProviderRuntimePolicy::none();
        assert!(!none.admits(ProviderRuntimeCapability::RawPtyLifecycle));
        assert!(!none.admits(ProviderRuntimeCapability::SemanticReadiness));
        assert_eq!(none.validate(), Ok(()));

        // The ACP shape: `session/prompt`/`session/update` are mandatory ACP
        // protocol surface, and `session/new` returns a `sessionId` under
        // that same specification -- none of it a PTY-terminal-text
        // inference, so this transport grants `semantic_readiness`/
        // `structured_prompt`/`provider_session_identity` with no raw PTY
        // lifecycle at all, and that is now a VALID policy, not the
        // `SemanticCapabilityRequiresRawPty` defect it used to be.
        assert_eq!(
            ProviderRuntimePolicy::new(false, true, true, true, false, false),
            Ok(ProviderRuntimePolicy {
                raw_pty_lifecycle: false,
                semantic_readiness: true,
                structured_prompt: true,
                provider_session_identity: true,
                semantic_resume: false,
                hook_semantics: false,
            }),
        );
        // `structured_prompt` still requires `semantic_readiness`, and that
        // rule holds independent of `raw_pty_lifecycle` -- it is not the rule
        // the ACP shape above loosened.
        assert_eq!(
            ProviderRuntimePolicy::new(false, false, true, false, false, false),
            Err(ProviderRuntimePolicyError::StructuredPromptRequiresReadiness),
        );
        assert_eq!(
            ProviderRuntimePolicy::new(true, false, true, false, false, false),
            Err(ProviderRuntimePolicyError::StructuredPromptRequiresReadiness),
        );
        assert_eq!(
            ProviderRuntimePolicy::new(true, true, true, false, true, false),
            Err(ProviderRuntimePolicyError::ResumeRequiresSessionIdentity),
        );
        assert_eq!(
            ProviderRuntimePolicy::new(false, false, false, false, false, true),
            Err(ProviderRuntimePolicyError::SemanticCapabilityRequiresRawPty),
        );
        // Granting `semantic_readiness`/`structured_prompt`/
        // `provider_session_identity` without a raw PTY lifecycle (the ACP
        // shape) must NOT silently unlock `semantic_resume`/`hook_semantics`
        // -- those two keep the old, stricter rule, even with every other
        // field in the ACP shape already granted.
        assert_eq!(
            ProviderRuntimePolicy::new(false, true, true, true, true, false),
            Err(ProviderRuntimePolicyError::SemanticCapabilityRequiresRawPty),
        );
        assert_eq!(
            ProviderRuntimePolicy::new(false, true, true, true, false, true),
            Err(ProviderRuntimePolicyError::SemanticCapabilityRequiresRawPty),
        );
        assert!(ProviderRuntimePolicy::new(true, true, true, true, true, true).is_ok());
    }

    /// The pair that makes hook ingestion work for a provider like grok: a
    /// hook adapter with no verified vendor terminal contract still admits
    /// its own events, and that admission never leaks into the PTY-parsing
    /// `SemanticReadiness` capability it is deliberately independent from.
    #[test]
    fn hook_semantics_and_semantic_readiness_are_independently_grantable() {
        let hook_only = ProviderRuntimePolicy::new(true, false, false, false, false, true)
            .expect("hook semantics alone requires only the raw PTY lifecycle");
        assert!(hook_only.admits(ProviderRuntimeCapability::HookSemantics));
        assert!(!hook_only.admits(ProviderRuntimeCapability::SemanticReadiness));

        let semantic_only = ProviderRuntimePolicy::new(true, true, false, false, false, false)
            .expect("semantic readiness alone requires only the raw PTY lifecycle");
        assert!(semantic_only.admits(ProviderRuntimeCapability::SemanticReadiness));
        assert!(!semantic_only.admits(ProviderRuntimeCapability::HookSemantics));
    }

    #[test]
    fn provider_runtime_policy_serde_requires_every_field_and_revalidates() {
        let raw = ProviderRuntimePolicy::raw_pty();
        let encoded = serde_json::to_string(&raw).unwrap();
        assert_eq!(
            encoded,
            r#"{"raw_pty_lifecycle":true,"semantic_readiness":false,"structured_prompt":false,"provider_session_identity":false,"semantic_resume":false,"hook_semantics":false}"#,
        );
        assert_eq!(
            serde_json::from_str::<ProviderRuntimePolicy>(&encoded).unwrap(),
            raw,
        );
        assert!(serde_json::from_str::<ProviderRuntimePolicy>(
            r#"{"raw_pty_lifecycle":true,"semantic_readiness":false,"structured_prompt":false,"provider_session_identity":false,"semantic_resume":false}"#,
        )
        .is_err());
        assert!(serde_json::from_str::<ProviderRuntimePolicy>(
            r#"{"raw_pty_lifecycle":true,"semantic_readiness":false,"structured_prompt":true,"provider_session_identity":false,"semantic_resume":false,"hook_semantics":false}"#,
        )
        .is_err());
        assert!(serde_json::from_str::<super::ControlCommand>(
            r#"{"kind":"start","instance_id":1,"request":{"working_directory":"C:\\repo","terminal_size":{"rows":24,"columns":80}}}"#,
        )
        .is_err());
        assert!(serde_json::from_str::<super::ControlEffect>(
            r#"{"kind":"spawn","agent_id":"claude","transport":"pty","request":{"working_directory":"C:\\repo","terminal_size":{"rows":24,"columns":80}}}"#,
        )
        .is_err());
    }

    #[test]
    fn foreground_process_is_bounded_and_agent_bound() {
        let claude = AgentId::new("claude").unwrap();
        let process = ForegroundProcess {
            root_process_id: 1,
            process_id: 2,
            process_name: "claude".to_owned(),
            kind: ForegroundProcessKind::Agent {
                agent_id: claude.clone(),
            },
        };
        assert!(process.is_valid_for(&claude));
        assert!(!process.is_valid_for(&AgentId::new("codex").unwrap()));
        assert!(!ForegroundProcess {
            process_name: "x".repeat(FOREGROUND_PROCESS_NAME_MAX_BYTES + 1),
            ..process
        }
        .is_valid_for(&claude));
    }

    #[test]
    fn terminal_frame_metadata_defaults_for_older_serialized_frames() {
        let frame: TerminalFrame = serde_json::from_str(
            r#"{"sequence":1,"size":{"rows":24,"columns":80},"cursor_row":0,"cursor_column":0,"contents":"ready","formatted":[114]}"#,
        )
        .expect("legacy terminal frame");

        assert!(frame.scrollback_formatted.is_empty());
        assert!(!frame.alternate_screen);
        assert!(!frame.mouse_protocol_enabled);
        assert_eq!(frame.mouse_protocol_encoding, TerminalMouseProtocolEncoding::Default);
        assert_eq!(frame.produced_at_unix_ms, 0);
    }

    #[test]
    fn a_terminal_frame_that_omits_screen_state_decodes_as_unknown_not_ready() {
        let frame: TerminalFrame = serde_json::from_str(
            r#"{"sequence":1,"size":{"rows":24,"columns":80},"cursor_row":0,"cursor_column":0,"contents":"ready","formatted":[114]}"#,
        )
        .expect("legacy terminal frame");

        assert_eq!(frame.screen_state, PtyScreenState::Unknown);
        // A peer that predates this field carries no classification at all --
        // reading that silence as `Ready` would hand a blind writer a green
        // light nobody ever actually gave.
        assert_ne!(frame.screen_state, PtyScreenState::Ready);
    }

    #[test]
    fn a_session_snapshot_that_omits_screen_state_decodes_as_unknown_not_ready() {
        let populated = SessionSnapshot {
            instance_id: AgentInstanceId(1),
            agent_id: AgentId::new("claude").unwrap(),
            transport: TransportKind::Pty,
            generation: SessionGeneration(1),
            status: SessionStatus::Running,
            pending_operation: None,
            pending_input: None,
            process_id: None,
            terminal_size: None,
            terminal_frame: None,
            terminal_stale: None,
            session_options: None,
            capabilities: CapabilitySnapshot::default(),
            history: HistorySnapshot::default(),
            resume: ResumeSnapshot::default(),
            foreground: ForegroundSnapshot::default(),
            provider: ProviderSnapshot::default(),
            screen_state: PtyScreenState::Ready,
        };
        let mut wire = serde_json::to_value(&populated).unwrap();
        wire.as_object_mut().unwrap().remove("screen_state");
        let decoded: SessionSnapshot = serde_json::from_value(wire).unwrap();

        assert_eq!(decoded.screen_state, PtyScreenState::Unknown);
        // Same reasoning as the terminal-frame case: an old peer's silence
        // on this field must never be upgraded into a claim it never made.
        assert_ne!(decoded.screen_state, PtyScreenState::Ready);
    }

    #[test]
    fn every_pty_screen_state_variant_round_trips_through_serde() {
        let variants = [
            PtyScreenState::Unknown,
            PtyScreenState::NotAgent {
                observed_process: "npm".to_owned(),
            },
            PtyScreenState::OperatorGate { gate: sample_gate() },
            PtyScreenState::Failing {
                reason: "startup-crash".to_owned(),
            },
            PtyScreenState::Ready,
        ];
        for variant in variants {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(
                serde_json::from_str::<PtyScreenState>(&json).unwrap(),
                variant,
            );
        }
    }

    /// Refusal follows a FINDING, not the absence of one. The three states
    /// that carry something the matcher actually read still refuse; `Ready`
    /// and `Unknown` both admit, because "recognized nothing" is not a
    /// reason to treat a screen as an obstacle -- and every provider sits in
    /// `Unknown` for the frame or two before process identity resolves,
    /// which is not a state worth refusing.
    #[test]
    fn admits_blind_write_refuses_a_finding_and_admits_the_absence_of_one() {
        assert!(PtyScreenState::Ready.admits_blind_write());
        assert!(PtyScreenState::Unknown.admits_blind_write());
        assert!(!PtyScreenState::NotAgent {
            observed_process: "npm".to_owned(),
        }
        .admits_blind_write());
        assert!(!PtyScreenState::OperatorGate { gate: sample_gate() }.admits_blind_write());
        assert!(!PtyScreenState::Failing {
            reason: "startup-crash".to_owned(),
        }
        .admits_blind_write());
    }

    #[test]
    fn pty_screen_state_is_valid_rejects_oversized_empty_and_control_carrying_fields() {
        assert!(PtyScreenState::NotAgent {
            observed_process: "npm install".to_owned(),
        }
        .is_valid());
        assert!(!PtyScreenState::NotAgent {
            observed_process: "x".repeat(FOREGROUND_PROCESS_NAME_MAX_BYTES + 1),
        }
        .is_valid());
        assert!(!PtyScreenState::NotAgent {
            observed_process: String::new(),
        }
        .is_valid());
        assert!(!PtyScreenState::NotAgent {
            observed_process: "bad\u{0000}process".to_owned(),
        }
        .is_valid());

        assert!(PtyScreenState::OperatorGate { gate: sample_gate() }.is_valid());
        assert!(!PtyScreenState::OperatorGate {
            gate: sample_gate().with_subject(OperatorGateSubject::Directory {
                path: Some("x".repeat(OPERATOR_GATE_PATH_MAX_BYTES + 1)),
            }),
        }
        .is_valid());
        assert!(!PtyScreenState::OperatorGate {
            gate: sample_gate().with_subject(OperatorGateSubject::Directory {
                path: Some(String::new()),
            }),
        }
        .is_valid());
        assert!(!PtyScreenState::OperatorGate {
            gate: sample_gate().with_subject(OperatorGateSubject::Directory {
                path: Some("bad\u{0000}path".to_owned()),
            }),
        }
        .is_valid());
        assert!(!PtyScreenState::OperatorGate {
            gate: sample_gate().with_options(
                OperatorGateInput::NumberedList,
                vec![OperatorGateOption {
                    text: "x".repeat(OPERATOR_GATE_OPTION_TEXT_MAX_BYTES + 1),
                    semantics: OperatorGateOptionSemantics::Accept,
                    selected: false,
                }],
            ),
        }
        .is_valid());
        assert!(!PtyScreenState::OperatorGate {
            gate: sample_gate().with_options(
                OperatorGateInput::NumberedList,
                vec![OperatorGateOption {
                    text: String::new(),
                    semantics: OperatorGateOptionSemantics::Accept,
                    selected: false,
                }],
            ),
        }
        .is_valid());
        assert!(!PtyScreenState::OperatorGate {
            gate: sample_gate().with_options(
                OperatorGateInput::NumberedList,
                vec![OperatorGateOption {
                    text: "bad\u{0000}option".to_owned(),
                    semantics: OperatorGateOptionSemantics::Accept,
                    selected: false,
                }],
            ),
        }
        .is_valid());
        assert!(!PtyScreenState::OperatorGate {
            gate: sample_gate().with_options(
                OperatorGateInput::NumberedList,
                (0..=OPERATOR_GATE_OPTIONS_MAX)
                    .map(|index| OperatorGateOption {
                        text: format!("option {index}"),
                        semantics: OperatorGateOptionSemantics::Unknown,
                        selected: false,
                    })
                    .collect(),
            ),
        }
        .is_valid());

        assert!(PtyScreenState::Failing {
            reason: "startup-crash".to_owned(),
        }
        .is_valid());
        assert!(!PtyScreenState::Failing {
            reason: "x".repeat(PTY_SCREEN_GATE_NAME_MAX_BYTES + 1),
        }
        .is_valid());
        assert!(!PtyScreenState::Failing {
            reason: String::new(),
        }
        .is_valid());
        assert!(!PtyScreenState::Failing {
            reason: "bad\u{0000}reason".to_owned(),
        }
        .is_valid());

        assert!(PtyScreenState::Unknown.is_valid());
        assert!(PtyScreenState::Ready.is_valid());
    }

    #[test]
    fn provider_interactions_require_bounded_identity_and_question_payloads() {
        let question = ProviderEvent::InteractionRequested {
            request_id: Some("question-1".to_owned()),
            interaction_kind: ProviderInteractionKind::Question,
            tool_name: "AskUserQuestion".to_owned(),
            prompt: "{\"question\":\"Continue?\"}".to_owned(),
            agent_id: Some("child-1".to_owned()),
        };
        assert_eq!(question.validate_ingress(), Ok(()));

        assert!(matches!(
            ProviderEvent::InteractionRequested {
                request_id: Some("bad\nrequest".to_owned()),
                interaction_kind: ProviderInteractionKind::Approval,
                tool_name: "shell".to_owned(),
                prompt: String::new(),
                agent_id: None,
            }
            .validate_ingress(),
            Err(ProviderEventValidationError::InvalidField {
                field: "interaction request id",
                ..
            })
        ));
        assert!(matches!(
            ProviderEvent::InteractionRequested {
                request_id: None,
                interaction_kind: ProviderInteractionKind::Question,
                tool_name: "AskUserQuestion".to_owned(),
                prompt: String::new(),
                agent_id: None,
            }
            .validate_ingress(),
            Err(ProviderEventValidationError::Empty {
                field: "interaction prompt"
            })
        ));

        for outcome in [
            ProviderInteractionOutcome::Approved,
            ProviderInteractionOutcome::Denied,
        ] {
            assert_eq!(
                ProviderEvent::InteractionResolved {
                    request_id: "approval-1".to_owned(),
                    outcome,
                }
                .validate_ingress(),
                Ok(())
            );
        }
        assert!(matches!(
            ProviderEvent::InteractionResolved {
                request_id: "bad\nrequest".to_owned(),
                outcome: ProviderInteractionOutcome::Approved,
            }
            .validate_ingress(),
            Err(ProviderEventValidationError::InvalidField {
                field: "interaction request id",
                ..
            })
        ));
        assert_eq!(
            ProviderEvent::InteractionResolved {
                request_id: "approval-1".to_owned(),
                outcome: ProviderInteractionOutcome::TurnEnded,
            }
            .validate_ingress(),
            Err(
                ProviderEventValidationError::InvalidInteractionResolutionOutcome {
                    outcome: ProviderInteractionOutcome::TurnEnded,
                }
            )
        );
    }

    #[test]
    fn provider_interaction_responses_are_kind_checked_and_bounded() {
        assert_eq!(
            ProviderInteractionResponse::ApproveOnce
                .validate_for(ProviderInteractionKind::Approval),
            Ok(())
        );
        assert_eq!(
            ProviderInteractionResponse::Deny.validate_for(ProviderInteractionKind::Question),
            Ok(())
        );
        assert_eq!(
            ProviderInteractionResponse::Answer {
                text: "continue".to_owned(),
            }
            .validate_for(ProviderInteractionKind::Question),
            Ok(())
        );
        assert_eq!(
            ProviderInteractionResponse::ApproveOnce
                .validate_for(ProviderInteractionKind::Question),
            Err(ProviderInteractionResponseError::ApprovalRequiresApproval)
        );
        assert_eq!(
            ProviderInteractionResponse::Answer {
                text: String::new(),
            }
            .validate_for(ProviderInteractionKind::Question),
            Err(ProviderInteractionResponseError::EmptyAnswer)
        );
        assert_eq!(
            ProviderInteractionResponse::Answer {
                text: "x".repeat(PROVIDER_INTERACTION_RESPONSE_MAX_BYTES + 1),
            }
            .validate_for(ProviderInteractionKind::Question),
            Err(ProviderInteractionResponseError::InvalidAnswer {
                max: PROVIDER_INTERACTION_RESPONSE_MAX_BYTES,
            })
        );
    }

    #[test]
    fn provider_ingress_allows_multiline_text_but_rejects_control_bytes() {
        ProviderEvent::Text {
            text: "first line\n\tsecond line".to_owned(),
            is_delta: false,
        }
        .validate_ingress()
        .unwrap();

        assert!(matches!(
            ProviderEvent::Text {
                text: "unsafe\u{0000}text".to_owned(),
                is_delta: false,
            }
            .validate_ingress(),
            Err(ProviderEventValidationError::InvalidField { field: "text", .. })
        ));
        assert!(ProviderEvent::SessionStarted {
            session_id: "session\nother".to_owned(),
            model: "model".to_owned(),
            tools: Vec::new(),
        }
        .validate_ingress()
        .is_err());
    }

    #[test]
    fn provider_session_identity_is_typed_and_bounded_at_ingress() {
        let valid = ProviderEvent::SessionIdentityObserved {
            identity: ProviderSessionIdentity {
                key: ProviderSessionKey::ConversationId,
                id: "conversation-1".to_owned(),
                transcript_path: Some("C:/sessions/conversation-1.jsonl".to_owned()),
            },
        };
        assert_eq!(valid.validate_ingress(), Ok(()));

        for identity in [
            ProviderSessionIdentity {
                key: ProviderSessionKey::SessionId,
                id: "--help".to_owned(),
                transcript_path: None,
            },
            ProviderSessionIdentity {
                key: ProviderSessionKey::SessionId,
                id: "session-1".to_owned(),
                transcript_path: Some("bad\npath".to_owned()),
            },
        ] {
            assert!(ProviderEvent::SessionIdentityObserved { identity }
                .validate_ingress()
                .is_err());
        }
    }

    /// The agent-to-host request event must carry a non-empty, bounded
    /// method and a bounded params payload -- the same shape and bounds as
    /// `ToolStarted`'s `id`/`input_json`, since this event carries the same
    /// kind of vendor-controlled JSON.
    #[test]
    fn host_request_observed_is_bounded_at_ingress() {
        let valid = ProviderEvent::HostRequestObserved {
            method: "session/request_permission".to_owned(),
            params_json: "{\"toolName\":\"bash\"}".to_owned(),
            decision: HostRequestDecision::Denied { by: HostDecisionAuthority::Policy },
        };
        assert_eq!(valid.validate_ingress(), Ok(()));

        assert!(matches!(
            ProviderEvent::HostRequestObserved {
                method: String::new(),
                params_json: String::new(),
                decision: HostRequestDecision::Denied { by: HostDecisionAuthority::Policy },
            }
            .validate_ingress(),
            Err(ProviderEventValidationError::Empty { field: "host request method" })
        ));

        let oversized_method = "m".repeat(PROVIDER_EVENT_ID_MAX_BYTES + 1);
        assert!(matches!(
            ProviderEvent::HostRequestObserved {
                method: oversized_method,
                params_json: String::new(),
                decision: HostRequestDecision::Granted { by: HostDecisionAuthority::Policy },
            }
            .validate_ingress(),
            Err(ProviderEventValidationError::InvalidField {
                field: "host request method",
                ..
            })
        ));

        let oversized_params = "p".repeat(PROVIDER_EVENT_TEXT_MAX_BYTES + 1);
        assert!(matches!(
            ProviderEvent::HostRequestObserved {
                method: "fs/read_text_file".to_owned(),
                params_json: oversized_params,
                decision: HostRequestDecision::Granted { by: HostDecisionAuthority::Policy },
            }
            .validate_ingress(),
            Err(ProviderEventValidationError::InvalidField {
                field: "host request params",
                ..
            })
        ));
    }

    /// The catch-all "protocol said something we don't parse" event must
    /// still enforce the same bounds as every other provider event carrying
    /// vendor JSON -- a hostile or garbled `session/update` payload cannot
    /// ride this fallback path past `PROVIDER_EVENT_TEXT_MAX_BYTES`.
    #[test]
    fn unrecognized_notification_is_bounded_at_ingress() {
        let valid = ProviderEvent::UnrecognizedNotification {
            method: "session/some_future_update".to_owned(),
            payload_json: "{\"unknown\":true}".to_owned(),
        };
        assert_eq!(valid.validate_ingress(), Ok(()));

        assert!(matches!(
            ProviderEvent::UnrecognizedNotification {
                method: String::new(),
                payload_json: String::new(),
            }
            .validate_ingress(),
            Err(ProviderEventValidationError::Empty {
                field: "unrecognized notification method"
            })
        ));

        let oversized_payload = "p".repeat(PROVIDER_EVENT_TEXT_MAX_BYTES + 1);
        assert!(matches!(
            ProviderEvent::UnrecognizedNotification {
                method: "session/update".to_owned(),
                payload_json: oversized_payload,
            }
            .validate_ingress(),
            Err(ProviderEventValidationError::InvalidField {
                field: "unrecognized notification payload",
                ..
            })
        ));
    }

    // -----------------------------------------------------------------------
    // ACP session/update coverage — Plan, AvailableCommandsUpdated,
    // ModeChanged, SessionInfoUpdated, UsageUpdated, ConfigOptionsUpdated,
    // UserMessage
    // -----------------------------------------------------------------------

    #[test]
    fn provider_user_message_is_bounded_at_ingress_like_text() {
        let valid = ProviderEvent::UserMessage { text: "hi".to_owned(), is_delta: true };
        assert_eq!(valid.validate_ingress(), Ok(()));

        assert!(matches!(
            ProviderEvent::UserMessage {
                text: "unsafe\u{0000}text".to_owned(),
                is_delta: true,
            }
            .validate_ingress(),
            Err(ProviderEventValidationError::InvalidField { field: "text", .. })
        ));
    }

    #[test]
    fn provider_plan_accepts_a_valid_snapshot_and_rejects_empty_step_content() {
        let valid = ProviderEvent::Plan {
            steps: vec![ProviderPlanStep {
                content: "read the file".to_owned(),
                priority: ProviderPlanPriority::High,
                status: ProviderPlanStatus::Completed,
            }],
        };
        assert_eq!(valid.validate_ingress(), Ok(()));

        assert!(matches!(
            ProviderEvent::Plan {
                steps: vec![ProviderPlanStep {
                    content: String::new(),
                    priority: ProviderPlanPriority::Low,
                    status: ProviderPlanStatus::Pending,
                }],
            }
            .validate_ingress(),
            Err(ProviderEventValidationError::Empty { field: "plan step content" })
        ));
    }

    #[test]
    fn provider_plan_rejects_too_many_steps() {
        let steps = (0..=PROVIDER_PLAN_STEPS_MAX)
            .map(|i| ProviderPlanStep {
                content: format!("step {i}"),
                priority: ProviderPlanPriority::Medium,
                status: ProviderPlanStatus::Pending,
            })
            .collect();
        assert!(matches!(
            ProviderEvent::Plan { steps }.validate_ingress(),
            Err(ProviderEventValidationError::TooManyPlanSteps {
                max: PROVIDER_PLAN_STEPS_MAX,
                ..
            })
        ));
    }

    #[test]
    fn provider_available_commands_updated_is_bounded_at_ingress() {
        let valid = ProviderEvent::AvailableCommandsUpdated {
            commands: vec![ProviderAvailableCommand {
                name: "review".to_owned(),
                description: "Review the diff".to_owned(),
                input_hint: Some("<file>".to_owned()),
            }],
        };
        assert_eq!(valid.validate_ingress(), Ok(()));

        assert!(matches!(
            ProviderEvent::AvailableCommandsUpdated {
                commands: vec![ProviderAvailableCommand {
                    name: String::new(),
                    description: String::new(),
                    input_hint: None,
                }],
            }
            .validate_ingress(),
            Err(ProviderEventValidationError::Empty { field: "command name" })
        ));

        let too_many = (0..=PROVIDER_AVAILABLE_COMMANDS_MAX)
            .map(|i| ProviderAvailableCommand {
                name: format!("cmd{i}"),
                description: String::new(),
                input_hint: None,
            })
            .collect();
        assert!(matches!(
            ProviderEvent::AvailableCommandsUpdated { commands: too_many }.validate_ingress(),
            Err(ProviderEventValidationError::TooManyAvailableCommands {
                max: PROVIDER_AVAILABLE_COMMANDS_MAX,
                ..
            })
        ));
    }

    #[test]
    fn provider_mode_changed_requires_a_mode_id() {
        assert_eq!(
            ProviderEvent::ModeChanged {
                mode_id: "architect".to_owned(),
                available: Vec::new(),
            }
            .validate_ingress(),
            Ok(())
        );
        assert!(matches!(
            ProviderEvent::ModeChanged { mode_id: String::new(), available: Vec::new() }
                .validate_ingress(),
            Err(ProviderEventValidationError::Empty { field: "mode id" })
        ));
    }

    #[test]
    fn provider_mode_changed_available_catalog_is_bounded_at_ingress() {
        let valid = ProviderEvent::ModeChanged {
            mode_id: "architect".to_owned(),
            available: vec![ProviderModeInfo {
                id: "architect".to_owned(),
                name: "Architect".to_owned(),
                description: Some("Plans before it edits".to_owned()),
            }],
        };
        assert_eq!(valid.validate_ingress(), Ok(()));

        assert!(matches!(
            ProviderEvent::ModeChanged {
                mode_id: "architect".to_owned(),
                available: vec![ProviderModeInfo {
                    id: String::new(),
                    name: "Architect".to_owned(),
                    description: None,
                }],
            }
            .validate_ingress(),
            Err(ProviderEventValidationError::Empty { field: "available mode id" })
        ));

        let too_many = (0..=PROVIDER_MODE_CATALOG_MAX)
            .map(|i| ProviderModeInfo {
                id: format!("mode{i}"),
                name: format!("Mode {i}"),
                description: None,
            })
            .collect();
        assert!(matches!(
            ProviderEvent::ModeChanged {
                mode_id: "architect".to_owned(),
                available: too_many,
            }
            .validate_ingress(),
            Err(ProviderEventValidationError::TooManyModes {
                max: PROVIDER_MODE_CATALOG_MAX,
                ..
            })
        ));
    }

    #[test]
    fn provider_session_info_updated_allows_no_title_and_rejects_control_bytes() {
        assert_eq!(
            ProviderEvent::SessionInfoUpdated { title: None }.validate_ingress(),
            Ok(())
        );
        assert!(matches!(
            ProviderEvent::SessionInfoUpdated {
                title: Some("bad\u{0000}title".to_owned()),
            }
            .validate_ingress(),
            Err(ProviderEventValidationError::InvalidField { field: "session title", .. })
        ));
    }

    #[test]
    fn provider_usage_updated_accepts_optional_cost_and_bounds_currency() {
        let valid = ProviderEvent::UsageUpdated {
            used_tokens: Some(100),
            context_window: Some(200_000),
            cost_amount: Some("0.42".to_owned()),
            cost_currency: Some("USD".to_owned()),
        };
        assert_eq!(valid.validate_ingress(), Ok(()));

        let no_cost = ProviderEvent::UsageUpdated {
            used_tokens: None,
            context_window: None,
            cost_amount: None,
            cost_currency: None,
        };
        assert_eq!(no_cost.validate_ingress(), Ok(()));

        assert!(matches!(
            ProviderEvent::UsageUpdated {
                used_tokens: None,
                context_window: None,
                cost_amount: None,
                cost_currency: Some("bad\ncurrency".to_owned()),
            }
            .validate_ingress(),
            Err(ProviderEventValidationError::InvalidField { field: "usage cost currency", .. })
        ));
    }

    #[test]
    fn provider_config_options_updated_is_typed_and_bounded_at_ingress() {
        let valid = ProviderEvent::ConfigOptionsUpdated {
            options: vec![ProviderConfigOption {
                id: "model".to_owned(),
                name: "Model".to_owned(),
                description: Some("Which model to use".to_owned()),
                category: Some("generation".to_owned()),
                kind: ProviderConfigOptionKind::Select,
                value_json: "\"opus\"".to_owned(),
                choices: vec![ProviderConfigChoice {
                    value_json: "\"opus\"".to_owned(),
                    label: Some("Opus".to_owned()),
                }],
            }],
        };
        assert_eq!(valid.validate_ingress(), Ok(()));

        assert!(matches!(
            ProviderEvent::ConfigOptionsUpdated {
                options: vec![ProviderConfigOption {
                    id: String::new(),
                    name: "Model".to_owned(),
                    description: None,
                    category: None,
                    kind: ProviderConfigOptionKind::Boolean,
                    value_json: "true".to_owned(),
                    choices: Vec::new(),
                }],
            }
            .validate_ingress(),
            Err(ProviderEventValidationError::Empty { field: "config option id" })
        ));
    }
}
