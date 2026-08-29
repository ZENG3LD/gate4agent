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
}

impl ProviderRuntimePolicy {
    pub fn new(
        raw_pty_lifecycle: bool,
        semantic_readiness: bool,
        structured_prompt: bool,
        provider_session_identity: bool,
        semantic_resume: bool,
    ) -> Result<Self, ProviderRuntimePolicyError> {
        let policy = Self {
            raw_pty_lifecycle,
            semantic_readiness,
            structured_prompt,
            provider_session_identity,
            semantic_resume,
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
        }
    }

    pub fn validate(self) -> Result<(), ProviderRuntimePolicyError> {
        if (self.semantic_readiness
            || self.structured_prompt
            || self.provider_session_identity
            || self.semantic_resume)
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
        }

        let wire = WirePolicy::deserialize(deserializer)?;
        Self::new(
            wire.raw_pty_lifecycle,
            wire.semantic_readiness,
            wire.structured_prompt,
            wire.provider_session_identity,
            wire.semantic_resume,
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
}

pub const FOREGROUND_PROCESS_NAME_MAX_BYTES: usize = 512;
pub const PTY_SCREEN_GATE_NAME_MAX_BYTES: usize = 128;

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
    /// rather than dispatching another agent turn.
    OperatorGate { gate: String },
    /// The foreground process matches, but the screen shows the agent came
    /// up wrong or fell over -- a crash/stack trace, an expired or rejected
    /// login, a fatal startup error. Kept distinct from `OperatorGate` on
    /// purpose: a gate is a screen a human resolves BY typing into it, while
    /// nothing typed into this screen fixes it. Collapsing the two would
    /// lose exactly the diagnosis an operator needs -- "waiting for you"
    /// versus "broken". For write-gating it behaves like every other
    /// non-`Ready` state (refused); the split buys a correct label, not
    /// different gating. `reason` is a short classifier label, the same
    /// shape as `OperatorGate::gate`, never raw terminal text -- nothing in
    /// this enum carries screen contents. Where a provider has a
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
            Self::OperatorGate { gate } | Self::Failing { reason: gate } => {
                !gate.trim().is_empty()
                    && gate.len() <= PTY_SCREEN_GATE_NAME_MAX_BYTES
                    && !gate.chars().any(char::is_control)
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
        ForegroundProcessKind, ForegroundSnapshot, HistorySnapshot, ProviderEvent,
        ProviderEventValidationError,
        ProviderInteractionKind, ProviderInteractionOutcome, ProviderInteractionResponse,
        ProviderInteractionResponseError, ProviderRuntimeCapability, ProviderRuntimePolicy,
        ProviderRuntimePolicyError,
        ProviderSessionIdentity, ProviderSessionKey, ProviderSnapshot, PtyScreenState,
        ResumeSnapshot, SessionGeneration, SessionSnapshot, SessionStatus, TerminalFrame,
        TerminalMouseProtocolEncoding, TransportKind,
        FOREGROUND_PROCESS_NAME_MAX_BYTES, PROVIDER_INTERACTION_RESPONSE_MAX_BYTES,
        PTY_SCREEN_GATE_NAME_MAX_BYTES,
    };

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
        assert_eq!(raw.validate(), Ok(()));

        assert_eq!(
            ProviderRuntimePolicy::new(false, true, false, false, false),
            Err(ProviderRuntimePolicyError::SemanticCapabilityRequiresRawPty),
        );
        assert_eq!(
            ProviderRuntimePolicy::new(true, false, true, false, false),
            Err(ProviderRuntimePolicyError::StructuredPromptRequiresReadiness),
        );
        assert_eq!(
            ProviderRuntimePolicy::new(true, true, true, false, true),
            Err(ProviderRuntimePolicyError::ResumeRequiresSessionIdentity),
        );
        assert!(ProviderRuntimePolicy::new(true, true, true, true, true).is_ok());
    }

    #[test]
    fn provider_runtime_policy_serde_requires_every_field_and_revalidates() {
        let raw = ProviderRuntimePolicy::raw_pty();
        let encoded = serde_json::to_string(&raw).unwrap();
        assert_eq!(
            encoded,
            r#"{"raw_pty_lifecycle":true,"semantic_readiness":false,"structured_prompt":false,"provider_session_identity":false,"semantic_resume":false}"#,
        );
        assert_eq!(
            serde_json::from_str::<ProviderRuntimePolicy>(&encoded).unwrap(),
            raw,
        );
        assert!(serde_json::from_str::<ProviderRuntimePolicy>(
            r#"{"raw_pty_lifecycle":true,"semantic_readiness":false,"structured_prompt":false,"provider_session_identity":false}"#,
        )
        .is_err());
        assert!(serde_json::from_str::<ProviderRuntimePolicy>(
            r#"{"raw_pty_lifecycle":true,"semantic_readiness":false,"structured_prompt":true,"provider_session_identity":false,"semantic_resume":false}"#,
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
            PtyScreenState::OperatorGate {
                gate: "workspace-trust".to_owned(),
            },
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
        assert!(!PtyScreenState::OperatorGate {
            gate: "workspace-trust".to_owned(),
        }
        .admits_blind_write());
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

        assert!(PtyScreenState::OperatorGate {
            gate: "workspace-trust".to_owned(),
        }
        .is_valid());
        assert!(!PtyScreenState::OperatorGate {
            gate: "x".repeat(PTY_SCREEN_GATE_NAME_MAX_BYTES + 1),
        }
        .is_valid());
        assert!(!PtyScreenState::OperatorGate {
            gate: String::new(),
        }
        .is_valid());
        assert!(!PtyScreenState::OperatorGate {
            gate: "bad\u{0000}gate".to_owned(),
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
}
