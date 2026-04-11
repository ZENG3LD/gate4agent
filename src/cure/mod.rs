//! Runtime model discovery for gate4agent.
//!
//! The cure module populates `~/.gate4agent/models.json` with live model
//! metadata from external sources so that `CliTool::discover_capabilities()`
//! can return accurate context-window sizes without hardcoding them.
//!
//! # Pipeline
//!
//! [`cure()`] runs the following steps (first success wins):
//!
//! 1. Parse `~/.cache/opencode/models.json` (disk-only, no network).
//! 2. *(with `cure-network` feature)* Fetch `https://openrouter.ai/api/v1/models`.
//! 3. Write a `Hardcoded` marker so future callers skip re-trying.
//!
//! Call once at application init:
//!
//! ```rust,ignore
//! let _ = gate4agent::cure(); // errors are non-fatal
//! ```
//!
//! After this call, `tool.discover_capabilities()` automatically picks up the
//! enriched context-window data.

mod merge;
mod opencode_cache;
mod store;
mod types;

#[cfg(feature = "cure-network")]
mod openrouter;

pub use types::{CureCache, CuredModel, CureError, CureSource};

/// Return the on-disk cure cache without triggering discovery.
///
/// Returns `None` if the cache file does not exist or cannot be parsed.
/// Called internally by [`crate::core::capabilities::discover`].
pub fn load_cure_cache() -> Option<CureCache> {
    store::read_cache()
}

/// Run the model discovery pipeline and persist results to
/// `~/.gate4agent/models.json`.
///
/// The pipeline tries each source in order; the first successful source wins.
/// Failures from individual sources are treated as non-fatal (the function
/// falls through to the next source). Only the final `write_cache` error
/// propagates as `Err`.
///
/// Returns [`Ok(CureSource)`] indicating which source was used.
///
/// Safe to call from any synchronous context. For async callers with the
/// `cure-network` feature enabled, prefer [`cure_async()`] to avoid blocking.
pub fn cure() -> Result<CureSource, CureError> {
    // Source 1: OpenCode disk cache.
    match opencode_cache::load_opencode_models() {
        Ok(tool_models) => {
            let cache = CureCache {
                updated_at: chrono::Utc::now().timestamp(),
                source: CureSource::OpenCodeCache,
                tools: tool_models,
            };
            store::write_cache(&cache)?;
            return Ok(CureSource::OpenCodeCache);
        }
        Err(_) => {
            // Non-fatal: fall through to next source.
        }
    }

    // Source 3: Hardcoded marker (written only if no cache exists yet).
    if store::read_cache().is_none() {
        let cache = CureCache {
            updated_at: chrono::Utc::now().timestamp(),
            source: CureSource::Hardcoded,
            tools: std::collections::HashMap::new(),
        };
        store::write_cache(&cache)?;
    }
    Ok(CureSource::Hardcoded)
}

/// Async variant of [`cure()`] that also tries OpenRouter when the
/// `cure-network` feature is enabled.
///
/// Tries sources in order:
/// 1. OpenCode disk cache (via `tokio::task::spawn_blocking`).
/// 2. OpenRouter HTTP fetch (async, 10-second timeout).
/// 3. Hardcoded marker.
#[cfg(feature = "cure-network")]
pub async fn cure_async() -> Result<CureSource, CureError> {
    // Source 1: OpenCode disk cache (blocking I/O on a dedicated thread).
    let opencode_result =
        tokio::task::spawn_blocking(opencode_cache::load_opencode_models).await;
    if let Ok(Ok(tool_models)) = opencode_result {
        let cache = CureCache {
            updated_at: chrono::Utc::now().timestamp(),
            source: CureSource::OpenCodeCache,
            tools: tool_models,
        };
        store::write_cache(&cache)?;
        return Ok(CureSource::OpenCodeCache);
    }

    // Source 2: OpenRouter live fetch.
    match openrouter::fetch_openrouter_models().await {
        Ok(tool_models) => {
            let cache = CureCache {
                updated_at: chrono::Utc::now().timestamp(),
                source: CureSource::OpenRouter,
                tools: tool_models,
            };
            store::write_cache(&cache)?;
            return Ok(CureSource::OpenRouter);
        }
        Err(_) => {
            // Non-fatal: fall through.
        }
    }

    // Source 3: Hardcoded marker.
    if store::read_cache().is_none() {
        let cache = CureCache {
            updated_at: chrono::Utc::now().timestamp(),
            source: CureSource::Hardcoded,
            tools: std::collections::HashMap::new(),
        };
        store::write_cache(&cache)?;
    }
    Ok(CureSource::Hardcoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;

    /// Mutex to serialize all tests that mutate the HOME environment variable.
    /// Tests within this module and `store::tests` may conflict without serialization.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn temp_home(suffix: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("gate4agent_cure_mod_{}", suffix));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn cure_falls_through_to_hardcoded_when_no_sources() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = temp_home("hardcoded");
        std::env::set_var("HOME", home.to_str().unwrap());

        // Remove any stale cure cache from a prior run.
        let _ = fs::remove_file(home.join(".gate4agent").join("models.json"));

        let result = cure();
        assert!(result.is_ok(), "cure() must not fail: {:?}", result);
        assert_eq!(result.unwrap(), CureSource::Hardcoded);

        let loaded = load_cure_cache();
        assert!(loaded.is_some(), "cache must exist after cure()");
        assert_eq!(loaded.unwrap().source, CureSource::Hardcoded);
    }

    #[test]
    fn cure_with_fake_opencode_cache() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = temp_home("opencode");
        std::env::set_var("HOME", home.to_str().unwrap());

        // Write a minimal fake OpenCode cache.
        let opencode_cache_dir = home.join(".cache").join("opencode");
        fs::create_dir_all(&opencode_cache_dir).unwrap();
        let fake_cache = r#"{
            "anthropic": {
                "id": "anthropic",
                "name": "Anthropic",
                "models": {
                    "claude-opus-4-6": {
                        "name": "Claude Opus 4.6",
                        "limit": { "context": 1000000, "output": 128000 }
                    }
                }
            }
        }"#;
        fs::write(opencode_cache_dir.join("models.json"), fake_cache).unwrap();

        let result = cure();
        assert!(result.is_ok(), "cure() must not fail: {:?}", result);
        assert_eq!(result.unwrap(), CureSource::OpenCodeCache);

        let loaded = load_cure_cache().expect("cache must exist after cure()");
        assert_eq!(loaded.source, CureSource::OpenCodeCache);
        assert!(
            loaded.tools.contains_key("claude_code"),
            "claude_code bucket must be present"
        );
        let models = loaded.tools.get("claude_code").unwrap();
        assert!(!models.is_empty());
        let opus = models.iter().find(|m| m.id == "claude-opus-4-6");
        assert!(opus.is_some(), "claude-opus-4-6 must be in the cache");
        assert_eq!(opus.unwrap().context_window, Some(1_000_000));
    }
}
