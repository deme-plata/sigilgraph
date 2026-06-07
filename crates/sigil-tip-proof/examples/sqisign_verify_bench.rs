//! sqisign_verify_bench — measure raw SQIsign L5 verify rate on this hardware.
//!
//! Per the Stargate 500M handoff: "Measure SQIsign verify rate first thing —
//! it may put the real wall at thousands/s, not 113k." This benchmark answers
//! that question for THIS machine (Epsilon, 48c AVX-512).
//!
//! Usage:
//!   cargo run --example sqisign_verify_bench --features sqisign
//!   fluxc run sigil-tip-proof --example sqisign_verify_bench

use std::time::Instant;

fn main() {
    println!("=== SQIsign L5 Verify Benchmark ===\n");

    // Generate a keypair
    let t0 = Instant::now();
    let (sk, pk) = flux_sqisign::keygen();
    let keygen_ms = t0.elapsed().as_millis();
    println!("keygen:   {:>4} ms  (sk={}B, pk={}B)", keygen_ms, sk.len(), pk.len());

    // Sign a 32-byte message (simulates signing a block hash)
    let msg: [u8; 32] = [0x42; 32];
    let t0 = Instant::now();
    let sig = flux_sqisign::sign(&msg, &sk, &pk).expect("sign failed");
    let sign_ms = t0.elapsed().as_millis();
    println!("sign:     {:>4} ms  (sig={}B)", sign_ms, sig.len());

    // Single verify latency
    let t0 = Instant::now();
    let ok = flux_sqisign::verify(&msg, &sig, &pk).expect("verify failed");
    let single_us = t0.elapsed().as_micros();
    println!("verify:   {:>4} µs  (ok={})\n", single_us, ok);

    // Batch verify — measure sustained rate over N iterations
    const WARMUP: u64 = 50;
    const MEASURE: u64 = 500;

    // Warmup
    for _ in 0..WARMUP {
        let _ = flux_sqisign::verify(&msg, &sig, &pk);
    }

    // Measure sustained
    let t0 = Instant::now();
    for _ in 0..MEASURE {
        let _ = flux_sqisign::verify(&msg, &sig, &pk);
    }
    let elapsed_s = t0.elapsed().as_secs_f64();
    let rate = MEASURE as f64 / elapsed_s;

    println!("=== Sustained Verify Rate ===");
    println!("iterations:  {}", MEASURE);
    println!("elapsed:     {:.2} s", elapsed_s);
    println!("verify/s:    {:.0}", rate);
    println!("per-verify:  {:.1} ms", 1000.0 / rate);
    println!();

    // Estimate on 48 cores (naive parallel — no contention modelled)
    let parallel_est = rate * 48.0;
    println!("=== Projections ===");
    println!("single-core: {:.0} verify/s", rate);
    println!("48-core est: {:.0} verify/s (naive ×48, no contention)", parallel_est);
    println!();

    // Compare against the wall numbers from the handoff doc
    println!("=== Wall Comparison (from Stargate 500M handoff) ===");
    println!("SQIsign L5, 48t (measured prior):  131 /s  (75 ms/verify)");
    println!("ed25519 single:                   9,123 /s  (110 µs/verify)");
    println!("ed25519 batch×48t (hot path):   110,821 /s  (844× over SQIsign)");
    println!();
    println!("This machine:                    {:.0} /s  ({:.0}× vs prior 131/s)", rate, rate / 131.0);
}
