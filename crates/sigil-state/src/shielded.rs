//! SHIELDED POOL STATE (PV-1 step 3, 2026-08-23).
//!
//! The consensus-side half of SIGIL's private transfers: the note-commitment tree (whose
//! root is the anonymity set) and the nullifier set (the double-spend guard). The circuit
//! side lives in `sigil-shield`; this module is what a node actually stores, and the two
//! are bound together by `sigil_shield::note_v1`, which is the single canonical definition
//! of a commitment and a nullifier.
//!
//! # The value model, stated explicitly
//!
//! SIGIL's supply is transparent in aggregate and private in distribution. Value moves
//! between two domains and the total is conserved across both:
//!
//! ```text
//!   transparent wallets  ──shield──▶  shielded pool   (note commitments)
//!         ▲                                │
//!         └──────────── unshield ──────────┘
//! ```
//!
//! * **shield** — burn `v` from a transparent wallet, append a note commitment worth `v`.
//! * **shielded spend** — consume one note, emit new ones. Amounts stay hidden; the only
//!   public numbers are the fee and the output commitments.
//! * **unshield** — consume a note, reveal `v`, credit a transparent wallet.
//!
//! [`ShieldedPool::value_locked`] tracks the pool's total so
//! `native_supply + value_locked` is the quantity the 21M cap applies to. Without that
//! accounting a shield would look like a burn and the cap would drift down every time
//! someone used privacy.
//!
//! # Why the root is recomputed rather than cached incrementally
//!
//! The circuit proves membership against a fixed-depth tree, so the pool is padded to
//! `2^DEPTH` leaves and the root is a pure function of the leaf vector. An incremental
//! append-only root is a known optimization and deliberately not done yet: a wrong
//! incremental root is a consensus split, and correctness first is the rule that the
//! `wallet_acc` accumulator earned the right to break only after it was proven. See
//! [`ShieldedPool::root`] for the cost note.

use std::collections::{BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::WalletId;

/// Tree depth for the shielded pool. `DEPTH + 1` must be a power of two because the
/// spend AIR's trace length is `(DEPTH+1)·64`; 15 gives a 32,768-note anonymity set.
pub const POOL_DEPTH: usize = 15;

/// Maximum notes the pool can hold at [`POOL_DEPTH`].
pub const POOL_CAPACITY: usize = 1 << POOL_DEPTH;

// ── PRIVACY PARAMETERS ──────────────────────────────────────────────────────────────
//
// Two structural leaks survive the cryptography, and both are closed by protocol rule
// rather than by better proofs. Neither needs a height gate: `Shield`, `ShieldedSpend` and
// `Unshield` have never appeared in a settled block, so there is no history whose
// validation these could change.

/// The ONE fee every shielded send must pay.
///
/// A freely-chosen fee is a fingerprint. If Alice always pays 1337 and Bob always pays
/// 9000, their transactions are trivially separable inside the anonymity set — the amounts
/// are hidden but the *fee* is public, and a distinctive fee identifies the sender as
/// effectively as a signature would. One mandatory value means the fee carries zero bits
/// about who sent the transaction.
///
/// The cost is that there is no fee market and therefore no fee-based priority. That is
/// the correct trade for a privacy chain: a fee market is an auction in which the bid is
/// public, and a public bid is an identifier.
pub const SHIELDED_FEE: u128 = 100_000;

/// Allowed shield / unshield amounts.
///
/// The ramps are transparent by nature — moving value between the transparent and shielded
/// domains necessarily names a wallet and an amount. That makes VALUE CORRELATION the
/// cheapest attack on this whole design: shield exactly 7,431,902 and unshield exactly
/// 7,431,902 an hour later, and an observer links the two without touching a single proof.
///
/// A coarse ladder collapses that. With everyone shielding the same handful of round
/// numbers, an amount identifies a bucket rather than a person, and someone moving an
/// unusual sum must split it across several ramp operations — which is exactly the
/// behaviour that makes correlation expensive.
///
/// 1/2/5 x powers of ten, from 1 up to 10^15 (the 21M cap is 2.1x10^15 raw).
///
/// # Why it reaches both extremes
///
/// The first version ran 10^3..10^9 and was simply wrong: chosen without checking it
/// against a real balance on this chain. The master wallet holds 19,930,436,350,512 raw,
/// which at a 10^9 ceiling is **19,930 separate shield operations** — not a privacy
/// trade-off, just a broken feature.
///
/// The bottom matters as much as the top. Stopping at 10^3 leaves a remainder that can
/// never enter the pool: that same balance is not a multiple of 1,000 (it ends in 512), so
/// 512 raw would be permanently stranded in the transparent domain. Including 1/2/5 means
/// EVERY integer amount decomposes exactly, and a wallet can move its whole balance rather
/// than "almost all of it".
///
/// Cost of the wide range: more buckets means fewer users per bucket, which is a real
/// privacy cost and is measured rather than assumed — see the chronos
/// `denominations_measurably_defeat_value_correlation` scenario. The small tail is less
/// damaging than it looks, because odd balances are universal: everyone's remainder
/// produces small-denomination notes, so those buckets fill from the whole population.
pub const DENOMINATIONS: &[u128] = &[
    1, 2, 5,
    10, 20, 50,
    100, 200, 500,
    1_000, 2_000, 5_000,
    10_000, 20_000, 50_000,
    100_000, 200_000, 500_000,
    1_000_000, 2_000_000, 5_000_000,
    10_000_000, 20_000_000, 50_000_000,
    100_000_000, 200_000_000, 500_000_000,
    1_000_000_000, 2_000_000_000, 5_000_000_000,
    10_000_000_000, 20_000_000_000, 50_000_000_000,
    100_000_000_000, 200_000_000_000, 500_000_000_000,
    1_000_000_000_000, 2_000_000_000_000, 5_000_000_000_000,
    10_000_000_000_000, 20_000_000_000_000, 50_000_000_000_000,
    100_000_000_000_000, 200_000_000_000_000, 500_000_000_000_000,
    1_000_000_000_000_000, 2_000_000_000_000_000, 5_000_000_000_000_000,
    // Rungs added when SIGIL moved from 8 to 10 decimals. The ladder is written in base
    // units (glyphs), so every rung became 100x SMALLER in coin terms overnight: the old top
    // rung was 50,000,000 SIGIL, comfortably above the supply cap, and at 10 dp the same
    // number means 500,000 SIGIL. Without these, shielding a large holding would have
    // silently required many ramps instead of one — and many ramps is exactly the
    // correlation pattern denominations exist to prevent.
    10_000_000_000_000_000, 20_000_000_000_000_000, 50_000_000_000_000_000,
    100_000_000_000_000_000, 200_000_000_000_000_000,
];

// The ladder must reach the supply cap (or no single ramp can move a large holding) and must
// stay inside the shielded circuit's range bound (or a legal denomination could not be
// proven). 2^58 = 28,823,038 SIGIL at 10 dp, so 20,000,000 is the last rung that fits —
// 50,000,000 would not, which is why the 1/2/5 pattern stops at 2 here.
const _: () = assert!(
    DENOMINATIONS[DENOMINATIONS.len() - 1] >= crate::MAX_SUPPLY / 2,
    "the ladder must cover a substantial fraction of the supply in one ramp"
);
const _: () = assert!(
    DENOMINATIONS[DENOMINATIONS.len() - 1] < (1u128 << 58),
    "every denomination must be provable inside the shielded circuit's range bound"
);

/// Is `amount` one of the permitted ramp denominations?
pub fn is_denomination(amount: u128) -> bool {
    DENOMINATIONS.binary_search(&amount).is_ok()
}

/// The largest denomination not exceeding `amount` — for a wallet splitting a payment
/// into legal ramp operations.
pub fn largest_denomination_at_most(amount: u128) -> Option<u128> {
    DENOMINATIONS.iter().rev().copied().find(|d| *d <= amount)
}

/// Decompose `amount` into permitted denominations, greedily. Returns `None` if the
/// remainder cannot be expressed (i.e. `amount` is not a multiple of the smallest one).
pub fn decompose(amount: u128) -> Option<Vec<u128>> {
    let mut left = amount;
    let mut out = Vec::new();
    while left > 0 {
        let d = largest_denomination_at_most(left)?;
        out.push(d);
        left -= d;
    }
    Some(out)
}

/// Errors from shielded-state transitions.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ShieldedError {
    #[error("double-spend: nullifier {0:02x?} already spent")]
    NullifierAlreadySpent([u8; 32]),
    #[error("shielded pool is full ({POOL_CAPACITY} notes); a pool epoch rotation is required")]
    PoolFull,
    #[error("unshield of {requested} exceeds the pool's locked value {locked}")]
    UnshieldExceedsLocked { requested: u128, locked: u128 },
    #[error("shielded value overflow")]
    ValueOverflow,
    #[error("spend proof rejected: {0}")]
    ProofRejected(String),
    #[error(
        "shielded send must pay exactly the fixed fee {expected} (got {got}) — a chosen fee \
         is a fingerprint that identifies the sender"
    )]
    WrongFee { expected: u128, got: u128 },
    #[error(
        "{amount} is not a permitted ramp denomination — shield/unshield in standard \
         amounts so values cannot be correlated across the transparent boundary"
    )]
    NotADenomination { amount: u128 },

    /// A note commitment that already exists in the pool was submitted again.
    ///
    /// 2026-08-25: `Shield`, `Unshield` and `ShieldedCoinbase` all mint a note by calling
    /// [`ShieldedPool::append_note`]/[`append_note_with_delivery`](ShieldedPool::append_note_with_delivery)
    /// with NO check that the commitment is new — unlike a spend, which cannot replay
    /// because [`spend_nullifier`](ShieldedPool::spend_nullifier) rejects a repeated
    /// nullifier before anything is appended. `Shield` has no nullifier (it is not
    /// spending an existing note), so nothing stopped the SAME signed `Shield`
    /// transaction — riding on `sigil-api`'s pending-pool retry/re-embed queue
    /// (`ShieldedBridge::snapshot_for_mint`, which keeps re-including a not-yet-confirmed
    /// tx into every new candidate block until `confirm_applied` fires, ~`final_depth`
    /// blocks / ~30 minutes later) — from being embedded, honestly, in many different
    /// candidate blocks before the first one ever reached finality. Each of those blocks
    /// independently carried the identical `Shield{from, amount, cm, fee}` payload, and
    /// each one, on landing, re-executed the debit and re-appended the same `cm` — a live
    /// production incident that produced 513 duplicate applications of a single 100-unit
    /// test deposit before it timed out. `confirm_applied` being wired (see
    /// `sigil-node/src/dag.rs`) stops NEW candidates from re-embedding it once ONE
    /// containing block lands, but by then every candidate minted before that point
    /// already has the tx baked into its body — that fix caps the damage, it does not
    /// prevent it. The real invariant a commitment tree needs is the same one a
    /// nullifier set already has: a value, once inserted, is never inserted again. A
    /// commitment is a pseudorandom output (`compress2(compress2(value, blinding),
    /// key)`); two HONEST, distinct notes colliding is cryptographically negligible, so
    /// rejecting a repeat can never refuse a genuine transaction — only a replay (or a
    /// client reusing a blinding, which is itself a note-privacy bug worth refusing).
    #[error("duplicate note commitment {0:02x?} — already in the pool (replay?)")]
    DuplicateCommitment([u8; 32]),
}

/// The shielded pool: append-only note commitments plus the spent-nullifier set.
///
/// Fields are private with `pub(crate)` mutators for the same reason the rest of
/// `SigilState` is: every write must arrive through `commit_state_transition`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShieldedPool {
    /// Note commitments in insertion order. Index IS the leaf position the nullifier
    /// binds to, so this vector must never be reordered or compacted.
    pub(crate) notes: Vec<[u8; 32]>,
    /// Every nullifier ever revealed. Membership here means "already spent".
    pub(crate) nullifiers: BTreeSet<[u8; 32]>,
    /// Total value currently locked in the pool. Increased by shield, decreased by
    /// unshield, unchanged by a shielded-to-shielded spend.
    pub(crate) value_locked: u128,
    /// Recent anonymity-set roots this pool has held, newest last.
    ///
    /// A spend proves membership against a root, and by the time its transaction is mined
    /// the pool has usually moved on. Requiring the *current* root would make every
    /// concurrent spend fail; accepting *any* root would let a prover invent a tree
    /// containing a note of any value. A bounded window of genuinely-held roots is the
    /// standard resolution (Zcash calls these anchors).
    pub(crate) anchors: VecDeque<[u8; 32]>,
    /// Set when the note set changes, so the next root query recomputes rather than
    /// serving a stale cached value.
    #[serde(skip)]
    pub(crate) anchors_dirty: bool,

    // ── APPEND AFTER THIS LINE ONLY ─────────────────────────────────────────
    // Same positional-encoding rule as `SigilState::shielded`: rmp_serde writes structs
    // as arrays, so a field inserted above shifts every later one when an older snapshot
    // is read. New fields go last, with `#[serde(default)]`.
    /// Wallets that have published a shielded public key, so value destined for them can
    /// be minted straight into the pool instead of landing transparently.
    ///
    /// Registration is one transparent transaction and is permanent-by-default: it is how
    /// a miner says "pay me privately from now on". A wallet that never registers keeps
    /// receiving transparent rewards, so this cannot break an existing miner.
    #[serde(default)]
    pub(crate) addresses: std::collections::BTreeMap<WalletId, [u8; 32]>,

    /// Wallets that have also published an X25519 note-delivery key (`pk_enc` in
    /// `sigil_shield::note_cipher::ShieldedAddress`). A DIFFERENT key from `addresses`
    /// above on purpose: `pk_shield` is a Goldilocks field element the CIRCUIT binds a
    /// note to, `pk_enc` is an X25519 curve point ciphertexts are sealed to — a hash
    /// image cannot perform a key exchange, so one key cannot do both jobs. A wallet
    /// that registered before note delivery existed has an `addresses` entry with no
    /// matching one here; senders that find no `encrypt_keys` entry cannot deliver a
    /// ciphertext to that wallet (it can still be paid, it just cannot auto-discover
    /// the payment — see `note_ciphertexts` below).
    #[serde(default)]
    pub(crate) encrypt_keys: std::collections::BTreeMap<WalletId, [u8; 32]>,

    /// A delivery ciphertext per note, positionally aligned with `notes` (same index,
    /// `None` where no delivery was attached). Only `ShieldedSend` outputs carry one —
    /// `Shield`, `Unshield` and `ShieldedCoinbase` outputs are notes the creator already
    /// knows the preimage of, so there is nothing to deliver. This is what lets a
    /// recipient who was never told anything out of band still discover a payment: the
    /// wallet trial-decrypts every entry here against its own key, and a successful open
    /// IS the ownership proof (`sigil_shield::note_cipher`).
    #[serde(default)]
    pub(crate) note_ciphertexts: Vec<Option<String>>,

    /// Derived index over `notes`, rebuilt on demand. `serde(skip)` because it is a cache:
    /// persisting it would create a second copy of the truth that could drift from
    /// `notes`, which is exactly the class of bug that killed `sigil-rpcd`.
    #[serde(skip)]
    pub(crate) tree: Option<sigil_shield::note_v1::IncrementalTree>,

    /// Which pool generation the *live* tree is. Epoch 0 is every chain that has never
    /// rotated, and its behaviour is byte-for-byte what it was before rotation existed.
    #[serde(default)]
    pub(crate) epoch: u32,

    /// Every SEALED epoch, oldest first — index IS the epoch number. Kept forever, not
    /// windowed, because a note in a sealed epoch must stay spendable for as long as the
    /// chain lives: its owner proves membership against `root`, and a wallet that has
    /// never scanned still needs `notes`/`ciphertexts` to find and open it.
    #[serde(default)]
    pub(crate) archive: Vec<EpochArchive>,

    /// Which epoch each windowed anchor belonged to, so a spend arriving against a root
    /// from before a rotation is scoped to the right generation. Absent ⇒ epoch 0, which
    /// is exactly right for a snapshot written before rotation existed.
    #[serde(default)]
    pub(crate) anchor_epoch: std::collections::BTreeMap<[u8; 32], u32>,

    /// Wallets that have published a SQIsign L5 public key (129 bytes, `sqisign-rs 0.3`
    /// via `flux-sqisign`).
    ///
    /// The shielded pool has two authorization models and only one of them was
    /// post-quantum. A `ShieldedSend` carries no signature at all — the STARK IS the
    /// authorization — so it is already PQ. But the RAMPS (`Shield`, `Unshield`) and
    /// registration itself are authorized by an Ed25519 signature over the wallet key,
    /// and a wallet id IS an Ed25519 public key. A quantum adversary forges that signature
    /// and shields someone else's balance into the pool, or unshields it out.
    ///
    /// Once a wallet appears here, every ramp operation it signs must ALSO carry a valid
    /// SQIsign signature — require-both, never either-or. Either-or would be worthless:
    /// an adversary who breaks Ed25519 simply presents the Ed25519 half. And because
    /// removal is not a supported operation, a wallet that has upgraded cannot be
    /// downgraded back to Ed25519-only by anyone, including itself — which is what makes
    /// this a real guarantee rather than a preference.
    ///
    /// 292-byte signatures, 129-byte keys: small enough to carry per-ramp-transaction,
    /// unlike Dilithium5's ~4.6 KB.
    #[serde(default)]
    pub(crate) sqi_keys: std::collections::BTreeMap<WalletId, Vec<u8>>,

    /// Every commitment this pool has EVER held, across all epochs. A cache (`serde(skip)`,
    /// same reasoning as `tree`): rebuilt on demand from `notes` + `archive`, so it cannot
    /// drift from the truth. It exists because the duplicate-commitment replay guard has
    /// to keep working after a rotation clears `notes` — without it, rotation would quietly
    /// re-open the exact replay hole `DuplicateCommitment` was added to close.
    #[serde(skip)]
    pub(crate) seen_commitments: Option<BTreeSet<[u8; 32]>>,
}

/// A sealed pool generation: the anonymity set exactly as it stood when it filled.
///
/// Sealed means frozen, NOT discarded. `root` stays a permanently-valid anchor and the
/// leaves stay published, because the alternative is confiscation: a note whose tree can
/// no longer be reconstructed can never produce an inclusion proof, and the value behind
/// it is gone. Rotation must cost a user nothing except that their anonymity set stops
/// growing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EpochArchive {
    /// The final anonymity-set root of this epoch — a permanent anchor.
    pub root: [u8; 32],
    /// The full leaf vector, positions unchanged. Position is what the nullifier binds to,
    /// so this must never be reordered or compacted.
    pub notes: Vec<[u8; 32]>,
    /// Delivery ciphertexts, positionally aligned with `notes`, so a wallet can still
    /// trial-decrypt a sealed epoch and discover a payment it never scanned for.
    pub ciphertexts: Vec<Option<String>>,
}

/// Length of a SQIsign L5 public key, in bytes.
///
/// Stated here rather than imported from `flux-sqisign` on purpose. This crate is the
/// consensus STATE — it decides what the chain holds — and pulling an isogeny-crypto
/// dependency into it to learn one integer would put a large, slow-building crate on the
/// critical path of every consumer, including ones that must stay lightweight. The actual
/// signature VERIFICATION lives where the signature arrives (the API/tx layer), which
/// legitimately depends on `flux-sqisign`; all this layer needs is "is this a plausible
/// key at all", so that a malformed registration cannot leave a wallet holding an
/// unusable second factor it can never satisfy.
///
/// Pinned against `flux_sqisign::public_key_size()` by a test in the crate that DOES link
/// it, so the two cannot silently drift.
pub const SQI_PUBLIC_KEY_LEN: usize = 129;

/// How many historical roots stay spendable. At one root per block this is a ~256-block
/// window for a transaction to be mined before its anchor expires.
pub const ANCHOR_WINDOW: usize = 256;

/// The spent-set key for `nf` observed in `epoch`.
///
/// # Why the raw nullifier is not enough once the pool rotates
///
/// The circuit derives `nf = compress2(spend_key, position)` where `position` is the leaf
/// index INSIDE the tree the proof members against — it is recovered from the Merkle
/// path's direction bits (`spend_full_v4::path_position`), so it is structurally confined
/// to `0..2^POOL_DEPTH` and cannot be made globally monotonic without changing the AIR.
///
/// So the moment a second epoch starts numbering leaves from zero again, one wallet
/// holding position 7 in epoch 0 and position 7 in epoch 1 derives the SAME nullifier for
/// two genuinely different notes. The spent-set would reject the second as a double-spend
/// and that note could never be spent by anyone, ever — silent confiscation, caused by the
/// very mechanism meant to keep the pool usable.
///
/// Scoping the key by epoch removes the collision without touching the circuit: the proof
/// still publishes the raw `nf`, and the node — which knows from the anchor which epoch
/// was proven against — files it under a key unique to that generation.
///
/// Epoch 0 is the IDENTITY on purpose. Every nullifier ever recorded before rotation
/// existed was stored raw, so this keeps those entries valid and keeps the double-spend
/// guard on a pre-rotation chain exactly as strong as it was.
pub fn scoped_nullifier(epoch: u32, nf: [u8; 32]) -> [u8; 32] {
    if epoch == 0 {
        return nf;
    }
    let mut h = blake3::Hasher::new();
    h.update(b"sigil-shielded-nullifier-epoch-v1");
    h.update(&epoch.to_le_bytes());
    h.update(&nf);
    *h.finalize().as_bytes()
}

impl ShieldedPool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of real (unpadded) notes.
    pub fn len(&self) -> usize {
        self.notes.len()
    }
    pub fn is_empty(&self) -> bool {
        self.notes.is_empty()
    }

    /// The real (unpadded) note commitments, in insertion order — index IS leaf position.
    ///
    /// 2026-08-23: this is what a wallet/miner actually needs to locate its own notes and
    /// build a spend's inclusion path. Padding is client-derivable from
    /// [`sigil_shield::note_v1::padding_leaf_wire`] (same formula server and client both
    /// use), so there is no reason to ship the padded tail over the wire.
    pub fn notes(&self) -> &[[u8; 32]] {
        &self.notes
    }

    /// Total value locked in the shielded domain. `native_supply + value_locked` is what
    /// the 21M cap governs.
    pub fn value_locked(&self) -> u128 {
        self.value_locked
    }

    /// Is this a root the pool genuinely held — either inside the rolling window, or as
    /// the sealed final root of a past epoch?
    ///
    /// Sealed roots never expire. The window exists so a spend has time to be mined
    /// before the set moves on; a SEALED epoch's set never moves again, so there is
    /// nothing to expire, and expiring it would strand every note it contains.
    pub fn is_known_anchor(&self, root: &[u8; 32]) -> bool {
        self.anchors.contains(root) || self.archive.iter().any(|a| a.root == *root)
    }

    /// Which epoch's tree does this anchor belong to?
    ///
    /// The spend proof publishes a raw nullifier; scoping it correctly (see
    /// [`scoped_nullifier`]) requires knowing which generation was proven against, and the
    /// anchor is the only thing in the transaction that says so. An anchor the pool does
    /// not know returns `None` and the caller must refuse the spend.
    pub fn epoch_of_anchor(&self, root: &[u8; 32]) -> Option<u32> {
        if let Some(e) = self.anchor_epoch.get(root) {
            return Some(*e);
        }
        if let Some(i) = self.archive.iter().position(|a| a.root == *root) {
            return Some(i as u32);
        }
        // A windowed anchor with no recorded epoch is a root written by a node that
        // predates rotation, which can only ever be epoch 0.
        if self.anchors.contains(root) {
            return Some(0);
        }
        None
    }

    /// The live epoch number. 0 on any chain that has never rotated.
    pub fn epoch(&self) -> u32 {
        self.epoch
    }

    /// The sealed epochs, oldest first — index IS the epoch number.
    pub fn archive(&self) -> &[EpochArchive] {
        &self.archive
    }

    /// Record a root as spendable, evicting beyond [`ANCHOR_WINDOW`].
    pub(crate) fn push_anchor(&mut self, root: [u8; 32]) {
        if self.anchors.contains(&root) {
            return;
        }
        self.anchors.push_back(root);
        self.anchor_epoch.insert(root, self.epoch);
        while self.anchors.len() > ANCHOR_WINDOW {
            if let Some(evicted) = self.anchors.pop_front() {
                // A sealed root outlives the window — it is the permanent anchor of a
                // whole generation, so its epoch mapping must survive eviction too.
                if !self.archive.iter().any(|a| a.root == evicted) {
                    self.anchor_epoch.remove(&evicted);
                }
            }
        }
    }

    /// Seal the live epoch and open an empty one.
    ///
    /// This is the answer to `PoolFull`. The tree the circuit proves against is
    /// fixed-depth, so an anonymity set has a hard ceiling of `2^POOL_DEPTH` leaves and a
    /// pool that only ever grows must eventually stop accepting notes. Rotation gives the
    /// pool somewhere to keep going without invalidating anything already in it.
    ///
    /// What is CARRIED, and why each one has to be:
    /// * `nullifiers` — the double-spend guard. Forgetting a spent nullifier would let
    ///   an already-consumed note be spent again; the set is monotone forever.
    /// * `value_locked` — rotation moves no value. The 21M cap is computed from
    ///   `native_supply + value_locked`, so dropping it would silently unbalance supply.
    /// * `addresses` / `encrypt_keys` — registrations are about wallets, not generations.
    ///
    /// What is SEALED (moved into [`EpochArchive`], never deleted): the leaf vector and
    /// its ciphertexts, plus the final root, which becomes a permanent anchor.
    ///
    /// Determinism: this is driven purely by the note count crossing a constant, inside
    /// the one append chokepoint, so every node rotates at exactly the same transaction of
    /// exactly the same block. Nothing here reads a clock, a config value or a local
    /// preference — a rotation that happened on one node and not another would be a
    /// consensus split.
    pub(crate) fn rotate_epoch(&mut self) {
        let root = self.current_root_fast();
        self.archive.push(EpochArchive {
            root,
            notes: std::mem::take(&mut self.notes),
            ciphertexts: std::mem::take(&mut self.note_ciphertexts),
        });
        // The sealed root stays spendable forever; record its epoch before bumping.
        self.anchor_epoch.insert(root, self.epoch);
        self.epoch += 1;
        // Build the all-epoch replay index now. Before any rotation the guard's fallback
        // scan is over the live notes only (what it always was); after one it would grow
        // linearly with every sealed epoch, so this is the moment to pay for the index.
        // Every node rotates at the same transaction, so every node builds it together.
        self.ensure_commitment_index();
        // Fresh, empty tree for the new generation.
        self.tree = None;
        // `seen_commitments` spans ALL epochs, so it deliberately survives rotation — the
        // archived commitments are still off-limits for a replay.
        self.anchors_dirty = true;
    }

    /// Mark the note set as changed. The producer calls [`refresh_anchor`] at block close
    /// to publish the new root; separating the two keeps the expensive tree build off the
    /// per-mutation path.
    pub(crate) fn remember_anchor_dirty(&mut self) {
        self.anchors_dirty = true;
    }

    pub fn anchors_dirty(&self) -> bool {
        self.anchors_dirty
    }

    /// Recompute the anonymity-set root and add it to the anchor window.
    ///
    /// Cost note: this builds the full `2^POOL_DEPTH`-leaf MiMC tree — 32,768 leaves at 63
    /// rounds each. It is deliberately called once per block at close, not per mutation.
    /// An incremental append-only root is the known optimization and is NOT done yet,
    /// because a wrong incremental root is a consensus split.
    pub(crate) fn refresh_anchor(&mut self) {
        let root = self.current_root_fast();
        self.push_anchor(root);
        self.anchors_dirty = false;
    }

    /// The shielded key a wallet has published, if any.
    pub fn shielded_address(&self, wallet: &WalletId) -> Option<[u8; 32]> {
        self.addresses.get(wallet).copied()
    }

    pub fn registered_addresses(&self) -> usize {
        self.addresses.len()
    }

    /// Publish a wallet's shielded key. Re-registering replaces it — a user who loses a
    /// seed must be able to redirect future income without abandoning the wallet.
    pub(crate) fn set_address(&mut self, wallet: WalletId, pk: [u8; 32]) {
        self.addresses.insert(wallet, pk);
    }

    /// The X25519 note-delivery key a wallet has published, if any.
    pub fn encrypt_key(&self, wallet: &WalletId) -> Option<[u8; 32]> {
        self.encrypt_keys.get(wallet).copied()
    }

    /// The SQIsign L5 public key a wallet has published, if any. `Some` means every ramp
    /// operation from this wallet MUST carry a valid SQIsign signature as well as Ed25519.
    pub fn sqi_key(&self, wallet: &WalletId) -> Option<&[u8]> {
        self.sqi_keys.get(wallet).map(|v| v.as_slice())
    }

    /// How many wallets have upgraded to post-quantum ramp authorization.
    pub fn sqi_registered(&self) -> usize {
        self.sqi_keys.len()
    }

    /// Publish a wallet's SQIsign key. UPGRADE-ONLY, deliberately: re-registering a
    /// DIFFERENT key is allowed (key rotation), but there is no path that removes one.
    /// A downgrade would let an adversary who can forge Ed25519 strip the second factor
    /// and then use the first, which is exactly the attack this defends against.
    pub(crate) fn set_sqi_key(&mut self, wallet: WalletId, pk: Vec<u8>) {
        if pk.len() == SQI_PUBLIC_KEY_LEN {
            self.sqi_keys.insert(wallet, pk);
        }
    }

    /// Publish a wallet's note-delivery key. Re-registering replaces it, same as
    /// [`set_address`](Self::set_address).
    pub(crate) fn set_encrypt_key(&mut self, wallet: WalletId, pk: [u8; 32]) {
        self.encrypt_keys.insert(wallet, pk);
    }

    /// Delivery ciphertexts in leaf-position order, `None` where a note carries none.
    /// Same length and index alignment as [`notes`](Self::notes) — a wallet scanning for
    /// its own payments zips this against the commitment list.
    pub fn ciphertexts(&self) -> &[Option<String>] {
        &self.note_ciphertexts
    }

    /// The incremental tree over `notes`, rebuilt if this pool was just deserialized.
    fn tree_mut(&mut self) -> &mut sigil_shield::note_v1::IncrementalTree {
        if self.tree.is_none() {
            let mut t = sigil_shield::note_v1::IncrementalTree::new();
            for cm in &self.notes {
                t.append(sigil_shield::note_v1::from_wire(cm).unwrap_or_default());
            }
            self.tree = Some(t);
        }
        self.tree.as_mut().unwrap()
    }

    /// The current anonymity-set root, as the circuit computes it.
    ///
    /// Delegates to `sigil-shield` rather than reimplementing the tree here — duplicating
    /// the circuit's hash in this crate is exactly the divergence PV-1 exists to prevent.
    /// Cost note: O(real notes + depth), NOT O(capacity). It builds over the real prefix
    /// and splices in precomputed all-padding subtree roots. Chronos measured the naive
    /// full-capacity rebuild at 107 ms per touching block on the producer's critical path,
    /// which is what motivated this; `sparse_pool_root` is proven to return the identical
    /// root a full build would (`sigil_shield::note_v1::tests::sparse_root_matches_full_build`),
    /// because a root that differs from the circuit's is a silent, total outage.
    pub fn current_root(&self) -> [u8; 32] {
        sigil_shield::note_v1::sparse_pool_root_wire(&self.notes, POOL_CAPACITY)
    }

    /// The same root via the incremental tree — O(depth) rather than O(notes).
    ///
    /// Proven equal to [`current_root`] by `sigil_shield::note_v1::tests::
    /// incremental_matches_sparse_at_every_size`, and re-checked here against live pool
    /// contents by `incremental_root_agrees_with_sparse`.
    pub(crate) fn current_root_fast(&mut self) -> [u8; 32] {
        let t = self.tree_mut();
        sigil_shield::note_v1::to_wire(t.root(POOL_CAPACITY))
    }

    /// Has this nullifier been spent, as observed in `epoch`?
    ///
    /// Always prefer this over [`is_spent`](Self::is_spent) on a spend path: the same raw
    /// `nf` can legitimately appear in two different epochs (see [`scoped_nullifier`]),
    /// and treating those as the same spend would freeze a note that was never spent.
    pub fn is_spent_in_epoch(&self, epoch: u32, nf: &[u8; 32]) -> bool {
        self.nullifiers.contains(&scoped_nullifier(epoch, *nf))
    }

    /// Has this nullifier been spent in epoch 0?
    ///
    /// Retained because epoch 0 is the identity scoping, so this is exactly the
    /// pre-rotation behaviour and every existing caller/test keeps its meaning.
    pub fn is_spent(&self, nf: &[u8; 32]) -> bool {
        self.is_spent_in_epoch(0, nf)
    }

    /// Every spent nullifier, ascending.
    ///
    /// Public by construction: a nullifier IS the double-spend guard, published on every
    /// spend, and it deliberately does not name the note it consumed — that unlinkability
    /// is the whole design. Handing out the set therefore leaks nothing, and it is the
    /// only way a wallet can net spends out of its balance: it derives its own
    /// `nullifier(position) = compress2(spend_key, position)` for each note it can open
    /// and checks membership here. Without it a wallet can only ever report a GROSS
    /// balance that never goes down after a spend — a number that is wrong in the one
    /// direction that matters.
    pub fn nullifiers(&self) -> Vec<[u8; 32]> {
        self.nullifiers.iter().copied().collect()
    }

    pub fn nullifier_count(&self) -> usize {
        self.nullifiers.len()
    }

    /// The note commitment at `position`, if any.
    pub fn note_at(&self, position: usize) -> Option<[u8; 32]> {
        self.notes.get(position).copied()
    }

    /// The leaf vector padded to [`POOL_CAPACITY`], ready to build the tree the circuit
    /// proves against.
    ///
    /// Padding uses a deterministic filler distinct from any real commitment rather than
    /// zeros — a zero leaf is a value an attacker can produce a preimage for by choosing
    /// `value = 0, blinding = 0`, which would let them "prove membership" of a note nobody
    /// ever inserted.
    pub fn padded_leaves(&self, filler: impl Fn(u64) -> [u8; 32]) -> Vec<[u8; 32]> {
        let mut leaves = self.notes.clone();
        for i in leaves.len()..POOL_CAPACITY {
            leaves.push(filler(i as u64));
        }
        leaves
    }

    /// Has this commitment ever been in the pool, in ANY epoch (live or sealed)?
    ///
    /// Backs the duplicate-commitment replay guard. The index is built lazily and held in
    /// a `serde(skip)` cache for the same reason `tree` is: persisting it would create a
    /// second copy of the truth that could drift from `notes`/`archive`.
    pub fn has_ever_held(&self, cm: &[u8; 32]) -> bool {
        if let Some(seen) = self.seen_commitments.as_ref() {
            return seen.contains(cm);
        }
        self.notes.contains(cm) || self.archive.iter().any(|a| a.notes.contains(cm))
    }

    /// Build the all-epoch commitment index. Cheap to call repeatedly — it is a no-op once
    /// built, and `append_note_with_delivery` keeps it in step from then on.
    pub(crate) fn ensure_commitment_index(&mut self) {
        if self.seen_commitments.is_some() {
            return;
        }
        let mut seen: BTreeSet<[u8; 32]> = BTreeSet::new();
        for a in &self.archive {
            seen.extend(a.notes.iter().copied());
        }
        seen.extend(self.notes.iter().copied());
        self.seen_commitments = Some(seen);
    }

    // ── mutators: pub(crate) so only the chokepoint may call them ────────────────────

    /// Append a note commitment, returning its leaf position. No delivery ciphertext —
    /// use [`append_note_with_delivery`](Self::append_note_with_delivery) for a
    /// `ShieldedSend` output that needs one.
    pub(crate) fn append_note(&mut self, cm: [u8; 32]) -> Result<usize, ShieldedError> {
        self.append_note_with_delivery(cm, None)
    }

    /// Append a note commitment together with the delivery ciphertext (if any) a sender
    /// sealed for it, keeping `note_ciphertexts` in lockstep with `notes` — same length,
    /// same index, so a position looked up in one is valid in the other.
    pub(crate) fn append_note_with_delivery(
        &mut self,
        cm: [u8; 32],
        ciphertext: Option<String>,
    ) -> Result<usize, ShieldedError> {
        // ROTATE, don't refuse (2026-08-26). This branch used to return `PoolFull`, which
        // meant the 32,768th note permanently ended shielded sends on the chain: every
        // subsequent private transfer — and every shielded coinbase, so registered miners
        // silently earned nothing — was dropped at apply time with no way back. The
        // fixed-depth circuit means one tree genuinely cannot hold more, so the pool moves
        // to a new generation instead of dying in the old one. See [`rotate_epoch`].
        if self.notes.len() >= POOL_CAPACITY {
            self.rotate_epoch();
        }
        // REPLAY GUARD (2026-08-25) — see `ShieldedError::DuplicateCommitment`'s docs for
        // the incident this closes. This is the ONE shared entry point every mint path
        // (`Shield`, `Unshield`, `ShieldedSpend` outputs, `ShieldedCoinbase`) funnels
        // through, so guarding here protects all of them with one check instead of
        // scattering it per call site — "fix problems at the root, not the symptom."
        // O(n) over at-most-`POOL_CAPACITY` (32,768) 32-byte entries: microseconds,
        // negligible next to the STARK verification already paid on the spend paths, and
        // this is the ONLY guard `Shield` has at all (it carries no nullifier to check).
        // Spans EVERY epoch, not just the live one. Scanning only `notes` would mean a
        // rotation silently re-opened the replay hole this guard exists to close, since
        // rotation empties `notes`.
        if self.has_ever_held(&cm) {
            return Err(ShieldedError::DuplicateCommitment(cm));
        }
        let position = self.notes.len();
        // keep the incremental index in step with the source of truth
        let leaf = sigil_shield::note_v1::from_wire(&cm).unwrap_or_default();
        self.tree_mut().append(leaf);
        self.notes.push(cm);
        self.note_ciphertexts.push(ciphertext);
        if let Some(seen) = self.seen_commitments.as_mut() {
            seen.insert(cm);
        }
        Ok(position)
    }

    /// Record a nullifier as spent within `epoch`. Rejects a repeat — this is the
    /// double-spend guard, and it is epoch-scoped for the reason [`scoped_nullifier`]
    /// spells out: without scoping, rotation turns an honest second note into a phantom
    /// double-spend and confiscates it.
    pub(crate) fn spend_nullifier_in_epoch(
        &mut self,
        epoch: u32,
        nf: [u8; 32],
    ) -> Result<(), ShieldedError> {
        if !self.nullifiers.insert(scoped_nullifier(epoch, nf)) {
            // Report the RAW nullifier: it is what the transaction published and what an
            // operator correlating against a client can actually recognise.
            return Err(ShieldedError::NullifierAlreadySpent(nf));
        }
        Ok(())
    }

    /// Record a nullifier as spent in epoch 0 — the pre-rotation behaviour, unchanged.
    pub(crate) fn spend_nullifier(&mut self, nf: [u8; 32]) -> Result<(), ShieldedError> {
        self.spend_nullifier_in_epoch(0, nf)
    }

    /// Move value into the shielded domain.
    pub(crate) fn lock_value(&mut self, v: u128) -> Result<(), ShieldedError> {
        self.value_locked = self
            .value_locked
            .checked_add(v)
            .ok_or(ShieldedError::ValueOverflow)?;
        Ok(())
    }

    /// Move value out of the shielded domain.
    pub(crate) fn unlock_value(&mut self, v: u128) -> Result<(), ShieldedError> {
        if v > self.value_locked {
            return Err(ShieldedError::UnshieldExceedsLocked {
                requested: v,
                locked: self.value_locked,
            });
        }
        self.value_locked -= v;
        Ok(())
    }

    /// A commitment over the shielded pool for folding into `wallet_state_root`.
    ///
    /// This is NOT the circuit's Merkle root — that is a MiMC `CompressTree` root computed
    /// by `sigil-shield` over [`padded_leaves`](Self::padded_leaves), and computing it here
    /// would mean duplicating the circuit's hash in this crate, which is precisely the
    /// divergence PV-1 exists to prevent. This digest binds the pool's contents into the
    /// header so a node cannot quietly hold a different note set; the anonymity-set root a
    /// spend proves against is supplied by the shield layer.
    pub fn digest(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        h.update(b"sigil-shielded-pool-v1");
        h.update(&(self.notes.len() as u64).to_le_bytes());
        for n in &self.notes {
            h.update(n);
        }
        h.update(&(self.nullifiers.len() as u64).to_le_bytes());
        for nf in &self.nullifiers {
            h.update(nf);
        }
        h.update(&self.value_locked.to_le_bytes());
        // Ciphertexts ride alongside notes and must be identical on every node, same as
        // the notes themselves — a node holding a different delivery ciphertext for the
        // same commitment is a state divergence exactly like a different note would be.
        for ct in &self.note_ciphertexts {
            match ct {
                Some(s) => {
                    h.update(&[1u8]);
                    h.update(&(s.len() as u32).to_le_bytes());
                    h.update(s.as_bytes());
                }
                None => { h.update(&[0u8]); }
            }
        }
        // POST-QUANTUM RAMP KEYS — folded in only once at least one wallet has upgraded.
        //
        // Same structural gate as the epoch state below, and for the same reason: this
        // digest feeds `wallet_state_root`, which settled headers commit to. Folding an
        // empty map in unconditionally would re-root every block already final. While no
        // wallet has registered a SQIsign key the digest is byte-identical to before this
        // field existed; the first registration is itself the activation, and it cannot
        // occur retroactively.
        if !self.sqi_keys.is_empty() {
            h.update(b"sigil-shielded-sqi-keys-v1");
            h.update(&(self.sqi_keys.len() as u64).to_le_bytes());
            for (w, pk) in &self.sqi_keys {
                h.update(w);
                h.update(&(pk.len() as u32).to_le_bytes());
                h.update(pk);
            }
        }
        // EPOCH STATE — folded in only once a rotation has actually happened.
        //
        // This digest folds into `wallet_state_root`, which every settled header commits
        // to, so an unconditional change here would re-root history and invalidate blocks
        // that are already final. Gating on `epoch > 0` makes the digest byte-identical to
        // the pre-rotation one on any chain that has never rotated — the structural
        // equivalent of a height gate, without needing an activation height, because the
        // first rotation is itself the activation and cannot occur retroactively.
        if self.epoch > 0 {
            h.update(b"sigil-shielded-epoch-v1");
            h.update(&self.epoch.to_le_bytes());
            h.update(&(self.archive.len() as u64).to_le_bytes());
            for a in &self.archive {
                h.update(&a.root);
                h.update(&(a.notes.len() as u64).to_le_bytes());
                for n in &a.notes {
                    h.update(n);
                }
                for ct in &a.ciphertexts {
                    match ct {
                        Some(s) => {
                            h.update(&[1u8]);
                            h.update(&(s.len() as u32).to_le_bytes());
                            h.update(s.as_bytes());
                        }
                        None => { h.update(&[0u8]); }
                    }
                }
            }
        }
        *h.finalize().as_bytes()
    }
}

/// Verify a shielded-spend STARK against its public inputs.
///
/// The chokepoint calls this and refuses the mutation on any error. It is a thin bridge
/// into `sigil-shield` so that this crate never reimplements verification — and so that
/// there is no seam a caller could substitute a stub into.
pub fn verify_spend_proof(
    anchor: &[u8; 32],
    nullifier: &[u8; 32],
    fee: u128,
    cm_outs: &[[u8; 32]],
    proof: &[u8],
) -> Result<(), ShieldedError> {
    verify_spend_proof_multi(anchor, std::slice::from_ref(nullifier), fee, cm_outs, proof)
}

/// Verify a spend proof with ONE OR TWO inputs.
///
/// The nullifier COUNT selects the circuit — 1 is v5 (falling back to v4 during the rollout
/// window), 2 is v6. It is the sender's own declaration, so nothing is inferred from the
/// proof's shape; a proof whose trace does not match the selected circuit is refused rather
/// than re-routed.
///
/// Callers must have already established that the tags are DISTINCT. This function does
/// check it (defence in depth, since the circuit itself cannot), but the state layer checks
/// first so a duplicate never reaches proof verification at all.
pub fn verify_spend_proof_multi(
    anchor: &[u8; 32],
    nullifiers: &[[u8; 32]],
    fee: u128,
    cm_outs: &[[u8; 32]],
    proof: &[u8],
) -> Result<(), ShieldedError> {
    sigil_shield::note_v1::verify_spend_wire_multi(anchor, nullifiers, fee, cm_outs, proof)
        .map_err(|e| ShieldedError::ProofRejected(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cm(n: u8) -> [u8; 32] {
        [n; 32]
    }

    /// A canonical wire commitment for leaf `i` — first 8 bytes little-endian, rest zero,
    /// which is what `note_v1::from_wire` accepts. `cm(n)` above is deliberately NOT
    /// canonical, so rotation tests that actually build trees use this instead.
    fn wire(i: u64) -> [u8; 32] {
        let mut b = [0u8; 32];
        b[..8].copy_from_slice(&i.to_le_bytes());
        b
    }

    fn fill_to_capacity(p: &mut ShieldedPool) {
        for i in 0..POOL_CAPACITY as u64 {
            p.append_note(wire(i)).expect("fills without error");
        }
        assert_eq!(p.len(), POOL_CAPACITY);
        assert_eq!(p.epoch(), 0, "filling alone must not rotate");
    }

    /// THE regression this whole mechanism exists for. Before rotation, the 32,768th note
    /// ended shielded transfers on the chain permanently: every later private send and
    /// every shielded coinbase was refused at apply time with `PoolFull` and there was no
    /// way back. Appending past capacity must now succeed.
    #[test]
    fn pool_rotates_instead_of_dying_when_full() {
        let mut p = ShieldedPool::new();
        fill_to_capacity(&mut p);

        let pos = p
            .append_note(wire(POOL_CAPACITY as u64))
            .expect("REGRESSION: a full pool must rotate, not refuse forever");
        assert_eq!(pos, 0, "the new epoch numbers leaves from zero");
        assert_eq!(p.epoch(), 1);
        assert_eq!(p.len(), 1, "live tree holds only the new epoch's notes");
        assert_eq!(p.archive().len(), 1, "the old generation is sealed, not discarded");
        assert_eq!(p.archive()[0].notes.len(), POOL_CAPACITY);
    }

    /// Sealing must not confiscate. A note in a sealed epoch is spent by proving against
    /// that epoch's final root, so the root has to stay a valid anchor forever — and the
    /// leaves have to stay published or no inclusion path can ever be rebuilt.
    #[test]
    fn sealed_epoch_stays_spendable_and_scannable() {
        let mut p = ShieldedPool::new();
        fill_to_capacity(&mut p);
        let sealed_root = p.current_root_fast();
        p.append_note(wire(999_999)).unwrap();

        assert!(
            p.is_known_anchor(&sealed_root),
            "SECURITY: a sealed root must remain a valid anchor or every note it contains \
             becomes unspendable"
        );
        assert_eq!(p.epoch_of_anchor(&sealed_root), Some(0));
        assert_eq!(
            p.archive()[0].notes[7],
            wire(7),
            "archived leaves keep their positions — position is what the nullifier binds to"
        );
        assert_eq!(
            p.archive()[0].ciphertexts.len(),
            POOL_CAPACITY,
            "delivery ciphertexts must survive sealing or an unscanned payment is lost"
        );
    }

    /// The cryptographic heart of the change.
    ///
    /// `nf = compress2(spend_key, position)` binds to the leaf index INSIDE the proven
    /// tree, and a new epoch numbers from zero again — so one wallet can honestly derive
    /// the SAME raw nullifier for two different notes in two different generations.
    /// Unscoped, the second would be rejected as a double-spend and silently confiscated.
    #[test]
    fn same_raw_nullifier_in_two_epochs_is_two_distinct_spends() {
        let mut p = ShieldedPool::new();
        let nf = cm(42);

        p.spend_nullifier_in_epoch(0, nf).expect("first epoch, first spend");
        assert_eq!(
            p.spend_nullifier_in_epoch(0, nf),
            Err(ShieldedError::NullifierAlreadySpent(nf)),
            "SECURITY: within one epoch a repeat is still a double-spend"
        );

        p.spend_nullifier_in_epoch(1, nf).expect(
            "SECURITY: the same raw nullifier in a LATER epoch is a different note and must \
             remain spendable — rejecting it would confiscate it",
        );
        assert_eq!(
            p.spend_nullifier_in_epoch(1, nf),
            Err(ShieldedError::NullifierAlreadySpent(nf)),
            "SECURITY: and it is still single-spend within its own epoch"
        );

        assert!(p.is_spent_in_epoch(0, &nf));
        assert!(p.is_spent_in_epoch(1, &nf));
        assert!(!p.is_spent_in_epoch(2, &nf), "an untouched epoch is unaffected");
    }

    /// Epoch 0 must scope to the identity, or every nullifier recorded before rotation
    /// existed would stop being recognised and could be spent a second time.
    #[test]
    fn epoch_zero_scoping_is_the_identity() {
        assert_eq!(scoped_nullifier(0, cm(3)), cm(3));
        assert_ne!(scoped_nullifier(1, cm(3)), cm(3));
        assert_ne!(scoped_nullifier(1, cm(3)), scoped_nullifier(2, cm(3)));

        let mut p = ShieldedPool::new();
        p.spend_nullifier(cm(3)).unwrap();
        assert!(p.is_spent(&cm(3)), "the legacy accessor still sees a legacy spend");
        assert!(p.is_spent_in_epoch(0, &cm(3)));
    }

    /// Rotation moves no money and forgets no spend.
    #[test]
    fn rotation_carries_value_and_the_spent_set() {
        let mut p = ShieldedPool::new();
        p.lock_value(5_000).unwrap();
        p.spend_nullifier_in_epoch(0, cm(11)).unwrap();
        fill_to_capacity(&mut p);
        p.append_note(wire(1_000_000)).unwrap();

        assert_eq!(p.epoch(), 1);
        assert_eq!(p.value_locked(), 5_000, "rotation must move no value");
        assert!(
            p.is_spent_in_epoch(0, &cm(11)),
            "SECURITY: forgetting a spent nullifier would allow a consumed note to be spent again"
        );
    }

    /// Rotation clears `notes`, so a duplicate-commitment guard that only scanned the live
    /// epoch would quietly re-open the replay hole it was added to close.
    #[test]
    fn replay_guard_spans_sealed_epochs() {
        let mut p = ShieldedPool::new();
        fill_to_capacity(&mut p);
        p.append_note(wire(2_000_000)).unwrap();
        assert_eq!(p.epoch(), 1);

        assert_eq!(
            p.append_note(wire(5)),
            Err(ShieldedError::DuplicateCommitment(wire(5))),
            "SECURITY: a commitment sealed in epoch 0 must still be refused in epoch 1"
        );
        assert!(p.has_ever_held(&wire(5)));
        assert!(!p.has_ever_held(&wire(3_000_000)));
    }

    /// The digest folds into `wallet_state_root`, which settled headers commit to. It must
    /// stay byte-identical on a chain that has never rotated, and must move once one has —
    /// otherwise two nodes disagreeing about epoch state could publish the same header.
    #[test]
    fn digest_is_stable_before_rotation_and_moves_after() {
        let mut plain = ShieldedPool::new();
        plain.append_note(wire(1)).unwrap();
        let before = plain.digest();

        let mut rotated = ShieldedPool::new();
        fill_to_capacity(&mut rotated);
        let pre_rotation_digest = rotated.digest();
        rotated.append_note(wire(7_000_000)).unwrap();

        assert_ne!(
            rotated.digest(),
            pre_rotation_digest,
            "epoch state must affect the digest once a rotation has happened"
        );
        assert_eq!(
            before,
            {
                let mut same = ShieldedPool::new();
                same.append_note(wire(1)).unwrap();
                same.digest()
            },
            "a never-rotated pool digests exactly as it always did"
        );
    }

    #[test]
    fn nullifier_set_blocks_double_spend() {
        let mut p = ShieldedPool::new();
        assert!(!p.is_spent(&cm(1)));
        p.spend_nullifier(cm(1)).expect("first spend");
        assert!(p.is_spent(&cm(1)));
        assert_eq!(
            p.spend_nullifier(cm(1)),
            Err(ShieldedError::NullifierAlreadySpent(cm(1))),
            "SECURITY: a repeated nullifier must be rejected"
        );
        p.spend_nullifier(cm(2)).expect("distinct nullifier");
        assert_eq!(p.nullifier_count(), 2);
    }

    #[test]
    fn notes_append_at_stable_positions() {
        let mut p = ShieldedPool::new();
        assert_eq!(p.append_note(cm(1)).unwrap(), 0);
        assert_eq!(p.append_note(cm(2)).unwrap(), 1);
        assert_eq!(p.note_at(0), Some(cm(1)));
        assert_eq!(p.note_at(1), Some(cm(2)));
        assert_eq!(p.note_at(2), None);
    }

    #[test]
    fn value_locked_conserves_and_refuses_overdraw() {
        let mut p = ShieldedPool::new();
        p.lock_value(1_000).unwrap();
        p.lock_value(500).unwrap();
        assert_eq!(p.value_locked(), 1_500);
        p.unlock_value(400).unwrap();
        assert_eq!(p.value_locked(), 1_100);
        assert_eq!(
            p.unlock_value(2_000),
            Err(ShieldedError::UnshieldExceedsLocked { requested: 2_000, locked: 1_100 }),
            "SECURITY: unshielding more than is locked would mint"
        );
        assert_eq!(p.value_locked(), 1_100, "a refused unshield must not mutate");
    }

    #[test]
    fn padding_fills_to_capacity_without_zero_leaves() {
        let mut p = ShieldedPool::new();
        p.append_note(cm(7)).unwrap();
        let leaves = p.padded_leaves(|i| [(i % 251) as u8 + 1; 32]);
        assert_eq!(leaves.len(), POOL_CAPACITY);
        assert_eq!(leaves[0], cm(7));
        assert!(leaves[1..].iter().all(|l| *l != [0u8; 32]), "no zero-preimage leaves");
    }

    /// The digest must change whenever anything a node could disagree about changes,
    /// otherwise two nodes with different pools could publish the same header.
    #[test]
    fn digest_covers_every_field() {
        let base = ShieldedPool::new();
        let mut with_note = base.clone();
        with_note.append_note(cm(1)).unwrap();
        let mut with_nf = base.clone();
        with_nf.spend_nullifier(cm(2)).unwrap();
        let mut with_value = base.clone();
        with_value.lock_value(1).unwrap();

        let d = base.digest();
        assert_ne!(d, with_note.digest(), "notes must affect the digest");
        assert_ne!(d, with_nf.digest(), "nullifiers must affect the digest");
        assert_ne!(d, with_value.digest(), "locked value must affect the digest");
        assert_eq!(base.digest(), ShieldedPool::new().digest(), "deterministic");
    }

    /// Ciphertexts must stay positionally aligned with notes, and a divergent ciphertext
    /// must move the digest just like a divergent note would — otherwise two nodes could
    /// hold different delivery data for the same commitment without ever disagreeing.
    #[test]
    fn ciphertexts_stay_aligned_with_notes_and_affect_the_digest() {
        let mut p = ShieldedPool::new();
        assert_eq!(p.append_note(cm(1)).unwrap(), 0, "plain append still works");
        assert_eq!(
            p.append_note_with_delivery(cm(2), Some("ct-for-note-2".into())).unwrap(),
            1
        );
        assert_eq!(p.append_note(cm(3)).unwrap(), 2);

        assert_eq!(p.ciphertexts().len(), p.notes().len(), "must stay lockstep with notes");
        assert_eq!(p.ciphertexts()[0], None);
        assert_eq!(p.ciphertexts()[1].as_deref(), Some("ct-for-note-2"));
        assert_eq!(p.ciphertexts()[2], None);

        let mut same_notes_different_ct = ShieldedPool::new();
        same_notes_different_ct.append_note(cm(1)).unwrap();
        same_notes_different_ct
            .append_note_with_delivery(cm(2), Some("a-different-ciphertext".into()))
            .unwrap();
        same_notes_different_ct.append_note(cm(3)).unwrap();
        assert_ne!(
            p.digest(),
            same_notes_different_ct.digest(),
            "SECURITY: identical commitments with different delivery ciphertexts must not \
             produce the same digest, or two nodes could silently disagree on delivery data"
        );
    }

    /// Encryption keys are a separate registry from the STARK owner key, and both must be
    /// independently settable — a wallet can hold one without the other (e.g. a miner who
    /// registered before note delivery existed).
    #[test]
    fn encrypt_key_registry_is_independent_of_the_shield_address_registry() {
        let mut p = ShieldedPool::new();
        let wallet = [7u8; 32];
        assert_eq!(p.encrypt_key(&wallet), None);
        p.set_encrypt_key(wallet, [9u8; 32]);
        assert_eq!(p.encrypt_key(&wallet), Some([9u8; 32]));
        assert_eq!(p.shielded_address(&wallet), None, "the two registries do not leak into each other");
    }
}
