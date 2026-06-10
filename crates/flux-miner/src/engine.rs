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
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use flux_vdf::ModSquaring;

use crate::client::{solve, Endpoints, MinerClient, Submission};

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
    while !stop.load(Ordering::Relaxed) {
        let c = match client.fetch_challenge() {
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
        let t0 = Instant::now();
        let block = solve(&c, &wallet, &g); // Lane A nonce search + Lane B VDF
        let dt = t0.elapsed().as_secs_f64().max(1e-9);
        let hashes = block.nonce as f64 + 1.0; // nonces tried ≈ BLAKE4 work
        let sub = Submission { height: c.height, wallet: wallet.clone(), block };
        let res = client.submit(&sub);
        {
            let mut s = stats.lock().unwrap();
            s.connected = true;
            s.last_err = None;
            s.vdf_t = c.vdf_t;
            s.last_height = c.height;
            s.last_solve_ms = dt * 1000.0;
            s.hashrate = hashes / dt;
            s.vdf_rate = c.vdf_t as f64 / dt;
            s.solve_hist.push_back((dt * 1000.0) as u64);
            while s.solve_hist.len() > 80 {
                s.solve_hist.pop_front();
            }
            match res {
                Ok(r) if r.accepted => {
                    s.shares_ok += 1;
                    push_log(&mut s.log, format!("✓ h={:<8} {:>6.0}ms  ACCEPTED", c.height, dt * 1000.0));
                }
                Ok(r) => {
                    s.shares_bad += 1;
                    push_log(&mut s.log, format!("✗ h={:<8} rejected: {}", c.height, r.reason.unwrap_or_default()));
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
    // LANE-C v0.50: thermal guard — while GPU mining, poll nvidia-smi every 10 s and
    // force a CPU fallback at >=85C (a laptop operator hard-powered-off twice mining
    // on GPU — Windows Kernel-Power 41). Spawned + fail-silent: no nvidia-smi → no
    // guard, CPU path untouched. GPU builds only (CPU-only builds never go GPU-active).
    #[cfg(feature = "gpu")]
    {
        let (st, dg, gf, sp) = (stats.clone(), desired_gpu.clone(), gpu_failed.clone(), stop.clone());
        thread::spawn(move || thermal_watch(st, dg, gf, sp));
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
                    thread::spawn(move || gpu_mining_loop(u, w, st, ws, gf));
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
) {
    use crate::client::build_header;
    // v0.37 STABILITY: this is usually the PRIMARY (display) GPU. A monopolizing
    // 1M-item dispatch loop with no yield starved the Windows desktop and tripped
    // WDDM TDR (driver reset -> near-BSOD). Keep each dispatch SHORT and sleep a
    // few ms between them so the driver can service the display. A dedicated rig
    // can raise the batch / disable the sleep via env.
    let batch: usize = std::env::var("SIGIL_GPU_BATCH").ok().and_then(|v| v.parse().ok())
        .filter(|&b| b >= 4096).unwrap_or(1 << 18); // 256K default (was 1M)
    let throttle = std::time::Duration::from_millis(
        std::env::var("SIGIL_GPU_THROTTLE_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(5),
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

    while !stop.load(Ordering::Relaxed) {
        let c = match client.fetch_challenge() {
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
        let header = build_header(&c, &wallet);
        let t0 = Instant::now();
        let mut nonce_base = 0u64;
        let mut found = None;
        while found.is_none() && !stop.load(Ordering::Relaxed) {
            match gpu.search(&header, c.blake4_target, rounds, nonce_base, batch) {
                Ok(r) => {
                    found = r;
                    nonce_base = nonce_base.wrapping_add(batch as u64);
                    // yield the GPU so it can still drive the display -> no freeze / TDR
                    if found.is_none() && !throttle.is_zero() {
                        thread::sleep(throttle);
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
            s.hashrate = nonce_base as f64 / dt; // GPU Lane-A rate
            s.vdf_rate = c.vdf_t as f64 / dt;
            s.solve_hist.push_back((dt * 1000.0) as u64);
            while s.solve_hist.len() > 80 {
                s.solve_hist.pop_front();
            }
            match res {
                Ok(r) if r.accepted => {
                    s.shares_ok += 1;
                    push_log(&mut s.log, format!("✓ h={:<8} {:>6.0}ms  GPU ACCEPTED", c.height, dt * 1000.0));
                }
                Ok(r) => {
                    s.shares_bad += 1;
                    push_log(&mut s.log, format!("✗ h={:<8} rejected: {}", c.height, r.reason.unwrap_or_default()));
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
    /// Too hot — drop GPU mining to CPU now.
    FallbackToCpu,
    /// Sustained-cool after a thermal fallback — GPU is safe to use again.
    AllowGpu,
    /// No change this sample.
    None,
}

/// Deterministic thermal policy. Time is counted in *samples* (the watcher polls on
/// a fixed cadence), so the decision is a pure function of the temperature stream —
/// trivially unit-testable with a mock temp source, no clock or real GPU needed.
#[cfg(any(feature = "gpu", test))]
#[derive(Debug, Clone)]
pub struct ThermalGuard {
    hot_c: f64,               // >= this → fall back (default 85C)
    cool_c: f64,              // <= this counts as "cool" (default 70C)
    cool_samples_needed: u32, // consecutive cool samples before re-allowing GPU
    cool_streak: u32,
    in_fallback: bool,        // true once WE forced a thermal fallback
}

#[cfg(any(feature = "gpu", test))]
impl ThermalGuard {
    /// Production policy: hot 85C, cool 70C, 30 samples (= 5 min at the 10 s cadence).
    pub fn new() -> Self { Self::with(85.0, 70.0, 30) }

    /// Construct with explicit thresholds (used by tests for short streaks).
    pub fn with(hot_c: f64, cool_c: f64, cool_samples_needed: u32) -> Self {
        ThermalGuard { hot_c, cool_c, cool_samples_needed, cool_streak: 0, in_fallback: false }
    }

    /// Whether the guard currently believes GPU is thermally disabled.
    pub fn in_fallback(&self) -> bool { self.in_fallback }

    /// Feed one temperature sample. `gpu_active` = is the engine GPU-mining right now.
    pub fn step(&mut self, temp_c: f64, gpu_active: bool) -> ThermalAction {
        if temp_c >= self.hot_c {
            self.cool_streak = 0;
            if gpu_active && !self.in_fallback {
                self.in_fallback = true;
                return ThermalAction::FallbackToCpu;
            }
            return ThermalAction::None;
        }
        if temp_c <= self.cool_c {
            self.cool_streak = self.cool_streak.saturating_add(1);
            if self.in_fallback && self.cool_streak >= self.cool_samples_needed {
                self.in_fallback = false;
                self.cool_streak = 0;
                return ThermalAction::AllowGpu;
            }
        } else {
            // between cool and hot — not sustained-cool, reset the streak.
            self.cool_streak = 0;
        }
        ThermalAction::None
    }
}

#[cfg(any(feature = "gpu", test))]
impl Default for ThermalGuard {
    fn default() -> Self { Self::new() }
}

/// Read GPU temperature (Celsius) via nvidia-smi. Returns None when nvidia-smi is
/// absent or errors — the guard then simply does nothing (CPU path untouched).
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

/// Background thermal watcher (GPU builds only). Polls every 10 s; on >=85C while
/// GPU mining it raises `gpu_failed` (the supervisor consumes it → CPU) and logs the
/// temp; after 5 min sustained <=70C it re-arms `desired_gpu` so the supervisor
/// switches GPU back on. Fail-silent when nvidia-smi is unavailable.
#[cfg(feature = "gpu")]
pub fn thermal_watch(
    stats: Arc<Mutex<MinerStats>>,
    desired_gpu: Arc<AtomicBool>,
    gpu_failed: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
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
            ThermalAction::FallbackToCpu => {
                gpu_failed.store(true, Ordering::Relaxed);
                if let Ok(mut s) = stats.lock() {
                    push_log(&mut s.log, format!("GPU {temp:.0}C — thermal fallback to CPU"));
                }
            }
            ThermalAction::AllowGpu => {
                desired_gpu.store(true, Ordering::Relaxed);
                if let Ok(mut s) = stats.lock() {
                    push_log(&mut s.log, format!("GPU {temp:.0}C cool 5+ min — re-enabling GPU"));
                }
            }
            ThermalAction::None => {}
        }
    }
}

#[cfg(test)]
mod thermal_tests {
    use super::{ThermalAction, ThermalGuard};

    #[test]
    fn hot_sample_falls_back_to_cpu_once() {
        let mut g = ThermalGuard::with(85.0, 70.0, 3);
        assert_eq!(g.step(72.0, true), ThermalAction::None);          // below hot → nothing
        assert_eq!(g.step(87.0, true), ThermalAction::FallbackToCpu); // hot + GPU → fall back
        assert!(g.in_fallback());
        assert_eq!(g.step(90.0, true), ThermalAction::None);          // still hot → no repeat
    }

    #[test]
    fn no_fallback_when_gpu_inactive() {
        let mut g = ThermalGuard::with(85.0, 70.0, 3);
        // hot but NOT GPU-mining (CPU mode / CPU-only build) → guard must not act.
        assert_eq!(g.step(95.0, false), ThermalAction::None);
        assert!(!g.in_fallback());
    }

    #[test]
    fn sustained_cool_re_enables_gpu_after_fallback() {
        let mut g = ThermalGuard::with(85.0, 70.0, 3);
        assert_eq!(g.step(88.0, true), ThermalAction::FallbackToCpu);
        assert_eq!(g.step(65.0, false), ThermalAction::None);   // cool streak 1
        assert_eq!(g.step(60.0, false), ThermalAction::None);   // streak 2
        assert_eq!(g.step(58.0, false), ThermalAction::AllowGpu); // streak 3 → re-enable
        assert!(!g.in_fallback());
    }

    #[test]
    fn cool_streak_resets_on_a_warm_blip() {
        let mut g = ThermalGuard::with(85.0, 70.0, 3);
        assert_eq!(g.step(88.0, true), ThermalAction::FallbackToCpu);
        assert_eq!(g.step(60.0, false), ThermalAction::None); // streak 1
        assert_eq!(g.step(78.0, false), ThermalAction::None); // warm blip → reset
        assert_eq!(g.step(60.0, false), ThermalAction::None); // streak 1 again
        assert_eq!(g.step(60.0, false), ThermalAction::None); // streak 2
        assert_eq!(g.step(60.0, false), ThermalAction::AllowGpu); // streak 3 → re-enable
        assert!(!g.in_fallback());
    }
}
