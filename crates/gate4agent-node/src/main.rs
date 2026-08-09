use gate4agent_node::{
    default_node_endpoint, default_state_path, ManagedWorktreeProfile, NodeServer,
    NodeServerConfig, WorkspaceConfig, WorktreeServiceMode,
};
use gate4agent_node::protocol::{
    ManagedWorktreeRetention, NodeId, WorktreeProfileId, WorktreeProfileRevision, WorkspaceId,
};
use std::collections::BTreeMap;

const NODE_TOKEN_ENV: &str = "GATE4AGENT_NODE_TOKEN";

#[tokio::main(flavor = "current_thread")]
async fn main() {
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
    let mut node_id = None;
    let mut workspaces = Vec::new();
    let mut worktree_modes = BTreeMap::new();
    let mut managed_profiles = Vec::new();
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
            "--help" | "-h" => {
                println!("gate4agent-node --node-id ID --workspace ID=ABSOLUTE_PATH [--worktree-mode ID=manual|managed|off] [--managed-worktree-profile 'ID=PROFILE|REVISION|ABS_ROOT|BRANCH_PREFIX|BASE|RETENTION'] [--endpoint ABSOLUTE_LOCAL_ENDPOINT] [--api-listen 127.0.0.1:PORT]");
                println!("RETENTION: remove-when-released or retain");
                println!("control token: {NODE_TOKEN_ENV} environment variable");
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
        .unwrap_or_else(|error| fail(&error.to_string()));
    let server = NodeServer::new(config).unwrap_or_else(|error| fail(&error.to_string()));
    if let Err(error) = server.run_until_ctrl_signal().await {
        fail(&error.to_string());
    }
}

fn required_value(flag: &str, value: Option<String>) -> String {
    value.unwrap_or_else(|| fail(&format!("{flag} requires a value")))
}

fn fail(message: &str) -> ! {
    eprintln!("gate4agent-node: {message}");
    std::process::exit(2)
}
