//! The reduced Burau representation of the braid group B_n:
//! (n−1)×(n−1) matrices over exact Laurent polynomials ℤ[t,t⁻¹].
//!
//! Generator images (0-based matrix indices, `i` = 1-based generator index;
//! every σ_i^± deviates from the identity only in column `i−1`):
//!
//! ```text
//!  σ_i   : M[i−1][i−1] = −t    and (when the rows exist)
//!          M[i−2][i−1] = t,     M[i][i−1] = 1
//!  σ_i⁻¹ : M[i−1][i−1] = −t⁻¹  and (when the rows exist)
//!          M[i−2][i−1] = 1,     M[i][i−1] = t⁻¹
//! ```
//!
//! which reproduces the textbook blocks σ₁ ↦ [[−t,0],[1,1]] ⊕ I,
//! σ_i ↦ I ⊕ [[1,t,0],[0,−t,0],[0,1,1]] ⊕ I, σ_{n−1} ↦ I ⊕ [[1,t],[0,−t]]
//! (and their exact inverses). Verified in-tests against the braid relations
//! σ_i σ_{i+1} σ_i = σ_{i+1} σ_i σ_{i+1} and far-commutation.

use crate::laurent::LaurentPoly;
use crate::{gen_index, BraidWord};

/// Dense (n−1)×(n−1) matrix over ℤ[t,t⁻¹].
pub type LMatrix = Vec<Vec<LaurentPoly>>;

pub fn identity(dim: usize) -> LMatrix {
    (0..dim)
        .map(|r| {
            (0..dim)
                .map(|c| if r == c { LaurentPoly::one() } else { LaurentPoly::zero() })
                .collect()
        })
        .collect()
}

pub fn mat_mul(a: &LMatrix, b: &LMatrix) -> LMatrix {
    let dim = a.len();
    assert!(b.len() == dim && (dim == 0 || b[0].len() == dim), "dimension mismatch");
    (0..dim)
        .map(|r| {
            (0..dim)
                .map(|c| {
                    let mut acc = LaurentPoly::zero();
                    for k in 0..dim {
                        if a[r][k].is_zero() || b[k][c].is_zero() {
                            continue;
                        }
                        acc = acc.add(&a[r][k].mul(&b[k][c]));
                    }
                    acc
                })
                .collect()
        })
        .collect()
}

/// Reduced Burau image of a single generator σ_i^{±1} in B_n
/// (`i` 1-based, `1 ≤ i ≤ n−1`).
pub fn burau_generator(strands: u32, i: usize, inverse: bool) -> LMatrix {
    let n = strands as usize;
    assert!(n >= 2 && i >= 1 && i < n, "generator index out of range");
    let mut m = identity(n - 1);
    let col = i - 1;
    if inverse {
        m[i - 1][col] = LaurentPoly::term(-1, -1); // −t⁻¹
        if i >= 2 {
            m[i - 2][col] = LaurentPoly::one();
        }
        if i <= n - 2 {
            m[i][col] = LaurentPoly::t_pow(-1);
        }
    } else {
        m[i - 1][col] = LaurentPoly::term(-1, 1); // −t
        if i >= 2 {
            m[i - 2][col] = LaurentPoly::t();
        }
        if i <= n - 2 {
            m[i][col] = LaurentPoly::one();
        }
    }
    m
}

/// Reduced Burau image of a whole word: ψ(g₁)·ψ(g₂)···ψ(g_k)
/// (left-to-right product; the empty word maps to the identity).
pub fn burau_reduced(w: &BraidWord) -> LMatrix {
    let n = w.strands as usize;
    let dim = n.saturating_sub(1);
    let mut acc = identity(dim);
    for &g in &w.gens {
        let i = gen_index(w.strands, g);
        acc = mat_mul(&acc, &burau_generator(w.strands, i, g < 0));
    }
    acc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(n: u32, gens: &[i32]) -> BraidWord {
        BraidWord::new(n, gens.to_vec())
    }

    #[test]
    fn generator_times_inverse_is_identity() {
        for n in 2..=6u32 {
            for i in 1..n as usize {
                let a = burau_generator(n, i, false);
                let b = burau_generator(n, i, true);
                assert_eq!(mat_mul(&a, &b), identity(n as usize - 1), "σ{i}·σ{i}⁻¹ ≠ I in B_{n}");
                assert_eq!(mat_mul(&b, &a), identity(n as usize - 1), "σ{i}⁻¹·σ{i} ≠ I in B_{n}");
            }
        }
    }

    #[test]
    fn braid_relation_holds() {
        // σ_i σ_{i+1} σ_i = σ_{i+1} σ_i σ_{i+1} — the representation property.
        for n in 3..=6u32 {
            for i in 1..(n as usize - 1) {
                let (a, b) = (i as i32, (i + 1) as i32);
                let lhs = burau_reduced(&word(n, &[a, b, a]));
                let rhs = burau_reduced(&word(n, &[b, a, b]));
                assert_eq!(lhs, rhs, "braid relation failed for σ{i},σ{} in B_{n}", i + 1);
                // Also for inverse generators.
                let lhs_inv = burau_reduced(&word(n, &[-a, -b, -a]));
                let rhs_inv = burau_reduced(&word(n, &[-b, -a, -b]));
                assert_eq!(lhs_inv, rhs_inv, "inverse braid relation failed in B_{n}");
            }
        }
    }

    #[test]
    fn far_commutation_on_matrices() {
        for n in 4..=6u32 {
            for i in 1..n as usize {
                for j in 1..n as usize {
                    if (i as i64 - j as i64).abs() < 2 {
                        continue;
                    }
                    let (a, b) = (i as i32, j as i32);
                    assert_eq!(
                        burau_reduced(&word(n, &[a, b])),
                        burau_reduced(&word(n, &[b, a])),
                        "σ{i}σ{j} ≠ σ{j}σ{i} in B_{n}"
                    );
                }
            }
        }
    }

    #[test]
    fn b2_sigma1_is_minus_t() {
        let m = burau_reduced(&word(2, &[1]));
        assert_eq!(m, vec![vec![LaurentPoly::term(-1, 1)]]);
        let cube = burau_reduced(&word(2, &[1, 1, 1]));
        assert_eq!(cube, vec![vec![LaurentPoly::term(-1, 3)]]); // (−t)³ = −t³
    }

    #[test]
    fn empty_word_is_identity() {
        assert_eq!(burau_reduced(&word(4, &[])), identity(3));
        assert_eq!(burau_reduced(&word(1, &[])), identity(0));
    }
}
