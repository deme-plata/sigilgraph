//! sigil-top's subcommands (`version`, `serve`, `mine`, `update`, `hash-at`, …), split out of
//! main() on 2026-10-08. Every edit here used to re-optimize all of main.rs; as its own module
//! it is its own incremental codegen unit.
use super::*;

/// Runs the subcommand named by `argv[0]`, if any. `true` = it handled this run and main()
/// must return; `false` = no subcommand matched, continue to the dashboard.
pub(crate) fn dispatch(argv: &[String]) -> bool {
    match argv.first().map(|s| s.as_str()) {
        // v0.25: updater PRE-FLIGHT probe. Print the version and exit 0 — touch NOTHING else
        // (no network, no TUI, no DB, no splash) — so `relaunch_new_binary` can confirm a
        // freshly-swapped binary actually STARTS and reports the expected version BEFORE it
        // tears down the running app to hand off. This is what stops a bad/corrupt/ABI-
        // mismatched update from making the app vanish on the restart-after-sync.
        Some("--selfcheck") => { println!("{VERSION}"); return true; }
        // v7.4.2: an EXPLICIT version command that prints and exits. Until now
        // `version`/`--version` were unhandled and fell through to the default
        // TUI, which only *sometimes* flushed its `v{VERSION}` header to stdout
        // before blocking on the async runtime — so `release-sigil-top.sh`'s
        // own `"$LBIN" version | grep -q "v$VER"` sanity check was a coin-flip
        // that finally hung the whole release ceremony (2026-08-29, v7.4.2).
        // Print the same `v{VERSION}` the header shows, so that grep is
        // deterministic and users get a real version command.
        Some("version") | Some("--version") | Some("-V") => { println!("sigil-top v{VERSION}"); return true; }
        // 2026-10-19: the chain this binary was BUILT for — compiled in, no node asked. Same format and
        // exit code as `sigil-node network-id`; the release ceremony refuses to sign on a mismatch.
        Some("network-id") => {
            let (h, n) = (sigil_header::NETWORK_ID_STR, sigil_net::NETWORK_ID_STR);
            println!("header={h} net={n} g3_rules={}", sigil_header::IS_G3);
            if h != n { std::process::exit(1); }
            return true;
        }
        // v0.27.5: manual rollback escape hatch — revert to the previous binary the last update
        // backed up (pre-flighted before the swap). The operator's "undo a bad update" button.
        Some("revert") => { do_revert(); return true; }
        // 10.4.22: `sigil-top wallet [show | phrase | import | new [--replace]]` — one wallet everywhere.
        #[cfg(feature = "shield-register")]
        Some("wallet") => { crate::wallet_phrase::run_cli(&argv[1..]); return true; }
        // Windows launch-at-login control: `sigil-top autostart on|off|status`. The TUI also
        // exposes this via the tray's "Start at login" checkbox.
        Some("autostart") => {
            match argv.get(1).map(|s| s.as_str()) {
                Some("on") | Some("enable") | Some("true") => match autostart_set(true) {
                    Ok(()) => println!("✓ launch-at-login ENABLED"),
                    Err(e) => { eprintln!("✗ {e}"); std::process::exit(1); }
                },
                Some("off") | Some("disable") | Some("false") => match autostart_set(false) {
                    Ok(()) => println!("✓ launch-at-login DISABLED"),
                    Err(e) => { eprintln!("✗ {e}"); std::process::exit(1); }
                },
                _ => println!("launch-at-login: {}", if autostart_enabled() { "ON" } else { "OFF" }),
            }
            return true;
        }
        // v0.38: print this binary\'s full provenance — version, flux-rev source id,
        // the binary\'s own BLAKE3, and which release channel/target it tracks.
        Some("provenance") => {
            let exe_hash = std::env::current_exe().ok()
                .and_then(|p| std::fs::read(p).ok())
                .map(|b| blake3::hash(&b).to_hex().to_string())
                .unwrap_or_else(|| "?".into());
            println!("sigil-top v{VERSION}");
            println!("flux-rev:      {FLUX_REV}");
            println!("binary blake3: {exe_hash}");
            println!("channel:       {UPDATE_MANIFEST}");
            println!("target:        {SELF_TARGET}");
            return true;
        }
        Some("login") => {
            let seed = argv.iter().position(|a| a == "--seed").and_then(|i| argv.get(i + 1)).cloned();
            do_login(seed);
            return true;
        }
        Some("logout") => { clear_session(); println!("\n  {DIM}logged out — session cleared{RESET}\n"); return true; }
        // Headless wallet server: start the embedded :9800 server (wallet + /api proxy)
        // and block — no TUI. Same server the [W] shortcut opens. Ctrl-C to stop.
        Some("serve") => {
            let serve_dir = std::env::var("FLUX_STATIC_DIR")
                .unwrap_or_else(|_| "/home/orobit/q-narwhalknight/dist-fluxapp".into());
            let port: u16 = argv.iter().position(|a| a == "--port")
                .and_then(|i| argv.get(i + 1)).and_then(|s| s.parse().ok()).unwrap_or(9800);
            // v0.11.0: local-first explorer API in headless serve too. No live sync here,
            // so status/recent/search proxy to SIGIL_NODE_URL (the real node) and only the
            // cortex panel + any persisted-spine content-verify answer locally. Best-effort:
            // if the store is locked by another instance, fall back to pure proxy (None).
            let local_api = block_store::BlockStore::open_blocking(&sigil_top_db_path()).ok().map(|st| {
                std::sync::Arc::new(local_api::LocalApi {
                    reader: {
                        let c = std::sync::Arc::new(std::sync::OnceLock::new());
                        let _ = c.set(st.reader());
                        c
                    },
                    sync: None,
                    cortex: std::sync::Arc::new(std::sync::Mutex::new(local_api::CortexSnapshot::default())),
                    network: build_network_id(),
                })
            });
            match serve::start_with_api(&serve_dir, port, local_api) {
                Ok(_stop) => {
                    let _ = flux_register_scheme(); // flux:// works after a single run
                    // The banner must name the node serve.rs::proxy_api ACTUALLY relays to.
                    // It used to print the retired `:8099` rpcd while the proxy already
                    // targeted the braid API — a user reading the banner concluded their
                    // wallet was wired to a dead backend (2026-09-05).
                    let node = std::env::var("SIGIL_NODE_URL")
                        .unwrap_or_else(|_| "http://sigilgraph.quillon.xyz:18181".into());
                    println!("\n  sigil-top serve → http://localhost:{port}/  (wallet at /, /api + /v1 → {node})");
                    println!("  embedded out-of-the-box — no dist dir needed. Ctrl-C to stop.\n");
                    loop { std::thread::sleep(Duration::from_secs(3600)); }
                }
                Err(e) => { eprintln!("  serve failed: {e}"); std::process::exit(1); }
            }
        }
        // flux:// URL handler. The OS invokes `sigil-top flux-open flux://wallet`
        // when the user types flux://wallet in the browser → ensure the local serve
        // is up, then open the mapped localhost page in the default browser.
        Some("flux-open") => {
            let url = argv.get(1).cloned().unwrap_or_default();
            flux_open(&url);
            return true;
        }
        // Register / unregister the flux:// scheme with this OS (sigil-top = handler).
        Some("flux-register") => { let _ = flux_register_scheme(); return true; }
        Some("flux-unregister") => { let _ = flux_unregister_scheme(); return true; }
        // Headless miner (same engine [M] drives): mine N shares to the node, print each. Scriptable.
        // `mine` USED TO RUN A MINER WHOSE BACKEND NO LONGER EXISTS.
        //
        // It drives the legacy BLAKE3 leading-zero-bits path at `mine_url()`, whose default is
        // the old **sigil-rpcd** `/v1/mine` route. sigil-rpcd was stopped and disabled on
        // 2026-08-17 and again on 2026-08-31, permanently. Measured 2026-09-02: that endpoint
        // answers HTTP 000 and nothing listens on :8447 anywhere, so `sigil-top mine` hashed
        // correctly and threw every share away:
        //
        //   ✗ submit: error sending request for url (https://…:8447/v1/mine) (retry 3s)
        //
        // The worst possible shape for a user: real hashrate, an honest-looking error, an
        // infinite retry, and no reward ever — while `mine-rig`, the *other* subcommand, mined
        // the same chain perfectly (measured the same day: 30 shares accepted, 0 rejected,
        // ~80 MH/s, one block found). Nothing told the user they had picked the dead one.
        //
        // They are NOT one miner with a bad address — they speak different protocols. The live
        // path is dual-lane (BLAKE4 Φ + VDF Ω) posting `Submission { height, wallet, block }` to
        // `/v1/mining/submit`; the legacy path posts `{miner_hex, header, pow_nonce}`. Pointing
        // the old miner at the new endpoint is not a fix — it earns
        // `missing field 'height'`, which is exactly what happened when it was tried.
        //
        // So `mine` now REFUSES rather than pretending, and names the command that works. It
        // does not silently alias to `mine-rig`: `mine` takes a share COUNT and exits, while
        // `mine-rig` runs until killed, so aliasing would change what a scripted caller gets.
        Some("mine") => {
            println!("\n  {RED}✗ `sigil-top mine` is retired — its backend no longer exists.{RESET}");
            println!("  {DIM}It submits to the old sigil-rpcd /v1/mine route, which has been off since");
            println!("  2026-08-17. Measured: HTTP 000, nothing listening. It would hash at full speed");
            println!("  and earn nothing.{RESET}");
            println!("\n  Use the dual-lane miner, which mines the live braid:");
            println!("    {GREEN}sigil-top mine-rig{RESET} [wallet-64hex] [node-url]");
            println!("\n  {DIM}Rewards mint as PRIVATE notes when you pass a seed instead of an address:");
            println!("    SIGIL_MINE_SEED=<64-hex> sigil-top mine-rig{RESET}\n");
            return true;
        }
        // Retained for one release so an operator who scripted it gets the explanation above
        // rather than a silent behaviour change. Delete `mine`, `mine_url` and `start_mining`
        // together once nobody is calling it.
        #[allow(unreachable_patterns)]
        Some("__mine_legacy") => {
            let n: u64 = argv.get(1).and_then(|s| s.parse().ok()).unwrap_or(3);
            println!("\n  {GOLD}▲ sigil-top miner{RESET} → {} · wallet {}…", mine_url(), &miner_wallet()[..8]);
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let rx = start_mining(stop.clone());
            let mut accepted = 0u64;
            while accepted < n {
                match rx.recv_timeout(Duration::from_secs(30)) {
                    Ok(msg) => { println!("  {msg}"); if msg.starts_with("✓ share") { accepted += 1; } }
                    Err(_) => { println!("  {RED}timeout{RESET}"); break; }
                }
            }
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            println!("  {GREEN}done — {accepted} shares accepted{RESET}\n");
            return true;
        }
        // Headless CONTINUOUS dual-lane mining for rigs (Hive OS / systemd / CI) —
        // the SAME in-process flux_miner::engine::supervisor the [5] Mining tab runs,
        // but with no TTY. This is why "one binary = node + light client + wallet +
        // miner": a rig runs `sigil-top mine-rig <wallet>` and needs no separate exe.
        // GPU by default (the `gpu` feature); the engine falls back to CPU if OpenCL
        // init fails. Runs until killed. Output matches the standalone sigil-miner's
        // run_headless line so the Hive OS h-stats parser reads it unchanged.
        //   sigil-top mine-rig [wallet-64hex] [node-url]
        //   env: SIGIL_MINE_WALLET, SIGIL_MINE_NODE, SIGIL_MINE_CPU=1 (force CPU)
        //   env: SIGIL_MINE_SEED — 2026-08-24, the "no opt-in" privacy path. When set,
        //   the wallet address is DERIVED from this seed (overriding SIGIL_MINE_WALLET
        //   / the positional wallet arg — a rig can't mine to a different wallet than
        //   the one it just proved ownership of), and this rig self-registers for
        //   shielded mining before the loop starts: every reward from here on mints as
        //   a private note instead of a transparent credit. A bare wallet address (no
        //   seed) still works exactly as before — nobody who was already mining is
        //   broken by this.
        Some("mine-rig") => {
            use std::sync::atomic::{AtomicBool, Ordering};
            use std::sync::{Arc, Mutex};
            let url = argv.get(2).cloned()
                .or_else(|| std::env::var("SIGIL_MINE_NODE").ok())
                .unwrap_or_else(engine_node_url);
            #[cfg(feature = "shield-register")]
            let seed_material = std::env::var("SIGIL_MINE_SEED").ok();
            #[cfg(not(feature = "shield-register"))]
            let seed_material: Option<String> = None;
            let fallback_wallet = || {
                argv.get(1).cloned()
                    .or_else(|| std::env::var("SIGIL_MINE_WALLET").ok())
                    .unwrap_or_else(miner_wallet)
            };
            let wallet = if let Some(seed) = seed_material.as_deref() {
                #[cfg(feature = "shield-register")]
                {
                    match shield_setup::wallet_signing_key(seed) {
                        Some(sk) => {
                            let derived = hex::encode(sk.verifying_key().to_bytes());
                            println!("  🔐 SIGIL_MINE_SEED set — mining wallet derived: {}…", &derived.chars().take(8).collect::<String>());
                            // 2026-08-26: a background KEEPER, not the one-shot attempt
                            // this replaces. Registration settles ~30 min after
                            // submission, so the old blocking call could not observe
                            // whether it landed — a rig whose registration was refused
                            // (node briefly down) or accepted-then-dropped printed one
                            // line and mined transparently forever. The keeper asks the
                            // chain first (so a restart costs nothing when already
                            // registered), submits when absent, watches for it to
                            // actually settle, and resubmits if it doesn't. Non-blocking:
                            // mining starts immediately either way.
                            shield_setup::spawn_registration_keeper(&url, seed, |line| {
                                println!("  {line}");
                            });
                            // Keep the SHIELDED balance visible too. A registered wallet is
                            // paid in notes, so its transparent balance is frozen forever —
                            // showing only that number is what made a miner earning ~4
                            // SIGIL/min believe they were earning nothing.
                            shield_setup::spawn_shielded_scanner(&url, seed);
                            derived
                        }
                        None => {
                            // Same format SIGIL_MINE_SEED already requires for [M]ine
                            // (main.rs's miner_keypair()) — a malformed value here would
                            // otherwise silently fall through to an unrelated wallet,
                            // which is worse than refusing to guess.
                            println!("  ⚠ SIGIL_MINE_SEED is set but isn't a valid 64-hex seed — ignoring it, mining to the address instead");
                            fallback_wallet()
                        }
                    }
                }
                #[cfg(not(feature = "shield-register"))]
                { let _ = seed; fallback_wallet() }
            } else {
                // No seed — so this rig CANNOT be auto-registered for shielded mining,
                // and that is a cryptographic limit rather than a missing feature:
                // registration is a signed tx over the wallet's own key, and a bare
                // address is exactly the thing that proves nobody here holds that key.
                // Say so once, plainly, instead of leaving the operator to wonder why
                // their rewards are public. Mining itself is unaffected.
                let w = fallback_wallet();
                // The wallet may ALREADY be registered — e.g. registered once from the web
                // wallet, then mined here by address alone. Then rewards ARE minted as
                // private notes, the transparent balance never moves, and this rig has no
                // seed with which to open them: the operator sees a frozen number and
                // concludes mining is broken. It is not; the money is real and this process
                // simply cannot see it. Say so explicitly rather than printing the generic
                // "rewards will be TRANSPARENT" line, which in that case is FALSE.
                #[cfg(feature = "shield-register")]
                {
                    let (wc, urlc) = (w.clone(), url.clone());
                    std::thread::spawn(move || {
                        if shield_setup::wallet_is_registered(&urlc, &wc) {
                            println!(
                                "  ⚠ {}… IS registered for shielded mining, but this rig has no \
                                 SIGIL_MINE_SEED.\n    Your rewards are being paid as PRIVATE NOTES \
                                 — the transparent balance shown below will never rise, and this \
                                 process cannot open them.\n    Set SIGIL_MINE_SEED=<64-hex seed for \
                                 this wallet> to see and spend what you are earning.",
                                &wc.chars().take(8).collect::<String>()
                            );
                        } else {
                            println!("  ℹ mining to {}… with no SIGIL_MINE_SEED — rewards will be TRANSPARENT. \n    Set SIGIL_MINE_SEED=<64-hex seed for this wallet> to have them mint as private notes instead.",
                                &wc.chars().take(8).collect::<String>());
                        }
                    });
                }
                #[cfg(not(feature = "shield-register"))]
                println!("  ℹ mining to {}… with no SIGIL_MINE_SEED — rewards will be TRANSPARENT. \n    Set SIGIL_MINE_SEED=<64-hex seed for this wallet> to have them mint as private notes instead.",
                    &w.chars().take(8).collect::<String>());
                w
            };
            let want_gpu = cfg!(feature = "gpu") && std::env::var("SIGIL_MINE_CPU").is_err();
            println!("\n  ▲ sigil-top mine-rig v{VERSION} — dual-lane (BLAKE4 Φ + VDF Ω) — headless");
            println!("  wallet: {}…  node: {url}  requested: {}",
                &wallet.chars().take(8).collect::<String>(), if want_gpu { "GPU" } else { "CPU" });
            let stats = Arc::new(Mutex::new(flux_miner::engine::MinerStats::default()));
            let stop = Arc::new(AtomicBool::new(false));
            let desired_gpu = Arc::new(AtomicBool::new(want_gpu));
            let gpu_failed = Arc::new(AtomicBool::new(false));
            {
                let (u, w, s, st, dg, gf) = (url.clone(), wallet.clone(), stats.clone(),
                    stop.clone(), desired_gpu.clone(), gpu_failed.clone());
                std::thread::spawn(move || flux_miner::engine::supervisor(u, w, s, st, dg, gf));
            }
            // Emit a line whenever the accepted+rejected share count changes — the
            // exact shape the Hive OS h-stats.sh parser reads.
            let mut last = 0u64;
            let mut last_update: Option<String> = None;
            loop {
                std::thread::sleep(Duration::from_millis(500));
                let s = stats.lock().unwrap();
                if s.update_msg != last_update {
                    last_update = s.update_msg.clone();
                    if let Some(u) = &last_update { println!("  [update] {u}"); }
                }
                if s.shares_ok + s.shares_bad != last {
                    last = s.shares_ok + s.shares_bad;
                    let line = s.log.front().cloned().unwrap_or_default();
                    // BALANCE, HONESTLY (2026-08-27). `s.balance` comes from
                    // `flux_miner::engine::fetch_balance`, i.e. GET /api/v1/balance — the
                    // TRANSPARENT balance. For a wallet that has published a shield key
                    // that number is frozen forever by construction: every reward it wins
                    // is minted as a hidden note, so the one line a headless rig prints
                    // reads as "mining earns nothing" no matter how much it earns.
                    // Operator-reported twice, the second time against a build that had
                    // already gained a shielded panel — in the interactive TUI's mining
                    // tab, which a headless `mine-rig` never renders.
                    //
                    // When a seed is present the scanner knows the real figure; show that
                    // and label it, and keep the transparent number visible so the two are
                    // never confused for each other.
                    #[cfg(feature = "shield-register")]
                    let bal_txt = match shield_setup::latest_shielded() {
                        Some(sn) => format!(
                            "shielded {} SIGIL (+{} transparent)",
                            flux_miner::engine::format_sigil(sn.balance),
                            flux_miner::engine::format_sigil(s.balance)
                        ),
                        None => format!("bal {} SIGIL", flux_miner::engine::format_sigil(s.balance)),
                    };
                    #[cfg(not(feature = "shield-register"))]
                    let bal_txt = format!("bal {} SIGIL", flux_miner::engine::format_sigil(s.balance));
                    println!(
                        "  [{}] {line}   [✓{} ✗{}]  {} (Φ {})  {bal_txt}",
                        s.mode, s.shares_ok, s.shares_bad,
                        flux_miner::engine::format_hps(s.hashrate), flux_miner::format_flux(s.hashrate),
                    );
                }
                if stop.load(Ordering::Relaxed) { break; }
            }
            return true;
        }
        // Scriptable flux-way self-update — fetch the release channel, BLAKE3-verify, hot-swap.
        Some("--self-update") | Some("update") => {
            println!("\n  {DIM}checking flux release channel{RESET} {CYAN}{UPDATE_MANIFEST}{RESET}");
            match fetch_latest() {
                Ok(rel) if version_gt(&rel.version, VERSION) => {
                    println!("  {GOLD}↑ v{VERSION} → v{}{RESET} ({SELF_TARGET}) — downloading + BLAKE3-verifying…", rel.version);
                    match self_update(&rel) {
                        Ok(msg) => {
                            println!("  {GREEN}{msg}{RESET}\n  {DIM}relaunching v{}…{RESET}", rel.version);
                            relaunch_new_binary(&rel.version); // re-exec/spawn the new binary instead of just exiting
                            std::process::exit(0); // only reached if the exe path can't be resolved
                        }
                        Err(e)  => { eprintln!("  {RED}✗ {e}{RESET}\n"); std::process::exit(1); }
                    }
                }
                Ok(rel) => {
                    let msg = release_channel_current_msg(&rel.version);
                    if version_gt(VERSION, &rel.version) {
                        println!("  {RED}⚠ {msg}{RESET}\n");
                    } else {
                        println!("  {GREEN}{msg}{RESET}\n");
                    }
                    return true;
                }
                Err(e) => { eprintln!("  {RED}✗ update check: {e}{RESET}\n"); std::process::exit(1); }
            }
        }
        // 2026-09-30: `hash-at <H>…` prints the block hash this LOCAL store holds at each height, so
        // independent nodes can be compared at EQUAL height with no network (`verify-chain` proves a
        // store is internally consistent; this proves two stores hold the same chain). Run it on a
        // store no other sigil-top has open.
        Some("hash-at") => {
            let json = argv.iter().any(|a| a == "--json");
            let heights: Vec<u64> = argv.iter().skip_while(|a| a.as_str() != "hash-at").skip(1)
                .filter_map(|a| a.parse().ok()).collect();
            if heights.is_empty() {
                eprintln!("usage: sigil-top hash-at <height> [<height> …] [--json]");
                std::process::exit(2);
            }
            let path = sigil_top_db_path();
            let store = match block_store::BlockStore::open_blocking(&path) {
                Ok(s) => s,
                Err(e) => { eprintln!("{RED}✗ open store {path}: {e}{RESET}"); std::process::exit(2); }
            };
            let rows: Vec<(u64, Option<String>)> = heights.iter().map(|h| (*h, store.get_stored_at_height(*h).map(|b| b.hash_hex))).collect();
            print!("{}", format_hash_rows(store.synced_to(), &rows, json));
            std::process::exit(if rows.iter().all(|(_, h)| h.is_some()) { 0 } else { 1 });
        }
        // v0.9.0: re-verify the LOCAL block store as a connected spine (precheck +
        // parent linkage), genesis→tip. No network. Exit 0 = clean chain to genesis,
        // 1 = a real integrity break, 2 = couldn't open the store.
        Some("verify-chain") => {
            let json = argv.iter().any(|a| a == "--json");
            let path = sigil_top_db_path();
            let mut store = match block_store::BlockStore::open_blocking(&path) {
                Ok(s) => s,
                Err(e) => { eprintln!("{RED}✗ open store {path}: {e}{RESET}"); std::process::exit(2); }
            };
            let synced = store.synced_to();
            let t0 = Instant::now();
            // RC5 boot speedup: parallel-precheck verify (v0.34 lane) — contract-identical
            // to verify_to (see chain_verify tests serial==parallel), ~Nx faster on the
            // full-chain pass that dominated startup. This is the boot verify (u64::MAX).
            let report = chain_verify::verify_to_parallel(&mut store, u64::MAX);
            let dt = t0.elapsed();
            // A `Missing` at/after the download frontier is the clean terminator, not a break.
            let real_break = match &report.first_break {
                Some((h, chain_verify::BreakReason::Missing)) if *h >= synced => None,
                Some((h, r)) => Some((*h, r.to_string())),
                None => None,
            };
            if json {
                let brk = real_break.as_ref()
                    .map(|(h, r)| format!("{{\"height\":{h},\"reason\":{}}}", serde_json::Value::String(r.clone())))
                    .unwrap_or_else(|| "null".into());
                println!("{{\"verified_to\":{},\"synced_to\":{},\"checked\":{},\"clean\":{},\"break\":{brk},\"ms\":{}}}",
                    report.verified_to, synced, report.checked, real_break.is_none(), dt.as_millis());
            } else {
                println!("\n  {VBRIGHT}{BOLD}◆ SIGIL chain verification{RESET}  {DIM}(local store · {path}){RESET}");
                println!("  {DIM}downloaded:{RESET} {GOLD}{}{RESET} blocks   {DIM}verified spine:{RESET} {GREEN}{}{RESET} blocks   {DIM}checked:{RESET} {} in {} ms",
                    synced, report.verified_to, report.checked, dt.as_millis());
                match &real_break {
                    None => println!("  {GREEN}✓ clean connected spine to genesis — every header prechecks and links to its parent{RESET}\n"),
                    Some((h, r)) => println!("  {RED}✗ integrity break at height {h}: {r}{RESET}\n"),
                }
            }
            std::process::exit(if real_break.is_some() { 1 } else { 0 });
        }
        // v0.9.0: headless FULL VERIFYING SYNC — launch the P2P backfill + the spine
        // verifier, stream progress, exit 0 only when the verified spine reaches the
        // network tip (or --target). Exit 1 on a verification break, 3 on timeout,
        // 2 on setup failure. Scriptable / CI ("did this node fully + verifiably sync?").
        Some("full-sync") => {
            let target_arg: Option<u64> = argv.iter().position(|a| a == "--target")
                .and_then(|i| argv.get(i + 1)).and_then(|s| s.parse().ok());
            let timeout_s: u64 = argv.iter().position(|a| a == "--timeout")
                .and_then(|i| argv.get(i + 1)).and_then(|s| s.parse().ok()).unwrap_or(1800);
            let path = sigil_top_db_path();
            let store = match block_store::BlockStore::open_blocking(&path) {
                Ok(s) => s,
                Err(e) => { eprintln!("{RED}✗ open store {path}: {e}{RESET}"); std::process::exit(2); }
            };
            println!("\n  {VBRIGHT}{BOLD}◆ SIGIL full verifying sync{RESET}  {DIM}(store {path}){RESET}");
            println!("  {DIM}connecting to the sigil-g0 mesh — downloading + verifying genesis→tip…{RESET}");
            if let Some(t) = target_arg { println!("  {DIM}target height pinned to {t}{RESET}"); }
            println!("  {DIM}timeout {timeout_s}s · Ctrl-C to stop{RESET}\n");
            // LANE-P: --recent / SIGIL_SYNC_RECENT=1 launches the MONITOR path (recent_only,
            // recent-window snap) headlessly — the exact branch where the unaligned-base 57345
            // freeze lived — so the CI gate can verify it, not just the genesis crawl.
            let recent = argv.iter().any(|a| a == "--recent")
                || std::env::var("SIGIL_SYNC_RECENT").map(|v| v == "1" || v == "true").unwrap_or(false);
            if recent { println!("  {DIM}monitor mode — recent-window snap (recent_only){RESET}"); }
            let sync = block_sync::P2PBlockSync::launch(store, recent);
            // v0.15.1: a pinned --target also SEEDS the backfill tip so the refill
            // fires immediately (the gate is peer_best>0, not target_arg). Without
            // this, a quiet mesh left peer_best=0 and full-sync --target never pulled.
            if let Some(t) = target_arg { sync.set_known_tip(t); }
            let start = Instant::now();
            let mut last_print = instant_ago(10);
            loop {
                // 0.77: poll_state is try_lock (None = sync thread busy) — headless loop just
                // retries on the next 250ms tick.
                let Some(st) = sync.poll_state() else {
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                };
                if let Some(b) = &st.verify_break {
                    eprintln!("  {RED}✗ verification break — {b}{RESET}");
                    eprintln!("  {RED}  the downloaded chain does NOT form one connected spine. Aborting.{RESET}\n");
                    std::process::exit(1);
                }
                // SPINE-BREAK fix: the no-progress watchdog confirmed an unfillable hole. Fail LOUD
                // naming the EXACT stuck height (exit 4) instead of the old silent rate-0 crawl that
                // only ever ended on the generic timeout.
                if let Some((h, reason)) = &st.sync_failure {
                    eprintln!("\n  {RED}✗ SPINE BREAK — sync stuck at height {}{RESET}", group(*h));
                    eprintln!("  {RED}  {reason}{RESET}\n");
                    std::process::exit(4);
                }
                let target = target_arg.unwrap_or(st.peer_best_height);
                if last_print.elapsed() >= Duration::from_secs(2) {
                    last_print = Instant::now();
                    let pct = if target > 0 { (st.verified as f64 / target as f64 * 100.0).min(100.0) } else { 0.0 };
                    println!("  {CYAN}⬇{RESET} verified {GREEN}{}{RESET} / synced {GOLD}{}{RESET} / tip {} · {VBRIGHT}{:.1}%{RESET} · {} peers · {}s",
                        group(st.verified), group(st.blocks_synced), if target > 0 { group(target) } else { "?".into() },
                        pct, st.peer_count, start.elapsed().as_secs());
                }
                // LANE-P: don't declare "complete" before the mesh has actually connected and
                // seeded a REAL network tip. With 0 peers, peer_best can momentarily seed to ou
                // own genesis (target=1) → verified>=1 → a FALSE "sync complete at height 1" the
                // instant we start (the gate's flaky FAIL + a real "looks synced but isn't" bug).
                // Require target>1 AND a live peer (or a 30s grace for a genuine solo-at-tip).
                if target > 1 && st.verified >= target
                    && (st.peer_count > 0 || start.elapsed() > Duration::from_secs(30)) {
                    println!("\n  {GREEN}{BOLD}✓ full verifying sync complete — {} blocks verified as one connected spine to genesis{RESET}\n", group(st.verified));
                    std::process::exit(0);
                }
                if start.elapsed() > Duration::from_secs(timeout_s) {
                    eprintln!("\n  {RED}✗ timeout after {timeout_s}s — verified {} / target {} (peers={}){RESET}\n",
                        group(st.verified), if target > 0 { group(target) } else { "?".into() }, st.peer_count);
                    std::process::exit(3);
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        }
        _ => {}
    }
    false
}
