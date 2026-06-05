//! # flux-vdf — the Wesolowski Verifiable Delay Function
//!
//! A VDF is *proof of elapsed TIME*: computing `y = x^(2^t)` takes `t`
//! **sequential** squarings (each depends on the last, so no parallelism helps),
//! while a Wesolowski proof lets anyone **verify** it in `O(1)` group operations.
//! This is the TIME lane (Ω) of the dual-lane Flux miner — the egalitarian,
//! ASIC-resistant counterpart to the parallel BLAKE4 power lane (Φ).
//!
//! The protocol is implemented over a [`VdfGroup`] trait so the group is
//! swappable:
//! * [`ModSquaring`] — repeated modular squaring, the working group (tested).
//! * `genus2::GenusTwoJacobian` — a genus-2 hyperelliptic Jacobian, the
//!   ASIC-hardest no-trusted-setup group; structured + documented, pending
//!   reference-vector validation before production (see [`genus2`]).
//!
//! Soundness relies on the group having *unknown order* (you cannot reduce the
//! exponent `2^t` mod the group order to shortcut the work). `ModSquaring` gets
//! this from an RSA-style modulus; genus-2 gets it with **no trusted setup**.

use num_bigint::BigUint;
use num_integer::Integer;
use num_traits::{One, Zero};

pub mod genus2;

/// A finite group of (believed) unknown order, with a hash-to-group and a
/// canonical encoding. The VDF squares within it `t` times.
pub trait VdfGroup {
    type Elem: Clone + PartialEq + std::fmt::Debug;
    fn identity(&self) -> Self::Elem;
    fn mul(&self, a: &Self::Elem, b: &Self::Elem) -> Self::Elem;
    fn square(&self, a: &Self::Elem) -> Self::Elem { self.mul(a, a) }
    fn from_seed(&self, seed: &[u8; 32]) -> Self::Elem;
    fn encode(&self, a: &Self::Elem) -> Vec<u8>;
    fn decode(&self, bytes: &[u8]) -> Self::Elem;
    /// Square-and-multiply exponentiation (used only in *verify*, never in the
    /// sequential eval — verify is allowed to be fast).
    fn exp(&self, base: &Self::Elem, e: &BigUint) -> Self::Elem {
        let mut result = self.identity();
        let mut b = base.clone();
        let mut ee = e.clone();
        while !ee.is_zero() {
            if ee.is_odd() {
                result = self.mul(&result, &b);
            }
            b = self.square(&b);
            ee >>= 1;
        }
        result
    }
}

/// A VDF evaluation: the output `y = x^(2^t)` and the Wesolowski proof `pi`,
/// both group-encoded, plus the difficulty `t` (number of sequential squarings).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VdfProof {
    pub y: Vec<u8>,
    pub pi: Vec<u8>,
    pub t: u64,
}

/// Fiat-Shamir challenge prime `l = H(x, y, t)` (a ~128-bit prime). Both prover
/// and verifier derive it identically, so the proof is non-interactive.
fn challenge_prime<G: VdfGroup>(g: &G, x: &G::Elem, y: &G::Elem, t: u64) -> BigUint {
    let mut h = blake3::Hasher::new();
    h.update(b"flux-vdf/wesolowski/challenge/v1");
    h.update(&g.encode(x));
    h.update(&g.encode(y));
    h.update(&t.to_le_bytes());
    let d = h.finalize();
    let mut cand = BigUint::from_bytes_le(&d.as_bytes()[..16]);
    cand |= BigUint::one(); // odd
    cand |= BigUint::one() << 127; // ~128-bit
    while !is_probable_prime(&cand, 12) {
        cand += 2u32;
    }
    cand
}

/// Evaluate the VDF: `t` sequential squarings to get `y = x^(2^t)`, then the
/// Wesolowski proof `pi = x^(floor(2^t / l))` via the long-division trick
/// (a second pass of `t` squarings — proving is ~2x eval, still sequential).
pub fn eval<G: VdfGroup>(g: &G, x: &G::Elem, t: u64) -> VdfProof {
    // --- the delay: t sequential squarings ---
    let mut y = x.clone();
    for _ in 0..t {
        y = g.square(&y);
    }
    let l = challenge_prime(g, x, &y, t);

    // --- the proof: pi = x^q where q = floor(2^t / l), computed in t squarings ---
    let two = BigUint::from(2u32);
    let mut pi = g.identity();
    let mut r = BigUint::one(); // running 2^i mod l
    for _ in 0..t {
        let rr = &r * &two;
        let bit = (&rr / &l).is_one(); // quotient digit (0 or 1)
        r = rr % &l;
        pi = g.square(&pi);
        if bit {
            pi = g.mul(&pi, x);
        }
    }
    VdfProof { y: g.encode(&y), pi: g.encode(&pi), t }
}

/// Verify a VDF proof in `O(1)` sequential work (two group exponentiations and a
/// `2^t mod l`), independent of how many turns `t` the prover actually did.
/// Checks `pi^l * x^r == y` with `r = 2^t mod l`.
pub fn verify<G: VdfGroup>(g: &G, x: &G::Elem, proof: &VdfProof) -> bool {
    let y = g.decode(&proof.y);
    let pi = g.decode(&proof.pi);
    let l = challenge_prime(g, x, &y, proof.t);
    let r = BigUint::from(2u32).modpow(&BigUint::from(proof.t), &l); // 2^t mod l, fast
    let lhs = g.mul(&g.exp(&pi, &l), &g.exp(x, &r));
    lhs == y
}

/// Deterministic Miller–Rabin with small fixed bases — ample for deriving a
/// ~128-bit challenge prime (the standard Wesolowski construction).
fn is_probable_prime(n: &BigUint, _rounds: u32) -> bool {
    let two = BigUint::from(2u32);
    if n < &two {
        return false;
    }
    for p in [2u32, 3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37] {
        let bp = BigUint::from(p);
        if n == &bp {
            return true;
        }
        if (n % &bp).is_zero() {
            return false;
        }
    }
    let one = BigUint::one();
    let n1 = n - &one;
    let mut d = n1.clone();
    let mut s = 0u32;
    while d.is_even() {
        d >>= 1;
        s += 1;
    }
    for a in [2u64, 3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37] {
        let mut x = BigUint::from(a).modpow(&d, n);
        if x == one || x == n1 {
            continue;
        }
        let mut composite = true;
        for _ in 0..s.saturating_sub(1) {
            x = x.modpow(&two, n);
            if x == n1 {
                composite = false;
                break;
            }
        }
        if composite {
            return false;
        }
    }
    true
}

// ── ModSquaring — the working group: y = x^(2^t) mod N ──────────────────────

/// Repeated modular squaring over a fixed modulus `N` of unknown factorization.
/// The deployable-today VDF group (RSA-style / class-group character).
pub struct ModSquaring {
    pub n: BigUint,
}

impl ModSquaring {
    pub fn new(n: BigUint) -> Self {
        Self { n }
    }
    /// A fixed ~2048-bit modulus for benches/tests. NOT a secure RSA modulus
    /// (factorization not hidden by a ceremony) — production uses a class group
    /// or genus-2 Jacobian with no trusted setup. The *sequential rate* and the
    /// protocol are identical regardless.
    pub fn bench_2048() -> Self {
        let mut n = (BigUint::one() << 2047) | BigUint::one();
        n |= BigUint::from(0x9e3779b97f4a7c15u64) << 900;
        n |= BigUint::from(0xbf58476d1ce4e5b9u64) << 1500;
        Self { n }
    }
}

impl VdfGroup for ModSquaring {
    type Elem = BigUint;
    fn identity(&self) -> BigUint {
        BigUint::one()
    }
    fn mul(&self, a: &BigUint, b: &BigUint) -> BigUint {
        (a * b) % &self.n
    }
    fn from_seed(&self, seed: &[u8; 32]) -> BigUint {
        let mut h = blake3::Hasher::new();
        h.update(b"flux-vdf/modsq/elem/v1");
        h.update(seed);
        let bytes = h.finalize();
        // expand to ~modulus width
        let mut wide = Vec::new();
        let mut ctr = 0u32;
        while wide.len() < (self.n.bits() as usize / 8) + 8 {
            let mut h2 = blake3::Hasher::new();
            h2.update(bytes.as_bytes());
            h2.update(&ctr.to_le_bytes());
            wide.extend_from_slice(h2.finalize().as_bytes());
            ctr += 1;
        }
        let v = BigUint::from_bytes_le(&wide) % (&self.n - 2u32) + 2u32;
        v
    }
    fn encode(&self, a: &BigUint) -> Vec<u8> {
        a.to_bytes_le()
    }
    fn decode(&self, bytes: &[u8]) -> BigUint {
        BigUint::from_bytes_le(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wesolowski_roundtrip_modsquaring() {
        let g = ModSquaring::bench_2048();
        let x = g.from_seed(&[7u8; 32]);
        let t = 5_000u64;
        let proof = eval(&g, &x, t);
        assert_eq!(proof.t, t);
        assert!(verify(&g, &x, &proof), "honest VDF proof must verify");
    }

    #[test]
    fn tampered_output_is_rejected() {
        let g = ModSquaring::bench_2048();
        let x = g.from_seed(&[9u8; 32]);
        let proof = eval(&g, &x, 3_000);

        // flip a byte of y → proof must fail
        let mut bad = proof.clone();
        bad.y[0] ^= 0x01;
        assert!(!verify(&g, &x, &bad), "tampered y must be rejected");

        // claim a different t → must fail
        let mut bad_t = proof.clone();
        bad_t.t += 1;
        assert!(!verify(&g, &x, &bad_t), "wrong t must be rejected");

        // a forged pi → must fail
        let mut bad_pi = proof;
        bad_pi.pi[0] ^= 0x02;
        assert!(!verify(&g, &x, &bad_pi), "forged proof must be rejected");
    }

    #[test]
    fn challenge_prime_is_prime_and_deterministic() {
        let g = ModSquaring::bench_2048();
        let x = g.from_seed(&[1u8; 32]);
        let y = g.square(&x);
        let l1 = challenge_prime(&g, &x, &y, 100);
        let l2 = challenge_prime(&g, &x, &y, 100);
        assert_eq!(l1, l2, "Fiat-Shamir challenge must be deterministic");
        assert!(is_probable_prime(&l1, 12));
    }
}
