use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DraftReadySignal {
    BracketedPaste,
    ClaudeComposerPrompt,
    QuietAfterBracketedPaste,
    CodexComposerPrompt,
    CursorAfterBracketedPaste,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentReadinessSpec {
    pub timeout_ms: u64,
    pub poll_interval_ms: u64,
    pub wrapper_child_fallback_after_polls: Option<u32>,
    pub allow_title_idle: bool,
    /// Require both foreground ownership and the configured terminal signal
    /// before submitting a follow-up prompt.
    #[serde(default)]
    pub followup_requires_terminal: bool,
    pub draft_signal: DraftReadySignal,
    pub draft_quiet_ms: u64,
}

impl Default for AgentReadinessSpec {
    fn default() -> Self {
        Self {
            timeout_ms: 5_000,
            poll_interval_ms: 150,
            wrapper_child_fallback_after_polls: Some(4),
            allow_title_idle: true,
            followup_requires_terminal: false,
            draft_signal: DraftReadySignal::QuietAfterBracketedPaste,
            draft_quiet_ms: 1_500,
        }
    }
}
