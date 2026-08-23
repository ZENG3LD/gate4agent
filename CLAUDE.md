# gate4agent — agent harness / thin C2

Node (owns PTY/processes/workspaces/worktrees) + C2 (relay) + Harness
(task kernel, SQLite SWC) + TUI (`gate4agent-tui` harness mode,
`gate4agent-tui-light` direct-C2). Plans/handoffs/audits live in the
owner's private workspace documentation tree, not in this repository.

## Local endpoints & credentials

- node pipe `\\.\pipe\gate4agent-node`, api `:18310`; primary c2 pipe
  `\\.\pipe\gate4agent-c2`, api `:18320`; harness operator read `:18330`.
- The harness does NOT share the primary c2. It connects through a SECOND
  c2 instance of its own on pipe `\\.\pipe\gate4agent-c2-harness`, api
  `:18321`, so a live stack is four processes: node, two c2, harness.
  Bringing one up without that second instance fails at the harness with
  "`--c2-endpoint` connect failed ... (os error 2)".
- A c2 instance needs the per-node secret, not just `GATE4AGENT_C2_TOKEN`:
  without `GATE4AGENT_NODE_TOKEN_<NORMALIZED_ID>` it refuses to start.
  The normalized id is the `--node-id` uppercased with every non-alnum
  character replaced by `_`.
- Operator credential: `g4aho_` + 64 hex, env
  `GATE4AGENT_HARNESS_OPERATOR_TOKEN`. Node/c2 secrets:
  `GATE4AGENT_NODE_TOKEN`, `GATE4AGENT_NODE_TOKEN_<NORMALIZED_ID>`
  (uppercase, non-alnum→`_`), `GATE4AGENT_C2_TOKEN`. Env-only, never argv.
- Windows E2Es run only via
  `target\release\windows-headless-supervisor.exe <ms> <ABS exe> --exact <fn>`.
- `crates/gate4agent-tui` is its own cargo workspace and depends on
  `uzor-tui` from crates.io — every TUI build compiles the uzor
  dependency tree, so it is a slow build from cold and a large target.

## Build output

**One target directory per workspace: the root `target/` and the TUI's
own `crates/gate4agent-tui/target/`. Never a directory per task, agent or
scenario.** Each such directory is a FULL copy of the dependency build —
seven of them had accumulated to 52 GB, and the machine was failing
builds outright (`rustc` exiting `STATUS_STACK_BUFFER_OVERRUN`, the shell
reporting the paging file too small) until they were cleaned.

The practice they came from was documented here as avoiding Cargo's build
lock on parallel runs. That trade is not worth taking: the lock makes a
second build WAIT, which is the point of it — it never corrupts anything
and never loses work. Waiting is cheap; tens of gigabytes and a compiler
that cannot allocate are not.

`.gitignore` matches `target*/` rather than `target/` as a safety net for
directories that already exist, not as permission to create more.
