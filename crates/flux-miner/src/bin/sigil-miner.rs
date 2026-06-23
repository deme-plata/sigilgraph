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

use flux_miner::engine::{format_hps, report_diag, supervisor, MinerStats};
use flux_miner::{format_flux, format_omega};

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

/// Auto-update loop (default-ON, sigil-top model): ~30 s after launch then every
/// 4 h, poll the pinned-channel manifest; if a newer version is promoted, stage
/// `<exe>.new` (swapped in on next launch by `swap_on_launch`). Surfaces status
/// to the TUI footer. Opt out with `--no-update`.
fn update_loop(
    stats: Arc<Mutex<MinerStats>>,
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
    // The GPU build mines on the GPU BY DEFAULT — downloading the -gpu exe IS the
    // request for GPU. `--cpu` forces CPU; `--gpu` is still accepted (and is the
    // only way to ask for GPU on a CPU build, where it stays CPU anyway).
    let force_cpu = args.iter().any(|a| a == "--cpu");
    let use_gpu = !force_cpu && (cfg!(feature = "gpu") || args.iter().any(|a| a == "--gpu"));
    // What lane A actually runs on. cfg!(feature="gpu") is false in the CPU build,
    // so this correctly reports CPU there.
    let mode = if use_gpu && cfg!(feature = "gpu") { "GPU" } else { "CPU" };

    eprintln!("\n  ⛏  SIGIL MINER v{VERSION} — dual-lane (BLAKE4 Φ + VDF Ω)  ·  MODE: {mode}");
    eprintln!("  wallet: {wallet}  ({wsource})");
    eprintln!("  node:   {url}");
    eprintln!("  keys:   q quit · u update-now · g toggle GPU/CPU    flags: --headless --gpu --gpu-selftest --no-update\n");

    // Startup ping → the node sees exactly which version + requested mode is running
    // (removes "is the user on the new build?" ambiguity when debugging GPU remotely).
    {
        let gpu_feat = cfg!(feature = "gpu");
        let os = if cfg!(windows) { "windows" } else { "linux" };
        report_diag(&url, &format!("START os={os} mode={mode} use_gpu={use_gpu} gpu_feature={gpu_feat}"));
    }

    let stats = Arc::new(Mutex::new(MinerStats::default()));
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
fn run_headless(stats: &Arc<Mutex<MinerStats>>, stop: &Arc<AtomicBool>, restart: &Arc<AtomicBool>) -> anyhow::Result<()> {
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
    stats: &Arc<Mutex<MinerStats>>,
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

fn draw(f: &mut Frame, stats: &Arc<Mutex<MinerStats>>, url: &str, wallet: &str, start: Instant) {
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
            Ok(g) => {
                let mut r = format!("GPU: {}\n", g.device_name);
                let mut ok = true;
                match g.selftest() {
                    Ok(true) => r.push_str("✓ KAT passed — kernel == pow.rs (R=7 ≡ BLAKE3, R=3)\n"),
                    Ok(false) => { r.push_str("✗ KAT FAILED — kernel disagrees with pow.rs\n"); ok = false; }
                    Err(e) => { r.push_str(&format!("✗ KAT error: {e}\n")); ok = false; }
                }
                // THE mining path — names the exact failing OpenCL call if it fails.
                match g.search_probe() {
                    Ok(n) => r.push_str(&format!("✓ SEARCH ok — GPU mining works (found nonce {n})\n")),
                    Err(e) => { r.push_str(&format!("✗ SEARCH FAILED: {e}\n")); ok = false; }
                }
                (r, if ok { 0 } else { 1 })
            }
            Err(e) => (format!("gpu init failed:\n{e}\n"), 1),
        };
        print!("  {report}");
        let _ = std::fs::write("sigil-gpu-selftest.txt", &report);
        report_diag(DEFAULT_URL, &format!("gpu-selftest: {report}"));
        eprintln!("  (also written to sigil-gpu-selftest.txt)");
        std::process::exit(code);
    }
    #[cfg(not(feature = "gpu"))]
    {
        println!("  built without GPU support — rebuild with:  --features gpu");
        std::process::exit(2);
    }
}
