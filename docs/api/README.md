# Symphony HTTP API

The contract lives in [`openapi.yaml`](./openapi.yaml) (OpenAPI 3.1). A running server also serves
it as JSON at `GET /api/openapi.json`. This page is the human guide.

- **Base URL:** `http://<server.host>:<port>`; the default host is `127.0.0.1`. The server starts
  only when `server.port` is set in `WORKFLOW.md`, `--port` is passed or `SYMPHONY_PORT` is
  set. `0` binds an ephemeral port; the binary prints the real URL on stdout
  (`Symphony listening on http://127.0.0.1:<port>/`).
- **Auth:** none. Bind to loopback, or put an authenticating proxy in front.
- **Format:** JSON (`application/json`). The one exception is the SSE stream, which is
  `text/event-stream`.
- **Compatibility:** `state`, `refresh`, `{issue_identifier}` and the 404/405 envelopes match the
  Elixir implementation exactly. `health`, `events`, `runs*`, `totals` and `openapi.json` are new.

```sh
export SYMPHONY=http://127.0.0.1:4000
```

## Endpoints at a glance

| Method | Path | Success | Errors |
|---|---|---|---|
| GET | `/api/v1/state` | 200 snapshot, or 200 with in-band `error` | 405 |
| POST | `/api/v1/refresh` | 202 | 405, 503 `orchestrator_unavailable` |
| GET | `/api/v1/{issue_identifier}` | 200 | 404 `issue_not_found`, 405 |
| GET | `/api/v1/health` | 200 | 405 |
| GET | `/api/v1/events` | 200 `text/event-stream` | 405 |
| GET | `/api/v1/runs` | 200 | 400, 405, 503 `store_disabled` |
| GET | `/api/v1/runs/{id}` | 200 | 400, 404 `run_not_found`, 405, 503 |
| GET | `/api/v1/runs/{id}/events` | 200 | 400, 404, 405, 503 |
| GET | `/api/v1/totals` | 200 | 405, 503 |
| GET | `/api/openapi.json` | 200 | 405 |
| any | anything else | — | 404 `not_found` |

Because literal routes match first, the identifiers `state`, `refresh`, `health`, `events`, `runs`
and `totals` are reserved and cannot be looked up as issues. A single trailing slash is ignored.
`HEAD` works on every `GET` route.

## Live state

### `GET /api/v1/state`

```sh
curl -s $SYMPHONY/api/v1/state | jq
```

```json
{
  "generated_at": "2026-02-24T20:15:30Z",
  "counts": { "running": 1, "retrying": 1, "blocked": 1 },
  "running": [{ "issue_id": "issue-http", "issue_identifier": "MT-HTTP", "state": "In Progress",
                "session_id": "thread-http", "turn_count": 7, "last_event": "notification",
                "last_message": "rendered", "started_at": "2026-02-24T20:10:12Z",
                "tokens": { "input_tokens": 4, "output_tokens": 8, "total_tokens": 12 }, "...": "..." }],
  "retrying": [{ "issue_identifier": "MT-RETRY", "attempt": 2, "due_at": "2026-02-24T20:15:32Z", "error": "boom", "...": "..." }],
  "blocked":  [{ "issue_identifier": "MT-BLOCKED", "error": "codex turn requires operator input", "...": "..." }],
  "codex_totals": { "input_tokens": 4, "output_tokens": 8, "total_tokens": 12, "seconds_running": 42 },
  "rate_limits": { "primary": { "remaining": 11 } }
}
```

This endpoint **always returns 200**. When the orchestrator does not answer within 15 s, or is not
running, the body is instead:

```json
{ "generated_at": "2026-02-24T20:15:30Z", "error": { "code": "snapshot_timeout", "message": "Snapshot timed out" } }
```

(`snapshot_unavailable` / `Snapshot unavailable` when it is not running.) Notes:

- `codex_totals.seconds_running` counts **ended** sessions only. For a live total, add
  `now - started_at` for each running row; the dashboard does this.
- `rate_limits` is the last object Codex reported, passed through verbatim (or `null`).
- Each list is sorted by `issue_id`. `due_at` is recomputed on every request, so it can jitter by 1 s.

### `GET /api/v1/{issue_identifier}`

```sh
curl -s $SYMPHONY/api/v1/MT-HTTP | jq
curl -s $SYMPHONY/api/v1/$(jq -rn --arg id 'MT/42' '$id|@uri')   # URL-encode odd identifiers
```

The response holds `status` (`running` | `retrying` | `blocked`), `workspace {path, host}`,
`attempts {restart_count, current_retry_attempt}`, and one object each for `running`, `retry` and
`blocked` (each may be `null`). It also holds `recent_events` (0 or 1 entries), `last_error`,
`logs.codex_session_logs` (always `[]`) and `tracked` (always `{}`). An identifier that is not
running, retrying or blocked, or any snapshot failure, gives:

```sh
$ curl -si $SYMPHONY/api/v1/NOPE-1
HTTP/1.1 404 Not Found
{"error":{"code":"issue_not_found","message":"Issue not found"}}
```

### `POST /api/v1/refresh`

```sh
curl -si -X POST $SYMPHONY/api/v1/refresh
```

```json
HTTP/1.1 202 Accepted
{ "queued": true, "coalesced": false, "requested_at": "2026-02-24T20:15:30.123456Z", "operations": ["poll", "reconcile"] }
```

This queues an immediate poll and reconcile. `coalesced: true` means a poll was already running or
due, so nothing extra was scheduled. The request body is ignored. `GET` on this path returns 405.
When the orchestrator is not running, or does not answer within 5 s, the response is
`503 {"error":{"code":"orchestrator_unavailable","message":"Orchestrator is unavailable"}}`.

## Live updates (Server-Sent Events)

### `GET /api/v1/events`

How the stream behaves:

1. It opens with `retry: 3000`, then immediately sends one `snapshot` event. Its `data` is the same
   JSON as `GET /api/v1/state`, including the in-band error form.
2. A new `snapshot` follows whenever orchestrator state changes. The server debounces changes
   (~200 ms, leading and trailing edge), so bursts of agent events yield at most about 5
   snapshots per second.
3. `id:` is the orchestrator **generation**, a monotonically increasing integer. Only `snapshot`
   events carry an id.
4. `event: heartbeat` is sent every 15 s with `data: {"at": "...", "generation": N}` and no id.
5. A client that reconnects with `Last-Event-ID` just gets the current snapshot first. Nothing is
   replayed, because every snapshot is complete.

```text
retry: 3000

event: snapshot
id: 42
data: {"generated_at":"2026-02-24T20:15:30Z","counts":{"running":1,"retrying":0,"blocked":0},...}

event: heartbeat
data: {"at":"2026-02-24T20:15:45Z","generation":42}
```

**curl** (`-N` disables buffering):

```sh
curl -N -H 'Accept: text/event-stream' $SYMPHONY/api/v1/events
# Only the counts, one line per snapshot:
curl -sN $SYMPHONY/api/v1/events | sed -n 's/^data: //p' | jq -c '.counts? // empty'
```

**Browser / JavaScript:**

```js
const source = new EventSource('/api/v1/events');

source.addEventListener('snapshot', (event) => {
  const state = JSON.parse(event.data);
  if (state.error) {
    console.warn('snapshot failed', state.error.code);
    return;
  }
  console.log(`generation ${event.lastEventId}:`, state.counts);
});

source.addEventListener('heartbeat', () => {
  /* the stream is alive */
});

source.onerror = () => {
  // The browser reconnects by itself (after `retry` ms) unless readyState is CLOSED.
  if (source.readyState === EventSource.CLOSED) {
    /* fall back to polling GET /api/v1/state */
  }
};
```

The bundled dashboard (`web/`) closes the stream on errors and manages reconnection itself, with
exponential backoff from 1 s to 30 s. After 3 consecutive failures it polls `GET /api/v1/state`
every 5 s, and it re-probes SSE every 60 s. It also treats 45 s without a snapshot or heartbeat as
a dead stream.

## Run history (SQLite)

These endpoints need persistence (`symphony-store`). When it is off they answer
`503 {"error":{"code":"store_disabled","message":"Run history store is disabled"}}`, and
`GET /api/v1/health` reports `"store": "disabled"`.

### `GET /api/v1/runs`

Query parameters, all optional:

| Parameter | Meaning |
|---|---|
| `limit` | 1–200, default 50 |
| `before_id` | keyset cursor |
| `issue` | exact `issue_identifier` |
| `status` | `running`, `succeeded`, `failed`, `cancelled` or `blocked` |

Results are ordered newest first (`id` descending).

```sh
curl -s "$SYMPHONY/api/v1/runs?limit=20" | jq
curl -s "$SYMPHONY/api/v1/runs?issue=MT-HTTP&status=failed" | jq '.runs[] | {id, attempt, error}'
```

```json
{
  "runs": [{
    "id": 42, "issue_id": "issue-http", "issue_identifier": "MT-HTTP",
    "issue_title": "Render the HTTP dashboard", "attempt": 0, "worker_host": null,
    "workspace_path": "/tmp/symphony_workspaces/MT-HTTP", "status": "succeeded", "error": null,
    "turns": 7, "started_at": "2026-02-24T20:10:12.004Z", "finished_at": "2026-02-24T20:31:40.250Z",
    "duration_ms": 1288246, "tokens": { "input": 18230, "output": 2207, "total": 20437 }
  }],
  "next_before_id": 42
}
```

To page, pass `next_before_id` as `before_id`. It is `null` on the last page.

```sh
cursor=""
while :; do
  page=$(curl -s "$SYMPHONY/api/v1/runs?limit=200${cursor:+&before_id=$cursor}")
  echo "$page" | jq -c '.runs[]'
  cursor=$(echo "$page" | jq -r '.next_before_id // empty')
  [ -z "$cursor" ] && break
done
```

### `GET /api/v1/runs/{id}`

```sh
curl -s $SYMPHONY/api/v1/runs/42 | jq
```

This returns one `RunRecord` (the same shape as a list item). Unknown or pruned ids return 404
`run_not_found`. A non-integer id returns 400 `invalid_parameter`. While a run is in progress,
`finished_at` and `duration_ms` are `null`.

### `GET /api/v1/runs/{id}/events`

Parameters: `after_seq` (default 0) and `limit` (1–1000, default 500). Events come oldest first.

```sh
curl -s "$SYMPHONY/api/v1/runs/42/events?limit=100" | jq '.events[] | [.seq, .at, .kind, .message] | @tsv' -r
curl -s "$SYMPHONY/api/v1/runs/42/events?after_seq=100" | jq
```

```json
{ "events": [
  { "run_id": 42, "seq": 1, "at": "2026-02-24T20:10:12.004Z", "kind": "session_started",
    "message": "session started (thread-http)", "payload": { "session_id": "thread-http" } }
] }
```

`seq` starts at 1 and has no gaps. To read further, pass the last `seq` you received as
`after_seq`. A page shorter than `limit` is the end for now; a running run keeps appending events.
`kind` is an open set, so render unknown kinds verbatim. `payload` is raw JSON with secrets
scrubbed, or `null`.

### `GET /api/v1/totals`

```sh
curl -s $SYMPHONY/api/v1/totals | jq
```

```json
{ "runs_total": 128, "runs_succeeded": 97, "runs_failed": 21,
  "tokens": { "input": 1830211, "output": 220407, "total": 2050618 }, "runtime_ms": 48210933 }
```

## Meta

```sh
curl -s $SYMPHONY/api/v1/health
# {"status":"ok","version":"0.1.0","uptime_seconds":3725,"store":"sqlite"}

curl -s $SYMPHONY/api/openapi.json | jq '.paths | keys'
```

`health` never touches the orchestrator, which makes it a good liveness probe.

## Errors

Every non-2xx response has the same envelope:

```json
{ "error": { "code": "issue_not_found", "message": "Issue not found" } }
```

| Status | `code` | `message` | When |
|---|---|---|---|
| 404 | `not_found` | `Route not found` | unknown path (`/unknown`, `/api/v1/a/b`, `POST /favicon.png`) |
| 405 | `method_not_allowed` | `Method not allowed` | known path, wrong method (`GET /api/v1/refresh`, `DELETE /api/v1/state`) |
| 404 | `issue_not_found` | `Issue not found` | issue not running, retrying or blocked, or the snapshot failed |
| 503 | `orchestrator_unavailable` | `Orchestrator is unavailable` | refresh while the orchestrator is down or unresponsive |
| 404 | `run_not_found` | `Run not found` | unknown run id |
| 400 | `invalid_parameter` | names the parameter, e.g. `limit must be an integer between 1 and 200` | bad path or query value |
| 503 | `store_disabled` | `Run history store is disabled` | history endpoints with persistence off |
| 4xx/5xx | `request_failed` | status text, e.g. `Internal Server Error` | unexpected failure |

In-band snapshot errors are **not** HTTP errors. They appear inside a `200` body from
`GET /api/v1/state` and inside SSE `snapshot` events:

| `code` | `message` |
|---|---|
| `snapshot_timeout` | `Snapshot timed out` |
| `snapshot_unavailable` | `Snapshot unavailable` |

Codes are stable, but messages are for humans and may change.
