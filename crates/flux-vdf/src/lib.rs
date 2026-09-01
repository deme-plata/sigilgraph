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
    /// Domain check: is `e` a valid, non-degenerate group element? A Wesolowski
    /// `verify` MUST reject inputs outside the group, because `decode` is total
    /// over arbitrary bytes. Without it, an attacker's `decode(empty) == 0` makes
    /// `0^l · x^r == 0 == y` verify a proof that did ZERO sequential work — a full
    /// VDF bypass reachable from any network block. See `verify`.
    fn is_valid_element(&self, e: &Self::Elem) -> bool;
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
    // Domain check BEFORE the equation: `decode` is total over arbitrary bytes,
    // so a malformed proof can hand us degenerate elements. The sharp case is the
    // all-zero proof — decode(empty)==0 and 0^l·x^r == 0 == y would otherwise
    // verify zero work. Reject anything outside the group. (Regression:
    // malformed_or_degenerate_proof_is_rejected_never_panics.)
    if !g.is_valid_element(&y) || !g.is_valid_element(&pi) {
        return false;
    }
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

    /// **Production VDF modulus (audit C10).** The RSA-2048 Factoring Challenge
    /// number — a 2048-bit semiprime whose factorization is unknown to *anyone*
    /// (the RSA-2048 challenge was never solved). Wesolowski's security needs a
    /// group of UNKNOWN order; `bench_2048` is a published bit-pattern whose order
    /// is computable, so its time-lane is forgeable (instant-mine / issuance
    /// capture). This constant has no factorization any party holds — a
    /// "nothing-up-my-sleeve" modulus with no trusted setup we run. The genus-2
    /// class group (`genus2.rs`) is the eventual no-RSA replacement; the protocol
    /// is identical, so this is a drop-in.
    pub fn rsa2048() -> Self {
        // RSA-2048 (decimal), RSA Laboratories Factoring Challenge.
        const RSA2048_DEC: &[u8] = b"25195908475657893494027183240048398571429282126204\
            03202777713783604366202070759555626401852588078440691829064124951508218929855914917618450280\
            84891200728449926873928072877767359714183472702618963750149718246911650776133798590957000973\
            30459748808428401797429100642458691817195118746121515172654632282216869987549182422433637259\
            08514186546204357679842338718477444792073993423658482382428119816381501067481045166037730605\
            62016196762561338441436038339044149526344321901146575444541784240209246165157233507787077498\
            171257724679629263863563732899121548314381678998850404453640235273819513786365643912120103971\
            22822120720357";
        // strip the leading-whitespace from the multiline literal
        let digits: Vec<u8> = RSA2048_DEC.iter().copied().filter(|b| b.is_ascii_digit()).collect();
        let n = BigUint::parse_bytes(&digits, 10).expect("RSA-2048 decimal constant must parse");
        Self { n }
    }

    /// The VDF group the production consensus path MUST use. Single switch point
    /// for node + every miner (they must share one group). Currently `rsa2048()`.
    pub fn production() -> Self {
        Self::rsa2048()
    }

    /// **Fail-closed security self-check on the modulus (audit C10 / SENTINEL #1).**
    /// A secure squaring-VDF needs `N` to be a large composite of *unknown* order:
    /// a prime `N` has known order `N-1` (forgeable); a smooth / small-factored `N`
    /// is factorable (order computable → forgeable). This rejects any modulus that
    /// is too small, even, prime, or has a *findable* factor (trial division +
    /// bounded Pollard-rho). Consequence: a mis-transcribed constant is either
    /// still a hard composite (secure) or trips this and the node refuses to boot —
    /// it can never *silently* weaken the time-lane. Callers should run this at
    /// startup and `expect()` it on the production group.
    pub fn assert_secure(&self) -> Result<(), String> {
        let bits = self.n.bits();
        if bits < 2048 {
            return Err(format!("VDF modulus too small: {bits} bits (need >= 2048)"));
        }
        if self.n.is_even() {
            return Err("VDF modulus is even (factor 2 → order known)".into());
        }
        // Must be COMPOSITE: a prime modulus has the fully-known order N-1.
        if is_probable_prime(&self.n, 40) {
            return Err("VDF modulus is prime — group order N-1 is known → VDF forgeable".into());
        }
        // No small factors: a smooth / unbalanced N is trivially factorable.
        let mut p: u32 = 3;
        while p < (1u32 << 16) {
            if (&self.n % p).is_zero() {
                return Err(format!("VDF modulus divisible by {p} → factorable, order computable"));
            }
            p += 2;
        }
        // Bounded Pollard-rho: a balanced ~2048-bit semiprime must NOT cough up a
        // factor in this budget. If it does, the modulus is weak — refuse it.
        if let Some(f) = pollard_rho_bounded(&self.n, 300_000) {
            return Err(format!(
                "VDF modulus factor found by Pollard-rho ({} bits) → weak group, refusing",
                f.bits()
            ));
        }
        Ok(())
    }
}

// ── C10: height-gated consensus VDF group selection ─────────────────────────
/// Block height at/above which the consensus VDF group switches from the
/// forgeable `bench_2048` benchmark modulus to the trustless [`ModSquaring::production`]
/// (RSA-2048, unknown order). **DORMANT by default (`u64::MAX`)** — historical
/// blocks were mined against `bench_2048`, so their proofs only verify under it;
/// a live cutover therefore needs an operator to pin a real activation height on
/// the node AND ship it to every miner (a coordinated upgrade — a miner still on
/// `bench_2048` past the boundary produces shares the node rejects, and vice
/// versa). Override for tests/staging via `SIGIL_VDF_ACTIVATION_HEIGHT`.
pub const VDF_PRODUCTION_ACTIVATION_HEIGHT: u64 = u64::MAX;

/// The active VDF cutover height: `SIGIL_VDF_ACTIVATION_HEIGHT` if set and
/// parseable, else [`VDF_PRODUCTION_ACTIVATION_HEIGHT`] (dormant).
pub fn vdf_activation_height() -> u64 {
    std::env::var("SIGIL_VDF_ACTIVATION_HEIGHT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(VDF_PRODUCTION_ACTIVATION_HEIGHT)
}

/// Pure height→group selector against an explicit `activation` height. Below
/// `activation` → `bench_2048`; at/above → `production`. Extracted so the choice
/// is unit-testable without touching process env.
pub fn group_for_height_with(height: u64, activation: u64) -> ModSquaring {
    if height >= activation {
        ModSquaring::production()
    } else {
        ModSquaring::bench_2048()
    }
}

/// The consensus VDF group for a block at `height` — the SINGLE switch point the
/// node and every miner MUST share. Reads the live activation height (dormant by
/// default), so it returns `bench_2048` unchanged until an operator flips the
/// cutover. Callers should key on the BLOCK's height (producer, followers, and
/// miners all agree on that), never on wall-clock.
pub fn group_for_height(height: u64) -> ModSquaring {
    group_for_height_with(height, vdf_activation_height())
}

/// Bounded Pollard-rho factor search. Returns a non-trivial factor of `n` if one
/// is found within `max_iters` (signals a *weak* modulus for VDF use), else None.
/// Brent's cycle variant; deterministic (fixed seed) so the check is reproducible.
fn pollard_rho_bounded(n: &BigUint, max_iters: u64) -> Option<BigUint> {
    if n.is_even() {
        return Some(BigUint::from(2u32));
    }
    let one = BigUint::one();
    let mut x = BigUint::from(2u32);
    let mut y = BigUint::from(2u32);
    let c = BigUint::from(1u32);
    let two = BigUint::from(2u32);
    let f = |v: &BigUint| -> BigUint { (v.modpow(&two, n) + &c) % n };
    for _ in 0..max_iters {
        x = f(&x);
        y = f(&f(&y));
        let d = if x >= y { &x - &y } else { &y - &x };
        if d.is_zero() {
            return None; // cycle closed with no factor in this budget
        }
        let g = d.gcd(n);
        if g > one && &g < n {
            return Some(g);
        }
    }
    None
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
    fn is_valid_element(&self, e: &BigUint) -> bool {
        // A squaring-group element lives in [1, n-1]. Reject 0 (the degenerate
        // all-zero-proof bypass) and any non-canonical value ≥ n. `from_seed`
        // already maps into [2, n-1], so every honest y/pi passes.
        !e.is_zero() && *e < self.n
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

    /// Structurally MALFORMED proofs (wrong-LENGTH y/pi, not just wrong-value)
    /// must be rejected AND never panic. This is the remote-crash / VDF-bypass
    /// class: a peer's block carries an attacker-chosen VdfProof, and `verify`
    /// decodes y/pi from raw bytes. The degenerate all-zero shape is the sharp
    /// one: decode(empty) == 0, and 0^l · x^r == 0 == y would verify a proof
    /// that did ZERO sequential work — so `verify` must reject non-group / zero
    /// elements, not just fail the equation by luck.
    #[test]
    fn malformed_or_degenerate_proof_is_rejected_never_panics() {
        let g = ModSquaring::bench_2048();
        let x = g.from_seed(&[3u8; 32]);
        for (label, y, pi) in [
            ("both empty (both decode to 0)", vec![], vec![]),
            ("both single zero byte", vec![0u8], vec![0u8]),
            ("y zero, pi tiny", vec![], vec![7u8]),
            ("absurdly oversized", vec![7u8; 10_000], vec![9u8; 10_000]),
        ] {
            let bad = VdfProof { y, pi, t: 3_000 };
            assert!(!verify(&g, &x, &bad), "malformed/degenerate proof accepted: {label}");
        }
    }

    #[test]
    fn production_modulus_passes_security_self_check() {
        // C10: the production VDF group must be a 2048-bit composite of unknown
        // order — bench_2048 is forbidden in production precisely because its
        // structure is public.
        let g = ModSquaring::production();
        assert!(g.n.bits() >= 2048, "production modulus must be >= 2048 bits");
        g.assert_secure().expect("RSA-2048 production modulus must pass the self-check");
    }

    #[test]
    fn wesolowski_roundtrip_over_production_group() {
        // The protocol is identical to bench_2048 — only N changed.
        let g = ModSquaring::production();
        let x = g.from_seed(&[3u8; 32]);
        let proof = eval(&g, &x, 4_000);
        assert!(verify(&g, &x, &proof), "honest VDF proof over RSA-2048 must verify");
    }

    #[test]
    fn height_gate_selects_bench_below_and_production_at_or_above() {
        // Dormant default: every height stays on bench_2048.
        assert_eq!(group_for_height_with(0, u64::MAX).n, ModSquaring::bench_2048().n);
        assert_eq!(group_for_height_with(u64::MAX - 1, u64::MAX).n, ModSquaring::bench_2048().n);
        // With a real activation at H: below → bench, at/above → production.
        let h = 1_000u64;
        assert_eq!(group_for_height_with(h - 1, h).n, ModSquaring::bench_2048().n);
        assert_eq!(group_for_height_with(h, h).n, ModSquaring::production().n);
        assert_eq!(group_for_height_with(h + 1, h).n, ModSquaring::production().n);
    }

    #[test]
    fn bench_forged_proof_is_rejected_under_production_group() {
        // C10 core assertion: bench_2048's order is computable, so a proof made
        // under it is worthless the moment the network moves to the production
        // group — verifying a bench proof against production() must FAIL. This is
        // exactly what the height gate enforces at the cutover boundary.
        let bench = ModSquaring::bench_2048();
        let prod = ModSquaring::production();
        let x_b = bench.from_seed(&[7u8; 32]);
        let proof = eval(&bench, &x_b, 2_000);
        assert!(verify(&bench, &x_b, &proof), "honest bench proof verifies under bench");
        // The same proof re-checked against the production group is rejected
        // (different modulus → the reduced element does not match).
        let x_p = prod.from_seed(&[7u8; 32]);
        assert!(!verify(&prod, &x_p, &proof), "a bench-group proof must not verify under production");
    }

    #[test]
    fn self_check_rejects_insecure_moduli() {
        // even → factor 2 known
        let even = ModSquaring::new((BigUint::one() << 2048) | (BigUint::one() << 4));
        assert!(even.assert_secure().is_err(), "even modulus must be rejected");
        // too small
        let small = ModSquaring::new((BigUint::one() << 1024) | BigUint::one());
        assert!(small.assert_secure().is_err(), "sub-2048-bit modulus must be rejected");
        // a small odd factor (3 | N) → trivially factorable
        let smooth = ModSquaring::new(BigUint::from(3u32) * ((BigUint::one() << 2047) | BigUint::one()));
        assert!(smooth.assert_secure().is_err(), "modulus with small factor must be rejected");
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
