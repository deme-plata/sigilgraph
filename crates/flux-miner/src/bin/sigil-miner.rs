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

// ── obsidian + violet SIGIL palette ──────────────────────────────────────────
const VIOLET: Color = Color::Rgb(0x8b, 0x5c, 0xf6);
const VIOLET_HI: Color = Color::Rgb(0xc0, 0x84, 0xfc);
const CYAN: Color = Color::Rgb(0x22, 0xd3, 0xee);
const GOLD: Color = Color::Rgb(0xfb, 0xbf, 0x24);
const GREEN: Color = Color::Rgb(0x34, 0xd3, 0x99);
const RED: Color = Color::Rgb(0xf8, 0x71, 0x71);
const DIM: Color = Color::Rgb(0x94, 0xa3, 0xb8);

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
}

fn push_log(log: &mut VecDeque<String>, line: String) {
    log.push_front(line);
    while log.len() > 200 {
        log.pop_back();
    }
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
    let wallet = positional
        .first()
        .map(|s| s.to_string())
        .or_else(|| std::env::var("SIGIL_WALLET").ok())
        .unwrap_or_default();
    let url = positional
        .get(1)
        .map(|s| s.to_string())
        .or_else(|| std::env::var("SIGIL_MINE_URL").ok())
        .unwrap_or_else(|| DEFAULT_URL.to_string());

    if wallet.len() != 64 || !wallet.chars().all(|c| c.is_ascii_hexdigit()) {
        eprintln!("\n  ⛏  SIGIL MINER — dual-lane (BLAKE4 Φ + VDF Ω)\n");
        eprintln!("  usage: sigil-miner <wallet-64hex> [node-url]");
        eprintln!("     or: SIGIL_WALLET=<64hex> sigil-miner");
        eprintln!("  flags: --headless     plain log, no TUI");
        eprintln!("         --gpu          mine Lane A on the GPU (needs `gpu` build feature)");
        eprintln!("         --gpu-list     list OpenCL GPUs");
        eprintln!("         --gpu-selftest on-hardware BLAKE4 KAT (gpu kernel == pow.rs)\n");
        eprintln!("  default node: {DEFAULT_URL}\n");
        std::process::exit(2);
    }

    let headless = args.iter().any(|a| a == "--headless" || a == "--no-tui");
    let use_gpu = args.iter().any(|a| a == "--gpu");
    let stats = Arc::new(Mutex::new(Stats::default()));
    let stop = Arc::new(AtomicBool::new(false));
    {
        let (s, st, u, w) = (stats.clone(), stop.clone(), url.clone(), wallet.clone());
        if use_gpu {
            #[cfg(feature = "gpu")]
            thread::spawn(move || gpu_mining_loop(u, w, s, st));
            #[cfg(not(feature = "gpu"))]
            {
                eprintln!("  --gpu ignored: built without the `gpu` feature — mining on CPU");
                thread::spawn(move || mining_loop(u, w, s, st));
            }
        } else {
            thread::spawn(move || mining_loop(u, w, s, st));
        }
    }

    if headless {
        run_headless(&stats, &stop)
    } else {
        run_tui(&stats, &stop, &url, &wallet)
    }
}

/// Plain-output fallback (no TTY / CI / `--headless`).
fn run_headless(stats: &Arc<Mutex<Stats>>, stop: &Arc<AtomicBool>) -> anyhow::Result<()> {
    println!("  ⛏  SIGIL MINER — dual-lane (BLAKE4 Φ + VDF Ω) — headless\n");
    let mut last_ok = 0u64;
    loop {
        thread::sleep(Duration::from_millis(500));
        let s = stats.lock().unwrap();
        if let Some(line) = s.log.front() {
            if s.shares_ok + s.shares_bad != last_ok {
                last_ok = s.shares_ok + s.shares_bad;
                println!(
                    "  {line}   [✓{} ✗{}]  Φ {}  bal {} SIGIL",
                    s.shares_ok,
                    s.shares_bad,
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

fn run_tui(stats: &Arc<Mutex<Stats>>, stop: &Arc<AtomicBool>, url: &str, wallet: &str) -> anyhow::Result<()> {
    enable_raw_mode()?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen)?;
    let mut term = Terminal::new(CrosstermBackend::new(out))?;
    let start = Instant::now();

    let res = (|| -> anyhow::Result<()> {
        loop {
            term.draw(|f| draw(f, stats, url, wallet, start))?;
            if event::poll(Duration::from_millis(250))? {
                if let Event::Key(k) = event::read()? {
                    match k.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => break,
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
    let area = f.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header
            Constraint::Length(6), // stat cards
            Constraint::Length(6), // sparkline
            Constraint::Min(5),    // log
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

    // ── stat cards ──
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Ratio(1, 5),
            Constraint::Ratio(1, 5),
            Constraint::Ratio(1, 5),
            Constraint::Ratio(1, 5),
            Constraint::Ratio(1, 5),
        ])
        .split(rows[1]);
    f.render_widget(card(format_flux(s.hashrate), "Φ  POWER (BLAKE4)", VIOLET_HI), cols[0]);
    f.render_widget(card(format_omega(s.vdf_rate), "Ω  TIME (VDF)", CYAN), cols[1]);
    f.render_widget(card(format!("{}", s.shares_ok), "SHARES ✓", GREEN), cols[2]);
    f.render_widget(card(format!("{}", s.shares_bad), "SHARES ✗", if s.shares_bad > 0 { RED } else { DIM }), cols[3]);
    f.render_widget(card(format!("{}", s.balance), "BALANCE · SIGIL", GOLD), cols[4]);

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
    f.render_widget(spark, rows[2]);

    // ── recent shares log ──
    let log_lines: Vec<Line> = s
        .log
        .iter()
        .take(rows[3].height.saturating_sub(2) as usize)
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
    f.render_widget(log, rows[3]);

    // ── footer ──
    let up = start.elapsed().as_secs();
    let err = s.last_err.clone().map(|e| format!("  ⚠ {e}")).unwrap_or_default();
    let footer = Paragraph::new(Line::from(vec![
        Span::styled("  q ", Style::default().fg(VIOLET).add_modifier(Modifier::BOLD)),
        Span::styled("quit", Style::default().fg(DIM)),
        Span::styled(format!("   uptime {}m{:02}s", up / 60, up % 60), Style::default().fg(DIM)),
        Span::styled(err, Style::default().fg(RED)),
    ]));
    f.render_widget(footer, rows[4]);
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
        match flux_miner::gpu::GpuBlake4::new() {
            Ok(g) => {
                println!("  GPU: {}", g.device_name);
                match g.selftest() {
                    Ok(true) => {
                        println!("  ✓ BLAKE4 GPU KAT passed — kernel == pow.rs (R=7 ≡ BLAKE3, R=3)");
                        std::process::exit(0);
                    }
                    Ok(false) => {
                        println!("  ✗ BLAKE4 GPU KAT FAILED — kernel disagrees with pow.rs");
                        std::process::exit(1);
                    }
                    Err(e) => {
                        eprintln!("  gpu selftest error: {e}");
                        std::process::exit(1);
                    }
                }
            }
            Err(e) => {
                eprintln!("  gpu init failed: {e}");
                std::process::exit(1);
            }
        }
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
fn gpu_mining_loop(url: String, wallet: String, stats: Arc<Mutex<Stats>>, stop: Arc<AtomicBool>) {
    use flux_miner::client::build_header;
    const BATCH: usize = 1 << 20; // 1M nonces per GPU dispatch

    let gpu = match flux_miner::gpu::GpuBlake4::new() {
        Ok(g) => g,
        Err(e) => {
            stats.lock().unwrap().last_err = Some(format!("gpu init: {e}"));
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
                    stats.lock().unwrap().last_err = Some(format!("gpu search: {e}"));
                    thread::sleep(Duration::from_secs(1));
                    break;
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
