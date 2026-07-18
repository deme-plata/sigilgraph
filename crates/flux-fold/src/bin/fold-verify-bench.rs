//! CPU-only, GPU-free honest benchmark of the flux-fold prove + VERIFY path at
//! the real light-client parameterization (m=64, n=256 → 2568-byte proof).
//!
//! The whitepaper's headline pairs "2,568 B proof" with "342 ms verification".
//! The 2,568 B figure is a real, exact constant ((m+n)*8+8 with m=64,n=256).
//! The 342 ms figure had, until this bench, no measurement asserting it — it was
//! a poster label (`docs/SIGIL_MURAL.md`). This bin measures the real numbers so
//! the paper can cite a measured value, and it surfaces the honest nuance that
//! `flux_fold::verify` is O(M): the *proof* is constant-size, but verification
//! reads all M commitments.
//!
//! Run:  fold-verify-bench [M]        (default M = 100_000)
//! Env:  FOLD_M / FOLD_N override the matrix dims (default 64 / 256).
use std::time::Instant;
use flux_fold::{Ajtai, fold, verify};

const Q: u64 = 2_147_483_647;

fn block_witness(seed: u64, n: usize) -> Vec<u64> {
    // Same construction family as chronos-fold-bench: blake3-XOF a witness per block.
    let mut h = blake3::Hasher::new();
    h.update(b"sigil-chronos/fold-block-witness/v1");
    h.update(&seed.to_le_bytes());
    let mut xof = h.finalize_xof();
    let mut buf = vec![0u8; n * 8];
    xof.fill(&mut buf);
    (0..n)
        .map(|j| {
            let mut a = [0u8; 8];
            a.copy_from_slice(&buf[j * 8..j * 8 + 8]);
            u64::from_le_bytes(a) % Q
        })
        .collect()
}

fn main() {
    let m: usize = std::env::var("FOLD_M").ok().and_then(|s| s.parse().ok()).unwrap_or(64);
    let n: usize = std::env::var("FOLD_N").ok().and_then(|s| s.parse().ok()).unwrap_or(256);
    let count: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(100_000);

    let ajtai = Ajtai::from_seed(m, n, &[7u8; 32]);
    let wits: Vec<Vec<u64>> = (0..count).map(|i| block_witness(i as u64, n)).collect();

    // Public commitments — the verifier's input. Measure their construction too.
    let t_c = Instant::now();
    let coms: Vec<Vec<u64>> = wits.iter().map(|w| ajtai.commit(w)).collect();
    let commit_ms = t_c.elapsed().as_secs_f64() * 1000.0;

    // Prove (fold).
    let t_f = Instant::now();
    let proof = fold(&ajtai, &wits);
    let fold_ms = t_f.elapsed().as_secs_f64() * 1000.0;

    // Verify — the headline number. Warm once, then time.
    assert!(verify(&ajtai, &coms, &proof), "honest fold must verify");
    let t_v = Instant::now();
    let ok = verify(&ajtai, &coms, &proof);
    let verify_ms = t_v.elapsed().as_secs_f64() * 1000.0;

    let proof_bytes = proof.size_bytes();
    let commit_input_bytes = coms.len() * m * 8; // what the verifier must hold

    println!("FLUX-FOLD-VERIFY-BENCH  {}", if ok { "OK" } else { "FAIL" });
    println!("  params            m={m} n={n}  blocks(M)={count}");
    println!("  proof size        {proof_bytes} B   (constant in M — the succinctness claim)");
    println!("  commit input      {commit_input_bytes} B   (verifier must hold M commitments — O(M))");
    println!("  commit build      {commit_ms:.1} ms");
    println!("  fold (prove)      {fold_ms:.1} ms");
    println!("  VERIFY            {verify_ms:.1} ms   <- the headline number, measured");
    println!("  verify throughput {:.0} blocks/s (amortized)", (count as f64) / (verify_ms / 1000.0).max(1e-9));
    if !ok {
        std::process::exit(1);
    }
}
