//! Zero-knowledge masking for winterfell AIRs — and a prover contract that REFUSES to
//! ship a proof containing its own witness.
//!
//! # Why this module exists
//!
//! Winterfell is a STARK library, not a zk-STARK library. It gives succinctness and
//! transparency; it does not hide the witness, in any released version (checked 0.9
//! through 0.13.1 — no trace randomisation, no salted commitments, no `zk` feature).
//! Every FRI query opening publishes the whole trace row at that position.
//!
//! For most AIRs that is a partial leak. For an AIR that parks a secret in a column held
//! CONSTANT down the trace it is a total one: a constant column interpolates to a constant
//! polynomial, whose low-degree extension equals that constant at *every* point of the
//! domain. One opening is enough. `spend_full_v4` did exactly this, and a real proof was
//! measured carrying the recipient key and both output amounts verbatim, 85 times each.
//!
//! # The construction: reserved random rows
//!
//! Textbook zk-STARK masking replaces each trace polynomial `T(x)` with
//! `T(x) + Z_H(x)·R(x)` for random `R`. That needs to reach inside the prover's low-degree
//! extension, which winterfell does not expose. The equivalent that IS reachable from
//! stock winterfell:
//!
//! ```text
//!   rows 0 .. real_len            the real execution trace, fully constrained
//!   rows real_len .. trace_len    uniform randomness, constrained by NOTHING
//! ```
//!
//! and then [`AirContext::set_num_transition_exemptions`] is told to exempt the tail, so
//! the random rows satisfy no transition constraint and are free to be anything.
//!
//! Each column polynomial still has degree < `trace_len`, so the FRI/DEEP degree budget is
//! untouched — but `zk_rows` of the values that determine it are now uniform and secret.
//! A verifier who opens `q` positions learns `q` linear functionals of all `trace_len`
//! values; when `zk_rows > q` the map from the random tail to those `q` openings is
//! surjective, so the openings are distributed independently of the real rows.
//!
//! **Honest scope.** This makes the openings of the *trace* commitment simulatable, which
//! is what kills the verbatim leak. A complete zero-knowledge claim also has to argue
//! about the constraint-composition and DEEP commitments and the out-of-domain frame.
//! Those inherit the trace's randomisation, and no leak survives the empirical gate below,
//! but a full simulator proof is NOT claimed here. Treat this as "the witness is no longer
//! recoverable from the proof", verified, not as a proven ZK theorem.
//!
//! # The gate
//!
//! [`HidingProver`] is the contribution that matters most. Winterfell's `Prover` will
//! happily hand back a proof that publishes everything it was supposed to hide, and
//! nothing in the type system objects. `HidingProver` makes an implementor NAME its
//! secrets, and [`HidingProver::prove_hiding`] refuses to return a proof containing any of
//! them. It is a smoke alarm, not a fire code: it catches verbatim leakage — the
//! catastrophic kind, and the kind that actually happened — not subtle statistical
//! leakage.

use winterfell::math::{fields::f64::BaseElement, FieldElement, StarkField};
use winterfell::{Proof, ProofOptions, Prover, ProverError, Trace};

/// Extra reserved rows beyond the opening count, so the hiding argument is not sitting on
/// its own boundary. Covers the two out-of-domain frame rows and leaves slack.
pub const ZK_MARGIN: usize = 8;

/// How many unconstrained random rows a proof needs to hide its trace from a verifier that
/// opens `options.num_queries()` positions.
///
/// The verifier's view of the main trace commitment is `num_queries` opened rows plus the
/// out-of-domain frame. Reserving strictly more random rows than that makes the tail able
/// to explain any observation.
pub fn zk_rows_for(options: &ProofOptions) -> usize {
    options.num_queries() + ZK_MARGIN
}

/// The padded trace length for a real trace of `real_len` rows: the next power of two that
/// leaves at least [`zk_rows_for`] rows of randomness.
///
/// Doubling is the common case and the one the exemption cap is friendliest to — winterfell
/// permits exempting at most `trace_len / 2 + 1` rows, which is exactly what a doubled
/// trace needs.
pub fn padded_len(real_len: usize, options: &ProofOptions) -> usize {
    let need = real_len + zk_rows_for(options) + 1;
    let mut n = real_len.next_power_of_two();
    while n < need {
        n *= 2;
    }
    n
}

/// Rows the AIR must exempt from transition constraints, given a padded trace whose real
/// region is `real_len` rows.
///
/// Transitions must hold for frames `(i, i+1)` with `i < real_len - 1`, so every row from
/// `real_len - 1` onward is exempt as a frame origin.
pub fn exemptions_for(trace_len: usize, real_len: usize) -> usize {
    trace_len - real_len + 1
}

/// A cryptographically random field element derived from `(seed, step, col)`.
///
/// `TraceTable::fill` takes `Fn` closures, so a stateful RNG cannot be threaded through it.
/// A PRF keyed by a per-proof random seed gives the same distribution and is reproducible
/// for tests. The seed MUST be fresh per proof — reusing one across proofs of the same
/// witness would let an observer difference them and recover the trace.
#[inline]
pub fn mask_value(seed: &[u8; 32], step: usize, col: usize) -> BaseElement {
    let mut h = blake3::Hasher::new();
    h.update(seed);
    h.update(&(step as u64).to_le_bytes());
    h.update(&(col as u64).to_le_bytes());
    let mut out = [0u8; 16];
    h.finalize_xof().fill(&mut out);
    // Reduce 128 bits into Goldilocks: the bias is below 2^-64, far under any bound that
    // matters here, and `BaseElement::new` is canonical-reducing on the low limb.
    let lo = u64::from_le_bytes(out[0..8].try_into().unwrap());
    let hi = u64::from_le_bytes(out[8..16].try_into().unwrap());
    BaseElement::new(lo) + BaseElement::new(hi) * BaseElement::new(1u64 << 32)
}

/// Fill one padded row with randomness. Call from a `TraceTable::fill` update closure for
/// every step at or beyond the real region.
#[inline]
pub fn fill_random_row(seed: &[u8; 32], step: usize, state: &mut [BaseElement]) {
    // DIAGNOSTIC ONLY: SIGIL_ZK_PAD=zero|hold isolates "does randomness break the proof"
    // from "is the exemption wiring wrong". Unset (the default) is the real behaviour.
    match std::env::var("SIGIL_ZK_PAD").ok().as_deref() {
        Some("zero") => { for s in state.iter_mut() { *s = BaseElement::ZERO; } }
        Some("hold") => { /* leave the previous row in place */ }
        _ => { for (col, s) in state.iter_mut().enumerate() { *s = mask_value(seed, step, col); } }
    }
}

/// A secret the proof must not contain, and the name to report if it does.
pub type Secret = (&'static str, BaseElement);

/// One verbatim appearance of a secret in a serialised proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leak {
    pub name: &'static str,
    pub value: u64,
    pub occurrences: usize,
}

impl std::fmt::Display for Leak {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}) appears {}x in the proof", self.name, self.value, self.occurrences)
    }
}

/// How many times a needle of this shape appears in `n` bytes purely by chance.
///
/// This matters more than it looks. A small field value like `50` serialises to one
/// nonzero byte followed by seven zeros, and proofs are full of zero runs — so a
/// byte-scan finds it about once in a 100 KB proof whether or not the witness contains
/// it. Measured directly: a control value never present in the trace scored the same 1
/// hit as a "leaked" secret. Without this correction the gate cries wolf on every small
/// amount, and a gate that cries wolf gets switched off.
fn low_entropy(v: BaseElement) -> bool {
    let b = v.as_int().to_le_bytes();
    b.iter().filter(|x| **x == 0).count() >= 5
}

/// Scan serialised proof bytes for the canonical encoding of each secret.
///
/// This is deliberately the dumbest possible check, because the failure it catches was
/// exactly that dumb: the value sitting in the clear. It cannot certify zero-knowledge —
/// a leak of `2·v` or `v + 1` sails past it. It can and does certify that the specific
/// catastrophic failure mode is gone.
pub fn scan_proof_for_secrets(proof_bytes: &[u8], secrets: &[Secret]) -> Vec<Leak> {
    scan_proof_for_secrets_with_threshold(proof_bytes, secrets, COINCIDENCE_THRESHOLD)
}

/// Occurrences below this are treated as byte-pattern coincidence for a low-entropy value.
/// A genuine constant-column leak scores once per query opening — 85 in the measured v4
/// case — so the gap between signal and noise is nearly two orders of magnitude, and this
/// threshold sits comfortably in it.
pub const COINCIDENCE_THRESHOLD: usize = 3;

/// As [`scan_proof_for_secrets`], with the coincidence threshold under your control. Pass
/// `1` to see every raw byte match, including the noise.
pub fn scan_proof_for_secrets_with_threshold(
    proof_bytes: &[u8],
    secrets: &[Secret],
    threshold: usize,
) -> Vec<Leak> {
    let mut leaks = Vec::new();
    if proof_bytes.len() < 8 {
        return leaks;
    }
    for (name, v) in secrets {
        let needle = v.as_int().to_le_bytes();
        let n = proof_bytes.windows(8).filter(|w| *w == needle).count();
        // High-entropy values (keys, commitments) never collide by chance, so ANY hit is
        // real. Low-entropy ones need to clear the noise floor.
        let floor = if low_entropy(*v) { threshold } else { 1 };
        if n >= floor {
            leaks.push(Leak { name, value: v.as_int(), occurrences: n });
        }
    }
    leaks
}

/// What went wrong in [`HidingProver::prove_hiding`].
#[derive(Debug)]
pub enum HidingError {
    /// The underlying winterfell prover failed.
    Prove(ProverError),
    /// The AIR reserves too little randomness to hide against this many openings.
    InsufficientMasking { reserved: usize, required: usize },
    /// The proof verified — and contained the witness. Never return this proof.
    WitnessLeaked(Vec<Leak>),
}

impl std::fmt::Display for HidingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HidingError::Prove(e) => write!(f, "prover failed: {e:?}"),
            HidingError::InsufficientMasking { reserved, required } => write!(
                f,
                "AIR reserves {reserved} random rows but {required} are needed to hide \
                 against this proof's openings"
            ),
            HidingError::WitnessLeaked(l) => {
                write!(f, "SECURITY: proof contains its own witness —")?;
                for leak in l {
                    write!(f, " [{leak}]")?;
                }
                Ok(())
            }
        }
    }
}
impl std::error::Error for HidingError {}

/// A [`Prover`] that is required to declare what it hides — and is checked on it.
///
/// Implement [`secrets`](HidingProver::secrets) with every witness value that must not
/// appear in the proof, and [`zk_reserved_rows`](HidingProver::zk_reserved_rows) with the
/// number of unconstrained random rows the AIR actually reserves. Then call
/// [`prove_hiding`](HidingProver::prove_hiding) instead of `prove`, and a proof that
/// publishes its witness becomes an `Err` rather than a shipped transaction.
///
/// The check costs one pass over the proof bytes — microseconds against a 20 ms prove.
/// There is no reason to run the unchecked path in production.
pub trait HidingProver: Prover<BaseField = BaseElement> {
    /// Values this proof must not reveal. Derived from the trace, so the caller cannot
    /// forget to update it when the witness shape changes.
    fn secrets(&self, trace: &Self::Trace) -> Vec<Secret>;

    /// Unconstrained random rows the AIR reserves via transition exemptions.
    fn zk_reserved_rows(&self, trace: &Self::Trace) -> usize;

    /// Prove, then refuse to hand back a proof that leaks.
    fn prove_hiding(&self, trace: Self::Trace) -> Result<Proof, HidingError> {
        let required = zk_rows_for(self.options());
        let reserved = self.zk_reserved_rows(&trace);
        if reserved < required {
            return Err(HidingError::InsufficientMasking { reserved, required });
        }
        let secrets = self.secrets(&trace);
        let proof = self.prove(trace).map_err(HidingError::Prove)?;
        let leaks = scan_proof_for_secrets(&proof.to_bytes(), &secrets);
        if !leaks.is_empty() {
            return Err(HidingError::WitnessLeaked(leaks));
        }
        Ok(proof)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use winterfell::FieldExtension;

    fn opts(q: usize) -> ProofOptions {
        ProofOptions::new(q, 8, 16, FieldExtension::Quadratic, 8, 31)
    }

    #[test]
    fn reserved_rows_exceed_openings() {
        for q in [21usize, 32, 42, 64, 84, 128] {
            let o = opts(q);
            assert!(zk_rows_for(&o) > o.num_queries(), "must out-reserve the openings");
        }
    }

    #[test]
    fn padded_len_is_a_power_of_two_with_room() {
        let o = opts(84);
        for real in [64usize, 128, 256, 512, 1024] {
            let n = padded_len(real, &o);
            assert!(n.is_power_of_two(), "padded length must stay a power of two");
            assert!(n > real, "padding must add rows");
            assert!(n - real > o.num_queries(), "padding must exceed the opening count");
        }
    }

    /// The exemption count must be exactly what winterfell will accept: no more than
    /// `trace_len / 2 + 1`. A doubled trace sits exactly on that bound, which is why
    /// `padded_len` doubles rather than growing by the minimum.
    #[test]
    fn exemptions_stay_within_winterfell_limit() {
        let o = opts(84);
        for real in [128usize, 256, 512] {
            let n = padded_len(real, &o);
            let ex = exemptions_for(n, real);
            assert!(ex <= n / 2 + 1, "exemptions {ex} exceed winterfell's cap {}", n / 2 + 1);
        }
    }

    #[test]
    fn mask_is_deterministic_per_seed_and_varies_per_cell() {
        let seed = [7u8; 32];
        assert_eq!(mask_value(&seed, 3, 4), mask_value(&seed, 3, 4));
        assert_ne!(mask_value(&seed, 3, 4), mask_value(&seed, 3, 5));
        assert_ne!(mask_value(&seed, 3, 4), mask_value(&seed, 4, 4));
        let other = [8u8; 32];
        assert_ne!(mask_value(&seed, 3, 4), mask_value(&other, 3, 4));
    }

    #[test]
    fn mask_rows_are_not_degenerate() {
        let seed = [42u8; 32];
        let mut row = vec![BaseElement::ZERO; 33];
        fill_random_row(&seed, 300, &mut row);
        assert!(row.iter().all(|v| *v != BaseElement::ZERO), "no zero cells");
        let uniq: std::collections::HashSet<u64> = row.iter().map(|v| v.as_int()).collect();
        assert_eq!(uniq.len(), row.len(), "every cell must differ");
    }

    /// A single stray match on a small value must NOT be reported: measured, a control
    /// value never present in the witness scored exactly 1 hit in a 100 KB proof.
    #[test]
    fn scanner_does_not_cry_wolf_on_one_coincidental_small_value() {
        let small = BaseElement::new(50);
        let mut bytes = vec![7u8; 4096];
        bytes.splice(100..108, small.as_int().to_le_bytes());
        assert!(scan_proof_for_secrets(&bytes, &[("small", small)]).is_empty(),
            "one hit on a low-entropy value is coincidence, not disclosure");
        // ...but a real constant-column leak scores once per opening, and must be caught.
        for i in 0..10 { bytes.splice(200 + i * 16..208 + i * 16, small.as_int().to_le_bytes()); }
        assert_eq!(scan_proof_for_secrets(&bytes, &[("small", small)]).len(), 1,
            "repeated occurrences are a real leak and must be reported");
    }

    /// A high-entropy value cannot collide by chance, so one hit is already a leak.
    #[test]
    fn scanner_reports_a_single_hit_on_a_high_entropy_value() {
        let key = BaseElement::new(17845265196358578996);
        let mut bytes = vec![7u8; 4096];
        bytes.splice(100..108, key.as_int().to_le_bytes());
        assert_eq!(scan_proof_for_secrets(&bytes, &[("key", key)]).len(), 1);
    }

    #[test]
    fn scanner_finds_a_planted_secret_and_ignores_an_absent_one() {
        let present = BaseElement::new(0xDEAD_BEEF_1234);
        let absent = BaseElement::new(0xFEED_FACE_5678);
        let mut bytes = vec![0u8; 64];
        bytes.extend_from_slice(&present.as_int().to_le_bytes());
        bytes.extend_from_slice(&[9u8; 32]);
        bytes.extend_from_slice(&present.as_int().to_le_bytes());
        let leaks = scan_proof_for_secrets(&bytes, &[("present", present), ("absent", absent)]);
        assert_eq!(leaks.len(), 1);
        assert_eq!(leaks[0].name, "present");
        assert_eq!(leaks[0].occurrences, 2);
    }

    #[test]
    fn scanner_tolerates_a_tiny_proof() {
        assert!(scan_proof_for_secrets(&[1, 2, 3], &[("x", BaseElement::new(5))]).is_empty());
    }
}
