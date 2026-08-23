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

    /// `cm = compress2(value, blinding)` — the hidden Merkle leaf.
    ///
    /// This is byte-for-byte the computation `spend_full` performs in segment 0 of its
    /// trace, which is what makes an off-circuit tree built from these leaves agree with
    /// an in-circuit membership proof.
    pub fn commitment(&self) -> BaseElement {
        compress2(self.value, self.blinding)
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

/// A deterministic filler leaf for padding a pool to a power of two.
///
/// Distinct from any real commitment with overwhelming probability, and — unlike a zero
/// leaf — not a value an attacker can produce a preimage for by choosing `value = 0`,
/// `blinding = 0`.
pub fn padding_leaf(index: u64) -> BaseElement {
    compress2(BaseElement::new(u64::MAX - 1), BaseElement::new(index))
}

// ── wire-level bridge for consensus (sigil-state calls these) ───────────────────────
//
// `sigil-state` stores commitments as `[u8; 32]` and must never reimplement the circuit's
// hash — a second implementation that drifts is a consensus split. These functions are the
// only sanctioned crossing between the wire shape and the field shape.

/// [`padding_leaf`] in the wire encoding.
pub fn padding_leaf_wire(index: u64) -> [u8; 32] {
    to_wire(padding_leaf(index))
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
    use crate::spend_full_v2::{verify_spend_full_v2, SpendFullV2PublicInputs, N_OUTS};

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
    verify_spend_full_v2(p, SpendFullV2PublicInputs { root, nf, fee: fee_e, cm_outs: outs })
        .map_err(|e| WireVerifyError::VerifierRejected(format!("{e:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spend_full::{
        build_spend_full_trace, verify_spend_full, SpendFullProver, SpendFullPublicInputs,
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
        let outs = vec![e(50), e(47)];
        let path = pool.path(position);
        assert_eq!(path.leaf, spender.commitment(), "path leaf must be our commitment");

        let nf = spender.nullifier(position as u64);
        let trace = build_spend_full_trace(
            spender.value,
            spender.blinding,
            spender.spend_key,
            fee,
            &outs,
            &path,
        );
        let proof = SpendFullProver::new(mimc_options())
            .prove(trace)
            .expect("production circuit must prove a well-formed spend");

        verify_spend_full(
            proof,
            SpendFullPublicInputs { root: pool.root(), nf, fee },
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
        assert_eq!(a.commitment(), b.commitment(), "commitment ignores spend_key");
        assert_ne!(a.nullifier(0), b.nullifier(0), "different key ⇒ different nullifier");
        assert_ne!(a.nullifier(0), a.nullifier(1), "different position ⇒ different nullifier");
        assert_eq!(a.nullifier(7), a.nullifier(7), "deterministic");
    }

    /// A pool must refuse shapes the AIR cannot prove, rather than failing later at proving.
    #[test]
    fn pool_rejects_non_power_of_two() {
        let leaves: Vec<BaseElement> = (0..3).map(|i| Note::new(i, i, i).unwrap().commitment()).collect();
        assert!(matches!(ShieldedPoolTree::new(leaves), Err(NoteError::BadLeafCount { got: 3 })));
        assert!(matches!(ShieldedPoolTree::new(vec![]), Err(NoteError::BadLeafCount { got: 0 })));
    }
}
