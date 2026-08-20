use std::str::FromStr;

use gate4agent_tui::{HarnessOperatorEndpoint, PtyColorMode, RunOptions};

const C2_TOKEN_ENV: &str = "GATE4AGENT_C2_TOKEN";

/// Parsed CLI surface: unchanged from the direct-C2 era (c2 endpoint arg +
/// `GATE4AGENT_C2_TOKEN` env, plus the mode-agnostic `--style` override) even
/// though what happens with it changed completely -- `main` now feeds
/// `c2_endpoint`/`c2_token` to `gate4agent_harness_light::start_harness_light`
/// instead of dialing the app's own (now-dead) direct-C2 worker.
struct LightStartup {
    c2_endpoint: String,
    c2_token: String,
    color_mode_override: Option<PtyColorMode>,
}

fn value(args: &[String], index: &mut usize, flag: &str) -> Result<String, String> {
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn parse_args_from(
    args: &[String],
    mut read_secret: impl FnMut(&str) -> Result<String, String>,
) -> Result<LightStartup, String> {
    let mut c2_control = None;
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
            "--style" => {
                color_mode_override = Some(PtyColorMode::from_str(&value(args, &mut index, "--style")?)?)
            }
            "--help" | "-h" => {
                return Err(
                    "usage: gate4agent-tui-light --c2-control PIPE [--style inherit|gate]\n\
                     credential env: GATE4AGENT_C2_TOKEN"
                        .to_owned(),
                )
            }
            unknown => return Err(format!("unknown argument: {unknown}")),
        }
        index += 1;
    }

    let c2_endpoint = c2_control
        .ok_or_else(|| "configure --c2-control PIPE".to_owned())?;
    let c2_token = read_secret(C2_TOKEN_ENV)?;
    if c2_token.is_empty() {
        return Err(format!("{C2_TOKEN_ENV} must not be empty"));
    }
    Ok(LightStartup { c2_endpoint, c2_token, color_mode_override })
}

fn parse_args() -> Result<LightStartup, String> {
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
    let startup = match parse_args() {
        Ok(startup) => startup,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(if message.starts_with("usage:") { 0 } else { 2 });
        }
    };
    // The light binary hosts `gate4agent-harness-light` in-process (see that
    // crate's own doc comment) instead of speaking the c2 dialect itself: the
    // minted operator credential/endpoint hand-off is an in-process return
    // value, never an env round trip.
    let running = match gate4agent_harness_light::start_harness_light(
        &startup.c2_endpoint,
        &startup.c2_token,
    ).await {
        Ok(running) => running,
        Err(error) => {
            eprintln!("gate4agent-tui-light: harness-light startup failed: {error}");
            std::process::exit(1);
        }
    };
    let options = RunOptions {
        operator: HarnessOperatorEndpoint {
            endpoint: running.operator_endpoint(),
            credential: running.operator_credential(),
            launch_plan_id: None,
        },
        // The kanban (task/run board) stays disabled: light-harness tasks/
        // runs are always empty (no task kernel, by canon), so the session
        // board is the default view -- matching light's pre-cutover UX.
        kanban_default: false,
        color_mode_override: startup.color_mode_override,
    };
    let run_result = gate4agent_tui::run(options).await;
    if let Err(error) = running.shutdown().await {
        eprintln!("gate4agent-tui-light: harness-light shutdown failed: {error}");
    }
    if let Err(error) = run_result {
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

    fn parse(args: &[&str], secrets: &[(&str, &str)]) -> Result<LightStartup, String> {
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
    fn direct_node_and_harness_operator_arguments_are_not_available() {
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
    fn startup_arguments_are_not_accepted() {
        for argument in ["--startup-node", "--workspace", "--agent"] {
            let error = parse(
                &["gate4agent-tui-light", argument, "value"],
                &[(C2_TOKEN_ENV, "token")],
            ).err().unwrap();
            assert_eq!(error, format!("unknown argument: {argument}"));
        }
    }

    #[test]
    fn c2_control_and_style_options_parse() {
        let startup = parse(
            &[
                "gate4agent-tui-light",
                "--c2-control",
                r"\\.\pipe\gate4agent-c2",
                "--style",
                "gate",
            ],
            &[(C2_TOKEN_ENV, "c2-token")],
        ).unwrap();
        assert_eq!(startup.c2_endpoint, r"\\.\pipe\gate4agent-c2");
        assert_eq!(startup.c2_token, "c2-token");
        assert_eq!(startup.color_mode_override, Some(PtyColorMode::GateOverride));
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
