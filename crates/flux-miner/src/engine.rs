//! # flux-miner::engine — the shared mining ORCHESTRATION
//!
//! The dual-lane engine's runtime: a [`MinerStats`] snapshot, the CPU + GPU
//! workers (each: fetch challenge -> dual-lane solve -> submit -> record), and
//! the [`supervisor`] that owns the worker lifecycle and hot-switches CPU<->GPU.
//!
//! This is lifted verbatim from the standalone `sigil-miner` binary so that BOTH
//! the standalone miner AND sigil-top's in-node Mining tab run byte-identical
//! mining code — no second copy to drift apart. Needs the HTTP `client`.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "gpu")]
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use flux_vdf::ModSquaring;

use crate::client::{build_header, solve, Endpoints, MinerClient, Submission};

/// This build's version (the flux-miner crate version) — stamped into diagnostics.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Shared mining state, polled by a renderer (the standalone TUI, or sigil-top's
/// Mining tab). Identical to the standalone miner's `Stats`.
#[derive(Default, Clone)]
pub struct MinerStats {
    pub connected: bool,
    pub last_err: Option<String>,
    pub shares_ok: u64,
    pub shares_bad: u64,
    pub last_height: u64,
    pub last_solve_ms: f64,
    pub hashrate: f64, // Φ — BLAKE4 hashes/sec (Lane A)
    pub vdf_rate: f64, // Ω — VDF turns/sec (Lane B)
    pub vdf_t: u64,
    pub balance: u128,
    pub solve_hist: VecDeque<u64>,  // recent solve ms (sparkline)
    pub log: VecDeque<String>,      // recent share lines (newest first)
    pub update_msg: Option<String>, // auto-updater status line
    pub mode: String,               // live mining mode ("CPU" / "GPU")
    // v7.0.8 network metrics — estimated from the challenge difficulty + observed mine-tip rate.
    pub net_hps: f64,      // TOTAL network hashrate ≈ 2^bits / block_interval (all miners combined)
    pub net_bits: u32,     // current mine difficulty (bits)
    pub net_block_ms: f64, // observed avg mine-block interval (ms)
}

/// Estimate TOTAL network power from the difficulty + observed block cadence:
/// `network_hashrate ≈ difficulty / block_interval = 2^bits / T`. `tracker` = (last height
/// seen, when it last advanced); persists across loop iterations. Called every challenge fetch
/// so the tip cadence — hence the whole network's combined hashrate — is tracked live.
pub fn update_net_power(s: &mut MinerStats, height: u64, bits: u32, tracker: &mut (u64, std::time::Instant)) {
    s.net_bits = bits;
    if height > tracker.0 && tracker.0 > 0 {
        let dt = tracker.1.elapsed().as_secs_f64().max(0.001);
        let per_block_ms = (dt / (height - tracker.0) as f64) * 1000.0;
        s.net_block_ms = if s.net_block_ms > 0.0 { s.net_block_ms * 0.7 + per_block_ms * 0.3 } else { per_block_ms };
    }
    if height > tracker.0 { *tracker = (height, std::time::Instant::now()); }
    // NOTE: net_hps (total power) is NOT computed here — the difficulty estimate
    // (2^bits/block_interval) undercounts with pinned difficulty + throttled submits. The caller
    // sets s.net_hps from Challenge.net_hps, which the node measures by SUMMING active miners' rates.
}

pub fn push_log(log: &mut VecDeque<String>, line: String) {
    log.push_front(line);
    while log.len() > 200 {
        log.pop_back();
    }
}

/// Classical hashrate ladder: H/s · kH/s · MH/s · GH/s · TH/s · PH/s · EH/s.
pub fn format_hps(hps: f64) -> String {
    const U: [&str; 7] = ["H/s", "kH/s", "MH/s", "GH/s", "TH/s", "PH/s", "EH/s"];
    let mut v = hps;
    let mut i = 0;
    while v >= 1000.0 && i < U.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    format!("{v:.2} {}", U[i])
}

/// Best-effort: POST a diagnostic to the node so it can be read server-side.
pub fn report_diag(url: &str, msg: &str) {
    let body = format!("[sigil-miner v{VERSION}] {}", msg.replace('\n', " | "));
    let _ = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .and_then(|c| c.post(format!("{url}/api/v1/diag")).body(body).send());
}

/// GET {url}/api/v1/balance?wallet=… → the NATIVE balance (flat-JSON pluck).
pub fn fetch_balance(url: &str, wallet: &str) -> Option<u128> {
    let u = format!("{url}/api/v1/balance?wallet={wallet}");
    let txt = reqwest::blocking::get(&u).ok()?.text().ok()?;
    let tail = txt.split("\"balance\":").nth(1)?;
    let digits: String = tail.trim_start().chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Mirror live mining state to a status file so an EXTERNAL reader (a separately
/// launched sigil-top) can show the miner's numbers. Throttled to 1/s.
pub fn write_miner_status(s: &MinerStats, wallet: &str) {
    use std::sync::atomic::AtomicU64;
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if now <= LAST.load(Ordering::Relaxed) {
        return;
    }
    LAST.store(now, Ordering::Relaxed);
    let j = format!(
        r#"{{"ts":{},"connected":{},"hashrate":{:.3},"vdf_rate":{:.3},"shares_ok":{},"shares_bad":{},"balance":{},"last_height":{},"mode":"{}","wallet":"{}"}}"#,
        now, s.connected, s.hashrate, s.vdf_rate, s.shares_ok, s.shares_bad, s.balance, s.last_height, s.mode, wallet
    );
    let _ = std::fs::write(std::env::temp_dir().join("sigil-miner-status.json"), j);
}

/// The CPU mining engine: fetch challenge → dual-lane solve → submit → record.
pub fn mining_loop(url: String, wallet: String, stats: Arc<Mutex<MinerStats>>, stop: Arc<AtomicBool>) {
    let g = ModSquaring::bench_2048(); // must match the node's group
    let client = match MinerClient::new(Endpoints::standard(&url), wallet.clone()) {
        Ok(c) => c,
        Err(e) => {
            stats.lock().unwrap().last_err = Some(format!("client init: {e}"));
            return;
        }
    };
    // MINER DISCIPLINE (v7.0.5): exactly ONE share per height. Only one submission per
    // mine-tip can ever be accepted (each accept IS the next block), so submitting more
    // than one share for the same height just floods "stale height" rejects — which is
    // exactly what trivial difficulty produces, since a solve finishes far faster than
    // the ~3s tip cadence. Track the last height we've already spent and hold (cheap
    // re-poll) until the tip actually advances, so the miner emits one clean accepted
    // share per block instead of a storm of doomed submits.
    let mut last_spent_height: u64 = u64::MAX;
    let mut net_tracker = (0u64, Instant::now()); // (last mine-tip seen, when it advanced) → block cadence
    let mut prev_hps: f64 = 0.0; // our last measured Φ rate — reported so the node SUMS total power
    // HASHRATE WINDOW (2026-07-24): pool slices end on every share — a few ms at
    // 16-bit shares — so per-slice rates are pure jitter (the "CPU hashrate keeps
    // fluctuating" symptom). Accumulate work across slices and publish the
    // EFFECTIVE rate (incl. VDF + submit round-trips) over ≥2 s windows; that is
    // also what the node sums into net_hps, so total network power stops being
    // inflated by burst-only slice rates.
    let mut win_hashes: f64 = 0.0;
    let mut win_t0 = Instant::now();
    // POOL-SHARES: resumable nonce cursor for the current height. In pool mode one
    // height yields MANY shares, so the search must continue where it stopped —
    // restarting at the same base would re-find already-credited nonces (the node
    // dedups (wallet, nonce)). Seeded per height off the pid so a miner restart
    // mid-height lands in fresh nonce space.
    let mut pool_h: u64 = u64::MAX;
    let mut pool_base: u64 = 0;
    // POOL-SHARES: height at which the node said our share cap is reached — for
    // the REST of that height we hunt the BLOCK target instead (full hashrate,
    // no doomed share submits). Reset implicitly when the height moves on.
    let mut pool_capped_h: u64 = u64::MAX;
    /// Nonces per pool search slice — keeps each iteration ~a second on a typical
    /// CPU so tip changes are noticed promptly (the challenge refetch between
    /// slices is the tip probe).
    const POOL_CPU_BUDGET: u64 = 4_000_000;
    while !stop.load(Ordering::Relaxed) {
        let c = match client.fetch_challenge(prev_hps) {
            Ok(c) => c,
            Err(e) => {
                {
                    let mut s = stats.lock().unwrap();
                    s.connected = false;
                    s.last_err = Some(format!("challenge: {e}"));
                }
                thread::sleep(Duration::from_secs(2));
                continue;
            }
        };
        // v7.0.9: difficulty/block-cadence for the card, + TOTAL network power from the node (summed
        // over all active miners' reported rates — accurate even with pinned difficulty + throttled submits).
        { let mut s = stats.lock().unwrap(); update_net_power(&mut s, c.height, c.blake4_target.leading_zeros(), &mut net_tracker); s.net_hps = c.net_hps; }
        // POOL-SHARES: a 7.1+ node advertises a sub-difficulty share target
        // (numerically LARGER = easier than the block target). Then we submit
        // every share-grade solve and keep mining the SAME height — payout is
        // proportional at the block, so there is no one-submit-per-height hold.
        let pool = c.share_target > c.blake4_target;
        // Already submitted for this exact tip → the tip hasn't moved yet. Don't burn a
        // doomed stale share; wait briefly and re-check the tip. (Solo mode only.)
        if !pool && c.height == last_spent_height {
            thread::sleep(Duration::from_millis(250));
            continue;
        }
        let t0 = Instant::now();
        let (block, _hashes, dt) = if pool {
            if pool_h != c.height {
                pool_h = c.height;
                pool_base = ((std::process::id() as u64) << 32)
                    ^ c.height.wrapping_mul(0x9E37_79B9_7F4A_7C15);
            }
            let header = build_header(&c, &wallet);
            // Capped this height → hunt the full block target (shares are done).
            let pool_target = if pool_capped_h == c.height { c.blake4_target } else { c.share_target };
            let (found, next) =
                crate::mine_dual_from(&header, pool_target, c.vdf_t, &g, pool_base, POOL_CPU_BUDGET);
            let tried = next.wrapping_sub(pool_base).max(1);
            pool_base = next;
            let dt = t0.elapsed().as_secs_f64().max(1e-9);
            win_hashes += tried as f64;
            let wdt = win_t0.elapsed().as_secs_f64();
            if wdt >= 2.0 {
                prev_hps = win_hashes / wdt;
                win_hashes = 0.0;
                win_t0 = Instant::now();
            } else if prev_hps <= 0.0 {
                prev_hps = tried as f64 / dt; // seed the first window with the slice rate
            }
            match found {
                Some(b) => (b, tried as f64, dt),
                None => {
                    // Budget slice exhausted with no share — publish the rate and
                    // loop straight back to the challenge fetch (the tip probe).
                    let mut s = stats.lock().unwrap();
                    s.connected = true;
                    s.hashrate = prev_hps;
                    s.last_height = c.height;
                    continue;
                }
            }
        } else {
            let b = solve(&c, &wallet, &g); // Lane A nonce search + Lane B VDF
            let h = b.nonce as f64 + 1.0; // nonces tried ≈ BLAKE4 work
            let dt = t0.elapsed().as_secs_f64().max(1e-9);
            win_hashes += h;
            let wdt = win_t0.elapsed().as_secs_f64();
            if wdt >= 2.0 {
                prev_hps = win_hashes / wdt;
                win_hashes = 0.0;
                win_t0 = Instant::now();
            } else if prev_hps <= 0.0 {
                prev_hps = h / dt;
            }
            (b, h, dt)
        };
        let sub = Submission { height: c.height, wallet: wallet.clone(), block };
        let res = client.submit(&sub);
        {
            let mut s = stats.lock().unwrap();
            s.connected = true;
            s.last_err = None;
            s.vdf_t = c.vdf_t;
            s.last_height = c.height;
            s.last_solve_ms = dt * 1000.0;
            s.hashrate = prev_hps; // windowed effective rate, not the per-slice burst
            s.vdf_rate = c.vdf_t as f64 / dt;
            s.solve_hist.push_back((dt * 1000.0) as u64);
            while s.solve_hist.len() > 80 {
                s.solve_hist.pop_front();
            }
            match res {
                Ok(r) if r.accepted && r.share => {
                    // POOL-SHARES: a sub-difficulty share is banked for this height's
                    // payout — keep mining the SAME height (no spent-hold). Balance
                    // only moves at the block payout, so poll it sparsely.
                    s.shares_ok += 1;
                    let poll_balance = s.shares_ok % 8 == 0;
                    push_log(&mut s.log, format!("✓ h={:<8} {:>6.0}ms  SHARE banked", c.height, dt * 1000.0));
                    drop(s);
                    if poll_balance {
                        if let Some(b) = fetch_balance(&url, &wallet) {
                            stats.lock().unwrap().balance = b;
                        }
                    }
                    write_miner_status(&stats.lock().unwrap(), &wallet);
                    continue;
                }
                Ok(r) if r.accepted => {
                    s.shares_ok += 1;
                    last_spent_height = c.height; // don't resubmit — the tip advances now
                    push_log(&mut s.log, format!("✓ h={:<8} {:>6.0}ms  {}", c.height, dt * 1000.0,
                        if pool { "BLOCK — payout split over shares" } else { "ACCEPTED" }));
                }
                Ok(r) => {
                    s.shares_bad += 1;
                    let reason = r.reason.unwrap_or_default();
                    // A stale/tip-moved reject means this height is gone — mark it spent so
                    // we advance to the real tip instead of re-solving a dead height.
                    if reason.contains("stale") || reason.contains("mineable tip") {
                        last_spent_height = c.height;
                    }
                    // POOL-SHARES: cap reached → block-hunt for the rest of this
                    // height instead of spamming doomed share submits.
                    if reason.contains("share cap") || reason.contains("share window full") {
                        pool_capped_h = c.height;
                    }
                    push_log(&mut s.log, format!("✗ h={:<8} rejected: {}", c.height, reason));
                }
                Err(e) => {
                    s.shares_bad += 1;
                    s.connected = false;
                    s.last_err = Some(format!("submit: {e}"));
                    push_log(&mut s.log, format!("! h={:<8} submit error: {e}", c.height));
                }
            }
        }
        if let Some(b) = fetch_balance(&url, &wallet) {
            stats.lock().unwrap().balance = b;
        }
        write_miner_status(&stats.lock().unwrap(), &wallet);
    }
}

/// Mining supervisor: owns the worker thread's lifecycle so the engine can be
/// hot-switched at runtime (`desired_gpu` flips). When the desired mode changes
/// it signals the current worker to stop and starts the other, and writes the
/// live mode into Stats so the badge reflects reality.
pub fn supervisor(
    url: String,
    wallet: String,
    stats: Arc<Mutex<MinerStats>>,
    stop: Arc<AtomicBool>,
    desired_gpu: Arc<AtomicBool>,
    gpu_failed: Arc<AtomicBool>,
) {
    let mut cur: Option<bool> = None;
    let mut wstop = Arc::new(AtomicBool::new(false));
    // LANE-C: thermal-driven GPU throttle channel — extra inter-dispatch sleep, in
    // microseconds, that the thermal watcher raises (and the GPU worker honours) to
    // shed heat in the THROTTLE band before a hard DISABLE. 0 = full speed. Owned by
    // the supervisor so it survives CPU<->GPU hot-switches. (gpu builds only — the
    // CPU worker has no dispatch loop to throttle.)
    #[cfg(feature = "gpu")]
    let thermal_extra_us = Arc::new(AtomicU64::new(0));
    // LANE-C v0.50 (tuned v0.90): thermal guard — while GPU mining, poll nvidia-smi
    // every 10 s. On a DEGRADED LAPTOP (idles ~72C, hard-powers-off under sustained
    // GPU load — Windows Kernel-Power 41) it THROTTLES at ≥85C (default) (raises the
    // inter-dispatch sleep) and hard-falls-back to CPU at ≥90C, BEFORE the hardware
    // browns out. No auto-rearm: once it disables GPU the operator must re-press [G].
    // Spawned + fail-silent: no nvidia-smi → no guard, CPU path untouched. GPU builds
    // only (CPU-only builds never go GPU-active).
    #[cfg(feature = "gpu")]
    {
        let (st, gf, sp, es) = (stats.clone(), gpu_failed.clone(), stop.clone(), thermal_extra_us.clone());
        thread::spawn(move || thermal_watch(st, gf, sp, es));
    }
    loop {
        if stop.load(Ordering::Relaxed) {
            wstop.store(true, Ordering::Relaxed);
            return;
        }
        // GPU worker reported an init failure → fall back to CPU (it logged why).
        if gpu_failed.swap(false, Ordering::Relaxed) {
            desired_gpu.store(false, Ordering::Relaxed);
            push_log(&mut stats.lock().unwrap().log, "↩ GPU unavailable — switched to CPU".into());
        }
        let mut want = desired_gpu.load(Ordering::Relaxed);
        if want && !cfg!(feature = "gpu") {
            // CPU-only build: can't switch to GPU — revert + tell the operator.
            want = false;
            desired_gpu.store(false, Ordering::Relaxed);
            push_log(
                &mut stats.lock().unwrap().log,
                "⚠ GPU not in this build — rebuild with --features gpu".into(),
            );
        }
        if cur != Some(want) {
            wstop.store(true, Ordering::Relaxed); // stop the previous worker
            wstop = Arc::new(AtomicBool::new(false));
            cur = Some(want);
            {
                let m = if want { "GPU" } else { "CPU" };
                let mut s = stats.lock().unwrap();
                s.mode = m.into();
                push_log(&mut s.log, format!("⚙ mining engine → {m}"));
            }
            let (u, w, st, ws) = (url.clone(), wallet.clone(), stats.clone(), wstop.clone());
            if want {
                #[cfg(feature = "gpu")]
                {
                    let gf = gpu_failed.clone();
                    let es = thermal_extra_us.clone();
                    thread::spawn(move || gpu_mining_loop(u, w, st, ws, gf, es));
                }
            } else {
                thread::spawn(move || mining_loop(u, w, st, ws));
            }
        }
        thread::sleep(Duration::from_millis(200));
    }
}

/// `--gpu`: hybrid mining — GPU searches Lane A (BLAKE4), CPU does Lane B (VDF).
/// Uses FULL_ROUNDS so shares pass the node's `verify_dual` (legacy blake4 == R7).
#[cfg(feature = "gpu")]
pub fn gpu_mining_loop(
    url: String,
    wallet: String,
    stats: Arc<Mutex<MinerStats>>,
    stop: Arc<AtomicBool>,
    gpu_failed: Arc<AtomicBool>,
    // LANE-C: thermal-throttle channel — extra inter-dispatch sleep in microseconds,
    // raised by `thermal_watch` while in the THROTTLE band (≥85C default). 0 = full speed.
    thermal_extra_us: Arc<AtomicU64>,
) {
    use crate::client::build_header;
    // v0.37 STABILITY: this is usually the PRIMARY (display) GPU. A monopolizing
    // 1M-item dispatch loop with no yield starved the Windows desktop and tripped
    // WDDM TDR (driver reset -> near-BSOD). Keep each dispatch SHORT and sleep a
    // few ms between them so the driver can service the display. A dedicated rig
    // can raise the batch / disable the sleep via env.
    let batch: usize = std::env::var("SIGIL_GPU_BATCH").ok().and_then(|v| v.parse().ok())
        .filter(|&b| b >= 4096).unwrap_or(1 << 24); // v7.0.8: 16M default — FULL GPU utilization
        // (~30ms/dispatch, well under the ~2s WDDM TDR limit). Was 256K, which starved the card
        // to ~40 MH/s. Lower SIGIL_GPU_BATCH only if a single display-GPU feels sluggish.
    let throttle = std::time::Duration::from_millis(
        std::env::var("SIGIL_GPU_THROTTLE_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(0), // v7.0.8: 0 (was 5ms — the sleep halved throughput; thermal_watch still throttles when hot)
    );

    let gpu = match crate::gpu::GpuBlake4::new() {
        Ok(g) => g,
        Err(e) => {
            // Surface the full error (incl. the OpenCL build log) + signal the
            // supervisor to fall back to CPU so the miner never silently stalls.
            let msg = format!("GPU init failed: {e}");
            {
                let mut s = stats.lock().unwrap();
                s.last_err = Some(msg.clone());
                push_log(&mut s.log, format!("✗ {msg}"));
            }
            let _ = std::fs::write("sigil-miner-gpu.log", &msg);
            report_diag(&url, &msg);
            gpu_failed.store(true, Ordering::Relaxed);
            return;
        }
    };
    {
        let mut s = stats.lock().unwrap();
        push_log(&mut s.log, format!("GPU: {}", gpu.device_name));
    }
    let g = ModSquaring::bench_2048();
    let rounds = crate::pow::FULL_ROUNDS; // MUST match the node's verify_dual
    let client = match MinerClient::new(Endpoints::standard(&url), wallet.clone()) {
        Ok(c) => c,
        Err(e) => {
            stats.lock().unwrap().last_err = Some(format!("client init: {e}"));
            return;
        }
    };

    // MINER DISCIPLINE (v7.0.5): one share per height — see mining_loop. Same guard on
    // the GPU path so it doesn't flood stale submits when the tip hasn't advanced.
    let mut last_spent_height: u64 = u64::MAX;
    let mut net_tracker = (0u64, Instant::now()); // (last mine-tip seen, when it advanced) → block cadence
    let mut prev_hps: f64 = 0.0; // our last measured GPU Φ rate — reported so the node SUMS total power
    // HASHRATE WINDOW (2026-07-24): see mining_loop. On a pool a 16-bit share ends
    // the slice after ONE dispatch, so the old per-slice number was the kernel
    // burst rate (GH/s) while the node-visible effective rate — after thermal
    // sleeps and submit/challenge round-trips — was 10-50× lower. Publish the
    // ≥2 s windowed effective rate instead, updated inside the dispatch loop so
    // long solo searches still tick live.
    let mut win_hashes: f64 = 0.0;
    let mut win_t0 = Instant::now();
    // POOL-SHARES: resumable per-height nonce cursor (see mining_loop) — the GPU
    // search continues from here across shares so credited nonces aren't re-found.
    let mut pool_h: u64 = u64::MAX;
    let mut pool_base: u64 = 0;
    // POOL-SHARES: cap reached this height → block-hunt (see mining_loop).
    let mut pool_capped_h: u64 = u64::MAX;
    while !stop.load(Ordering::Relaxed) {
        let c = match client.fetch_challenge(prev_hps) {
            Ok(c) => c,
            Err(e) => {
                {
                    let mut s = stats.lock().unwrap();
                    s.connected = false;
                    s.last_err = Some(format!("challenge: {e}"));
                }
                thread::sleep(Duration::from_secs(2));
                continue;
            }
        };
        // v7.0.9: difficulty/block-cadence for the card, + TOTAL network power from the node (summed).
        { let mut s = stats.lock().unwrap(); update_net_power(&mut s, c.height, c.blake4_target.leading_zeros(), &mut net_tracker); s.net_hps = c.net_hps; }
        // POOL-SHARES: share-grade target from a 7.1+ node → submit every share,
        // keep mining the same height (no one-submit-per-height hold).
        let pool = c.share_target > c.blake4_target;
        let search_target = if pool && pool_capped_h != c.height { c.share_target } else { c.blake4_target };
        if !pool && c.height == last_spent_height {
            thread::sleep(Duration::from_millis(250));
            continue;
        }
        let header = build_header(&c, &wallet);
        let t0 = Instant::now();
        let mut nonce_base = if pool {
            if pool_h != c.height {
                pool_h = c.height;
                pool_base = ((std::process::id() as u64) << 32)
                    ^ c.height.wrapping_mul(0x9E37_79B9_7F4A_7C15);
            }
            pool_base
        } else {
            0u64
        };
        let search_base = nonce_base;
        let mut found = None;
        while found.is_none() && !stop.load(Ordering::Relaxed) {
            match gpu.search(&header, search_target, rounds, nonce_base, batch) {
                Ok(r) => {
                    found = r;
                    nonce_base = nonce_base.wrapping_add(batch as u64);
                    win_hashes += batch as f64;
                    let wdt = win_t0.elapsed().as_secs_f64();
                    if wdt >= 2.0 {
                        prev_hps = win_hashes / wdt;
                        win_hashes = 0.0;
                        win_t0 = Instant::now();
                        stats.lock().unwrap().hashrate = prev_hps;
                    }
                    // yield the GPU so it can still drive the display -> no freeze / TDR
                    if found.is_none() && !throttle.is_zero() {
                        thread::sleep(throttle);
                    }
                    // LANE-C thermal throttle: while in the throttle band the watcher
                    // raises this; sleeping between dispatches lowers the GPU duty
                    // cycle (and so the temperature) WITHOUT dropping to CPU. Applied
                    // every dispatch (incl. on a hit) so heat is shed promptly.
                    let extra = thermal_extra_us.load(Ordering::Relaxed);
                    if extra > 0 {
                        thread::sleep(Duration::from_micros(extra));
                    }
                }
                Err(e) => {
                    // a search failure (not just init) → log it + fall back to CPU
                    // instead of silently stalling.
                    let msg = format!("GPU search failed: {e}");
                    {
                        let mut s = stats.lock().unwrap();
                        s.last_err = Some(msg.clone());
                        push_log(&mut s.log, format!("✗ {msg} — falling back to CPU"));
                    }
                    let _ = std::fs::write("sigil-miner-gpu.log", &msg);
                    report_diag(&url, &msg);
                    gpu_failed.store(true, Ordering::Relaxed);
                    return;
                }
            }
        }
        let nonce = match found {
            Some(n) => n,
            None => continue,
        };
        if pool {
            // Resume AFTER the found nonce: the tail of its batch gets re-scanned
            // next slice (those nonces were never submitted → no dup rejects).
            pool_base = nonce.wrapping_add(1);
        }
        let dt = t0.elapsed().as_secs_f64().max(1e-9);
        let block = crate::block_for_nonce(&header, nonce, &g, c.vdf_t); // Lane B on CPU
        let sub = Submission { height: c.height, wallet: wallet.clone(), block };
        let res = client.submit(&sub);
        {
            let mut s = stats.lock().unwrap();
            s.connected = true;
            s.last_err = None;
            s.vdf_t = c.vdf_t;
            s.last_height = c.height;
            s.last_solve_ms = dt * 1000.0;
            // windowed effective GPU rate (falls back to the slice rate until the
            // first 2 s window closes) — reported on the next challenge fetch so
            // the node's total-power sum reflects work actually delivered.
            if prev_hps <= 0.0 {
                prev_hps = nonce_base.wrapping_sub(search_base) as f64 / dt;
            }
            s.hashrate = prev_hps;
            s.vdf_rate = c.vdf_t as f64 / dt;
            s.solve_hist.push_back((dt * 1000.0) as u64);
            while s.solve_hist.len() > 80 {
                s.solve_hist.pop_front();
            }
            match res {
                Ok(r) if r.accepted && r.share => {
                    // POOL-SHARES: banked for this height's payout — keep the GPU on
                    // the SAME height, resuming from the advanced nonce cursor.
                    // Balance only moves at the block payout → poll sparsely.
                    s.shares_ok += 1;
                    let poll_balance = s.shares_ok % 8 == 0;
                    push_log(&mut s.log, format!("✓ h={:<8} {:>6.0}ms  GPU SHARE banked", c.height, dt * 1000.0));
                    drop(s);
                    if poll_balance {
                        if let Some(b) = fetch_balance(&url, &wallet) {
                            stats.lock().unwrap().balance = b;
                        }
                    }
                    write_miner_status(&stats.lock().unwrap(), &wallet);
                    continue;
                }
                Ok(r) if r.accepted => {
                    s.shares_ok += 1;
                    last_spent_height = c.height;
                    push_log(&mut s.log, format!("✓ h={:<8} {:>6.0}ms  {}", c.height, dt * 1000.0,
                        if pool { "GPU BLOCK — payout split over shares" } else { "GPU ACCEPTED" }));
                }
                Ok(r) => {
                    s.shares_bad += 1;
                    let reason = r.reason.unwrap_or_default();
                    if reason.contains("stale") || reason.contains("mineable tip") {
                        last_spent_height = c.height;
                    }
                    // POOL-SHARES: cap reached → block-hunt for the rest of this height.
                    if reason.contains("share cap") || reason.contains("share window full") {
                        pool_capped_h = c.height;
                    }
                    push_log(&mut s.log, format!("✗ h={:<8} rejected: {}", c.height, reason));
                }
                Err(e) => {
                    s.shares_bad += 1;
                    s.connected = false;
                    push_log(&mut s.log, format!("! submit error: {e}"));
                }
            }
        }
        if let Some(b) = fetch_balance(&url, &wallet) {
            stats.lock().unwrap().balance = b;
        }
        write_miner_status(&stats.lock().unwrap(), &wallet);
    }
}

// ── LANE-C v0.50: GPU thermal guard ───────────────────────────────────────────
// A laptop operator hard-powered-off twice mining on GPU (Windows Kernel-Power 41 —
// the machine browned out under sustained GPU heat). This guard watches the GPU
// temperature and forces a CPU fallback before the hardware does it the hard way.

/// What the thermal policy decides for one temperature sample.
#[cfg(any(feature = "gpu", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThermalAction {
    /// No change this sample.
    None,
    /// Warm (in the [throttle, disable) band) — keep mining on GPU but slow the
    /// dispatch loop by `sleep_us` microseconds per batch to shed heat. Hysteretic:
    /// stays engaged until the temperature drops back below `unthrottle_c`.
    Throttle { sleep_us: u64 },
    /// Too hot (>= disable) — drop GPU mining to CPU now. NO auto-rearm: the operator
    /// must re-enable GPU explicitly ([G]).
    FallbackToCpu,
}

/// Deterministic thermal policy with TWO bands tuned for a degraded laptop. The
/// decision is a pure function of the temperature stream + a single hysteresis latch,
/// so it is trivially unit-testable with a mock temp source — no clock or real GPU.
///
/// Bands (defaults): below `throttle_c` (85C) full speed; in `[throttle_c, disable_c)`
/// (85–90C) THROTTLE (slow the dispatch loop to shed heat, keep mining on GPU); at or
/// above `disable_c` (90C) hard-FALL-BACK to CPU. The throttle band is hysteretic:
/// once engaged it stays engaged until the temperature drops back to `unthrottle_c`
/// (80C). There is NO auto-rearm after a disable — re-enabling GPU is an explicit
/// operator action ([G]) — so on a machine that hard-powers-off under load the guard
/// errs toward staying on the safe (CPU) path.
#[cfg(any(feature = "gpu", test))]
#[derive(Debug, Clone)]
pub struct ThermalGuard {
    throttle_c: f64,        // >= this (and < disable) → throttle GPU (default 85C)
    disable_c: f64,         // >= this → CPU fallback (default 90C)
    unthrottle_c: f64,      // <= this while throttling → stop throttling (default 74C)
    throttle_sleep_ms: u64, // extra per-dispatch sleep while throttling (default 50ms)
    throttling: bool,       // hysteresis latch for the throttle band
}

#[cfg(any(feature = "gpu", test))]
impl ThermalGuard {
    /// Desktop-sane policy (2026-07-24): throttle 85C, disable 90C, un-throttle 80C,
    /// +50ms per-dispatch sleep while throttling. The previous 78/82/74 defaults were
    /// tuned for one degraded laptop and put every healthy desktop card (which runs
    /// 75-83C mining flat-out) permanently in the throttle band — the "3 GH/s falls
    /// to 150-300 MH/s" collapse. NVIDIA silicon self-throttles at ~91-95C, so 85/90
    /// still backs off well before the hardware does. A fragile laptop LOWERS these
    /// via env (SIGIL_GPU_TEMP_THROTTLE=78 SIGIL_GPU_TEMP_DISABLE=82 ...).
    pub fn new() -> Self {
        fn envf(k: &str, d: f64) -> f64 { std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d) }
        fn envu(k: &str, d: u64) -> u64 { std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d) }
        // v4.1: hardware-configurable thermal limits via env.
        // NOTE: the thermal sleep is SIGIL_GPU_THERMAL_SLEEP_MS — it used to read
        // SIGIL_GPU_THROTTLE_MS, which COLLIDED with the static per-dispatch sleep
        // in gpu_mining_loop (same var, default 0 there / 50 here), so softening the
        // thermal throttle silently added a constant sleep to every cool dispatch.
        Self::with(
            envf("SIGIL_GPU_TEMP_THROTTLE", 85.0),
            envf("SIGIL_GPU_TEMP_DISABLE", 90.0),
            envf("SIGIL_GPU_TEMP_UNTHROTTLE", 80.0),
            envu("SIGIL_GPU_THERMAL_SLEEP_MS", 50),
        )
    }

    /// Construct with explicit thresholds (used by tests).
    pub fn with(throttle_c: f64, disable_c: f64, unthrottle_c: f64, throttle_sleep_ms: u64) -> Self {
        ThermalGuard { throttle_c, disable_c, unthrottle_c, throttle_sleep_ms, throttling: false }
    }

    /// Whether the guard is currently throttling the GPU (in the warm band).
    pub fn is_throttling(&self) -> bool { self.throttling }

    /// Feed one temperature sample. `gpu_active` = is the engine GPU-mining right now.
    pub fn step(&mut self, temp_c: f64, gpu_active: bool) -> ThermalAction {
        // Nothing to police when not GPU-mining (CPU mode / CPU-only build): clear the
        // latch so a later GPU re-enable starts fresh.
        if !gpu_active {
            self.throttling = false;
            return ThermalAction::None;
        }
        // Hard ceiling first — drop to CPU immediately and clear the throttle latch.
        if temp_c >= self.disable_c {
            self.throttling = false;
            return ThermalAction::FallbackToCpu;
        }
        if self.throttling {
            // Stay throttled until we cool past the lower (hysteresis) threshold.
            if temp_c <= self.unthrottle_c {
                self.throttling = false;
                ThermalAction::None
            } else {
                ThermalAction::Throttle { sleep_us: self.throttle_sleep_ms * 1000 }
            }
        } else if temp_c >= self.throttle_c {
            self.throttling = true;
            ThermalAction::Throttle { sleep_us: self.throttle_sleep_ms * 1000 }
        } else {
            ThermalAction::None
        }
    }
}

#[cfg(any(feature = "gpu", test))]
impl Default for ThermalGuard {
    fn default() -> Self { Self::new() }
}

/// Read GPU temperature (Celsius) via nvidia-smi. Returns None when nvidia-smi is
/// absent or errors — the guard then simply does nothing (CPU path untouched).
/// NOTE: this protects NVIDIA GPUs only. On an AMD/Intel laptop nvidia-smi is absent
/// → no temperature → no guard; the one-time [G] warning says so, and such machines
/// must rely on their own firmware thermal throttling (or simply not GPU-mine).
#[cfg(feature = "gpu")]
fn read_gpu_temp() -> Option<f64> {
    let out = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=temperature.gpu", "--format=csv,noheader,nounits"])
        .output()
        .ok()?;
    if !out.status.success() { return None; }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()?
        .trim()
        .parse::<f64>()
        .ok()
}

/// Background thermal watcher (GPU builds only). Polls every 10 s while GPU mining:
/// in the 78–82C band it raises `thermal_extra_us` so the GPU worker slows its
/// dispatch loop (sheds heat without leaving the GPU); at >=82C it raises `gpu_failed`
/// (the supervisor consumes it → CPU, and clears `desired_gpu` so it stays there) and
/// logs the temp. NO auto-rearm — the operator presses [G] to retry. Fail-silent when
/// nvidia-smi is unavailable.
#[cfg(feature = "gpu")]
pub fn thermal_watch(
    stats: Arc<Mutex<MinerStats>>,
    gpu_failed: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thermal_extra_us: Arc<AtomicU64>,
) {
    let mut guard = ThermalGuard::new();
    loop {
        // sleep ~10 s in 100 ms steps so `stop` stays responsive.
        for _ in 0..100 {
            if stop.load(Ordering::Relaxed) { return; }
            thread::sleep(Duration::from_millis(100));
        }
        let temp = match read_gpu_temp() { Some(t) => t, None => continue };
        let gpu_active = stats.lock().map(|s| s.mode == "GPU").unwrap_or(false);
        match guard.step(temp, gpu_active) {
            ThermalAction::Throttle { sleep_us } => {
                // Log only on the rising edge (entering the throttle band).
                let prev = thermal_extra_us.swap(sleep_us, Ordering::Relaxed);
                if prev == 0 {
                    if let Ok(mut s) = stats.lock() {
                        push_log(&mut s.log, format!("GPU {temp:.0}C — throttling to shed heat"));
                    }
                }
            }
            ThermalAction::FallbackToCpu => {
                thermal_extra_us.store(0, Ordering::Relaxed);
                gpu_failed.store(true, Ordering::Relaxed);
                if let Ok(mut s) = stats.lock() {
                    push_log(&mut s.log, format!("GPU {temp:.0}C — thermal fallback to CPU (press [G] to retry)"));
                }
            }
            ThermalAction::None => {
                // Falling edge: back to full speed if we had been throttling.
                let prev = thermal_extra_us.swap(0, Ordering::Relaxed);
                if prev != 0 {
                    if let Ok(mut s) = stats.lock() {
                        push_log(&mut s.log, format!("GPU {temp:.0}C — cooled, full speed"));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod thermal_tests {
    use super::{ThermalAction, ThermalGuard};

    // laptop-tuned guard for tests (same as ::new(), explicit for clarity).
    fn guard() -> ThermalGuard { ThermalGuard::with(78.0, 82.0, 74.0, 50) }
    fn sleep_us(a: ThermalAction) -> Option<u64> {
        if let ThermalAction::Throttle { sleep_us } = a { Some(sleep_us) } else { None }
    }

    #[test]
    fn hot_sample_falls_back_to_cpu() {
        let mut g = guard();
        assert_eq!(g.step(72.0, true), ThermalAction::None);          // idle band → nothing
        assert_eq!(g.step(83.0, true), ThermalAction::FallbackToCpu); // >=82 + GPU → fall back
        // still hot → keeps reporting fallback (idempotent; supervisor already on CPU)
        assert_eq!(g.step(90.0, true), ThermalAction::FallbackToCpu);
    }

    #[test]
    fn throttle_band_engages_and_releases_with_hysteresis() {
        let mut g = guard();
        assert_eq!(g.step(77.0, true), ThermalAction::None);        // below throttle
        assert_eq!(sleep_us(g.step(79.0, true)), Some(50_000));     // >=78 → throttle
        assert_eq!(sleep_us(g.step(81.0, true)), Some(50_000));     // still warm → still throttling
        assert_eq!(sleep_us(g.step(75.0, true)), Some(50_000));     // 75 > 74 → hysteresis holds
        assert_eq!(g.step(74.0, true), ThermalAction::None);        // <=74 → release
        assert_eq!(g.step(76.0, true), ThermalAction::None);        // 76 < 78 → stays off
        assert!(!g.is_throttling());
    }

    #[test]
    fn disable_beats_throttle_and_clears_latch() {
        let mut g = guard();
        assert!(sleep_us(g.step(79.0, true)).is_some()); // throttling
        assert_eq!(g.step(82.0, true), ThermalAction::FallbackToCpu); // disable wins, clears latch
        assert!(!g.is_throttling());
    }

    #[test]
    fn no_action_when_gpu_inactive() {
        let mut g = guard();
        // hot but NOT GPU-mining (CPU mode / CPU-only build) → guard must not act.
        assert_eq!(g.step(95.0, false), ThermalAction::None);
        assert!(!g.is_throttling());
        // an inactive sample also clears any standing throttle latch.
        assert!(sleep_us(g.step(79.0, true)).is_some());
        assert_eq!(g.step(79.0, false), ThermalAction::None);
        assert!(!g.is_throttling());
    }
}
