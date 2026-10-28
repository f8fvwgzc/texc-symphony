-- Prompt-cache reads of a run, metered apart from `runs.input_tokens` (which no longer includes
-- them; runs recorded before this migration have no row here and count their cache reads in
-- `input_tokens`). A table of its own keeps the migration safe to re-run, like `run_costs`.
CREATE TABLE IF NOT EXISTS run_cached_tokens (
    run_id              INTEGER PRIMARY KEY REFERENCES runs (id) ON DELETE CASCADE,
    cached_input_tokens INTEGER NOT NULL DEFAULT 0
);
