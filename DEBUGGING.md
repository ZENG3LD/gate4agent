# gate4agent Debugging Notes

Running list of known issues, gotchas, and diagnostic recipes per CLI. Updated as problems are hit and fixed.

Most of this file covers the root `gate4agent` transport crate (per-CLI parsing, spawn, PTY). For the node/c2/harness/observation layers (see [README.md](README.md#layers)), see [Node / C2 / Harness](#node--c2--harness) below.

## General diagnostic flow

If a session produces no events:

1. **Is the CLI binary on `PATH`?** Run it manually first (`claude --version`, `codex --version`, etc.).
2. **Is the CLI logged in?** gate4agent doesn't handle auth. Each CLI manages its own credentials.
3. **Capture raw stdout** — before blaming the parser, spawn the exact argv gate4agent uses and pipe to a file. Compare against the fixture NDJSON in `tests/` for that CLI.
4. **Check for interactive prompts** — headless mode must not prompt. gate4agent always adds `--skip-git-repo-check` for Codex, and always passes an explicit sandbox mode, which defaults to `read-only` (`src/pipe/cli/codex.rs`). It never passes `--full-auto`: that name survives only as an alias a caller may set in `permission_mode` to select `danger-full-access`.
5. **Check exit code** — `SessionEnd { result: "exit_code=N", is_error: ... }` tells you if the child crashed. `exit_code=0` without real events usually means the CLI wrote something we don't parse.

## Per-CLI issues

### Claude Code

- **Prompt is delivered via stdin**, not argv. If stdin is closed before the prompt is written, Claude will exit with no output.
- **`--permission-mode`** is always passed and defaults to `plan` (`src/pipe/cli/claude.rs`). `--dangerously-skip-permissions` is never passed. A session that produces no edits is usually not stuck on a prompt — it is planning, because plan mode is read-only by design.
- **`--append-system-prompt`** containing double quotes: arguments are passed as separate elements rather than joined into a shell string, so quoting is `CreateProcess`'s job, not ours. If a prompt with nested quotes still misbehaves, write it to a file and reference it.
- **Resume session id**: UUID string from previous session's `SessionStart` event. Must be exact.

### Codex

- **Production bug fixed in 0.2.0**: `CodexNdjsonParser` was reading `item.get("output")` for command results but Codex actually emits `aggregated_output`. Any 0.1.x consumer would see empty shell output. Upgrade to 0.2.0+ if you care about tool results.
- **Interactive hangs without `--full-auto`**: fixed in 0.2.0. If you still see hangs, check you're on 0.2.0+.
- **`--skip-git-repo-check`**: fixed in 0.2.0. Without it, Codex refuses to run in non-git directories.
- **Resume shape**: `codex exec resume <session_id> --json ...`. Note the sub-sub-command — this is why gate4agent uses function-per-CLI builders instead of a declarative spec.
- **No terminal event**: Codex doesn't emit any `session_end`-equivalent. gate4agent synthesizes `SessionEnd` when the child process exits. If you see two `SessionEnd` events per session, the parser is double-counting — please file an issue with the raw NDJSON.
- **`assistant_message` vs `agent_message`**: both naming conventions are accepted by the parser (0.2.0). Older Codex versions used one, newer use the other.
- **Session storage**: `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` — useful for debugging without re-running.

### Gemini

- **No resume in pipe mode**: the Gemini CLI doesn't expose `--resume` for `-p` headless mode. `SpawnOptions::resume_session_id` is silently ignored. If you need multi-turn with Gemini, bundle the prior context into the prompt itself.
- **`--yolo` was removed** in 0.2.0 spawn args — not needed for `--output-format stream-json` and only adds stderr noise.
- **Session storage**: `~/.gemini/tmp/<hash>/chats/` — reference for debugging.

### OpenCode (sst/opencode)

- **6-event schema**: `step_start`, `tool_use`, `text`, `reasoning`, `step_finish`, `error`. Some versions use `tool_use` as an alias for `step_start` — parser accepts both.
- **Session id prefix**: `ses_XXXX`. Parser tracks this automatically; use with `SpawnOptions::resume_session_id` to resume.
- **Parser is doc-based**. If real output differs, file an issue with raw stdout.
- **Don't confuse with `charmbracelet/crush`** or `opencode-ai/opencode`. gate4agent targets `sst/opencode` v1.4.0+.
- Source docs: https://opencode.ai/docs/cli/

## Transport-level issues

### SessionEnd synthesis

- Exactly one `SessionEnd` is guaranteed per session: either the parser emitted one, or the reader loop synthesizes one on child exit. If you see zero or two, that's a bug — please file it.
- Synthetic SessionEnd format: `{ result: "exit_code=N", cost_usd: None, is_error: N != 0 }`.

### Windows-specific

- **Spawn does not go through `cmd.exe` when it can be avoided.** For Claude/Codex/Kimi, `build_command_with_options` (`src/pipe/process.rs`) first resolves the direct `.exe`/JS entrypoint via `windows_direct_npm_command`; every argument is then passed as its own element, never joined into a shell string. Where `cmd.exe` IS still involved, a prompt containing backticks, `%var%` or `^` can be interpreted by it — use `extra_args` cautiously.
- **PTY path uses ConPTY**. If you see corrupt output in PTY mode, verify your Windows version supports ConPTY (Windows 10 1809+).

### Reader thread deadlocks

- The reader thread does raw `read` into an 8KB buffer (`src/pipe/process.rs`), not line-based reads, so a CLI that never emits a newline is not by itself a hang.
- It cannot hang forever: `reader_loop` fixes a deadline of `PIPE_TIMEOUT_SECONDS` (60) at loop start and force-terminates once it elapses, with no manual `kill()` needed.
- `kill()` is not graceful. Both `PipeSession::kill()` and `PipeProcess::kill()` go straight to `kill_tree()` — `taskkill /PID <id> /T /F` on Windows, `kill -KILL -- -<pid>` on unix — an immediate hard kill of the whole process tree. There is no drop-stdin-then-wait step.

## Node / C2 / Harness

The node, c2, harness, and observation crates are newer than the notes above
and mostly untested by the per-CLI recipes in this file. Two things carry
over from the transport core and two are specific to these layers:

- **Tracing**: `gate4agent-node` and `gate4agent-harness-service` (bin
  `gate4agent-harness`) both use `tracing` + `tracing-subscriber`. Run either
  binary with `RUST_LOG=info` (or a per-crate filter, e.g.
  `RUST_LOG=gate4agent_node=debug`) for connection lifecycle, spawn/session
  dispatch, and store errors on stderr.
- **Windows E2E tests run only through the headless supervisor** — see
  [Test runner](#test-runner) below. A test gated by
  `require_windows_headless_supervisor_for_test()`
  (`gate4agent-testkit`) fails immediately under plain `cargo test`.
- **Credentials are env-only** — `GATE4AGENT_NODE_TOKEN[_<ID>]`,
  `GATE4AGENT_C2_TOKEN`, `GATE4AGENT_HARNESS_OPERATOR_TOKEN` (see
  [README.md](README.md#credentials)). A rejected connection with no other
  symptom is usually a missing or stale token in the environment the service
  was started from, not a code bug.
- **Rejections are logged with a typed cause** on the node and harness hosts —
  check the service's own stderr (via `RUST_LOG=info`) before assuming a
  request is malformed.

## Test runner

```bash
# All unit tests
cd gate4agent && cargo test --lib

# Builder argv parity tests
cargo test --test builder_argv

# Live integration tests (require a CLI installed and logged in)
cargo test --test pipe_live -- --ignored --nocapture

# SessionEnd synthesis unit tests
cargo test --lib pipe::session::tests
```

Node/c2/harness/observation Windows E2E tests do not run under plain
`cargo test`. They execute through the headless supervisor binary, which
suppresses Windows fault dialogs and enforces a hard per-test timeout:

```
cargo build --release -p <crate> --test <test_file>
target\release\windows-headless-supervisor.exe <timeout_ms> <ABS path to test exe> --exact <test_fn>
```

Build into the workspace's own `target/`. Do not give a run its own
`--target-dir`: each one is a full copy of the dependency build, and they
accumulate into tens of gigabytes. If two builds overlap, Cargo's build lock
makes the second WAIT -- that is the lock working, not a problem to route
around.

If any test fails on a clean checkout with a released version, file an issue with:
- OS + version
- `cargo --version`
- Full test output
- Installed CLI versions (`claude --version`, `codex --version`, etc.) if running CLI-level tests

## Reporting a bug

1. Reproduce with `RUST_LOG=gate4agent=trace`
2. Capture the raw NDJSON (or PTY screen) from the CLI directly
3. File an issue on GitHub with: CLI name + version, gate4agent version, OS, raw output, expected vs actual event sequence

## Windows spawn: cmd /C vs bash fallback

gate4agent detects whether a CLI has a `.cmd` wrapper on PATH:
- If `.cmd` exists: `cmd /C program.cmd arg1 arg2` (npm-installed tools)
- If no `.cmd`: `bash -c 'program arg1 ...'` (bash scripts, native binaries)

**Why not join args into a shell string?** `cmd.exe /C` has bizarre quote-stripping rules: if the first char after `/C` is `"`, cmd may strip enclosing quotes and break the inner command. Passing args individually via `.arg()` lets Windows `CreateProcess` handle quoting correctly.

**Why `/S /C "..."` doesn't work?** Tested — cmd.exe still misinterprets nested quotes in certain edge cases (e.g., prompts with periods and colons). The individual-args approach is more reliable.
