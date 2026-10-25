-- Tasks submitted through the task API (POST /api/v1/tasks): work without a tracker issue.
-- Their runs are ordinary `runs` rows with issue_identifier `TASK-<id>`.
CREATE TABLE IF NOT EXISTS tasks (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    title       TEXT,
    prompt      TEXT    NOT NULL,
    repo        TEXT,
    branch      TEXT,
    backend     TEXT,
    max_tokens  INTEGER,
    timeout_ms  INTEGER,
    metadata    TEXT,   -- JSON object from the submitter, or NULL
    status      TEXT    NOT NULL DEFAULT 'queued'
                CHECK (status IN ('queued', 'running', 'succeeded', 'failed', 'cancelled', 'blocked')),
    result      TEXT,
    error       TEXT,
    attempts    INTEGER NOT NULL DEFAULT 0,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    started_at  INTEGER,
    finished_at INTEGER
);

CREATE INDEX IF NOT EXISTS tasks_status_idx ON tasks (status, id);
