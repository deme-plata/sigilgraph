//! sigil-miner — the SIGIL standalone miner, with a TUI.
//!
//! Drives the dual-lane work surface (`/api/v1/mining/{challenge,submit}`) of a
//! SIGIL node via flux-miner's `MinerClient`: Lane A = BLAKE4 nonce search (Φ,
//! the POWER lane), Lane B = a Wesolowski VDF (Ω, the TIME lane). A valid share
//! carries BOTH. The engine is the flux-miner crate; this binary is the
//! operator-facing front end, with a ratatui dashboard in SIGIL's obsidian +
//! violet theme (the look lifted from the QUG standalone miner's TUI).
//!
//!   sigil-miner <wallet-64hex> [node-url]      # TUI (default)
//!   sigil-miner <wallet> [url] --headless      # plain log, no TUI / no TTY
//!   SIGIL_WALLET=<64hex> sigil-miner           # wallet via env
//!
//! Env: SIGIL_WALLET, SIGIL_MINE_URL.

use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph, Sparkline, Wrap},
    Frame, Terminal,
};

use flux_miner::client::{solve, Endpoints, MinerClient, Submission};
use flux_miner::{format_flux, format_omega};
use flux_vdf::ModSquaring;

/// Public dual-lane mining endpoint — sigil-rpcd's API port, reachable directly
/// (firewall ACCEPTs :8099). Override with a positional arg or SIGIL_MINE_URL.
const DEFAULT_URL: &str = "http://sigilgraph.quillon.xyz:8099";
/// This build's version (the flux-miner crate version) — what the auto-updater
/// compares against the published manifest.
const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Pinned-channel auto-update manifest (per-platform), same model as sigil-top:
/// only updates to the operator-promoted version in this file.
fn manifest_url() -> String {
    if let Ok(u) = std::env::var("SIGIL_MINER_MANIFEST") {
        return u; // override (testing / private channels)
    }
    let plat = if cfg!(windows) { "windows" } else { "linux" };
    // GPU builds track their OWN channel so they never self-downgrade to the CPU
    // binary (the CPU exe must run on machines with no OpenCL).
    let variant = if cfg!(feature = "gpu") { "-gpu" } else { "" };
    format!("https://sigilgraph.quillon.xyz/downloads/sigil-miner-latest-{plat}{variant}.json")
}

// ── obsidian + violet SIGIL palette ──────────────────────────────────────────
const VIOLET: Color = Color::Rgb(0x8b, 0x5c, 0xf6);
const VIOLET_HI: Color = Color::Rgb(0xc0, 0x84, 0xfc);
const CYAN: Color = Color::Rgb(0x22, 0xd3, 0xee);
const GOLD: Color = Color::Rgb(0xfb, 0xbf, 0x24);
const GREEN: Color = Color::Rgb(0x34, 0xd3, 0x99);
const RED: Color = Color::Rgb(0xf8, 0x71, 0x71);
const DIM: Color = Color::Rgb(0x94, 0xa3, 0xb8);

fn wallet_valid(w: &str) -> bool {
    w.len() == 64 && w.chars().all(|c| c.is_ascii_hexdigit())
}

/// Generate a throwaway 64-hex payout wallet from runtime entropy (time + pid,
/// xorshift). Fine for a test miner — it's just an on-testnet payout identifier.
fn gen_wallet() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut x = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
        ^ (std::process::id() as u64).wrapping_mul(0x0100_0000_01b3);
    let mut out = String::with_capacity(64);
    for _ in 0..32 {
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        let b = (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 56) as u8;
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Resolve the payout wallet so the miner ALWAYS runs (no flash-and-exit on a
/// bare double-click): explicit arg/env → `sigil-wallet.txt` → freshly generated
/// (and saved to the file so reruns reuse it). Returns (wallet, human source).
fn resolve_wallet(explicit: Option<String>) -> (String, String) {
    if let Some(w) = explicit {
        if wallet_valid(&w) {
            return (w, "arg/env".into());
        }
    }
    let path = "sigil-wallet.txt";
    if let Ok(s) = std::fs::read_to_string(path) {
        let w = s.trim().to_string();
        if wallet_valid(&w) {
            return (w, format!("{path}"));
        }
    }
    let w = gen_wallet();
    let _ = std::fs::write(path, &w);
    (w, format!("generated → {path}"))
}

/// Shared mining state, polled by the TUI/headless renderer.
#[derive(Default)]
struct Stats {
    connected: bool,
    last_err: Option<String>,
    shares_ok: u64,
    shares_bad: u64,
    last_height: u64,
    last_solve_ms: f64,
    hashrate: f64, // Φ — BLAKE4 hashes/sec (Lane A)
    vdf_rate: f64, // Ω — VDF turns/sec (Lane B)
    vdf_t: u64,
    balance: u128,
    solve_hist: VecDeque<u64>, // recent solve ms (sparkline)
    log: VecDeque<String>,     // recent share lines (newest first)
    update_msg: Option<String>, // auto-updater status line
    mode: String,              // live mining mode ("CPU" / "GPU"), owned by the supervisor
}

/// Auto-update loop (default-ON, sigil-top model): ~30 s after launch then every
/// 4 h, poll the pinned-channel manifest; if a newer version is promoted, stage
/// `<exe>.new` (swapped in on next launch by `swap_on_launch`). Surfaces status
/// to the TUI footer. Opt out with `--no-update`.
fn update_loop(
    stats: Arc<Mutex<Stats>>,
    stop: Arc<AtomicBool>,
    update_now: Arc<AtomicBool>,
    restart: Arc<AtomicBool>,
) {
    let mut first = true;
    loop {
        let wait = if first { 30 } else { 4 * 3600 };
        let mut waited = 0;
        loop {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            if update_now.swap(false, Ordering::Relaxed) {
                break; // manual 'u' trigger
            }
            thread::sleep(Duration::from_secs(1));
            waited += 1;
            if waited >= wait {
                break;
            }
        }
        first = false;
        stats.lock().unwrap().update_msg = Some(format!("checking for update (v{VERSION})…"));
        match flux_miner::updater::check(&manifest_url(), VERSION) {
            Some(info) => match flux_miner::updater::stage(&info.url) {
                Ok(_) => {
                    stats.lock().unwrap().update_msg = Some(format!("⬆ v{} — restarting…", info.version));
                    restart.store(true, Ordering::Relaxed); // main applies + re-execs
                    return;
                }
                Err(e) => {
                    stats.lock().unwrap().update_msg = Some(format!("update v{} fetch failed: {e}", info.version));
                }
            },
            None => {
                stats.lock().unwrap().update_msg = Some(format!("✓ up to date (v{VERSION})"));
            }
        }
    }
}

/// Apply a staged update (swap `<exe>.new` into place) and re-exec with the same
/// args — the auto-restart sigil-top does. Call ONLY after the terminal is
/// restored. Does not return on success.
fn apply_and_restart() -> ! {
    flux_miner::updater::swap_on_launch(); // self→.old, .new→self
    let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("sigil-miner"));
    let args: Vec<String> = std::env::args().skip(1).collect();
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let _ = std::process::Command::new(&exe).args(&args).exec(); // replaces the image
    }
    #[cfg(not(unix))]
    {
        let _ = std::process::Command::new(&exe).args(&args).spawn();
    }
    std::process::exit(0);
}

/// Mining supervisor: owns the worker thread's lifecycle so the engine can be
/// hot-switched at runtime (the `g` key flips `desired_gpu`). When the desired
/// mode changes it signals the current worker to stop and starts the other, and
/// writes the live mode into Stats so the TUI badge reflects reality.
fn supervisor(
    url: String,
    wallet: String,
    stats: Arc<Mutex<Stats>>,
    stop: Arc<AtomicBool>,
    desired_gpu: Arc<AtomicBool>,
    gpu_failed: Arc<AtomicBool>,
) {
    let mut cur: Option<bool> = None;
    let mut wstop = Arc::new(AtomicBool::new(false));
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

fn push_log(log: &mut VecDeque<String>, line: String) {
    log.push_front(line);
    while log.len() > 200 {
        log.pop_back();
    }
}

/// Classical hashrate ladder: H/s · kH/s · MH/s · GH/s · TH/s · PH/s · EH/s.
fn format_hps(hps: f64) -> String {
    const U: [&str; 7] = ["H/s", "kH/s", "MH/s", "GH/s", "TH/s", "PH/s", "EH/s"];
    let mut v = hps;
    let mut i = 0;
    while v >= 1000.0 && i < U.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    format!("{v:.2} {}", U[i])
}

/// GET {url}/api/v1/balance?wallet=… → the NATIVE balance (flat-JSON pluck).
fn fetch_balance(url: &str, wallet: &str) -> Option<u128> {
    let u = format!("{url}/api/v1/balance?wallet={wallet}");
    let txt = reqwest::blocking::get(&u).ok()?.text().ok()?;
    let tail = txt.split("\"balance\":").nth(1)?;
    let digits: String = tail.trim_start().chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// The mining engine: fetch challenge → dual-lane solve → submit → record.
fn mining_loop(url: String, wallet: String, stats: Arc<Mutex<Stats>>, stop: Arc<AtomicBool>) {
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
    }
}

fn main() -> anyhow::Result<()> {
    flux_miner::updater::swap_on_launch(); // apply any update staged last run
    let args: Vec<String> = std::env::args().collect();

    // GPU utility modes (no wallet needed): enumerate devices / run the on-hardware
    // KAT that proves the OpenCL kernel == pow.rs. The RTX-2060 validation path.
    if args.iter().any(|a| a == "--gpu-list") {
        gpu_list_and_exit();
    }
    if args.iter().any(|a| a == "--gpu-selftest") {
        gpu_selftest_and_exit();
    }

    let positional: Vec<&String> = args.iter().skip(1).filter(|a| !a.starts_with("--")).collect();
    // Wallet ALWAYS resolves (arg/env → file → generated) so a bare double-click
    // runs + mines instead of flashing a usage screen and closing.
    let explicit = positional
        .first()
        .map(|s| s.to_string())
        .or_else(|| std::env::var("SIGIL_WALLET").ok());
    let (wallet, wsource) = resolve_wallet(explicit);
    let url = positional
        .get(1)
        .map(|s| s.to_string())
        .or_else(|| std::env::var("SIGIL_MINE_URL").ok())
        .unwrap_or_else(|| DEFAULT_URL.to_string());
    let headless = args.iter().any(|a| a == "--headless" || a == "--no-tui");
    let use_gpu = args.iter().any(|a| a == "--gpu");
    // What lane A actually runs on. cfg!(feature="gpu") is false in the CPU build,
    // so --gpu there correctly still reports CPU.
    let mode = if use_gpu && cfg!(feature = "gpu") { "GPU" } else { "CPU" };

    eprintln!("\n  ⛏  SIGIL MINER v{VERSION} — dual-lane (BLAKE4 Φ + VDF Ω)  ·  MODE: {mode}");
    eprintln!("  wallet: {wallet}  ({wsource})");
    eprintln!("  node:   {url}");
    eprintln!("  keys:   q quit · u update-now · g toggle GPU/CPU    flags: --headless --gpu --gpu-selftest --no-update\n");

    let stats = Arc::new(Mutex::new(Stats::default()));
    stats.lock().unwrap().mode = mode.into();
    let stop = Arc::new(AtomicBool::new(false));
    let desired_gpu = Arc::new(AtomicBool::new(use_gpu && cfg!(feature = "gpu")));
    let gpu_failed = Arc::new(AtomicBool::new(false));
    let update_now = Arc::new(AtomicBool::new(false));
    let restart = Arc::new(AtomicBool::new(false));

    // mining supervisor — owns the worker; hot-switches CPU↔GPU on the `g` key,
    // and falls back to CPU if the GPU worker reports an init failure.
    {
        let (u, w, s, st, dg, gf) = (
            url.clone(),
            wallet.clone(),
            stats.clone(),
            stop.clone(),
            desired_gpu.clone(),
            gpu_failed.clone(),
        );
        thread::spawn(move || supervisor(u, w, s, st, dg, gf));
    }

    // auto-updater (default ON; --no-update to skip; `u` triggers an immediate check)
    if !args.iter().any(|a| a == "--no-update") {
        let (s, st, un, rs) = (stats.clone(), stop.clone(), update_now.clone(), restart.clone());
        thread::spawn(move || update_loop(s, st, un, rs));
    }

    let res = if headless {
        run_headless(&stats, &stop, &restart)
    } else {
        run_tui(&stats, &stop, &url, &wallet, &desired_gpu, &update_now, &restart)
    };
    // a staged update breaks the UI loop → apply it + re-exec with the same args
    if restart.load(Ordering::Relaxed) {
        apply_and_restart();
    }
    res
}

/// Plain-output fallback (no TTY / CI / `--headless`).
fn run_headless(stats: &Arc<Mutex<Stats>>, stop: &Arc<AtomicBool>, restart: &Arc<AtomicBool>) -> anyhow::Result<()> {
    println!("  ⛏  SIGIL MINER v{VERSION} — dual-lane (BLAKE4 Φ + VDF Ω) — headless\n");
    let mut last_ok = 0u64;
    let mut last_update: Option<String> = None;
    loop {
        if restart.load(Ordering::Relaxed) {
            println!("  [update] restarting into the new version…");
            break;
        }
        thread::sleep(Duration::from_millis(500));
        let s = stats.lock().unwrap();
        if s.update_msg != last_update {
            last_update = s.update_msg.clone();
            if let Some(u) = &last_update {
                println!("  [update] {u}");
            }
        }
        if let Some(line) = s.log.front() {
            if s.shares_ok + s.shares_bad != last_ok {
                last_ok = s.shares_ok + s.shares_bad;
                println!(
                    "  [{}] {line}   [✓{} ✗{}]  {} (Φ {})  bal {} SIGIL",
                    s.mode,
                    s.shares_ok,
                    s.shares_bad,
                    format_hps(s.hashrate),
                    format_flux(s.hashrate),
                    s.balance
                );
            }
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }
    }
    Ok(())
}

fn run_tui(
    stats: &Arc<Mutex<Stats>>,
    stop: &Arc<AtomicBool>,
    url: &str,
    wallet: &str,
    desired_gpu: &Arc<AtomicBool>,
    update_now: &Arc<AtomicBool>,
    restart: &Arc<AtomicBool>,
) -> anyhow::Result<()> {
    enable_raw_mode()?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen)?;
    let mut term = Terminal::new(CrosstermBackend::new(out))?;
    let start = Instant::now();

    let res = (|| -> anyhow::Result<()> {
        loop {
            if restart.load(Ordering::Relaxed) {
                break; // staged update → leave the loop so main re-execs
            }
            term.draw(|f| draw(f, stats, url, wallet, start))?;
            if event::poll(Duration::from_millis(250))? {
                if let Event::Key(k) = event::read()? {
                    match k.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => break,
                        // u — check for update now (stages on a newer promoted version)
                        KeyCode::Char('u') | KeyCode::Char('U') => {
                            update_now.store(true, Ordering::Relaxed);
                            stats.lock().unwrap().update_msg = Some("checking for update…".into());
                        }
                        // g — toggle the mining engine GPU ↔ CPU (supervisor switches)
                        KeyCode::Char('g') | KeyCode::Char('G') => {
                            let now = desired_gpu.load(Ordering::Relaxed);
                            desired_gpu.store(!now, Ordering::Relaxed);
                        }
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    })();

    stop.store(true, Ordering::Relaxed);
    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    term.show_cursor()?;
    res
}

/// One stat card: a bordered block with a label + a big value.
fn card(value: String, label: &str, color: Color) -> Paragraph<'static> {
    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(value, Style::default().fg(color).add_modifier(Modifier::BOLD))),
        Line::from(Span::styled(label.to_string(), Style::default().fg(DIM))),
    ];
    Paragraph::new(lines).alignment(Alignment::Center).block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::Rgb(0x2a, 0x20, 0x3a))),
    )
}

fn draw(f: &mut Frame, stats: &Arc<Mutex<Stats>>, url: &str, wallet: &str, start: Instant) {
    let s = stats.lock().unwrap();
    let mode = s.mode.clone();
    let area = f.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header
            Constraint::Length(5), // stat cards · row 1 (rates)
            Constraint::Length(5), // stat cards · row 2 (tally)
            Constraint::Length(5), // sparkline
            Constraint::Min(4),    // log
            Constraint::Length(1), // footer
        ])
        .split(area);

    // ── header ──
    let conn = if s.connected {
        Span::styled("● LIVE", Style::default().fg(GREEN).add_modifier(Modifier::BOLD))
    } else {
        Span::styled("● connecting…", Style::default().fg(GOLD))
    };
    let wshort = format!("{}…{}", &wallet[..8.min(wallet.len())], &wallet[wallet.len().saturating_sub(6)..]);
    let header = Paragraph::new(vec![Line::from(vec![
        Span::styled("  ⛏ SIGIL MINER  ", Style::default().fg(VIOLET_HI).add_modifier(Modifier::BOLD)),
        Span::styled(
            format!("[{mode}] "),
            Style::default().fg(if mode == "GPU" { GREEN } else { CYAN }).add_modifier(Modifier::BOLD),
        ),
        Span::styled("dual-lane  ", Style::default().fg(DIM)),
        Span::styled("BLAKE4 Φ", Style::default().fg(VIOLET).add_modifier(Modifier::BOLD)),
        Span::styled(" + ", Style::default().fg(DIM)),
        Span::styled("VDF Ω", Style::default().fg(CYAN).add_modifier(Modifier::BOLD)),
        Span::raw("   "),
        conn,
    ])])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(VIOLET))
            .title(Span::styled(
                format!(" {} · {} ", url, wshort),
                Style::default().fg(DIM),
            ))
            .title_alignment(Alignment::Right),
    );
    f.render_widget(header, rows[0]);

    // ── stat cards · row 1 (the rates) ──
    let r1 = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Ratio(1, 4); 4])
        .split(rows[1]);
    f.render_widget(card(format_hps(s.hashrate), "HASHRATE (BLAKE4)", VIOLET_HI), r1[0]);
    f.render_widget(card(format_flux(s.hashrate), "Φ FLUX  (1Φ=1EH/s)", VIOLET), r1[1]);
    f.render_widget(card(format_omega(s.vdf_rate), "Ω VDF  (TIME)", CYAN), r1[2]);
    f.render_widget(
        card(mode.to_string(), "MODE", if mode == "GPU" { GREEN } else { CYAN }),
        r1[3],
    );

    // ── stat cards · row 2 (the tally) ──
    let total = s.shares_ok + s.shares_bad;
    let accept = if total > 0 { s.shares_ok as f64 / total as f64 * 100.0 } else { 100.0 };
    let r2 = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Ratio(1, 5); 5])
        .split(rows[2]);
    f.render_widget(card(format!("{}", s.shares_ok), "SHARES ✓", GREEN), r2[0]);
    f.render_widget(card(format!("{}", s.shares_bad), "SHARES ✗", if s.shares_bad > 0 { RED } else { DIM }), r2[1]);
    f.render_widget(card(format!("{accept:.0}%"), "ACCEPT", if accept >= 99.0 { GREEN } else { GOLD }), r2[2]);
    f.render_widget(card(format!("{}", s.balance), "BALANCE · SIGIL", GOLD), r2[3]);
    f.render_widget(card(format!("{}", s.last_height), "HEIGHT", VIOLET_HI), r2[4]);

    // ── sparkline of recent solve times ──
    let hist: Vec<u64> = s.solve_hist.iter().copied().collect();
    let spark = Sparkline::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::Rgb(0x2a, 0x20, 0x3a)))
                .title(Span::styled(
                    format!(" solve time · last {:.0}ms · vdf_t={} · height {} ", s.last_solve_ms, s.vdf_t, s.last_height),
                    Style::default().fg(DIM),
                )),
        )
        .data(&hist)
        .style(Style::default().fg(VIOLET));
    f.render_widget(spark, rows[3]);

    // ── recent shares log ──
    let log_lines: Vec<Line> = s
        .log
        .iter()
        .take(rows[4].height.saturating_sub(2) as usize)
        .map(|l| {
            let color = if l.starts_with('✓') {
                GREEN
            } else if l.starts_with('✗') {
                GOLD
            } else {
                RED
            };
            Line::from(Span::styled(format!("  {l}"), Style::default().fg(color)))
        })
        .collect();
    let log = Paragraph::new(log_lines)
        .wrap(Wrap { trim: true })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::Rgb(0x2a, 0x20, 0x3a)))
                .title(Span::styled(" recent shares ", Style::default().fg(VIOLET_HI))),
        );
    f.render_widget(log, rows[4]);

    // ── footer ──
    let up = start.elapsed().as_secs();
    let err = s.last_err.clone().map(|e| format!("  ⚠ {e}")).unwrap_or_default();
    let update = s.update_msg.clone().map(|u| format!("   {u}")).unwrap_or_default();
    let footer = Paragraph::new(Line::from(vec![
        Span::styled("  q ", Style::default().fg(VIOLET).add_modifier(Modifier::BOLD)),
        Span::styled("quit  ", Style::default().fg(DIM)),
        Span::styled("u ", Style::default().fg(VIOLET).add_modifier(Modifier::BOLD)),
        Span::styled("update  ", Style::default().fg(DIM)),
        Span::styled("g ", Style::default().fg(VIOLET).add_modifier(Modifier::BOLD)),
        Span::styled("GPU/CPU", Style::default().fg(DIM)),
        Span::styled(format!("   {}m{:02}s", up / 60, up % 60), Style::default().fg(DIM)),
        Span::styled(update, Style::default().fg(GOLD)),
        Span::styled(err, Style::default().fg(RED)),
    ]));
    f.render_widget(footer, rows[5]);
}

// ── GPU modes ────────────────────────────────────────────────────────────────

/// `--gpu-list`: enumerate OpenCL GPUs, then exit.
fn gpu_list_and_exit() -> ! {
    #[cfg(feature = "gpu")]
    {
        let devs = flux_miner::gpu::list_devices();
        if devs.is_empty() {
            println!("  no OpenCL GPU found (is the driver / OpenCL runtime installed?)");
        }
        for d in devs {
            println!("  GPU: {}  ·  {} MB  ·  max work-group {}", d.name, d.global_mem_mb, d.max_work_group);
        }
    }
    #[cfg(not(feature = "gpu"))]
    println!("  built without GPU support — rebuild with:  --features gpu");
    std::process::exit(0);
}

/// `--gpu-selftest`: run the on-hardware BLAKE4 KAT (GPU kernel must equal
/// `pow::blake4_word`), then exit 0 on pass / 1 on fail.
fn gpu_selftest_and_exit() -> ! {
    #[cfg(feature = "gpu")]
    {
        // Accumulate the result so it's ALSO written to a file — a Windows
        // double-click flashes the window shut, so the file is how you read it.
        let (report, code) = match flux_miner::gpu::GpuBlake4::new() {
            Ok(g) => match g.selftest() {
                Ok(true) => (
                    format!("GPU: {}\n✓ BLAKE4 GPU KAT passed — kernel == pow.rs (R=7 ≡ BLAKE3, R=3)\n", g.device_name),
                    0,
                ),
                Ok(false) => (
                    format!("GPU: {}\n✗ BLAKE4 GPU KAT FAILED — kernel disagrees with pow.rs\n", g.device_name),
                    1,
                ),
                Err(e) => (format!("GPU: {}\ngpu selftest error: {e}\n", g.device_name), 1),
            },
            Err(e) => (format!("gpu init failed (build log below):\n{e}\n"), 1),
        };
        print!("  {report}");
        let _ = std::fs::write("sigil-gpu-selftest.txt", &report);
        eprintln!("  (also written to sigil-gpu-selftest.txt)");
        std::process::exit(code);
    }
    #[cfg(not(feature = "gpu"))]
    {
        println!("  built without GPU support — rebuild with:  --features gpu");
        std::process::exit(2);
    }
}

/// `--gpu`: hybrid mining — GPU searches Lane A (BLAKE4), CPU does Lane B (VDF).
/// Uses FULL_ROUNDS so shares pass the node's `verify_dual` (legacy blake4 == R7).
#[cfg(feature = "gpu")]
fn gpu_mining_loop(
    url: String,
    wallet: String,
    stats: Arc<Mutex<Stats>>,
    stop: Arc<AtomicBool>,
    gpu_failed: Arc<AtomicBool>,
) {
    use flux_miner::client::build_header;
    const BATCH: usize = 1 << 20; // 1M nonces per GPU dispatch

    let gpu = match flux_miner::gpu::GpuBlake4::new() {
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
            gpu_failed.store(true, Ordering::Relaxed);
            return;
        }
    };
    {
        let mut s = stats.lock().unwrap();
        push_log(&mut s.log, format!("GPU: {}", gpu.device_name));
    }
    let g = ModSquaring::bench_2048();
    let rounds = flux_miner::pow::FULL_ROUNDS; // MUST match the node's verify_dual
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
            match gpu.search(&header, c.blake4_target, rounds, nonce_base, BATCH) {
                Ok(r) => {
                    found = r;
                    nonce_base = nonce_base.wrapping_add(BATCH as u64);
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
        let block = flux_miner::block_for_nonce(&header, nonce, &g, c.vdf_t); // Lane B on CPU
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
    }
}
