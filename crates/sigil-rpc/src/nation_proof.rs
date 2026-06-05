//! nation_proof.rs — **succinct citizen-action proofs**. Makes the v3
//! statsministeriet claim literally true: a light client (a browser node)
//! verifies that a specific *borger-handling* — an e-Boks receipt, a utility
//! payment, an attestation — is committed in a published root, with **only the
//! leaf + a log₂(n) Merkle path + the root**. No full state, no server.
//!
//! The action log is a balanced binary BLAKE3 Merkle tree (same construction as
//! the chain's `event_log_root`), so the prototype composes with the existing
//! tip-proof: publish `nation_action_root` in the tip, hand the citizen their
//! `ActionProof`, and the light client checks it offline.

use blake3::Hasher;
use sigil_state::WalletId;

/// 32-byte BLAKE3 root (mirrors `sigil_state`'s internal `Root`).
type Root = [u8; 32];

/// Action tags — what kind of borger-handling a leaf attests.
pub const TAG_ATTEST: u8 = 0x01;
pub const TAG_EBOKS: u8 = 0x02;
pub const TAG_UTILITY: u8 = 0x03;
pub const TAG_PRESENCE: u8 = 0x04;

/// The leaf for one citizen action: BLAKE3(citizen ‖ tag ‖ payload_hash).
/// `payload_hash` is e.g. the e-Boks document hash, or BLAKE3 of (provider,amount).
pub fn action_leaf(citizen: &WalletId, tag: u8, payload_hash: &[u8; 32]) -> [u8; 32] {
    let mut h = Hasher::new();
    h.update(citizen);
    h.update(&[tag]);
    h.update(payload_hash);
    *h.finalize().as_bytes()
}

fn hash_pair(l: &[u8; 32], r: &[u8; 32]) -> [u8; 32] {
    let mut h = Hasher::new();
    h.update(l);
    h.update(r);
    *h.finalize().as_bytes()
}

/// Root of the action log (balanced binary Merkle; pad odd levels with the last
/// node — IDENTICAL rule to `sigil_state`'s event_log_root so it composes).
pub fn nation_action_root(leaves: &[[u8; 32]]) -> Root {
    if leaves.is_empty() {
        return [0u8; 32];
    }
    let mut layer = leaves.to_vec();
    while layer.len() > 1 {
        if layer.len() % 2 == 1 {
            layer.push(*layer.last().unwrap());
        }
        layer = layer.chunks(2).map(|p| hash_pair(&p[0], &p[1])).collect();
    }
    layer[0]
}

/// A succinct inclusion proof for one action.
#[derive(Debug, Clone, PartialEq)]
pub struct ActionProof {
    pub index: usize,
    pub siblings: Vec<[u8; 32]>,
}

/// Build the proof that `leaves[index]` is in `nation_action_root(leaves)`.
pub fn prove_action(leaves: &[[u8; 32]], index: usize) -> Option<ActionProof> {
    if index >= leaves.len() {
        return None;
    }
    let mut layer = leaves.to_vec();
    let mut idx = index;
    let mut siblings = Vec::new();
    while layer.len() > 1 {
        if layer.len() % 2 == 1 {
            layer.push(*layer.last().unwrap());
        }
        let sib = idx ^ 1; // adjacent node; for a padded last node this is its own copy
        siblings.push(layer[sib]);
        layer = layer.chunks(2).map(|p| hash_pair(&p[0], &p[1])).collect();
        idx >>= 1;
    }
    Some(ActionProof { index, siblings })
}

/// Verify offline: recompute the root from `leaf` + the path, compare to `root`.
/// This is what runs in the browser light client — no state, no server.
pub fn verify_action_proof(leaf: &[u8; 32], proof: &ActionProof, root: &Root) -> bool {
    let mut h = *leaf;
    let mut idx = proof.index;
    for sib in &proof.siblings {
        h = if idx % 2 == 0 { hash_pair(&h, sib) } else { hash_pair(sib, &h) };
        idx >>= 1;
    }
    &h == root
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaves(n: usize) -> Vec<[u8; 32]> {
        (0..n)
            .map(|i| action_leaf(&[i as u8; 32], TAG_EBOKS, &[(i * 7) as u8; 32]))
            .collect()
    }

    #[test]
    fn every_index_proves_and_verifies() {
        for n in 1..=9 {
            let ls = leaves(n);
            let root = nation_action_root(&ls);
            for i in 0..n {
                let p = prove_action(&ls, i).unwrap();
                assert!(verify_action_proof(&ls[i], &p, &root), "n={n} i={i} should verify");
                // light client never needs the other leaves — proof is log₂(n)
                assert!(p.siblings.len() <= (n as f64).log2().ceil() as usize + 1);
            }
        }
    }

    #[test]
    fn tampered_leaf_fails() {
        let ls = leaves(6);
        let root = nation_action_root(&ls);
        let p = prove_action(&ls, 3).unwrap();
        let mut fake = ls[3];
        fake[0] ^= 0xff; // forge the receipt
        assert!(!verify_action_proof(&fake, &p, &root));
    }

    #[test]
    fn wrong_root_fails() {
        let ls = leaves(6);
        let p = prove_action(&ls, 1).unwrap();
        let mut bad_root = nation_action_root(&ls);
        bad_root[0] ^= 0xff;
        assert!(!verify_action_proof(&ls[1], &p, &bad_root));
    }

    #[test]
    fn proof_for_other_index_fails() {
        let ls = leaves(6);
        let root = nation_action_root(&ls);
        let p2 = prove_action(&ls, 2).unwrap();
        // try to pass off leaf 4 with leaf 2's path
        assert!(!verify_action_proof(&ls[4], &p2, &root));
    }

    #[test]
    fn single_action_log() {
        let ls = leaves(1);
        let root = nation_action_root(&ls);
        let p = prove_action(&ls, 0).unwrap();
        assert!(p.siblings.is_empty());
        assert!(verify_action_proof(&ls[0], &p, &root));
        assert_eq!(root, ls[0]); // root of a 1-leaf tree is the leaf
    }

    #[test]
    fn eboks_receipt_end_to_end() {
        // Alice's e-Boks receipt is one of 5 published citizen actions.
        let alice: WalletId = [0xA1; 32];
        let doc: [u8; 32] = [0xD0; 32];
        let mut acts = leaves(4);
        let alice_leaf = action_leaf(&alice, TAG_EBOKS, &doc);
        acts.push(alice_leaf);
        let root = nation_action_root(&acts);
        let proof = prove_action(&acts, 4).unwrap();
        // a browser, given ONLY (alice, doc, proof, root), confirms the receipt
        let recomputed_leaf = action_leaf(&alice, TAG_EBOKS, &doc);
        assert!(verify_action_proof(&recomputed_leaf, &proof, &root));
    }

    /// "Test with SIGIL also" — the full borger-handling on a real `SigilState`:
    /// attest citizens → issue e-Boks receipts (committed in contract_state_root)
    /// → build the action log → succinct proof → light-client verify. Tamper is
    /// rejected on BOTH the chain slot and the offline proof.
    #[test]
    fn sigil_end_to_end_borger_handling() {
        use crate::nation::{attest_citizen, issue_eboks_receipt, verify_eboks_receipt, BORGER_AUTHORITY};
        use sigil_state::{commit_state_transition, SigilState, StateMutation, StateTransition};

        let mut state = SigilState::new();
        commit_state_transition(&mut state, &StateTransition { at_height: 0,
            mutations: vec![StateMutation::SetMasterWallet { wallet: [0xFF; 32] }] }, 0).unwrap();

        let citizens: Vec<WalletId> = (0..3u8).map(|i| [0x10 + i; 32]).collect();
        let docs: Vec<[u8; 32]> = (0..3u8).map(|i| [0x80 + i; 32]).collect();
        for (i, c) in citizens.iter().enumerate() {
            attest_citizen(&mut state, 1, BORGER_AUTHORITY, *c, [0x40 + i as u8; 32]).unwrap();
        }
        let root_genesis = state.roots().contract_state_root;
        for (c, d) in citizens.iter().zip(&docs) {
            issue_eboks_receipt(&mut state, 2, *c, *d).unwrap();
        }
        // 1) CHAIN: the receipt is committed in the SIGIL contract_state_root.
        assert!(verify_eboks_receipt(&state, &citizens[1], &docs[1]));
        assert_ne!(state.roots().contract_state_root, root_genesis, "actions must move the root");

        // 2) SUCCINCT proof a browser verifies with no state + no server.
        let leaves: Vec<[u8; 32]> =
            citizens.iter().zip(&docs).map(|(c, d)| action_leaf(c, TAG_EBOKS, d)).collect();
        let action_root = nation_action_root(&leaves);
        let proof = prove_action(&leaves, 1).unwrap();
        let leaf = action_leaf(&citizens[1], TAG_EBOKS, &docs[1]);
        assert!(verify_action_proof(&leaf, &proof, &action_root));

        // 3) a forged document fails BOTH the chain slot and the offline proof.
        assert!(!verify_eboks_receipt(&state, &citizens[1], &[0xEE; 32]));
        let forged = action_leaf(&citizens[1], TAG_EBOKS, &[0xEE; 32]);
        assert!(!verify_action_proof(&forged, &proof, &action_root));
    }
}
