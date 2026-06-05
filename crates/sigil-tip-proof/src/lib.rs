//! sigil-tip-proof — the wire artifact a joining node verifies in ≤10 ms
//! instead of downloading the chain.
//!
//! See `SIGIL_PROTOTYPE_4_SCOPE.md` for the prototype framing. The wire
//! shape is locked at v0; the **flavor** field tags which crypto backs the
//! `signature` bytes so producer and verifier can swap STARK in later
//! without breaking format compatibility:
//!
//! | Flavor                | What it is                                              | Status |
//! |-----------------------|---------------------------------------------------------|--------|
//! | `Blake3Fingerprint`   | BLAKE3 over canonical bytes — typo-resistant, NOT secure | v0 ✓   |
//! | `SqiSignBlob`         | SQIsign5 producer signature over canonical bytes        | P4.1   |
//! | `StarkRecursive`      | flux-recursive-proofs::tip_proof_v2 over chain prefix   | P4.2   |
//!
//! The v0 flavor exists so the **end-to-end pipeline ships now** (producer
//! emits, verifier reads, browser demos verify-in-10 ms) while the real
//! crypto chokepoints catch up. A joining node treats `Blake3Fingerprint`
//! as a typo-prevention check only — the doc strings on
//! [`TipProof::verify`] say so loudly. Operators who care about adversarial
//! safety wait for the SQIsign or STARK flavors before trusting the
//! light-client output.

#![warn(missing_docs)]

pub mod observatory;

use serde::{Deserialize, Serialize};

use sigil_header::Root;
use sigil_state::StateRoots;

/// Network ID this format is locked to. Receiver-side rejection of a
/// mismatch is one of the first checks in [`TipProof::verify`].
/// `native` reads it from `sigil_net`; the light/WASM build mirrors the same
/// const so it never pulls the transport stack just for 8 bytes.
#[cfg(feature = "native")]
pub const NETWORK_ID_BYTES: &[u8] = sigil_net::NETWORK_ID;
#[cfg(not(feature = "native"))]
pub const NETWORK_ID_BYTES: &[u8] = b"sigil-g0";

/// Wire-format version. Receivers refuse non-zero values until a v1
/// migration ships. Bump in lockstep across producer + verifier.
pub const TIP_PROOF_VERSION: u16 = 0;

/// Cryptographic backing of the `signature` field. See module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TipProofFlavor {
    /// BLAKE3 over the canonical [`TipProof::signing_bytes`]. Catches typos
    /// and bit-rot on the wire; provides **no** adversary resistance.
    Blake3Fingerprint,
    /// SQIsign5 signature over [`TipProof::signing_bytes`]. Lands when
    /// `flux-eternal-cypher` chokepoint ports. Reserved tag.
    SqiSignBlob,
    /// `flux-recursive-proofs::tip_proof_v2::Proof` bytes. Lands in P4.2.
    /// Reserved tag.
    StarkRecursive,
}

impl TipProofFlavor {
    /// Is this flavor adversary-resistant under the threat model SIGIL
    /// nodes commit to once they join the network? Joining nodes that get
    /// only `Blake3Fingerprint` should refuse to expose a `/balance` query
    /// API to external users until a stronger flavor lands.
    pub fn adversary_resistant(self) -> bool {
        matches!(self, TipProofFlavor::SqiSignBlob | TipProofFlavor::StarkRecursive)
    }

    /// Compact tag used in log lines + future binary serializations. Stable
    /// — don't renumber.
    pub fn tag(self) -> u8 {
        match self {
            TipProofFlavor::Blake3Fingerprint => 0,
            TipProofFlavor::SqiSignBlob       => 1,
            TipProofFlavor::StarkRecursive    => 2,
        }
    }
}

/// One tip-proof. Published on `/sigil/g0/tip-proofs` after each block
/// commits. The wire shape is the same regardless of flavor — only the
/// interpretation of `signature` differs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TipProof {
    /// Format version. Must equal [`TIP_PROOF_VERSION`] on the wire.
    pub version: u16,
    /// SIGIL network this proof attests to. Verifier rejects mismatch
    /// against its own `sigil_net::NETWORK_ID`.
    pub network_id: Vec<u8>,
    /// Block height being attested.
    pub height: u64,
    /// The four committed state roots at this height.
    pub roots: StateRoots,
    /// Backing-crypto tag for [`signature`].
    pub flavor: TipProofFlavor,
    /// Variable-length signature/proof bytes. For `Blake3Fingerprint`:
    /// 32 bytes (the BLAKE3 of canonical bytes). For `SqiSignBlob`: 292
    /// bytes (SQIsign5). For `StarkRecursive`: tip_proof_v2's serialized
    /// proof (size depends on chain prefix depth).
    pub signature: Vec<u8>,
}

/// Errors surfaced by [`TipProof::verify`]. Each is distinguishable so the
/// joining node can route HTTP status / log severity correctly.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TipProofError {
    /// `version` didn't match [`TIP_PROOF_VERSION`].
    #[error("wrong version: expected {expected}, got {got}")]
    WrongVersion { expected: u16, got: u16 },
    /// `network_id` didn't match what the verifier was configured for.
    #[error("network id mismatch: expected {expected:?}, got {got:?}")]
    WrongNetworkId { expected: Vec<u8>, got: Vec<u8> },
    /// Signature bytes were the wrong length for the declared flavor.
    #[error("signature length: flavor {flavor:?} expected {expected}, got {got}")]
    SigLengthMismatch { flavor: TipProofFlavor, expected: usize, got: usize },
    /// `Blake3Fingerprint`: the bytes did not match BLAKE3(signing_bytes).
    #[error("blake3 fingerprint mismatch — wire bit-rot or non-canonical encoding")]
    Blake3Mismatch,
    /// STARK path isn't ported yet. Joining node should fall back to a
    /// different verification strategy (or refuse to operate in adversarial
    /// mode).
    #[error("flavor not yet implemented: {0:?}")]
    NotImplemented(TipProofFlavor),
    /// `SqiSignBlob`: the SQIsign signature did not verify under the supplied
    /// producer public key — either a forgery or the wrong producer key.
    #[error("sqisign signature did not verify under the producer key")]
    SqiSignInvalid,
    /// `SqiSignBlob`: backend error (malformed key or signature encoding).
    #[error("sqisign backend error: {0}")]
    SqiSignBackend(String),
}

impl TipProof {
    /// Build a v0 [`TipProofFlavor::Blake3Fingerprint`] proof for the given
    /// roots. The signature is BLAKE3 over [`Self::signing_bytes`].
    pub fn new_blake3(height: u64, roots: StateRoots) -> Self {
        let mut proof = Self {
            version: TIP_PROOF_VERSION,
            network_id: NETWORK_ID_BYTES.to_vec(),
            height,
            roots,
            flavor: TipProofFlavor::Blake3Fingerprint,
            signature: Vec::new(),
        };
        let fp = blake3::hash(&proof.signing_bytes());
        proof.signature = fp.as_bytes().to_vec();
        proof
    }

    /// Build a [`TipProofFlavor::SqiSignBlob`] proof: the producer signs the
    /// canonical [`Self::signing_bytes`] with its **SQIsign Level-5** secret key
    /// (292-byte signature). Unlike [`Self::new_blake3`], this is
    /// **adversary-resistant** — only the holder of the producer secret key can
    /// produce a signature that verifies under the producer public key, so a
    /// "self-consistent but false" seal is no longer forgeable. The verifier
    /// pins the producer public key out of band (genesis-embedded or
    /// DNS-anchored) and calls [`Self::verify_sqisign`].
    #[cfg(feature = "native")]
    pub fn new_sqisign(
        height: u64,
        roots: StateRoots,
        sk_bytes: &[u8],
        pk_bytes: &[u8],
    ) -> Result<Self, TipProofError> {
        let mut proof = Self {
            version: TIP_PROOF_VERSION,
            network_id: NETWORK_ID_BYTES.to_vec(),
            height,
            roots,
            flavor: TipProofFlavor::SqiSignBlob,
            signature: Vec::new(),
        };
        let sig = flux_sqisign::sign(&proof.signing_bytes(), sk_bytes, pk_bytes)
            .map_err(TipProofError::SqiSignBackend)?;
        proof.signature = sig;
        Ok(proof)
    }

    /// Canonical bytes that the signature covers. For SqiSignBlob and
    /// StarkRecursive, the producer signs/proves these same bytes — that's
    /// what keeps the wire shape stable across flavor upgrades.
    ///
    /// Layout: `version (u16 LE) || len(network_id) (u32 LE) || network_id ||
    /// height (u64 LE) || wallet_root || dex_root || event_root ||
    /// contract_root`. NO trailing flavor tag, NO trailing signature — the
    /// signature commits to everything *before* it.
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(2 + 4 + self.network_id.len() + 8 + 32 * 4);
        buf.extend_from_slice(&self.version.to_le_bytes());
        buf.extend_from_slice(&(self.network_id.len() as u32).to_le_bytes());
        buf.extend_from_slice(&self.network_id);
        buf.extend_from_slice(&self.height.to_le_bytes());
        buf.extend_from_slice(&self.roots.wallet_state_root);
        buf.extend_from_slice(&self.roots.dex_state_root);
        buf.extend_from_slice(&self.roots.event_log_root);
        buf.extend_from_slice(&self.roots.contract_state_root);
        buf
    }

    /// Verify the proof against the verifier's expected `network_id`.
    ///
    /// **Security caveat (v0):** Successful verification of a
    /// `Blake3Fingerprint` flavor proves only that the wire bytes weren't
    /// corrupted in transit. It does NOT prove the height/roots are
    /// honest — any peer can fabricate one. Joining nodes that need
    /// adversarial safety must wait for `SqiSignBlob` / `StarkRecursive`.
    /// [`TipProofFlavor::adversary_resistant`] surfaces this distinction
    /// in code.
    pub fn verify(&self, expected_network_id: &[u8]) -> Result<(), TipProofError> {
        if self.version != TIP_PROOF_VERSION {
            return Err(TipProofError::WrongVersion {
                expected: TIP_PROOF_VERSION, got: self.version,
            });
        }
        if self.network_id != expected_network_id {
            return Err(TipProofError::WrongNetworkId {
                expected: expected_network_id.to_vec(),
                got: self.network_id.clone(),
            });
        }
        match self.flavor {
            TipProofFlavor::Blake3Fingerprint => {
                if self.signature.len() != 32 {
                    return Err(TipProofError::SigLengthMismatch {
                        flavor: self.flavor, expected: 32, got: self.signature.len(),
                    });
                }
                let want = blake3::hash(&self.signing_bytes());
                if want.as_bytes()[..] != self.signature[..] {
                    return Err(TipProofError::Blake3Mismatch);
                }
                Ok(())
            }
            // SqiSignBlob needs the producer public key — call verify_sqisign.
            // StarkRecursive isn't ported yet.
            TipProofFlavor::SqiSignBlob | TipProofFlavor::StarkRecursive => {
                Err(TipProofError::NotImplemented(self.flavor))
            }
        }
    }

    /// Verify a [`TipProofFlavor::SqiSignBlob`] proof against the verifier's
    /// expected network id **and a pinned producer public key**. This is the
    /// adversary-resistant path: the proof is trusted only if the SQIsign
    /// signature verifies under a producer key the verifier independently pins
    /// (genesis-embedded or DNS-anchored). A forged or tampered seal fails here
    /// — you cannot produce the producer's signature without its secret key.
    #[cfg(feature = "native")]
    pub fn verify_sqisign(
        &self,
        expected_network_id: &[u8],
        producer_pk: &[u8],
    ) -> Result<(), TipProofError> {
        if self.version != TIP_PROOF_VERSION {
            return Err(TipProofError::WrongVersion { expected: TIP_PROOF_VERSION, got: self.version });
        }
        if self.network_id != expected_network_id {
            return Err(TipProofError::WrongNetworkId {
                expected: expected_network_id.to_vec(),
                got: self.network_id.clone(),
            });
        }
        if self.flavor != TipProofFlavor::SqiSignBlob {
            return Err(TipProofError::NotImplemented(self.flavor));
        }
        match flux_sqisign::verify(&self.signing_bytes(), &self.signature, producer_pk) {
            Ok(true) => Ok(()),
            Ok(false) => Err(TipProofError::SqiSignInvalid),
            Err(e) => Err(TipProofError::SqiSignBackend(e)),
        }
    }

    /// Encode as JSON bytes for gossipsub publication on
    /// `sigil_net::TOPIC_TIP_PROOFS`. Binary encoding (bincode/postcard)
    /// lands with the storage P3 binary-format pass.
    pub fn encode_json(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }

    /// Decode from JSON bytes. The companion of [`Self::encode_json`].
    pub fn decode_json(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }

    /// Stable on-the-wire identifier for this proof — BLAKE3 of the
    /// canonical signing bytes regardless of flavor. Useful for dedup in
    /// gossipsub seen-cache, logs, indexers.
    pub fn fingerprint(&self) -> Root {
        *blake3::hash(&self.signing_bytes()).as_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_roots() -> StateRoots {
        StateRoots {
            wallet_state_root:   [0x11; 32],
            dex_state_root:      [0x22; 32],
            event_log_root:      [0x33; 32],
            contract_state_root: [0x44; 32],
        }
    }

    #[test]
    fn new_blake3_round_trips_through_verify() {
        let proof = TipProof::new_blake3(42, fake_roots());
        proof.verify(NETWORK_ID_BYTES).expect("self-produced proof must verify");
        assert_eq!(proof.flavor, TipProofFlavor::Blake3Fingerprint);
        assert_eq!(proof.height, 42);
        assert_eq!(proof.signature.len(), 32);
    }

    #[test]
    fn signing_bytes_is_deterministic() {
        let a = TipProof::new_blake3(42, fake_roots());
        let b = TipProof::new_blake3(42, fake_roots());
        assert_eq!(a.signing_bytes(), b.signing_bytes());
        assert_eq!(a.fingerprint(), b.fingerprint());
    }

    #[test]
    fn json_round_trip_preserves_verification() {
        let proof = TipProof::new_blake3(7, fake_roots());
        let bytes = proof.encode_json();
        let decoded = TipProof::decode_json(&bytes).unwrap();
        decoded.verify(NETWORK_ID_BYTES).expect("decoded proof must verify");
        assert_eq!(decoded.height, 7);
        assert_eq!(decoded.signature, proof.signature);
    }

    #[test]
    fn tampered_height_breaks_blake3() {
        let mut proof = TipProof::new_blake3(42, fake_roots());
        proof.height = 43; // tamper without recomputing the signature
        let err = proof.verify(NETWORK_ID_BYTES).unwrap_err();
        assert_eq!(err, TipProofError::Blake3Mismatch);
    }

    #[test]
    fn tampered_roots_break_blake3() {
        let mut proof = TipProof::new_blake3(42, fake_roots());
        proof.roots.wallet_state_root = [0xFF; 32];
        let err = proof.verify(NETWORK_ID_BYTES).unwrap_err();
        assert_eq!(err, TipProofError::Blake3Mismatch);
    }

    #[test]
    fn sqisign_roundtrips_rejects_tamper_and_wrong_key() {
        // Honest-limit #1 fix: the SqiSignBlob flavor is adversary-resistant.
        let (sk, pk) = flux_sqisign::keygen();
        let proof = TipProof::new_sqisign(99, fake_roots(), &sk, &pk).expect("sign");
        assert_eq!(proof.flavor, TipProofFlavor::SqiSignBlob);
        // valid under the right producer key
        proof.verify_sqisign(NETWORK_ID_BYTES, &pk).expect("genuine proof must verify");
        // tampering a root after signing → signature no longer matches
        let mut tampered = TipProof::new_sqisign(99, fake_roots(), &sk, &pk).expect("sign");
        tampered.roots.wallet_state_root = [0xFF; 32];
        assert_eq!(tampered.verify_sqisign(NETWORK_ID_BYTES, &pk), Err(TipProofError::SqiSignInvalid));
        // a different (attacker) producer key → rejected
        let (_sk2, pk2) = flux_sqisign::keygen();
        assert_eq!(proof.verify_sqisign(NETWORK_ID_BYTES, &pk2), Err(TipProofError::SqiSignInvalid));
        // wrong network id → rejected before crypto
        assert!(matches!(proof.verify_sqisign(b"other-net", &pk), Err(TipProofError::WrongNetworkId { .. })));
    }

    #[test]
    fn wrong_network_id_rejected() {
        let proof = TipProof::new_blake3(42, fake_roots());
        let err = proof.verify(b"some-other-net").unwrap_err();
        assert!(matches!(err, TipProofError::WrongNetworkId { .. }));
    }

    #[test]
    fn wrong_version_rejected() {
        let mut proof = TipProof::new_blake3(42, fake_roots());
        proof.version = 99;
        let err = proof.verify(NETWORK_ID_BYTES).unwrap_err();
        assert!(matches!(err, TipProofError::WrongVersion { expected: 0, got: 99 }));
    }

    #[test]
    fn signature_length_mismatch_caught() {
        let mut proof = TipProof::new_blake3(42, fake_roots());
        proof.signature = vec![0u8; 16]; // wrong size for Blake3Fingerprint
        let err = proof.verify(NETWORK_ID_BYTES).unwrap_err();
        assert!(matches!(err, TipProofError::SigLengthMismatch { expected: 32, got: 16, .. }));
    }

    #[test]
    fn sqisign_via_keyless_verify_needs_producer_key() {
        // The SqiSignBlob flavor IS implemented (see verify_sqisign), but the
        // keyless verify() cannot check it — it has no producer key — so it
        // correctly routes the caller to verify_sqisign via NotImplemented.
        let mut proof = TipProof::new_blake3(42, fake_roots());
        proof.flavor = TipProofFlavor::SqiSignBlob;
        let err = proof.verify(NETWORK_ID_BYTES).unwrap_err();
        assert!(matches!(err, TipProofError::NotImplemented(TipProofFlavor::SqiSignBlob)));
    }

    #[test]
    fn flavor_adversary_resistance() {
        assert!(!TipProofFlavor::Blake3Fingerprint.adversary_resistant());
        assert!(TipProofFlavor::SqiSignBlob.adversary_resistant());
        assert!(TipProofFlavor::StarkRecursive.adversary_resistant());
    }

    #[test]
    fn flavor_tag_is_stable() {
        assert_eq!(TipProofFlavor::Blake3Fingerprint.tag(), 0);
        assert_eq!(TipProofFlavor::SqiSignBlob.tag(), 1);
        assert_eq!(TipProofFlavor::StarkRecursive.tag(), 2);
    }
}
