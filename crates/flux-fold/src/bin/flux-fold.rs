//! flux-fold demo — show the succinctness: fold M statements into a proof whose
//! size does NOT grow with M (constant), and verify it.

use std::time::Instant;
use flux_fold::{fold, verify, Ajtai, Q};

fn gen(count: usize, n: usize, seed: u64) -> Vec<Vec<u64>> {
    let mut s = seed | 1;
    let mut nx = || { s ^= s << 13; s ^= s >> 7; s ^= s << 17; s % Q };
    (0..count).map(|_| (0..n).map(|_| nx()).collect()).collect()
}

fn main() {
    println!("\n  flux-fold — succinct lattice backend (Ajtai/SIS commit + Fiat-Shamir fold)\n");
    let (m, n) = (64usize, 256usize);
    let ajtai = Ajtai::from_seed(m, n, &[7u8; 32]);
    println!("  transparent setup: A is {m}×{n} over Z_q (q=2^31-1), derived from a public seed");
    println!("  post-quantum: binding under SIS · no trusted setup\n");

    println!("  {:>10}  {:>12}  {:>12}  {:>10}", "M folded", "proof bytes", "fold ms", "verify ms");
    println!("  {}", "-".repeat(50));
    for &count in &[1usize, 16, 256, 1024, 4096] {
        let ws = gen(count, n, 0x55 + count as u64);
        let coms: Vec<Vec<u64>> = ws.iter().map(|w| ajtai.commit(w)).collect();
        let t = Instant::now();
        let proof = fold(&ajtai, &ws);
        let fold_ms = t.elapsed().as_secs_f64() * 1000.0;
        let t = Instant::now();
        let ok = verify(&ajtai, &coms, &proof);
        let verify_ms = t.elapsed().as_secs_f64() * 1000.0;
        assert!(ok);
        println!("  {count:>10}  {:>12}  {:>12.2}  {:>10.3}", proof.size_bytes(), fold_ms, verify_ms);
    }

    println!("\n  ── the succinctness ──");
    println!("    The proof is {} bytes ({}+{} field elements) NO MATTER how many", (m + n) * 8 + 8, m, n);
    println!("    statements are folded — 1 or 4096, same size. That's succinct in M:");
    println!("    a proof that does NOT grow with the number of things it attests.");
    println!("    (Open lane v0.3: recursively fold the witness dimension n itself,");
    println!("     LaBRADOR's √-step, so it's succinct in n too → ~KB absolute.)\n");
}
