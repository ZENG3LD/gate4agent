# gate4agent node kit

Default `gate4agent-node` is the standard kit. Feature `kit` (on by default) links:

1. `dig2browser` — path `../../../dig2browser` (local node browser machine; not the crates.io 0.5.0 tarball)
2. `mail4agent` — path `../../../mail4agent` (crates.io 0.1.2 is binary-only)
3. session-restore — `claude-session-restore` and `codex-session-restore` 0.1.3 from crates.io; `grok-session-restore` and `kimi-session-restore` path deps because crates.io 0.1.3 is binary-only
4. kernel WireGuard — feature `wireguard`, `ip link add type wireguard` / `wg`. Node and C2 are peers (either side may dial). Not the UDP+ChaCha mesh underlay, not `open_wireguard_daemon_stub`, and not HQ.

`bare` builds a node with those cores off:

```
cargo build -p gate4agent-node --no-default-features --features bare
```

`cargo build -p gate4agent-node --features bare` without `--no-default-features` is a build error (kit and bare together).

`hatchery-git` is an optional extra and is not default. It does not depend on hatchery-tui or any hatchery crate. Hatchery git helpers are not part of the core kit.

Provider CLIs (`claude`, `codex`, `kimi`, `grok`) are packages. They are not copied credentials.
The node flag `--install-provider-clis` is **off by default**. When set (or when
`INSTALL_PROVIDER_CLIS=1` for the Docker/QEMU install script), the install path
checks PATH for each binary and downloads only the missing ones. It does not run
provider logins. Without the flag, nothing is downloaded. Mode 1's unit file does
not pass the flag.

## Mode 1 — service (implemented)

Entry point: the `gate4agent-node` binary from a default-feature build, installed as `deploy/node/gate4agent-node.service`.

The process links the four cores at startup (`force_link`). It does not start Chrome, the mailbox, or a tunnel unless the operator sets `GATE4AGENT_WG_*`. Those variables name a key *path* and the peer; the unit file does not contain secrets. If `ip link add type wireguard` fails, the process exits. It does not fall back to the mesh stub.

## Mode 2 — container (image build)

Entry point: `deploy/node/Dockerfile`, invoked by `deploy/node/docker-build.sh`. The image runs the same kit binary. `ENTRYPOINT` is `gate4agent-node`. No secret is copied into the image. Provider CLIs stay uninstalled unless `INSTALL_PROVIDER_CLIS=1` and `PROVIDER_CLI_INSTALL` is set at build time (`install-provider-clis.sh`).

## Mode 3 — QEMU guest (stub)

Entry point: `deploy/node/qemu-guest.sh`. It prints the guest flow and exits. It uses the same kit binary as mode 1. Provider CLIs are the same package script as mode 2. The stub does not boot QEMU and does not provision secrets.
