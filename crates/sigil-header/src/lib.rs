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
pub const NETWORK_ID: [u8; 8] = *b"sigil-g2";

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
    /// 2026-08-20: SQIsign5 + Ed25519, REQUIRE-BOTH (defense-in-depth — a break
    /// in either family alone does not forge a block). This is the real
    /// post-quantum-safe scheme for block production: unlike solo `Ed25519Hot`
    /// (classical only) or solo `SqiSign5` (PQ but currently unverifiable — see
    /// `verify_producer_sig`'s doc), this is both quantum-resistant AND
    /// verifiable today. Wire format = `flux_sqisign::hybrid::serialize_hybrid`
    /// over exactly `[SQIsign, Ed25519]` in that order — 529 bytes, fixed
    /// (2 header bytes + 426-byte SQIsign leg + 101-byte Ed25519 leg). Actual
    /// verification happens at the `sigil-node` layer (`producer_signing`),
    /// not here — see `verify_producer_sig`'s doc for why.
    HybridSqiEd25519 = 3,
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
            // 2 (version+count) + SQIsign leg [1+2+292+2+129=426] + Ed25519 leg
            // [1+2+64+2+32=101] = 529, fixed since the leg order/sizes are fixed.
            SigScheme::HybridSqiEd25519 => 529,
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
            // Not a single flat pubkey (two independent keys embedded in the
            // signature bundle itself) — unused by anything that reads this.
            SigScheme::HybridSqiEd25519 => 0,
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

    // ── topology (QTFT) ────────────────────────────────────────────────────
    /// BLAKE3, domain-tagged `SIGIL/QTFT/TOPOLOGY/V1`, over the exact
    /// Alexander polynomial of the recent DAG braid (see
    /// `SIGIL_QTFT_TOPOLOGY_v0.md`) — a real, exactly-computed topological
    /// invariant of how recent blocks merge, distinct from the four state
    /// roots (which prove the LEDGER state) and `fluxc_artifact_proof`
    /// (which proves the SOFTWARE that sealed the block). `None` when no
    /// braid context was available (genesis, linear/non-DAG mode, or too
    /// close to genesis for a full window).
    ///
    /// **INFORMATIONAL ONLY as of this field's introduction.** Nothing in
    /// `precheck()` or `verify_at_height()` inspects this field — a producer
    /// omitting it, or computing it differently, does not currently affect
    /// whether a block is accepted. It rides along, content-addressed and
    /// signed (part of `signing_bytes()` like every other field), so its
    /// history is tamper-evident from the moment it starts appearing, even
    /// though nothing enforces it yet. See
    /// [`TOPOLOGY_COMMITMENT_ACTIVATION_HEIGHT`] for how real enforcement
    /// would be turned on later, mirroring the H1 pattern above.
    ///
    /// **Second real incident on this exact field, same day (2026-08-15) —
    /// read this before touching the attributes below.** The FIRST incident
    /// (see `hash()`'s doc comment) was fixed with
    /// `#[serde(skip_serializing_if = "Option::is_none")]`, which restored
    /// JSON hash-stability for historical blocks — but `SigilBlockHeaderV0`
    /// is ALSO `bincode`-serialized throughout this codebase (`StoredBlock`
    /// in sigil-top, `BackfillResp`/`Vec<SigilBlockHeaderV0>` in
    /// sigil-node's own block-serving path). `bincode` is NOT
    /// self-describing — every field is a fixed sequence of encode/decode
    /// calls with no keys to skip by name. `skip_serializing_if` still
    /// fired during bincode ENCODE (omitting the Option's bytes entirely
    /// for `None`), but bincode DECODE has no way to know a field was
    /// skipped — it just reads the next bytes as if a normal
    /// `Option<[u8;32]>` tag+payload were there, silently misinterpreting
    /// whatever field actually came next in the buffer. Confirmed with a
    /// live round-trip test: decoding either errored (`InvalidTagEncoding`)
    /// or, worse, silently returned a structurally-valid but COMPLETELY
    /// WRONG header. This would have corrupted every bincode-served block
    /// or header for the ~99% of the chain minted before this field
    /// existed (`None`) the moment it reached production.
    ///
    /// **The fix: `skip_serializing_if` is REMOVED from the struct
    /// annotation entirely** (bincode must never see field-skipping — it
    /// always gets the plain, uniform `Option` encoding). Hash-stability
    /// for `None` historical blocks is instead handled EXPLICITLY inside
    /// `hash()`/`signing_bytes()` via string-level surgery on their own
    /// JSON output (strip the exact `,"topology_commitment":null`
    /// substring when the field is `None`) — hash-stability is now a
    /// property of those two methods specifically, not of this struct's
    /// general-purpose (de)serialization, so nothing else that
    /// JSON-or-bincode-serializes a header needs to reason about it. See
    /// `hash_is_unaffected_by_a_none_topology_commitment` (JSON stability)
    /// and `bincode_roundtrips_correctly_for_none_and_some` (the bincode
    /// regression this second incident produced).
    #[serde(default)]
    pub topology_commitment: Option<[u8; 32]>,
}

/// The exact JSON fragment `serde_json` emits for a `None`
/// `topology_commitment` with plain `#[serde(default)]` (no
/// `skip_serializing_if`) — `topology_commitment` is a fixed, known field
/// name, so a substring removal is safe and unambiguous, and doesn't depend
/// on the field's position within the struct.
const NULL_TOPOLOGY_COMMITMENT_JSON_FRAGMENT: &str = ",\"topology_commitment\":null";

/// Strip [`NULL_TOPOLOGY_COMMITMENT_JSON_FRAGMENT`] from `json`, used by
/// `hash()`/`signing_bytes()` to keep their OWN canonical bytes stable for
/// historical (`None`) blocks without affecting this struct's real
/// `Serialize` impl (which must stay `skip_serializing_if`-free for bincode
/// correctness — see the field's doc comment above).
fn strip_null_topology_commitment_for_hashing(mut json: Vec<u8>) -> Vec<u8> {
    if let Ok(s) = std::str::from_utf8(&json) {
        if let Some(pos) = s.find(NULL_TOPOLOGY_COMMITMENT_JSON_FRAGMENT) {
            json.drain(pos..pos + NULL_TOPOLOGY_COMMITMENT_JSON_FRAGMENT.len());
        }
    }
    json
}

/// Activation height for real enforcement of `topology_commitment` (e.g.
/// requiring it be present and independently recomputable, once a validator
/// registry and multi-validator committee exist to make cross-checking it
/// meaningful). **Dormant by default** — `u64::MAX` means "never active", so
/// nothing in this crate currently checks the field against this height; it
/// exists so a future enforcement pass has a named, mainnet-safe place to
/// schedule a real height, mirroring [`H1_PRODUCER_SIG_ACTIVATION_HEIGHT`].
pub const TOPOLOGY_COMMITMENT_ACTIVATION_HEIGHT: u64 = u64::MAX;

impl SigilBlockHeaderV0 {
    /// Compute the canonical block hash — BLAKE3 over the deterministic
    /// serialization. This is the block's network identity.
    pub fn hash(&self) -> BlockHash {
        let mut hasher = blake3::Hasher::new();
        // bincode/CBOR could go here; for Phase 0 use JSON canonicalization
        // because every dep is already pinned to serde_json. P1 swaps in
        // bincode for ~3× size reduction.
        if let Ok(bytes) = serde_json::to_vec(self) {
            let bytes = if self.topology_commitment.is_none() {
                strip_null_topology_commitment_for_hashing(bytes)
            } else {
                bytes
            };
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
        let bytes = serde_json::to_vec(&clone).unwrap_or_default();
        if clone.topology_commitment.is_none() {
            strip_null_topology_commitment_for_hashing(bytes)
        } else {
            bytes
        }
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

    /// Verify the producer's signature over the canonical [`signing_bytes`]
    /// (H1 fix). This is the check that block-apply historically SKIPPED — the
    /// producer wrote a zeroed `producer_sig` and no one verified it, so any peer
    /// could forge a header the network applied. Fail-closed.
    ///
    /// Only the `Ed25519Hot` scheme is verifiable from the header alone: its
    /// 32-byte `producer` ValidatorId **is** the ed25519 public key. `SqiSign5`,
    /// `Dilithium5`, and `HybridSqiEd25519` carry larger public keys not present
    /// in the header (SQIsign alone is 129 B), so they require a validator
    /// registry / pinned trusted pubkey to resolve — this crate deliberately
    /// stays dependency-light (no `flux-sqisign`/`sqisign_rs` link, so a light
    /// client including just `sigil-header` doesn't pull in ~50 MB of PQ crypto
    /// — see the module doc). `sigil-node`'s `producer_signing` module (which
    /// DOES depend on `flux-sqisign`) does the real check for
    /// `HybridSqiEd25519`, layered on top of `verify_at_height` — see
    /// `ChainTip::apply`. Until an operator wires that up, these fail closed.
    pub fn verify_producer_sig(&self) -> Result<(), HeaderError> {
        // Length must match the declared scheme before we touch crypto.
        if self.producer_sig.0.len() != self.sig_scheme.expected_sig_len() {
            return Err(HeaderError::SigLengthMismatch {
                scheme: self.sig_scheme,
                expected: self.sig_scheme.expected_sig_len(),
                got: self.producer_sig.0.len(),
            });
        }
        match self.sig_scheme {
            SigScheme::Ed25519Hot => {
                use ed25519_dalek::{Signature, VerifyingKey, Verifier};
                let vk = VerifyingKey::from_bytes(&self.producer)
                    .map_err(|_| HeaderError::ProducerSigInvalid)?;
                let sig_arr: [u8; 64] = self
                    .producer_sig
                    .0
                    .as_slice()
                    .try_into()
                    .map_err(|_| HeaderError::ProducerSigInvalid)?;
                let sig = Signature::from_bytes(&sig_arr);
                vk.verify(&self.signing_bytes(), &sig)
                    .map_err(|_| HeaderError::ProducerSigInvalid)
            }
            scheme @ (SigScheme::SqiSign5 | SigScheme::Dilithium5 | SigScheme::HybridSqiEd25519) => {
                Err(HeaderError::ProducerPubkeyUnavailable { scheme })
            }
        }
    }

    /// Height-gated validation for block apply (the H1 upgrade). Below
    /// [`H1_PRODUCER_SIG_ACTIVATION_HEIGHT`] this is `precheck()` only — exactly
    /// the legacy behaviour, so every historical block still validates under the
    /// rules that were live when it was produced. The activation height is
    /// [`u64::MAX`] by default (dormant): merging this code changes nothing on
    /// the live chain until an operator schedules a real future height.
    ///
    /// 2026-08-20 (operator-scoped activation): at/above the activation height,
    /// enforcement is SCHEME-GATED, not blanket. Only `Ed25519Hot` is checked
    /// HERE (this crate can verify it standalone). `SqiSign5`/`Dilithium5`
    /// (externally-mined/pool blocks; `SqiSign5` is also the v0 default for
    /// everything until an operator opts in) are left at precheck-only
    /// PERMANENTLY, regardless of height — `header.producer` for a solved
    /// block is part of the PoW challenge itself (bound to the miner's wallet
    /// at solve time), so the sealing node has no way to hold every miner's
    /// private key; blanket enforcement would reject every real external
    /// miner's block the moment the height hit, with no way for them to comply.
    /// `HybridSqiEd25519` (the real post-quantum-safe scheme, SQIsign+Ed25519
    /// require-both) is ALSO left alone here — this crate can't verify it
    /// (see `verify_producer_sig`'s doc) — its enforcement happens ONE LAYER
    /// UP, in `sigil-node`'s `ChainTip::apply`, as an additional check beyond
    /// this function. This makes activation genuinely safe to schedule on a
    /// chain with live external miners: it tightens exactly the blocks a node
    /// can actually sign for, and touches nothing else.
    pub fn verify_at_height(&self, apply_height: u64) -> Result<(), HeaderError> {
        self.precheck()?;
        if apply_height >= H1_PRODUCER_SIG_ACTIVATION_HEIGHT
            && self.sig_scheme == SigScheme::Ed25519Hot
        {
            self.verify_producer_sig()?;
        }
        Ok(())
    }
}

/// Activation height for H1 (producer-signature verification on block apply,
/// scheme-gated to `Ed25519Hot` only — see `verify_at_height`'s doc comment).
///
/// 2026-08-20: SET to a real future height, operator-directed. Epsilon's
/// height was ~1,940,626 at set time; true current throughput was hard to
/// pin down live (observed anywhere from near-zero to the adaptive rate
/// governor's ~60 blk/s ceiling depending on concurrent sync/backfill load at
/// the moment), so this uses a generous buffer safe under any of those rates
/// — many hours at minimum, comfortably longer at the low end — giving real
/// time to observe self-mined blocks landing correctly Ed25519-signed before
/// enforcement becomes mandatory. External miners are permanently exempt
/// (SqiSign5 stays precheck-only forever), so this activation cannot break
/// their blocks regardless of when it's reached.
pub const H1_PRODUCER_SIG_ACTIVATION_HEIGHT: u64 = 8_000_000;

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

    /// The producer signature did not verify against the producer's public key
    /// over the canonical signing bytes (H1). Forged or malleated header.
    #[error("producer signature did not verify")]
    ProducerSigInvalid,

    /// The declared scheme's public key is not present in the header (SqiSign5 /
    /// Dilithium5 carry larger keys), so the signature can't be verified without
    /// a validator registry / DNS anchor — which is not yet wired (fail-closed).
    #[error("producer pubkey unavailable for {scheme:?} — validator registry required")]
    ProducerPubkeyUnavailable { scheme: SigScheme },
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
            topology_commitment: None,
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

    /// 2026-08-23: the browser wallet's real (non-superficial) verification port
    /// needs a JS canonical serializer that produces BYTE-IDENTICAL JSON to this
    /// crate's `hash()`/`signing_bytes()` — one wrong field order or type shape
    /// and every JS-computed hash silently disagrees with the real chain. This is
    /// the cross-language conformance fixture: a header with VARIED (non-all-
    /// zero/non-empty) values in every field so a field-order or type-shape bug
    /// can't hide behind a coincidentally-matching default. Run with
    /// `--nocapture` and copy the printed JSON + hex hashes into the JS test as
    /// hardcoded expected output — this is the "prove it before trusting it"
    /// step [[feedback_verify_before_claiming_results]] calls for on money-
    /// adjacent code, not a throwaway scratch test.
    #[test]
    fn js_port_conformance_fixture() {
        let parent: [u8; 32] = {
            let mut a = [0u8; 32];
            for i in 0..32 { a[i] = (i as u8) + 1; }
            a
        };
        let nonce = SqiSignature::from_array({
            let mut a = [0u8; SQISIGN_L5_LEN];
            for i in 0..SQISIGN_L5_LEN { a[i] = ((i * 7 + 3) % 256) as u8; }
            a
        });
        let mut h = blake3::Hasher::new();
        h.update(&parent);
        h.update(nonce.as_bytes());
        let vdf_input: [u8; 32] = *h.finalize().as_bytes();
        let merge1: [u8; 32] = {
            let mut a = [0u8; 32];
            for i in 0..32 { a[i] = (200 + i) as u8; }
            a
        };

        let header = SigilBlockHeaderV0 {
            version: HEADER_VERSION,
            network_id: NETWORK_ID,
            height: 2_003_042,
            parent_hash: parent,
            merge_parents: vec![merge1],
            timestamp_ms: 1_787_500_000_123,
            nonce_sqisign: nonce,
            vdf_input,
            vdf_proof: WesolowskiProof { y: vec![1, 2, 3, 4], pi: vec![5, 6, 7], t: 12345 },
            difficulty: 987_654,
            wallet_state_root: [11u8; 32],
            dex_state_root: [22u8; 32],
            event_log_root: [33u8; 32],
            contract_state_root: [44u8; 32],
            state_transition_proof: StarkProof { bytes: vec![9, 8, 7], public_inputs_hash: [55u8; 32] },
            txs_merkle_root: [66u8; 32],
            tx_count: 17,
            fluxc_artifact_proof: ProofBundle {
                artifact_blake3: [77u8; 32],
                sqisign_sig: vec![1, 1, 2, 3, 5],
                sqisign_pubkey: vec![8, 13, 21],
                settle_tx: Some([88u8; 32]),
            },
            sig_scheme: SigScheme::Ed25519Hot,
            producer: [99u8; 32],
            producer_sig: SignatureBytes(vec![0xAB; 64]),
            topology_commitment: Some([111u8; 32]),
        };

        let hash_bytes = header.hash();
        let signing = header.signing_bytes();
        let mut sh = blake3::Hasher::new();
        sh.update(&signing);
        let signing_hash: [u8; 32] = *sh.finalize().as_bytes();

        let canonical_json = String::from_utf8(serde_json::to_vec(&header).unwrap()).unwrap();

        println!("=== JS PORT CONFORMANCE FIXTURE (topology_commitment: Some) ===");
        println!("CANONICAL_JSON={canonical_json}");
        println!("HASH_HEX={}", hex_encode(&hash_bytes));
        println!("SIGNING_BYTES_HASH_HEX={}", hex_encode(&signing_hash));

        // Second fixture: topology_commitment: None, exercising the null-strip path.
        let mut header_none = header.clone();
        header_none.topology_commitment = None;
        let hash_none = header_none.hash();
        let canonical_json_none = String::from_utf8(serde_json::to_vec(&header_none).unwrap()).unwrap();
        println!("=== JS PORT CONFORMANCE FIXTURE (topology_commitment: None) ===");
        println!("CANONICAL_JSON_NONE={canonical_json_none}");
        println!("HASH_HEX_NONE={}", hex_encode(&hash_none));

        // Sanity: the two fixtures must NOT collide (proves the field is actually
        // load-bearing in the hash, not silently dropped by the strip logic).
        assert_ne!(hash_bytes, hash_none);
    }

    fn hex_encode(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn hash_is_deterministic() {
        let h = fake_header();
        assert_eq!(h.hash(), h.hash());
    }

    // ── regression test for the 2026-08-15 snapshot-boot incident ──────────
    // A None `topology_commitment` MUST be omitted from serialization, not
    // emitted as `"topology_commitment":null` — otherwise hash() (which
    // serializes the whole struct) silently changes for every block that
    // was minted before this field existed, breaking the snapshot-boot tail
    // replay (every historical block's re-derived hash stops matching what
    // was recorded when it was minted) and forcing a full replay from
    // genesis. This bug shipped once, live, and was caught + rolled back
    // the same session — this test is what should have existed first.

    #[test]
    fn none_topology_commitment_appears_in_plain_serialize_but_not_in_hash_bytes() {
        // Plain Serialize (what bincode/generic JSON callers see) must ALWAYS
        // include the field, `null` or not — that uniformity is exactly what
        // fixes the second incident's bincode corruption. Only hash()'s own
        // canonical bytes strip it, for the FIRST incident's JSON
        // hash-stability property. Two different guarantees, two different
        // code paths — this test pins both at once so they can't drift back
        // into either bug silently.
        let h = fake_header();
        assert!(h.topology_commitment.is_none());
        let json = serde_json::to_string(&h).unwrap();
        assert!(
            json.contains("topology_commitment"),
            "plain Serialize must ALWAYS include the field (even as null) — omitting it here is \
             what broke bincode round-tripping in the 2026-08-15 incident"
        );
        let hash_bytes = strip_null_topology_commitment_for_hashing(serde_json::to_vec(&h).unwrap());
        let hash_json = std::str::from_utf8(&hash_bytes).unwrap();
        assert!(
            !hash_json.contains("topology_commitment"),
            "hash()'s OWN canonical bytes must omit a None field, or hash() changes for every \
             block minted before this field existed (the FIRST 2026-08-15 incident)"
        );
    }

    #[test]
    fn some_topology_commitment_does_appear_and_changes_the_hash() {
        // The opposite property: for a genuinely NEW block (Some(..)), the
        // field is expected to be present and to change the hash — that's
        // correct, not a bug. Only the None/historical case needs stability.
        let mut h = fake_header();
        h.topology_commitment = None;
        let hash_none = h.hash();
        h.topology_commitment = Some([9u8; 32]);
        let hash_some = h.hash();
        assert_ne!(hash_none, hash_some);
        let json = serde_json::to_string(&h).unwrap();
        assert!(json.contains("topology_commitment"), "a Some value must be present in the wire format");
    }

    #[test]
    fn bincode_roundtrips_correctly_for_none_and_some() {
        // Regression test for the SECOND 2026-08-15 incident: skip_serializing_if
        // broke bincode round-tripping for every header with topology_commitment
        // == None (i.e. every block minted before this field existed) — bincode
        // is not self-describing, so an encoder that omits bytes for a skipped
        // field leaves the decoder silently misaligned reading everything after
        // it. A Vec<Header> (exactly what sigil-node's headers-only backfill
        // serve path sends) with a None header followed by a Some header is the
        // sharpest reproduction: any misalignment corrupts BOTH entries.
        let mut none_h = fake_header();
        none_h.topology_commitment = None;
        let mut some_h = fake_header();
        some_h.topology_commitment = Some([9u8; 32]);
        let v = vec![none_h.clone(), some_h.clone()];
        let bytes = bincode::serialize(&v).expect("bincode encode must succeed");
        let back: Vec<SigilBlockHeaderV0> = bincode::deserialize(&bytes).expect("bincode decode must succeed, not misalign");
        assert_eq!(back.len(), 2);
        assert_eq!(back[0], none_h, "the None-topology header must round-trip byte-for-byte");
        assert_eq!(back[1], some_h, "the header AFTER a None-topology header must not be corrupted by it");
    }

    #[test]
    fn signing_bytes_excludes_producer_sig() {
        let h = fake_header();
        let bytes = h.signing_bytes();
        // Should not contain the producer's sig bytes pattern (all 0u8).
        // Since producer_sig.0 is all zero, this is a weak check; just verify
        // signing_bytes deserializes back with empty sig. fake_header() has
        // topology_commitment: None, so signing_bytes()'s own null-stripping
        // (same as hash(), see its doc comment) applies here too.
        let mut clone = h.clone();
        clone.producer_sig = SignatureBytes(Vec::new());
        let expected = strip_null_topology_commitment_for_hashing(serde_json::to_vec(&clone).unwrap());
        assert_eq!(expected, bytes);
    }

    /// Build a properly ed25519-signed Ed25519Hot header from a test seed.
    fn signed_ed25519_header(seed: [u8; 32]) -> SigilBlockHeaderV0 {
        use ed25519_dalek::{SigningKey, Signer};
        let sk = SigningKey::from_bytes(&seed);
        let mut h = fake_header();
        h.sig_scheme = SigScheme::Ed25519Hot;
        h.producer = sk.verifying_key().to_bytes(); // ValidatorId == ed25519 pubkey
        h.producer_sig = SignatureBytes(vec![0u8; 64]); // placeholder before signing
        let sig = sk.sign(&h.signing_bytes());          // signing_bytes zeroes the sig
        h.producer_sig = SignatureBytes(sig.to_bytes().to_vec());
        h
    }

    // ---- H1: producer-signature verification (height-gated) ----

    #[test]
    fn h1_valid_ed25519_producer_sig_verifies() {
        let h = signed_ed25519_header([42u8; 32]);
        assert!(h.verify_producer_sig().is_ok(), "honestly-signed header must verify");
    }

    #[test]
    fn h1_tampered_sig_rejected() {
        let mut h = signed_ed25519_header([42u8; 32]);
        h.producer_sig.0[0] ^= 0x01;
        assert!(matches!(h.verify_producer_sig(), Err(HeaderError::ProducerSigInvalid)));
    }

    #[test]
    fn h1_wrong_producer_key_rejected() {
        let mut h = signed_ed25519_header([42u8; 32]);
        // swap in a different producer pubkey (attacker claims someone else's slot)
        use ed25519_dalek::SigningKey;
        h.producer = SigningKey::from_bytes(&[7u8; 32]).verifying_key().to_bytes();
        assert!(matches!(h.verify_producer_sig(), Err(HeaderError::ProducerSigInvalid)));
    }

    #[test]
    fn h1_forged_zero_sig_rejected() {
        // The exact H1 attack: a header carrying the historical zeroed sig.
        let mut h = signed_ed25519_header([42u8; 32]);
        h.producer_sig = SignatureBytes(vec![0u8; 64]);
        assert!(matches!(h.verify_producer_sig(), Err(HeaderError::ProducerSigInvalid)));
    }

    #[test]
    fn h1_pq_scheme_needs_registry_fails_closed() {
        let h = fake_header(); // SqiSign5, 292-byte zero sig
        assert!(matches!(
            h.verify_producer_sig(),
            Err(HeaderError::ProducerPubkeyUnavailable { scheme: SigScheme::SqiSign5 })
        ));
    }

    #[test]
    fn h1_height_gate_below_activation_is_legacy() {
        // Below activation, verify_at_height is precheck-only: even a zeroed sig
        // (the legacy state) passes, so historical blocks still validate.
        let mut h = fake_header();
        h.producer_sig = SignatureBytes(vec![0u8; 292]);
        assert!(h.verify_at_height(1).is_ok(), "pre-activation must be legacy/precheck-only");
    }

    #[test]
    fn h1_height_gate_at_activation_enforces_sig() {
        // AT/above activation (use u64::MAX as the activated height), the sig is
        // enforced: a valid ed25519 header passes, a forged one is rejected.
        let good = signed_ed25519_header([42u8; 32]);
        assert!(good.verify_at_height(H1_PRODUCER_SIG_ACTIVATION_HEIGHT).is_ok());

        let mut forged = signed_ed25519_header([42u8; 32]);
        forged.producer_sig = SignatureBytes(vec![0u8; 64]);
        assert!(matches!(
            forged.verify_at_height(H1_PRODUCER_SIG_ACTIVATION_HEIGHT),
            Err(HeaderError::ProducerSigInvalid)
        ));
    }

    #[test]
    fn h1_activation_exempts_sqisign5_permanently() {
        // At/above activation, a SqiSign5 header (the v0 default; every
        // externally-mined/pool block, since the sealing node can't hold
        // arbitrary miners' private keys) must NOT be rejected just because
        // it carries the historical zeroed placeholder signature — real
        // enforcement only applies to Ed25519Hot. This is what makes
        // activation safe on a chain with live external miners: it can't
        // suddenly start rejecting their blocks.
        let h = fake_header(); // SqiSign5, 292-byte zero sig
        assert_eq!(h.sig_scheme, SigScheme::SqiSign5);
        assert!(
            h.verify_at_height(H1_PRODUCER_SIG_ACTIVATION_HEIGHT).is_ok(),
            "SqiSign5 must stay precheck-only even at/above activation"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Snapshot / checkpoint fast-sync WIRE TYPES (v3 sync sprint, LANE-A).
//
// Shared here so the CLIENT (sigil-top `block_sync::fetch`) and the SERVER (sigil-node
// backfill responder) compile against ONE definition. Client-only logic
// (SnapshotVerifier / pull_snapshot) stays in sigil-top. See
// docs/SIGIL_SKELETON_CODEC2_v0.md + docs/SIGIL_SNAPSHOT_PULL_DESIGN_v0.md.
// ─────────────────────────────────────────────────────────────────────────────

/// Snapshot magic — "SiGil SNapshot".
pub const SNAPSHOT_MAGIC: [u8; 4] = *b"SGSN";
/// Snapshot wire-format version.
pub const SNAPSHOT_VERSION: u16 = 1;

/// One skeleton record on the snapshot wire (codec=2 `'S'`). 72 B fixed under bincode
/// (one u64 + 2×[u8;32]). Drops the ~8 KB of PQ proofs AND the 4 state roots — they can't
/// be made sound on the prefix (B #416: the fold is PoK over peer-supplied commitments, a
/// flat order-independent sum, and roots don't chain like `parent_hash`). Trusted roots
/// come only from the frontier's real headers or the DNS anchor. `block_hash` (committed
/// BLAKE3 of the FULL header) is REQUIRED: the linkage walk checks
/// `rec[i].parent_hash == rec[i-1].block_hash` and B's fold witness is `f(block_hash)`.
/// ~113× vs 8 KB.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkeletonRecord {
    pub height: u64,
    pub block_hash: BlockHash,
    pub parent_hash: BlockHash,
}

impl SkeletonRecord {
    /// Producer side (sigil-node responder) / tests: derive a skeleton from a full header.
    pub fn from_header(h: &SigilBlockHeaderV0) -> Self {
        Self { height: h.height, block_hash: h.hash(), parent_hash: h.parent_hash }
    }
}

/// Snapshot framing prefix (codec=3 `'P'`), sent before the record stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotHeader {
    pub magic: [u8; 4],
    pub version: u16,
    pub base_height: u64,
    pub anchor_height: u64,
    pub anchor_hash: BlockHash,
    pub count: u64,
}

/// Snapshot trailer (codec=4 `'F'`), sent after the record stream. `archive_root` = BLAKE3
/// over the canonical bincode of every `SkeletonRecord`, in order; `anchor_sig` = producer
/// SQIsign over `(archive_root ‖ anchor_height ‖ anchor_hash ‖ epoch)`; `fold_blob` =
/// opaque `bincode(FoldCheckpoint)` (LANE-B decodes — empty for the M1 path).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotTrailer {
    pub archive_root: BlockHash,
    pub anchor_sig: Vec<u8>,
    pub fold_blob: Vec<u8>,
}
