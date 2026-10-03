//! The version reported by `symphony --version` and `GET /api/v1/health`.
//!
//! The crate version plus an optional build-time suffix: release and Docker builds of the rolling
//! nightly set `SYMPHONY_VERSION_SUFFIX=-nightly` so `Cargo.toml` (and `Cargo.lock` under
//! `--locked`) stay untouched. `option_env!` is tracked by cargo, so changing the variable rebuilds.

use std::sync::LazyLock;

/// `SYMPHONY_VERSION_SUFFIX` at build time (empty when unset).
pub const VERSION_SUFFIX: &str = match option_env!("SYMPHONY_VERSION_SUFFIX") {
    Some(suffix) => suffix,
    None => "",
};

static VERSION: LazyLock<String> =
    LazyLock::new(|| format!("{}{}", env!("CARGO_PKG_VERSION"), VERSION_SUFFIX));

/// `CARGO_PKG_VERSION` + [`VERSION_SUFFIX`], e.g. `0.1.0` or `0.1.0-nightly`.
pub fn version() -> &'static str {
    VERSION.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_starts_with_the_crate_version_and_ends_with_the_suffix() {
        assert!(version().starts_with(env!("CARGO_PKG_VERSION")));
        assert!(version().ends_with(VERSION_SUFFIX));
    }
}
