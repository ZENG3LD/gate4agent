//! Maps provider keys from external sources to gate4agent `CliTool` buckets.
//!
//! Provider key → tool_id mapping (compile-time, exhaustive):
//!
//! | Source provider key | gate4agent tool_id |
//! |---------------------|--------------------|
//! | `"anthropic"`       | `"claude_code"`    |
//! | `"openai"`          | `"codex"`          |
//! | `"google"`          | `"gemini"`         |
//! | `"opencode"`        | `"opencode"`       |

use std::collections::HashMap;

use super::types::CuredModel;

/// Internal raw model representation from the OpenCode disk cache.
#[derive(Debug, serde::Deserialize)]
pub(crate) struct OpenCodeProvider {
    #[serde(default)]
    pub(crate) models: HashMap<String, OpenCodeModel>,
}

/// A single model entry in the OpenCode cache.
#[derive(Debug, serde::Deserialize)]
pub(crate) struct OpenCodeModel {
    #[serde(default)]
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) limit: OpenCodeLimits,
}

/// Token limits for a model in the OpenCode cache.
#[derive(Debug, Default, serde::Deserialize)]
pub(crate) struct OpenCodeLimits {
    #[serde(default)]
    pub(crate) context: u64,
    #[serde(default)]
    pub(crate) output: u64,
}

/// Maps a provider key to the corresponding gate4agent tool_id.
///
/// Returns `None` for unrecognised providers (they are silently ignored).
fn provider_to_tool_id(provider: &str) -> Option<&'static str> {
    match provider {
        "anthropic" => Some("claude_code"),
        "openai"    => Some("codex"),
        "google"    => Some("gemini"),
        "opencode"  => Some("opencode"),
        _           => None,
    }
}

/// Convert a map of OpenCode providers into per-tool `CuredModel` lists.
///
/// Model IDs from the OpenCode cache are used **as-is** — they already
/// match our CLI-level IDs (e.g. `claude-opus-4-6`, `gpt-5.4`).
///
/// Within each tool bucket, models are deduplicated by ID (last write wins),
/// so duplicate entries in the `opencode` provider do not inflate the list.
pub(crate) fn extract_tool_models(
    providers: &HashMap<String, OpenCodeProvider>,
) -> HashMap<String, Vec<CuredModel>> {
    // tool_id → (id → CuredModel) accumulator for dedup.
    let mut acc: HashMap<String, HashMap<String, CuredModel>> = HashMap::new();

    for (provider_key, provider) in providers {
        let Some(tool_id) = provider_to_tool_id(provider_key) else {
            continue;
        };
        let bucket = acc.entry(tool_id.to_string()).or_default();
        for (model_id, model) in &provider.models {
            bucket.insert(
                model_id.clone(),
                CuredModel {
                    id: model_id.clone(),
                    display_name: model.name.clone(),
                    context_window: if model.limit.context > 0 {
                        Some(model.limit.context)
                    } else {
                        None
                    },
                    max_output: if model.limit.output > 0 {
                        Some(model.limit.output)
                    } else {
                        None
                    },
                },
            );
        }
    }

    acc.into_iter()
        .map(|(tool_id, models_map)| (tool_id, models_map.into_values().collect()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── extract_tool_models tests ─────────────────────────────────────────────

    #[test]
    fn extract_anthropic_maps_to_claude_code() {
        let mut providers = HashMap::new();
        let mut models = HashMap::new();
        models.insert(
            "claude-opus-4-6".to_string(),
            OpenCodeModel {
                name: "Claude Opus 4.6".to_string(),
                limit: OpenCodeLimits { context: 1_000_000, output: 128_000 },
            },
        );
        providers.insert("anthropic".to_string(), OpenCodeProvider { models });

        let result = extract_tool_models(&providers);
        assert!(result.contains_key("claude_code"), "anthropic must map to claude_code");
        let claude_models = result.get("claude_code").unwrap();
        assert_eq!(claude_models.len(), 1);
        assert_eq!(claude_models[0].id, "claude-opus-4-6");
        assert_eq!(claude_models[0].context_window, Some(1_000_000));
    }

    #[test]
    fn extract_unknown_provider_is_ignored() {
        let mut providers = HashMap::new();
        providers.insert(
            "xai".to_string(),
            OpenCodeProvider { models: HashMap::new() },
        );
        let result = extract_tool_models(&providers);
        assert!(result.is_empty(), "unknown provider 'xai' must produce no output");
    }

    #[test]
    fn extract_openai_maps_to_codex() {
        let mut providers = HashMap::new();
        let mut models = HashMap::new();
        models.insert(
            "gpt-5.4".to_string(),
            OpenCodeModel {
                name: "GPT-5.4".to_string(),
                limit: OpenCodeLimits { context: 272_000, output: 0 },
            },
        );
        providers.insert("openai".to_string(), OpenCodeProvider { models });

        let result = extract_tool_models(&providers);
        assert!(result.contains_key("codex"));
        // OpenCode cache IDs are used as-is — gpt-5.4 stays gpt-5.4
        let codex_models = result.get("codex").unwrap();
        assert_eq!(codex_models[0].id, "gpt-5.4");
    }
}
