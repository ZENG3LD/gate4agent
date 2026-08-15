use std::net::SocketAddr;
use std::str::FromStr;

use gate4agent_harness_client::HarnessOperatorCredential;
use gate4agent_harness_protocol::HarnessSelectorV1;
use gate4agent_tui::{HarnessOperatorEndpoint, PtyColorMode, RunOptions, StartupMode};

const HARNESS_OPERATOR_TOKEN_ENV: &str = "GATE4AGENT_HARNESS_OPERATOR_TOKEN";
const HARNESS_LAUNCH_PLAN_ID_ENV: &str = "GATE4AGENT_HARNESS_LAUNCH_PLAN_ID";

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
            "--help" | "-h" => {
                return Err(
                    "usage: gate4agent-tui --harness-operator LOOPBACK_SOCKET [--style inherit|gate]\n\
                     credential env: GATE4AGENT_HARNESS_OPERATOR_TOKEN\n\
                     optional selector env: GATE4AGENT_HARNESS_LAUNCH_PLAN_ID"
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
    Ok(RunOptions {
        mode: StartupMode::Harness(HarnessOperatorEndpoint {
            endpoint,
            credential,
            launch_plan_id: None,
        }),
        startup: None,
        color_mode_override,
    })
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
    let StartupMode::Harness(operator) = &mut options.mode else {
        unreachable!("the primary TUI parser only constructs Harness mode")
    };
    operator.launch_plan_id = launch_plan_id;
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
        let StartupMode::Harness(endpoint) = options.mode else { panic!("Harness mode") };
        assert_eq!(endpoint.endpoint, "127.0.0.1:18080".parse().unwrap());
        assert!(options.startup.is_none());
        assert_eq!(options.color_mode_override, Some(PtyColorMode::GateOverride));
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
