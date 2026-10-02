//! Station **network allowlist** catalog load (opaque policy ids only).
//!
//! Operator startup path for `NodeShared::network_allowlist_catalog`. Empty
//! when unset. File lists one id per line; `#` comments and blank lines are
//! skipped. Never cookies / OAuth / proxy credentials — ids only. Plan
//! `station-network-and-browser-profile-knobs-2026-10-02.md` §2.1 / handoff
//! catalog-enforce gap "Catalog persistence / operator load path".
//! Dig2browser dig2 probe remains stubbed elsewhere.

use crate::protocol::SpawnNetworkAllowlistId;
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Soft bound on station network allowlist catalog membership (ids only).
pub use crate::protocol::MAX_NETWORK_ALLOWLIST_CATALOG_ENTRIES;

/// Env path to an optional allowlist-id catalog file (absolute regular file).
/// Unset → empty catalog (deny unknown at resolve). Never a secret store.
pub const NETWORK_ALLOWLIST_CATALOG_ENV: &str = "GATE4AGENT_NODE_NETWORK_ALLOWLIST_CATALOG";

#[derive(Debug, Error)]
pub enum NetworkAllowlistCatalogError {
    #[error("network allowlist catalog path must be an absolute regular file: {0}")]
    InvalidPath(String),
    #[error("network allowlist catalog could not be read: {0}")]
    Io(#[source] io::Error),
    #[error("network allowlist catalog line {line}: {message}")]
    InvalidEntry { line: usize, message: String },
    #[error("network allowlist catalog exceeds the {max}-entry limit")]
    Capacity { max: usize },
}

/// Load opaque allowlist ids from `path`.
///
/// Format: one id per line; trim ASCII whitespace; skip empty lines and lines
/// whose first non-whitespace character is `#`. Duplicates collapse via set.
/// Refuse relative paths, missing files, non-files, invalid ids, and overflow.
pub fn load_network_allowlist_catalog_file(
    path: impl AsRef<Path>,
) -> Result<BTreeSet<SpawnNetworkAllowlistId>, NetworkAllowlistCatalogError> {
    let path = path.as_ref();
    let display = path.to_string_lossy().into_owned();
    if !path.is_absolute() {
        return Err(NetworkAllowlistCatalogError::InvalidPath(display));
    }
    let metadata = fs::metadata(path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            NetworkAllowlistCatalogError::InvalidPath(display.clone())
        } else {
            NetworkAllowlistCatalogError::Io(error)
        }
    })?;
    if !metadata.is_file() {
        return Err(NetworkAllowlistCatalogError::InvalidPath(display));
    }
    let text = fs::read_to_string(path).map_err(NetworkAllowlistCatalogError::Io)?;
    parse_network_allowlist_catalog_text(&text)
}

/// Parse catalog text (same line rules as [`load_network_allowlist_catalog_file`]).
pub fn parse_network_allowlist_catalog_text(
    text: &str,
) -> Result<BTreeSet<SpawnNetworkAllowlistId>, NetworkAllowlistCatalogError> {
    let mut catalog = BTreeSet::new();
    for (index, raw_line) in text.lines().enumerate() {
        let line = index + 1;
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let id = SpawnNetworkAllowlistId::new(trimmed).map_err(|error| {
            NetworkAllowlistCatalogError::InvalidEntry {
                line,
                message: error.to_string(),
            }
        })?;
        if catalog.len() == MAX_NETWORK_ALLOWLIST_CATALOG_ENTRIES && !catalog.contains(&id) {
            return Err(NetworkAllowlistCatalogError::Capacity {
                max: MAX_NETWORK_ALLOWLIST_CATALOG_ENTRIES,
            });
        }
        catalog.insert(id);
    }
    Ok(catalog)
}

/// Resolve optional catalog from an explicit path, else from
/// [`NETWORK_ALLOWLIST_CATALOG_ENV`], else empty.
pub fn resolve_network_allowlist_catalog(
    explicit_path: Option<PathBuf>,
) -> Result<BTreeSet<SpawnNetworkAllowlistId>, NetworkAllowlistCatalogError> {
    let path = match explicit_path {
        Some(path) => Some(path),
        None => std::env::var_os(NETWORK_ALLOWLIST_CATALOG_ENV).map(PathBuf::from),
    };
    match path {
        Some(path) => load_network_allowlist_catalog_file(path),
        None => Ok(BTreeSet::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_catalog_path(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "gate4agent-node-allowlist-{label}-{}-{unique}.txt",
            std::process::id(),
        ))
    }

    #[test]
    fn unset_path_yields_empty_catalog_when_env_absent() {
        // Isolate from ambient operator env so the empty-default contract stays
        // deterministic in shared CI / agent boxes.
        let previous = std::env::var_os(NETWORK_ALLOWLIST_CATALOG_ENV);
        std::env::remove_var(NETWORK_ALLOWLIST_CATALOG_ENV);
        let catalog = resolve_network_allowlist_catalog(None).unwrap();
        assert!(catalog.is_empty());
        match previous {
            Some(value) => std::env::set_var(NETWORK_ALLOWLIST_CATALOG_ENV, value),
            None => std::env::remove_var(NETWORK_ALLOWLIST_CATALOG_ENV),
        }
    }

    #[test]
    fn explicit_temp_path_wins_over_absent_env() {
        let previous = std::env::var_os(NETWORK_ALLOWLIST_CATALOG_ENV);
        std::env::remove_var(NETWORK_ALLOWLIST_CATALOG_ENV);
        let path = temp_catalog_path("explicit");
        fs::write(&path, "from-explicit\n").unwrap();
        let catalog = resolve_network_allowlist_catalog(Some(path.clone())).unwrap();
        assert!(catalog.contains(&SpawnNetworkAllowlistId::new("from-explicit").unwrap()));
        let _ = fs::remove_file(&path);
        match previous {
            Some(value) => std::env::set_var(NETWORK_ALLOWLIST_CATALOG_ENV, value),
            None => std::env::remove_var(NETWORK_ALLOWLIST_CATALOG_ENV),
        }
    }

    #[test]
    fn load_temp_file_registers_ids_and_skips_comments() {
        let path = temp_catalog_path("ok");
        fs::write(
            &path,
            "# station network allowlist ids (opaque)\n\
             egress-default\n\
             \n\
             # another comment\n\
             lab-egress\n\
             egress-default\n",
        )
        .unwrap();
        let catalog = load_network_allowlist_catalog_file(&path).unwrap();
        assert_eq!(catalog.len(), 2);
        assert!(catalog.contains(&SpawnNetworkAllowlistId::new("egress-default").unwrap()));
        assert!(catalog.contains(&SpawnNetworkAllowlistId::new("lab-egress").unwrap()));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn empty_file_yields_empty_catalog() {
        let path = temp_catalog_path("empty");
        fs::write(&path, "# only comments\n\n").unwrap();
        let catalog = load_network_allowlist_catalog_file(&path).unwrap();
        assert!(catalog.is_empty());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn relative_path_refuses() {
        let error = load_network_allowlist_catalog_file("relative-allowlist.txt").unwrap_err();
        assert!(matches!(
            error,
            NetworkAllowlistCatalogError::InvalidPath(_)
        ));
    }

    #[test]
    fn missing_file_refuses() {
        let path = temp_catalog_path("missing");
        let _ = fs::remove_file(&path);
        let error = load_network_allowlist_catalog_file(&path).unwrap_err();
        assert!(matches!(
            error,
            NetworkAllowlistCatalogError::InvalidPath(_)
        ));
    }

    #[test]
    fn invalid_id_line_refuses_with_line_number() {
        let path = temp_catalog_path("bad-id");
        fs::write(&path, "good-id\nbad/id\n").unwrap();
        let error = load_network_allowlist_catalog_file(&path).unwrap_err();
        match error {
            NetworkAllowlistCatalogError::InvalidEntry { line, message } => {
                assert_eq!(line, 2);
                assert!(!message.is_empty());
            }
            other => panic!("expected InvalidEntry, got {other:?}"),
        }
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn parse_text_capacity_refuses_overflow() {
        let mut body = String::new();
        for index in 0..=MAX_NETWORK_ALLOWLIST_CATALOG_ENTRIES {
            body.push_str(&format!("allow-{index}\n"));
        }
        let error = parse_network_allowlist_catalog_text(&body).unwrap_err();
        assert!(matches!(
            error,
            NetworkAllowlistCatalogError::Capacity { max: MAX_NETWORK_ALLOWLIST_CATALOG_ENTRIES }
        ));
    }
}
