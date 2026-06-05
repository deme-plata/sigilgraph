//! CHRONOS-B — multiverse fork/diff over the real SIGIL chain.
//!
//! Snapshot a [`SigilSimNode`] at a checkpoint (a `Clone`), fork it into N
//! parallel timelines, run each forward under a different workload/fault,
//! then diff where they diverge. Because `SigilSimNode` is a value type
//! (Phase-0 `SigilState` is BTreeMap-backed), forking is a cheap `.clone()`
//! — 1000 branches from one checkpoint costs 1000 clones, no trait-object
//! factory needed (the limitation that keeps the generic Universe-layer fork
//! harder; here we fork the concrete node directly).
//!
//! The headline demonstration: two branches running the IDENTICAL workload
//! land on byte-identical state roots (determinism survives the fork), while
//! a branch that misses transactions (simulating a node that dropped gossip)
//! lands on a DIFFERENT root — and the diff pinpoints exactly that. This is
//! the tool you reach for to answer "if node X had seen a different subset of
//! txs, where would its state have split from the canonical timeline?"
//!
//! Complements `tourbillon` (ordering-permutation fork) and `property`
//! (random-fuzz fork): tourbillon varies *order*, property varies *random
//! conditions*, multiverse varies *explicit, labelled faults* you want to
//! compare side by side.

use flux_chronos::{secs, NodeId, SimNode, TickId};

use crate::{demo_genesis, GenesisSpec, SigilSimNode};
use sigil_tx::SignedTx;

/// One timeline to explore: a human label + the tail workload it processes
/// from the checkpoint forward.
#[derive(Debug, Clone)]
pub struct Branch {
    /// Human-readable label for the diff report + viz.
    pub label: String,
    /// Transactions this timeline feeds its forked node after the checkpoint.
    pub txs: Vec<SignedTx>,
}

/// What one branch ended up at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchResult {
    /// The branch's label.
    pub label: String,
    /// Chain height at the end of this timeline.
    pub final_height: u64,
    /// Hex of the first 8 bytes of the wallet state root (enough to diff).
    pub wallet_root_hex: String,
    /// Master-wallet native balance at the end (dev-fee accrual visible here).
    pub master_balance: u128,
    /// Divergences this branch's node flagged (should be 0 for honest play).
    pub divergence_count: u64,
    /// Label of the baseline branch this one's wallet root MATCHES, if any.
    /// `Some(baseline)` = converged with baseline; `None` = diverged.
    pub converged_with: Option<String>,
}

/// The full multiverse run.
#[derive(Debug, Clone)]
pub struct MultiverseReport {
    /// Height the checkpoint was taken at.
    pub checkpoint_height: u64,
    /// One result per branch, in input order. The first branch is the
    /// baseline every other branch is diffed against.
    pub branches: Vec<BranchResult>,
}

impl MultiverseReport {
    /// How many branches diverged from the baseline (different wallet root).
    pub fn divergent_branches(&self) -> usize {
        self.branches.iter().filter(|b| b.converged_with.is_none() && !b.is_baseline()).count()
    }

    /// JSON for the CHRONOS-G browser viz (`multiverse.html`). Self-contained,
    /// no chain types leak — just the diff-able scalars.
    pub fn to_json(&self) -> String {
        let mut items = Vec::new();
        for (i, b) in self.branches.iter().enumerate() {
            items.push(format!(
                "{{\"label\":{:?},\"height\":{},\"wallet_root\":{:?},\"master_balance\":{},\"divergence\":{},\"is_baseline\":{},\"converged_with\":{}}}",
                b.label,
                b.final_height,
                b.wallet_root_hex,
                b.master_balance,
                b.divergence_count,
                i == 0,
                match &b.converged_with { Some(l) => format!("{:?}", l), None => "null".into() },
            ));
        }
        format!(
            "{{\"checkpoint_height\":{},\"branches\":[{}]}}",
            self.checkpoint_height,
            items.join(",")
        )
    }
}

impl BranchResult {
    fn is_baseline(&self) -> bool {
        self.converged_with.as_deref() == Some(self.label.as_str())
    }
}

fn root_hex(node: &SigilSimNode) -> String {
    let r = node.roots().wallet_state_root;
    format!("{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        r[0], r[1], r[2], r[3], r[4], r[5], r[6], r[7])
}

/// Drive a producer node forward by feeding `txs` and stepping once per tx
/// (one block per tx — the Phase-0 production model). Returns when the
/// workload is exhausted.
fn drive(node: &mut SigilSimNode, txs: Vec<SignedTx>, start_tick: TickId) {
    let count = txs.len();
    for tx in txs {
        node.enqueue_tx(tx);
    }
    // Step once per queued tx; each step mints one block. Extra steps on an
    // empty mempool are harmless no-ops.
    for k in 0..count {
        let _ = node.step(start_tick + secs(1) * (k as u64 + 1), &[]);
    }
}

/// Build a producer node from `genesis`, run `base_txs` to a checkpoint, and
/// return the checkpoint node ready to fork.
pub fn checkpoint(genesis: &GenesisSpec, base_txs: Vec<SignedTx>) -> SigilSimNode {
    // peers=[dummy] so the producer's step emits (harmless here; we don't
    // gossip). block_time arbitrary — we step manually.
    let mut node = SigilSimNode::new("checkpoint", NodeId(0), vec![NodeId(1)], true, secs(1), genesis);
    drive(&mut node, base_txs, 0);
    node
}

/// Fork `checkpoint` into one node per branch, run each branch's tail
/// workload, and diff the resulting wallet roots. The FIRST branch is the
/// baseline; every other branch is marked converged (root matches baseline)
/// or divergent.
pub fn run_multiverse(checkpoint_node: &SigilSimNode, branches: Vec<Branch>) -> MultiverseReport {
    let checkpoint_height = checkpoint_node.height();
    let base_tick = secs(1) * checkpoint_height; // continue the clock past checkpoint

    // Run every branch on its own clone.
    let mut roots: Vec<(String, String, u64, u128, u64)> = Vec::new(); // label, root, height, master_bal, divergence
    for br in &branches {
        let mut forked = checkpoint_node.clone();
        drive(&mut forked, br.txs.clone(), base_tick);
        roots.push((
            br.label.clone(),
            root_hex(&forked),
            forked.height(),
            forked.balance_of(&[0x99u8; 32], &sigil_tx::NATIVE),
            forked.divergence_count,
        ));
    }

    // Baseline = first branch. Diff every branch's root against it.
    let baseline_label = roots.first().map(|r| r.0.clone()).unwrap_or_default();
    let baseline_root = roots.first().map(|r| r.1.clone()).unwrap_or_default();

    let results = roots
        .into_iter()
        .map(|(label, root, height, master_bal, divergence)| {
            let converged_with = if label == baseline_label {
                Some(baseline_label.clone()) // baseline converges with itself
            } else if root == baseline_root {
                Some(baseline_label.clone())
            } else {
                None
            };
            BranchResult {
                label,
                final_height: height,
                wallet_root_hex: root,
                master_balance: master_bal,
                divergence_count: divergence,
                converged_with,
            }
        })
        .collect();

    MultiverseReport { checkpoint_height, branches: results }
}

/// Convenience: the standard demo multiverse used by the browser viz +
/// docs. Checkpoint after 10 sends, then fork into three timelines:
/// honest, honest-replay (identical), and missed-half (dropped 5 txs).
pub fn demo_multiverse() -> MultiverseReport {
    use sigil_tx::{SigilTx, NATIVE};
    let g = demo_genesis();
    let w = |t: u8| [t; 32];
    let send = |n: u32| {
        crate::sign_dummy(SigilTx::Send {
            from: w((n % 5 + 1) as u8),
            to: w(((n + 1) % 5 + 1) as u8),
            amount: 10,
            token: NATIVE,
            fee: 1,
        })
    };

    let base: Vec<SignedTx> = (0..10).map(send).collect();
    let cp = checkpoint(&g, base);

    let honest: Vec<SignedTx> = (10..20).map(send).collect();
    let honest_replay = honest.clone();
    let missed_half: Vec<SignedTx> = (10..15).map(send).collect(); // only 5 of the 10

    run_multiverse(
        &cp,
        vec![
            Branch { label: "honest".into(), txs: honest },
            Branch { label: "honest-replay".into(), txs: honest_replay },
            Branch { label: "missed-half".into(), txs: missed_half },
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_branches_converge_divergent_branch_splits() {
        let report = demo_multiverse();
        assert_eq!(report.branches.len(), 3);

        let honest = &report.branches[0];
        let replay = &report.branches[1];
        let missed = &report.branches[2];

        // Determinism across the fork: identical workload → identical root.
        assert_eq!(
            honest.wallet_root_hex, replay.wallet_root_hex,
            "identical timelines must converge on the same root"
        );
        assert_eq!(replay.converged_with.as_deref(), Some("honest"));

        // The branch that missed txs lands on a DIFFERENT root → the diff
        // pinpoints it as divergent.
        assert_ne!(
            missed.wallet_root_hex, honest.wallet_root_hex,
            "a timeline that missed txs must diverge"
        );
        assert_eq!(missed.converged_with, None);
        assert_eq!(report.divergent_branches(), 1);
    }

    #[test]
    fn no_branch_flags_internal_divergence() {
        // Honest single-producer play never trips the exit-78 root-mismatch
        // (that's about follower-vs-producer; here each node is its own
        // producer). All divergence_counts must be 0.
        let report = demo_multiverse();
        for b in &report.branches {
            assert_eq!(b.divergence_count, 0, "branch {} flagged divergence", b.label);
        }
    }

    #[test]
    fn report_json_is_wellformed() {
        let report = demo_multiverse();
        let j = report.to_json();
        assert!(j.contains("\"checkpoint_height\":11")); // genesis(0)+10 sends → height 11
        assert!(j.contains("\"label\":\"honest\""));
        assert!(j.contains("\"label\":\"missed-half\""));
        assert!(j.contains("\"is_baseline\":true"));
        // serde-parse it back to prove it's valid JSON.
        let v: serde_json::Value = serde_json::from_str(&j).expect("valid JSON");
        assert_eq!(v["branches"].as_array().unwrap().len(), 3);
    }
}
