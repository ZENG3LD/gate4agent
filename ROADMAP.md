# gate4agent Roadmap

Living document. Current state + what's next. Updated per release.

## Current — 0.2.3 (April 2026)

Shipped in 0.2.3:

- **4 CLI tools**: Claude Code, Codex, Gemini, OpenCode (`sst/opencode`)
- **Two transport classes**: Pipe, PTY
- **core/pty/pipe source layout**: clean separation — `core/` for types+errors, `pty/` for PTY transport + per-CLI screen parsers, `pipe/` for Pipe transport + per-CLI NDJSON parsers
- **Research-based pipe parsers**: Codex, Gemini, OpenCode parsers rewritten from actual docs/source (not Claude-copy-paste)
- **Gemini resume support**: `--resume <id>` flag added to GeminiPipeBuilder
- **NdjsonParser trait**: `parse_line(&mut self, line: &str) -> Vec<CliEvent>` + `session_id() -> Option<&str>`
- **CliCommandBuilder trait**: per-tool command builder handles each CLI's quirks
- **`PipeSession` restored**: 0.1.x-compatible entry point
- **`SpawnOptions`**: single options struct
- **SessionEnd synthesis**: guaranteed one `SessionEnd` per session regardless of CLI
- **Transport-neutral `AgentEvent`**: `Text`, `ToolStart`, `ToolResult`, `Thinking`, `TurnComplete`, `SessionStart`, `SessionEnd`

What changed from 0.2.1 → 0.2.3:

- **0.2.2**: Parser isolation — `NdjsonParser` trait extracted, per-CLI parser modules split out from monolithic file
- **0.2.3**: Full source tree restructure (core/pty/pipe), proper pipe builders+parsers for all 3 non-Claude CLIs based on research

### Testing status

- **Claude pipe**: live-verified (0.2.5). Full session lifecycle.
- **Codex pipe**: live-verified (0.2.5). Full session.
- **Gemini pipe**: **live-verified (0.2.6)**. Full session: SessionStart → AssistantText → TurnComplete → SessionEnd. Previously 429 rate-limited.
- **OpenCode pipe**: **live-verified (0.2.6)**. Full session: AssistantText → TurnComplete. Parser rewritten from real CLI output. Free model (`opencode/nemotron-3-super-free`) used for testing.
- **PTY**: structurally unchanged, low risk. Not formally tested.

## Next — 0.2.x patch line

Small, additive, non-breaking:

- [x] **Research actual OpenCode session storage** — done (0.2.3), session persistence via `--session ses_XXX`
- [x] **Research Gemini resume** — done (0.2.3), `--resume <id>` supported
- [x] **Live integration tests** — done (0.2.5): Claude+Codex fully verified, Gemini+OpenCode parser-verified
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
- **Cursor Agent CLI** — no native Windows binary (`node_sqlite3.node` is Linux-only ELF), headless mode hangs (SIGTERM on spawn), closed-source. Removed in 0.2.7.

## Out-of-band projects that may feed back into gate4agent

- **`gate4agent-cli-flow`** — separate higher-level orchestration crate (mailboxes, supervision, broadcast fan-out). Does NOT depend on gate4agent as a crate dep — historical name only. They're siblings, not parent/child.
- **Downstream consumers** in the nemo workspace: `agent2overlay`, `dig2crawl`, `mylittlechart`. Migration notes for 0.1.x → 0.2.1 live in README.md.
