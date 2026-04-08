# gate4agent Roadmap

Living document. Current state + what's next. Updated per release.

## Current — 0.2.0 (April 2026)

Shipped in 0.2.0:

- **6 CLI tools**: Claude Code, Codex, Gemini, Cursor Agent, OpenCode (`sst/opencode`), OpenClaw (via `acpx`)
- **Three transport classes**: Pipe, PTY, DaemonHarness
- **Unified entry point**: `TransportSession::spawn(tool, cwd, prompt, options)`
- **`SpawnOptions`**: single options struct replacing per-CLI option blobs
- **Per-CLI `CliCommandBuilder`**: function-per-CLI command builder handles each CLI's quirks (Codex's `exec resume <id>` sub-sub-command, Cursor's positional prompt, OpenCode's `--session ses_XXX`, etc.)
- **SessionEnd synthesis**: on child exit, if the parser never emitted `SessionEnd`, the runner synthesizes `SessionEnd { result: "exit_code=N", is_error: N != 0 }`. Fixes Codex's missing terminal event.
- **Raw TCP daemon probe**: `std::net::TcpStream::connect_timeout` only. No `reqwest`, no HTTP client.
- **Transport-neutral `AgentEvent`**: `PipeText` → `Text`, `PipeSessionEnd` → `SessionEnd`, etc.
- **Codex bugfix**: `CodexNdjsonParser` now reads `aggregated_output` (was `output`, always empty)
- **Codex spawn flags**: `--ask-for-approval never --skip-git-repo-check` added to prevent interactive hangs
- **Gemini cleanup**: removed `--yolo` from headless spawn args
- **112 tests**: 87 lib + 1 daemon probe + 19 argv parity + 5 transport session

### Known limitations shipped in 0.2.0

Documented honestly — see [DEBUGGING.md](DEBUGGING.md) for detail:

1. **Cursor / OpenCode / OpenClaw parsers are doc-based**, not verified against live CLI output. If upstream field names differ, a patch release will reconcile.
2. **OpenClaw daemon port is assumed** (`127.0.0.1:8787` — acpx gateway default). Needs verification against a running install.
3. **Gemini resume is not supported** — the Gemini CLI does not expose a `--resume` flag in pipe mode. `SpawnOptions::resume_session_id` is silently ignored for Gemini.
4. **PTY variant in `TransportHandle` is dead-code-allowed** — `TransportSession::spawn` does not yet dispatch to the PTY path. PTY consumers still use `PtySession` directly.
5. **OpenClaw resume flag (`--session <id>`) is a best-effort guess** — the ACP-over-acpx resume convention is not documented. If it turns out to be wrong, patch release will fix.

## Next — 0.2.x patch line

Small, additive, non-breaking:

- [ ] **Live-capture verification** of Cursor / OpenCode / OpenClaw parsers. Reconcile any field-name drift.
- [ ] **Verify OpenClaw daemon port** against a real install and update `default_daemon_probe()` if needed.
- [ ] **Parser fuzzing** — feed random NDJSON through each parser, assert no panics.
- [ ] **Rate-limit pattern expansion** — add known session/daily/weekly limit patterns for Cursor / OpenCode / OpenClaw.
- [ ] **Gemini resume** if upstream ships a resume flag (track gemini-cli releases).
- [ ] **Parser divergence notes** — if OpenCode versions change field names (e.g. `tool_use` vs `step_start`), add version-aware dispatch.

## 0.3.0 — capability queries + session listing

- [ ] **`fn capabilities(tool: CliTool) -> CliCapabilities`** — static capability matrix (supports_resume, supports_model_override, supports_stream_json, supports_tool_use, ...). Lets consumers ask before spawning.
- [ ] **Session listing per CLI** — read on-disk session storage (Codex `~/.codex/sessions/...`, Gemini `~/.gemini/tmp/...`, etc.) and enumerate past sessions.
- [ ] **`TransportSession::spawn_with_pty`** — route pty-class tools through `TransportSession` too, finally wiring the dead-code Pty variant.
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

## Not planned (explicitly excluded)

- **Harness implementation** — gate4agent is transport, not a harness. The LLM tool loop lives in the CLI itself, not in gate4agent.
- **Sandboxing primitives** — use external sandboxes (Docker, bubblewrap, Windows Sandbox). gate4agent is not a sandbox.
- **Aider / Cline / Continue / Amp / Goose integration** — scope excluded by upstream user decision.
- **Crush (`charmbracelet/crush`)** — no structured headless output, PTY-only, not worth the integration cost until it ships a structured mode. Track `charmbracelet/crush` issue #1030.
- **Config-based auth / API keys** — out of scope. Each CLI handles its own auth; gate4agent just spawns.

## Out-of-band projects that may feed back into gate4agent

- **`gate4agent-cli-flow`** — separate higher-level orchestration crate (mailboxes, supervision, broadcast fan-out). Does NOT depend on gate4agent as a crate dep — historical name only. They're siblings, not parent/child.
- **Downstream consumers** in the nemo workspace: `agent2overlay`, `dig2crawl`, `mylittlechart`. Migration notes for 0.1.x → 0.2.0 live in README.md.
