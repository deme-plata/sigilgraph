//! v7 NOTE FORMAT — 256-bit (4-element) digests over a Goldilocks-native algebraic hash.
//!
//! # Why v7 exists (the "64-bit MiMC" audit item)
//!
//! Today's canonical note ([`crate::note_v1`]) carries every security-critical value —
//! commitment, nullifier, blinding, Merkle node — as ONE Goldilocks field element. On the
//! wire that is a `[u8; 32]` whose first 8 bytes are data and whose other 24 are dead
//! padding. One 64-bit element gives:
//!
//!   * **hiding** of an amount: break by brute-forcing the 64-bit blinding, ~2^64
//!     classical / ~2^32 under Grover — far below any post-quantum bar;
//!   * **binding** (a commitment opening is unique): a collision on the 64-bit commitment
//!     output is a ~2^32 birthday search, and a useful collision (a small honest opening
//!     and a large attacker-owned one sharing a `cm`) mints money — the deposit door's
//!     opening recompute cannot catch it, because it only checks that ONE opening equals
//!     `cm`, never that a second does not exist;
//!   * **nullifier** uniqueness: a ~2^32 birthday collision, which at a 32,768-leaf pool
//!     (`sigil_state::POOL_CAPACITY`) and an unbounded cross-epoch nullifier set becomes a
//!     non-negligible accidental-confiscation / targeted-denial risk.
//!
//! v7 lifts all four to **four** Goldilocks elements (256 bits, no padding) using
//! winterfell's own Rescue-Prime `Rp64_256` — the same hash family this crate already
//! imports ([`crate::notes`]), Goldilocks-native so it can still be an AIR transition
//! constraint, and documented by winter-crypto at `COLLISION_RESISTANCE = 128`.
//!
//! | property | v1 (1 element) | v7 (4 elements) |
//! |---|---|---|
//! | digest / blinding / nullifier width | 64 bits | 256 bits |
//! | hiding (preimage of a secret blinding) | ~2^64 cl / ~2^32 Q | ~2^128 cl / ~2^128 Q (Grover) |
//! | binding (collision → mint) | ~2^32 | ~2^128 cl / ~2^85 Q (BHT) |
//! | nullifier collision | ~2^32 | ~2^128 cl / ~2^85 Q (BHT) |
//! | wire bytes used / sent | 8 of 32 | 32 of 32 |
//!
//! # Scope of THIS module (spike)
//!
//! Off-circuit primitives ONLY: commitment, nullifier, a Merkle tree / path over 4-element
//! nodes, and a strict canonical wire codec. The in-circuit Rescue AIR that would make a
//! spend prove membership against these is the next milestone (see `DESIGN.md`); nothing
//! here is wired into consensus, the wallet or the node yet. Wiring it in is a consensus
//! change and therefore height-gated and irreversible from the first minted block — see
//! `DESIGN.md`, "Migration".
//!
//! # Hash construction
//!
//! Rescue `Rp64_256` absorbs field elements (count injected into capacity, zero padding)
//! and emits a 4-element digest; `merge` compresses two digests (an 8-element absorb) into
//! one. Both are consistent with `hash_elements` over the same elements. v7 uses:
//!
//! ```text
//!   pk    = H( DOM_PK  ‖ spend_key[0..4] )                       owner key, 4 elements
//!   inner = H( DOM_VAL ‖ value ‖ blinding[0..4] )               value commitment, 4 elements
//!   cm    = merge( inner, pk )                                   the hidden owner-bound leaf
//!   nf    = H( DOM_NF  ‖ spend_key[0..4] ‖ position )           revealed on spend
//!   node  = merge( left, right )                                 Merkle interior node
//! ```
//!
//! The two-level `merge(inner, pk)` shape mirrors v1's `compress2(compress2(v,b), pk)`:
//! owner-bound (a spender must exhibit `spend_key`, and `H` is one-way, so publishing `pk`
//! or handing a recipient `(value, blinding)` never yields the key — the precondition for
//! note ciphertexts). The domain tags make the three hashes non-interchangeable even if an
//! attacker controls their inputs.

use winterfell::crypto::{hashers::Rp64_256, Digest as _, ElementHasher, Hasher};
use winterfell::math::{fields::f64::BaseElement, FieldElement, StarkField};

/// Goldilocks modulus p = 2^64 − 2^32 + 1. A wire limb at or above this is not a canonical
/// field element and is refused by [`Digest4::from_wire`]. Same constant as `note_v1::P`.
pub const P: u128 = 0xFFFF_FFFF_0000_0001;

/// Range bound on an amount: `value < 2^RANGE_BITS`, so field conservation coincides with
/// integer conservation. Unchanged from v1 — a 256-bit note widens the HASH, not the
/// amount: `sigil_state`'s max supply `21_000_000 × 10^10 < 2^58`, so one element holds any
/// amount and a wider value field would buy nothing.
pub const RANGE_BITS: u32 = 58;

/// Rescue-Prime over Goldilocks, 256-bit digest. The same hash family `crate::notes`
/// already uses; `winter-crypto` targets 128-bit security with a 40% round-count margin.
pub type Rescue = Rp64_256;

// Domain separators ("SIGIL_v7" family), distinct 64-bit tags so the three hash call sites
// cannot be made to collide by an attacker who chooses their inputs. ASCII, big-endian.
pub const DOM_PK: u64 = 0x5347_4C37_5F5F_504B; // "SGL7__PK"
pub const DOM_VAL: u64 = 0x5347_4C37_5F56_414C; // "SGL7_VAL"
pub const DOM_NF: u64 = 0x5347_4C37_5F5F_4E46; // "SGL7__NF"

/// Errors from v7 note construction / decoding.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NoteV7Error {
    #[error("non-canonical field encoding in limb {limb}: value {got} >= p")]
    NonCanonical { limb: usize, got: u128 },
    #[error("amount {got} exceeds the range bound 2^{bits}")]
    AmountOutOfRange { got: u64, bits: u32 },
    #[error("a shielded pool needs a power-of-two leaf count >= 2, got {got}")]
    BadLeafCount { got: usize },
    #[error("leaf index {index} is outside a tree of {len} leaves")]
    IndexOutOfRange { index: usize, len: usize },
}

/// A 256-bit digest: four Goldilocks elements, the v7 commitment / nullifier / Merkle node.
///
/// Wire form is exactly 32 bytes — four little-endian 64-bit limbs, every byte carrying
/// data. There is no padding to get wrong (v1's 24 dead bytes were a non-canonicality
/// surface); the only malleability to refuse is a limb `>= p`, which [`Self::from_wire`]
/// does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Digest4(pub [BaseElement; 4]);

impl Digest4 {
    pub fn elements(&self) -> &[BaseElement; 4] {
        &self.0
    }

    /// Canonical 32-byte encoding: limb 0 in bytes 0..8, limb 3 in bytes 24..32, all LE.
    /// Byte-identical to `Rp64_256`'s own digest `as_bytes`, so a v7 node and the hash it
    /// came from serialize the same way.
    pub fn to_wire(&self) -> [u8; 32] {
        let mut out = [0u8; 32];
        for (i, e) in self.0.iter().enumerate() {
            out[i * 8..i * 8 + 8].copy_from_slice(&e.as_int().to_le_bytes());
        }
        out
    }

    /// Decode a 32-byte wire value, REJECTING any limb that is not a canonical field
    /// element (`>= p`). Strict on purpose: two spellings of one element would be two wire
    /// encodings of one nullifier, i.e. a double-spend vector. (winter-crypto's own digest
    /// `Deserializable` skips this check — its source carries a `TODO` saying so — which is
    /// exactly why v7 does not reuse it on the consensus path.)
    pub fn from_wire(b: &[u8; 32]) -> Result<Self, NoteV7Error> {
        let mut elems = [BaseElement::ZERO; 4];
        for (i, slot) in elems.iter_mut().enumerate() {
            let mut lo = [0u8; 8];
            lo.copy_from_slice(&b[i * 8..i * 8 + 8]);
            let v = u64::from_le_bytes(lo);
            if (v as u128) >= P {
                return Err(NoteV7Error::NonCanonical { limb: i, got: v as u128 });
            }
            *slot = BaseElement::new(v);
        }
        Ok(Self(elems))
    }

    fn from_digest(d: <Rescue as Hasher>::Digest) -> Self {
        let e = d.as_elements();
        Self([e[0], e[1], e[2], e[3]])
    }

    fn as_digest(&self) -> <Rescue as Hasher>::Digest {
        <Rescue as Hasher>::Digest::from(self.0)
    }
}

/// Hash a sequence of field elements to a 4-element digest (domain-separated by the caller
/// placing a tag first).
fn h(elems: &[BaseElement]) -> Digest4 {
    Digest4::from_digest(Rescue::hash_elements(elems))
}

/// 2-to-1 compression of two digests — the Merkle interior-node hash. `merge` is
/// consistent with hashing the eight constituent elements, so the off-circuit tree here
/// and an in-circuit Rescue climb agree by construction (the same guarantee `note_v1`'s
/// `compress2` gives the MiMC circuit).
pub fn merge(left: Digest4, right: Digest4) -> Digest4 {
    Digest4::from_digest(Rescue::merge(&[left.as_digest(), right.as_digest()]))
}

/// A v7 spendable note. `value` is one field element (amounts are < 2^58); `blinding` and
/// `spend_key` are 256-bit secrets so hiding and owner-binding reach the hash's full
/// strength rather than 64 bits. All fields are witness data — none is public.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoteV7 {
    pub value: BaseElement,
    pub blinding: [BaseElement; 4],
    pub spend_key: [BaseElement; 4],
}

impl NoteV7 {
    /// Build a note from a u64 amount and 256-bit blinding / spend-key limbs, enforcing the
    /// amount range bound (so an unprovable note is unrepresentable off-circuit too).
    pub fn new(value: u64, blinding: [u64; 4], spend_key: [u64; 4]) -> Result<Self, NoteV7Error> {
        if value >= (1u64 << RANGE_BITS) {
            return Err(NoteV7Error::AmountOutOfRange { got: value, bits: RANGE_BITS });
        }
        Ok(Self {
            value: BaseElement::new(value),
            blinding: blinding.map(BaseElement::new),
            spend_key: spend_key.map(BaseElement::new),
        })
    }

    /// `pk = H(DOM_PK ‖ spend_key)` — the owner public key, one-way from the key.
    pub fn owner_pk(&self) -> Digest4 {
        let mut e = Vec::with_capacity(5);
        e.push(BaseElement::new(DOM_PK));
        e.extend_from_slice(&self.spend_key);
        h(&e)
    }

    /// `inner = H(DOM_VAL ‖ value ‖ blinding)` — the hidden value commitment.
    pub fn inner_commitment(&self) -> Digest4 {
        let mut e = Vec::with_capacity(6);
        e.push(BaseElement::new(DOM_VAL));
        e.push(self.value);
        e.extend_from_slice(&self.blinding);
        h(&e)
    }

    /// `cm = merge(inner, pk)` — the hidden, owner-bound Merkle leaf.
    pub fn commitment(&self) -> Digest4 {
        merge(self.inner_commitment(), self.owner_pk())
    }

    /// `nf = H(DOM_NF ‖ spend_key ‖ position)` — revealed on spend, unlinkable to the leaf.
    ///
    /// Still keyed by `(spend_key, position)` exactly as v1, so the node's epoch scoping
    /// ([`sigil_state::shielded::scoped_nullifier`]) applies to the raw v7 nullifier
    /// unchanged — v7 only widens the raw value from 64 to 256 bits.
    pub fn nullifier(&self, position: u64) -> Digest4 {
        let mut e = Vec::with_capacity(6);
        e.push(BaseElement::new(DOM_NF));
        e.extend_from_slice(&self.spend_key);
        e.push(BaseElement::new(position));
        h(&e)
    }
}

/// An authentication path in a v7 pool tree: the leaf, its sibling digests bottom-up, and
/// the direction bit per level (`true` ⇒ this node is the RIGHT child).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MerklePathV7 {
    pub leaf: Digest4,
    pub siblings: Vec<Digest4>,
    pub bits: Vec<bool>,
}

impl MerklePathV7 {
    /// The leaf index recovered from the direction bits (LSB = level 0), the value the
    /// in-circuit nullifier binds to.
    pub fn position(&self) -> u64 {
        let mut pos = 0u64;
        for (level, &bit) in self.bits.iter().enumerate() {
            if bit {
                pos |= 1u64 << level;
            }
        }
        pos
    }

    /// Recompute the root this path climbs to, folding with [`merge`] at each level.
    pub fn compute_root(&self) -> Digest4 {
        let mut node = self.leaf;
        for (sib, &bit) in self.siblings.iter().zip(self.bits.iter()) {
            node = if bit { merge(*sib, node) } else { merge(node, *sib) };
        }
        node
    }
}

/// A full binary Merkle tree over v7 commitments — the off-circuit reference the in-circuit
/// Rescue climb must agree with. Rebuilt from the leaf set (the production node would use an
/// append-only incremental frontier, exactly as `note_v1::IncrementalTree`; this spike
/// keeps the simple full build, same root semantics).
pub struct PoolTreeV7 {
    /// `levels[0]` = leaves, `levels[depth]` = `[root]`.
    levels: Vec<Vec<Digest4>>,
}

impl PoolTreeV7 {
    /// Build over a power-of-two (>= 2) set of leaves.
    pub fn new(leaves: Vec<Digest4>) -> Result<Self, NoteV7Error> {
        if leaves.len() < 2 || !leaves.len().is_power_of_two() {
            return Err(NoteV7Error::BadLeafCount { got: leaves.len() });
        }
        let mut levels = vec![leaves];
        while levels.last().unwrap().len() > 1 {
            let cur = levels.last().unwrap();
            let next: Vec<Digest4> = cur.chunks(2).map(|p| merge(p[0], p[1])).collect();
            levels.push(next);
        }
        Ok(Self { levels })
    }

    pub fn root(&self) -> Digest4 {
        self.levels.last().unwrap()[0]
    }

    pub fn depth(&self) -> usize {
        self.levels.len() - 1
    }

    /// The authentication path for the leaf at `index`.
    pub fn path(&self, index: usize) -> Result<MerklePathV7, NoteV7Error> {
        let n = self.levels[0].len();
        if index >= n {
            return Err(NoteV7Error::IndexOutOfRange { index, len: n });
        }
        let leaf = self.levels[0][index];
        let mut siblings = Vec::with_capacity(self.depth());
        let mut bits = Vec::with_capacity(self.depth());
        let mut idx = index;
        for level in 0..self.depth() {
            let is_right = idx & 1 == 1;
            let sib = if is_right { idx - 1 } else { idx + 1 };
            siblings.push(self.levels[level][sib]);
            bits.push(is_right);
            idx >>= 1;
        }
        Ok(MerklePathV7 { leaf, siblings, bits })
    }
}

/// The padding leaf for rounding a pool up to a power of two: a domain-separated constant,
/// NOT zero — a zero leaf is a value an attacker reaches with `value = 0, blinding = 0`,
/// letting them "prove membership" of a note nobody inserted. Matching this instead needs a
/// Rescue preimage, the same assumption the commitments already rest on. (Mirrors
/// `note_v1::padding_constant`.)
pub fn padding_leaf() -> Digest4 {
    h(&[
        BaseElement::new(u64::MAX - 1),
        BaseElement::new(0x5347_4C37_5F50_4144), // "SGL7_PAD"
    ])
}

/// `pads[k]` = the root of a height-`k` subtree made entirely of padding leaves.
fn padding_subtree_roots_v7() -> &'static [Digest4] {
    use std::sync::OnceLock;
    static ROOTS: OnceLock<Vec<Digest4>> = OnceLock::new();
    ROOTS.get_or_init(|| {
        let mut v = vec![padding_leaf()];
        for k in 1..=40 {
            let prev = v[k - 1];
            v.push(merge(prev, prev));
        }
        v
    })
}

/// Append-only frontier over v7 leaves: O(depth) per append and per root, the structure a
/// node keeps for the live epoch (the v7 twin of `note_v1::IncrementalTree`). Proven to
/// agree with [`PoolTreeV7`] at EVERY occupancy by `incremental_matches_full_build_at_every_size`,
/// because a wrong incremental root is a consensus split.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IncrementalTreeV7 {
    /// `frontier[k]` = a pending left node at level `k`, waiting for its right sibling.
    frontier: Vec<Option<Digest4>>,
    count: usize,
}

impl IncrementalTreeV7 {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn len(&self) -> usize {
        self.count
    }
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Append one leaf, carrying up like binary increment.
    pub fn append(&mut self, leaf: Digest4) {
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
                    cur = merge(left, cur);
                    level += 1;
                }
            }
        }
        self.count += 1;
    }

    /// The root for a tree of `capacity` leaves, the rest padding.
    pub fn root(&self, capacity: usize) -> Digest4 {
        assert!(capacity.is_power_of_two() && capacity >= 2);
        assert!(self.count <= capacity, "pool overflow: {} > {capacity}", self.count);
        let pads = padding_subtree_roots_v7();
        let depth = capacity.trailing_zeros() as usize;
        // Exactly full: the final carry deposited the finished root one level above the climb.
        if self.count == capacity {
            if let Some(Some(root)) = self.frontier.get(depth) {
                return *root;
            }
        }
        let mut node = pads[0];
        for k in 0..depth {
            node = match self.frontier.get(k).copied().flatten() {
                Some(left) => merge(left, node),
                None => merge(node, pads[k]),
            };
        }
        node
    }
}


// ────────────────────────────────────────────────────────────────────────────────────────
// WIRE-LEVEL VERIFICATION (M3) — the chokepoint consensus will call for a v7 spend.
// ────────────────────────────────────────────────────────────────────────────────────────

use crate::note_v1::WireVerifyError;

/// Which note generation a pool anchor belongs to. Decided by the NODE from the anchor's
/// epoch (M4), never from the proof's shape — a proof that "looks like" a generation is
/// exactly the ambiguity the arity dispatch in `note_v1` exists to avoid.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(into = "u8", try_from = "u8")]
pub enum PoolGeneration {
    /// 64-bit MiMC notes: `note_v1`, proofs by `spend_full_v4` / `v5` / `v6`.
    #[default]
    V1,
    /// 256-bit Rescue notes: this module, proofs by `spend_full_v7`.
    V7,
}
impl From<PoolGeneration> for u8 {
    fn from(g: PoolGeneration) -> u8 {
        match g {
            PoolGeneration::V1 => 1,
            PoolGeneration::V7 => 7,
        }
    }
}
impl TryFrom<u8> for PoolGeneration {
    type Error = String;
    fn try_from(b: u8) -> Result<Self, String> {
        match b {
            1 => Ok(PoolGeneration::V1),
            7 => Ok(PoolGeneration::V7),
            other => Err(format!("unknown pool generation {other}")),
        }
    }
}

/// Verify a v7 spend from its wire form.
///
/// `anchor`, `nullifiers[0]`, `cm_outs[0..2]` are 32-byte canonical [`Digest4`]s;
/// `public_value` is what leaves the pool in the clear (a private send's fee, a
/// withdrawal's amount). Every refusal that does not need the proof happens BEFORE the
/// proof bytes are parsed, in this order: output arity, input arity, public-value range,
/// canonical decode of every public input. Then the proof is parsed, its declared shape is
/// checked against this circuit's (a mismatch is a refusal, never a re-route, and would
/// otherwise be a process abort inside `Air::new`), and only then does any cryptography run.
///
/// v7 has one input form. A two-input (merge) spend is not a v7 circuit yet, so a
/// two-nullifier declaration is `UnsupportedInputCount`; the duplicate-nullifier hazard of
/// `spend_full_v6` therefore cannot arise here.
pub fn verify_spend_wire_v7(
    anchor: &[u8; 32],
    nullifiers: &[[u8; 32]],
    public_value: u128,
    cm_outs: &[[u8; 32]],
    proof: &[u8],
) -> Result<(), WireVerifyError> {
    use crate::spend_full_v7::{verify_spend_full_v7, SpendFullV7PublicInputs, N_OUTS, TRACE_LEN, WIDTH};

    if cm_outs.len() != N_OUTS {
        return Err(WireVerifyError::WrongOutputCount { expected: N_OUTS, got: cm_outs.len() });
    }
    if nullifiers.len() != 1 {
        return Err(WireVerifyError::UnsupportedInputCount { got: nullifiers.len() });
    }
    if public_value >= (1u128 << RANGE_BITS) {
        return Err(WireVerifyError::PublicValueOutOfRange(public_value));
    }
    let root = Digest4::from_wire(anchor)?;
    let nf = Digest4::from_wire(&nullifiers[0])?;
    let mut outs = [[BaseElement::ZERO; 4]; N_OUTS];
    for (slot, cm) in outs.iter_mut().zip(cm_outs.iter()) {
        *slot = Digest4::from_wire(cm)?.0;
    }

    let p = winterfell::Proof::from_bytes(proof).map_err(|_| WireVerifyError::MalformedProof)?;

    // SHAPE GUARDS — both attacker-chosen numbers a `Proof` carries, checked before any
    // AIR is constructed (see `note_v1::verify_spend_wire_multi` for the history). v7 has
    // exactly one shape, so the length check is an equality, not a range.
    let got_width = p.trace_info().main_trace_width();
    if got_width != WIDTH {
        return Err(WireVerifyError::WrongTraceWidth { expected: WIDTH, got: got_width });
    }
    let got_len = p.trace_info().length();
    if got_len != TRACE_LEN {
        return Err(WireVerifyError::WrongTraceLength { got: got_len, min: TRACE_LEN, max: TRACE_LEN });
    }

    let pi = SpendFullV7PublicInputs {
        root: root.0,
        nf: nf.0,
        public_value: BaseElement::new(public_value as u64),
        cm_outs: outs,
    };
    verify_spend_full_v7(p, pi).map_err(|e| WireVerifyError::VerifierRejected(format!("{e:?}")))
}

/// The dual-accept chokepoint for the migration window: the node names the generation the
/// anchor belongs to, and the matching verifier runs. V1 keeps today's v5/v6 (and legacy
/// v4) acceptance untouched; V7 is strict. Nothing is inferred from the proof.
pub fn verify_spend_wire_dual(
    generation: PoolGeneration,
    anchor: &[u8; 32],
    nullifiers: &[[u8; 32]],
    public_value: u128,
    cm_outs: &[[u8; 32]],
    proof: &[u8],
) -> Result<(), WireVerifyError> {
    match generation {
        PoolGeneration::V1 => crate::note_v1::verify_spend_wire_multi(anchor, nullifiers, public_value, cm_outs, proof),
        PoolGeneration::V7 => verify_spend_wire_v7(anchor, nullifiers, public_value, cm_outs, proof),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(v: u64) -> NoteV7 {
        NoteV7::new(v, [1, 2, 3, 4], [10, 11, 12, 13]).unwrap()
    }

    /// DETERMINISM: the commitment and nullifier are pure functions of the opening.
    #[test]
    fn commitment_and_nullifier_are_deterministic() {
        let n = note(1_000);
        assert_eq!(n.commitment(), n.commitment());
        assert_eq!(n.nullifier(7), n.nullifier(7));
        // and the three hash call sites are domain-separated, so they do not coincide
        assert_ne!(n.commitment().to_wire(), n.nullifier(0).to_wire());
        assert_ne!(n.owner_pk(), n.inner_commitment());
    }

    /// HIDING SANITY: a different blinding gives a different commitment (the whole point of
    /// a 256-bit blinding), and a different amount does too, and neither leaks the key into
    /// the public `pk` being equal.
    #[test]
    fn different_blinding_or_value_changes_commitment() {
        let a = NoteV7::new(500, [1, 2, 3, 4], [10, 11, 12, 13]).unwrap();
        let b = NoteV7::new(500, [9, 9, 9, 9], [10, 11, 12, 13]).unwrap();
        let c = NoteV7::new(501, [1, 2, 3, 4], [10, 11, 12, 13]).unwrap();
        assert_ne!(a.commitment(), b.commitment(), "blinding must change the commitment");
        assert_ne!(a.commitment(), c.commitment(), "value must change the commitment");
        // same owner ⇒ same pk, different notes ⇒ different commitments: owner binding holds
        assert_eq!(a.owner_pk(), b.owner_pk());
    }

    /// A different spend key gives a different owner pk and a different nullifier — the
    /// owner binding and the nullifier both depend on the (now 256-bit) key.
    #[test]
    fn spend_key_binds_pk_and_nullifier() {
        let a = NoteV7::new(500, [1, 2, 3, 4], [10, 11, 12, 13]).unwrap();
        let d = NoteV7::new(500, [1, 2, 3, 4], [20, 21, 22, 23]).unwrap();
        assert_ne!(a.owner_pk(), d.owner_pk());
        assert_ne!(a.commitment(), d.commitment());
        assert_ne!(a.nullifier(0), d.nullifier(0));
        assert_ne!(a.nullifier(0), a.nullifier(1), "position must change the nullifier");
    }

    /// PATH VERIFICATION: every leaf's path climbs to the real root, and the recovered
    /// position matches the leaf index. A tampered sibling must NOT climb to the root.
    #[test]
    fn path_verifies_and_position_is_recovered() {
        let leaves: Vec<Digest4> = (0..8u64).map(|i| note(1000 + i).commitment()).collect();
        let tree = PoolTreeV7::new(leaves.clone()).unwrap();
        let root = tree.root();
        assert_eq!(tree.depth(), 3);
        for (i, _) in leaves.iter().enumerate() {
            let path = tree.path(i).unwrap();
            assert_eq!(path.compute_root(), root, "path for leaf {i} must reach the root");
            assert_eq!(path.position(), i as u64, "position bits must recover the index");
        }
        // tamper a sibling: the climb must miss the root
        let mut bad = tree.path(3).unwrap();
        bad.siblings[0] = padding_leaf();
        assert_ne!(bad.compute_root(), root, "a tampered path must not verify");
    }

    /// CANONICAL WIRE CODEC: round-trips, uses all 32 bytes, and rejects a non-canonical
    /// limb (>= p). There is no padding to reject — every byte is data — which is the whole
    /// improvement over v1's `[u8; 32]` with 24 dead bytes.
    #[test]
    fn wire_roundtrips_and_rejects_non_canonical() {
        let cm = note(42).commitment();
        let w = cm.to_wire();
        assert_eq!(Digest4::from_wire(&w).unwrap(), cm, "wire must round-trip");

        // craft a non-canonical limb 0 = p exactly, which is 0 mod p but an illegal spelling
        let mut bad = w;
        bad[0..8].copy_from_slice(&(P as u64).to_le_bytes());
        match Digest4::from_wire(&bad) {
            Err(NoteV7Error::NonCanonical { limb: 0, .. }) => {}
            other => panic!("p in limb 0 must be rejected, got {other:?}"),
        }

        // p-1 (= largest canonical) in every limb is accepted
        let mut ok = [0u8; 32];
        for i in 0..4 {
            ok[i * 8..i * 8 + 8].copy_from_slice(&((P - 1) as u64).to_le_bytes());
        }
        assert!(Digest4::from_wire(&ok).is_ok(), "p-1 is the largest canonical limb");

        // u64::MAX in a limb is >= p ⇒ rejected (catches the all-ones malleability)
        let mut maxed = w;
        maxed[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(matches!(Digest4::from_wire(&maxed), Err(NoteV7Error::NonCanonical { limb: 1, .. })));
    }

    /// The amount range bound is enforced at construction, as in v1.
    #[test]
    fn amount_range_is_enforced() {
        assert!(NoteV7::new(1u64 << RANGE_BITS, [0; 4], [0; 4]).is_err());
        assert!(NoteV7::new((1u64 << RANGE_BITS) - 1, [0; 4], [0; 4]).is_ok());
    }

    /// THE INCREMENTAL GATE: the frontier root equals the full build at every occupancy
    /// of a 16-leaf tree, including empty and exactly full.
    #[test]
    fn incremental_matches_full_build_at_every_size() {
        let cap = 16usize;
        let mut inc = IncrementalTreeV7::new();
        for n in 0..=cap {
            let mut leaves: Vec<Digest4> = (0..n as u64).map(|i| note(1000 + i).commitment()).collect();
            leaves.resize(cap, padding_leaf());
            let full = PoolTreeV7::new(leaves).unwrap().root();
            assert_eq!(inc.root(cap), full, "incremental root must match the full build at {n} leaves");
            if n < cap {
                inc.append(note(1000 + n as u64).commitment());
            }
        }
        assert_eq!(inc.len(), cap);
    }

    /// The generation tag round-trips as one byte and refuses unknown values.
    #[test]
    fn pool_generation_is_one_byte() {
        assert_eq!(u8::from(PoolGeneration::V1), 1);
        assert_eq!(u8::from(PoolGeneration::V7), 7);
        assert_eq!(PoolGeneration::try_from(7u8), Ok(PoolGeneration::V7));
        assert!(PoolGeneration::try_from(2u8).is_err());
        assert_eq!(PoolGeneration::default(), PoolGeneration::V1);
    }

    /// A v7 digest is 32 bytes = 4×8 with no wasted space, versus v1's 8-of-32.
    #[test]
    fn wire_is_four_full_limbs() {
        let cm = note(7).commitment();
        let w = cm.to_wire();
        // extremely unlikely for a Rescue digest to have a zero high limb by accident; this
        // asserts the encoding USES the high bytes (v1 always left them zero)
        let high_nonzero = w[8..32].iter().any(|&b| b != 0);
        assert!(high_nonzero, "v7 must carry data beyond the first 8 bytes");
    }
}

#[cfg(test)]
mod wire_tests {
    use super::*;
    use crate::note_v1::WireVerifyError;
    use crate::spend_full_v7::{
        build_spend_full_v7_trace_seeded, v7_options, OutputV7, SpendFullV7Prover, DEPTH, N_OUTS, TRACE_LEN, WIDTH,
    };
    use winterfell::Prover;

    struct Fixture {
        anchor: [u8; 32],
        nf: [u8; 32],
        public_value: u128,
        cms: Vec<[u8; 32]>,
        proof: Vec<u8>,
    }

    /// One honest v7 spend at the real pool depth, in wire form.
    fn v7_fixture() -> Fixture {
        let note = NoteV7::new(1_234_567, [1, 2, 3, 4], [5, 6, 7, 8]).unwrap();
        let bob = NoteV7::new(0, [0; 4], [9, 9, 9, 9]).unwrap().owner_pk();
        let outs = [OutputV7::new(1_000_000, [11, 12, 13, 14], bob), OutputV7::new(234_560, [21, 22, 23, 24], note.owner_pk())];
        let pos = 4_242usize;
        let mut leaves = vec![padding_leaf(); 1 << DEPTH];
        leaves[pos] = note.commitment();
        let tree = PoolTreeV7::new(leaves).unwrap();
        let path = tree.path(pos).unwrap();
        let t = build_spend_full_v7_trace_seeded(&note, &path, BaseElement::new(7), &outs, &v7_options(), [3u8; 32]);
        let proof = SpendFullV7Prover::new(v7_options()).prove(t).unwrap().to_bytes();
        Fixture {
            anchor: tree.root().to_wire(),
            nf: note.nullifier(pos as u64).to_wire(),
            public_value: 7,
            cms: outs.iter().map(|o| o.commitment().to_wire()).collect(),
            proof,
        }
    }

    /// A v5 (64-bit MiMC) proof, so the two generations can be handed to each other.
    fn v5_fixture() -> ([u8; 32], [u8; 32], Vec<[u8; 32]>, Vec<u8>) {
        use crate::membership::CompressTree;
        use crate::mimc::compress2;
        use crate::note_v1::{padding_leaf as pad1, to_wire};
        use crate::spend_full_v5::{build_spend_full_v5_trace, v5_options, SpendFullV5Prover, PK_DOMAIN};
        let e = BaseElement::new;
        let (value, blinding, sk) = (e(100), e(4242), e(0xDEAD));
        let pk = compress2(sk, e(PK_DOMAIN));
        let leaf = compress2(compress2(value, blinding), pk);
        let mut leaves: Vec<BaseElement> = (0..7).map(|i| pad1(i as u64)).collect();
        leaves.insert(3, leaf);
        let tree = CompressTree::new(leaves);
        let path = tree.path(3);
        let outs = [(e(50), e(777), pk), (e(47), e(888), pk)];
        let t = build_spend_full_v5_trace(value, blinding, sk, e(3), &outs, &path, &v5_options());
        let proof = SpendFullV5Prover::new(v5_options()).prove(t).unwrap().to_bytes();
        let cm = |v, b| to_wire(compress2(compress2(v, b), pk));
        (to_wire(tree.root()), to_wire(compress2(sk, e(3))), vec![cm(e(50), e(777)), cm(e(47), e(888))], proof)
    }

    /// ROUND TRIP through the wire chokepoint, and the dual dispatcher routes V7 to it.
    #[test]
    #[cfg_attr(debug_assertions, ignore)]
    fn v7_wire_round_trip_and_dual_dispatch() {
        let f = v7_fixture();
        verify_spend_wire_v7(&f.anchor, &[f.nf], f.public_value, &f.cms, &f.proof).expect("honest v7 wire spend");
        verify_spend_wire_dual(PoolGeneration::V7, &f.anchor, &[f.nf], f.public_value, &f.cms, &f.proof)
            .expect("dual dispatcher, V7 generation");
        // a flipped proof byte is a refusal, never a panic
        let mut bad = f.proof.clone();
        let mid = bad.len() / 2;
        bad[mid] ^= 0x5A;
        assert!(verify_spend_wire_v7(&f.anchor, &[f.nf], f.public_value, &f.cms, &bad).is_err());
        // a wrong public value is a refusal
        assert!(verify_spend_wire_v7(&f.anchor, &[f.nf], 8, &f.cms, &f.proof).is_err());
    }

    /// CROSS-GENERATION REPLAY: a v7 proof handed to the v1 chokepoint, and a v5 proof
    /// handed to the v7 one, are both refused by the shape guard — an error, not an abort —
    /// and the dual dispatcher follows the DECLARED generation, never the proof's shape.
    #[test]
    #[cfg_attr(debug_assertions, ignore)]
    fn proofs_never_cross_generations() {
        let f = v7_fixture();
        let (a5, nf5, cms5, p5) = v5_fixture();
        // A v7 digest uses all 32 bytes, and v1's strict decoder refuses non-zero padding —
        // so the v1 chokepoint turns a v7 spend away at the PUBLIC INPUTS, before the proof is
        // even parsed (measured: `BadPublicInput(DirtyPadding { at: 8 })`). Either refusal is
        // the property; neither is a panic.
        match crate::note_v1::verify_spend_wire_multi(&f.anchor, &[f.nf], f.public_value, &f.cms, &f.proof) {
            Err(WireVerifyError::BadPublicInput(_)) => {}
            Err(WireVerifyError::WrongTraceWidth { expected, got }) => {
                assert_eq!(got, WIDTH);
                assert_ne!(expected, WIDTH);
            }
            other => panic!("v7 proof into the v1 chokepoint: expected a refusal, got {other:?}"),
        }
        match verify_spend_wire_v7(&a5, &[nf5], 3, &cms5, &p5) {
            Err(WireVerifyError::WrongTraceWidth { expected, got }) => {
                assert_eq!(expected, WIDTH);
                assert_ne!(got, WIDTH);
            }
            other => panic!("v5 proof into the v7 chokepoint: expected a width refusal, got {other:?}"),
        }
        // dual: V1 declared with a v7 proof → refused; V7 declared with a v5 proof → refused
        assert!(verify_spend_wire_dual(PoolGeneration::V1, &f.anchor, &[f.nf], f.public_value, &f.cms, &f.proof).is_err());
        assert!(verify_spend_wire_dual(PoolGeneration::V7, &a5, &[nf5], 3, &cms5, &p5).is_err());
        // and the v5 proof still verifies under its own generation (dual-accept intact)
        verify_spend_wire_dual(PoolGeneration::V1, &a5, &[nf5], 3, &cms5, &p5).expect("v5 under V1");
    }

    /// REFUSED BEFORE PARSING (runs in debug): arity, range and canonical-decode refusals
    /// are reported as themselves even when the proof bytes are junk. If any of them
    /// drifted behind `Proof::from_bytes`, this would read `MalformedProof` instead.
    #[test]
    fn wire_refusals_happen_before_the_proof_is_parsed() {
        let d = NoteV7::new(1, [1; 4], [2; 4]).unwrap().commitment().to_wire();
        let junk = b"not a proof";
        assert!(matches!(
            verify_spend_wire_v7(&d, &[d], 1, &[d], junk),
            Err(WireVerifyError::WrongOutputCount { expected: N_OUTS, got: 1 })
        ));
        assert!(matches!(
            verify_spend_wire_v7(&d, &[d, d], 1, &[d, d], junk),
            Err(WireVerifyError::UnsupportedInputCount { got: 2 })
        ));
        assert!(matches!(
            verify_spend_wire_v7(&d, &[d], 1u128 << RANGE_BITS, &[d, d], junk),
            Err(WireVerifyError::PublicValueOutOfRange(_))
        ));
        let mut non_canonical = d;
        non_canonical[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(matches!(
            verify_spend_wire_v7(&d, &[non_canonical], 1, &[d, d], junk),
            Err(WireVerifyError::BadPublicInputV7(NoteV7Error::NonCanonical { limb: 1, .. }))
        ));
        // with every public input well-formed, junk bytes are a parse refusal
        assert!(matches!(verify_spend_wire_v7(&d, &[d], 1, &[d, d], junk), Err(WireVerifyError::MalformedProof)));
        let _ = TRACE_LEN;
    }
}
