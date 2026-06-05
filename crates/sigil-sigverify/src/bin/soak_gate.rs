//! P7-F — the soak measurement gate.
//!
//! Runs parallel signature verification continuously for a fixed duration and
//! measures SUSTAINED throughput (per-second samples), then PASS/FAILs against a
//! target. The P7 ship gate is "sustain 1M+ sigs/sec for 1h on a 48-core box";
//! this binary is what decides that, honestly — a peak burst doesn't pass, the
//! *worst* one-second window has to clear the bar.
//!
//! Usage:
//!   soak_gate [--secs N] [--threshold SIGS_PER_SEC] [--batch N] [--scheme ed25519]
//!
//! Defaults: --secs 60  --threshold 1000000  --batch 10000
//! Full ship gate:  soak_gate --secs 3600 --threshold 1000000
//!
//! Exit code: 0 = PASS (worst-second >= threshold), 1 = FAIL, 2 = bad args.
//!
//! Honesty notes baked in:
//!   - keys/sigs are built ONCE outside the timed loop (we measure verify, not keygen)
//!   - throughput is sampled per wall-clock second, not averaged over the whole run,
//!     so a warmup spike or a GC-style stall both show up
//!   - reports min/avg/max per-second + the worst window, and gates on the WORST

use std::time::{Duration, Instant};

use ed25519_dalek::{Signer, SigningKey};
use sigil_sigverify::{verify_batch_parallel, Ed25519Verifier, VerifyItem};

struct Args {
    secs: u64,
    threshold: f64,
    batch: usize,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args { secs: 60, threshold: 1_000_000.0, batch: 10_000 };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--secs" => a.secs = next_val(&mut it, "--secs")?,
            "--threshold" => a.threshold = next_val(&mut it, "--threshold")?,
            "--batch" => a.batch = next_val(&mut it, "--batch")?,
            "--scheme" => {
                let s: String = next_val(&mut it, "--scheme")?;
                if s != "ed25519" {
                    return Err(format!("only --scheme ed25519 supported here (got {s}); \
                        SQIsign soak needs --features sqisign + SqiSignVerifier"));
                }
            }
            "-h" | "--help" => return Err("help".into()),
            other => return Err(format!("unknown flag: {other}")),
        }
    }
    Ok(a)
}

fn next_val<T: std::str::FromStr>(
    it: &mut impl Iterator<Item = String>,
    flag: &str,
) -> Result<T, String> {
    it.next()
        .ok_or_else(|| format!("{flag} needs a value"))?
        .parse()
        .map_err(|_| format!("{flag}: invalid value"))
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            if e == "help" {
                eprintln!("soak_gate [--secs N] [--threshold SIGS/S] [--batch N] [--scheme ed25519]");
                std::process::exit(0);
            }
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    };

    let cores = num_cpus::get();
    println!("=== P7-F soak gate (ed25519, parallel) ===");
    println!("cores:     {cores}");
    println!("duration:  {}s", args.secs);
    println!("threshold: {:.0} sigs/s (PASS = worst 1s window >= this)", args.threshold);
    println!("batch:     {}", args.batch);

    // ── build the signed batch ONCE, outside timing ───────────────────────
    println!("\nbuilding {} signed items (untimed)...", args.batch);
    let built: Vec<(Vec<u8>, Vec<u8>, Vec<u8>)> = (0..args.batch)
        .map(|i| {
            let mut seed = [0u8; 32];
            seed[..8].copy_from_slice(&(i as u64).to_le_bytes());
            let sk = SigningKey::from_bytes(&seed);
            let msg = format!("sigil-soak-tx-{i}").into_bytes();
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

    // ── soak loop with per-second sampling ────────────────────────────────
    println!("\nsoaking for {}s...\n", args.secs);
    let run_start = Instant::now();
    let total_dur = Duration::from_secs(args.secs);

    let mut samples: Vec<f64> = Vec::with_capacity(args.secs as usize + 1);
    let mut window_start = Instant::now();
    let mut window_verifies: u64 = 0;
    let mut grand_total: u64 = 0;

    while run_start.elapsed() < total_dur {
        let out = verify_batch_parallel(&v, &items);
        // sanity: all must verify (a false here = a correctness bug, abort the soak)
        if !out.iter().all(|&r| r) {
            eprintln!("FATAL: a known-good signature failed verification during soak — \
                correctness bug, aborting (not a perf result).");
            std::process::exit(3);
        }
        window_verifies += items.len() as u64;
        grand_total += items.len() as u64;

        // close a 1-second sampling window
        let w = window_start.elapsed();
        if w >= Duration::from_secs(1) {
            let rate = window_verifies as f64 / w.as_secs_f64();
            samples.push(rate);
            println!("  [{:>4}s] {:>12.0} verifies/s", samples.len(), rate);
            window_start = Instant::now();
            window_verifies = 0;
        }
    }
    // flush a final partial window if it has meaningful data (>=100ms)
    let w = window_start.elapsed();
    if w >= Duration::from_millis(100) && window_verifies > 0 {
        samples.push(window_verifies as f64 / w.as_secs_f64());
    }

    let elapsed = run_start.elapsed().as_secs_f64();

    // ── verdict ───────────────────────────────────────────────────────────
    let (min, max, avg) = if samples.is_empty() {
        let overall = grand_total as f64 / elapsed;
        (overall, overall, overall)
    } else {
        let min = samples.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = samples.iter().cloned().fold(0.0, f64::max);
        let avg = samples.iter().sum::<f64>() / samples.len() as f64;
        (min, max, avg)
    };

    println!("\n════════ SOAK GATE RESULT ════════");
    println!("  host cores:       {cores}");
    println!("  wall time:        {elapsed:.1}s");
    println!("  total verifies:   {grand_total}");
    println!("  per-second min:   {min:.0} sigs/s   <- the gate looks at THIS");
    println!("  per-second avg:   {avg:.0} sigs/s");
    println!("  per-second max:   {max:.0} sigs/s");
    println!("  overall avg:      {:.0} sigs/s", grand_total as f64 / elapsed);
    println!("  threshold:        {:.0} sigs/s", args.threshold);

    let pass = min >= args.threshold;
    if pass {
        println!("\n  ✅ PASS — worst 1s window cleared the bar; sustained for {elapsed:.0}s.");
        std::process::exit(0);
    } else {
        let gap = args.threshold / min.max(1.0);
        println!("\n  ❌ FAIL — worst window {min:.0}/s is {gap:.1}× below the {:.0}/s target.", args.threshold);
        println!("     (ed25519 alone won't reach 1M/s on this box; the gate is for the");
        println!("      combined P7 work — batch-verify P7-A, BLS P7-B, SIMD P7-D.)");
        std::process::exit(1);
    }
}
