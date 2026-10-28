-- Per-task retry policy (`retry` of POST /api/v1/tasks) and its state: failed attempts counted
-- against `max_attempts` since creation or the last unblock, and when a retried task may run.
-- `retry_on` is a comma-separated subset of error, timeout, interrupted. Tasks created before
-- this migration get the default policy (3 attempts, interruptions only).
CREATE TABLE IF NOT EXISTS task_retry (
    task_id        INTEGER PRIMARY KEY REFERENCES tasks (id) ON DELETE CASCADE,
    max_attempts   INTEGER NOT NULL,
    backoff_ms     INTEGER NOT NULL,
    max_backoff_ms INTEGER NOT NULL,
    retry_on       TEXT    NOT NULL,
    failures       INTEGER NOT NULL DEFAULT 0,
    not_before     INTEGER            -- unix milliseconds; NULL: as soon as a slot is free
);

INSERT INTO task_retry (task_id, max_attempts, backoff_ms, max_backoff_ms, retry_on)
SELECT id, 3, 10000, 300000, 'interrupted' FROM tasks WHERE true
ON CONFLICT (task_id) DO NOTHING;
