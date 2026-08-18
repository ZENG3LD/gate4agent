# gate4agent — agent harness / thin C2

Node (owns PTY/processes/workspaces/worktrees) + C2 (relay) + Harness
(task kernel, SQLite SWC) + TUI (`gate4agent-tui` harness mode,
`gate4agent-tui-light` direct-C2). Plans/handoffs/audits live in the
owner's private workspace documentation tree, not in this repository.

## Local endpoints & credentials

- node pipe `\\.\pipe\gate4agent-node`, api `:18310`; primary c2 pipe
  `\\.\pipe\gate4agent-c2`, api `:18320`; harness operator read `:18330`.
- Operator credential: `g4aho_` + 64 hex, env
  `GATE4AGENT_HARNESS_OPERATOR_TOKEN`. Node/c2 secrets:
  `GATE4AGENT_NODE_TOKEN`, `GATE4AGENT_NODE_TOKEN_<NORMALIZED_ID>`
  (uppercase, non-alnum→`_`), `GATE4AGENT_C2_TOKEN`. Env-only, never argv.
- Windows E2Es run only via
  `target\release\windows-headless-supervisor.exe <ms> <ABS exe> --exact <fn>`.
- `crates/gate4agent-tui` is its own cargo workspace and carries a path
  dependency on `../../../uzor/uzor-tui` — every TUI build compiles the
  uzor dependency tree.
