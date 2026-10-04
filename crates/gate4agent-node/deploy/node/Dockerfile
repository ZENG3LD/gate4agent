# Mode 2 — node as a container. Builds the standard kit (default features).
# Does not provision secrets, tokens, or WireGuard private keys.
#
# Build context is a directory that contains these siblings:
#   gate4agent/   dig2browser/   mail4agent/   session-restore/
# `deploy/node/docker-build.sh` stages that context (it excludes target/ and .git).
#
#   ./crates/gate4agent-node/deploy/node/docker-build.sh
#
# Provider CLIs are not installed unless you pass
#   --build-arg INSTALL_PROVIDER_CLIS=1 --build-arg PROVIDER_CLI_INSTALL='...'
# The service unit (mode 1) does not install them.

FROM rust:bookworm AS build
WORKDIR /src
COPY gate4agent /src/gate4agent
COPY dig2browser /src/dig2browser
COPY mail4agent /src/mail4agent
COPY session-restore /src/session-restore
WORKDIR /src/gate4agent
RUN cargo build --locked --release -p gate4agent-node

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        iproute2 \
        wireguard-tools \
        libssl3 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/gate4agent/target/release/gate4agent-node /usr/local/bin/gate4agent-node
COPY gate4agent/crates/gate4agent-node/deploy/node/install-provider-clis.sh /usr/local/sbin/install-provider-clis
COPY gate4agent/crates/gate4agent-node/deploy/node/gate4agent-node.service /etc/systemd/system/gate4agent-node.service
ARG INSTALL_PROVIDER_CLIS=0
ARG PROVIDER_CLI_INSTALL=
RUN chmod 0755 /usr/local/sbin/install-provider-clis \
    && INSTALL_PROVIDER_CLIS="$INSTALL_PROVIDER_CLIS" PROVIDER_CLI_INSTALL="$PROVIDER_CLI_INSTALL" /usr/local/sbin/install-provider-clis
# No ENV token, no COPY of a key. The process reads those from the operator at runtime.
ENTRYPOINT ["gate4agent-node"]
