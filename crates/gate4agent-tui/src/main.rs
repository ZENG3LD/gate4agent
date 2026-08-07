use std::collections::BTreeSet;
use std::str::FromStr;

use gate4agent_node::protocol::{NodeId, WorkspaceId};
use gate4agent_tui::{NodeEndpoint, Provider, PtyColorMode, RunOptions, StartupRequest};

const TOKEN_ENV_PREFIX: &str = "GATE4AGENT_NODE_TOKEN_";

fn value(args: &[String], index: &mut usize, flag: &str) -> Result<String, String> {
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn token_env_name(node_id: &NodeId) -> String {
    format!(
        "{TOKEN_ENV_PREFIX}{}",
        node_id.as_str().to_ascii_uppercase().replace('-', "_")
    )
}

fn parse_node(value: &str) -> Result<(NodeId, String), String> {
    let (node_id, endpoint) = value
        .split_once('=')
        .ok_or_else(|| "--node must be NODE_ID=PIPE".to_owned())?;
    let node_id = NodeId::new(node_id).map_err(|error| error.to_string())?;
    if endpoint.is_empty() {
        return Err("--node pipe endpoint cannot be empty".to_owned());
    }
    Ok((node_id, endpoint.to_owned()))
}

fn parse_args_from(
    args: &[String],
    mut read_secret: impl FnMut(&str) -> Result<String, String>,
) -> Result<RunOptions, String> {
    let mut node_specs = Vec::new();
    let mut startup_node = None;
    let mut workspace = None;
    let mut provider = None;
    let mut color_mode = PtyColorMode::Inherited;
    let mut index = 1;

    while index < args.len() {
        match args[index].as_str() {
            "--node" => node_specs.push(parse_node(&value(args, &mut index, "--node")?)?),
            "--startup-node" => {
                startup_node = Some(
                    NodeId::new(value(args, &mut index, "--startup-node")?)
                        .map_err(|error| error.to_string())?,
                )
            }
            "--workspace" => {
                workspace = Some(
                    WorkspaceId::new(value(args, &mut index, "--workspace")?)
                        .map_err(|error| error.to_string())?,
                )
            }
            "--agent" => {
                provider = Some(Provider::from_str(&value(args, &mut index, "--agent")?)?)
            }
            "--style" => {
                color_mode = PtyColorMode::from_str(&value(args, &mut index, "--style")?)?
            }
            "--help" | "-h" => {
                return Err(
                    "usage: gate4agent-tui --node NODE_ID=PIPE [--node NODE_ID=PIPE ...]\n\
                     token env: GATE4AGENT_NODE_TOKEN_<NORMALIZED_NODE_ID>\n\
                     optional startup: --startup-node NODE_ID --workspace WORKSPACE_ID \
                     --agent claude|codex|kimi [--style inherit|gate]"
                        .to_owned(),
                )
            }
            unknown => return Err(format!("unknown argument: {unknown}")),
        }
        index += 1;
    }
    if node_specs.is_empty() {
        return Err("at least one explicit --node NODE_ID=PIPE is required".to_owned());
    }
    let mut unique = BTreeSet::new();
    let mut nodes = Vec::new();
    for (node_id, endpoint) in node_specs {
        if !unique.insert(node_id.clone()) {
            return Err(format!("duplicate --node for {node_id}"));
        }
        let token_env = token_env_name(&node_id);
        let token = read_secret(&token_env)?;
        if token.is_empty() {
            return Err(format!("{token_env} must not be empty"));
        }
        nodes.push(NodeEndpoint {
            expected_node_id: node_id,
            endpoint,
            token,
        });
    }

    let startup_requested = startup_node.is_some() || workspace.is_some() || provider.is_some();
    let startup = if startup_requested {
        let node_id = startup_node
            .ok_or_else(|| "startup requires --startup-node".to_owned())?;
        let workspace_id = workspace
            .ok_or_else(|| "startup requires --workspace".to_owned())?;
        let provider = provider.ok_or_else(|| "startup requires --agent".to_owned())?;
        if !nodes.iter().any(|node| node.expected_node_id == node_id) {
            return Err(format!("startup node {node_id} is not configured by --node"));
        }
        Some(StartupRequest {
            node_id,
            workspace_id,
            provider,
        })
    } else {
        None
    };
    Ok(RunOptions { nodes, startup, color_mode })
}

fn parse_args() -> Result<RunOptions, String> {
    let args = std::env::args().collect::<Vec<_>>();
    parse_args_from(&args, |name| {
        let value = std::env::var(name)
            .map_err(|_| format!("{name} is required and must be valid Unicode"))?;
        std::env::remove_var(name);
        Ok(value)
    })
}

#[tokio::main]
async fn main() {
    let options = match parse_args() {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(if message.starts_with("usage:") { 0 } else { 2 });
        }
    };
    if let Err(error) = gate4agent_tui::run(options).await {
        eprintln!("gate4agent-tui: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn parse(args: &[&str], secrets: &[(&str, &str)]) -> Result<RunOptions, String> {
        let args = args.iter().map(|value| value.to_string()).collect::<Vec<_>>();
        let secrets = secrets
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect::<BTreeMap<_, _>>();
        parse_args_from(&args, |name| {
            secrets.get(name).cloned().ok_or_else(|| format!("missing {name}"))
        })
    }

    #[test]
    fn repeated_nodes_use_separate_normalized_token_environments() {
        let options = parse(
            &[
                "gate4agent-tui",
                "--node",
                r"desk-a=\\.\pipe\desk-a",
                "--node",
                r"lab_2=\\.\pipe\lab-2",
            ],
            &[
                ("GATE4AGENT_NODE_TOKEN_DESK_A", "desk-token"),
                ("GATE4AGENT_NODE_TOKEN_LAB_2", "lab-token"),
            ],
        )
        .unwrap();
        assert_eq!(options.nodes.len(), 2);
        assert_eq!(options.nodes[0].expected_node_id.as_str(), "desk-a");
        assert_eq!(options.nodes[1].token, "lab-token");
    }

    #[test]
    fn startup_requires_explicit_configured_node_and_workspace() {
        let error = parse(
            &[
                "gate4agent-tui",
                "--node",
                r"desk-a=\\.\pipe\desk-a",
                "--agent",
                "codex",
            ],
            &[("GATE4AGENT_NODE_TOKEN_DESK_A", "token")],
        )
        .err()
        .unwrap();
        assert_eq!(error, "startup requires --startup-node");
    }

    #[test]
    fn cwd_is_not_a_supported_tui_argument() {
        let error = parse(
            &[
                "gate4agent-tui",
                "--node",
                r"desk-a=\\.\pipe\desk-a",
                "--cwd",
                r"C:\work",
            ],
            &[("GATE4AGENT_NODE_TOKEN_DESK_A", "token")],
        )
        .err()
        .unwrap();
        assert_eq!(error, "unknown argument: --cwd");
    }

    #[test]
    fn style_is_explicit_and_defaults_to_terminal_inheritance() {
        let inherited = parse(
            &["gate4agent-tui", "--node", r"desk-a=\\.\pipe\desk-a"],
            &[("GATE4AGENT_NODE_TOKEN_DESK_A", "token")],
        )
        .unwrap();
        assert_eq!(inherited.color_mode, PtyColorMode::Inherited);
        let gate = parse(
            &[
                "gate4agent-tui",
                "--node",
                r"desk-a=\\.\pipe\desk-a",
                "--style",
                "gate",
            ],
            &[("GATE4AGENT_NODE_TOKEN_DESK_A", "token")],
        )
        .unwrap();
        assert_eq!(gate.color_mode, PtyColorMode::GateOverride);
    }

}
