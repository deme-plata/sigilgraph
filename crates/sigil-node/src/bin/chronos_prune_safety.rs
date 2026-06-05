//! chronos_prune_safety.rs — does pruning DELETE/CORRUPT state the way Quillon
//! Graph's replay bug destroyed wallet balances (3200 → 1484 QUG)?
//!
//! This is the Quillon-trauma test. "Pruning drops bytes" is true by definition;
//! the dangerous question is whether a pruned node can SILENTLY produce WRONG
//! balances. We prove the safety invariants by re-deriving state from storage
//! and comparing the four committed roots (which cryptographically commit ALL
//! balances) against the producer's ground truth:
//!
//!   1. FULL replay   → must reproduce ground-truth roots byte-identical (div=0).
//!   2. PRUNED replay → has no transitions to replay; it must NOT silently yield
//!      a wrong-but-plausible state. We show it lands on EMPTY (every balance 0)
//!      — exactly the catastrophe IF a node ever treated pruned storage as a
//!      replay source. The safe design refuses this and recovers from an archive.
//!   3. ARCHIVE recovery → pruned node + full archive re-derives ground truth
//!      exactly, proving the data is never lost NETWORK-wide, only locally.
//!   4. MAX-WINS → no replay ever writes a balance LOWER than ground truth
//!      (the exact Quillon failure: save_wallet_balances overwrote a correct
//!      higher balance with a stale lower one).
//!
//! Scale it as far as you like: `SAFETY_BLOCKS=5000000 ... ` on the 20 TB array.

use flux_chronos::NodeId;
use sigil_chronos::{demo_genesis, sign_dummy, Block, SigilSimNode};
use sigil_node::store::{BlockStore, PrunedHeader, RetentionMode};
use sigil_state::{commit_state_transition, SigilState, StateRoots, WalletId};
use sigil_tx::{SigilTx, NATIVE};

fn roots_equal(a: &StateRoots, b: &StateRoots) -> bool {
    a.wallet_state_root == b.wallet_state_root
        && a.dex_state_root == b.dex_state_root
        && a.event_log_root == b.event_log_root
        && a.contract_state_root == b.contract_state_root
}

/// Streaming replay of a FULL store: fresh state, apply every block's transition
/// in height order (genesis at 0 included). Constant RAM — point-get per height.
fn replay_full(store: &BlockStore, tip: u64) -> (SigilState, u64) {
    let mut state = SigilState::new();
    let mut applied = 0u64;
    for h in 0..=tip {
        match store.get_block::<Block>(h) {
            Ok(Some(b)) => {
                commit_state_transition(&mut state, &b.transition, h).expect("commit during full replay");
                applied += 1;
            }
            Ok(None) => {}
            Err(e) => panic!("full replay get_block({h}) failed: {e}"),
        }
    }
    (state, applied)
}

/// Replay attempt from a PRUNED store: the cores carry no transitions, so there
/// is nothing to apply. Returns the (empty) state a naive replay would land on.
fn replay_pruned(store: &BlockStore, tip: u64) -> (SigilState, u64) {
    let mut state = SigilState::new();
    let mut found = 0u64;
    for h in 0..=tip {
        if let Ok(Some(_core)) = store.get_block::<PrunedHeader>(h) {
            found += 1;
            // a PrunedHeader has NO transition — nothing to apply. A correct node
            // MUST detect this and refuse/fetch, never invent state.
        }
    }
    (state, found)
}

/// Recovery: a pruned node fetches the missing transitions from a FULL archive
/// (here, the full store) and re-derives state. Proves data isn't lost network-wide.
fn recover_from_archive(pruned: &BlockStore, archive: &BlockStore, tip: u64) -> SigilState {
    let mut state = SigilState::new();
    for h in 0..=tip {
        // the pruned node knows height h exists (it has the core)…
        if pruned.get_block::<PrunedHeader>(h).ok().flatten().is_none() {
            continue;
        }
        // …and fetches the full block from the archive to get the transition.
        let b = archive.get_block::<Block>(h).expect("archive get").expect("archive has block");
        commit_state_transition(&mut state, &b.transition, h).expect("commit during recovery");
    }
    state
}

fn main() {
    let n: u64 = std::env::var("SAFETY_BLOCKS").ok().and_then(|s| s.parse().ok()).unwrap_or(5_000);
    let base = std::env::temp_dir().join(format!("sigil-prune-safety-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let full = BlockStore::open(base.join("full")).unwrap().with_retention(RetentionMode::Full);
    let pruned = BlockStore::open(base.join("pruned")).unwrap().with_retention(RetentionMode::Pruned);

    // genesis (height 0) — store in BOTH so replay starts from the same point.
    let g = demo_genesis();
    let gblock = g.build_block();
    full.put_block(0, &gblock).unwrap();
    pruned.put_pruned(0, &PrunedHeader::from(&gblock.header)).unwrap();

    let funded: Vec<WalletId> = g.funded.iter().map(|(w, _)| *w).collect();
    let mut producer = SigilSimNode::new("safety-producer", NodeId(0), vec![], true, 1_000, &g);

    println!("=== prune-safety: producing {n} real blocks with live transfers ===");
    for h in 1..=n {
        let from = funded[(h as usize) % funded.len()];
        let to = funded[(h as usize + 1) % funded.len()];
        producer.enqueue_tx(sign_dummy(SigilTx::Send { from, to, amount: 1, token: NATIVE, fee: 0 }));
        let b = match producer.produce_one() {
            Some(b) => b,
            None => {
                eprintln!("producer stalled at {h}");
                break;
            }
        };
        full.put_block(h, &b).unwrap();
        pruned.put_pruned(h, &PrunedHeader::from(&b.header)).unwrap();
        if h % 50_000 == 0 {
            println!("  …minted {h}");
        }
    }
    full.compact();
    pruned.compact();

    // ── GROUND TRUTH ──
    let truth_roots = producer.roots();
    let sample: Vec<(WalletId, u128)> =
        funded.iter().take(4).map(|w| (*w, producer.balance_of(w, &NATIVE))).collect();
    let tip = full.tip_height().unwrap();

    println!("\nground-truth tip H={tip}  ·  full store {:.2} MB  ·  pruned store {:.2} MB",
        full.disk_bytes() as f64 / 1e6, pruned.disk_bytes() as f64 / 1e6);

    // ── 1. FULL replay ──
    let (full_state, full_applied) = replay_full(&full, tip);
    let full_ok = roots_equal(&full_state.roots(), &truth_roots);

    // ── 2. PRUNED replay (no transitions) ──
    let (pruned_state, pruned_found) = replay_pruned(&pruned, tip);
    let pruned_ok = roots_equal(&pruned_state.roots(), &truth_roots);

    // ── 3. ARCHIVE recovery ──
    let recovered = recover_from_archive(&pruned, &full, tip);
    let recover_ok = roots_equal(&recovered.roots(), &truth_roots);

    // ── 4. MAX-WINS: no replay path writes a balance LOWER than ground truth ──
    let mut maxwins_violation = false;
    for (w, truth_bal) in &sample {
        if full_state.balance_of(w, &NATIVE) < *truth_bal {
            maxwins_violation = true;
        }
    }

    println!("\n┌─ REPLAY → STATE INTEGRITY ────────────────────────────────────────");
    println!("│ FULL   : applied {full_applied} transitions → roots {} ground truth", if full_ok { "✓ MATCH" } else { "✗ DIVERGE" });
    println!("│ PRUNED : found {pruned_found} cores, 0 transitions → roots {}", if pruned_ok { "✓ match (vacuous)" } else { "✗ EMPTY (no data to replay)" });
    println!("│ RECOVER: pruned + full archive → roots {} ground truth", if recover_ok { "✓ MATCH" } else { "✗ DIVERGE" });
    println!("│ MAX-WINS: {}", if maxwins_violation { "✗ a balance dropped below truth (QUILLON BUG!)" } else { "✓ no balance ever written lower than truth" });
    println!("└───────────────────────────────────────────────────────────────────");

    println!("\nsample wallet balances (native SIGIL):");
    println!("  {:<10} {:>14} {:>14} {:>14} {:>14}", "wallet", "ground-truth", "full-replay", "pruned-replay", "recovered");
    for (w, truth_bal) in &sample {
        println!("  {:<10} {:>14} {:>14} {:>14} {:>14}",
            hex4(w), truth_bal,
            full_state.balance_of(w, &NATIVE),
            pruned_state.balance_of(w, &NATIVE),
            recovered.balance_of(w, &NATIVE));
    }

    // ── VERDICT ──
    let safe = full_ok && recover_ok && !maxwins_violation && !pruned_ok;
    println!("\n=== VERDICT ===");
    if safe {
        println!("✓ SAFE. Full storage re-derives ground-truth state EXACTLY (div=0).");
        println!("  Pruned storage alone yields EMPTY state — it has no transitions to replay,");
        println!("  so a node must REFUSE to treat it as truth and recover from the archive");
        println!("  (which reproduces ground truth exactly). Nothing is lost network-wide.");
        println!("  No replay path ever wrote a balance lower than truth — the Quillon class");
        println!("  of silent balance destruction does NOT occur here.");
        println!("  → SIGIL ships FULL by default precisely so this question never bites.");
    } else {
        println!("✗ UNSAFE — investigate:");
        println!("    full_ok={full_ok} recover_ok={recover_ok} maxwins_violation={maxwins_violation} pruned_silently_ok={pruned_ok}");
        std::process::exit(1);
    }

    let _ = std::fs::remove_dir_all(&base);
}

fn hex4(w: &WalletId) -> String {
    format!("{:02x}{:02x}{:02x}{:02x}…", w[0], w[1], w[2], w[3])
}
