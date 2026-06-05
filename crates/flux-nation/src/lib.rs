//! # flux-nation — the consent-signed prototype behind `nation.html`
//!
//! Not a demo (fake data) and not a bare stub — a **working prototype**: every
//! citizen action is gated by a **real post-quantum MitID-style consent
//! signature** ([`flux_sqisign`], SQIsign Level-5), then executed through the
//! real, tested [`sigil_rpc::nation`] state primitives (attest / pay-utility /
//! e-Boks) which commit into the chain's `contract_state_root` — so a flux
//! lightweight node can verify each action client-side, no server trusted.
//!
//! The ONLY pretend part, named honestly: the **gov data source** (NemLog-in
//! broker, borger.dk, e-Boks API). That is a swappable [`Backend`]; the
//! `LocalPrototype` backend runs the full flow today, the `NemLogin` backend
//! returns [`FluxNationError::NotOnboarded`] until an official broker agreement
//! + NSIS registration exists. Crypto, consent, state + conservation are REAL;
//! the broker handshake is the one thing waiting on Denmark.

use sigil_rpc::nation::{self, NationError, BORGER_AUTHORITY};
use sigil_state::{SigilState, WalletId, NATIVE};

/// Which gov data source backs the citizen-data tools.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Backend {
    /// Working local prototype — runs the real on-chain flow now.
    LocalPrototype,
    /// Real NemLog-in broker / borger.dk / e-Boks — pending official onboarding.
    NemLogin,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum FluxNationError {
    #[error("consent signature invalid")]
    BadConsent,
    #[error("consent is for a different action (expected `{expected}`, got `{got}`)")]
    ActionMismatch { expected: String, got: String },
    #[error("nation: {0:?}")]
    Nation(NationError),
    #[error("sqisign: {0}")]
    Sqi(String),
    #[error("backend `{0:?}` not onboarded — needs NemLog-in broker agreement + NSIS registration")]
    NotOnboarded(Backend),
}

/// A per-action consent: the citizen's SQIsign signature over a canonical action
/// string. This is the "samtykke pr. handling" guarantee, in bytes.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Consent {
    pub action: String,
    pub citizen_pk: Vec<u8>,
    pub sig: Vec<u8>,
}

/// The citizen's on-chain wallet is the BLAKE3 of their SQIsign public key —
/// stable, and bound to the key that signs their consent.
pub fn citizen_wallet(pk: &[u8]) -> WalletId {
    *blake3::hash(pk).as_bytes()
}

/// Citizen side: sign consent for a canonical action with the MitID-style key.
pub fn sign_consent(action: &str, sk: &[u8], pk: &[u8]) -> Result<Consent, FluxNationError> {
    let sig = flux_sqisign::sign(action.as_bytes(), sk, pk).map_err(FluxNationError::Sqi)?;
    Ok(Consent { action: action.to_string(), citizen_pk: pk.to_vec(), sig })
}

/// Verify a consent is (a) for exactly `expected` and (b) a valid signature.
fn check_consent(c: &Consent, expected: &str) -> Result<(), FluxNationError> {
    if c.action != expected {
        return Err(FluxNationError::ActionMismatch { expected: expected.to_string(), got: c.action.clone() });
    }
    let ok = flux_sqisign::verify(c.action.as_bytes(), &c.sig, &c.citizen_pk).map_err(FluxNationError::Sqi)?;
    if ok { Ok(()) } else { Err(FluxNationError::BadConsent) }
}

fn hx(b: &[u8]) -> String {
    b.iter().take(6).map(|x| format!("{x:02x}")).collect()
}

/// One audited action — the provenance record a citizen can inspect.
#[derive(Debug, Clone)]
pub struct AuditEntry {
    pub action: String,
    pub citizen: WalletId,
    pub height: u64,
    pub ok: bool,
}
#[derive(Debug, Default)]
pub struct AuditLog(pub Vec<AuditEntry>);

/// Canonical action strings — the citizen signs exactly these.
pub fn act_pay_utility(provider: &WalletId, amount: u128) -> String {
    format!("flux-nation:pay_utility:provider={}:amount={}", hx(provider), amount)
}
pub fn act_eboks_ack(doc_hash: &[u8; 32]) -> String {
    format!("flux-nation:eboks_ack:doc={}", hx(doc_hash))
}

/// **nation_electricity_bill (pay)** — consent-gated. Verifies the citizen's
/// SQIsign consent over the exact (provider, amount), then runs the real
/// NATIVE-conserved on-chain payment. Returns the citizen's new balance.
pub fn pay_utility_bill(
    state: &mut SigilState,
    audit: &mut AuditLog,
    height: u64,
    backend: Backend,
    consent: &Consent,
    provider: WalletId,
    amount: u128,
) -> Result<u128, FluxNationError> {
    if backend == Backend::NemLogin {
        return Err(FluxNationError::NotOnboarded(backend));
    }
    let expected = act_pay_utility(&provider, amount);
    check_consent(consent, &expected)?;
    let citizen = citizen_wallet(&consent.citizen_pk);
    let r = nation::pay_utility_bill(state, height, citizen, provider, amount)
        .map_err(FluxNationError::Nation);
    audit.0.push(AuditEntry { action: expected, citizen, height, ok: r.is_ok() });
    r
}

/// **nation_eboks_read (ack)** — consent-gated e-Boks receipt: the citizen signs
/// acknowledgement of a document, committing its hash to the e-Boks ledger so
/// the receipt is later client-side verifiable via [`verify_eboks`].
pub fn eboks_ack(
    state: &mut SigilState,
    audit: &mut AuditLog,
    height: u64,
    backend: Backend,
    consent: &Consent,
    doc_hash: [u8; 32],
) -> Result<(), FluxNationError> {
    if backend == Backend::NemLogin {
        return Err(FluxNationError::NotOnboarded(backend));
    }
    let expected = act_eboks_ack(&doc_hash);
    check_consent(consent, &expected)?;
    let citizen = citizen_wallet(&consent.citizen_pk);
    let r = nation::issue_eboks_receipt(state, height, citizen, doc_hash)
        .map_err(FluxNationError::Nation);
    audit.0.push(AuditEntry { action: expected, citizen, height, ok: r.is_ok() });
    r
}

/// **nation_mitid_sign / borger attestation** — the gov authority attests a
/// citizen (wallet → cpr_hash). The raw CPR is never handled here, only its hash.
/// (Authority↔on-chain `BORGER_AUTHORITY` binding is a prototype simplification.)
pub fn attest_citizen(
    state: &mut SigilState,
    height: u64,
    citizen: WalletId,
    cpr_hash: [u8; 32],
) -> Result<(), FluxNationError> {
    nation::attest_citizen(state, height, BORGER_AUTHORITY, citizen, cpr_hash)
        .map_err(FluxNationError::Nation)
}

/// Client-side e-Boks verify (no server trusted) — re-exported for the light node.
pub fn verify_eboks(state: &SigilState, citizen: &WalletId, doc_hash: &[u8; 32]) -> bool {
    nation::verify_eboks_receipt(state, citizen, doc_hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sigil_state::{commit_state_transition, StateMutation, StateTransition};

    const POWER_CO: WalletId = [0x9E; 32];
    const CPR: [u8; 32] = [0x42; 32];

    fn funded_citizen(pk: &[u8], native: u128) -> (SigilState, WalletId) {
        let citizen = citizen_wallet(pk);
        let mut s = SigilState::new();
        commit_state_transition(&mut s, &StateTransition { at_height: 0, mutations: vec![
            StateMutation::SetMasterWallet { wallet: [0xFF; 32] },
            StateMutation::SetBalance { wallet: citizen, token: NATIVE, amount: native },
        ] }, 0).unwrap();
        super::attest_citizen(&mut s, 1, citizen, CPR).unwrap();
        (s, citizen)
    }

    #[test]
    fn full_consent_signed_pay_flow_conserves_native() {
        let (sk, pk) = flux_sqisign::keygen();
        let (mut s, citizen) = funded_citizen(&pk, 1000);
        let mut audit = AuditLog::default();

        // citizen signs consent for THIS exact (provider, amount)
        let action = act_pay_utility(&POWER_CO, 300);
        let consent = sign_consent(&action, &sk, &pk).unwrap();

        let left = pay_utility_bill(&mut s, &mut audit, 2, Backend::LocalPrototype, &consent, POWER_CO, 300).unwrap();
        assert_eq!(left, 700);
        assert_eq!(s.balance_of(&citizen, &NATIVE), 700);
        assert_eq!(s.balance_of(&POWER_CO, &NATIVE), 300); // conserved
        assert_eq!(audit.0.len(), 1);
        assert!(audit.0[0].ok);
    }

    #[test]
    fn tampered_consent_is_rejected() {
        let (sk, pk) = flux_sqisign::keygen();
        let (mut s, _c) = funded_citizen(&pk, 1000);
        let mut audit = AuditLog::default();

        // sign consent for 300 but try to pay 999 → action mismatch
        let consent = sign_consent(&act_pay_utility(&POWER_CO, 300), &sk, &pk).unwrap();
        let err = pay_utility_bill(&mut s, &mut audit, 2, Backend::LocalPrototype, &consent, POWER_CO, 999).unwrap_err();
        assert!(matches!(err, FluxNationError::ActionMismatch { .. }));

        // flip a signature byte → BadConsent
        let mut bad = sign_consent(&act_pay_utility(&POWER_CO, 300), &sk, &pk).unwrap();
        bad.sig[0] ^= 0xFF;
        let err2 = pay_utility_bill(&mut s, &mut audit, 2, Backend::LocalPrototype, &bad, POWER_CO, 300).unwrap_err();
        assert!(matches!(err2, FluxNationError::BadConsent | FluxNationError::Sqi(_)));
    }

    #[test]
    fn eboks_consent_then_client_side_verify() {
        let (sk, pk) = flux_sqisign::keygen();
        let (mut s, citizen) = funded_citizen(&pk, 0);
        let mut audit = AuditLog::default();
        let doc = [0xD0u8; 32];
        let consent = sign_consent(&act_eboks_ack(&doc), &sk, &pk).unwrap();
        eboks_ack(&mut s, &mut audit, 2, Backend::LocalPrototype, &consent, doc).unwrap();
        assert!(verify_eboks(&s, &citizen, &doc));            // light node verifies
        assert!(!verify_eboks(&s, &citizen, &[0x01; 32]));    // tamper rejected
    }

    #[test]
    fn nemlogin_backend_is_honestly_not_onboarded() {
        let (sk, pk) = flux_sqisign::keygen();
        let (mut s, _c) = funded_citizen(&pk, 1000);
        let mut audit = AuditLog::default();
        let consent = sign_consent(&act_pay_utility(&POWER_CO, 300), &sk, &pk).unwrap();
        let err = pay_utility_bill(&mut s, &mut audit, 2, Backend::NemLogin, &consent, POWER_CO, 300).unwrap_err();
        assert_eq!(err, FluxNationError::NotOnboarded(Backend::NemLogin));
    }
}
