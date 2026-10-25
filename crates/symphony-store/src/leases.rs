//! Dispatch leases (`leases` table): the replica that dispatches an issue or task holds its
//! lease (key = issue id, `task:<id>` for tasks) while it has it claimed, renewing it before it
//! expires. Another replica sharing the database cannot take a live lease, so no issue or task
//! runs twice at once; when a replica dies its leases expire and its interrupted runs and tasks
//! are recovered (`ops::mark_interrupted_runs`, `tasks::requeue_interrupted_tasks`).

use chrono::{DateTime, Utc};

use crate::db::{Db, in_transaction, vals};
use crate::error::Result;

fn expiry(now: DateTime<Utc>, ttl: std::time::Duration) -> i64 {
    let ttl = i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX);
    now.timestamp_millis().saturating_add(ttl)
}

/// Takes `key` for `owner` until `now + ttl`. `true` when `owner` holds it afterwards: it was
/// free, expired, or already `owner`'s (then renewed).
pub(crate) fn acquire(
    db: &mut dyn Db,
    key: &str,
    owner: &str,
    now: DateTime<Utc>,
    ttl: std::time::Duration,
) -> Result<bool> {
    let changed = db.execute(
        "INSERT INTO leases (key, owner, expires_at) VALUES (?1, ?2, ?3) \
         ON CONFLICT (key) DO UPDATE SET owner = excluded.owner, expires_at = excluded.expires_at \
         WHERE leases.owner = excluded.owner OR leases.expires_at <= ?4",
        &vals![key, owner, expiry(now, ttl), now.timestamp_millis()],
    )?;
    Ok(changed > 0)
}

/// Extends `owner`'s leases on `keys` to `now + ttl`; returns how many it still held.
pub(crate) fn renew(
    db: &mut dyn Db,
    owner: &str,
    keys: &[String],
    now: DateTime<Utc>,
    ttl: std::time::Duration,
) -> Result<u64> {
    if keys.is_empty() {
        return Ok(0);
    }
    let expires_at = expiry(now, ttl);
    in_transaction(db, |db| {
        let mut held = 0;
        for key in keys {
            held += db.execute(
                "UPDATE leases SET expires_at = ?1 WHERE key = ?2 AND owner = ?3",
                &vals![expires_at, key.as_str(), owner],
            )?;
        }
        Ok(held)
    })
}

/// Gives up `owner`'s lease on `key` (another owner's lease is left alone).
pub(crate) fn release(db: &mut dyn Db, key: &str, owner: &str) -> Result<()> {
    db.execute(
        "DELETE FROM leases WHERE key = ?1 AND owner = ?2",
        &vals![key, owner],
    )?;
    Ok(())
}

/// Drops every lease of `owner` (a restarted replica with a stable id); returns how many.
pub(crate) fn release_owner(db: &mut dyn Db, owner: &str) -> Result<u64> {
    db.execute("DELETE FROM leases WHERE owner = ?1", &vals![owner])
}

/// The live holder of `key`, if any.
pub(crate) fn holder(db: &mut dyn Db, key: &str, now: DateTime<Utc>) -> Result<Option<String>> {
    db.query_opt(
        "SELECT owner FROM leases WHERE key = ?1 AND expires_at > ?2",
        &vals![key, now.timestamp_millis()],
    )?
    .map(|row| row.text(0))
    .transpose()
}
