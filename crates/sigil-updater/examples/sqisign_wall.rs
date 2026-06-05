//! STARGATE lever #1 — measure the SQIsign verify rate (the real TPS wall).
//!
//! The 500M handoff (SIGIL_STARGATE_500M_HANDOFF.md) found that state is solved
//! (~13M sound commits/s) but ed25519 sig-verify caps end-to-end TPS at ~113k/s.
//! SIGIL's PRODUCTION signature is SQIsign (post-quantum, isogeny) — slower than
//! ed25519. This bench answers the single most decision-relevant question in the
//! whole throughput project: **how many SQIsign verifies per second can Epsilon
//! actually do?**
//!
//! If it's ~1k/s, the real ceiling is 100× below ed25519 and the entire roadmap
//! reprioritizes (hot-path/settlement split + Narwhal verify-once become
//! mandatory, not optional).
//!
//! Run (release, on Epsilon 48c):
//!   fluxc build --package sigil-updater --example sqisign_wall --release
//!   ./target/release/examples/sqisign_wall
//!
//! Measures: keygen cost, sign cost, single-thread verify rate, N-thread verify
//! rate (rayon, scaled to num_cpus), and the implied end-to-end TPS ceiling.

use std::time::Instant;

use rayon::prelude::*;

fn main() {
    println!("=== SQIsign verify wall — STARGATE lever #1 ===");
    println!("cores: {}", num_cpus::get());
    println!("sig size: {} B, pubkey size: {} B\n",
        flux_sqisign::signature_size(), flux_sqisign::public_key_size());

    // ── Warm up + cost of keygen / sign (these are NOT the hot path, but worth
    //    knowing — block production signs once, the network verifies many times).
    let t = Instant::now();
    let (sk, pk) = flux_sqisign::keygen();
    println!("keygen:  {:?} (one-time per identity)", t.elapsed());

    let msg = b"sigil-block-header-canonical-bytes-stand-in-for-the-real-thing";
    let t = Instant::now();
    let sig = flux_sqisign::sign(msg, &sk, &pk).expect("sign");
    println!("sign:    {:?} (once per block by the producer)", t.elapsed());

    // Sanity: the sig verifies.
    assert!(flux_sqisign::verify(msg, &sig, &pk).expect("verify call"), "sig must verify");

    // ── Build a corpus of distinct (msg, sig, pk) so we measure verify, not
    //    cache effects. Keygen is expensive, so reuse one keypair but vary the
    //    message + re-sign — verify cost is dominated by the isogeny path, not
    //    the key, so this is representative. We pre-sign OUTSIDE the timed loop.
    const N: usize = 512; // enough to get a stable rate without waiting on 512 slow signs forever
    println!("\npre-signing {} messages (outside timing)...", N);
    let t = Instant::now();
    let corpus: Vec<(Vec<u8>, Vec<u8>, Vec<u8>)> = (0..N)
        .map(|i| {
            let m = format!("sigil-tx-{i}-{}", "x".repeat(32)).into_bytes();
            let s = flux_sqisign::sign(&m, &sk, &pk).expect("sign corpus");
            (m, s, pk.clone())
        })
        .collect();
    println!("  pre-sign of {} took {:?} ({:.1} signs/s)",
        N, t.elapsed(), N as f64 / t.elapsed().as_secs_f64());

    // ── Single-thread verify rate ──────────────────────────────────────────
    let t = Instant::now();
    let mut ok = 0usize;
    for (m, s, p) in &corpus {
        if flux_sqisign::verify(m, s, p).unwrap_or(false) { ok += 1; }
    }
    let st_elapsed = t.elapsed();
    let st_rate = N as f64 / st_elapsed.as_secs_f64();
    assert_eq!(ok, N, "all corpus sigs must verify");
    println!("\n── single-thread ──");
    println!("  {} verifies in {:?}", N, st_elapsed);
    println!("  {:.1} verifies/sec/core", st_rate);
    println!("  {:.0} µs per verify", st_elapsed.as_micros() as f64 / N as f64);

    // ── Multi-thread verify rate (rayon, all cores) ────────────────────────
    let t = Instant::now();
    let mt_ok: usize = corpus
        .par_iter()
        .map(|(m, s, p)| if flux_sqisign::verify(m, s, p).unwrap_or(false) { 1 } else { 0 })
        .sum();
    let mt_elapsed = t.elapsed();
    let mt_rate = N as f64 / mt_elapsed.as_secs_f64();
    assert_eq!(mt_ok, N);
    println!("\n── {}-thread (rayon) ──", num_cpus::get());
    println!("  {} verifies in {:?}", N, mt_elapsed);
    println!("  {:.0} verifies/sec total", mt_rate);
    println!("  scaling efficiency: {:.0}% of linear",
        100.0 * mt_rate / (st_rate * num_cpus::get() as f64));

    // ── The verdict ────────────────────────────────────────────────────────
    println!("\n════════ THE WALL ════════");
    println!("  SQIsign verify ceiling: {:.0} /s ({}t)", mt_rate, num_cpus::get());
    println!("  ed25519 reference:      113,655 /s (from 500M handoff)");
    let ratio = 113_655.0 / mt_rate;
    if mt_rate < 113_655.0 {
        println!("  → SQIsign is {:.0}× SLOWER than ed25519", ratio);
        println!("  → end-to-end TPS ceiling on SQIsign hot-path: ~{:.0} TPS", mt_rate);
        println!("  → hot-path/settlement split (lever #4) + Narwhal verify-once");
        println!("     (lever #3) are MANDATORY, not optional.");
    } else {
        println!("  → SQIsign keeps up with ed25519 — surprising, double-check the build is release");
    }
    println!("\n(state ceiling is 13M sound / 209M unsafe — sig-verify is {:.0}× below sound state)",
        13_000_000.0 / mt_rate);
}
