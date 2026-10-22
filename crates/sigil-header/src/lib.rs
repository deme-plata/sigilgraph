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
/// Quillon/SIGIL boundary, and between SIGIL networks. See genesis §1 lock #3.
///
/// Chosen at build time by `SIGIL_NETWORK` (see `build.rs`); unset = `sigil-g2`.
/// It is part of the genesis header, so each network has its own genesis hash, and
/// `precheck` refuses a header from any other network.
pub const NETWORK_ID: [u8; 8] = network_id_bytes(NETWORK_ID_STR);

/// String form of [`NETWORK_ID`].
pub const NETWORK_ID_STR: &str = env!("SIGIL_NET_ID");

/// The `sigil-g3` family (2026-10-19, Viktor: "sigil-g3, clean start, chain rule"): the reset of
/// g2, and `sigil-r3`, its rehearsal twin that runs the identical rules on the docker test chain
/// so the launch binary is rehearsed without a second chain ever calling itself g3.
pub const fn g3_family(net: &[u8; 8]) -> bool {
    bytes8_eq(net, b"sigil-g3") || bytes8_eq(net, b"sigil-r3")
}
/// Is this binary built for the g3 family? See [`g3_family`].
pub const IS_G3: bool = g3_family(&NETWORK_ID);

/// The activation height of a switch that is ON in today's g2 (`g2_height` is where g2 turned
/// it on): 0 on the g3 family, so the reset chain starts with everything g2 runs today, and
/// unchanged everywhere else, so g2/r1/m1 replay their history byte-identically.
pub const fn live_from_genesis_on_g3(g2_height: u64) -> u64 {
    if IS_G3 { 0 } else { g2_height }
}

const fn network_id_bytes(s: &str) -> [u8; 8] {
    let b = s.as_bytes();
    assert!(b.len() == 8, "network id must be exactly 8 bytes");
    let mut out = [0u8; 8];
    let mut i = 0;
    while i < 8 {
        out[i] = b[i];
        i += 1;
    }
    out
}

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
    /// Commitment over the SHIELDED POOL at the end of this block — notes root, note
    /// count, nullifier multiset, value locked, epoch and every sealed epoch's root
    /// (`sigil_state::ShieldedPool::pool_root`, the fifth root).
    ///
    /// Since transparent sends were retired the pool is where the money lives, and until
    /// 2026-09-25 it was the one piece of consensus state NO header field covered: two
    /// nodes could disagree on every note and nullifier and still carry identical
    /// headers (`ShieldedPool::digest` existed for exactly this and had no caller).
    ///
    /// Height-gated: `None` below [`SHIELDED_POOL_ROOT_ACTIVATION_HEIGHT`] and `Some`
    /// REQUIRED from it — [`precheck`](Self::precheck) refuses a missing root at/after the
    /// gate AND a premature one before it, so old and new binaries agree on every pre-gate
    /// block. Hash/signature stability for the ~26 M historical blocks is the
    /// `topology_commitment` recipe: plain `#[serde(default)]` (bincode stays uniform —
    /// never `skip_serializing_if`, see the 2026-08-15 incident above) and the exact
    /// `,"shielded_pool_root":null` fragment is stripped from the JSON that `hash()` and
    /// `signing_bytes()` digest while the field is `None`.
    #[serde(default)]
    pub shielded_pool_root: Option<[u8; 32]>,
    /// EXTENSIONS (2026-10-19, sigil-g3): the one field future header data goes into, as
    /// numbered entries, so the positional layouts below never need another legacy struct.
    /// Empty on every network today. `hash()`/`signing_bytes()` strip the empty field's
    /// JSON (`,"extensions":[]`), so every existing block hash is unchanged. What a `kind`
    /// means — and from which height a node must understand it — is a consensus rule that
    /// arrives with a header `version` bump ([`header_version_allowed`]); until then
    /// [`SigilBlockHeaderV0::precheck`] refuses any extension at all.
    #[serde(default)]
    pub extensions: Vec<HeaderExtension>,
}

/// One numbered piece of header data (see [`SigilBlockHeaderV0::extensions`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeaderExtension {
    pub kind: u16,
    pub data: Vec<u8>,
}

/// The exact JSON fragment `serde_json` emits for an empty `extensions` (always the last
/// field, so the fragment is unambiguous).
const EMPTY_EXTENSIONS_JSON_FRAGMENT: &str = ",\"extensions\":[]";

/// Which header versions a node accepts, and from which height (2026-10-19, sigil-g3).
/// A future header change adds a row here and bumps `version` at that height: an older
/// node then refuses the first new header plainly ("unsupported version — upgrade")
/// instead of hashing it differently and silently parking. Today: version 0 everywhere.
const HEADER_VERSIONS: &[(u16, u64)] = &[(HEADER_VERSION, 0)];

/// May a header at `height` carry `version`? See [`HEADER_VERSIONS`].
pub fn header_version_allowed(version: u16, height: u64) -> bool {
    HEADER_VERSIONS.iter().any(|&(v, from)| v == version && height >= from)
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

/// The exact JSON fragment `serde_json` emits for a `None` `shielded_pool_root`
/// (same shape and same reasoning as [`NULL_TOPOLOGY_COMMITMENT_JSON_FRAGMENT`]).
const NULL_SHIELDED_POOL_ROOT_JSON_FRAGMENT: &str = ",\"shielded_pool_root\":null";
/// Strip [`NULL_SHIELDED_POOL_ROOT_JSON_FRAGMENT`] from `json` — `hash()` /
/// `signing_bytes()` only, while the field is `None`, so every block minted before the
/// field existed keeps its hash and its producer signature byte-for-byte.
fn strip_null_shielded_pool_root_for_hashing(mut json: Vec<u8>) -> Vec<u8> {
    if let Ok(s) = std::str::from_utf8(&json) {
        if let Some(pos) = s.find(NULL_SHIELDED_POOL_ROOT_JSON_FRAGMENT) {
            json.drain(pos..pos + NULL_SHIELDED_POOL_ROOT_JSON_FRAGMENT.len());
        }
    }
    json
}
/// Canonical bytes for `hash()` / `signing_bytes()`: the header's JSON with the `null`
/// fragments of the two height-gated optional fields removed while they are `None`.
fn canonical_json_for_hashing(json: Vec<u8>, topology_none: bool, pool_root_none: bool) -> Vec<u8> {
    let json = if topology_none { strip_null_topology_commitment_for_hashing(json) } else { json };
    let json = if pool_root_none { strip_null_shielded_pool_root_for_hashing(json) } else { json };
    strip_empty_extensions_for_hashing(json)
}

/// Strip [`EMPTY_EXTENSIONS_JSON_FRAGMENT`] (2026-10-19): a header without extensions hashes
/// exactly as it did before the field existed. Only the LAST occurrence is a candidate, and
/// only when the JSON ends with it plus `}` — the field is the struct's last.
fn strip_empty_extensions_for_hashing(mut json: Vec<u8>) -> Vec<u8> {
    let frag = EMPTY_EXTENSIONS_JSON_FRAGMENT.as_bytes();
    if json.len() >= frag.len() + 1 && json.ends_with(b"}") && json[json.len() - 1 - frag.len()..json.len() - 1] == *frag {
        let end = json.len() - 1;
        json.drain(end - frag.len()..end);
    }
    json
}
/// From this height every header MUST carry `Some(shielded_pool_root)`; below it every
/// header MUST carry `None` (both directions are refused by [`SigilBlockHeaderV0::precheck`]
/// and by the follower's root check in `sigil-node`). Scheduled 2026-09-25 at tip ≈ 25.6 M:
/// ~1.4 M blocks ahead — two days at the 8 blk/s floor, ~3.5 h at the 110 blk/s ceiling —
/// so every `sigil-top` client can auto-update before the first gated block is minted.
/// A producer cannot be rolled back out of its own blocks: if this must move, move it
/// BEFORE the tip reaches it, never after.
pub const SHIELDED_POOL_ROOT_ACTIVATION_HEIGHT: u64 = live_from_genesis_on_g3(27_000_000);
thread_local! {
    // THREAD-LOCAL on purpose: a simulation lowers the gate on ITS thread only, so parallel
    // test threads (the chronos library, the tourbillon and frontier soaks) never see each
    // other's gate flip mid-run. Production never sets it; every thread reads the const.
    static SHIELDED_POOL_ROOT_ACTIVATION_OVERRIDE: std::cell::Cell<u64> = const { std::cell::Cell::new(u64::MAX) };
}
/// The live activation height: [`SHIELDED_POOL_ROOT_ACTIVATION_HEIGHT`] unless a
/// simulation lowered it on this thread via
/// [`override_shielded_pool_root_activation_for_simulation`].
pub fn shielded_pool_root_activation_height() -> u64 {
    let o = SHIELDED_POOL_ROOT_ACTIVATION_OVERRIDE.with(|c| c.get());
    if o == u64::MAX { SHIELDED_POOL_ROOT_ACTIVATION_HEIGHT } else { o }
}
/// SIMULATION ONLY (chronos, integration tests): lower the gate so a chain built from
/// genesis crosses it. Thread-local; never called by `sigil-node start`. `u64::MAX`
/// restores the compiled-in height.
#[doc(hidden)]
pub fn override_shielded_pool_root_activation_for_simulation(height: u64) {
    SHIELDED_POOL_ROOT_ACTIVATION_OVERRIDE.with(|c| c.set(height));
}
/// Does a header at `height` have to carry the shielded pool root?
pub fn shielded_pool_root_required(height: u64) -> bool {
    shielded_pool_root_required_with(height, shielded_pool_root_activation_height())
}
/// Pure form of [`shielded_pool_root_required`] (testable without the global).
pub fn shielded_pool_root_required_with(height: u64, activation: u64) -> bool {
    height >= activation
}

/// V7 POOL ACTIVATION (2026-10-12): the block at which the shielded pool seals its 64-bit
/// (v1, MiMC) epoch and opens a 256-bit (v7, Rescue-Prime) generation — see
/// `sigil_state::shielded::ShieldedPool::activate_v7`. Per network, baked in at build time:
/// the rehearsal chain crosses it early; g2 and m1 are `u64::MAX` (never) until scheduled.
/// A producer cannot be rolled back out of its own blocks: if this must move, move it
/// BEFORE the tip reaches it, never after.
pub const V7_POOL_ACTIVATION_HEIGHT: u64 = v7_activation_for(&NETWORK_ID);
const fn v7_activation_for(net: &[u8; 8]) -> u64 {
    // g3 family: the genesis block itself opens v7 — and ONLY v7 (see
    // `ShieldedPool::activate_v7_only`); no 64-bit note ever exists on it.
    if g3_family(net) {
        0
    } else if bytes8_eq(net, b"sigil-r1") {
        1_000
    } else {
        u64::MAX
    }
}
const fn bytes8_eq(a: &[u8; 8], b: &[u8; 8]) -> bool {
    let mut i = 0;
    while i < 8 {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}
thread_local! {
    static V7_POOL_ACTIVATION_OVERRIDE: std::cell::Cell<u64> = const { std::cell::Cell::new(u64::MAX) };
}
/// The live v7 activation height: [`V7_POOL_ACTIVATION_HEIGHT`] unless a simulation lowered
/// it on this thread via [`override_v7_pool_activation_for_simulation`].
pub fn v7_pool_activation_height() -> u64 {
    let o = V7_POOL_ACTIVATION_OVERRIDE.with(|c| c.get());
    if o == u64::MAX { V7_POOL_ACTIVATION_HEIGHT } else { o }
}
/// SIMULATION ONLY: lower the v7 gate so a chain built from genesis crosses it. Thread-local;
/// never called by `sigil-node start`. `u64::MAX` restores the compiled-in height.
#[doc(hidden)]
pub fn override_v7_pool_activation_for_simulation(height: u64) {
    V7_POOL_ACTIVATION_OVERRIDE.with(|c| c.set(height));
}
/// Is the v7 note generation active at `height`?
pub fn v7_pool_active(height: u64) -> bool {
    height >= v7_pool_activation_height()
}
/// The gate rule as one pure function: present ⇔ required. Used by `precheck` and by
/// the follower's root check, so the two can never drift.
pub fn check_shielded_pool_root_gate(height: u64, present: bool, activation: u64) -> Result<(), HeaderError> {
    let required = shielded_pool_root_required_with(height, activation);
    match (required, present) {
        (true, false) => Err(HeaderError::ShieldedPoolRootMissing { height, activation }),
        (false, true) => Err(HeaderError::ShieldedPoolRootPremature { height, activation }),
        _ => Ok(()),
    }
}
// ─── The header-page WIRE: legacy layout vs the fifth-root layout ──────────────────
//
// Every header page on the backfill wire (`serve_read` codecs 0 `'H'` and 1 `'Z'`), every
// client decoder (`sigil-top`, `sigil-serve`) and the client block store encode headers
// with POSITIONAL bincode. Positional means a missing trailing field is `UnexpectedEof`,
// never `#[serde(default)]` — so the moment a node serialized the fifth root under the
// old codecs, every deployed client would misalign on every page, before any gate. The
// rule here: the old codecs keep carrying the OLD byte layout (this view, byte-identical
// to what was written before 2026-09-25 — pinned by `legacy_page_is_byte_identical…`),
// and the full layout rides its own codec, [`HEADER_PAGE_CODEC_V1`] / tag `'V'`.
// Below the activation height the two are lossless mirrors of each other. From the
// gate, a client still on the legacy codecs receives headers WITHOUT the root, hashes
// them differently, and parks — which is why clients ship before the gate.

/// The pre-fifth-root bincode layout of [`SigilBlockHeaderV0`]: every field except
/// `shielded_pool_root`, in the same order. Its bincode is byte-identical to what every
/// node and client produced before the field existed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SigilBlockHeaderV0Legacy {
    pub version: u16,
    pub network_id: [u8; 8],
    pub height: u64,
    pub parent_hash: BlockHash,
    #[serde(default)]
    pub merge_parents: Vec<BlockHash>,
    pub timestamp_ms: u64,
    pub nonce_sqisign: SqiSignature,
    pub vdf_input: [u8; 32],
    pub vdf_proof: WesolowskiProof,
    pub difficulty: u64,
    pub wallet_state_root: Root,
    pub dex_state_root: Root,
    pub event_log_root: Root,
    pub contract_state_root: Root,
    pub state_transition_proof: StarkProof,
    pub txs_merkle_root: Root,
    #[serde(default)]
    pub tx_count: u32,
    pub fluxc_artifact_proof: ProofBundle,
    pub sig_scheme: SigScheme,
    pub producer: ValidatorId,
    pub producer_sig: SignatureBytes,
    #[serde(default)]
    pub topology_commitment: Option<[u8; 32]>,
}

impl From<&SigilBlockHeaderV0> for SigilBlockHeaderV0Legacy {
    /// Drops the fifth root. Lossless below the activation height (it is `None` there).
    fn from(h: &SigilBlockHeaderV0) -> Self {
        Self {
            version: h.version, network_id: h.network_id, height: h.height, parent_hash: h.parent_hash,
            merge_parents: h.merge_parents.clone(), timestamp_ms: h.timestamp_ms,
            nonce_sqisign: h.nonce_sqisign.clone(), vdf_input: h.vdf_input, vdf_proof: h.vdf_proof.clone(),
            difficulty: h.difficulty, wallet_state_root: h.wallet_state_root, dex_state_root: h.dex_state_root,
            event_log_root: h.event_log_root, contract_state_root: h.contract_state_root,
            state_transition_proof: h.state_transition_proof.clone(), txs_merkle_root: h.txs_merkle_root,
            tx_count: h.tx_count, fluxc_artifact_proof: h.fluxc_artifact_proof.clone(), sig_scheme: h.sig_scheme,
            producer: h.producer, producer_sig: h.producer_sig.clone(), topology_commitment: h.topology_commitment,
        }
    }
}

impl From<SigilBlockHeaderV0Legacy> for SigilBlockHeaderV0 {
    /// A legacy-layout header has no fifth root: `None`, exactly what every pre-gate
    /// block carries. (A gated block read through this path hashes differently — that
    /// is the client's signal that it must speak codec 5.)
    fn from(l: SigilBlockHeaderV0Legacy) -> Self {
        Self {
            version: l.version, network_id: l.network_id, height: l.height, parent_hash: l.parent_hash,
            merge_parents: l.merge_parents, timestamp_ms: l.timestamp_ms, nonce_sqisign: l.nonce_sqisign,
            vdf_input: l.vdf_input, vdf_proof: l.vdf_proof, difficulty: l.difficulty,
            wallet_state_root: l.wallet_state_root, dex_state_root: l.dex_state_root,
            event_log_root: l.event_log_root, contract_state_root: l.contract_state_root,
            state_transition_proof: l.state_transition_proof, txs_merkle_root: l.txs_merkle_root,
            tx_count: l.tx_count, fluxc_artifact_proof: l.fluxc_artifact_proof, sig_scheme: l.sig_scheme,
            producer: l.producer, producer_sig: l.producer_sig, topology_commitment: l.topology_commitment,
            shielded_pool_root: None,
            extensions: Vec::new(),
        }
    }
}

/// Backfill header-page codecs (`BackfillReq.codec`). 0/1/2 are the frozen pre-fifth-root
/// wires; 3/4 are the snapshot header/trailer and live elsewhere; **5 is the fifth-root
/// layout** — `'V'` + zstd-1 of `bincode(Vec<SigilBlockHeaderV0>)`.
pub const HEADER_PAGE_CODEC_LEGACY_RAW: u8 = 0;
pub const HEADER_PAGE_CODEC_LEGACY_ZSTD: u8 = 1;
pub const HEADER_PAGE_CODEC_SKELETON: u8 = 2;
pub const HEADER_PAGE_CODEC_V1: u8 = 5;
/// Frame tag of a codec-5 page.
pub const HEADER_PAGE_TAG_V1: u8 = b'V';
/// Is `codec` one of the header-page codecs a node answers with a header run
/// (as opposed to the tip-dependent snapshot codecs 3/4)?
pub fn is_header_page_codec(codec: u8) -> bool {
    codec < 3 || codec == HEADER_PAGE_CODEC_V1
}
/// The body of a legacy (`'H'` / inside `'Z'`) page: `bincode(Vec<SigilBlockHeaderV0Legacy>)`.
pub fn encode_legacy_header_page(headers: &[SigilBlockHeaderV0]) -> Vec<u8> {
    let v: Vec<SigilBlockHeaderV0Legacy> = headers.iter().map(SigilBlockHeaderV0Legacy::from).collect();
    bincode::serialize(&v).unwrap_or_default()
}
/// Decode a legacy page body into full headers (fifth root `None`).
pub fn decode_legacy_header_page(body: &[u8]) -> Option<Vec<SigilBlockHeaderV0>> {
    let v: Vec<SigilBlockHeaderV0Legacy> = bincode::deserialize(body).ok()?;
    Some(v.into_iter().map(SigilBlockHeaderV0::from).collect())
}
/// The fifth-root positional layout: the legacy fields, then `shielded_pool_root` — and no
/// `extensions`. Positional bincode writes a nested struct as its fields in order, so this
/// is byte-identical to what every node, page and store wrote before 2026-10-19.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SigilBlockHeaderV0FifthRoot {
    pub base: SigilBlockHeaderV0Legacy,
    pub shielded_pool_root: Option<[u8; 32]>,
}
impl From<&SigilBlockHeaderV0> for SigilBlockHeaderV0FifthRoot {
    /// Drops `extensions` (empty on every network outside the g3 family).
    fn from(h: &SigilBlockHeaderV0) -> Self {
        Self { base: SigilBlockHeaderV0Legacy::from(h), shielded_pool_root: h.shielded_pool_root }
    }
}
impl From<SigilBlockHeaderV0FifthRoot> for SigilBlockHeaderV0 {
    fn from(f: SigilBlockHeaderV0FifthRoot) -> Self {
        let mut h = SigilBlockHeaderV0::from(f.base);
        h.shielded_pool_root = f.shielded_pool_root;
        h
    }
}
/// The body of a codec-5 page (inside `'V'` + zstd). Outside the g3 family it carries the
/// fifth-root layout — byte-identical to every page ever served; on the g3 family (whose
/// nodes and clients are all built from this layout) the full header with `extensions`.
pub fn encode_v1_header_page(headers: &[SigilBlockHeaderV0]) -> Vec<u8> {
    if IS_G3 {
        bincode::serialize(&headers.to_vec()).unwrap_or_default()
    } else {
        let v: Vec<SigilBlockHeaderV0FifthRoot> = headers.iter().map(SigilBlockHeaderV0FifthRoot::from).collect();
        bincode::serialize(&v).unwrap_or_default()
    }
}
/// Decode a codec-5 page body (the layout [`encode_v1_header_page`] writes on this network).
pub fn decode_v1_header_page(body: &[u8]) -> Option<Vec<SigilBlockHeaderV0>> {
    if IS_G3 {
        bincode::deserialize::<Vec<SigilBlockHeaderV0>>(body).ok()
    } else {
        let v: Vec<SigilBlockHeaderV0FifthRoot> = bincode::deserialize(body).ok()?;
        Some(v.into_iter().map(SigilBlockHeaderV0::from).collect())
    }
}
/// Decode ONE stored header record of either layout, distinguished by a tag byte the
/// writer prepends: [`STORED_HEADER_TAG_V1`] + `bincode(full)`, or (no tag — every record
/// written before 2026-09-25) `bincode(legacy)`. Unambiguous because a legacy record
/// begins with `version: u16 = 0` (byte `0x00`) and the tag is `0x01`.
pub const STORED_HEADER_TAG_V1: u8 = 0x01;
/// 2026-10-19: tag of a record in the full layout (with `extensions`), written only on the
/// g3 family. Outside it the writer keeps tag `0x01` and the fifth-root bytes.
pub const STORED_HEADER_TAG_V2: u8 = 0x02;
pub fn encode_stored_header(h: &SigilBlockHeaderV0) -> Vec<u8> {
    if IS_G3 {
        let mut out = vec![STORED_HEADER_TAG_V2];
        out.extend(bincode::serialize(h).unwrap_or_default());
        out
    } else {
        let mut out = vec![STORED_HEADER_TAG_V1];
        out.extend(bincode::serialize(&SigilBlockHeaderV0FifthRoot::from(h)).unwrap_or_default());
        out
    }
}
pub fn decode_stored_header(bytes: &[u8]) -> Option<SigilBlockHeaderV0> {
    match bytes.first()? {
        &STORED_HEADER_TAG_V2 => bincode::deserialize::<SigilBlockHeaderV0>(&bytes[1..]).ok(),
        &STORED_HEADER_TAG_V1 => bincode::deserialize::<SigilBlockHeaderV0FifthRoot>(&bytes[1..]).ok().map(SigilBlockHeaderV0::from),
        _ => bincode::deserialize::<SigilBlockHeaderV0Legacy>(bytes).ok().map(SigilBlockHeaderV0::from),
    }
}

/// Activation height for real enforcement of `topology_commitment` (e.g.
/// requiring it be present and independently recomputable, once a validator
/// registry and multi-validator committee exist to make cross-checking it
/// meaningful). **Dormant by default** — `u64::MAX` means "never active", so
/// nothing in this crate currently checks the field against this height; it
/// exists so a future enforcement pass has a named, mainnet-safe place to
/// schedule a real height, mirroring [`H1_PRODUCER_SIG_ACTIVATION_HEIGHT`].
pub const TOPOLOGY_COMMITMENT_ACTIVATION_HEIGHT: u64 = u64::MAX;

/// The canonical VDF-input derivation: `BLAKE3(parent_hash || nonce)`.
///
/// The producer (`sigil-node`'s `mint::mint_next_block`) and the verifier
/// ([`SigilBlockHeaderV0::precheck`]) MUST derive this byte-for-byte
/// identically. If they ever drift, every block a producer mints fails
/// `precheck` with [`HeaderError::VdfInputMismatch`] and the chain stalls
/// with no obviously-wrong value anywhere. Defining it once, here in the
/// crate both sides depend on, makes that drift impossible.
pub fn vdf_input_from(parent_hash: &[u8; 32], nonce: &[u8]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(parent_hash);
    h.update(nonce);
    *h.finalize().as_bytes()
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
            let bytes = canonical_json_for_hashing(
                bytes, self.topology_commitment.is_none(), self.shielded_pool_root.is_none(),
            );
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
        canonical_json_for_hashing(bytes, clone.topology_commitment.is_none(), clone.shielded_pool_root.is_none())
    }

    /// Cheap pre-validation that catches obvious schema breakage before any
    /// crypto. Mandatory in `sigil-node`'s ingress path. Does NOT validate
    /// signatures or VDF — those are wired separately when the relevant
    /// crates land.
    pub fn precheck(&self) -> Result<(), HeaderError> {
        // 2026-10-19: no extension kind is defined yet, so none may appear (a future kind
        // arrives with a version bump, and an old node refuses it right here).
        if !self.extensions.is_empty() {
            return Err(HeaderError::UnknownExtension { kind: self.extensions[0].kind });
        }
        if !header_version_allowed(self.version, self.height) {
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
        // The fifth root is present exactly from its activation height (see the field).
        check_shielded_pool_root_gate(
            self.height, self.shielded_pool_root.is_some(), shielded_pool_root_activation_height(),
        )?;
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
        let expected = vdf_input_from(&self.parent_hash, self.nonce_sqisign.as_bytes());
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
                // 2026-10-22 (sync speed): decoding the key (a point decompression) was ~3 % of a syncing node's
                // busy thread, and every block of a chain carries the same producer key. Remember the last
                // decoded key per thread; the same 32 bytes always decode to the same key.
                thread_local! {
                    static LAST_VK: std::cell::Cell<Option<([u8; 32], VerifyingKey)>> = const { std::cell::Cell::new(None) };
                }
                let vk = match LAST_VK.with(|c| c.get()).filter(|(k, _)| *k == self.producer) {
                    Some((_, vk)) => vk,
                    None => {
                        let vk = VerifyingKey::from_bytes(&self.producer)
                            .map_err(|_| HeaderError::ProducerSigInvalid)?;
                        LAST_VK.with(|c| c.set(Some((self.producer, vk))));
                        vk
                    }
                };
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
pub const H1_PRODUCER_SIG_ACTIVATION_HEIGHT: u64 = live_from_genesis_on_g3(8_000_000);

/// Header-layer validation errors. Crypto-layer errors live in the relevant
/// crates (flux-sqisign, flux-vdf, flux-zk-stark).
#[derive(Debug, thiserror::Error)]
pub enum HeaderError {
    /// Header.version didn't match [`HEADER_VERSION`].
    #[error("wrong header version: expected {expected}, got {got}")]
    WrongVersion { expected: u16, got: u16 },

    /// 2026-10-19: the header carries an extension this binary does not know — made by a
    /// newer node (upgrade this one) or invalid. See [`SigilBlockHeaderV0::extensions`].
    #[error("unknown header extension kind {kind} — this node is older than the block (upgrade)")]
    UnknownExtension { kind: u16 },

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
    /// Height ≥ the shielded-pool-root activation, but the header carries no root.
    #[error("shielded_pool_root missing at height {height} (required from {activation})")]
    ShieldedPoolRootMissing { height: u64, activation: u64 },
    /// Height < the activation, but the header already carries a root — would hash
    /// differently on every node that has not activated yet.
    #[error("shielded_pool_root present at height {height} before activation {activation}")]
    ShieldedPoolRootPremature { height: u64, activation: u64 },

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

    /// 2026-10-19: an empty `extensions` changes no hash — the field's JSON is stripped.
    #[test]
    fn empty_extensions_leave_the_hash_unchanged() {
        let h = fake_header();
        let json = serde_json::to_vec(&h).unwrap();
        assert!(std::str::from_utf8(&json).unwrap().ends_with(",\"extensions\":[]}"));
        let without = canonical_json_for_hashing(json.clone(), h.topology_commitment.is_none(), h.shielded_pool_root.is_none());
        assert!(!std::str::from_utf8(&without).unwrap().contains("extensions"), "stripped before hashing");
        let mut with_ext = h.clone();
        with_ext.extensions.push(HeaderExtension { kind: 1, data: vec![1, 2, 3] });
        assert_ne!(with_ext.hash(), h.hash(), "a non-empty extension IS part of the hash");
    }

    /// 2026-10-19: no extension kind exists yet, so precheck refuses any.
    #[test]
    fn precheck_refuses_any_extension_until_a_kind_is_defined() {
        let mut h = fake_header();
        assert!(h.precheck().is_ok());
        h.extensions.push(HeaderExtension { kind: 7, data: vec![] });
        assert!(matches!(h.precheck(), Err(HeaderError::UnknownExtension { kind: 7 })));
    }

    /// 2026-10-19: the version table — version 0 everywhere today, nothing else.
    #[test]
    fn header_version_table() {
        assert!(header_version_allowed(HEADER_VERSION, 0));
        assert!(header_version_allowed(HEADER_VERSION, u64::MAX));
        assert!(!header_version_allowed(HEADER_VERSION + 1, 0));
    }

    /// 2026-10-19: stored headers round-trip, and outside the g3 family the writer still
    /// emits tag 0x01 with the fifth-root bytes — byte-identical to before.
    #[test]
    fn stored_header_layouts_round_trip() {
        let h = fake_header();
        let rec = encode_stored_header(&h);
        assert_eq!(decode_stored_header(&rec).unwrap(), h);
        if IS_G3 {
            assert_eq!(rec[0], STORED_HEADER_TAG_V2, "the g3 family writes the full layout");
        }
        if !IS_G3 {
            assert_eq!(rec[0], STORED_HEADER_TAG_V1);
            let mut old = vec![STORED_HEADER_TAG_V1];
            old.extend(bincode::serialize(&SigilBlockHeaderV0FifthRoot::from(&h)).unwrap());
            assert_eq!(rec, old, "non-g3 records are the fifth-root bytes");
        }
        let mut full = vec![STORED_HEADER_TAG_V2];
        full.extend(bincode::serialize(&h).unwrap());
        assert_eq!(decode_stored_header(&full).unwrap(), h, "a full-layout record decodes on every build");
        let page = encode_v1_header_page(std::slice::from_ref(&h));
        assert_eq!(decode_v1_header_page(&page).unwrap(), vec![h]);
    }

    fn fake_header() -> SigilBlockHeaderV0 {
        let parent: [u8; 32] = [9u8; 32];
        let nonce = SqiSignature::from_array([7u8; SQISIGN_L5_LEN]);
        // Build the field through the SAME shared fn precheck uses, so
        // `precheck_accepts_well_formed_header` also proves the producer's
        // derivation is accepted.
        let vdf_input: [u8; 32] = vdf_input_from(&parent, nonce.as_bytes());

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
            // a root exactly when the gate requires one: None on g2 (height 1 is far below its
            // gate), present on the sigil-g3 family (required from genesis) — 2026-10-19
            shielded_pool_root: shielded_pool_root_required(1).then_some([0u8; 32]),
            extensions: Vec::new(),
        }
    }

    #[test]
    fn precheck_accepts_well_formed_header() {
        assert!(fake_header().precheck().is_ok());
    }

    #[test]
    fn vdf_input_from_is_blake3_of_parent_then_nonce() {
        let parent = [3u8; 32];
        let nonce = [5u8; SQISIGN_L5_LEN];
        // Known-answer: recomputed independently in the canonical order.
        let mut h = blake3::Hasher::new();
        h.update(&parent);
        h.update(&nonce);
        assert_eq!(vdf_input_from(&parent, &nonce), *h.finalize().as_bytes());
        // Order matters — parent-then-nonce must NOT equal nonce-then-parent.
        let mut swapped = blake3::Hasher::new();
        swapped.update(&nonce);
        swapped.update(&parent);
        assert_ne!(vdf_input_from(&parent, &nonce), *swapped.finalize().as_bytes());
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
            shielded_pool_root: None,
            extensions: Vec::new(),
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

    #[test]
    fn hash_commits_to_every_consensus_field() {
        // The block hash is its NETWORK IDENTITY. If it omitted a field, two
        // different blocks could collide, or that field could be tampered without
        // changing the identity. Assert a one-bit flip of each consensus-relevant
        // field changes the hash.
        let base = fake_header();
        let h0 = base.hash();

        let mut h = base.clone(); h.height ^= 1; assert_ne!(h.hash(), h0, "height");
        let mut h = base.clone(); h.parent_hash[0] ^= 1; assert_ne!(h.hash(), h0, "parent_hash");
        let mut h = base.clone(); h.vdf_input[0] ^= 1; assert_ne!(h.hash(), h0, "vdf_input");
        let mut h = base.clone(); h.wallet_state_root[0] ^= 1; assert_ne!(h.hash(), h0, "wallet_state_root");
        let mut h = base.clone(); h.dex_state_root[0] ^= 1; assert_ne!(h.hash(), h0, "dex_state_root");
        let mut h = base.clone(); h.event_log_root[0] ^= 1; assert_ne!(h.hash(), h0, "event_log_root");
        let mut h = base.clone(); h.contract_state_root[0] ^= 1; assert_ne!(h.hash(), h0, "contract_state_root");
        let mut h = base.clone(); h.txs_merkle_root[0] ^= 1; assert_ne!(h.hash(), h0, "txs_merkle_root");
        let mut h = base.clone(); h.producer[0] ^= 1; assert_ne!(h.hash(), h0, "producer");

        // A PRESENT topology_commitment is committed too (the None case is stripped
        // for cross-version stability — covered by the regression test below).
        let mut h = base.clone();
        h.topology_commitment = Some([9u8; 32]);
        assert_ne!(h.hash(), h0, "a present topology_commitment must be committed");
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

    /// THE FIFTH ROOT keeps every historical hash: while the field is `None` the JSON
    /// `hash()` / `signing_bytes()` digest is byte-identical to what a header without the
    /// field produced (the fragment is emitted by serde and stripped before hashing).
    #[test]
    fn hash_and_signing_bytes_are_unaffected_by_a_none_shielded_pool_root() {
        let mut h = fake_header();
        h.shielded_pool_root = None; // the None case is this test's subject (fake_header carries one on g3)
        assert!(h.shielded_pool_root.is_none() && h.topology_commitment.is_none());
        let raw = serde_json::to_vec(&h).unwrap();
        let raw_s = std::str::from_utf8(&raw).unwrap();
        assert!(raw_s.contains(NULL_SHIELDED_POOL_ROOT_JSON_FRAGMENT), "serde emits the null fragment (no skip_serializing_if)");
        // the pre-field canonical bytes: strip BOTH null fragments by hand
        // 2026-10-19: the empty `extensions` field did not exist before either — strip it too.
        let legacy: Vec<u8> = raw_s
            .replacen(NULL_TOPOLOGY_COMMITMENT_JSON_FRAGMENT, "", 1)
            .replacen(NULL_SHIELDED_POOL_ROOT_JSON_FRAGMENT, "", 1)
            .replacen(EMPTY_EXTENSIONS_JSON_FRAGMENT, "", 1)
            .into_bytes();
        assert!(!std::str::from_utf8(&legacy).unwrap().contains("shielded_pool_root"));
        let expected = *blake3::hash(&legacy).as_bytes();
        assert_eq!(h.hash(), expected, "a None fifth root must not move the block hash");
        let mut unsigned = h.clone();
        unsigned.producer_sig = SignatureBytes(Vec::new());
        let legacy_signing: Vec<u8> = std::str::from_utf8(&serde_json::to_vec(&unsigned).unwrap()).unwrap()
            .replacen(NULL_TOPOLOGY_COMMITMENT_JSON_FRAGMENT, "", 1)
            .replacen(NULL_SHIELDED_POOL_ROOT_JSON_FRAGMENT, "", 1)
            .replacen(EMPTY_EXTENSIONS_JSON_FRAGMENT, "", 1)
            .into_bytes();
        assert_eq!(h.signing_bytes(), legacy_signing, "a None fifth root must not move the signed bytes");
    }

    /// And once the field is `Some`, it is committed: hash and signed bytes both move.
    #[test]
    fn hash_and_signing_bytes_commit_a_some_shielded_pool_root() {
        let base = fake_header();
        let mut a = base.clone();
        a.shielded_pool_root = Some([0x5A; 32]);
        let mut b = base.clone();
        b.shielded_pool_root = Some([0x5B; 32]);
        assert_ne!(base.hash(), a.hash());
        assert_ne!(a.hash(), b.hash());
        assert_ne!(base.signing_bytes(), a.signing_bytes());
        assert_ne!(a.signing_bytes(), b.signing_bytes());
        assert!(std::str::from_utf8(&a.signing_bytes()).unwrap().contains("\"shielded_pool_root\":["));
    }

    /// bincode is POSITIONAL: a record written before the field existed is the new encoding
    /// minus its trailing `Option` byte, and `#[serde(default)]` does NOT rescue a missing
    /// trailing element — it fails with `UnexpectedEof`. Pinned here so nobody assumes the
    /// bincode header framings (`serve_read` codecs 0/1) are cross-version: the node serves
    /// those codecs from the pre-field layout (`SigilBlockHeaderV0Legacy`) and the full
    /// layout only under codec 5 (`'V'`). Same-version None/Some and mixed Vecs must
    /// round-trip (the 2026-08-15 misalignment regression, for the fifth root); a NAMED
    /// record without the key (msgpack `to_vec_named` / JSON — the chain log, gossip, block
    /// backfill) decodes to `None`.
    #[test]
    fn bincode_is_not_cross_version_but_named_formats_are() {
        let mut h = fake_header();
        h.shielded_pool_root = None; // the None case is this test's subject (fake_header carries one on g3)
        let full = bincode::serialize(&h).unwrap();
        assert_eq!(full[full.len() - 1], 0u8, "trailing byte is the None discriminant of the last field");
        let legacy = &full[..full.len() - 1];
        assert!(bincode::deserialize::<SigilBlockHeaderV0>(legacy).is_err(), "positional bincode cannot default a missing trailing field");
        let mut some = h.clone();
        some.shielded_pool_root = Some([0x77; 32]);
        let bytes = bincode::serialize(&some).unwrap();
        let back: SigilBlockHeaderV0 = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back, some);
        let v = vec![h.clone(), some.clone(), h.clone()];
        let bytes = bincode::serialize(&v).unwrap();
        let back: Vec<SigilBlockHeaderV0> = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back, v, "a mixed None/Some Vec must not misalign");
        // and the named (MessagePack-style) path: a JSON object WITHOUT the key decodes to None
        let mut val: serde_json::Value = serde_json::to_value(&h).unwrap();
        val.as_object_mut().unwrap().remove("shielded_pool_root");
        let back: SigilBlockHeaderV0 = serde_json::from_value(val).unwrap();
        assert_eq!(back, h);
    }

    /// The gate rule, pure: present ⇔ height ≥ activation; both violations are named.
    #[test]
    fn shielded_pool_root_gate_is_present_iff_required() {
        let act = 1_000u64;
        assert!(check_shielded_pool_root_gate(999, false, act).is_ok());
        assert!(check_shielded_pool_root_gate(1_000, true, act).is_ok());
        assert!(check_shielded_pool_root_gate(u64::MAX, true, act).is_ok());
        assert!(matches!(
            check_shielded_pool_root_gate(1_000, false, act),
            Err(HeaderError::ShieldedPoolRootMissing { height: 1_000, activation: 1_000 })
        ));
        assert!(matches!(
            check_shielded_pool_root_gate(999, true, act),
            Err(HeaderError::ShieldedPoolRootPremature { height: 999, activation: 1_000 })
        ));
        // On the g3 family the gate is 0: there is no height below it (2026-10-19).
        if let Some(below) = SHIELDED_POOL_ROOT_ACTIVATION_HEIGHT.checked_sub(1) {
            assert!(!shielded_pool_root_required_with(below, SHIELDED_POOL_ROOT_ACTIVATION_HEIGHT));
        }
        assert!(shielded_pool_root_required_with(SHIELDED_POOL_ROOT_ACTIVATION_HEIGHT, SHIELDED_POOL_ROOT_ACTIVATION_HEIGHT));
        // precheck itself at fake_header's height 1: far below the gate on g2, at/above it on
        // the sigil-g3 family (gate 0) — both sides of the rule, on both networks.
        let mut h = fake_header();
        assert!(h.precheck().is_ok());
        if shielded_pool_root_required(1) {
            h.shielded_pool_root = None;
            assert!(matches!(h.precheck(), Err(HeaderError::ShieldedPoolRootMissing { .. })));
        } else {
            h.shielded_pool_root = Some([1u8; 32]);
            assert!(matches!(h.precheck(), Err(HeaderError::ShieldedPoolRootPremature { .. })));
        }
        let mut g = fake_header();
        g.height = SHIELDED_POOL_ROOT_ACTIVATION_HEIGHT;
        g.shielded_pool_root = None;
        assert!(matches!(g.precheck(), Err(HeaderError::ShieldedPoolRootMissing { .. })));
        g.shielded_pool_root = Some([1u8; 32]);
        assert!(g.precheck().is_ok());
    }

    /// The legacy page body is BYTE-IDENTICAL to what every deployed client decoded before
    /// the fifth root: the full encoding minus its trailing `Option` byte, for every header
    /// whose root is `None`. And the two decoders are each other's inverse.
    #[test]
    fn legacy_page_is_byte_identical_to_the_pre_field_encoding() {
        // The legacy codecs exist for clients deployed before the fifth root. The sigil-g3
        // family has none (a new chain, every client built from the full layout), so this pin
        // is g2's; `stored_header_layouts_round_trip` covers g3's layout (2026-10-19).
        if IS_G3 {
            return;
        }
        let mut hs: Vec<SigilBlockHeaderV0> = (1u64..5).map(|i| { let mut h = fake_header(); h.height = i; h }).collect();
        hs[2].topology_commitment = Some([3u8; 32]);
        let legacy = encode_legacy_header_page(&hs);
        // the pre-field bytes: the fifth-root encoding (= the full layout before 2026-10-19's
        // `extensions`) with each element's trailing None byte removed
        let mut expected = Vec::new();
        expected.extend(bincode::serialize(&(hs.len() as u64)).unwrap());
        for h in &hs {
            let full = bincode::serialize(&SigilBlockHeaderV0FifthRoot::from(h)).unwrap();
            assert_eq!(full[full.len() - 1], 0u8);
            expected.extend(&full[..full.len() - 1]);
        }
        assert_eq!(legacy, expected, "legacy page == the bytes every old client reads");
        let back = decode_legacy_header_page(&legacy).unwrap();
        assert_eq!(back, hs, "legacy decode restores the full header with root None");
        let v1 = encode_v1_header_page(&hs);
        assert_eq!(decode_v1_header_page(&v1).unwrap(), hs);
        assert!(decode_v1_header_page(&legacy).is_none() || decode_v1_header_page(&legacy).unwrap() != hs,
            "a legacy body must never silently decode as a v1 page");
        // a gated header loses its root through the legacy view, by design
        let mut gated = fake_header();
        gated.shielded_pool_root = Some([9u8; 32]);
        let round: SigilBlockHeaderV0 = SigilBlockHeaderV0Legacy::from(&gated).into();
        assert!(round.shielded_pool_root.is_none());
        assert_ne!(round.hash(), gated.hash());
        // stored records: untagged legacy and tagged v1 both decode
        let stored_legacy = bincode::serialize(&SigilBlockHeaderV0Legacy::from(&hs[0])).unwrap();
        assert_eq!(stored_legacy[0], 0u8, "a legacy record starts with version=0");
        assert_eq!(decode_stored_header(&stored_legacy).unwrap(), hs[0]);
        let stored_v1 = encode_stored_header(&gated);
        assert_eq!(stored_v1[0], STORED_HEADER_TAG_V1);
        assert_eq!(decode_stored_header(&stored_v1).unwrap(), gated);
        assert!(is_header_page_codec(0) && is_header_page_codec(1) && is_header_page_codec(2) && is_header_page_codec(5));
        assert!(!is_header_page_codec(3) && !is_header_page_codec(4));
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
        let expected = canonical_json_for_hashing(serde_json::to_vec(&clone).unwrap(), true, true);
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
