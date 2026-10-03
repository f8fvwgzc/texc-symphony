# syntax=docker/dockerfile:1
#
# Symphony container image: the `symphony` binary with the web dashboard embedded.
#
# Multi-arch (linux/amd64 + linux/arm64) and fast on any host:
#   * the web bundle is built once, natively, on $BUILDPLATFORM (it is architecture-independent);
#   * the Rust workspace is cross-compiled on $BUILDPLATFORM with tonistiigi/xx, never under QEMU;
#   * only the small runtime stage (apt-get) runs on $TARGETPLATFORM.
#
# Build from the repository root (see docker/README.md):
#   docker buildx build -f docker/Dockerfile -t symphony:dev --load .
#   docker buildx build -f docker/Dockerfile --platform linux/amd64,linux/arm64 -t <image> --push .

ARG RUST_VERSION=1.99.0
ARG NODE_VERSION=22
ARG DEBIAN_RELEASE=bookworm
ARG XX_VERSION=1.9.0
# Set to "false" to leave Node.js and the Codex CLI out of the image (bring your own agent).
ARG INSTALL_CODEX=true
# Pinned @openai/codex npm version (bump deliberately; agents speak its app-server protocol).
ARG CODEX_VERSION=0.160.0

# ---------------------------------------------------------------------------------------------
# Cross-compilation helpers (xx-cargo, xx-apt-get, xx-clang, xx-verify).
FROM --platform=$BUILDPLATFORM tonistiigi/xx:${XX_VERSION} AS xx

# ---------------------------------------------------------------------------------------------
# (a) Web dashboard -> /web/dist. Runs natively; the output is plain static files.
FROM --platform=$BUILDPLATFORM node:${NODE_VERSION}-${DEBIAN_RELEASE}-slim AS web
ENV PNPM_HOME=/pnpm \
    COREPACK_ENABLE_DOWNLOAD_PROMPT=0 \
    CI=true
ENV PATH=$PNPM_HOME:$PATH
WORKDIR /web
# Corepack installs exactly the pnpm version pinned by `packageManager` in package.json.
COPY web/package.json web/pnpm-lock.yaml ./
RUN corepack enable && corepack install
RUN --mount=type=cache,id=symphony-pnpm-store,target=/pnpm/store \
    pnpm config set store-dir /pnpm/store && \
    pnpm install --frozen-lockfile
COPY web/ ./
RUN pnpm build

# ---------------------------------------------------------------------------------------------
# (b) Rust workspace, cross-compiled for $TARGETPLATFORM.
FROM --platform=$BUILDPLATFORM rust:${RUST_VERSION}-${DEBIAN_RELEASE} AS rust
COPY --from=xx / /
# Host-side toolchain for the C/C++ dependencies (aws-lc-sys via reqwest/rustls, bundled SQLite
# via rusqlite): clang/lld drive the cross-compile, cmake/perl are used by aws-lc-sys.
RUN apt-get update && \
    apt-get install -y --no-install-recommends clang lld cmake perl pkg-config && \
    rm -rf /var/lib/apt/lists/*
ARG TARGETPLATFORM
# Target-side libc/libstdc++ headers and libraries, plus the rustup target for $TARGETPLATFORM.
RUN xx-apt-get install -y --no-install-recommends xx-c-essentials xx-cxx-essentials && \
    xx-cargo --setup-target-triple && \
    xx-clang --setup-target-triple
WORKDIR /src
# Only what the build needs; the toolchain is pinned by the base image tag (RUST_VERSION), so
# rust-toolchain.toml is not copied (it would make rustup fetch extra components).
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY xtask ./xtask
# symphony-server's build script turns the API contract into /api/openapi.json ...
COPY docs/api/openapi.yaml ./docs/api/openapi.yaml
# ... and embeds the dashboard bundle (it would fall back to a placeholder page without it).
COPY --from=web /web/dist ./web/dist
# Optional suffix baked into `symphony --version` (e.g. "-nightly"); empty for tagged releases.
ARG SYMPHONY_VERSION_SUFFIX=""
# Extra cargo flags, e.g. "--features ...". The default builds every crate in default-members,
# which compiles all C dependencies even while the binary does not link them yet.
ARG CARGO_BUILD_ARGS=""
ENV CARGO_TERM_COLOR=never \
    CARGO_INCREMENTAL=0 \
    SYMPHONY_WEB_DIST=/src/web/dist \
    SYMPHONY_VERSION_SUFFIX=${SYMPHONY_VERSION_SUFFIX}
RUN --mount=type=cache,id=symphony-cargo-registry,target=/usr/local/cargo/registry,sharing=shared \
    --mount=type=cache,id=symphony-cargo-git,target=/usr/local/cargo/git,sharing=shared \
    --mount=type=cache,id=symphony-target-${TARGETPLATFORM},target=/src/target,sharing=locked \
    xx-cargo build --release --locked ${CARGO_BUILD_ARGS} && \
    triple="$(xx-cargo --print-target-triple)" && \
    xx-verify "target/${triple}/release/symphony" && \
    install -D -m 0755 "target/${triple}/release/symphony" /out/symphony

# ---------------------------------------------------------------------------------------------
# Optional Codex CLI (@openai/codex). npm resolves the platform package for $TARGETARCH, so this
# runs natively too; only the `node` binary itself is taken from the target-platform image.
FROM --platform=$BUILDPLATFORM node:${NODE_VERSION}-${DEBIAN_RELEASE}-slim AS codex-npm
ARG TARGETARCH
ARG CODEX_VERSION
RUN set -eu; \
    case "$TARGETARCH" in \
      amd64) cpu=x64 ;; \
      arm64) cpu=arm64 ;; \
      *) echo "unsupported TARGETARCH for codex: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    npm install --global --prefix /out/opt/codex --os=linux --cpu="$cpu" \
      --no-audit --no-fund --omit=dev "@openai/codex@${CODEX_VERSION}"; \
    test -n "$(find /out/opt/codex/lib/node_modules -path "*linux-${cpu}*" -print -quit)"; \
    mkdir -p /out/usr/local/lib/node_modules /out/usr/local/bin; \
    cp -a /usr/local/lib/node_modules/npm /usr/local/lib/node_modules/corepack /out/usr/local/lib/node_modules/; \
    ln -s ../lib/node_modules/npm/bin/npm-cli.js /out/usr/local/bin/npm; \
    ln -s ../lib/node_modules/npm/bin/npx-cli.js /out/usr/local/bin/npx; \
    ln -s ../lib/node_modules/corepack/dist/corepack.js /out/usr/local/bin/corepack

FROM node:${NODE_VERSION}-${DEBIAN_RELEASE}-slim AS node-target

FROM scratch AS codex-true
COPY --from=codex-npm /out/ /
COPY --from=node-target /usr/local/bin/node /usr/local/bin/node

FROM scratch AS codex-false

FROM codex-${INSTALL_CODEX} AS codex

# ---------------------------------------------------------------------------------------------
# (c) Runtime.
FROM debian:${DEBIAN_RELEASE}-slim AS runtime
ARG VERSION=0.1.0
ARG REVISION=unknown
ARG CREATED=""
ARG CODEX_VERSION
ARG INSTALL_CODEX
LABEL org.opencontainers.image.title="symphony" \
      org.opencontainers.image.description="Symphony: turn tracker work into isolated, autonomous coding-agent runs" \
      org.opencontainers.image.source="https://github.com/f8fvwgzc/texc-symphony" \
      org.opencontainers.image.url="https://github.com/f8fvwgzc/texc-symphony" \
      org.opencontainers.image.documentation="https://github.com/f8fvwgzc/texc-symphony/blob/main/docker/README.md" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.vendor="Symphony contributors" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}" \
      org.opencontainers.image.created="${CREATED}" \
      org.opencontainers.image.base.name="docker.io/library/debian:bookworm-slim" \
      dev.symphony.codex.installed="${INSTALL_CODEX}" \
      dev.symphony.codex.version="${CODEX_VERSION}"

# git/bash/ssh for workspace hooks, SSH workers and agents; tini as PID 1 so signals reach
# symphony and zombie hook/agent processes are reaped; wget for the health check.
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
      bash ca-certificates git openssh-client tini wget && \
    rm -rf /var/lib/apt/lists/* && \
    groupadd --gid 10001 symphony && \
    useradd --uid 10001 --gid 10001 --home-dir /home/symphony --create-home \
      --shell /bin/bash symphony && \
    install -d -o symphony -g symphony -m 0750 /data /workspaces /srv/symphony /home/symphony/.codex && \
    install -d -m 0755 /config

COPY --from=codex / /
COPY --from=rust /out/symphony /usr/local/bin/symphony

ENV PATH=/opt/codex/bin:$PATH \
    SYMPHONY_HOST=0.0.0.0 \
    SYMPHONY_PORT=4000 \
    SYMPHONY_DB_PATH=/data/symphony.db \
    SYMPHONY_LOGS_ROOT=/data/logs

USER symphony:symphony
WORKDIR /srv/symphony
VOLUME ["/data", "/workspaces"]
EXPOSE 4000

HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
  CMD wget -q -T 4 -O /dev/null "http://127.0.0.1:${SYMPHONY_PORT}/api/v1/health" || exit 1

ENTRYPOINT ["tini", "--", "symphony"]
CMD ["--i-understand-that-this-will-be-running-without-the-usual-guardrails", "/config/WORKFLOW.md"]
