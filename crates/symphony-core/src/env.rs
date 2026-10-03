//! Environment access and `$VAR` indirection rules.
//!
//! Two resolution flavours exist in the Elixir code and both are reproduced exactly:
//!
//! - **Linear / schema** ([`resolve_secret_setting`], [`resolve_path_value`]): no trimming, invalid
//!   `$refs` stay literal, an env var set to `""` yields `None` (no fallback), an unset var yields the
//!   fallback.
//! - **Other adapters** ([`resolve_setting`]): values are trimmed (blank -> `None`), an invalid `$ref`
//!   yields `None`, an env var set to `""` yields `None` (no fallback), an unset var yields the fallback.
//!
//! Only a value that is *entirely* a reference (`$NAME`, `NAME =~ ^[A-Za-z_][A-Za-z0-9_]*$`) is resolved;
//! there is no interpolation, `${X}` and the legacy `env:X` syntax are kept literally.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;

use serde_json::Value;

/// Source of environment variables (injectable so tests never mutate the process environment).
pub trait EnvSource: Send + Sync + fmt::Debug {
    /// Returns the variable's value, or `None` when unset (or not valid UTF-8).
    fn var(&self, name: &str) -> Option<String>;

    /// The system temp dir (Elixir `System.tmp_dir!()`), used for the default workspace root.
    fn tmp_dir(&self) -> PathBuf {
        std::env::temp_dir()
    }
}

/// The real process environment.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessEnv;

impl EnvSource for ProcessEnv {
    fn var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

/// An in-memory environment for tests and embedding.
#[derive(Debug, Clone, Default)]
pub struct MapEnv {
    vars: HashMap<String, String>,
    tmp_dir: Option<PathBuf>,
}

impl MapEnv {
    /// Empty environment.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder-style setter for one variable.
    pub fn with(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.vars.insert(name.into(), value.into());
        self
    }

    /// Overrides the temp dir reported by [`EnvSource::tmp_dir`].
    pub fn with_tmp_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.tmp_dir = Some(dir.into());
        self
    }

    /// Sets a variable in place.
    pub fn set(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.vars.insert(name.into(), value.into());
    }

    /// Removes a variable.
    pub fn remove(&mut self, name: &str) {
        self.vars.remove(name);
    }
}

impl EnvSource for MapEnv {
    fn var(&self, name: &str) -> Option<String> {
        self.vars.get(name).cloned()
    }

    fn tmp_dir(&self) -> PathBuf {
        self.tmp_dir.clone().unwrap_or_else(std::env::temp_dir)
    }
}

/// Returns `true` when `name` matches `^[A-Za-z_][A-Za-z0-9_]*$`.
pub fn valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {
            chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        _ => false,
    }
}

/// If `value` is a whole-value `$NAME` reference with a valid name, returns `NAME`.
pub fn env_reference_name(value: &str) -> Option<&str> {
    value.strip_prefix('$').filter(|name| valid_env_name(name))
}

/// Names referenced by the given values (non-strings and invalid references are skipped).
pub fn env_reference_names<'a>(values: impl IntoIterator<Item = Option<&'a Value>>) -> Vec<String> {
    values
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(env_reference_name)
        .map(str::to_owned)
        .collect()
}

/// Deduplicates while preserving first-occurrence order (Elixir `Enum.uniq/1`).
pub fn uniq(values: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for value in values {
        if !out.contains(&value) {
            out.push(value);
        }
    }
    out
}

fn normalize_secret(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.is_empty())
}

/// Linear-flavoured secret resolution (`Schema.resolve_secret_setting/2`).
///
/// - absent/`null` -> the fallback (`""` -> `None`);
/// - `"$NAME"`: env set and non-empty -> its value; env `""` -> `None`; env unset -> the fallback;
/// - any other string is kept literally (no trimming; only `""` becomes `None`);
/// - non-string values yield `None` (Elixir keeps the raw term so validation rejects it later; callers
///   that must distinguish this case inspect the raw value themselves).
pub fn resolve_secret_setting(
    value: Option<&Value>,
    fallback: Option<String>,
    env: &dyn EnvSource,
) -> Option<String> {
    match value {
        None | Some(Value::Null) => normalize_secret(fallback),
        Some(Value::String(raw)) => match env_reference_name(raw) {
            Some(name) => match env.var(name) {
                None => normalize_secret(fallback),
                Some(found) if found.is_empty() => None,
                Some(found) => Some(found),
            },
            None => normalize_secret(Some(raw.clone())),
        },
        Some(_) => None,
    }
}

/// `workspace.root` resolution (`Schema.resolve_path_value/2`): a valid `$NAME` resolves to the env value
/// (unset or `""` -> `default`), a literal `""` -> `default`, anything else is kept raw.
pub fn resolve_path_value(value: &str, default: &str, env: &dyn EnvSource) -> String {
    let resolved = match env_reference_name(value) {
        Some(name) => match env.var(name) {
            None => return default.to_owned(),
            Some(found) => found,
        },
        None => value.to_owned(),
    };
    if resolved.is_empty() {
        default.to_owned()
    } else {
        resolved
    }
}

/// Trims; a blank result becomes `None` (the adapters' `normalize_string/1`).
pub fn normalize_optional_string(value: Option<&str>) -> Option<String> {
    let trimmed = value?.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

/// Adapter-flavoured resolution (GitHub/GitLab/Jira/Asana `resolve_setting/2`).
///
/// - absent/`null` -> trimmed fallback;
/// - valid `"$NAME"` -> trimmed `env || fallback` (an empty env string does *not* fall back);
/// - invalid `"$..."` -> `None`;
/// - other strings -> trimmed (blank -> `None`);
/// - non-strings -> `None`.
pub fn resolve_setting(
    value: Option<&Value>,
    fallback: Option<String>,
    env: &dyn EnvSource,
) -> Option<String> {
    match value {
        None | Some(Value::Null) => normalize_optional_string(fallback.as_deref()),
        Some(Value::String(raw)) => match raw.strip_prefix('$') {
            Some(name) if valid_env_name(name) => {
                normalize_optional_string(env.var(name).or(fallback).as_deref())
            }
            Some(_) => None,
            None => normalize_optional_string(Some(raw)),
        },
        Some(_) => None,
    }
}
