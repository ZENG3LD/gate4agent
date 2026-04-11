//! Source 2: fetch live model list from `https://openrouter.ai/api/v1/models`.
//!
//! Gated behind the `cure-network` feature. When this feature is enabled,
//! `reqwest` is available and `cure_async()` will call this module after
//! trying the OpenCode disk cache.

#![cfg(feature = "cure-network")]

use std::collections::HashMap;
use std::time::Duration;

use serde::Deserialize;

use super::types::{CuredModel, CureError};

const OPENROUTER_MODELS_URL: &str = "https://openrouter.ai/api/v1/models";
const REQUEST_TIMEOUT_SECS: u64 = 10;

/// Normalise a raw model ID from OpenRouter.
///
/// 1. Strip the provider prefix (`"anthropic/claude-sonnet-4.6"` → `"claude-sonnet-4.6"`).
/// 2. Replace digit `.` digit version separators with dashes
///    (`"claude-sonnet-4.6"` → `"claude-sonnet-4-6"`).
fn normalize_model_id(raw: &str) -> String {
    let model_part = raw.split('/').next_back().unwrap_or(raw);
    replace_version_dots(model_part)
}

/// Replace `digit.digit` with `digit-digit` using byte-by-byte scanning.
fn replace_version_dots(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if i + 2 <= bytes.len().saturating_sub(1)
            && bytes[i].is_ascii_digit()
            && bytes[i + 1] == b'.'
            && bytes[i + 2].is_ascii_digit()
        {
            out.push(bytes[i] as char);
            out.push('-');
            i += 2; // skip emitted digit and the dot; trailing digit handled next iteration
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

/// Minimal shape of the OpenRouter models response.
#[derive(Debug, Deserialize)]
struct OpenRouterResponse {
    data: Vec<OpenRouterModel>,
}

/// One model entry from the OpenRouter response.
#[derive(Debug, Deserialize)]
struct OpenRouterModel {
    /// Full model ID, e.g. `"anthropic/claude-sonnet-4.6"`.
    id: String,
    /// Human-readable name.
    name: String,
    /// Context window size in tokens (always present per OpenRouter spec).
    context_length: u64,
}

/// Maps an OpenRouter provider prefix to a gate4agent tool_id.
///
/// Returns `None` for providers we don't track.
fn openrouter_provider_to_tool_id(provider: &str) -> Option<&'static str> {
    match provider {
        "anthropic" => Some("claude_code"),
        "openai"    => Some("codex"),
        "google"    => Some("gemini"),
        _           => None,
    }
}

/// Fetch the full model list from OpenRouter and return per-tool `CuredModel` lists.
///
/// Uses a 10-second timeout. Network errors and malformed responses are both
/// propagated as [`CureError`].
pub(crate) async fn fetch_openrouter_models() -> Result<HashMap<String, Vec<CuredModel>>, CureError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .build()?;

    let response: OpenRouterResponse = client
        .get(OPENROUTER_MODELS_URL)
        .send()
        .await?
        .json()
        .await?;

    // tool_id → (id → CuredModel) accumulator for dedup.
    let mut acc: HashMap<String, HashMap<String, CuredModel>> = HashMap::new();

    for model in response.data {
        // Split "provider/model-name" on the first '/'.
        let Some((provider, _)) = model.id.split_once('/') else {
            continue;
        };
        let Some(tool_id) = openrouter_provider_to_tool_id(provider) else {
            continue;
        };
        // normalize_model_id strips the prefix and converts digit.digit → digit-dash-digit.
        let normalized_id = normalize_model_id(&model.id);
        acc.entry(tool_id.to_string())
            .or_default()
            .insert(
                normalized_id.clone(),
                CuredModel {
                    id: normalized_id,
                    display_name: model.name,
                    context_window: if model.context_length > 0 {
                        Some(model.context_length)
                    } else {
                        None
                    },
                    max_output: None,
                },
            );
    }

    Ok(acc
        .into_iter()
        .map(|(tool_id, models_map)| (tool_id, models_map.into_values().collect()))
        .collect())
}
