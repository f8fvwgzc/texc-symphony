# Deploying Symphony

Symphony is one self-contained binary (`symphony`) with the web dashboard embedded. It needs a
`WORKFLOW.md`, credentials for one tracker, and the tools its agents use. This page covers running
it as a plain binary, in Docker, and under systemd, plus reverse proxies, backups and upgrades.

> [!WARNING]
> Symphony runs coding agents unattended. Run it in a trusted environment and give agents only the
> credentials they need. The HTTP API needs a bearer token (`server.api_token` or
> `SYMPHONY_API_TOKEN`) on any non-loopback bind; Symphony refuses to start without one. Do not put
> an HTTP basic-auth proxy in front: it uses the same `Authorization` header as the token.

## Command line and environment

```text
symphony --i-understand-that-this-will-be-running-without-the-usual-guardrails \
         [--logs-root <path>] [--port <port>] [--host <addr>] \
         [--db-path <path> | --no-db] [path-to-WORKFLOW.md]
symphony workspace before-remove [--branch B] [--repo R]
symphony --version
```

Without the acknowledgement flag Symphony prints the guardrails banner and exits 1. The workflow
path defaults to `./WORKFLOW.md`. Flags win over environment variables, which win over
`WORKFLOW.md`; the full reference is [`cli.md`](cli.md).

| Variable | Default | Meaning |
|---|---|---|
| `SYMPHONY_WORKFLOW` | `./WORKFLOW.md` | workflow file (the positional argument wins) |
| `SYMPHONY_HOST` | `127.0.0.1` | HTTP bind address (`0.0.0.0` in containers) |
| `SYMPHONY_PORT` | unset (no HTTP server) | HTTP port; same as `--port` or `server.port` in `WORKFLOW.md`; `0` picks a free port |
| `SYMPHONY_LOGS_ROOT` | current directory | log root; the rotating files are `<root>/log/symphony.log*` |
| `SYMPHONY_DB_PATH` | `./data/symphony.db` | SQLite run history; `off` disables it (same as `--no-db`) |
| `SYMPHONY_DB_RETENTION_DAYS` | `30` | days of finished runs to keep; `0` keeps everything |
| `SYMPHONY_DB_PAYLOAD_RETENTION_DAYS` | `7` | days to keep raw event payloads (agent messages, command output); `0` keeps them as long as the run |
| `SYMPHONY_API_TOKEN` | unset | API bearer token when `server.api_token` is not set; required for a non-loopback `SYMPHONY_HOST` |
| `LLM_*` | unset | the platform LLM connection contract: provider, endpoint, key file, model, gateway ([configuration.md](configuration.md#llm-connection-contract)) |
| `SYMPHONY_LOG_FORMAT` | `text` | `json` for one JSON object per line on stdout (the log file stays text) |
| `RUST_LOG` | `info` | `tracing` filter, e.g. `info` or `symphony=debug,info` |
| `SYMPHONY_SSH_CONFIG` | | SSH client config for `worker.ssh_hosts` |
| `LINEAR_API_KEY`, `GITHUB_TOKEN`, `GITLAB_PAT`, `JIRA_BASE_URL` / `JIRA_EMAIL` / `JIRA_API_TOKEN`, `ASANA_PAT` | | tracker credentials (only the one your workflow uses); kept out of the Codex child environment |

The HTTP server serves the dashboard at `/`, the API under `/api/v1` (see
[`api/README.md`](api/README.md)) and a liveness probe at `GET /api/v1/health`, which never touches
the orchestrator and needs no token. The `Host` allow-list still applies to it: a probe from
outside the container must use a loopback name or one listed in `server.allowed_hosts` (a
`0.0.0.0` bind host is not added to the list).

**Exposure.** Symphony refuses to bind a non-loopback address unless an API token is configured
(`server.api_token` or `SYMPHONY_API_TOKEN`). With a token, every other `/api/*` call needs
`Authorization: Bearer <token>`; the dashboard asks for it. Requests whose `Host` is not loopback,
the bind address or a `server.allowed_hosts` entry are refused, and browser writes from other
origins are refused. Use `budget:` in `WORKFLOW.md` and the kill switch (`POST /api/v1/pause`) to
cap spending.

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
version=$(curl -fsSLI -o /dev/null -w '%{url_effective}' https://github.com/f8fvwgzc/texc-symphony/releases/latest | sed 's|.*/||')   # or a tag, e.g. v0.1.0
target=linux_x86_64                          # or linux_arm64, macos_arm64, macos_x86_64
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
CLI, runs as uid 10001 under its own init (`symphony init`), keeps history and logs on `/data`
and workspaces on `/workspaces`. See [`docker/README.md`](../docker/README.md) for Compose,
Apple Silicon/Intel, Linux servers, Codex login and multi-arch builds. In short:

```sh
cd docker && cp .env.example .env && mkdir -p config && cp WORKFLOW.example.md config/WORKFLOW.md
docker compose up -d
```

## Option 3: Kubernetes (Helm)

The chart in [`deploy/helm/symphony`](../deploy/helm/symphony) runs one replica with the image,
`WORKFLOW.md` from values (a ConfigMap at `/config`), persistent volumes for `/data` and
`/workspaces`, the API token from a Secret, and the platform LLM connection contract
([configuration.md](configuration.md#llm-connection-contract)):

```sh
kubectl create secret generic symphony-api-token --from-literal=token="$(openssl rand -hex 32)"
kubectl create configmap llm-config --from-literal=LLM_GATEWAY_URL=http://attache-gateway:8080 \
  --from-literal=LLM_MODEL=claude-sonnet-4-5          # usually shared by every core
kubectl create secret generic llm-credentials --from-file=gateway-token=./gateway-token
helm install symphony deploy/helm/symphony --set acknowledgeGuardrails=true \
  --set global.llm.gatewayUrl=http://attache-gateway:8080
```

- `global.llm.*` (`provider`, `baseUrl`, `model`, `modelSmall`, `timeoutSecs`, `maxRetries`,
  `gatewayUrl`) set `LLM_*` explicitly and win over the `llm-config` ConfigMap. Set
  `global.llm.gatewayUrl` in gateway mode so only the gateway token is mounted.
- Credentials are files under `/var/run/secrets/llm` (Secret `llm-credentials`, keys `api-key`
  and `gateway-token`, both optional). Set `global.llm.apiKeySecretKey=""` for keyless servers.
- The pod runs as uid 10001 with no capabilities. `make helm-check` lints and renders the chart.

## Option 4: systemd (Linux server, binary install)

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
journalctl -u symphony -f              # log stream (stdout); files in /var/log/symphony/log/
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
`symphony.db`, `symphony.db-wal` and `symphony.db-shm`. The same database holds the retry queue,
so issues that were waiting for a retry keep their attempt count and due time across a restart.
The rest of the live scheduling state (running, blocked) is in memory and is rebuilt from the
tracker after a restart. With `--no-db` nothing is kept.

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
hooks. Logs rotate at 10 MiB × 5 files under `<SYMPHONY_LOGS_ROOT>/log/`.

## Upgrading

1. Read the release notes. Back up the database (above).
2. Replace the binary (or `docker compose pull && docker compose up -d`; pin `SYMPHONY_IMAGE_TAG`
   to a version in production instead of `latest`).
3. Restart. Database migrations are applied automatically and exactly once at startup. Runs that
   were in flight are closed as `cancelled` ("interrupted by restart"), and still-active issues
   are picked up again from the tracker.

### Changes that need action (security hardening and agent backends)

| Change | What to do |
|---|---|
| A non-loopback `server.host`/`SYMPHONY_HOST` (the Docker image: `0.0.0.0`) refuses to start without an API token. | Set `SYMPHONY_API_TOKEN` (or `server.api_token`); clients send `Authorization: Bearer`, the dashboard asks once. |
| Requests with another `Host` get `403 host_not_allowed`; cross-origin browser writes are refused. | List proxy names in `server.allowed_hosts`, other origins in `server.allowed_origins`. Remove HTTP basic auth in front (it uses the same header). |
| The image no longer passes `--i-understand-that-this-will-be-running-without-the-usual-guardrails`. | Review the sandbox settings, then pass the flag (Compose override, `docker run` args, Helm `acknowledgeGuardrails: true`). |
| Shipped `WORKFLOW.md` examples use Codex's safe defaults (no network in the turn sandbox, escalations rejected). | Re-enable `turn_sandbox_policy.networkAccess` deliberately if agents need to push or install. |
| Tracker tools are scoped to the configured repository/project, `DELETE` and admin paths are off. | `tracker.agent_tools.methods` / `allow_admin_paths` to widen them. GitHub and GitLab need `required_labels` or `provider.allowed_authors`. |
| Raw event payloads expire after 7 days; recorded events no longer keep the `raw` message copy. | `SYMPHONY_DB_PAYLOAD_RETENTION_DAYS=0` keeps payloads with their runs. |
| `api` backends: `sandbox: none` and `sandbox: seatbelt` need `unconfined: true`, and `none` refuses an explicit `network: false`; `auto` is bubblewrap only (seatbelt must be chosen explicitly, for development). | Install bubblewrap with unprivileged user namespaces, or set `unconfined: true` knowingly. |
| `claude_code` backends always run with `--setting-sources user --strict-mcp-config`; `extra_args` may not change them. | Move project settings the operator wants into the Symphony user's own Claude settings. |
| The `LLM_*` contract configures Codex's provider (OpenAI-compatible or gateway) unless `codex.provider` is set; it never changes `agent.backend`. | Set `codex.provider` to keep a different provider; set `agent.backend: llm` to use the built-in loop. The startup log names what the contract configures. |
| The image's entrypoint is `symphony init -- symphony` (was `tini -- symphony`): tini's `/proc/1/environ` exposed every secret to agents. | If you override the entrypoint (Kubernetes `command:`, Compose `entrypoint:`), start it with `symphony init -- symphony`. Do not enable Docker's `--init` / Compose `init: true`. |
| Task identifiers are `task-<id>` (was `TASK-<id>`); task workspaces live in `<workspace.root>/.tasks/<id>` and are removed when the task ends. | Update clients that matched on the identifier. |

Downgrades: an older binary refuses to open a database written by a newer schema
(`store_schema_too_new`). Restore the pre-upgrade backup, or start the old version with
`--no-db`/a fresh `SYMPHONY_DB_PATH`.

Release channels: `vX.Y.Z` tags (binaries and the `X.Y.Z`, `X.Y`, `latest` image tags), the
optional rolling `nightly` prerelease and image tag, and `edge` images built from every push to
`main`.
