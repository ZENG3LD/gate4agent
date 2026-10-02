use gate4agent_node::{
    default_node_endpoint, default_state_path, ManagedWorktreeProfile, NodeServer,
    HistorySourceLayout, NativeHistoryConfig, NativeHistoryRoot, NodeServerConfig,
    WorkspaceConfig, WorktreeServiceMode, NETWORK_ALLOWLIST_CATALOG_ENV,
    resolve_network_allowlist_catalog,
};
use gate4agent_node::protocol::{
    ManagedWorktreeRetention, NodeId, SessionRecordRetentionConfig, WorktreeProfileId,
    WorktreeProfileRevision, WorkspaceId,
};
use gate4agent_types::AdapterId;
use std::collections::BTreeMap;
use std::path::PathBuf;

const NODE_TOKEN_ENV: &str = "GATE4AGENT_NODE_TOKEN";
/// Optional node-local bridge secret (distinct from NODE_TOKEN). Never logged.
const BRIDGE_TOKEN_ENV: &str = "GATE4AGENT_BRIDGE_TOKEN";
/// Tip 6: 64 hex chars = 32-byte underlay AEAD key (never logged; ≠ BRIDGE_TOKEN).
const MESH_UNDERLAY_KEY_ENV: &str = "GATE4AGENT_MESH_UNDERLAY_KEY";
/// Tip 6: underlay probe/auth token barrier (never logged; ≠ BRIDGE_TOKEN).
const MESH_UNDERLAY_TOKEN_ENV: &str = "GATE4AGENT_MESH_UNDERLAY_TOKEN";

#[tokio::main(flavor = "current_thread")]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let mut endpoint = default_node_endpoint()
        .and_then(|path| {
            path.into_os_string().into_string().map_err(|_| {
                gate4agent_node::NodeServerError::InvalidEndpoint
            })
        })
        .unwrap_or_else(|error| fail(&error.to_string()));
    let mut api_listen = "127.0.0.1:18310"
        .parse()
        .expect("the built-in node API listen address must be valid");
    // Opt-in: default off. Loopback-only when set (enforced by with_bridge_listen).
    let mut bridge_listen: Option<std::net::SocketAddr> = None;
    // Tip 6: optional underlay UDP accept (may be non-loopback); relays to bridge_listen.
    let mut bridge_underlay_listen: Option<std::net::SocketAddr> = None;
    let mut call_home: Option<std::net::SocketAddr> = None;
    let mut node_id = None;
    let mut workspaces = Vec::new();
    let mut worktree_modes = BTreeMap::new();
    let mut managed_profiles = Vec::new();
    let mut history_roots = Vec::new();
    let mut harness_mcp_helper: Option<PathBuf> = None;
    let mut network_allowlist_catalog: Option<PathBuf> = None;
    let mut session_record_retention = SessionRecordRetentionConfig::default();
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--endpoint" => endpoint = required_value("--endpoint", args.next()),
            "--api-listen" => {
                let value = required_value("--api-listen", args.next());
                api_listen = value
                    .parse()
                    .unwrap_or_else(|error| fail(&format!("--api-listen is invalid: {error}")));
            }
            // Node-envelope HTTP+WS bridge (separate door from --api-listen ops HTTP).
            // Loopback only. Off unless supplied. Optional GATE4AGENT_BRIDGE_TOKEN.
            "--bridge-listen" => {
                let value = required_value("--bridge-listen", args.next());
                let addr = value.parse().unwrap_or_else(|error| {
                    fail(&format!("--bridge-listen is invalid: {error}"))
                });
                if bridge_listen.replace(addr).is_some() {
                    fail("--bridge-listen may only be supplied once");
                }
            }
            // Tip 6: mesh underlay UDP door → TCP relay to local --bridge-listen.
            // Requires BRIDGE_TOKEN + MESH_UNDERLAY_KEY + MESH_UNDERLAY_TOKEN.
            // HQ dial-only (node accepts as NodePeer). Not a WireGuard daemon.
            "--bridge-underlay-listen" => {
                let value = required_value("--bridge-underlay-listen", args.next());
                let addr = value.parse().unwrap_or_else(|error| {
                    fail(&format!("--bridge-underlay-listen is invalid: {error}"))
                });
                if bridge_underlay_listen.replace(addr).is_some() {
                    fail("--bridge-underlay-listen may only be supplied once");
                }
            }
            // Both default to `0` (disabled) via `SessionRecordRetentionConfig::default`
            // above -- a fresh node must never start deleting durable
            // records until an operator has chosen real values here.
            "--session-record-retention-age-ms" => {
                let value = required_value("--session-record-retention-age-ms", args.next());
                session_record_retention.age_ms = parse_session_record_retention_age_ms(&value)
                    .unwrap_or_else(|error| fail(&error));
            }
            "--session-record-retention-keep" => {
                let value = required_value("--session-record-retention-keep", args.next());
                session_record_retention.keep_per_workspace =
                    parse_session_record_retention_keep(&value)
                        .unwrap_or_else(|error| fail(&error));
            }
            // Dial the relay instead of only waiting to be dialled -- for
            // a node the relay has no way to reach. The node stays the
            // wire's server either way; only who places the call changes.
            "--c2-dial" => {
                let value = required_value("--c2-dial", args.next());
                call_home = Some(value.parse().unwrap_or_else(|error| {
                    fail(&format!("--c2-dial is invalid: {error}"))
                }));
            }
            "--node-id" => {
                let value = required_value("--node-id", args.next());
                let parsed = NodeId::new(value)
                    .unwrap_or_else(|error| fail(&error.to_string()));
                if node_id.replace(parsed).is_some() {
                    fail("--node-id may only be supplied once");
                }
            }
            "--workspace" => {
                let value = required_value("--workspace", args.next());
                let (id, root) = value.split_once('=').unwrap_or_else(|| {
                    fail("--workspace requires ID=ABSOLUTE_PATH")
                });
                let workspace_id = WorkspaceId::new(id)
                    .unwrap_or_else(|error| fail(&error.to_string()));
                workspaces.push(
                    WorkspaceConfig::new(workspace_id, root)
                        .unwrap_or_else(|error| fail(&error.to_string())),
                );
            }
            "--worktree-mode" => {
                let value = required_value("--worktree-mode", args.next());
                let (id, mode) = value.split_once('=').unwrap_or_else(|| {
                    fail("--worktree-mode requires WORKSPACE_ID=manual|managed|off")
                });
                let workspace_id = WorkspaceId::new(id)
                    .unwrap_or_else(|error| fail(&error.to_string()));
                let mode = match mode {
                    "manual" => WorktreeServiceMode::Manual,
                    "managed" => WorktreeServiceMode::Managed,
                    "off" => WorktreeServiceMode::Off,
                    _ => fail("--worktree-mode requires manual, managed, or off"),
                };
                if worktree_modes.insert(workspace_id, mode).is_some() {
                    fail("--worktree-mode may only be supplied once per workspace");
                }
            }
            "--managed-worktree-profile" => {
                let value = required_value("--managed-worktree-profile", args.next());
                let (workspace, fields) = value.split_once('=').unwrap_or_else(|| {
                    fail("--managed-worktree-profile requires WORKSPACE_ID=PROFILE|REVISION|ABS_ROOT|BRANCH_PREFIX|BASE|RETENTION")
                });
                let workspace_id = WorkspaceId::new(workspace)
                    .unwrap_or_else(|error| fail(&error.to_string()));
                let fields = fields.split('|').collect::<Vec<_>>();
                if fields.len() != 6 {
                    fail("--managed-worktree-profile requires exactly six pipe-separated profile fields");
                }
                let retention = match fields[5] {
                    "remove-when-released" => ManagedWorktreeRetention::RemoveWhenReleased,
                    "retain" => ManagedWorktreeRetention::Retain,
                    _ => fail("managed worktree retention must be remove-when-released or retain"),
                };
                let profile = ManagedWorktreeProfile::new(
                    WorktreeProfileId::new(fields[0])
                        .unwrap_or_else(|error| fail(&error.to_string())),
                    WorktreeProfileRevision::new(fields[1])
                        .unwrap_or_else(|error| fail(&error.to_string())),
                    fields[2],
                    fields[3],
                    fields[4],
                    retention,
                ).unwrap_or_else(|error| fail(&error));
                managed_profiles.push((workspace_id, profile));
            }
            "--history-root" => {
                let value = required_value("--history-root", args.next());
                history_roots.push(parse_history_root(&value));
            }
            "--harness-mcp-helper" => {
                let value = PathBuf::from(required_value("--harness-mcp-helper", args.next()));
                if harness_mcp_helper.replace(value).is_some() {
                    fail("--harness-mcp-helper may only be supplied once");
                }
            }
            // Retained as a compatibility no-op. History discovery is opt-in;
            // Gate4Agent never derives provider storage from the process home.
            "--no-default-history" => {}
            // Station network allowlist catalog file (opaque ids, one per line).
            // Empty/unset keeps the empty-default catalog (unknown ids refuse).
            // Dig2 station probe is a separate optional feature — this flag is network ids only.
            "--network-allowlist-catalog" => {
                let value = PathBuf::from(required_value("--network-allowlist-catalog", args.next()));
                if network_allowlist_catalog.replace(value).is_some() {
                    fail("--network-allowlist-catalog may only be supplied once");
                }
            }
            "--help" | "-h" => {
                println!("gate4agent-node --node-id ID --workspace ID=ABSOLUTE_PATH [--worktree-mode ID=manual|managed|off] [--managed-worktree-profile 'ID=PROFILE|REVISION|ABS_ROOT|BRANCH_PREFIX|BASE|RETENTION'] [--history-root 'ADAPTER|LAYOUT|ABS_ROOT'] [--harness-mcp-helper ABSOLUTE_REGULAR_FILE] [--network-allowlist-catalog ABSOLUTE_REGULAR_FILE] [--endpoint ABSOLUTE_LOCAL_ENDPOINT] [--api-listen 127.0.0.1:PORT] [--bridge-listen 127.0.0.1:PORT] [--bridge-underlay-listen ADDR] [--c2-dial 127.0.0.1:PORT] [--session-record-retention-age-ms MILLISECONDS] [--session-record-retention-keep COUNT]");
                println!("RETENTION: remove-when-released or retain");
                println!("--session-record-retention-age-ms/--session-record-retention-keep: retire dead Unavailable managed session records; both default to 0 (disabled)");
                println!("LAYOUT: single-ndjson|single-json|json-or-ndjson|ndjson-with-optional-index|summary-json-with-sibling-ndjson|metadata-json-with-sibling-json|session-json-with-sibling-message-json|readonly-sqlite-projection|state-json-with-index-and-sibling-ndjson");
                println!("control token: {NODE_TOKEN_ENV} environment variable");
                println!("--bridge-listen: opt-in node-envelope HTTP+WS bridge (loopback only; separate from --api-listen)");
                println!("optional bridge token: {BRIDGE_TOKEN_ENV} (never logged; distinct from control token)");
                println!("--bridge-underlay-listen: tip 6 mesh underlay UDP accept → TCP relay to --bridge-listen (requires {BRIDGE_TOKEN_ENV} + {MESH_UNDERLAY_KEY_ENV} + {MESH_UNDERLAY_TOKEN_ENV}; HQ dial-only; no WG daemon)");
                println!("network allowlist catalog: --network-allowlist-catalog or {NETWORK_ALLOWLIST_CATALOG_ENV} (opaque ids, one per line; empty default)");
                println!("--c2-dial: dial a relay's call-home listener instead of waiting to be dialled");
                return;
            }
            unknown => fail(&format!("unknown argument: {unknown}")),
        }
    }
    let token = std::env::var(NODE_TOKEN_ENV)
        .unwrap_or_else(|_| fail(&format!("{NODE_TOKEN_ENV} is required")));
    std::env::remove_var(NODE_TOKEN_ENV);
    let node_id = node_id.unwrap_or_else(|| fail("--node-id is required"));
    for workspace in &mut workspaces {
        if let Some(mode) = worktree_modes.remove(workspace.workspace_id()) {
            *workspace = workspace.clone().with_worktree_service_mode(mode);
        }
    }
    if !worktree_modes.is_empty() {
        fail("--worktree-mode references an unknown workspace");
    }
    for (workspace_id, profile) in managed_profiles {
        let workspace = workspaces.iter_mut()
            .find(|workspace| workspace.workspace_id() == &workspace_id)
            .unwrap_or_else(|| fail("--managed-worktree-profile references an unknown workspace"));
        *workspace = workspace.clone().with_managed_worktree_profile(profile)
            .unwrap_or_else(|error| fail(&error.to_string()));
    }
    let state_path = default_state_path(&node_id).unwrap_or_else(|error| fail(&error.to_string()));
    let config = NodeServerConfig::new(endpoint, token, node_id, workspaces)
        .and_then(|config| config.with_state_path(state_path))
        .and_then(|config| config.with_api_listen(api_listen))
        .and_then(|config| match bridge_listen {
            Some(addr) => config.with_bridge_listen(addr),
            None => Ok(config),
        })
        .and_then(|config| match call_home {
            Some(relay) => config.with_call_home(relay),
            None => Ok(config),
        })
        .unwrap_or_else(|error| fail(&error.to_string()));
    // Optional bridge secret — read then scrub from the process environment.
    // Never print. Distinct from NODE_TOKEN (already removed above).
    let config = match std::env::var(BRIDGE_TOKEN_ENV) {
        Ok(value) => {
            std::env::remove_var(BRIDGE_TOKEN_ENV);
            config
                .with_bridge_token(value)
                .unwrap_or_else(|error| fail(&error.to_string()))
        }
        Err(_) => config,
    };
    let config = match bridge_underlay_listen {
        Some(bind) => {
            let key_hex = std::env::var(MESH_UNDERLAY_KEY_ENV).unwrap_or_else(|_| {
                fail(&format!(
                    "{MESH_UNDERLAY_KEY_ENV} is required with --bridge-underlay-listen (64 hex digits)"
                ))
            });
            std::env::remove_var(MESH_UNDERLAY_KEY_ENV);
            let underlay_token = std::env::var(MESH_UNDERLAY_TOKEN_ENV).unwrap_or_else(|_| {
                fail(&format!(
                    "{MESH_UNDERLAY_TOKEN_ENV} is required with --bridge-underlay-listen"
                ))
            });
            std::env::remove_var(MESH_UNDERLAY_TOKEN_ENV);
            let transport_key = parse_underlay_key_hex(&key_hex)
                .unwrap_or_else(|error| fail(&error));
            config
                .with_bridge_underlay(bind, transport_key, underlay_token)
                .unwrap_or_else(|error| fail(&error.to_string()))
        }
        None => config,
    };
    let config = if let Some(history) = explicit_history_config(history_roots)
        .unwrap_or_else(|error| fail(&error))
    {
        config.with_history(history)
    } else {
        config
    };
    let config = if let Some(helper) = harness_mcp_helper {
        config.with_harness_mcp_helper(helper)
            .unwrap_or_else(|error| fail(&error.to_string()))
    } else {
        config
    };
    let config = config.with_session_record_retention(session_record_retention);
    // Station network allowlist catalog: CLI path wins, else env path, else empty.
    // Dig2 station probe is optional (`dig2-station-probe`). Never print GATE4AGENT_NODE_TOKEN.
    let allowlist_catalog = resolve_network_allowlist_catalog(network_allowlist_catalog)
        .unwrap_or_else(|error| fail(&error.to_string()));
    let config = config
        .with_network_allowlist_catalog(allowlist_catalog)
        .unwrap_or_else(|error| fail(&error.to_string()));
    let server = NodeServer::new(config).unwrap_or_else(|error| fail(&error.to_string()));
    if let Err(error) = server.run_until_ctrl_signal().await {
        fail(&error.to_string());
    }
}

fn explicit_history_config(
    roots: Vec<NativeHistoryRoot>,
) -> Result<Option<NativeHistoryConfig>, String> {
    if roots.is_empty() {
        Ok(None)
    } else {
        NativeHistoryConfig::new(roots)
            .map(Some)
            .map_err(|error| error.to_string())
    }
}

fn parse_history_root(value: &str) -> NativeHistoryRoot {
    let mut fields = value.splitn(3, '|');
    let adapter = fields.next().unwrap_or_default();
    let layout = fields.next().unwrap_or_default();
    let root = fields.next().unwrap_or_default();
    if adapter.is_empty() || layout.is_empty() || root.is_empty() {
        fail("--history-root requires ADAPTER|LAYOUT|ABS_ROOT");
    }
    let adapter = AdapterId::new(adapter)
        .unwrap_or_else(|error| fail(&error.to_string()));
    let layout = parse_history_layout(layout);
    NativeHistoryRoot::new(adapter, layout, root)
        .unwrap_or_else(|error| fail(&error.to_string()))
}

fn parse_history_layout(value: &str) -> HistorySourceLayout {
    match value {
        "single-ndjson" => HistorySourceLayout::SingleNdjson,
        "single-json" => HistorySourceLayout::SingleJson,
        "json-or-ndjson" => HistorySourceLayout::JsonOrNdjson,
        "ndjson-with-optional-index" => HistorySourceLayout::NdjsonWithOptionalIndex,
        "summary-json-with-sibling-ndjson" => {
            HistorySourceLayout::SummaryJsonWithSiblingNdjson
        }
        "metadata-json-with-sibling-json" => {
            HistorySourceLayout::MetadataJsonWithSiblingJson
        }
        "session-json-with-sibling-message-json" => {
            HistorySourceLayout::SessionJsonWithSiblingMessageJson
        }
        "readonly-sqlite-projection" => HistorySourceLayout::ReadOnlySqliteProjection,
        "state-json-with-index-and-sibling-ndjson" => {
            HistorySourceLayout::StateJsonWithIndexAndSiblingNdjson
        }
        _ => fail("--history-root contains an unsupported layout"),
    }
}

fn parse_session_record_retention_age_ms(value: &str) -> Result<u64, String> {
    value.parse()
        .map_err(|error| format!("--session-record-retention-age-ms is invalid: {error}"))
}

fn parse_session_record_retention_keep(value: &str) -> Result<u32, String> {
    value.parse()
        .map_err(|error| format!("--session-record-retention-keep is invalid: {error}"))
}

fn required_value(flag: &str, value: Option<String>) -> String {
    value.unwrap_or_else(|| fail(&format!("{flag} requires a value")))
}


fn parse_underlay_key_hex(value: &str) -> Result<[u8; 32], String> {
    let value = value.trim();
    if value.len() != 64 || !value.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(
            "GATE4AGENT_MESH_UNDERLAY_KEY must be exactly 64 hex digits (32 bytes)".into(),
        );
    }
    let mut out = [0u8; 32];
    for (i, chunk) in value.as_bytes().chunks(2).enumerate() {
        let hi = hex_nibble(chunk[0])?;
        let lo = hex_nibble(chunk[1])?;
        out[i] = (hi << 4) | lo;
    }
    Ok(out)
}

fn hex_nibble(b: u8) -> Result<u8, String> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err("invalid hex digit in GATE4AGENT_MESH_UNDERLAY_KEY".into()),
    }
}

fn fail(message: &str) -> ! {
    eprintln!("gate4agent-node: {message}");
    std::process::exit(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_history_is_disabled_without_explicit_history_root() {
        assert_eq!(explicit_history_config(Vec::new()).unwrap(), None);
    }

    #[test]
    fn explicit_history_root_remains_the_only_history_authority() {
        let root = parse_history_root(
            r"codex|ndjson-with-optional-index|C:\operator-approved-history",
        );
        let config = explicit_history_config(vec![root]).unwrap().unwrap();
        assert_eq!(config.roots().len(), 1);
    }

    #[test]
    fn session_record_retention_age_ms_parses_a_valid_flag_value() {
        assert_eq!(
            parse_session_record_retention_age_ms("604800000").unwrap(),
            604_800_000,
        );
    }

    #[test]
    fn session_record_retention_age_ms_refuses_a_non_numeric_value_by_name() {
        let error = parse_session_record_retention_age_ms("not-a-number").unwrap_err();
        assert!(error.starts_with("--session-record-retention-age-ms is invalid: "));
    }

    #[test]
    fn session_record_retention_keep_parses_a_valid_flag_value() {
        assert_eq!(parse_session_record_retention_keep("32").unwrap(), 32);
    }

    #[test]
    fn session_record_retention_keep_refuses_a_non_numeric_value_by_name() {
        let error = parse_session_record_retention_keep("not-a-number").unwrap_err();
        assert!(error.starts_with("--session-record-retention-keep is invalid: "));
    }
}
