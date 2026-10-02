//! Station **network allowlist** catalog (opaque policy ids + optional
//! node-local permit-set / provider-native mapping).
//!
//! Operator startup path for `NodeShared::network_allowlist_catalog`. Empty
//! when unset. **C2 / spawn wire stays id-only**; inventory lists ids.
//! Never cookies / OAuth / proxy credentials.
//!
//! Dual file formats (Track B §3.2.1):
//! - **v1 id-list** (backward compatible): one id per line; `#` comments /
//!   blanks skipped. Entries are membership-only (empty permits).
//! - **v2 JSON**: `{ "schema_version": 2, "entries": [ { "id", "permits?",
//!   "provider_native?" } ] }`. Detected when the first non-comment /
//!   non-blank content starts with `{`.
//!
//! Plan: `dig2browser-station-probe-and-network-permit-set-2026-10-02.md` §3.2.1.
//! Dig2browser bind remains stubbed elsewhere. Codex provider-native first
//! slice: `-c sandbox_workspace_write.network_access` under Moderate only.

use crate::protocol::SpawnNetworkAllowlistId;
use gate4agent_types::ApprovalLevel;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Soft bound on station network allowlist catalog membership (ids only).
pub use crate::protocol::MAX_NETWORK_ALLOWLIST_CATALOG_ENTRIES;

/// Soft bound on permit rows per catalog entry (dig2 `MAX_NETWORK_PERMITS` spirit).
pub const MAX_NETWORK_PERMITS_PER_ENTRY: usize = 64;

/// Soft bound on peer string bytes (non-secret host:port / CIDR sketch).
pub const MAX_NETWORK_PERMIT_PEER_BYTES: usize = 256;

/// Structured catalog schema version (JSON v2).
pub const NETWORK_ALLOWLIST_CATALOG_SCHEMA_VERSION: u32 = 2;

/// Env path to an optional allowlist catalog file (absolute regular file).
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
    #[error("network allowlist catalog: {0}")]
    InvalidCatalog(String),
    #[error("network allowlist catalog exceeds the {max}-entry limit")]
    Capacity { max: usize },
}

/// dig2browser-spirit peer+protocol permit (node-local; non-secret).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkPermitSketch {
    pub protocol: NetworkPermitProtocol,
    pub peer: String,
}

/// Allowed permit protocols (string form in JSON: `tcp` / `udp`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkPermitProtocol {
    Tcp,
    Udp,
}

impl NetworkPermitProtocol {
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "tcp" => Ok(Self::Tcp),
            "udp" => Ok(Self::Udp),
            other => Err(format!(
                "unsupported network permit protocol {other:?} (expected tcp or udp)"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        }
    }
}

/// Honest, partial provider-native network mapping (node-local).
///
/// Only measured / documented knobs appear here. Unknown JSON keys refuse at
/// load (`deny_unknown_fields`). Claude network is **settings-shaped**
/// (`sandbox.network.allowedDomains` / `--settings`) — not a Codex-style
/// bool argv; Kimi has **no** first-party network allowlist on the CLI.
/// Omit invented `claude_*` / `kimi_*` fields rather than fake flags; resolve
/// refuses when a required Codex-only mapping is asked of a non-Codex
/// provider. Inventory:
/// hatchery-websession-docs
/// `research/claude-kimi-network-argv-vs-station-catalog-2026-10-02.md`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderNativeNetworkSketch {
    /// Codex `networkAccess`-shaped knob when measured. `None` = unmapped.
    #[serde(default)]
    pub codex_network_access: Option<bool>,
}

impl ProviderNativeNetworkSketch {
    pub fn is_empty(&self) -> bool {
        self.codex_network_access.is_none()
    }
}

/// One node-local catalog entry. Wire / inventory still use [`SpawnNetworkAllowlistId`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkAllowlistEntry {
    pub id: SpawnNetworkAllowlistId,
    /// Empty = id membership only (v1 id-list behavior).
    pub permits: Vec<NetworkPermitSketch>,
    pub provider_native: Option<ProviderNativeNetworkSketch>,
}

impl NetworkAllowlistEntry {
    pub fn membership_only(id: SpawnNetworkAllowlistId) -> Self {
        Self {
            id,
            permits: Vec::new(),
            provider_native: None,
        }
    }
}

/// Empty-default station network allowlist catalog.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetworkAllowlistCatalog {
    entries: BTreeMap<SpawnNetworkAllowlistId, NetworkAllowlistEntry>,
}

impl NetworkAllowlistCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn contains(&self, id: &SpawnNetworkAllowlistId) -> bool {
        self.entries.contains_key(id)
    }

    pub fn get(&self, id: &SpawnNetworkAllowlistId) -> Option<&NetworkAllowlistEntry> {
        self.entries.get(id)
    }

    /// Opaque ids in sorted order (LaunchInventory surface).
    pub fn ids(&self) -> impl Iterator<Item = &SpawnNetworkAllowlistId> {
        self.entries.keys()
    }

    pub fn iter(&self) -> impl Iterator<Item = &NetworkAllowlistEntry> {
        self.entries.values()
    }

    /// Insert or replace. Refuses when at capacity and `id` is new.
    pub fn insert(
        &mut self,
        entry: NetworkAllowlistEntry,
    ) -> Result<(), NetworkAllowlistCatalogError> {
        if self.entries.len() == MAX_NETWORK_ALLOWLIST_CATALOG_ENTRIES
            && !self.entries.contains_key(&entry.id)
        {
            return Err(NetworkAllowlistCatalogError::Capacity {
                max: MAX_NETWORK_ALLOWLIST_CATALOG_ENTRIES,
            });
        }
        self.entries.insert(entry.id.clone(), entry);
        Ok(())
    }

    /// Convenience: register membership-only id (tests / in-memory config).
    pub fn insert_id(
        &mut self,
        id: SpawnNetworkAllowlistId,
    ) -> Result<(), NetworkAllowlistCatalogError> {
        self.insert(NetworkAllowlistEntry::membership_only(id))
    }
}

impl FromIterator<SpawnNetworkAllowlistId> for NetworkAllowlistCatalog {
    fn from_iter<T: IntoIterator<Item = SpawnNetworkAllowlistId>>(iter: T) -> Self {
        let mut catalog = Self::new();
        for id in iter {
            // Capacity: drop extras silently only in FromIterator would hide
            // overflow — callers that need refuse use `insert` / loader.
            let _ = catalog.insert_id(id);
        }
        catalog
    }
}

impl FromIterator<NetworkAllowlistEntry> for NetworkAllowlistCatalog {
    fn from_iter<T: IntoIterator<Item = NetworkAllowlistEntry>>(iter: T) -> Self {
        let mut catalog = Self::new();
        for entry in iter {
            let _ = catalog.insert(entry);
        }
        catalog
    }
}

/// Load catalog from `path` (absolute regular file). Dual format.
pub fn load_network_allowlist_catalog_file(
    path: impl AsRef<Path>,
) -> Result<NetworkAllowlistCatalog, NetworkAllowlistCatalogError> {
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

/// Parse catalog text: JSON v2 when first non-comment content is `{`, else
/// v1 id-list (one id per line).
pub fn parse_network_allowlist_catalog_text(
    text: &str,
) -> Result<NetworkAllowlistCatalog, NetworkAllowlistCatalogError> {
    if looks_like_json_catalog(text) {
        parse_network_allowlist_catalog_json(text)
    } else {
        parse_network_allowlist_catalog_id_list(text)
    }
}

/// Resolve optional catalog from an explicit path, else from
/// [`NETWORK_ALLOWLIST_CATALOG_ENV`], else empty.
pub fn resolve_network_allowlist_catalog(
    explicit_path: Option<PathBuf>,
) -> Result<NetworkAllowlistCatalog, NetworkAllowlistCatalogError> {
    let path = match explicit_path {
        Some(path) => Some(path),
        None => std::env::var_os(NETWORK_ALLOWLIST_CATALOG_ENV).map(PathBuf::from),
    };
    match path {
        Some(path) => load_network_allowlist_catalog_file(path),
        None => Ok(NetworkAllowlistCatalog::new()),
    }
}

fn looks_like_json_catalog(text: &str) -> bool {
    for raw_line in text.lines() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        return trimmed.starts_with('{');
    }
    false
}

fn parse_network_allowlist_catalog_id_list(
    text: &str,
) -> Result<NetworkAllowlistCatalog, NetworkAllowlistCatalogError> {
    let mut catalog = NetworkAllowlistCatalog::new();
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
        catalog.insert(NetworkAllowlistEntry::membership_only(id))?;
    }
    Ok(catalog)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogFileV2 {
    schema_version: u32,
    #[serde(default)]
    entries: Vec<CatalogEntryV2>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogEntryV2 {
    id: String,
    #[serde(default)]
    permits: Vec<CatalogPermitV2>,
    #[serde(default)]
    provider_native: Option<ProviderNativeNetworkSketch>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogPermitV2 {
    protocol: String,
    peer: String,
}

fn strip_leading_comments_and_blanks(text: &str) -> &str {
    let mut rest = text;
    loop {
        let trimmed_start = rest.trim_start();
        if trimmed_start.is_empty() {
            return trimmed_start;
        }
        // Advance past leading blank lines already handled by trim_start.
        rest = trimmed_start;
        if rest.starts_with('#') {
            rest = match rest.split_once('\n') {
                Some((_, after)) => after,
                None => return "",
            };
            continue;
        }
        return rest;
    }
}

fn parse_network_allowlist_catalog_json(
    text: &str,
) -> Result<NetworkAllowlistCatalog, NetworkAllowlistCatalogError> {
    let json_text = strip_leading_comments_and_blanks(text);
    let file: CatalogFileV2 = serde_json::from_str(json_text).map_err(|error| {
        NetworkAllowlistCatalogError::InvalidCatalog(format!("invalid JSON catalog: {error}"))
    })?;
    if file.schema_version != NETWORK_ALLOWLIST_CATALOG_SCHEMA_VERSION {
        return Err(NetworkAllowlistCatalogError::InvalidCatalog(format!(
            "unsupported schema_version {} (expected {})",
            file.schema_version, NETWORK_ALLOWLIST_CATALOG_SCHEMA_VERSION
        )));
    }
    let mut catalog = NetworkAllowlistCatalog::new();
    for (index, raw) in file.entries.into_iter().enumerate() {
        let entry_label = index + 1;
        let id = SpawnNetworkAllowlistId::new(&raw.id).map_err(|error| {
            NetworkAllowlistCatalogError::InvalidCatalog(format!(
                "entries[{entry_label}].id: {error}"
            ))
        })?;
        if catalog.contains(&id) {
            return Err(NetworkAllowlistCatalogError::InvalidCatalog(format!(
                "duplicate catalog id {}",
                id.as_str()
            )));
        }
        if raw.permits.len() > MAX_NETWORK_PERMITS_PER_ENTRY {
            return Err(NetworkAllowlistCatalogError::InvalidCatalog(format!(
                "entries[{entry_label}]: exceeds {MAX_NETWORK_PERMITS_PER_ENTRY} permits"
            )));
        }
        let mut permits = Vec::with_capacity(raw.permits.len());
        for (permit_index, permit) in raw.permits.into_iter().enumerate() {
            let protocol = NetworkPermitProtocol::parse(&permit.protocol).map_err(|message| {
                NetworkAllowlistCatalogError::InvalidCatalog(format!(
                    "entries[{entry_label}].permits[{}]: {message}",
                    permit_index + 1
                ))
            })?;
            let peer = permit.peer.trim();
            if peer.is_empty() {
                return Err(NetworkAllowlistCatalogError::InvalidCatalog(format!(
                    "entries[{entry_label}].permits[{}]: peer must be non-empty",
                    permit_index + 1
                )));
            }
            if peer.len() > MAX_NETWORK_PERMIT_PEER_BYTES {
                return Err(NetworkAllowlistCatalogError::InvalidCatalog(format!(
                    "entries[{entry_label}].permits[{}]: peer exceeds {MAX_NETWORK_PERMIT_PEER_BYTES} bytes",
                    permit_index + 1
                )));
            }
            // Non-secret sketch only — refuse obvious credential-shaped keys in peer.
            if peer.contains('@') && peer.contains(':') && peer.split('@').next().is_some_and(|u| u.contains(':')) {
                // user:pass@host — refuse; credentials never belong in catalog.
                return Err(NetworkAllowlistCatalogError::InvalidCatalog(format!(
                    "entries[{entry_label}].permits[{}]: peer must not carry credentials",
                    permit_index + 1
                )));
            }
            permits.push(NetworkPermitSketch {
                protocol,
                peer: peer.to_owned(),
            });
        }
        let provider_native = match raw.provider_native {
            Some(native) if native.is_empty() => None,
            other => other,
        };
        catalog.insert(NetworkAllowlistEntry {
            id,
            permits,
            provider_native,
        })?;
    }
    Ok(catalog)
}


/// Whether `provider_native` on a catalog entry can be honored for `provider`
/// at the resolved [`ApprovalLevel`].
///
/// Codex first slice (research `codex-network-access-mapping-2026-10-02`):
/// `codex_network_access` maps only under **Moderate** (workspace-write).
/// ReadOnly / FullAuto / Unmanaged refuse clearly rather than silent ambient.
/// Non-Codex providers refuse Codex-only knobs.
pub fn provider_native_mapping_supported(
    provider: &str,
    approval_level: ApprovalLevel,
    native: &ProviderNativeNetworkSketch,
) -> Result<(), String> {
    if native.is_empty() {
        return Ok(());
    }
    if native.codex_network_access.is_some() {
        if provider != "codex" {
            return Err(format!(
                "provider-native codex_network_access is unsupported for provider {provider:?}"
            ));
        }
        match approval_level {
            ApprovalLevel::Moderate => Ok(()),
            ApprovalLevel::ReadOnly => Err(
                "provider-native codex_network_access requires Codex Moderate (workspace-write); ReadOnly has no workspace-write network axis"
                    .to_owned(),
            ),
            ApprovalLevel::FullAuto => Err(
                "provider-native codex_network_access is unsupported under Codex FullAuto (sandbox bypass); refuse clear mapping rather than silent no-op"
                    .to_owned(),
            ),
            ApprovalLevel::Unmanaged => Err(
                "provider-native codex_network_access requires Codex Moderate (workspace-write); Unmanaged has no sandbox contract"
                    .to_owned(),
            ),
        }
    } else {
        Ok(())
    }
}

/// Codex `-c` overlay for legacy `sandbox_workspace_write.network_access`.
///
/// Same argv channel as catalog `windows_wsl_setup_acknowledged=true`.
pub fn codex_network_access_config_overlay(enabled: bool) -> Vec<String> {
    vec![
        "-c".to_owned(),
        format!(
            "sandbox_workspace_write.network_access={}",
            if enabled { "true" } else { "false" }
        ),
    ]
}

/// Resolve provider-native launch overlay argv for a catalog entry, or refuse.
///
/// Empty when `native` is empty / unset. Codex Moderate + `Some(flag)` yields
/// [`codex_network_access_config_overlay`]. Other cases refuse via
/// [`provider_native_mapping_supported`].
pub fn resolve_provider_native_launch_overlay(
    provider: &str,
    approval_level: ApprovalLevel,
    native: &ProviderNativeNetworkSketch,
) -> Result<Vec<String>, String> {
    provider_native_mapping_supported(provider, approval_level, native)?;
    match native.codex_network_access {
        Some(flag) if provider == "codex" && approval_level == ApprovalLevel::Moderate => {
            Ok(codex_network_access_config_overlay(flag))
        }
        _ => Ok(Vec::new()),
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
        let entry = catalog
            .get(&SpawnNetworkAllowlistId::new("egress-default").unwrap())
            .unwrap();
        assert!(entry.permits.is_empty());
        assert!(entry.provider_native.is_none());
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
            NetworkAllowlistCatalogError::Capacity {
                max: MAX_NETWORK_ALLOWLIST_CATALOG_ENTRIES
            }
        ));
    }

    #[test]
    fn json_v2_loads_permits_and_provider_native() {
        let text = r#"
        {
          "schema_version": 2,
          "entries": [
            {
              "id": "egress-default",
              "permits": [
                { "protocol": "tcp", "peer": "127.0.0.1:443" },
                { "protocol": "UDP", "peer": "10.0.0.1:53" }
              ],
              "provider_native": { "codex_network_access": true }
            },
            { "id": "lab-egress" }
          ]
        }
        "#;
        let catalog = parse_network_allowlist_catalog_text(text).unwrap();
        assert_eq!(catalog.len(), 2);
        let entry = catalog
            .get(&SpawnNetworkAllowlistId::new("egress-default").unwrap())
            .unwrap();
        assert_eq!(entry.permits.len(), 2);
        assert_eq!(entry.permits[0].protocol, NetworkPermitProtocol::Tcp);
        assert_eq!(entry.permits[0].peer, "127.0.0.1:443");
        assert_eq!(entry.permits[1].protocol, NetworkPermitProtocol::Udp);
        let native = entry.provider_native.as_ref().unwrap();
        assert_eq!(native.codex_network_access, Some(true));
        let bare = catalog
            .get(&SpawnNetworkAllowlistId::new("lab-egress").unwrap())
            .unwrap();
        assert!(bare.permits.is_empty());
        assert!(bare.provider_native.is_none());
    }

    #[test]
    fn json_v2_refuses_unsupported_schema_version() {
        let text = r#"{ "schema_version": 99, "entries": [] }"#;
        let error = parse_network_allowlist_catalog_text(text).unwrap_err();
        assert!(matches!(
            error,
            NetworkAllowlistCatalogError::InvalidCatalog(_)
        ));
        assert!(error.to_string().contains("schema_version"));
    }

    #[test]
    fn json_v2_refuses_unknown_provider_native_keys() {
        // Claude/Kimi have no Codex-style provider_native bool — invented
        // keys must refuse at load (deny_unknown_fields), same honesty as
        // UnsupportedNetworkAllowlistMapping at resolve for non-Codex.
        for bad_key in ["claude_network", "kimi_network", "allowed_domains"] {
            let text = format!(
                r#"{{
                  "schema_version": 2,
                  "entries": [{{
                    "id": "egress-default",
                    "provider_native": {{ "{bad_key}": true }}
                  }}]
                }}"#
            );
            let error = parse_network_allowlist_catalog_text(&text).unwrap_err();
            assert!(matches!(
                error,
                NetworkAllowlistCatalogError::InvalidCatalog(_)
            ));
            let message = error.to_string();
            assert!(
                message.contains(bad_key) || message.contains("unknown field"),
                "key={bad_key} message={message}"
            );
        }
    }

    #[test]
    fn json_v2_refuses_unsupported_permit_protocol() {
        let text = r#"
        {
          "schema_version": 2,
          "entries": [{
            "id": "egress-default",
            "permits": [{ "protocol": "quic", "peer": "127.0.0.1:443" }]
          }]
        }
        "#;
        let error = parse_network_allowlist_catalog_text(text).unwrap_err();
        assert!(error.to_string().contains("quic"));
    }

    #[test]
    fn json_v2_comment_preamble_still_detects_json() {
        let text = "# operator catalog\n{ \"schema_version\": 2, \"entries\": [{\"id\":\"a\"}] }\n";
        let catalog = parse_network_allowlist_catalog_text(text).unwrap();
        assert!(catalog.contains(&SpawnNetworkAllowlistId::new("a").unwrap()));
    }

    #[test]
    fn provider_native_mapping_refuses_non_codex() {
        let native = ProviderNativeNetworkSketch {
            codex_network_access: Some(true),
        };
        let err = provider_native_mapping_supported(
            "claude",
            ApprovalLevel::Moderate,
            &native,
        )
        .unwrap_err();
        assert!(err.contains("claude"));
        assert!(provider_native_mapping_supported(
            "codex",
            ApprovalLevel::Moderate,
            &native,
        )
        .is_ok());
        assert!(provider_native_mapping_supported(
            "claude",
            ApprovalLevel::Moderate,
            &ProviderNativeNetworkSketch::default()
        )
        .is_ok());
    }

    #[test]
    fn provider_native_mapping_refuses_codex_readonly_and_full_auto() {
        let native = ProviderNativeNetworkSketch {
            codex_network_access: Some(true),
        };
        let ro = provider_native_mapping_supported(
            "codex",
            ApprovalLevel::ReadOnly,
            &native,
        )
        .unwrap_err();
        assert!(ro.contains("ReadOnly") || ro.contains("workspace-write"));
        let fa = provider_native_mapping_supported(
            "codex",
            ApprovalLevel::FullAuto,
            &native,
        )
        .unwrap_err();
        assert!(fa.contains("FullAuto"));
        let un = provider_native_mapping_supported(
            "codex",
            ApprovalLevel::Unmanaged,
            &native,
        )
        .unwrap_err();
        assert!(un.contains("Unmanaged") || un.contains("workspace-write"));
    }

    #[test]
    fn codex_network_access_overlay_uses_legacy_c_channel() {
        assert_eq!(
            codex_network_access_config_overlay(true),
            ["-c", "sandbox_workspace_write.network_access=true"]
        );
        assert_eq!(
            codex_network_access_config_overlay(false),
            ["-c", "sandbox_workspace_write.network_access=false"]
        );
        let overlay = resolve_provider_native_launch_overlay(
            "codex",
            ApprovalLevel::Moderate,
            &ProviderNativeNetworkSketch {
                codex_network_access: Some(true),
            },
        )
        .unwrap();
        assert_eq!(overlay, codex_network_access_config_overlay(true));
        assert!(resolve_provider_native_launch_overlay(
            "codex",
            ApprovalLevel::FullAuto,
            &ProviderNativeNetworkSketch {
                codex_network_access: Some(false),
            },
        )
        .is_err());
    }
}
