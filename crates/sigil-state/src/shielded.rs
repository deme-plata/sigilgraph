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
/// 1/2/5 x powers of ten, so any amount can be composed from a few entries.
pub const DENOMINATIONS: &[u128] = &[
    1_000,
    2_000,
    5_000,
    10_000,
    20_000,
    50_000,
    100_000,
    200_000,
    500_000,
    1_000_000,
    2_000_000,
    5_000_000,
    10_000_000,
    20_000_000,
    50_000_000,
    100_000_000,
    200_000_000,
    500_000_000,
    1_000_000_000,
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
        let root = self.current_root();
        self.push_anchor(root);
        self.anchors_dirty = false;
    }

    /// The current anonymity-set root, as the circuit computes it.
    ///
    /// Delegates to `sigil-shield` rather than reimplementing the tree here — duplicating
    /// the circuit's hash in this crate is exactly the divergence PV-1 exists to prevent.
    pub fn current_root(&self) -> [u8; 32] {
        let leaves = self.padded_leaves(sigil_shield::note_v1::padding_leaf_wire);
        sigil_shield::note_v1::pool_root_wire(&leaves)
    }

    /// Has this nullifier been spent?
    pub fn is_spent(&self, nf: &[u8; 32]) -> bool {
        self.nullifiers.contains(nf)
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

    /// Append a note commitment, returning its leaf position.
    pub(crate) fn append_note(&mut self, cm: [u8; 32]) -> Result<usize, ShieldedError> {
        if self.notes.len() >= POOL_CAPACITY {
            return Err(ShieldedError::PoolFull);
        }
        let position = self.notes.len();
        self.notes.push(cm);
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
}
