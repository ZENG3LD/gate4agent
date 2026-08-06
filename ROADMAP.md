# gate4agent Roadmap

Living document. Current state + what's next. Updated per release.

## Backend control plane (unreleased 0.3 line)

- [x] Split the workspace into pure contracts, lifecycle engines, kernel,
  bounded handles, shell adapters, native runtime, and testkit crates.
- [x] Add control protocol v26 with persistent session generations, transport-
  preserving resume, optional resume prompts, and bounded live/retained
  identity admission.
- [x] Add a bounded capability protocol and engine with host-owned providers,
  exact scoped grants/approvals, lifecycle cancellation, terminal completions,
  and explicit queue/sequence health.
- [x] Reduce control and capability ingress in one deterministic kernel tick and
  publish one immutable combined snapshot through trusted/scoped handles.
- [x] Add provider-bound runtime handles with monotonic binding identities,
  exact request/effect/observation correlation, bounded work and receipt
  queues, operation-scoped cancellation, teardown fencing, and atomic local
  availability publication.
- [ ] Add reviewed host-owned provider adapters and supervisors. Physical
  subprocess/browser cancellation and shutdown acknowledgement remain
  unconfirmed until those concrete executors report them.
- [ ] Add transport admission that rejects oversized frames/bodies before serde
  allocation, plus backend boot/incarnation and connection-scoped identity.
- [ ] Connect approved capability providers. Filesystem, shell, MCP, and browser
  execution remain unavailable until their provider runtime is explicit.

This line intentionally changes previously exhaustive internal APIs and the
wire protocol. The next release must use the 0.3 version boundary.

## Current — 0.2.37 (April 2026)

### Current ACP authority boundary

`AcpSession` currently provides process lifecycle, multi-turn prompts, and
structured events. It advertises host filesystem and terminal capabilities as
disabled, denies agent-to-host filesystem, terminal, and permission callbacks,
and always sends an empty MCP server list. Those authorities stay unavailable
until they are routed through the canonical control plane.

### Shipped in 0.2.37

- **feat: full OpenCode model catalog**: All 49 OpenCode built-in models listed — 12 free-tier first (GPT-5 Nano, GLM, Kimi, Mimo, MiniMax, Nemotron, Qwen, Trinity), then 37 paid. Removed old cross-provider entries (`anthropic/`, `openai/`, `google/`); all use `opencode/` prefix. Own-key users configure `opencode.json` → `discover_capabilities()`.
- **fix: remove Claude alias models**: Dropped `opus`/`sonnet`/`haiku` aliases — redundant with versioned IDs.

### Shipped in 0.2.36

- **feat: lazy cure on first use**: `ensure_cure_once()` runs cure pipeline (OpenCode cache → hardcoded) on first history load or SessionStart. `tool.capabilities()` returns cure-enriched context windows from the first interaction. No explicit init call needed by consumers.

### Shipped in 0.2.35

- **feat(history): SessionUsage from loaded sessions**: `HistoryReader::load_session_with_usage()` returns aggregated token counts alongside chat messages. Claude JSONL reader extracts `input_tokens`, `output_tokens`, `cache_read_input_tokens`, `cache_creation_input_tokens` from assistant messages. All 4 history-loading paths in `MultiCliManager` now initialize `ContextTracker` from loaded usage — `context_percent` shows real values when browsing past sessions.

### Shipped in 0.2.34

- **fix(context): correct usage_percent formula**: `used_tokens()` now = `input + output + cache_read + cache_write` (matches OpenCode's overflow.ts formula). Per-turn mode: input/cache REPLACE (last turn = current context snapshot), output ACCUMULATES (grows context). Codex `event_msg` normalizes input by subtracting cached to avoid double-counting.
- **cure module**: runtime model discovery pipeline — reads OpenCode disk cache (`~/.cache/opencode/models.json`), optional OpenRouter fallback (`cure-network` feature), persists to `~/.gate4agent/models.json`. `discover_capabilities()` overlays cure data onto hardcoded defaults.

### Shipped in 0.2.33

- **fix(capabilities)**: correct context windows and model IDs for all 4 CLIs — Claude Opus/Sonnet 4.6 → 1M tokens, Codex all → 272K, Gemini preview IDs fixed, OpenCode models updated to current.

### Shipped in 0.2.32

- **Fix context_percent always 0%**: Initialize `ContextTracker` from model capabilities at `SessionStart` (matches model ID → `context_window`). Reset tracker on new session spawn so stale data doesn't persist across sessions.

### Shipped in 0.2.31

- **ContextTracker wired into runtime**: `AgentInstance` holds a `ContextTracker`, fed on every `TurnComplete` event in `drain_one()`. `AgentRenderSnapshot` gains `context_percent: Option<f64>` — consumers get live context window usage percentage without any extra work.

### Shipped in 0.2.30

- **Probe + context tracking**: `probe_all()` discovers installed CLIs with caching (`~/.gate4agent/probe-cache.json`, 1h TTL). `ContextTracker` accumulates tokens per session and computes remaining context window capacity.
- **Extended `TurnComplete`**: new fields `cache_read_tokens`, `cache_write_tokens`, `reasoning_tokens`, `context_window`, `is_cumulative`.
- **Codex `event_msg/token_count` parser**: extracts cumulative token totals and `model_context_window` from Codex pipe output.
- **Cache + reasoning tokens**: Claude, Gemini, and OpenCode parsers now extract cache read/write and reasoning token counts.
- **Fixed Claude model IDs**: `claude-opus-4` → `claude-opus-4.6` and related corrections.
- **Removed `image_to_prompt_reference()`** and **`PipeSession::tool()`**: both functions deleted (unused internal API surface).

### Shipped in 0.2.29

- **Dynamic model discovery**: `discover_capabilities()` reads CLI configs (Codex `~/.codex/config.toml`, OpenCode `opencode.json`) at runtime. Model picker enrichment without hardcoded lists.

### Shipped in 0.2.25–0.2.28

- **`CliCapabilities` API**: `ModelInfo`, `PermissionModeInfo`, `CliFeatures` per CLI tool — static capability matrix queryable before spawning.
- **Gemini `--model` flag support**: model override via `--model` passed through to the Gemini CLI.
- **Codex configurable permission modes**: `PermissionModeInfo` for `full-auto`, `auto-edit`, `suggest`.
- **Claude conditional `--dangerously-skip-permissions`**: flag only appended when permission mode requires it.

### Shipped in 0.2.24

- **Codex duplicate message fix**: `response_item` events with `role=user` skipped to avoid double-rendering.
- **Old `.json` session format removed**: Claude sessions without a `cwd` field are no longer loaded — they leaked across all projects.

### Shipped in 0.2.23

- **Codex zombie session filter**: sessions with no user input are excluded from history listing (avoids polluting the list with incomplete/aborted sessions).
- **OpenCode SQLite reader**: reads from `~/.local/share/opencode/opencode.db` instead of the nonexistent `~/.opencode/` path.

### Shipped in 0.2.22

- **Preview extraction for Codex, Gemini, OpenCode history readers**: the first real user message is extracted and surfaced as a session preview.
- **System message filtering for Codex**: injected XML and AGENTS.md content is excluded from previews and event streams.

### Shipped in 0.2.21

- **Docs**: fixed README Quick Start example. Renamed `rpc_hello` example to `acp_hello` to reflect current transport.

### Shipped in 0.2.20

- **workdir scoping for Codex, Gemini, OpenCode history readers**: session history is now scoped to the working directory, preventing cross-project session pollution in multi-repo setups.

### Shipped in 0.2.19

- **RpcSession removed**: standalone RPC transport was a pre-ACP intermediate step. ACP does everything RpcSession did (bidirectional JSON-RPC 2.0, host handlers, multi-turn sessions) but follows the standard Agent Client Protocol. Shared `rpc/` primitives (message, pending, handler, id) retained for ACP internal use.

Shipped in 0.2.16–0.2.18:

- **4 CLI tools**: Claude Code, Codex, Gemini, OpenCode
- **Three transport classes**: Pipe, PTY, ACP (Agent Client Protocol)
- **core/pty/pipe source layout**: clean separation — `core/` for types+errors, `pty/` for PTY transport + per-CLI screen parsers, `pipe/` for Pipe transport + per-CLI NDJSON parsers
- **Research-based pipe parsers**: Codex, Gemini, OpenCode parsers rewritten from actual docs/source (not Claude-copy-paste)
- **Gemini resume support**: `--resume <id>` flag added to GeminiPipeBuilder
- **NdjsonParser trait**: `parse_line(&mut self, line: &str) -> Vec<CliEvent>` + `session_id() -> Option<&str>`
- **CliCommandBuilder trait**: per-tool command builder handles each CLI's quirks
- **`PipeSession` restored**: 0.1.x-compatible entry point
- **`SpawnOptions`**: single options struct
- **SessionEnd synthesis**: guaranteed one `SessionEnd` per session regardless of CLI
- **Transport-neutral `AgentEvent`**: `Text`, `ToolStart`, `ToolResult`, `Thinking`, `TurnComplete`, `SessionStart`, `SessionEnd`
- **ACP transport** (0.2.16): `AcpSession` — bidirectional JSON-RPC 2.0 over stdio, multi-turn sessions, agent→host callbacks (fs, terminal, permissions). Live-verified with all 4 native/adapter CLIs.
- **TerminalAcpHandler** (0.2.18): real terminal execution via host handler.

### Testing status

- **Claude pipe**: live-verified (0.2.5). **Claude ACP**: live-verified (0.2.16) via claude-agent-acp adapter.
- **Codex pipe**: live-verified (0.2.5). **Codex ACP**: live-verified (0.2.16) via codex-acp adapter.
- **Gemini pipe**: live-verified (0.2.6). **Gemini ACP**: live-verified (0.2.16) native `--experimental-acp`.
- **OpenCode pipe**: live-verified (0.2.6). **OpenCode ACP**: live-verified (0.2.16) native `opencode acp`.
- **PTY**: structurally unchanged, low risk. Not formally tested.

## Next — 0.2.x patch line

Small, additive, non-breaking:

- [x] **Research actual OpenCode session storage** — done (0.2.3), session persistence via `--session ses_XXX`
- [x] **Research Gemini resume** — done (0.2.3), `--resume <id>` supported
- [x] **Live integration tests** — done (0.2.5): Claude+Codex fully verified, Gemini+OpenCode parser-verified
- [x] **Daemon transport skeleton** — done (0.2.9): DaemonSession, DaemonConfig for OpenCode serve + OpenClaw. API surface documented, not yet functional. Needs live testing against `opencode serve` and OpenClaw instances.
- [x] **JSON-RPC 2.0 primitives** — done (0.2.10): shared RPC building blocks (message, pending, handler, id) for ACP transport.
- [x] **Critical bugfixes** — done (0.2.11): stale session cleanup, stdin error visibility, OpenCode SessionStart synthesis, Gemini banner suppression, per-CLI history readers
- [x] **ACP transport** — done (0.2.16): AcpSession with initialize + session/new handshake, multi-turn prompt(), session/update streaming, agent→host callbacks. Live-verified: Gemini, OpenCode, Claude, Codex.
- [x] **Cursor support** — done (0.2.16), removed in 0.2.17: `cursor-agent` ships Linux/macOS only — `node_sqlite3.node` is a Linux ELF binary, crashes on Windows. No official Windows build.
- [x] **RpcSession removed** — done (0.2.19): pre-ACP intermediate transport removed. AcpSession supersedes it completely.
- [x] **`CliCapabilities` API** — done (0.2.25–0.2.28): static capability matrix, ModelInfo, PermissionModeInfo, CliFeatures per CLI.
- [x] **Dynamic model discovery** — done (0.2.29): `discover_capabilities()` reads CLI config files at runtime.
- [x] **Probe + context tracking** — done (0.2.30): `probe_all()` with 1h cache, `ContextTracker`, extended `TurnComplete` token fields.
- [ ] **Parser fuzzing** — feed random NDJSON through each parser, assert no panics
- [ ] **Rate-limit pattern expansion** — add known session/daily/weekly limit patterns for OpenCode

## 0.3.0 — capability queries + session listing

- [ ] **`fn capabilities(tool: CliTool) -> CliCapabilities`** — static capability matrix (supports_resume, supports_model_override, supports_stream_json, supports_tool_use, ...). Lets consumers ask before spawning.
- [ ] **Session listing per CLI** — read on-disk session storage (Codex `~/.codex/sessions/...`, Gemini `~/.gemini/tmp/...`, etc.) and enumerate past sessions.
- [ ] **`TransportSession::spawn_pty`** — route PTY-class tools through `TransportSession` too, completing the dispatch layer.
- [ ] **Unified history reader** — replay past sessions through the new parsers to backfill events.

## 0.4.0 — observability

- [ ] **Structured tracing**: `tracing::instrument` annotations on spawn, read, parse paths.
- [ ] **Session metrics**: events/sec, bytes/sec, parse errors, truncation flags.
- [ ] **Process supervision**: optional auto-restart on crash with backoff.
- [ ] **Cost attribution**: surface `cost_usd` from CLIs that report it (Claude, others) through `SessionEnd`.

## Local daemon and browser delivery (after backend readiness)

- [ ] **Local daemon**: authenticated IPC or loopback WebSocket shell over the
  canonical kernel/handle boundary, with bounded frames before deserialization.
- [ ] **Connection authority**: backend-minted boot, connection, client, and
  instance identities; explicit origin checks and user-visible approval.
- [ ] **WASM client**: browser-side protocol client and projection only. PTY,
  filesystem, credentials, and CLI processes remain owned by the local daemon.
- [ ] **Packaging**: make installation/startup an explicit user-approved product
  flow; a normal web page cannot silently install or launch a local service.

## Future: HTTP transport for agent daemons

If a real HTTP-based agent daemon API becomes available (e.g. an agent SDK that exposes a local HTTP server), gate4agent can add an HTTP transport at that time. This will be driven by a real implementation to read, not speculative docs.

## Not planned (explicitly excluded)

- **Harness implementation** — gate4agent is transport, not a harness. The LLM tool loop lives in the CLI itself, not in gate4agent.
- **Sandboxing primitives** — use external sandboxes (Docker, bubblewrap, Windows Sandbox). gate4agent is not a sandbox.
- **Aider / Cline / Continue / Amp / Goose integration** — scope excluded by upstream user decision.
- **Crush (`charmbracelet/crush`)** — no structured headless output, PTY-only, not worth the integration cost until it ships a structured mode. Track `charmbracelet/crush` issue #1030.
- **Config-based auth / API keys** — out of scope. Each CLI handles its own auth; gate4agent just spawns.
- **Hosted agent middleware** — out of scope. Browser delivery uses a local
  backend gate and never uploads vendor CLI credentials to our servers.
- **Cursor** — `cursor-agent` ships Linux/macOS only — `node_sqlite3.node` is a Linux ELF binary, crashes on Windows with "is not a valid Win32 application". No official Windows build. Community patch (gitcnd/cursor-agent-cli-windows) exists but is unofficial. Re-added in 0.2.16 for ACP, removed again in 0.2.17.

## Out-of-band projects that may feed back into gate4agent

- **`gate4agent-cli-flow`** — separate higher-level orchestration crate (mailboxes, supervision, broadcast fan-out). Does NOT depend on gate4agent as a crate dep — historical name only. They're siblings, not parent/child.
- **Downstream consumers** in the nemo workspace: `agent2overlay`, `dig2crawl`, `mylittlechart`. Migration notes for 0.1.x → 0.2.1 live in README.md.
