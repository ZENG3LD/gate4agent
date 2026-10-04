#!/bin/sh
# Mode 3 — node as a QEMU guest. Stub: it does not download an image, start
# QEMU, or provision secrets. The guest runs the same kit binary as mode 1.
set -eu
HERE=$(CDPATH= cd -- "$(dirname "$0")" && pwd)
REPO=$(CDPATH= cd -- "$HERE/../../../.." && pwd)
KIT_BIN=${KIT_BIN:-$REPO/target/release/gate4agent-node}

cat <<EOF
QEMU guest uses the standard kit, not a bare build and not a second tree.
  cargo build --release -p gate4agent-node
  binary: $KIT_BIN

Not executed by this stub:
  1. Create a generic guest disk. Do not copy host credentials, tokens, or SSH keys onto it.
  2. Inside the guest, install iproute2 and wireguard-tools so kernel WireGuard can come up.
  3. Install provider CLIs as packages only, by running:
       INSTALL_PROVIDER_CLIS=1 PROVIDER_CLI_INSTALL='<image package command>' $HERE/install-provider-clis.sh
     That script prints the package names (claude, codex, kimi, grok) and does not copy session files.
  4. Copy the kit binary and gate4agent-node.service into the guest. Copy no env file with secrets.
  5. Boot with something like:
       qemu-system-x86_64 -m 2048 -nographic -drive file=node-guest.qcow2,if=virtio
EOF
