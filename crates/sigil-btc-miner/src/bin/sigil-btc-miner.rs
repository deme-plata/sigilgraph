//! sigil-btc-miner — Prototype-1 experiment runner. Mines SHA256d at ~10% of
//! cores for a few seconds, feeds shares into the provable flux-pool, and prints
//! the HONEST expected yield + a provable LN payout split.
//!
//!   sigil-btc-miner [seconds] [share_bits]   (defaults: 3s, 22-bit shares)

use std::time::Duration;

use flux_pool::ShareLedger;
use sigil_btc_miner::{mine, MinerStats};

fn main() {
    let secs: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(3);
    let share_bits: u32 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(22);

    println!("\n  sigil-btc-miner — Prototype-1 EXPERIMENT (BTC SHA256d, ~10% cores → flux-pool → LN)\n");
    let st: MinerStats = mine(b"sigil-btc-experiment/header/v1", share_bits, Duration::from_secs(secs));

    println!("  resources : {} of {} cores ({}%)  · CPU only (CUDA = Vast follow-on)",
        st.threads, st.total_cores, st.threads * 100 / st.total_cores.max(1));
    println!("  hashrate  : {:.2} MH/s  ({} hashes in {:.1}s)", st.hashrate_hps / 1e6, st.hashes, st.elapsed_s);
    println!("  shares    : {} found @ {}-bit share difficulty", st.shares_found, st.share_bits);

    // HONEST yield — the whole point of the experiment.
    let btc_day = st.expected_btc_per_day();
    println!("  ── honest yield ──");
    println!("  expected mainnet BTC/day at this hashrate: {:.3e} BTC  (≈ ${:.2e})", btc_day, btc_day * 73_000.0);
    println!("  → at 200 miners: {:.3e} BTC/day. CPU/GPU SHA256d does NOT earn real BTC.", btc_day * 200.0);
    println!("    Real tiny-BTC paths: pool shares / LN routing — which the pipeline below proves.\n");

    // Feed shares into the PROVABLE pool + show a fair LN payout split.
    let mut pool = ShareLedger::new();
    pool.record_share("rocky", st.shares_found.max(1) as u128); // this node's shares
    pool.record_share("miner-2", 40); // a couple of peers for the demo
    pool.record_share("miner-3", 25);
    let reward_sat = 5_000u128; // a tiny pooled LN reward to split
    println!("  PROVABLE POOL ({} miners, {} sat to split):", pool.miners(), reward_sat);
    for p in pool.payouts(reward_sat) {
        println!("    {:<10} {:>5} weight → {:>5} sat", p.miner, p.weight, p.sats);
    }
    let att = pool.attest();
    println!("  fold attestation: {} B (constant ∀ miner count) — every miner verifies their cut\n", att.size_bytes());
}
