# gate4agent — everything about providers: library, node, C2

gate4agent owns everything about CLI coding-agent providers (Claude Code,
Codex, Kimi, Grok): the library that spawns, streams, resumes, and owns
their subprocesses over PTY, pipe, ACP, and daemon transports behind one
API; the node process that wraps providers for one machine and serves them
over a wire; and the C2 that reaches the nodes on remote machines. The
harness, TUI, observation, and task/mail stack lives in `hatchery`, a
sibling repository that links these crates by path and talks to the node
only through its wire (`gate4agent-node-protocol`) and the C2's
(`gate4agent-c2-protocol`).
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

## Node and C2

- `gate4agent-node` (binary `gate4agent-node`) — the node server: wraps
  providers, owns PTY/inline/ACP sessions, the file browser, local git, and
  worktrees for its machine. `gate4agent-node-protocol` is its bounded wire
  contract, `gate4agent-node-wire` the transport/auth/client.
- `gate4agent-c2` (binary `gate4agent-c2`) — the relay that aggregates node
  state and routes commands down; `gate4agent-c2-protocol` and
  `gate4agent-c2-client` (CLI `gate4agent-c2ctl`) are its contract and client.
- `gate4agent-build-stamp` — the content hash of THIS repository's working
  tree that every node/C2 handshake carries; peers built from different
  trees refuse each other.
- The node names no harness. What a session's harness-MCP door is called —
  server name, argv, the environment variables that carry its endpoint and
  token — arrives from the caller in `HarnessMcpLaunchV1` on the reservation.
- The node derives no observation/telemetry vocabulary. It publishes
  `NodeEvent::Control` and the agent stream; the C2 relays a sanitized
  telemetry view of control events (`C2ControlEvent::detail`, capability
  `control-detail-v1`) to a client that negotiated it, and the client builds
  its own telemetry from that.

Lifecycle hooks are retired: nothing here installs a hook into any provider's
configuration, and no code or test may write under the real user home
(`~/.claude`, `~/.codex`, `~/.kimi-code`, `~/.grok`, `~/.gate4agent`).

## Forbidden

- No harness. The harness (tasks, runs, mail, observation, TUI) is
  `hatchery`'s; the node and C2 are gate4agent's and stay harness-agnostic.
- No task/harness/observation vocabulary. Task kanban, session extraction
  for a client app, and the read-only observation projection are `hatchery`
  concerns — do not grow them here.
- No dependency on any `hatchery-*` crate, in any direction, and no source
  path naming one. `hatchery` depends on `gate4agent` by path; the reverse
  dependency must never exist (`tests/no_hatchery_dependency.rs` enforces it
  over every manifest and every Rust source, node and C2 included).

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

The wire stamp (`gate4agent-build-stamp`) is this repository's, and hatchery's
binaries carry it through `gate4agent-node-protocol`: a wire-visible change
made here changes the stamp hatchery speaks with, so a hatchery binary built
before the change refuses a node or C2 built after it.
