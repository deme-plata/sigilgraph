//! NOTE CIPHERTEXTS — the layer that makes RECEIVING possible (2026-08-23).
//!
//! A note commitment is `compress2(compress2(value, blinding), pk)`. It is *hiding*, so a
//! recipient staring at the chain cannot read `value` or `blinding` out of it. Without a
//! side channel a wallet can only recognise notes it created itself — which is why
//! shielded self-custody worked and shielded *payments* did not.
//!
//! This module is that side channel: the sender seals `(value, blinding)` to the
//! recipient's encryption key and publishes the ciphertext alongside the commitment. The
//! recipient trial-decrypts every ciphertext in every block; the AEAD tag fails for
//! everyone else, so a successful open IS the ownership signal. Nobody learns who the
//! recipient is — only that *someone* could open it, and only that someone knows.
//!
//! # Why this had to come after owner binding
//!
//! Handing a recipient `(value, blinding)` is precisely what this does. Under the earlier
//! circuit, a note's commitment bound no owner, so *both* parties could then spend it —
//! each producing a different nullifier, which the spent-set cannot catch. Shipping
//! ciphertexts first would have shipped a double-spend. Owner binding
//! (`spend_full_v4`) is what makes disclosure safe: the sender learns nothing it can
//! spend, because it lacks the recipient's `sk`.
//!
//! # A shielded address is two keys
//!
//! ```text
//!   pk_shield : field element — what the CIRCUIT binds a note to
//!   pk_enc    : X25519 key    — what CIPHERTEXTS are sealed to
//! ```
//!
//! Two keys because they do different jobs and cannot be the same object: `pk_shield` is
//! a MiMC image chosen so it is cheap to prove *inside* the AIR, and a hash image cannot
//! perform a key exchange. Both descend from one seed, so a user still backs up one
//! secret.
//!
//! # What is deliberately NOT hidden
//!
//! The ciphertext's presence and size are public, so an observer learns how many outputs
//! a transaction had. Amounts, recipients and the link to any prior note stay hidden.
//! Padding output count to a fixed arity (`N_OUTS`) is what keeps that leak constant
//! rather than proportional to real activity.

use serde::{Deserialize, Serialize};
use winterfell::math::fields::f64::BaseElement;
use winterfell::math::StarkField;

use crate::note_v1::{from_wire, to_wire, NoteError};

/// Domain tag so a note ciphertext can never be confused with another sealed payload.
const NOTE_MAGIC: &[u8; 8] = b"SIGILNT1";

/// Errors from sealing or opening a note.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CipherError {
    #[error("bad encryption public key: {0}")]
    BadEncKey(String),
    #[error("malformed note plaintext")]
    MalformedPlaintext,
    #[error("ciphertext is not for us")]
    NotForUs,
    #[error("bad address encoding: {0}")]
    BadAddress(&'static str),
    #[error(transparent)]
    Note(#[from] NoteError),
}

/// What a sender transmits to a recipient about one output note.
///
/// Only `(value, blinding)` — the recipient supplies its own `pk_shield` when
/// reconstructing the commitment, so a ciphertext cannot be used to make the recipient
/// accept a note bound to somebody else's key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotePlaintext {
    pub value: u64,
    pub blinding: BaseElement,
}

impl NotePlaintext {
    fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(24);
        v.extend_from_slice(NOTE_MAGIC);
        v.extend_from_slice(&self.value.to_le_bytes());
        v.extend_from_slice(&self.blinding.as_int().to_le_bytes());
        v
    }

    fn decode(b: &[u8]) -> Result<Self, CipherError> {
        if b.len() != 24 || &b[..8] != NOTE_MAGIC {
            return Err(CipherError::MalformedPlaintext);
        }
        let mut v = [0u8; 8];
        v.copy_from_slice(&b[8..16]);
        let mut r = [0u8; 8];
        r.copy_from_slice(&b[16..24]);
        let raw = u64::from_le_bytes(r);
        // Reject a non-canonical blinding: two encodings of one field element would give
        // the same note two commitments, and a note with two spellings is a headache the
        // nullifier set should never have to reason about.
        let mut wire = [0u8; 32];
        wire[..8].copy_from_slice(&raw.to_le_bytes());
        let blinding = from_wire(&wire)?;
        Ok(Self { value: u64::from_le_bytes(v), blinding })
    }
}

/// A published, opaque note ciphertext. Serialized as the sealed envelope's JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteCiphertext(pub String);

/// A shielded address: the circuit key plus the encryption key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShieldedAddress {
    /// Hex of the wire-encoded `pk_shield` field element.
    pub pk_shield: String,
    /// Hex X25519 public key.
    pub pk_enc: String,
}

impl ShieldedAddress {
    pub fn new(pk_shield: BaseElement, pk_enc_hex: &str) -> Self {
        Self { pk_shield: hex::encode(to_wire(pk_shield)), pk_enc: pk_enc_hex.to_string() }
    }

    /// The circuit key a payer must bind an output note to.
    pub fn shield_key(&self) -> Result<BaseElement, CipherError> {
        let v = hex::decode(&self.pk_shield)
            .map_err(|_| CipherError::BadAddress("pk_shield must be hex"))?;
        if v.len() != 32 {
            return Err(CipherError::BadAddress("pk_shield must be 32 bytes"));
        }
        let mut w = [0u8; 32];
        w.copy_from_slice(&v);
        Ok(from_wire(&w)?)
    }

    /// A single copy-pasteable string, `sigil1s:<pk_shield>:<pk_enc>`.
    pub fn encode(&self) -> String {
        format!("sigil1s:{}:{}", self.pk_shield, self.pk_enc)
    }

    pub fn decode(s: &str) -> Result<Self, CipherError> {
        let mut parts = s.split(':');
        match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some("sigil1s"), Some(a), Some(b), None) if !a.is_empty() && !b.is_empty() => {
                Ok(Self { pk_shield: a.to_string(), pk_enc: b.to_string() })
            }
            _ => Err(CipherError::BadAddress("expected sigil1s:<pk_shield>:<pk_enc>")),
        }
    }
}

/// Seal a note's `(value, blinding)` to a recipient address.
///
/// Sealed-box style: anyone may seal to an address, and only the holder of the matching
/// secret can open it. The sender cannot open its own ciphertext afterwards — the
/// ephemeral key is consumed — which is a property worth having, not a limitation: a
/// sender who kept it could later prove to a third party what it paid.
pub fn seal_note(
    plaintext: &NotePlaintext,
    recipient: &ShieldedAddress,
) -> Result<NoteCiphertext, CipherError> {
    let pk = flux_swarm_secret::parse_pubkey_hex(&recipient.pk_enc)
        .map_err(|e| CipherError::BadEncKey(e.to_string()))?;
    let env = flux_swarm_secret::seal(&plaintext.encode(), &pk);
    let json = serde_json::to_string(&env).map_err(|_| CipherError::MalformedPlaintext)?;
    Ok(NoteCiphertext(json))
}

/// Try to open a ciphertext with our encryption identity.
///
/// Returns `NotForUs` for anything not addressed to us — which is the common case and
/// must stay cheap, since a wallet runs this over every ciphertext in every block. The
/// AEAD tag does the work: a wrong key cannot produce a valid tag, so a successful open
/// is proof the note is ours. There is no separate "is this mine?" marker to leak.
pub fn try_open_note(
    ct: &NoteCiphertext,
    id: &flux_swarm_secret::SecretIdentity,
) -> Result<NotePlaintext, CipherError> {
    let env: flux_swarm_secret::SealedEnvelope =
        serde_json::from_str(&ct.0).map_err(|_| CipherError::NotForUs)?;
    let pt = flux_swarm_secret::open(&env, id).map_err(|_| CipherError::NotForUs)?;
    NotePlaintext::decode(&pt)
}

/// Derive a deterministic X25519 identity from a wallet seed.
///
/// Domain-separated from the spend key so the two secrets are independent: compromising
/// the viewing side must not hand over the ability to spend.
pub fn enc_identity_from_seed(seed: &[u8; 32]) -> flux_swarm_secret::SecretIdentity {
    let mut h = blake3::Hasher::new();
    h.update(b"sigil-shielded-enc-key-v1");
    h.update(seed);
    flux_swarm_secret::SecretIdentity::from_sk_bytes(*h.finalize().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(id: &flux_swarm_secret::SecretIdentity, pk_shield: u64) -> ShieldedAddress {
        ShieldedAddress::new(BaseElement::new(pk_shield), &id.public_hex())
    }

    /// THE RECEIVING GATE: a sealed note opens for its recipient and yields the exact
    /// preimage needed to reconstruct — and later spend — the note.
    #[test]
    fn a_sealed_note_opens_for_its_recipient() {
        let bob = enc_identity_from_seed(&[0xB0; 32]);
        let bob_addr = addr(&bob, 0xBEEF);
        let pt = NotePlaintext { value: 4_242, blinding: BaseElement::new(0x1234_5678) };

        let ct = seal_note(&pt, &bob_addr).expect("seal");
        let got = try_open_note(&ct, &bob).expect("bob must be able to open his own note");
        assert_eq!(got, pt, "the recovered preimage must be exact");
    }

    /// THE PRIVACY PROPERTY: nobody else can open it. This is what lets a wallet trial-
    /// decrypt every ciphertext on the chain without learning anything about other people's.
    #[test]
    fn nobody_else_can_open_it() {
        let bob = enc_identity_from_seed(&[0xB0; 32]);
        let eve = enc_identity_from_seed(&[0xE7; 32]);
        let pt = NotePlaintext { value: 999, blinding: BaseElement::new(7) };
        let ct = seal_note(&pt, &addr(&bob, 1)).expect("seal");

        assert_eq!(
            try_open_note(&ct, &eve),
            Err(CipherError::NotForUs),
            "SECURITY: a non-recipient must not recover the note preimage"
        );
    }

    /// A tampered ciphertext must not open — the AEAD tag is what makes a successful open
    /// mean something.
    #[test]
    fn tampering_breaks_the_open() {
        let bob = enc_identity_from_seed(&[0xB0; 32]);
        let pt = NotePlaintext { value: 5, blinding: BaseElement::new(6) };
        let ct = seal_note(&pt, &addr(&bob, 1)).expect("seal");

        let mut env: serde_json::Value = serde_json::from_str(&ct.0).unwrap();
        let mut hexct = env["ct"].as_str().unwrap().to_string();
        // flip one hex nibble of the ciphertext body
        let c = hexct.remove(0);
        hexct.insert(0, if c == 'a' { 'b' } else { 'a' });
        env["ct"] = serde_json::Value::String(hexct);
        let tampered = NoteCiphertext(env.to_string());

        assert_eq!(
            try_open_note(&tampered, &bob),
            Err(CipherError::NotForUs),
            "SECURITY: a tampered ciphertext must not open"
        );
    }

    /// Sealing twice must produce different ciphertexts, or equal payments would be
    /// linkable by comparing bytes.
    #[test]
    fn sealing_is_randomized() {
        let bob = enc_identity_from_seed(&[0xB0; 32]);
        let a = addr(&bob, 1);
        let pt = NotePlaintext { value: 100, blinding: BaseElement::new(1) };
        let c1 = seal_note(&pt, &a).expect("seal");
        let c2 = seal_note(&pt, &a).expect("seal");
        assert_ne!(c1, c2, "PRIVACY: identical notes must not produce identical ciphertexts");
        // ...and both still open to the same plaintext.
        assert_eq!(try_open_note(&c1, &bob).unwrap(), pt);
        assert_eq!(try_open_note(&c2, &bob).unwrap(), pt);
    }

    #[test]
    fn address_round_trips_and_rejects_junk() {
        let bob = enc_identity_from_seed(&[0xB0; 32]);
        let a = addr(&bob, 0xABCD);
        let s = a.encode();
        assert!(s.starts_with("sigil1s:"));
        let back = ShieldedAddress::decode(&s).expect("round-trip");
        assert_eq!(back, a);
        assert_eq!(back.shield_key().unwrap(), BaseElement::new(0xABCD));

        for junk in ["", "sigil1s:", "nope:a:b", "sigil1s:a:b:c", "sigil1s::b"] {
            assert!(ShieldedAddress::decode(junk).is_err(), "must reject {junk:?}");
        }
    }

    /// A malformed plaintext must not be accepted as a note — an attacker who could make
    /// a wallet accept junk could make it track a note that does not exist.
    #[test]
    fn malformed_plaintext_is_rejected() {
        assert_eq!(NotePlaintext::decode(&[]).unwrap_err(), CipherError::MalformedPlaintext);
        assert_eq!(NotePlaintext::decode(&[0u8; 24]).unwrap_err(), CipherError::MalformedPlaintext);
        let mut wrong_magic = NotePlaintext { value: 1, blinding: BaseElement::new(1) }.encode();
        wrong_magic[0] = b'X';
        assert_eq!(
            NotePlaintext::decode(&wrong_magic).unwrap_err(),
            CipherError::MalformedPlaintext
        );
    }

    /// The encryption key must be domain-separated from the seed itself, and deterministic.
    #[test]
    fn enc_identity_is_deterministic_and_separated() {
        let a = enc_identity_from_seed(&[9u8; 32]);
        let b = enc_identity_from_seed(&[9u8; 32]);
        let c = enc_identity_from_seed(&[8u8; 32]);
        assert_eq!(a.public_hex(), b.public_hex(), "same seed ⇒ same identity");
        assert_ne!(a.public_hex(), c.public_hex(), "different seeds ⇒ different identities");
        assert_ne!(
            a.to_sk_bytes(),
            [9u8; 32],
            "the encryption secret must not BE the wallet seed"
        );
    }
}
