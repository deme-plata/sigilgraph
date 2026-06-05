//! BLAKE4 + the FLUX hashpower unit — the heart of the Flux miner.
//!
//! "BLAKE4" was judged DEAD for STATE ROOTS (once roots are O(1), a faster leaf
//! hash buys ~nothing — see roots_throughput.rs). But mining is the OPPOSITE
//! regime: the hash IS the product. Every extra hash/sec is hashrate. So for the
//! miner, BLAKE4 is alive — and the win is "the Flux way": fan the nonce search
//! across every core in a tight, branch-free loop.
//!
//! Two cores, measured side by side:
//!   • BLAKE4-sound  — BLAKE3 compression, Flux-parallel over all cores. The
//!                     DEPLOYABLE PoW hash (preimage-hard → can't shortcut the
//!                     difficulty search).
//!   • BLAKE4-turbo  — a fast non-crypto mix core. The SPEED CEILING ("what if
//!                     the hash were free"); NOT a sound PoW hash (invertible) —
//!                     shown only to bound the parallel ceiling, exactly like
//!                     roots_throughput's "fasthash" lever.
//!
//! …and reports everything in the FLUX unit (Φ): 1 Φ ≡ 1 EH/s, so the SI prefix
//! lines up with the hash magnitude shifted by 18 — nanoflux IS gigahash.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

const HEADER: &[u8] = b"sigil-g0-block-header-preimage-80-bytes-padding-padding-padding-padding-pad!!";

// ── BLAKE4 cores ─────────────────────────────────────────────────────────────

/// Sound PoW hash: BLAKE3 over header‖nonce → first 8 bytes as the target word.
#[inline]
fn blake4_sound(header: &[u8], nonce: u64) -> u64 {
    let mut h = blake3::Hasher::new();
    h.update(header);
    h.update(&nonce.to_le_bytes());
    let b = h.finalize();
    u64::from_le_bytes(b.as_bytes()[0..8].try_into().unwrap())
}

/// Turbo (ceiling-only) core: a fast keyed mix. Preimage-WEAK → not deployable,
/// measures the parallel ceiling if the per-hash cost went to ~zero.
#[inline]
fn blake4_turbo(seed: u64, nonce: u64) -> u64 {
    let mut x = seed ^ nonce.wrapping_mul(0x9e3779b97f4a7c15);
    x ^= x >> 30; x = x.wrapping_mul(0xbf58476d1ce4e5b9);
    x ^= x >> 27; x = x.wrapping_mul(0x94d049bb133111eb);
    x ^= x >> 31; x
}

// ── the FLUX hashpower unit (Φ): 1 Φ ≡ 1 EH/s ───────────────────────────────
fn format_flux(hps: f64) -> String {
    // SI prefix on Φ; nanoΦ = GH/s, microΦ = TH/s, milliΦ = PH/s, Φ = EH/s.
    let flux = hps / 1e18; // value in Φ
    let (v, unit) = if flux >= 1.0 { (flux, "Φ") }
        else if flux >= 1e-3 { (flux * 1e3, "mΦ") }
        else if flux >= 1e-6 { (flux * 1e6, "µΦ") }
        else if flux >= 1e-9 { (flux * 1e9, "nΦ") }
        else if flux >= 1e-12 { (flux * 1e12, "pΦ") }
        else { (flux * 1e15, "fΦ") };
    format!("{v:.3} {unit}")
}
fn format_hps(hps: f64) -> String {
    if hps >= 1e9 { format!("{:.2} GH/s", hps / 1e9) }
    else if hps >= 1e6 { format!("{:.2} MH/s", hps / 1e6) }
    else { format!("{:.0} H/s", hps) }
}

/// Mine for `secs` across `threads`, fanning disjoint nonce ranges. Returns
/// (total_hashes, solutions_found) for a difficulty of `lead_zero_bits`.
fn mine(secs: f64, threads: usize, lead_zero_bits: u32, turbo: bool) -> (u64, u64) {
    let target = if lead_zero_bits >= 64 { 0 } else { u64::MAX >> lead_zero_bits };
    let stop = AtomicBool::new(false);
    let total = AtomicU64::new(0);
    let solved = AtomicU64::new(0);
    let t0 = Instant::now();
    std::thread::scope(|s| {
        for t in 0..threads {
            let (stop, total, solved) = (&stop, &total, &solved);
            s.spawn(move || {
                let mut nonce = (t as u64) << 40; // disjoint lane per thread
                let mut local = 0u64;
                let mut sol = 0u64;
                loop {
                    // unrolled batch so the stop-flag/clock checks amortize
                    for _ in 0..4096 {
                        let h = if turbo { blake4_turbo(0x5161, nonce) } else { blake4_sound(HEADER, nonce) };
                        if h <= target { sol += 1; }
                        nonce += 1;
                    }
                    local += 4096;
                    if stop.load(Ordering::Relaxed) { break; }
                }
                total.fetch_add(local, Ordering::Relaxed);
                solved.fetch_add(sol, Ordering::Relaxed);
            });
        }
        while t0.elapsed().as_secs_f64() < secs { std::thread::sleep(std::time::Duration::from_millis(20)); }
        stop.store(true, Ordering::Relaxed);
    });
    (total.load(Ordering::Relaxed), solved.load(Ordering::Relaxed))
}

fn main() {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    println!("\n  BLAKE4 — the Flux miner hash  [{cores} cores]\n");
    println!("  the FLUX unit:  1 Φ ≡ 1 EH/s  →  1 nΦ = 1 GH/s · 1 µΦ = 1 TH/s · 1 mΦ = 1 PH/s\n");

    let secs = 4.0;
    println!("  {:>16}  {:>14}  {:>16}  {:>14}", "core", "hashrate", "FLUX", "vs 1 EH/s");
    println!("  {}", "-".repeat(66));
    for (name, turbo) in [("BLAKE4-sound (PoW)", false), ("BLAKE4-turbo (ceiling)", true)] {
        // single-thread baseline
        let (h1, _) = mine(secs, 1, 64, turbo);
        let hps1 = h1 as f64 / secs;
        // full parallel
        let (hn, _) = mine(secs, cores, 64, turbo);
        let hpsn = hn as f64 / secs;
        let eh_frac = hpsn / 1e18;
        println!("  {name:>16}  {:>14}  {:>16}  1 / {:>10.0}", format_hps(hpsn), format_flux(hpsn), 1.0 / eh_frac);
        println!("      └ 1 core {} → {} cores {} ({:.0}× parallel)",
                 format_hps(hps1), cores, format_hps(hpsn), hpsn / hps1.max(1.0));
    }

    println!("\n  ── reading the number ──");
    let (hn, _) = mine(secs, cores, 64, false);
    let hpsn = hn as f64 / secs;
    println!("    SIGIL block PoW (BLAKE4-sound) on this box: {} = {}", format_hps(hpsn), format_flux(hpsn));
    println!("    A typical miner 'starts around 1–5 GH/s' → that's 1–5 nanoflux (nΦ).");
    println!("    The whole network reaching an exahash = 1.000 Φ (one Flux).");
    println!("    So you begin at a few BILLIONTHS of a Flux and the network climbs toward 1 Φ.\n");
}
