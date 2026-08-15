//! fair_order_bias_bench — Phase G (SIGIL_BRAIDPOOL_v1_1.md §15): prints the
//! measured tie-winner distribution for the naive validator-index tiebreak
//! vs the content-hash tiebreak, side by side.
//!
//! This is NOT a benchmark of Tilikum, MRV, Themis, or Aequitas — see
//! `fair_order_experiment`'s module doc comment for exactly what is and is
//! not being measured. It is a bias-visualization tool for the one narrow
//! mechanism this crate actually implements.
//!
//! Usage: fair_order_bias_bench [worker_count] [cohorts]

use sigil_narwhal_mempool::fair_order_experiment::{order_content_tiebreak, order_naive_index_tiebreak, synthetic_tie_cohort};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let worker_count: u16 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(8);
    let cohorts: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(2000);

    println!("fair_order_bias_bench: worker_count={worker_count} cohorts={cohorts}");
    println!("Each cohort = one batch per worker, all tied on the same first_seen_round.\n");

    let mut naive_wins = vec![0u32; worker_count as usize];
    let mut content_wins = vec![0u32; worker_count as usize];

    for i in 0..cohorts {
        let mut a = synthetic_tie_cohort(b"bench", i, worker_count);
        order_naive_index_tiebreak(&mut a);
        naive_wins[a[0].creator.0 as usize] += 1;

        let mut b = synthetic_tie_cohort(b"bench", i, worker_count);
        order_content_tiebreak(&mut b);
        content_wins[b[0].creator.0 as usize] += 1;
    }

    let uniform = cohorts as f64 / worker_count as f64;
    println!("── naive index tiebreak (KNOWN-BAD baseline §15 warns about) ──");
    for (w, &n) in naive_wins.iter().enumerate() {
        println!("  worker {w:>2}: {n:>5} wins ({:>5.1}%)", 100.0 * n as f64 / cohorts as f64);
    }
    println!(
        "  verdict: worker 0 wins {}/{cohorts} ({:.1}%) — pure identity determines every tie.\n",
        naive_wins[0],
        100.0 * naive_wins[0] as f64 / cohorts as f64
    );

    println!("── content (tx_root) tiebreak ──");
    for (w, &n) in content_wins.iter().enumerate() {
        println!("  worker {w:>2}: {n:>5} wins ({:>5.1}%, uniform would be {uniform:.1})", 100.0 * n as f64 / cohorts as f64);
    }
    println!(
        "  verdict: wins are spread across workers roughly evenly — identity does not determine who wins a tie.\n"
    );

    println!("── what this does and does not show ──");
    println!("  Shown: the specific bias mechanism (sorting ties by validator/worker index) exists and is total under the naive");
    println!("  scheme, and one simple substitution (sort ties by content hash instead) removes IDENTITY as the deciding factor.");
    println!("  NOT shown: robustness against an adversary who grinds tx_root, or anything resembling Byzantine agreement on");
    println!("  cross-validator arrival order. That is the actual subject of Tilikum/MRV/Themis/Aequitas and is explicitly");
    println!("  out of scope here — see SIGIL_BRAIDPOOL_v1_1.md §15 and this crate's fair_order_experiment module doc comment.");
}
