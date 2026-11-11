-- Normal exits counted against `agent.max_continuations` (SQLite migration 0009).
CREATE TABLE IF NOT EXISTS retry_continuations (
    issue_id      TEXT   PRIMARY KEY REFERENCES retry_queue (issue_id) ON DELETE CASCADE,
    continuations BIGINT NOT NULL
);
