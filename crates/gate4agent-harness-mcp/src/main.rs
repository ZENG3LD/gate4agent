use std::io;

fn main() {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    let result = if arguments.is_empty() {
        let client = gate4agent_harness_mcp::client_from_env()
            .unwrap_or_else(|_| configuration_unavailable());
        gate4agent_harness_mcp::run_stdio(client, &mut reader, &mut writer)
    } else if arguments.len() == 1 && arguments[0] == "--session-proxy" {
        let client = gate4agent_harness_mcp::session_proxy_client_from_env()
            .unwrap_or_else(|_| configuration_unavailable());
        gate4agent_harness_mcp::run_stdio(client, &mut reader, &mut writer)
    } else {
        eprintln!("gate4agent-harness-mcp: invalid arguments");
        std::process::exit(2);
    };
    if result.is_err() {
        eprintln!("gate4agent-harness-mcp: stdio unavailable");
        std::process::exit(1);
    }
}

fn configuration_unavailable() -> ! {
    eprintln!("gate4agent-harness-mcp: configuration unavailable");
    std::process::exit(2);
}
