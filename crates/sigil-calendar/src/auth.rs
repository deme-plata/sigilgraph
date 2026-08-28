//! Wallet-native authentication — identical pattern to `sigil-mail::auth`
//! (same rationale: don't reimplement identity per SIGIL surface, every
//! citizen-facing crate verifies against the one real `sigil-oauth`
//! machinery). See that module's doc for the full reasoning; this is a
//! deliberately near-identical twin, not a divergent reinvention.

use sigil_oauth::{verify_token_via_dns, AnchorResolver, OAuthError, TokenClaims};

pub const CALENDAR_SCOPE: &str = "calendar";

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("token invalid: {0}")]
    Token(#[from] OAuthError),
    #[error("token does not carry the {CALENDAR_SCOPE:?} scope")]
    MissingScope,
}

pub fn authenticate<R: AnchorResolver>(
    resolver: &R,
    bearer_token: &str,
    now_ts: u64,
) -> Result<TokenClaims, AuthError> {
    let claims = verify_token_via_dns(bearer_token, resolver, now_ts)?;
    if !claims.has_scope(CALENDAR_SCOPE) {
        return Err(AuthError::MissingScope);
    }
    Ok(claims)
}

pub fn wallet_id_of(claims: &TokenClaims) -> &str {
    &claims.sub
}

#[cfg(test)]
mod tests {
    use super::*;
    use sigil_oauth::{DnsAnchor, IssuerSigner, Keypair, StaticResolver};

    fn issue_test_token(scope: &str) -> (String, StaticResolver) {
        let issuer = "sigilgraph.org";
        let signer = IssuerSigner::Ed25519(Keypair::generate());
        let wallet = Keypair::generate();

        let claims = TokenClaims {
            typ: "access".into(),
            iss: issuer.into(),
            sub: sigil_oauth::wallet_id(&wallet.pubkey()),
            aud: "sigil-calendar".into(),
            scope: scope.into(),
            iat: 1000,
            exp: 1_000_000,
            epoch: 0,
            cnf: None,
        };
        let body = serde_json::to_vec(&claims).unwrap();
        let sig = signer.sign(blake3::hash(&body).as_bytes());
        let token = format!("{}.{}", hex::encode(&body), hex::encode(sig));

        let anchor = DnsAnchor::for_issuer(issuer, &signer.pubkey(), signer.alg(), 0);
        (token, StaticResolver::default().with(anchor))
    }

    #[test]
    fn a_token_with_the_calendar_scope_authenticates() {
        let (token, resolver) = issue_test_token(CALENDAR_SCOPE);
        let claims = authenticate(&resolver, &token, 5000).expect("should authenticate");
        assert!(!wallet_id_of(&claims).is_empty());
    }

    #[test]
    fn a_token_missing_the_calendar_scope_is_rejected() {
        let (token, resolver) = issue_test_token("mail");
        let err = authenticate(&resolver, &token, 5000).expect_err("must reject");
        assert!(matches!(err, AuthError::MissingScope));
    }
}
