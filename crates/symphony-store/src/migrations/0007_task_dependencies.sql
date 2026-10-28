-- Task dependencies (`depends_on` of POST /api/v1/tasks): a queued task with a dependency that
-- has not succeeded is reported as `waiting` and not dispatched; one whose dependency failed or
-- was cancelled fails too. Dependencies exist before their dependants, so there are no cycles.
CREATE TABLE IF NOT EXISTS task_dependencies (
    task_id    INTEGER NOT NULL REFERENCES tasks (id) ON DELETE CASCADE,
    depends_on INTEGER NOT NULL REFERENCES tasks (id),
    PRIMARY KEY (task_id, depends_on)
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS task_dependencies_depends_on_idx ON task_dependencies (depends_on);
