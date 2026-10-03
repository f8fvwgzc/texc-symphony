//! Per-issue workspace directory names (Elixir `Workspace.workspace_key/1`).

use sha2::{Digest, Sha256};

/// Key used when the issue has no string identifier.
pub const FALLBACK_WORKSPACE_KEY: &str = "issue";

/// Replaces every byte outside `[a-zA-Z0-9._-]` with `_`.
///
/// Elixir's regex runs without the `u` flag, so it is **byte** oriented: each byte of a multi-byte UTF-8
/// character becomes one `_` (`"é"` -> `"__"`). This is kept for parity so workspaces created by the
/// Elixir implementation are found again.
pub fn sanitize_identifier(identifier: &str) -> String {
    identifier
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-') {
                char::from(b)
            } else {
                '_'
            }
        })
        .collect()
}

/// Workspace key for an identifier: the sanitized identifier, plus `--<first 16 lowercase hex chars of
/// SHA-256(identifier)>` whenever sanitization changed anything (so `team/a-1` and `team_a-1` do not
/// collide). `None` yields [`FALLBACK_WORKSPACE_KEY`].
pub fn workspace_key(identifier: Option<&str>) -> String {
    let Some(identifier) = identifier else {
        return FALLBACK_WORKSPACE_KEY.to_owned();
    };
    let safe = sanitize_identifier(identifier);
    if safe == identifier {
        safe
    } else {
        let digest = hex::encode(Sha256::digest(identifier.as_bytes()));
        format!("{safe}--{}", &digest[..16])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_path_is_deterministic_per_issue_identifier() {
        let first = workspace_key(Some("MT/Det"));
        assert_eq!(first, workspace_key(Some("MT/Det")));
        assert!(first.starts_with("MT_Det--"));
        assert_eq!(first.len(), "MT_Det--".len() + 16);
        assert!(
            first["MT_Det--".len()..]
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }

    #[test]
    fn workspace_keys_disambiguate_identifiers_that_sanitize_to_the_same_path() {
        let slash = workspace_key(Some("team/a-1"));
        let plain = workspace_key(Some("team_a-1"));
        assert!(slash.starts_with("team_a-1--"));
        assert_eq!(plain, "team_a-1");
        assert_ne!(slash, plain);
    }

    #[test]
    fn safe_identifiers_are_kept_verbatim() {
        assert_eq!(workspace_key(Some("MT-1.fix_x")), "MT-1.fix_x");
        assert_eq!(workspace_key(None), "issue");
    }

    #[test]
    fn non_ascii_is_replaced_per_byte_like_elixir() {
        assert_eq!(sanitize_identifier("éa/ą"), "__a___");
        let digest = hex::encode(Sha256::digest("éa/ą".as_bytes()));
        assert_eq!(
            workspace_key(Some("éa/ą")),
            format!("__a___--{}", &digest[..16])
        );
    }

    #[test]
    fn keys_match_the_elixir_implementation() {
        // Reference values produced by `SymphonyElixir.Workspace.workspace_key/1`.
        assert_eq!(workspace_key(Some("MT/Det")), "MT_Det--b45ad8f201f2d40b");
        assert_eq!(workspace_key(Some("éa/ą")), "__a___--81ff463176b0cdfa");
    }
}
