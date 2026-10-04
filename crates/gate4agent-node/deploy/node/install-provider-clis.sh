#!/bin/sh
# Provider CLIs for deploy modes 2 (container) and 3 (QEMU guest) only.
# Installs packages. Does not copy credentials, tokens, or session homes.
set -eu

echo "provider CLI packages (not credentials):"
echo "  claude   npm:@anthropic-ai/claude-code"
echo "  codex    npm:@openai/codex"
echo "  kimi     package:kimi (image package set; no credential files)"
echo "  grok     package:grok (image package set; no credential files)"

if [ "${INSTALL_PROVIDER_CLIS:-0}" != "1" ]; then
  echo "install-provider-clis: not installing (set INSTALL_PROVIDER_CLIS=1)."
  exit 0
fi

if [ -z "${PROVIDER_CLI_INSTALL:-}" ]; then
  echo "install-provider-clis: INSTALL_PROVIDER_CLIS=1 but PROVIDER_CLI_INSTALL is empty." >&2
  echo "Set PROVIDER_CLI_INSTALL to the image package command. This script will not invent a credential copy." >&2
  exit 1
fi

# The operator-supplied command installs packages only. It is not given a home directory or a token.
# shellcheck disable=SC2086
$PROVIDER_CLI_INSTALL
