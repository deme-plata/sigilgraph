//! The root ledger: everything a node has said about a height, keyed by height, so any node can be
//! checked at any height it reports — including a follower that lags the producer by hundreds of blocks.
//!
//! Three facts are recorded per (height, node):
//! - `spine`  — the certificate's `spine_block_hash` (committee members only; every block).
//! - `block`  — the block hash from `/v1/dagknight/recent` (every node; the last 200 blocks).
//! - `roots`  — the four state roots + supply + shielded anchor from `/v1/integrity` at its applied height.
//!
//! Block hashes are compared at EQUAL height (exact: a hash is tied to its height).
//!
//! Live roots are compared at EQUAL MONEY SUPPLY, not at equal reported height (2026-11-11). A node reads its
//! height and its state at two different instants, so the pair is often one block apart — measured on g3: 128 of
//! 321 reported heights showed two different states, on the block-maker itself as much as on followers. The roots
//! and the supply, however, are read together from one state, and the supply grows with every block (the block
//! reward), so it names the state exactly: 2,867 samples from three nodes, 321 supplies, each with exactly one
//! state. Two nodes at the same supply with different roots hold the same amount of money distributed
//! differently — a real disagreement, with no read race left to blame.
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Roots {
    pub wallet: String,
    pub contract: String,
    pub dex: String,
    pub event_log: String,
    pub native_supply: String,
    pub anchor: String,
    pub notes: u64,
    pub nullifiers: u64,
}

/// Which of the eight root fields differ between two samples.
pub fn roots_diff(a: &Roots, b: &Roots) -> Vec<&'static str> {
    let mut d = Vec::new();
    if a.wallet != b.wallet { d.push("wallet"); }
    if a.contract != b.contract { d.push("contract"); }
    if a.dex != b.dex { d.push("dex"); }
    if a.event_log != b.event_log { d.push("event_log"); }
    if a.native_supply != b.native_supply { d.push("native_supply"); }
    if a.anchor != b.anchor { d.push("anchor"); }
    if a.notes != b.notes { d.push("notes"); }
    if a.nullifiers != b.nullifiers { d.push("nullifiers"); }
    d
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HeightRow {
    pub spine: HashMap<String, String>,
    pub block: HashMap<String, String>,
    pub roots: HashMap<String, Roots>,
    pub first_seen_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum Verdict {
    /// The first voice at this height — nothing to compare yet.
    First,
    /// Same as every other node that has spoken at this height.
    Agree { with: usize },
    /// Differs from at least one other node at this height.
    Disagree { against: Vec<String> },
}

/// Two samples with the same money supply are compared only when their reported heights are this close: the
/// height may lag or lead its state by a block or two, never by more. (Equal supply far apart in height is not
/// the same state: a different chain, or a stretch of blocks without a reward.)
pub const CLOCK_HEIGHT_SLACK: u64 = 2;

/// The money supply as the state's clock; `None` when a node does not report one (then the height is all we have).
fn supply_clock(r: &Roots) -> Option<u128> {
    r.native_supply.trim().parse::<u128>().ok().filter(|s| *s > 0)
}

#[derive(Debug, Default)]
pub struct Ledger {
    pub rows: BTreeMap<u64, HeightRow>,
    /// what each node showed at a given money supply: node → (the height it reported, its roots)
    pub states: BTreeMap<u128, HashMap<String, (u64, Roots)>>,
    /// how many heights to keep (the producer runs ~3 blk/s; 20k ≈ 2 h)
    pub retain: usize,
}

impl Ledger {
    pub fn new(retain: usize) -> Self {
        Self { rows: BTreeMap::new(), states: BTreeMap::new(), retain }
    }

    fn row(&mut self, height: u64, now_ms: u64) -> &mut HeightRow {
        let r = self.rows.entry(height).or_default();
        if r.first_seen_ms == 0 {
            r.first_seen_ms = now_ms;
        }
        r
    }

    fn trim(&mut self) {
        while self.rows.len() > self.retain {
            if let Some(&h) = self.rows.keys().next() {
                self.rows.remove(&h);
            } else {
                break;
            }
        }
    }

    fn compare<'a>(mine: &str, node: &str, others: impl Iterator<Item = (&'a String, &'a String)>) -> Verdict {
        let mut with = 0usize;
        let mut against = Vec::new();
        for (n, v) in others {
            if n == node {
                continue;
            }
            if v == mine {
                with += 1;
            } else {
                against.push(n.clone());
            }
        }
        if !against.is_empty() {
            Verdict::Disagree { against }
        } else if with > 0 {
            Verdict::Agree { with }
        } else {
            Verdict::First
        }
    }

    /// A committee certificate: the spine block hash at `height`, from `node`.
    pub fn record_spine(&mut self, node: &str, height: u64, hash: &str, now_ms: u64) -> Verdict {
        let row = self.row(height, now_ms);
        // a spine hash must also agree with every BLOCK hash any node reported at this height
        let mut all: Vec<(String, String)> = row.spine.iter().map(|(n, h)| (n.clone(), h.clone())).collect();
        all.extend(row.block.iter().map(|(n, h)| (format!("{n}:block"), h.clone())));
        row.spine.insert(node.to_string(), hash.to_string());
        let v = Self::compare(hash, node, all.iter().map(|(a, b)| (a, b)));
        self.trim();
        v
    }

    /// A block hash at `height` as `node` holds it (from `/v1/dagknight/recent`).
    pub fn record_block(&mut self, node: &str, height: u64, hash: &str, now_ms: u64) -> Verdict {
        let row = self.row(height, now_ms);
        let mut all: Vec<(String, String)> = row.block.iter().map(|(n, h)| (n.clone(), h.clone())).collect();
        all.extend(row.spine.iter().map(|(n, h)| (format!("{n}:spine"), h.clone())));
        row.block.insert(node.to_string(), hash.to_string());
        let v = Self::compare(hash, node, all.iter().map(|(a, b)| (a, b)));
        self.trim();
        v
    }

    /// The state roots `node` reports. Compared with what other nodes showed at the SAME MONEY SUPPLY (see the
    /// module text); a repeat of what the node already said is not a new verdict. `against` names the other node
    /// AND the fields that differ, e.g. `node7[wallet]`. A node that reports no supply is compared by height.
    pub fn record_roots(&mut self, node: &str, height: u64, roots: Roots, now_ms: u64) -> Verdict {
        let Some(clock) = supply_clock(&roots) else {
            return self.record_roots_by_height(node, height, roots, now_ms);
        };
        let row = self.states.entry(clock).or_default();
        if row.get(node).is_some_and(|(_, r)| *r == roots) {
            return Verdict::First;
        }
        let mut with = 0usize;
        let mut against = Vec::new();
        for (n, (h, r)) in row.iter() {
            if n == node || h.abs_diff(height) > CLOCK_HEIGHT_SLACK {
                continue;
            }
            if *r == roots {
                with += 1;
            } else {
                against.push(format!("{n}[{}]", roots_diff(r, &roots).join(",")));
            }
        }
        row.insert(node.to_string(), (height, roots));
        while self.states.len() > self.retain.max(1) {
            self.states.pop_first();
        }
        if !against.is_empty() {
            Verdict::Disagree { against }
        } else if with > 0 {
            Verdict::Agree { with }
        } else {
            Verdict::First
        }
    }

    /// The old rule, for a node that reports no supply: compare at equal reported height (racy; the tracker
    /// only believes a mismatch that persists).
    fn record_roots_by_height(&mut self, node: &str, height: u64, roots: Roots, now_ms: u64) -> Verdict {
        let row = self.row(height, now_ms);
        if row.roots.get(node) == Some(&roots) {
            return Verdict::First;
        }
        let mut with = 0usize;
        let mut against = Vec::new();
        for (n, r) in row.roots.iter() {
            if n == node {
                continue;
            }
            if *r == roots {
                with += 1;
            } else {
                against.push(format!("{n}[{}]", roots_diff(r, &roots).join(",")));
            }
        }
        row.roots.insert(node.to_string(), roots);
        self.trim();
        if !against.is_empty() {
            Verdict::Disagree { against }
        } else if with > 0 {
            Verdict::Agree { with }
        } else {
            Verdict::First
        }
    }

    /// The highest height at which at least `n` nodes have spoken (spine or block) and all agree.
    pub fn last_common_agreement(&self, n: usize) -> Option<(u64, usize)> {
        self.rows.iter().rev().find_map(|(h, row)| {
            let mut hashes: Vec<&String> = row.spine.values().collect();
            hashes.extend(row.block.values());
            let voices = row.spine.len() + row.block.len();
            if voices >= n && hashes.iter().all(|x| *x == hashes[0]) {
                Some((*h, voices))
            } else {
                None
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// a node that reports no supply (compared by height, the old rule)
    fn roots(w: &str) -> Roots {
        Roots { wallet: w.into(), contract: "0".into(), dex: "0".into(), event_log: "0".into(), native_supply: "".into(), anchor: "a".into(), notes: 1, nullifiers: 0 }
    }

    /// the state after `blocks` blocks: its own supply, and the wallet root that state has (`w`)
    fn state(blocks: u64, w: &str) -> Roots {
        Roots { native_supply: (blocks * 104_000_000).to_string(), ..roots(w) }
    }

    #[test]
    fn a_height_read_one_block_off_is_not_a_disagreement() {
        // The measured g3 shape: both nodes hold identical chains, each reports its height a block early or late.
        let mut l = Ledger::new(100);
        assert_eq!(l.record_roots("epsilon", 100, state(100, "w100"), 1), Verdict::First);
        // node7 says "height 100" while its state is already block 101's: the old rule called this a disagreement
        assert_eq!(l.record_roots("node7", 100, state(101, "w101"), 2), Verdict::First);
        // epsilon reaches that state and reports it under height 101, or still under 100 — same state either way
        assert_eq!(l.record_roots("epsilon", 101, state(101, "w101"), 3), Verdict::Agree { with: 1 });
        assert_eq!(l.record_roots("node8", 100, state(101, "w101"), 4), Verdict::Agree { with: 2 });
    }

    #[test]
    fn the_same_supply_with_different_roots_is_a_real_disagreement() {
        let mut l = Ledger::new(100);
        l.record_roots("epsilon", 100, state(100, "w100"), 1);
        let v = l.record_roots("node7", 101, state(100, "wXXX"), 2); // same money, distributed differently
        assert_eq!(v, Verdict::Disagree { against: vec!["epsilon[wallet]".into()] });
        // the same supply far apart in height is not the same state and is not compared
        assert_eq!(l.record_roots("node8", 500, state(100, "wYYY"), 3), Verdict::First);
        // a repeat is not a new verdict
        assert_eq!(l.record_roots("node7", 101, state(100, "wXXX"), 4), Verdict::First);
    }

    #[test]
    fn the_state_table_is_trimmed_oldest_first() {
        let mut l = Ledger::new(3);
        for b in 1..=5u64 {
            l.record_roots("e", b, state(b, "w"), b);
        }
        assert_eq!(l.states.len(), 3);
        assert_eq!(*l.states.keys().next().unwrap(), 3 * 104_000_000);
    }

    #[test]
    fn spine_then_block_agree_and_disagree() {
        let mut l = Ledger::new(100);
        assert_eq!(l.record_spine("epsilon", 10, "aa", 1), Verdict::First);
        assert_eq!(l.record_spine("happysrv", 10, "aa", 2), Verdict::Agree { with: 1 });
        // a lagging follower reports the block later — checked against the spine already on file
        assert_eq!(l.record_block("node3", 10, "aa", 90), Verdict::Agree { with: 2 });
        assert_eq!(l.record_block("node4", 10, "bb", 91).normalize(), Verdict::Disagree { against: vec!["epsilon:spine".into(), "happysrv:spine".into(), "node3".into()] }.normalize());
    }

    #[test]
    fn without_a_supply_roots_compare_at_equal_height() {
        let mut l = Ledger::new(100);
        assert_eq!(l.record_roots("epsilon", 5, roots("w1"), 1), Verdict::First);
        assert_eq!(l.record_roots("happysrv", 6, roots("w2"), 1), Verdict::First); // different height: no verdict
        assert_eq!(l.record_roots("happysrv", 5, roots("w1"), 2), Verdict::Agree { with: 1 });
        assert!(matches!(l.record_roots("node3", 5, roots("w9"), 3), Verdict::Disagree { .. }));
        // the same node repeating the same roots at the same height is not a new verdict
        assert_eq!(l.record_roots("node3", 5, roots("w9"), 4), Verdict::First);
        assert_eq!(roots_diff(&roots("w1"), &roots("w9")), vec!["wallet"]);
    }

    #[test]
    fn retention_trims_oldest() {
        let mut l = Ledger::new(3);
        for h in 1..=5 {
            l.record_spine("e", h, "x", h);
        }
        assert_eq!(l.rows.keys().copied().collect::<Vec<_>>(), vec![3, 4, 5]);
    }

    #[test]
    fn last_common_agreement_wants_n_voices() {
        let mut l = Ledger::new(100);
        l.record_spine("e", 1, "x", 1);
        l.record_spine("e", 2, "y", 1);
        l.record_block("n3", 1, "x", 1);
        assert_eq!(l.last_common_agreement(2), Some((1, 2)));
        assert_eq!(l.last_common_agreement(3), None);
    }

    impl Verdict {
        /// order-insensitive form for the assertion above
        fn normalize(self) -> Self {
            match self {
                Verdict::Disagree { mut against } => {
                    against.sort();
                    Verdict::Disagree { against }
                }
                v => v,
            }
        }
    }
}
