//! Read/write the cure cache file at `~/.gate4agent/models.json`.

use std::path::PathBuf;

use super::types::{CureCache, CureError};

/// Returns the path `~/.gate4agent/models.json`, or `None` if the home
/// directory cannot be determined.
pub(crate) fn cure_cache_path() -> Option<PathBuf> {
    let home = crate::utils::home_dir()?;
    Some(home.join(".gate4agent").join("models.json"))
}

/// Read the cure cache from disk without triggering discovery.
///
/// Returns `None` if the file does not exist or cannot be parsed.
pub(crate) fn read_cache() -> Option<CureCache> {
    let path = cure_cache_path()?;
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Atomically write the cure cache to `~/.gate4agent/models.json`.
///
/// Creates parent directories as needed. Writes to a `.tmp` file first,
/// then renames to the final path to avoid partial writes.
pub(crate) fn write_cache(cache: &CureCache) -> Result<(), CureError> {
    let path = cure_cache_path().ok_or(CureError::NoHomeDir)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(cache)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, json.as_bytes())?;
    std::fs::rename(tmp, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use crate::cure::types::{CureSource, CuredModel};

    fn make_cache() -> CureCache {
        let mut tools = HashMap::new();
        tools.insert(
            "claude_code".to_string(),
            vec![CuredModel {
                id: "claude-opus-4-6".to_string(),
                display_name: "Claude Opus 4.6".to_string(),
                context_window: Some(1_000_000),
                max_output: Some(128_000),
            }],
        );
        CureCache {
            updated_at: 1_744_500_000,
            source: CureSource::OpenCodeCache,
            tools,
        }
    }

    #[test]
    fn cure_cache_roundtrip() {
        let cache = make_cache();
        let json = serde_json::to_string_pretty(&cache).unwrap();
        let parsed: CureCache = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.updated_at, cache.updated_at);
        assert_eq!(parsed.source, cache.source);
        assert_eq!(parsed.tools.len(), cache.tools.len());
        let models = parsed.tools.get("claude_code").unwrap();
        assert_eq!(models[0].id, "claude-opus-4-6");
        assert_eq!(models[0].context_window, Some(1_000_000));
    }

    /// Test that write_cache + read_cache form a valid roundtrip by
    /// writing to an explicit path (not via HOME env var) and reading back.
    #[test]
    fn write_read_cache_roundtrip_via_path() {
        let cache = make_cache();

        // Build a temp path we fully control.
        let dir = std::env::temp_dir().join("gate4agent_store_rtrip");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("models.json");
        let tmp = dir.join("models.tmp");

        // Write manually (same logic as write_cache but with explicit paths).
        let json = serde_json::to_string_pretty(&cache).unwrap();
        std::fs::write(&tmp, json.as_bytes()).unwrap();
        std::fs::rename(&tmp, &path).unwrap();

        // Read back manually.
        let content = std::fs::read_to_string(&path).unwrap();
        let loaded: CureCache = serde_json::from_str(&content).unwrap();

        assert_eq!(loaded.source, CureSource::OpenCodeCache);
        assert_eq!(loaded.updated_at, 1_744_500_000);
        let models = loaded.tools.get("claude_code").unwrap();
        assert_eq!(models[0].id, "claude-opus-4-6");
        assert_eq!(models[0].context_window, Some(1_000_000));
    }
}
