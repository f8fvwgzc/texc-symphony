-- How many runs of a queued retry's issue ended normally with the issue still active since it was
-- claimed, counted against `agent.max_continuations`. A queued retry without a row has none. A
-- table of its own keeps the migration safe to re-run, like `run_costs`.
CREATE TABLE IF NOT EXISTS retry_continuations (
    issue_id      TEXT PRIMARY KEY REFERENCES retry_queue (issue_id) ON DELETE CASCADE,
    continuations INTEGER NOT NULL
) WITHOUT ROWID;
