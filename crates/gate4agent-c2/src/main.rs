#[cfg(windows)]
use gate4agent_c2::{C2Config, C2NodeConfig, C2Running};
#[cfg(windows)]
use gate4agent_c2::protocol::{NodeId, DEFAULT_C2_API_LISTEN, MAX_C2_NODES};
#[cfg(windows)]
use std::collections::BTreeSet;

#[cfg(windows)]
const C2_TOKEN_ENV: &str = "GATE4AGENT_C2_TOKEN";

#[cfg(windows)]
#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut api_listen = DEFAULT_C2_API_LISTEN.parse().expect("built-in C2 listen address is valid");
    let mut node_args = Vec::new();
    let mut seen = BTreeSet::new();
    let mut seen_endpoints = BTreeSet::new();
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--api-listen" => {
                api_listen = required_value("--api-listen", args.next()).parse()
                    .unwrap_or_else(|error| fail(&format!("--api-listen is invalid: {error}")));
            }
            "--node" => {
                if node_args.len() == MAX_C2_NODES { fail("at most 64 --node values are allowed"); }
                let value = required_value("--node", args.next());
                let (id, endpoint) = value.split_once('=').unwrap_or_else(|| fail("--node requires NODE_ID=NAMED_PIPE"));
                let node_id = NodeId::new(id).unwrap_or_else(|error| fail(&error.to_string()));
                if !seen.insert(node_id.clone()) { fail(&format!("duplicate node ID: {node_id}")); }
                if !seen_endpoints.insert(endpoint.to_ascii_lowercase()) { fail(&format!("duplicate named-pipe endpoint: {endpoint}")); }
                node_args.push((node_id, endpoint.to_owned()));
            }
            "--help" | "-h" => {
                println!("gate4agent-c2 --node NODE_ID=\\\\.\\pipe\\ENDPOINT [--node ...] [--api-listen 127.0.0.1:PORT]");
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
    let config = C2Config::new(api_listen, api_token, nodes).unwrap_or_else(|error| fail(&error.to_string()));
    let running = C2Running::start(config).await.unwrap_or_else(|error| fail(&error.to_string()));
    let shutdown = running.shutdown_handle();
    let wait = running.wait();
    tokio::pin!(wait);
    let mut ctrl_c = tokio::signal::windows::ctrl_c().unwrap_or_else(|error| fail(&error.to_string()));
    let mut ctrl_break = tokio::signal::windows::ctrl_break().unwrap_or_else(|error| fail(&error.to_string()));
    tokio::select! {
        result = &mut wait => if let Err(error) = result { fail(&error.to_string()); },
        signal = ctrl_c.recv() => {
            if signal.is_none() { fail("Ctrl-C signal stream closed"); }
            shutdown.shutdown();
            if let Err(error) = wait.await { fail(&error.to_string()); }
        }
        signal = ctrl_break.recv() => {
            if signal.is_none() { fail("Ctrl-Break signal stream closed"); }
            shutdown.shutdown();
            if let Err(error) = wait.await { fail(&error.to_string()); }
        }
    }
}

#[cfg(windows)]
fn node_token_env(node_id: &NodeId) -> String {
    let normalized = node_id.as_str().bytes().map(|byte| match byte {
        b'a'..=b'z' => (byte - b'a' + b'A') as char,
        b'0'..=b'9' => byte as char,
        _ => '_',
    }).collect::<String>();
    format!("GATE4AGENT_NODE_TOKEN_{normalized}")
}

#[cfg(windows)]
fn required_value(flag: &str, value: Option<String>) -> String {
    value.unwrap_or_else(|| fail(&format!("{flag} requires a value")))
}

#[cfg(windows)]
fn fail(message: &str) -> ! {
    eprintln!("gate4agent-c2: {message}");
    std::process::exit(2)
}

#[cfg(not(windows))]
fn main() {
    eprintln!("gate4agent-c2: Windows named pipes are currently required");
    std::process::exit(2);
}
