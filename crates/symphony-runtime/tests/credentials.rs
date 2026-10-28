//! GitHub App task credentials: JWT signing (verified with the public key), the token flow
//! against a mock API, typed failures, and the git credential helper environment.

use std::path::{Path, PathBuf};
use std::process::Command;

use aws_lc_rs::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use symphony_core::config::GitHubAppSettings;
use symphony_runtime::credentials::{
    CredentialError, JWT_BACKDATE_SECS, JWT_LIFETIME_SECS, TaskCredentials, app_jwt,
    mint_task_credentials, parse_private_key,
};
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const TOKEN: &str = "ghs_TaskToken0123456789abcdefghijklmnopq";

fn openssl(args: &[&str]) -> bool {
    Command::new("openssl")
        .args(args)
        .output()
        .is_ok_and(|out| out.status.success())
}

/// A fresh RSA key: (PKCS#1 PEM, PKCS#8 PEM, SubjectPublicKeyInfo DER), or `None` without openssl.
fn generate_key(dir: &Path) -> Option<(PathBuf, PathBuf, Vec<u8>)> {
    let generated = dir.join("generated.pem");
    let pkcs1 = dir.join("pkcs1.pem");
    let pkcs8 = dir.join("pkcs8.pem");
    let public = dir.join("public.der");
    let path = |p: &Path| p.to_string_lossy().into_owned();
    if !openssl(&["genrsa", "-out", &path(&generated), "2048"]) {
        eprintln!("skipped: openssl is not available");
        return None;
    }
    // OpenSSL 3 writes PKCS#8 by default, LibreSSL PKCS#1: derive both forms explicitly.
    let traditional = openssl(&[
        "rsa",
        "-in",
        &path(&generated),
        "-traditional",
        "-out",
        &path(&pkcs1),
    ]) || openssl(&["rsa", "-in", &path(&generated), "-out", &path(&pkcs1)]);
    assert!(traditional, "openssl rsa failed");
    assert!(openssl(&[
        "pkcs8",
        "-topk8",
        "-nocrypt",
        "-in",
        &path(&generated),
        "-out",
        &path(&pkcs8),
    ]));
    assert!(openssl(&[
        "rsa",
        "-in",
        &path(&generated),
        "-pubout",
        "-outform",
        "DER",
        "-out",
        &path(&public),
    ]));
    let pkcs1_text = std::fs::read_to_string(&pkcs1).unwrap();
    assert!(pkcs1_text.contains("BEGIN RSA PRIVATE KEY"), "{pkcs1_text}");
    assert!(
        std::fs::read_to_string(&pkcs8)
            .unwrap()
            .contains("BEGIN PRIVATE KEY")
    );
    Some((pkcs1, pkcs8, std::fs::read(public).unwrap()))
}

fn decode(part: &str) -> Value {
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(part).unwrap()).unwrap()
}

/// Verifies `jwt` with `public_der` and returns its claims.
fn verify(jwt: &str, public_der: &[u8]) -> Value {
    let parts: Vec<&str> = jwt.split('.').collect();
    assert_eq!(parts.len(), 3, "{jwt}");
    assert_eq!(decode(parts[0]), json!({"alg": "RS256", "typ": "JWT"}));
    let signature = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
    UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, public_der)
        .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
        .expect("valid RS256 signature");
    decode(parts[1])
}

#[test]
fn jwts_are_rs256_signed_with_both_key_formats() {
    let dir = tempfile::tempdir().unwrap();
    let Some((pkcs1, pkcs8, public)) = generate_key(dir.path()) else {
        return;
    };
    for pem in [pkcs1, pkcs8] {
        let key = parse_private_key(&std::fs::read_to_string(&pem).unwrap()).unwrap();
        let jwt = app_jwt(&key, "123456", 1_700_000_000).unwrap();
        let claims = verify(&jwt, &public);
        assert_eq!(
            claims,
            json!({
                "iat": 1_700_000_000 - JWT_BACKDATE_SECS,
                "exp": 1_700_000_000 + JWT_LIFETIME_SECS,
                "iss": "123456",
            })
        );
    }
}

#[test]
fn unusable_keys_are_typed_errors() {
    for (pem, fragment) in [
        ("not a key", "no PEM block"),
        (
            "-----BEGIN RSA PRIVATE KEY-----\n!!!\n-----END RSA PRIVATE KEY-----",
            "not base64",
        ),
        (
            "-----BEGIN RSA PRIVATE KEY-----\nAAAA\n-----END RSA PRIVATE KEY-----",
            "not a usable RSA key",
        ),
        (
            "-----BEGIN EC PRIVATE KEY-----\nAAAA\n-----END EC PRIVATE KEY-----",
            "unsupported PEM block",
        ),
    ] {
        match parse_private_key(pem) {
            Err(CredentialError::KeyInvalid(message)) => {
                assert!(message.contains(fragment), "{fragment}: {message}");
            }
            Err(other) => panic!("{fragment}: unexpected {other:?}"),
            Ok(_) => panic!("{fragment}: accepted"),
        }
    }
}

fn app(server: &MockServer, key: &Path) -> GitHubAppSettings {
    GitHubAppSettings {
        app_id: "4242".into(),
        private_key_file: key.to_path_buf(),
        api_url: server.uri(),
        git_host: "github.com".into(),
    }
}

/// The `Bearer <jwt>` of `request`, verified against `public`.
fn bearer_claims(request: &Request, public: &[u8]) -> Value {
    let auth = request
        .headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .expect("authorization header");
    let jwt = auth.strip_prefix("Bearer ").expect("bearer");
    verify(jwt, public)
}

#[tokio::test]
async fn a_scoped_installation_token_is_minted_for_the_task_repository() {
    let dir = tempfile::tempdir().unwrap();
    let Some((key, _, public)) = generate_key(dir.path()) else {
        return;
    };
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/installation"))
        .and(header("accept", "application/vnd.github+json"))
        .and(header("x-github-api-version", "2022-11-28"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 777})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/app/installations/777/access_tokens"))
        .and(body_json(json!({
            "repositories": ["widgets"],
            "permissions": {"contents": "write", "pull_requests": "write", "metadata": "read"},
        })))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(json!({"token": TOKEN, "expires_at": "2026-10-26T13:00:00Z"})),
        )
        .expect(1)
        .mount(&server)
        .await;

    let credentials = mint_task_credentials(&app(&server, &key), "git@github.com:acme/widgets.git")
        .await
        .unwrap()
        .expect("a github.com repository");
    assert_eq!(credentials.token(), TOKEN);
    assert_eq!(
        credentials.clone_url(),
        "https://github.com/acme/widgets.git"
    );
    assert!(!format!("{credentials:?}").contains(TOKEN));
    for request in server.received_requests().await.unwrap() {
        let claims = bearer_claims(&request, &public);
        assert_eq!(claims["iss"], json!("4242"));
        let lifetime = claims["exp"].as_i64().unwrap() - claims["iat"].as_i64().unwrap();
        assert_eq!(lifetime, JWT_BACKDATE_SECS + JWT_LIFETIME_SECS);
    }

    // Another host gets nothing (and no request is made).
    assert_eq!(
        mint_task_credentials(&app(&server, &key), "https://gitlab.com/acme/widgets")
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn api_failures_are_typed_and_never_carry_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let Some((key, _, _)) = generate_key(dir.path()) else {
        return;
    };
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/missing/installation"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"message": "Not Found"})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/denied/installation"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 9})))
        .mount(&server)
        .await;
    // An error page that echoes the credentials it was sent.
    Mock::given(method("POST"))
        .and(path("/app/installations/9/access_tokens"))
        .respond_with(|request: &Request| {
            let auth = request
                .headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            ResponseTemplate::new(403).set_body_string(format!("forbidden for {auth} {TOKEN}"))
        })
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/weird/installation"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "x"})))
        .mount(&server)
        .await;

    let err = mint_task_credentials(&app(&server, &key), "https://github.com/acme/missing")
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            CredentialError::Status {
                step: "installation",
                status: 404,
                ..
            }
        ),
        "{err:?}"
    );
    assert!(
        err.to_string().starts_with("github_app_http_status: "),
        "{err}"
    );

    let err = mint_task_credentials(&app(&server, &key), "https://github.com/acme/denied")
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            CredentialError::Status {
                step: "access_tokens",
                status: 403,
                ..
            }
        ),
        "{err:?}"
    );
    let text = err.to_string();
    assert!(!text.contains("eyJ"), "the JWT is scrubbed: {text}");
    assert!(text.contains("[REDACTED]"), "{text}");

    let err = mint_task_credentials(&app(&server, &key), "https://github.com/acme/weird")
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            CredentialError::InvalidResponse {
                step: "installation",
                ..
            }
        ),
        "{err:?}"
    );

    let mut missing_key = app(&server, &key);
    missing_key.private_key_file = dir.path().join("absent.pem");
    let err = mint_task_credentials(&missing_key, "https://github.com/acme/widgets")
        .await
        .unwrap_err();
    assert!(matches!(err, CredentialError::KeyUnreadable(_)), "{err:?}");

    let mut unreachable = app(&server, &key);
    unreachable.api_url = "http://127.0.0.1:9".into();
    let err = mint_task_credentials(&unreachable, "https://github.com/acme/widgets")
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            CredentialError::Request {
                step: "installation",
                ..
            }
        ),
        "{err:?}"
    );
    assert!(!err.to_string().contains("eyJ"), "{err}");
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// `git credential fill` for `host` with the credentials' environment, isolated from the
/// developer's own git configuration.
fn credential_fill(credentials: &TaskCredentials, host: &str, home: &Path) -> (bool, String) {
    use std::io::Write;
    let mut child = Command::new("git")
        .args(["credential", "fill"])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .envs(credentials.env())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("protocol=https\nhost={host}\n\n").as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

#[test]
fn the_credential_helper_answers_for_the_git_host_only() {
    if !git_available() {
        eprintln!("skipped: git is not installed");
        return;
    }
    let home = tempfile::tempdir().unwrap();
    // A helper the user's configuration names is reset, not consulted.
    std::fs::write(
        home.path().join(".gitconfig"),
        "[credential]\n\thelper = \"!f() { echo username=operator; echo password=operator-secret; }; f\"\n",
    )
    .unwrap();
    let credentials = TaskCredentials::new(TOKEN.into(), "github.com", "acme", "widgets");
    let (ok, out) = credential_fill(&credentials, "github.com", home.path());
    assert!(ok, "{out}");
    assert!(out.contains("username=x-access-token\n"), "{out}");
    assert!(out.contains(&format!("password={TOKEN}\n")), "{out}");

    let (ok, out) = credential_fill(&credentials, "evil.example", home.path());
    assert!(!ok, "no credentials for another host: {out}");
    assert!(!out.contains(TOKEN), "{out}");
    assert!(!out.contains("operator-secret"), "{out}");

    // The token travels in the environment only; the configuration names the variable.
    let env = credentials.env();
    let config: Vec<&(String, String)> = env
        .iter()
        .filter(|(name, _)| name.starts_with("GIT_CONFIG_"))
        .collect();
    assert!(config.iter().all(|(_, value)| !value.contains(TOKEN)));
    assert!(env.contains(&("GH_TOKEN".to_owned(), TOKEN.to_owned())));
}
