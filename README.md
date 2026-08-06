# gate4agent

Slim Rust transport library for Claude Code, Codex CLI, and Kimi Code. Spawn,
stream, resume, and own interactive terminal processes through one API.

**Not a harness. Not a sandbox.** The root crate remains the transport layer
between a Rust application and CLI-agent subprocesses: spawn, write typed
input, read structured events, and resume by session id. The workspace also
contains an optional backend control plane for consumers that need one
deterministic owner for session lifecycle and host-capability authority.

## Supported CLI tools

| Tool | Transport | Pipe mode | ACP | Resume | Notes |
|---|---|---|---|---|---|
| **Claude Code** | Structured inline + PTY | current `stream-json` | not in default catalog | current `--resume <id>` | Inline fresh/resume and full native Windows PTY lifecycle verified on 2.1.223 |
| **Codex CLI** | Structured inline + PTY | current `exec --json` | not in default catalog | current `exec resume` | Inline and Windows PTY verified on 0.144.6; inline defaults to read-only |
| **Kimi Code** | Structured inline + raw PTY | current `stream-json` | unsupported | current `-r <id>` | Inline and Windows terminal fidelity verified on 0.31.1; PTY semantic events are not claimed |

### Transitional reference registry

The table above is the active product-support matrix. The separate `agent`
module still contains the 33-entry Orca-derived reference inventory from the
earlier grounding cycle. Those dormant entries are transitional code debt, not
support claims. Qwen Code is deferred until vendor-owned subscription access is
available; Gemini, OpenCode, and the other reference entries are outside the
current product target.

The launch entries are marked `SpecVerification::Reference`: their interactive
launch shape is pinned to a reviewed source snapshot but still requires
provider/version verification before a product presents it as fully supported.

```rust
use gate4agent::{builtin_registry, plan_launch, LaunchRequest};

let grok = builtin_registry().get_by_id("grok").unwrap();
let plan = plan_launch(
    grok,
    LaunchRequest {
        prompt: Some("--version".into()),
        ..LaunchRequest::default()
    },
)?;

assert_eq!(plan.program.to_string_lossy(), "grok");
let args: Vec<_> = plan.args.iter().map(|arg| arg.to_string_lossy()).collect();
assert_eq!(args, ["--", "--version"]);
```

`LaunchPlan` contains an executable plus argument vector; it never concatenates
the prompt into a shell command. `PtySession::spawn_agent()` consumes the plan.
Agents such as Kimi retain a one-shot `followup_prompt`, which can be submitted
only with an opaque permit issued by `ReadinessTracker` after the expected
foreground/readiness evidence succeeds.

The same module separates prompt, draft, agent-command, shell-command, terminal
text, and terminal-control actions. `prepare_input()` produces bounded,
UTF-8-safe writes; bracketed paste neutralizes embedded terminal control
sequences and keeps submission as a separate final write.

`PtySession::spawn_agent_draft()` keeps reviewable drafts distinct from
auto-submitted prompts. Claude and OpenClaude reference specs use native
`--prefill`; other agents retain a one-shot post-readiness draft. On Windows,
unsafe or oversized inline drafts automatically use the post-readiness path.

Declared slash-command capability is target-bound to the readiness permit.
Command bodies and prompt bodies use a separate Enter write after the pinned
500 ms TUI settle delay; controls/newlines in inline command arguments are
rejected. Specs without the capability fail explicitly.

The sequenced PTY event stream (`subscribe_events` / `attach_events`) reports
subscriber and replay gaps explicitly, keeps a bounded 64 KiB raw-output tail,
and supports atomic replay plus future subscription. Terminal snapshots are
pinned to the last incorporated sequence. PTY output is drained before the
ordered exit event. The producer-side reader queue applies byte-based 256 KiB /
32 KiB high/low watermarks, so backpressure is independent of read chunking.
Event envelopes, replay cursors, and terminal snapshots pin the agent-spec plus
PTY protocol revision; attach rejects a revision mismatch. Fresh foreground
observations and snapshot availability are part of the same ordered sequence.

`PtySession::observe_foreground()` performs a fresh, time- and size-bounded OS
process-table observation using the real PTY root PID. On POSIX it also uses the
kernel foreground process group; on Windows it prefers a hidden CIM probe and
falls back to native Toolhelp/GetProcessTimes identity data when WMI is blocked.
Expected agent wrappers outrank deeper tool children during readiness checks.

`terminate_tree()` snapshots descendants before killing the root process handle.
POSIX force escalation revalidates PID, process group, and start identity;
Windows descendants are revalidated against CIM or native creation identity.
Consuming `shutdown()` joins both the ordered Tokio consumer and the OS reader
thread within a bounded deadline and returns an explicit root-only degradation
report when process inspection was unavailable.
Generic registry sessions remain raw-only; semantic adapters are not selected
from an `AgentId` supplied by a custom specification. The legacy `CliTool`
constructor remains a compatibility surface and is not the active product
support matrix.

`PtyColdRestoreCheckpoint` is a bounded, serde-compatible terminal checkpoint.
It reconstructs a checkpoint plus a contiguous event tail and rejects gaps,
identity/revision changes, invalid resize data, and oversized payloads. Storage,
current-CWD capture, and process restart policy remain owned by the embedding
shell or daemon.

The PTY transport never auto-accepts workspace trust/safety prompts or update
menus. Those remain visible for operator input.

Active transport classes:

- **Structured inline**: spawn one owned vendor child, read JSONL, then create a
  new child with the provider session ID to resume;
- **PTY**: own the interactive terminal process, ordered bytes, VT state,
  bounded replay, input, resize, interrupt, and teardown.

ACP and daemon modules remain compatibility code but are not enabled by the
default Claude/Codex/Kimi catalog or part of the current product target.

## Quick start

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

### Resume an existing session

```rust
let opts = SpawnOptions {
    resume_session_id: Some("abc-123-session".into()),
    ..opts
};
```

Each active CLI handles resume in its own way: Codex uses the current
`exec resume` shape, Claude uses `--resume <id>`, and Kimi uses `-r <id>`.
Gate4Agent hides that difference behind `SpawnOptions::resume_session_id`.

### Using PipeSession directly (backwards-compatible API)

```rust
use gate4agent::{PipeSession, PipeProcessOptions, ClaudeOptions, SessionConfig, CliTool};

let config = SessionConfig {
    tool: CliTool::ClaudeCode,
    working_dir: std::env::current_dir()?,
    env_vars: vec![],
    name: None,
};
let opts = PipeProcessOptions {
    claude: ClaudeOptions { model: Some("claude-opus-4".into()), ..Default::default() },
    ..Default::default()
};
let session = PipeSession::spawn(config, "hello", opts).await?;
```

### Legacy ACP compatibility surface

```rust
use gate4agent::acp::{AcpSession, AcpSessionOptions};
use gate4agent::{CliTool, AgentEvent};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let session = AcpSession::spawn(
        CliTool::Gemini,
        &std::env::current_dir()?,
        AcpSessionOptions::default(),
    ).await?;

    let mut rx = session.subscribe();

    session.prompt("Say hello in 3 words").await?;

    while let Ok(event) = rx.recv().await {
        match event {
            AgentEvent::Text { text, .. } => print!("{text}"),
            AgentEvent::TurnComplete { .. } => break,
            _ => {}
        }
    }

    session.kill().await?;
    Ok(())
}
```

ACP provides multi-turn sessions — call `prompt()` repeatedly without respawning the agent process. Agent-to-host filesystem, terminal, and permission requests are advertised as unavailable and denied by default; raw MCP server injection is not exposed by `AcpSessionOptions`.

### Backend control plane (workspace crates)

Control protocol v26 is reduced by a single-writer kernel. It combines session
lifecycle with a separate, bounded capability engine for exact request,
approval, cancellation, and completion correlation. Consumer handles inject
their private consumer/actor identity, expose scoped snapshots, and disconnect
slow subscribers instead of silently dropping ordered results.

The capability engine is policy and lifecycle infrastructure, not a second
agent loop. It does not execute filesystem, shell, MCP, or browser operations.
Host-owned executors attach through bounded provider-runtime handles. The
in-process handle authority reserves a monotonic binding identity; the kernel
validates and canonicalizes it, requires every request, effect, and observation
to match that exact binding, and fences stale work across detach and rebind. An
executor receives opaque `ProviderInvocation` tickets and sends typed
completion observations; a separate correlated receipt reports whether the
canonical reducer applied, ignored, or rejected the observation.

Cancellation is operation-scoped. The invocation token is switched before the
matching `ProviderWork::Cancel` becomes observable, while provider work or
receipt channel loss, runtime teardown, or port disconnection fences the
entire binding. An engine-side failure to enqueue a cancel remains an explicit
unconfirmed cancellation disposition. The token therefore proves local
cancellation delivery only: physical subprocess or browser termination remains
unconfirmed until a concrete provider supervisor reports it. Provider
availability and the immutable backend snapshot are published atomically, so a
client cannot dispatch through a restored or foreign binding with no local
executor. The current ACP adapter remains fail-closed until a reviewed
host-owned runtime is connected through this boundary.

This is an in-process authority boundary. `BackendIngress` and `KernelStep`
remain trusted low-level integration APIs; a future IPC/WebSocket shell must
authenticate a connection, mint its identities, enforce frame limits before
deserialization, and map only validated messages into these contracts.

No browser or remote delivery layer is included yet. A future browser shell
will talk to a user-approved local backend; gate4agent will not collect vendor
API keys or proxy CLI authentication through a hosted middleware service.

### Legacy daemon skeleton

`DaemonSession` connects to long-running HTTP/WebSocket agent daemons (OpenCode `serve`, OpenClaw). Not yet functional — API surface documented for future implementation.

## Active backend features

- Claude, Codex, and Kimi structured inline fresh/resume with provider-native
  session identity;
- one process owner per session and explicit caller-owned fan-out, without a
  Gate4Agent swarm scheduler;
- observable PTY lifecycle: ordered bytes, bounded replay and gaps,
  sequence-pinned snapshots, resize, interrupt, exit ordering, and process-tree
  teardown;
- typed terminal input with UTF-8-safe chunks and readiness-gated submission;
- transport-neutral structured events without inventing vendor data that is
  absent from the current stream;
- no Git/worktree isolation, task policy, vendor credential proxy, or
  Gate4Agent-owned RAG/AST/tool domain.

The 33-entry Orca registry, ACP, daemon, cure, broad history readers, and old
capability/policy crates are transitional compatibility debt. They are visible
in the source tree but are not active support claims.

## Architecture

```
gate4agent/
├── src/
│   ├── lib.rs           — Library root, re-exports
│   ├── agent/           — AgentId, registry, built-in specs, argv planner, typed input preparation
│   ├── core/            — AgentEvent, CliTool, SessionConfig, AgentError
│   ├── transport/       — TransportSession (thin router over PipeSession), SpawnOptions
│   ├── pipe/            — PipeSession, PipeProcess, per-CLI NDJSON parsers + command builders
│   │   └── cli/         — claude.rs, codex.rs, gemini.rs, opencode.rs
│   ├── pty/             — PtyWrapper, PtySession, VTE/screen parsers, per-CLI PTY parsers
│   │   └── cli/         — Per-CLI PTY output parsers
│   ├── acp/             — ACP transport: AcpSession, protocol types, reader loop, host handler
│   │   ├── session.rs   — AcpSession::spawn(), prompt(), cancel(), kill()
│   │   ├── protocol.rs  — ACP wire types (InitializeParams, SessionUpdate, ContentBlock)
│   │   ├── reader.rs    — Blocking JSON-RPC reader loop
│   │   ├── host.rs      — Private fail-closed ACP host adapter and test-only legacy fixtures
│   │   └── spawn.rs     — AcpProcess + per-CLI spawn specs
│   ├── rpc/             — Shared JSON-RPC 2.0 primitives (message, pending, handler, id)
│   │                      Used internally by acp/. Not a standalone transport.
│   ├── probe/          — probe_all(), ProbeResult, CliProbe, cache logic
│   ├── context/        — ContextTracker, TurnCompleteData
│   ├── cure/           — Runtime model discovery (OpenCode cache → OpenRouter → hardcoded)
│   ├── daemon/         — DaemonSession, per-daemon adapters [skeleton]
│   ├── history/         — Session history readers (per-CLI format)
│   │   ├── claude.rs    — JSONL from ~/.claude/projects/{cwd}/
│   │   ├── codex.rs     — JSONL from ~/.codex/sessions/ (event_msg format)
│   │   ├── gemini.rs    — JSON from ~/.gemini/tmp/{slug}/chats/
│   │   └── opencode.rs  — SQLite from ~/.local/share/opencode/opencode.db
│   └── utils.rs         — String utilities
└── crates/
    ├── gate4agent-types          — Control protocol v26 contracts
    ├── gate4agent-engine         — Deterministic session lifecycle reducer
    ├── gate4agent-tool-protocol  — Capability request/effect/result contracts
    ├── gate4agent-tool-engine    — Bounded policy, approval, and correlation reducer
    ├── gate4agent-kernel         — Unified backend tick and atomic snapshot
    ├── gate4agent-handle         — Bounded trusted/scoped process-local ports
    └── gate4agent-runtime-native — Native effect execution adapter
```

## Current live testing status

| Tool | Pipe | PTY | ACP | Notes |
|---|---|---|---|---|
| **Claude Code 2.1.223** | ✓ fresh/resume | ✓ full lifecycle | not active | Initial/follow-up prompt, resize, in-flight interrupt, recovery, and teardown live-verified |
| **Codex 0.144.6** | ✓ fresh/resume | ✓ live | not active | Initial/follow-up, resize, in-flight interrupt, recovery, cleanup |
| **Kimi Code 0.31.1** | ✓ fresh/resume | ✓ raw terminal fidelity | unsupported | PTY structured semantic events are not claimed |

Vendor-live inline and PTY cases are opt-in because they require installed,
authenticated CLIs and network access. Run them with `--ignored`; ordinary
`cargo test` is hermetic and does not contact provider accounts.

## Windows spawn strategy

On Windows, reviewed Claude, Codex, and Kimi npm installations are resolved to
their direct executable or JavaScript entrypoint. Prompts therefore remain real
argv/stdin data and are not reparsed by a `.cmd` shim. Unknown legacy wrappers
retain a shell fallback. Unix uses direct process execution.

## Prerequisites

At least one CLI agent must be installed on the host. gate4agent does not install them.

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
