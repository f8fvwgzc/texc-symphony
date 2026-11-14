-- Normal exits counted against `agent.max_continuations` (SQLite migration 0010).
CREATE TABLE IF NOT EXISTS retry_continuations (
    issue_id      TEXT   PRIMARY KEY REFERENCES retry_queue (issue_id) ON DELETE CASCADE,
    continuations BIGINT NOT NULL
);

-- Before they were merged, this migration and 0005 (`retry_read_failures`) were both numbered
-- 0005 on their branches: create that table too if a build of this one's branch skipped it.
CREATE TABLE IF NOT EXISTS retry_read_failures (
    issue_id      TEXT   PRIMARY KEY REFERENCES retry_queue (issue_id) ON DELETE CASCADE,
    read_failures BIGINT NOT NULL DEFAULT 0
);
