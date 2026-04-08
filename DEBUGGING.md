# gate4agent Debugging Notes

Running list of known issues, gotchas, and diagnostic recipes per CLI. Updated as problems are hit and fixed.

## General diagnostic flow

If a session produces no events:

1. **Is the CLI binary on `PATH`?** Run it manually first (`claude --version`, `codex --version`, etc.).
2. **Is the CLI logged in?** gate4agent doesn't handle auth. Each CLI manages its own credentials.
3. **Capture raw stdout** — before blaming the parser, spawn the exact argv gate4agent uses and pipe to a file. Compare against the fixture NDJSON in `tests/` for that CLI.
4. **Check for interactive prompts** — headless mode must not prompt. Codex needs `--ask-for-approval never --skip-git-repo-check` (gate4agent adds these automatically).
5. **Check exit code** — `SessionEnd { result: "exit_code=N", is_error: ... }` tells you if the child crashed. `exit_code=0` without real events usually means the CLI wrote something we don't parse.

## Per-CLI issues

### Claude Code

- **Prompt is delivered via stdin**, not argv. If stdin is closed before the prompt is written, Claude will exit with no output.
- **`--dangerously-skip-permissions`** is always passed. Without it, headless mode blocks on permission prompts.
- **`--append-system-prompt`** containing double quotes: gate4agent's Windows shell-wrapper (`argv_to_windows_shell_string`) handles escaping, but complex prompts with nested quotes can still break. If that happens, write the prompt to a file and reference it.
- **Resume session id**: UUID string from previous session's `SessionStart` event. Must be exact.

### Codex

- **Production bug fixed in 0.2.0**: `CodexNdjsonParser` was reading `item.get("output")` for command results but Codex actually emits `aggregated_output`. Any 0.1.x consumer would see empty shell output. Upgrade to 0.2.0+ if you care about tool results.
- **Interactive hangs without `--ask-for-approval never`**: fixed in 0.2.0. If you still see hangs, check you're on 0.2.0+.
- **`--skip-git-repo-check`**: fixed in 0.2.0. Without it, Codex refuses to run in non-git directories.
- **Resume shape**: `codex exec resume <session_id> --json ...`. Note the sub-sub-command — this is why gate4agent uses function-per-CLI builders instead of a declarative spec.
- **No terminal event**: Codex doesn't emit any `session_end`-equivalent. gate4agent synthesizes `SessionEnd` when the child process exits. If you see two `SessionEnd` events per session, the parser is double-counting — please file an issue with the raw NDJSON.
- **`assistant_message` vs `agent_message`**: both naming conventions are accepted by the parser (0.2.0). Older Codex versions used one, newer use the other.
- **Session storage**: `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` — useful for debugging without re-running.

### Gemini

- **No resume in pipe mode**: the Gemini CLI doesn't expose `--resume` for `-p` headless mode. `SpawnOptions::resume_session_id` is silently ignored. If you need multi-turn with Gemini, bundle the prior context into the prompt itself.
- **`--yolo` was removed** in 0.2.0 spawn args — not needed for `--output-format stream-json` and only adds stderr noise.
- **Session storage**: `~/.gemini/tmp/<hash>/chats/` — reference for debugging.

### Cursor Agent

- **Parser is doc-based, not live-verified**. If you see unexpected empty events, diff your live output against `tests/` fixtures and file an issue with the raw NDJSON — we'll patch and release.
- **Prompt is positional**, not stdin. Differs from Claude.
- Source docs: https://cursor.com/docs/cli/headless

### OpenCode (sst/opencode)

- **5-event schema**: `step_start`, `tool_use`, `text`, `step_finish`, `error`. Some versions use `tool_use` as an alias for `step_start` — parser accepts both.
- **Session id prefix**: `ses_XXXX`. Parser tracks this automatically; use with `SpawnOptions::resume_session_id` to resume.
- **Parser is doc-based**. Same caveat as Cursor — if real output differs, file an issue with raw stdout.
- **Don't confuse with `charmbracelet/crush`** or `opencode-ai/opencode`. gate4agent targets `sst/opencode` v1.4.0+.
- Source docs: https://opencode.ai/docs/cli/

## Transport-level issues

### SessionEnd synthesis

- Exactly one `SessionEnd` is guaranteed per session: either the parser emitted one, or the reader loop synthesizes one on child exit. If you see zero or two, that's a bug — please file it.
- Synthetic SessionEnd format: `{ result: "exit_code=N", cost_usd: None, is_error: N != 0 }`.

### Windows-specific

- **Spawn uses `cmd /C <shell_string>`** on Windows. The shell string is built by `argv_to_windows_shell_string` which wraps each token in `"..."` with `\"` escaping. If you pass a prompt containing backticks, `%var%`, or `^` escapes, `cmd.exe` may interpret them — use `extra_args` cautiously.
- **PTY path uses ConPTY**. If you see corrupt output in PTY mode, verify your Windows version supports ConPTY (Windows 10 1809+).

### Reader thread deadlocks

- Reader thread blocks on `child.stdout.read_line()`. If the CLI never closes stdout and never exits, the thread hangs forever. Kill the session via `TransportSession::kill()` or `PipeSession::kill()` to force cleanup.
- On kill, gate4agent drops the stdin handle first (which usually causes the CLI to exit cleanly), then waits up to 2s, then `child.kill()` if needed.

## Test runner

```bash
# All tests
cd gate4agent && cargo test --lib --tests

# Just parser fixtures
cargo test --lib ndjson::parsers::tests

# Just argv parity
cargo test --test spawn_argv_parity

# Just transport session integration
cargo test --test transport_session

# SessionEnd synthesis unit tests
cargo test --lib pipe::session::tests
```

If any test fails on a clean checkout with a released version, file an issue with:
- OS + version
- `cargo --version`
- Full test output
- Installed CLI versions (`claude --version`, `codex --version`, etc.) if running CLI-level tests

## Reporting a bug

1. Reproduce with `RUST_LOG=gate4agent=trace`
2. Capture the raw NDJSON (or PTY screen) from the CLI directly
3. File an issue on GitHub with: CLI name + version, gate4agent version, OS, raw output, expected vs actual event sequence
