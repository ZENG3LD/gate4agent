use gate4agent_node::{
    default_node_endpoint, default_state_path, NodeServer, NodeServerConfig, WorkspaceConfig,
};
use gate4agent_node::protocol::{NodeId, WorkspaceId};

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
            "--help" | "-h" => {
                println!("gate4agent-node --node-id ID --workspace ID=ABSOLUTE_PATH [--workspace ID=ABSOLUTE_PATH ...] [--endpoint ABSOLUTE_LOCAL_ENDPOINT] [--api-listen 127.0.0.1:PORT]");
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
