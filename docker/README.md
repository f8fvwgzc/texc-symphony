# Running Symphony in Docker

The image holds one static-ish `symphony` binary with the web dashboard embedded, the tools that
workspace hooks and agents need (`git`, `bash`, `openssh-client`, CA certificates), `tini` as
PID 1, and, by default, Node.js 22 plus the Codex CLI (`@openai/codex`).

| | |
|---|---|
| Image | `ghcr.io/f8fvwgzc/texc-symphony` (`latest` = newest release, `X.Y.Z` / `X.Y` = pinned release, `edge` = newest `main`, `sha-<short>` = one commit) |
| Platforms | `linux/amd64`, `linux/arm64` (Apple Silicon, Graviton, Ampere, Raspberry Pi 4/5 with a 64-bit OS) |
| User | `symphony` (uid/gid 10001), home `/home/symphony`, working dir `/srv/symphony` |
| Port | `4000`: dashboard at `/`, API under `/api/v1`, health at `/api/v1/health` |
| Volumes | `/data` (SQLite run history + logs), `/workspaces` (per-issue workspaces) |
| Config | `/config/WORKFLOW.md` (mount read-only) |
| Entrypoint | `tini -- symphony`; default command `--i-understand-that-this-will-be-running-without-the-usual-guardrails /config/WORKFLOW.md` |

Environment set by the image: `SYMPHONY_HOST=0.0.0.0`, `SYMPHONY_PORT=4000`,
`SYMPHONY_DB_PATH=/data/symphony.db`, `SYMPHONY_LOGS_ROOT=/data/logs`. Everything else
(`SYMPHONY_DB_RETENTION_DAYS`, `RUST_LOG`, tracker credentials, ...) is documented in
[`.env.example`](.env.example).

> [!WARNING]
> Symphony runs coding agents unattended, and its HTTP API has no authentication. Only publish
> the port on loopback, or behind a firewall or an authenticating reverse proxy.

## Quick start (Docker Compose)

Requirements: Docker Engine 24+ with the Compose v2 plugin, or Docker Desktop.

```sh
cd docker
cp .env.example .env                                     # add LINEAR_API_KEY (or another tracker)
mkdir -p config && cp WORKFLOW.example.md config/WORKFLOW.md   # edit tracker + hooks
docker compose up -d
docker compose ps                                        # wait for "healthy"
open http://127.0.0.1:4000                               # or xdg-open / your browser
```

`docker compose up` uses the published image (`pull_policy: missing`). To build from this
checkout instead: `docker compose up -d --build`, or set `SYMPHONY_PULL_POLICY=build`.

Useful commands:

```sh
docker compose logs -f symphony                 # log stream (stdout; SYMPHONY_LOG_FORMAT=json for JSON)
docker compose exec symphony ls /data/logs/log  # rotating log files (symphony.log*)
curl -s http://127.0.0.1:4000/api/v1/health     # {"status":"ok",...}
docker compose restart symphony                 # after editing .env (WORKFLOW.md reloads by itself)
docker compose down                             # stop; volumes (history, workspaces) are kept
```

`config/` is mounted as a directory, so editing `config/WORKFLOW.md` is picked up by Symphony's
1 s reload poll even when your editor replaces the file. `workspace.root` in the workflow must be
`/workspaces` (or another path on a volume), otherwise workspaces vanish with the container.

### Codex authentication

Agents run `codex app-server`, which needs a Codex login. Pick one:

1. **Log in inside the container** (recommended for servers). The login is stored in the
   writable `codex-home` volume and Codex can refresh it:

   ```sh
   docker compose exec symphony codex login --device-auth
   # or, with an API key:
   printenv OPENAI_API_KEY | docker compose exec -T symphony codex login --with-api-key
   ```

2. **Reuse the host login read-only** (quick local runs). Run `codex login` on the host, then
   enable the overlay in `.env`:

   ```sh
   COMPOSE_FILE=compose.yaml:compose.codex-auth.yaml
   # CODEX_AUTH_JSON=~/.codex/auth.json   # default
   ```

   Compose refuses to start if the file is missing. A read-only `auth.json` cannot be refreshed
   by Codex, so prefer option 1 for long-running deployments.

Codex CLI flags change between versions; `docker compose exec symphony codex login --help` shows
what the bundled version (`CODEX_VERSION`, default `0.160.0`) supports.

### Git and repository access

Workspace hooks usually clone a repository. Tracker tokens (`LINEAR_API_KEY`, `GITHUB_TOKEN`, ...)
are deliberately stripped from the agent environment, so give git its own credential:

- HTTPS: put a read/write token in the clone URL via an env var your hook uses
  (`SOURCE_REPO_URL=https://x-access-token:${TOKEN}@github.com/org/repo.git`), or mount a git
  credential store into `/home/symphony`.
- SSH: mount a deploy key and `known_hosts`, for example
  `- ./ssh:/home/symphony/.ssh:ro` (files must be readable by uid 10001; the key must be mode
  `0600`).

Set `GIT_AUTHOR_NAME`/`GIT_AUTHOR_EMAIL` (and the committer pair) in `.env` for agent commits.

### SSH worker hosts

`worker.ssh_hosts` in `WORKFLOW.md` makes Symphony run agents on other machines over SSH. Mount
the client config and keys (for example `./ssh:/home/symphony/.ssh:ro`) and point
`SYMPHONY_SSH_CONFIG` at the config file if it is not `~/.ssh/config`.

### Codex sandbox inside a container

Codex's Linux sandbox (`thread_sandbox: workspace-write`) relies on kernel features (Landlock,
seccomp, user namespaces) that Docker's default seccomp/AppArmor profiles may block. If turns
fail with sandbox errors, either treat the container as the sandbox and use
`thread_sandbox: danger-full-access` in `WORKFLOW.md` (keep the container unprivileged, as here),
or run the container with a seccomp profile that allows the calls Codex needs. Never add
`--privileged`.

## Platforms

### macOS: Apple Silicon or Intel

Docker Desktop pulls the native image (`linux/arm64` on Apple Silicon, `linux/amd64` on Intel)
automatically; nothing to configure. Building locally is also native:

```sh
# from the repository root
docker buildx build -f docker/Dockerfile -t symphony:dev --load .
```

The Rust code is cross-compiled with [`tonistiigi/xx`](https://github.com/tonistiigi/xx) on the
build machine's own architecture, so building the *other* architecture is also fast (no Rust
compilation under emulation). Only the small runtime stage (`apt-get`) runs emulated; Docker
Desktop ships that emulation.

### Linux server

```sh
# Docker Engine + Compose plugin (Debian/Ubuntu: https://docs.docker.com/engine/install/)
git clone https://github.com/f8fvwgzc/texc-symphony.git && cd texc-symphony/docker
cp .env.example .env && mkdir -p config && cp WORKFLOW.example.md config/WORKFLOW.md
$EDITOR .env config/WORKFLOW.md
docker compose up -d
```

`restart: unless-stopped` brings Symphony back after reboots (the Docker service must be enabled:
`systemctl enable --now docker`). Keep `SYMPHONY_PUBLISH=127.0.0.1` and put a reverse proxy in
front (below), or set `SYMPHONY_PUBLISH=0.0.0.0` only on a private network. Compose applies the
CPU, memory and PID limits from `.env` (`SYMPHONY_CPUS`, `SYMPHONY_MEMORY`, `SYMPHONY_PIDS`);
size them for `agent.max_concurrent_agents` parallel Codex runs plus their builds.

Without Compose:

```sh
docker volume create symphony-data && docker volume create symphony-workspaces
docker run -d --name symphony --restart unless-stopped \
  --env-file docker/.env \
  -p 127.0.0.1:4000:4000 \
  -v symphony-data:/data -v symphony-workspaces:/workspaces \
  -v "$PWD/docker/config:/config:ro" \
  ghcr.io/f8fvwgzc/texc-symphony:latest
```

Pass different arguments by replacing the command, for example
`... ghcr.io/f8fvwgzc/texc-symphony:latest --i-understand-that-this-will-be-running-without-the-usual-guardrails --port 4000 /config/other.md`.
`docker run --rm ghcr.io/f8fvwgzc/texc-symphony:latest --version` prints the version.

### Behind a reverse proxy

The dashboard's live view uses **Server-Sent Events** (`GET /api/v1/events`, one long-lived
response with a heartbeat every 15 s). The proxy must not buffer it and must allow long reads;
otherwise the dashboard falls back to 5 s polling (it still works, just less live).

nginx:

```nginx
server {
    listen 443 ssl;
    server_name symphony.example.com;
    # ssl_certificate ...; ssl_certificate_key ...;
    auth_basic "Symphony";                     # the API itself has no auth
    auth_basic_user_file /etc/nginx/symphony.htpasswd;

    location / {
        proxy_pass http://127.0.0.1:4000;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
    }

    location = /api/v1/events {
        proxy_pass http://127.0.0.1:4000;
        proxy_http_version 1.1;
        proxy_set_header Connection "";        # keep the upstream connection open
        proxy_buffering off;                   # deliver each event immediately
        proxy_cache off;
        gzip off;
        proxy_read_timeout 1h;                 # > 15 s heartbeat; reconnects are automatic
    }
}
```

Caddy streams `text/event-stream` responses without buffering by default:

```caddyfile
symphony.example.com {
    basic_auth { admin <bcrypt-hash> }
    reverse_proxy 127.0.0.1:4000
}
```

Traefik and HAProxy need no special settings beyond an idle/read timeout above 15 s. For
Cloudflare or other CDNs, disable response buffering/caching for `/api/v1/events`.

## Building images

All builds run from the **repository root** (the build context), with
[`docker/Dockerfile`](Dockerfile).

```sh
# Native architecture, loaded into the local image store
docker buildx build -f docker/Dockerfile -t symphony:dev --load .

# Another single architecture (cross-compiled, still fast)
docker buildx build -f docker/Dockerfile --platform linux/amd64 -t symphony:dev-amd64 --load .

# Multi-arch, pushed to a registry (multi-platform images cannot be --load-ed into the classic
# image store; push them, or enable Docker Desktop's containerd image store)
docker buildx create --name symphony-builder --driver docker-container --use   # once
docker buildx build -f docker/Dockerfile \
  --platform linux/amd64,linux/arm64 \
  --build-arg VERSION=0.1.0 --build-arg REVISION="$(git rev-parse HEAD)" \
  -t ghcr.io/<owner>/<repo>:dev --push .
```

`make docker` and `make docker-multiarch` wrap the first and last commands.

Build arguments:

| Arg | Default | Meaning |
|---|---|---|
| `INSTALL_CODEX` | `true` | `false` leaves out Node.js and Codex (about 190 MiB instead of about 700 MiB uncompressed) |
| `CODEX_VERSION` | `0.160.0` | `@openai/codex` npm version |
| `RUST_VERSION` | `1.99.0` | Rust toolchain image tag (matches `rust-toolchain.toml`) |
| `NODE_VERSION` | `22` | Node.js major for the web build and the Codex runtime |
| `CARGO_BUILD_ARGS` | empty | extra `cargo build` flags |
| `SYMPHONY_VERSION_SUFFIX` | empty | appended to the version (`-nightly` for nightly builds) |
| `VERSION`, `REVISION`, `CREATED` | | OCI labels |

How the build is laid out:

1. `web`: `node:22-bookworm-slim` on the build platform; Corepack installs the pnpm version
   pinned in `web/package.json`; `pnpm install --frozen-lockfile && pnpm build`.
2. `rust`: `rust:1.99.0-bookworm` on the build platform plus `xx`; `xx-cargo build --release
   --locked` for the target platform, with BuildKit cache mounts for the cargo registry, git
   checkouts and `target/` (per target platform). `web/dist` is copied in first so
   `symphony-server` embeds it; `xx-verify` checks the binary's architecture.
3. `codex-npm`: `npm install --os=linux --cpu=<target>` of `@openai/codex`, on the build
   platform; only the target-platform `node` binary is copied from the target image.
4. `runtime`: `debian:bookworm-slim` with the packages above, non-root user, health check
   (`wget` against `/api/v1/health`).

Rebuilds after a source change only recompile the changed crates (the cache mounts keep
`target/`). `docker buildx prune --filter type=exec.cachemount` clears those caches.
