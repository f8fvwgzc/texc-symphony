-- Task dependencies (SQLite migration 0007).
CREATE TABLE IF NOT EXISTS task_dependencies (
    task_id    BIGINT NOT NULL REFERENCES tasks (id) ON DELETE CASCADE,
    depends_on BIGINT NOT NULL REFERENCES tasks (id),
    PRIMARY KEY (task_id, depends_on)
);

CREATE INDEX IF NOT EXISTS task_dependencies_depends_on_idx ON task_dependencies (depends_on);
