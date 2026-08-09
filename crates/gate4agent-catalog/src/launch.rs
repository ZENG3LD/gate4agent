use crate::{
    resolve_session_option_launch_for, AgentId, AgentSpec, InitialPromptMode, NativeDraftMode,
    RuntimePlatform, SessionOptionCatalogError, SessionOptionSelection,
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
    fn opencode_uses_a_prompt_flag_without_shell_quoting() {
        let spec = builtin_registry().get_by_id("opencode").unwrap();
        let prompt = "fix 'quotes'\nand Unicode: Привет";
        let plan = plan_launch(spec, request(prompt)).unwrap();
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
    fn explicit_cursor_options_are_composed_but_untouched_launches_stay_vanilla() {
        let spec = builtin_registry().get_by_id("cursor").unwrap();
        let vanilla = plan_launch(
            spec,
            LaunchRequest {
                platform: RuntimePlatform::Linux,
                ..LaunchRequest::default()
            },
        )
        .unwrap();
        assert!(vanilla.args.is_empty());
        assert!(vanilla.applied_session_options.is_none());

        let selected = SessionOptionSelection::new("gpt-5.3-codex")
            .with_value("effort", "medium")
            .with_value("fastMode", true);
        let plan = plan_launch(
            spec,
            LaunchRequest {
                session_options: Some(selected.clone()),
                platform: RuntimePlatform::Linux,
                ..LaunchRequest::default()
            },
        )
        .unwrap();
        assert_eq!(
            args_as_strings(&plan),
            ["--model", "gpt-5.3-codex-medium-fast"]
        );
        assert_eq!(plan.applied_session_options, Some(selected));
    }

    #[test]
    fn launch_only_agents_cannot_borrow_another_provider_option_catalog() {
        let spec = builtin_registry().get_by_id("opencode").unwrap();
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
}
