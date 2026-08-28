//! `sigil-boundary-soak` — a **100-hour** SIGIL soak in virtual time, auditing the
//! supply boundary integral at EVERY block boundary.
//!
//! The point of chronos: 100 simulated hours resolves in seconds of wall clock,
//! deterministically, against the REAL `apply_tx` → `commit_state_transition`
//! pipeline. No Docker, no nohup, no waiting.
//!
//! At 1 block/simulated-second, 100 h = 360,000 blocks.
//!
//! ```
//! SOAK_HOURS=100 fluxc run -p sigil-chronos --bin sigil-boundary-soak
//! ```
//!
//! Checks, per block:
//!   - `Δsupply == declared_mint − declared_burn`  (the CCC boundary law)
//!   - O(1) incremental supply == O(state) recomputed supply (accumulator drift)
//! and at the end: monotonic height, no divergence, supply == blocks × BLOCK_REWARD.

use flux_chronos::NodeId;
use sigil_chronos::boundary::{check_crossing, Boundary, DeclaredFlux, Verdict};
use sigil_chronos::{demo_genesis, sign_dummy, SigilSimNode, BLOCK_REWARD};
use sigil_state::NATIVE;
use sigil_tx::SigilTx;

const BLOCK_TIME_US: u64 = 1_000_000; // 1 simulated second per block

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key).ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}

fn main() {
    let hours = env_u64("SOAK_HOURS", 100);
    let blocks = hours * 3600; // 1 blk / simulated second
    let report_every = (blocks / 10).max(1);
    let inject_at = std::env::var("SOAK_INJECT_AT").ok().and_then(|s| s.parse::<u64>().ok());
    let inject_amount = env_u64("SOAK_INJECT_AMOUNT", 777);
    // Fee per tx. Fees are BURNED (debited, credited nowhere) and a block does NOT
    // declare its burn — so with SOAK_FEE>0 a chain-data-only verifier MUST report
    // every block as leaking exactly `fee`. That is the open gap, measured at scale.
    let fee = env_u64("SOAK_FEE", 0) as u128;

    println!("=== SIGIL boundary soak — {hours} simulated hours = {blocks} blocks ===");
    println!("    real pipeline: apply_tx -> commit_state_transition -> roots");
    println!("    auditing the CCC boundary law at EVERY block boundary\n");

    let g = demo_genesis();
    let mut node = SigilSimNode::new("soak-producer", NodeId(0), vec![], true, BLOCK_TIME_US, &g);

    let genesis_boundary = Boundary::of_node(&node);
    let wall = std::time::Instant::now();

    let mut conserved = 0u64;
    let mut violations: Vec<(u64, Verdict)> = Vec::new();
    let mut drifts = 0u64;
    let mut stalled_at: Option<u64> = None;
    let mut last_height = node.height();

    for i in 0..blocks {
        // One tx per block — the producer drains FIFO, one per block-time tick.
        let from = [(i % 5 + 1) as u8; 32];
        let to = [((i + 1) % 5 + 1) as u8; 32];
        node.enqueue_tx(sign_dummy(SigilTx::Send {
            from, to, amount: 100, token: NATIVE, fee,
        }));

        let before = Boundary::of_node(&node);
        let Some(block) = node.produce_one() else {
            stalled_at = Some(i);
            break;
        };

        // ADVERSARIAL: a green soak that CANNOT fail is worthless. `SOAK_INJECT_AT=N`
        // commits an undeclared mint (a phantom Hawking point) straight through the
        // chokepoint at block N — the cap check accepts it because the total stays far
        // under MAX_SUPPLY. The audit MUST catch it, or the whole harness is theatre.
        if inject_at == Some(i) {
            node.inject_undeclared_mint(inject_amount as u128);
            println!("  💉 injected UNDECLARED mint of {inject_amount} at block {i}");
        }

        let after = Boundary::of_node(&node);

        // Height must be strictly monotonic — the sync-down invariant.
        assert!(after.height > last_height, "height went backwards at block {i}");
        last_height = after.height;

        match check_crossing(&before, &after, DeclaredFlux::from_block(&block)) {
            Verdict::Conserved { .. } => conserved += 1,
            v @ Verdict::AccumulatorDrift { .. } => {
                drifts += 1;
                if violations.len() < 10 { violations.push((i, v)); }
            }
            v @ Verdict::UndeclaredFlux { .. } => {
                if violations.len() < 10 { violations.push((i, v)); }
            }
        }

        if (i + 1) % report_every == 0 {
            let el = wall.elapsed().as_secs_f64();
            let sim_h = (i + 1) as f64 / 3600.0;
            println!(
                "  {:>7} blocks | sim {:>6.1} h | {:>6.2}s wall | {:>9.0} blk/s | conserved {} | violations {}",
                i + 1, sim_h, el, (i + 1) as f64 / el.max(1e-9), conserved, violations.len()
            );
        }
    }

    let el = wall.elapsed().as_secs_f64();
    let final_boundary = Boundary::of_node(&node);
    let produced = final_boundary.height - genesis_boundary.height;

    println!("\n=== RESULT ===");
    if let Some(at) = stalled_at {
        println!("  🚨 PRODUCER STALLED at block {at} (mempool starved) — soak incomplete");
    }
    println!("  simulated       : {:.1} h ({} blocks)", produced as f64 / 3600.0, produced);
    println!("  wall clock      : {el:.2} s  =>  {:.0} blk/s  ({:.0}x realtime)",
        produced as f64 / el.max(1e-9), (produced as f64) / el.max(1e-9));
    println!("  boundaries OK   : {conserved} / {produced}");
    println!("  accumulator drift: {drifts}");
    println!("  violations      : {}", violations.len());
    for (i, v) in &violations {
        println!("     block {i}: {v:?}");
    }

    let expected = produced as u128 * BLOCK_REWARD;
    let actual = final_boundary.supply_recomputed - genesis_boundary.supply_recomputed;
    println!("\n  supply minted   : {actual} base units");
    println!("  expected        : {expected}  ({produced} blocks x {BLOCK_REWARD})");
    println!("  incremental==recomputed at tip: {}", final_boundary.is_consistent());

    // With an injection, the CORRECT outcome is a caught violation.
    if inject_at.is_some() {
        let caught = violations.iter().any(|(_, v)| matches!(v, Verdict::UndeclaredFlux { .. }));
        println!("\n  INJECTION MODE: undeclared mint of {inject_amount} at block {:?}", inject_at.unwrap());
        println!("  VERDICT: {}", if caught {
            "✅ CAUGHT — the audit detects an undeclared mint the cap check waved through"
        } else {
            "❌ MISSED — the audit is blind; the soak's green means NOTHING"
        });
        std::process::exit(if caught { 0 } else { 1 });
    }

    let ok = violations.is_empty()
        && drifts == 0
        && stalled_at.is_none()
        && actual == expected
        && final_boundary.is_consistent()
        && conserved == produced;
    println!("\n  VERDICT: {}", if ok { "✅ CONSERVED across the whole soak" } else { "❌ FAILED" });
    if !ok { std::process::exit(1); }
}
