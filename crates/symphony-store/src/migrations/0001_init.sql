-- symphony-store schema v1: agent runs and their event log.
-- Timestamps are INTEGER unix epoch milliseconds (UTC).

CREATE TABLE IF NOT EXISTS runs (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    issue_id         TEXT    NOT NULL,
    issue_identifier TEXT    NOT NULL,
    issue_title      TEXT,
    attempt          INTEGER NOT NULL DEFAULT 0,
    worker_host      TEXT,
    workspace_path   TEXT,
    status           TEXT    NOT NULL DEFAULT 'running'
                     CHECK (status IN ('running', 'succeeded', 'failed', 'cancelled', 'blocked')),
    error            TEXT,
    turns            INTEGER NOT NULL DEFAULT 0,
    started_at       INTEGER NOT NULL,
    finished_at      INTEGER,
    duration_ms      INTEGER,
    input_tokens     INTEGER NOT NULL DEFAULT 0,
    output_tokens    INTEGER NOT NULL DEFAULT 0,
    total_tokens     INTEGER NOT NULL DEFAULT 0,
    -- Highest event seq handed out for this run (seq is allocated per run, starting at 1).
    last_event_seq   INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS runs_issue_identifier_idx ON runs (issue_identifier, id);
CREATE INDEX IF NOT EXISTS runs_status_idx ON runs (status, id);
CREATE INDEX IF NOT EXISTS runs_started_at_idx ON runs (started_at);

CREATE TABLE IF NOT EXISTS run_events (
    run_id  INTEGER NOT NULL REFERENCES runs (id) ON DELETE CASCADE,
    seq     INTEGER NOT NULL,
    at      INTEGER NOT NULL,
    kind    TEXT    NOT NULL,
    message TEXT,
    payload TEXT,   -- JSON document, or NULL
    PRIMARY KEY (run_id, seq)
) WITHOUT ROWID;
