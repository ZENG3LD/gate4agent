# gate4agent

Universal Rust wrapper for CLI AI agents (Claude Code, Codex, Gemini).

Two transport modes:
- **PTY mirror**: Spawns agent in a real PTY, captures raw output, vt100 screen parsing
- **Pipe mode**: `claude -p --output-format stream-json`, plain OS pipes, NDJSON event streaming

Both modes produce `AgentEvent` values on a `tokio::sync::broadcast` channel.

## Supported CLI tools

| Tool | PTY mode | Pipe mode |
|------|----------|-----------|
| Claude Code | ✓ | ✓ |
| Codex CLI | ✓ | ✓ |
| Gemini CLI | ✓ | ✓ |

## Quick start

```rust
use gate4agent::pipe::{PipeSession, PipeProcessOptions, ClaudeOptions};
use gate4agent::{AgentEvent, SessionConfig};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = SessionConfig::default();
    let options = PipeProcessOptions {
        claude: ClaudeOptions {
            model: Some("claude-sonnet-4-20250514".into()),
            ..ClaudeOptions::default()
        },
        ..PipeProcessOptions::default()
    };

    let session = PipeSession::spawn(config, "Hello, Claude!", options).await?;
    let mut rx = session.subscribe();

    loop {
        match rx.recv().await? {
            AgentEvent::PipeText { text, .. } => print!("{text}"),
            AgentEvent::PipeSessionEnd { .. } => break,
            _ => {}
        }
    }

    Ok(())
}
```

## Architecture

```
gate4agent/
├── src/
│   ├── lib.rs          — Library root, re-exports
│   ├── types.rs        — AgentEvent, SessionConfig, CliTool
│   ├── error.rs        — Error types
│   ├── cli/            — Per-tool output parsers (Claude, Codex, Gemini)
│   ├── parser/         — VTE + screen parsers for PTY mode
│   ├── ndjson/         — NDJSON stream parser for pipe mode
│   ├── pty/            — PTY session (PtyWrapper, PtySession)
│   ├── pipe/           — Pipe session (PipeProcess, PipeSession)
│   └── detection/      — Rate limit detection
```

## Features

- **Multi-turn sessions**: `--resume <session_id>` for Claude Code continuity
- **System prompt injection**: `--append-system-prompt` for custom instructions
- **Rate limit detection**: Pattern-based detection of session/daily/weekly limits
- **Cross-platform**: Windows (ConPTY) and Unix (POSIX PTY) support
- **Zero-copy streaming**: tokio broadcast channels, no buffering

## Prerequisites

At least one CLI agent must be installed:
- Claude Code: `npm install -g @anthropic-ai/claude-code`
- Codex CLI: `npm install -g @openai/codex`
- Gemini CLI: `npm install -g @google/gemini-cli`

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
