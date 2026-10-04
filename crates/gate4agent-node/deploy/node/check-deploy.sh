#!/bin/sh
# Static check for mode 1/2/3 entry points. No Docker build, no QEMU boot.
# Prefer: cargo test -p gate4agent-node --lib deploy_artifacts
set -eu
HERE=$(CDPATH= cd -- "$(dirname "$0")" && pwd)
test -f "$HERE/gate4agent-node.service"
test -f "$HERE/Dockerfile"
test -f "$HERE/docker-build.sh"
test -x "$HERE/qemu-guest.sh" -o -f "$HERE/qemu-guest.sh"
bash -n "$HERE/docker-build.sh"
bash -n "$HERE/qemu-guest.sh"
bash -n "$HERE/install-provider-clis.sh"
# Run the lib assertions when cargo is available.
if command -v cargo >/dev/null 2>&1; then
  REPO=$(CDPATH= cd -- "$HERE/../../../.." && pwd)
  (CDPATH= cd -- "$REPO" && cargo test -p gate4agent-node --lib deploy_artifacts)
fi
echo "deploy static check OK"
