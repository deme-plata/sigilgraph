//! receipt.rs — a signed ACCEPTANCE receipt, returned synchronously when a
//! payment is submitted.
//!
//! # Acceptance is not settlement. Read this before using anything here.
//!
//! SIGIL settles at a consensus DEPTH: `BraidConfig::final_depth = 512` blocks
//! (`sigil-dagknight`). At the block rate this network actually produces —
//! **measured 5.66 blk/s on Epsilon, 2026-09-02** (255 blocks in 45.06 s of
//! wall clock, polled from `/v1/mining/challenge`) — that is roughly **90
//! seconds** before a freshly-submitted transaction can settle *at the very
//! best*. No proof system compresses that; it is a property of the consensus
//! rule, not of the software's speed.
//!
//! What CAN be sub-millisecond is **acceptance**: this node authenticated the
//! request, queued it into the pending pool the producer actually drains, and
//! is willing to say so under its own signing key. That is a genuinely useful
//! thing to hold — it is evidence, attributable to a named node, that the
//! payment was received and not silently dropped at the door — and it is
//! *nothing at all* like a settlement guarantee.
//!
//! A receipt that could be mistaken for settlement would be **worse than no
//! receipt**, because a payee would treat an accepted-then-orphaned payment as
//! final and ship the goods. So the distinction is enforced structurally, not
//! by documentation alone:
//!
//! * The type is named [`AcceptanceReceipt`]. There is no `SettlementReceipt`.
//! * [`FINALITY_DISCRIMINATOR`] — the literal string `"accepted, not settled"` —
//!   is a REQUIRED field, is inside the signed bytes, and [`verify_against`]
//!   rejects any receipt whose value differs. There is therefore no
//!   representable, verifiable receipt that claims anything stronger.
//! * [`AcceptanceReceipt::is_settlement_proof`] exists and always returns
//!   `false`, so a caller reaching for "is this final?" gets the truth instead
//!   of a plausible-looking field.
//! * Every receipt carries the settlement depth and the earliest height at
//!   which settlement is even possible, so a client can display the real wait
//!   rather than a spinner that implies imminence.
//!
//! # What the signature does and does not prove
//!
//! It proves: *the holder of this key produced this exact set of facts.* Flip
//! any byte of any signed field and verification fails.
//!
//! It does NOT prove the transaction will settle, that the sender had a
//! balance, or that any block exists. Balance is checked by `apply_tx` at mint
//! time, long after this receipt is handed back. An accepted transaction that
//! turns out to be unfunded is dropped by `SendBridge::snapshot_for_mint`'s
//! retry budget with a loud log — and the receipt for it stays perfectly
//! valid, because the receipt only ever claimed acceptance.
//!
//! # Canonical bytes, not JSON
//!
//! The signed payload is a domain-separated, length-prefixed, fixed-order
//! binary encoding ([`AcceptanceReceipt::canonical_bytes`]) — never
//! `serde_json`. Signing a JSON rendering would mean a future `#[serde(...)]`
//! attribute, a field reorder, or a serde version bump silently invalidating
//! every signature ever issued. This is the same discipline
//! `fluxc-core::provenance::canonical_bundle_bytes` uses for release
//! provenance.
//!
//! The domain tag also gives cross-protocol separation for free. This node's
//! producer key signs `SigilBlockHeaderV0::signing_bytes()`, which is
//! `serde_json::to_vec` of a struct and therefore always begins with `0x7b`
//! (`{`); the wallet RPC-auth scheme signs ASCII beginning `sigil-rpc/v1|`.
//! A receipt's canonical bytes begin with the u32 length `0x1b` of the tag
//! `sigil-acceptance-receipt/v1`. No receipt can ever be replayed as a block
//! header signature, and no header can ever be read as a receipt.

use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

// ── constants ───────────────────────────────────────────────────────────────

/// Domain-separation tag. Bumping this invalidates every previously-issued
/// receipt by construction — which is the point: a schema change must not be
/// able to masquerade as the old schema.
pub const RECEIPT_DOMAIN: &str = "sigil-acceptance-receipt/v1";

/// Schema string echoed in the JSON body so a client can branch on it without
/// parsing the canonical bytes.
pub const RECEIPT_SCHEMA: &str = "sigil-acceptance-receipt/v1";

/// **The whole point of this file.** A verifiable receipt can carry exactly
/// this value in its `finality` field and no other; [`verify_against`] fails
/// closed on anything else. There is no code path in this crate that produces
/// a receipt saying "settled", "final", or "confirmed".
pub const FINALITY_DISCRIMINATOR: &str = "accepted, not settled";

/// Human-readable expansion, signed alongside the discriminator so it cannot be
/// swapped out by an intermediary that re-renders the JSON.
pub const SETTLEMENT_NOTE: &str =
    "This proves the node ACCEPTED and queued the transaction. It is not proof of \
     settlement. The transaction settles only after it is carried by a block that \
     reaches the braid's finality depth, and it may still be dropped before then \
     (insufficient balance, or the retry budget expiring).";

/// `BraidConfig::final_depth` (`sigil-dagknight`, crates/sigil-dagknight/src/lib.rs:169).
///
/// Deliberately restated rather than imported, matching the existing precedent in
/// `send.rs`'s `max_age_clears_the_worst_case_finality_lag`: the number is part of
/// the receipt's *claim to the client*, so it should fail loudly and visibly if
/// consensus ever moves it, rather than silently re-deriving a different promise.
/// A node that runs a non-default `SIGIL_DAG_FINAL_DEPTH` can override it with
/// `SIGIL_RECEIPT_FINAL_DEPTH`.
pub const DEFAULT_SETTLEMENT_DEPTH_BLOCKS: u64 = 512;

/// Block rate used for the ETA, in **milli-blocks per second** (5_660 = 5.66 blk/s).
///
/// MEASURED, not assumed: 255 blocks in 45.06 s on Epsilon, 2026-09-02, by polling
/// `/v1/mining/challenge`. This is deliberately the *slower* of the two figures on
/// record for this window (`send.rs` cites 6.6 blk/s the same day, and 6.28 blk/s on
/// 2026-08-26) because an ETA that is too long disappoints and an ETA that is too
/// short lies. Overridable per-node with `SIGIL_RECEIPT_BLOCK_RATE_MHZ`.
///
/// The rate is the ONLY estimated input here. `depth_blocks` and
/// `earliest_settlement_height` are exact; the seconds figure is not, and is
/// flagged `eta_is_estimate: true` in every receipt.
pub const DEFAULT_BLOCK_RATE_MHZ: u64 = 5_660;

const SEED_ENV_DEDICATED: &str = "SIGIL_RECEIPT_SIGNING_SEED_HEX";
const SEED_ENV_PRODUCER: &str = "SIGIL_PRODUCER_SIGNING_SEED_HEX";

// ── what was accepted ───────────────────────────────────────────────────────

/// Which submit path minted this receipt.
///
/// An unrecognized kind makes [`verify_against`] fail closed
/// ([`ReceiptError::UnknownKind`]) rather than accept a receipt it cannot
/// interpret. That means adding a payment path is a deliberate, breaking change
/// for old verifiers — the correct trade for money.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitKind {
    /// `POST /v1/send` — transparent native send. Currently refused chain-wide
    /// (`sigil_tx::SHIELDED_ONLY_HEIGHT == 0` makes SIGIL privacy-only), so no
    /// receipt of this kind is issued on the live chain today.
    Send,
    /// `POST /v1/shield` — transparent value into the shielded pool.
    Shield,
    /// `POST /v1/shielded_send` — shielded → shielded. No submitting wallet and
    /// no nonce exist: the proof authorizes it, not an address. Both fields are
    /// `None` in the receipt, and that absence is itself signed.
    ShieldedSend,
    /// `POST /v1/unshield` — shielded pool → transparent wallet. Also
    /// proof-authorized, so also carries no submitting wallet.
    Unshield,
    /// `POST /v1/shielded/register` — publish a shielded key.
    ShieldedRegister,
}

impl SubmitKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SubmitKind::Send => "send",
            SubmitKind::Shield => "shield",
            SubmitKind::ShieldedSend => "shielded_send",
            SubmitKind::Unshield => "unshield",
            SubmitKind::ShieldedRegister => "shielded_register",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "send" => SubmitKind::Send,
            "shield" => SubmitKind::Shield,
            "shielded_send" => SubmitKind::ShieldedSend,
            "unshield" => SubmitKind::Unshield,
            "shielded_register" => SubmitKind::ShieldedRegister,
            _ => return None,
        })
    }
}

/// Where the signing key came from. Carried in the receipt (and signed) so a
/// verifier is never left guessing how much the signature is worth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyProvenance {
    /// `SIGIL_RECEIPT_SIGNING_SEED_HEX` — a key dedicated to receipts. Preferred:
    /// it lets an operator give receipts their own key without widening the blast
    /// radius of the consensus key.
    DedicatedReceiptKey,
    /// `SIGIL_PRODUCER_SIGNING_SEED_HEX` — the node's block-producing identity.
    ///
    /// This is the STRONGEST available binding and is what Epsilon runs today
    /// (verified 2026-09-02: the seed in the live node's environment derives
    /// `73b7745271b6be22dd8ca4be17f6fbff2df794d2d0b3c98ae791b219a6bc33d9`, which
    /// is byte-identical to the `producer` field of the blocks that node is
    /// currently minting). A third party can therefore anchor `node_pubkey` to
    /// on-chain reality instead of trusting the receipt to describe itself.
    ProducerConsensusKey,
    /// No key configured: a random key generated once per process.
    ///
    /// **This authenticates nothing about node identity.** It gives tamper-evidence
    /// relative to a pubkey the client pins for the life of one node process, and
    /// nothing more. It is labelled honestly rather than omitted, because silently
    /// returning an unsigned or fake-signed receipt would be the worse failure.
    EphemeralProcessKey,
}

impl KeyProvenance {
    pub fn as_str(self) -> &'static str {
        match self {
            KeyProvenance::DedicatedReceiptKey => "dedicated-receipt-key",
            KeyProvenance::ProducerConsensusKey => "producer-consensus-key",
            KeyProvenance::EphemeralProcessKey => "ephemeral-process-key",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "dedicated-receipt-key" => KeyProvenance::DedicatedReceiptKey,
            "producer-consensus-key" => KeyProvenance::ProducerConsensusKey,
            "ephemeral-process-key" => KeyProvenance::EphemeralProcessKey,
            _ => return None,
        })
    }

    /// Whether this key is one a third party can independently tie to a known
    /// identity. `false` for the ephemeral key — a client should treat such a
    /// receipt as a session-local integrity check, not as attribution.
    pub fn is_externally_anchorable(self) -> bool {
        !matches!(self, KeyProvenance::EphemeralProcessKey)
    }
}

// ── the receipt ─────────────────────────────────────────────────────────────

/// The settlement facts, spelled out so a client can render the truth rather
/// than inventing one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settlement {
    /// Exact. Blocks of depth required before the carrying block is final.
    pub depth_blocks: u64,
    /// Exact when present: `accepted_at_height + depth_blocks`. The EARLIEST
    /// height at which settlement is possible — not a prediction that it happens
    /// there. `None` when the node could not observe its own tip (a non-producing
    /// node whose mining frontier has never been published).
    pub earliest_settlement_height: Option<u64>,
    /// Estimated, never exact. Derived as `depth_blocks / block_rate`.
    pub eta_seconds_estimate: u64,
    /// The rate that estimate used, in milli-blocks per second.
    pub eta_basis_block_rate_mhz: u64,
    /// Always `true`. Present so a UI cannot read `eta_seconds_estimate` without
    /// also seeing that it is an estimate.
    pub eta_is_estimate: bool,
    /// [`SETTLEMENT_NOTE`], signed so it cannot be stripped in transit.
    pub note: String,
}

/// Cryptographic proof that a specific node ACCEPTED a specific transaction at a
/// specific instant. Not proof of settlement — see the module docs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptanceReceipt {
    /// [`RECEIPT_SCHEMA`].
    pub schema: String,
    /// [`FINALITY_DISCRIMINATOR`] — always exactly `"accepted, not settled"`.
    pub finality: String,
    /// Which submit path this came from ([`SubmitKind::as_str`]).
    pub kind: String,
    /// 64-hex `SigilTx::hash()` — the id the caller polls on.
    pub tx_id: String,
    /// 64-hex submitting wallet, when one exists. `None` for the proof-authorized
    /// shielded paths, which have no submitter by design.
    pub wallet: Option<String>,
    /// The client-chosen replay nonce, when the path has one.
    pub req_nonce: Option<u64>,
    /// Unix milliseconds at which this node accepted the request.
    pub accepted_at_ms: u64,
    /// The node's tip height at acceptance, if observable.
    pub accepted_at_height: Option<u64>,
    pub settlement: Settlement,
    /// 64-hex Ed25519 public key of the signer. See [`KeyProvenance`] for how
    /// much that identity is worth.
    pub node_pubkey: String,
    /// [`KeyProvenance::as_str`].
    pub key_provenance: String,
    /// 128-hex Ed25519 signature over [`AcceptanceReceipt::canonical_bytes`].
    pub sig: String,
}

/// Everything the caller must supply to mint a receipt. Kept separate from the
/// receipt itself so the submit paths hand over facts and the signer owns the
/// derived/constant fields — a caller cannot accidentally set `finality`.
#[derive(Debug, Clone)]
pub struct AcceptanceFacts {
    pub kind: SubmitKind,
    pub tx_id: [u8; 32],
    pub wallet: Option<[u8; 32]>,
    pub req_nonce: Option<u64>,
    /// The node's tip height, if the caller can observe it (handlers pass
    /// `st.mining.tip().map(|t| t.height)`).
    pub accepted_at_height: Option<u64>,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// u32-LE length prefix + bytes. Every variable-length field goes through this,
/// which is what makes the encoding injective: no choice of field values can
/// produce the same byte string as a different choice.
fn lp(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
}

fn opt_u64(out: &mut Vec<u8>, v: Option<u64>) {
    match v {
        Some(x) => {
            out.push(1);
            out.extend_from_slice(&x.to_le_bytes());
        }
        None => out.push(0),
    }
}

impl AcceptanceReceipt {
    /// **Always `false`.** A receipt is never settlement evidence. This exists so
    /// a caller asking the question gets the answer in code, not in prose.
    pub fn is_settlement_proof(&self) -> bool {
        false
    }

    /// The exact bytes the signature covers. Excludes `sig` itself (a signature
    /// cannot sign over itself) and includes every other field, so flipping any
    /// of them invalidates the receipt.
    ///
    /// Layout — domain-tagged, versioned, fixed order, every variable-length
    /// field length-prefixed:
    ///
    /// ```text
    ///   lp("sigil-acceptance-receipt/v1")
    ///   lp(schema) lp(finality) lp(kind)
    ///   lp(tx_id) opt(lp(wallet)) lp(node_pubkey) lp(key_provenance)
    ///   opt(req_nonce) u64(accepted_at_ms) opt(accepted_at_height)
    ///   u64(depth_blocks) opt(earliest_settlement_height)
    ///   u64(eta_seconds_estimate) u64(eta_basis_block_rate_mhz)
    ///   u8(eta_is_estimate) lp(note)
    /// ```
    ///
    /// Note there is no floating point anywhere in the signed bytes — the block
    /// rate is carried as an integer milli-hertz. An `f64` in a signed payload is
    /// a portability and NaN hazard for no benefit.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(512);
        lp(&mut out, RECEIPT_DOMAIN.as_bytes());
        lp(&mut out, self.schema.as_bytes());
        lp(&mut out, self.finality.as_bytes());
        lp(&mut out, self.kind.as_bytes());
        lp(&mut out, self.tx_id.as_bytes());
        // A missing wallet and an empty-string wallet must not collide. The
        // length prefix already distinguishes them, but be explicit: absent is
        // encoded as a 0-length field preceded by a 0 tag byte.
        match &self.wallet {
            Some(w) => {
                out.push(1);
                lp(&mut out, w.as_bytes());
            }
            None => out.push(0),
        }
        lp(&mut out, self.node_pubkey.as_bytes());
        lp(&mut out, self.key_provenance.as_bytes());
        opt_u64(&mut out, self.req_nonce);
        out.extend_from_slice(&self.accepted_at_ms.to_le_bytes());
        opt_u64(&mut out, self.accepted_at_height);
        out.extend_from_slice(&self.settlement.depth_blocks.to_le_bytes());
        opt_u64(&mut out, self.settlement.earliest_settlement_height);
        out.extend_from_slice(&self.settlement.eta_seconds_estimate.to_le_bytes());
        out.extend_from_slice(&self.settlement.eta_basis_block_rate_mhz.to_le_bytes());
        out.push(u8::from(self.settlement.eta_is_estimate));
        lp(&mut out, self.settlement.note.as_bytes());
        out
    }
}

// ── minting ─────────────────────────────────────────────────────────────────

/// Holds the node's receipt-signing key and the settlement parameters it
/// advertises. Cheap to clone-free share; [`global`] keeps one per process.
pub struct ReceiptSigner {
    key: SigningKey,
    provenance: KeyProvenance,
    pubkey_hex: String,
    depth_blocks: u64,
    block_rate_mhz: u64,
}

impl ReceiptSigner {
    pub fn from_key(key: SigningKey, provenance: KeyProvenance) -> Self {
        Self::from_key_with(key, provenance, DEFAULT_SETTLEMENT_DEPTH_BLOCKS, DEFAULT_BLOCK_RATE_MHZ)
    }

    pub fn from_key_with(
        key: SigningKey,
        provenance: KeyProvenance,
        depth_blocks: u64,
        block_rate_mhz: u64,
    ) -> Self {
        let pubkey_hex = hex::encode(key.verifying_key().to_bytes());
        Self {
            key,
            provenance,
            pubkey_hex,
            depth_blocks,
            // A zero rate would divide by zero below. Fail safe to the measured
            // default rather than panicking on an operator typo.
            block_rate_mhz: if block_rate_mhz == 0 { DEFAULT_BLOCK_RATE_MHZ } else { block_rate_mhz },
        }
    }

    /// The public key a verifier checks against, 64-hex.
    pub fn public_key_hex(&self) -> &str {
        &self.pubkey_hex
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.key.verifying_key().to_bytes()
    }

    pub fn provenance(&self) -> KeyProvenance {
        self.provenance
    }

    /// Mint and sign a receipt.
    ///
    /// Pure CPU: one Ed25519 signature over a few hundred bytes, no I/O, no lock,
    /// no allocation of consequence. Callers must not hold a mutex across it (the
    /// submit paths release the pending-pool lock before minting), but it is safe
    /// on the request path — the cost is tens of microseconds, measured by
    /// `signing_is_cheap_enough_for_the_request_path`.
    pub fn mint(&self, facts: AcceptanceFacts) -> AcceptanceReceipt {
        let earliest = facts.accepted_at_height.map(|h| h.saturating_add(self.depth_blocks));
        // Integer ceiling division on milli-hertz: depth / (mhz/1000) rounded up.
        // Rounding UP is deliberate — an ETA that runs slightly long is honest,
        // one that runs short invites a payee to act before settlement.
        let eta_seconds = (self.depth_blocks.saturating_mul(1_000) + self.block_rate_mhz - 1)
            / self.block_rate_mhz;

        let mut receipt = AcceptanceReceipt {
            schema: RECEIPT_SCHEMA.to_string(),
            finality: FINALITY_DISCRIMINATOR.to_string(),
            kind: facts.kind.as_str().to_string(),
            tx_id: hex::encode(facts.tx_id),
            wallet: facts.wallet.map(hex::encode),
            req_nonce: facts.req_nonce,
            accepted_at_ms: now_ms(),
            accepted_at_height: facts.accepted_at_height,
            settlement: Settlement {
                depth_blocks: self.depth_blocks,
                earliest_settlement_height: earliest,
                eta_seconds_estimate: eta_seconds,
                eta_basis_block_rate_mhz: self.block_rate_mhz,
                eta_is_estimate: true,
                note: SETTLEMENT_NOTE.to_string(),
            },
            node_pubkey: self.pubkey_hex.clone(),
            key_provenance: self.provenance.as_str().to_string(),
            sig: String::new(),
        };
        let sig = self.key.sign(&receipt.canonical_bytes());
        receipt.sig = hex::encode(sig.to_bytes());
        receipt
    }
}

fn seed_from_hex(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    let s = s.strip_prefix("0x").unwrap_or(s);
    // is_ascii: a 64-BYTE multibyte string would split a UTF-8 boundary in the
    // byte-slicing below and panic. Same guard as `hex32`/`parse_hex64`.
    if s.len() != 64 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// Choose the signing key from the two candidate seeds, in preference order.
/// Pure function of its inputs — deliberately NOT reading the environment — so
/// the selection policy is testable without racing `set_var` across the test
/// binary's threads.
///
/// A malformed seed is treated as absent (fail safe to the next option) rather
/// than a panic: an operator typo should degrade the receipt's provenance, not
/// take the money API down.
pub fn select_key(
    dedicated_seed_hex: Option<&str>,
    producer_seed_hex: Option<&str>,
) -> (SigningKey, KeyProvenance) {
    if let Some(seed) = dedicated_seed_hex.and_then(seed_from_hex) {
        return (SigningKey::from_bytes(&seed), KeyProvenance::DedicatedReceiptKey);
    }
    if let Some(seed) = producer_seed_hex.and_then(seed_from_hex) {
        return (SigningKey::from_bytes(&seed), KeyProvenance::ProducerConsensusKey);
    }
    (ephemeral_key(), KeyProvenance::EphemeralProcessKey)
}

/// 32 bytes from `/dev/urandom`, falling back to a BLAKE3 of time+pid+ASLR
/// address (logged loudly) — the same shape and the same fallback
/// `sigil-node`'s `sync_auth::fresh_seed` uses.
fn ephemeral_key() -> SigningKey {
    use std::io::Read;
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let mut sk = [0u8; 32];
        if f.read_exact(&mut sk).is_ok() {
            return SigningKey::from_bytes(&sk);
        }
    }
    eprintln!(
        "⚠ receipt: /dev/urandom unavailable — deriving an ephemeral receipt key from \
         time+pid (weaker). Receipts will be labelled ephemeral-process-key."
    );
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let marker = &ephemeral_key as *const _ as usize;
    let mut h = blake3::Hasher::new();
    h.update(b"sigil-acceptance-receipt/fallback-seed/v1");
    h.update(&now.to_le_bytes());
    h.update(&marker.to_le_bytes());
    h.update(&std::process::id().to_le_bytes());
    SigningKey::from_bytes(h.finalize().as_bytes())
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

static GLOBAL: OnceLock<ReceiptSigner> = OnceLock::new();

/// The process-wide receipt signer, built once from the environment.
///
/// A process-global rather than an `AppState` field on purpose: `AppState` is
/// constructed by `sigil-node`'s `main.rs` as a struct literal, so adding a
/// field there would break a crate this change is not allowed to touch. The
/// key material it reads (`SIGIL_PRODUCER_SIGNING_SEED_HEX`) is already
/// process-global environment state, so a `OnceLock` adds no exposure that the
/// process did not already have — it only avoids re-parsing the seed on every
/// request.
pub fn global() -> &'static ReceiptSigner {
    GLOBAL.get_or_init(|| {
        let dedicated = std::env::var(SEED_ENV_DEDICATED).ok();
        let producer = std::env::var(SEED_ENV_PRODUCER).ok();
        let (key, provenance) = select_key(dedicated.as_deref(), producer.as_deref());
        if provenance == KeyProvenance::EphemeralProcessKey {
            eprintln!(
                "⚠ receipt: no {SEED_ENV_DEDICATED} or {SEED_ENV_PRODUCER} configured — \
                 acceptance receipts will be signed by an EPHEMERAL per-process key. They \
                 are tamper-evident but attributable to nothing outside this process."
            );
        }
        ReceiptSigner::from_key_with(
            key,
            provenance,
            env_u64("SIGIL_RECEIPT_FINAL_DEPTH", DEFAULT_SETTLEMENT_DEPTH_BLOCKS),
            env_u64("SIGIL_RECEIPT_BLOCK_RATE_MHZ", DEFAULT_BLOCK_RATE_MHZ),
        )
    })
}

/// Convenience for the submit paths: mint with the process signer.
pub fn mint(facts: AcceptanceFacts) -> AcceptanceReceipt {
    global().mint(facts)
}

// ── verification (the third-party entry point) ──────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiptError {
    /// `schema` is not [`RECEIPT_SCHEMA`].
    BadSchema { found: String },
    /// `finality` is not [`FINALITY_DISCRIMINATOR`]. **This is the guard that
    /// makes it impossible for a verifiable receipt to claim settlement.**
    NotAnAcceptanceReceipt { found: String },
    UnknownKind { found: String },
    UnknownKeyProvenance { found: String },
    BadTxIdEncoding,
    BadWalletEncoding,
    BadPubkeyEncoding,
    BadSignatureEncoding,
    /// The signature does not match the receipt's own bytes — something was
    /// altered after signing.
    SignatureInvalid,
    /// The receipt verified against ITS OWN embedded key, but that key is not
    /// the one the verifier expected. A self-consistent receipt from an unknown
    /// signer proves nothing about the node you think you talked to.
    WrongSigner { expected: String, got: String },
    /// `eta_is_estimate` was false — a receipt must never present its ETA as
    /// exact.
    EtaPresentedAsExact,
    /// `earliest_settlement_height != accepted_at_height + depth_blocks`.
    InconsistentSettlementHeight { accepted_at: u64, depth: u64, claimed: u64 },
    /// The note was stripped or replaced.
    SettlementNoteAltered,
}

impl ReceiptError {
    pub fn message(&self) -> String {
        match self {
            ReceiptError::BadSchema { found } =>
                format!("unknown receipt schema {found:?} (want {RECEIPT_SCHEMA:?})"),
            ReceiptError::NotAnAcceptanceReceipt { found } => format!(
                "finality field is {found:?}, not {FINALITY_DISCRIMINATOR:?} — this is not a \
                 valid acceptance receipt, and no receipt from this API ever claims more"
            ),
            ReceiptError::UnknownKind { found } => format!("unknown submit kind {found:?}"),
            ReceiptError::UnknownKeyProvenance { found } =>
                format!("unknown key provenance {found:?}"),
            ReceiptError::BadTxIdEncoding => "tx_id must be 64-hex".into(),
            ReceiptError::BadWalletEncoding => "wallet must be 64-hex when present".into(),
            ReceiptError::BadPubkeyEncoding => "node_pubkey must be 64-hex".into(),
            ReceiptError::BadSignatureEncoding => "sig must be 128-hex (64 bytes)".into(),
            ReceiptError::SignatureInvalid =>
                "signature does not match the receipt's contents — it was altered in transit".into(),
            ReceiptError::WrongSigner { expected, got } =>
                format!("receipt was signed by {got}, expected {expected}"),
            ReceiptError::EtaPresentedAsExact =>
                "eta_is_estimate must be true — the settlement ETA is never exact".into(),
            ReceiptError::InconsistentSettlementHeight { accepted_at, depth, claimed } => format!(
                "earliest_settlement_height {claimed} != accepted_at_height {accepted_at} + \
                 depth {depth}"
            ),
            ReceiptError::SettlementNoteAltered =>
                "the settlement note was altered or stripped".into(),
        }
    }
}

/// What a verified receipt actually establishes, in typed form so nothing is
/// implied by omission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedAcceptance {
    pub kind: SubmitKind,
    pub tx_id: [u8; 32],
    pub wallet: Option<[u8; 32]>,
    pub req_nonce: Option<u64>,
    pub accepted_at_ms: u64,
    pub accepted_at_height: Option<u64>,
    pub signer: [u8; 32],
    pub key_provenance: KeyProvenance,
    pub earliest_settlement_height: Option<u64>,
    pub settlement_depth_blocks: u64,
}

impl VerifiedAcceptance {
    /// **Always `false`.** Verifying a receipt establishes acceptance. It never
    /// establishes settlement, no matter how many checks passed.
    pub fn is_settled(&self) -> bool {
        false
    }
}

/// **The real check.** Verify a receipt against the public key you independently
/// expect the node to hold.
///
/// `expected_signer` is not optional on purpose. Verifying a signature against a
/// key the receipt itself supplies proves only internal consistency — an
/// attacker who rewrites the whole receipt and signs it with their own key
/// passes that check trivially. A caller must obtain the node's key out of band:
/// for a [`KeyProvenance::ProducerConsensusKey`] receipt that means reading the
/// `producer` field of a block the node minted (they are the same 32 bytes,
/// verified live on Epsilon 2026-09-02).
pub fn verify_against(
    receipt: &AcceptanceReceipt,
    expected_signer: &[u8; 32],
) -> Result<VerifiedAcceptance, ReceiptError> {
    let verified = verify_self_consistent(receipt)?;
    if &verified.signer != expected_signer {
        return Err(ReceiptError::WrongSigner {
            expected: hex::encode(expected_signer),
            got: hex::encode(verified.signer),
        });
    }
    Ok(verified)
}

/// Check every structural invariant and the signature *against the receipt's own
/// embedded key*.
///
/// **This is not attribution.** It answers "was this receipt altered after the
/// holder of `node_pubkey` signed it?" and nothing else. Anyone can mint a
/// self-consistent receipt with a key they generated. Use [`verify_against`]
/// whenever you know which node you expect; this form is for the case where you
/// genuinely only want tamper-evidence (e.g. checking your own stored copy has
/// not been corrupted).
pub fn verify_self_consistent(
    receipt: &AcceptanceReceipt,
) -> Result<VerifiedAcceptance, ReceiptError> {
    if receipt.schema != RECEIPT_SCHEMA {
        return Err(ReceiptError::BadSchema { found: receipt.schema.clone() });
    }
    // The load-bearing check: fail closed on anything that is not verbatim
    // "accepted, not settled".
    if receipt.finality != FINALITY_DISCRIMINATOR {
        return Err(ReceiptError::NotAnAcceptanceReceipt { found: receipt.finality.clone() });
    }
    let kind = SubmitKind::parse(&receipt.kind)
        .ok_or_else(|| ReceiptError::UnknownKind { found: receipt.kind.clone() })?;
    let key_provenance = KeyProvenance::parse(&receipt.key_provenance)
        .ok_or_else(|| ReceiptError::UnknownKeyProvenance { found: receipt.key_provenance.clone() })?;
    if !receipt.settlement.eta_is_estimate {
        return Err(ReceiptError::EtaPresentedAsExact);
    }
    if receipt.settlement.note != SETTLEMENT_NOTE {
        return Err(ReceiptError::SettlementNoteAltered);
    }
    if let (Some(at), Some(claimed)) =
        (receipt.accepted_at_height, receipt.settlement.earliest_settlement_height)
    {
        let want = at.saturating_add(receipt.settlement.depth_blocks);
        if claimed != want {
            return Err(ReceiptError::InconsistentSettlementHeight {
                accepted_at: at,
                depth: receipt.settlement.depth_blocks,
                claimed,
            });
        }
    }

    let tx_id = hex32_exact(&receipt.tx_id).ok_or(ReceiptError::BadTxIdEncoding)?;
    let wallet = match &receipt.wallet {
        Some(w) => Some(hex32_exact(w).ok_or(ReceiptError::BadWalletEncoding)?),
        None => None,
    };
    let signer = hex32_exact(&receipt.node_pubkey).ok_or(ReceiptError::BadPubkeyEncoding)?;

    let sig_bytes = hex::decode(receipt.sig.strip_prefix("0x").unwrap_or(&receipt.sig))
        .map_err(|_| ReceiptError::BadSignatureEncoding)?;
    let sig_arr: [u8; 64] =
        sig_bytes.try_into().map_err(|_| ReceiptError::BadSignatureEncoding)?;

    let vk = VerifyingKey::from_bytes(&signer).map_err(|_| ReceiptError::BadPubkeyEncoding)?;
    vk.verify(&receipt.canonical_bytes(), &Signature::from_bytes(&sig_arr))
        .map_err(|_| ReceiptError::SignatureInvalid)?;

    Ok(VerifiedAcceptance {
        kind,
        tx_id,
        wallet,
        req_nonce: receipt.req_nonce,
        accepted_at_ms: receipt.accepted_at_ms,
        accepted_at_height: receipt.accepted_at_height,
        signer,
        key_provenance,
        earliest_settlement_height: receipt.settlement.earliest_settlement_height,
        settlement_depth_blocks: receipt.settlement.depth_blocks,
    })
}

fn hex32_exact(s: &str) -> Option<[u8; 32]> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    if s.len() != 64 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_signer() -> ReceiptSigner {
        let sk = SigningKey::generate(&mut rand::rngs::OsRng);
        ReceiptSigner::from_key(sk, KeyProvenance::DedicatedReceiptKey)
    }

    fn facts() -> AcceptanceFacts {
        AcceptanceFacts {
            kind: SubmitKind::Shield,
            tx_id: [0xAB; 32],
            wallet: Some([0x11; 32]),
            req_nonce: Some(1_788_342_047_000),
            accepted_at_height: Some(2_183_132),
        }
    }

    #[test]
    fn a_freshly_minted_receipt_verifies_against_the_signer() {
        let signer = test_signer();
        let r = signer.mint(facts());
        let v = verify_against(&r, &signer.public_key()).expect("must verify");
        assert_eq!(v.tx_id, [0xAB; 32]);
        assert_eq!(v.wallet, Some([0x11; 32]));
        assert_eq!(v.kind, SubmitKind::Shield);
        assert_eq!(v.accepted_at_height, Some(2_183_132));
        assert_eq!(v.earliest_settlement_height, Some(2_183_132 + 512));
    }

    /// **The tamper test.** Flip one byte anywhere in a signed field and
    /// verification must fail. Runs over every field individually so a future
    /// field added to `canonical_bytes` without a matching test is visible.
    #[test]
    fn flipping_any_signed_byte_breaks_verification() {
        let signer = test_signer();
        let good = signer.mint(facts());
        let pk = signer.public_key();
        assert!(verify_against(&good, &pk).is_ok());

        // 1. the amount-equivalent field: the tx id itself.
        let mut t = good.clone();
        let mut id = hex32_exact(&t.tx_id).unwrap();
        id[0] ^= 0x01;
        t.tx_id = hex::encode(id);
        assert_eq!(verify_against(&t, &pk), Err(ReceiptError::SignatureInvalid));

        // 2. the wallet.
        let mut t = good.clone();
        let mut w = hex32_exact(t.wallet.as_ref().unwrap()).unwrap();
        w[31] ^= 0x80;
        t.wallet = Some(hex::encode(w));
        assert_eq!(verify_against(&t, &pk), Err(ReceiptError::SignatureInvalid));

        // 3. the nonce.
        let mut t = good.clone();
        t.req_nonce = Some(good.req_nonce.unwrap() + 1);
        assert_eq!(verify_against(&t, &pk), Err(ReceiptError::SignatureInvalid));

        // 4. the timestamp — backdating a receipt must not survive.
        let mut t = good.clone();
        t.accepted_at_ms = good.accepted_at_ms - 1;
        assert_eq!(verify_against(&t, &pk), Err(ReceiptError::SignatureInvalid));

        // 5. presence itself: dropping the wallet is a different message, not a
        //    truncation that still verifies.
        let mut t = good.clone();
        t.wallet = None;
        assert_eq!(verify_against(&t, &pk), Err(ReceiptError::SignatureInvalid));

        // 6. the kind — a shield must not be re-labelled an unshield.
        let mut t = good.clone();
        t.kind = SubmitKind::Unshield.as_str().to_string();
        assert_eq!(verify_against(&t, &pk), Err(ReceiptError::SignatureInvalid));

        // 7. the settlement depth — shortening the advertised wait is the most
        //    valuable lie an intermediary could tell, so it must be signed.
        let mut t = good.clone();
        t.settlement.depth_blocks = 1;
        t.settlement.earliest_settlement_height = Some(2_183_132 + 1); // keep it self-consistent
        assert_eq!(verify_against(&t, &pk), Err(ReceiptError::SignatureInvalid));

        // 8. the ETA.
        let mut t = good.clone();
        t.settlement.eta_seconds_estimate = 0;
        assert_eq!(verify_against(&t, &pk), Err(ReceiptError::SignatureInvalid));

        // 9. the signature bytes themselves.
        let mut t = good.clone();
        let mut sig = hex::decode(&t.sig).unwrap();
        sig[0] ^= 0x01;
        t.sig = hex::encode(sig);
        assert_eq!(verify_against(&t, &pk), Err(ReceiptError::SignatureInvalid));

        // 10. the key provenance — an ephemeral-key receipt must not be
        //     re-labelled as the consensus key.
        let mut t = good.clone();
        t.key_provenance = KeyProvenance::ProducerConsensusKey.as_str().to_string();
        assert_eq!(verify_against(&t, &pk), Err(ReceiptError::SignatureInvalid));

        // The original is still fine — none of the above mutated it.
        assert!(verify_against(&good, &pk).is_ok());
    }

    /// **A receipt can never be read as a settlement proof.**
    ///
    /// Three independent guarantees, each of which alone would be enough:
    /// the accessor always answers false; the discriminator is fixed; and no
    /// rewrite of the discriminator survives verification.
    #[test]
    fn a_receipt_can_never_be_read_as_a_settlement_proof() {
        let signer = test_signer();
        let r = signer.mint(facts());
        let pk = signer.public_key();

        // (1) The type says so, unconditionally.
        assert!(!r.is_settlement_proof());
        let v = verify_against(&r, &pk).unwrap();
        assert!(!v.is_settled(), "verification must never imply settlement");

        // (2) The discriminator is exactly the acceptance string, and the note
        //     spelling out the difference rides along signed.
        assert_eq!(r.finality, "accepted, not settled");
        assert!(r.settlement.note.contains("not proof of settlement"));

        // (3) Rewriting the discriminator to claim settlement fails verification
        //     — you cannot forge a "settled" receipt even by editing the JSON,
        //     and even the structural check refuses it before the signature is
        //     consulted at all.
        for claim in ["settled", "final", "confirmed", "accepted and settled"] {
            let mut t = r.clone();
            t.finality = claim.to_string();
            assert_eq!(
                verify_against(&t, &pk),
                Err(ReceiptError::NotAnAcceptanceReceipt { found: claim.to_string() }),
                "a receipt claiming {claim:?} must be refused outright"
            );
        }

        // (4) Re-signing the forged claim with the node's OWN key still fails:
        //     the discriminator check is structural, before any crypto.
        let mut forged = r.clone();
        forged.finality = "settled".to_string();
        let resigned = signer.key.sign(&forged.canonical_bytes());
        forged.sig = hex::encode(resigned.to_bytes());
        assert_eq!(
            verify_against(&forged, &pk),
            Err(ReceiptError::NotAnAcceptanceReceipt { found: "settled".to_string() }),
            "not even the signing node can mint a receipt that claims settlement"
        );
    }

    /// A self-consistent receipt from an unknown key must not pass as the node's.
    /// This is the attack `verify_against` exists to stop and the reason the
    /// expected key is a required argument.
    #[test]
    fn a_receipt_signed_by_a_stranger_does_not_pass_as_the_nodes() {
        let node = test_signer();
        let attacker = test_signer();
        let forged = attacker.mint(facts());

        // Internally consistent — the attacker signed their own lie correctly.
        assert!(verify_self_consistent(&forged).is_ok());

        // But it is not the node's receipt.
        match verify_against(&forged, &node.public_key()) {
            Err(ReceiptError::WrongSigner { expected, got }) => {
                assert_eq!(expected, hex::encode(node.public_key()));
                assert_eq!(got, hex::encode(attacker.public_key()));
            }
            other => panic!("expected WrongSigner, got {other:?}"),
        }
    }

    #[test]
    fn canonical_bytes_are_domain_separated_from_every_other_signature_in_this_chain() {
        let signer = test_signer();
        let canon = signer.mint(facts()).canonical_bytes();

        // Starts with the u32-LE length of the domain tag, then the tag.
        assert_eq!(&canon[..4], &(RECEIPT_DOMAIN.len() as u32).to_le_bytes());
        assert_eq!(&canon[4..4 + RECEIPT_DOMAIN.len()], RECEIPT_DOMAIN.as_bytes());

        // `SigilBlockHeaderV0::signing_bytes()` is `serde_json::to_vec` of a
        // struct, so it always begins with `{` (0x7b). A receipt begins with
        // 0x1b. Neither can ever be replayed as the other, which matters because
        // the producer key signs both when `key_provenance` is
        // `producer-consensus-key`.
        assert_ne!(canon[0], b'{', "must not collide with a JSON header payload");
        assert_eq!(canon[0], RECEIPT_DOMAIN.len() as u8);

        // And it is not the wallet RPC-auth scheme's ASCII message either.
        assert!(!canon.starts_with(b"sigil-rpc/v1|"));
    }

    /// The encoding must be injective: two different receipts must never produce
    /// the same signing input. The absent-vs-empty case is the classic way a
    /// hand-rolled encoder gets this wrong.
    #[test]
    fn absent_and_empty_fields_do_not_collide() {
        let signer = test_signer();
        let mut a = signer.mint(facts());
        a.wallet = None;
        let mut b = signer.mint(facts());
        b.wallet = Some(String::new());
        assert_ne!(a.canonical_bytes(), b.canonical_bytes());

        let mut c = signer.mint(facts());
        c.req_nonce = None;
        let mut d = signer.mint(facts());
        d.req_nonce = Some(0);
        assert_ne!(c.canonical_bytes(), d.canonical_bytes());
    }

    /// The proof-authorized shielded paths genuinely have no submitter. A receipt
    /// for one must say so rather than invent an address.
    #[test]
    fn a_shielded_send_receipt_carries_no_wallet_and_still_verifies() {
        let signer = test_signer();
        let r = signer.mint(AcceptanceFacts {
            kind: SubmitKind::ShieldedSend,
            tx_id: [0x5A; 32],
            wallet: None,
            req_nonce: None,
            accepted_at_height: Some(100),
        });
        assert!(r.wallet.is_none());
        assert!(r.req_nonce.is_none());
        let v = verify_against(&r, &signer.public_key()).unwrap();
        assert_eq!(v.kind, SubmitKind::ShieldedSend);
        assert_eq!(v.wallet, None);
    }

    #[test]
    fn key_selection_prefers_dedicated_then_producer_then_ephemeral() {
        let dedicated = hex::encode([0x11u8; 32]);
        let producer = hex::encode([0x22u8; 32]);

        let (k, p) = select_key(Some(&dedicated), Some(&producer));
        assert_eq!(p, KeyProvenance::DedicatedReceiptKey);
        assert_eq!(k.to_bytes(), [0x11u8; 32]);

        let (k, p) = select_key(None, Some(&producer));
        assert_eq!(p, KeyProvenance::ProducerConsensusKey);
        assert_eq!(k.to_bytes(), [0x22u8; 32]);
        assert!(p.is_externally_anchorable());

        let (_, p) = select_key(None, None);
        assert_eq!(p, KeyProvenance::EphemeralProcessKey);
        assert!(
            !p.is_externally_anchorable(),
            "an ephemeral key must never claim to be anchorable — that would be the \
             one dishonest thing this module could do"
        );

        // A malformed seed degrades to the next option, never panics.
        let (_, p) = select_key(Some("not-a-seed"), Some(&producer));
        assert_eq!(p, KeyProvenance::ProducerConsensusKey);
        let (_, p) = select_key(Some(&"zz".repeat(32)), None);
        assert_eq!(p, KeyProvenance::EphemeralProcessKey);
    }

    /// Two ephemeral keys must differ, or "per-process" would be a lie and every
    /// node would share one forgeable identity.
    #[test]
    fn ephemeral_keys_are_actually_random() {
        let (a, _) = select_key(None, None);
        let (b, _) = select_key(None, None);
        assert_ne!(a.to_bytes(), b.to_bytes());
    }

    /// The ETA arithmetic, checked against the measured rate rather than asserted.
    /// 512 blocks at 5.66 blk/s = 90.5 s, rounded UP to 91.
    #[test]
    fn the_eta_is_derived_from_the_measured_block_rate_and_rounds_up() {
        let signer = test_signer();
        let r = signer.mint(facts());
        assert_eq!(r.settlement.eta_basis_block_rate_mhz, DEFAULT_BLOCK_RATE_MHZ);
        let expected = (DEFAULT_SETTLEMENT_DEPTH_BLOCKS * 1_000 + DEFAULT_BLOCK_RATE_MHZ - 1)
            / DEFAULT_BLOCK_RATE_MHZ;
        assert_eq!(r.settlement.eta_seconds_estimate, expected);
        assert_eq!(expected, 91, "512 blk / 5.66 blk/s = 90.5 s, rounded up");
        assert!(r.settlement.eta_is_estimate);
    }

    /// A receipt whose advertised settlement height does not follow from its own
    /// accepted-at height is refused even before the signature is checked.
    #[test]
    fn an_internally_inconsistent_settlement_height_is_refused() {
        let signer = test_signer();
        let mut r = signer.mint(facts());
        r.settlement.earliest_settlement_height = Some(r.accepted_at_height.unwrap() + 1);
        assert!(matches!(
            verify_against(&r, &signer.public_key()),
            Err(ReceiptError::InconsistentSettlementHeight { .. })
        ));
    }

    /// The receipt must survive the JSON round trip a client actually performs —
    /// signing canonical bytes is pointless if serde cannot carry the fields.
    #[test]
    fn a_receipt_survives_a_json_round_trip() {
        let signer = test_signer();
        let r = signer.mint(facts());
        let json = serde_json::to_string(&r).unwrap();
        let back: AcceptanceReceipt = serde_json::from_str(&json).unwrap();
        assert_eq!(r, back);
        assert!(verify_against(&back, &signer.public_key()).is_ok());
    }

    /// Backs the claim in `ReceiptSigner::mint`'s docs that signing is safe on the
    /// request path. Bound is deliberately loose (this box also runs a production
    /// node) — the point is the order of magnitude, and the real figure is printed.
    #[test]
    fn signing_is_cheap_enough_for_the_request_path() {
        let signer = test_signer();
        let n = 200;
        let t0 = std::time::Instant::now();
        for i in 0..n {
            let mut f = facts();
            f.req_nonce = Some(i);
            let _ = signer.mint(f);
        }
        let per = t0.elapsed() / n as u32;
        println!("mint (canonical encode + Ed25519 sign) = {per:?} per receipt");
        assert!(
            per < std::time::Duration::from_millis(50),
            "minting took {per:?} per receipt — that is no longer a sub-millisecond ack"
        );
    }
}
