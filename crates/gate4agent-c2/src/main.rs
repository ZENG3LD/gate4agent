#[cfg(any(windows, unix))]
use gate4agent_c2::{default_c2_control_endpoint, C2Config, C2NodeConfig, C2Running};
#[cfg(any(windows, unix))]
use gate4agent_c2::protocol::{NodeId, DEFAULT_C2_API_LISTEN, MAX_C2_NODES};
#[cfg(any(windows, unix))]
use std::collections::BTreeSet;

#[cfg(any(windows, unix))]
const C2_TOKEN_ENV: &str = "GATE4AGENT_C2_TOKEN";

#[cfg(any(windows, unix))]
#[tokio::main(flavor = "current_thread")]
async fn main() {
    // Both c2 instances in a live stack had produced zero bytes of log output
    // over fourteen hours -- not a filtered-out level, an absent subscriber.
    // Without this, every `tracing::info!`/`warn!` call anywhere in this
    // binary and its dependencies is a no-op. Mirrors gate4agent-node's own
    // `main.rs` init exactly.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let mut api_listen = DEFAULT_C2_API_LISTEN.parse().expect("built-in C2 listen address is valid");
    let mut control_endpoint = default_c2_control_endpoint()
        .unwrap_or_else(|error| fail(&error.to_string()));
    let mut node_listen: Option<std::net::SocketAddr> = None;
    let mut control_tunnel: Option<std::net::SocketAddr> = None;
    let mut node_args = Vec::new();
    let mut seen = BTreeSet::new();
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--api-listen" => {
                api_listen = required_value("--api-listen", args.next()).parse()
                    .unwrap_or_else(|error| fail(&format!("--api-listen is invalid: {error}")));
            }
            "--control-endpoint" => control_endpoint = required_value("--control-endpoint", args.next()),
            "--control-tunnel" => {
                let value = required_value("--control-tunnel", args.next());
                control_tunnel = Some(value.parse().unwrap_or_else(|error| {
                    fail(&format!("--control-tunnel is invalid: {error}"))
                }));
            }
            // Where nodes that cannot be dialled call in. Required by, and
            // only by, a `--node ID=accept` assignment.
            "--node-listen" => {
                node_listen = Some(required_value("--node-listen", args.next()).parse()
                    .unwrap_or_else(|error| fail(&format!("--node-listen is invalid: {error}"))));
            }
            "--node" => {
                if node_args.len() == MAX_C2_NODES { fail("at most 64 --node values are allowed"); }
                let value = required_value("--node", args.next());
                let (id, endpoint) = value.split_once('=').unwrap_or_else(|| fail(node_assignment_error()));
                let node_id = NodeId::new(id).unwrap_or_else(|error| fail(&error.to_string()));
                if !seen.insert(node_id.clone()) { fail(&format!("duplicate node ID: {node_id}")); }
                node_args.push((node_id, endpoint.to_owned()));
            }
            "--help" | "-h" => {
                print_help();
                println!("API token: {C2_TOKEN_ENV}; node tokens: GATE4AGENT_NODE_TOKEN_<NORMALIZED_NODE_ID>");
                return;
            }
            unknown => fail(&format!("unknown argument: {unknown}")),
        }
    }
    let env_name_list = node_args.iter().map(|(node_id, _)| node_token_env(node_id)).collect::<Vec<_>>();
    let env_names = env_name_list.iter().cloned().collect::<BTreeSet<_>>();
    if env_names.len() != env_name_list.len() {
        fail("node IDs normalize to the same node-token environment variable");
    }
    let mut nodes = Vec::with_capacity(node_args.len());
    for ((node_id, endpoint), env_name) in node_args.into_iter().zip(env_name_list) {
        let token = std::env::var(&env_name).unwrap_or_else(|_| fail(&format!("{env_name} is required")));
        std::env::remove_var(&env_name);
        nodes.push(C2NodeConfig::new(node_id, endpoint, token).unwrap_or_else(|error| fail(&error.to_string())));
    }
    let api_token = std::env::var(C2_TOKEN_ENV).unwrap_or_else(|_| fail(&format!("{C2_TOKEN_ENV} is required")));
    std::env::remove_var(C2_TOKEN_ENV);
    let config = C2Config::new(api_listen, api_token, nodes)
        .and_then(|config| config.with_control_endpoint(control_endpoint))
        .and_then(|config| match node_listen {
            Some(listen) => config.with_node_listen(listen),
            None => Ok(config),
        })
        .and_then(|config| match control_tunnel {
            Some(listen) => config.with_control_tunnel_listen(listen),
            None => Ok(config),
        })
        .and_then(|config| config.validate_call_home().map(|()| config))
        .unwrap_or_else(|error| fail(&error.to_string()));
    let running = C2Running::start(config).await.unwrap_or_else(|error| fail(&error.to_string()));
    let shutdown = running.shutdown_handle();
    let wait = running.wait();
    tokio::pin!(wait);
    tokio::select! {
        result = &mut wait => if let Err(error) = result { fail(&error.to_string()); },
        result = shutdown_signal() => {
            if let Err(error) = result { fail(&error.to_string()); }
            shutdown.shutdown();
            if let Err(error) = wait.await { fail(&error.to_string()); }
        }
    }
}

#[cfg(windows)]
async fn shutdown_signal() -> std::io::Result<()> {
    let mut ctrl_c = tokio::signal::windows::ctrl_c()?;
    let mut ctrl_break = tokio::signal::windows::ctrl_break()?;
    tokio::select! {
        signal = ctrl_c.recv() => signal.map(|_| ()).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Ctrl-C signal stream closed")
        }),
        signal = ctrl_break.recv() => signal.map(|_| ()).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Ctrl-Break signal stream closed")
        }),
    }
}

#[cfg(unix)]
async fn shutdown_signal() -> std::io::Result<()> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result,
        signal = terminate.recv() => signal.map(|_| ()).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "SIGTERM signal stream closed")
        }),
    }
}

#[cfg(any(windows, unix))]
fn node_token_env(node_id: &NodeId) -> String {
    let normalized = node_id.as_str().bytes().map(|byte| match byte {
        b'a'..=b'z' => (byte - b'a' + b'A') as char,
        b'0'..=b'9' => byte as char,
        _ => '_',
    }).collect::<String>();
    format!("GATE4AGENT_NODE_TOKEN_{normalized}")
}

#[cfg(any(windows, unix))]
fn required_value(flag: &str, value: Option<String>) -> String {
    value.unwrap_or_else(|| fail(&format!("{flag} requires a value")))
}

#[cfg(any(windows, unix))]
fn fail(message: &str) -> ! {
    eprintln!("gate4agent-c2: {message}");
    std::process::exit(2)
}

#[cfg(windows)]
fn node_assignment_error() -> &'static str { "--node requires NODE_ID=NAMED_PIPE_OR_TCP_LOOPBACK_OR_ACCEPT" }

#[cfg(unix)]
fn node_assignment_error() -> &'static str { "--node requires NODE_ID=LOCAL_ENDPOINT_OR_TCP_LOOPBACK_OR_ACCEPT" }

/// How a local endpoint is spelled on this platform. The one part of the
/// usage line that legitimately differs per platform -- see `print_help`.
#[cfg(windows)]
const LOCAL_ENDPOINT_SPELLING: &str = "\\\\.\\pipe\\ENDPOINT";

#[cfg(unix)]
const LOCAL_ENDPOINT_SPELLING: &str = "LOCAL_ENDPOINT";

/// One body, not one per platform.
///
/// This was two `#[cfg]`-gated functions carrying two copies of the whole
/// flag list, and they drifted exactly the way that arrangement invites:
/// `accept` and `--node-listen` were added to the unix copy and not the
/// windows one, so on Windows -- where this is actually developed --
/// `--help` described a binary without the call-home support it had
/// shipped with. Only the endpoint spelling is platform-specific, so now
/// only the endpoint spelling is.
fn print_help() {
    println!(
        "gate4agent-c2 --node NODE_ID={endpoint}|tcp://127.0.0.1:PORT|tcp://[::1]:PORT|accept \
[--node ...] [--api-listen 127.0.0.1:PORT] [--node-listen 127.0.0.1:PORT] \
[--control-endpoint {endpoint}] [--control-tunnel TUNNEL_IP:PORT]",
        endpoint = LOCAL_ENDPOINT_SPELLING,
    );
    println!("  --control-tunnel: HQ WireGuard client control TCP, bound to the C2 tunnel address only (not 0.0.0.0). Unix --control-endpoint stays.");
    println!("  accept: this node dials in instead of being dialled; requires --node-listen");
    println!("  --node-listen: loopback or the C2 WireGuard tunnel address (not 0.0.0.0); kit peers dial over WG");
}

#[cfg(not(any(windows, unix)))]
fn main() {
    eprintln!("gate4agent-c2: this operating system has no supported local transport");
    std::process::exit(2);
}
