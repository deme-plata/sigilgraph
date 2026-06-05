//! Optimize the wall — signature verification is the binding constraint.
//!
//! 500m handoff established: state is free (~209M/s), ed25519 verify is the
//! wall (~113k/s), and SQIsign (the post-quantum PRODUCTION sig) is slower
//! still. This harness measures the REAL walls and the available lifts:
//!
//!   A. SQIsign L5 verify         — the production sig (the true wall)
//!   B. ed25519 single verify     — the hot-path fallback (baseline)
//!   C. ed25519 verify_batch      — batch MSM (the cheap lift)
//!   D. ed25519 batch × parallel  — batch + all cores (the combined lift)
//!
//! Plus the structural lever: "verify-once" amortization — in a Narwhal
//! mempool a sig is verified ONCE on ingest, then the tx is ordered +
//! executed by hash N times without re-verification. Effective verify rate
//! = raw rate × reuse factor.

use std::time::Instant;

use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey, Signature};
use rand::rngs::OsRng;

const MSG: &[u8] = b"sigil-tx-canonical-bytes-placeholder-48b-payload";

fn main() {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    println!("\n  OPTIMIZE THE WALL — signature verification ({} cores)\n", cores);

    // ── A. SQIsign L5 — the production wall ──────────────────────────────
    println!("  ── A. SQIsign Level 5 (post-quantum, the PRODUCTION sig) ──");
    let (sq_sk, sq_pk) = flux_sqisign::keygen();   // keygen returns (sk, pk)
    let sq_sig = flux_sqisign::sign(MSG, &sq_sk, &sq_pk).expect("sqisign sign");
    // warm
    let _ = flux_sqisign::verify(MSG, &sq_sig, &sq_pk);
    let n_sq = 200usize;
    let t0 = Instant::now();
    let mut ok = 0u64;
    for _ in 0..n_sq { if flux_sqisign::verify(MSG, &sq_sig, &sq_pk).unwrap_or(false) { ok += 1; } }
    let sq_single = n_sq as f64 / t0.elapsed().as_secs_f64();
    println!("    single-thread: {:>10.0} verifies/s  ({:.2} ms each)", sq_single, 1000.0/sq_single);
    // parallel SQIsign
    let t0 = Instant::now();
    let per = (n_sq * cores).div_ceil(cores);
    std::thread::scope(|s|{
        let mut hs=Vec::new();
        for _ in 0..cores {
            let pk=sq_pk.clone(); let sig=sq_sig.clone();
            hs.push(s.spawn(move|| { let mut o=0u64; for _ in 0..per { if flux_sqisign::verify(MSG,&sig,&pk).unwrap_or(false){o+=1;} } o }));
        }
        let mut t=0u64; for h in hs { t+=h.join().unwrap(); }
        std::hint::black_box(t);
    });
    let sq_par = (per*cores) as f64 / t0.elapsed().as_secs_f64();
    println!("    {cores}-thread:     {sq_par:>10.0} verifies/s", );
    let _ = ok;

    // ── B/C/D. ed25519 single / batch / batch×parallel ──────────────────
    println!("\n  ── B/C/D. ed25519 (hot-path candidate) ──");
    let mut rng = OsRng;
    let n = 20_000usize;
    let keys: Vec<SigningKey> = (0..n).map(|_| SigningKey::generate(&mut rng)).collect();
    let vks: Vec<VerifyingKey> = keys.iter().map(|k| k.verifying_key()).collect();
    let sigs: Vec<Signature> = keys.iter().map(|k| k.sign(MSG)).collect();
    let msgs: Vec<&[u8]> = vec![MSG; n];

    // B: single
    let t0 = Instant::now();
    let mut o=0u64; for i in 0..n { if vks[i].verify(MSG, &sigs[i]).is_ok(){o+=1;} }
    let ed_single = n as f64 / t0.elapsed().as_secs_f64(); std::hint::black_box(o);
    println!("    B single:          {ed_single:>10.0} verifies/s");

    // C: verify_batch (single thread, batched MSM)
    let t0 = Instant::now();
    ed25519_dalek::verify_batch(&msgs, &sigs, &vks).expect("batch verify");
    let ed_batch = n as f64 / t0.elapsed().as_secs_f64();
    println!("    C verify_batch:    {ed_batch:>10.0} verifies/s   ({:.1}× vs single)", ed_batch/ed_single);

    // D: batch × parallel — split into per-core batches
    let t0 = Instant::now();
    let chunk = n.div_ceil(cores);
    std::thread::scope(|s|{
        let mut hs=Vec::new();
        for c in 0..cores {
            let lo=c*chunk; let hi=(lo+chunk).min(n); if lo>=hi {continue;}
            let m=&msgs[lo..hi]; let sg=&sigs[lo..hi]; let vk=&vks[lo..hi];
            hs.push(s.spawn(move|| ed25519_dalek::verify_batch(m, sg, vk).is_ok()));
        }
        for h in hs { let _=h.join().unwrap(); }
    });
    let ed_bp = n as f64 / t0.elapsed().as_secs_f64();
    println!("    D batch×{cores}t:      {ed_bp:>10.0} verifies/s   ({:.1}× vs single)", ed_bp/ed_single);

    // ── verify-once amortization (the Narwhal structural lever) ──────────
    println!("\n  ── E. verify-once amortization (Narwhal mempool) ──");
    println!("    A sig is verified ONCE on mempool ingest, then the tx is ordered +");
    println!("    executed by hash without re-verification. Effective TPS =");
    println!("    ingest_verify_rate (no per-block re-verify tax).");
    println!("    → ed25519 batch×parallel ingest: {ed_bp:.0} tx/s sustainable");
    println!("    → SQIsign ingest:                {sq_par:.0} tx/s sustainable");

    // ── verdict ──────────────────────────────────────────────────────────
    println!("\n  ── VERDICT: the wall, optimized ──");
    println!("    SQIsign L5:           {sq_par:>10.0}/s  (post-quantum, settlement only)");
    println!("    ed25519 batch×{cores}t:   {ed_bp:>10.0}/s  (hot path)");
    let lift = ed_bp / sq_par.max(1.0);
    println!("    hot-path lift over SQIsign: {lift:.0}×");
    println!();
    println!("    RECOMMENDATION (crypto-agility split, rocky #101):");
    println!("    • Hot path (high-freq agent txs): ed25519 batch×parallel → {ed_bp:.0}/s.");
    println!("    • Settlement / finality only:     SQIsign L5 → {sq_par:.0}/s.");
    println!("    • Narwhal: verify once on ingest at the hot-path rate; consensus");
    println!("      orders hashes → no per-block re-verify. The wall moves from");
    println!("      113k/s (naive single ed25519) to {ed_bp:.0}/s (batch×parallel),");
    println!("      a {:.0}× lift, with SQIsign reserved for the slow settlement path.\n", ed_bp/113655.0);
}
