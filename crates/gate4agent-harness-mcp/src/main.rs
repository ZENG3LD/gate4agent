use std::io;

use gate4agent_harness_mcp::HarnessMcpTrace;

fn main() {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();
    let argv = std::env::args_os().collect::<Vec<_>>();
    let arguments = argv.get(1..).unwrap_or(&[]);
    // `G4A_HARNESS_MCP_TRACE` is a debugging aid only: absent (the default),
    // it changes nothing below. See `HarnessMcpStdioTrace`.
    let mut trace = gate4agent_harness_mcp::HarnessMcpStdioTrace::open_from_env(&argv);
    let trace_handle: Option<&mut dyn HarnessMcpTrace> =
        trace.as_mut().map(|trace| trace as &mut dyn HarnessMcpTrace);
    let result = if arguments.is_empty() {
        let client = gate4agent_harness_mcp::client_from_env()
            .unwrap_or_else(|_| configuration_unavailable());
        gate4agent_harness_mcp::run_stdio_traced(client, &mut reader, &mut writer, trace_handle)
    } else if arguments.len() == 1 && arguments[0] == "--session-proxy" {
        let client = gate4agent_harness_mcp::session_proxy_client_from_env()
            .unwrap_or_else(|_| configuration_unavailable());
        gate4agent_harness_mcp::run_stdio_traced(client, &mut reader, &mut writer, trace_handle)
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
