# Deploying Symphony

Symphony is one self-contained binary (`symphony`) with the web dashboard embedded. It needs a
`WORKFLOW.md`, credentials for one tracker, and the tools its agents use. This page covers running
it as a plain binary, in Docker, and under systemd, plus reverse proxies, backups and upgrades.

> [!WARNING]
> Symphony runs coding agents unattended, and its HTTP API has no authentication. Run it in a
> trusted environment, keep the HTTP port on loopback or behind an authenticating proxy, and give
> agents only the credentials they need.

## Command line and environment

```text
symphony --i-understand-that-this-will-be-running-without-the-usual-guardrails \
         [--logs-root <path>] [--port <port>] [--host <addr>] \
         [--db-path <path> | --no-db] [path-to-WORKFLOW.md]
symphony workspace before-remove [--branch B] [--repo R]
symphony --version
```

Without the acknowledgement flag Symphony prints the guardrails banner and exits 1. The workflow
path defaults to `./WORKFLOW.md`.

| Variable | Default | Meaning |
|---|---|---|
| `SYMPHONY_WORKFLOW` | `./WORKFLOW.md` | workflow file (the positional argument wins) |
| `SYMPHONY_HOST` | `127.0.0.1` | HTTP bind address (`0.0.0.0` in containers) |
| `SYMPHONY_PORT` | unset (no HTTP server) | HTTP port; same as `--port` or `server.port` in `WORKFLOW.md`; `0` picks a free port |
| `SYMPHONY_LOGS_ROOT` | `./log` | directory for the rotating log files |
| `SYMPHONY_DB_PATH` | `./data/symphony.db` | SQLite run history; `off` disables it (same as `--no-db`) |
| `SYMPHONY_DB_RETENTION_DAYS` | `30` | days of finished runs to keep; `0` keeps everything |
| `RUST_LOG` | | `tracing` filter, e.g. `info` or `symphony=debug,info` |
| `SYMPHONY_SSH_CONFIG` | | SSH client config for `worker.ssh_hosts` |
| `LINEAR_API_KEY`, `GITHUB_TOKEN`, `GITLAB_PAT`, `JIRA_BASE_URL` / `JIRA_EMAIL` / `JIRA_API_TOKEN`, `ASANA_PAT` | | tracker credentials (only the one your workflow uses); kept out of the Codex child environment |

The HTTP server serves the dashboard at `/`, the API under `/api/v1` (see
[`api/README.md`](api/README.md)) and a liveness probe at `GET /api/v1/health`, which never touches
the orchestrator.

## Runtime requirements

On every machine that runs agents (the Symphony host, and each SSH worker):

- `git`, `bash`, `openssh-client`, CA certificates;
- the Codex CLI (`npm install -g @openai/codex`, needs Node.js 22+), logged in (`codex login`);
- whatever your workspace hooks and agents use (language toolchains, `gh`, ...).

## Option 1: release binary

Release assets are raw executables plus checksums, named
`symphony-<vX.Y.Z|nightly>-<linux_x86_64|linux_arm64|macos_x86_64|macos_arm64>`. The Linux builds
are static (musl) and run on any distribution.

```sh
version=v0.1.0 target=linux_x86_64          # or linux_arm64, macos_arm64, macos_x86_64
base=https://github.com/f8fvwgzc/texc-symphony/releases/download/$version
curl -fL -O "$base/symphony-$version-$target" -O "$base/symphony-$version-$target.sha256"
shasum -a 256 -c "symphony-$version-$target.sha256"
install -m 0755 "symphony-$version-$target" /usr/local/bin/symphony
# macOS only, for a browser download: xattr -d com.apple.quarantine /usr/local/bin/symphony
gh attestation verify /usr/local/bin/symphony --repo f8fvwgzc/texc-symphony   # optional
```

Run it:

```sh
export LINEAR_API_KEY=...
symphony --i-understand-that-this-will-be-running-without-the-usual-guardrails --port 4000 ./WORKFLOW.md
open http://127.0.0.1:4000
```

### From source

Requires Rust 1.99 (pinned by `rust-toolchain.toml`), Node.js 22.12+ and pnpm (`corepack enable`).

```sh
make build              # pnpm build (web/dist) + cargo build --release -p symphony
make run WORKFLOW=./WORKFLOW.md PORT=4000
```

Without `web/dist` the binary still builds but serves a placeholder page instead of the
dashboard.

## Option 2: Docker (any OS, amd64 or arm64)

The image `ghcr.io/f8fvwgzc/texc-symphony` bundles the binary, git, ssh, Node.js and the Codex
CLI, runs as uid 10001 under `tini`, keeps history and logs on `/data` and workspaces on
`/workspaces`. See [`docker/README.md`](../docker/README.md) for Compose, Apple Silicon/Intel,
Linux servers, Codex login and multi-arch builds. In short:

```sh
cd docker && cp .env.example .env && mkdir -p config && cp WORKFLOW.example.md config/WORKFLOW.md
docker compose up -d
```

## Option 3: systemd (Linux server, binary install)

Create a service user and directories:

```sh
sudo useradd --system --create-home --home-dir /var/lib/symphony --shell /bin/bash symphony
sudo install -d -o symphony -g symphony /var/lib/symphony/workspaces /etc/symphony
sudo install -m 0640 -o root -g symphony WORKFLOW.md /etc/symphony/WORKFLOW.md
sudo install -m 0640 -o root -g symphony /dev/null /etc/symphony/symphony.env   # secrets go here
sudo -u symphony -H codex login --device-auth                                 # Codex auth in ~symphony
```

`/etc/symphony/symphony.env`:

```sh
LINEAR_API_KEY=...
SYMPHONY_PORT=4000
SYMPHONY_DB_RETENTION_DAYS=30
RUST_LOG=info
```

`/etc/systemd/system/symphony.service`:

```ini
[Unit]
Description=Symphony coding-agent orchestrator
Documentation=https://github.com/f8fvwgzc/texc-symphony
Wants=network-online.target
After=network-online.target

[Service]
Type=simple
User=symphony
Group=symphony
WorkingDirectory=/var/lib/symphony
EnvironmentFile=/etc/symphony/symphony.env
Environment=SYMPHONY_HOST=127.0.0.1
Environment=SYMPHONY_DB_PATH=/var/lib/symphony/data/symphony.db
Environment=SYMPHONY_LOGS_ROOT=/var/log/symphony
ExecStart=/usr/local/bin/symphony --i-understand-that-this-will-be-running-without-the-usual-guardrails /etc/symphony/WORKFLOW.md
Restart=on-failure
RestartSec=5s
# SIGTERM stops agents (their process groups) and flushes the store.
KillMode=mixed
TimeoutStopSec=30s
LogsDirectory=symphony
StateDirectory=symphony
# Hardening that still lets agents build software in their workspaces.
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=full
ProtectControlGroups=true
ProtectKernelModules=true
ProtectKernelTunables=true
RestrictSUIDSGID=true
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now symphony
systemctl status symphony
journalctl -u symphony -f              # process output; Symphony's own logs are in /var/log/symphony
curl -s http://127.0.0.1:4000/api/v1/health
```

Point `workspace.root` in the workflow at `/var/lib/symphony/workspaces`. Editing
`/etc/symphony/WORKFLOW.md` takes effect within a second (the file is polled); changes to
`symphony.env` need `systemctl restart symphony`.

## Reverse proxy and SSE

Expose Symphony through a TLS-terminating, authenticating reverse proxy, and keep Symphony itself
on `127.0.0.1`. The dashboard's live updates are **Server-Sent Events** on `GET /api/v1/events`:
one long-lived response that receives a `snapshot` event on every state change and a `heartbeat`
every 15 s. The proxy must forward it unbuffered and must not time it out before the heartbeat:

- **nginx:** in a `location = /api/v1/events` block set `proxy_buffering off;`,
  `proxy_cache off;`, `proxy_http_version 1.1;`, `proxy_set_header Connection "";`,
  `gzip off;` and `proxy_read_timeout 1h;`. A full example is in
  [`docker/README.md`](../docker/README.md#behind-a-reverse-proxy).
- **Caddy:** works as is (`reverse_proxy 127.0.0.1:4000`); it flushes `text/event-stream`
  immediately.
- **Traefik, HAProxy, cloud load balancers:** raise the idle/read timeout above 15 s (60 s or more).
- **CDNs (Cloudflare, ...):** bypass caching and buffering for `/api/v1/events`.

If the stream is blocked anyway, the dashboard falls back to polling `GET /api/v1/state` every
5 s and shows `Polling` in its connection badge; everything still works, just less promptly. Test
the stream through the proxy with `curl -N https://symphony.example.com/api/v1/events`: events
must appear immediately, not in bursts.

## Data and backups

Symphony keeps run history in one SQLite database (`SYMPHONY_DB_PATH`, default
`./data/symphony.db`; `/data/symphony.db` in Docker). It runs in WAL mode, so the live files are
`symphony.db`, `symphony.db-wal` and `symphony.db-shm`. Live scheduling state (running, retrying,
blocked) is in memory and is rebuilt from the tracker after a restart; only history is stored.

- **Online backup** (safe while Symphony runs; needs the `sqlite3` CLI):

  ```sh
  sqlite3 /var/lib/symphony/data/symphony.db ".backup '/backups/symphony-$(date +%F).db'"
  ```

  A daily cron/systemd timer with this command plus rotation is enough for most setups.
- **Cold backup:** stop Symphony, then copy the `.db` file together with any `-wal`/`-shm` files.
  In Docker:

  ```sh
  docker compose stop symphony
  docker run --rm -v symphony_symphony-data:/data:ro -v "$PWD":/backup debian:bookworm-slim \
    tar czf /backup/symphony-data-$(date +%F).tgz -C /data .
  docker compose start symphony
  ```

- **Restore:** stop Symphony, replace the database file (delete stale `-wal`/`-shm` files next to
  it), start Symphony.
- **Size:** finished runs older than `SYMPHONY_DB_RETENTION_DAYS` are pruned at startup and every
  24 h (the newest 100 runs are always kept). History is optional: `SYMPHONY_DB_PATH=off` or
  `--no-db` disables it, and the history endpoints then answer `503 store_disabled`.

Workspaces (`workspace.root`) are disposable: Symphony recreates them from the tracker and your
hooks. Logs rotate at 10 MiB × 5 files under `SYMPHONY_LOGS_ROOT`.

## Upgrading

1. Read the release notes. Back up the database (above).
2. Replace the binary (or `docker compose pull && docker compose up -d`; pin `SYMPHONY_IMAGE_TAG`
   to a version in production instead of `latest`).
3. Restart. Database migrations are applied automatically and exactly once at startup. Runs that
   were in flight are closed as `cancelled` ("interrupted by restart"), and still-active issues
   are picked up again from the tracker.

Downgrades: an older binary refuses to open a database written by a newer schema
(`store_schema_too_new`). Restore the pre-upgrade backup, or start the old version with
`--no-db`/a fresh `SYMPHONY_DB_PATH`.

Release channels: `vX.Y.Z` tags (binaries and the `X.Y.Z`, `X.Y`, `latest` image tags), the
optional rolling `nightly` prerelease and image tag, and `edge` images built from every push to
`main`.
