# gate4agent Roadmap

Living document. Current state + what's next. Updated per release.

## Current — 0.2.19 (April 2026)

Shipped in 0.2.19:

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

## 0.5.0 — thin server

- [ ] **`gate4agent-server` binary**: axum wrapper over `TransportSession` with WS/SSE fan-out.
- [ ] **Auth**: bearer tokens for remote spawn access.
- [ ] **Worktree sandbox profiles**: spawn each session in an ephemeral git worktree with configurable cleanup.
- [ ] **Multi-tenant session registry**: HTTP endpoints to list/spawn/kill sessions remotely.

## Future: HTTP transport for agent daemons

If a real HTTP-based agent daemon API becomes available (e.g. an agent SDK that exposes a local HTTP server), gate4agent can add an HTTP transport at that time. This will be driven by a real implementation to read, not speculative docs.

## Not planned (explicitly excluded)

- **Harness implementation** — gate4agent is transport, not a harness. The LLM tool loop lives in the CLI itself, not in gate4agent.
- **Sandboxing primitives** — use external sandboxes (Docker, bubblewrap, Windows Sandbox). gate4agent is not a sandbox.
- **Aider / Cline / Continue / Amp / Goose integration** — scope excluded by upstream user decision.
- **Crush (`charmbracelet/crush`)** — no structured headless output, PTY-only, not worth the integration cost until it ships a structured mode. Track `charmbracelet/crush` issue #1030.
- **Config-based auth / API keys** — out of scope. Each CLI handles its own auth; gate4agent just spawns.
- **Cursor** — `cursor-agent` ships Linux/macOS only — `node_sqlite3.node` is a Linux ELF binary, crashes on Windows with "is not a valid Win32 application". No official Windows build. Community patch (gitcnd/cursor-agent-cli-windows) exists but is unofficial. Re-added in 0.2.16 for ACP, removed again in 0.2.17.

## Out-of-band projects that may feed back into gate4agent

- **`gate4agent-cli-flow`** — separate higher-level orchestration crate (mailboxes, supervision, broadcast fan-out). Does NOT depend on gate4agent as a crate dep — historical name only. They're siblings, not parent/child.
- **Downstream consumers** in the nemo workspace: `agent2overlay`, `dig2crawl`, `mylittlechart`. Migration notes for 0.1.x → 0.2.1 live in README.md.
