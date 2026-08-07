use super::{is_agent_foreground_wrapper, is_expected_agent_process, AgentSpec, RuntimePlatform};
use serde::{Deserialize, Serialize};
pub use gate4agent_types::DraftReadySignal;

const DECSET_BRACKETED_PASTE: &[u8] = b"\x1b[?2004h";
const DECTCEM_SHOW_CURSOR: &[u8] = b"\x1b[?25h";
const DECTCEM_HIDE_CURSOR: &[u8] = b"\x1b[?25l";
const ED_CLEAR_SCREEN: &[u8] = b"\x1b[2J";
const CLAUDE_COMPOSER_PROMPT: &[u8] = "❯".as_bytes();
const CODEX_COMPOSER_PROMPT: &[u8] = "›".as_bytes();
const CURRENT_CODEX_COMPOSER_PROMPT: &[u8] = "❯".as_bytes();
const SCANNER_TAIL_BYTES: usize = 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadinessIntent {
    FollowupPrompt,
    DraftPaste,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ForegroundObservation {
    pub process_name: Option<String>,
    pub has_child_processes: bool,
    pub is_shell: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadyReason {
    TitleIdle,
    ForegroundMatch,
    WrapperWithChild,
    DraftSignal,
    DraftQuiet,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadinessStatus {
    Waiting,
    Ready(ReadyReason),
    TimedOut,
}

/// Opaque proof that a readiness tracker reached a positive terminal state.
#[derive(Debug, Eq, PartialEq)]
pub struct ReadinessPermit {
    agent_id: super::AgentId,
    intent: ReadinessIntent,
    reason: ReadyReason,
}

impl ReadinessPermit {
    pub fn agent_id(&self) -> &super::AgentId {
        &self.agent_id
    }

    pub fn intent(&self) -> ReadinessIntent {
        self.intent
    }

    pub fn reason(&self) -> ReadyReason {
        self.reason
    }
}

/// Pure readiness state machine. Callers provide elapsed monotonic milliseconds,
/// foreground observations, title evidence, and raw PTY output.
pub struct ReadinessTracker<'a> {
    spec: &'a AgentSpec,
    platform: RuntimePlatform,
    intent: ReadinessIntent,
    status: ReadinessStatus,
    poll_count: u32,
    foreground_reason: Option<ReadyReason>,
    terminal_reason: Option<ReadyReason>,
    scanner: DraftReadyScanner,
}

impl<'a> ReadinessTracker<'a> {
    pub fn new(spec: &'a AgentSpec, platform: RuntimePlatform, intent: ReadinessIntent) -> Self {
        Self {
            spec,
            platform,
            intent,
            status: ReadinessStatus::Waiting,
            poll_count: 0,
            foreground_reason: None,
            terminal_reason: None,
            scanner: DraftReadyScanner::new(
                spec.id.as_str(),
                spec.readiness.draft_signal,
                spec.readiness.draft_quiet_ms,
            ),
        }
    }

    pub fn status(&self) -> ReadinessStatus {
        self.status
    }

    /// Consume this tracker and issue one agent/intent-bound permit only after
    /// the state machine is ready.
    pub fn into_permit(self) -> Option<ReadinessPermit> {
        match self.status {
            ReadinessStatus::Ready(reason) => Some(ReadinessPermit {
                agent_id: self.spec.id.clone(),
                intent: self.intent,
                reason,
            }),
            ReadinessStatus::Waiting | ReadinessStatus::TimedOut => None,
        }
    }

    pub fn observe_title_idle(&mut self, elapsed_ms: u64) -> ReadinessStatus {
        if self.spec.readiness.allow_title_idle {
            self.foreground_reason = Some(ReadyReason::TitleIdle);
        }
        self.evaluate(elapsed_ms)
    }

    pub fn observe_foreground(
        &mut self,
        observation: &ForegroundObservation,
        elapsed_ms: u64,
    ) -> ReadinessStatus {
        self.poll_count = self.poll_count.saturating_add(1);
        if let Some(process_name) = observation.process_name.as_deref() {
            if is_expected_agent_process(self.spec, process_name, self.platform) {
                self.foreground_reason = Some(ReadyReason::ForegroundMatch);
            } else if self
                .spec
                .readiness
                .wrapper_child_fallback_after_polls
                .is_some_and(|minimum| self.poll_count >= minimum)
                && observation.has_child_processes
                && !observation.is_shell
                && is_agent_foreground_wrapper(process_name, self.platform)
            {
                self.foreground_reason = Some(ReadyReason::WrapperWithChild);
            }
        }
        self.evaluate(elapsed_ms)
    }

    pub fn observe_output(&mut self, data: &[u8], elapsed_ms: u64) -> ReadinessStatus {
        if let Some(reason) = self.scanner.observe(data, elapsed_ms) {
            self.terminal_reason = Some(reason);
        }
        self.evaluate(elapsed_ms)
    }

    pub fn poll(&mut self, elapsed_ms: u64) -> ReadinessStatus {
        if let Some(reason) = self.scanner.poll(elapsed_ms) {
            self.terminal_reason = Some(reason);
        }
        self.evaluate(elapsed_ms)
    }

    fn evaluate(&mut self, elapsed_ms: u64) -> ReadinessStatus {
        if !matches!(self.status, ReadinessStatus::Waiting) {
            return self.status;
        }
        if let Some(reason) = self.scanner.poll(elapsed_ms) {
            self.terminal_reason = Some(reason);
        }
        let ready = match self.intent {
            ReadinessIntent::FollowupPrompt => {
                if self.foreground_reason.is_some() {
                    if self.spec.readiness.followup_requires_terminal {
                        self.terminal_reason
                    } else {
                        self.foreground_reason
                    }
                } else {
                    None
                }
            }
            ReadinessIntent::DraftPaste => {
                if self.foreground_reason.is_some() {
                    self.terminal_reason
                } else {
                    None
                }
            }
        };
        if let Some(reason) = ready {
            self.status = ReadinessStatus::Ready(reason);
        } else if elapsed_ms >= self.spec.readiness.timeout_ms {
            self.status = ReadinessStatus::TimedOut;
        }
        self.status
    }
}

struct DraftReadyScanner {
    bootstrap_settle: BootstrapSettle,
    signal: DraftReadySignal,
    quiet_ms: u64,
    recent: Vec<u8>,
    post_handshake_recent: Vec<u8>,
    saw_bracketed_paste: bool,
    saw_claude_composer: bool,
    saw_cursor_show: bool,
    saw_cursor_hide: bool,
    saw_clear_screen: bool,
    quiet_deadline_ms: Option<u64>,
    ready: Option<ReadyReason>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BootstrapSettle {
    None,
    RestartOnOutput,
    FixedFromHandshake,
}

impl DraftReadyScanner {
    fn new(agent_id: &str, signal: DraftReadySignal, quiet_ms: u64) -> Self {
        let bootstrap_settle = match (agent_id, signal) {
            ("claude", DraftReadySignal::CursorAfterBracketedPaste) => {
                BootstrapSettle::RestartOnOutput
            }
            ("codex", DraftReadySignal::CodexComposerPrompt) => {
                BootstrapSettle::FixedFromHandshake
            }
            _ => BootstrapSettle::None,
        };
        Self {
            bootstrap_settle,
            signal,
            quiet_ms,
            recent: Vec::new(),
            post_handshake_recent: Vec::new(),
            saw_bracketed_paste: false,
            saw_claude_composer: false,
            saw_cursor_show: false,
            saw_cursor_hide: false,
            saw_clear_screen: false,
            quiet_deadline_ms: None,
            ready: None,
        }
    }

    fn observe(&mut self, data: &[u8], elapsed_ms: u64) -> Option<ReadyReason> {
        if self.ready.is_some() {
            return self.ready;
        }
        let combined = concat_tail(&self.recent, data);
        self.recent = tail(&combined, SCANNER_TAIL_BYTES);
        self.saw_cursor_show |= find_subslice(&combined, DECTCEM_SHOW_CURSOR).is_some();
        self.saw_cursor_hide |= find_subslice(&combined, DECTCEM_HIDE_CURSOR).is_some();
        self.saw_clear_screen |= find_subslice(&combined, ED_CLEAR_SCREEN).is_some();
        if self.signal == DraftReadySignal::ClaudeComposerPrompt
            && find_subslice(&combined, CLAUDE_COMPOSER_PROMPT).is_some()
        {
            self.saw_claude_composer = true;
        }
        if !self.saw_bracketed_paste {
            let marker_index = find_subslice(&combined, DECSET_BRACKETED_PASTE)?;
            self.saw_bracketed_paste = true;
            if self.signal == DraftReadySignal::BracketedPaste {
                self.ready = Some(ReadyReason::DraftSignal);
                return self.ready;
            }
            if self.signal == DraftReadySignal::ClaudeComposerPrompt
                && self.saw_claude_composer
            {
                self.ready = Some(ReadyReason::DraftSignal);
                return self.ready;
            }
            let post_handshake = &combined[marker_index + DECSET_BRACKETED_PASTE.len()..];
            if self.marker_seen(post_handshake) {
                self.ready = Some(ReadyReason::DraftSignal);
                return self.ready;
            }
            self.post_handshake_recent = tail(post_handshake, SCANNER_TAIL_BYTES);
        } else {
            if self.signal == DraftReadySignal::ClaudeComposerPrompt
                && self.saw_claude_composer
            {
                self.ready = Some(ReadyReason::DraftSignal);
                return self.ready;
            }
            let post_combined = concat_tail(&self.post_handshake_recent, data);
            if self.marker_seen(&post_combined) {
                self.ready = Some(ReadyReason::DraftSignal);
                return self.ready;
            }
            self.post_handshake_recent = tail(&post_combined, SCANNER_TAIL_BYTES);
        }

        if self.signal == DraftReadySignal::QuietAfterBracketedPaste {
            self.quiet_deadline_ms = Some(elapsed_ms.saturating_add(self.quiet_ms));
        }
        if self.vendor_bootstrap_complete() {
            match self.bootstrap_settle {
                BootstrapSettle::RestartOnOutput => {
                    self.quiet_deadline_ms = Some(elapsed_ms.saturating_add(self.quiet_ms));
                }
                BootstrapSettle::FixedFromHandshake => {
                    self.quiet_deadline_ms
                        .get_or_insert(elapsed_ms.saturating_add(self.quiet_ms));
                }
                BootstrapSettle::None => {}
            }
        }
        None
    }

    fn poll(&mut self, elapsed_ms: u64) -> Option<ReadyReason> {
        if self.ready.is_none()
            && self.saw_bracketed_paste
            && (self.signal == DraftReadySignal::QuietAfterBracketedPaste
                || self.vendor_bootstrap_complete())
            && self
                .quiet_deadline_ms
                .is_some_and(|deadline| elapsed_ms >= deadline)
        {
            self.ready = Some(ReadyReason::DraftQuiet);
        }
        self.ready
    }

    fn marker_seen(&self, bytes: &[u8]) -> bool {
        match self.signal {
            DraftReadySignal::BracketedPaste => return true,
            DraftReadySignal::ClaudeComposerPrompt => {
                find_subslice(bytes, CLAUDE_COMPOSER_PROMPT).is_some()
            }
            DraftReadySignal::QuietAfterBracketedPaste => return false,
            DraftReadySignal::CodexComposerPrompt => {
                find_subslice(bytes, CODEX_COMPOSER_PROMPT).is_some()
                    || find_subslice(bytes, CURRENT_CODEX_COMPOSER_PROMPT).is_some()
            }
            DraftReadySignal::CursorAfterBracketedPaste => {
                find_subslice(bytes, DECTCEM_SHOW_CURSOR).is_some()
            }
        }
    }

    fn vendor_bootstrap_complete(&self) -> bool {
        self.bootstrap_settle != BootstrapSettle::None
            && self.saw_bracketed_paste
            && self.saw_cursor_show
            && self.saw_cursor_hide
            && self.saw_clear_screen
    }
}

fn concat_tail(previous: &[u8], data: &[u8]) -> Vec<u8> {
    let keep = previous.len().min(SCANNER_TAIL_BYTES);
    let mut combined = Vec::with_capacity(keep + data.len());
    combined.extend_from_slice(&previous[previous.len().saturating_sub(keep)..]);
    combined.extend_from_slice(data);
    combined
}

fn tail(bytes: &[u8], limit: usize) -> Vec<u8> {
    bytes[bytes.len().saturating_sub(limit)..].to_vec()
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::builtin_registry;

    fn tracker(agent: &str, intent: ReadinessIntent) -> ReadinessTracker<'static> {
        ReadinessTracker::new(
            builtin_registry().get_by_id(agent).unwrap(),
            RuntimePlatform::Linux,
            intent,
        )
    }

    #[test]
    fn followup_requires_foreground_and_terminal_readiness() {
        let mut tracker = tracker("kimi", ReadinessIntent::FollowupPrompt);
        assert_eq!(
            tracker.observe_output(b"\x1b[?2004h", 100),
            ReadinessStatus::Waiting
        );
        assert_eq!(
            tracker.observe_foreground(
                &ForegroundObservation {
                    process_name: Some("bash".to_owned()),
                    has_child_processes: false,
                    is_shell: true,
                },
                150,
            ),
            ReadinessStatus::Waiting
        );
        assert_eq!(
            tracker.observe_foreground(
                &ForegroundObservation {
                    process_name: Some("kimi".to_owned()),
                    has_child_processes: false,
                    is_shell: false,
                },
                300,
            ),
            ReadinessStatus::Ready(ReadyReason::DraftSignal)
        );
    }

    #[test]
    fn codex_marker_can_cross_chunks() {
        let mut tracker = tracker("codex", ReadinessIntent::DraftPaste);
        tracker.observe_foreground(
            &ForegroundObservation {
                process_name: Some("codex".to_owned()),
                has_child_processes: false,
                is_shell: false,
            },
            100,
        );
        assert_eq!(
            tracker.observe_output(b"\x1b[?20", 200),
            ReadinessStatus::Waiting
        );
        assert_eq!(
            tracker.observe_output(b"04h\xe2\x80", 250),
            ReadinessStatus::Waiting
        );
        assert_eq!(
            tracker.observe_output(b"\xba", 300),
            ReadinessStatus::Ready(ReadyReason::DraftSignal)
        );
    }

    #[test]
    fn quiet_policy_waits_after_the_last_render() {
        let mut spec = builtin_registry().get_by_id("kimi").unwrap().clone();
        spec.readiness.draft_signal = DraftReadySignal::QuietAfterBracketedPaste;
        let mut tracker = ReadinessTracker::new(
            &spec,
            RuntimePlatform::Linux,
            ReadinessIntent::DraftPaste,
        );
        tracker.observe_foreground(
            &ForegroundObservation {
                process_name: Some("kimi".to_owned()),
                has_child_processes: false,
                is_shell: false,
            },
            100,
        );
        tracker.observe_output(b"\x1b[?2004hbanner", 200);
        tracker.observe_output(b"more output", 1_000);
        assert_eq!(tracker.poll(2_000), ReadinessStatus::Waiting);
        assert_eq!(
            tracker.poll(2_500),
            ReadinessStatus::Ready(ReadyReason::DraftQuiet)
        );
    }

    #[test]
    fn marker_policy_does_not_fall_through_to_quiet() {
        let mut tracker = tracker("opencode", ReadinessIntent::DraftPaste);
        tracker.observe_foreground(
            &ForegroundObservation {
                process_name: Some("opencode".to_owned()),
                has_child_processes: false,
                is_shell: false,
            },
            100,
        );
        tracker.observe_output(b"\x1b[?2004h", 200);
        assert_eq!(tracker.poll(2_000), ReadinessStatus::Waiting);
        assert_eq!(
            tracker.observe_output(b"\x1b[?25h", 2_100),
            ReadinessStatus::Ready(ReadyReason::DraftSignal)
        );
    }

    #[test]
    fn title_idle_cannot_bypass_draft_terminal_handshake() {
        let mut tracker = tracker("codex", ReadinessIntent::DraftPaste);
        assert_eq!(tracker.observe_title_idle(100), ReadinessStatus::Waiting);
        assert!(tracker.into_permit().is_none());
    }

    #[test]
    fn permit_is_bound_to_agent_and_intent() {
        let mut tracker = tracker("kimi", ReadinessIntent::FollowupPrompt);
        tracker.observe_foreground(
            &ForegroundObservation {
                process_name: Some("kimi".to_owned()),
                has_child_processes: false,
                is_shell: false,
            },
            100,
        );
        tracker.observe_output(b"\x1b[?2004h", 200);
        let permit = tracker.into_permit().expect("ready permit");
        assert_eq!(permit.agent_id().as_str(), "kimi");
        assert_eq!(permit.intent(), ReadinessIntent::FollowupPrompt);
        assert_eq!(permit.reason(), ReadyReason::DraftSignal);
    }

    #[test]
    fn current_codex_composer_marker_can_cross_chunks() {
        let mut tracker = tracker("codex", ReadinessIntent::DraftPaste);
        tracker.observe_foreground(
            &ForegroundObservation {
                process_name: Some("codex".to_owned()),
                has_child_processes: false,
                is_shell: false,
            },
            100,
        );
        assert_eq!(
            tracker.observe_output(b"\x1b[?2004h\xe2\x9d", 200),
            ReadinessStatus::Waiting
        );
        assert_eq!(
            tracker.observe_output(b"\xaf", 250),
            ReadinessStatus::Ready(ReadyReason::DraftSignal)
        );
    }

    #[test]
    fn current_codex_bootstrap_settles_without_a_composer_glyph() {
        let mut tracker = tracker("codex", ReadinessIntent::DraftPaste);
        tracker.observe_foreground(
            &ForegroundObservation {
                process_name: Some("codex".to_owned()),
                has_child_processes: false,
                is_shell: false,
            },
            100,
        );
        assert_eq!(
            tracker.observe_output(b"\x1b[?25h\x1b[?25l\x1b[2J\x1b[?2004h", 200),
            ReadinessStatus::Waiting
        );
        assert_eq!(tracker.poll(1_699), ReadinessStatus::Waiting);
        assert_eq!(
            tracker.poll(1_700),
            ReadinessStatus::Ready(ReadyReason::DraftQuiet)
        );
    }

    #[test]
    fn codex_bootstrap_settle_deadline_survives_continuous_spinner_output() {
        let mut tracker = tracker("codex", ReadinessIntent::DraftPaste);
        tracker.observe_foreground(
            &ForegroundObservation {
                process_name: Some("codex".to_owned()),
                has_child_processes: false,
                is_shell: false,
            },
            100,
        );
        tracker.observe_output(b"\x1b[?25h\x1b[?25l\x1b[2J\x1b[?2004h", 200);
        assert_eq!(
            tracker.observe_output(b"spinner frame one", 1_000),
            ReadinessStatus::Waiting
        );
        assert_eq!(
            tracker.observe_output(b"spinner frame two", 1_699),
            ReadinessStatus::Waiting
        );
        assert_eq!(
            tracker.poll(1_700),
            ReadinessStatus::Ready(ReadyReason::DraftQuiet)
        );
    }

    #[test]
    fn codex_bootstrap_fallback_rejects_an_incomplete_terminal_handshake() {
        let mut tracker = tracker("codex", ReadinessIntent::DraftPaste);
        tracker.observe_foreground(
            &ForegroundObservation {
                process_name: Some("codex".to_owned()),
                has_child_processes: false,
                is_shell: false,
            },
            100,
        );
        tracker.observe_output(b"\x1b[?25h\x1b[?25l\x1b[?2004h", 200);
        assert_eq!(tracker.poll(1_700), ReadinessStatus::Waiting);
    }

    #[test]
    fn bracketed_paste_policy_is_ready_at_the_input_handshake() {
        let mut spec = builtin_registry().get_by_id("kimi").unwrap().clone();
        spec.readiness.draft_signal = DraftReadySignal::BracketedPaste;
        let mut tracker = ReadinessTracker::new(
            &spec,
            RuntimePlatform::Linux,
            ReadinessIntent::FollowupPrompt,
        );
        tracker.observe_foreground(
            &ForegroundObservation {
                process_name: Some("kimi".to_owned()),
                has_child_processes: false,
                is_shell: false,
            },
            100,
        );
        assert_eq!(
            tracker.observe_output(b"\x1b[?2004h", 200),
            ReadinessStatus::Ready(ReadyReason::DraftSignal)
        );
    }

    #[test]
    fn claude_policy_requires_cursor_visibility_after_bracketed_paste() {
        let mut tracker = tracker("claude", ReadinessIntent::FollowupPrompt);
        tracker.observe_foreground(
            &ForegroundObservation {
                process_name: Some("claude".to_owned()),
                has_child_processes: false,
                is_shell: false,
            },
            100,
        );
        assert_eq!(
            tracker.observe_output(b"\x1b[?25h", 150),
            ReadinessStatus::Waiting
        );
        assert_eq!(
            tracker.observe_output(b"\x1b[?2004h", 200),
            ReadinessStatus::Waiting
        );
        assert_eq!(
            tracker.observe_output(b"\x1b[?25h", 250),
            ReadinessStatus::Ready(ReadyReason::DraftSignal)
        );
    }

    #[test]
    fn current_claude_bootstrap_settles_without_a_post_handshake_cursor() {
        let mut tracker = tracker("claude", ReadinessIntent::FollowupPrompt);
        tracker.observe_foreground(
            &ForegroundObservation {
                process_name: Some("claude".to_owned()),
                has_child_processes: false,
                is_shell: false,
            },
            100,
        );
        assert_eq!(
            tracker.observe_output(b"\x1b[?25h\x1b[?25l\x1b[2J\x1b[?2004h", 200),
            ReadinessStatus::Waiting
        );
        assert_eq!(tracker.poll(1_699), ReadinessStatus::Waiting);
        assert_eq!(
            tracker.poll(1_700),
            ReadinessStatus::Ready(ReadyReason::DraftQuiet)
        );
    }

    #[test]
    fn claude_quiet_readiness_wins_when_foreground_arrives_at_timeout() {
        let mut tracker = tracker("claude", ReadinessIntent::FollowupPrompt);
        assert_eq!(
            tracker.observe_output(b"\x1b[?25h\x1b[?25l\x1b[2J\x1b[?2004h", 200),
            ReadinessStatus::Waiting
        );
        assert_eq!(
            tracker.observe_foreground(
                &ForegroundObservation {
                    process_name: Some("claude".to_owned()),
                    has_child_processes: false,
                    is_shell: false,
                },
                5_000,
            ),
            ReadinessStatus::Ready(ReadyReason::DraftQuiet)
        );
    }

    #[test]
    fn claude_bootstrap_quiet_window_restarts_on_late_output() {
        let mut tracker = tracker("claude", ReadinessIntent::FollowupPrompt);
        tracker.observe_foreground(
            &ForegroundObservation {
                process_name: Some("claude".to_owned()),
                has_child_processes: false,
                is_shell: false,
            },
            100,
        );
        tracker.observe_output(b"\x1b[?25h\x1b[?25l\x1b[2J\x1b[?2004h", 200);
        assert_eq!(
            tracker.observe_output(b"late onboarding render", 1_000),
            ReadinessStatus::Waiting
        );
        assert_eq!(tracker.poll(1_700), ReadinessStatus::Waiting);
        assert_eq!(
            tracker.poll(2_500),
            ReadinessStatus::Ready(ReadyReason::DraftQuiet)
        );
    }

    #[test]
    fn unverified_followup_keeps_the_foreground_only_contract() {
        let mut tracker = tracker("opencode", ReadinessIntent::FollowupPrompt);
        assert_eq!(
            tracker.observe_foreground(
                &ForegroundObservation {
                    process_name: Some("opencode".to_owned()),
                    has_child_processes: false,
                    is_shell: false,
                },
                100,
            ),
            ReadinessStatus::Ready(ReadyReason::ForegroundMatch)
        );
    }
}
