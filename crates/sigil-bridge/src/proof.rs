//! proof.rs — the cryptographic core: a source-chain SPV inclusion proof the
//! SIGIL node VERIFIES before minting (vs Quillon trusting committee signatures).
//!
//! v0.2 closes the trust gap: the proof now carries the **Bitcoin header chain**
//! burying the deposit, and verification checks **real proof-of-work** — each
//! header's double-SHA256 hash ≤ its `nBits` target, and each links to the prior
//! by `prev_block`. So `confirmations` is no longer a number to trust; it's the
//! count of valid-PoW headers an attacker would have to actually mine. End to end:
//!
//!   tx_bytes ─dsha256→ tx_hash ─merkle branch→ headers[0].merkle_root
//!   headers[0..n]: each hash ≤ target(nBits) ∧ links via prev_block  →  n confs of PoW
//!
//! Remaining hardening (documented, not pretended): anchoring the chain to a
//! known checkpoint / requiring it be the most-work chain (today: N valid-PoW
//! headers on top of the deposit, the standard SPV confirmation proof), and
//! parsing amount/recipient out of `tx_bytes`.

use sha2::{Digest, Sha256};

/// Bitcoin's hash: SHA256(SHA256(data)).
pub fn dsha256(data: &[u8]) -> [u8; 32] {
    let first = Sha256::digest(data);
    let second = Sha256::digest(first);
    let mut out = [0u8; 32];
    out.copy_from_slice(&second);
    out
}

/// Decode a Bitcoin compact `nBits` into a 256-bit big-endian target.
/// target = mantissa(3 bytes) · 256^(exponent-3).
pub fn target_from_nbits(nbits: u32) -> [u8; 32] {
    let exp = ((nbits >> 24) & 0xff) as usize;
    let mant = nbits & 0x00ff_ffff;
    let mut t = [0u8; 32];
    if mant == 0 || exp == 0 {
        return t;
    }
    // mantissa MSB→LSB; LSB sits at BE-index (34 - exp).
    let mbytes = [(mant >> 16) as u8, (mant >> 8) as u8, mant as u8];
    for (k, b) in mbytes.iter().enumerate() {
        let idx = 32isize - exp as isize + k as isize;
        if (0..32).contains(&idx) {
            t[idx as usize] = *b;
        }
    }
    t
}

/// Does this 80-byte header satisfy its own PoW target? (dsha256(header), read
/// as a little-endian 256-bit number, ≤ target(nBits)).
pub fn header_meets_target(header: &[u8; 80]) -> bool {
    let mut be = dsha256(header);
    be.reverse(); // dsha256 output is little-endian; reverse → big-endian for compare
    let nbits = u32::from_le_bytes(header[72..76].try_into().unwrap());
    be <= target_from_nbits(nbits)
}

/// Fold a leaf up through a Merkle `branch` (sibling hashes, leaf→root) using
/// the bits of `index` (0 = leaf is the left child) to reach `merkle_root`.
pub fn verify_merkle_inclusion(leaf: [u8; 32], branch: &[[u8; 32]], mut index: u64, merkle_root: [u8; 32]) -> bool {
    let mut acc = leaf;
    for sib in branch {
        let mut buf = [0u8; 64];
        if index & 1 == 0 {
            buf[..32].copy_from_slice(&acc);
            buf[32..].copy_from_slice(sib);
        } else {
            buf[..32].copy_from_slice(sib);
            buf[32..].copy_from_slice(&acc);
        }
        acc = dsha256(&buf);
        index >>= 1;
    }
    acc == merkle_root
}

/// Why a proof was rejected.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProofError {
    #[error("empty header chain")]
    EmptyChain,
    #[error("insufficient confirmations: chain has {got} PoW headers, need {need}")]
    InsufficientConfirmations { got: u32, need: u32 },
    #[error("header {0} fails its proof-of-work target")]
    BadProofOfWork(usize),
    #[error("header {0} does not link to the previous header (prev_block mismatch)")]
    BrokenChain(usize),
    #[error("tx bytes do not hash to the claimed tx_hash")]
    TxHashMismatch,
    #[error("Merkle branch does not reach the deposit block's merkle root")]
    MerkleMismatch,
}

/// A self-verifying source-chain deposit proof with the burying PoW chain.
#[derive(Debug, Clone)]
pub struct SpvProof {
    /// Raw source-chain transaction bytes (the deposit).
    pub tx_bytes: Vec<u8>,
    /// dsha256(tx_bytes) — the Merkle leaf.
    pub tx_hash: [u8; 32],
    /// Sibling hashes from the leaf up to the deposit block's Merkle root.
    pub branch: Vec<[u8; 32]>,
    /// Leaf position in the deposit block's tx list.
    pub tx_index: u64,
    /// `headers[0]` is the block containing the deposit; the rest are its PoW
    /// successors. Length = confirmations.
    pub headers: Vec<[u8; 80]>,
}

impl SpvProof {
    /// The deposit block's hash (dsha256 of headers[0]).
    pub fn block_hash(&self) -> Option<[u8; 32]> {
        self.headers.first().map(|h| dsha256(h))
    }

    /// Verify the whole chain: PoW on every header, prev_block linkage,
    /// confirmation depth, the tx↔tx_hash bind, and Merkle inclusion in the
    /// deposit block. No trusted party.
    pub fn verify(&self, min_confirmations: u32) -> Result<(), ProofError> {
        let n = self.headers.len();
        if n == 0 {
            return Err(ProofError::EmptyChain);
        }
        // 1. real proof-of-work on every header.
        for (i, h) in self.headers.iter().enumerate() {
            if !header_meets_target(h) {
                return Err(ProofError::BadProofOfWork(i));
            }
        }
        // 2. the chain links: headers[i].prev_block == dsha256(headers[i-1]).
        for i in 1..n {
            let prev_hash = dsha256(&self.headers[i - 1]);
            if self.headers[i][4..36] != prev_hash {
                return Err(ProofError::BrokenChain(i));
            }
        }
        // 3. enough PoW burying the deposit.
        if (n as u32) < min_confirmations {
            return Err(ProofError::InsufficientConfirmations { got: n as u32, need: min_confirmations });
        }
        // 4. tx_bytes ─dsha256→ tx_hash.
        if dsha256(&self.tx_bytes) != self.tx_hash {
            return Err(ProofError::TxHashMismatch);
        }
        // 5. tx_hash ─branch→ deposit block's merkle root (header bytes 36..68).
        let mut merkle_root = [0u8; 32];
        merkle_root.copy_from_slice(&self.headers[0][36..68]);
        if !verify_merkle_inclusion(self.tx_hash, &self.branch, self.tx_index, merkle_root) {
            return Err(ProofError::MerkleMismatch);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Regtest powLimit: a very easy target so a few nonce tries satisfy PoW.
    const EASY_NBITS: u32 = 0x207f_ffff;

    /// Mine a header (grind the nonce) that links to `prev` and commits
    /// `merkle_root`, until it meets the easy target.
    fn mine(prev: [u8; 32], merkle_root: [u8; 32]) -> [u8; 80] {
        let mut h = [0u8; 80];
        h[0] = 1; // version
        h[4..36].copy_from_slice(&prev);
        h[36..68].copy_from_slice(&merkle_root);
        h[72..76].copy_from_slice(&EASY_NBITS.to_le_bytes());
        let mut nonce: u32 = 0;
        loop {
            h[76..80].copy_from_slice(&nonce.to_le_bytes());
            if header_meets_target(&h) {
                return h;
            }
            nonce += 1;
        }
    }

    /// A single-tx deposit block + `extra` PoW successors on top.
    fn proof_with_chain(tx: &[u8], extra: usize) -> SpvProof {
        let leaf = dsha256(tx);
        let merkle_root = leaf; // single-tx block: root == leaf
        let mut headers = vec![mine([0u8; 32], merkle_root)];
        for _ in 0..extra {
            let prev = dsha256(headers.last().unwrap());
            headers.push(mine(prev, [7u8; 32]));
        }
        SpvProof { tx_bytes: tx.to_vec(), tx_hash: leaf, branch: vec![], tx_index: 0, headers }
    }

    #[test]
    fn nbits_decoding_is_sane() {
        // 0x1d00ffff (Bitcoin genesis bits) → top nonzero byte 0x00ffff at the right place.
        let t = target_from_nbits(0x1d00_ffff);
        // exponent 0x1d=29 → mantissa 0x00ffff lsb at index 34-29=5 → bytes 3,4,5 = 00,ff,ff
        assert_eq!(t[3], 0x00);
        assert_eq!(t[4], 0xff);
        assert_eq!(t[5], 0xff);
        assert_eq!(t[0], 0x00);
    }

    #[test]
    fn valid_chain_verifies_with_real_pow() {
        let proof = proof_with_chain(b"btc-deposit", 5); // 6 headers total
        assert!(proof.verify(6).is_ok());
        // every header actually meets its target
        assert!(proof.headers.iter().all(header_meets_target));
    }

    #[test]
    fn forged_pow_header_fails() {
        let mut proof = proof_with_chain(b"btc-deposit", 5);
        // wreck a header's nonce so it no longer meets target (with overwhelming prob).
        proof.headers[3][76..80].copy_from_slice(&[0xff; 4]);
        // also break nBits to a hard target so it definitely fails PoW
        proof.headers[3][72..76].copy_from_slice(&0x0300_0001u32.to_le_bytes());
        assert!(matches!(proof.verify(6), Err(ProofError::BadProofOfWork(3))));
    }

    #[test]
    fn broken_chain_link_fails() {
        let mut proof = proof_with_chain(b"btc-deposit", 5);
        proof.headers[2][4..36].copy_from_slice(&[9u8; 32]); // wrong prev_block
        // re-mine so it still meets PoW (so we hit the LINK error, not PoW error)
        let mr = { let mut m = [0u8; 32]; m.copy_from_slice(&proof.headers[2][36..68]); m };
        let prev = { let mut p = [0u8; 32]; p.copy_from_slice(&proof.headers[2][4..36]); p };
        proof.headers[2] = mine(prev, mr);
        assert!(matches!(proof.verify(6), Err(ProofError::BrokenChain(2))));
    }

    #[test]
    fn too_few_confirmations_fails() {
        let proof = proof_with_chain(b"btc-deposit", 2); // 3 headers
        assert_eq!(proof.verify(6), Err(ProofError::InsufficientConfirmations { got: 3, need: 6 }));
    }

    #[test]
    fn tampered_tx_fails() {
        let mut proof = proof_with_chain(b"btc-deposit", 5);
        proof.tx_bytes[0] ^= 1;
        assert_eq!(proof.verify(6), Err(ProofError::TxHashMismatch));
    }
}
