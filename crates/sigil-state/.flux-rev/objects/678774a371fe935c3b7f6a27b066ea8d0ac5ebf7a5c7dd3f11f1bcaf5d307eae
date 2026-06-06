//! Incremental multiset-accumulator roots — Project Stargate-v1 item #1.
//!
//! ## Why this exists (measured, not guessed)
//!
//! `sigil-chronos::throughput` drove the real `apply_tx` + `commit_state_
//! transition` pipeline and found that on small blocks **73% of wall-clock
//! is spent re-hashing the four state roots**. The Phase-0 `hash_map`
//! serialises and BLAKE3s the *entire* `BTreeMap` every single block — O(n)
//! in the number of accounts, paid per block, forever. With 10,000 small
//! blocks that O(n) rehash dominates everything.
//!
//! This module replaces that with an **additive multiset hash** (the
//! LtHash / MSet-Add family, the same idea behind Facebook's LtHash and
//! Bitcoin's rolling-UTXO-commitment proposals):
//!
//! - every leaf `(key, value)` maps to `h = BLAKE3(LEAF_DOMAIN ‖ key ‖ value)`,
//!   interpreted as a little-endian 256-bit integer;
//! - the accumulator holds a running **wrapping 256-bit sum** of all live
//!   leaf hashes plus a `count`;
//! - inserting a leaf adds its hash; removing subtracts it; **updating a
//!   value is remove-old + add-new — all O(1)**, independent of map size;
//! - the published `root()` is `BLAKE3(ROOT_DOMAIN ‖ sum ‖ count_le)`, so the
//!   raw additive sum is never exposed and the committed root is a
//!   collision-resistant hash, not a linear function.
//!
//! `roots()` then becomes an O(1) read of three cached accumulators instead
//! of three O(n) rehashes. That is the single change the chronos harness
//! says recovers most of the wall-clock — the whole point of Stargate #1.
//!
//! ## Honest security posture
//!
//! Order-independence + O(1) updates come for free with additive multiset
//! hashing. Collision-resistance of the *published* root rests on BLAKE3
//! (the final hash) under the standard random-oracle assumption for the
//! per-leaf hash. A single 256-bit additive lane is a **Phase-0 performance
//! prototype** — production soundness against subset-sum / lattice attacks
//! wants either LtHash's wide (2 KiB) lane layout or, better, the real
//! Sparse Merkle Tree the genesis doc promises for P3 (which additionally
//! yields inclusion + non-membership proofs this accumulator does NOT).
//! Stargate #1 buys the *fast root*; proofs are a later, separate lane.
//!
//! The accumulator is deterministic and exactly reproducible, which is all
//! the chronos wind-tunnel needs to re-measure the moved bottleneck.

use serde::{Deserialize, Serialize};

use sigil_header::Root;

const LEAF_DOMAIN: &[u8] = b"sigil-acc-leaf-v1";
const ROOT_DOMAIN: &[u8] = b"sigil-acc-root-v1";

/// An incremental additive multiset accumulator over a set of `(key,value)`
/// leaves. Order-independent, O(1) per insert/remove/update, O(1) root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Accumulator {
    /// Running wrapping-256-bit sum of all live leaf hashes, little-endian.
    sum: [u8; 32],
    /// Number of live leaves. Bound into the root so an empty accumulator
    /// and a set that happens to sum to zero are distinguishable.
    count: u64,
}

impl Default for Accumulator {
    fn default() -> Self {
        Self { sum: [0u8; 32], count: 0 }
    }
}

impl Accumulator {
    /// Empty accumulator.
    pub fn new() -> Self {
        Self::default()
    }

    /// Leaf hash for `(key, value)` — BLAKE3 over the domain + both byte
    /// strings, length-prefixed so `(a, bc)` and `(ab, c)` can't collide.
    pub fn leaf_hash(key: &[u8], value: &[u8]) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        h.update(LEAF_DOMAIN);
        h.update(&(key.len() as u32).to_le_bytes());
        h.update(key);
        h.update(&(value.len() as u32).to_le_bytes());
        h.update(value);
        *h.finalize().as_bytes()
    }

    /// Insert a leaf. Adds its hash into the running sum.
    pub fn insert(&mut self, key: &[u8], value: &[u8]) {
        let leaf = Self::leaf_hash(key, value);
        self.sum = wrapping_add_256(self.sum, leaf);
        self.count = self.count.wrapping_add(1);
    }

    /// Remove a leaf. Subtracts its hash. Caller must remove only leaves
    /// that were previously inserted (the chokepoint in `lib.rs` guarantees
    /// this by reading the old value before overwriting it).
    pub fn remove(&mut self, key: &[u8], value: &[u8]) {
        let leaf = Self::leaf_hash(key, value);
        self.sum = wrapping_sub_256(self.sum, leaf);
        self.count = self.count.wrapping_sub(1);
    }

    /// Update a key's value: remove the old leaf, add the new one. The net
    /// effect on `count` is zero. This is the hot path — every balance
    /// change is one `update`, O(1) regardless of how many accounts exist.
    pub fn update(&mut self, key: &[u8], old_value: &[u8], new_value: &[u8]) {
        let old = Self::leaf_hash(key, old_value);
        let new = Self::leaf_hash(key, new_value);
        self.sum = wrapping_sub_256(self.sum, old);
        self.sum = wrapping_add_256(self.sum, new);
        // count unchanged — same key, still present.
    }

    /// Number of live leaves.
    pub fn len(&self) -> u64 {
        self.count
    }

    /// True when no leaves are present.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The published root: a domain-separated BLAKE3 over the additive sum
    /// and the leaf count. O(1). This is what goes into the block header's
    /// state-root fields.
    pub fn root(&self) -> Root {
        let mut h = blake3::Hasher::new();
        h.update(ROOT_DOMAIN);
        h.update(&self.sum);
        h.update(&self.count.to_le_bytes());
        *h.finalize().as_bytes()
    }

    /// Rebuild an accumulator from scratch over an iterator of
    /// `(key_bytes, value_bytes)` leaves. Used by tests to assert the
    /// incremental path matches a fresh fold, and available to callers that
    /// want to re-derive the accumulator after a cold load from storage.
    pub fn from_leaves<'a, I>(leaves: I) -> Self
    where
        I: IntoIterator<Item = (Vec<u8>, Vec<u8>)>,
    {
        let mut acc = Self::new();
        for (k, v) in leaves {
            acc.insert(&k, &v);
        }
        acc
    }
}

/// Wrapping 256-bit little-endian addition of two byte arrays.
fn wrapping_add_256(a: [u8; 32], b: [u8; 32]) -> [u8; 32] {
    let mut out = [0u8; 32];
    let mut carry: u16 = 0;
    for i in 0..32 {
        let s = a[i] as u16 + b[i] as u16 + carry;
        out[i] = (s & 0xff) as u8;
        carry = s >> 8;
    }
    out // final carry discarded → mod 2^256
}

/// Wrapping 256-bit little-endian subtraction `a - b`.
fn wrapping_sub_256(a: [u8; 32], b: [u8; 32]) -> [u8; 32] {
    let mut out = [0u8; 32];
    let mut borrow: i16 = 0;
    for i in 0..32 {
        let d = a[i] as i16 - b[i] as i16 - borrow;
        if d < 0 {
            out[i] = (d + 256) as u8;
            borrow = 1;
        } else {
            out[i] = d as u8;
            borrow = 0;
        }
    }
    out // final borrow discarded → mod 2^256
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(n: u8) -> Vec<u8> { vec![n; 8] }
    fn v(n: u128) -> Vec<u8> { n.to_le_bytes().to_vec() }

    #[test]
    fn add_then_sub_is_identity() {
        let x = [3u8; 32];
        let y = [7u8; 32];
        let s = wrapping_add_256(x, y);
        assert_eq!(wrapping_sub_256(s, y), x);
        assert_eq!(wrapping_sub_256(s, x), y);
    }

    #[test]
    fn add_wraps_mod_2_256() {
        let max = [0xffu8; 32];
        let one = {
            let mut a = [0u8; 32]; a[0] = 1; a
        };
        // 2^256-1 + 1 = 0 mod 2^256
        assert_eq!(wrapping_add_256(max, one), [0u8; 32]);
    }

    #[test]
    fn empty_root_is_stable_and_nonzero() {
        let a = Accumulator::new();
        let b = Accumulator::new();
        assert_eq!(a.root(), b.root());
        // domain + count binding means even empty hashes to something specific
        assert_ne!(a.root(), [0u8; 32]);
    }

    #[test]
    fn insert_changes_root_remove_restores_it() {
        let mut a = Accumulator::new();
        let empty = a.root();
        a.insert(&k(1), &v(100));
        assert_ne!(a.root(), empty);
        a.remove(&k(1), &v(100));
        assert_eq!(a.root(), empty, "insert+remove must return to empty root");
        assert!(a.is_empty());
    }

    #[test]
    fn update_is_equivalent_to_remove_then_add() {
        let mut a = Accumulator::new();
        a.insert(&k(1), &v(100));
        let mut b = a;
        // a: in-place update
        a.update(&k(1), &v(100), &v(250));
        // b: remove old, add new
        b.remove(&k(1), &v(100));
        b.insert(&k(1), &v(250));
        assert_eq!(a.root(), b.root());
        assert_eq!(a.len(), b.len());
    }

    #[test]
    fn order_independent() {
        // Insert the same leaves in two different orders → identical root.
        let mut a = Accumulator::new();
        a.insert(&k(1), &v(10)); a.insert(&k(2), &v(20)); a.insert(&k(3), &v(30));
        let mut b = Accumulator::new();
        b.insert(&k(3), &v(30)); b.insert(&k(1), &v(10)); b.insert(&k(2), &v(20));
        assert_eq!(a.root(), b.root());
    }

    #[test]
    fn incremental_matches_from_scratch_over_random_sequence() {
        // The core correctness property: a long mutation sequence's
        // incremental accumulator must equal a fresh fold over the final
        // live set. Deterministic pseudo-random so it's reproducible.
        use std::collections::BTreeMap;
        let mut live: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        let mut acc = Accumulator::new();
        let mut seed: u64 = 0x9e3779b97f4a7c15;
        let mut next = || { seed ^= seed << 13; seed ^= seed >> 7; seed ^= seed << 17; seed };

        for _ in 0..5000 {
            let key = vec![(next() % 64) as u8; 8];
            let op = next() % 3;
            match op {
                0 | 1 => {
                    // insert or update
                    let nv = v((next() % 1_000_000) as u128);
                    if let Some(old) = live.get(&key).cloned() {
                        acc.update(&key, &old, &nv);
                    } else {
                        acc.insert(&key, &nv);
                    }
                    live.insert(key, nv);
                }
                _ => {
                    // remove if present
                    if let Some(old) = live.remove(&key) {
                        acc.remove(&key, &old);
                    }
                }
            }
        }

        let fresh = Accumulator::from_leaves(live.iter().map(|(k, v)| (k.clone(), v.clone())));
        assert_eq!(acc.root(), fresh.root(), "incremental root diverged from from-scratch fold");
        assert_eq!(acc.len(), fresh.len(), "count diverged");
    }

    #[test]
    fn distinct_states_give_distinct_roots() {
        let mut a = Accumulator::new();
        a.insert(&k(1), &v(100));
        let mut b = Accumulator::new();
        b.insert(&k(1), &v(101)); // one unit different
        assert_ne!(a.root(), b.root());
    }

    #[test]
    fn serde_round_trip() {
        let mut a = Accumulator::new();
        a.insert(&k(5), &v(42));
        a.insert(&k(6), &v(43));
        let bytes = serde_json::to_vec(&a).unwrap();
        let b: Accumulator = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(a.root(), b.root());
    }
}
