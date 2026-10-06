-- Retries waiting for their timer. Kept so a restart continues with the same attempt counts and
-- due times instead of starting every failing issue over.
CREATE TABLE IF NOT EXISTS retry_queue (
    issue_id       TEXT PRIMARY KEY,
    attempt        INTEGER NOT NULL,
    due_at         INTEGER NOT NULL,   -- unix milliseconds
    identifier     TEXT NOT NULL,
    issue_url      TEXT,
    error          TEXT,
    worker_host    TEXT,
    workspace_path TEXT
) WITHOUT ROWID;
