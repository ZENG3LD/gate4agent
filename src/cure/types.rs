//! Core types for the cure module: model discovery and caching.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Persisted cure output file: `~/.gate4agent/models.json`.
///
/// Written by [`super::cure()`] / [`super::cure_async()`] and read lazily
/// by `CliTool::discover_capabilities()` to enrich context-window data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CureCache {
    /// Unix seconds when this file was written.
    pub updated_at: i64,
    /// Which source populated this cache.
    pub source: CureSource,
    /// Per-tool model lists, keyed by `CliTool::tool_id()` (snake_case).
    pub tools: HashMap<String, Vec<CuredModel>>,
}

impl CureCache {
    /// Returns `true` if the cache is older than `max_age_secs` seconds.
    pub fn is_stale(&self, max_age_secs: i64) -> bool {
        let now = chrono::Utc::now().timestamp();
        now - self.updated_at > max_age_secs
    }
}

/// Which external source was used to populate a [`CureCache`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CureSource {
    /// Parsed from `~/.cache/opencode/models.json` (no network required).
    OpenCodeCache,
    /// Fetched live from `https://openrouter.ai/api/v1/models` (requires
    /// the `cure-network` feature and an active internet connection).
    OpenRouter,
    /// Neither source was available; a marker file was written so future
    /// callers can skip re-trying until the app is restarted.
    Hardcoded,
}

/// A single model entry stored in the cure cache.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CuredModel {
    /// CLI-level model ID (dashes for version separators, e.g. `claude-sonnet-4-6`).
    pub id: String,
    /// Human-readable name from the source data.
    pub display_name: String,
    /// Context window in tokens, if available from the source.
    pub context_window: Option<u64>,
    /// Max output tokens, if available from the source (informational only).
    pub max_output: Option<u64>,
}

/// Error type for the cure module.
#[derive(Debug, thiserror::Error)]
pub enum CureError {
    /// The OpenCode cache file did not exist at the expected path.
    #[error("OpenCode cache not found at {path}")]
    OpenCodeCacheNotFound { path: PathBuf },

    /// The OpenCode cache file existed but could not be parsed.
    #[error("failed to parse OpenCode cache: {0}")]
    OpenCodeCacheParse(#[from] serde_json::Error),

    /// A filesystem I/O error occurred while reading or writing the cure cache.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// The home directory could not be determined (no `$HOME` or `$USERPROFILE`).
    #[error("home directory could not be determined")]
    NoHomeDir,

    /// An HTTP error occurred while fetching from OpenRouter.
    #[cfg(feature = "cure-network")]
    #[error("OpenRouter request failed: {0}")]
    OpenRouter(#[from] reqwest::Error),

    /// The OpenRouter response body was not in the expected format.
    #[cfg(feature = "cure-network")]
    #[error("OpenRouter response malformed: {0}")]
    OpenRouterParse(String),
}
