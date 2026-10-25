-- Dispatch leases: who runs an issue or task right now. A process holds the lease of every issue
-- it has claimed and renews it; replicas sharing a database skip issues another live replica
-- holds, and work whose lease expired (its process died) is recovered.
CREATE TABLE IF NOT EXISTS leases (
    key        TEXT    PRIMARY KEY,   -- issue id (`task:<id>` for tasks)
    owner      TEXT    NOT NULL,      -- replica id
    expires_at INTEGER NOT NULL       -- unix milliseconds
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS leases_owner_idx ON leases (owner);
