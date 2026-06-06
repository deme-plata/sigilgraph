//! EdDSA-in-circuit AIR — measure the PROVER cost of the SOUND 500M path.
//!
//! `sig_agg.rs` measured the verifier side: proof-carrying blocks give O(1)-in-N
//! verification (1.07B eff verify/s). But it proved a TINY folded commitment with
//! EMPTY constraints (~1ms) — it commits to the sig-SET, it does NOT prove the
//! sigs are VALID. The honest gap is: to be SOUND, the ed25519 verification
//! equation  [S]·B = R + [k]·A  must run INSIDE the trace.
//!
//! ed25519 verify ≈ two 256-bit scalar multiplications via double-and-add ≈
//! ~2^13–2^14 field-operation steps (plus the SHA-512 for k). This harness
//! builds a trace at that realistic scale — a real twisted-Edwards-style
//! double-and-add recurrence over a prime field — and measures the prover
//! (`flux-zk-stark` FRI/Merkle) cost as the trace grows to ed25519 size.
//!
//! HONEST: flux-zk-stark's prove is real FFT+Merkle+FRI (cost ∝ trace size), but
//! its constraint EVALUATION is a documented placeholder. So prove_ms here is the
//! commitment FLOOR for an ed25519-scale trace — a LOWER BOUND on the true sound
//! prover cost (real AIR constraint evaluation adds more on top). That floor is
//! already the number that decides "sound 500M = how many prover boxes".

use std::time::Instant;
use flux_zk_stark::StarkSystem;

// 61-bit Mersenne prime field for the in-trace arithmetic (fast reduction).
const P: u128 = (1 << 61) - 1;
#[inline] fn fmul(a: u64, b: u64) -> u64 { ((a as u128 * b as u128) % P) as u64 }
#[inline] fn fadd(a: u64, b: u64) -> u64 { ((a as u128 + b as u128) % P) as u64 }

/// Emit a trace of `rows` steps of a real twisted-Edwards-style double-and-add
/// recurrence over `cols` field registers. Faithful in SHAPE and COST to the
/// scalar-mult inner loop that dominates ed25519 verification.
fn eddsa_scalar_mul_trace(rows: usize, cols: usize) -> Vec<Vec<u64>> {
    let mut st = [1u64, 2, 1, 2, 3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37, 41];
    let cols = cols.max(4);
    let mut trace = Vec::with_capacity(rows);
    for i in 0..rows {
        // point doubling (b,c) + addition (a,d,e) field ops — the per-bit work
        let (x, y) = (st[0], st[1]);
        let a = fmul(x, y);
        let b = fmul(x, x);
        let c = fmul(y, y);
        let d = fadd(a, b);
        let e = fadd(c, a);
        st[0] = fadd(d, st[2]);
        st[1] = fadd(e, st[3]);
        st[2] = fmul(d, e);
        st[3] = fadd(st[3], fadd(a, (i as u64) & 1)); // conditional add on scalar bit
        for j in 4..cols.min(st.len()) {
            st[j] = fadd(fmul(st[j], st[(j + 1) % st.len()]), b);
        }
        let mut row = vec![0u64; cols];
        let n = st.len().min(cols);
        row[..n].copy_from_slice(&st[..n]);
        trace.push(row);
    }
    trace
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    println!("\n  EdDSA-IN-CIRCUIT AIR — PROVER cost of the SOUND 500M path  [{cores} cores]\n");
    println!("  ed25519 verify ≈ 2× 256-bit double-and-add ≈ 2^13–2^14 field-op rows.\n");

    let mut sys = StarkSystem::new(false).await.expect("stark system (cpu)");
    let cols = 16;

    println!("  {:>7}  {:>6}  {:>11}  {:>12}  {:>16}", "rows", "cols", "prove_ms", "proof_B", "sigs/s/box (floor)");
    let mut per_sig_ms = 0.0f64;
    for &log_rows in &[10usize, 12, 13, 14] {
        let rows = 1usize << log_rows;
        let trace = eddsa_scalar_mul_trace(rows, cols);
        let _ = sys.prove(&trace, &[]).await; // warm
        let t0 = Instant::now();
        let proof = sys.prove(&trace, &[]).await.expect("prove");
        let prove_ms = t0.elapsed().as_secs_f64() * 1000.0;
        // one ed25519 verify ≈ one trace of this size → amortized 1 sig / prove
        let sigs_s = 1000.0 / prove_ms;
        if log_rows == 14 { per_sig_ms = prove_ms; }
        println!("  {rows:>7}  {cols:>6}  {prove_ms:>11.1}  {:>12}  {sigs_s:>16.1}", proof.size_bytes());
    }

    // ── the honest 500M math ─────────────────────────────────────────────
    let sigs_s = 1000.0 / per_sig_ms.max(1e-9);
    let boxes_500m = 500_000_000.0 / sigs_s;
    println!("\n  ── SOUND 500M — the prover wall (from the 2^14-row ed25519-scale floor) ──");
    println!("    per-sig prover floor (commitment only): {per_sig_ms:.1} ms");
    println!("    prover throughput, 1 CPU box:           {sigs_s:.0} sound sigs/s");
    println!("    CPU prover boxes for 500M TPS:          {boxes_500m:.0}");
    println!("    with GPU proving (flux-zk path, ~10–50×): {:.0}–{:.0} boxes",
             boxes_500m / 50.0, boxes_500m / 10.0);
    println!();
    println!("  ── the asymmetry that makes it WORK anyway ──");
    println!("    • VERIFIER stays O(1): every node checks ONE ~50KB proof in 0.06ms");
    println!("      (sig_agg.rs: 1.07B eff verify/s) — UNCHANGED by prover cost.");
    println!("    • PROVING is offloadable: a few dedicated GPU prover boxes serve a");
    println!("      whole network of cheap O(1) verifiers. Provers don't gate consensus.");
    println!("    • This floor is commitment-only; real AIR constraint evaluation (the");
    println!("      EdDSA equation) adds more — so treat {sigs_s:.0} sigs/s/box as an");
    println!("      UPPER bound on sound prover throughput, not a promise.\n");
}
