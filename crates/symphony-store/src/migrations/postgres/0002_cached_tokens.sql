-- Prompt-cache reads of a run, metered apart from `runs.input_tokens` (SQLite migration 0006).
CREATE TABLE IF NOT EXISTS run_cached_tokens (
    run_id              BIGINT PRIMARY KEY REFERENCES runs (id) ON DELETE CASCADE,
    cached_input_tokens BIGINT NOT NULL DEFAULT 0
);
