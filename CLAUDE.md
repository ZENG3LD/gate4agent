# gate4agent — agent harness / thin C2

Node (owns PTY/processes/workspaces/worktrees) + C2 (relay) + Harness
(task kernel, SQLite SWC) + TUI. Both clients speak ONLY the harness
operator wire: `gate4agent-tui` against a durable harness, and
`gate4agent-tui-light` against `gate4agent-harness-light`, which it hosts
in-process over c2 with no task kernel behind it. Neither app speaks c2
itself. Plans/handoffs/audits live in the owner's private workspace
documentation tree, not in this repository.

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

## Running the live stack and the TUI

Bring the four processes up in order — node, both c2, harness — then the
TUI. Secrets live in `.run/p0-live-20260818/relaunch.env` and are loaded
into the environment; never pass them in argv.

```powershell
$root = "C:\Users\VA PC\CODING\ML_TRADING\nemo\gate4agent"
$R    = Join-Path $root ".run\p0-live-20260818"
$bin  = Join-Path $root "target\release"
Get-Content (Join-Path $R "relaunch.env") | ForEach-Object {
  if ($_ -match '^([A-Z0-9_]+)=(.*)$') { Set-Item -Path ("env:" + $matches[1]) -Value $matches[2] } }
$env:GATE4AGENT_NODE_TOKEN_OPBOX_WINDOWS_X86_64_1D67E837F8FA = $env:GATE4AGENT_NODE_TOKEN
```

`Start-Process -ArgumentList` must get ONE quoted string, not an array.
An array is joined with spaces and nothing is re-quoted, so every path
holding a space is split — node reports `workspace 'gate4agent' root
'C:\Users\VA' is invalid: path is not a directory` and exits.

- node — `--node-id opbox-windows-x86-64-1d67e837f8fa`, one
  `--workspace "<name>=<abs path>"` per repo, a matching
  `--worktree-mode <name>=manual`, one
  `--history-root "<provider>|<layout>|<abs root>"` per provider, then
  `--endpoint \.\pipe\gate4agent-node --api-listen 127.0.0.1:18310`.
- c2 (primary) — `--node opbox-windows-x86-64-1d67e837f8fa=\.\pipe\gate4agent-node
  --api-listen 127.0.0.1:18320 --control-endpoint \.\pipe\gate4agent-c2`.
- c2 (harness's own) — the same line with `:18321` and
  `\.\pipe\gate4agent-c2-harness`.
- harness — `--harness-db "$R\harness.sqlite3" --observation-db
  "$R\observation.sqlite3" --c2-endpoint \.\pipe\gate4agent-c2-harness
  --read-bind 127.0.0.1:18330`.

Up means all four ports listening and node `/health` returning 200. c2's
`/status` wants a credential, so a bare request coming back non-2xx is not
by itself a failure — read `<name>.err.log` in `$R` before deciding.

Build the TUI from INSIDE its own workspace. `cargo build -p gate4agent-tui`
at the repo root fails with `did not match any packages`, because
`crates/gate4agent-tui/Cargo.toml` declares its own `[workspace]`:

```powershell
cd (Join-Path $root "crates\gate4agent-tui")
cargo build --release --bin gate4agent-tui --bin gate4agent-tui-light
```

Launch it in Windows Terminal exactly like this, exe path quoted:

```powershell
$tui = Join-Path $root "crates\gate4agent-tui\target\release\gate4agent-tui.exe"
$q   = '"'
Start-Process "$env:LOCALAPPDATA\Microsoft\WindowsApps\wt.exe" `
  -ArgumentList "-w new --title G4A $q$tui$q --harness-operator 127.0.0.1:18330"
```

**Do not add flags to that line.** `--size` and `--pos` are window options
accepted only BEFORE `-w`; placed after it, `wt` reads the tail as the
command to run, opens a window titled `Error` and starts nothing. They are
not options of `new-tab` at all. Want a bigger window — resize it by hand.

`wt` exits 0 whether or not it started anything, and `MainWindowTitle` is a
property of the PROCESS while a single `WindowsTerminal` process hosts every
window — so an unrelated window's title gets read back and believed. Verify
a launch by the thing you launched: `Get-Process gate4agent-tui`. To capture
the window, force it foreground first (`AttachThreadInput` +
`SetForegroundWindow`); `CopyFromScreen` over its rect otherwise captures
whatever happens to be on top of it.

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
