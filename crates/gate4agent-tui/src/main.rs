use std::net::SocketAddr;
use std::str::FromStr;

use gate4agent_harness_client::HarnessOperatorCredential;
use gate4agent_harness_protocol::HarnessSelectorV1;
use gate4agent_tui::control_plane::{ControlPlaneCredential, ControlPlaneEndpoint};
use gate4agent_tui::{HarnessOperatorEndpoint, PtyColorMode, RunOptions};

const HARNESS_OPERATOR_TOKEN_ENV: &str = "GATE4AGENT_HARNESS_OPERATOR_TOKEN";
const HARNESS_LAUNCH_PLAN_ID_ENV: &str = "GATE4AGENT_HARNESS_LAUNCH_PLAN_ID";
/// Only read when `--control-plane` is given -- see `control_plane`'s own
/// module doc comment for why the endpoint stays entirely off (no socket,
/// no thread, no env read) otherwise.
const CONTROL_PLANE_TOKEN_ENV: &str = "GATE4AGENT_TUI_CONTROL_TOKEN";

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
    let mut harness_operator = None;
    let mut color_mode_override = None;
    let mut control_plane_bind = None;
    let mut index = 1;

    while index < args.len() {
        match args[index].as_str() {
            "--harness-operator" => {
                if harness_operator.is_some() {
                    return Err("--harness-operator can be specified only once".to_owned());
                }
                let endpoint = value(args, &mut index, "--harness-operator")?;
                harness_operator = Some(parse_harness_operator_endpoint(&endpoint)?);
            }
            "--style" => {
                color_mode_override = Some(PtyColorMode::from_str(&value(args, &mut index, "--style")?)?)
            }
            "--control-plane" => {
                if control_plane_bind.is_some() {
                    return Err("--control-plane can be specified only once".to_owned());
                }
                let endpoint = value(args, &mut index, "--control-plane")?;
                control_plane_bind = Some(parse_control_plane_bind(&endpoint)?);
            }
            "--help" | "-h" => {
                return Err(
                    "usage: gate4agent-tui --harness-operator LOOPBACK_SOCKET [--style inherit|gate]\n\
                     [--control-plane LOOPBACK_SOCKET]\n\
                     credential env: GATE4AGENT_HARNESS_OPERATOR_TOKEN\n\
                     optional selector env: GATE4AGENT_HARNESS_LAUNCH_PLAN_ID\n\
                     optional control-plane credential env (required only with --control-plane): \
                     GATE4AGENT_TUI_CONTROL_TOKEN"
                        .to_owned(),
                )
            }
            unknown => return Err(format!("unknown argument: {unknown}")),
        }
        index += 1;
    }
    let endpoint = harness_operator
        .ok_or_else(|| "configure --harness-operator LOOPBACK_SOCKET".to_owned())?;
    let token = read_secret(HARNESS_OPERATOR_TOKEN_ENV)?;
    let credential = HarnessOperatorCredential::parse(token)
        .map_err(|_| format!("{HARNESS_OPERATOR_TOKEN_ENV} is malformed"))?;
    // Only read when the flag was actually given -- a run without
    // `--control-plane` never touches `CONTROL_PLANE_TOKEN_ENV` at all, so
    // its absence is never an error for the (default) disabled path.
    let control_plane = match control_plane_bind {
        Some(bind) => {
            let token = read_secret(CONTROL_PLANE_TOKEN_ENV)?;
            let credential = ControlPlaneCredential::parse(token)
                .map_err(|_| format!("{CONTROL_PLANE_TOKEN_ENV} is malformed"))?;
            Some(ControlPlaneEndpoint { bind, credential })
        }
        None => None,
    };
    Ok(RunOptions {
        operator: HarnessOperatorEndpoint {
            endpoint,
            credential,
            launch_plan_id: None,
        },
        kanban_default: true,
        color_mode_override,
        control_plane,
    })
}

fn parse_control_plane_bind(value: &str) -> Result<SocketAddr, String> {
    let endpoint = value.parse::<SocketAddr>()
        .map_err(|_| "--control-plane must be an IP socket address".to_owned())?;
    if !endpoint.ip().is_loopback() || endpoint.port() == 0 {
        return Err("--control-plane must be a concrete loopback socket address".to_owned());
    }
    Ok(endpoint)
}

fn parse_harness_operator_endpoint(value: &str) -> Result<SocketAddr, String> {
    let endpoint = value.parse::<SocketAddr>()
        .map_err(|_| "--harness-operator must be an IP socket address".to_owned())?;
    if !endpoint.ip().is_loopback() || endpoint.port() == 0 {
        return Err("--harness-operator must be a concrete loopback socket address".to_owned());
    }
    Ok(endpoint)
}

fn parse_args() -> Result<RunOptions, String> {
    let args = std::env::args().collect::<Vec<_>>();
    let mut options = parse_args_from(&args, |name| {
        let value = std::env::var(name)
            .map_err(|_| format!("{name} is required and must be valid Unicode"))?;
        std::env::remove_var(name);
        Ok(value)
    })?;
    let launch_plan_id = std::env::var(HARNESS_LAUNCH_PLAN_ID_ENV).ok()
        .map(HarnessSelectorV1::new)
        .transpose()
        .map_err(|_| format!("{HARNESS_LAUNCH_PLAN_ID_ENV} is malformed"))?;
    options.operator.launch_plan_id = launch_plan_id;
    Ok(options)
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
    fn harness_endpoint_is_required_before_secret_access() {
        let args = vec!["gate4agent-tui".to_owned()];
        let mut requested_secrets = Vec::new();
        let error = parse_args_from(&args, |name| {
            requested_secrets.push(name.to_owned());
            Err("unexpected secret read".to_owned())
        }).err().unwrap();
        assert_eq!(error, "configure --harness-operator LOOPBACK_SOCKET");
        assert!(requested_secrets.is_empty());
    }

    #[test]
    fn harness_mode_is_authenticated_and_loopback_only() {
        let non_loopback = parse_harness_operator_endpoint("192.0.2.1:18080")
            .err().unwrap();
        assert_eq!(
            non_loopback,
            "--harness-operator must be a concrete loopback socket address",
        );
        let token = format!("g4aho_{}", "0".repeat(64));
        let options = parse(
            &[
                "gate4agent-tui",
                "--harness-operator",
                "127.0.0.1:18080",
                "--style",
                "gate",
            ],
            &[(HARNESS_OPERATOR_TOKEN_ENV, token.as_str())],
        ).unwrap();
        assert_eq!(options.operator.endpoint, "127.0.0.1:18080".parse().unwrap());
        assert!(options.kanban_default);
        assert_eq!(options.color_mode_override, Some(PtyColorMode::GateOverride));
        // Requirement: the control plane is absent unless `--control-plane`
        // is explicitly given -- no flag means `None`, which is what
        // `client::run` checks before ever binding a socket.
        assert!(options.control_plane.is_none());
    }

    #[test]
    fn control_plane_flag_is_off_by_default_and_never_reads_its_token_env() {
        let token = format!("g4aho_{}", "0".repeat(64));
        let mut requested_secrets = Vec::new();
        let options = parse_args_from(
            &["gate4agent-tui".to_owned(), "--harness-operator".to_owned(), "127.0.0.1:18080".to_owned()],
            |name| {
                requested_secrets.push(name.to_owned());
                if name == HARNESS_OPERATOR_TOKEN_ENV {
                    Ok(token.clone())
                } else {
                    Err(format!("unexpected secret read: {name}"))
                }
            },
        ).unwrap();
        assert!(options.control_plane.is_none());
        assert_eq!(requested_secrets, vec![HARNESS_OPERATOR_TOKEN_ENV.to_owned()]);
    }

    #[test]
    fn control_plane_flag_binds_loopback_and_reads_its_own_credential() {
        let harness_token = format!("g4aho_{}", "0".repeat(64));
        let control_token = format!("g4atc_{}", "1".repeat(64));
        let options = parse(
            &[
                "gate4agent-tui",
                "--harness-operator",
                "127.0.0.1:18080",
                "--control-plane",
                "127.0.0.1:18333",
            ],
            &[
                (HARNESS_OPERATOR_TOKEN_ENV, harness_token.as_str()),
                (CONTROL_PLANE_TOKEN_ENV, control_token.as_str()),
            ],
        ).unwrap();
        let control_plane = options.control_plane.expect("--control-plane must populate RunOptions");
        assert_eq!(control_plane.bind, "127.0.0.1:18333".parse().unwrap());
        assert_eq!(control_plane.credential.expose(), control_token);
    }

    #[test]
    fn control_plane_flag_rejects_a_non_loopback_bind_address() {
        let error = parse_control_plane_bind("192.0.2.1:18333").err().unwrap();
        assert_eq!(error, "--control-plane must be a concrete loopback socket address");
    }

    #[test]
    fn control_plane_flag_requires_its_own_credential_env() {
        let harness_token = format!("g4aho_{}", "0".repeat(64));
        let error = parse(
            &[
                "gate4agent-tui",
                "--harness-operator",
                "127.0.0.1:18080",
                "--control-plane",
                "127.0.0.1:18333",
            ],
            &[(HARNESS_OPERATOR_TOKEN_ENV, harness_token.as_str())],
        ).err().unwrap();
        assert_eq!(error, format!("missing {CONTROL_PLANE_TOKEN_ENV}"));
    }

    #[test]
    fn manual_c2_and_startup_arguments_are_not_accepted() {
        for argument in ["--node", "--c2-control", "--startup-node", "--workspace", "--agent"] {
            let error = parse(&["gate4agent-tui", argument, "value"], &[])
                .err().unwrap();
            assert_eq!(error, format!("unknown argument: {argument}"));
        }
    }
}
