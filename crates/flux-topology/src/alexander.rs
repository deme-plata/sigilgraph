//! The Alexander polynomial of a braid closure — exact, via the reduced
//! Burau representation and a fraction-free Bareiss determinant.
//!
//! For a word `w ∈ B_n` with reduced Burau image `B̄(w)`:
//!
//! ```text
//! Δ(ŵ)(t) ≐ det(B̄(w) − I) · (1 − t) / (1 − tⁿ)
//! ```
//!
//! up to the units ±t^k (the classical Burau formula). Both the determinant
//! (Bareiss: every division is exact in the integral domain ℤ[t,t⁻¹]) and the
//! final `(1 − tⁿ)` division are exact — no floats, no approximation. The
//! result is [`LaurentPoly::normalize_alexander`]-canonical. A zero result is
//! honest (e.g. split links have Δ = 0). Intended for small strand counts
//! (n ≤ 8 in v0) — the (n−1)×(n−1) determinant is trivial there.

use crate::burau::{burau_reduced, LMatrix};
use crate::laurent::LaurentPoly;
use crate::BraidWord;

/// Fraction-free Bareiss determinant with row pivoting. Entries stay ± minors
/// of the input, so every `div_exact` is guaranteed exact.
fn bareiss_det(mut m: LMatrix) -> LaurentPoly {
    let n = m.len();
    if n == 0 {
        return LaurentPoly::one(); // det of the 0×0 matrix (empty product)
    }
    let mut negate = false;
    let mut prev = LaurentPoly::one();
    for k in 0..n - 1 {
        if m[k][k].is_zero() {
            let Some(r) = (k + 1..n).find(|&r| !m[r][k].is_zero()) else {
                return LaurentPoly::zero();
            };
            m.swap(k, r);
            negate = !negate;
        }
        for i in k + 1..n {
            for j in k + 1..n {
                let num = m[k][k].mul(&m[i][j]).sub(&m[i][k].mul(&m[k][j]));
                m[i][j] = num
                    .div_exact(&prev)
                    .expect("Bareiss division must be exact over ℤ[t,t⁻¹]");
            }
            m[i][k] = LaurentPoly::zero();
        }
        prev = m[k][k].clone();
    }
    let det = m[n - 1][n - 1].clone();
    if negate { det.neg() } else { det }
}

/// Δ(t) of the CLOSURE of `w`, canonically normalized (±t^k unit fixed by
/// [`LaurentPoly::normalize_alexander`]). Returns 1 for the unknot,
/// 0 for split links (honestly), and `t − 1 + t⁻¹` for the trefoil.
pub fn alexander_poly(w: &BraidWord) -> LaurentPoly {
    let n = w.strands as usize;
    if n <= 1 {
        // Closure of the empty/1-strand braid is the unknot (by convention).
        return LaurentPoly::one();
    }
    let mut m = burau_reduced(w);
    for (k, row) in m.iter_mut().enumerate() {
        row[k] = row[k].sub(&LaurentPoly::one()); // B̄(w) − I
    }
    let det = bareiss_det(m);
    if det.is_zero() {
        return LaurentPoly::zero();
    }
    let one_minus_t = LaurentPoly::from_coeffs(0, vec![1, -1]);
    let mut den_coeffs = vec![0i128; n + 1];
    den_coeffs[0] = 1;
    den_coeffs[n] = -1; // 1 − tⁿ
    let den = LaurentPoly::from_coeffs(0, den_coeffs);
    det.mul(&one_minus_t)
        .div_exact(&den)
        .expect("det(B̄−I)·(1−t) must be divisible by (1−tⁿ) — Burau formula")
        .normalize_alexander()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bareiss_matches_hand_determinants() {
        // det [[−t−1, −t²],[1, −1]] = t + 1 + t² (the σ1σ2 ∈ B_3 case, by hand).
        let m = vec![
            vec![LaurentPoly::from_coeffs(0, vec![-1, -1]), LaurentPoly::term(-1, 2)],
            vec![LaurentPoly::one(), LaurentPoly::term(-1, 0)],
        ];
        assert_eq!(bareiss_det(m), LaurentPoly::from_coeffs(0, vec![1, 1, 1]));
    }

    #[test]
    fn bareiss_zero_pivot_row_swap() {
        // [[0, 1],[t, 0]]: det = −t, needs the pivot swap (and the sign flip).
        let m = vec![
            vec![LaurentPoly::zero(), LaurentPoly::one()],
            vec![LaurentPoly::t(), LaurentPoly::zero()],
        ];
        assert_eq!(bareiss_det(m), LaurentPoly::term(-1, 1));
    }

    #[test]
    fn bareiss_singular_is_zero() {
        let m = vec![
            vec![LaurentPoly::t(), LaurentPoly::t()],
            vec![LaurentPoly::t(), LaurentPoly::t()],
        ];
        assert_eq!(bareiss_det(m), LaurentPoly::zero());
    }

    #[test]
    fn trivial_braid_on_n_strands_is_split_unlink() {
        // B̄ = I ⇒ det(B̄−I) = 0 ⇒ Δ = 0 — the honest answer for the n-unlink.
        for n in 2..=5u32 {
            assert_eq!(alexander_poly(&BraidWord::new(n, vec![])), LaurentPoly::zero());
        }
        // n = 1: the unknot.
        assert_eq!(alexander_poly(&BraidWord::new(1, vec![])), LaurentPoly::one());
    }

    #[test]
    fn cinquefoil_5_1_torus_knot() {
        // σ1⁵ ∈ B_2 closes to the (2,5) torus knot: Δ ≐ t² − t + 1 − t⁻¹ + t⁻².
        let w = BraidWord::new(2, vec![1, 1, 1, 1, 1]);
        assert_eq!(
            alexander_poly(&w),
            LaurentPoly::from_coeffs(-2, vec![1, -1, 1, -1, 1])
        );
    }

    #[test]
    fn markov_stabilization_preserves_delta() {
        // w ∈ B_n and w·σ_n ∈ B_{n+1} close to the same link (Markov move),
        // so Δ must agree: trefoil as σ1³ ∈ B_2 vs σ1³σ2 ∈ B_3.
        let a = alexander_poly(&BraidWord::new(2, vec![1, 1, 1]));
        let b = alexander_poly(&BraidWord::new(3, vec![1, 1, 1, 2]));
        assert_eq!(a, b);
        assert_eq!(a, LaurentPoly::from_coeffs(-1, vec![1, -1, 1]));
    }
}
