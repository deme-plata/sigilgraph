//! flux-miner — the dual-lane miner.
//!
//!   flux-miner                                   # demo: measure Φ/Ω, mine+verify one block
//!   flux-miner mine <node-url> <wallet> [n]      # LIVE: challenge→solve→submit loop
//!                                                #   against a node (n shares, default ∞)

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use flux_miner::client::{Endpoints, MineStats, MinerClient};
use flux_miner::{blake4, format_flux, format_omega, mine_dual, verify_dual};
use flux_vdf::{eval, ModSquaring, VdfGroup};

fn main() {
    // software autoupdater: swap in any staged binary before doing anything.
    flux_miner::updater::swap_on_launch();
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("mine") => run_mine(&args),
        Some("update") => run_update(&args),
        _ => run_demo(),
    }
}

/// Check a version manifest and self-update if newer. `flux-miner update <url>`
fn run_update(args: &[String]) {
    let url = args.get(2).cloned().unwrap_or_else(|| {
        eprintln!("usage: flux-miner update <manifest-url>");
        std::process::exit(2);
    });
    let current = env!("CARGO_PKG_VERSION");
    println!("flux-miner update — current v{current}, checking {url}");
    match flux_miner::updater::check(&url, current) {
        Some(info) => {
            println!("  ⬆ v{} available — downloading…", info.version);
            match flux_miner::updater::stage(&info.url) {
                Ok(p) => println!("  ✓ staged → {p} · restart to apply"),
                Err(e) => println!("  ✗ stage failed: {e}"),
            }
        }
        None => println!("  ✓ up to date (or no applicable manifest)"),
    }
}

/// LIVE mining: poll a node for challenges, solve the dual-lane block, submit.
fn run_mine(args: &[String]) {
    let url = args.get(2).cloned().unwrap_or_else(|| {
        eprintln!("usage: flux-miner mine <node-url> <wallet> [n-shares]");
        std::process::exit(2);
    });
    let wallet = args.get(3).cloned().unwrap_or_else(|| {
        eprintln!("usage: flux-miner mine <node-url> <wallet> [n-shares]");
        std::process::exit(2);
    });
    let max = args.get(4).and_then(|s| s.parse::<u64>().ok());

    println!("\n  flux-miner LIVE — node {url} · wallet {wallet}");
    println!("  challenge → dual-lane solve (BLAKE4 Φ + VDF Ω) → submit\n");
    let g = ModSquaring::bench_2048();
    let client = match MinerClient::new(Endpoints::standard(&url), wallet) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("  client init failed: {e}");
            std::process::exit(1);
        }
    };
    let mut stats = MineStats::default();
    loop {
        match client.mine_one(&g, &mut stats) {
            Ok(r) => println!(
                "  h={:<8} solve {:>6.0}ms  → {}{}  [✓{} ✗{}]",
                stats.last_height, stats.last_solve_ms,
                if r.accepted { "ACCEPTED" } else { "rejected" },
                r.reason.map(|s| format!(" ({s})")).unwrap_or_default(),
                stats.shares_accepted, stats.shares_rejected,
            ),
            Err(e) => {
                stats.fetch_errors += 1;
                eprintln!("  fetch/submit error: {e} (backing off 3s)");
                std::thread::sleep(Duration::from_secs(3));
            }
        }
        if let Some(m) = max {
            if stats.shares_accepted + stats.shares_rejected >= m {
                println!("\n  done: {} accepted, {} rejected, {} errors", stats.shares_accepted, stats.shares_rejected, stats.fetch_errors);
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn run_demo() {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    println!("\n  flux-miner — DUAL-LANE   BLAKE4 power (Φ)  +  Wesolowski VDF time (Ω)   [{cores} cores]\n");
    let g = ModSquaring::bench_2048();
    let header = b"sigil-g0-demo-block-header";

    // ── Lane A: measure BLAKE4 hashrate across all cores (~2.5 s) ──
    let secs = 2.5;
    let stop = AtomicBool::new(false);
    let total = AtomicU64::new(0);
    let t0 = Instant::now();
    std::thread::scope(|s| {
        for c in 0..cores {
            let (stop, total) = (&stop, &total);
            s.spawn(move || {
                let mut nonce = (c as u64) << 40;
                let mut local = 0u64;
                loop {
                    for _ in 0..4096 {
                        std::hint::black_box(blake4(header, nonce));
                        nonce += 1;
                    }
                    local += 4096;
                    if stop.load(Ordering::Relaxed) { break; }
                }
                total.fetch_add(local, Ordering::Relaxed);
            });
        }
        while t0.elapsed().as_secs_f64() < secs { std::thread::sleep(std::time::Duration::from_millis(20)); }
        stop.store(true, Ordering::Relaxed);
    });
    let hps = total.load(Ordering::Relaxed) as f64 / secs;
    println!("  Lane A · BLAKE4 (power):  {:.1} MH/s  =  {}", hps / 1e6, format_flux(hps));

    // ── Lane B: measure VDF sequential rate (single core) ──
    let x = g.from_seed(&[3u8; 32]);
    let t1 = Instant::now();
    let steps = 20_000u64;
    let _ = eval(&g, &x, steps); // includes the proof pass (2x squarings); rate ~ steps/elapsed*2
    let turns = steps as f64 * 2.0 / t1.elapsed().as_secs_f64();
    println!("  Lane B · VDF    (time):   {:.0} turns/s = {}  (sequential — no core scaling)", turns, format_omega(turns));

    // ── mine a real dual-lane block + verify both lanes ──
    let target = u64::MAX >> 18; // easy demo difficulty
    let vdf_t = 4_000u64;
    let t2 = Instant::now();
    let block = mine_dual(header, target, vdf_t, &g);
    let mine_ms = t2.elapsed().as_secs_f64() * 1000.0;
    let ok = verify_dual(&g, &block, target);

    println!("\n  ── mined block ──");
    println!("    Lane A: nonce={} · BLAKE4 hash 0x{:016x} <= target 0x{:016x}", block.nonce, block.blake4_hash, target);
    println!("    Lane B: VDF t={} turns · y={}B · pi={}B (Wesolowski)", block.vdf.t, block.vdf.y.len(), block.vdf.pi.len());
    println!("    mined in {mine_ms:.0} ms · dual-lane verify: {}", if ok { "✓ BOTH lanes valid" } else { "✗ INVALID" });

    println!("\n  ── the design ──");
    println!("    a valid block needs BOTH: BLAKE4 <= target (you did the WORK, Φ)");
    println!("    AND a verified VDF proof (real TIME elapsed, Ω). Power can't fake");
    println!("    time; time can't fake power. Φ is how much; Ω is how long.\n");
}
