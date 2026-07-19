use gate4agent_types::{
    AdapterId, SessionOptionSelection, SessionOptionValue, SESSION_OPTION_ID_MAX_BYTES,
    SESSION_OPTION_VALUES_MAX, SESSION_OPTION_VALUE_MAX_BYTES,
};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

pub const SESSION_OPTION_CATALOG_REVISION: &str = "gate4agent-session-options/orca-d8629c4/v1";
pub const SESSION_OPTION_MODELS_MAX: usize = 512;
pub const SESSION_OPTION_MODEL_LIST_MAX_BYTES: usize = 1_048_576;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionOptionCategory {
    Model,
    ThoughtLevel,
    ModelConfig,
    Mode,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionOptionChoice {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionOptionKind {
    Select {
        choices: Vec<SessionOptionChoice>,
        default_value: String,
    },
    Boolean {
        default_value: bool,
    },
}

impl SessionOptionKind {
    fn default_value(&self) -> SessionOptionValue {
        match self {
            Self::Select { default_value, .. } => default_value.clone().into(),
            Self::Boolean { default_value } => (*default_value).into(),
        }
    }

    fn accepts(&self, value: &SessionOptionValue) -> bool {
        matches!(
            (self, value),
            (Self::Select { .. }, SessionOptionValue::String(_))
                | (Self::Boolean { .. }, SessionOptionValue::Boolean(_))
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionOptionLaunchApplication {
    Flag { flag: String },
    Config { flag: String, key: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionOptionArgumentOverride {
    Flags(Vec<String>),
    CodexReasoningEffort,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionOptionInteractionDetection {
    ClaudeModelSwitchConfirmation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionOptionMidSessionApplication {
    Command {
        command: String,
        picker_command: Option<String>,
        interaction_detection: Option<SessionOptionInteractionDetection>,
    },
    ToggleCommand {
        command: String,
    },
    AgentPicker {
        command: String,
    },
    Unsupported,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionOptionApply {
    pub launch: Option<SessionOptionLaunchApplication>,
    pub argument_override: Option<SessionOptionArgumentOverride>,
    pub composed_into_model: bool,
    pub mid_session: SessionOptionMidSessionApplication,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionOption {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    pub category: Option<SessionOptionCategory>,
    pub kind: SessionOptionKind,
    pub apply: SessionOptionApply,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionOptionModel {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    pub is_default: Option<bool>,
    pub options: Vec<SessionOption>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionOptionModelListSpec {
    pub program: String,
    pub args: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentSessionOptionCatalog {
    pub adapter_id: AdapterId,
    pub revision: String,
    pub models: Vec<SessionOptionModel>,
    pub model_apply: SessionOptionApply,
    pub model_list: Option<SessionOptionModelListSpec>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedSessionOptionLaunch {
    pub args: Vec<String>,
    pub applied: Option<SessionOptionSelection>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionOptionMidSessionPlan {
    Noop,
    Command {
        command: String,
        target: Option<SessionOptionValue>,
        interaction_detection: Option<SessionOptionInteractionDetection>,
        expected_choice_label: Option<String>,
    },
    AgentPicker {
        command: String,
    },
}

pub fn session_option_catalog(
    adapter_id: &AdapterId,
) -> Result<AgentSessionOptionCatalog, SessionOptionAdapterError> {
    match adapter_id.as_str() {
        "claude-code" => Ok(claude_catalog(adapter_id.clone())),
        "codex" => Ok(codex_catalog(adapter_id.clone())),
        "gemini" => Ok(gemini_catalog(adapter_id.clone())),
        "cursor" => Ok(cursor_catalog(adapter_id.clone())),
        id => Err(SessionOptionAdapterError::UnsupportedAdapter(id.to_owned())),
    }
}

pub fn resolve_session_option_launch(
    adapter_id: &AdapterId,
    selection: &SessionOptionSelection,
    trailing_agent_args: &[String],
) -> Result<ResolvedSessionOptionLaunch, SessionOptionAdapterError> {
    let catalog = session_option_catalog(adapter_id)?;
    let selection = normalize_selection(selection)?;
    let model = find_model(&catalog, &selection.model);
    let values = model
        .map(|model| resolved_model_values(model, &selection))
        .transpose()?
        .unwrap_or_default();
    let model_value = if adapter_id.as_str() == "cursor" {
        compose_cursor_model(&selection.model, &values)
    } else {
        selection.model.clone()
    };
    let model_overridden = is_overridden(&catalog.model_apply, trailing_agent_args);
    let mut args = launch_args(&catalog.model_apply, &model_value).unwrap_or_default();
    let mut applied = (!model_overridden).then(|| SessionOptionSelection::new(&selection.model));

    let Some(model) = model else {
        return Ok(ResolvedSessionOptionLaunch { args, applied });
    };
    for option in &model.options {
        let value = values
            .get(&option.id)
            .expect("resolved model values contain every catalog option");
        if option.apply.composed_into_model {
            if let Some(applied) = &mut applied {
                applied.values.insert(option.id.clone(), value.clone());
            }
            continue;
        }
        if option.apply.launch.is_none() {
            continue;
        }
        let option_args = launch_args(&option.apply, value_as_argument(value)?)
            .expect("launch application was checked above");
        args.extend(option_args);
        if !model_overridden && !is_overridden(&option.apply, trailing_agent_args) {
            if let Some(applied) = &mut applied {
                applied.values.insert(option.id.clone(), value.clone());
            }
        }
    }
    Ok(ResolvedSessionOptionLaunch { args, applied })
}

pub fn plan_mid_session_option(
    adapter_id: &AdapterId,
    current: &SessionOptionSelection,
    option_id: &str,
    target: SessionOptionValue,
) -> Result<SessionOptionMidSessionPlan, SessionOptionAdapterError> {
    let catalog = session_option_catalog(adapter_id)?;
    let current = normalize_selection(current)?;
    validate_option_id(option_id)?;
    let (apply, model_id, expected_choice_label) = if option_id == "model" {
        let target_model =
            target
                .as_str()
                .ok_or_else(|| SessionOptionAdapterError::InvalidValueType {
                    option_id: option_id.to_owned(),
                })?;
        (
            &catalog.model_apply,
            target_model.to_owned(),
            Some(
                find_model(&catalog, target_model)
                    .map(|model| model.label.clone())
                    .unwrap_or_else(|| target_model.to_owned()),
            ),
        )
    } else {
        let model = find_model(&catalog, &current.model).ok_or_else(|| {
            SessionOptionAdapterError::UnknownModel {
                model_id: current.model.clone(),
            }
        })?;
        let option = model
            .options
            .iter()
            .find(|option| option.id == option_id)
            .ok_or_else(|| SessionOptionAdapterError::UnknownOption {
                option_id: option_id.to_owned(),
            })?;
        if !option.kind.accepts(&target) {
            return Err(SessionOptionAdapterError::InvalidValueType {
                option_id: option_id.to_owned(),
            });
        }
        (&option.apply, current.model.clone(), None)
    };

    match &apply.mid_session {
        SessionOptionMidSessionApplication::Command {
            command,
            interaction_detection,
            ..
        } => {
            let value = value_as_command_atom(&target)?;
            Ok(SessionOptionMidSessionPlan::Command {
                command: format!("{command} {value}"),
                target: Some(target),
                interaction_detection: *interaction_detection,
                expected_choice_label,
            })
        }
        SessionOptionMidSessionApplication::ToggleCommand { command } => {
            let current_value = current
                .values
                .get(option_id)
                .and_then(SessionOptionValue::as_bool);
            let target_value =
                target
                    .as_bool()
                    .ok_or_else(|| SessionOptionAdapterError::InvalidValueType {
                        option_id: option_id.to_owned(),
                    })?;
            match current_value {
                Some(value) if value == target_value => Ok(SessionOptionMidSessionPlan::Noop),
                Some(_) => Ok(SessionOptionMidSessionPlan::Command {
                    command: command.clone(),
                    target: Some(target),
                    interaction_detection: None,
                    expected_choice_label: None,
                }),
                None => Err(SessionOptionAdapterError::UnknownToggleBaseline {
                    option_id: option_id.to_owned(),
                }),
            }
        }
        SessionOptionMidSessionApplication::AgentPicker { command } => {
            Ok(SessionOptionMidSessionPlan::AgentPicker {
                command: command.clone(),
            })
        }
        SessionOptionMidSessionApplication::Unsupported if apply.composed_into_model => {
            let model = find_model(&catalog, &model_id).ok_or_else(|| {
                SessionOptionAdapterError::UnknownModel {
                    model_id: model_id.clone(),
                }
            })?;
            let mut values = resolved_model_values(model, &current)?;
            values.insert(option_id.to_owned(), target.clone());
            let composed = compose_cursor_model(&model_id, &values);
            let SessionOptionMidSessionApplication::Command {
                command,
                interaction_detection,
                ..
            } = &catalog.model_apply.mid_session
            else {
                return Err(SessionOptionAdapterError::MidSessionUnsupported {
                    option_id: option_id.to_owned(),
                });
            };
            let composed_value = SessionOptionValue::from(composed);
            Ok(SessionOptionMidSessionPlan::Command {
                command: format!("{command} {}", value_as_command_atom(&composed_value)?),
                target: Some(target),
                interaction_detection: *interaction_detection,
                expected_choice_label: None,
            })
        }
        SessionOptionMidSessionApplication::Unsupported => {
            Err(SessionOptionAdapterError::MidSessionUnsupported {
                option_id: option_id.to_owned(),
            })
        }
    }
}

pub fn plan_mid_session_action(
    adapter_id: &AdapterId,
    current: &SessionOptionSelection,
    option_id: &str,
) -> Result<SessionOptionMidSessionPlan, SessionOptionAdapterError> {
    let catalog = session_option_catalog(adapter_id)?;
    let current = normalize_selection(current)?;
    validate_option_id(option_id)?;
    let apply = if option_id == "model" {
        &catalog.model_apply
    } else {
        let model = find_model(&catalog, &current.model).ok_or_else(|| {
            SessionOptionAdapterError::UnknownModel {
                model_id: current.model.clone(),
            }
        })?;
        &model
            .options
            .iter()
            .find(|option| option.id == option_id)
            .ok_or_else(|| SessionOptionAdapterError::UnknownOption {
                option_id: option_id.to_owned(),
            })?
            .apply
    };
    match &apply.mid_session {
        SessionOptionMidSessionApplication::AgentPicker { command } => {
            Ok(SessionOptionMidSessionPlan::AgentPicker {
                command: command.clone(),
            })
        }
        SessionOptionMidSessionApplication::ToggleCommand { command }
            if !current.values.contains_key(option_id) =>
        {
            Ok(SessionOptionMidSessionPlan::Command {
                command: command.clone(),
                target: None,
                interaction_detection: None,
                expected_choice_label: None,
            })
        }
        _ => Err(SessionOptionAdapterError::ActionUnavailable {
            option_id: option_id.to_owned(),
        }),
    }
}

pub fn parse_session_option_models(
    adapter_id: &AdapterId,
    stdout: &str,
) -> Result<Vec<SessionOptionModel>, SessionOptionAdapterError> {
    let catalog = session_option_catalog(adapter_id)?;
    if stdout.len() > SESSION_OPTION_MODEL_LIST_MAX_BYTES {
        return Err(SessionOptionAdapterError::ModelListTooLarge);
    }
    if catalog.model_list.is_none() {
        return Err(SessionOptionAdapterError::ModelListUnsupported(
            adapter_id.as_str().to_owned(),
        ));
    }
    let mut seen = BTreeSet::new();
    let mut models = Vec::new();
    for line in stdout.lines() {
        let mut candidate = line.trim();
        if let Some(rest) = candidate
            .strip_prefix("- ")
            .or_else(|| candidate.strip_prefix("* "))
        {
            candidate = rest.trim();
        }
        if let Some(index) = candidate.find(" (") {
            if candidate.ends_with(')') {
                candidate = &candidate[..index];
            }
        }
        if !valid_dynamic_model_id(candidate)
            || candidate.eq_ignore_ascii_case("models")
            || !seen.insert(candidate.to_owned())
        {
            continue;
        }
        models.push(SessionOptionModel {
            id: candidate.to_owned(),
            label: if candidate == "auto" {
                "Auto".to_owned()
            } else {
                candidate.to_owned()
            },
            description: None,
            is_default: None,
            options: Vec::new(),
        });
        if models.len() == SESSION_OPTION_MODELS_MAX {
            break;
        }
    }
    Ok(models)
}

pub fn merge_session_option_models(
    seed: &[SessionOptionModel],
    discovered: &[SessionOptionModel],
) -> Vec<SessionOptionModel> {
    let discovered_by_id = discovered
        .iter()
        .cloned()
        .map(|model| (model.id.clone(), model))
        .collect::<BTreeMap<_, _>>();
    let seed_ids = seed
        .iter()
        .map(|model| model.id.as_str())
        .collect::<BTreeSet<_>>();
    let mut merged = Vec::new();
    for model in seed {
        let Some(live) = discovered_by_id.get(&model.id) else {
            merged.push(model.clone());
            continue;
        };
        merged.push(SessionOptionModel {
            id: model.id.clone(),
            label: live.label.clone(),
            description: live
                .description
                .clone()
                .or_else(|| model.description.clone()),
            is_default: live.is_default.or(model.is_default),
            options: model.options.clone(),
        });
    }
    let mut appended = BTreeSet::new();
    for model in discovered {
        if !seed_ids.contains(model.id.as_str()) && appended.insert(model.id.as_str()) {
            if let Some(model) = discovered_by_id.get(&model.id) {
                merged.push(model.clone());
            }
        }
    }
    merged
}

fn normalize_selection(
    selection: &SessionOptionSelection,
) -> Result<SessionOptionSelection, SessionOptionAdapterError> {
    let model = normalize_value(&selection.model)?;
    if selection.values.len() > SESSION_OPTION_VALUES_MAX {
        return Err(SessionOptionAdapterError::TooManyValues);
    }
    let mut values = BTreeMap::new();
    for (id, value) in &selection.values {
        validate_option_id(id)?;
        if id == "model" {
            return Err(SessionOptionAdapterError::ReservedModelOption);
        }
        let value = match value {
            SessionOptionValue::String(value) => normalize_value(value)?.into(),
            SessionOptionValue::Boolean(value) => (*value).into(),
        };
        values.insert(id.clone(), value);
    }
    Ok(SessionOptionSelection { model, values })
}

fn normalize_value(value: &str) -> Result<String, SessionOptionAdapterError> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > SESSION_OPTION_VALUE_MAX_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(SessionOptionAdapterError::InvalidValue);
    }
    Ok(value.to_owned())
}

fn validate_option_id(value: &str) -> Result<(), SessionOptionAdapterError> {
    if value.is_empty()
        || value.len() > SESSION_OPTION_ID_MAX_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(SessionOptionAdapterError::InvalidOptionId);
    }
    Ok(())
}

fn value_as_argument(value: &SessionOptionValue) -> Result<&str, SessionOptionAdapterError> {
    value
        .as_str()
        .ok_or(SessionOptionAdapterError::InvalidValue)
}

fn value_as_command_atom(value: &SessionOptionValue) -> Result<&str, SessionOptionAdapterError> {
    let value = value_as_argument(value)?;
    if value.chars().any(char::is_whitespace) {
        return Err(SessionOptionAdapterError::UnsafeCommandValue);
    }
    Ok(value)
}

fn resolved_model_values(
    model: &SessionOptionModel,
    selection: &SessionOptionSelection,
) -> Result<BTreeMap<String, SessionOptionValue>, SessionOptionAdapterError> {
    let mut values = BTreeMap::new();
    for option in &model.options {
        let value = selection
            .values
            .get(&option.id)
            .cloned()
            .unwrap_or_else(|| option.kind.default_value());
        if !option.kind.accepts(&value) {
            return Err(SessionOptionAdapterError::InvalidValueType {
                option_id: option.id.clone(),
            });
        }
        values.insert(option.id.clone(), value);
    }
    Ok(values)
}

fn launch_args(apply: &SessionOptionApply, value: &str) -> Option<Vec<String>> {
    match apply.launch.as_ref()? {
        SessionOptionLaunchApplication::Flag { flag } => Some(vec![flag.clone(), value.to_owned()]),
        SessionOptionLaunchApplication::Config { flag, key } => {
            Some(vec![flag.clone(), format!("{key}={value}")])
        }
    }
}

fn is_overridden(apply: &SessionOptionApply, tokens: &[String]) -> bool {
    match &apply.argument_override {
        Some(SessionOptionArgumentOverride::Flags(flags)) => has_flag(tokens, flags),
        Some(SessionOptionArgumentOverride::CodexReasoningEffort) => {
            has_flag(tokens, &["--reasoning-effort".to_owned()])
                || tokens.iter().enumerate().any(|(index, token)| {
                    let previous = index.checked_sub(1).and_then(|index| tokens.get(index));
                    (token.starts_with("model_reasoning_effort=")
                        && matches!(previous.map(String::as_str), Some("-c" | "--config")))
                        || token.starts_with("-cmodel_reasoning_effort=")
                        || token.starts_with("-c=model_reasoning_effort=")
                        || token.starts_with("--config=model_reasoning_effort=")
                })
        }
        None => false,
    }
}

fn has_flag(tokens: &[String], flags: &[String]) -> bool {
    tokens.iter().any(|token| {
        flags.iter().any(|flag| {
            token == flag
                || token.starts_with(&format!("{flag}="))
                || (flag.starts_with('-') && !flag.starts_with("--") && token.starts_with(flag))
        })
    })
}

fn find_model<'a>(
    catalog: &'a AgentSessionOptionCatalog,
    model_id: &str,
) -> Option<&'a SessionOptionModel> {
    catalog.models.iter().find(|model| model.id == model_id)
}

fn compose_cursor_model(model_id: &str, values: &BTreeMap<String, SessionOptionValue>) -> String {
    if model_id == "auto" {
        return model_id.to_owned();
    }
    if model_id.starts_with("claude-") {
        let thinking = if values.get("thinking").and_then(SessionOptionValue::as_bool) == Some(true)
        {
            "-thinking"
        } else {
            ""
        };
        let effort = values
            .get("effort")
            .and_then(SessionOptionValue::as_str)
            .map(|value| format!("-{value}"))
            .unwrap_or_default();
        return format!("{model_id}{thinking}{effort}");
    }
    let effort = values
        .get("effort")
        .and_then(SessionOptionValue::as_str)
        .map(|value| format!("-{value}"))
        .unwrap_or_default();
    let fast = if values.get("fastMode").and_then(SessionOptionValue::as_bool) == Some(true) {
        "-fast"
    } else {
        ""
    };
    format!("{model_id}{effort}{fast}")
}

fn valid_dynamic_model_id(value: &str) -> bool {
    value
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        && value.len() <= SESSION_OPTION_VALUE_MAX_BYTES
}

fn choice(value: &str, label: &str) -> SessionOptionChoice {
    SessionOptionChoice {
        value: value.to_owned(),
        label: label.to_owned(),
        description: None,
    }
}

fn select_option(
    id: &str,
    label: &str,
    category: SessionOptionCategory,
    choices: &[(&str, &str)],
    default_value: &str,
    apply: SessionOptionApply,
) -> SessionOption {
    SessionOption {
        id: id.to_owned(),
        label: label.to_owned(),
        description: None,
        category: Some(category),
        kind: SessionOptionKind::Select {
            choices: choices
                .iter()
                .map(|(value, label)| choice(value, label))
                .collect(),
            default_value: default_value.to_owned(),
        },
        apply,
    }
}

fn boolean_option(
    id: &str,
    label: &str,
    category: SessionOptionCategory,
    default_value: bool,
    apply: SessionOptionApply,
) -> SessionOption {
    SessionOption {
        id: id.to_owned(),
        label: label.to_owned(),
        description: None,
        category: Some(category),
        kind: SessionOptionKind::Boolean { default_value },
        apply,
    }
}

fn model(
    id: &str,
    label: &str,
    is_default: bool,
    options: Vec<SessionOption>,
) -> SessionOptionModel {
    SessionOptionModel {
        id: id.to_owned(),
        label: label.to_owned(),
        description: None,
        is_default: is_default.then_some(true),
        options,
    }
}

fn command_apply(command: &str) -> SessionOptionMidSessionApplication {
    SessionOptionMidSessionApplication::Command {
        command: command.to_owned(),
        picker_command: None,
        interaction_detection: None,
    }
}

fn claude_effort(extended: bool) -> SessionOption {
    let mut choices = vec![("low", "Low"), ("medium", "Medium"), ("high", "High")];
    if extended {
        choices.extend([("xhigh", "Extra high"), ("max", "Max")]);
    }
    select_option(
        "effort",
        "Effort",
        SessionOptionCategory::ThoughtLevel,
        &choices,
        "high",
        SessionOptionApply {
            launch: Some(SessionOptionLaunchApplication::Flag {
                flag: "--effort".to_owned(),
            }),
            argument_override: Some(SessionOptionArgumentOverride::Flags(vec![
                "--effort".to_owned()
            ])),
            composed_into_model: false,
            mid_session: command_apply("/effort"),
        },
    )
}

fn claude_catalog(adapter_id: AdapterId) -> AgentSessionOptionCatalog {
    let fast_mode = || {
        boolean_option(
            "fastMode",
            "Fast mode",
            SessionOptionCategory::Mode,
            false,
            SessionOptionApply {
                launch: None,
                argument_override: None,
                composed_into_model: false,
                mid_session: SessionOptionMidSessionApplication::ToggleCommand {
                    command: "/fast".to_owned(),
                },
            },
        )
    };
    AgentSessionOptionCatalog {
        adapter_id,
        revision: SESSION_OPTION_CATALOG_REVISION.to_owned(),
        models: vec![
            model("fable", "Fable 5", false, vec![claude_effort(true)]),
            model(
                "opus",
                "Opus 4.8",
                false,
                vec![claude_effort(true), fast_mode()],
            ),
            model("sonnet", "Sonnet 5", true, vec![claude_effort(true)]),
            model("haiku", "Haiku", false, Vec::new()),
        ],
        model_apply: SessionOptionApply {
            launch: Some(SessionOptionLaunchApplication::Flag {
                flag: "--model".to_owned(),
            }),
            argument_override: Some(SessionOptionArgumentOverride::Flags(vec![
                "--model".to_owned()
            ])),
            composed_into_model: false,
            mid_session: SessionOptionMidSessionApplication::Command {
                command: "/model".to_owned(),
                picker_command: Some("/model".to_owned()),
                interaction_detection: Some(
                    SessionOptionInteractionDetection::ClaudeModelSwitchConfirmation,
                ),
            },
        },
        model_list: None,
    }
}

fn codex_effort(include_extra_high: bool) -> SessionOption {
    let mut choices = vec![
        ("minimal", "Minimal"),
        ("low", "Low"),
        ("medium", "Medium"),
        ("high", "High"),
    ];
    if include_extra_high {
        choices.push(("xhigh", "Extra high"));
    }
    select_option(
        "effort",
        "Reasoning effort",
        SessionOptionCategory::ThoughtLevel,
        &choices,
        "medium",
        SessionOptionApply {
            launch: Some(SessionOptionLaunchApplication::Config {
                flag: "-c".to_owned(),
                key: "model_reasoning_effort".to_owned(),
            }),
            argument_override: Some(SessionOptionArgumentOverride::CodexReasoningEffort),
            composed_into_model: false,
            mid_session: SessionOptionMidSessionApplication::AgentPicker {
                command: "/model".to_owned(),
            },
        },
    )
}

fn codex_catalog(adapter_id: AdapterId) -> AgentSessionOptionCatalog {
    AgentSessionOptionCatalog {
        adapter_id,
        revision: SESSION_OPTION_CATALOG_REVISION.to_owned(),
        models: vec![
            model(
                "gpt-5.6-sol",
                "GPT-5.6 Sol",
                false,
                vec![codex_effort(true)],
            ),
            model(
                "gpt-5.6-terra",
                "GPT-5.6 Terra",
                false,
                vec![codex_effort(true)],
            ),
            model(
                "gpt-5.6-luna",
                "GPT-5.6 Luna",
                false,
                vec![codex_effort(false)],
            ),
            model("gpt-5.5", "GPT-5.5", false, vec![codex_effort(true)]),
            model(
                "gpt-5.2-codex",
                "GPT-5.2 Codex",
                false,
                vec![codex_effort(true)],
            ),
        ],
        model_apply: SessionOptionApply {
            launch: Some(SessionOptionLaunchApplication::Flag {
                flag: "-m".to_owned(),
            }),
            argument_override: Some(SessionOptionArgumentOverride::Flags(vec![
                "-m".to_owned(),
                "--model".to_owned(),
            ])),
            composed_into_model: false,
            mid_session: SessionOptionMidSessionApplication::AgentPicker {
                command: "/model".to_owned(),
            },
        },
        model_list: None,
    }
}

fn gemini_catalog(adapter_id: AdapterId) -> AgentSessionOptionCatalog {
    AgentSessionOptionCatalog {
        adapter_id,
        revision: SESSION_OPTION_CATALOG_REVISION.to_owned(),
        models: vec![
            model(
                "gemini-3-pro-preview",
                "Gemini 3 Pro Preview",
                false,
                Vec::new(),
            ),
            model(
                "gemini-3-flash-preview",
                "Gemini 3 Flash Preview",
                false,
                Vec::new(),
            ),
            model("gemini-2.5-pro", "Gemini 2.5 Pro", false, Vec::new()),
            model("gemini-2.5-flash", "Gemini 2.5 Flash", false, Vec::new()),
        ],
        model_apply: SessionOptionApply {
            launch: Some(SessionOptionLaunchApplication::Flag {
                flag: "-m".to_owned(),
            }),
            argument_override: Some(SessionOptionArgumentOverride::Flags(vec![
                "-m".to_owned(),
                "--model".to_owned(),
            ])),
            composed_into_model: false,
            mid_session: SessionOptionMidSessionApplication::AgentPicker {
                command: "/model".to_owned(),
            },
        },
        model_list: None,
    }
}

fn cursor_catalog(adapter_id: AdapterId) -> AgentSessionOptionCatalog {
    let effort = || {
        select_option(
            "effort",
            "Effort",
            SessionOptionCategory::ThoughtLevel,
            &[("low", "Low"), ("medium", "Medium"), ("high", "High")],
            "high",
            SessionOptionApply {
                launch: None,
                argument_override: None,
                composed_into_model: true,
                mid_session: SessionOptionMidSessionApplication::Unsupported,
            },
        )
    };
    AgentSessionOptionCatalog {
        adapter_id,
        revision: SESSION_OPTION_CATALOG_REVISION.to_owned(),
        models: vec![
            model("auto", "Auto", true, Vec::new()),
            model(
                "gpt-5.3-codex",
                "GPT-5.3 Codex",
                false,
                vec![
                    effort(),
                    boolean_option(
                        "fastMode",
                        "Fast mode",
                        SessionOptionCategory::Mode,
                        false,
                        SessionOptionApply {
                            launch: None,
                            argument_override: None,
                            composed_into_model: true,
                            mid_session: SessionOptionMidSessionApplication::Unsupported,
                        },
                    ),
                ],
            ),
            model(
                "claude-opus-4-8",
                "Claude Opus 4.8",
                false,
                vec![
                    boolean_option(
                        "thinking",
                        "Thinking",
                        SessionOptionCategory::ModelConfig,
                        true,
                        SessionOptionApply {
                            launch: None,
                            argument_override: None,
                            composed_into_model: true,
                            mid_session: SessionOptionMidSessionApplication::Unsupported,
                        },
                    ),
                    effort(),
                ],
            ),
        ],
        model_apply: SessionOptionApply {
            launch: Some(SessionOptionLaunchApplication::Flag {
                flag: "--model".to_owned(),
            }),
            argument_override: Some(SessionOptionArgumentOverride::Flags(vec![
                "-m".to_owned(),
                "--model".to_owned(),
            ])),
            composed_into_model: false,
            mid_session: command_apply("/model"),
        },
        model_list: Some(SessionOptionModelListSpec {
            program: "cursor-agent".to_owned(),
            args: vec!["models".to_owned()],
        }),
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SessionOptionAdapterError {
    #[error("session-option adapter is unavailable for {0}")]
    UnsupportedAdapter(String),
    #[error("session-option value is empty, contains controls, or exceeds its bound")]
    InvalidValue,
    #[error("session-option ID is empty, contains controls, or exceeds its bound")]
    InvalidOptionId,
    #[error("session-option selection cannot store a second model field")]
    ReservedModelOption,
    #[error("session-option selection exceeds the value count bound")]
    TooManyValues,
    #[error("session option {option_id} has the wrong value type")]
    InvalidValueType { option_id: String },
    #[error("session-option model {model_id} is not in the versioned catalog")]
    UnknownModel { model_id: String },
    #[error("unknown session option {option_id}")]
    UnknownOption { option_id: String },
    #[error("session-option command values must be single atoms")]
    UnsafeCommandValue,
    #[error("session option {option_id} has an unknown toggle baseline")]
    UnknownToggleBaseline { option_id: String },
    #[error("session option {option_id} cannot be changed mid-session")]
    MidSessionUnsupported { option_id: String },
    #[error("session option {option_id} has no value-less action")]
    ActionUnavailable { option_id: String },
    #[error("model-list parsing is unavailable for {0}")]
    ModelListUnsupported(String),
    #[error("model-list output exceeds the supported bound")]
    ModelListTooLarge,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(value: &str) -> AdapterId {
        AdapterId::new(value).unwrap()
    }

    fn choice_values(option: &SessionOption) -> Vec<&str> {
        let SessionOptionKind::Select { choices, .. } = &option.kind else {
            panic!("expected a select option");
        };
        choices.iter().map(|choice| choice.value.as_str()).collect()
    }

    #[test]
    fn pinned_catalogs_keep_model_scoped_option_shapes() {
        let claude = session_option_catalog(&id("claude-code")).unwrap();
        assert_eq!(
            claude
                .models
                .iter()
                .find(|model| model.id == "opus")
                .unwrap()
                .options
                .iter()
                .map(|option| option.id.as_str())
                .collect::<Vec<_>>(),
            ["effort", "fastMode"]
        );
        assert!(claude
            .models
            .iter()
            .find(|model| model.id == "haiku")
            .unwrap()
            .options
            .is_empty());
        assert_eq!(claude.revision, SESSION_OPTION_CATALOG_REVISION);

        let fable_effort = &claude
            .models
            .iter()
            .find(|model| model.id == "fable")
            .unwrap()
            .options[0];
        assert_eq!(
            choice_values(fable_effort),
            ["low", "medium", "high", "xhigh", "max"]
        );

        let codex = session_option_catalog(&id("codex")).unwrap();
        let sol_effort = &codex
            .models
            .iter()
            .find(|model| model.id == "gpt-5.6-sol")
            .unwrap()
            .options[0];
        let luna_effort = &codex
            .models
            .iter()
            .find(|model| model.id == "gpt-5.6-luna")
            .unwrap()
            .options[0];
        assert_eq!(
            choice_values(sol_effort),
            ["minimal", "low", "medium", "high", "xhigh"]
        );
        assert_eq!(
            choice_values(luna_effort),
            ["minimal", "low", "medium", "high"]
        );
    }

    #[test]
    fn pinned_catalog_model_inventories_are_exact() {
        for (adapter, expected) in [
            ("claude-code", vec!["fable", "opus", "sonnet", "haiku"]),
            (
                "codex",
                vec![
                    "gpt-5.6-sol",
                    "gpt-5.6-terra",
                    "gpt-5.6-luna",
                    "gpt-5.5",
                    "gpt-5.2-codex",
                ],
            ),
            (
                "gemini",
                vec![
                    "gemini-3-pro-preview",
                    "gemini-3-flash-preview",
                    "gemini-2.5-pro",
                    "gemini-2.5-flash",
                ],
            ),
            ("cursor", vec!["auto", "gpt-5.3-codex", "claude-opus-4-8"]),
        ] {
            let actual = session_option_catalog(&id(adapter))
                .unwrap()
                .models
                .into_iter()
                .map(|model| model.id)
                .collect::<Vec<_>>();
            assert_eq!(actual, expected, "model inventory drift for {adapter}");
        }
    }

    #[test]
    fn launch_resolution_matches_pinned_defaults_and_override_precedence() {
        let selected = SessionOptionSelection::new("opus")
            .with_value("effort", "xhigh")
            .with_value("fastMode", true);
        let resolved = resolve_session_option_launch(
            &id("claude-code"),
            &selected,
            &["--model".to_owned(), "haiku".to_owned()],
        )
        .unwrap();
        assert_eq!(resolved.args, ["--model", "opus", "--effort", "xhigh"]);
        assert!(resolved.applied.is_none());

        let resolved = resolve_session_option_launch(
            &id("claude-code"),
            &SessionOptionSelection::new("sonnet"),
            &[],
        )
        .unwrap();
        assert_eq!(resolved.args, ["--model", "sonnet", "--effort", "high"]);
        assert_eq!(
            resolved.applied,
            Some(SessionOptionSelection::new("sonnet").with_value("effort", "high"))
        );
    }

    #[test]
    fn codex_override_detection_preserves_only_truthful_applied_values() {
        let selected = SessionOptionSelection::new("gpt-5.6-sol").with_value("effort", "medium");
        let resolved = resolve_session_option_launch(
            &id("codex"),
            &selected,
            &["-c".to_owned(), "model_reasoning_effort=high".to_owned()],
        )
        .unwrap();
        assert_eq!(
            resolved.args,
            ["-m", "gpt-5.6-sol", "-c", "model_reasoning_effort=medium"]
        );
        assert_eq!(
            resolved.applied,
            Some(SessionOptionSelection::new("gpt-5.6-sol"))
        );
    }

    #[test]
    fn cursor_composes_options_for_launch_and_live_model_commands() {
        let current = SessionOptionSelection::new("gpt-5.3-codex")
            .with_value("effort", "high")
            .with_value("fastMode", true);
        let resolved = resolve_session_option_launch(&id("cursor"), &current, &[]).unwrap();
        assert_eq!(resolved.args, ["--model", "gpt-5.3-codex-high-fast"]);
        assert_eq!(resolved.applied, Some(current.clone()));

        assert_eq!(
            plan_mid_session_option(&id("cursor"), &current, "effort", "low".into(),).unwrap(),
            SessionOptionMidSessionPlan::Command {
                command: "/model gpt-5.3-codex-low-fast".to_owned(),
                target: Some("low".into()),
                interaction_detection: None,
                expected_choice_label: None,
            }
        );
    }

    #[test]
    fn mid_session_plans_distinguish_commands_pickers_and_unknown_toggles() {
        let claude = SessionOptionSelection::new("opus").with_value("fastMode", false);
        assert!(matches!(
            plan_mid_session_option(&id("claude-code"), &claude, "model", "sonnet".into(),)
                .unwrap(),
            SessionOptionMidSessionPlan::Command {
                interaction_detection: Some(
                    SessionOptionInteractionDetection::ClaudeModelSwitchConfirmation
                ),
                ..
            }
        ));
        assert!(matches!(
            plan_mid_session_option(
                &id("codex"),
                &SessionOptionSelection::new("gpt-5.5"),
                "model",
                "gpt-5.6-sol".into(),
            )
            .unwrap(),
            SessionOptionMidSessionPlan::AgentPicker { .. }
        ));
        assert_eq!(
            plan_mid_session_option(&id("claude-code"), &claude, "fastMode", true.into(),).unwrap(),
            SessionOptionMidSessionPlan::Command {
                command: "/fast".to_owned(),
                target: Some(true.into()),
                interaction_detection: None,
                expected_choice_label: None,
            }
        );
        assert!(matches!(
            plan_mid_session_option(
                &id("claude-code"),
                &SessionOptionSelection::new("opus"),
                "fastMode",
                true.into(),
            ),
            Err(SessionOptionAdapterError::UnknownToggleBaseline { .. })
        ));
        assert_eq!(
            plan_mid_session_action(
                &id("claude-code"),
                &SessionOptionSelection::new("opus"),
                "fastMode",
            )
            .unwrap(),
            SessionOptionMidSessionPlan::Command {
                command: "/fast".to_owned(),
                target: None,
                interaction_detection: None,
                expected_choice_label: None,
            }
        );
    }

    #[test]
    fn cursor_model_discovery_is_bounded_and_preserves_seed_option_shapes() {
        let catalog = session_option_catalog(&id("cursor")).unwrap();
        assert_eq!(
            catalog.model_list,
            Some(SessionOptionModelListSpec {
                program: "cursor-agent".to_owned(),
                args: vec!["models".to_owned()],
            })
        );
        let discovered = parse_session_option_models(
            &id("cursor"),
            "Available models:\n- auto (default)\n- gpt-5.3-codex\nmodels\n- account-model\n",
        )
        .unwrap();
        assert_eq!(
            discovered
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["auto", "gpt-5.3-codex", "account-model"]
        );
        let merged = merge_session_option_models(&catalog.models, &discovered);
        assert_eq!(
            merged
                .iter()
                .find(|model| model.id == "gpt-5.3-codex")
                .unwrap()
                .options
                .len(),
            2
        );
        assert_eq!(merged.last().unwrap().id, "account-model");
    }

    #[test]
    fn unsupported_or_unsafe_values_fail_typed_without_fallback() {
        assert!(matches!(
            session_option_catalog(&id("opencode")),
            Err(SessionOptionAdapterError::UnsupportedAdapter(_))
        ));
        let future = resolve_session_option_launch(
            &id("claude-code"),
            &SessionOptionSelection::new("opus").with_value("effort", "future-effort"),
            &[],
        )
        .unwrap();
        assert_eq!(
            future.args,
            ["--model", "opus", "--effort", "future-effort"]
        );
        assert!(matches!(
            plan_mid_session_option(
                &id("claude-code"),
                &SessionOptionSelection::new("opus"),
                "effort",
                "high now".into(),
            ),
            Err(SessionOptionAdapterError::UnsafeCommandValue)
        ));
    }
}
