-- Spend accounting (per-run cost) and operator controls such as the dispatch kill switch.
-- Cost lives in its own table so this migration stays safe to re-run (SQLite has no
-- `ADD COLUMN IF NOT EXISTS`).
CREATE TABLE IF NOT EXISTS run_costs (
    run_id         INTEGER PRIMARY KEY REFERENCES runs (id) ON DELETE CASCADE,
    cost_micro_usd INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS runs_issue_id_idx ON runs (issue_id, id);

CREATE TABLE IF NOT EXISTS control (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID;
