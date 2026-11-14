-- Failed tracker reads of a queued retry since its issue last ran. They lengthen its backoff but
-- are not attempts, so a tracker outage never counts toward the retry limit. A table of its own
-- keeps the migration safe to re-run; retries queued before it have no row here (no failures).
CREATE TABLE IF NOT EXISTS retry_read_failures (
    issue_id      TEXT PRIMARY KEY REFERENCES retry_queue (issue_id) ON DELETE CASCADE,
    read_failures INTEGER NOT NULL DEFAULT 0
) WITHOUT ROWID;
