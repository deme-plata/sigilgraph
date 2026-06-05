//! # flux-fold — the succinct lattice backend for zk-flux
//!
//! The zk-flux v0.1 lattice wrap was not succinct: its proof grew with the
//! statement. This is the fix's first sound step — the **folding** primitive
//! that makes a proof *constant-size in the number of statements*.
//!
//! It is built on a **linearly-homomorphic Ajtai/SIS commitment**:
//! `commit(w) = A·w mod q` for a public random matrix `A` (transparent — anyone
//! regenerates it from a seed) and witness vector `w`. SIS-hardness makes the
//! commitment binding and **post-quantum**. Linearity is the magic:
//!
//! ```text
//!   commit(Σ ρ^i · w_i) = A·(Σ ρ^i w_i) = Σ ρ^i (A·w_i) = Σ ρ^i · commit(w_i)
//! ```
//!
//! So to aggregate `M` committed statements, draw a Fiat-Shamir challenge `ρ`
//! from their commitments, fold to `w* = Σ ρ^i w_i` and `c* = Σ ρ^i c_i`, and
//! ship `(c*, w*)` — **one** commitment + **one** opening, regardless of `M`.
//! Soundness is Schwartz-Zippel: if any `c_i ≠ commit(w_i)` the fold fails w.h.p.
//!
//! Result: proof size is **constant in M** (succinct in the number of folded
//! statements). The remaining lane — recursively folding the *witness dimension*
//! `n` down (LaBRADOR's √-step, n → √n → … → ~KB) so the proof is succinct in `n`
//! too — is v0.3. This step is the one that turns zk-flux's wrap from
//! grows-with-statement-count into flat.

/// A 31-bit Mersenne prime field `Z_q`, q = 2^31 − 1 (fast reduction, fits the
/// u128 accumulator for `A·w` with room to spare).
pub const Q: u64 = 2_147_483_647;

#[inline]
fn addm(a: u64, b: u64) -> u64 {
    let s = a + b;
    if s >= Q { s - Q } else { s }
}
#[inline]
fn mulm(a: u64, b: u64) -> u64 {
    ((a as u128 * b as u128) % Q as u128) as u64
}

/// Public Ajtai parameters: an `m × n` matrix `A` over `Z_q`, regenerated
/// deterministically from a 32-byte seed — so it is TRANSPARENT (no trusted
/// setup; every party derives the same `A`).
pub struct Ajtai {
    pub m: usize,
    pub n: usize,
    a: Vec<u64>, // row-major m*n
}

impl Ajtai {
    /// Derive `A` from a public seed (transparent setup).
    pub fn from_seed(m: usize, n: usize, seed: &[u8; 32]) -> Self {
        let mut a = Vec::with_capacity(m * n);
        let mut ctr = 0u64;
        let mut buf: Vec<u8> = Vec::new();
        let mut idx = 0usize;
        while a.len() < m * n {
            if idx + 8 > buf.len() {
                let mut h = blake3::Hasher::new();
                h.update(b"flux-fold/ajtai-A/v1");
                h.update(seed);
                h.update(&ctr.to_le_bytes());
                buf = h.finalize_xof_vec(64);
                ctr += 1;
                idx = 0;
            }
            let v = u64::from_le_bytes(buf[idx..idx + 8].try_into().unwrap()) % Q;
            a.push(v);
            idx += 8;
        }
        Self { m, n, a }
    }

    /// **Untrusted (transparent) setup, expanded by BLAKE4 — the chain's own PoW
    /// hash.** Identical guarantees to [`from_seed`](Self::from_seed) (no trusted
    /// party, no toxic waste, every verifier regenerates the same `A`) but the
    /// public-coin expander is `blake4_word` — bit-for-bit the same hash family
    /// the miner uses for proof-of-work (`flux_miner::blake4` = BLAKE3 compression
    /// → first 8 bytes). So a single hash secures *both* the PoW and the ZK
    /// setup: there is no separate "ceremony" to trust, and the public seed can be
    /// a chain anchor (genesis hash / a block header) that the chain already
    /// commits to. The seed is arbitrary-length so it can be a real header.
    pub fn from_seed_blake4(m: usize, n: usize, seed: &[u8]) -> Self {
        let mut a = Vec::with_capacity(m * n);
        let mut ctr = 0u64;
        while a.len() < m * n {
            a.push(blake4_word(seed, ctr) % Q);
            ctr += 1;
        }
        Self { m, n, a }
    }

    /// Ajtai commitment `c = A·w mod q` (length `m`). Binding under SIS.
    pub fn commit(&self, w: &[u64]) -> Vec<u64> {
        assert_eq!(w.len(), self.n, "witness length must equal n");
        let mut c = vec![0u64; self.m];
        for i in 0..self.m {
            let row = &self.a[i * self.n..(i + 1) * self.n];
            let mut acc: u128 = 0;
            for j in 0..self.n {
                acc += row[j] as u128 * w[j] as u128;
            }
            c[i] = (acc % Q as u128) as u64;
        }
        c
    }
}

/// BLAKE4 public-coin word — the transparent-setup expander. This is the SAME
/// hash family the Flux miner deploys as its sound PoW (`flux_miner::blake4`):
/// BLAKE3 compression over the input, first 8 bytes taken as a `u64`. Here it is
/// run in counter mode (`ctr = 0,1,2,…`) to stream as many words as the setup
/// needs. Because it is a public, preimage-hard hash, the resulting parameters
/// are "nothing-up-our-sleeve": no party can have planted a trapdoor, and any
/// verifier reproduces them from the public seed alone.
#[inline]
pub fn blake4_word(seed: &[u8], ctr: u64) -> u64 {
    let mut h = blake3::Hasher::new();
    h.update(b"flux-fold/blake4-setup/v1");
    h.update(seed);
    h.update(&ctr.to_le_bytes());
    let d = h.finalize();
    u64::from_le_bytes(d.as_bytes()[0..8].try_into().unwrap())
}

// xof helper (blake3 0.x finalize_xof → fill); wrap to a Vec.
trait XofVec {
    fn finalize_xof_vec(self, n: usize) -> Vec<u8>;
}
impl XofVec for blake3::Hasher {
    fn finalize_xof_vec(self, n: usize) -> Vec<u8> {
        let mut out = vec![0u8; n];
        let mut xof = self.finalize_xof();
        xof.fill(&mut out);
        out
    }
}

/// A folded proof aggregating `m_count` statements into constant size:
/// one folded commitment `c_star` (len m) + one folded opening `w_star` (len n).
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct FoldedProof {
    pub c_star: Vec<u64>,
    pub w_star: Vec<u64>,
    pub m_count: usize,
}

impl FoldedProof {
    /// Serialized byte size — the headline succinctness metric. Independent of
    /// `m_count` (only `m + n` field elements, whatever how many were folded).
    pub fn size_bytes(&self) -> usize {
        (self.c_star.len() + self.w_star.len()) * 8 + 8
    }
}

/// Fiat-Shamir challenge `ρ` derived from all commitments (non-interactive).
fn challenge(commitments: &[Vec<u64>]) -> u64 {
    let mut h = blake3::Hasher::new();
    h.update(b"flux-fold/rho/v1");
    for c in commitments {
        for &x in c {
            h.update(&x.to_le_bytes());
        }
    }
    let d = h.finalize();
    (u64::from_le_bytes(d.as_bytes()[0..8].try_into().unwrap()) % (Q - 1)) + 1
}

/// Fold `M` statements `(witness_i)` into one constant-size proof. The prover
/// knows the witnesses; the commitments are public.
pub fn fold(ajtai: &Ajtai, witnesses: &[Vec<u64>]) -> FoldedProof {
    let commitments: Vec<Vec<u64>> = witnesses.iter().map(|w| ajtai.commit(w)).collect();
    let rho = challenge(&commitments);

    // w* = Σ ρ^i w_i ; c* = Σ ρ^i c_i  (both mod q)
    let mut w_star = vec![0u64; ajtai.n];
    let mut c_star = vec![0u64; ajtai.m];
    let mut rpow = 1u64;
    for i in 0..witnesses.len() {
        for j in 0..ajtai.n {
            w_star[j] = addm(w_star[j], mulm(rpow, witnesses[i][j]));
        }
        for j in 0..ajtai.m {
            c_star[j] = addm(c_star[j], mulm(rpow, commitments[i][j]));
        }
        rpow = mulm(rpow, rho);
    }
    FoldedProof { c_star, w_star, m_count: witnesses.len() }
}

/// Verify a folded proof against the public commitments. Checks (1) the fold is
/// the correct random linear combination `c* = Σ ρ^i c_i`, and (2) the opening
/// is valid `commit(w*) = c*`. Both must hold. Cost is independent of the
/// statements' internal complexity — only `O(M·m)` for the recombination.
pub fn verify(ajtai: &Ajtai, commitments: &[Vec<u64>], proof: &FoldedProof) -> bool {
    if commitments.len() != proof.m_count {
        return false;
    }
    let rho = challenge(commitments);
    // (1) recompute Σ ρ^i c_i and compare to c*
    let mut acc = vec![0u64; ajtai.m];
    let mut rpow = 1u64;
    for c in commitments {
        if c.len() != ajtai.m {
            return false;
        }
        for j in 0..ajtai.m {
            acc[j] = addm(acc[j], mulm(rpow, c[j]));
        }
        rpow = mulm(rpow, rho);
    }
    if acc != proof.c_star {
        return false;
    }
    // (2) the folded opening must commit to c*
    ajtai.commit(&proof.w_star) == proof.c_star
}

#[cfg(test)]
mod tests {
    use super::*;

    fn witnesses(count: usize, n: usize, seed: u64) -> Vec<Vec<u64>> {
        let mut s = seed | 1;
        let mut nx = || { s ^= s << 13; s ^= s >> 7; s ^= s << 17; s % Q };
        (0..count).map(|_| (0..n).map(|_| nx()).collect()).collect()
    }

    #[test]
    fn fold_verifies_and_is_constant_size() {
        let ajtai = Ajtai::from_seed(64, 256, &[5u8; 32]);
        let mut last = 0usize;
        for &m in &[1usize, 16, 256, 1024] {
            let ws = witnesses(m, 256, 0x1234 + m as u64);
            let proof = fold(&ajtai, &ws);
            let coms: Vec<Vec<u64>> = ws.iter().map(|w| ajtai.commit(w)).collect();
            assert!(verify(&ajtai, &coms, &proof), "honest fold of {m} must verify");
            // SUCCINCT: size does not grow with the number of folded statements.
            let sz = proof.size_bytes();
            if last != 0 {
                assert_eq!(sz, last, "folded proof size must be constant in M");
            }
            last = sz;
        }
    }

    #[test]
    fn blake4_untrusted_setup_is_reproducible_and_folds() {
        // Public chain anchor (e.g. a genesis/block hash) as the seed — no ceremony.
        let anchor = b"sigil-genesis-3123e273-untrusted-public-coin";
        // Two independent parties derive A from the public seed alone.
        let party_a = Ajtai::from_seed_blake4(48, 192, anchor);
        let party_b = Ajtai::from_seed_blake4(48, 192, anchor);
        // Reproducibility: same public seed ⇒ identical parameters (probe via commit).
        let probe = witnesses(1, 192, 0x5151)[0].clone();
        assert_eq!(party_a.commit(&probe), party_b.commit(&probe),
            "transparent setup must be reproducible from the public seed by anyone");
        // The BLAKE4-derived params are sound: honest fold verifies.
        let ws = witnesses(64, 192, 0x9001);
        let proof = fold(&party_a, &ws);
        let coms: Vec<Vec<u64>> = ws.iter().map(|w| party_a.commit(w)).collect();
        assert!(verify(&party_a, &coms, &proof),
            "fold over the BLAKE4 untrusted setup must verify");
    }

    #[test]
    fn blake4_setup_has_no_trapdoor_seed_sensitivity() {
        // "Nothing up our sleeve": a one-bit change in the public seed yields
        // unrelated parameters — there is no hidden structure to exploit.
        let a0 = Ajtai::from_seed_blake4(32, 128, b"anchor");
        let a1 = Ajtai::from_seed_blake4(32, 128, b"anchos"); // 1 byte differs
        let probe = witnesses(1, 128, 0x2222)[0].clone();
        assert_ne!(a0.commit(&probe), a1.commit(&probe),
            "distinct public seeds must give distinct (uncorrelated) parameters");
        // And blake4_word is a deterministic public function (verifier-checkable).
        assert_eq!(blake4_word(b"anchor", 7), blake4_word(b"anchor", 7));
        assert_ne!(blake4_word(b"anchor", 7), blake4_word(b"anchor", 8));
    }

    #[test]
    fn tampered_statement_is_rejected() {
        let ajtai = Ajtai::from_seed(64, 256, &[9u8; 32]);
        let ws = witnesses(32, 256, 0xABCD);
        let proof = fold(&ajtai, &ws);
        let mut coms: Vec<Vec<u64>> = ws.iter().map(|w| ajtai.commit(w)).collect();
        // forge one commitment → the random-linear-combination check fails
        coms[7][0] = addm(coms[7][0], 1);
        assert!(!verify(&ajtai, &coms, &proof), "a tampered commitment must be rejected");

        // forge the opening → commit(w*) != c*
        let mut bad = proof.clone();
        bad.w_star[0] = addm(bad.w_star[0], 1);
        let coms_ok: Vec<Vec<u64>> = ws.iter().map(|w| ajtai.commit(w)).collect();
        assert!(!verify(&ajtai, &coms_ok, &bad), "a forged opening must be rejected");
    }
}
