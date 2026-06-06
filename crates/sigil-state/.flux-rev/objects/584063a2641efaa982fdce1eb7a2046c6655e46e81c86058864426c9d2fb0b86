//! Sparse Merkle Tree — incremental state-root accumulator (STAR-1).
//!
//! ## Why this exists
//!
//! Phase-0 `roots()` re-hashed the ENTIRE state map on every block (O(n)).
//! `sigil-chronos::throughput` measured this as **73% of block wall-clock** —
//! the #1 obstacle to Stargate's 1M-TPS target. This SMT replaces it: each
//! mutation updates one root-to-leaf path (a fixed 256 hashes, i.e. O(1) in
//! the state size), and the root is read in O(1). Per-block root cost becomes
//! proportional to *touched* keys, not *total* keys — so it stays flat as the
//! state grows to millions of accounts.
//!
//! Bonus (the genesis §3 reason): an SMT gives **inclusion + non-membership
//! proofs**, which the tip-proof / verify-before-sync path needs. The flat
//! BLAKE3-of-sorted-leaves it replaces could not.
//!
//! ## Design
//!
//! - Depth 256, keyed by `BLAKE3(key)`. No collisions (full 256-bit path), so
//!   a leaf commits only its value hash — no key-in-leaf disambiguation.
//! - **Default-hash optimization:** an empty subtree at depth `d` has a fixed,
//!   precomputed hash `DEFAULT[d]`, so the vast sparse majority of the tree is
//!   never materialized. Only non-default nodes live in the `nodes` map.
//! - **Incremental:** `update(key, value)` walks the single affected path from
//!   leaf to root, recomputing each parent from its two children (each child
//!   is a stored node or a default). 256 hashes per update, independent of n.
//!
//! Determinism: pure BLAKE3 over fixed byte layouts — identical on every node,
//! which is the prerequisite the whole verifiable-execution story rests on.

use std::collections::HashMap;

/// Tree depth = key-hash bit length.
const DEPTH: usize = 256;

/// A node's address: (depth, path) where `path` is the key-hash with all bits
/// below `depth` cleared. Depth 0 = root, depth 256 = leaf.
type NodeKey = (u16, [u8; 32]);

/// Incremental Sparse Merkle Tree.
#[derive(Clone)]
pub struct Smt {
    /// Non-default nodes only (the tree is overwhelmingly default).
    nodes: HashMap<NodeKey, [u8; 32]>,
    /// `default[d]` = hash of an entirely-empty subtree rooted at depth `d`.
    /// `default[256]` = the empty-leaf sentinel.
    default: Vec<[u8; 32]>,
    /// Cached root (= node(0, 0) or `default[0]`).
    root: [u8; 32],
}

impl std::fmt::Debug for Smt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Smt {{ nodes: {}, root: {} }}", self.nodes.len(), hex8(&self.root))
    }
}

impl Default for Smt {
    fn default() -> Self {
        Self::new()
    }
}

impl Smt {
    /// Empty tree.
    pub fn new() -> Self {
        // default[256] = empty-leaf sentinel (all zero). Each level up hashes
        // two empty children.
        let mut default = vec![[0u8; 32]; DEPTH + 1];
        for d in (0..DEPTH).rev() {
            default[d] = hash_node(&default[d + 1], &default[d + 1]);
        }
        let root = default[0];
        Self { nodes: HashMap::new(), default, root }
    }

    /// Current root.
    pub fn root(&self) -> [u8; 32] {
        self.root
    }

    /// Number of materialized (non-default) nodes — for diagnostics/tests.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Insert / update / delete a key. `value == None` (or an all-zero leaf
    /// produced by an empty value) deletes. The value bytes are hashed into the
    /// leaf; callers pass a canonical serialization of the stored value.
    pub fn update(&mut self, key: &[u8], value: Option<&[u8]>) {
        let key_hash = *blake3::hash(key).as_bytes();
        let leaf = match value {
            Some(v) => *blake3::hash(v).as_bytes(),
            None => [0u8; 32],
        };
        self.set_leaf(key_hash, leaf);
    }

    /// Set the leaf at `key_hash` to `leaf_hash` and recompute the path to root.
    fn set_leaf(&mut self, key_hash: [u8; 32], leaf_hash: [u8; 32]) {
        // Place (or clear) the leaf.
        self.put(DEPTH as u16, key_hash, leaf_hash);

        // Walk up: at each step compute the parent from this node + its sibling.
        let mut cur = leaf_hash;
        for d in (1..=DEPTH).rev() {
            let bit = bit_at(&key_hash, d - 1); // the branch bit into this node
            let sib_path = sibling_path(&key_hash, d);
            let sibling = self.get(d as u16, &sib_path);

            // Parent = H(left || right); `bit` says whether `cur` is right(1).
            let parent = if bit == 0 {
                hash_node(&cur, &sibling)
            } else {
                hash_node(&sibling, &cur)
            };

            let parent_path = mask_to_depth(&key_hash, d - 1);
            self.put((d - 1) as u16, parent_path, parent);
            cur = parent;
        }
        self.root = cur;
    }

    /// Get a node hash, falling back to the depth's default for empty subtrees.
    fn get(&self, depth: u16, path: &[u8; 32]) -> [u8; 32] {
        self.nodes
            .get(&(depth, *path))
            .copied()
            .unwrap_or(self.default[depth as usize])
    }

    /// Store a node, or remove it if it equals the depth's default (keeping the
    /// map sparse + the structure canonical).
    fn put(&mut self, depth: u16, path: [u8; 32], hash: [u8; 32]) {
        if hash == self.default[depth as usize] {
            self.nodes.remove(&(depth, path));
        } else {
            self.nodes.insert((depth, path), hash);
        }
    }
}

/// A Merkle proof: the 256 sibling hashes along a key's root-to-leaf path,
/// plus the leaf value hash being proven. Verifies against a root WITHOUT the
/// tree — this is what a 10ms light client checks (genesis §3). An
/// all-zero `leaf` proves NON-membership (the key maps to an empty leaf).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MerkleProof {
    /// BLAKE3(key) — the path.
    pub key_hash: [u8; 32],
    /// Hash of the value at the leaf (`[0;32]` = absent → non-membership).
    pub leaf: [u8; 32],
    /// Sibling hashes from depth 256 (leaf level) up to depth 1.
    pub siblings: Vec<[u8; 32]>,
}

impl Smt {
    /// Produce an inclusion proof for `key` (or a non-membership proof if the
    /// key is absent — the leaf hash will be `[0;32]`).
    pub fn prove(&self, key: &[u8]) -> MerkleProof {
        let key_hash = *blake3::hash(key).as_bytes();
        let leaf = self.get(DEPTH as u16, &key_hash);
        let mut siblings = Vec::with_capacity(DEPTH);
        for d in (1..=DEPTH).rev() {
            siblings.push(self.get(d as u16, &sibling_path(&key_hash, d)));
        }
        MerkleProof { key_hash, leaf, siblings }
    }

    /// Hash a value the way `update` does, for building expected-leaf checks.
    pub fn leaf_hash(value: &[u8]) -> [u8; 32] {
        *blake3::hash(value).as_bytes()
    }
}

/// Verify a Merkle proof against `root`. Returns true iff the proof's
/// leaf+siblings recompute to `root`. Stateless — no tree needed, which is the
/// whole point: a light client holding only the tip-proof's root can check any
/// key in 256 hashes (~microseconds). Pass `leaf == [0;32]` to verify
/// non-membership.
pub fn verify_proof(root: &[u8; 32], proof: &MerkleProof) -> bool {
    if proof.siblings.len() != DEPTH {
        return false;
    }
    let mut cur = proof.leaf;
    // siblings[0] is at depth 256 (leaf), … siblings[255] at depth 1.
    for (idx, sib) in proof.siblings.iter().enumerate() {
        let d = DEPTH - idx; // current node depth (256 down to 1)
        let bit = bit_at(&proof.key_hash, d - 1);
        cur = if bit == 0 {
            hash_node(&cur, sib)
        } else {
            hash_node(sib, &cur)
        };
    }
    &cur == root
}

/// Hash an internal node from its two children.
fn hash_node(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"smt-node");
    h.update(left);
    h.update(right);
    *h.finalize().as_bytes()
}

/// Bit `i` (0 = most-significant) of a 256-bit key hash. Bit `i` is the branch
/// decision at depth `i` (0 = left, 1 = right).
fn bit_at(key: &[u8; 32], i: usize) -> u8 {
    let byte = key[i / 8];
    (byte >> (7 - (i % 8))) & 1
}

/// `key` with all bits at position `>= depth` cleared — the canonical path
/// identifying a node at `depth`.
fn mask_to_depth(key: &[u8; 32], depth: usize) -> [u8; 32] {
    let mut out = [0u8; 32];
    let full_bytes = depth / 8;
    out[..full_bytes].copy_from_slice(&key[..full_bytes]);
    let rem = depth % 8;
    if rem != 0 {
        // keep the top `rem` bits of the next byte
        let mask = 0xFFu8 << (8 - rem);
        out[full_bytes] = key[full_bytes] & mask;
    }
    out
}

/// Path of the sibling of the node at `depth` on `key`'s path: same as the
/// node's path but with the branch bit (`depth-1`) flipped.
fn sibling_path(key: &[u8; 32], depth: usize) -> [u8; 32] {
    let mut p = mask_to_depth(key, depth);
    let i = depth - 1;
    p[i / 8] ^= 1u8 << (7 - (i % 8));
    p
}

fn hex8(b: &[u8; 32]) -> String {
    let mut s = String::with_capacity(16);
    for x in &b[..8] {
        s.push_str(&format!("{:02x}", x));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_root_is_stable() {
        let a = Smt::new();
        let b = Smt::new();
        assert_eq!(a.root(), b.root());
        assert_eq!(a.node_count(), 0);
    }

    #[test]
    fn insert_changes_root_delete_restores_it() {
        let mut s = Smt::new();
        let empty = s.root();
        s.update(b"alice", Some(b"100"));
        let after = s.root();
        assert_ne!(after, empty, "insert must change the root");
        // Deleting the only key returns to the empty root + zero nodes.
        s.update(b"alice", None);
        assert_eq!(s.root(), empty, "deleting the last key restores empty root");
        assert_eq!(s.node_count(), 0, "no leftover nodes after delete");
    }

    #[test]
    fn order_independent() {
        // Same key/value set, inserted in different orders → identical root.
        let mut a = Smt::new();
        a.update(b"k1", Some(b"v1"));
        a.update(b"k2", Some(b"v2"));
        a.update(b"k3", Some(b"v3"));

        let mut b = Smt::new();
        b.update(b"k3", Some(b"v3"));
        b.update(b"k1", Some(b"v1"));
        b.update(b"k2", Some(b"v2"));

        assert_eq!(a.root(), b.root());
    }

    #[test]
    fn update_in_place() {
        let mut a = Smt::new();
        a.update(b"k", Some(b"v1"));
        a.update(b"k", Some(b"v2"));

        let mut b = Smt::new();
        b.update(b"k", Some(b"v2"));

        assert_eq!(a.root(), b.root(), "overwriting a key == inserting the final value");
    }

    #[test]
    fn deterministic_across_instances() {
        let build = || {
            let mut s = Smt::new();
            for i in 0..500u32 {
                s.update(&i.to_le_bytes(), Some(&(i * 7).to_le_bytes()));
            }
            s.root()
        };
        assert_eq!(build(), build());
    }

    #[test]
    fn inclusion_proof_verifies_and_tamper_is_caught() {
        let mut s = Smt::new();
        s.update(b"alice", Some(b"100"));
        s.update(b"bob", Some(b"250"));
        s.update(b"carol", Some(b"7"));
        let root = s.root();

        // Prove alice = 100 against the root, no tree needed.
        let proof = s.prove(b"alice");
        assert_eq!(proof.leaf, Smt::leaf_hash(b"100"));
        assert!(verify_proof(&root, &proof), "valid inclusion proof must verify");

        // Tamper the leaf → must fail.
        let mut bad = proof.clone();
        bad.leaf = Smt::leaf_hash(b"999");
        assert!(!verify_proof(&root, &bad), "tampered value must not verify");

        // Tamper a sibling → must fail.
        let mut bad2 = proof.clone();
        bad2.siblings[10] = [0xAB; 32];
        assert!(!verify_proof(&root, &bad2), "tampered sibling must not verify");
    }

    #[test]
    fn non_membership_proof_verifies() {
        let mut s = Smt::new();
        s.update(b"alice", Some(b"100"));
        let root = s.root();
        // "mallory" was never inserted → non-membership proof (leaf == 0).
        let proof = s.prove(b"mallory");
        assert_eq!(proof.leaf, [0u8; 32], "absent key proves an empty leaf");
        assert!(verify_proof(&root, &proof), "non-membership proof must verify");
    }

    #[test]
    fn delete_subset_matches_fresh_build() {
        // Insert 100, delete the even ones, compare to a fresh tree of just the
        // odds — proves incremental delete keeps the tree canonical.
        let mut a = Smt::new();
        for i in 0..100u32 {
            a.update(&i.to_le_bytes(), Some(b"x"));
        }
        for i in (0..100u32).step_by(2) {
            a.update(&i.to_le_bytes(), None);
        }

        let mut b = Smt::new();
        for i in (1..100u32).step_by(2) {
            b.update(&i.to_le_bytes(), Some(b"x"));
        }
        assert_eq!(a.root(), b.root());
        assert_eq!(a.node_count(), b.node_count());
    }
}
