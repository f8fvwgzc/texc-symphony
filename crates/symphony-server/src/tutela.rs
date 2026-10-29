//! Tutela-verified callers (`server.tutela`, Tutela's `docs/INTEGRATION.md` §2): the HTTP API
//! accepts bearer tokens issued for exactly one audience, `symphony`, by a trusted issuer for a
//! tenant on the trust list. [`verifier`] turns the settings into a [`tutela_sdk::Verifier`] (JWKS
//! fetched through `tutela-safenet`, cached); [`crate::security`] runs it as `tutela-sdk`'s axum
//! layer.
//!
//! TODO(Tutela Phase B): delegated agent tokens (`typ: agent+jwt`). Turn on `delegation` and
//! register symphony `delegation_aware` once Tutela issues them; symphony must never forward such
//! a token, and gets a new one per downstream audience. Until then they are refused.

use std::collections::HashSet;
use std::sync::Arc;

use symphony_core::config::TutelaSettings;
use tutela_contracts::claims::Audience;
use tutela_contracts::tenant::{Issuer, TenantId, TrustList};
use tutela_sdk::{
    Alg, CacheRules, IssuerConfig, JwksSource, SystemClock, Verifier, VerifierConfig,
};

/// The one audience symphony accepts.
pub const AUDIENCE: &str = "symphony";

/// The verifier settings for `settings`. The error names the setting at fault.
pub fn verifier_config(settings: &TutelaSettings) -> Result<VerifierConfig, String> {
    let audience: Audience = AUDIENCE
        .parse()
        .map_err(|err| format!("audience {AUDIENCE:?}: {err}"))?;
    let mut trust = TrustList::new();
    let mut issuers = Vec::with_capacity(settings.issuers.len());
    for (i, issuer) in settings.issuers.iter().enumerate() {
        let at = format!("server.tutela.issuers[{i}]");
        let iss: Issuer = issuer
            .iss
            .parse()
            .map_err(|err| format!("{at}.iss: {err}"))?;
        for tenant in &issuer.tenants {
            let tenant: TenantId = tenant
                .parse()
                .map_err(|err| format!("{at}.tenants: {tenant:?}: {err}"))?;
            trust
                .insert(iss.clone(), tenant)
                .map_err(|err| format!("{at}.iss: {err}"))?;
        }
        let algs = issuer
            .algs
            .iter()
            .map(|alg| Alg::parse(alg).ok_or_else(|| format!("{at}.algs: {alg:?}")))
            .collect::<Result<Vec<_>, _>>()?;
        issuers.push(IssuerConfig {
            iss,
            jwks_uri: issuer.jwks_uri.clone(),
            algs,
            external: issuer.external,
            allow_typ_jwt: issuer.allow_typ_jwt,
        });
    }
    Ok(VerifierConfig {
        audience,
        trust,
        issuers,
        // TODO(Tutela Phase B): delegated agent tokens; see the module docs.
        delegation: false,
        delegation_aware: false,
        denied_kids: HashSet::new(),
        cache: CacheRules::default(),
    })
}

/// The production verifier for `settings` (JWKS over `tutela-safenet`, the system clock). Needs
/// the process's rustls crypto provider installed first.
pub fn verifier(settings: &TutelaSettings) -> Result<Arc<Verifier>, String> {
    let config = verifier_config(settings)?;
    Verifier::new(config)
        .map(Arc::new)
        .map_err(|err| format!("server.tutela: the JWKS client cannot start: {err}"))
}

/// A verifier for `settings` that fetches JWKS from `source` (tests and embedders).
pub fn verifier_with(
    settings: &TutelaSettings,
    source: Arc<dyn JwksSource>,
) -> Result<Arc<Verifier>, String> {
    let config = verifier_config(settings)?;
    Ok(Arc::new(Verifier::with(
        config,
        source,
        Arc::new(SystemClock),
    )))
}
