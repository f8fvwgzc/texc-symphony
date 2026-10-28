-- Per-task retry policy and state (SQLite migration 0008).
CREATE TABLE IF NOT EXISTS task_retry (
    task_id        BIGINT PRIMARY KEY REFERENCES tasks (id) ON DELETE CASCADE,
    max_attempts   BIGINT NOT NULL,
    backoff_ms     BIGINT NOT NULL,
    max_backoff_ms BIGINT NOT NULL,
    retry_on       TEXT   NOT NULL,
    failures       BIGINT NOT NULL DEFAULT 0,
    not_before     BIGINT
);

INSERT INTO task_retry (task_id, max_attempts, backoff_ms, max_backoff_ms, retry_on)
SELECT id, 3, 10000, 300000, 'interrupted' FROM tasks
ON CONFLICT (task_id) DO NOTHING;
