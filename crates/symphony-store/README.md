# symphony-store

SQLite persistence for Symphony agent runs. This is new in the Rust port: the Elixir version kept
everything in memory, so history was lost on restart.

- SQLite is bundled through `rusqlite` with the `bundled` feature. Builds are identical on
  linux/amd64, linux/arm64 and macOS, and need no system `libsqlite3`.
- File databases run in WAL mode with `synchronous=NORMAL`, `busy_timeout=5s` and `foreign_keys=ON`.
- Schema migrations are embedded SQL (`src/migrations/NNNN_*.sql`), tracked through
  `PRAGMA user_version`. Each migration and its version bump run in one `IMMEDIATE` transaction,
  and re-opening a database never applies anything twice. If the database has a newer version
  than this build supports, the store refuses to open it with `store_schema_too_new`.
- This crate does not depend on any other Symphony crate.

## Usage

```rust
let store = StoreConfig::from_env()?.open()?;    // or Store::open(path) / open_in_memory() / disabled()
store.mark_interrupted_runs().await?;              // at startup
let run = store.start_run(NewRun::new(issue_id, identifier)).await?;
store.append_event(run, "session_started", None, Some(payload)).detach(); // fire-and-forget
store.update_tokens(run, TokenUsage { input, output, total }).detach();
store.increment_turns(run).detach();
store.finish_run(run, RunStatus::Failed, Some(error)).await?;
```

`Store` is `Clone + Send + Sync`. Every clone shares one connection, which is owned by a dedicated
`symphony-store` thread. Each operation is queued to that thread in submission order and returns
`Pending<T>`, which you can use in two ways:

- Await it to get `Result<T, StoreError>`.
- Drop it, or call `.detach()`, to fire and forget. The operation still runs, and if it fails the
  error is logged with `tracing::warn!`.

Queuing never blocks the caller. Errors come back as values and never panic. If an operation
panics, the panic is caught on the database thread and returned as `store_internal_error`. If the
thread is gone, calls return `store_unavailable`.

`Store::disabled()` accepts every call and stores nothing. In that mode `start_run` returns
`RunId(0)` and reads return empty results.

## Schema (v1)

`runs` has one row per agent run (dispatch attempt):

| column | type | notes |
|---|---|---|
| `id` | INTEGER PK AUTOINCREMENT | `RunRecord.id`; ids are never reused |
| `issue_id`, `issue_identifier`, `issue_title` | TEXT | if the identifier is empty, `issue_id` is used |
| `attempt` | INTEGER | retry attempt (0 = first dispatch) |
| `worker_host`, `workspace_path` | TEXT NULL | set at start or later with `update_runtime_info` |
| `status` | TEXT CHECK | `running`, `succeeded`, `failed`, `cancelled` or `blocked` |
| `error` | TEXT NULL | |
| `turns` | INTEGER | `increment_turns` |
| `started_at`, `finished_at` | INTEGER | Unix epoch milliseconds (UTC) |
| `duration_ms` | INTEGER NULL | `max(0, finished_at - started_at)` |
| `input_tokens`, `output_tokens`, `total_tokens` | INTEGER | cumulative for the run; `update_tokens` overwrites them |
| `last_event_seq` | INTEGER | per-run seq allocator |

There are indexes on `(issue_identifier, id)`, `(status, id)` and `started_at`.

`run_events` (`WITHOUT ROWID`, PK `(run_id, seq)`, FK to `runs` with `ON DELETE CASCADE`) holds one
row per event: `at` (epoch ms), `kind`, `message`, and `payload` (JSON text).

- `seq` starts at 1 for each run and has no gaps.
- Messages longer than 16 KiB are truncated.
- If a payload's JSON is larger than 256 KiB, it is replaced with
  `{"truncated": true, "original_bytes": n}`.

## JSON contract

These are the shapes the server and web UI rely on:

- `RunRecord`: `{id, issue_id, issue_identifier, issue_title, attempt, worker_host, workspace_path,
  status, error, turns, started_at, finished_at, duration_ms, tokens: {input, output, total}}`
- `RunEvent`: `{run_id, seq, at, kind, message, payload}`
- `TotalsRecord`: `{runs_total, runs_succeeded, runs_failed, tokens: {input, output, total}, runtime_ms}`
- `RunPage`: `{runs: [RunRecord], next_before_id}`. Runs are listed newest first. To get the next
  page, pass `next_before_id` back as `before_id`; it is `null` on the last page.
- `RunQuery` (deserializable from a query string): `limit` (default 50, max 200), `before_id`,
  `issue_identifier`, `status`.

Timestamps are RFC 3339 UTC strings with millisecond precision.

## Configuration (wired by the CLI)

| env var | default | meaning |
|---|---|---|
| `SYMPHONY_DB_PATH` | `./data/symphony.db` | path to the database file; parent directories are created. Use `:memory:` for an in-memory database. An empty value, `off`, `none`, `disabled`, `false` or `0` turns persistence off |
| `SYMPHONY_DB_RETENTION_DAYS` | `30` | `prune` window in whole days; `0` keeps history forever |

`StoreConfig::from_env()` parses both variables. Suggested wiring:

1. Open the store at startup and call `mark_interrupted_runs()`. Any run that was still `running`
   becomes `cancelled` with error `"interrupted by restart"`, and a `run_interrupted` event is
   added to it.
2. Call `prune(retention, DEFAULT_KEEP_MIN_RUNS = 100)` at startup and then every
   `DEFAULT_PRUNE_INTERVAL` (24 h). Pruning deletes finished runs that started before the cutoff,
   together with their events. It never deletes `running` runs or the newest `keep_min_runs` runs.
3. Call `flush().await` during graceful shutdown.

`totals()` aggregates over the runs that are still retained, so pruning lowers the totals.
