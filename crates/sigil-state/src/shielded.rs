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
pub const SHIELDED_FEE: u128 = 1_000;

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
];

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
}

/// How many historical roots stay spendable. At one root per block this is a ~256-block
/// window for a transaction to be mined before its anchor expires.
pub const ANCHOR_WINDOW: usize = 256;

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

    /// Is this a root the pool genuinely held within the anchor window?
    pub fn is_known_anchor(&self, root: &[u8; 32]) -> bool {
        self.anchors.contains(root)
    }

    /// Record a root as spendable, evicting beyond [`ANCHOR_WINDOW`].
    pub(crate) fn push_anchor(&mut self, root: [u8; 32]) {
        if self.anchors.contains(&root) {
            return;
        }
        self.anchors.push_back(root);
        while self.anchors.len() > ANCHOR_WINDOW {
            self.anchors.pop_front();
        }
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

    /// Has this nullifier been spent?
    pub fn is_spent(&self, nf: &[u8; 32]) -> bool {
        self.nullifiers.contains(nf)
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
        if self.notes.len() >= POOL_CAPACITY {
            return Err(ShieldedError::PoolFull);
        }
        // REPLAY GUARD (2026-08-25) — see `ShieldedError::DuplicateCommitment`'s docs for
        // the incident this closes. This is the ONE shared entry point every mint path
        // (`Shield`, `Unshield`, `ShieldedSpend` outputs, `ShieldedCoinbase`) funnels
        // through, so guarding here protects all of them with one check instead of
        // scattering it per call site — "fix problems at the root, not the symptom."
        // O(n) over at-most-`POOL_CAPACITY` (32,768) 32-byte entries: microseconds,
        // negligible next to the STARK verification already paid on the spend paths, and
        // this is the ONLY guard `Shield` has at all (it carries no nullifier to check).
        if self.notes.contains(&cm) {
            return Err(ShieldedError::DuplicateCommitment(cm));
        }
        let position = self.notes.len();
        // keep the incremental index in step with the source of truth
        let leaf = sigil_shield::note_v1::from_wire(&cm).unwrap_or_default();
        self.tree_mut().append(leaf);
        self.notes.push(cm);
        self.note_ciphertexts.push(ciphertext);
        Ok(position)
    }

    /// Record a nullifier as spent. Rejects a repeat — this is the double-spend guard.
    pub(crate) fn spend_nullifier(&mut self, nf: [u8; 32]) -> Result<(), ShieldedError> {
        if !self.nullifiers.insert(nf) {
            return Err(ShieldedError::NullifierAlreadySpent(nf));
        }
        Ok(())
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
    sigil_shield::note_v1::verify_spend_wire(anchor, nullifier, fee, cm_outs, proof)
        .map_err(|e| ShieldedError::ProofRejected(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cm(n: u8) -> [u8; 32] {
        [n; 32]
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
