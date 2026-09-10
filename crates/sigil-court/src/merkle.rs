//! Balanced BLAKE3 Merkle tree with the SAME padding rule as `sigil_state::hash_event_log`
//! (duplicate the last hash on odd levels; empty → all-zero). Parity with the chain is pinned by
//! a test against `sigil_events::prove_inclusion` / `verify_inclusion`.

use sigil_events::MerkleProof;

/// Root over `leaves` (all-zero for an empty set — the chain's rule).
pub fn root(leaves: &[[u8; 32]]) -> [u8; 32] {
    if leaves.is_empty() {
        return [0u8; 32];
    }
    let mut layer = leaves.to_vec();
    while layer.len() > 1 {
        if layer.len() % 2 == 1 {
            let last = *layer.last().unwrap();
            layer.push(last);
        }
        let mut next = Vec::with_capacity(layer.len() / 2);
        for pair in layer.chunks(2) {
            let mut h = blake3::Hasher::new();
            h.update(&pair[0]);
            h.update(&pair[1]);
            next.push(*h.finalize().as_bytes());
        }
        layer = next;
    }
    layer[0]
}

/// Inclusion proof for `leaves[index]` (None if out of range).
pub fn prove(leaves: &[[u8; 32]], index: usize) -> Option<MerkleProof> {
    if index >= leaves.len() {
        return None;
    }
    let mut layer = leaves.to_vec();
    let total = layer.len() as u32;
    let mut idx = index;
    let mut siblings = Vec::new();
    while layer.len() > 1 {
        if layer.len() % 2 == 1 {
            let last = *layer.last().unwrap();
            layer.push(last);
        }
        let sib = if idx % 2 == 0 { idx + 1 } else { idx - 1 };
        siblings.push(layer[sib]);
        let mut next = Vec::with_capacity(layer.len() / 2);
        for pair in layer.chunks(2) {
            let mut h = blake3::Hasher::new();
            h.update(&pair[0]);
            h.update(&pair[1]);
            next.push(*h.finalize().as_bytes());
        }
        layer = next;
        idx /= 2;
    }
    Some(MerkleProof { index: index as u32, total, siblings })
}

/// Verify a leaf hash against a root through `proof`.
pub fn verify(leaf: [u8; 32], proof: &MerkleProof, expected_root: [u8; 32]) -> bool {
    if proof.index >= proof.total {
        return false;
    }
    let depth = ceil_log2(proof.total.max(1) as usize);
    if proof.siblings.len() != depth {
        return false;
    }
    let mut acc = leaf;
    let mut idx = proof.index as usize;
    for sib in &proof.siblings {
        let mut h = blake3::Hasher::new();
        if idx % 2 == 0 {
            h.update(&acc);
            h.update(sib);
        } else {
            h.update(sib);
            h.update(&acc);
        }
        acc = *h.finalize().as_bytes();
        idx /= 2;
    }
    acc == expected_root
}

fn ceil_log2(n: usize) -> usize {
    if n <= 1 {
        return 0;
    }
    (usize::BITS - (n - 1).leading_zeros()) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use sigil_events::{prove_inclusion, verify_inclusion, SigilEvent};

    fn events() -> Vec<SigilEvent> {
        (0..5u64)
            .map(|i| SigilEvent::MintReward { miner: [i as u8; 32], height: i, amount: 10 + i as u128 })
            .collect()
    }

    #[test]
    fn root_and_proof_match_the_chain_for_every_index() {
        let ev = events();
        let leaves: Vec<[u8; 32]> = ev.iter().map(|e| e.leaf_hash()).collect();
        let r = root(&leaves);
        for i in 0..ev.len() {
            let theirs = prove_inclusion(&ev, i).unwrap();
            let ours = prove(&leaves, i).unwrap();
            assert_eq!(theirs, ours, "proof shape diverged at {i}");
            verify_inclusion(&ev[i], &ours, r).expect("chain verifier accepts our root");
            assert!(verify(leaves[i], &theirs, r));
        }
        assert_eq!(root(&[]), [0u8; 32]);
    }

    /// THE LINCHPIN. The court recomputes a block's event-log root and refuses any block whose
    /// supplied root disagrees. If this crate's Merkle rule ever diverged from
    /// `sigil_state::hash_event_log` — the rule the block HEADER commits — the court would refuse
    /// every real block and its archive would silently stay empty. Pin the two together against the
    /// chain's own chokepoint, not against our own reimplementation of it.
    #[test]
    fn our_root_equals_the_chains_event_log_root() {
        use sigil_state::{commit_state_transition, SigilState, StateMutation, StateTransition};
        for n in [1usize, 2, 3, 5, 8, 9] {
            let ev: Vec<SigilEvent> = (0..n as u64)
                .map(|i| SigilEvent::MintReward { miner: [i as u8; 32], height: i, amount: 7 + i as u128 })
                .collect();
            let mut st = SigilState::new();
            let mutations = ev.iter().map(|e| StateMutation::PushEventHash(e.leaf_hash())).collect();
            let roots = commit_state_transition(&mut st, &StateTransition { at_height: 1, mutations }, 1)
                .expect("chokepoint accepts the transition");
            let leaves: Vec<[u8; 32]> = ev.iter().map(|e| e.leaf_hash()).collect();
            assert_eq!(
                root(&leaves), roots.event_log_root,
                "court Merkle rule diverged from the chain's event_log_root at n={n}"
            );
        }
    }

    #[test]
    fn tampered_leaf_fails() {
        let ev = events();
        let leaves: Vec<[u8; 32]> = ev.iter().map(|e| e.leaf_hash()).collect();
        let r = root(&leaves);
        let p = prove(&leaves, 2).unwrap();
        assert!(!verify([9u8; 32], &p, r));
    }
}
