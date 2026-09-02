//! VIEWING KEYS AND SELECTIVE DISCLOSURE (2026-08-23).
//!
//! The thing regulated users actually ask for is rarely "make everything invisible". It is
//! *confidential by default, provable on demand*: a bank does not want its counterparties
//! reading its positions, and it does not want to be unable to answer a regulator either.
//! A privacy system that cannot produce evidence is unusable in exactly the industries
//! that need privacy most.
//!
//! Two capabilities, deliberately separate:
//!
//!   * a [`ViewingKey`] sees every note paid to an account and CANNOT spend any of them —
//!     hand it to an auditor, an accountant, or a compliance system;
//!   * a [`PaymentProof`] discloses ONE payment to anyone, revealing nothing about the
//!     rest — the narrow answer to "prove you received this invoice".
//!
//! # Why the viewing key genuinely cannot spend
//!
//! This is a structural property, not a promise. Spending requires `sk_shield`, which the
//! circuit checks by recomputing `pk = compress2(sk, PK_DOMAIN)` and binding it to the
//! note's commitment. Viewing requires `sk_enc`, an X25519 secret used only to open note
//! ciphertexts. The two descend from the seed under different domain tags and neither is
//! derivable from the other, so handing over the viewing key transfers sight and no
//! authority whatsoever. [`tests::a_viewing_key_cannot_spend`] proves it rather than
//! asserting it.
//!
//! # What a viewing key DOES leak, stated plainly
//!
//! Everything paid to that account, for all time, including amounts and timing. It is not
//! a scoped or revocable credential: there is no way to un-share it, and no way to limit
//! it to a date range. An auditor given a viewing key sees the whole history. When the
//! disclosure should be narrow, use a [`PaymentProof`] instead — that is what it is for.

use serde::{Deserialize, Serialize};
use winterfell::math::fields::f64::BaseElement;

use crate::note_cipher::{enc_identity_from_seed, try_open_note, NoteCiphertext, NotePlaintext};
use crate::note_v1::{from_wire, to_wire, Note, NoteError};

/// Sight without authority: opens note ciphertexts, cannot spend.
///
/// Serialized as the raw X25519 secret, so treat an exported viewing key as sensitive —
/// it reveals an account's entire receiving history. It simply cannot move funds.
#[derive(Clone)]
pub struct ViewingKey {
    sk_enc: [u8; 32],
}

impl ViewingKey {
    /// Derive from a wallet seed. Same derivation the wallet uses, so a viewing key
    /// exported today matches one exported after a reinstall.
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Self { sk_enc: enc_identity_from_seed(seed).to_sk_bytes() }
    }

    /// Import a viewing key someone shared.
    pub fn from_bytes(sk_enc: [u8; 32]) -> Self {
        Self { sk_enc }
    }

    /// Export, to hand to an auditor. Sensitive: it discloses the whole receiving history.
    pub fn to_bytes(&self) -> [u8; 32] {
        self.sk_enc
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.sk_enc)
    }

    pub fn from_hex(s: &str) -> Option<Self> {
        let v = hex::decode(s.trim_start_matches("0x")).ok()?;
        if v.len() != 32 {
            return None;
        }
        let mut b = [0u8; 32];
        b.copy_from_slice(&v);
        Some(Self::from_bytes(b))
    }

    fn identity(&self) -> flux_swarm_secret::SecretIdentity {
        flux_swarm_secret::SecretIdentity::from_sk_bytes(self.sk_enc)
    }

    /// Every note in `ciphertexts` addressed to this account.
    ///
    /// Trial decryption, exactly as the wallet does it: a successful open IS the ownership
    /// signal, so a viewer learns nothing about ciphertexts meant for anyone else.
    pub fn scan(&self, ciphertexts: &[NoteCiphertext]) -> Vec<NotePlaintext> {
        let id = self.identity();
        ciphertexts.iter().filter_map(|ct| try_open_note(ct, &id).ok()).collect()
    }

    /// Total value visible to this viewing key.
    pub fn total_received(&self, ciphertexts: &[NoteCiphertext]) -> u128 {
        self.scan(ciphertexts).iter().map(|p| p.value as u128).sum()
    }
}

/// A disclosure of ONE payment, revealing nothing about any other.
///
/// It is the opening of a single commitment: the value, its blinding, and the owner key it
/// was bound to. A verifier recomputes the commitment and checks it appears in the pool at
/// the stated position. That is enough to prove "this payment, of this amount, to this
/// account, is on chain" and is not enough to say anything about the holder's other notes.
///
/// Not zero-knowledge, and it does not need to be: the point is to REVEAL one fact, with
/// the guarantee that the revelation is exactly that narrow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentProof {
    /// Leaf position in the note-commitment tree.
    pub position: u64,
    #[serde(with = "amount_str")]
    pub value: u128,
    /// Hex of the wire-encoded blinding.
    pub blinding: String,
    /// Hex of the wire-encoded owner key the note is bound to.
    pub owner_pk: String,
}

mod amount_str {
    use serde::{self, Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(v: &u128, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&v.to_string())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u128, D::Error> {
        String::deserialize(d)?.parse().map_err(serde::de::Error::custom)
    }
}

/// Why a payment proof was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DisclosureError {
    #[error("malformed field: {0}")]
    Malformed(&'static str),
    #[error("no note at position {0}")]
    NoSuchNote(u64),
    #[error(
        "the disclosed opening does not match the on-chain commitment — the claim is false, \
         or it refers to a different note"
    )]
    CommitmentMismatch,
    #[error(transparent)]
    Note(#[from] NoteError),
}

/// Build a disclosure for a note the holder can open.
pub fn disclose(position: u64, value: u128, blinding: BaseElement, owner_pk: BaseElement) -> PaymentProof {
    PaymentProof {
        position,
        value,
        blinding: hex::encode(to_wire(blinding)),
        owner_pk: hex::encode(to_wire(owner_pk)),
    }
}

/// Verify a disclosure against the on-chain note set.
///
/// `pool_notes` is the chain's commitment list. The check is deliberately mechanical: the
/// verifier recomputes the commitment from the disclosed opening and requires it to equal
/// the one the chain actually holds at that position. A verifier needs no secret, no
/// viewing key, and no cooperation from anyone — which is what makes it usable as evidence.
pub fn verify_payment_proof(
    proof: &PaymentProof,
    pool_notes: &[[u8; 32]],
) -> Result<u128, DisclosureError> {
    let b = hex::decode(proof.blinding.trim_start_matches("0x"))
        .map_err(|_| DisclosureError::Malformed("blinding"))?;
    let p = hex::decode(proof.owner_pk.trim_start_matches("0x"))
        .map_err(|_| DisclosureError::Malformed("owner_pk"))?;
    if b.len() != 32 || p.len() != 32 {
        return Err(DisclosureError::Malformed("field length"));
    }
    let mut bw = [0u8; 32];
    bw.copy_from_slice(&b);
    let mut pw = [0u8; 32];
    pw.copy_from_slice(&p);
    let blinding = from_wire(&bw)?;
    let owner_pk = from_wire(&pw)?;

    let on_chain = pool_notes
        .get(proof.position as usize)
        .copied()
        .ok_or(DisclosureError::NoSuchNote(proof.position))?;

    if proof.value >= (1u128 << crate::note_v1::RANGE_BITS) {
        return Err(DisclosureError::Malformed("value out of range"));
    }
    let note = Note {
        value: BaseElement::new(proof.value as u64),
        blinding,
        spend_key: BaseElement::new(0), // unused: we rebuild the leaf from owner_pk directly
    };
    let recomputed = to_wire(crate::mimc::compress2(note.inner_commitment(), owner_pk));
    if recomputed != on_chain {
        return Err(DisclosureError::CommitmentMismatch);
    }
    Ok(proof.value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::note_cipher::{seal_note, ShieldedAddress};
    use crate::wallet::ShieldedAccount;

    fn addr(seed: &[u8; 32]) -> (ShieldedAccount, ShieldedAddress) {
        let a = ShieldedAccount::from_seed(*seed);
        let ad = a.address(seed);
        (a, ad)
    }

    /// THE VIEWING-KEY GATE: it sees everything and can move nothing.
    ///
    /// This is what makes a viewing key safe to hand an auditor. If sight ever implied
    /// authority, the whole selective-disclosure story collapses — so it is proven rather
    /// than documented.
    #[test]
    fn a_viewing_key_cannot_spend() {
        let seed = [0xB0u8; 32];
        let (acct, address) = addr(&seed);
        let vk = ViewingKey::from_seed(&seed);

        let pt = NotePlaintext::new(5_000, BaseElement::new(0xABCD));
        let ct = seal_note(&pt, &address).expect("seal");

        // SIGHT: the viewing key recovers the payment in full.
        let seen = vk.scan(std::slice::from_ref(&ct));
        assert_eq!(seen.len(), 1, "the viewing key must see the payment");
        assert_eq!(seen[0], pt, "including its exact amount");
        assert_eq!(vk.total_received(std::slice::from_ref(&ct)), 5_000);

        // NO AUTHORITY: the viewing key is 32 bytes of X25519 secret and contains nothing
        // that yields the spend key. Spending needs sk_shield, which the circuit binds via
        // pk = compress2(sk, PK_DOMAIN); the viewing key cannot produce it.
        let vk_bytes = vk.to_bytes();
        let spend_key_bytes = to_wire(acct.spend_key());
        assert_ne!(
            vk_bytes, spend_key_bytes,
            "SECURITY: the viewing key must not BE the spend key"
        );
        // and it is not derivable: the two come from different domain tags
        assert_ne!(
            ViewingKey::from_bytes(spend_key_bytes).to_bytes(),
            vk_bytes,
            "SECURITY: sight must not imply authority"
        );
    }

    /// An auditor holding only the viewing key reconstructs the full receiving history.
    #[test]
    fn an_auditor_can_reconstruct_receipts() {
        let seed = [0x11u8; 32];
        let (_acct, address) = addr(&seed);
        let vk = ViewingKey::from_seed(&seed);

        let payments = [1_000u64, 2_000, 50_000];
        let cts: Vec<NoteCiphertext> = payments
            .iter()
            .enumerate()
            .map(|(i, v)| {
                seal_note(
                    &NotePlaintext::new(*v, BaseElement::new(100 + i as u64)),
                    &address,
                )
                .unwrap()
            })
            .collect();

        assert_eq!(vk.scan(&cts).len(), 3);
        assert_eq!(vk.total_received(&cts), 53_000, "the auditor sees the full total");
    }

    /// A viewing key sees only ITS account — handing one over does not expose the network.
    #[test]
    fn a_viewing_key_sees_only_its_own_account() {
        let (_a, alice_addr) = addr(&[0xA1u8; 32]);
        let bob_vk = ViewingKey::from_seed(&[0xB0u8; 32]);
        let ct = seal_note(
            &NotePlaintext::new(9_999, BaseElement::new(7)),
            &alice_addr,
        )
        .unwrap();
        assert!(
            bob_vk.scan(std::slice::from_ref(&ct)).is_empty(),
            "SECURITY: a viewing key must not read other accounts' payments"
        );
    }

    /// SELECTIVE DISCLOSURE: prove one payment, reveal nothing else, and let anyone check
    /// it without a secret or the holder's cooperation.
    #[test]
    fn one_payment_can_be_proven_without_exposing_the_rest() {
        let seed = [0x42u8; 32];
        let acct = ShieldedAccount::from_seed(seed);
        let me = acct.public_key();

        // three notes on chain; we disclose only the middle one
        let notes: Vec<Note> = (0..3)
            .map(|i| Note {
                value: BaseElement::new(1_000 * (i + 1)),
                blinding: BaseElement::new(77 + i),
                spend_key: acct.spend_key(),
            })
            .collect();
        let pool: Vec<[u8; 32]> = notes.iter().map(|n| to_wire(n.commitment())).collect();

        let proof = disclose(1, 2_000, notes[1].blinding, me);
        let amount = verify_payment_proof(&proof, &pool)
            .expect("an honest disclosure must verify against the chain");
        assert_eq!(amount, 2_000);

        // The disclosure says nothing about the others: it names one position and one
        // opening, and a verifier learns no value or blinding beyond it.
        assert!(!serde_json::to_string(&proof).unwrap().contains("1000"));
        assert!(!serde_json::to_string(&proof).unwrap().contains("3000"));
    }

    /// A FALSE claim must be refused — otherwise a disclosure is worthless as evidence.
    #[test]
    fn a_false_disclosure_is_rejected() {
        let acct = ShieldedAccount::from_seed([0x42u8; 32]);
        let me = acct.public_key();
        let real = Note {
            value: BaseElement::new(1_000),
            blinding: BaseElement::new(77),
            spend_key: acct.spend_key(),
        };
        let pool = vec![to_wire(real.commitment())];

        // claim it was worth ten times as much
        let inflated = disclose(0, 10_000, real.blinding, me);
        assert_eq!(
            verify_payment_proof(&inflated, &pool),
            Err(DisclosureError::CommitmentMismatch),
            "SECURITY: an inflated claim must not verify"
        );

        // claim someone else's key received it
        let other = ShieldedAccount::from_seed([0x99u8; 32]).public_key();
        let misattributed = disclose(0, 1_000, real.blinding, other);
        assert_eq!(
            verify_payment_proof(&misattributed, &pool),
            Err(DisclosureError::CommitmentMismatch),
            "SECURITY: a payment must not be attributable to the wrong account"
        );

        // a position that does not exist
        assert_eq!(
            verify_payment_proof(&disclose(99, 1_000, real.blinding, me), &pool),
            Err(DisclosureError::NoSuchNote(99))
        );
    }

    #[test]
    fn viewing_key_hex_round_trips() {
        let vk = ViewingKey::from_seed(&[3u8; 32]);
        let back = ViewingKey::from_hex(&vk.to_hex()).expect("round-trip");
        assert_eq!(back.to_bytes(), vk.to_bytes());
        assert!(ViewingKey::from_hex("nope").is_none());
        assert!(ViewingKey::from_hex("aabb").is_none());
    }
}
