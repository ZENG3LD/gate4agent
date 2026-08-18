use std::str::FromStr;

use gate4agent_node_protocol::{NodeId, WorkspaceId};
use gate4agent_tui::{
    C2Endpoint, Provider, PtyColorMode, RunOptions, StartupMode, StartupRequest,
};

const C2_TOKEN_ENV: &str = "GATE4AGENT_C2_TOKEN";

fn value(args: &[String], index: &mut usize, flag: &str) -> Result<String, String> {
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn parse_args_from(
    args: &[String],
    mut read_secret: impl FnMut(&str) -> Result<String, String>,
) -> Result<RunOptions, String> {
    let mut c2_control = None;
    let mut startup_node = None;
    let mut workspace = None;
    let mut provider = None;
    let mut color_mode_override = None;
    let mut index = 1;

    while index < args.len() {
        match args[index].as_str() {
            "--node" => {
                return Err("direct Node mode is disabled; use --c2-control".to_owned());
            }
            "--c2-control" => {
                if c2_control.is_some() {
                    return Err("--c2-control can be specified only once".to_owned());
                }
                c2_control = Some(value(args, &mut index, "--c2-control")?);
            }
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
                provider = Some(
                    Provider::from_str(&value(args, &mut index, "--agent")?)
                        .map_err(|error| error.to_string())?,
                )
            }
            "--style" => {
                color_mode_override = Some(PtyColorMode::from_str(&value(args, &mut index, "--style")?)?)
            }
            "--help" | "-h" => {
                return Err(
                    "usage: gate4agent-tui-light --c2-control PIPE\n\
                     credential env: GATE4AGENT_C2_TOKEN\n\
                     optional startup: --startup-node NODE_ID --workspace WORKSPACE_ID \
                     --agent PROVIDER_ID [--style inherit|gate]"
                        .to_owned(),
                )
            }
            unknown => return Err(format!("unknown argument: {unknown}")),
        }
        index += 1;
    }

    let endpoint = c2_control
        .ok_or_else(|| "configure --c2-control PIPE".to_owned())?;
    let token = read_secret(C2_TOKEN_ENV)?;
    if token.is_empty() {
        return Err(format!("{C2_TOKEN_ENV} must not be empty"));
    }
    let startup_requested = startup_node.is_some() || workspace.is_some() || provider.is_some();
    let startup = if startup_requested {
        let node_id = startup_node
            .ok_or_else(|| "startup requires --startup-node".to_owned())?;
        let workspace_id = workspace
            .ok_or_else(|| "startup requires --workspace".to_owned())?;
        let provider = provider.ok_or_else(|| "startup requires --agent".to_owned())?;
        Some(StartupRequest {
            node_id,
            workspace_id,
            provider,
        })
    } else {
        None
    };
    Ok(RunOptions {
        mode: StartupMode::ManualC2(C2Endpoint { endpoint, token }),
        startup,
        color_mode_override,
    })
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
    gate4agent_tui::diagnostics::install_panic_hook();
    let options = match parse_args() {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(if message.starts_with("usage:") { 0 } else { 2 });
        }
    };
    if let Err(error) = gate4agent_tui::run(options).await {
        gate4agent_tui::diagnostics::record_runtime(
            gate4agent_tui::diagnostics::RuntimeDiagnostic::Fatal,
        );
        eprintln!("gate4agent-tui-light: {error}");
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
    fn c2_endpoint_is_required_before_secret_access() {
        let args = vec!["gate4agent-tui-light".to_owned()];
        let mut requested_secrets = Vec::new();
        let error = parse_args_from(&args, |name| {
            requested_secrets.push(name.to_owned());
            Err("unexpected secret read".to_owned())
        }).err().unwrap();
        assert_eq!(error, "configure --c2-control PIPE");
        assert!(requested_secrets.is_empty());
    }

    #[test]
    fn direct_node_and_harness_modes_are_not_available() {
        let direct = parse(
            &["gate4agent-tui-light", "--node", r"desk-a=\\.\pipe\desk-a"],
            &[],
        ).err().unwrap();
        assert_eq!(direct, "direct Node mode is disabled; use --c2-control");

        let harness = parse(
            &["gate4agent-tui-light", "--harness-operator", "127.0.0.1:18080"],
            &[],
        ).err().unwrap();
        assert_eq!(harness, "unknown argument: --harness-operator");
    }

    #[test]
    fn startup_requires_all_explicit_identifiers() {
        let error = parse(
            &[
                "gate4agent-tui-light",
                "--c2-control",
                r"\\.\pipe\gate4agent-c2",
                "--agent",
                "codex",
            ],
            &[(C2_TOKEN_ENV, "token")],
        ).err().unwrap();
        assert_eq!(error, "startup requires --startup-node");
    }

    #[test]
    fn manual_c2_mode_preserves_startup_and_style_options() {
        let options = parse(
            &[
                "gate4agent-tui-light",
                "--c2-control",
                r"\\.\pipe\gate4agent-c2",
                "--startup-node",
                "desk-a",
                "--workspace",
                "acme",
                "--agent",
                "claude",
                "--style",
                "gate",
            ],
            &[(C2_TOKEN_ENV, "c2-token")],
        ).unwrap();
        let StartupMode::ManualC2(endpoint) = &options.mode else { panic!("manual C2 mode") };
        assert_eq!(endpoint.endpoint, r"\\.\pipe\gate4agent-c2");
        assert_eq!(endpoint.token, "c2-token");
        let startup = options.startup.as_ref().unwrap();
        assert_eq!(startup.node_id.as_str(), "desk-a");
        assert_eq!(startup.workspace_id.as_str(), "acme");
        assert_eq!(startup.provider.as_str(), "claude");
        assert_eq!(options.color_mode_override, Some(PtyColorMode::GateOverride));
    }

    #[test]
    fn unsupported_workspace_path_is_rejected() {
        let error = parse(
            &[
                "gate4agent-tui-light",
                "--c2-control",
                r"\\.\pipe\gate4agent-c2",
                "--cwd",
                r"C:\work",
            ],
            &[(C2_TOKEN_ENV, "token")],
        ).err().unwrap();
        assert_eq!(error, "unknown argument: --cwd");
    }
}
