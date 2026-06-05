//! sigil-header — SIGIL block header v0.
//!
//! See `SIGIL_GENESIS_v0.md` §2 for the field-by-field spec. This crate's
//! single responsibility is the on-the-wire layout, deterministic
//! serialization, and a content-addressed hash. Cryptographic verification
//! (SQIsign, VDF, STARK, fluxc-proof) is delegated to the relevant crates and
//! invoked by the consensus layer, NOT here.
//!
//! Why this matters: keeping the schema crate dependency-light means a light
//! client can include `sigil-header` to compute parent-pointer chains
//! without pulling in 50 MB of PQ crypto.

#![warn(missing_docs)]

use serde::{Deserialize, Serialize};

/// Network ID baked into every header. Prevents replay across the
/// Quillon/SIGIL boundary. See genesis §1 lock #3.
pub const NETWORK_ID: [u8; 8] = *b"sigil-g0";

/// Header schema version. Bumped only when the on-wire layout changes.
/// Code that reads a foreign-version header MUST refuse and not guess.
pub const HEADER_VERSION: u16 = 0;

/// Hash output of `SigilBlockHeaderV0::hash()` — BLAKE3-256, the canonical
/// block identifier across consensus / db / RPC / wallet layers.
pub type BlockHash = [u8; 32];

/// Validator pubkey identifier, content-addressed.
pub type ValidatorId = [u8; 32];

/// 32-byte sparse-merkle-tree / merkle root, used for all four state roots
/// plus the tx_merkle_root.
pub type Root = [u8; 32];

/// Crypto-agile signature scheme tag — height-gated via `flux-eternal-cypher`
/// when that crate ports from Quillon. v0 defaults to SQIsign Level 5 (292 B
/// signatures, 16× smaller than Dilithium5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum SigScheme {
    /// SQIsign Level 5 — 292 B signatures. The v0 default.
    SqiSign5   = 0,
    /// Dilithium Level 5 — 4595 B signatures. Available as fallback per
    /// crypto-agility lock.
    Dilithium5 = 1,
    /// Ed25519 — 64 B signatures, 32 B pubkeys. The CLASSICAL hot-path scheme
    /// (crypto-agility split, Stargate handoff #4): batch×parallel verify at
    /// ~10^5–10^6/s for the high-frequency agent-tx hot path, while SqiSign5
    /// stays the post-quantum SETTLEMENT scheme. NOT post-quantum — must be
    /// gated to the hot path; never for settlement/finality.
    Ed25519Hot = 2,
}

/// Variable-length signature bytes. The concrete length is determined by
/// `SigScheme` (292 for SqiSign5, 4595 for Dilithium5). Validation MUST
/// check that `len() == scheme.expected_sig_len()`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignatureBytes(pub Vec<u8>);

/// Variable-length public-key bytes carried on a `SignedTx` so the verifier
/// has the full scheme pubkey (a 32-byte WalletId can't hold a 129-byte
/// SQIsign key). Length is checked against `SigScheme::expected_pubkey_len`;
/// the account binding is `WalletId == BLAKE3(pubkey)`, enforced at verify.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PubKeyBytes(pub Vec<u8>);

impl SigScheme {
    /// Bytes the producer signature is expected to occupy under this scheme.
    pub fn expected_sig_len(self) -> usize {
        match self {
            SigScheme::SqiSign5   => 292,
            SigScheme::Dilithium5 => 4595,
            SigScheme::Ed25519Hot => 64,
        }
    }

    /// Bytes the PUBLIC KEY occupies under this scheme. A 32-byte WalletId
    /// can't hold a 129-byte SQIsign key, so a SignedTx carries the full
    /// pubkey and the verifier checks its length against this.
    pub fn expected_pubkey_len(self) -> usize {
        match self {
            SigScheme::SqiSign5   => 129,  // flux_sqisign::public_key_size()
            SigScheme::Dilithium5 => 2592, // Dilithium5 public key (NIST FIPS-204)
            SigScheme::Ed25519Hot => 32,   // ed25519 compressed Edwards-Y point
        }
    }
}

/// SQIsign signature, expected to be exactly 292 bytes. Used for
/// `nonce_sqisign` whose scheme is locked (not crypto-agile — the nonce IS
/// the SQIsign sig). Stored as `Vec<u8>` to avoid the serde-big-array
/// dependency; constructors validate the length.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SqiSignature(pub Vec<u8>);

/// Expected byte length of an SQIsign Level 5 signature.
pub const SQISIGN_L5_LEN: usize = 292;

impl SqiSignature {
    /// Construct from a 292-byte array. Use this when the caller has fixed
    /// bytes (e.g. fresh from the signer) — no allocation at the call site
    /// other than the `Vec`.
    pub fn from_array(bytes: [u8; SQISIGN_L5_LEN]) -> Self {
        Self(bytes.to_vec())
    }

    /// Construct from a vec, validating the length matches
    /// [`SQISIGN_L5_LEN`]. Returns `None` if the length is wrong — header
    /// validation surfaces that to the consensus layer.
    pub fn from_vec(bytes: Vec<u8>) -> Option<Self> {
        if bytes.len() == SQISIGN_L5_LEN {
            Some(Self(bytes))
        } else {
            None
        }
    }

    /// Read-only borrow of the signature bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// True iff the wrapped vec is exactly [`SQISIGN_L5_LEN`] bytes.
    pub fn is_well_formed(&self) -> bool {
        self.0.len() == SQISIGN_L5_LEN
    }
}

/// Wesolowski VDF output proof. Phase 0 placeholder — real type comes from
/// `flux-vdf` when that crate ports. Keeping the shape stable here lets the
/// rest of the system compile against it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WesolowskiProof {
    /// VDF output `y = x^(2^t) mod N`.
    pub y: Vec<u8>,
    /// Pietrzak/Wesolowski proof.
    pub pi: Vec<u8>,
    /// Difficulty parameter `t` (squaring count), surfaced for fast pre-check
    /// before invoking the verifier.
    pub t: u64,
}

/// STARK proof attesting that header.state_roots = apply_txs(parent_state, txs).
/// Phase 0 placeholder — real type comes from `flux-zk-stark`. The 10ms verify
/// gate is enforced by `flux-zk` when wiring lands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StarkProof {
    /// Opaque proof bytes, verifier-defined.
    pub bytes: Vec<u8>,
    /// Public inputs digest, content-addressed.
    pub public_inputs_hash: [u8; 32],
}

/// fluxc provenance bundle — BLAKE3 + SQIsign by the agent who built the
/// producer binary. Tracked here so every block height carries the
/// cryptographic record of which binary it was produced by. Phase 0
/// placeholder — real type lives in `fluxc-core::provenance::ProvenanceProof`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofBundle {
    /// BLAKE3 of the producer binary.
    pub artifact_blake3: [u8; 32],
    /// SQIsign signature over the canonical bundle by the agent wallet.
    pub sqisign_sig: Vec<u8>,
    /// Agent's SQIsign pubkey.
    pub sqisign_pubkey: Vec<u8>,
    /// Optional on-chain settle_tx hash linking the build to a swarm payment.
    pub settle_tx: Option<[u8; 32]>,
}

/// SIGIL block header v0 — schema-locked. Every field is mandatory; producers
/// cannot omit any. See genesis §2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SigilBlockHeaderV0 {
    // ── identity ───────────────────────────────────────────────────────────
    /// Schema version, always [`HEADER_VERSION`] for v0.
    pub version: u16,
    /// Network ID, always [`NETWORK_ID`] for the SIGIL `g0` genesis.
    pub network_id: [u8; 8],
    /// Block height, monotonically increasing from 0.
    pub height: u64,
    /// BLAKE3 hash of the parent block's header (the GHOSTDAG *selected*
    /// parent — the spine).
    pub parent_hash: BlockHash,
    /// DagKnight: ADDITIONAL DAG parents this block merges — the other tips it
    /// observed beyond `parent_hash`. COMMITTED in the header, so they are
    /// hashed (in `hash()`) AND signed (in `signing_bytes()`/`producer_sig`)
    /// and cannot be malleated. Empty for a linear or genesis block.
    #[serde(default)]
    pub merge_parents: Vec<BlockHash>,
    /// Producer's wall-clock at the moment the block was sealed (ms since
    /// UNIX epoch). NOT consensus-critical; informational only.
    pub timestamp_ms: u64,

    // ── mining ─────────────────────────────────────────────────────────────
    /// SQIsign signature over `(parent_hash || height_le || producer)` —
    /// the cryptographic nonce. Cannot be replayed or forged.
    pub nonce_sqisign: SqiSignature,
    /// VDF input = `BLAKE3(parent_hash || nonce_sqisign.0)`.
    pub vdf_input: [u8; 32],
    /// Wesolowski VDF proof attesting the time-bound work was done.
    pub vdf_proof: WesolowskiProof,
    /// Adaptive difficulty parameter, per `ConservativeVDFParams`.
    pub difficulty: u64,

    // ── state roots (THE Quillon fix) ──────────────────────────────────────
    /// SMT root over all wallet balances at end of this block.
    pub wallet_state_root: Root,
    /// SMT root over DEX state (pools + LP + accrued fees).
    pub dex_state_root: Root,
    /// Merkle root over the typed event log emitted in this block.
    pub event_log_root: Root,
    /// SMT root over VM contract storage.
    pub contract_state_root: Root,

    // ── proof of transition correctness ────────────────────────────────────
    /// STARK proof that the four state roots above are the correct result of
    /// applying `txs` to the parent state. ≤10 ms verify.
    pub state_transition_proof: StarkProof,
    /// Merkle root over the tx hashes included in this block.
    pub txs_merkle_root: Root,
    /// Number of verify-once transactions committed by `txs_merkle_root`. The
    /// receiver sums this across blocks to measure end-to-end TPS without
    /// re-verifying. `#[serde(default)]` keeps pre-tx-count blocks decoding to 0.
    #[serde(default)]
    pub tx_count: u32,

    // ── provenance (the Flux dividend) ─────────────────────────────────────
    /// `.proof` bundle of the producer binary that sealed this block.
    pub fluxc_artifact_proof: ProofBundle,

    // ── authorship ─────────────────────────────────────────────────────────
    /// Signature scheme for `producer_sig`. v0 default is SQIsign5.
    pub sig_scheme: SigScheme,
    /// Producer's validator ID.
    pub producer: ValidatorId,
    /// Producer's signature over the *unsigned* header bytes (every field
    /// above, with `producer_sig` itself zeroed out for canonicalization).
    pub producer_sig: SignatureBytes,
}

impl SigilBlockHeaderV0 {
    /// Compute the canonical block hash — BLAKE3 over the deterministic
    /// serialization. This is the block's network identity.
    pub fn hash(&self) -> BlockHash {
        let mut hasher = blake3::Hasher::new();
        // bincode/CBOR could go here; for Phase 0 use JSON canonicalization
        // because every dep is already pinned to serde_json. P1 swaps in
        // bincode for ~3× size reduction.
        if let Ok(bytes) = serde_json::to_vec(self) {
            hasher.update(&bytes);
        }
        *hasher.finalize().as_bytes()
    }

    /// Canonical bytes used as the SIGNED payload for `producer_sig`. Returns
    /// the header serialized with `producer_sig` zeroed out, so the signature
    /// can't sign over itself. Verification: zero out `producer_sig`,
    /// re-serialize, verify scheme/sig against the producer's pubkey.
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut clone = self.clone();
        clone.producer_sig = SignatureBytes(Vec::new());
        serde_json::to_vec(&clone).unwrap_or_default()
    }

    /// Cheap pre-validation that catches obvious schema breakage before any
    /// crypto. Mandatory in `sigil-node`'s ingress path. Does NOT validate
    /// signatures or VDF — those are wired separately when the relevant
    /// crates land.
    pub fn precheck(&self) -> Result<(), HeaderError> {
        if self.version != HEADER_VERSION {
            return Err(HeaderError::WrongVersion {
                expected: HEADER_VERSION,
                got: self.version,
            });
        }
        if self.network_id != NETWORK_ID {
            return Err(HeaderError::WrongNetwork {
                expected: NETWORK_ID,
                got: self.network_id,
            });
        }
        if self.producer_sig.0.len() != self.sig_scheme.expected_sig_len() {
            return Err(HeaderError::SigLengthMismatch {
                scheme: self.sig_scheme,
                expected: self.sig_scheme.expected_sig_len(),
                got: self.producer_sig.0.len(),
            });
        }
        if !self.nonce_sqisign.is_well_formed() {
            return Err(HeaderError::NonceLengthMismatch {
                expected: SQISIGN_L5_LEN,
                got: self.nonce_sqisign.0.len(),
            });
        }
        // VDF input MUST = BLAKE3(parent_hash || nonce_sqisign).
        let mut h = blake3::Hasher::new();
        h.update(&self.parent_hash);
        h.update(self.nonce_sqisign.as_bytes());
        let expected: [u8; 32] = *h.finalize().as_bytes();
        if expected != self.vdf_input {
            return Err(HeaderError::VdfInputMismatch);
        }
        Ok(())
    }
}

/// Header-layer validation errors. Crypto-layer errors live in the relevant
/// crates (flux-sqisign, flux-vdf, flux-zk-stark).
#[derive(Debug, thiserror::Error)]
pub enum HeaderError {
    /// Header.version didn't match [`HEADER_VERSION`].
    #[error("wrong header version: expected {expected}, got {got}")]
    WrongVersion { expected: u16, got: u16 },

    /// Header.network_id didn't match [`NETWORK_ID`].
    #[error("wrong network id: expected {expected:?}, got {got:?}")]
    WrongNetwork { expected: [u8; 8], got: [u8; 8] },

    /// producer_sig byte length didn't match what the declared scheme expects.
    #[error("sig length mismatch under {scheme:?}: expected {expected}, got {got}")]
    SigLengthMismatch {
        scheme: SigScheme,
        expected: usize,
        got: usize,
    },

    /// VDF input wasn't BLAKE3(parent_hash || nonce_sqisign) — header is
    /// internally inconsistent and any further validation is pointless.
    #[error("vdf_input != blake3(parent_hash || nonce_sqisign)")]
    VdfInputMismatch,

    /// `nonce_sqisign` was not exactly [`SQISIGN_L5_LEN`] bytes long.
    #[error("nonce_sqisign length wrong: expected {expected}, got {got}")]
    NonceLengthMismatch {
        /// Expected length (always [`SQISIGN_L5_LEN`]).
        expected: usize,
        /// Length actually present on the wire.
        got: usize,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_header() -> SigilBlockHeaderV0 {
        let parent: [u8; 32] = [9u8; 32];
        let nonce = SqiSignature::from_array([7u8; SQISIGN_L5_LEN]);
        let mut h = blake3::Hasher::new();
        h.update(&parent);
        h.update(nonce.as_bytes());
        let vdf_input: [u8; 32] = *h.finalize().as_bytes();

        SigilBlockHeaderV0 {
            version: HEADER_VERSION,
            network_id: NETWORK_ID,
            height: 1,
            parent_hash: parent,
            merge_parents: vec![],
            timestamp_ms: 1_780_000_000_000,
            nonce_sqisign: nonce,
            vdf_input,
            vdf_proof: WesolowskiProof { y: vec![], pi: vec![], t: 100 },
            difficulty: 0,
            wallet_state_root: [0u8; 32],
            dex_state_root: [0u8; 32],
            event_log_root: [0u8; 32],
            contract_state_root: [0u8; 32],
            state_transition_proof: StarkProof { bytes: vec![], public_inputs_hash: [0u8; 32] },
            txs_merkle_root: [0u8; 32],
            tx_count: 0,
            fluxc_artifact_proof: ProofBundle {
                artifact_blake3: [0u8; 32],
                sqisign_sig: vec![],
                sqisign_pubkey: vec![],
                settle_tx: None,
            },
            sig_scheme: SigScheme::SqiSign5,
            producer: [0u8; 32],
            producer_sig: SignatureBytes(vec![0u8; 292]),
        }
    }

    #[test]
    fn precheck_accepts_well_formed_header() {
        assert!(fake_header().precheck().is_ok());
    }

    #[test]
    fn precheck_rejects_wrong_version() {
        let mut h = fake_header();
        h.version = 99;
        assert!(matches!(h.precheck(), Err(HeaderError::WrongVersion { .. })));
    }

    #[test]
    fn precheck_rejects_wrong_network() {
        let mut h = fake_header();
        h.network_id = *b"badbadg0";
        assert!(matches!(h.precheck(), Err(HeaderError::WrongNetwork { .. })));
    }

    #[test]
    fn precheck_rejects_sig_length_mismatch() {
        let mut h = fake_header();
        h.producer_sig = SignatureBytes(vec![0u8; 1]);
        assert!(matches!(h.precheck(), Err(HeaderError::SigLengthMismatch { .. })));
    }

    #[test]
    fn precheck_rejects_vdf_input_mismatch() {
        let mut h = fake_header();
        h.vdf_input = [42u8; 32];
        assert!(matches!(h.precheck(), Err(HeaderError::VdfInputMismatch)));
    }

    #[test]
    fn hash_is_deterministic() {
        let h = fake_header();
        assert_eq!(h.hash(), h.hash());
    }

    #[test]
    fn signing_bytes_excludes_producer_sig() {
        let h = fake_header();
        let bytes = h.signing_bytes();
        // Should not contain the producer's sig bytes pattern (all 0u8).
        // Since producer_sig.0 is all zero, this is a weak check; just verify
        // signing_bytes deserializes back with empty sig.
        let mut clone = h.clone();
        clone.producer_sig = SignatureBytes(Vec::new());
        assert_eq!(serde_json::to_vec(&clone).unwrap(), bytes);
    }
}
