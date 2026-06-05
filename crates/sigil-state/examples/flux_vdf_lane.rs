//! The VDF lane — the second lane of the dual-lane Flux miner.
//!
//! BLAKE4 (flux_blake4.rs) is the POWER axis: parallel hashes/sec, measured in
//! Φ. You buy more of it with more hardware. The VDF lane is the TIME axis:
//! a Wesolowski verifiable delay function, y = x^(2^t) mod N — t SEQUENTIAL
//! squarings that CANNOT be parallelized (each depends on the last). You cannot
//! buy it with more cores; only a faster single core helps. That's why Quillon's
//! VDF mining is "brilliant": it's egalitarian + ASIC-resistant — one fast core
//! ≈ one vote — and it's proof-of-elapsed-TIME, not proof-of-spent-POWER.
//!
//! This measures the sequential squaring rate (the VDF's speed) and introduces
//! the Ω unit. The group here is a 2048-bit modulus for the RATE measurement;
//! a production VDF uses a no-trusted-setup group (class group, or a genus-2
//! hyperelliptic Jacobian — Quillon's choice, ASIC-hardest). The sequential
//! CHARACTER — and the dual-lane economics — are what matter here.

use std::time::Instant;
use num_bigint::BigUint;

// A fixed 2048-bit odd modulus for the squaring-rate bench (not a real RSA/class
// group — we measure turns/sec, which is group-size-bound, not group-kind-bound).
fn modulus_2048() -> BigUint {
    let mut n = (BigUint::from(1u32) << 2047) | BigUint::from(1u32); // top + bottom bit
    n |= BigUint::from(0x9e3779b97f4a7c15u64) << 900; // some spread bits
    n |= BigUint::from(0xbf58476d1ce4e5b9u64) << 1500;
    n
}

/// `steps` SEQUENTIAL squarings x := x^2 mod n. Returns (final, turns/sec).
fn vdf_run(x0: &BigUint, n: &BigUint, steps: u64) -> (BigUint, f64) {
    let mut x = x0.clone();
    let t0 = Instant::now();
    for _ in 0..steps {
        x = (&x * &x) % n; // ONE turn — the unit of sequential work
    }
    let secs = t0.elapsed().as_secs_f64().max(1e-9);
    (x, steps as f64 / secs)
}

// ── the Ω unit: sequential delay-rate. 1 Ω = 1 Mega-turn/s (10^6 squarings/s) ──
fn format_omega(tps: f64) -> String {
    let o = tps / 1e6;
    if o >= 1.0 { format!("{o:.3} Ω") } else { format!("{:.1} mΩ", o * 1e3) }
}

fn main() {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    println!("\n  THE VDF LANE — proof of elapsed TIME (dual-lane Flux miner)  [{cores} cores]\n");
    println!("  Ω unit: 1 Ω = 1 Mega-turn/s (10^6 sequential squarings/s). A 'turn' = one x^2 mod N.\n");

    let n = modulus_2048();
    let x0 = BigUint::from(0xC0FFEEu64);
    let warm = vdf_run(&x0, &n, 2000); std::hint::black_box(warm.0);

    // 1) single-core sequential rate (the VDF's true speed)
    let steps = 40_000u64;
    let (_, rate1) = vdf_run(&x0, &n, steps);
    println!("  single-core VDF rate: {:.0} turns/s = {}", rate1, format_omega(rate1));

    // 2) the anti-parallel proof: run `cores` SEPARATE chains at once. You get
    //    `cores` proofs, but EACH chain still runs at ~the single-core rate —
    //    so a SINGLE block's VDF gets ZERO speedup from more cores.
    let t0 = Instant::now();
    let per = 8_000u64;
    std::thread::scope(|s| {
        for c in 0..cores {
            let (n, x0) = (n.clone(), BigUint::from(0xC0FFEEu64 + c as u64));
            s.spawn(move || { let r = vdf_run(&x0, &n, per); std::hint::black_box(r.0); });
        }
    });
    let wall = t0.elapsed().as_secs_f64();
    let per_chain_rate = per as f64 / wall; // rate of ONE chain while all cores busy
    println!("  {cores} parallel chains: each still ~{} (one VDF can't go faster with more cores)",
             format_omega(per_chain_rate));

    // 3) what the rate BUYS — delay. A chain difficulty of T turns takes:
    println!("\n  what the Ω rate buys (delay = T / rate):");
    for t in [1_000_000u64, 10_000_000, 100_000_000] {
        println!("    T = {:>11} turns  →  {:.2} s of unforgeable sequential delay", t, t as f64 / rate1);
    }

    // 4) verify is asymmetric-cheap (the VDF promise: slow to prove, fast to check)
    //    — Wesolowski verify ≈ a couple of exponentiations, independent of T.
    println!("\n  verify: O(1) in T (a Wesolowski proof checks in ~one exp, regardless of how");
    println!("          many turns were done) — same asymmetric-work shape as proof-carrying blocks.");

    // ── the DUAL-LANE picture ────────────────────────────────────────────
    println!("\n  ── DUAL-LANE: the two axes of the Flux miner ──");
    println!("    LANE A — BLAKE4   (Φ, POWER):  ~155 MH/s sound on {cores}c → scales ~{cores}× with cores.");
    println!("                                   parallel · hardware-buyable · GPU/ASIC edge · liveness.");
    println!("    LANE B — VDF g-2  (Ω, TIME):   {} single-core → scales ~1× (sequential!).", format_omega(rate1));
    println!("                                   anti-parallel · one-fast-core-one-vote · ASIC-resistant.");
    println!();
    println!("    A block carries BOTH: a BLAKE4 solution (you did the WORK) AND a VDF proof");
    println!("    (real TIME elapsed, un-shortcuttable). Power can't fake time; time can't fake power.");
    println!("    That's the brilliance — throughput-PoW for liveness, genus-2 VDF for fair,");
    println!("    grind-proof, egalitarian leader election. Φ is how much; Ω is how long.\n");
}
