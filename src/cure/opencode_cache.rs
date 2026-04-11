//! Source 1: parse `~/.cache/opencode/models.json` (no network required).
//!
//! The file is written by the OpenCode CLI when it fetches available models
//! from each configured provider. We parse only what we need via
//! `#[serde(default)]`, so unknown fields are ignored.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

use super::merge::{OpenCodeProvider, extract_tool_models};
use super::types::{CuredModel, CureError};

/// Top-level shape of `~/.cache/opencode/models.json`.
///
/// It is a flat map from provider key (e.g. `"anthropic"`) to provider data.
#[derive(Debug, Deserialize)]
struct OpenCodeProviders(HashMap<String, OpenCodeProvider>);

/// Returns the path `~/.cache/opencode/models.json`.
///
/// On Windows/MSYS the file lives under `$USERPROFILE/.cache/opencode/`.
/// On Linux/macOS it lives under `$HOME/.cache/opencode/`.
pub(crate) fn opencode_cache_path() -> Option<PathBuf> {
    let home = crate::utils::home_dir()?;
    Some(home.join(".cache").join("opencode").join("models.json"))
}

/// Parse the OpenCode model cache and return per-tool `CuredModel` lists.
///
/// # Errors
///
/// - [`CureError::OpenCodeCacheNotFound`] if the file does not exist.
/// - [`CureError::OpenCodeCacheParse`] if the JSON is malformed.
/// - [`CureError::Io`] for other read errors.
pub(crate) fn load_opencode_models() -> Result<HashMap<String, Vec<CuredModel>>, CureError> {
    let path = opencode_cache_path().ok_or(CureError::NoHomeDir)?;

    if !path.exists() {
        return Err(CureError::OpenCodeCacheNotFound { path });
    }

    let content = std::fs::read_to_string(&path)?;
    let providers: OpenCodeProviders = serde_json::from_str(&content)?;
    Ok(extract_tool_models(&providers.0))
}
