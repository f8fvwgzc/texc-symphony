# Migration from Elixir

Per-crate notes on parity decisions and new capabilities in the Rust port.

## symphony-store

- **New capability.** Elixir kept all run state in memory, so it was lost on restart. The Rust port
  adds a SQLite store (`crates/symphony-store`, with bundled SQLite and WAL) that records each agent
  run along with:
  - its status: `running`, `succeeded`, `failed`, `cancelled` or `blocked`;
  - the error;
  - the number of turns;
  - cumulative token usage;
  - the duration;
  - a per-run event log.
- **Live state is unaffected.** `/api/v1/state` parity still comes from the orchestrator's
  in-memory snapshot, and the store is purely additive history. `TotalsRecord` counts all retained
  runs, unlike the Elixir `codex_totals`, which only counts the current process lifetime.
- **Restart recovery.** At startup, runs left `running` by a crashed process are closed as
  `cancelled` with error `"interrupted by restart"`.
- **Configuration.** `SYMPHONY_DB_PATH` (default `./data/symphony.db`; `off` disables it, and
  `Store::disabled()` is then a no-op) and `SYMPHONY_DB_RETENTION_DAYS` (default 30; `0` keeps
  history forever). The CLI will wire these.
- **Failure handling.** Persistence failures never crash the orchestrator. Operations return
  `StoreError` values, and fire-and-forget writes log their failures with `tracing::warn!`.
