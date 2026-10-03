# Command line reference

```text
symphony [--i-understand-that-this-will-be-running-without-the-usual-guardrails]
         [--logs-root <path>] [--port <port>] [--host <addr>]
         [--db-path <path> | --no-db] [path-to-WORKFLOW.md]
symphony workspace before-remove [--branch <name>] [--repo <owner/name>]
symphony --version | -V
symphony --help | -h
```

`symphony` runs the orchestrator in the foreground until it receives SIGINT or SIGTERM. Its
behaviour is configured by [`WORKFLOW.md`](configuration.md); this page covers the command line,
the environment, logging, the terminal dashboard and process lifecycle. For running it as a
service or in Docker see [`deployment.md`](deployment.md); for the HTTP API see
[`api/README.md`](api/README.md).

## Options

| Option | Environment fallback | Default | Meaning |
|---|---|---|---|
| `--i-understand-that-this-will-be-running-without-the-usual-guardrails` | none | | Required acknowledgement that agents run unattended. Without it Symphony prints a warning banner and exits 1. |
| `[path-to-WORKFLOW.md]` | `SYMPHONY_WORKFLOW` | `./WORKFLOW.md` | The workflow file. Relative paths are resolved against the current directory. It must be a regular file. |
| `--logs-root <path>` | `SYMPHONY_LOGS_ROOT` | current directory | Root for the log files, which go to `<path>/log/`. Surrounding whitespace is trimmed; an empty value is a usage error. |
| `--port <port>` | `SYMPHONY_PORT` | `server.port` from `WORKFLOW.md`; unset means no HTTP server | Starts the HTTP server (dashboard, API, event stream) on this port. `0..65535`; `0` picks a free port. |
| `--host <addr>` | `SYMPHONY_HOST` | `server.host`, else `127.0.0.1` | Bind address for the HTTP server: an IP literal or a DNS name. |
| `--db-path <path>` | `SYMPHONY_DB_PATH` | `./data/symphony.db` | SQLite run-history database. `:memory:` keeps history in memory only. |
| `--no-db` | `SYMPHONY_DB_PATH=off` | | Disables run history; the history endpoints then answer `503 store_disabled`. Conflicts with `--db-path`. |
| `--version`, `-V` | | | Prints `symphony <version>` (for example `symphony 0.1.0`, or `symphony 0.1.0-nightly` for nightly builds) and exits 0. |
| `--help`, `-h` | | | Prints help and exits 0. |

Precedence: **a flag always wins over its environment variable, and the environment variable wins
over `WORKFLOW.md`.** For the workflow path, the positional argument wins over
`SYMPHONY_WORKFLOW`. When `--logs-root` or `--port` is given more than once, the last value wins.

## Environment variables

| Variable | Default | Meaning |
|---|---|---|
| `SYMPHONY_WORKFLOW` | `./WORKFLOW.md` | Workflow path when no positional argument is given. |
| `SYMPHONY_HOST` | `server.host`, else `127.0.0.1` | HTTP bind address. |
| `SYMPHONY_PORT` | `server.port`, else unset | HTTP port; setting it enables the HTTP server. |
| `SYMPHONY_LOGS_ROOT` | current directory | Log root; files go to `$SYMPHONY_LOGS_ROOT/log/symphony.log*`. |
| `SYMPHONY_DB_PATH` | `./data/symphony.db` | Run-history database path; `:memory:` for in-memory. `off`, `none`, `disabled`, `false`, `0` or an empty value disable history (same as `--no-db`). |
| `SYMPHONY_DB_RETENTION_DAYS` | `30` | Finished runs older than this are pruned at startup and every 24 h; the newest 100 runs are always kept. `0` keeps everything. |
| `SYMPHONY_LOG_FORMAT` | `text` | Format of the stdout log stream: `text` or `json`. The log file is always text. |
| `RUST_LOG` | `info` | `tracing` filter, for example `debug` or `symphony_runtime=debug,info`. |
| `SYMPHONY_SSH_CONFIG` | | SSH client config file passed as `ssh -F` for `worker.ssh_hosts`. |
| `COLUMNS` | | Terminal width for the dashboard when it cannot be detected. |
| Tracker credentials | | `LINEAR_API_KEY`, `LINEAR_ASSIGNEE`, `GITHUB_TOKEN`, `GITHUB_REPO`, `GITLAB_PAT`, `GITLAB_PROJECT_PATH`, `JIRA_BASE_URL`, `JIRA_EMAIL`, `JIRA_API_TOKEN`, `ASANA_PAT`: see [configuration.md](configuration.md#tracker-kinds). |

An invalid value (for example `SYMPHONY_PORT=abc`) stops startup with a message that names the
variable, and exit code 1.

## Startup checks

Symphony checks its input in this order and stops at the first problem:

1. **Arguments.** An unknown option, a missing option value, a malformed port or more than one
   workflow path prints, on stderr,

   ```text
   Usage: symphony [--logs-root <path>] [--port <port>] [path-to-WORKFLOW.md]
   Run `symphony --help` for all options.
   ```

   and exits 1.
2. **Acknowledgement.** Without
   `--i-understand-that-this-will-be-running-without-the-usual-guardrails`, a red boxed banner
   whose first line is `This Symphony implementation is a low key engineering preview.` is printed
   on stderr, and Symphony exits 1.
3. **`--logs-root`.** An empty (or whitespace-only) value is a usage error.
4. **`--port`.** Must be in `0..65535`.
5. **Workflow file.** The path must be a regular file, else
   `Workflow file not found: <absolute path>` and exit 1.
6. **Workflow contents.** The file is loaded and validated (see
   [validation errors](configuration.md#validation-errors)); an invalid file prints
   `Failed to start Symphony with workflow <path>: <reason>` and exits 1.
7. **Boot**, as described below. Failing to bind the HTTP address (bad host, port in use) or to
   open the database is a startup failure (exit 1).

## Startup and shutdown

Startup order:

1. the workflow file is loaded and validated (errors go to stderr, exit 1);
2. logging (rotating file, and stdout when applicable; see [Logging](#logging)), then the
   workflow reload poll (every second);
3. the run-history store: open (applying migrations), mark runs left `running` by a previous
   crash as `cancelled`, prune by retention;
4. the orchestrator, which removes the workspaces of issues that are already terminal, then polls
   the tracker;
5. the HTTP server, if a port is configured. Once bound it prints
   `Symphony listening on http://<host>:<port>/` to stdout (a wildcard host such as `0.0.0.0` is
   shown as `127.0.0.1`; IPv6 hosts are bracketed). With `--port 0` this line shows the real port;
6. the terminal dashboard, if enabled.

Shutdown, on SIGINT (Ctrl-C) or SIGTERM:

1. dispatching stops;
2. every agent run is cancelled. Each gets 10 s to finish its `after_run` hook, then its process
   group (Codex, hooks, SSH sessions) is killed;
3. the HTTP server stops accepting connections and drains open requests for up to 10 s;
4. the run-history store is flushed;
5. if the terminal dashboard was running, it prints a final frame with `app_status=offline`;
6. Symphony exits 0.

A second SIGINT or SIGTERM during shutdown exits immediately with code 1. If the orchestrator
crashes it is restarted (running agents are stopped and work is picked up again from the
tracker); more than 3 crashes within 5 s end the process with exit 1.

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Graceful stop after SIGINT/SIGTERM; `--help`; `--version`. |
| `1` | Usage error, missing acknowledgement, missing or invalid workflow file, invalid environment value, startup failure (bad host, port in use, database cannot be opened), fatal runtime error (orchestrator crash loop), or forced shutdown (second signal). |

## Logging

Symphony always logs to a size-rotating file:

- **Path:** `<logs root>/log/symphony.log`. The logs root is `--logs-root`, else
  `SYMPHONY_LOGS_ROOT`, else the current directory, so the default is `./log/symphony.log`. The
  Docker image sets `SYMPHONY_LOGS_ROOT=/data/logs`, so its files are
  `/data/logs/log/symphony.log*`.
- **Rotation:** at 10 MiB; 5 rotated files are kept, `symphony.log.1` to `symphony.log.5`.
- **Format:** one line per event, plain text.

Symphony **also** logs to stdout when stdout is not a terminal, or when the terminal dashboard is
disabled, so `docker logs`, `docker compose logs` and `journalctl` show the log stream.
`SYMPHONY_LOG_FORMAT=json` switches that stream to one JSON object per line. While the terminal
dashboard is active, stdout belongs to the dashboard and logs go only to the file. Verbosity is
controlled by `RUST_LOG` (default `info`).

Conventions that make the logs searchable (the `debug` skill in `.codex/skills/` relies on them):

- Lines about an issue carry `issue_id=<tracker id>` and `issue_identifier=<key>` (for example
  `issue_identifier=MT-620`).
- Lines about a Codex session carry `session_id=<thread_id>-<turn_id>`.
- Context is written as `key=value` pairs in the message text, with stable wording for lifecycle
  events (`Codex session started`, `Codex session completed`, `Codex session ended with error`,
  `Issue stalled ... restarting with backoff`, `Agent task exited ... reason=...`) and the reason
  for failures.
- Credentials are never logged.

```sh
rg -n "issue_identifier=MT-625" log/symphony.log*
rg -o "session_id=[^ ;]+" log/symphony.log* | sort -u
```

## Terminal status dashboard

When `observability.dashboard_enabled` is true (the default) **and** stdout is a terminal,
Symphony draws a live status screen instead of printing logs. It shows:

- running agents out of the maximum, throughput (tokens per second over a rolling 5 s window),
  total runtime and tokens, and the latest Codex rate limits;
- the Linear project link (for Linear workflows), the dashboard URL when the HTTP server runs,
  and a countdown to the next tracker poll;
- a table of running agents: ID, STAGE, PID, AGE / TURN, TOKENS, SESSION, EVENT;
- the backoff queue of issues waiting to be retried.

It redraws at most every `observability.render_interval_ms` (16 ms by default), refreshes every
`observability.refresh_ms` (1 s) and immediately on every orchestrator state change; identical
frames are never rewritten. Both intervals are re-read on every tick, and setting
`dashboard_enabled: false` in a running instance stops the dashboard (turning it back on needs a
restart). On shutdown it prints a final `app_status=offline` frame. The width comes from the
terminal, else from `COLUMNS` (120 columns when `COLUMNS` is unset, 115 when it is not a positive
number).

Row colours follow the latest Codex event: magenta after a completed turn, green when a task
started, yellow on token-count updates, blue otherwise.

Set `observability.dashboard_enabled: false` (or redirect stdout) to get plain log output in a
terminal. The web dashboard is described in [`api/README.md`](api/README.md) and
[`architecture.md`](architecture.md#live-monitoring).

## `symphony workspace before-remove`

```text
symphony workspace before-remove [--branch <name>] [--repo <owner/name>]
```

Closes the open GitHub pull requests of a workspace's branch. It is meant for the
`hooks.before_remove` hook, which runs inside the workspace before Symphony deletes it:

```yaml
hooks:
  before_remove: |
    if command -v symphony >/dev/null 2>&1; then
      symphony workspace before-remove --repo your-org/your-repo
    fi
```

- The branch is `--branch`, else the output of `git branch --show-current`.
- The repository is `--repo`, default `openai/symphony`; set it for your repository.
- If there is no branch, or the `gh` CLI is missing or not authenticated, it does nothing.
- Otherwise it lists the open pull requests for the branch
  (`gh pr list --repo <repo> --head <branch> --state open --json number --jq .[].number`) and
  closes each one with a comment, printing `Closed PR #<n> for branch <branch>`, or
  `Failed to close PR #<n> for branch <branch>: exit <status>` (followed by ` output="..."` when
  `gh` printed something).
- It exits 0 even when closing a pull request fails. The only failure exit is for invalid options
  (`Invalid option(s): ...`, exit 1). `--help` exits 0.

## Examples

```sh
# Minimal: Linear workflow in the current directory, no HTTP server.
export LINEAR_API_KEY=...
symphony --i-understand-that-this-will-be-running-without-the-usual-guardrails

# Web dashboard and API on http://127.0.0.1:4000/
symphony --i-understand-that-this-will-be-running-without-the-usual-guardrails --port 4000 ./WORKFLOW.md

# Free port, logs under /var/log/symphony/log/, no run history
symphony --i-understand-that-this-will-be-running-without-the-usual-guardrails \
  --port 0 --logs-root /var/log/symphony --no-db /etc/symphony/WORKFLOW.md

# Configured through the environment (as under systemd or in a container)
SYMPHONY_WORKFLOW=/etc/symphony/WORKFLOW.md SYMPHONY_PORT=4000 SYMPHONY_LOG_FORMAT=json \
  symphony --i-understand-that-this-will-be-running-without-the-usual-guardrails

# Docker: dashboard on http://127.0.0.1:4000/ (see docker/README.md for Compose and volumes)
docker run --rm -p 127.0.0.1:4000:4000 -v "$PWD/WORKFLOW.md:/config/WORKFLOW.md:ro" \
  -e LINEAR_API_KEY ghcr.io/f8fvwgzc/texc-symphony:latest

symphony --version
```
