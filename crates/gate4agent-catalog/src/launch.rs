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
        /// vendor documentation where one exists.
        args: Vec<String>,
        /// Whether that vendor mode's own documented behaviour has the CLI
        /// raise a permission question on the wire at all -- never inferred
        /// from the level's name.
        asks_for_permission: bool,
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
/// - `claude` and `codex` have a verified flag and verified `asks_for_permission`
///   for all three non-`Unmanaged` levels.
/// - `grok` (xAI Grok Build) has three documented modes -- Ask (default,
///   prompts for anything not pre-allowed), Auto (classifier auto-approves
///   safer tools, dangerous ones still prompt), Always-approve (skips
///   prompts; deny rules and hooks still apply) -- but no documented
///   read-only mode. `FullAuto`'s flag string is inherited, UNCONFIRMED
///   against Grok Build's own CLI surface (its *mode semantics* --
///   "skips prompts" -- are confirmed from the vendor user guide, only the
///   exact flag spelling is not); `Moderate` has no confirmed flag for
///   "Auto" and falls back to the vendor's own interactive default rather
///   than inventing one; `ReadOnly` is `Unsupported` -- refused, not
///   silently downgraded to "Ask".
/// - `kimi` (Kimi Code) confirms `--yolo` as a real flag (from its own
///   release notes: rejected only when combined with `--prompt`, i.e. it is
///   otherwise accepted); its "skip everything" semantics are inferred from
///   the universal industry meaning of "yolo mode" rather than read directly
///   from a Kimi Code permissions doc, so treat that inference as
///   HIGH-CONFIDENCE but UNCONFIRMED-at-the-source. Kimi's release notes
///   also name `--auto` and `--plan` flags, but only as flags rejected
///   together with `--prompt` -- there is no confirmation either applies to
///   (or has the same meaning under) the `kimi acp` subcommand this project
///   actually spawns, so neither is wired in here; `Moderate` and `ReadOnly`
///   both need live confirmation before either gets a flag, and `ReadOnly`
///   is `Unsupported` in the meantime rather than guessed.
/// - `Unmanaged` always resolves `Supported` with no flag and
///   `asks_for_permission: true`, for every agent ID including one this
///   catalog does not recognize: it imposes nothing by definition, so it can
///   never be refused, and "impose nothing" means the vendor's own default
///   is in control and cannot be predicted -- assume it can ask.
/// - An agent ID this catalog does not recognize resolves `Unsupported` at
///   every other level: this function never guesses at an unknown
///   provider's flag surface, and refusing is honest where the old
///   behaviour (silently launching with no flag at all) was not.
pub fn approval_level_resolution(agent_id: &AgentId, level: ApprovalLevel) -> ApprovalLevelResolution {
    use ApprovalLevelResolution::{Supported, Unsupported};
    match (agent_id.as_str(), level) {
        // claude -- code.claude.com/docs/en/cli-reference `--permission-mode`.
        ("claude", ApprovalLevel::FullAuto) => Supported {
            args: vec!["--permission-mode".to_owned(), "bypassPermissions".to_owned()],
            // bypassPermissions: every tool call is auto-approved. Never asks.
            asks_for_permission: false,
        },
        ("claude", ApprovalLevel::Moderate) => Supported {
            args: vec!["--permission-mode".to_owned(), "acceptEdits".to_owned()],
            // acceptEdits: file edits are auto-approved, everything else
            // (Bash, other tools) still raises a permission question.
            asks_for_permission: true,
        },
        ("claude", ApprovalLevel::ReadOnly) => Supported {
            args: vec!["--permission-mode".to_owned(), "plan".to_owned()],
            // Plan mode refuses edits and mutating commands outright rather
            // than asking about them -- a restriction, not a question, which
            // is what this level's name promises. `default` (the prior
            // flag here) was claude's own interactive mode and asked about
            // everything; this row is the fix for that drift.
            asks_for_permission: false,
        },

        // codex -- OpenAI Codex CLI `--sandbox` / `--ask-for-approval`.
        ("codex", ApprovalLevel::FullAuto) => Supported {
            args: vec!["--dangerously-bypass-approvals-and-sandbox".to_owned()],
            // Bypasses approvals AND the sandbox entirely. Never asks.
            asks_for_permission: false,
        },
        ("codex", ApprovalLevel::Moderate) => Supported {
            args: vec![
                "--sandbox".to_owned(),
                "workspace-write".to_owned(),
                "--ask-for-approval".to_owned(),
                "on-request".to_owned(),
            ],
            // on-request: Codex decides when to ask -- sandboxed writes
            // proceed unasked, escalation (e.g. a command outside the
            // sandbox) raises a permission question.
            asks_for_permission: true,
        },
        ("codex", ApprovalLevel::ReadOnly) => Supported {
            args: vec![
                "--sandbox".to_owned(),
                "read-only".to_owned(),
                "--ask-for-approval".to_owned(),
                "never".to_owned(),
            ],
            // never: Codex never asks. A mutating action fails against the
            // read-only sandbox instead of prompting -- a restriction, not
            // a question, and this row already had that right.
            asks_for_permission: false,
        },

        // grok (xAI Grok Build) -- see this function's own doc comment for
        // the three documented modes and per-row confidence.
        ("grok", ApprovalLevel::FullAuto) => Supported {
            args: vec!["--permission-mode".to_owned(), "bypassPermissions".to_owned()],
            // Mapped to vendor-documented "Always-approve": "skips prompts;
            // explicit deny rules and PreToolUse hooks still apply." Mode
            // semantics confirmed; this exact flag spelling is not --
            // UNCONFIRMED, needs live confirmation.
            asks_for_permission: false,
        },
        ("grok", ApprovalLevel::Moderate) => Supported {
            args: Vec::new(),
            // No confirmed flag for vendor-documented "Auto" (classifier
            // auto-approves safer tools, dangerous ones still prompt) --
            // falls back to no flag rather than inventing one, same as
            // `Unmanaged`. The vendor's own default with no flag is "Ask",
            // which does ask -- UNCONFIRMED whether it is reachable any
            // other way, needs live confirmation.
            asks_for_permission: true,
        },
        // No documented read-only mode exists for Grok Build at all (Ask /
        // Auto / Always-approve, none of them a restriction) -- refuse by
        // name rather than silently substitute "Ask", which would launch at
        // a wider authority than requested.
        ("grok", ApprovalLevel::ReadOnly) => Unsupported,

        // kimi (Kimi Code) -- see this function's own doc comment.
        ("kimi", ApprovalLevel::FullAuto) => Supported {
            args: vec!["--yolo".to_owned()],
            // Flag confirmed to exist (Kimi Code release notes). "Skips
            // everything" semantics inferred from the universal industry
            // meaning of "yolo mode", not read directly from a Kimi
            // permissions doc -- UNCONFIRMED at the source, high confidence.
            asks_for_permission: false,
        },
        ("kimi", ApprovalLevel::Moderate) => Supported {
            args: Vec::new(),
            // `--auto` is named in Kimi's own release notes but only as a
            // flag rejected together with `--prompt`; whether it applies to
            // (or means the same thing under) the `kimi acp` subcommand this
            // project spawns is UNCONFIRMED, so it is not wired in here --
            // falls back to no flag, same as `Unmanaged`, needs live
            // confirmation.
            asks_for_permission: true,
        },
        // Symmetric to grok: `--plan` is named in Kimi's release notes but
        // its behaviour under `kimi acp` is UNCONFIRMED -- refuse by name
        // rather than guess that it is this project's read-only mode.
        ("kimi", ApprovalLevel::ReadOnly) => Unsupported,

        // Impose nothing, for every agent ID: never refused, and the
        // vendor's own default is unpredictable by definition -- assume it
        // can ask.
        (_, ApprovalLevel::Unmanaged) => Supported {
            args: Vec::new(),
            asks_for_permission: true,
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
/// argv-only accessor over [`approval_level_resolution`], kept because
/// `gate4agent`'s `src/acp/spawn.rs` forwards this `Vec<String>` straight
/// into a provider's own argv and only needs the flags, not the rest of the
/// resolution. Where a provider has no verified flag for a level --
/// including a level `approval_level_resolution` refuses outright -- this
/// returns an empty `Vec`, the same as `ApprovalLevel::Unmanaged`, rather
/// than inventing one.
///
/// `plan_launch` (the PTY launch planner in this module) never calls this: a
/// PTY is opened by a human, who picks their own agent's permission flags,
/// so this crate does not impose one. The one caller that does apply this
/// table is the ACP transport (`gate4agent`'s `src/acp`, and
/// `gate4agent-shell-native`'s own native ACP spawn path), which this
/// project opens programmatically with no human at the keyboard to make
/// that choice instead -- and which must consult
/// `approval_level_resolution` directly (not this function) before spawning,
/// to refuse a level `approval_level_resolution` marks `Unsupported` instead
/// of silently launching it with no flag.
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
            ["--permission-mode", "bypassPermissions"]
        );
        assert!(approval_level_args(&grok, ApprovalLevel::Unmanaged).is_empty());

        let kimi = AgentId::new("kimi").unwrap();
        assert_eq!(approval_level_args(&kimi, ApprovalLevel::FullAuto), ["--yolo"]);
        assert!(approval_level_args(&kimi, ApprovalLevel::Unmanaged).is_empty());
    }

    /// `grok` and `kimi` have exactly one verified flag each (`FullAuto`).
    /// Neither `Moderate` nor `ReadOnly` may fabricate a flag for either --
    /// at the argv-only layer both produce exactly the same empty result as
    /// `Unmanaged`. `ReadOnly` additionally resolves `Unsupported` one layer
    /// up (see `approval_level_resolution_matches_the_verified_provider_table`
    /// and `unsupported_resolution_still_yields_empty_argv_not_a_fabricated_flag`)
    /// -- this test only covers the argv-only accessor, which has no channel
    /// to carry that refusal.
    #[test]
    fn grok_and_kimi_moderate_never_invents_a_flag_and_matches_unmanaged() {
        for id in ["grok", "kimi"] {
            let agent = AgentId::new(id).unwrap();
            for level in [ApprovalLevel::Moderate, ApprovalLevel::ReadOnly] {
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

    /// Item 3's table: one row per provider x level, asserting both the
    /// expected flag(s) and the expected `asks_for_permission`, so a wrong
    /// claim about vendor behaviour fails loudly here instead of surfacing
    /// live as a missing `host-request-observed`.
    #[test]
    fn approval_level_resolution_matches_the_verified_provider_table() {
        fn supported(args: &[&str], asks_for_permission: bool) -> ApprovalLevelResolution {
            ApprovalLevelResolution::Supported {
                args: args.iter().map(|value| (*value).to_owned()).collect(),
                asks_for_permission,
            }
        }

        let claude = AgentId::new("claude").unwrap();
        assert_eq!(
            approval_level_resolution(&claude, ApprovalLevel::FullAuto),
            supported(&["--permission-mode", "bypassPermissions"], false)
        );
        assert_eq!(
            approval_level_resolution(&claude, ApprovalLevel::Moderate),
            supported(&["--permission-mode", "acceptEdits"], true)
        );
        assert_eq!(
            approval_level_resolution(&claude, ApprovalLevel::ReadOnly),
            supported(&["--permission-mode", "plan"], false)
        );
        assert_eq!(
            approval_level_resolution(&claude, ApprovalLevel::Unmanaged),
            supported(&[], true)
        );

        let codex = AgentId::new("codex").unwrap();
        assert_eq!(
            approval_level_resolution(&codex, ApprovalLevel::FullAuto),
            supported(&["--dangerously-bypass-approvals-and-sandbox"], false)
        );
        assert_eq!(
            approval_level_resolution(&codex, ApprovalLevel::Moderate),
            supported(
                &["--sandbox", "workspace-write", "--ask-for-approval", "on-request"],
                true
            )
        );
        assert_eq!(
            approval_level_resolution(&codex, ApprovalLevel::ReadOnly),
            supported(&["--sandbox", "read-only", "--ask-for-approval", "never"], false)
        );
        assert_eq!(
            approval_level_resolution(&codex, ApprovalLevel::Unmanaged),
            supported(&[], true)
        );

        let grok = AgentId::new("grok").unwrap();
        assert_eq!(
            approval_level_resolution(&grok, ApprovalLevel::FullAuto),
            supported(&["--permission-mode", "bypassPermissions"], false)
        );
        assert_eq!(
            approval_level_resolution(&grok, ApprovalLevel::Moderate),
            supported(&[], true)
        );
        assert_eq!(
            approval_level_resolution(&grok, ApprovalLevel::ReadOnly),
            ApprovalLevelResolution::Unsupported
        );
        assert_eq!(
            approval_level_resolution(&grok, ApprovalLevel::Unmanaged),
            supported(&[], true)
        );

        let kimi = AgentId::new("kimi").unwrap();
        assert_eq!(
            approval_level_resolution(&kimi, ApprovalLevel::FullAuto),
            supported(&["--yolo"], false)
        );
        assert_eq!(
            approval_level_resolution(&kimi, ApprovalLevel::Moderate),
            supported(&[], true)
        );
        assert_eq!(
            approval_level_resolution(&kimi, ApprovalLevel::ReadOnly),
            ApprovalLevelResolution::Unsupported
        );
        assert_eq!(
            approval_level_resolution(&kimi, ApprovalLevel::Unmanaged),
            supported(&[], true)
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
            supported(&[], true)
        );
    }

    /// A level `approval_level_resolution` refuses (`Unsupported`) must
    /// still surface as an empty `Vec` from the argv-only accessor, exactly
    /// like `Unmanaged` -- `approval_level_args` has no channel to carry a
    /// refusal, which is precisely why the ACP transport must call
    /// `approval_level_resolution` directly and refuse the spawn itself
    /// rather than trusting this function's empty result to mean "safe to
    /// launch with no flag".
    #[test]
    fn unsupported_resolution_still_yields_empty_argv_not_a_fabricated_flag() {
        for id in ["grok", "kimi"] {
            let agent = AgentId::new(id).unwrap();
            assert!(approval_level_args(&agent, ApprovalLevel::ReadOnly).is_empty(), "{id}");
            assert!(matches!(
                approval_level_resolution(&agent, ApprovalLevel::ReadOnly),
                ApprovalLevelResolution::Unsupported
            ));
        }
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
