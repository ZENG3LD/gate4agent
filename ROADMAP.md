# gate4agent Roadmap

The current direction closes the gap between the node/c2/harness/TUI layers
(see [README.md](README.md#layers)) and the target shape: one protocol between
the client app and its harness, a light harness standing next to the full one
behind that same protocol, full node-surface coverage reachable through the
harness, and multi-client cowork on top. Phases build in order; each is
owner-visible before the next starts.

## P1 — one app↔harness protocol contract

Extend the harness operator surface into the single interface the client app
is allowed to use: node roster, sessions with interactive verbs (spawn, input,
resize, stop), file browser, git, and an event push subscription — one
long-lived connection replacing snapshot polling. Every place the app still
speaks directly to c2 gets classified: becomes a light-harness capability, or
is retired.

## P2 — light-harness extraction, app split

Move the app's direct-c2 data plane behind the P1 contract into its own
component, the light harness, hosted inside the light TUI binary and spoken to
through the same client code as the full harness. The client app itself splits
into an app-backend (panel/layout/preference state, persisted per user) and a
shell (the rendering surface). One app codebase runs unchanged against either
harness.

## P3 — full node-surface relay

Everything a node offers — PTY, inline runs, files, git, worktrees, history,
context, delivery — reachable through the harness for every node on every
attached c2, so light and full windows are indistinguishable in capability.

## P4 — cowork

Many client apps attached to one harness: user identity, an actor stamped on
every operation, input-controller arbitration, and event push fan-out to every
attached client. This is the multi-user backbone the earlier phases build
toward.

See [README.md](README.md) for the current architecture and
[DEBUGGING.md](DEBUGGING.md) for known issues.
