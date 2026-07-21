//! Shielded notes, Merkle membership, and nullifiers — on winterfell's REAL, audited
//! Rescue-Prime hash (Rp64_256, Goldilocks f64, x⁷ S-box). This is the consensus-state
//! layer: the note-commitment tree (its root is the anonymity set) and the nullifier set
//! (double-spend guard). It uses winterfell's own `MerkleTree` + `Rescue` — no homemade
//! crypto — so membership + nullifier derivation are correct by construction.
//!
//! Two things live here:
//!   * OFF-circuit (this module): the real tree + nullifier + set — used by consensus to
//!     maintain the anonymity set and reject double-spends.
//!   * IN-circuit (`stark.rs`): the AIR that proves "I know a note in the tree at this root
//!     and its nullifier is nf" WITHOUT revealing which note — verified against this module's
//!     reference so the circuit can't silently diverge from real Rescue.
//!
//! Note commitment  cm = Rescue(value ‖ pk ‖ r)
//! Nullifier        nf = Rescue(spend_key ‖ position)

use std::collections::HashSet;

use winterfell::crypto::{hashers::Rp64_256, Digest, ElementHasher, Hasher, MerkleTree};
use winterfell::math::fields::f64::BaseElement;

/// The hash used throughout the shielded pool — winterfell's Rescue-Prime.
pub type Rescue = Rp64_256;
/// A note commitment / tree node / nullifier is a Rescue digest.
pub type Hash = <Rescue as Hasher>::Digest;

/// A spendable note (the witness a spender holds). Amounts are field elements.
#[derive(Clone, Copy, Debug)]
pub struct Note {
    pub value: BaseElement,
    /// recipient one-time public key (a field element for P1; a real PQ key in P3)
    pub pk: BaseElement,
    /// commitment randomness (blinding)
    pub r: BaseElement,
}

impl Note {
    /// cm = Rescue(value ‖ pk ‖ r) — the value hidden inside the tree leaf.
    pub fn commitment(&self) -> Hash {
        Rescue::hash_elements(&[self.value, self.pk, self.r])
    }

    /// nf = Rescue(spend_key ‖ position). Revealed on spend; unlinkable to the note.
    pub fn nullifier(spend_key: BaseElement, position: u64) -> Hash {
        Rescue::hash_elements(&[spend_key, BaseElement::new(position)])
    }
}

/// The note-commitment tree. Append-only in production (incremental root); here we rebuild
/// from the leaf set for clarity — the root + membership semantics are identical.
pub struct NoteTree {
    tree: MerkleTree<Rescue>,
    leaves: Vec<Hash>,
}

impl NoteTree {
    /// Build a tree over the given note commitments. `leaves.len()` must be a power of two.
    pub fn new(leaves: Vec<Hash>) -> Result<Self, String> {
        let tree = MerkleTree::<Rescue>::new(leaves.clone()).map_err(|e| format!("{e:?}"))?;
        Ok(Self { tree, leaves })
    }

    /// The anonymity-set root committed into `wallet_state_root`.
    pub fn root(&self) -> Hash {
        *self.tree.root()
    }

    pub fn depth(&self) -> usize {
        self.tree.depth()
    }

    /// The authentication path (sibling digests) for the leaf at `index`.
    pub fn path(&self, index: usize) -> Result<Vec<Hash>, String> {
        self.tree.prove(index).map_err(|e| format!("{e:?}"))
    }

    /// Verify a leaf + path against a root — the off-circuit reference the in-circuit AIR
    /// must agree with. `path[0]` is the leaf itself (winterfell convention).
    pub fn verify(root: Hash, index: usize, path: &[Hash]) -> bool {
        MerkleTree::<Rescue>::verify(root, index, path).is_ok()
    }

    pub fn leaf(&self, index: usize) -> Option<Hash> {
        self.leaves.get(index).copied()
    }
}

/// The spent-note set: every revealed nullifier. Rejecting a repeat is the double-spend guard.
/// In consensus this is a persistent flux-db column; its root folds into `wallet_state_root`.
#[derive(Default)]
pub struct NullifierSet {
    seen: HashSet<[u8; 32]>,
}

impl NullifierSet {
    pub fn new() -> Self {
        Self { seen: HashSet::new() }
    }

    /// Insert a spend's nullifier. Returns Err if it was already spent (double-spend).
    pub fn spend(&mut self, nf: Hash) -> Result<(), String> {
        if !self.seen.insert(nf.as_bytes()) {
            return Err("double-spend: nullifier already revealed".into());
        }
        Ok(())
    }

    pub fn contains(&self, nf: Hash) -> bool {
        self.seen.contains(&nf.as_bytes())
    }

    pub fn len(&self) -> usize {
        self.seen.len()
    }
    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(x: u64) -> BaseElement {
        BaseElement::new(x)
    }

    /// Real note commitments in a real Rescue Merkle tree: every leaf's path verifies against
    /// the root, and a tampered path is rejected. This is membership's off-circuit ground truth.
    #[test]
    fn note_tree_membership_verifies_and_rejects_tampering() {
        let notes: Vec<Note> = (0..8)
            .map(|i| Note { value: e(100 + i), pk: e(7 * i + 1), r: e(999 - i) })
            .collect();
        let leaves: Vec<Hash> = notes.iter().map(|n| n.commitment()).collect();
        let tree = NoteTree::new(leaves.clone()).expect("build tree");
        let root = tree.root();

        for i in 0..8 {
            let path = tree.path(i).expect("path");
            assert!(NoteTree::verify(root, i, &path), "leaf {i} must prove membership");

            // tamper the sibling → path must fail
            let mut bad = path.clone();
            if bad.len() > 1 {
                bad[1] = Rescue::hash_elements(&[e(424242)]); // wrong sibling
                assert!(!NoteTree::verify(root, i, &bad), "tampered path for leaf {i} must fail");
            }
            // claim membership at the WRONG index → fail
            let other = (i + 1) % 8;
            assert!(!NoteTree::verify(root, other, &path), "path must not verify at wrong index");
        }
    }

    /// Nullifiers are deterministic, unlinkable-looking, and the set catches double-spends.
    #[test]
    fn nullifier_is_deterministic_and_blocks_double_spend() {
        let sk = e(0xDEAD);
        let nf0 = Note::nullifier(sk, 0);
        let nf0_again = Note::nullifier(sk, 0);
        let nf1 = Note::nullifier(sk, 1);

        assert_eq!(nf0.as_bytes(), nf0_again.as_bytes(), "nullifier must be deterministic");
        assert_ne!(nf0.as_bytes(), nf1.as_bytes(), "different positions → different nullifiers");
        // a different spend key at the same position differs too
        assert_ne!(nf0.as_bytes(), Note::nullifier(e(0xBEEF), 0).as_bytes());

        let mut set = NullifierSet::new();
        set.spend(nf0).expect("first spend ok");
        assert!(set.contains(nf0));
        assert!(set.spend(nf0).is_err(), "SECURITY: double-spend must be rejected");
        set.spend(nf1).expect("distinct nullifier ok");
        assert_eq!(set.len(), 2);
    }
}
