//! Tutela-verified callers (Tutela's `docs/INTEGRATION.md` §2): tokens for exactly `aud=symphony`
//! from a trusted issuer and tenant, checked by `tutela-sdk`'s axum layer; the legacy API token
//! during its window; and the local single-user mode on loopback.

mod common;

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair as _};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use common::{Harness, static_snapshot};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use symphony_core::config::{TutelaIssuer, TutelaSettings};
use symphony_server::testing::StaticControlPlane;
use symphony_server::{Auth, ServerConfig, serve};
use symphony_store::Store;
use tokio_util::sync::CancellationToken;
use tutela_sdk::JwksSource;

/// A Tutela issuer (Tutela-format access and delegated tokens).
const ISS: &str = "https://auth.example.test";
const TID: &str = "ten_0193a1b2-c3d4-7e5f-8a6b-7c8d9e0f1a2b";
/// An external issuer (Keycloak): plain OIDC access tokens, its tenant from the trust list.
const KEYCLOAK: &str = "https://kc.example.test/realms/acme";
const KEYCLOAK_TID: &str = "ten_0193a1b2-c3d4-7e5f-8a6b-7c8d9e0f1a2c";
const USER: &str = "user:usr_0193a1b2-c3d4-7a11-8b22-3c4d5e6f7081";
const AGENT: &str = "agent:agt_0193a1b2-c3d4-7b33-9c44-5d6e7f809102";
const LEGACY: &str = "legacy-token-0123456789";

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// An ES256 key of one issuer.
struct Signer(EcdsaKeyPair);

impl Signer {
    fn new() -> Self {
        Self(EcdsaKeyPair::generate(&ECDSA_P256_SHA256_FIXED_SIGNING).unwrap())
    }

    fn xy(&self) -> (String, String) {
        let point = self.0.public_key().as_ref();
        (B64.encode(&point[1..33]), B64.encode(&point[33..65]))
    }

    /// The RFC 7638 thumbprint (a Tutela issuer's `kid`).
    fn kid(&self) -> String {
        let (x, y) = self.xy();
        let canonical = format!(r#"{{"crv":"P-256","kty":"EC","x":"{x}","y":"{y}"}}"#);
        B64.encode(Sha256::digest(canonical.as_bytes()))
    }

    fn jwk(&self) -> Value {
        let (x, y) = self.xy();
        json!({"kty": "EC", "crv": "P-256", "x": x, "y": y, "kid": self.kid(), "use": "sig"})
    }

    fn token(&self, typ: &str, claims: &Value) -> String {
        let header = json!({"typ": typ, "alg": "ES256", "kid": self.kid()});
        let signed = format!(
            "{}.{}",
            B64.encode(serde_json::to_vec(&header).unwrap()),
            B64.encode(serde_json::to_vec(claims).unwrap())
        );
        let signature = self
            .0
            .sign(&SystemRandom::new(), signed.as_bytes())
            .unwrap();
        format!("{signed}.{}", B64.encode(signature.as_ref()))
    }
}

/// Serves each issuer's JWKS from memory, by URL.
struct Keys(Vec<(String, Vec<u8>)>);

impl JwksSource for Keys {
    fn fetch(
        &self,
        url: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<Vec<u8>>> + Send>> {
        let body = self
            .0
            .iter()
            .find(|(at, _)| *at == url)
            .map(|(_, body)| body.clone());
        Box::pin(async move { body })
    }
}

fn jwks_uri(iss: &str) -> String {
    format!("{iss}/jwks.json")
}

/// Settings trusting both issuers, as WORKFLOW.md's `server.tutela` would give them.
fn settings() -> TutelaSettings {
    let issuer = |iss: &str, tid: &str, external: bool| TutelaIssuer {
        iss: iss.into(),
        jwks_uri: jwks_uri(iss),
        tenants: vec![tid.into()],
        algs: vec!["ES256".into()],
        external,
        allow_typ_jwt: external,
    };
    TutelaSettings {
        issuers: vec![
            issuer(ISS, TID, false),
            issuer(KEYCLOAK, KEYCLOAK_TID, true),
        ],
    }
}

struct World {
    tutela: Signer,
    keycloak: Signer,
}

impl World {
    fn new() -> Self {
        Self {
            tutela: Signer::new(),
            keycloak: Signer::new(),
        }
    }

    fn auth(&self) -> Auth {
        let body = |signer: &Signer| serde_json::to_vec(&json!({"keys": [signer.jwk()]})).unwrap();
        let keys = Keys(vec![
            (jwks_uri(ISS), body(&self.tutela)),
            (jwks_uri(KEYCLOAK), body(&self.keycloak)),
        ]);
        Auth::tutela(symphony_server::tutela::verifier_with(&settings(), Arc::new(keys)).unwrap())
    }

    fn harness(&self, auth: Auth) -> Harness {
        Harness::with(
            StaticControlPlane::new(Ok(static_snapshot())),
            Store::open_in_memory().unwrap(),
            ServerConfig {
                auth,
                ..ServerConfig::default()
            },
        )
    }

    /// A Tutela access token (`at+jwt`) with `claims` changed by `edit`.
    fn access(&self, edit: impl FnOnce(&mut Value)) -> String {
        let now = now();
        let mut claims = json!({
            "iss": ISS, "sub": USER, "aud": "symphony", "azp": "nexc-engine",
            "exp": now + 300, "iat": now, "nbf": now,
            "jti": "0193a1b2-c3d4-7e99-8f00-a1b2c3d4e5f6",
            "sid": "ses_0193a1b2-c3d4-7123-8456-789abcdef012",
            "tid": TID, "epoch": 3, "rev": 42, "amr": ["pwd"]
        });
        edit(&mut claims);
        self.tutela.token("at+jwt", &claims)
    }

    /// A Keycloak access token (`typ: JWT`) with `claims` changed by `edit`.
    fn keycloak(&self, edit: impl FnOnce(&mut Value)) -> String {
        let now = now();
        let mut claims = json!({
            "iss": KEYCLOAK, "sub": "f81d4fae-7dec-11d0-a765-00a0c91e6bf6", "aud": "symphony",
            "azp": "nexc-engine", "exp": now + 300, "iat": now
        });
        edit(&mut claims);
        self.keycloak.token("JWT", &claims)
    }

    /// A delegated agent token (`agent+jwt`) for `aud=symphony`.
    fn agent(&self) -> String {
        let now = now();
        self.tutela.token(
            "agent+jwt",
            &json!({
                "iss": ISS, "sub": USER, "act": {"sub": AGENT},
                "cap": {"snapshot": "opaque"}, "run_id": "run-1", "aud": "symphony",
                "azp": "attache", "exp": now + 90, "iat": now, "nbf": now,
                "jti": "0193a1b2-c3d4-7e99-8f00-a1b2c3d4e5f7",
                "tid": TID, "epoch": 3, "rev": 42
            }),
        )
    }
}

async fn state(harness: &Harness, token: Option<&str>) -> StatusCode {
    let mut request = Request::builder().uri("/api/v1/state");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = harness.request(request.body(Body::empty()).unwrap()).await;
    let status = response.status();
    if status == StatusCode::UNAUTHORIZED {
        // Symphony's own envelope, whichever check refused.
        let body = axum::body::to_bytes(response.into_body(), 1 << 16)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["code"], "unauthorized");
    }
    status
}

#[tokio::test]
async fn tokens_for_aud_symphony_from_a_trusted_issuer_are_accepted() {
    let world = World::new();
    let harness = world.harness(world.auth());
    assert_eq!(
        state(&harness, Some(&world.access(|_| {}))).await,
        StatusCode::OK
    );
    assert_eq!(
        state(&harness, Some(&world.keycloak(|_| {}))).await,
        StatusCode::OK
    );
    // Health and the contract stay public.
    let response = harness
        .request(
            Request::builder()
                .uri("/api/v1/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn missing_and_wrong_tokens_are_refused() {
    let world = World::new();
    let harness = world.harness(world.auth());
    assert_eq!(
        state(&harness, None).await,
        StatusCode::UNAUTHORIZED,
        "no token"
    );
    let wrong_audience = [
        world.access(|c| c["aud"] = json!("nexc")),
        world.keycloak(|c| c["aud"] = json!("nexc")),
        // Exactly one audience: a token for symphony and another service is refused too.
        world.access(|c| c["aud"] = json!(["symphony", "nexc"])),
        world.keycloak(|c| c["aud"] = json!(["symphony", "account"])),
    ];
    for token in &wrong_audience {
        assert_eq!(state(&harness, Some(token)).await, StatusCode::UNAUTHORIZED);
    }
    let untrusted = [
        // A tenant the issuer may not speak for.
        world.access(|c| c["tid"] = json!("ten_0193a1b2-c3d4-7e5f-8a6b-7c8d9e0f1aff")),
        // An issuer that is not on the trust list.
        world.access(|c| c["iss"] = json!("https://evil.example.test")),
        // Expired.
        world.keycloak(|c| {
            c["iat"] = json!(now() - 900);
            c["exp"] = json!(now() - 600);
        }),
        // Signed by the other issuer's key.
        world.tutela.token(
            "JWT",
            &json!({"iss": KEYCLOAK, "sub": "x", "aud": "symphony",
            "exp": now() + 300, "iat": now()}),
        ),
        "not-a-jwt".to_owned(),
    ];
    for token in &untrusted {
        assert_eq!(state(&harness, Some(token)).await, StatusCode::UNAUTHORIZED);
    }
}

/// TODO(Tutela Phase B): delegated agent tokens are refused until symphony turns on delegation.
#[tokio::test]
async fn agent_tokens_are_refused_while_delegation_is_off() {
    let world = World::new();
    let harness = world.harness(world.auth());
    let agent = world.agent();
    assert_eq!(
        state(&harness, Some(&agent)).await,
        StatusCode::UNAUTHORIZED
    );
    // The same token passes once delegation is on: the refusal above is the delegation switch.
    let mut config = symphony_server::tutela::verifier_config(&settings()).unwrap();
    config.delegation = true;
    config.delegation_aware = true;
    let keys = Keys(vec![(
        jwks_uri(ISS),
        serde_json::to_vec(&json!({"keys": [world.tutela.jwk()]})).unwrap(),
    )]);
    let verifier =
        tutela_sdk::Verifier::with(config, Arc::new(keys), Arc::new(tutela_sdk::SystemClock));
    assert!(verifier.verify(&agent).await.is_ok());
}

#[tokio::test]
async fn the_legacy_token_works_only_inside_its_window() {
    let world = World::new();
    let both = world.harness(world.auth().with_legacy_token(LEGACY));
    assert_eq!(state(&both, Some(LEGACY)).await, StatusCode::OK);
    assert_eq!(
        state(&both, Some(&world.access(|_| {}))).await,
        StatusCode::OK
    );
    assert_eq!(state(&both, Some("wrong")).await, StatusCode::UNAUTHORIZED);
    // With the window closed (`legacy_api_token: false`), the old token is just a bad token.
    let tutela_only = world.harness(world.auth());
    assert_eq!(
        state(&tutela_only, Some(LEGACY)).await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn the_local_single_user_mode_stays_on_loopback() {
    let control = || Arc::new(StaticControlPlane::new(Ok(static_snapshot())));
    // No credentials: open, but only on a loopback bind.
    let open = World::new().harness(Auth::disabled());
    assert_eq!(state(&open, None).await, StatusCode::OK);
    let refused = serve(
        ServerConfig::new("0.0.0.0", 0),
        control(),
        Store::disabled(),
        CancellationToken::new(),
    )
    .await;
    assert!(refused.is_err(), "any other address needs tokens");
    // With Tutela tokens any address may be served.
    let world = World::new();
    let shutdown = CancellationToken::new();
    let bound = serve(
        ServerConfig {
            auth: world.auth(),
            ..ServerConfig::new("0.0.0.0", 0)
        },
        control(),
        Store::disabled(),
        shutdown.clone(),
    )
    .await
    .unwrap();
    assert_ne!(bound.port(), 0);
    shutdown.cancel();
}
