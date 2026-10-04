#!/bin/sh
# Provider CLIs for deploy modes 2 (container) and 3 (QEMU guest), and for
# gate4agent-node --install-provider-clis. Installs packages only. Does not
# copy credentials, tokens, or session homes. Does not run provider logins.
set -eu

echo "provider CLI packages (not credentials):"
echo "  claude   npm:@anthropic-ai/claude-code"
echo "  codex    npm:@openai/codex"
echo "  kimi     official kimi installer (binary only; no login)"
echo "  grok     official grok installer (binary only; no login)"

# Accept either the env used by Dockerfile build-args or the node CLI flag
# path that exports INSTALL_PROVIDER_CLIS=1.
if [ "${INSTALL_PROVIDER_CLIS:-0}" != "1" ]; then
  echo "install-provider-clis: not installing (set INSTALL_PROVIDER_CLIS=1 or pass --install-provider-clis)."
  exit 0
fi

# Optional override: operator-supplied package command runs instead of the
# per-binary defaults below. Still packages only — no home, no token.
if [ -n "${PROVIDER_CLI_INSTALL:-}" ]; then
  # shellcheck disable=SC2086
  $PROVIDER_CLI_INSTALL
  exit 0
fi

have_cmd() {
  command -v "$1" >/dev/null 2>&1
}

install_npm_global() {
  pkg=$1
  if ! have_cmd npm; then
    echo "install-provider-clis: npm is required to install $pkg" >&2
    return 1
  fi
  npm install -g "$pkg"
}

# Download only when the binary is missing. Never invoke login / auth flows.
missing=0
if have_cmd claude; then
  echo "install-provider-clis: claude already present ($(command -v claude))"
else
  echo "install-provider-clis: downloading claude (@anthropic-ai/claude-code)"
  install_npm_global @anthropic-ai/claude-code || missing=1
fi

if have_cmd codex; then
  echo "install-provider-clis: codex already present ($(command -v codex))"
else
  echo "install-provider-clis: downloading codex (@openai/codex)"
  install_npm_global @openai/codex || missing=1
fi

if have_cmd kimi; then
  echo "install-provider-clis: kimi already present ($(command -v kimi))"
else
  echo "install-provider-clis: kimi missing; set PROVIDER_CLI_INSTALL to an image package command for kimi (no credential copy)." >&2
  missing=1
fi

if have_cmd grok; then
  echo "install-provider-clis: grok already present ($(command -v grok))"
else
  echo "install-provider-clis: grok missing; set PROVIDER_CLI_INSTALL to an image package command for grok (no credential copy)." >&2
  missing=1
fi

if [ "$missing" -ne 0 ]; then
  echo "install-provider-clis: one or more provider CLIs were not installed" >&2
  exit 1
fi
echo "install-provider-clis: done (packages only; no logins run)"
