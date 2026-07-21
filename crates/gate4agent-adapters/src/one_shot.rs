use gate4agent_types::{
    AdapterId, LaunchSpec, SessionOptionSelection, SessionOptionValue,
    PROVIDER_EVENT_TEXT_MAX_BYTES,
};
use thiserror::Error;

pub const ONE_SHOT_REVISION: &str = "gate4agent-one-shot/orca-d8629c4/v2";
pub const ONE_SHOT_OUTPUT_MAX_BYTES: usize = 4 * 1024 * 1024;
pub const ONE_SHOT_TIMEOUT_SECONDS: u64 = 60;
pub const ONE_SHOT_THINKING_OPTION_ID: &str = "thinking-level";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OneShotPromptDelivery {
    StdinClose,
    Positional,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OneShotModelSource {
    Static,
    Dynamic,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OneShotThinkingLevel {
    pub id: String,
    pub label: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OneShotModelSpec {
    pub id: String,
    pub label: String,
    pub thinking_levels: Vec<OneShotThinkingLevel>,
    pub default_thinking_level: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OneShotAdapterSpec {
    pub adapter_id: AdapterId,
    pub label: String,
    pub prompt_delivery: OneShotPromptDelivery,
    pub model_source: OneShotModelSource,
    pub models: Vec<OneShotModelSpec>,
    pub default_model_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OneShotPlan {
    pub program: String,
    pub args: Vec<String>,
    pub stdin_payload: Option<String>,
    pub label: String,
    pub applied: SessionOptionSelection,
}

pub fn one_shot_specs() -> Vec<OneShotAdapterSpec> {
    [
        "claude",
        "codex",
        "opencode",
        "pi",
        "amp",
        "cursor",
        "kimi",
        "copilot",
        "antigravity",
    ]
    .into_iter()
    .map(|id| spec_for(id).expect("hardcoded one-shot adapter"))
    .collect()
}

pub fn one_shot_spec(adapter_id: &AdapterId) -> Result<OneShotAdapterSpec, OneShotAdapterError> {
    spec_for(adapter_id.as_str())
}

pub fn resolve_one_shot_plan(
    adapter_id: &AdapterId,
    launch: &LaunchSpec,
    prompt: &str,
    selection: Option<&SessionOptionSelection>,
) -> Result<OneShotPlan, OneShotAdapterError> {
    if prompt.trim().is_empty()
        || prompt.len() > PROVIDER_EVENT_TEXT_MAX_BYTES
        || prompt.contains('\0')
    {
        return Err(OneShotAdapterError::InvalidPrompt);
    }
    if launch.program.trim().is_empty() || launch.program.contains('\0') {
        return Err(OneShotAdapterError::InvalidLaunch);
    }
    if launch.fixed_args.iter().any(|arg| arg.contains('\0')) {
        return Err(OneShotAdapterError::InvalidLaunch);
    }

    let spec = one_shot_spec(adapter_id)?;
    let (model, thinking) = resolve_selection(&spec, selection)?;
    let mut args = launch.fixed_args.clone();
    match adapter_id.as_str() {
        "claude" => {
            args.extend(strings(&[
                "-p",
                "--output-format",
                "text",
                "--model",
                &model,
                "--permission-mode",
                "plan",
            ]));
            if let Some(thinking) = &thinking {
                args.extend(["--effort".to_owned(), thinking.clone()]);
            }
        }
        "codex" => {
            args.extend(strings(&[
                "exec",
                "--ephemeral",
                "--skip-git-repo-check",
                "-s",
                "read-only",
                "--model",
                &model,
            ]));
            if let Some(thinking) = &thinking {
                args.extend([
                    "-c".to_owned(),
                    format!("model_reasoning_effort={thinking}"),
                ]);
            }
        }
        "opencode" => {
            args.extend(strings(&[
                "run", "--model", &model, "--agent", "build", "--format", "default",
            ]));
            if let Some(thinking) = &thinking {
                args.extend(["--variant".to_owned(), thinking.clone()]);
            }
            args.push(prompt.to_owned());
        }
        "pi" => {
            args.extend(strings(&[
                "--print",
                "--no-session",
                "--no-tools",
                "--no-extensions",
                "--no-skills",
                "--no-context-files",
                "--mode",
                "text",
                "--model",
                &model,
            ]));
            if let Some(thinking) = &thinking {
                args.extend(["--thinking".to_owned(), thinking.clone()]);
            }
        }
        "amp" => {
            args.extend(strings(&[
                "--execute",
                "--no-notifications",
                "--no-ide",
                "--no-jetbrains",
                "--mode",
                &model,
            ]));
            if let Some(thinking) = &thinking {
                args.extend(["--effort".to_owned(), thinking.clone()]);
            }
        }
        "cursor" => args.extend(strings(&[
            "--print",
            "--mode",
            "ask",
            "--trust",
            "--output-format",
            "text",
            "--model",
            &model,
            prompt,
        ])),
        "kimi" => {
            args.extend(strings(&["--print", "--quiet"]));
            if model != "default" {
                args.extend(["--model".to_owned(), model.clone()]);
            }
            match thinking.as_deref() {
                Some("on") => args.push("--thinking".to_owned()),
                Some("off") => args.push("--no-thinking".to_owned()),
                _ => {}
            }
        }
        "copilot" => {
            args.extend(strings(&[
                "--prompt",
                prompt,
                "--silent",
                "--stream",
                "off",
                "--no-custom-instructions",
                "--model",
                &model,
            ]));
            if let Some(thinking) = &thinking {
                args.extend(["--effort".to_owned(), thinking.clone()]);
            }
        }
        "antigravity" => args.extend(strings(&["--print", "--sandbox", "--model", &model])),
        id => return Err(OneShotAdapterError::UnsupportedAdapter(id.to_owned())),
    }

    let mut applied = SessionOptionSelection::new(model);
    if let Some(thinking) = thinking {
        applied
            .values
            .insert(ONE_SHOT_THINKING_OPTION_ID.to_owned(), thinking.into());
    }
    Ok(OneShotPlan {
        program: launch.program.clone(),
        args,
        stdin_payload: (spec.prompt_delivery == OneShotPromptDelivery::StdinClose)
            .then(|| prompt.to_owned()),
        label: spec.label,
        applied,
    })
}

fn resolve_selection(
    spec: &OneShotAdapterSpec,
    selection: Option<&SessionOptionSelection>,
) -> Result<(String, Option<String>), OneShotAdapterError> {
    if let Some(selection) = selection {
        selection
            .validate()
            .map_err(|_| OneShotAdapterError::InvalidSelection)?;
        if selection
            .values
            .keys()
            .any(|key| key != ONE_SHOT_THINKING_OPTION_ID)
        {
            return Err(OneShotAdapterError::UnknownOption);
        }
    }
    let model_id = selection
        .map(|selection| selection.model.clone())
        .unwrap_or_else(|| spec.default_model_id.clone());
    let known = spec.models.iter().find(|model| model.id == model_id);
    if known.is_none() && spec.model_source == OneShotModelSource::Static {
        return Err(OneShotAdapterError::UnknownModel(model_id));
    }
    let requested_thinking = selection
        .and_then(|selection| selection.values.get(ONE_SHOT_THINKING_OPTION_ID))
        .map(|value| match value {
            SessionOptionValue::String(value) => Ok(value.clone()),
            SessionOptionValue::Boolean(_) => Err(OneShotAdapterError::InvalidSelection),
        })
        .transpose()?;
    let default_thinking = known
        .and_then(|model| model.default_thinking_level.clone())
        .or_else(|| {
            (spec.model_source == OneShotModelSource::Dynamic
                && supports_openai_thinking(&model_id))
            .then(|| "low".to_owned())
        });
    let thinking = requested_thinking.or(default_thinking);
    if let (Some(model), Some(thinking)) = (known, &thinking) {
        if !model.thinking_levels.is_empty()
            && !model
                .thinking_levels
                .iter()
                .any(|level| &level.id == thinking)
        {
            return Err(OneShotAdapterError::UnknownThinkingLevel(thinking.clone()));
        }
        if model.thinking_levels.is_empty() && spec.model_source == OneShotModelSource::Static {
            return Err(OneShotAdapterError::ThinkingUnsupported(model.id.clone()));
        }
    }
    Ok((model_id, thinking))
}

fn spec_for(id: &str) -> Result<OneShotAdapterSpec, OneShotAdapterError> {
    let (label, delivery, source, models, default_model_id) = match id {
        "claude" => (
            "Claude",
            OneShotPromptDelivery::StdinClose,
            OneShotModelSource::Static,
            vec![
                model("haiku", "Haiku", &[], None),
                model("sonnet", "Sonnet", &claude_thinking(), Some("low")),
                model("opus", "Opus", &claude_thinking(), Some("low")),
            ],
            "sonnet",
        ),
        "codex" => (
            "Codex",
            OneShotPromptDelivery::StdinClose,
            OneShotModelSource::Dynamic,
            [
                "gpt-5.5",
                "gpt-5.4",
                "gpt-5.4-mini",
                "gpt-5.3-codex",
                "gpt-5.3-codex-spark",
                "gpt-5.2",
            ]
            .into_iter()
            .map(|id| model(id, id, &openai_thinking(), Some("low")))
            .collect(),
            "gpt-5.5",
        ),
        "opencode" => (
            "OpenCode",
            // OpenCode 1.4.3 documents `run [message..]` and exits with empty
            // output when the message is supplied only through stdin. This is
            // live-vendor evidence that intentionally supersedes the pinned
            // Orca stdin contract at d8629c4.
            OneShotPromptDelivery::Positional,
            OneShotModelSource::Dynamic,
            vec![
                model(
                    "opencode/deepseek-v4-flash-free",
                    "OpenCode DeepSeek V4 Flash Free",
                    &[],
                    None,
                ),
                model(
                    "opencode/gpt-5.4-mini",
                    "OpenCode GPT 5.4 Mini",
                    &openai_thinking(),
                    Some("low"),
                ),
            ],
            "opencode/deepseek-v4-flash-free",
        ),
        "pi" => (
            "Pi",
            OneShotPromptDelivery::StdinClose,
            OneShotModelSource::Dynamic,
            vec![model(
                "github-copilot/gpt-5.4-mini",
                "Github Copilot GPT 5.4 Mini",
                &[
                    ("off", "Off"),
                    ("low", "Low"),
                    ("medium", "Medium"),
                    ("high", "High"),
                    ("xhigh", "Extra High"),
                ],
                Some("low"),
            )],
            "github-copilot/gpt-5.4-mini",
        ),
        "amp" => (
            "Amp",
            OneShotPromptDelivery::StdinClose,
            OneShotModelSource::Static,
            vec![
                model("smart", "Smart", &[], None),
                model("rush", "Rush", &[], None),
                model("large", "Large", &basic_thinking(), Some("low")),
                model("deep", "Deep", &basic_thinking(), Some("low")),
            ],
            "smart",
        ),
        "cursor" => (
            "Cursor",
            OneShotPromptDelivery::Positional,
            OneShotModelSource::Dynamic,
            vec![model("auto", "Auto", &[], None)],
            "auto",
        ),
        "kimi" => (
            "Kimi",
            OneShotPromptDelivery::StdinClose,
            OneShotModelSource::Static,
            vec![
                model("default", "Config default", &[], None),
                model(
                    "kimi-code/kimi-for-coding",
                    "Kimi K2.6",
                    &[("on", "On"), ("off", "Off")],
                    Some("on"),
                ),
            ],
            "default",
        ),
        "copilot" => (
            "GitHub Copilot",
            OneShotPromptDelivery::Positional,
            OneShotModelSource::Static,
            copilot_models(),
            "gpt-5.4",
        ),
        "antigravity" => (
            "Antigravity",
            OneShotPromptDelivery::StdinClose,
            OneShotModelSource::Dynamic,
            [
                "Gemini 3.5 Flash (Medium)",
                "Gemini 3.5 Flash (High)",
                "Gemini 3.5 Flash (Low)",
            ]
            .into_iter()
            .map(|id| model(id, id, &[], None))
            .collect(),
            "Gemini 3.5 Flash (Medium)",
        ),
        id => return Err(OneShotAdapterError::UnsupportedAdapter(id.to_owned())),
    };
    Ok(OneShotAdapterSpec {
        adapter_id: AdapterId::new(id).expect("hardcoded adapter ID"),
        label: label.to_owned(),
        prompt_delivery: delivery,
        model_source: source,
        models,
        default_model_id: default_model_id.to_owned(),
    })
}

fn copilot_models() -> Vec<OneShotModelSpec> {
    [
        "auto",
        "claude-haiku-4.5",
        "claude-sonnet-4.5",
        "claude-sonnet-4.6",
        "claude-opus-4.5",
        "claude-opus-4.6",
        "claude-opus-4.6-fast",
        "claude-opus-4.7",
        "gpt-4.1",
        "gpt-5-mini",
        "gpt-5.2",
        "gpt-5.2-codex",
        "gpt-5.3-codex",
        "gpt-5.4",
        "gpt-5.4-mini",
        "gpt-5.5",
    ]
    .into_iter()
    .map(|id| {
        if supports_openai_thinking(id) {
            model(id, id, &openai_thinking(), Some("low"))
        } else {
            model(id, id, &[], None)
        }
    })
    .collect()
}

fn model(
    id: &str,
    label: &str,
    thinking: &[(&str, &str)],
    default_thinking: Option<&str>,
) -> OneShotModelSpec {
    OneShotModelSpec {
        id: id.to_owned(),
        label: label.to_owned(),
        thinking_levels: thinking
            .iter()
            .map(|(id, label)| OneShotThinkingLevel {
                id: (*id).to_owned(),
                label: (*label).to_owned(),
            })
            .collect(),
        default_thinking_level: default_thinking.map(str::to_owned),
    }
}

fn basic_thinking() -> [(&'static str, &'static str); 3] {
    [("low", "Low"), ("medium", "Medium"), ("high", "High")]
}

fn openai_thinking() -> [(&'static str, &'static str); 4] {
    [
        ("low", "Low"),
        ("medium", "Medium"),
        ("high", "High"),
        ("xhigh", "Extra High"),
    ]
}

fn claude_thinking() -> [(&'static str, &'static str); 5] {
    [
        ("low", "Low"),
        ("medium", "Medium"),
        ("high", "High"),
        ("xhigh", "Extra High"),
        ("max", "Max"),
    ]
}

fn supports_openai_thinking(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    id.contains("gpt-5") || id.contains("codex")
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum OneShotAdapterError {
    #[error("unsupported one-shot adapter '{0}'")]
    UnsupportedAdapter(String),
    #[error("one-shot prompt is empty, contains NUL, or exceeds its bound")]
    InvalidPrompt,
    #[error("one-shot launch contract is invalid")]
    InvalidLaunch,
    #[error("one-shot session-option selection is invalid")]
    InvalidSelection,
    #[error("one-shot selection contains an unknown option")]
    UnknownOption,
    #[error("one-shot model '{0}' is not declared by the provider adapter")]
    UnknownModel(String),
    #[error("one-shot model '{0}' does not support thinking effort")]
    ThinkingUnsupported(String),
    #[error("one-shot thinking level '{0}' is not declared by the model")]
    UnknownThinkingLevel(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn launch(program: &str) -> LaunchSpec {
        LaunchSpec {
            program: program.to_owned(),
            fixed_args: vec!["fixed".to_owned()],
        }
    }

    #[test]
    fn pinned_orca_inventory_and_defaults_are_exact() {
        let specs = one_shot_specs();
        let actual = specs
            .iter()
            .map(|spec| spec.adapter_id.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            actual,
            [
                "amp",
                "antigravity",
                "claude",
                "codex",
                "copilot",
                "cursor",
                "kimi",
                "opencode",
                "pi",
            ]
            .into_iter()
            .collect()
        );
        assert_eq!(
            specs
                .iter()
                .find(|spec| spec.adapter_id.as_str() == "copilot")
                .unwrap()
                .default_model_id,
            "gpt-5.4"
        );
    }

    #[test]
    fn exact_provider_plans_preserve_prompt_delivery_and_safety_flags() {
        let claude = resolve_one_shot_plan(
            &AdapterId::new("claude").unwrap(),
            &launch("claude-custom"),
            "large prompt",
            None,
        )
        .unwrap();
        assert_eq!(claude.program, "claude-custom");
        assert_eq!(claude.stdin_payload.as_deref(), Some("large prompt"));
        assert!(claude
            .args
            .windows(2)
            .any(|pair| pair == ["--permission-mode", "plan"]));
        assert!(claude
            .args
            .windows(2)
            .any(|pair| pair == ["--effort", "low"]));

        let codex = resolve_one_shot_plan(
            &AdapterId::new("codex").unwrap(),
            &launch("codex"),
            "prompt",
            Some(
                &SessionOptionSelection::new("gpt-5.4")
                    .with_value(ONE_SHOT_THINKING_OPTION_ID, "xhigh"),
            ),
        )
        .unwrap();
        assert!(codex.args.contains(&"--ephemeral".to_owned()));
        assert!(codex.args.contains(&"read-only".to_owned()));
        assert!(codex
            .args
            .contains(&"model_reasoning_effort=xhigh".to_owned()));

        let opencode = resolve_one_shot_plan(
            &AdapterId::new("opencode").unwrap(),
            &launch("opencode"),
            "prompt in argv",
            None,
        )
        .unwrap();
        assert_eq!(opencode.stdin_payload, None);
        assert_eq!(
            opencode.args.last().map(String::as_str),
            Some("prompt in argv")
        );

        let cursor = resolve_one_shot_plan(
            &AdapterId::new("cursor").unwrap(),
            &launch("cursor-agent"),
            "prompt in argv",
            None,
        )
        .unwrap();
        assert_eq!(cursor.stdin_payload, None);
        assert_eq!(
            cursor.args.last().map(String::as_str),
            Some("prompt in argv")
        );
        assert!(cursor.args.contains(&"--trust".to_owned()));
    }

    #[test]
    fn every_pinned_provider_builds_its_default_executed_path() {
        let expected_flag = [
            ("claude", "--permission-mode"),
            ("codex", "--ephemeral"),
            ("opencode", "--agent"),
            ("pi", "--no-tools"),
            ("amp", "--no-notifications"),
            ("cursor", "--trust"),
            ("kimi", "--quiet"),
            ("copilot", "--no-custom-instructions"),
            ("antigravity", "--sandbox"),
        ];
        for (id, flag) in expected_flag {
            let plan = resolve_one_shot_plan(
                &AdapterId::new(id).unwrap(),
                &launch(&format!("{id}-binary")),
                "provider prompt",
                None,
            )
            .unwrap();
            assert_eq!(plan.program, format!("{id}-binary"));
            assert_eq!(plan.args.first().map(String::as_str), Some("fixed"));
            assert!(plan.args.iter().any(|arg| arg == flag), "{id}: {flag}");
            let delivery = one_shot_spec(&AdapterId::new(id).unwrap())
                .unwrap()
                .prompt_delivery;
            assert_eq!(
                plan.stdin_payload.is_some(),
                delivery == OneShotPromptDelivery::StdinClose,
                "{id}"
            );
        }
    }

    #[test]
    fn static_models_fail_closed_while_dynamic_models_remain_discovery_compatible() {
        assert!(matches!(
            resolve_one_shot_plan(
                &AdapterId::new("amp").unwrap(),
                &launch("amp"),
                "prompt",
                Some(&SessionOptionSelection::new("unknown")),
            ),
            Err(OneShotAdapterError::UnknownModel(_))
        ));
        let dynamic = resolve_one_shot_plan(
            &AdapterId::new("opencode").unwrap(),
            &launch("opencode"),
            "prompt",
            Some(&SessionOptionSelection::new("vendor/new-model")),
        )
        .unwrap();
        assert!(dynamic.args.contains(&"vendor/new-model".to_owned()));
    }
}
