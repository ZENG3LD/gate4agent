# gate4agent

gate4agent is an agent workbench, not just a transport library: a
node/c2/harness/TUI stack for running, observing, and orchestrating CLI
coding-agent sessions (first tier: Claude Code, Codex, Kimi, Grok) on top of a provider
transport core. A node wraps one machine's providers (PTY/inline sessions, the
file browser, local git, worktrees); c2 relays any number of nodes to their
clients; a harness — light or full — is the one stateful backend a client app
talks to, behind a single app-facing protocol, adding task kanban, session
context, and delivery on top of the c2 transport; the TUI is the current
client, running in harness mode or in a direct-c2 light mode. The library that
started this repo — spawn, stream, resume CLI-agent subprocesses through one
API — is still here, still usable standalone, and is now the substrate the
rest of the stack builds on; see [Transport core](#transport-core) below.

## Layers

One direction of wrapping: providers → node → c2 → harness → client app. All
crate names below are prefixed `gate4agent-` (e.g. `-node` = `gate4agent-node`).

- **Providers** — blackbox vendor CLIs (Claude Code, Codex, Kimi, Grok,
  qwen-code) wrapped by the transport core: root crate `gate4agent` (`src/`),
  `vendor/portable-pty`, `-types`, `-adapters`, `-provider-ports`, `-catalog`,
  `-engine`, `-kernel`, `-handle`, `-tool-protocol`, `-tool-engine`,
  `-shell-history`, `-shell-capabilities`, `-shell-hooks`,
  `-shell-managed-hooks`, `-shell-one-shot`, `-shell-native`,
  `-runtime-native`.
- **Observation** — read-only monitoring facts projected from provider
  sessions, never prompts/transcripts/credentials: `-observation-protocol`,
  `-observation-api`, `-observation-engine`, `-observation-store`,
  `-observation-service`.
- **Node** — wraps providers on one machine: PTY/inline sessions, the file
  browser, local git, worktrees: `-node-protocol`, `-node-wire`, `-node`
  (bin `gate4agent-node`).
- **C2** — relays any number of nodes to their clients and routes commands
  (spawn, session control) down to nodes: `-c2-protocol`, `-c2-client`
  (bin `gate4agent-c2ctl`), `-c2` (bin `gate4agent-c2`).
- **Harness** — the stateful backend behind one app-facing protocol: task
  kanban over SQLite, session extraction/continuation, delivery of
  skills/plugins/MCP config, an operator surface: `-harness-protocol`,
  `-harness-engine`, `-harness-service` (bin `gate4agent-harness`),
  `-harness-api`, `-harness-client` (bin `gate4agent-harnessctl`),
  `-harness-mcp` (bin `gate4agent-harness-mcp`), `-harness-delivery`.
- **Client** — `crates/gate4agent-tui`, its own nested cargo workspace: bins
  `gate4agent-tui` (harness mode) and `gate4agent-tui-light` (direct-c2 light
  mode).
- **Testing** — `gate4agent-testkit`: authentication-free provider fixtures
  and the Windows headless test supervisor.

## Local endpoints

| Layer | Local pipe | API |
|---|---|---|
| Node | `\\.\pipe\gate4agent-node` (Unix: local socket) | `127.0.0.1:18310` |
| C2 | `\\.\pipe\gate4agent-c2` (Unix: local socket) | `127.0.0.1:18320` |
| Harness | — | operator surface on `127.0.0.1:18330` |

All three are loopback/local-only; nothing here is reachable off the host by
default.

## Credentials

Env vars only — never pass a token as argv, never commit a value:

- `GATE4AGENT_NODE_TOKEN`, or `GATE4AGENT_NODE_TOKEN_<NORMALIZED_ID>` for a
  per-node override (id uppercased, non-alphanumeric characters replaced with
  `_`)
- `GATE4AGENT_C2_TOKEN`
- `GATE4AGENT_HARNESS_OPERATOR_TOKEN`

## Tests

Windows PTY/session-touching tests run only through the headless test
supervisor (`gate4agent-testkit`'s `windows-headless-supervisor` binary) — it
suppresses Windows fault dialogs and enforces a hard per-test timeout that
plain `cargo test` cannot:

```
target\release\windows-headless-supervisor.exe <timeout_ms> <ABS path to test exe> --exact <test_fn>
```

Parallel test arcs build against isolated `--target-dir` values
(`target-<scenario>`) instead of sharing `target/`, so independent runs never
collide on Cargo's build lock. Tests gated by
`require_windows_headless_supervisor_for_test()` reject themselves outright if
run any other way.

## The TUI's uzor dependency

`crates/gate4agent-tui` carries a path dependency on `../../../uzor/uzor-tui`,
a sibling repo that lives outside this one. A fresh clone of this repo alone
cannot build the TUI (`gate4agent-tui` / `gate4agent-tui-light`) until that
sibling checkout is present alongside it; every other crate in the workspace
builds standalone.

## Transport core

The root `gate4agent` crate is a standalone Rust library for spawning,
streaming, resuming, and owning interactive CLI-agent subprocesses through one
API — usable on its own, with no node/c2/harness in the loop.

### Supported CLI tools

| Tool | Transport | Pipe mode | ACP | Resume | Notes |
|---|---|---|---|---|---|
| **Claude Code** | Structured inline + PTY | current `stream-json` | not in default catalog | current `--resume <id>` | Full native Windows PTY lifecycle verified on 2.1.224 |
| **Codex CLI** | Structured inline + PTY | current `exec --json` | not in default catalog | current `exec resume` | Inline and Windows PTY verified on 0.144.6; inline defaults to read-only |
| **Kimi Code** | Structured inline + raw PTY | current `stream-json` | unsupported | active adapter `--session <id>`; legacy `PipeSession` `-r <id>` | Version 0.31.1 exposes `--session`; the latest PTY canary exited before readiness with a local provider `EPERM`, so current PTY lifecycle is not claimed |

The table above is the transport core's own verified matrix. Grok and
qwen-code ride the modern adapter registry (`gate4agent-adapters`) used by the
node stack: Grok is a first-tier provider (a known resume gap is tracked);
qwen-code is wired but unverified. The separate `agent` module also carries a
33-entry Orca-derived reference registry from an earlier grounding cycle;
those entries are transitional code debt, not support claims. Gemini,
OpenCode, and the other reference entries were last live-verified in the
0.2.5–0.2.6 era and are outside the current product target.

### Quick start

```rust
use gate4agent::{CliTool, SessionConfig, AgentEvent, PipeSession, PipeProcessOptions};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = SessionConfig {
        tool: CliTool::ClaudeCode,
        working_dir: std::env::current_dir()?,
        env_vars: vec![],
        name: None,
    };
    let session = PipeSession::spawn(config, "Say hello in 3 words", PipeProcessOptions::default()).await?;

    let mut rx = session.subscribe();
    while let Ok(event) = rx.recv().await {
        match event {
            AgentEvent::Text { text, .. } => print!("{text}"),
            AgentEvent::SessionEnd { .. } => break,
            _ => {}
        }
    }
    Ok(())
}
```

Resume an existing session with
`SpawnOptions { resume_session_id: Some("abc-123-session".into()), ..opts }`;
each active adapter handles it in its own way (Codex `exec resume`, Claude
`--resume <id>`, Kimi `--session <id>`) behind that one field.

### Transport classes

- **Structured inline** — spawn one owned vendor child, read JSONL, then
  create a new child with the provider session id to resume.
- **PTY** — own the interactive terminal process: ordered bytes, VT100 state,
  bounded replay, input, resize, interrupt, teardown. Never auto-accepts
  workspace-trust or update prompts; those stay visible for the operator.
- **ACP** and **daemon** modules are compatibility surfaces, not part of the
  current product target (`acp::AcpSession`, `daemon::DaemonSession`).

`LaunchPlan` (via `plan_launch`) always produces an executable plus an
argument vector — it never concatenates a prompt into a shell command.
`prepare_input()` produces bounded, UTF-8-safe writes with bracketed-paste
neutralization of embedded terminal control sequences.

### Source layout

```
src/
├── lib.rs      — library root, re-exports
├── agent/      — AgentId, registry, built-in specs, argv planner, typed input preparation
├── core/       — AgentEvent, CliTool, SessionConfig, AgentError
├── transport/  — TransportSession (thin router over PipeSession), SpawnOptions
├── pipe/       — PipeSession, per-CLI NDJSON parsers + command builders
├── pty/        — PtyWrapper, PtySession, VTE/screen parsers, per-CLI PTY parsers
├── acp/        — Agent Client Protocol transport (compatibility surface)
├── rpc/        — shared JSON-RPC 2.0 primitives, used internally by acp/
├── probe/      — probe_all(), CliProbe, cache logic
├── context/    — ContextTracker, TurnCompleteData
├── cure/       — runtime model discovery (OpenCode cache → OpenRouter → hardcoded)
├── daemon/     — DaemonSession, per-daemon adapters [skeleton, not functional]
└── history/    — per-CLI session history readers (Claude, Codex, Gemini, OpenCode)
```

### Windows spawn strategy

Reviewed Claude, Codex, and Kimi npm installations resolve to their direct
executable or JavaScript entrypoint, so prompts stay real argv/stdin data
instead of being reparsed by a `.cmd` shim. Unknown legacy wrappers fall back
to a shell. Unix uses direct process execution.

### Current live testing status

| Tool | Pipe | PTY | ACP | Notes |
|---|---|---|---|---|
| **Claude Code 2.1.224** | fresh observed; current resume canary failed | full lifecycle verified | not active | Current PTY: initial/follow-up prompt, resize, in-flight interrupt, recovery, and teardown live-verified |
| **Codex 0.144.6** | fresh/resume verified | live | not active | Initial/follow-up, resize, in-flight interrupt, recovery, cleanup |
| **Kimi Code 0.31.1** | current canary exited before completion | current canary failed before readiness | unsupported | Local provider state reported `EPERM`; no current PTY lifecycle claim |

Vendor-live inline/PTY tests are opt-in (`--ignored`) — they need an
installed, authenticated CLI and network access. Plain `cargo test` is
hermetic and never touches a provider account.

## Prerequisites

At least one CLI agent must be installed on the host. gate4agent does not
install them.

| CLI | Install |
|---|---|
| Claude Code | `npm install -g @anthropic-ai/claude-code` |
| Codex | `npm install -g @openai/codex` |
| Kimi Code | `npm install -g @moonshot-ai/kimi-code` |

## Versioning

- **0.1.x** — original 3-CLI library (Claude, Codex, Gemini)
- **0.2.0** — breaking: 6 CLIs, `TransportSession`, `AgentEvent` renamed, `PipeSession` removed, OpenClaw fantasy transport
- **0.2.1** — cleanup: OpenClaw removed (was never functional), `PipeSession` restored for 0.1.x compatibility, `TransportSession` is now a thin router over `PipeSession`
- **0.2.2** — parser isolation: NdjsonParser trait extracted, per-CLI parser modules split out
- **0.2.3** — source tree restructure into core/pty/pipe layout; proper pipe builders+parsers for Codex, Gemini, Cursor, OpenCode (research-based, NOT yet tested against live CLI output)
- **0.2.4** — docs update, Codex flags fixed (`--full-auto` replaces removed `--ask-for-approval`)
- **0.2.5** — live integration tests: fixed Codex flags, OpenCode `run` subcommand, Gemini `-p` flag, Windows `cmd /C` quoting; all parsers verified against real CLI output
- **0.2.6** — Gemini + OpenCode live-verified; OpenCode parser rewritten from real CLI output
- **0.2.7** — Cursor removed (no native Windows support, broken headless mode, closed-source CLI). 4 CLI tools remain: Claude Code, Codex, Gemini, OpenCode.
- **0.2.8** — SpawnOptions extended: continue_last, allowed_tools, permission_mode, mcp_config, max_turns, sandbox. Per-CLI builders updated.
- **0.2.9** — Daemon transport skeleton: DaemonSession, DaemonConfig, DaemonType (OpenCode, OpenClaw). Not yet functional — API surface documented for future implementation.
- **0.2.10** — Bidirectional JSON-RPC 2.0 primitives: RpcRequest, RpcResponse, RpcNotification, PendingRequests, HostHandler, MethodRouter. Shared infrastructure for ACP transport.
- **0.2.11** — Critical bugfixes: stale transport_session cleared on exit, send_prompt() returns BrokenPipe instead of silent no-op, OpenCode emits SessionStart, Gemini skips non-JSON banners silently, history readers for Codex/Gemini/OpenCode
- **0.2.12** — Test coverage: Gemini parser (14 tests), Claude parser (+8), builder argv parity (22 tests), PipeSession live test. README/DEBUGGING.md fixed. Examples added.
- **0.2.13–0.2.15** — OpenCode default model, env sanitization, test cleanup, TermCell improvements
- **0.2.16** — **ACP transport**: full Agent Client Protocol (JSON-RPC 2.0 over stdio) implementation. AcpSession with initialize + session/new handshake, multi-turn prompt(), session/update streaming, agent→host callbacks (fs, terminal, permissions). Live-verified with Gemini, OpenCode, Claude, Codex. 199 unit tests.
- **0.2.17** — Cursor removed again (no Windows binary: `node_sqlite3.node` is a Linux ELF, crashes on Windows with "is not a valid Win32 application"; no official Windows build exists). 4 CLI tools remain: Claude Code, Codex, Gemini, OpenCode.
- **0.2.18** — ACP host handler extended: TerminalAcpHandler with real terminal execution, FilesystemAcpHandler root whitelisting.
- **0.2.19** — RpcSession removed: standalone RPC transport was a pre-ACP intermediate step, now superseded by AcpSession. Shared JSON-RPC primitives (message, pending, handler, id) retained in `rpc/` for ACP internal use.
- **0.2.20** — History readers: workdir scoping for Codex (cwd field), Gemini (projects.json slug), OpenCode (directory field). All readers now filter sessions by working directory.
- **0.2.21** — Docs: fixed README Quick Start example, renamed rpc_hello → acp_hello example.
- **0.2.22** — History readers: preview extraction for Codex/Gemini/OpenCode (first real user message), system message filtering (Codex injected XML/AGENTS.md content excluded).
- **0.2.23** — History readers: Codex zombie session filter (sessions with no user input excluded), OpenCode SQLite reader (reads from ~/.local/share/opencode/opencode.db instead of nonexistent ~/.opencode/).
- **0.2.24** — History readers: Codex duplicate message fix (skip `response_item` with role=user), old `.json` session format removed (no cwd field = leaked into all projects).
- **0.2.25–0.2.28** — `CliCapabilities` API: `ModelInfo`, `PermissionModeInfo`, `CliFeatures` per CLI tool. Gemini `--model` flag support, Codex configurable permission modes, Claude conditional `--dangerously-skip-permissions`.
- **0.2.29** — Dynamic model discovery: `discover_capabilities()` reads CLI configs (Codex `~/.codex/config.toml`, OpenCode `opencode.json`). Model picker enrichment at runtime.
- **0.2.30** — **Probe + Context tracking**: `probe_all()` discovers installed CLIs with caching (`~/.gate4agent/probe-cache.json`). `ContextTracker` accumulates tokens per session, computes remaining context. Extended `TurnComplete` with `cache_read_tokens`, `cache_write_tokens`, `reasoning_tokens`, `context_window`, `is_cumulative`. Codex `event_msg/token_count` parser (cumulative totals + `model_context_window`). Claude/Gemini/OpenCode parsers extract cache and reasoning tokens. Fixed Claude model IDs (4 → 4.6). Removed `image_to_prompt_reference()` and `PipeSession::tool()`.
- **0.2.31** — **ContextTracker wired into runtime**: `AgentInstance` now holds a `ContextTracker`, updated on every `TurnComplete` event. `AgentRenderSnapshot` gains `context_percent: Option<f64>` — consumers get live context window usage without any extra work.
- **0.2.37** — **Full OpenCode model catalog + remove Claude aliases**. All 49 OpenCode built-in models (12 free first, 37 paid). Removed redundant `opus`/`sonnet`/`haiku` alias entries from Claude.
- **0.2.36** — **feat: cure runs lazily on first history load or session start**. `ensure_cure_once()` populates `~/.gate4agent/models.json` from OpenCode cache before `tool.capabilities()` is called, so context windows are accurate from the first interaction.
- **0.2.35** — **feat(history): SessionUsage from loaded sessions**. `load_session_with_usage()` extracts token counts from Claude JSONL history. Context tracker is initialized when loading past sessions, so `context_percent` shows real values in UI instead of 0%.
- **0.2.34** — **fix(context): correct usage_percent formula + cure module**. `used_tokens()` now = `input + output + cache_read + cache_write` (matches OpenCode's formula). Per-turn mode: input/cache REPLACE (snapshot), output ACCUMULATES. Codex `event_msg` normalizes `input_tokens` by subtracting `cached_input_tokens` to avoid double-counting. New `cure` module: runtime model discovery from OpenCode disk cache (`~/.cache/opencode/models.json`) with optional OpenRouter fallback (`cure-network` feature). Persists to `~/.gate4agent/models.json`, overlays context windows onto hardcoded capabilities.
- **0.2.33** — **fix(capabilities)**: correct context windows and model IDs for all 4 CLIs — Claude Opus/Sonnet 4.6 → 1M tokens, Codex all → 272K, Gemini preview IDs fixed, OpenCode models updated to current.
- **0.2.32** — **Fix context_percent always 0%**: Initialize `ContextTracker` from model capabilities at `SessionStart` (matches model ID → `context_window`). Reset tracker on new session spawn so stale data doesn't persist across sessions.

See [ROADMAP.md](ROADMAP.md) for what's next and [DEBUGGING.md](DEBUGGING.md) for known issues and mitigations.

## Migration guide

### 0.2.0 → 0.2.1

- **OpenClaw removed** — `CliTool::OpenClaw` no longer exists. If you matched on it, delete that arm. OpenClaw was never functional (unverified daemon protocol, fictional acpx API surface).
- **`PipeSession` restored** — 0.1.x callers that used `PipeSession::spawn(config, prompt, options)` compile again. The `PipeSession` now includes SessionEnd synthesis (previously only in the 0.2.0 `pipe_runner`).
- **`TransportSession`** is now a thin wrapper over `PipeSession`. Its public API (`spawn`, `subscribe`, `session_id`, `send_prompt`, `kill`) is unchanged. Internal: no more `TransportHandle` enum, no dead `Pty` variant.
- **`DaemonNotRunning` / `DaemonProbeTimeout` error variants removed** — they were only reachable via OpenClaw. Remove any match arms for these.

### 0.2.18 → 0.2.19

- **`RpcSession` removed** — if you were using `gate4agent::rpc::RpcSession` or the top-level `gate4agent::RpcSession` / `RpcSessionOptions` / `RpcSessionError` re-exports, migrate to [`AcpSession`] instead. ACP does everything RpcSession did (bidirectional JSON-RPC 2.0, host handlers, multi-turn) but follows the standard Agent Client Protocol.
- **Shared `rpc` primitives unchanged** — `RpcRequest`, `RpcResponse`, `RpcError`, `RpcNotification`, `RpcId`, `HostHandler`, `MethodRouter`, `RejectAllHandler`, `PendingRequests`, `IdGen`, `classify_line` are all still exported. Only the `RpcSession` transport struct is gone.

### 0.1.x → 0.2.1

1. **Events**: `AgentEvent::Pipe*` → neutral names. Rename all match arms:
   - `PipeText` → `Text`
   - `PipeToolStart` → `ToolStart`
   - `PipeToolResult` → `ToolResult`
   - `PipeThinking` → `Thinking`
   - `PipeTurnComplete` → `TurnComplete`
   - `PipeSessionStart` → `SessionStart`
   - `PipeSessionEnd` → `SessionEnd`

2. **`PipeSession::spawn`** — signature unchanged: `PipeSession::spawn(config, prompt, options)`. Compiles directly.

3. **`SpawnOptions`**: new unified struct. Fields: `working_dir`, `prompt`, `resume_session_id`, `model`, `append_system_prompt`, `extra_args`, `env_vars`.

4. **`CliTool`** is now non-exhaustive in effect (new variant: `OpenCode`). Add arms or a `_ =>` fallback.

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
