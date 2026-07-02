//! # flux-topology — real braid invariants (QTFT-1)
//!
//! Pure mathematics over plain Artin braid words. Zero chain deps, zero sigil
//! deps — the only input type is [`BraidWord`].
//!
//! What this crate computes is REAL, exact, and nothing more:
//!
//! * [`permutation`] — the strand permutation induced by a braid word,
//! * [`writhe`] — the signed crossing sum,
//! * [`linking_matrix`] / [`linking_number`] — signed crossing counts between
//!   strand pairs of the braid closure (halved: the linking-number convention),
//! * [`burau_reduced`] — the reduced Burau representation, (n−1)×(n−1)
//!   matrices over exact integer Laurent polynomials ℤ[t,t⁻¹],
//! * [`alexander_poly`] — the Alexander polynomial of the braid closure,
//!   Δ(t) ≐ det(B̄(w)−I)·(1−t)/(1−tⁿ), computed EXACTLY with a fraction-free
//!   Bareiss determinant and normalized to a canonical ±t^k unit.
//!
//! Explicitly NOT here: there is NO Jones polynomial and NO Jones-style
//! approximation or heuristic anywhere in this crate. If an invariant is not
//! computed exactly, it is not offered.
//!
//! Context (honest-naming rule of `docs/SIGIL_DAGKNIGHT_LANE_v0.md`, binding):
//! this crate is the QTFT-1 math lane later consumed by `sigil-dagknight` via
//! a one-line struct-literal bridge. That lane's v1 is DETERMINISTIC BRAID
//! LINEARIZATION — NOT GHOSTDAG (no blue score / k-cluster) and NOT
//! DagKnight-the-paper (no parameterless min-cut anticone bound). The braid
//! words it hands to this crate are a presentation convention over a
//! deterministic block order; no physical-braid claim is made there either.
//!
//! ## Word convention
//! `gens[k] = ±i` encodes the Artin generator σ_i^{±1} with the 1-based
//! generator index `1 ≤ i ≤ strands−1` (equivalently ±(p+1) where `p` is the
//! 0-based position of the left strand of the crossing). Positive entry =
//! positive crossing. Words are read left to right.

#![forbid(unsafe_code)]

pub mod alexander;
pub mod burau;
pub mod laurent;
pub mod linking;

pub use alexander::alexander_poly;
pub use burau::burau_reduced;
pub use laurent::LaurentPoly;
pub use linking::{linking_matrix, linking_number};

/// A braid word in Artin generators. `gens[k] = ±i` means σ_i^{±1}
/// (1-based generator index, sign = crossing sign); `strands` = n.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BraidWord {
    pub strands: u32,
    pub gens: Vec<i32>,
}

impl BraidWord {
    pub fn new(strands: u32, gens: Vec<i32>) -> Self {
        Self { strands, gens }
    }
}

/// 1-based generator index of `g` for a word on `strands` strands.
/// Panics on a malformed generator — a braid word with out-of-range entries
/// has no meaning, so failing loud beats a silent wrong invariant.
pub(crate) fn gen_index(strands: u32, g: i32) -> usize {
    let i = g.unsigned_abs() as usize;
    assert!(
        g != 0 && i < strands as usize,
        "generator {g} out of range for {strands} strands (need 1 ≤ |g| ≤ strands−1)"
    );
    i
}

/// The strand permutation induced by `w`: `perm[s]` = final position of the
/// strand that STARTS at position `s` (positions and strands 0-based).
pub fn permutation(w: &BraidWord) -> Vec<usize> {
    let n = w.strands as usize;
    // occupant[pos] = strand currently at that position.
    let mut occupant: Vec<usize> = (0..n).collect();
    for &g in &w.gens {
        let i = gen_index(w.strands, g); // crossing swaps positions (i−1, i)
        occupant.swap(i - 1, i);
    }
    let mut perm = vec![0usize; n];
    for (pos, &strand) in occupant.iter().enumerate() {
        perm[strand] = pos;
    }
    perm
}

/// The writhe of `w`: Σ signs of all crossings.
pub fn writhe(w: &BraidWord) -> i64 {
    w.gens
        .iter()
        .map(|&g| {
            gen_index(w.strands, g); // validate range even though only the sign matters
            g.signum() as i64
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::laurent::LaurentPoly;

    // ── KAT gate (docs/SIGIL_DAGKNIGHT_LANE_v0.md §5) ────────────────────────

    #[test]
    fn kat_unknot_sigma1_n2_delta_is_1() {
        let w = BraidWord::new(2, vec![1]);
        assert_eq!(alexander_poly(&w), LaurentPoly::one());
        // Mirror generator closes to the unknot too.
        let m = BraidWord::new(2, vec![-1]);
        assert_eq!(alexander_poly(&m), LaurentPoly::one());
    }

    #[test]
    fn kat_trefoil_sigma1_cubed_delta() {
        // Δ(trefoil) ≐ t − 1 + t⁻¹
        let expected = LaurentPoly::from_coeffs(-1, vec![1, -1, 1]);
        let w = BraidWord::new(2, vec![1, 1, 1]);
        assert_eq!(alexander_poly(&w), expected);
        // Alexander is mirror-insensitive: σ1⁻³ gives the same Δ.
        let m = BraidWord::new(2, vec![-1, -1, -1]);
        assert_eq!(alexander_poly(&m), expected);
    }

    #[test]
    fn kat_figure_eight_delta() {
        // (σ1 σ2⁻¹)² on 3 strands closes to the figure-eight knot: Δ ≐ t − 3 + t⁻¹
        let w = BraidWord::new(3, vec![1, -2, 1, -2]);
        let d = alexander_poly(&w);
        assert_eq!(d, LaurentPoly::from_coeffs(-1, vec![1, -3, 1]));
        assert!(d.is_palindromic(), "knot Δ must be symmetric");
        assert_eq!(d.eval_one().abs(), 1, "Δ(1) = ±1 for a knot");
    }

    #[test]
    fn kat_hopf_link_linking_and_writhe() {
        let hopf = BraidWord::new(2, vec![1, 1]);
        assert_eq!(linking_number(&hopf, 0, 1), 1);
        assert_eq!(writhe(&hopf), 2);
        let neg = BraidWord::new(2, vec![-1, -1]);
        assert_eq!(linking_number(&neg, 0, 1), -1);
        assert_eq!(writhe(&neg), -2);
    }

    #[test]
    fn kat_sigma1_sigma2_unknot_closure() {
        // σ1σ2 on 3 strands: closure is the unknot (Δ = 1), permutation a 3-cycle.
        let w = BraidWord::new(3, vec![1, 2]);
        assert_eq!(alexander_poly(&w), LaurentPoly::one());
        let p = permutation(&w);
        assert_eq!(p, vec![2, 0, 1]);
        // Explicit 3-cycle check: orbit of 0 has length 3.
        assert_eq!(p[p[p[0]]], 0);
        assert_ne!(p[0], 0);
        assert_ne!(p[p[0]], 0);
    }

    // ── far-commutation property test (seeded, deterministic) ───────────────

    fn xorshift64(s: &mut u64) -> u64 {
        let mut x = *s;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *s = x;
        x
    }

    fn random_word(seed: u64) -> BraidWord {
        let mut s = seed.max(1);
        let n = 5 + (xorshift64(&mut s) % 2) as u32; // 5 or 6 strands
        let len = 10 + (xorshift64(&mut s) % 5) as usize; // 10..=14 crossings
        let gens = (0..len)
            .map(|_| {
                let i = 1 + (xorshift64(&mut s) % (n as u64 - 1)) as i32;
                if xorshift64(&mut s) & 1 == 0 { i } else { -i }
            })
            .collect();
        BraidWord::new(n, gens)
    }

    #[test]
    fn prop_far_commutation_invariance() {
        // σiσj = σjσi for |i−j| ≥ 2: swapping any far-commuting adjacent pair
        // must leave every invariant unchanged.
        let mut far_pairs_tested = 0usize;
        for t in 0..6u64 {
            let w = random_word(0xC0FFEE ^ (t.wrapping_mul(0x9E3779B97F4A7C15)));
            let base_perm = permutation(&w);
            let base_writhe = writhe(&w);
            let base_lk = linking_matrix(&w);
            let base_delta = alexander_poly(&w);
            for k in 0..w.gens.len() - 1 {
                let (a, b) = (w.gens[k].unsigned_abs(), w.gens[k + 1].unsigned_abs());
                if (a as i64 - b as i64).abs() < 2 {
                    continue;
                }
                far_pairs_tested += 1;
                let mut swapped = w.clone();
                swapped.gens.swap(k, k + 1);
                assert_eq!(permutation(&swapped), base_perm, "perm changed (seed {t}, k {k})");
                assert_eq!(writhe(&swapped), base_writhe, "writhe changed (seed {t}, k {k})");
                assert_eq!(linking_matrix(&swapped), base_lk, "lk changed (seed {t}, k {k})");
                assert_eq!(alexander_poly(&swapped), base_delta, "Δ changed (seed {t}, k {k})");
            }
        }
        assert!(
            far_pairs_tested >= 5,
            "property test vacuous: only {far_pairs_tested} far pairs found"
        );
    }

    // ── unit coverage ────────────────────────────────────────────────────────

    #[test]
    fn permutation_identity_on_empty_word() {
        let w = BraidWord::new(4, vec![]);
        assert_eq!(permutation(&w), vec![0, 1, 2, 3]);
        assert_eq!(writhe(&w), 0);
    }

    #[test]
    #[should_panic(expected = "out of range")]
    fn out_of_range_generator_panics() {
        permutation(&BraidWord::new(2, vec![2]));
    }
}
