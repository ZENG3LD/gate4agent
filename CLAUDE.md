# gate4agent — CLI-agent transport library

gate4agent is a library, not a product: it spawns, streams, resumes, and
owns interactive CLI coding-agent subprocesses (Claude Code, Codex, Kimi,
Grok) over PTY, pipe, ACP, and daemon transports, behind one API. The
node/c2/harness/TUI stack that used to sit on top of it now lives in
`hatchery`, a sibling repository that links these crates by path.
Plans/handoffs/audits live in the owner's private workspace documentation
tree, not in this repository.

## Two generations

- **Transport core** — the root `gate4agent` crate (`src/`) plus
  `gate4agent-pty` (the in-house PTY backend). Spawns and owns the vendor
  CLI subprocess directly: structured inline (JSONL), PTY (ConPTY / unix
  pty), plus the ACP and daemon compatibility surfaces. This is the
  original generation and the one every downstream crate ultimately calls
  into.
- **Session runtime substrate** — `gate4agent-catalog` → `gate4agent-kernel`
  → `gate4agent-runtime-native` → `gate4agent-shell-native`, plus
  `gate4agent-types`, `gate4agent-adapters`, `gate4agent-engine`,
  `gate4agent-handle`, `gate4agent-tool-protocol`, `gate4agent-tool-engine`,
  `gate4agent-provider-ports`, `gate4agent-shell-history`,
  `gate4agent-shell-capabilities`, `gate4agent-shell-hooks`,
  `gate4agent-shell-managed-hooks`, `gate4agent-shell-one-shot`. This is
  the second generation: the managed-session substrate a node embeds
  (catalog lookup → kernel session lifecycle → native runtime execution →
  native shell integration), and it calls DOWN into the transport core for
  the actual subprocess. It does not replace the transport core; it wraps
  it.

Hooks belong here permanently: `~/.gate4agent/agent-hooks`,
`GATE4AGENT_HOOK_*`. They are part of the session runtime substrate a node
embeds, not workbench/task vocabulary, and they do not move to `hatchery`.

## Forbidden

- No daemon binary. This repository ships no long-running service — no
  node, no c2, no harness. `gate4agent-testkit` and its
  `windows-headless-supervisor` binary are test infrastructure, not a
  product daemon.
- No wire protocol. Node/c2/harness wire types (`*-protocol` crates) live
  in `hatchery`, not here.
- No task/harness/observation vocabulary. Task kanban, session extraction
  for a client app, delivery of skills/plugins/MCP config, and read-only
  observation projection are `hatchery` concerns — do not grow them here.
- No dependency on any `hatchery-*` crate, in any direction. `hatchery`
  depends on `gate4agent` by path; the reverse dependency must never exist.

## Windows PTY tests

Run only through the headless test supervisor — it suppresses Windows
fault dialogs and enforces a hard per-test timeout that plain `cargo test`
cannot:

```
target\release\windows-headless-supervisor.exe <timeout_ms> <ABS path to test exe> --exact <test_fn>
```

Tests gated by `require_windows_headless_supervisor_for_test()` reject
themselves outright if run any other way.

## Build output

One target directory per workspace: the root `target/`, nothing per task,
agent, or scenario. A per-run `--target-dir` is a full copy of the
dependency build; a handful of them pile up to tens of gigabytes fast
enough to fail builds outright on disk/paging-file pressure. Cargo's build
lock already makes a second concurrent build wait instead of corrupting
anything — that wait is cheap, a spare target directory is not.

## On crates.io

`gate4agent`, `gate4agent-types`, `gate4agent-pty`, `gate4agent-adapters`,
`gate4agent-catalog` (0.4.0), and `g4a` (a placeholder reserving the name).
The rest of the session runtime substrate crates are not yet published.

## Downstream: hatchery

`hatchery` links these crates by path (`../../../gate4agent/crates/...`, or
the root `gate4agent` crate directly) — it is not a crates.io consumer for
local development. A public-API change here (root crate, or any
`gate4agent-*` crate hatchery depends on) can break hatchery's build;
check `hatchery`'s crates after any such change, before considering the
change done.

hatchery's own build stamp (`hatchery-build-stamp`) hashes only the
`hatchery` repository's working tree — a wire-visible change made here in
`gate4agent-types` (or any other crate hatchery links by path) does not
change that stamp, even though it changes what hatchery's binaries
actually speak on the wire. Do not treat a stable hatchery build stamp as
evidence that nothing wire-relevant changed upstream.
