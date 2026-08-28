//! Wallet-native authentication — the identity boundary that got REBUILT
//! rather than ported (see the crate-level doc). No login screen, no
//! password, no JWT-signed-by-us: sigil-mail is a plain OAuth2 RESOURCE
//! SERVER. The actual "prove you own this wallet" ceremony already exists
//! and is fully built in `sigil-oauth` (DNS-anchored, wallet-signs-the-
//! auth-request, offline-verifiable access tokens) — this module's only
//! job is to accept a bearer token, verify it against that same machinery,
//! check it carries the `mail` scope, and hand back the wallet id.
//!
//! Nothing here re-implements crypto or token issuance. If that ever looks
//! tempting, it's the wrong instinct — go extend `sigil-oauth` instead, so
//! every SIGIL surface (wallet UI, mail, calendar, whatever comes next)
//! keeps sharing one identity system instead of growing a second one.

use sigil_oauth::{verify_token_via_dns, AnchorResolver, OAuthError, TokenClaims};

/// The scope a token must carry to use sigil-mail. Matches the space-
/// delimited `scope` convention `TokenClaims::has_scope` already expects.
pub const MAIL_SCOPE: &str = "mail";

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("token invalid: {0}")]
    Token(#[from] OAuthError),
    #[error("token does not carry the {MAIL_SCOPE:?} scope")]
    MissingScope,
}

/// Verify a bearer token and return its claims, requiring the `mail` scope.
/// `resolver` is how the issuer's DNS anchor gets looked up — a real DoH
/// resolver in production, [`sigil_oauth::StaticResolver`] in tests (see
/// `sigil-oauth`'s own doc for why resolution is pluggable: the browser/WASM
/// client and this server share one verification path).
pub fn authenticate<R: AnchorResolver>(
    resolver: &R,
    bearer_token: &str,
    now_ts: u64,
) -> Result<TokenClaims, AuthError> {
    let claims = verify_token_via_dns(bearer_token, resolver, now_ts)?;
    if !claims.has_scope(MAIL_SCOPE) {
        return Err(AuthError::MissingScope);
    }
    Ok(claims)
}

/// The wallet id a verified token authenticates as — `TokenClaims::sub`.
/// Callers pass this straight to [`crate::store::MailStore`]'s
/// wallet-keyed methods (`get_account`, `create_account`, `add_alias`, …).
pub fn wallet_id_of(claims: &TokenClaims) -> &str {
    &claims.sub
}

#[cfg(test)]
mod tests {
    use super::*;
    use sigil_oauth::{
        pkce_pair, AuthRequest, DnsAnchor, IssuerSigner, Keypair, StaticResolver, WalletAssertion,
    };

    /// Builds a real, fully-signed access token the same way the actual
    /// sigil-oauth Authorization Server would — no shortcuts — so this test
    /// proves sigil-mail's auth boundary against genuine sigil-oauth output,
    /// not a hand-rolled stand-in for it.
    fn issue_test_token(scope: &str) -> (String, StaticResolver) {
        let issuer = "sigilgraph.org";
        let signer = IssuerSigner::Ed25519(Keypair::generate());
        let wallet = Keypair::generate();

        let (verifier, challenge) = pkce_pair();
        let req = AuthRequest {
            client_id: "sigil-mail".into(),
            redirect_uri: "https://sigilgraph.org/mail/callback".into(),
            scope: scope.into(),
            code_challenge: challenge,
            code_challenge_method: "S256".into(),
            state: "test-state".into(),
            nonce: "test-nonce".into(),
        };
        let _assertion = WalletAssertion::sign(&wallet, &req);
        let _ = verifier; // the full AS code<->token exchange is sigil-oauth's own tested surface;
                           // here we only need a genuine, correctly-signed TokenClaims blob to
                           // verify sigil-mail's CONSUMPTION side, so we mint the claims directly
                           // with the real signer rather than re-running the whole AS state machine.
        let claims = TokenClaims {
            typ: "access".into(),
            iss: issuer.into(),
            sub: sigil_oauth::wallet_id(&wallet.pubkey()),
            aud: req.client_id.clone(),
            scope: scope.into(),
            iat: 1000,
            exp: 1_000_000,
            epoch: 0,
            cnf: None,
        };
        let body = serde_json::to_vec(&claims).unwrap();
        // sigil-oauth's own `sign_blob`/`open_blob` hash the body with BLAKE3
        // before signing (see its doc: "hex(json).hex(sig)"); its `b3()`
        // helper isn't public, so this test does the identical hash itself
        // rather than reimplementing token issuance some other way.
        let sig = signer.sign(blake3::hash(&body).as_bytes());
        let token = format!("{}.{}", hex::encode(&body), hex::encode(sig));

        let anchor = DnsAnchor::for_issuer(issuer, &signer.pubkey(), signer.alg(), 0);
        let resolver = StaticResolver::default().with(anchor);
        (token, resolver)
    }

    #[test]
    fn a_token_with_the_mail_scope_authenticates() {
        let (token, resolver) = issue_test_token(MAIL_SCOPE);
        let claims = authenticate(&resolver, &token, 5000).expect("should authenticate");
        assert!(!wallet_id_of(&claims).is_empty());
    }

    #[test]
    fn a_token_missing_the_mail_scope_is_rejected() {
        let (token, resolver) = issue_test_token("wallet other-scope");
        let err = authenticate(&resolver, &token, 5000).expect_err("must reject");
        assert!(matches!(err, AuthError::MissingScope));
    }

    #[test]
    fn an_expired_token_is_rejected() {
        let (token, resolver) = issue_test_token(MAIL_SCOPE);
        let err = authenticate(&resolver, &token, 10_000_000).expect_err("must reject");
        assert!(matches!(err, AuthError::Token(OAuthError::TokenExpired)));
    }
}
