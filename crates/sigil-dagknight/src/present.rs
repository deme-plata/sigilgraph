//! Braid-word extractor — an Artin-generator presentation of the window's
//! merge topology, consumed by `flux-topology` (QTFT-1).
//!
//! **PRESENTATION CONVENTION (this is a convention over the deterministic
//! linearization, not a physical-braid claim):**
//!
//! - **Strands** = the distinct producers of resident blocks with height in
//!   `[from_height, to_height]`, ranked by ascending byte-order of producer
//!   id — strand `i` is `producers[i]`. The ranking is deterministic, so two
//!   nodes with the same window derive the same strand indexing.
//! - Walk the deterministic linearization restricted to the window, keeping a
//!   left-to-right strand arrangement that starts as the identity (strand `i`
//!   at position `i`).
//! - For each block `B` (strand `s`) and each of its merge parents `M`, in
//!   header order, where `M` is resident, in-window, and on a different
//!   strand `t`: strand `s` walks adjacent-swap-by-swap from its current
//!   position until it sits next to strand `t`, permanently updating the
//!   arrangement. Each adjacent swap at positions `(i, i+1)` (0-based) emits
//!   the Artin generator `±(i+1)`: **positive iff the overtaking (moving)
//!   strand's producer id is byte-wise smaller than the overtaken strand's
//!   producer id**, negative otherwise.
//! - Merge edges to out-of-window / non-resident blocks or to the same
//!   strand contribute nothing. A linear chain (no merge edges) therefore
//!   yields the **empty word**, regardless of how many producers alternate.
//!
//! `word[k] = ±(i+1)` encodes `σ_i^{±1}` with 1-based generator index — the
//! encoding `flux-topology::BraidWord` consumes (`BraidWord { strands,
//! gens: word }`).
//!
//! Blocks cleaned below the braid's retention band (more than `final_depth`
//! below the finalized height) are no longer resident and are skipped; call
//! this on windows within the retained range (the QTFT contract windows are
//! ≤ `final_depth` wide).

use std::collections::HashMap;

use crate::braid::Braid;

/// Artin-generator presentation of one window of the braid. See the module
/// doc for the exact convention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BraidPresentation {
    /// Number of strands (distinct in-window producers).
    pub strands: u32,
    /// Braid word: `±(i+1)` = `σ_i^{±1}`, 1-based generator index.
    pub word: Vec<i32>,
    /// Strand index → producer id (ascending byte-order — the ranking).
    pub producers: Vec<[u8; 32]>,
}

impl Braid {
    /// Extract the braid word for the window `[from_height, to_height]`
    /// (inclusive) per the module-doc convention. Deterministic: a pure
    /// function of the DAG contents and the window bounds.
    pub fn braid_word(&self, from_height: u64, to_height: u64) -> BraidPresentation {
        if from_height > to_height {
            return BraidPresentation {
                strands: 0,
                word: Vec::new(),
                producers: Vec::new(),
            };
        }

        // Window blocks in deterministic linearized order, resident only.
        let in_window = |h: u64| h >= from_height && h <= to_height;
        let ordered: Vec<_> = self
            .linearize()
            .into_iter()
            .filter_map(|hash| self.view_of(&hash))
            .filter(|v| in_window(v.height))
            .collect();

        // Strand ranking: ascending producer id.
        let mut producers: Vec<[u8; 32]> = ordered.iter().map(|v| v.producer).collect();
        producers.sort_unstable();
        producers.dedup();
        let strand_of: HashMap<[u8; 32], usize> = producers
            .iter()
            .enumerate()
            .map(|(i, p)| (*p, i))
            .collect();

        let n = producers.len();
        // pos_of[strand] = current position; arr[position] = strand.
        let mut arr: Vec<usize> = (0..n).collect();
        let mut pos_of: Vec<usize> = (0..n).collect();
        let mut word: Vec<i32> = Vec::new();

        let swap_adjacent = |arr: &mut Vec<usize>,
                                 pos_of: &mut Vec<usize>,
                                 word: &mut Vec<i32>,
                                 left: usize,
                                 moving: usize| {
            let a = arr[left];
            let b = arr[left + 1];
            let passed = if a == moving { b } else { a };
            arr.swap(left, left + 1);
            pos_of[arr[left]] = left;
            pos_of[arr[left + 1]] = left + 1;
            let gen = (left + 1) as i32;
            let sign_pos = producers[moving] < producers[passed];
            word.push(if sign_pos { gen } else { -gen });
        };

        for view in &ordered {
            let s = strand_of[&view.producer];
            for mp in &view.merge_parents {
                let Some(mv) = self.view_of(mp) else { continue };
                if !in_window(mv.height) {
                    continue;
                }
                let Some(&t) = strand_of.get(&mv.producer) else {
                    continue;
                };
                if t == s {
                    continue;
                }
                // Move strand s until adjacent to strand t.
                loop {
                    let ps = pos_of[s];
                    let pt = pos_of[t];
                    if ps.abs_diff(pt) <= 1 {
                        break;
                    }
                    if ps < pt {
                        swap_adjacent(&mut arr, &mut pos_of, &mut word, ps, s);
                    } else {
                        swap_adjacent(&mut arr, &mut pos_of, &mut word, ps - 1, s);
                    }
                }
            }
        }

        BraidPresentation {
            strands: n as u32,
            word,
            producers,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::BlockView;
    use crate::{BlockHash, BraidConfig};

    // Offset by 1 so h(0) never collides with the all-zero genesis parent.
    fn h(n: u8) -> BlockHash {
        [n + 1; 32]
    }

    const PA: [u8; 32] = [0xAA; 32];
    const PB: [u8; 32] = [0xBB; 32];
    const PC: [u8; 32] = [0xCC; 32];

    fn v(
        hash: BlockHash,
        parent: BlockHash,
        merge_parents: Vec<BlockHash>,
        height: u64,
        producer: [u8; 32],
    ) -> BlockView {
        BlockView {
            hash,
            parent,
            merge_parents,
            height,
            producer,
        }
    }

    fn feed(views: &[BlockView]) -> Braid {
        let mut b = Braid::new(BraidConfig::default());
        for view in views {
            b.insert(view.clone());
        }
        b
    }

    #[test]
    fn linear_chain_yields_empty_word() {
        // Alternating producers but NO merge edges: still the empty word.
        let views = vec![
            v(h(0), [0u8; 32], vec![], 0, PA),
            v(h(1), h(0), vec![], 1, PB),
            v(h(2), h(1), vec![], 2, PA),
            v(h(3), h(2), vec![], 3, PB),
        ];
        let p = feed(&views).braid_word(0, 3);
        assert_eq!(p.strands, 2);
        assert_eq!(p.producers, vec![PA, PB]);
        assert!(p.word.is_empty());
    }

    #[test]
    fn adjacent_strand_merge_yields_no_crossing() {
        // Two strands are already adjacent — a merge between them emits
        // nothing under the walk-until-adjacent convention.
        let views = vec![
            v(h(0), [0u8; 32], vec![], 0, PA),
            v(h(1), h(0), vec![], 1, PA),
            v(h(2), h(0), vec![], 1, PB),
            v(h(3), h(1), vec![h(2)], 2, PA),
        ];
        let p = feed(&views).braid_word(0, 2);
        assert_eq!(p.strands, 2);
        assert!(p.word.is_empty());
    }

    #[test]
    fn three_strand_reach_over_emits_signed_generator() {
        // PA(strand 0) merges PC(strand 2): strand 0 must pass strand 1 (PB)
        // — one positive crossing σ₁ (moving producer PA < passed PB).
        let views = vec![
            v(h(0), [0u8; 32], vec![], 0, PA),
            v(h(1), h(0), vec![], 1, PA),
            v(h(2), h(0), vec![], 1, PB),
            v(h(3), h(0), vec![], 1, PC),
            v(h(4), h(1), vec![h(3)], 2, PA),
        ];
        let p = feed(&views).braid_word(0, 2);
        assert_eq!(p.strands, 3);
        assert_eq!(p.producers, vec![PA, PB, PC]);
        assert_eq!(p.word, vec![1]);

        // Mirror: PC (largest id) merges PA — moving PC > passed PB → −2
        // (strand 2 swaps with strand 1 at positions (1,2) → generator 2).
        let views2 = vec![
            v(h(0), [0u8; 32], vec![], 0, PA),
            v(h(1), h(0), vec![], 1, PA),
            v(h(2), h(0), vec![], 1, PB),
            v(h(3), h(0), vec![], 1, PC),
            v(h(4), h(3), vec![h(1)], 2, PC),
        ];
        let p2 = feed(&views2).braid_word(0, 2);
        assert_eq!(p2.word, vec![-2]);
    }

    #[test]
    fn braid_word_deterministic_across_arrival_orders() {
        let views = vec![
            v(h(0), [0u8; 32], vec![], 0, PA),
            v(h(1), h(0), vec![], 1, PA),
            v(h(2), h(0), vec![], 1, PB),
            v(h(3), h(0), vec![], 1, PC),
            v(h(4), h(1), vec![h(3)], 2, PA),
            v(h(5), h(2), vec![h(1)], 2, PB),
            v(h(6), h(3), vec![h(5), h(4)], 3, PC),
        ];
        let forward = feed(&views).braid_word(0, 3);
        let mut reversed = views.clone();
        reversed.reverse();
        let backward = feed(&reversed).braid_word(0, 3);
        assert_eq!(forward, backward);
        assert_eq!(forward.strands, 3);
        assert!(!forward.word.is_empty());

        // Sub-window is also deterministic and different.
        let sub_a = feed(&views).braid_word(1, 2);
        let sub_b = feed(&reversed).braid_word(1, 2);
        assert_eq!(sub_a, sub_b);
    }
}
