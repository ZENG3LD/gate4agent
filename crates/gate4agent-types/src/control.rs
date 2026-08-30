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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StartRequest {
    pub working_directory: String,
    pub terminal_size: TerminalSize,
    #[serde(default)]
    pub initial_prompt: Option<String>,
    #[serde(default)]
    pub session_options: Option<SessionOptionSelection>,
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

    pub fn validate(self) -> Result<(), ProviderRuntimePolicyError> {
        if (self.semantic_readiness
            || self.structured_prompt
            || self.provider_session_identity
            || self.semantic_resume
            || self.hook_semantics)
            && !self.raw_pty_lifecycle
        {
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
    #[error("semantic provider capabilities require the raw PTY lifecycle")]
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
    /// True only for `Ready`. The one predicate a blind/automated writer
    /// should consult; written as a method so no call site open-codes
    /// `matches!(.., Ready)` and quietly gets the `Unknown` case wrong.
    pub fn admits_blind_write(&self) -> bool {
        matches!(self, Self::Ready)
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
        limit_type: String,
        resets_at: Option<String>,
        usage_percent: Option<String>,
        raw_message: String,
    },
    /// The agent sent a JSON-RPC request to the ACP host -- `session/
    /// request_permission`, `fs/read_text_file`, `terminal/create`,
    /// `terminal/write` -- and the host's fixed, fail-closed policy
    /// (`DefaultAcpHandler`) has already decided on it by the time this
    /// event exists. This event does not change what the host does; it
    /// exists purely so an operator sees the request AND the decision
    /// instead of the request silently disappearing into a refusal nobody
    /// downstream ever hears about. `granted` is read off the host's own
    /// `Result` for the call, not re-derived here -- see the source
    /// (`gate4agent`'s `AgentEvent::RpcIncomingRequest::granted`) for
    /// exactly how.
    HostRequestObserved {
        method: String,
        params_json: String,
        granted: bool,
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
                limit_type,
                resets_at,
                usage_percent,
                raw_message,
            } => {
                validate_required("limit type", limit_type, PROVIDER_EVENT_ID_MAX_BYTES)?;
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
        ForegroundProcessKind, ForegroundSnapshot, HistorySnapshot, OperatorGateInput,
        OperatorGateKind, OperatorGateOption, OperatorGateOptionSemantics, OperatorGateState,
        OperatorGateSubject, ProviderEvent,
        ProviderEventValidationError,
        ProviderInteractionKind, ProviderInteractionOutcome, ProviderInteractionResponse,
        ProviderInteractionResponseError, ProviderRuntimeCapability, ProviderRuntimePolicy,
        ProviderRuntimePolicyError,
        ProviderSessionIdentity, ProviderSessionKey, ProviderSnapshot, PtyScreenState,
        ResumeSnapshot, SessionGeneration, SessionSnapshot, SessionStatus, TerminalFrame,
        TerminalMouseProtocolEncoding, TransportKind,
        FOREGROUND_PROCESS_NAME_MAX_BYTES, OPERATOR_GATE_OPTIONS_MAX,
        OPERATOR_GATE_OPTION_TEXT_MAX_BYTES, OPERATOR_GATE_PATH_MAX_BYTES,
        PROVIDER_EVENT_ID_MAX_BYTES, PROVIDER_EVENT_TEXT_MAX_BYTES,
        PROVIDER_INTERACTION_RESPONSE_MAX_BYTES,
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

        assert_eq!(
            ProviderRuntimePolicy::new(false, true, false, false, false, false),
            Err(ProviderRuntimePolicyError::SemanticCapabilityRequiresRawPty),
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

    #[test]
    fn admits_blind_write_is_true_only_for_ready() {
        assert!(!PtyScreenState::Unknown.admits_blind_write());
        assert!(!PtyScreenState::NotAgent {
            observed_process: "npm".to_owned(),
        }
        .admits_blind_write());
        assert!(!PtyScreenState::OperatorGate { gate: sample_gate() }.admits_blind_write());
        assert!(!PtyScreenState::Failing {
            reason: "startup-crash".to_owned(),
        }
        .admits_blind_write());
        assert!(PtyScreenState::Ready.admits_blind_write());
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
            granted: false,
        };
        assert_eq!(valid.validate_ingress(), Ok(()));

        assert!(matches!(
            ProviderEvent::HostRequestObserved {
                method: String::new(),
                params_json: String::new(),
                granted: false,
            }
            .validate_ingress(),
            Err(ProviderEventValidationError::Empty { field: "host request method" })
        ));

        let oversized_method = "m".repeat(PROVIDER_EVENT_ID_MAX_BYTES + 1);
        assert!(matches!(
            ProviderEvent::HostRequestObserved {
                method: oversized_method,
                params_json: String::new(),
                granted: true,
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
                granted: true,
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
}
