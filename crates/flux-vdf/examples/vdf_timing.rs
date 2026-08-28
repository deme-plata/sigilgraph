//! One-off real measurement: how long does `eval` actually take at the
//! live block-level depth (t=600) vs. the new share depth (t=8)? Written
//! 2026-08-20 because the whole VDF-bound-hashrate diagnosis this session
//! relied on an INFERRED timing (matched against observed cadence), never a
//! direct measurement — this closes that gap before trusting the diagnosis
//! further. `cargo run --release --example vdf_timing` (or via fluxc).

use flux_vdf::{eval, ModSquaring, VdfGroup};
use std::time::Instant;

fn main() {
    let g = ModSquaring::bench_2048();
    let x = g.from_seed(&[0x42u8; 32]);

    for t in [1u64, 8, 100, 600, 2000] {
        let t0 = Instant::now();
        let _proof = eval(&g, &x, t);
        let dt = t0.elapsed();
        println!(
            "t={t:<5} wall={:>9.3}ms  per-turn={:>8.4}ms",
            dt.as_secs_f64() * 1000.0,
            dt.as_secs_f64() * 1000.0 / t as f64,
        );
    }
}
