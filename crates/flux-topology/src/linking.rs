//! Linking numbers of the braid closure — a single O(len(word)) sweep.
//!
//! In the standard closure of a braid word every crossing of the link diagram
//! is a crossing of the word (the closure arcs are nested and crossing-free),
//! so the linking number between two closure components is half the signed
//! count of crossings between them. This module decomposes that count per
//! STRAND pair: `linking_matrix(w)[i][j]` = ½ · (signed crossings between
//! strands `i` and `j`). For strand pairs that close into distinct components
//! the count is even and the entry is the exact linking-number contribution;
//! for same-component pairs the halving (truncating toward zero) is a stated
//! convention — the underlying signed count is the invariant either way.

use crate::{gen_index, BraidWord};

/// Signed crossing counts between strand pairs (full, un-halved). Symmetric,
/// zero diagonal.
fn crossing_counts(w: &BraidWord) -> Vec<Vec<i64>> {
    let n = w.strands as usize;
    let mut occupant: Vec<usize> = (0..n).collect(); // occupant[pos] = strand
    let mut cnt = vec![vec![0i64; n]; n];
    for &g in &w.gens {
        let i = gen_index(w.strands, g); // crossing at positions (i−1, i)
        let (a, b) = (occupant[i - 1], occupant[i]);
        let sign = g.signum() as i64;
        cnt[a][b] += sign;
        cnt[b][a] += sign;
        occupant.swap(i - 1, i);
    }
    cnt
}

/// `lk(i,j)` for all strand pairs of the closure: half the signed crossing
/// count (see module doc for the same-component convention).
pub fn linking_matrix(w: &BraidWord) -> Vec<Vec<i64>> {
    let mut m = crossing_counts(w);
    for row in m.iter_mut() {
        for e in row.iter_mut() {
            *e /= 2;
        }
    }
    m
}

/// `linking_matrix(w)[i][j]` without building the caller's copy of the matrix.
pub fn linking_number(w: &BraidWord, i: usize, j: usize) -> i64 {
    let n = w.strands as usize;
    assert!(i < n && j < n, "strand index out of range");
    crossing_counts(w)[i][j] / 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hopf_link_positive_and_negative() {
        let hopf = BraidWord::new(2, vec![1, 1]);
        let m = linking_matrix(&hopf);
        assert_eq!(m, vec![vec![0, 1], vec![1, 0]]);
        assert_eq!(linking_number(&hopf, 0, 1), 1);
        let neg = BraidWord::new(2, vec![-1, -1]);
        assert_eq!(linking_number(&neg, 0, 1), -1);
    }

    #[test]
    fn matrix_is_symmetric_zero_diagonal() {
        let w = BraidWord::new(4, vec![1, 2, -3, 2, 1, -2]);
        let m = linking_matrix(&w);
        for i in 0..4 {
            assert_eq!(m[i][i], 0);
            for j in 0..4 {
                assert_eq!(m[i][j], m[j][i]);
            }
        }
    }

    #[test]
    fn mixed_crossings_cancel() {
        // σ1 σ1⁻¹: the two crossings between strands 0,1 cancel → lk = 0.
        let w = BraidWord::new(2, vec![1, -1]);
        assert_eq!(linking_number(&w, 0, 1), 0);
    }

    #[test]
    fn strand_tracking_follows_strands_not_positions() {
        // σ1 σ2 σ1 on 3 strands: crossings are between strand PAIRS
        // (0,1), (0,2), (1,2) — each pair once.
        let w = BraidWord::new(3, vec![1, 2, 1]);
        let c = crossing_counts(&w);
        assert_eq!(c[0][1], 1);
        assert_eq!(c[0][2], 1);
        assert_eq!(c[1][2], 1);
    }
}
