//! CANONICAL NOTE SHAPE (PV-1 convergence, 2026-08-23).
//!
//! Before this module there were **three** incompatible note shapes in the tree:
//!
//!   1. `sigil-mixer::commitment` — `BLAKE3(value ‖ token ‖ blinding)`, explicitly
//!      non-homomorphic and never verified (its `verify_shielded_send` is hard-wired
//!      to never return `Ok`).
//!   2. `sigil-shield::notes` — `Rescue(value ‖ pk ‖ r)` off-circuit, whose docs claim
//!      the in-circuit AIR is "verified against this module's reference". That claim
//!      does not hold: no AIR in this crate computes Rescue.
//!   3. `sigil-shield::{membership, spend_full}` — MiMC `compress2`, which is what the
//!      **production** fully-folded spend circuit actually proves.
//!
//! Shape 3 is the only one a verifier ever checks, so it is the canonical one and this
//! module is its single definition. Every off-circuit consumer (state, mempool, wallet)
//! must derive commitments and nullifiers from here so that "what consensus stores" and
//! "what the circuit proves" cannot drift apart. The equality is not asserted in a
//! comment — [`tests::off_circuit_matches_in_circuit_spend_full`] proves a note built
//! here verifies against `spend_full`, which is the only binding worth having.
//!
//! ```text
//! commitment  cm = compress2(value, blinding)          — the hidden Merkle leaf
//! nullifier   nf = compress2(spend_key, position)      — revealed on spend
//! tree                CompressTree (compress2 climb)   — root is the anonymity set
//! ```
//!
//! # Why the leaf is never revealed
//!
//! `compress2(value, blinding)` is itself one compression level, so the commitment
//! computation IS "level −1" of the Merkle climb. `spend_full` folds both into one
//! trace, which is what lets a spend prove membership without exposing WHICH note it
//! consumes. That is the unlinkability property, and it is why this shape — not
//! Rescue, not BLAKE3 — is the one consensus must store.
//!
//! # Wire encoding
//!
//! Field elements are Goldilocks (p = 2^64 − 2^32 + 1), so a note element is 8 bytes.
//! The wire type stays `[u8; 32]` — the shape `sigil-mixer` promised its integrators —
//! via [`to_wire`] / [`from_wire`], which encode canonically (little-endian in the low
//! 8 bytes, zero padding above) and **reject** non-canonical encodings on the way back
//! rather than silently truncating. A malleable commitment encoding would be a
//! double-spend vector, so `from_wire` is strict.

use winterfell::math::fields::f64::BaseElement;
use winterfell::math::{FieldElement, StarkField};

use crate::membership::{CompressTree, MerklePath};
use crate::mimc::compress2;

/// Goldilocks modulus, p = 2^64 − 2^32 + 1. A wire value at or above this is not a
/// canonical field element and is refused by [`from_wire`].
pub const P: u128 = 0xFFFF_FFFF_0000_0001;

/// Errors from note construction / decoding.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NoteError {
    #[error("non-canonical field encoding: value {got} >= p")]
    NonCanonical { got: u128 },
    #[error("wire bytes 8..32 must be zero padding, found a nonzero byte at index {at}")]
    DirtyPadding { at: usize },
    #[error("amount {got} exceeds the range bound 2^{bits}")]
    AmountOutOfRange { got: u64, bits: u32 },
    #[error("a shielded pool needs a power-of-two leaf count >= 2, got {got}")]
    BadLeafCount { got: usize },
}

/// Range bound on any amount (value, fee, output). Amounts must satisfy `v < 2^RANGE_BITS`
/// so that field-arithmetic conservation coincides with INTEGER conservation — without it
/// a prover could pick outputs summing to the input only by wrapping the modulus, which is
/// forged money. Mirrors `range::RANGE_BITS`; [`Note::new`] enforces it at construction so
/// an out-of-range note cannot be built in the first place.
pub const RANGE_BITS: u32 = 58;

/// A spendable note. `value` and `blinding` are the commitment preimage; `spend_key` is
/// the secret that derives the nullifier. All three are witness data — none is public.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Note {
    pub value: BaseElement,
    pub blinding: BaseElement,
    pub spend_key: BaseElement,
}

impl Note {
    /// Build a note, enforcing the range bound on `value`.
    ///
    /// The bound is checked here rather than only in-circuit so that an out-of-range
    /// note is unrepresentable off-circuit too — consensus should never hold a note it
    /// could not later prove.
    pub fn new(value: u64, blinding: u64, spend_key: u64) -> Result<Self, NoteError> {
        if value >= (1u64 << RANGE_BITS) {
            return Err(NoteError::AmountOutOfRange { got: value, bits: RANGE_BITS });
        }
        Ok(Self {
            value: BaseElement::new(value),
            blinding: BaseElement::new(blinding),
            spend_key: BaseElement::new(spend_key),
        })
    }

    /// The owner's public key: `pk = compress2(spend_key, PK_DOMAIN)`.
    ///
    /// One-way, so publishing `pk` never exposes the spend key.
    pub fn owner_pk(&self) -> BaseElement {
        compress2(self.spend_key, BaseElement::new(crate::spend_full_v4::PK_DOMAIN))
    }

    /// The value commitment `compress2(value, blinding)` — the inner half of the leaf.
    pub fn inner_commitment(&self) -> BaseElement {
        compress2(self.value, self.blinding)
    }

    /// `cm = compress2(compress2(value, blinding), pk)` — the hidden, OWNER-BOUND leaf.
    ///
    /// The owner binding is not decoration. Before it, a commitment was just
    /// `compress2(value, blinding)`, so anyone who learned that pair could spend the note
    /// — each with a different nullifier, which the spent-set cannot catch. Demonstrated
    /// against the old circuit before this was changed. Binding `pk` in means a spender
    /// must also exhibit the matching secret, which is what makes it safe to hand a
    /// recipient `(value, blinding)` at all.
    ///
    /// Byte-for-byte what `spend_full_v4` computes, which is what makes an off-circuit
    /// tree agree with an in-circuit membership proof.
    pub fn commitment(&self) -> BaseElement {
        compress2(self.inner_commitment(), self.owner_pk())
    }

    /// `nf = compress2(spend_key, position)` — revealed on spend, unlinkable to the note.
    ///
    /// Binding the nullifier to the leaf *position* (not to the commitment) is what lets
    /// `spend_full` keep the commitment hidden while still proving the nullifier belongs
    /// to the leaf actually membered.
    pub fn nullifier(&self, position: u64) -> BaseElement {
        compress2(self.spend_key, BaseElement::new(position))
    }
}

/// Encode a field element into the stable 32-byte wire shape.
pub fn to_wire(e: BaseElement) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[..8].copy_from_slice(&e.as_int().to_le_bytes());
    out
}

/// Decode a 32-byte wire value, rejecting non-canonical encodings.
///
/// Strict on purpose: accepting either dirty padding or an integer >= p would give two
/// distinct wire encodings of the same field element, and a nullifier with two encodings
/// is a double-spend vector (spend once under each spelling).
pub fn from_wire(b: &[u8; 32]) -> Result<BaseElement, NoteError> {
    for (i, &byte) in b.iter().enumerate().skip(8) {
        if byte != 0 {
            return Err(NoteError::DirtyPadding { at: i });
        }
    }
    let mut lo = [0u8; 8];
    lo.copy_from_slice(&b[..8]);
    let v = u64::from_le_bytes(lo);
    if (v as u128) >= P {
        return Err(NoteError::NonCanonical { got: v as u128 });
    }
    Ok(BaseElement::new(v))
}

/// The shielded pool's note-commitment tree: the canonical anonymity set.
///
/// Thin wrapper over [`CompressTree`] so that consensus and the circuit cannot be pointed
/// at different tree constructions by accident.
pub struct ShieldedPoolTree {
    tree: CompressTree,
    leaves: Vec<BaseElement>,
}

impl ShieldedPoolTree {
    /// Build over note commitments. Requires a power-of-two count >= 2 (the AIR's trace
    /// length is derived from the depth, so this is a hard structural requirement, not a
    /// convenience). Pad with [`padding_leaf`] to reach the next power of two.
    pub fn new(leaves: Vec<BaseElement>) -> Result<Self, NoteError> {
        if leaves.len() < 2 || !leaves.len().is_power_of_two() {
            return Err(NoteError::BadLeafCount { got: leaves.len() });
        }
        Ok(Self { tree: CompressTree::new(leaves.clone()), leaves })
    }

    /// The anonymity-set root. This is the value a block header commits to.
    pub fn root(&self) -> BaseElement {
        self.tree.root()
    }

    pub fn depth(&self) -> usize {
        self.tree.depth()
    }

    /// The hidden-path witness a spender feeds to `spend_full`.
    pub fn path(&self, index: usize) -> MerklePath {
        self.tree.path(index)
    }

    pub fn leaf(&self, index: usize) -> Option<BaseElement> {
        self.leaves.get(index).copied()
    }

    pub fn len(&self) -> usize {
        self.leaves.len()
    }
    pub fn is_empty(&self) -> bool {
        self.leaves.is_empty()
    }
}

/// The filler leaf for padding a pool to a power of two.
///
/// # Why a single CONSTANT rather than an index-dependent value
///
/// It was `compress2(MAX-1, index)` — different per slot. That is safe but it made every
/// padding subtree distinct, which forces a full `2^DEPTH` tree rebuild for every root:
/// chronos measured 107 ms per block, dominated by padding the producer recomputes forever.
///
/// One constant makes every all-padding subtree at a given level IDENTICAL, so their roots
/// can be precomputed once per level ([`padding_subtree_roots`]) and the tree built over
/// only the real prefix — O(notes + depth) instead of O(capacity).
///
/// Still not zero, and that distinction matters: a zero leaf is a value an attacker could
/// hit by choosing `value = 0, blinding = 0`, letting them "prove membership" of a note
/// nobody inserted. Matching THIS constant instead requires a preimage of `compress2`,
/// which is the same assumption the commitments already rest on.
pub fn padding_leaf(_index: u64) -> BaseElement {
    padding_constant()
}

/// The single padding value. Domain-separated so it cannot collide with a note commitment
/// derived from any plausible `(value, blinding)` pair.
pub fn padding_constant() -> BaseElement {
    compress2(BaseElement::new(u64::MAX - 1), BaseElement::new(0x5349_4749_4C5F_5041))
}

/// Root of a fully-padded subtree at each level: `[0] = the padding leaf`, `[k] = root of a
/// 2^k-leaf all-padding subtree`. Computed once; the whole point of the constant.
fn padding_subtree_roots() -> &'static [BaseElement] {
    use std::sync::OnceLock;
    static ROOTS: OnceLock<Vec<BaseElement>> = OnceLock::new();
    ROOTS.get_or_init(|| {
        let mut v = vec![padding_constant()];
        for k in 1..=40 {
            let prev = v[k - 1];
            v.push(compress2(prev, prev));
        }
        v
    })
}

/// The anonymity-set root over `leaves` (the real notes only), padded to `capacity`.
///
/// Builds only over the real prefix and splices in precomputed padding subtree roots, so a
/// pool holding `n` notes costs O(n + depth) rather than O(capacity). Returns the identical
/// root a full build would produce — [`tests::sparse_root_matches_full_build`] is what
/// makes that a fact rather than an intention, because a root that differs from what the
/// CIRCUIT computes is a total, silent outage.
pub fn sparse_pool_root(leaves: &[BaseElement], capacity: usize) -> BaseElement {
    assert!(capacity.is_power_of_two() && capacity >= 2);
    assert!(leaves.len() <= capacity);
    let pads = padding_subtree_roots();
    // level 0 = the real prefix; everything past it is padding.
    let mut level: Vec<BaseElement> = leaves.to_vec();
    let mut width = capacity;
    let mut k = 0usize; // current level index
    while width > 1 {
        let mut next = Vec::with_capacity(level.len().div_ceil(2));
        let mut i = 0;
        while i < level.len() {
            let l = level[i];
            // the right sibling is either real, or the all-padding subtree root at level k
            let r = if i + 1 < level.len() { level[i + 1] } else { pads[k] };
            next.push(compress2(l, r));
            i += 2;
        }
        level = next;
        width /= 2;
        k += 1;
        if level.is_empty() {
            // no real notes at all — the whole tree is padding
            return pads[k + (width.trailing_zeros() as usize)];
        }
    }
    level[0]
}

// ── wire-level bridge for consensus (sigil-state calls these) ───────────────────────
//
// `sigil-state` stores commitments as `[u8; 32]` and must never reimplement the circuit's
// hash — a second implementation that drifts is a consensus split. These functions are the
// only sanctioned crossing between the wire shape and the field shape.

/// [`padding_leaf`] in the wire encoding.
///
/// MEMOISED. Padding leaves are a pure function of the index and never change, but a naive
/// pool-root rebuild recomputes all `2^DEPTH - notes` of them every time — at depth 15 with
/// a nearly-empty pool that is ~32,767 `compress2` calls (63 MiMC rounds each) of pure
/// waste on the producer's critical path. Chronos measured the cost at 107 ms per block and
/// falling as the pool filled, which is the signature of padding dominating: fewer pads,
/// less work. The cache turns that into a one-time table.
pub fn padding_leaf_wire(index: u64) -> [u8; 32] {
    use std::sync::OnceLock;
    /// Enough for depth 15. A larger pool falls through to computing on demand rather
    /// than silently returning a wrong leaf.
    const CACHED: usize = 1 << 15;
    static TABLE: OnceLock<Vec<[u8; 32]>> = OnceLock::new();
    let t = TABLE.get_or_init(|| (0..CACHED as u64).map(|i| to_wire(padding_leaf(i))).collect());
    match t.get(index as usize) {
        Some(v) => *v,
        None => to_wire(padding_leaf(index)),
    }
}

/// The anonymity-set root over wire-encoded leaves, as the circuit computes it.
///
/// Non-canonical leaves are mapped through [`from_wire`] and a malformed one is replaced
/// by a deterministic filler rather than silently truncated — consensus must be total
/// here, and a leaf that cannot decode is a leaf no valid spend can reference anyway.
pub fn pool_root_wire(leaves: &[[u8; 32]]) -> [u8; 32] {
    let elems: Vec<BaseElement> = leaves
        .iter()
        .enumerate()
        .map(|(i, l)| from_wire(l).unwrap_or_else(|_| padding_leaf(i as u64)))
        .collect();
    match ShieldedPoolTree::new(elems) {
        Ok(t) => to_wire(t.root()),
        // A non-power-of-two leaf set cannot produce a root the circuit would accept;
        // return a value no spend can match rather than panicking inside consensus.
        Err(_) => [0xFFu8; 32],
    }
}

/// APPEND-ONLY INCREMENTAL TREE — O(depth) per append, O(depth) to read the root.
///
/// `sparse_pool_root` is O(real notes): fine at a few thousand, fatal beyond. Chronos
/// measured 33 ms at 16,384 notes, and coinbase shielding would push the pool past a
/// million within days — ~2 SECONDS per block, on the producer's critical path.
///
/// A note tree is append-only, so the whole left side is immutable once written. This
/// keeps only the FRONTIER — one node per level, the left sibling still waiting for a
/// partner — which is all an append can possibly affect. Everything left of it is already
/// hashed into those nodes; everything right of it is padding whose subtree roots are
/// precomputed. Cost stops depending on how full the pool is.
///
/// I deferred this twice, on the grounds that a wrong incremental root is a consensus
/// split. That risk is real and the answer to it is
/// [`tests::incremental_matches_sparse_at_every_size`], which checks agreement at EVERY
/// count up to a full tree rather than at a few convenient sizes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IncrementalTree {
    /// `frontier[k]` = a pending left node at level `k`, waiting for its right sibling.
    frontier: Vec<Option<BaseElement>>,
    count: usize,
}

impl IncrementalTree {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.count
    }
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Append one leaf. Carries up exactly like binary increment: a level with a pending
    /// left node combines and carries; an empty level parks the value and stops.
    pub fn append(&mut self, leaf: BaseElement) {
        let mut cur = leaf;
        let mut level = 0usize;
        loop {
            if self.frontier.len() <= level {
                self.frontier.push(None);
            }
            match self.frontier[level].take() {
                None => {
                    self.frontier[level] = Some(cur);
                    break;
                }
                Some(left) => {
                    cur = compress2(left, cur);
                    level += 1;
                }
            }
        }
        self.count += 1;
    }

    /// The root for a tree of `capacity` leaves, the rest padding.
    ///
    /// Climbs once per level. At each level the accumulated subtree is either the RIGHT
    /// sibling of a pending frontier node, or — when no node is pending — the LEFT sibling
    /// of a padding subtree, because append-only means real content is always leftmost.
    /// Starting from the padding leaf makes the empty case fall out for free: with nothing
    /// appended it folds padding into padding all the way up.
    pub fn root(&self, capacity: usize) -> BaseElement {
        assert!(capacity.is_power_of_two() && capacity >= 2);
        assert!(self.count <= capacity, "pool overflow: {} > {capacity}", self.count);
        let pads = padding_subtree_roots();
        let depth = capacity.trailing_zeros() as usize;
        // EXACTLY FULL is its own case: the final carry deposits the finished root at
        // `frontier[depth]`, one level above everything the climb below inspects. Missing
        // it produced a root that was correct at all 64 other occupancies and wrong at the
        // 65th — which is why the gate checks every count rather than a sample.
        if self.count == capacity {
            if let Some(Some(root)) = self.frontier.get(depth) {
                return *root;
            }
        }
        let mut node = pads[0];
        for k in 0..depth {
            node = match self.frontier.get(k).copied().flatten() {
                Some(left) => compress2(left, node),
                None => compress2(node, pads[k]),
            };
        }
        node
    }
}

/// The blinding for a block reward paid into the shielded pool.
///
/// PUBLICLY DERIVABLE, and that is correct here rather than a weakness. A blinding hides
/// the amount by preventing brute-force over a small value space — but a coinbase amount
/// is already public and already bound to its block, so there is nothing left to hide at
/// creation. Making it derivable buys the property that matters: the miner can recompute
/// its own note from `(height, pk)` alone and spend it, with NO ciphertext published and
/// no registration beyond the one-time key.
///
/// Anonymity for a coinbase note therefore arrives at SPEND time, not mint time. Everyone
/// can see which leaf is whose reward; nobody can see which leaf a later spend consumed,
/// because the nullifier does not name it. That is the same trade Zcash makes for shielded
/// coinbase, and it is the reason this fills a pool without any coordination.
pub fn coinbase_blinding(height: u64, pk_shield: BaseElement) -> BaseElement {
    compress2(compress2(BaseElement::new(height), pk_shield), BaseElement::new(0x5349_4749_4C5F_4342))
}

/// The commitment a shielded block reward mints, and what a miner recomputes to find it.
pub fn coinbase_commitment(height: u64, pk_shield: BaseElement, amount: u64) -> BaseElement {
    let blinding = coinbase_blinding(height, pk_shield);
    compress2(compress2(BaseElement::new(amount), blinding), pk_shield)
}

/// Wire-level form for consensus.
pub fn coinbase_commitment_wire(height: u64, pk_shield: &[u8; 32], amount: u128) -> Option<[u8; 32]> {
    let pk = from_wire(pk_shield).ok()?;
    if amount >= (1u128 << RANGE_BITS) {
        return None;
    }
    Some(to_wire(coinbase_commitment(height, pk, amount as u64)))
}

/// [`sparse_pool_root`] over wire-encoded real notes. The consensus entry point.
pub fn sparse_pool_root_wire(notes: &[[u8; 32]], capacity: usize) -> [u8; 32] {
    let elems: Vec<BaseElement> = notes
        .iter()
        .map(|l| from_wire(l).unwrap_or_else(|_| padding_constant()))
        .collect();
    to_wire(sparse_pool_root(&elems, capacity))
}

/// Errors from wire-level spend verification.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WireVerifyError {
    #[error("malformed public input: {0}")]
    BadPublicInput(#[from] NoteError),
    #[error("fee {0} exceeds the range bound")]
    FeeOutOfRange(u128),
    #[error("expected {expected} output commitments, got {got}")]
    WrongOutputCount { expected: usize, got: usize },
    #[error("proof bytes malformed")]
    MalformedProof,
    #[error("STARK verification failed: {0}")]
    VerifierRejected(String),
}

/// Verify a shielded-spend proof from its wire-encoded public inputs.
///
/// This is the function the settlement chokepoint calls. Every input is decoded strictly
/// (see [`from_wire`]) before it reaches the verifier, because a malleable public input is
/// as good as a forged proof: two encodings of one nullifier would allow a double-spend.
pub fn verify_spend_wire(
    anchor: &[u8; 32],
    nullifier: &[u8; 32],
    fee: u128,
    cm_outs: &[[u8; 32]],
    proof: &[u8],
) -> Result<(), WireVerifyError> {
    use crate::spend_full_v4::{verify_spend_full_v4, SpendFullV4PublicInputs, N_OUTS};

    if cm_outs.len() != N_OUTS {
        return Err(WireVerifyError::WrongOutputCount { expected: N_OUTS, got: cm_outs.len() });
    }
    if fee >= (1u128 << RANGE_BITS) {
        return Err(WireVerifyError::FeeOutOfRange(fee));
    }

    let root = from_wire(anchor)?;
    let nf = from_wire(nullifier)?;
    let fee_e = BaseElement::new(fee as u64);
    let mut outs = [BaseElement::ZERO; N_OUTS];
    for (slot, cm) in outs.iter_mut().zip(cm_outs.iter()) {
        *slot = from_wire(cm)?;
    }

    let p = winterfell::Proof::from_bytes(proof).map_err(|_| WireVerifyError::MalformedProof)?;
    verify_spend_full_v4(p, SpendFullV4PublicInputs { root, nf, fee: fee_e, cm_outs: outs })
        .map_err(|e| WireVerifyError::VerifierRejected(format!("{e:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spend_full_v4::{
        build_spend_full_v4_trace, verify_spend_full_v4, SpendFullV4Prover,
        SpendFullV4PublicInputs,
    };
    use crate::mimc::mimc_options;
    use winterfell::Prover;

    fn e(v: u64) -> BaseElement {
        BaseElement::new(v)
    }

    /// THE CONVERGENCE GATE — the whole point of this module.
    ///
    /// Builds a pool from [`Note`]s off-circuit, then proves a spend of one of them with
    /// the PRODUCTION `spend_full` circuit against this module's root and nullifier. If
    /// the off-circuit shape ever drifts from the in-circuit shape, this fails. A comment
    /// asserting the two agree would not have caught the Rescue-vs-MiMC divergence that
    /// motivated this module; this test does.
    #[test]
    fn off_circuit_matches_in_circuit_spend_full() {
        // depth 3 ⇒ 8 leaves, and depth+1 = 4 is a power of two as spend_full requires.
        let spender = Note::new(100, 4242, 0xDEAD).expect("in range");
        let mut leaves: Vec<BaseElement> = (0..7)
            .map(|i| Note::new(10 + i, 900 + i, 5000 + i).unwrap().commitment())
            .collect();
        let position = 3usize;
        leaves.insert(position, spender.commitment());
        assert_eq!(leaves.len(), 8);

        let pool = ShieldedPoolTree::new(leaves).expect("pool");
        assert_eq!(pool.depth(), 3, "8 leaves ⇒ depth 3");

        // Spend 100 as fee 3 + outputs 50 + 47.
        let fee = e(3);
        let me = spender.owner_pk();
        let outs = [(e(50), e(777), me), (e(47), e(888), me)];
        let path = pool.path(position);
        assert_eq!(path.leaf, spender.commitment(), "path leaf must be our commitment");

        let nf = spender.nullifier(position as u64);
        let trace = build_spend_full_v4_trace(
            spender.value,
            spender.blinding,
            spender.spend_key,
            fee,
            &outs,
            &path,
        );
        let proof = SpendFullV4Prover::new(mimc_options())
            .prove(trace)
            .expect("production circuit must prove a well-formed spend");

        let cm_outs = [
            compress2(compress2(e(50), e(777)), me),
            compress2(compress2(e(47), e(888)), me),
        ];
        verify_spend_full_v4(
            proof,
            SpendFullV4PublicInputs { root: pool.root(), nf, fee, cm_outs },
        )
        .expect("SECURITY: off-circuit note shape must verify against the in-circuit AIR");
    }

    /// Wire encoding round-trips, and every malleable spelling is refused. Two encodings
    /// of one nullifier would let the same note be spent twice.
    #[test]
    fn wire_encoding_is_canonical_and_rejects_malleability() {
        let v = e(0x0123_4567_89AB_CDEF);
        assert_eq!(from_wire(&to_wire(v)).unwrap(), v, "round-trip");

        let mut dirty = to_wire(v);
        dirty[8] = 1; // padding must be zero
        assert!(matches!(from_wire(&dirty), Err(NoteError::DirtyPadding { at: 8 })));

        let mut non_canonical = [0u8; 32];
        non_canonical[..8].copy_from_slice(&u64::MAX.to_le_bytes()); // >= p
        assert!(matches!(from_wire(&non_canonical), Err(NoteError::NonCanonical { .. })));

        // p itself must be refused (it encodes 0, giving a second spelling of zero).
        let mut p_bytes = [0u8; 32];
        p_bytes[..8].copy_from_slice(&(P as u64).to_le_bytes());
        assert!(matches!(from_wire(&p_bytes), Err(NoteError::NonCanonical { .. })));
    }

    /// Out-of-range amounts are unrepresentable, and nullifiers separate by position and key.
    #[test]
    fn range_bound_and_nullifier_separation() {
        assert!(matches!(
            Note::new(1u64 << RANGE_BITS, 1, 1),
            Err(NoteError::AmountOutOfRange { .. })
        ));
        let n = Note::new((1u64 << RANGE_BITS) - 1, 1, 1).expect("just under the bound");
        assert_eq!(n.value, e((1u64 << RANGE_BITS) - 1));

        let a = Note::new(5, 6, 0xAAAA).unwrap();
        let b = Note::new(5, 6, 0xBBBB).unwrap();
        assert_eq!(a.inner_commitment(), b.inner_commitment(), "value commitment ignores the key");
        assert_ne!(
            a.commitment(), b.commitment(),
            "SECURITY: the LEAF must bind the owner — identical (value, blinding) under \
             different keys must be different notes, or either holder could spend both"
        );
        assert_ne!(a.nullifier(0), b.nullifier(0), "different key ⇒ different nullifier");
        assert_ne!(a.nullifier(0), a.nullifier(1), "different position ⇒ different nullifier");
        assert_eq!(a.nullifier(7), a.nullifier(7), "deterministic");
    }

    /// THE INCREMENTAL GATE. The incremental root must equal the sparse root at EVERY
    /// count, not at a few convenient sizes — an off-by-one at one occupancy is a
    /// consensus split that appears exactly once and is unreproducible afterwards.
    #[test]
    fn incremental_matches_sparse_at_every_size() {
        const CAP: usize = 64;
        let mut inc = IncrementalTree::new();
        let mut leaves: Vec<BaseElement> = Vec::new();

        // empty first
        assert_eq!(inc.root(CAP), sparse_pool_root(&leaves, CAP), "empty tree");

        for n in 1..=CAP {
            let leaf = Note::new(1_000 + n as u64, 7 * n as u64 + 1, 3).unwrap().commitment();
            inc.append(leaf);
            leaves.push(leaf);
            assert_eq!(
                inc.root(CAP),
                sparse_pool_root(&leaves, CAP),
                "SECURITY: incremental root diverged from sparse at n={n} — the chain and \
                 the prover would disagree and no honest spend could verify"
            );
            assert_eq!(inc.len(), n);
        }
    }

    /// It must also agree at a realistic depth, where the frontier is deep.
    #[test]
    fn incremental_matches_sparse_at_pool_depth() {
        const CAP: usize = 1 << 15;
        let mut inc = IncrementalTree::new();
        let mut leaves: Vec<BaseElement> = Vec::new();
        for n in 0..300usize {
            let leaf = Note::new(1_000 + n as u64, 31 * n as u64 + 5, 9).unwrap().commitment();
            inc.append(leaf);
            leaves.push(leaf);
        }
        assert_eq!(inc.root(CAP), sparse_pool_root(&leaves, CAP), "at depth 15 with 300 notes");
    }

    /// THE EQUIVALENCE GATE for the sparse root.
    ///
    /// `sparse_pool_root` must return EXACTLY what a full build returns. If it ever
    /// differs, every honest proof still verifies against the prover's tree and fails
    /// against the chain's — a total outage with no error message pointing anywhere useful.
    /// Checked across pool occupancies including the awkward ones: empty, one, odd counts,
    /// and exactly full.
    #[test]
    fn sparse_root_matches_full_build() {
        const CAP: usize = 256;
        for n in [0usize, 1, 2, 3, 7, 8, 9, 100, 255, 256] {
            let leaves: Vec<BaseElement> = (0..n)
                .map(|i| Note::new(1_000 + i as u64, 7 * i as u64 + 1, 3).unwrap().commitment())
                .collect();

            // full build: real notes then explicit padding, exactly as before
            let mut full = leaves.clone();
            for i in full.len()..CAP {
                full.push(padding_leaf(i as u64));
            }
            let full_root = ShieldedPoolTree::new(full).expect("full").root();

            let sparse = sparse_pool_root(&leaves, CAP);
            assert_eq!(
                sparse, full_root,
                "SECURITY: sparse root diverged from the full build at n={n} — the chain and \
                 the prover would compute different anchors and NO honest spend could verify"
            );
        }
    }

    /// The padding constant must not be zero, and must not be reachable from a plausible
    /// note. A padding value an attacker can produce is a membership forgery.
    #[test]
    fn padding_constant_is_not_forgeable_by_a_trivial_note() {
        let pad = padding_constant();
        assert_ne!(pad, BaseElement::ZERO, "a zero pad is trivially forgeable");
        for (v, b) in [(0u64, 0u64), (0, 1), (1, 0), (1, 1), (1_000, 0)] {
            let n = Note::new(v, b, 0).unwrap();
            assert_ne!(n.commitment(), pad, "note ({v},{b}) collided with the padding value");
            assert_ne!(n.inner_commitment(), pad);
        }
    }

    /// A pool must refuse shapes the AIR cannot prove, rather than failing later at proving.
    #[test]
    fn pool_rejects_non_power_of_two() {
        let leaves: Vec<BaseElement> = (0..3).map(|i| Note::new(i, i, i).unwrap().commitment()).collect();
        assert!(matches!(ShieldedPoolTree::new(leaves), Err(NoteError::BadLeafCount { got: 3 })));
        assert!(matches!(ShieldedPoolTree::new(vec![]), Err(NoteError::BadLeafCount { got: 0 })));
    }
}
