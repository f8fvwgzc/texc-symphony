-- Failed tracker reads of a queued retry since its issue last ran (SQLite migration 0009).
CREATE TABLE IF NOT EXISTS retry_read_failures (
    issue_id      TEXT   PRIMARY KEY REFERENCES retry_queue (issue_id) ON DELETE CASCADE,
    read_failures BIGINT NOT NULL DEFAULT 0
);
