//! Pure rules for tasks submitted through the task API (`POST /api/v1/tasks`).

/// Longest accepted task prompt, in bytes.
pub const MAX_TASK_PROMPT_BYTES: usize = 256 * 1024;

/// `true` for a repository URL a task may clone: `https://` or `ssh://` URLs with a host, or the
/// scp-like `user@host:path` form. Everything else is refused, notably local paths, `file://` and
/// git transport helpers such as `ext::` (which run commands).
pub fn valid_repo_url(repo: &str) -> bool {
    let repo = repo.trim();
    if repo.is_empty()
        || repo.starts_with('-')
        || repo.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return false;
    }
    if let Some(rest) = repo
        .strip_prefix("https://")
        .or_else(|| repo.strip_prefix("ssh://"))
    {
        let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
        let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
        return !host.is_empty() && rest.len() > host.len();
    }
    // scp-like: user@host:path (no scheme, no `::`).
    match repo.split_once(':') {
        Some((user_host, path)) => {
            !repo.contains("::")
                && !repo.contains("://")
                && user_host
                    .split_once('@')
                    .is_some_and(|(user, host)| !user.is_empty() && !host.is_empty())
                && !path.is_empty()
        }
        None => false,
    }
}

/// `true` for a branch or tag name `git clone --branch` may receive.
pub fn valid_branch(branch: &str) -> bool {
    !branch.is_empty()
        && !branch.starts_with('-')
        && !branch.contains("..")
        && branch
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_network_repositories_are_cloned() {
        for ok in [
            "https://github.com/f8fvwgzc/texc-symphony",
            "https://github.com/f8fvwgzc/texc-symphony.git",
            "ssh://git@github.com/f8fvwgzc/texc-symphony.git",
            "git@github.com:f8fvwgzc/texc-symphony.git",
        ] {
            assert!(valid_repo_url(ok), "{ok}");
        }
        for bad in [
            "",
            "/etc",
            "../repo",
            "file:///etc",
            "ext::sh -c touch% /tmp/pwned",
            "http://example.com/repo",
            "https://",
            "https://host",
            "--upload-pack=touch /tmp/x",
            "host:path",
            "git@github.com:a b",
        ] {
            assert!(!valid_repo_url(bad), "{bad}");
        }
    }

    #[test]
    fn branches_are_plain_ref_names() {
        assert!(valid_branch("main"));
        assert!(valid_branch("release/1.2"));
        assert!(!valid_branch("--orphan"));
        assert!(!valid_branch("a..b"));
        assert!(!valid_branch("a b"));
    }
}
