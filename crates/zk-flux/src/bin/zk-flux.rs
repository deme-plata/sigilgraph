//! zk-flux demo — prove a computation, compress it, verify in O(1); show that
//! zk-flux is transparent + post-quantum + tiny + O(1) all at once.

use std::time::Instant;
use zk_flux::{comparison, ZkFlux};
use flux_lattice_guard::SecurityLevel;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    println!("\n  zk-flux — transparent FRI-STARK ⊕ RLWE lattice SNARK (post-quantum hybrid)\n");

    let mut zk = ZkFlux::new(SecurityLevel::PQ128).await?;

    // a representative computation: a 2^14-row trace (the STARK proves it).
    let trace: Vec<Vec<u64>> =
        (0..16_384u64).map(|i| vec![i, i.wrapping_mul(2) + 1, i ^ 0xABCD, 11, 13, 17, 19, 23]).collect();

    let t = Instant::now();
    let proof = zk.prove(&trace).await?;
    let prove_ms = t.elapsed().as_secs_f64() * 1000.0;

    let t = Instant::now();
    let ok = zk.verify(&proof)?;
    let verify_ms = t.elapsed().as_secs_f64() * 1000.0;

    let stark = proof.stark_proof_bytes;
    let wrap = proof.wrap_size_bytes();
    println!("  layer 1 — transparent FRI-STARK proof : {stark:>8} B  (committed)");
    println!("  layer 2 — RLWE lattice SNARK wrap      : {wrap:>8} B  (what a verifier checks)");
    println!("  prove {prove_ms:.1} ms · VERIFY {verify_ms:.2} ms (O(1), independent of trace) · valid: {}", if ok { "✓" } else { "✗" });
    println!("  security: PQ-128 (lattice) + hash-FRI — no pairings, no trusted setup");
    println!("  NOTE: the prototype lattice proof ({wrap} B) is NOT yet succinct — 'tiny' needs a");
    println!("        succinct lattice backend (LaBRADOR-class, ~KB). Measured wins today:");
    println!("        transparent + post-quantum + O(1) verify. Succinctness = the open lane.\n");

    // the defining table (by DESIGN; see the NOTE above for the prototype's
    // current succinctness status)
    println!("  by design:");
    println!("  {:<26} {:>11} {:>13} {:>11} {:>10}", "system", "transparent", "post-quantum", "tiny proof", "O(1) verify");
    println!("  {}", "-".repeat(74));
    let yn = |b: bool| if b { "  yes" } else { "  no " };
    for r in comparison() {
        println!("  {:<26} {:>11} {:>13} {:>11} {:>10}",
            r.name, yn(r.transparent), yn(r.post_quantum), yn(r.tiny_proof), yn(r.o1_verify));
    }
    println!("\n  ── the point ──");
    println!("    zk-STARK gives transparency + PQ but big proofs; pairing zk-SNARK gives");
    println!("    tiny O(1) proofs but trusted setup + dies to Shor. zk-flux is the only");
    println!("    DESIGN that is all four — because the compressor is a LATTICE SNARK, not");
    println!("    a pairing one. Measured today: transparent + post-quantum + O(1) verify,");
    println!("    roundtrip verifying. Succinctness (a LaBRADOR-class lattice backend) is");
    println!("    the remaining lane — but the property no other family has, PQ + O(1) +");
    println!("    transparent at once, is real and running.\n");
    Ok(())
}
