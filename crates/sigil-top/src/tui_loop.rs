//! Split out of main.rs (2026-10-08) so an edit in main() no longer re-optimizes this
//! code: rustc gives each module its own incremental codegen unit.
use super::*;

pub(crate) fn measure_eclipse_k(tip_height: u64, tip_ok: bool) -> (u32, Vec<(String, bool)>) {
    const ANCHOR: &str = "_sigil-tip.sigilgraph.quillon.xyz";
    let resolvers = [
        ("cloudflare", "https://cloudflare-dns.com/dns-query"),
        ("google", "https://dns.google/resolve"),
        ("quad9", "https://dns.quad9.net/dns-query"),
    ];
    let mut sources: Vec<(String, bool)> = vec![("node (verified)".into(), tip_ok)];
    let marker = tip_height.to_string();
    // 0.77: shared pooled client — was a fresh Client per eclipse-k measurement.
    {
        let client = &*HTTP;
        for (name, base) in resolvers {
            let url = format!("{base}?name={ANCHOR}&type=TXT");
            let agree = client
                .get(&url)
                .header("accept", "application/dns-json")
                .timeout(Duration::from_millis(2200))
                .send()
                .ok()
                .and_then(|r| r.text().ok())
                .map(|body| tip_ok && tip_height > 0 && body.contains(&marker))
                .unwrap_or(false);
            sources.push((name.to_string(), agree));
        }
    }
    let k = sources.iter().filter(|(_, ok)| *ok).count() as u32;
    (k, sources)
}

pub(crate) fn ensure_serve_up(app: &mut App) -> bool {
    // 10.4.39: `/w` opens this address (the server never derives it itself — see
    // serve::SHORT_LINK_ADDR for why).
    serve::remember_wallet_addr(&miner_wallet());
    if wallet_server_alive() {
        return true;
    }
    if let Some(old) = app.serve_stop.take() {
        old.store(true, Ordering::Relaxed);
    }
    let serve_dir = std::env::var("FLUX_STATIC_DIR")
        .unwrap_or_else(|_| "/home/orobit/q-narwhalknight/dist-fluxapp".into());
    match serve::start(&serve_dir, wallet_ui::WALLET_PORT) {
        Ok(stop) => {
            app.serve_stop = Some(stop);
            // Let the listener bind before we hand the URL to a browser.
            for _ in 0..20 {
                if wallet_server_alive() {
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
            let ok = wallet_server_alive();
            app.serve_status = if ok {
                "serve :9800 ✓ (re)started on demand".into()
            } else {
                "serve :9800 started but not answering".into()
            };
            ok
        }
        Err(e) => {
            app.serve_status = format!("serve: {e}");
            false
        }
    }
}

pub(crate) fn run_tui(cfg: Config) -> std::io::Result<()> {
    // v0.27.5: self-healing crash-loop guard — long-running dashboard only (`--once` renders
    // and exits faster than HEAL_SECS, which would false-trigger a revert). If THIS version
    // has crash-looped, this reverts to the last working binary and relaunches.
    if !cfg.once && crashloop_guard() { return Ok(()); }
    // Local mining endpoint (loopback only). It answers 503 until this node has verified
    // the chain, so starting it early is safe; SIGIL_MINING_RELAY=0 turns it off.
    if !cfg.once { mining_relay::start(kgauge_ui::api_base_of(&cfg.api)); }

    // ── v0.35 (sync-starts-earlier, DeepSeek audit S4): the ENTIRE sync bootstrap runs
    // BEFORE the terminal is touched. It used to sit after raw-mode + alt-screen + panic-
    // hook + Terminal::new, so the store open (formerly a full-db scan!), the aethe
    // bootstrap and the P2P launch all waited on UI plumbing. Now the mesh is dialing and
    // the first probe is in flight while crossterm sets up; pre-TUI prints go to stder
    // (IN_TUI is still false), which is exactly where pre-UI diagnostics belong. ─────────
    let mut app = App::new(cfg);
    // v4 AUTO-START: env-driven mining + full-sync (headless rigs, no keypress).
    if std::env::var("SIGIL_AUTOMINE").is_ok() {
        use std::sync::atomic::Ordering;
        if cfg!(feature = "gpu") && std::env::var("SIGIL_MINE_CPU").is_err() {
            app.mine_gpu_warned = true;
            app.mine_desired_gpu.store(true, Ordering::Relaxed);
        }
        app.toggle_engine_mining();
        boot_trace("AUTOMINE: engine started via SIGIL_AUTOMINE");
    }
    // 10.6.0: with your own node running the screen never downloads a second copy of the chain.
    if std::env::var("SIGIL_AUTOFULLSYNC").is_ok() && app.p2p_sync.is_none() && !own_node_on() {
        if let Some(store) = app.take_idle_store() {
            let p2p = block_sync::P2PBlockSync::launch(store, false);
            app.p2p_sync = Some(p2p);
            app.full_sync = true;
            persist_sync_mode("full");
            boot_trace("AUTOFULLSYNC: full archive started");
        }
    }
    // ── [A]I READY BEFORE ANYONE ASKS (wired 2026-09-06) ─────────────────────
    //
    // `flux_moe::autosetup_at_boot()` has existed since 8.0.6, is unit-tested, and its
    // own doc comment says "at start-up, in the background (default since 8.0.6 — the
    // [A]I tab is ready by the time anyone opens it)". It had **zero production
    // callers**. Nothing ran until `ai_enter_tab` fired, and that only fires when
    // somebody opens the tab — so ollama was never installed, never started and the
    // model was never pulled "behind the scenes", exactly as reported. Specified,
    // tested, documented, never called: grep for a caller before believing a fix ships.
    //
    // Pulling a model is hundreds of megabytes, so this is deliberately NOT silent —
    // `ai_setup_start` streams progress into the [A]I transcript, and the tab shows it
    // whenever the user gets there. Opt out with SIGIL_AI_AUTOSETUP=tab (tab-only, the
    // 8.0.4 behaviour) or =0 (never).
    if flux_moe::autosetup_at_boot() {
        app.ai_models = crate::flux_moe::list_models();
        app.ai_model = app.ai_models.first().cloned();
        if app.ai_model.is_none() {
            boot_trace("AI: autosetup at boot — installing/starting ollama and pulling the model");
            ai_setup_start(&mut app);
        } else {
            boot_trace("AI: model already present, no setup needed");
        }
    }
    flux_webhook("boot", concat!("sigil-top v", env!("CARGO_PKG_VERSION"), " starting"));
    // 2026-09-06 PAYABLE OUT OF THE BOX: a wallet that never PUBLISHES its shielded receiving
    // key cannot be paid at all — SIGIL has no public transfers to fall back on, so a phone
    // paying it gets "that wallet has not published a private receiving key". Registration
    // used to happen only inside [M]ine (`start_mining`), so a node that merely watched the
    // chain kept an unpayable address in its own header. If this process holds the wallet's
    // seed, publish the key at boot: idempotent (the keeper asks the chain first), off-thread,
    // once per process. Same feature gate and same keeper as the [M] path.
    #[cfg(feature = "shield-register")]
    {
        static BOOT_REG: std::sync::Once = std::sync::Once::new();
        // 2026-10-15: env seed OR this install's wallet file (see `miner_seed`); env-only left
        // every wallet sigil-top made for itself unpayable.
        if let Some(seed_hex) = miner_seed().map(hex::encode) {
            BOOT_REG.call_once(|| {
                boot_trace("shield-register: publishing this wallet's receiving key (boot keeper)");
                shield_setup::spawn_registration_keeper(&engine_node_url(), &seed_hex, |line| {
                    log_line(format!("shield-register: {line}"));
                });
            });
        }
    }
    // v0.71.1 LANE-V: resume the operator's chosen sync mode after update/restart.
    // Only "full" does anything; no file (fresh install) = safe light monitor.
    if false && read_sync_mode().as_deref() == Some("full") && app.p2p_sync.is_none() {
        if let Some(store) = app.take_idle_store() {
            let p2p = block_sync::P2PBlockSync::launch(store, true);
            app.p2p_sync = Some(p2p);
            app.toast = "⬇ FULL-SYNC RESUMED (sticky mode from last session — [F] to toggle)".into();
            app.toast_sticky = true;
            boot_trace("sticky full-sync resumed from mode file");
        }
    }
    // Windows: bring up the notification-area icon (Open Wallet / Explorer / Start-at-login /
    // Quit). Detached + best-effort, so it can never gate or crash the node. No-op elsewhere.
    // v0.64: the tray helper Stop-Process'es NodePid and broke double-click launches
    // (window closed instantly). OFF by default now; SIGIL_TOP_TRAY=1 opts back in.
    if std::env::var("SIGIL_TOP_TRAY").is_ok() { spawn_system_tray(); }
    boot_trace("tray gated (off unless SIGIL_TOP_TRAY)");
    // v0.10.5: async — kicks off the first fetch without blocking the first paint.
    app.request_refresh();

    // v0.12.1: sync is ON BY DEFAULT. Opt OUT with --no-sync / SIGIL_TOP_NO_SYNC=1.
    // v0.39.1 WINDOWS: default OFF — the LIGHT monitor. v0.10.2 already learned
    // this lesson (the sync engine ate the operator's PC; made opt-in), v0.12.1
    // regressed it to on-by-default, and it froze the desktop twice today. The
    // Mining tab / wallet / explorer don't need the local backfill engine — a
    // fleet node (epsilon) carries the chain. Opt IN with --sync / SIGIL_TOP_SYNC=1.
    let want_sync = if cfg!(windows) {
        std::env::args().any(|a| a == "--sync") || std::env::var("SIGIL_TOP_SYNC").is_ok()
    } else {
        !(std::env::args().any(|a| a == "--no-sync")
            || std::env::var("SIGIL_TOP_NO_SYNC").is_ok())
    };

    // v0.7.22: cross-platform PERSISTENT store path. The old /tmp + /dev/shm paths
    // don't exist on Windows → the store never persisted → re-sync from 0 every launch
    // ("starts over on update"). Now a per-user dir (override with SIGIL_TOP_DB).
    let db_path = sigil_top_db_path();
    // v7.0.17 NEVER-BLANK splash (instant now — the open no longer blocks this thread).
    println!("\n  \u{25c7} SIGIL v{} — starting… (block store opens in the background; the mesh dials immediately)\n",
        env!("CARGO_PKG_VERSION"));
    let _ = std::io::Write::flush(&mut std::io::stdout());
    // v7.0.19: flux-db compaction progress goes to raw stderr, which corrupts the TUI's
    // alternate screen ("[flux-db] compact L1->L2 ..." painted over the dashboard). Quiet
    // the chatter (errors still print) for this process before the store opens.
    std::env::set_var("FLUX_DB_QUIET", "1");

    // ── v7.0.21 DIAL-WHILE-OPENING ────────────────────────────────────────────────
    // The store open (heal → fallbacks → aether) moves to a BACKGROUND thread; the
    // sync engine launches IMMEDIATELY with a deferred store, so the mesh dials and
    // handshakes during a long compaction instead of sitting at 0 peers for minutes
    // (the v7.0.19 case: p2p couldn't start until a 2.6 GiB compaction finished).
    // The explorer's local reader arrives via a OnceLock — until then it proxies.
    let reader_cell: std::sync::Arc<std::sync::OnceLock<block_store::BlockReader>> =
        std::sync::Arc::new(std::sync::OnceLock::new());
    let (store_tx, store_rx) = std::sync::mpsc::channel::<block_sync::OpenedStore>();
    {
        let db_path = db_path.clone();
        let cell = reader_cell.clone();
        let progress = app.open_progress.clone();
        let _ = std::thread::Builder::new().name("sigil-store-open".into()).spawn(move || {
            match open_store_with_fallbacks(&db_path, want_sync, progress) {
                Ok((store, note)) => {
                    let _ = cell.set(store.reader());
                    let _ = store_tx.send(block_sync::OpenedStore { store, note });
                }
                Err(e) => {
                    // tx drops → the deferred engine / [F] helper surface "store unavailable".
                    boot_trace(&format!("store open FAILED on all fallbacks: {e}"));
                    eprintln!("sigil-top: {e}");
                }
            }
        });
    }

    // v0.26 hardening #8 (DeepSeek-reviewed): graceful SIGTERM/SIGINT — restore the
    // terminal (harmless no-op if Ctrl-C lands before raw mode) and exit cleanly; the
    // sync thread's BlockStore persists watermarks on every advance, so state is durable.
    {
        let _ = ctrlc::set_handler(move || {
            let _ = disable_raw_mode();
            let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
            std::process::exit(0);
        });
    }

    let mut sync_handle: Option<std::sync::Arc<std::sync::Mutex<block_sync::P2PSyncState>>> = None;
    // 0.77/v7.0.21: the persisted [F] choice ("full") now RESUMES on every platform —
    // Windows defaulted want_sync=false, so an operator's full archive silently didn't
    // restart after an update ("it doesn't resume") until they pressed F again.
    // 10.6.0: your own node holds and checks the whole chain, so a saved "full" or SIGIL_AUTOFULLSYNC no longer
    // starts the light engine's second copy while it runs (both work as before with SIGIL_TOP_NODE=0).
    let autostart_full = read_sync_mode().as_deref() == Some("full") && !own_node_on();
    let autofullsync = std::env::var("SIGIL_AUTOFULLSYNC").is_ok() && !own_node_on();
    // ONE-CHAIN (v7.1.6): the LEDGER header mirror is the default sync — full
    // floor→tip in ~1.4s. The SPINE monitor/backfill (31.5M empty dyno blocks,
    // ~8 blk/s serve valve → a 78-DAY eta) no longer auto-starts: it is opt-in
    // via SIGIL_SPINE_SYNC=1, a saved full-sync mode, or the explicit [F] key.
    // The spine retires in P3 (docs/SIGIL_ONE_CHAIN_SCOPE_v0.md).
    block_sync::ledger::ensure_running();
    // v10.4.17 OUT OF THE BOX: the monitor path (recent_only — fast-snap to the verified live
    // tip, NO genesis crawl) now joins the peer mesh on EVERY platform by default. The light
    // monitor used to stay "mesh off" until [F] or SIGIL_SPINE_SYNC=1, so a double-clicked exe
    // (Windows defaults want_sync=false) showed "0 peers / OFFLINE" and never dialed anyone.
    // Opt OUT with SIGIL_SPINE_SYNC=0, --no-sync or SIGIL_TOP_NO_SYNC=1.
    let spine_opt_in = mesh_join_default(
        std::env::var("SIGIL_SPINE_SYNC").ok().as_deref(),
        std::env::args().any(|a| a == "--no-sync") || std::env::var("SIGIL_TOP_NO_SYNC").is_ok(),
    );
    if spine_opt_in || autostart_full || autofullsync {
        // v0.22.1: monitor path (recent_only=true) → fast-snap to the verified live tip.
        // v0.33.5: SIGIL_FULLSYNC=1 launches a genuine genesis→tip crawl instead.
        let recent_only = std::env::var("SIGIL_FULLSYNC").map(|v| v == "0" || v.is_empty()).unwrap_or(true)
            && !autostart_full && !autofullsync;
        let p2p = block_sync::P2PBlockSync::launch_deferred(store_rx, recent_only);
        sync_handle = Some(p2p.state_handle());
        if !recent_only { app.full_sync = true; }
        app.p2p_sync = Some(p2p);
        if app.toast.is_empty() {
            app.toast = "⚡ P2P mesh connecting → Epsilon…".into(); // Delta is gone since 2026-07
        }
    } else {
        // v0.40.3/v7.0.21: park the RECEIVER so [F] can launch the sync engine ON
        // DEMAND — the store arrives from the opener thread (take_idle_store polls it).
        app.idle_store_rx = Some(store_rx);
        if app.toast.is_empty() {
            app.toast = "◆ light monitor — press F to start live sync".into();
        }
    }

    // v3: NON-FATAL terminal setup. A console that rejects raw mode / the alternate screen
    // must not abort the dashboard (that was a silent "no TUI" exit). Log and continue — the
    // render loop still paints; keys may be line-buffered in the degraded case, which beats blank.
    if let Err(e) = enable_raw_mode() { boot_trace(&format!("enable_raw_mode failed (continuing): {e}")); }
    // From here ratatui owns the screen — divert background eprintln to the logfile
    // (was smearing the dashboard with [p2p-sync]/[aether] lines).
    IN_TUI.store(true, std::sync::atomic::Ordering::Relaxed);
    // ...and the same for DEPENDENCIES that print on their own. IN_TUI only
    // diverts THIS crate's macros; flux-db writes storage diagnostics with raw
    // `eprintln!`, which land in the middle of the frame. Reported live: a real
    // SST read error ("failed to fill whole buffer") rendered as corrupted text
    // spliced through the sync card — it looked like a UI bug AND hid the
    // actual storage error. flux-db honors FLUX_DB_LOG by appending there
    // instead of writing to stderr; point it at the same logfile everything
    // else uses. Only set when unset, so an operator can still override.
    if std::env::var_os("FLUX_DB_LOG").is_none() {
        // Same resolution the logger above uses, so both streams land together.
        let p = std::env::var("HOME")
            .map(|h| format!("{h}/.sigil-top.log"))
            .or_else(|_| std::env::var("TEMP").map(|t| format!("{t}\\sigil-top.log")))
            .unwrap_or_else(|_| "sigil-top.log".into());
        std::env::set_var("FLUX_DB_LOG", p);
    }
    let mut stdout = std::io::stdout();
    if let Err(e) = execute!(stdout, EnterAlternateScreen) { boot_trace(&format!("EnterAlternateScreen failed (continuing): {e}")); }
    // 2026-09-17: from here on the TUI owns the terminal, so every `eprintln!` in the
    // node library this binary embeds (braid config, hybrid checkpoint verdicts, …) goes
    // to the logfile instead of over the dashboard. Restored on normal exit.
    if let Ok(logp) = std::env::var("FLUX_DB_LOG") {
        match platform::redirect_stderr_to(&logp) {
            Some(r) => { if let Ok(mut g) = STDERR_REDIRECT.lock() { *g = Some(r); } }
            None => boot_trace("stderr → logfile redirect failed (continuing; stray stderr may paint over the TUI)"),
        }
    }

    // v0.10.5 "stable uptime": a panic anywhere below would otherwise leave the
    // user's terminal in raw mode + alternate screen = a bricked, unusable shell.
    // Install a hook that ALWAYS restores the terminal first, then runs the
    // default panic printer. Graceful recovery instead of a wedged terminal.
    {
        let default_hook = std::panic::take_hook();
        let _ = &default_hook; // kept for reference; v0.27 hook is LOG-ONLY (see below)
        std::panic::set_hook(Box::new(move |info| {
            // v0.27: LOG ONLY — do NOT tear down the terminal here. A background-thread panic
            // must not break the still-running TUI, and a render panic is CAUGHT by catch_unwind
            // around term.draw (which re-inits + continues). Terminal restore happens on normal
            // exit (run_tui cleanup) or in the catch handler — never from this hook.
            let msg = info.payload().downcast_ref::<&str>().map(|s| s.to_string())
                .or_else(|| info.payload().downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "<non-string panic>".into());
            let loc = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
            let line = format!("[PANIC] {msg} @ {loc}");
            remember_panic(line.clone());
            log_line(line);
        }));
    }

    // LANE-Z root fix: wrap in SafeSizeBackend so a degenerate (0×0) conhost size can
    // never produce a blank screen — ratatui always gets a paintable area.
    let backend = SafeSizeBackend { inner: CrosstermBackend::new(stdout) };
    let mut term = Terminal::new(backend)?;
    // (v0.35: app/store/aether/ctrlc/sync-launch all moved ABOVE terminal init — see the
    // sync-starts-earlier block after crashloop_guard. block_reader/sync_handle/app are in
    // scope from there.)

    // v0.11.0: local-first explorer API over the verified spine + cortex snapshot.
    app.block_reader = reader_cell.clone();
    let local_api = std::sync::Arc::new(local_api::LocalApi {
        reader: reader_cell.clone(),
        sync: sync_handle,
        cortex: app.cortex_shared.clone(),
        network: build_network_id(),
    });

    // v0.7.0: Start embedded HTTP server (no external process needed)
    let serve_dir = std::env::var("FLUX_STATIC_DIR")
        .unwrap_or_else(|_| "/home/orobit/q-narwhalknight/dist-fluxapp".into());
    match serve::start_with_api(&serve_dir, 9800, Some(local_api)) {
        Ok(stop) => {
            app.serve_stop = Some(stop);
            app.serve_status = "serve :9800 ✓ embedded".into();
            let _ = flux_register_scheme(); // register flux:// (best-effort, once)
        }
        Err(e) => {
            app.serve_status = format!("serve: {e}");
        }
    }

    let res = (|| -> std::io::Result<()> {
        loop {
            // ── LANE-Z: Windows conhost RENDER-HANG guard (the real "won't start / blank") ─────
            // Some Windows consoles (conhost / Windows Terminal) transiently answer the crossterm
            // terminal-size query with 0×0. ratatui then draws an EMPTY 0-size frame on EVERY
            // iteration = a BLACK screen spinning ~96% of a core that never paints the first frame
            // (screen-share-confirmed; reproduces on 0.70.0 too, so it is NOT the updater). While
            // the reported size is degenerate, DON'T draw a black frame: block on input for 100 ms
            // (so the loop can't busy-spin and a real resize/quit still gets through), then retry.
            // The plain-text `--once` / `render_full` path never enters this loop — which is exactly
            // why it always renders while `--tui` spun black.
            let size_now = crossterm::terminal::size();
            match size_now {
                Ok((w, h)) if w >= 2 && h >= 2 => {
                    if app.bad_size_streak != 0 {
                        boot_trace(&format!("terminal size recovered after {} bad reads: {w}x{h}", app.bad_size_streak));
                    }
                    app.bad_size_streak = 0;
                }
                other => {
                    app.bad_size_streak = app.bad_size_streak.saturating_add(1);
                    if app.bad_size_streak >= 20 {
                        if app.bad_size_streak == 20 {
                            let detail = format!("terminal size stayed degenerate for ~2s ({other:?}); forcing draw so failure is visible");
                            boot_trace(&detail);
                            log_line(format!("[render] {detail}"));
                        }
                    } else {
                    if event::poll(Duration::from_millis(100))? {
                        if let Event::Key(k) = event::read()? {
                            if k.kind == KeyEventKind::Press
                                && matches!(k.code, KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc)
                            {
                                return Ok(());
                            }
                        }
                    }
                    continue;
                    }
                }
            }
            if app.splash_until.map(|u| Instant::now() < u).unwrap_or(false) {
                app.splash_frame = app.splash_frame.wrapping_add(1);
            }
            // LANE-B v0.50: advance/schedule the rune animation + accumulate mining
            // history. Both are pure frame-state and never block the render loop.
            app.tick_rune();
            app.tick_mining_history();
            // v0.10.5: drain any completed async refresh (never blocks).
            app.poll_refresh();
            // v0.27 CRASH-PROOF: a panic inside rendering (bad slice, unwrap on odd data, etc.)
            // used to unwind out of run_tui and EXIT the app ("crashes after 20s"). Catch it —
            // the panic hook logs [PANIC] with file:line — re-init the terminal and keep running;
            // the next frame redraws. The monitor must never die on a single bad render frame.
            let mut frame_panicked = false;
            // [A]I OUT-OF-THE-BOX (2026-09-05): the tab must set itself up no matter HOW it
            // was reached. Before this the probe + auto-setup lived only in the `5` key arm,
            // so a user who cycled here with Tab (the route the header itself advertises)
            // sat on "press F5" for ever — and a phone keyboard has no F5 at all. Measured on
            // Viktor's Termux build: banner "no local model yet", empty transcript, every typed
            // question answered by the same hint. Latching on `ai_detected` keeps this a no-op
            // on every later frame.
            if app.tab == Tab::Ai { ai_enter_tab(&mut app); }
            term.draw(|f| {
                // Catch the panic AROUND draw_ui (the render code, which is the panic source) —
                // term.draw itself still owns the closure so there's no borrow-escape. A rende
                // panic leaves a partial frame (harmless; the next frame redraws) instead of
                // unwinding out of run_tui and killing the app.
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| draw_ui(f, &app))).is_err() {
                    frame_panicked = true;
                    log_line("[render] frame panicked — caught".into());
                }
                // v0.33 GLOBAL ASCII pass: on Windows (or SIGIL_ASCII=1) rewrite any remaining
                // wide/emoji cell symbol to width-1 ASCII. Done on the buffer AFTER layout but
                // BEFORE flush: ratatui already reserved 2 cells for a wide glyph, so emitting a
                // width-1 symbol makes the backend emit a corrective MoveTo for the next cell →
                // exact alignment on consoles that draw emoji at the wrong width. Catches every
                // glyph everywhere in one place (belt-and-suspenders with the source sa() wraps).
                if ui_ascii() {
                    let buf = f.buffer_mut();
                    for cell in buf.content.iter_mut() {
                        let sym = cell.symbol().to_string();
                        if !sym.is_ascii() {
                            let repl = sa(sym.as_str());
                            if repl != sym { cell.set_symbol(&repl); }
                        }
                    }
                }
            })?;
            if frame_panicked {
                app.render_panic_streak = app.render_panic_streak.saturating_add(1);
                let detail = last_panic_line();
                boot_trace(&format!(
                    "render panic streak {}/3: {detail}",
                    app.render_panic_streak
                ));
                if app.render_panic_streak >= 3 {
                    let _ = disable_raw_mode();
                    let _ = execute!(term.backend_mut(), LeaveAlternateScreen);
                    let _ = term.show_cursor();
                    IN_TUI.store(false, std::sync::atomic::Ordering::Relaxed);
                    eprintln!(
                        "sigil-top: render failed 3 frames in a row; exiting cleanly instead of black-screening.\n  {detail}\n  startup log: {}",
                        std::env::temp_dir().join("sigil-top-startup.log").display()
                    );
                    return Ok(());
                }
                continue;
            } else if app.render_panic_streak != 0 {
                boot_trace("render recovered after caught panic");
                app.render_panic_streak = 0;
            }
            if !app.first_frame_logged {
                match crossterm::terminal::size() {
                    Ok((w, h)) => boot_trace(&format!("first frame drawn {w}x{h}")),
                    Err(e) => boot_trace(&format!("first frame drawn (size unavailable: {e})")),
                }
                app.first_frame_logged = true;
            }
            // v0.10.5 "smooth cruise": adaptive frame pacing. When something is
            // moving — splash animation, an in-flight refresh, or live mining — poll
            // at ~30 fps so motion is buttery. When parked, fall back to a calm 200 ms
            // so an idle cockpit barely touches the CPU. Keys stay responsive either way.
            let animating = app.splash_until.map(|u| Instant::now() < u).unwrap_or(false)
                || app.refresh_inflight
                || app.mining
                || app.rune_active();   // LANE-B v0.50: smooth the rune band's draw-on/fade
            // v0.40: 33ms full redraws are cheap on a modern terminal but heavy on the
            // legacy CP437 conhost — halve the animating cadence on Windows so the
            // console host never becomes a load source itself.
            // (10.4.27–10.6.0 polled faster for the node band's moving belt; 10.6.1's SYNC card does not animate.)
            let poll_ms = if animating { if cfg!(windows) { 66 } else { 33 } } else { 200 };
            if event::poll(Duration::from_millis(poll_ms))? {
                if let Event::Key(k) = event::read()? {
                    if k.kind == KeyEventKind::Press {
                        // LANE-U v0.67: the first key dismisses the welcome modal (F also falls
                        // through to start sync/mining as usual).
                        if app.welcome_until.is_some() { app.welcome_until = None; }
                        match k.code {
                            // [A]I tab captures typing — these guarded arms MUST precede the
                            // global shortcuts so 'q'/Esc/digits type into the chat instead of
                            // quitting/switching. Tab still cycles out (falls through below).
                            KeyCode::Enter if app.tab == Tab::Ai => { ai_submit(&mut app); }
                            KeyCode::F(5) if app.tab == Tab::Ai => { ai_setup_start(&mut app); }   // re-run auto-setup
                            KeyCode::F(6) if app.tab == Tab::Ai => { ai_skills_start(&mut app); }  // reload signed skills
                            KeyCode::Backspace if app.tab == Tab::Ai => { app.ai_input.pop(); }
                            KeyCode::Esc if app.tab == Tab::Ai => { app.tab = Tab::Node; }
                            KeyCode::Char(c) if app.tab == Tab::Ai => { app.ai_input.push(c); }
                            KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => return Ok(()),
                            KeyCode::Char('r') | KeyCode::Char('R') => { app.refresh(); app.toast_sticky = false; }
                            // v0.13: tab switching — Tab cycles, 1/2/3 jump
                            KeyCode::Tab | KeyCode::BackTab => { app.tab = app.tab.next(); }
                            KeyCode::Char('1') => { app.tab = Tab::Node; }
                            KeyCode::Char('2') => { app.tab = Tab::SyncLog; }
                            KeyCode::Char('3') => { app.tab = Tab::Mining; }
                            KeyCode::Char('4') => { app.tab = Tab::Queues; }
                            KeyCode::Char('5') => { app.tab = Tab::Ai; ai_enter_tab(&mut app); }
                            // 2026-09-02: a second `Char('w')` arm used to sit here, added ahead
                            // of the real [W] handler below and pointing at the HOSTED wallet.
                            // Rust takes the first matching arm, so [W] silently became [L] and
                            // the embedded :9800 wallet (which now carries Nation › SIGIL OS too)
                            // was unreachable from the keyboard. [L] stays the hosted option.
                            KeyCode::Char('y') | KeyCode::Char('Y') => {
                                if !node_resync_key(&mut app) { app.resync(); }
                                app.toast_sticky = false;
                            }
                            KeyCode::Char('m') | KeyCode::Char('M') => { app.toggle_engine_mining(); }
                            KeyCode::Char('g') | KeyCode::Char('G') if app.tab == Tab::Mining => {
                                // [G] gates GPU use: GPU mining is OFF by default and only
                                // engages on an explicit opt-in here (and only on a -gpu
                                // build — the engine supervisor reverts the flag otherwise).
                                use std::sync::atomic::Ordering;
                                let now = app.mine_desired_gpu.load(Ordering::Relaxed);
                                if now {
                                    // turning OFF — always immediate.
                                    app.mine_desired_gpu.store(false, Ordering::Relaxed);
                                    app.toast = "⚙ engine → CPU".into();
                                } else if !cfg!(feature = "gpu") {
                                    // CPU-only build: flip the flag so the engine logs the
                                    // "rebuild with --features gpu" hint, but it never runs GPU.
                                    app.mine_desired_gpu.store(true, Ordering::Relaxed);
                                    app.toast = "⚙ engine → GPU (needs a -gpu build)".into();
                                } else if !app.mine_gpu_warned {
                                    // First-ever GPU opt-in on a -gpu build: WARN and require a
                                    // second [G] to actually enable (don't enable yet). GPU
                                    // mining can overheat a laptop; the guard auto-throttles
                                    // ≥78C and drops to CPU ≥82C, but it needs nvidia-smi.
                                    app.mine_gpu_warned = true;
                                    app.toast = "⚠ GPU mining can overheat a laptop. Guard auto-throttles ≥78C, drops to CPU ≥82C (needs nvidia-smi; NVIDIA only). Press [G] again to enable GPU.".into();
                                } else {
                                    app.mine_desired_gpu.store(true, Ordering::Relaxed);
                                    app.toast = "⚙ engine → GPU".into();
                                }
                                app.toast_sticky = true;
                            }
                            KeyCode::Char('f') | KeyCode::Char('F') => {
                                // 10.6.5 (Viktor: light mode for users who only mine): [F] switches your full node
                                // off and on; the choice is remembered. (10.6.0–10.6.4 only said "nothing to switch".)
                                if cfg!(feature = "producer") {
                                    app.toast = node_mode_key();
                                    app.toast_sticky = false;
                                } else
                                // 0.77 GENESIS ARCHIVE (#156): [F] is the explicit "hold the whole
                                // chain" switch. No engine → start it in FULL-ARCHIVE mode (base
                                // anchored at genesis, every block held — the redundant-backup
                                // promise; the old code launched recent_only=true and the toggle
                                // never reached the engine). Engine running → LIVE-FLIP it between
                                // full-archive and light-monitor through the engine atomics.
                                // Light-monitor's 10ms tip-verify stays the untouched startup default.
                                if app.p2p_sync.is_none() {
                                    // BUG FIX (Viktor report, "sync doesnt work", v7.1.17):
                                    // take_idle_store() ALREADY sets an accurate toast on
                                    // both its None outcomes — "still opening (compaction)
                                    // — press again in a moment" when the background opener
                                    // thread just hasn't finished yet (the common case: a
                                    // fresh launch, pressing F within the first few seconds,
                                    // uptime 0m19s in the reported repro), or "unavailable —
                                    // see ~/.sigil-top.log" when the opener genuinely failed.
                                    // This branch used to UNCONDITIONALLY overwrite that with
                                    // "restart with --sync" — wrong and misleading in the
                                    // (much more common) transient case: the store was never
                                    // unavailable, it just wasn't ready yet, and --sync isn't
                                    // even required (F starts it on demand by design — see the
                                    // idle_store_rx parking a few hundred lines up). Do NOT set
                                    // a toast here; let take_idle_store's own message stand.
                                    if let Some(store) = app.take_idle_store() {
                                        let p2p = block_sync::P2PBlockSync::launch(store, false);
                                        app.p2p_sync = Some(p2p);
                                        app.full_sync = true;
                                        persist_sync_mode("full"); // LANE-V: survives update/restart
                                        app.toast = full_node_toast("⛓ FULL ARCHIVE started — syncing genesis→tip, holding every block (~1GB). F again = light monitor.");
                                    }
                                    app.toast_sticky = true;
                                } else if let Some(ref p2p) = app.p2p_sync {
                                    if p2p.is_recent_only() {
                                        p2p.set_full_archive();
                                        app.full_sync = true;
                                        persist_sync_mode("full");
                                        app.toast = full_node_toast("⛓ FULL ARCHIVE — base → genesis, downloading + holding every block (~1GB). Press F for light monitor.");
                                    } else {
                                        p2p.set_light_monitor();
                                        app.full_sync = false;
                                        persist_sync_mode("light");
                                        app.toast = light_monitor_toast();
                                    }
                                    app.toast_sticky = false;
                                }
                            }
                            KeyCode::Char('v') | KeyCode::Char('V') => {
                                app.toast = match app.st.tip.as_ref() {
                                    Some(t) => { let v = verify_tip(t); let m = if v.ok { format!("✓ tip {} verified in {} µs · 0 blocks", v.height, v.latency_us) } else { format!("✗ verify failed: {}", v.err.clone().unwrap_or_default()) }; app.verify = Some(v); m }
                                    None => "no tip published by node — nothing to verify".into(),
                                };
                                app.toast_sticky = false;
                            }
                            KeyCode::Char('u') | KeyCode::Char('U') => {
                                // Flux-way self-update on a BACKGROUND thread so the 11 MB
                                // download never freezes the TUI (the "intet" bug). The result
                                // string lands on update_rx and is shown on the next draw.
                                if app.update_rx.is_some() {
                                    app.toast = "↓ update already running…".into();
                                    app.toast_sticky = false;
                                } else {
                                    app.toast = "↓ checking flux release channel…".into();
                                    app.toast_sticky = false;
                                    let (tx, rx) = mpsc::channel();
                                    thread::spawn(move || {
                                        let msg = match fetch_latest() {
                                            Ok(rel) if version_gt(&rel.version, VERSION) => match self_update(&rel) {
                                                Ok(m) => m,
                                                Err(e) => format!(
                                                    "update v{} failed: {e} — fallback: wget quillon.xyz/downloads/sigil-top-v{}-{SELF_TARGET}",
                                                    rel.version, rel.version),
                                            },
                                            Ok(rel) => release_channel_current_msg(&rel.version),
                                            Err(e) => format!("⚠ update check: {e}"),
                                        };
                                        let _ = tx.send(msg);
                                    });
                                    app.update_rx = Some(rx);
                                }
                            }
                            KeyCode::Char('l') | KeyCode::Char('L') => {
                                // v7.1.30: [L]/[W] now open the HOSTED React/Vite/TS wallet
                                // (official_wallet_url(), the app with the live Swap page)
                                // directly — no dependency on the local :9800 embedded server
                                // or the block store being open. The old local-server path
                                // opened the single-file Tron wallet instead; that page still
                                // exists (served at :9800 for headless/offline use and by [T]/
                                // [B]'s #stats/#activity deep links, which it alone supports)
                                // but is no longer what [L]/[W] launch by default.
                                let url = official_wallet_url();
                                if open_browser_private(&url) {
                                    app.toast = format!("🌐 wallet (private window) → {url}").into();
                                    app.toast_sticky = false;
                                } else {
                                    app.toast = format!("🔗 headless — open the wallet in any browser:  {url}").into();
                                    app.toast_sticky = true;
                                }
                            }
                            KeyCode::Char('d') | KeyCode::Char('D') => {
                                // v0.3.1: fetch the real _sigil-tip DNS anchor via DoH,
                                // parse + structural-validate with sigil-dns-anchor crate.
                                app.toast = "↓ DNS anchor: fetching _sigil-tip.sigilgraph.quillon.xyz…".into();
                                app.toast_sticky = false;
                                let (tx, rx) = mpsc::channel();
                                thread::spawn(move || {
                                    let msg = fetch_dns_anchor();
                                    let _ = tx.send(msg);
                                });
                                app.update_rx = Some(rx);
                            }
                            KeyCode::Char('w') | KeyCode::Char('W') => {
                                // [W] is the node-local wallet: bring up the embedded server
                                // first, then open its compiled-in UI. Keep this URL exact so
                                // SIGIL_WALLET_URL/FLUX_WALLET_URL cannot redirect the shortcut
                                // to the hosted wallet; [L] remains the explicit hosted option.
                                //
                                // 2026-08-25: was missing `?addr=`, unlike the headless fallback
                                // below (headless_wallet_view_url) which already does this. The
                                // page's ADDR (the header "yours" pill, driven by ?addr= or else
                                // localStorage['sigil-wallet-address'] from an earlier, unrelated
                                // onboarding visit) is a SEPARATE identity from MINE_ADDR (the
                                // balance number, seed-derived, already correct). Without ?addr=
                                // here, a browser tab with any prior onboarding history keeps
                                // showing that stale/unrelated address next to the correct
                                // balance — "the balance is right but it's a different wallet."
                                // Passing the real mining address explicitly overrides whatever
                                // is cached, same as the headless link already does.
                                let url = format!("http://localhost:{}/?addr={}", wallet_ui::WALLET_PORT, miner_wallet());
                                if !ensure_serve_up(&mut app) {
                                    app.toast = format!("✗ local wallet server down ({})", app.serve_status).into();
                                    app.toast_sticky = true;
                                } else if open_browser_private(&url) {
                                    // 10.4.39: show `localhost:9800/w` when this server answers it —
                                    // the full link (~110 characters) was cut at the terminal's edge
                                    // (operator, 2026-10-18). The browser still gets the full URL.
                                    app.toast = format!("◆ local wallet → {}", wallet_ui::shown_wallet_link(&url)).into();
                                    app.toast_sticky = false;
                                } else {
                                    // Headless (SSH/terminal-only): localhost:9800 only means
                                    // something on THIS box, so handing it out is a dead link on
                                    // any other device. Give a link that actually works elsewhere —
                                    // see wallet_ui::headless_wallet_view_url's doc comment for why
                                    // this carries no secret and is safe to print/copy anywhere.
                                    //
                                    // 2026-08-26: that remote link is seedless by design, so it
                                    // still prompts for the recovery phrase on every Send — the
                                    // zero-prompt server (SIGIL_MINE_SEED-backed :9800) never went
                                    // away, headless just never pointed at it. Offer the tunnel
                                    // that actually reaches it, alongside the remote fallback.
                                    let fallback = wallet_ui::headless_wallet_view_url();
                                    app.toast = match wallet_ui::ssh_tunnel_hint(&wallet_ui::shown_wallet_link(&url)) {
                                        Some(hint) => format!("🔐 {hint}\n🔗 or, prompted: {fallback}").into(),
                                        None => format!("🔗 headless — open your wallet from any device:  {fallback}").into(),
                                    };
                                    app.toast_sticky = true;
                                }
                            }
                            KeyCode::Char('t') | KeyCode::Char('T') => {
                                // Network-stats modal in the tron wallet UI — the SIGIL
                                // equivalent of the Quillon graph dashboard stats. The
                                // #stats deep-link auto-opens the modal on load; data is
                                // same-origin via the :9800 /api proxy + feed JSON.
                                let url = format!("{}#stats", local_wallet_url());
                                if !ensure_serve_up(&mut app) {
                                    app.toast = format!("✗ local wallet server down ({}) — stats need the :9800 server", app.serve_status).into();
                                    app.toast_sticky = true;
                                } else if open_browser_private(&url) {
                                    app.toast = format!("📊 network stats → {url}").into();
                                    app.toast_sticky = false;
                                } else {
                                    app.toast = format!("🔗 headless — open the stats modal in any browser:  {}#stats", official_wallet_url()).into();
                                    app.toast_sticky = true;
                                }
                            }
                            KeyCode::Char('b') | KeyCode::Char('B') => {
                                // Recent activity — the Explorer/Activity view INSIDE the
                                // local wallet, deep-linked via #activity and served from
                                // this node's own :9800. Was: the public fluxapp.xyz
                                // explorer, which is a different origin and shows the
                                // PUBLIC node's chain — so on a local/private node [B]
                                // showed someone else's blocks. Same pattern as [T].
                                let url = format!("{}#activity", local_wallet_url());
                                if !ensure_serve_up(&mut app) {
                                    app.toast = format!("✗ local wallet server down ({}) — recent activity needs the :9800 server", app.serve_status).into();
                                    app.toast_sticky = true;
                                } else if open_browser_private(&url) {
                                    app.toast = format!("📜 recent activity → {url}").into();
                                    app.toast_sticky = false;
                                } else {
                                    app.toast = format!("🔗 headless — open recent activity in any browser:  {}#activity", official_wallet_url()).into();
                                    app.toast_sticky = true;
                                }
                            }
                            KeyCode::Char('s') | KeyCode::Char('S') => {
                                let url = "https://sigilgraph.fluxapp.xyz/sigil-top/";
                                if open_browser(url) {
                                    app.toast = "🌐 Cockpit opened in browser".into();
                                    app.toast_sticky = false;
                                } else {
                                    app.toast = format!("🔗 headless — open the cockpit in any browser:  {url}").into();
                                    app.toast_sticky = true;
                                }
                            }
                            // v0.6.0: Cortex MCP combo verbs
                            // (2026-09-17: the [C]ortex / [A]udit / [H]eal arms are gone — the last two
                            // printed hardcoded "✓ … complete" results without running anything.)
                            // v0.7.0: Fleet health check — AIs monitor their node fleet
                            KeyCode::Char('n') | KeyCode::Char('N') => {
                                app.fleet_last_check = Instant::now();
                                check_fleet_health(&mut app.fleet_nodes);
                                let online = app.fleet_nodes.iter().filter(|n| n.online).count();
                                let total = app.fleet_nodes.len();
                                app.toast = format!("⚓ Fleet check: {}/{} nodes online", online, total);
                                app.toast_sticky = false;
                            }
                            _ => {}
                        }
                    }
                }
            }
            // v0.10.5.1: adaptive cadence — fast when online, gentle backoff when offline.
            if !app.refresh_inflight && app.last_fetch.elapsed() >= app.refresh_delay() {
                app.request_refresh();
            }
            // v0.13: keep the Swarm AI / Results board live (2s) while it's on screen.
            // v7.0.13: Swarm AI / Results tabs removed — no periodic swarm reload needed.
            // v0.10.5.1: embedded-serve watchdog — if the :9800 wallet server died,
            // restart it so the local wallet/[W] never silently goes dark. Probe is
            // throttled to 15s and only blocks (briefly) in the rare dead case.
            // v7.1.10: NO `serve_stop.is_some()` gate — that gate meant a failed
            // INITIAL start could never be retried, leaving :9800 dark forever.
            if app.last_serve_check.elapsed() >= Duration::from_secs(15) {
                app.last_serve_check = Instant::now();
                if !wallet_server_alive() {
                    ensure_serve_up(&mut app);
                }
            }
            // v0.6.5: Poll P2P sync state + drain synced blocks into the TUI block list
            if let Some(ref p2p) = app.p2p_sync {
                // 0.77: poll_state is try_lock — None = the sync thread holds the lock right
                // now (heavy ingest/flush). Keep rendering the previous clone; never block
                // the draw thread (the Windows full-archive BLACK SCREEN, #156 item 2).
                if let Some(s) = p2p.poll_state() { app.p2p_state = s; }
                record_spine_sample(&mut app.spine_samples, std::time::Instant::now(), app.p2p_state.verified);
                // v0.13.1: feed the HTTP status height into the P2P backfill so the
                // refill starts requesting chunks even when gossip/probe are silent —
                // fixes the sync sitting forever on "connecting" with peer_best=0.
                p2p.set_known_tip(app.target_height);
                // Backfill rate (blocks/s) = 10s TRAILING window — the CURRENT speed, not
                // a lifetime average. A cumulative avg decayed after the catch-up burst
                // (huge start → ~production rate → looked like it was "slowing to 19/s").
                // The 10s window absorbs the per-cycle chunk bursts yet tracks real speed.
                // v0.23: rate = advance of the SYNC HEIGHT (blocks_synced), not the contiguous
                // download counter (fetched_total). For a light monitor blocks_synced tracks the
                // verified live tip (peer_best), so the rate shows the real network block rate
                // (~prod rate) and reads >0 even when gossip mesh isn't grafted / backfill parks
                // on the gappy head — the "0 blk/s" cause. (full-sync: blocks_synced = contiguous,
                // so it still shows true download speed.)
                let now = std::time::Instant::now();
                let rate_metric = app.p2p_state.blocks_synced.max(app.p2p_state.fetched_total);
                // v0.31: a one-time CATCH-UP jump (synced snaps 0→~tip the instant the monito
                // reaches the head, or after a re-snap over a big gap) is NOT a sustained rate —
                // it spiked the readout to ~1M blk/s then craters to 0 as the jump scrolls out of
                // the 10s window (the "1M → 0" the user saw). If the metric leaps > CATCHUP_JUMP in
                // one sample, reset the window so the rate reflects STEADY tip advance, never the
                // snap. chronos confirmed delivery is fine — this was purely a display artifact.
                const CATCHUP_JUMP: u64 = 50_000;
                if let Some(&(_, last_b)) = app.p2p_rate_samples.back() {
                    if rate_metric.saturating_sub(last_b) > CATCHUP_JUMP {
                        app.p2p_rate_samples.clear();
                    }
                }
                app.p2p_rate_samples.push_back((now, rate_metric));
                // v7.0.22: 10s → 30s trailing window. The storage engine legitimately pauses
                // apply for a few seconds (memtable flush / bulk-mode compaction settle); a
                // 10s window collapsed the readout to ~0 on every pause, which reads as
                // FAULTY even though fetch continues and the blocks land in a burst right
                // after. Over 30s a 3-5s breath is a ~10-15% dip — visible, honest, calm.
                // A REAL stall still decays to 0 (and the STALLED/WEDGE headline + watchdog
                // name it loudly long before the window empties).
                while app.p2p_rate_samples.len() > 1
                    && now.duration_since(app.p2p_rate_samples[0].0).as_secs_f64() > 30.0
                {
                    app.p2p_rate_samples.pop_front();
                }
                if let (Some(&(t0, b0)), Some(&(t1, b1))) =
                    (app.p2p_rate_samples.front(), app.p2p_rate_samples.back())
                {
                    let dt = t1.duration_since(t0).as_secs_f64();
                    if dt >= 1.0 {
                        let long_rate = b1.saturating_sub(b0) as f64 / dt;

                        // REGIME CHANGE, DOWNWARD (2026-08-27). `CATCHUP_JUMP` above already
                        // resets the window when the metric LEAPS UP, so an instantaneous
                        // catch-up snap cannot masquerade as a sustained rate. The mirror case
                        // had no such guard, and it is the one an operator actually stares at:
                        // bulk import runs at thousands of blk/s, then the sync reaches the
                        // frontier and drops to the network's real ~10 blk/s. Both effects then
                        // conspire to keep the old number on screen:
                        //
                        //   1. the 30 s trailing window still spans mostly BULK samples, so the
                        //      measurement itself stays high for a further 30 s; and
                        //   2. the Kalman gain (0.167 steady-state) needs ~34 more samples to
                        //      cross a 400x drop.
                        //
                        // Reported live at 98.6% synced: "4.6k blk/s even though its more down
                        // to 10". The panel was averaging across a boundary where averaging has
                        // no meaning — those are two different processes, not one noisy one.
                        //
                        // So: measure a SHORT trailing window too, and when it disagrees with
                        // the long one by a wide margin, believe the recent past. Discard the
                        // stale bulk samples and snap the filter rather than slewing.
                        const SHORT_WINDOW_SECS: f64 = 5.0;
                        let short_rate = app
                            .p2p_rate_samples
                            .iter()
                            .rev()
                            .find(|(t, _)| t1.duration_since(*t).as_secs_f64() >= SHORT_WINDOW_SECS)
                            .and_then(|&(ts, bs)| {
                                let sdt = t1.duration_since(ts).as_secs_f64();
                                (sdt >= 1.0).then(|| b1.saturating_sub(bs) as f64 / sdt)
                            });

                        // Only a COLLAPSE counts, and only a large one: a quarter of the long
                        // rate, and at least 100 blk/s of absolute difference so ordinary
                        // frontier jitter (12 -> 4 blk/s) never trips it. Those small moves are
                        // exactly what the filter is good at and should keep handling.
                        let collapsed = matches!(short_rate, Some(sr)
                            if long_rate > 0.0 && sr < long_rate * 0.25 && long_rate - sr > 100.0);

                        if let (true, Some(sr)) = (collapsed, short_rate) {
                            while app.p2p_rate_samples.len() > 1
                                && now
                                    .duration_since(app.p2p_rate_samples[0].0)
                                    .as_secs_f64()
                                    > SHORT_WINDOW_SECS
                            {
                                app.p2p_rate_samples.pop_front();
                            }
                            app.p2p_rate = sr;
                            app.sync_kf.reset_to(sr);
                        } else {
                            app.p2p_rate = long_rate;
                            // v0.33.3: feed the measured rate into the Kalman filter for a stable ETA.
                            app.sync_kf.update(app.p2p_rate);
                        }
                    }
                }
                let mut drained_any = false;
                for block in p2p.drain_new_blocks() {
                    drained_any = true;
                    app.p2p_blocks_synced += 1;
                    // Also feed into the block stream display
                    let fb = FeedBlock {
                        height: block.header.height,
                        hash: block.hash_hex.clone(),
                        producer: String::new(),
                        txs: 0,
                        tip_ms: 0,
                    };
                    app.blocks.push(fb);
                }
                // Newest first, one row per height, bounded. Appending gossip blocks at the END and only
                // sorting past 500 froze the stream panel on the FIRST 14 blocks received.
                if drained_any || app.blocks.len() > 500 { merge_block_stream(&mut app.blocks, 500); }
            }
            // v0.7.0: Fleet health check every 60s — AIs worry about their nodes
            if app.fleet_last_check.elapsed() >= Duration::from_secs(60) {
                app.fleet_last_check = Instant::now();
                check_fleet_health(&mut app.fleet_nodes);
            }
            // [A]I: the local model's reply lands here (computed off the UI thread)
            if app.ai_rx.is_some() { ai_drain(&mut app); }
            mining_relay::set_node_verified(app.verify.as_ref().map(|v| v.ok).unwrap_or(false), app.target_height);
            // [A]I auto-setup progress (install / pull) — drain everything pending this frame
            if let Some(rx) = app.ai_setup_rx.as_ref() {
                let mut finished = false;
                while let Ok(ev) = rx.try_recv() {
                    match ev {
                        ai_setup::SetupEvent::Line(l) => app.ai_msgs.push(("assistant".to_string(), l)),
                        ai_setup::SetupEvent::Done { model } => {
                            app.ai_models = crate::flux_moe::list_models();
                            app.ai_model = Some(model.clone());
                            app.ai_msgs.push(("assistant".to_string(), format!("✓ flux-moe ready on {model} — ask me about mining, the wallet, the Nation, or the node")));
                            finished = true;
                        }
                        ai_setup::SetupEvent::Fail(e) => {
                            app.ai_msgs.push(("assistant".to_string(), format!("⚠ auto-setup stopped: {e}\n  press F5 to retry")));
                            finished = true;
                        }
                    }
                }
                if finished { app.ai_setup_rx = None; app.ai_setup_running = false; }
            }
            // [A]I flux-signed skills — load result (rejections are shown, never silently dropped)
            if let Some(rx) = app.ai_skills_rx.as_ref() {
                if let Ok(res) = rx.try_recv() {
                    match res {
                        Ok(res) => {
                            let names: Vec<String> = res.loaded.iter().map(|s| s.name.clone()).collect();
                            app.ai_skills_note = res.note();
                            app.ai_skills = res.loaded;
                            if !names.is_empty() {
                                app.ai_msgs.push(("assistant".to_string(), format!("🔏 skills loaded (flux-signed, blake3-verified): {}", names.join(", "))));
                            }
                            // Withheld is routine — a skill that simply is not for
                            // this install. Say it once, quietly, and only if the
                            // user could actually turn it on.
                            for w in res.withheld { app.ai_msgs.push(("assistant".to_string(), format!("· skill not loaded here — {w}"))); }
                            // Rejected means a gate FAILED. Loud, always.
                            for r in res.rejected { app.ai_msgs.push(("assistant".to_string(), format!("⚠ skill rejected — {r}"))); }
                        }
                        Err(e) => {
                            app.ai_skills_note = "skills unavailable".into();
                            app.ai_msgs.push(("assistant".to_string(), format!("⚠ skills: {e}")));
                        }
                    }
                    app.ai_skills_rx = None;
                }
            }
            // v0.7.0: Embedded serve is a thread — no health check needed
            // background self-update result (if any) → toast
            if let Some(rx) = app.update_rx.as_ref() {
                match rx.try_recv() {
                    Ok(msg) => {
                        // v0.7.5: Silent auto-check — just update the banner version
                        if let Some(ver) = msg.strip_prefix("AUTO-CHECK:") {
                            if !ver.is_empty() { app.latest = ver.to_string(); }
                            app.update_rx = None;
                        } else if let Some(e) = msg.strip_prefix("AUTO-CHECK-ERR:") {
                            app.toast = format!("⚠ update channel: {e}");
                            app.toast_sticky = false;
                            app.update_rx = None;
                        } else {
                        // v0.7.0: Auto-restart after ANY successful update (swapped, saved, or downloaded).
                        // The user pressed [U] to upgrade — they expect to be running the new version.
                        // We re-exec the new binary immediately so the fleet stays current without
                        // manual intervention. AI fleet operators depend on this.
                        // ONLY relaunch on a REAL update. The old gate also matched
                        // "✓ up to date" (the already-current message) → it relaunched when
                        // NOTHING changed, and on exec-failure the spawn-fallback detached a
                        // TUI child that fought the terminal = the "animation appears then it
                        // crashes/exits" bug. Now: relaunch only on swap/save, and if the
                        // relaunch fails, restore the TUI instead of crashing.
                        // Windows lock-fallback ("staged …"): a detached helper applies the swap
                        // and relaunches AFTER we exit — so leave the TUI cleanly and exit now
                        // (relaunching ourselves would pre-flight the still-OLD install path).
                        if msg.starts_with("staged v") {
                            let _ = disable_raw_mode();
                            let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
                            println!("\n  {msg}\n  applying update — relaunching…");
                            std::process::exit(0);
                        }
                        let is_update_ok = msg.contains("swapped") || msg.contains("saved v");
                        let mut relaunch_failed = false;
                        if is_update_ok {
                            let _ = disable_raw_mode();
                            let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
                            if !relaunch_new_binary(&app.latest) {
                                // relaunch failed — re-enter the TUI, don't crash out
                                let _ = enable_raw_mode();
                                let _ = execute!(std::io::stdout(), EnterAlternateScreen);
                                relaunch_failed = true;
                            }
                        }
                        // LANE-O (b): do NOT clobber a relaunch-FAILURE toast with the self_update
                        // "restart to run" msg — that hid WHY the restart didn't happen. On failure
                        // keep a STICKY honest message; only show msg when the handoff succeeded
                        // (unix exec never returns here) or nothing was swapped.
                        if relaunch_failed {
                            app.toast = format!("↑ {msg} — but AUTO-RESTART FAILED; please quit and relaunch sigil-top manually");
                            app.toast_sticky = true;
                        } else {
                            app.toast = msg; app.toast_sticky = false;
                        }
                        app.update_rx = None;
                        } // end else (non-auto-check message)
                    }
                    Err(mpsc::TryRecvError::Disconnected) => { app.update_rx = None; }
                    Err(mpsc::TryRecvError::Empty) => {}
                }
            }
            // v0.37: mirror the in-process dual-lane engine into the Node hero legacy fields.
            if app.mining {
                if let Ok(es) = app.mine_stats.try_lock() {
                    app.mine_hashrate = es.hashrate / 1_000_000.0;
                    app.mine_accepted = es.shares_ok;
                }
            }
            // Drain live mining progress (accepted shares + hashrate) onto the toast + counter.
            // v0.2.35: mining noise never overwrites sticky user-action toasts ([U], [V], [L], [M]).
            if let Some(rx) = app.mine_rx.as_ref() {
                while let Ok(msg) = rx.try_recv() {
                    if msg.starts_with("✓ share") { app.mine_accepted += 1; app.score += 50; }
                    // v0.2.35: parse hashrate messages: "⛏ 12.34 MH/s · 5M hashes"
                    if msg.starts_with("⛏ ") {
                        if let Some(rate_part) = msg.strip_prefix("⛏ ").and_then(|s| s.split(" MH/s").next()) {
                            if let Ok(rate) = rate_part.parse::<f64>() {
                                app.mine_hashrate = rate;
                            }
                        }
                        if let Some(hash_part) = msg.split("· ").nth(1).and_then(|s| s.split('M').next()) {
                            if let Ok(mega) = hash_part.parse::<f64>() {
                                app.mine_hashes = (mega * 1_000_000.0) as u64;
                            }
                        }
                    }
                    if !app.toast_sticky { app.toast = msg; }
                }
            }
        }
    })();

    // v0.7.0: Signal embedded serve to stop
    if let Some(stop) = app.serve_stop.take() {
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    term.show_cursor()?;
    IN_TUI.store(false, std::sync::atomic::Ordering::Relaxed);
    if let Ok(mut g) = STDERR_REDIRECT.lock() {
        if let Some(r) = g.take() { platform::restore_stderr(&r); }
    }
    res
}

/// The stderr → logfile redirect installed for the TUI's lifetime (see `platform.rs`).
pub(crate) static STDERR_REDIRECT: std::sync::Mutex<Option<platform::StderrRedirect>> = std::sync::Mutex::new(None);



/// v0.6.5: Create a flux-rev content-addressed snapshot of the workspace.
/// Uses flux-rev (the git replacement) to create a BLAKE3-hashed manifest
/// that can be synced over flux-p2p to other compile servers.
pub(crate) fn rev_snapshot(ws_root: &std::path::Path) -> Result<String, String> {
    // Store::open creates .flux-rev inside ws_root automatically
    let store = Store::open(ws_root)
        .map_err(|e| format!("flux-rev store: {e}"))?;
    let genesis_id = "sigil-top-genesis-0";
    let rev = snapshot(
        ws_root,
        &store,
        None,                      // parent
        genesis_id,
        VERSION,                   // workspace_version
        "sigil-top-cortex",        // autho
        &format!("sigil-top v{VERSION} cortex auto-snapshot"),
    ).map_err(|e| format!("flux-rev snapshot: {e}"))?;
    Ok(rev.id)
}


/// v0.6.0: Cortex loop result for the TUI
pub(crate) fn render_update_splash(frame: u8) -> Paragraph<'static> {
    const FRAMES: [&str; 8] = [
        "    ◆─────────◆",
        "   ╱◆─────────◆╲",
        "  ╱ ╲◆───────◆╱ ╲",
        " │   ◆───────◆   │",
        "  ╲ ╱◆───────◆╲ ╱",
        "   ╲◆─────────◆╱",
        "    ◇─────────◇",
        "     ╲───────╱",
    ];
    let ring = FRAMES[frame as usize % FRAMES.len()];
    let spin = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"][frame as usize % 8];
    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(ring, Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD))),
        Line::from(""),
        Line::from(vec![
            Span::styled(format!(" {spin} "), Style::default().fg(C_GOLD)),
            Span::styled("SIGIL", Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD)),
            Span::styled(format!(" · v{VERSION}"), Style::default().fg(C_GOLD)),
        ]),
        Line::from(""),
        Line::from(Span::styled("  flux channel synced — BLAKE3 verified", Style::default().fg(C_CYAN))),
        Line::from(Span::styled("  restarting lightweight node…", Style::default().fg(C_DIM))),
        Line::from(""),
        Line::from(Span::styled("  ████████████████████░░░░  updating", Style::default().fg(C_GOLD))),
    ];
    Paragraph::new(lines)
        .alignment(ratatui::layout::Alignment::Center)
        .style(Style::default().bg(Color::Rgb(5, 5, 15)))
}


// ── v0.13: tabbed cockpit — Node dashboard + MCP Swarm AI job board + Results ──

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Tab { Node, SyncLog, Mining, Queues, Ai }

impl Tab {
    pub(crate) fn next(self) -> Tab {
        match self {
            Tab::Node => Tab::SyncLog,
            Tab::SyncLog => Tab::Mining,
            Tab::Mining => Tab::Queues,
            Tab::Queues => Tab::Ai,
            Tab::Ai => Tab::Node,
        }
    }
}

/// [A]I: send the typed line to the local model on a BACKGROUND thread; the reply
/// lands on `app.ai_rx` (drained in the render loop) so a slow model never freezes
/// the dashboard. With no model chosen, answer with the setup hint instead.
pub(crate) fn ai_submit(app: &mut App) {
    let msg = app.ai_input.trim().to_string();
    if msg.is_empty() { return; }
    // A reply is still streaming: replacing `ai_rx` now would DROP it (the old bug — "it
    // missed my prompt"). Queue the message instead; `ai_drain` sends it when this one ends.
    if app.ai_thinking {
        app.ai_input.clear();
        app.ai_queued.push_back(msg);
        return;
    }
    app.ai_input.clear();
    app.ai_msgs.push(("user".to_string(), msg.clone()));
    // An ACTION the model asked for waits for exactly this: the user's own `yes`.
    if let Some((name, _args)) = app.ai_pending.take() {
        let yes = matches!(msg.trim().to_ascii_lowercase().as_str(), "yes" | "y" | "ja" | "j" | "ok");
        let reply = if !yes { format!("Cancelled — I did not {}.", ai_tools::action_summary(&name)) } else { ai_run_action(app, &name) };
        app.ai_msgs.push(("assistant".to_string(), reply));
        return;
    }
    // Typed setup commands: `setup` / `install` / `retry` / `f5`. A phone or a terminal
    // that swallows function keys has no F5, so the keypress can never be the ONLY way to
    // (re)run the out-of-the-box flow.
    // `privacy` / `egress` / `veil`: what (if anything) this AI has sent off the machine.
    if flux_moe::is_privacy_command(&msg) {
        app.ai_msgs.push(("assistant".to_string(), flux_moe::privacy_report()));
        return;
    }
    if flux_moe::is_setup_command(&msg) {
        if app.ai_setup_running {
            app.ai_msgs.push(("assistant".to_string(), "Auto-setup is already running — progress is above.".to_string()));
        } else {
            app.ai_msgs.push(("assistant".to_string(), "Re-running auto-setup…".to_string()));
            ai_setup_start(app);
        }
        return;
    }
    let model = match app.ai_model.clone() {
        Some(m) => m,
        None => {
            // No model yet. Do not hand the user a chore ("press F5") — start the setup
            // for them, once, and tell them what is happening. If it is already running,
            // say so instead of restarting it.
            if app.ai_setup_running {
                app.ai_msgs.push(("assistant".to_string(),
                    "Your local AI is still being set up — progress is above. Ask again when it says ✓ ready.".to_string()));
            } else {
                app.ai_msgs.push(("assistant".to_string(), flux_moe::setup_hint().to_string()));
                ai_setup_start(app);
            }
            return;
        }
    };
    let history: Vec<(String, String)> = app.ai_msgs[..app.ai_msgs.len().saturating_sub(1)]
        .iter().filter(|(r, c)| r != "tool" && !c.is_empty()).cloned().collect();
    let api_base = kgauge_ui::api_base_of(&app.cfg.api);
    let extra = skills::context_block(&app.ai_skills); // flux-signed skills → system context
    let (tx, rx) = mpsc::channel();
    app.ai_rx = Some(rx);
    app.ai_thinking = true;
    app.ai_started = Some(Instant::now());
    app.ai_first_token = None;
    app.ai_think_chars = 0;
    app.ai_tokens = 0;
    // The streamed answer grows in place inside this (initially empty) assistant message.
    app.ai_msgs.push(("assistant".to_string(), String::new()));
    app.ai_reply_idx = Some(app.ai_msgs.len() - 1);
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    app.ai_load_stop = Some(stop.clone());
    let slot = app.ai_load.clone();
    std::thread::spawn(move || flux_moe::sample_load(slot, stop));
    std::thread::spawn(move || ai_tools::chat_agent(&model, &history, &msg, &extra, &api_base, &tx));
}

/// Run an ACTION the user just confirmed with `yes`. Never reachable from the model alone.
pub(crate) fn ai_run_action(app: &mut App, name: &str) -> String {
    match name {
        "sigil_create_wallet" => match ai_tools::create_wallet() {
            Ok((addr, path)) => format!("✓ New wallet created:\n  {addr}\n\nIts secret seed is saved to {path} (owner-only). Back that file up — it is the wallet. The AI never saw it.\nThis node now mines to this wallet. Say \"start mining\" when you're ready."),
            Err(e) => format!("⚠ {e}"),
        },
        "sigil_start_mining" => {
            if app.mining { return "Already mining — see tab 3.".into(); }
            if miner_keypair().is_none() { return "⚠ This node has no wallet yet, and mining needs one it can sign for. Ask me to create a wallet first.".into(); }
            app.toggle_engine_mining();
            format!("✓ Mining started to {} — the Mining tab shows hashrate and shares.", miner_wallet())
        }
        "sigil_stop_mining" => {
            if !app.mining { return "Not mining right now.".into(); }
            app.toggle_engine_mining();
            "✓ Mining stopped.".into()
        }
        other => format!("⚠ unknown action {other}"),
    }
}

/// Drain every streamed event that arrived since the last frame into the transcript.
pub(crate) fn ai_drain(app: &mut App) {
    let Some(rx) = app.ai_rx.take() else { return };
    let idx = app.ai_reply_idx.filter(|&i| i < app.ai_msgs.len()).unwrap_or(app.ai_msgs.len().saturating_sub(1));
    let mut idx = idx;
    let mut finished = false;
    while let Ok(ev) = rx.try_recv() {
        match ev {
            flux_moe::StreamEv::Thinking(t) => app.ai_think_chars += t.chars().count(),
            flux_moe::StreamEv::Token(t) => {
                if app.ai_first_token.is_none() { app.ai_first_token = Some(Instant::now()); }
                app.ai_tokens += 1;
                if let Some(last) = app.ai_msgs.get_mut(idx) { last.1.push_str(&t); }
            }
            flux_moe::StreamEv::ToolCalls(_) => {}
            flux_moe::StreamEv::ToolRan(line) => {
                app.ai_msgs.insert(idx, ("tool".to_string(), line));
                idx += 1;
                app.ai_reply_idx = Some(idx);
            }
            flux_moe::StreamEv::Confirm { name, args, summary } => {
                if let Some(last) = app.ai_msgs.get_mut(idx) {
                    if !last.1.trim().is_empty() { last.1.push_str("\n\n"); }
                    last.1.push_str(&format!("⚠ I'd like to {summary}. Type yes to allow — anything else cancels."));
                }
                app.ai_pending = Some((name, args));
                finished = true;
            }
            flux_moe::StreamEv::Whole(r) => {
                if let Some(last) = app.ai_msgs.get_mut(idx) { last.1 = r; }
                app.ai_last_stats = String::new();
                finished = true;
            }
            flux_moe::StreamEv::Done { eval_count, eval_ns, prompt_count, reason } => {
                if let Some(last) = app.ai_msgs.get_mut(idx) {
                    last.1 = flux_moe::strip_think_blocks(&last.1);
                    if reason == "interrupted" { last.1.push_str("\n\n⚠ the connection to the model dropped mid-answer."); }
                }
                let secs = app.ai_started.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
                let n = if eval_count > 0 { eval_count } else { app.ai_tokens };
                let rate = if eval_ns > 0 { eval_count as f64 / (eval_ns as f64 / 1e9) } else { 0.0 };
                app.ai_last_stats = if rate > 0.0 {
                    format!("{n} tok · {rate:.1} tok/s · {secs:.1} s total · prompt {prompt_count} tok")
                } else { format!("{n} tok · {secs:.1} s total") };
                if n > 0 {
                    app.ai_avg_reply_tokens = if app.ai_avg_reply_tokens == 0.0 { n as f64 }
                        else { 0.7 * app.ai_avg_reply_tokens + 0.3 * n as f64 };
                }
                finished = true;
            }
        }
    }
    if finished {
        app.ai_thinking = false;
        if let Some(next) = app.ai_queued.pop_front() {
            if let Some(s) = app.ai_load_stop.take() { s.store(true, std::sync::atomic::Ordering::Relaxed); }
            app.ai_input = next;
            ai_submit(app);
            return;
        }
        if let Some(s) = app.ai_load_stop.take() { s.store(true, std::sync::atomic::Ordering::Relaxed); }
        if let Some(m) = app.ai_msgs.get_mut(idx) {
            if m.1.trim().is_empty() && app.ai_pending.is_none() {
                m.1 = "(the model returned no text — ask again, or try SIGIL_OLLAMA_THINK=0)".into();
            }
        }
        app.ai_reply_idx = None;
    } else {
        app.ai_rx = Some(rx);
    }
}

/// First entry into the [A]I tab, by ANY route: one local-ollama probe (localhost, instant),
/// then — if no model is there — the out-of-the-box setup, off-thread, progress in the
/// transcript. Idempotent (`ai_detected` latches). `SIGIL_AI_AUTOSETUP=0` keeps the probe
/// but leaves installing/pulling to an explicit `setup` command or F5.
pub(crate) fn ai_enter_tab(app: &mut App) {
    if app.ai_detected { return; }
    app.ai_detected = true;
    app.ai_models = crate::flux_moe::list_models();
    app.ai_model = app.ai_models.first().cloned();
    if app.ai_model.is_none() && flux_moe::autosetup_enabled() { ai_setup_start(app); }
    ai_skills_start(app); // flux-signed skills, off-thread
}

/// [A]I OUT-OF-THE-BOX: install/start ollama + pull the flux-signed manifest's model,
/// on a background thread; progress streams into the transcript via `ai_setup_rx`.
pub(crate) fn ai_setup_start(app: &mut App) {
    if app.ai_setup_running { return; }
    let (tx, rx) = mpsc::channel();
    app.ai_setup_rx = Some(rx);
    app.ai_setup_running = true;
    let base = release::active_base().to_string();
    std::thread::spawn(move || ai_setup::bootstrap(base, tx));
}

/// (Re)load flux-signed skills on a background thread; result lands on `ai_skills_rx`.
pub(crate) fn ai_skills_start(app: &mut App) {
    let (tx, rx) = mpsc::channel();
    app.ai_skills_rx = Some(rx);
    app.ai_skills_note = "loading…".into();
    let base = release::active_base().to_string();
    std::thread::spawn(move || { let _ = tx.send(skills::load(&base)); });
}


/// [Y] (10.6.3, Viktor: "Y resync doesnt work"): with your own node running, [Y] re-syncs THE NODE from block 0 —
/// pressed twice within 5 s, because the whole chain downloads again. Returns false when there is no own node (the
/// light engine's resync then runs, as before 10.6.0).
fn node_resync_key(app: &mut App) -> bool {
    #[cfg(feature = "producer")]
    if crate::forge_ui::band_visible() {
        static ARMED: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);
        let mut g = match ARMED.lock() { Ok(g) => g, Err(e) => e.into_inner() };
        if g.is_some_and(|t| t.elapsed() < Duration::from_secs(5)) {
            *g = None;
            crate::own_node::status::request_resync();
            app.toast = "⟲ re-syncing your node from block 0 — its old chain data is moved aside, not deleted".into();
        } else {
            *g = Some(Instant::now());
            app.toast = "press [Y] again within 5 s to re-sync YOUR NODE from block 0 (the whole chain downloads again)".into();
        }
        return true;
    }
    let _ = app;
    false
}


/// [F] (10.6.5): switch your full node off (light mode) and on; the message says what changed.
fn node_mode_key() -> String {
    #[cfg(feature = "producer")]
    {
        return crate::own_node::toggle_light_mode();
    }
    #[allow(unreachable_code)]
    String::new()
}
