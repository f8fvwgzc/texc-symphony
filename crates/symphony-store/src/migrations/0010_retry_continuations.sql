-- How many runs of a queued retry's issue ended normally with the issue still active since it was
-- claimed, counted against `agent.max_continuations`. A queued retry without a row has none. A
-- table of its own keeps the migration safe to re-run, like `run_costs`.
CREATE TABLE IF NOT EXISTS retry_continuations (
    issue_id      TEXT PRIMARY KEY REFERENCES retry_queue (issue_id) ON DELETE CASCADE,
    continuations INTEGER NOT NULL
) WITHOUT ROWID;

-- Before they were merged, this migration and 0009 (`retry_read_failures`) were both numbered
-- 0009 on their branches. A database that a build of this one's branch brought to version 9 has
-- no `retry_read_failures` table and skips 0009 here, so it is created now if it is missing.
CREATE TABLE IF NOT EXISTS retry_read_failures (
    issue_id      TEXT PRIMARY KEY REFERENCES retry_queue (issue_id) ON DELETE CASCADE,
    read_failures INTEGER NOT NULL DEFAULT 0
) WITHOUT ROWID;
