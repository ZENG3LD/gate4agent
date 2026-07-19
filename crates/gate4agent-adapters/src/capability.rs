use gate4agent_types::{AdapterId, CapabilityModelSummary, CAPABILITY_MODELS_MAX};
use std::collections::BTreeSet;
use thiserror::Error;

pub const CAPABILITY_PROBE_REVISION: &str = "gate4agent-capability-probe/orca-d8629c4/v1";
pub const CAPABILITY_PROBE_OUTPUT_MAX_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilityProbePlan {
    pub program: String,
    pub args: Vec<String>,
}

pub fn capability_probe_plan(
    adapter_id: &AdapterId,
) -> Result<CapabilityProbePlan, CapabilityProbeAdapterError> {
    match adapter_id.as_str() {
        // Pinned Orca's native-chat path delegates to commit-message model
        // discovery, whose executed Cursor contract is `--list-models`.
        "cursor" => Ok(CapabilityProbePlan {
            program: "cursor-agent".to_owned(),
            args: vec!["--list-models".to_owned()],
        }),
        id => Err(CapabilityProbeAdapterError::UnsupportedAdapter(
            id.to_owned(),
        )),
    }
}

pub fn parse_capability_models(
    adapter_id: &AdapterId,
    stdout: &str,
) -> Result<Vec<CapabilityModelSummary>, CapabilityProbeAdapterError> {
    capability_probe_plan(adapter_id)?;
    if stdout.len() > CAPABILITY_PROBE_OUTPUT_MAX_BYTES {
        return Err(CapabilityProbeAdapterError::OutputTooLarge);
    }

    let mut seen = BTreeSet::new();
    let mut models = Vec::new();
    for raw_line in stdout.lines() {
        let line = raw_line.trim();
        let Some(separator) = model_separator(line) else {
            continue;
        };
        let id = line[..separator].trim();
        let label = strip_cursor_status_suffix(line[separator + 1..].trim());
        let model = CapabilityModelSummary {
            id: id.to_owned(),
            label: label.to_owned(),
        };
        if model.validate().is_err() || !seen.insert(model.id.clone()) {
            continue;
        }
        models.push(model);
        if models.len() == CAPABILITY_MODELS_MAX {
            break;
        }
    }
    Ok(models)
}

fn model_separator(line: &str) -> Option<usize> {
    line.char_indices().find_map(|(index, character)| {
        if character != '-' {
            return None;
        }
        let before = line[..index].chars().next_back()?;
        let after = line[index + 1..].chars().next()?;
        (before.is_whitespace() && after.is_whitespace()).then_some(index)
    })
}

fn strip_cursor_status_suffix(label: &str) -> &str {
    for suffix in ["(default)", "(current)"] {
        if label.len() >= suffix.len()
            && label[label.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
        {
            let prefix = &label[..label.len() - suffix.len()];
            if prefix.chars().next_back().is_some_and(char::is_whitespace) {
                return prefix.trim_end();
            }
        }
    }
    label
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum CapabilityProbeAdapterError {
    #[error("capability probe adapter is unavailable for {0}")]
    UnsupportedAdapter(String),
    #[error("capability probe output exceeds its byte bound")]
    OutputTooLarge,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cursor() -> AdapterId {
        AdapterId::new("cursor").unwrap()
    }

    #[test]
    fn cursor_uses_the_actually_executed_orca_probe_contract() {
        assert_eq!(
            capability_probe_plan(&cursor()).unwrap(),
            CapabilityProbePlan {
                program: "cursor-agent".to_owned(),
                args: vec!["--list-models".to_owned()],
            }
        );
    }

    #[test]
    fn cursor_models_preserve_live_labels_and_drop_status_suffixes() {
        let models = parse_capability_models(
            &cursor(),
            "noise\nauto - Auto (default)\ngpt-5.3-codex - GPT-5.3 Codex\ntab-model - Tab model\t(current)\nauto - Duplicate\n",
        )
        .unwrap();
        assert_eq!(
            models,
            vec![
                CapabilityModelSummary {
                    id: "auto".to_owned(),
                    label: "Auto".to_owned(),
                },
                CapabilityModelSummary {
                    id: "gpt-5.3-codex".to_owned(),
                    label: "GPT-5.3 Codex".to_owned(),
                },
                CapabilityModelSummary {
                    id: "tab-model".to_owned(),
                    label: "Tab model".to_owned(),
                },
            ]
        );
    }
}
