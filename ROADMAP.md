# gate4agent Roadmap

Living document. Current state + what's next. Updated per release.

## Current — 0.2.1 (April 2026)

Shipped in 0.2.1 (cleanup release):

- **5 CLI tools**: Claude Code, Codex, Gemini, Cursor Agent, OpenCode (`sst/opencode`)
- **Two transport classes**: Pipe, PTY
- **Honest dispatch**: `TransportSession` is a thin router over `PipeSession` — no dead code, no fantasy enum variants
- **`PipeSession` restored**: 0.1.x-compatible `PipeSession::spawn(config, prompt, options)` entry point is back
- **`SpawnOptions`**: single options struct, unchanged from 0.2.0
- **SessionEnd synthesis**: guaranteed one `SessionEnd` per session regardless of CLI
- **Per-CLI `CliCommandBuilder`**: per-tool command builder handles each CLI's quirks
- **Transport-neutral `AgentEvent`**: `Text`, `ToolStart`, `ToolResult`, `Thinking`, `TurnComplete`, `SessionStart`, `SessionEnd`

What changed from 0.2.0:

- **OpenClaw removed entirely** — `CliTool::OpenClaw`, `AgentCli::OpenClaw`, `DaemonSpec`, `DaemonProbe`, `ensure_daemon_running`, `DaemonNotRunning`, `DaemonProbeTimeout` all deleted. OpenClaw was never functional: the acpx API surface was fiction from unread docs, and no live capture was ever performed.
- **`TransportHandle` enum deleted** — replaced by `TransportSession` holding a `PipeSession` directly. No dead `Pty` variant.
- **`pipe_runner.rs` deleted** — its reader-loop logic is now in `pipe/session.rs` where it belongs.
- **`daemon_runner.rs` / `daemon_spec.rs` / `daemon/` deleted** — no daemon transport exists.

### Known limitations

1. **Cursor / OpenCode parsers are doc-based**, not verified against live CLI output. If upstream field names differ, a patch release will reconcile.
2. **Gemini resume is not supported** — the Gemini CLI does not expose a `--resume` flag in pipe mode. `SpawnOptions::resume_session_id` is silently ignored for Gemini.

## Next — 0.2.x patch line

Small, additive, non-breaking:

- [ ] **Live-capture verification** of Cursor / OpenCode parsers. Reconcile any field-name drift against real `cursor-agent` and `opencode` output.
- [ ] **Research actual OpenCode session storage** — read `sst/opencode` source to understand session persistence, resume semantics, and `ses_XXX` ID format.
- [ ] **Live-verify Cursor parser** — run `cursor-agent -p --output-format stream-json` and diff against fixture tests.
- [ ] **Live-verify Gemini resume limitation** — confirm `--resume` is truly absent in pipe mode; track `gemini-cli` releases for future support.
- [ ] **Parser fuzzing** — feed random NDJSON through each parser, assert no panics.
- [ ] **Rate-limit pattern expansion** — add known session/daily/weekly limit patterns for Cursor / OpenCode.

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

## Out-of-band projects that may feed back into gate4agent

- **`gate4agent-cli-flow`** — separate higher-level orchestration crate (mailboxes, supervision, broadcast fan-out). Does NOT depend on gate4agent as a crate dep — historical name only. They're siblings, not parent/child.
- **Downstream consumers** in the nemo workspace: `agent2overlay`, `dig2crawl`, `mylittlechart`. Migration notes for 0.1.x → 0.2.1 live in README.md.
