//! flux-dev-gate — a Flux Dev Score becomes a verifiable credential.
//!
//! `flux-ai-bench` scores an AI agent 0–100 on Flux-native dev skills (compile,
//! fix-cycle, provenance, swarm coord, VarFlow, cache, ZK gate, dogfood, honest
//! measurement, recovery). On its own that score is just a number in a log. This
//! crate turns it into the thing the swarm can actually *act* on: a **registrar
//! credential** — a `sigil-oauth` access token, signed by a DNS-anchored key,
//! verifiable OFFLINE by any lane or MCP gate, and revocable via a DNS epoch bump.
//!
//! ```text
//!   flux-ai-bench → score:u8 ─┐
//!                             ├─ issue_dev_credential(registrar, sub, score, pass)
//!                             ▼
//!     credential: scope = "flux:dev:verified flux:dev:score:NN"   (90-day TTL)
//!                             │
//!   lane / MCP gate ─ verify_dev_credential(token, anchor, sub, min) ─► Ok(score)
//! ```
//!
//! Why a credential and not the raw score: the gate verifier needs *one trust
//! input* (the registrar's DNS anchor) and *no* call to the benchmark, the scorer,
//! or any database — exactly like `sigil-oauth` token verification. It works today
//! with no P2P; once the P2P layer is up, the same credential is a SAP input.
//!
//! Decoupled by design: this crate takes a `score: u8`, never depends on
//! `flux-ai-bench`, so the benchmark can evolve freely.

use sigil_oauth::{verify_token, DnsAnchor, Issuer, TokenClaims};

/// Audience stamped on dev credentials (the benchmark that vouches).
pub const AUD: &str = "flux-ai-bench";
/// Scope token asserting the agent passed the benchmark.
pub const VERIFIED_SCOPE: &str = "flux:dev:verified";
/// Default pass mark out of 100 (≥70 = verified).
pub const DEFAULT_PASS: u8 = 70;
/// Credentials are valid 90 days (then re-bench), or until a registrar epoch bump.
pub const CRED_TTL_SECS: u64 = 90 * 86_400;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateError {
    /// The credential token didn't verify against the registrar anchor (bad/rogue
    /// key, expired, or revoked via epoch bump).
    BadCredential,
    /// The credential's subject is not this agent.
    SubjectMismatch,
    /// The credential lacks the `flux:dev:verified` scope (or its score tag).
    NotVerified,
    /// The credential's score is below the verifier's required minimum.
    ScoreTooLow { have: u8, need: u8 },
}

impl std::fmt::Display for GateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GateError::BadCredential => write!(f, "credential did not verify against the registrar anchor"),
            GateError::SubjectMismatch => write!(f, "credential subject != agent"),
            GateError::NotVerified => write!(f, "credential lacks flux:dev:verified"),
            GateError::ScoreTooLow { have, need } => write!(f, "score {have} below required {need}"),
        }
    }
}
impl std::error::Error for GateError {}

/// The scope string a verified credential carries for `score`.
pub fn dev_scope(score: u8) -> String {
    format!("{VERIFIED_SCOPE} flux:dev:score:{score}")
}

/// Issue a Flux-Dev credential to `agent_sub` **iff** `score >= pass`. The score
/// is encoded in the scope so a verifier can demand a minimum later. Returns
/// `None` (no credential) when the agent didn't clear the bar.
pub fn issue_dev_credential(registrar: &Issuer, agent_sub: &str, score: u8, pass: u8) -> Option<String> {
    if score < pass {
        return None;
    }
    Some(registrar.issue_credential(agent_sub, AUD, &dev_scope(score), CRED_TTL_SECS))
}

/// Extract the score tag from a credential's scope, if present.
fn score_in(claims: &TokenClaims) -> Option<u8> {
    claims
        .scope
        .split_whitespace()
        .find_map(|s| s.strip_prefix("flux:dev:score:").and_then(|n| n.parse().ok()))
}

/// Verify a Flux-Dev credential **offline** against the registrar's DNS anchor:
/// signature + issuer + epoch (revocation) + subject binding + the verified scope
/// + a minimum score. Returns the proven score on success.
pub fn verify_dev_credential(
    token: &str,
    registrar: &DnsAnchor,
    agent_sub: &str,
    min_score: u8,
    now: u64,
) -> Result<u8, GateError> {
    let claims = verify_token(token, registrar, now).map_err(|_| GateError::BadCredential)?;
    if claims.sub != agent_sub {
        return Err(GateError::SubjectMismatch);
    }
    if !claims.has_scope(VERIFIED_SCOPE) {
        return Err(GateError::NotVerified);
    }
    let score = score_in(&claims).ok_or(GateError::NotVerified)?;
    if score < min_score {
        return Err(GateError::ScoreTooLow { have: score, need: min_score });
    }
    Ok(score)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sigil_oauth::Keypair;

    fn registrar() -> Issuer {
        Issuer::new("registrar.sigilgraph.quillon.xyz", Keypair::from_seed(&[5u8; 32]))
    }
    const SUB: &str = "sglu_agent_rocky";
    const NOW: u64 = 1_000_000;

    #[test]
    fn below_pass_issues_nothing() {
        let r = registrar();
        assert!(issue_dev_credential(&r, SUB, 69, DEFAULT_PASS).is_none());
        assert!(issue_dev_credential(&r, SUB, 70, DEFAULT_PASS).is_some());
    }

    #[test]
    fn issued_credential_verifies_with_score() {
        let r = registrar();
        let tok = issue_dev_credential(&r, SUB, 88, DEFAULT_PASS).expect("issued");
        let score = verify_dev_credential(&tok, &r.anchor(), SUB, DEFAULT_PASS, NOW).expect("valid");
        assert_eq!(score, 88);
    }

    #[test]
    fn min_score_enforced() {
        let r = registrar();
        let tok = issue_dev_credential(&r, SUB, 72, 70).unwrap();
        // a lane requiring 85 rejects a 72 credential
        assert_eq!(
            verify_dev_credential(&tok, &r.anchor(), SUB, 85, NOW),
            Err(GateError::ScoreTooLow { have: 72, need: 85 })
        );
    }

    #[test]
    fn wrong_subject_rejected() {
        let r = registrar();
        let tok = issue_dev_credential(&r, SUB, 90, 70).unwrap();
        assert_eq!(
            verify_dev_credential(&tok, &r.anchor(), "sglu_someone_else", 70, NOW),
            Err(GateError::SubjectMismatch)
        );
    }

    #[test]
    fn rogue_registrar_rejected() {
        let real = registrar();
        let rogue = Issuer::new("registrar.sigilgraph.quillon.xyz", Keypair::from_seed(&[0xAA; 32]));
        let forged = issue_dev_credential(&rogue, SUB, 100, 70).unwrap();
        assert_eq!(
            verify_dev_credential(&forged, &real.anchor(), SUB, 70, NOW),
            Err(GateError::BadCredential)
        );
    }

    #[test]
    fn registrar_epoch_bump_revokes() {
        let mut r = registrar();
        let tok = issue_dev_credential(&r, SUB, 95, 70).unwrap();
        assert!(verify_dev_credential(&tok, &r.anchor(), SUB, 70, NOW).is_ok());
        r.revoke_all(); // DNS epoch bump → every dev credential dies
        assert_eq!(
            verify_dev_credential(&tok, &r.anchor(), SUB, 70, NOW),
            Err(GateError::BadCredential)
        );
    }
}
