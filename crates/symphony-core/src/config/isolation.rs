//! `isolation:` — per-task isolation of task-API runs (tracker issues are not affected).
//!
//! ```yaml
//! isolation:
//!   task_uids: {first: 200000, count: 1000}   # uid = gid = first + (task id mod count)
//!   github_app:                               # short-lived per-task repository credentials
//!     app_id: 123456
//!     private_key_file: /run/secrets/github-app/private-key.pem
//!     api_url: https://api.github.com         # default
//!     git_host: github.com                    # default
//!   network:
//!     mode: deny                              # default while `isolation:` is present
//!     allow: ["pypi.org:443"]                 # extra host:port entries
//! ```
//!
//! With `network.mode: deny` a task reaches only its repository's git host on 443, the LLM
//! gateway (`LLM_GATEWAY_URL`) and the `allow` entries. The runtime enforces the three parts
//! (`symphony_runtime::isolation`); this module only casts and validates them.

use std::fmt;
use std::path::PathBuf;

use super::cast::{Errors, Section};
use super::llm::LlmConnection;
use super::tracker::is_https_url;

/// Default `isolation.github_app.api_url`.
pub const DEFAULT_GITHUB_API_URL: &str = "https://api.github.com";
/// Default `isolation.github_app.git_host`.
pub const DEFAULT_GIT_HOST: &str = "github.com";
/// The only port a task may use to reach its repository's git host (https; never ssh).
pub const GIT_HTTPS_PORT: u16 = 443;

/// `isolation:`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IsolationSettings {
    /// `isolation.task_uids`: run every process of a task as its own uid/gid.
    pub task_uids: Option<TaskUids>,
    /// `isolation.github_app`: mint a repository-scoped GitHub App token per task.
    pub github_app: Option<GitHubAppSettings>,
    /// `isolation.network`.
    pub network: NetworkIsolation,
}

impl IsolationSettings {
    /// The `host:port` entries a task for `repo` may reach in deny mode: the repository's git
    /// host on 443, the GitHub API of `github_app` when the repository is on its git host (`gh`
    /// uses the task's token there), the LLM gateway (when the LLM contract uses one) and
    /// `network.allow`.
    pub fn egress_allow_list(
        &self,
        repo: Option<&str>,
        llm: Option<&LlmConnection>,
    ) -> Vec<HostPort> {
        // Only an operator-allowed git host (the submitter does not choose what to reach).
        let repo = repo
            .and_then(repo_host)
            .filter(|host| self.git_host_allowed(host))
            .map(|host| HostPort::new(&host, GIT_HTTPS_PORT));
        let api = self.github_app.as_ref().and_then(|app| {
            repo.as_ref()
                .filter(|git| git.host == app.git_host)
                .and_then(|_| url_host_port(&app.api_url))
        });
        let gateway = llm
            .and_then(|llm| llm.gateway.as_ref())
            .and_then(|gateway| url_host_port(&gateway.url));
        let mut out: Vec<HostPort> = Vec::new();
        for entry in repo
            .into_iter()
            .chain(api)
            .chain(gateway)
            .chain(self.network.allow.iter().cloned())
        {
            if !out.contains(&entry) {
                out.push(entry);
            }
        }
        out
    }
}

impl IsolationSettings {
    /// The git hosts a task's repository may use: `network.git_hosts` plus `github_app.git_host`
    /// (both operator-configured).
    pub fn allowed_git_hosts(&self) -> Vec<String> {
        let mut hosts = self.network.git_hosts.clone();
        if let Some(app) = &self.github_app
            && !hosts.contains(&app.git_host)
        {
            hosts.push(app.git_host.clone());
        }
        hosts
    }

    /// `true` when `host` (already lowercased) is an allowed git host.
    pub fn git_host_allowed(&self, host: &str) -> bool {
        self.allowed_git_hosts().iter().any(|h| h == host)
    }

    /// `Ok` unless `repo`'s host is one the operator did not allow, in deny mode (allow mode
    /// does not restrict egress, so it does not restrict the repo host either). `Err` names the
    /// offending host (or the whole `repo` when no host parses).
    pub fn check_repo_host(&self, repo: Option<&str>) -> Result<(), String> {
        if !self.network.denies() {
            return Ok(());
        }
        let Some(repo) = repo else { return Ok(()) };
        match repo_host(repo) {
            Some(host) if self.git_host_allowed(&host) => Ok(()),
            Some(host) => Err(host),
            None => Err(repo.to_owned()),
        }
    }
}

/// `isolation.task_uids`: the uid range tasks run as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaskUids {
    /// First uid (and gid) of the range (`> 0`).
    pub first: u32,
    /// Number of uids in the range (`> 0`; `first + count - 1` stays below `u32::MAX`).
    pub count: u32,
}

impl TaskUids {
    /// The uid (= gid) of task `task_id`: `first + (task_id mod count)`.
    pub fn for_task(&self, task_id: i64) -> u32 {
        let offset = task_id.rem_euclid(i64::from(self.count.max(1)));
        // `0 <= offset < count`, and casting checked that `first + count - 1` fits.
        self.first
            .saturating_add(u32::try_from(offset).unwrap_or(0))
    }
}

/// `isolation.github_app`: the GitHub App whose installation tokens tasks get.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitHubAppSettings {
    /// The App id (or client id), sent as the JWT `iss`.
    pub app_id: String,
    /// PEM private key of the App (PKCS#1 or PKCS#8), an absolute path; read per task.
    pub private_key_file: PathBuf,
    /// REST API root without a trailing `/` (default [`DEFAULT_GITHUB_API_URL`]; `https`, or
    /// `http` to a loopback host).
    pub api_url: String,
    /// Git host whose repositories get tokens, lowercased (default [`DEFAULT_GIT_HOST`]).
    pub git_host: String,
}

/// `isolation.network.mode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum NetworkMode {
    /// Only the allow-list is reachable (the default).
    #[default]
    Deny,
    /// No network restriction beyond the backend's own settings.
    Allow,
}

impl NetworkMode {
    /// The config spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Deny => "deny",
            Self::Allow => "allow",
        }
    }
}

/// `isolation.network`.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct NetworkIsolation {
    /// `deny` (default) or `allow`.
    pub mode: NetworkMode,
    /// Extra reachable `host:port` entries in deny mode.
    pub allow: Vec<HostPort>,
    /// Git hosts a task's repository may use (lowercased). The submitter picks `repo`, so in
    /// deny mode the host must be one the operator listed here (or `github_app.git_host`);
    /// otherwise the task cannot pick a host to reach (F4).
    pub git_hosts: Vec<String>,
}

impl NetworkIsolation {
    /// `true` in deny mode.
    pub fn denies(&self) -> bool {
        self.mode == NetworkMode::Deny
    }
}

/// One allow-list entry: an exact host (lowercased; no wildcards) and a port.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HostPort {
    /// Host name or IP literal (IPv6 without brackets), lowercased.
    pub host: String,
    /// Port (`> 0`).
    pub port: u16,
}

impl HostPort {
    /// `host:port` with `host` lowercased (brackets around an IPv6 literal removed).
    pub fn new(host: &str, port: u16) -> Self {
        let host = host
            .trim()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_ascii_lowercase();
        Self { host, port }
    }

    /// Parses `host:port` or `[v6]:port`; `None` for anything else (paths, userinfo, port 0).
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        let (host, port) = match value.strip_prefix('[') {
            Some(rest) => rest.split_once("]:")?,
            None => {
                let (host, port) = value.rsplit_once(':')?;
                if host.contains(':') {
                    return None;
                }
                (host, port)
            }
        };
        if host.is_empty()
            || host
                .chars()
                .any(|c| c.is_whitespace() || matches!(c, '/' | '@' | '[' | ']' | '?' | '#'))
            || !port.chars().all(|c| c.is_ascii_digit())
        {
            return None;
        }
        let port = port.parse::<u16>().ok().filter(|p| *p > 0)?;
        Some(Self::new(host, port))
    }

    /// `true` when `host` (any case, brackets optional) and `port` name this entry.
    pub fn matches(&self, host: &str, port: u16) -> bool {
        self.port == port && self == &Self::new(host, port)
    }
}

impl fmt::Display for HostPort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.host.contains(':') {
            write!(f, "[{}]:{}", self.host, self.port)
        } else {
            write!(f, "{}:{}", self.host, self.port)
        }
    }
}

/// `host[:port]` of an `http://` / `https://` URL (default ports 80 / 443).
pub fn url_host_port(url: &str) -> Option<HostPort> {
    let (scheme, rest) = url.split_once("://")?;
    let default_port = if scheme.eq_ignore_ascii_case("https") {
        443
    } else if scheme.eq_ignore_ascii_case("http") {
        80
    } else {
        return None;
    };
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    if authority.is_empty() {
        return None;
    }
    let has_port = match authority.strip_prefix('[') {
        Some(inner) => inner.contains("]:"),
        None => authority.contains(':'),
    };
    if has_port {
        HostPort::parse(authority)
    } else {
        Some(HostPort::new(authority, default_port))
    }
}

/// The host of a task repository URL (`https://host/...`, `ssh://[user@]host[:port]/...` or
/// scp-like `user@host:path`), lowercased.
pub fn repo_host(repo: &str) -> Option<String> {
    let repo = repo.trim();
    let host = if let Some((_, rest)) = repo.split_once("://") {
        let authority = rest.split('/').next()?;
        let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        match authority.strip_prefix('[') {
            Some(inner) => inner.split(']').next()?,
            None => authority.split(':').next()?,
        }
    } else {
        let (user_host, _) = repo.split_once(':')?;
        user_host.rsplit_once('@').map_or(user_host, |(_, h)| h)
    };
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

fn repo_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// `(owner, repo)` of `repo` when it is `https://<git_host>/OWNER/REPO[.git]` or
/// `git@<git_host>:OWNER/REPO[.git]` (host compared case-insensitively).
pub fn github_repo(repo: &str, git_host: &str) -> Option<(String, String)> {
    let repo = repo.trim();
    let path = if let Some(rest) = repo.strip_prefix("https://") {
        let (authority, path) = rest.split_once('/')?;
        if !authority.eq_ignore_ascii_case(git_host) {
            return None;
        }
        path
    } else {
        let rest = repo.strip_prefix("git@")?;
        let (host, path) = rest.split_once(':')?;
        if !host.eq_ignore_ascii_case(git_host) {
            return None;
        }
        path
    };
    let path = path.strip_suffix('/').unwrap_or(path);
    let (owner, name) = path.split_once('/')?;
    let name = name.strip_suffix(".git").unwrap_or(name);
    (repo_segment(owner) && repo_segment(name)).then(|| (owner.to_owned(), name.to_owned()))
}

/// `https://...`, or `http://` to a loopback host (local mocks and tests).
fn api_url_ok(url: &str) -> bool {
    if is_https_url(url, true) {
        return true;
    }
    url.get(..7)
        .is_some_and(|s| s.eq_ignore_ascii_case("http://"))
        && !url.contains(['?', '#'])
        && url_host_port(url)
            .is_some_and(|hp| matches!(hp.host.as_str(), "127.0.0.1" | "localhost" | "::1"))
}

fn valid_hostname(host: &str) -> bool {
    !host.is_empty()
        && !host.starts_with(['.', '-'])
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
}

/// Casts `isolation:`; `None` when the section is absent.
pub(crate) fn cast_isolation(
    root: &serde_json::Map<String, serde_json::Value>,
    errors: &mut Errors,
) -> Option<IsolationSettings> {
    let section = Section::new(root, "isolation", errors);
    if !section.is_present() {
        return None;
    }
    let task_uids = cast_task_uids(
        &section.nested("task_uids", "isolation.task_uids", errors),
        errors,
    );
    let github_app = cast_github_app(
        &section.nested("github_app", "isolation.github_app", errors),
        errors,
    );
    let network = cast_network(
        &section.nested("network", "isolation.network", errors),
        errors,
    );
    // The App key mints tokens for every repository of the App: only a task uid that cannot
    // read it keeps it from the task's agents.
    if github_app.is_some() && task_uids.is_none() {
        errors.push(
            "isolation.github_app",
            "needs isolation.task_uids (an agent running as Symphony's user could read the App key)",
        );
    }
    Some(IsolationSettings {
        task_uids,
        github_app,
        network,
    })
}

fn required_u32(s: &Section<'_>, field: &str, errors: &mut Errors) -> Option<u32> {
    if s.raw(field).is_none() {
        errors.push(&s.path(field), "can't be blank");
        return None;
    }
    let value = s.integer(field, errors);
    let value = s.positive(field, value, errors);
    s.fit::<u32>(field, value, errors)
}

fn cast_task_uids(s: &Section<'_>, errors: &mut Errors) -> Option<TaskUids> {
    if !s.is_present() {
        return None;
    }
    let first = required_u32(s, "first", errors);
    let count = required_u32(s, "count", errors);
    let (first, count) = (first?, count?);
    if u64::from(first) + u64::from(count) > u64::from(u32::MAX) {
        errors.push(&s.path("count"), "is too large for first");
        return None;
    }
    Some(TaskUids { first, count })
}

fn cast_github_app(s: &Section<'_>, errors: &mut Errors) -> Option<GitHubAppSettings> {
    if !s.is_present() {
        return None;
    }
    let before = errors.len();
    let app_id = match s.raw("app_id") {
        None => {
            errors.push(&s.path("app_id"), "can't be blank");
            None
        }
        Some(serde_json::Value::Number(n)) => match n.as_u64().filter(|n| *n > 0) {
            Some(id) => Some(id.to_string()),
            None => {
                errors.push(&s.path("app_id"), "is invalid");
                None
            }
        },
        Some(serde_json::Value::String(id)) if !id.trim().is_empty() => Some(id.trim().to_owned()),
        Some(_) => {
            errors.push(&s.path("app_id"), "is invalid");
            None
        }
    };
    let private_key_file = match s.string("private_key_file", errors) {
        None if s.raw("private_key_file").is_none() => {
            errors.push(&s.path("private_key_file"), "can't be blank");
            None
        }
        None => None,
        Some(path) if path.trim().is_empty() => {
            errors.push(&s.path("private_key_file"), "can't be blank");
            None
        }
        Some(path) if !path.trim().starts_with('/') => {
            errors.push(&s.path("private_key_file"), "must be an absolute path");
            None
        }
        Some(path) => Some(PathBuf::from(path.trim())),
    };
    let api_url = match s.string("api_url", errors) {
        None => DEFAULT_GITHUB_API_URL.to_owned(),
        Some(url) if api_url_ok(url.trim()) => url.trim().trim_end_matches('/').to_owned(),
        Some(_) => {
            errors.push(&s.path("api_url"), "is invalid");
            String::new()
        }
    };
    let git_host = match s.string("git_host", errors) {
        None => DEFAULT_GIT_HOST.to_owned(),
        Some(host) if valid_hostname(host.trim()) => host.trim().to_ascii_lowercase(),
        Some(_) => {
            errors.push(&s.path("git_host"), "is invalid");
            String::new()
        }
    };
    if errors.len() != before {
        return None;
    }
    Some(GitHubAppSettings {
        app_id: app_id?,
        private_key_file: private_key_file?,
        api_url,
        git_host,
    })
}

fn cast_network(s: &Section<'_>, errors: &mut Errors) -> NetworkIsolation {
    let mode = match s.string("mode", errors).as_deref().map(str::trim) {
        None | Some("deny") => NetworkMode::Deny,
        Some("allow") => NetworkMode::Allow,
        Some(_) => {
            errors.push(&s.path("mode"), "is invalid");
            NetworkMode::Deny
        }
    };
    let allow = match s.string_list("allow", errors) {
        None => Vec::new(),
        Some(entries) => {
            let parsed: Option<Vec<HostPort>> =
                entries.iter().map(|entry| HostPort::parse(entry)).collect();
            match parsed {
                Some(mut list) => {
                    list.dedup();
                    list
                }
                None => {
                    errors.push(&s.path("allow"), "is invalid");
                    Vec::new()
                }
            }
        }
    };
    let git_hosts = match s.string_list("git_hosts", errors) {
        None => Vec::new(),
        Some(entries) => entries
            .iter()
            .filter_map(|entry| {
                let host = entry.trim().to_ascii_lowercase();
                if host.is_empty() || host.contains(['/', ' ', '@', ':']) {
                    errors.push(&s.path("git_hosts"), "is invalid");
                    None
                } else {
                    Some(host)
                }
            })
            .collect(),
    };
    NetworkIsolation {
        mode,
        allow,
        git_hosts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_ports_parse_exact_entries_only() {
        assert_eq!(
            HostPort::parse("PyPI.org:443"),
            Some(HostPort::new("pypi.org", 443))
        );
        assert_eq!(
            HostPort::parse("[::1]:8080"),
            Some(HostPort::new("::1", 8080))
        );
        assert_eq!(HostPort::new("::1", 8080).to_string(), "[::1]:8080");
        for bad in [
            "pypi.org",
            "pypi.org:0",
            "pypi.org:x",
            "https://pypi.org:443",
            "u@pypi.org:443",
            "::1:80",
            ":443",
            "a b:1",
        ] {
            assert_eq!(HostPort::parse(bad), None, "{bad}");
        }
        assert!(HostPort::new("github.com", 443).matches("GitHub.COM", 443));
        assert!(!HostPort::new("github.com", 443).matches("github.com", 22));
        assert!(!HostPort::new("github.com", 443).matches("api.github.com", 443));
    }

    #[test]
    fn repositories_are_recognized_by_host() {
        assert_eq!(
            github_repo("https://GitHub.com/acme/widgets.git", "github.com"),
            Some(("acme".into(), "widgets".into()))
        );
        assert_eq!(
            github_repo("git@github.com:acme/widgets", "github.com"),
            Some(("acme".into(), "widgets".into()))
        );
        assert_eq!(
            github_repo("https://gitlab.com/acme/widgets", "github.com"),
            None
        );
        assert_eq!(
            github_repo("https://github.com/acme/widgets/tree/main", "github.com"),
            None
        );
        assert_eq!(
            github_repo("https://github.com/acme/..", "github.com"),
            None
        );
        assert_eq!(
            repo_host("ssh://git@Git.Example:2222/a/b.git").as_deref(),
            Some("git.example")
        );
        assert_eq!(
            repo_host("git@github.com:a/b.git").as_deref(),
            Some("github.com")
        );
        assert_eq!(
            repo_host("https://github.com/a/b").as_deref(),
            Some("github.com")
        );
        assert_eq!(
            url_host_port("http://gateway.internal:8080/v1"),
            Some(HostPort::new("gateway.internal", 8080))
        );
        assert_eq!(
            url_host_port("https://gw.example"),
            Some(HostPort::new("gw.example", 443))
        );
        assert_eq!(url_host_port("ftp://x"), None);
    }

    #[test]
    fn task_uids_wrap_around_the_range() {
        let uids = TaskUids {
            first: 200_000,
            count: 10,
        };
        assert_eq!(uids.for_task(0), 200_000);
        assert_eq!(uids.for_task(13), 200_003);
        assert_eq!(uids.for_task(-1), 200_009);
    }
}
