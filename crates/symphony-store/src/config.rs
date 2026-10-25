//! Environment knobs for the store. The CLI reads these at startup.

use std::path::PathBuf;
use std::time::Duration;

use crate::error::{Result, StoreError};
use crate::store::Store;

/// Env var naming the database file (`:memory:` for an in-memory database; empty, `off`,
/// `none`, `disabled`, `false` or `0` to turn persistence off; a `postgres://` URL works too).
pub const ENV_DB_PATH: &str = "SYMPHONY_DB_PATH";
/// Env var with a PostgreSQL connection string (`postgres://user:pass@host/db?sslmode=require`);
/// wins over [`ENV_DB_PATH`].
pub const ENV_DATABASE_URL: &str = "SYMPHONY_DATABASE_URL";
/// Env var with the retention window in whole days (`0` keeps history forever).
pub const ENV_RETENTION_DAYS: &str = "SYMPHONY_DB_RETENTION_DAYS";
/// Env var with the payload retention window in whole days (`0` keeps payloads as long as the
/// runs). Event payloads hold raw agent messages and command output, so they expire sooner.
pub const ENV_PAYLOAD_RETENTION_DAYS: &str = "SYMPHONY_DB_PAYLOAD_RETENTION_DAYS";
/// Payload retention window used when [`ENV_PAYLOAD_RETENTION_DAYS`] is unset.
pub const DEFAULT_PAYLOAD_RETENTION_DAYS: u32 = 7;
/// Database path used when [`ENV_DB_PATH`] is unset.
pub const DEFAULT_DB_PATH: &str = "./data/symphony.db";
/// Retention window used when [`ENV_RETENTION_DAYS`] is unset.
pub const DEFAULT_RETENTION_DAYS: u32 = 30;
/// Suggested `keep_min_runs` for retention passes: the newest runs survive regardless of age.
pub const DEFAULT_KEEP_MIN_RUNS: u32 = 100;
/// Suggested interval between retention passes.
pub const DEFAULT_PRUNE_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

const SECONDS_PER_DAY: u64 = 24 * 60 * 60;

/// Where the store keeps its data.
#[derive(Clone, PartialEq, Eq)]
pub enum StoreLocation {
    /// Persistence turned off ([`Store::disabled`]).
    Disabled,
    /// Private in-memory database (lost on exit).
    Memory,
    /// SQLite database file.
    File(PathBuf),
    /// PostgreSQL connection string (its password is never printed).
    Postgres(String),
}

impl std::fmt::Debug for StoreLocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => f.write_str("Disabled"),
            Self::Memory => f.write_str("Memory"),
            Self::File(path) => f.debug_tuple("File").field(path).finish(),
            Self::Postgres(url) => f
                .debug_tuple("Postgres")
                .field(&crate::store::redact_url(url))
                .finish(),
        }
    }
}

fn is_postgres_url(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.starts_with("postgres://") || lower.starts_with("postgresql://")
}

/// Resolved store configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreConfig {
    /// Database location.
    pub location: StoreLocation,
    /// Retention window; `None` keeps history forever.
    pub retention: Option<Duration>,
    /// Payload retention window; `None` keeps payloads as long as their runs.
    pub payload_retention: Option<Duration>,
}

impl Default for StoreConfig {
    fn default() -> Self {
        StoreConfig {
            location: StoreLocation::File(PathBuf::from(DEFAULT_DB_PATH)),
            retention: Some(days(DEFAULT_RETENTION_DAYS)),
            payload_retention: Some(days(DEFAULT_PAYLOAD_RETENTION_DAYS)),
        }
    }
}

impl StoreConfig {
    /// Read [`ENV_DB_PATH`] and [`ENV_RETENTION_DAYS`] from the process environment.
    pub fn from_env() -> Result<StoreConfig> {
        StoreConfig::from_lookup(|name| std::env::var(name).ok())
    }

    /// Like [`StoreConfig::from_env`] with an injectable variable lookup (for tests).
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<StoreConfig> {
        let url = lookup(ENV_DATABASE_URL)
            .map(|raw| raw.trim().to_owned())
            .filter(|raw| !raw.is_empty());
        let location = match (url, lookup(ENV_DB_PATH)) {
            (Some(url), _) if is_postgres_url(&url) => StoreLocation::Postgres(url),
            (Some(_), _) => {
                return Err(StoreError::InvalidConfig(format!(
                    "{ENV_DATABASE_URL} must be a postgres:// or postgresql:// URL"
                )));
            }
            (None, None) => StoreLocation::File(PathBuf::from(DEFAULT_DB_PATH)),
            (None, Some(raw)) => {
                let value = raw.trim();
                match value.to_ascii_lowercase().as_str() {
                    "" | "off" | "none" | "disabled" | "false" | "0" => StoreLocation::Disabled,
                    ":memory:" => StoreLocation::Memory,
                    _ if is_postgres_url(value) => StoreLocation::Postgres(value.to_owned()),
                    _ => StoreLocation::File(PathBuf::from(value)),
                }
            }
        };
        let window = |name: &str, default: u32| match lookup(name).as_deref().map(str::trim) {
            None | Some("") => Ok(Some(days(default))),
            Some(raw) => match raw.parse::<u32>() {
                Ok(0) => Ok(None),
                Ok(n) => Ok(Some(days(n))),
                Err(_) => Err(StoreError::InvalidConfig(format!(
                    "{name} must be a non-negative integer number of days, got {raw:?}"
                ))),
            },
        };
        Ok(StoreConfig {
            location,
            retention: window(ENV_RETENTION_DAYS, DEFAULT_RETENTION_DAYS)?,
            payload_retention: window(ENV_PAYLOAD_RETENTION_DAYS, DEFAULT_PAYLOAD_RETENTION_DAYS)?,
        })
    }

    /// Open the configured store.
    pub fn open(&self) -> Result<Store> {
        match &self.location {
            StoreLocation::Disabled => Ok(Store::disabled()),
            StoreLocation::Memory => Store::open_in_memory(),
            StoreLocation::File(path) => Store::open(path),
            StoreLocation::Postgres(url) => Store::open_postgres(url),
        }
    }
}

fn days(n: u32) -> Duration {
    Duration::from_secs(u64::from(n) * SECONDS_PER_DAY)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(vars: &[(&str, &str)]) -> Result<StoreConfig> {
        StoreConfig::from_lookup(|name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        })
    }

    #[test]
    fn defaults_apply_when_unset() {
        let cfg = config(&[]).unwrap();
        assert_eq!(cfg, StoreConfig::default());
        assert_eq!(
            cfg.location,
            StoreLocation::File(PathBuf::from("./data/symphony.db"))
        );
        assert_eq!(cfg.retention, Some(Duration::from_secs(30 * 86_400)));
    }

    #[test]
    fn parses_path_and_retention() {
        let cfg = config(&[
            (ENV_DB_PATH, " /var/lib/symphony/runs.db "),
            (ENV_RETENTION_DAYS, "7"),
        ])
        .unwrap();
        assert_eq!(
            cfg.location,
            StoreLocation::File(PathBuf::from("/var/lib/symphony/runs.db"))
        );
        assert_eq!(cfg.retention, Some(Duration::from_secs(7 * 86_400)));
        assert_eq!(
            config(&[(ENV_RETENTION_DAYS, "0")]).unwrap().retention,
            None
        );
    }

    #[test]
    fn special_path_values() {
        for off in ["", "off", "OFF", "none", "disabled", "false", "0"] {
            assert_eq!(
                config(&[(ENV_DB_PATH, off)]).unwrap().location,
                StoreLocation::Disabled
            );
        }
        let cfg = config(&[(ENV_DB_PATH, ":memory:")]).unwrap();
        assert_eq!(cfg.location, StoreLocation::Memory);
        assert!(cfg.open().unwrap().is_enabled());
        let disabled = config(&[(ENV_DB_PATH, "off")]).unwrap().open().unwrap();
        assert!(!disabled.is_enabled());
    }

    #[test]
    fn payload_retention_defaults_to_a_week() {
        assert_eq!(
            config(&[]).unwrap().payload_retention,
            Some(Duration::from_secs(7 * 86_400))
        );
        assert_eq!(
            config(&[(ENV_PAYLOAD_RETENTION_DAYS, "2")])
                .unwrap()
                .payload_retention,
            Some(Duration::from_secs(2 * 86_400))
        );
        assert_eq!(
            config(&[(ENV_PAYLOAD_RETENTION_DAYS, "0")])
                .unwrap()
                .payload_retention,
            None
        );
        assert!(config(&[(ENV_PAYLOAD_RETENTION_DAYS, "x")]).is_err());
    }

    #[test]
    fn rejects_bad_retention() {
        for bad in ["-1", "thirty", "1.5"] {
            let err = config(&[(ENV_RETENTION_DAYS, bad)]).unwrap_err();
            assert!(matches!(err, StoreError::InvalidConfig(_)), "{bad}: {err}");
        }
    }

    #[test]
    fn postgres_urls_select_postgres_and_hide_the_password() {
        let url = "postgres://symphony:s3cret@db.internal:5432/symphony?sslmode=require";
        let cfg = config(&[(ENV_DATABASE_URL, url), (ENV_DB_PATH, "/ignored.db")]).unwrap();
        assert_eq!(cfg.location, StoreLocation::Postgres(url.into()));
        let shown = format!("{cfg:?}");
        assert!(!shown.contains("s3cret"), "{shown}");
        assert!(shown.contains("db.internal"), "{shown}");
        let cfg = config(&[(ENV_DB_PATH, "postgresql://u@h/db")]).unwrap();
        assert_eq!(
            cfg.location,
            StoreLocation::Postgres("postgresql://u@h/db".into())
        );
        assert!(config(&[(ENV_DATABASE_URL, "mysql://x")]).is_err());
        assert_eq!(
            crate::store::redact_url("host=db user=u password=secret dbname=s"),
            "host=db user=u password=*** dbname=s"
        );
    }
}
