//! P7-E proof: parallel sig-verify moves the wall. Measures single-thread vs
//! all-core ed25519 verify throughput on a realistic block-sized batch.
//!
//! Run:
//!   fluxc build --package sigil-sigverify --example parallel_speedup --release
//!   ./target/release/examples/parallel_speedup
//!
//! Expected on a 48-core box: near-linear speedup (sig-verifies are
//! independent), pushing the ed25519 ceiling from ~9k/s/core toward
//! cores×9k/s — exactly the lever the STARGATE 500M handoff called for.

use std::time::Instant;

use ed25519_dalek::{Signer, SigningKey};
use sigil_sigverify::{
    all_valid_parallel, verify_batch_parallel, Ed25519Verifier, VerifyItem, Verifier,
};

fn main() {
    let cores = num_cpus::get();
    println!("=== P7-E: parallel sig-verify speedup (ed25519) ===");
    println!("cores: {}", cores);

    // A realistic large block: 10k transactions, each with its own keypair.
    const N: usize = 10_000;
    println!("\nbuilding {} signed items (outside timing)...", N);
    let built: Vec<(Vec<u8>, Vec<u8>, Vec<u8>)> = (0..N)
        .map(|i| {
            // deterministic distinct key per tx (no rand dep)
            let mut seed = [0u8; 32];
            seed[..8].copy_from_slice(&(i as u64).to_le_bytes());
            let sk = SigningKey::from_bytes(&seed);
            let msg = format!("sigil-tx-{i}-canonical-bytes-stand-in").into_bytes();
            let sig = sk.sign(&msg).to_bytes().to_vec();
            let pk = sk.verifying_key().to_bytes().to_vec();
            (msg, sig, pk)
        })
        .collect();
    let items: Vec<VerifyItem> = built
        .iter()
        .map(|(m, s, p)| VerifyItem { msg: m, sig: s, pubkey: p })
        .collect();

    let v = Ed25519Verifier;

    // ── single-thread baseline (sequential loop) ──────────────────────────
    let t = Instant::now();
    let mut ok = 0usize;
    for it in &items {
        if v.verify(it) { ok += 1; }
    }
    let st = t.elapsed();
    assert_eq!(ok, N);
    let st_rate = N as f64 / secs(&st);
    println!("\n── single-thread ──");
    println!("  {} verifies in {:?}", N, st);
    println!("  {:.0} verifies/sec", st_rate);

    // ── parallel (all cores) ──────────────────────────────────────────────
    let t = Instant::now();
    let results = verify_batch_parallel(&v, &items);
    let mt = t.elapsed();
    assert_eq!(results.iter().filter(|&&r| r).count(), N);
    let mt_rate = N as f64 / secs(&mt);
    println!("\n── parallel ({} cores) ──", cores);
    println!("  {} verifies in {:?}", N, mt);
    println!("  {:.0} verifies/sec", mt_rate);

    // ── block-gate short-circuit ──────────────────────────────────────────
    let t = Instant::now();
    let all_ok = all_valid_parallel(&v, &items);
    let gate = t.elapsed();
    assert!(all_ok);
    println!("\n── all_valid_parallel (block gate) ──");
    println!("  all {} valid: {} in {:?}", N, all_ok, gate);

    // ── verdict ───────────────────────────────────────────────────────────
    let speedup = mt_rate / st_rate;
    println!("\n════════ THE WALL MOVES ════════");
    println!("  single-thread: {:.0} verifies/s", st_rate);
    println!("  parallel:      {:.0} verifies/s", mt_rate);
    println!("  speedup:       {:.1}× across {} cores ({:.0}% of linear)",
        speedup, cores, 100.0 * speedup / cores as f64);
    println!("\n  → at 10k-tx blocks, parallel verify turns the ed25519 wall");
    println!("    from a single-core bottleneck into an all-core one. Composes");
    println!("    with P7-A (verify-once on ingest) + P7-C (hot/settlement split).");
}

fn secs(d: &std::time::Duration) -> f64 { d.as_secs_f64() }
