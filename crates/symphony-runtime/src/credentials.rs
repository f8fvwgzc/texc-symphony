//! Short-lived per-task repository credentials: GitHub App installation tokens
//! (`isolation.github_app`).
//!
//! For a task whose repository lives on `git_host`, [`mint_task_credentials`]:
//!
//! 1. signs an App JWT ([`app_jwt`]: RS256, `iat` = now − 60 s, `exp` = now + 9 min, `iss` = the
//!    App id) with the App's PEM private key (PKCS#1 `RSA PRIVATE KEY` or PKCS#8 `PRIVATE KEY`),
//!    read per task so a rotated Secret applies to the next task;
//! 2. finds the repository's installation (`GET {api_url}/repos/{owner}/{repo}/installation`);
//! 3. creates an installation token for that one repository with `contents: write`,
//!    `pull_requests: write` and `metadata: read`
//!    (`POST {api_url}/app/installations/{id}/access_tokens`).
//!
//! The token (GitHub gives it one hour; it is not refreshed) reaches git and the agent only
//! through the environment ([`TaskCredentials::env`]): a credential helper for `https://<git_host>`
//! prints `$SYMPHONY_GIT_TOKEN`, and `GH_TOKEN` serves `gh`. It never appears in argv, and errors
//! never contain the token or the JWT.

use std::time::Duration;

use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use symphony_core::config::GitHubAppSettings;
use symphony_core::config::isolation::github_repo;

/// How far `iat` is backdated (clock drift allowance GitHub recommends).
pub const JWT_BACKDATE_SECS: i64 = 60;
/// How long after now the JWT expires (GitHub allows at most 10 minutes).
pub const JWT_LIFETIME_SECS: i64 = 9 * 60;
/// Bound on each GitHub API request.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Variable the credential helper reads the token from.
pub const TOKEN_ENV: &str = "SYMPHONY_GIT_TOKEN";
/// The git credential helper (shell code run by git; the token comes from [`TOKEN_ENV`]).
pub const CREDENTIAL_HELPER: &str =
    "!f() { echo username=x-access-token; echo password=$SYMPHONY_GIT_TOKEN; }; f";
/// `X-GitHub-Api-Version` sent with every request.
pub const GITHUB_API_VERSION: &str = "2022-11-28";
/// Bytes of an error response quoted in [`CredentialError::Status`].
const ERROR_EXCERPT_CHARS: usize = 300;

/// Why task credentials could not be created. Never contains the token or the JWT.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CredentialError {
    /// The private key file could not be read.
    #[error("github_app_key_unreadable: {0}")]
    KeyUnreadable(String),
    /// The private key is not an RSA key in PEM (PKCS#1 or PKCS#8).
    #[error("github_app_key_invalid: {0}")]
    KeyInvalid(String),
    /// Signing the JWT failed.
    #[error("github_app_signing_failed")]
    Signing,
    /// A request to the GitHub API failed before a response arrived.
    #[error("github_app_request_failed: {step}: {reason}")]
    Request {
        /// `installation` or `access_tokens`.
        step: &'static str,
        /// The transport error.
        reason: String,
    },
    /// The GitHub API answered with an error status.
    #[error("github_app_http_status: {step} returned HTTP {status}: {body}")]
    Status {
        /// `installation` or `access_tokens`.
        step: &'static str,
        /// HTTP status.
        status: u16,
        /// Start of the response body.
        body: String,
    },
    /// The GitHub API answered with an unexpected body.
    #[error("github_app_invalid_response: {step}: {reason}")]
    InvalidResponse {
        /// `installation` or `access_tokens`.
        step: &'static str,
        /// What was wrong.
        reason: String,
    },
}

/// A task's repository token and where it applies.
#[derive(Clone, PartialEq, Eq)]
pub struct TaskCredentials {
    token: String,
    git_host: String,
    owner: String,
    repo: String,
}

impl std::fmt::Debug for TaskCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskCredentials")
            .field("token", &"<redacted>")
            .field("git_host", &self.git_host)
            .field("owner", &self.owner)
            .field("repo", &self.repo)
            .finish()
    }
}

impl TaskCredentials {
    /// Credentials from parts (tests, embedders).
    pub fn new(token: String, git_host: &str, owner: &str, repo: &str) -> Self {
        Self {
            token,
            git_host: git_host.to_ascii_lowercase(),
            owner: owner.to_owned(),
            repo: repo.to_owned(),
        }
    }

    /// The installation token (for redaction; never log it).
    pub fn token(&self) -> &str {
        &self.token
    }

    /// The git host the token is for.
    pub fn git_host(&self) -> &str {
        &self.git_host
    }

    /// `https://<git_host>/<owner>/<repo>.git`: the clone URL (no credentials in it; an ssh
    /// repository URL is cloned over https so the token applies and port 22 is never needed).
    pub fn clone_url(&self) -> String {
        format!("https://{}/{}/{}.git", self.git_host, self.owner, self.repo)
    }

    /// The environment that lets git and `gh` use the token: a credential helper for
    /// `https://<git_host>` (configured through `GIT_CONFIG_*`, after resetting any helper the
    /// system or user configuration names), [`TOKEN_ENV`] and `GH_TOKEN`.
    pub fn env(&self) -> Vec<(String, String)> {
        let pairs = [
            ("GIT_CONFIG_COUNT", "2".to_owned()),
            ("GIT_CONFIG_KEY_0", "credential.helper".to_owned()),
            ("GIT_CONFIG_VALUE_0", String::new()),
            (
                "GIT_CONFIG_KEY_1",
                format!("credential.https://{}.helper", self.git_host),
            ),
            ("GIT_CONFIG_VALUE_1", CREDENTIAL_HELPER.to_owned()),
            (TOKEN_ENV, self.token.clone()),
            ("GH_TOKEN", self.token.clone()),
        ];
        pairs
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value))
            .collect()
    }
}

/// The DER body of the first PEM block in `pem` and its label (`RSA PRIVATE KEY`, ...).
fn pem_der(pem: &str) -> Result<(String, Vec<u8>), CredentialError> {
    let invalid = |why: &str| CredentialError::KeyInvalid(why.to_owned());
    let start = pem
        .find("-----BEGIN ")
        .ok_or_else(|| invalid("no PEM block"))?;
    let rest = &pem[start + "-----BEGIN ".len()..];
    let (label, rest) = rest
        .split_once("-----")
        .ok_or_else(|| invalid("malformed PEM header"))?;
    let end_marker = format!("-----END {label}-----");
    let body = rest
        .split_once(&end_marker)
        .map(|(body, _)| body)
        .ok_or_else(|| invalid("no PEM end line"))?;
    let base64: String = body.chars().filter(|c| !c.is_whitespace()).collect();
    let der = STANDARD
        .decode(base64)
        .map_err(|_| invalid("the PEM body is not base64"))?;
    Ok((label.to_owned(), der))
}

/// The RSA key pair in `pem` (PKCS#1 `RSA PRIVATE KEY` or PKCS#8 `PRIVATE KEY`).
pub fn parse_private_key(pem: &str) -> Result<RsaKeyPair, CredentialError> {
    let (label, der) = pem_der(pem)?;
    let parsed = match label.as_str() {
        "RSA PRIVATE KEY" => RsaKeyPair::from_der(&der),
        "PRIVATE KEY" => RsaKeyPair::from_pkcs8(&der),
        other => {
            return Err(CredentialError::KeyInvalid(format!(
                "unsupported PEM block `{other}` (expected RSA PRIVATE KEY or PRIVATE KEY)"
            )));
        }
    };
    parsed.map_err(|err| CredentialError::KeyInvalid(format!("not a usable RSA key: {err}")))
}

/// An App JWT for `app_id` signed with `key`, issued at `now` (Unix seconds).
pub fn app_jwt(key: &RsaKeyPair, app_id: &str, now: i64) -> Result<String, CredentialError> {
    let header = URL_SAFE_NO_PAD.encode(json!({"alg": "RS256", "typ": "JWT"}).to_string());
    let claims = URL_SAFE_NO_PAD.encode(
        json!({
            "iat": now - JWT_BACKDATE_SECS,
            "exp": now + JWT_LIFETIME_SECS,
            "iss": app_id,
        })
        .to_string(),
    );
    let signing_input = format!("{header}.{claims}");
    let mut signature = vec![0u8; key.public_modulus_len()];
    key.sign(
        &RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        signing_input.as_bytes(),
        &mut signature,
    )
    .map_err(|_| CredentialError::Signing)?;
    Ok(format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature)
    ))
}

fn scrub(text: &str, secrets: &[&str]) -> String {
    let mut out = text.to_owned();
    for secret in secrets.iter().filter(|s| !s.is_empty()) {
        out = out.replace(secret, "[REDACTED]");
    }
    out.chars().take(ERROR_EXCERPT_CHARS).collect()
}

/// Sends `request` and returns the JSON body of a success response.
async fn call(
    request: reqwest::RequestBuilder,
    step: &'static str,
    jwt: &str,
) -> Result<Value, CredentialError> {
    let response = request
        .header("Authorization", format!("Bearer {jwt}"))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
        .header("User-Agent", "symphony")
        .send()
        .await
        .map_err(|err| CredentialError::Request {
            step,
            reason: scrub(&err.to_string(), &[jwt]),
        })?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|err| CredentialError::Request {
            step,
            reason: scrub(&err.to_string(), &[jwt]),
        })?;
    if !status.is_success() {
        return Err(CredentialError::Status {
            step,
            status: status.as_u16(),
            body: scrub(text.trim(), &[jwt]),
        });
    }
    serde_json::from_str(&text).map_err(|_| CredentialError::InvalidResponse {
        step,
        reason: "the body is not JSON".into(),
    })
}

/// Mints the credentials of a task for `repo`: `Ok(None)` when `repo` is not
/// `https://<git_host>/OWNER/REPO[.git]` or `git@<git_host>:OWNER/REPO[.git]`.
pub async fn mint_task_credentials(
    app: &GitHubAppSettings,
    repo: &str,
) -> Result<Option<TaskCredentials>, CredentialError> {
    let Some((owner, name)) = github_repo(repo, &app.git_host) else {
        return Ok(None);
    };
    let pem = tokio::fs::read_to_string(&app.private_key_file)
        .await
        .map_err(|err| {
            CredentialError::KeyUnreadable(format!("{}: {err}", app.private_key_file.display()))
        })?;
    let key = parse_private_key(&pem)?;
    let jwt = app_jwt(&key, &app.app_id, chrono::Utc::now().timestamp())?;
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|err| CredentialError::Request {
            step: "installation",
            reason: err.to_string(),
        })?;
    let api = app.api_url.trim_end_matches('/');

    let installation = call(
        client.get(format!("{api}/repos/{owner}/{name}/installation")),
        "installation",
        &jwt,
    )
    .await?;
    let id = installation
        .get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| CredentialError::InvalidResponse {
            step: "installation",
            reason: "no numeric `id`".into(),
        })?;

    let body = json!({
        "repositories": [name],
        "permissions": {"contents": "write", "pull_requests": "write", "metadata": "read"},
    });
    let created = call(
        client
            .post(format!("{api}/app/installations/{id}/access_tokens"))
            .json(&body),
        "access_tokens",
        &jwt,
    )
    .await?;
    let token = created
        .get("token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| CredentialError::InvalidResponse {
            step: "access_tokens",
            reason: "no `token`".into(),
        })?;
    tracing::info!(
        "Minted a repository token for the task git_host={} repo={owner}/{name} installation_id={id}",
        app.git_host
    );
    Ok(Some(TaskCredentials::new(
        token.to_owned(),
        &app.git_host,
        &owner,
        &name,
    )))
}
