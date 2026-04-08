# gate4agent

Universal Rust transport library for CLI AI agents. Spawn, stream, resume — for six different CLI agents through one unified API.

**Not a harness. Not a sandbox.** gate4agent is the thin wiring layer between your Rust app and the CLI agent's subprocess: spawn the binary, write the prompt, read structured events, resume by session id. That's it.

## Supported CLI tools

| Tool | Transport | Pipe mode | Resume | Notes |
|---|---|---|---|---|
| **Claude Code** | Pipe + PTY | ✓ stream-json | ✓ `--resume <id>` | Prompt via stdin |
| **Codex** | Pipe + PTY | ✓ `--json` | ✓ `exec resume <id>` | Requires `--ask-for-approval never --skip-git-repo-check` |
| **Gemini** | Pipe + PTY | ✓ stream-json | ✗ (not in CLI) | — |
| **Cursor Agent** | Pipe | ✓ stream-json | ✓ `--resume <id>` | Claude-compatible schema |
| **OpenCode** (`sst/opencode`) | Pipe | ✓ `--format json` | ✓ `--session ses_XXX` | 5-event NDJSON schema |
| **OpenClaw** | DaemonHarness | ✓ via `acpx` | ✓ (assumed) | Requires running openclaw daemon + `npm i -g acpx` |

Transport classes:
- **Pipe**: spawn the CLI directly, read NDJSON over stdout
- **PTY**: spawn inside a pseudo-terminal, scrape the screen with vt100 (for agents without structured output)
- **DaemonHarness**: pre-probe a running daemon via raw TCP, then spawn the client binary over pipe

## Quick start

```rust
use gate4agent::{TransportSession, SpawnOptions, CliTool, AgentEvent};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let opts = SpawnOptions {
        working_dir: PathBuf::from("."),
        prompt: "Say hello in 3 words".into(),
        resume_session_id: None,
        model: None,
        append_system_prompt: None,
        extra_args: vec![],
        env_vars: vec![],
    };

    let mut session = TransportSession::spawn(
        CliTool::ClaudeCode,
        &opts.working_dir.clone(),
        &opts.prompt.clone(),
        opts,
    )?;

    let rx = session.subscribe();
    while let Ok(event) = rx.recv() {
        match event {
            AgentEvent::Text { text, .. } => print!("{text}"),
            AgentEvent::SessionEnd { .. } => break,
            _ => {}
        }
    }
    Ok(())
}
```

### Resume an existing session

```rust
let opts = SpawnOptions {
    resume_session_id: Some("abc-123-session".into()),
    ..opts
};
```

Each CLI handles resume in its own way — Codex swaps `exec` → `exec resume <id>`, Claude/Cursor use `--resume <id>`, OpenCode uses `--session <ses_XXX>`. gate4agent hides the difference behind `SpawnOptions::resume_session_id`.

### DaemonHarness (OpenClaw)

OpenClaw requires a running daemon. gate4agent probes it via raw TCP (no HTTP client) before spawning the `acpx` client. If the daemon is down, `TransportSession::spawn` returns `AgentError::DaemonNotRunning` or `DaemonProbeTimeout`.

```rust
let session = TransportSession::spawn(CliTool::OpenClaw, &cwd, &prompt, opts)?;
```

## Features

- **Single API for 6 CLIs** — `TransportSession::spawn(tool, cwd, prompt, options)`
- **SessionEnd synthesis** — Codex has no terminal event; gate4agent synthesizes `SessionEnd { result: "exit_code=N", is_error: N != 0 }` on child exit
- **Raw TCP daemon probe** — no `reqwest`, no HTTP client, `std::net` only
- **Transport-neutral events** — `AgentEvent::{Text, ToolStart, ToolResult, Thinking, TurnComplete, SessionStart, SessionEnd}`
- **Cross-platform** — Windows (ConPTY + `cmd /C` argv wrapping) and Unix (POSIX PTY + bare exec)
- **Rate-limit detection** — pattern-based session/daily/weekly limit detection per CLI
- **Zero new dependencies** — gate4agent 0.2.0 added 3 CLIs, a daemon transport, and a unified session API without pulling in any new crate

## Architecture

```
gate4agent/
├── src/
│   ├── lib.rs           — Library root, re-exports
│   ├── types.rs         — AgentEvent, CliTool, SessionConfig
│   ├── error.rs         — AgentError (DaemonNotRunning, DaemonProbeTimeout, ...)
│   ├── transport/       — ★ TransportSession, SpawnOptions, pipe_runner, daemon_runner
│   ├── cli/             — Per-tool spawn builders + parsers (claude, codex, gemini, cursor, opencode, openclaw)
│   ├── ndjson/          — NDJSON parsers per CLI
│   ├── parser/          — VTE + screen parser for PTY mode
│   ├── pty/             — PTY session (PtyWrapper, PtySession)
│   ├── pipe/            — Low-level PipeProcess (used internally by pipe_runner)
│   ├── daemon/          — TCP liveness probe (std::net only)
│   └── detection/       — Rate limit detection
```

## Prerequisites

At least one CLI agent must be installed on the host. gate4agent does not install them.

| CLI | Install |
|---|---|
| Claude Code | `npm install -g @anthropic-ai/claude-code` |
| Codex | `npm install -g @openai/codex` |
| Gemini | `npm install -g @google/gemini-cli` |
| Cursor Agent | See https://cursor.com/docs/cli |
| OpenCode | `npm install -g opencode-ai` (or see https://opencode.ai) |
| OpenClaw (via acpx) | `npm install -g acpx` + running openclaw daemon |

## Versioning

- **0.1.x** — original 3-CLI library (Claude, Codex, Gemini)
- **0.2.0** — breaking: 6 CLIs, `TransportSession`, `AgentEvent` renamed, `PipeSession` removed

See [ROADMAP.md](ROADMAP.md) for what's next and [DEBUGGING.md](DEBUGGING.md) for known issues and mitigations.

## Migration from 0.1.x to 0.2.0

1. **Events**: `AgentEvent::Pipe*` → neutral names. Rename all match arms:
   - `PipeText` → `Text`
   - `PipeToolStart` → `ToolStart`
   - `PipeToolResult` → `ToolResult`
   - `PipeThinking` → `Thinking`
   - `PipeTurnComplete` → `TurnComplete`
   - `PipeSessionStart` → `SessionStart`
   - `PipeSessionEnd` → `SessionEnd`

2. **Entry point**: `PipeSession::spawn(config, prompt, options)` → `TransportSession::spawn(tool, cwd, prompt, SpawnOptions)`. `PipeSession` no longer exists.

3. **SpawnOptions**: replaces `PipeProcessOptions` / `ClaudeOptions`. Fields: `working_dir`, `prompt`, `resume_session_id`, `model`, `append_system_prompt`, `extra_args`, `env_vars`.

4. **CliTool** is now non-exhaustive in effect (3 new variants: `Cursor`, `OpenCode`, `OpenClaw`). Add arms or a `_ =>` fallback.

5. **AgentError**: new variants `DaemonNotRunning { host, port, detail }` and `DaemonProbeTimeout { host, port, timeout_ms }` — handle them when calling OpenClaw.

## Support the Project

If you find this tool useful, consider supporting development:

| Currency | Network | Address |
|----------|---------|---------|
| USDT | TRC20 | `TNxMKsvVLYViQ5X5sgCYmkzH4qjhhh5U7X` |
| USDC | Arbitrum | `0xEF3B94Fe845E21371b4C4C5F2032E1f23A13Aa6e` |
| ETH | Ethereum | `0xEF3B94Fe845E21371b4C4C5F2032E1f23A13Aa6e` |
| BTC | Bitcoin | `bc1qjgzthxja8umt5tvrp5tfcf9zeepmhn0f6mnt40` |
| SOL | Solana | `DZJjmH8Cs5wEafz5Ua86wBBkurSA4xdWXa3LWnBUR94c` |

## License

MIT
