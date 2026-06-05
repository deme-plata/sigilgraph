//! scaling.rs — horizontal-scaling + node-density harness.
//!
//! Answers, on REAL OS threads over REAL cores (not virtual time — this is the
//! one chronos module that measures wall-clock parallelism, because "does it
//! scale horizontally" is a wall-clock question):
//!
//!   1. Does aggregate throughput scale horizontally?  → ARCHIVE shard regime
//!   2. Do replicas of one chain add speed? (no)        → REPLICA regime
//!   3. How cheap is a 10 ms tip-verify-only node, and  → LIGHT regime
//!      how many fit per host?
//!
//! Two node CLASSES, matching the real SIGIL topology Viktor specified:
//!   • ARCHIVE node — full `apply_tx` + `commit_state_transition` + 4 roots
//!                    (+ would persist block history). RAM grows with state.
//!                    Few of these; we have 80 TB so they keep everything.
//!   • LIGHT node   — receives roots, does ONLY the 10 ms-gate tip-proof
//!                    `verify`, keeps just the current tip. RAM is O(1) in
//!                    chain length — NO archive history. Thousands per VPS.
//!
//! The honest scaling law this produces:
//!   - SHARD (independent chains, 1 thread each): aggregate TPS rises ~linearly
//!     with N until the host's cores saturate, then plateaus. Real horizontal
//!     scaling — but it's N independent lanes, not one faster chain.
//!   - REPLICA (N validators, same chain): aggregate USEFUL TPS falls ~1/N
//!     because the host redoes the same chain's work N times. Redundancy, not
//!     speed. This is why "add more nodes to go faster" is a category error.
//!   - LIGHT: per-verify cost is ~constant and tiny; thousands of light nodes
//!     are CPU-bound by verify rate, not RAM — the node-count ceiling is huge.

use std::thread;
use std::time::Instant;

use sigil_state::{commit_state_transition, SigilState, StateRoots, StateTransition};
use sigil_tip_proof::{TipProof, NETWORK_ID_BYTES};

use crate::throughput::run_throughput;

/// One measured point on a scaling curve.
#[derive(Debug, Clone)]
pub struct ScalingPoint {
    /// "archive-shard" | "replica" | "light".
    pub regime: &'static str,
    /// Number of parallel nodes (= OS threads).
    pub nodes: u64,
    /// Aggregate operations/sec across all nodes (wall-clock).
    pub agg_ops: f64,
    /// Average per-node operations/sec.
    pub per_node_ops: f64,
    /// Total operations completed (txs for archive, verifies for light).
    pub total_ops: u64,
    /// Wall-clock for the whole parallel run.
    pub wall_ms: u128,
    /// Aggregate speedup vs the N=1 baseline of the same regime.
    pub speedup: f64,
    /// Ideal linear speedup would be `nodes`; this is `speedup / nodes`.
    pub efficiency: f64,
}

impl ScalingPoint {
    /// One table row.
    pub fn line(&self) -> String {
        format!(
            "{:>13} │ N={:>4} │ {:>12.0} ops/s │ {:>10.0}/node │ {:>5.1}× speedup │ {:>3.0}% eff │ {}ms",
            self.regime, self.nodes, self.agg_ops, self.per_node_ops,
            self.speedup, self.efficiency * 100.0, self.wall_ms
        )
    }
}

/// A representative committed root set (empty genesis). Cheap; reused as the
/// tip the light nodes verify against — verify cost is independent of the root
/// values, so this is a faithful stand-in for "the current tip".
fn sample_roots() -> StateRoots {
    let mut s = SigilState::new();
    commit_state_transition(&mut s, &StateTransition { at_height: 0, mutations: vec![] }, 0)
        .expect("empty genesis must commit")
}

/// ARCHIVE shard regime: `nodes` independent full chains, one OS thread each.
/// Each thread plays the real apply+commit pipeline. Aggregate TPS = real
/// wall-clock throughput across all lanes — the horizontal-scaling curve.
pub fn run_archive_shard(nodes: u64, blocks: u64, tpb: u64, wallets: u64) -> ScalingPoint {
    let nodes = nodes.max(1);
    let t = Instant::now();
    let handles: Vec<_> = (0..nodes)
        .map(|_| thread::spawn(move || run_throughput(blocks, tpb, wallets)))
        .collect();
    let mut total_ops = 0u64;
    for h in handles {
        total_ops += h.join().expect("shard thread").txs;
    }
    let wall_ms = t.elapsed().as_millis();
    let secs = (wall_ms as f64 / 1000.0).max(1e-6);
    let agg = total_ops as f64 / secs;
    ScalingPoint {
        regime: "archive-shard",
        nodes,
        agg_ops: agg,
        per_node_ops: agg / nodes as f64,
        total_ops,
        wall_ms,
        speedup: 0.0,
        efficiency: 0.0,
    }
}

/// REPLICA regime: ONE chain, `nodes` validators that each redo the full chain
/// work (serially — the host has to do every node's verification). Models
/// adding validators to a single chain. The USEFUL throughput is one chain's
/// worth; reporting it against the N×-larger wall shows it falling ~1/N.
pub fn run_replica(nodes: u64, blocks: u64, tpb: u64, wallets: u64) -> ScalingPoint {
    let nodes = nodes.max(1);
    let t = Instant::now();
    let mut chain_txs = 0u64;
    for _ in 0..nodes {
        // Every validator independently applies the same chain.
        chain_txs = run_throughput(blocks, tpb, wallets).txs;
    }
    let wall_ms = t.elapsed().as_millis();
    let secs = (wall_ms as f64 / 1000.0).max(1e-6);
    // Useful output is ONE chain (chain_txs), not N×, because all N nodes agree
    // on the same blocks. Dividing by the N×-larger wall is the honest "useful
    // throughput per unit host effort".
    let agg = chain_txs as f64 / secs;
    ScalingPoint {
        regime: "replica",
        nodes,
        agg_ops: agg,
        per_node_ops: agg,
        total_ops: chain_txs,
        wall_ms,
        speedup: 0.0,
        efficiency: 0.0,
    }
}

/// LIGHT regime: `nodes` tip-verify-only light clients, one OS thread each.
/// Each thread verifies `verifies` tip-proofs (the 10 ms gate) while holding
/// ONLY the current tip — O(1) memory, no block history. This is the node
/// class you pack thousands of onto one VPS.
pub fn run_light(nodes: u64, verifies: u64) -> ScalingPoint {
    let nodes = nodes.max(1);
    let roots = sample_roots();
    let t = Instant::now();
    let handles: Vec<_> = (0..nodes)
        .map(|_| {
            let r = roots; // Copy — each light node's tiny constant state
            thread::spawn(move || {
                // Build the tip once (the proof a producer gossiped); a real
                // light node holds exactly this and nothing else.
                let tip = TipProof::new_blake3(1, r);
                let mut ok = 0u64;
                for _ in 0..verifies {
                    if tip.verify(NETWORK_ID_BYTES).is_ok() {
                        ok += 1;
                    }
                }
                ok
            })
        })
        .collect();
    let mut total_ops = 0u64;
    for h in handles {
        total_ops += h.join().expect("light thread");
    }
    let wall_ms = t.elapsed().as_millis();
    let secs = (wall_ms as f64 / 1000.0).max(1e-6);
    let agg = total_ops as f64 / secs;
    ScalingPoint {
        regime: "light",
        nodes,
        agg_ops: agg,
        per_node_ops: agg / nodes as f64,
        total_ops,
        wall_ms,
        speedup: 0.0,
        efficiency: 0.0,
    }
}

/// Sweep a regime across node counts, filling in speedup + efficiency vs N=1.
pub fn sweep(
    regime: &str,
    counts: &[u64],
    blocks: u64,
    tpb: u64,
    wallets: u64,
    verifies: u64,
) -> Vec<ScalingPoint> {
    let run = |n: u64| match regime {
        "light" => run_light(n, verifies),
        "replica" => run_replica(n, blocks, tpb, wallets),
        _ => run_archive_shard(n, blocks, tpb, wallets),
    };
    let base = run(1).agg_ops.max(1e-6);
    counts
        .iter()
        .map(|&n| {
            let mut p = run(n);
            p.speedup = p.agg_ops / base;
            p.efficiency = p.speedup / n as f64;
            p
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn light_verify_is_cheap_and_correct() {
        let p = run_light(2, 1_000);
        assert_eq!(p.total_ops, 2_000, "all tip-proofs must verify");
        assert!(p.agg_ops > 0.0);
    }

    #[test]
    fn archive_shards_run_in_parallel() {
        // 2 shards, small: each 50 blocks × 20 txs.
        let p = run_archive_shard(2, 50, 20, 128);
        assert_eq!(p.nodes, 2);
        assert!(p.total_ops >= 2 * 50 * 20 - 4, "both shards apply their txs");
    }

    #[test]
    fn shard_scales_better_than_replica() {
        // On a multi-core host, 4 independent shards should achieve higher
        // aggregate throughput than 4 replicas of one chain (which is pure
        // redundancy). This is the whole horizontal-scaling thesis in one
        // assertion. Use a workload big enough to dwarf thread overhead.
        let shard = run_archive_shard(4, 200, 50, 256);
        let replica = run_replica(4, 200, 50, 256);
        assert!(
            shard.agg_ops > replica.agg_ops,
            "4 shards ({:.0} ops/s) should beat 4 replicas ({:.0} ops/s)",
            shard.agg_ops, replica.agg_ops
        );
    }
}
