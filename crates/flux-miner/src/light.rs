//! Lightweight node via flux-fold — verify the chain's state without its bodies.
//!
//! v0.3 application of the succinct lattice backend: a full node folds every
//! block's state-commitment into ONE constant-size proof (`flux-fold`). A light
//! client then accepts the entire chain by downloading only the per-block
//! commitments (tiny — m field elements each) plus that one 2.5 KB fold, and
//! running a SINGLE fold-verify instead of re-verifying N blocks. No block
//! bodies, no per-block signature/VDF re-checks.
//!
//! Honest scope: verify still reads the N commitments (O(N) small data) to
//! recompute the random linear combination. Collapsing that to O(1) — a true
//! "verify the whole chain from one proof + one root" tip — is the IVC recursion
//! (v0.4). v0.3 already removes block bodies + per-block re-verification.

use flux_fold::{fold, verify as fold_verify, Ajtai, FoldedProof, Q};

/// A light client's whole-chain proof: per-block state commitments (public,
/// tiny) + ONE folded proof attesting they all fold consistently.
pub struct ChainProof {
    pub commitments: Vec<Vec<u64>>,
    pub fold: FoldedProof,
    pub n_blocks: usize,
}

impl ChainProof {
    /// Bytes a light node downloads: the N commitments + the constant fold.
    pub fn light_download_bytes(&self, m: usize) -> usize {
        self.commitments.len() * m * 8 + self.fold.size_bytes()
    }
}

/// Expand a 32-byte block digest into an `n`-element Ajtai witness — the block's
/// committed state. Deterministic, so prover and light node agree.
pub fn block_witness(n: usize, block_digest: &[u8; 32]) -> Vec<u64> {
    let mut w = Vec::with_capacity(n);
    let mut ctr = 0u32;
    while w.len() < n {
        let mut h = blake3::Hasher::new();
        h.update(b"flux-light/block-witness/v1");
        h.update(block_digest);
        h.update(&ctr.to_le_bytes());
        for chunk in h.finalize().as_bytes().chunks(8) {
            if w.len() < n {
                w.push(u64::from_le_bytes(chunk.try_into().unwrap()) % Q);
            }
        }
        ctr += 1;
    }
    w
}

/// Full node / producer: fold all `block_digests` into one chain proof.
pub fn prove_chain(ajtai: &Ajtai, block_digests: &[[u8; 32]]) -> ChainProof {
    let witnesses: Vec<Vec<u64>> =
        block_digests.iter().map(|d| block_witness(ajtai.n, d)).collect();
    let commitments: Vec<Vec<u64>> = witnesses.iter().map(|w| ajtai.commit(w)).collect();
    let folded = fold(ajtai, &witnesses);
    ChainProof { commitments, fold: folded, n_blocks: block_digests.len() }
}

/// Light node: accept the WHOLE chain via the single folded proof — no bodies,
/// one verification regardless of how the blocks were produced.
pub fn light_verify(ajtai: &Ajtai, proof: &ChainProof) -> bool {
    fold_verify(ajtai, &proof.commitments, &proof.fold)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn light_node_accepts_chain_via_one_fold() {
        let ajtai = Ajtai::from_seed(64, 256, &[3u8; 32]);
        let digests: Vec<[u8; 32]> = (0..512u32)
            .map(|i| *blake3::hash(&i.to_le_bytes()).as_bytes())
            .collect();
        let proof = prove_chain(&ajtai, &digests);
        assert_eq!(proof.n_blocks, 512);
        assert!(light_verify(&ajtai, &proof), "light node must accept the honest chain");
        // the fold itself is constant-size regardless of chain length
        assert_eq!(proof.fold.size_bytes(), (64 + 256) * 8 + 8);

        // tamper one block's commitment → light verify rejects the chain
        let mut bad = ChainProof { commitments: proof.commitments.clone(), fold: proof.fold.clone(), n_blocks: 512 };
        bad.commitments[100][0] ^= 1;
        assert!(!light_verify(&ajtai, &bad), "a tampered block must be rejected");
    }
}
