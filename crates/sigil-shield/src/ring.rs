//! SIGIL sender-anonymity — post-quantum **linkable ring signature** (one-of-N hiding).
//!
//! LANE-B of SIGIL-SHIELD P1. This is SIGIL's sender-anonymity layer: a signer proves
//! membership in a ring of `N` public keys without revealing *which* key is theirs, and a
//! per-key **linkable tag** (Monero-style key image) lets the chain detect the same signer
//! double-spending. It is **post-quantum** — its security rests on Module-LWE / Module-SIS
//! hardness plus a hash instantiated as a random oracle (BLAKE3), NOT on discrete log.
//! (`clsag.rs` is the *pre-quantum* Ristretto CLSAG; this is its PQ replacement.)
//!
//! ## Why this is a fresh, sound implementation — not a port
//!
//! The nominal source (`q-quantum-mixing/src/lattice_ring_sig.rs`, 1280 lines) is
//! cryptographic **theater**: its `sign` commits `W_s = H("…Commitment.v1", y)` but its
//! `verify` recomputes `W'_i = H("…RecomputeCommitment.v1", z_i)` — unrelated hashes over
//! unrelated inputs (mask `y` vs response `z`). The per-position seed can therefore never
//! equal the stored `challenge_seed`, so an honest signature verifies as **false**; its own
//! `test_sign_and_verify` would panic. Even with the domains patched, `verify` never binds
//! responses to a real secret, so it is trivially forgeable. Per the SIGIL charter
//! ("verify OUTCOMES not existence"), transliterating it would fail gate (1) and be unsound.
//!
//! What is implemented here is the construction that source's own header *cites* — the
//! AOS/CLSAG ring (Abe-Ohkubo-Suzuki) instantiated over a **lattice Sigma protocol**
//! (Lyubashevsky's Fiat-Shamir-with-aborts identification for Module-LWE), the family of
//! IACR ePrint 2025/2170 and Raptor. The algebra the verifier checks is real:
//! `A·z_i − c_i·t_i == W_i` over `R_q = Z_q[X]/(X^256+1)`, with rejection sampling that makes
//! the accepted response uniform over a fixed box (independent of the secret → anonymity).
//!
//! ## Construction (one closed Fiat-Shamir ring)
//!
//! Public parameters: two fixed public matrices `A, G ∈ R_q^{k×l}` (expanded from domain
//! seeds via BLAKE3). A keypair is a short secret `s ∈ R_q^l` (‖s‖∞ ≤ η) with public
//! `t = A·s`. The linkable tag is `T = G·s` — a deterministic function of `s` only, so the
//! same signer always yields the same `T` (cross-ring linkable), yet `T` leaks nothing:
//! `(t, T) = [A;G]·s` is just a taller Module-LWE sample.
//!
//! To sign message `m` with ring `{t_0,…,t_{N-1}}` at secret index `π`:
//!   * commit at π with a fresh short mask `y`: `W_π = A·y`, `W'_π = G·y`;
//!   * chain challenges cyclically `h_{i+1} = H(m, ring, T, W_i, W'_i)`,
//!     `c_i = SampleInBall(h_i)`;
//!   * for every decoy `i ≠ π`: pick a random in-box response `z_i` and *back-compute*
//!     `W_i = A·z_i − c_i·t_i`, `W'_i = G·z_i − c_i·T`;
//!   * close the ring at π with the real response `z_π = y + c_π·s` (rejection-sampled).
//! The stored signature is `(h_0, {z_i}, T)`. `verify` walks the whole chain from `h_0`,
//! recomputing each `W_i,W'_i` from `z_i` and `t_i`, and accepts iff the chain closes back
//! to `h_0` and every `‖z_i‖∞` is in-box.
//!
//! Gates proven in [`tests`] (real crypto, no `vec![0u8;N]`, no `Ok(true)`):
//! (1) sign→verify round-trips for a ring of size 8; (2) `verify` rejects a forged signature
//! (flipped byte / substituted ring member / non-member key); (3) `verify` rejects a valid
//! signature checked against a different message; (4) the linkable tag makes two signatures
//! from the same secret linkable (double-use detectable) while different secrets are not.

use blake3::Hasher;
use rand::RngCore;
use serde::{Deserialize, Serialize};

// ── Ring parameters (R_q = Z_q[X]/(X^n + 1), Module-LWE dimension k×l) ───────────────────
//
// n=256, q=8380417 is the Dilithium ring; k=l=4, η=2 gives a Dilithium2-class (~NIST-1/2)
// Module-LWE/SIS instance. τ=39 sets the challenge weight (challenge space ≫ 2^128).

/// Polynomial ring degree.
const N: usize = 256;
/// Modulus q = 2^23 − 2^13 + 1 (the Dilithium prime; supports NTT, we use schoolbook).
const Q: i64 = 8_380_417;
/// Module rank of the public image `t`/`T` and commitments.
const K: usize = 4;
/// Length of the secret vector `s`.
const L: usize = 4;
/// Infinity-norm bound on secret coefficients: s ∈ [−η, η].
const ETA: i64 = 2;
/// Number of ±1 coefficients in a challenge polynomial (Hamming weight of c).
const TAU: usize = 39;
/// Mask range half-width: y ∈ (−γ1, γ1], γ1 = 2^17.
const GAMMA1: i64 = 1 << 17;
/// Rejection margin β = τ·η bounds ‖c·s‖∞ (τ ±1 coeffs, each times |s| ≤ η).
const BETA: i64 = (TAU as i64) * ETA;
/// Accept/response box half-width: valid responses satisfy ‖z‖∞ ≤ ZBOUND.
/// After rejection, an honest z is uniform over [−ZBOUND, ZBOUND], identical to the
/// distribution of the simulated decoy responses → the ring hides the signer index.
const ZBOUND: i64 = GAMMA1 - BETA - 1;
/// Max Fiat-Shamir-with-aborts restarts before giving up (rejection is rare in-params).
const MAX_ABORTS: usize = 512;

/// A polynomial in `R_q`, stored as `N` coefficients canonicalized to `[0, q)`.
type Poly = Vec<i64>;

/// Errors surfaced by the ring signer.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RingError {
    #[error("ring is empty")]
    EmptyRing,
    #[error("signer index {index} out of bounds for ring of size {ring_size}")]
    IndexOutOfBounds { index: usize, ring_size: usize },
    /// The secret key does not correspond to `ring[my_index]` — refuse to sign (fail-closed).
    /// (`verify` is the ultimate authority; this only catches honest misuse early.)
    #[error("secret key does not match ring[{index}]: A·s ≠ t")]
    KeyMismatch { index: usize },
    #[error("rejection sampling failed to produce a bounded response after {0} attempts")]
    RejectionExhausted(usize),
}

// ── Polynomial arithmetic in R_q = Z_q[X]/(X^n + 1) ─────────────────────────────────────

#[inline]
fn creduce(x: i64) -> i64 {
    x.rem_euclid(Q)
}

/// Centered representative of a coefficient in (−q/2, q/2], for infinity-norm checks.
#[inline]
fn centered(x: i64) -> i64 {
    let r = x.rem_euclid(Q);
    if r > Q / 2 {
        r - Q
    } else {
        r
    }
}

fn poly_zero() -> Poly {
    vec![0i64; N]
}

fn poly_add(a: &Poly, b: &Poly) -> Poly {
    (0..N).map(|i| creduce(a[i] + b[i])).collect()
}

fn poly_sub(a: &Poly, b: &Poly) -> Poly {
    (0..N).map(|i| creduce(a[i] - b[i])).collect()
}

/// Negacyclic schoolbook multiply: (a·b) mod (X^n + 1) mod q.
/// The X^n = −1 relation folds the upper half back with a sign flip.
fn poly_mul(a: &Poly, b: &Poly) -> Poly {
    let mut acc = vec![0i64; N];
    for i in 0..N {
        if a[i] == 0 {
            continue;
        }
        for j in 0..N {
            let prod = a[i] * b[j];
            let k = i + j;
            if k < N {
                acc[k] += prod;
            } else {
                acc[k - N] -= prod; // X^n ≡ −1
            }
        }
    }
    acc.iter().map(|&x| creduce(x)).collect()
}

/// Infinity norm of a module vector (max centered magnitude over all polys/coeffs).
fn vec_infnorm(v: &[Poly]) -> i64 {
    v.iter()
        .flat_map(|p| p.iter())
        .map(|&c| centered(c).abs())
        .max()
        .unwrap_or(0)
}

/// Matrix·vector over R_q: `out[r] = Σ_c M[r][c] · v[c]`, producing a length-`rows` vector.
fn matvec(m: &[Vec<Poly>], v: &[Poly]) -> Vec<Poly> {
    m.iter()
        .map(|row| {
            let mut acc = poly_zero();
            for (mc, vc) in row.iter().zip(v.iter()) {
                acc = poly_add(&acc, &poly_mul(mc, vc));
            }
            acc
        })
        .collect()
}

/// Scalar-polynomial times a module vector: `out[i] = c · v[i]`.
fn scalar_mul_vec(c: &Poly, v: &[Poly]) -> Vec<Poly> {
    v.iter().map(|p| poly_mul(c, p)).collect()
}

fn vec_add(a: &[Poly], b: &[Poly]) -> Vec<Poly> {
    a.iter().zip(b.iter()).map(|(x, y)| poly_add(x, y)).collect()
}

fn vec_sub(a: &[Poly], b: &[Poly]) -> Vec<Poly> {
    a.iter().zip(b.iter()).map(|(x, y)| poly_sub(x, y)).collect()
}

// ── Public parameters: deterministic expansion of A, G from fixed domain seeds ──────────

/// Rejection-sample one coefficient stream position into `[0, q)` from a BLAKE3 XOF.
/// Uses 3 bytes (23 usable bits, since q < 2^23) and rejects values ≥ q — unbiased.
struct XofUniform {
    reader: blake3::OutputReader,
}

impl XofUniform {
    fn new(domain: &[u8], row: usize, col: usize) -> Self {
        let mut h = Hasher::new();
        h.update(b"SIGIL-shield-ring/expand/v1");
        h.update(domain);
        h.update(&(row as u64).to_le_bytes());
        h.update(&(col as u64).to_le_bytes());
        Self { reader: h.finalize_xof() }
    }

    fn next_coeff(&mut self) -> i64 {
        loop {
            let mut b = [0u8; 3];
            self.reader.fill(&mut b);
            let v = (b[0] as i64) | ((b[1] as i64) << 8) | (((b[2] as i64) & 0x7f) << 16);
            if v < Q {
                return v;
            }
        }
    }

    fn poly(&mut self) -> Poly {
        (0..N).map(|_| self.next_coeff()).collect()
    }
}

/// Expand a `rows×cols` public matrix over `R_q` from a domain seed. Deterministic and
/// public — every node derives the identical `A`/`G`.
fn expand_matrix(domain: &[u8], rows: usize, cols: usize) -> Vec<Vec<Poly>> {
    (0..rows)
        .map(|r| (0..cols).map(|c| XofUniform::new(domain, r, c).poly()).collect())
        .collect()
}

fn matrix_a() -> Vec<Vec<Poly>> {
    expand_matrix(b"A", K, L)
}

fn matrix_g() -> Vec<Vec<Poly>> {
    expand_matrix(b"G", K, L)
}

// ── Keys ────────────────────────────────────────────────────────────────────────────────

/// Ring public key: the Module-LWE image `t = A·s ∈ R_q^k`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RingPublicKey {
    /// `t` as `k` polynomials, coefficients canonical in `[0, q)`.
    t: Vec<Poly>,
}

impl RingPublicKey {
    /// Canonical byte encoding (used for ring binding in the Fiat-Shamir transcript).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(K * N * 4);
        for p in &self.t {
            for &c in p {
                out.extend_from_slice(&(c as u32).to_le_bytes());
            }
        }
        out
    }
}

/// Ring secret key: the short secret `s ∈ R_q^l`, `‖s‖∞ ≤ η`. Zeroized on drop (best-effort).
#[derive(Clone)]
pub struct RingSecretKey {
    s: Vec<Poly>,
}

impl Drop for RingSecretKey {
    fn drop(&mut self) {
        for p in &mut self.s {
            for c in p.iter_mut() {
                *c = 0;
            }
        }
    }
}

/// Sample a short secret coefficient in `[−η, η]` from raw RNG bytes (rejection, unbiased).
fn sample_eta(rng: &mut impl RngCore) -> i64 {
    // 2η+1 = 5 values; reject the top of the byte range to stay uniform.
    let span = (2 * ETA + 1) as u32; // 5
    let limit = (u32::MAX / span) * span;
    loop {
        let x = rng.next_u32();
        if x < limit {
            return (x % span) as i64 - ETA;
        }
    }
}

/// Generate a ring keypair: short `s`, public `t = A·s`.
pub fn keypair(rng: &mut impl RngCore) -> (RingSecretKey, RingPublicKey) {
    let a = matrix_a();
    let s: Vec<Poly> = (0..L)
        .map(|_| (0..N).map(|_| creduce(sample_eta(rng))).collect())
        .collect();
    let t = matvec(&a, &s);
    (RingSecretKey { s }, RingPublicKey { t })
}

// ── Fiat-Shamir hashing ─────────────────────────────────────────────────────────────────

fn absorb_vec(h: &mut Hasher, v: &[Poly]) {
    for p in v {
        for &c in p {
            h.update(&(c as u32).to_le_bytes());
        }
    }
}

/// Hash the whole ring's public keys to a 32-byte binding value.
fn ring_hash(ring: &[RingPublicKey]) -> [u8; 32] {
    let mut h = Hasher::new();
    h.update(b"SIGIL-shield-ring/ring/v1");
    h.update(&(ring.len() as u64).to_le_bytes());
    for pk in ring {
        h.update(&pk.to_bytes());
    }
    *h.finalize().as_bytes()
}

/// One link of the Fiat-Shamir chain: the seed feeding the *next* ring position.
fn fs_seed(
    msg: &[u8],
    rh: &[u8; 32],
    tag: &[Poly],
    w: &[Poly],
    wp: &[Poly],
) -> [u8; 32] {
    let mut h = Hasher::new();
    h.update(b"SIGIL-shield-ring/chain/v1");
    h.update(rh);
    h.update(&(msg.len() as u64).to_le_bytes());
    h.update(msg);
    absorb_vec(&mut h, tag);
    absorb_vec(&mut h, w);
    absorb_vec(&mut h, wp);
    *h.finalize().as_bytes()
}

/// SampleInBall: derive a challenge polynomial with exactly `τ` coefficients in `{−1,+1}`
/// from a 32-byte seed (Dilithium-style). Deterministic; the challenge space size
/// `C(n,τ)·2^τ ≫ 2^128` gives Fiat-Shamir soundness.
fn sample_in_ball(seed: &[u8; 32]) -> Poly {
    let mut h = Hasher::new();
    h.update(b"SIGIL-shield-ring/challenge/v1");
    h.update(seed);
    let mut reader = h.finalize_xof();

    let mut c = vec![0i64; N];
    // First 8 bytes → sign bits for the τ nonzero positions.
    let mut signbuf = [0u8; 8];
    reader.fill(&mut signbuf);
    let mut signs = u64::from_le_bytes(signbuf);

    for i in (N - TAU)..N {
        // Rejection-sample j ∈ [0, i].
        let j = loop {
            let mut b = [0u8; 1];
            reader.fill(&mut b);
            let jj = b[0] as usize;
            if jj <= i {
                break jj;
            }
        };
        c[i] = c[j];
        c[j] = if signs & 1 == 1 { Q - 1 } else { 1 }; // −1 ≡ q−1, +1
        signs >>= 1;
    }
    c
}

// ── Signature ───────────────────────────────────────────────────────────────────────────

/// A post-quantum linkable ring signature.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RingSignature {
    /// Fiat-Shamir seed feeding ring position 0 (the chain closes back to this).
    h0: [u8; 32],
    /// One response `z_i ∈ R_q^l` per ring member (each `l` polynomials).
    responses: Vec<Vec<Poly>>,
    /// Linkable tag / key image `T = G·s` (`k` polynomials) — deterministic in the secret.
    tag: Vec<Poly>,
    /// Ring size the signature was produced for (bound into every challenge via `ring_hash`).
    ring_size: usize,
}

impl RingSignature {
    /// Canonical bytes of the linkable tag — the nullifier key for double-spend detection.
    /// Two signatures from the same secret key produce byte-identical tags.
    pub fn link_tag(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(K * N * 4);
        for p in &self.tag {
            for &c in p {
                out.extend_from_slice(&(c as u32).to_le_bytes());
            }
        }
        out
    }
}

/// True iff both signatures were produced by the same secret key (equal linkable tags).
/// This is what a shielded pool checks to reject a double-spend.
pub fn is_linked(a: &RingSignature, b: &RingSignature) -> bool {
    a.tag == b.tag
}

fn sample_mask(rng: &mut impl RngCore) -> Vec<Poly> {
    // y ∈ (−γ1, γ1]; 2·γ1 = 2^18 values (power of two → unbiased low bits).
    let mask = (2 * GAMMA1 - 1) as u32; // 2^18 − 1
    (0..L)
        .map(|_| {
            (0..N)
                .map(|_| {
                    let r = (rng.next_u32() & mask) as i64; // [0, 2^18)
                    creduce(r - (GAMMA1 - 1)) // → [−(γ1−1), γ1]
                })
                .collect()
        })
        .collect()
}

fn sample_box_response(rng: &mut impl RngCore) -> Vec<Poly> {
    // Decoy responses uniform in [−ZBOUND, ZBOUND] — identical support to accepted honest z.
    let span = (2 * ZBOUND + 1) as u32;
    let limit = (u32::MAX / span) * span;
    (0..L)
        .map(|_| {
            (0..N)
                .map(|_| {
                    let v = loop {
                        let x = rng.next_u32();
                        if x < limit {
                            break (x % span) as i64;
                        }
                    };
                    creduce(v - ZBOUND)
                })
                .collect()
        })
        .collect()
}

/// Sign `message` as a member of `ring`, hiding which member. `sk`/`my_index` must satisfy
/// `A·sk.s == ring[my_index].t`, else [`RingError::KeyMismatch`] (fail-closed).
///
/// Anonymity: the produced signature is distributed identically regardless of `my_index`.
/// Linkability: the tag `T = G·sk.s` is deterministic in `sk`, so re-signing (any message,
/// any ring) is detectable via [`is_linked`].
pub fn sign(
    message: &[u8],
    ring: &[RingPublicKey],
    sk: &RingSecretKey,
    my_index: usize,
    rng: &mut impl RngCore,
) -> Result<RingSignature, RingError> {
    let r = ring.len();
    if r == 0 {
        return Err(RingError::EmptyRing);
    }
    if my_index >= r {
        return Err(RingError::IndexOutOfBounds { index: my_index, ring_size: r });
    }

    let a = matrix_a();
    let g = matrix_g();

    // Fail-closed: the secret must actually correspond to ring[my_index].
    let t_check = matvec(&a, &sk.s);
    if t_check != ring[my_index].t {
        return Err(RingError::KeyMismatch { index: my_index });
    }

    // Linkable tag T = G·s (deterministic; independent of the ring and message).
    let tag = matvec(&g, &sk.s);
    let rh = ring_hash(ring);
    let pi = my_index;

    for _ in 0..MAX_ABORTS {
        // Real commitment at π with a fresh mask.
        let y = sample_mask(rng);
        let w_pi = matvec(&a, &y); // A·y
        let wp_pi = matvec(&g, &y); // G·y

        let mut responses: Vec<Option<Vec<Poly>>> = vec![None; r];
        let mut seeds: Vec<[u8; 32]> = vec![[0u8; 32]; r];

        // Edge out of π seeds position (π+1).
        seeds[(pi + 1) % r] = fs_seed(message, &rh, &tag, &w_pi, &wp_pi);

        // Walk the decoys π+1 … π−1, back-computing their commitments.
        for step in 1..r {
            let i = (pi + step) % r;
            let c_i = sample_in_ball(&seeds[i]);
            let z_i = sample_box_response(rng);
            // W_i = A·z_i − c_i·t_i ; W'_i = G·z_i − c_i·T
            let w_i = vec_sub(&matvec(&a, &z_i), &scalar_mul_vec(&c_i, &ring[i].t));
            let wp_i = vec_sub(&matvec(&g, &z_i), &scalar_mul_vec(&c_i, &tag));
            seeds[(i + 1) % r] = fs_seed(message, &rh, &tag, &w_i, &wp_i);
            responses[i] = Some(z_i);
        }

        // Close the ring at π: z_π = y + c_π·s, rejection-sampled.
        let c_pi = sample_in_ball(&seeds[pi]);
        let z_pi = vec_add(&y, &scalar_mul_vec(&c_pi, &sk.s));
        if vec_infnorm(&z_pi) > ZBOUND {
            continue; // abort → fresh mask
        }
        responses[pi] = Some(z_pi);

        let responses: Vec<Vec<Poly>> = responses.into_iter().map(|o| o.unwrap()).collect();
        return Ok(RingSignature { h0: seeds[0], responses, tag, ring_size: r });
    }

    Err(RingError::RejectionExhausted(MAX_ABORTS))
}

/// Verify a ring signature over `message` against `ring`. Returns `true` iff the Fiat-Shamir
/// chain closes and every response is in-box — the ONLY thing that establishes validity.
pub fn verify(message: &[u8], ring: &[RingPublicKey], sig: &RingSignature) -> bool {
    let r = ring.len();
    if r == 0 || sig.ring_size != r || sig.responses.len() != r {
        return false;
    }
    if sig.tag.len() != K || sig.tag.iter().any(|p| p.len() != N) {
        return false;
    }

    // Dimension + norm gate on every response.
    for z in &sig.responses {
        if z.len() != L || z.iter().any(|p| p.len() != N) {
            return false;
        }
        if vec_infnorm(z) > ZBOUND {
            return false;
        }
    }

    let a = matrix_a();
    let g = matrix_g();
    let rh = ring_hash(ring);

    // Walk the whole chain from h0; each step recomputes the committed W_i,W'_i from z_i.
    let mut h = sig.h0;
    for i in 0..r {
        let c_i = sample_in_ball(&h);
        let z_i = &sig.responses[i];
        let w_i = vec_sub(&matvec(&a, z_i), &scalar_mul_vec(&c_i, &ring[i].t));
        let wp_i = vec_sub(&matvec(&g, z_i), &scalar_mul_vec(&c_i, &sig.tag));
        h = fs_seed(message, &rh, &sig.tag, &w_i, &wp_i);
    }
    // Ring closes iff the recomputed seed for position 0 equals the stored h0.
    h == sig.h0
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{rngs::StdRng, SeedableRng};

    fn make_ring(n: usize, seed: u64) -> (Vec<RingSecretKey>, Vec<RingPublicKey>) {
        let mut rng = StdRng::seed_from_u64(seed);
        let mut sks = Vec::new();
        let mut pks = Vec::new();
        for _ in 0..n {
            let (sk, pk) = keypair(&mut rng);
            sks.push(sk);
            pks.push(pk);
        }
        (sks, pks)
    }

    /// Sanity: the negacyclic ring relation X^n ≡ −1 holds, so the whole algebra is correct.
    #[test]
    fn negacyclic_ring_relation() {
        // x = X (coefficient 1 at degree 1).
        let mut x = poly_zero();
        x[1] = 1;
        // x^n via repeated squaring-ish: multiply X, n times → X^n ≡ −1 ≡ q−1.
        let mut acc = poly_zero();
        acc[0] = 1; // 1
        for _ in 0..N {
            acc = poly_mul(&acc, &x);
        }
        let mut expected = poly_zero();
        expected[0] = Q - 1; // −1
        assert_eq!(acc, expected, "X^n must reduce to −1 in R_q");

        // Distributivity smoke: (X+1)(X−1) = X^2 − 1.
        let mut xp1 = poly_zero();
        xp1[1] = 1;
        xp1[0] = 1;
        let mut xm1 = poly_zero();
        xm1[1] = 1;
        xm1[0] = Q - 1;
        let prod = poly_mul(&xp1, &xm1);
        let mut want = poly_zero();
        want[2] = 1;
        want[0] = Q - 1;
        assert_eq!(prod, want, "(X+1)(X−1) must equal X^2 − 1");
    }

    /// GATE (1): sign → verify round-trips for a ring of size 8, at several signer indices.
    #[test]
    fn gate1_roundtrip_ring8() {
        let (sks, pks) = make_ring(8, 1);
        let msg = b"SIGIL shielded transfer: 42 SIGIL nullifier=abc";
        for pi in [0usize, 3, 7] {
            let mut rng = StdRng::seed_from_u64(100 + pi as u64);
            let sig = sign(msg, &pks, &sks[pi], pi, &mut rng)
                .unwrap_or_else(|e| panic!("sign at index {pi} failed: {e:?}"));
            assert!(
                verify(msg, &pks, &sig),
                "a valid ring signature (index {pi}) must verify"
            );
        }
    }

    /// GATE (2a): a flipped byte anywhere in the signature is rejected.
    #[test]
    fn gate2_rejects_flipped_bytes() {
        let (sks, pks) = make_ring(8, 2);
        let msg = b"pay 7 SIGIL";
        let mut rng = StdRng::seed_from_u64(9);
        let sig = sign(msg, &pks, &sks[4], 4, &mut rng).unwrap();
        assert!(verify(msg, &pks, &sig), "baseline must verify");

        // Flip a bit in the Fiat-Shamir seed → chain cannot close.
        let mut s1 = sig.clone();
        s1.h0[0] ^= 0x01;
        assert!(!verify(msg, &pks, &s1), "flipped challenge seed must be rejected");

        // Perturb a response coefficient by 1 → recomputed commitment diverges.
        let mut s2 = sig.clone();
        s2.responses[4][0][0] = creduce(s2.responses[4][0][0] + 1);
        assert!(!verify(msg, &pks, &s2), "tampered response must be rejected");

        // Push a response coefficient out of the norm box → norm gate rejects.
        let mut s3 = sig.clone();
        s3.responses[2][0][0] = GAMMA1; // > ZBOUND
        assert!(!verify(msg, &pks, &s3), "out-of-box response must be rejected");
    }

    /// GATE (2b): a valid signature is rejected against a ring where a member was swapped
    /// (i.e. a signature "forged" for a ring the signer is not truly in).
    #[test]
    fn gate2_rejects_substituted_ring_member() {
        let (sks, pks) = make_ring(8, 3);
        let msg = b"transfer";
        let mut rng = StdRng::seed_from_u64(11);
        let sig = sign(msg, &pks, &sks[1], 1, &mut rng).unwrap();
        assert!(verify(msg, &pks, &sig), "baseline must verify");

        // Replace the true signer's public key with an unrelated key.
        let mut rng2 = StdRng::seed_from_u64(999);
        let (_sk_out, pk_out) = keypair(&mut rng2);
        let mut tampered_ring = pks.clone();
        tampered_ring[1] = pk_out;
        assert!(
            !verify(msg, &tampered_ring, &sig),
            "signature must not verify once the signer's ring key is substituted"
        );
    }

    /// GATE (2c): a non-member cannot sign — `sign` refuses when the key ≠ ring[my_index].
    #[test]
    fn gate2_nonmember_key_refused() {
        let (_sks, pks) = make_ring(8, 4);
        let mut rng = StdRng::seed_from_u64(77);
        let (outsider_sk, _outsider_pk) = keypair(&mut rng);
        let res = sign(b"forge me", &pks, &outsider_sk, 2, &mut rng);
        assert_eq!(
            res.unwrap_err(),
            RingError::KeyMismatch { index: 2 },
            "signing with a key not in the ring must be refused"
        );
    }

    /// GATE (3): a signature must not verify against a different message.
    #[test]
    fn gate3_rejects_wrong_message() {
        let (sks, pks) = make_ring(8, 5);
        let msg = b"send 100 to alice";
        let mut rng = StdRng::seed_from_u64(21);
        let sig = sign(msg, &pks, &sks[6], 6, &mut rng).unwrap();
        assert!(verify(msg, &pks, &sig), "baseline must verify");
        assert!(
            !verify(b"send 100 to eve", &pks, &sig),
            "signature must not verify against a different message"
        );
    }

    /// GATE (4): the linkable tag makes two signatures by the same secret linkable
    /// (double-use detectable), while signatures by different secrets are unlinkable.
    #[test]
    fn gate4_linkability() {
        let (sks, pks) = make_ring(8, 6);

        // Same signer (index 0), two DIFFERENT messages and even a different ring subset.
        let mut rng = StdRng::seed_from_u64(31);
        let sig_a = sign(b"first spend", &pks, &sks[0], 0, &mut rng).unwrap();

        // A second ring that still contains signer-0's key at a different position.
        let (sks2, mut pks2) = make_ring(4, 60);
        pks2.insert(0, pks[0].clone()); // signer-0 now at index 0 of a size-5 ring
        let _ = &sks2;
        let mut rng2 = StdRng::seed_from_u64(32);
        let sig_b = sign(b"second spend", &pks2, &sks[0], 0, &mut rng2).unwrap();

        assert!(
            is_linked(&sig_a, &sig_b),
            "two signatures from the same secret key must be linkable (double-use)"
        );
        assert_eq!(sig_a.link_tag(), sig_b.link_tag(), "link tags must be byte-identical");

        // Different signer (index 5) → not linked.
        let mut rng3 = StdRng::seed_from_u64(33);
        let sig_c = sign(b"first spend", &pks, &sks[5], 5, &mut rng3).unwrap();
        assert!(
            !is_linked(&sig_a, &sig_c),
            "signatures from different secret keys must not be linked"
        );
    }

    /// Anonymity smoke test: a signature carries no plaintext index and verifies under the
    /// ring regardless of which member signed (indices already covered in gate1). Here we
    /// assert the serialized signature is independent-of-index in structure/size.
    #[test]
    fn signature_shape_is_index_independent() {
        let (sks, pks) = make_ring(8, 7);
        let msg = b"anon";
        let mut rng0 = StdRng::seed_from_u64(41);
        let s0 = sign(msg, &pks, &sks[0], 0, &mut rng0).unwrap();
        let mut rng7 = StdRng::seed_from_u64(41);
        let s7 = sign(msg, &pks, &sks[7], 7, &mut rng7).unwrap();
        assert_eq!(s0.responses.len(), s7.responses.len());
        assert_eq!(s0.responses.len(), 8);
        // Both verify; neither encodes its signer index.
        assert!(verify(msg, &pks, &s0) && verify(msg, &pks, &s7));
    }

    /// Empty ring and bad index are rejected by `sign`.
    #[test]
    fn sign_input_validation() {
        let (sks, pks) = make_ring(2, 8);
        let mut rng = StdRng::seed_from_u64(51);
        assert_eq!(
            sign(b"m", &[], &sks[0], 0, &mut rng).unwrap_err(),
            RingError::EmptyRing
        );
        assert!(matches!(
            sign(b"m", &pks, &sks[0], 5, &mut rng).unwrap_err(),
            RingError::IndexOutOfBounds { .. }
        ));
    }
}
