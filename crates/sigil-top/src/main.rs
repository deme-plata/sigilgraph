//! sigil-top — a `top`/`htop`-style terminal monitor for a SIGIL node.
//!
//! Two modes:
//!   • full  (default) — multi-panel dashboard: node / 4 state roots / economics
//!                        (21 M cap bar) / flux-fold succinct-sync capability.
//!   • lite  (`--lite`) — one compact scorecard line, for tmux strips & SSH peeks.
//!
//! Polls `https://sigilgraph.fluxapp.xyz/api/v1/status` with a hand-rolled std TCP GET (no
//! http-client dep → builds warm in seconds). If the node is unreachable it shows
//! an explicit OFFLINE card plus the known SIGIL constants, so the binary is always
//! useful. Obsidian+violet ANSI to match the SIGIL visual identity.
//!
//! Usage:
//!   sigil-top                 full dashboard, refresh every 2s until Ctrl-C
//!   sigil-top --lite          compact one-box scorecard
//!   sigil-top --once          render a single snapshot and exit (scripts/screens)
//!   sigil-top --interval 5    set refresh seconds
//!   sigil-top --api URL       point at a remote node status endpoint

// 2026-08-24: extracted to the `sigil-block-store` library crate so `sigil-chronos`
// (a deterministic test harness that cannot import a binary-only crate) can drive the
// real height-index conflict logic instead of a re-modelled copy — same reason
// `sigil-sync` was extracted earlier. `block_store::X` below now resolves via the
// `use sigil_block_store as block_store;` alias so every existing call site in this
// crate is unchanged; only this module declaration and that alias line differ.
// tlog!/log_line/IN_TUI/instant_ago/http_get/boot_trace live in sigil-top-base (2026-10-07 split).
#[macro_use]
extern crate sigil_top_base;
pub(crate) use sigil_top_base::tlog;
use sigil_block_store as block_store;
// pub(crate) (2026-08-23): producer::run reaches block_sync::verify::inflate_gossip_frame
// through this path — everything else stays exactly as private as before.
pub(crate) use sigil_top_sync::block_sync; // moved to crates/sigil-top-sync (2026-10-07)
pub(crate) use sigil_top_sync::chain_verify; // moved to sigil-top-sync · v0.9.0: full verifying sync — spine continuity + precheck walk
mod ledger_verify; // ONE-CHAIN P2a: verify the MONEY chain's header spine + real /supply truth
pub(crate) use sigil_top_sync::gap_sync; // moved to sigil-top-sync · SPINE-BREAK fix: testable genesis-up contiguity engine + shared watchdog/classify
mod help;
mod serve;
mod heroes;
use heroes::*;
mod mining_ui;  // LANE-U: mining tab + hero renderers
use mining_ui::*;
mod sync_ui;    // LANE-U: sync hero + sync-log tab renderers
mod kgauge_ui;  // Kristensen K-gauge card (Node tab) — proposer entropy, never block-rate deviation
mod integrity_ui; // ◈ DATA INTEGRITY card — our block hash vs every network node's at equal height
mod flux_moe;   // on-device AI brain (local ollama) for the [A]I tab
mod mining_relay; // miners point at THIS node (127.0.0.1:18186); balanced failover across upstream nodes
mod ai_tools;   // [A]I: the SIGIL wallet MCP surface as tools the model can use (read / propose / confirmed actions)
mod ai_setup;   // [A]I auto-setup: install/start ollama + pull the model, from a flux-SIGNED manifest
mod skills;     // flux-signed skill packs (text-only) for flux-moe
use sync_ui::*;
mod tabs_ui;    // LANE-U: draw_ui dispatcher + tab/card/footer renderers
use tabs_ui::*;
mod wallet_ui;  // LANE-U: wallet/browser/tray/scheme plumbing
use wallet_ui::*;
mod local_api;   // v0.11.0: serve the explorer /api/* from the LOCAL verified spine
mod cathedral;     // CATHEDRAL DAGKNIGHT: vaulted 4-root + DagKnight finality view
// 2026-08-26: local-only signing/shield endpoints for the embedded wallet server, so a
// wallet opened via [W] (localhost:9800, same box as SIGIL_MINE_SEED) can Shield/Send/
// Swap/Bridge with ZERO prompts. `mine-sign` (Swap/Bridge) is unconditional — it only
// ever needs sigil_oauth::Keypair, already a non-optional dep. `mine-shield` and
// `mine-send-private` need sigil-shield's real note-construction/proving math and are
// internally gated on the `shield-register` feature (default-on), same as
// `shield_setup` above. See the module's own doc comment for the full design.
mod mine_local_api;
// v7.1.40 (grogu-sync-perf, 2026-08-19): PRODUCER-MODE Phase 1 — inert scaffolding only.
// `cathedral.rs`'s own doc comment already anticipated this: "Real flux-narwhal-core /
// flux-consensus linearizer can be dropped in the run_dagknight_linearize slot later
// without changing the surface." This module is that drop-in, ported (not yet — Phase 2)
// from sigil-node's main.rs. Behind the `producer` Cargo feature (off by default) AND a
// runtime double-gate (SIGIL_TOP_PRODUCER env, checked in main() below) so a shipped
// producer-capable binary still defaults to today's light-client-only behavior.
#[cfg(feature = "producer")]
mod producer;

// `mine-rig --seed`'s auto-registration for shielded mining. Default-on (see the
// crate's Cargo.toml `shield-register` feature comment).
#[cfg(feature = "shield-register")]
mod shield_setup;

use crate::cathedral::Cathedral;

use std::io::{IsTerminal, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

// instant_ago, IN_TUI, log_line, tlog!, http_get, boot_trace: moved to sigil-top-base (2026-10-07).
pub(crate) use sigil_top_base::{instant_ago, log_line, IN_TUI};

/// LANE-B v0.50: how long the SIGIL rune animation stays on screen per play
/// (draw-on → hold → fade). Kept inside the 3–5 s band the side-mission specifies.
const RUNE_PLAY: Duration = Duration::from_millis(4200);
/// LANE-B v0.50: re-play cadence. An update being available is the loud signal →
/// play often (every 10 min); otherwise it is a quiet "still alive" pulse (every 2 h).
const RUNE_INTERVAL_UPDATE: Duration = Duration::from_secs(600);
const RUNE_INTERVAL_IDLE: Duration = Duration::from_secs(7200);

use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Deserialize;

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Gauge, Padding, Paragraph},
    Frame, Terminal,
};

use sigil_state::StateRoots;
use sigil_tip_proof::TipProof;
use sigil_oauth::{AuthRequest, Keypair, WalletAssertion, pkce_pair, verify_sig, wallet_id};
// v0.6.0: P2P mesh, swarm coordination, content-addressed version control
use flux_p2p::NetworkManager;
use flux_rev::{Store, snapshot, Genesis};

// v0.11.0: combined release — explorer local-spine /api (rocky-explorer) +
// smooth-cruise (async refresh, panic-restore, offline backoff/banner, serve
// watchdog). 0.11.0 is valid 3-part SemVer so VERSION flows straight from Cargo.
const VERSION: &str = env!("CARGO_PKG_VERSION");

mod channel_msg;
pub(crate) use channel_msg::{release_channel_current_msg, release_channel_stale_msg};

/// Offline fallback only — the *live* update signal is fetched at runtime from the
/// flux release channel (see [`UPDATE_MANIFEST`]). The update bar glows when the
/// channel reports a version newer than this binary, so an OLD build learns about a
/// new release without recompilation — the whole point of "auto-update the flux way".
// Tracks the binary's own version so it can never go stale on a release bump
// (a hardcoded "0.7.5" here caused the updater to re-exec the OLD versioned binary).
const LATEST: &str = VERSION; // ship cadence, not the 3-part Cargo version
/// The flux release channel for the lightweight node: `<product>-latest.json` in the
/// q-flux downloads dir — the SAME manifest `flux_release_check` reads. Fetched at
/// startup (throttled) and on `[U]`, so the running binary discovers new releases live.
const UPDATE_MANIFEST: &str = "https://sigilgraph.org/downloads/sigil-top-latest.json";
/// v8.0.0: release-channel BASES, tried in order — `sigilgraph.org` FIRST.
///
/// Two corrections land here, both measured 2026-09-02 rather than assumed:
///
/// 1. **`sigilgraph.org` is the canonical SIGIL home** (operator ruling 2026-08-26) and
///    was missing from this list entirely, so every manifest the client trusts — the
///    updater's, the [A]I tab's, the skills pack's — was fetched from `fluxapp.xyz`, a
///    domain whose `/api` + `/v1` still point at the retired `sigil-rpcd`. The release
///    channel worked there by accident, not by design.
/// 2. **`:8099` is DEAD, not an escape hatch.** The doc comment this replaces described
///    it as the fallback for networks filtering :443. That stopped being true when
///    `sigil-rpcd` was stopped and disabled (2026-08-17, re-confirmed 2026-08-31);
///    `curl` to it now returns HTTP 000, so it is a guaranteed-wasted retry on every
///    failover — worse than absent, because it burns the timeout budget before the
///    client reaches a base that answers.
///
/// The remaining two are verified live mirrors (identical manifest + sig bytes).
const CHANNEL_BASES: &[&str] = &[
    "https://sigilgraph.org/downloads",
    "https://sigilgraph.fluxapp.xyz/downloads",
    "https://quillon.xyz/downloads",
];
/// Which prebuilt this binary self-updates to (its per-OS entry in the manifest).
const SELF_TARGET: &str = if cfg!(all(windows, feature = "gpu")) { "windows-x64-gpu" }
    else if cfg!(windows) { "windows-x64" }
    else if cfg!(target_os = "macos") { "macos-arm64" }
    // ARM64 Linux (phones under Termux, Raspberry Pi, Ampere boxes): its own channel key.
    // Without this an aarch64 build called itself "linux-x64-gpu" and would have
    // self-replaced with an x86-64 binary — a bricked install on the next [U]pdate.
    else if cfg!(all(target_os = "linux", target_arch = "aarch64")) { "linux-arm64" }
    else if cfg!(feature = "gpu") { "linux-x64-gpu" }
    else { "linux-x64" };
/// Live testnet feed (same source flux-node.html uses): status + tip + block stream.
const DEFAULT_FEED: &str = "https://sigilgraph.fluxapp.xyz/sigil-status.json";
/// v0.38: flux-rev SOURCE provenance, stamped at release-build time via
/// `SIGIL_TOP_FLUX_REV=$(flux-rev snapshot crates/sigil-top)` -> the BLAKE3
/// `full:` content address of the source tree this binary was built from.
/// "unstamped" on ad-hoc dev builds. Surfaced in the header + `provenance` cmd
/// + every webhook event — SIGIL north-star claim #1 made visible.
const FLUX_REV: &str = match option_env!("SIGIL_TOP_FLUX_REV") { Some(v) => v, None => "unstamped" };

// 2026-08-29: 8 → 10 decimals (the sigil-g2 move; /v1/supply's max_supply is
// 2.1e17). At 8 dp every SIGIL amount this client rendered — supply, balances,
// send previews — was overstated exactly 100x.
const MAX_SUPPLY_BASE: u128 = 210_000_000_000_000_000; // 21 M × 10^10
const DECIMALS: u32 = 10;

// obsidian + violet ANSI (256-color)
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[38;5;245m";
const VIOLET: &str = "\x1b[38;5;141m";
const VBRIGHT: &str = "\x1b[38;5;177m";
const GOLD: &str = "\x1b[38;5;220m";
const GREEN: &str = "\x1b[38;5;114m";
const RED: &str = "\x1b[38;5;203m";
const CYAN: &str = "\x1b[38;5;80m";

mod feed;
pub(crate) use feed::{
    fetch_best, fetch_refresh, last_feed_err, verify_tip, RefreshOutcome, TipVerify,
};

/// Minimal blocking HTTP GET — no http-client dependency.
mod http_client;
pub(crate) use http_client::{fetch, parse_status};
pub(crate) use sigil_top_base::http_get;

mod fmt;
pub(crate) use fmt::{bar, countdown_now, fmt_countdown, fmt_eta, fmt_supply, fmt_uptime, hex, short_hex, short_root, MAINNET_LAUNCH_LABEL, MAINNET_LAUNCH_UNIX};

// ───────────────────────── ST-2: freeze / stall detection ─────────────────────────
mod stall;
pub(crate) use stall::{stall_check, StallState};

mod text_width;
pub(crate) use text_width::display_width;
mod render;
pub(crate) use render::{render_full, render_lite};

// ─── wallet login (sigil-oauth: OAuth2 PKCE, wallet signs — no password) ─────

#[cfg(test)]
mod hex_to_32_tests {
    use super::hex_to_32;
    const V: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
    #[test]
    fn valid_and_prefix() {
        let r = hex_to_32(V).unwrap();
        assert_eq!(r[0], 0x00); assert_eq!(r[1], 0x11); assert_eq!(r[31], 0xff);
        assert_eq!(hex_to_32(&format!("0x{V}")), hex_to_32(V)); // 0x prefix stripped
    }
    #[test]
    fn bad_input_returns_none_never_panics() {
        assert!(hex_to_32("").is_none());
        assert!(hex_to_32("tooshort").is_none());
        assert!(hex_to_32(&"z".repeat(64)).is_none()); // 64 chars, non-hex
        // 64 BYTES but multibyte + straddling — the exact case that used to panic on the slice.
        let straddle = format!("a{}", "€".repeat(21)); // 1 + 63 = 64 bytes, not ASCII
        assert_eq!(straddle.len(), 64);
        assert!(hex_to_32(&straddle).is_none()); // fixed: !is_ascii → None, no panic
    }
}

// Windows console / process-priority FFI helpers extracted to platform.rs
// (god-file split). Re-exported so main()'s call sites stay unqualified.
mod platform;
pub(crate) use platform::{enable_rich_console, win_has_console};
#[cfg(windows)]
pub(crate) use platform::lower_process_priority;

// Console glyph subsystem (UI_ASCII / ui_ascii / init_ui_ascii / sa) extracted to
// console_glyphs.rs (god-file split, 2026-09-01). Re-exported so every call site —
// including heroes.rs / sync_ui.rs / tabs_ui.rs via `use super::*` — is unchanged.
mod console_glyphs;
pub(crate) use console_glyphs::{init_ui_ascii, sa, ui_ascii};
mod session;
pub(crate) use session::{clear_session, read_session, session_path, write_session};

// Sync-mode persistence + boot_trace extracted to startup_util.rs (god-file split,
// 2026-09-01). Re-exported so main.rs's unqualified calls are unchanged.
mod startup_util;
mod app; #[allow(unused_imports)] use app::*;
mod tui_loop; #[allow(unused_imports)] use tui_loop::*;
mod cli;
mod top_core; #[allow(unused_imports)] use top_core::*;
pub(crate) use startup_util::{boot_trace, persist_sync_mode, read_sync_mode};

fn main() {
    sigil_top_base::set_version(VERSION); // http_get's user-agent keeps saying sigil-top/<this version>
    // v0.64: STARTUP TRACE + panic capture. If the app exits unexpectedly on a
    // Windows double-click, the breadcrumb + panic land in this file so we can
    // read the EXACT reason instead of guessing.
    boot_trace(&format!("main() entry v{} pid {}", env!("CARGO_PKG_VERSION"), std::process::id()));
    std::panic::set_hook(Box::new(|info| { boot_trace(&format!("PANIC: {info}")); }));
    // v7.1.40 producer-mode Phase 1: wire the runtime double-gate end-to-end now, while
    // it's still guaranteed inert (both functions hardcode `false` until Phase 3/5), so
    // the gate's presence and its boot-trace visibility are proven before anything real
    // depends on them. A `producer`-feature binary run WITHOUT either env var behaves
    // identically to today's shipping light client — this line changes nothing yet.
    #[cfg(feature = "producer")]
    boot_trace(&format!(
        "producer-mode gates: SIGIL_TOP_PRODUCER→{} SIGIL_TOP_PRODUCE→{} (both inert, Phase 1)",
        producer::producer_mode_enabled(), producer::should_produce()
    ));
    // POOL-DIAG: report THIS release's version on every challenge fetch (`&v=`),
    // not the flux-miner engine-crate fallback — the pool's /mining/miners is how
    // stale rig installs get spotted.
    let _ = flux_miner::client::CLIENT_VERSION.set(VERSION.to_string());
    // Capture the real install path NOW, before anything can self-replace us.
    if let Ok(e) = std::env::current_exe() {
        if e.exists() { let _ = INSTALL_EXE.set(e); }
    }
    #[cfg(windows)]
    lower_process_priority();
    enable_rich_console(); // UTF-8 + VT so icons/colours render (fixes the `?` glyphs)
    init_ui_ascii();       // decide ASCII vs rich glyphs (Windows-safe layout)
    // subcommands: login / logout (handled before the render loop)
    let argv: Vec<String> = std::env::args().skip(1).collect();
    // v0.77.7: clean up the `…exe.old` left by the Windows lock-fallback swap (the previous
    // running image, renamed aside so the new binary could take the install path). It's only
    // unlockable once the old process is gone, i.e. now. Skip during --selfcheck (must stay pure).
    #[cfg(windows)]
    if argv.first().map(|s| s.as_str()) != Some("--selfcheck") {
        if let Some(exe) = INSTALL_EXE.get().cloned().or_else(|| std::env::current_exe().ok()) {
            let old = exe.with_extension("exe.old");
            if old.exists() { let _ = std::fs::remove_file(&old); }
        }
    }
    if cli::dispatch(&argv) { return; }
    // 2026-08-24 (sync-then-produce bridge, operator-directed: "work on unifiyhing
    // the sigil top node so that i can produce blocks and actual is a real node ...
    // every user downloading sigil top wil be full node operator"). This is the
    // ACTUAL start of the producer loop — `producer::run::maybe_start` existed,
    // fully tested, since Phase 3/5 (2026-08-23) but was never called from here, so
    // no real `sigil-top` binary ever produced a block no matter what env vars were
    // set. Hard no-op unless the operator has explicitly set BOTH
    // `SIGIL_TOP_PRODUCER=1` AND `SIGIL_TOP_PRODUCE=1` (see
    // `producer::producer_mode_enabled`/`producer::should_produce`) — every other
    // launch of `sigil-top` (the vast majority: plain light-client wallet use) is
    // byte-for-byte unaffected. The returned handle MUST be kept alive for the rest
    // of `main()` (dropping it signals the background loop to stop — see
    // `ProducerLoopHandle`'s `Drop` impl) so it is bound here, before the
    // once/interactive dispatch below, and never re-bound to `_`.
    // v7.5.2 STARTUP FIX (Viktor, "won't start on Windows / loads all blocks from genesis"):
    // producer mode is DEFAULT-ON, and `maybe_start` runs a FULL sync
    // from genesis ON THE CALLING THREAD before returning. Called synchronously here it froze
    // startup before the TUI ever drew its first frame (reproduced under Wine: the boot trace
    // stopped right after the producer-mode gates line). Move the whole sync-then-produce
    // bootstrap onto a BACKGROUND thread so the dashboard opens instantly and the sync runs
    // behind it — the operator's "full sync first, produce out of the box" intent is preserved
    // (production still waits for the synced tip inside the loop), just off the UI thread. The
    // handle is parked in a slot main() holds for its whole life, so its Drop still stops the
    // loop on exit.
    #[cfg(feature = "producer")]
    let _producer_handle: std::sync::Arc<std::sync::Mutex<Option<producer::run::ProducerLoopHandle>>> = {
        let tick_ms: u64 = std::env::var("SIGIL_TOP_PRODUCE_TICK_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(500);
        let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
        let slot_bg = slot.clone();
        let _ = std::thread::Builder::new().name("producer-bootstrap".into()).spawn(move || {
            // 2026-09-17 (Viktor: "press F for full archive … afterwards it produces real
            // blocks and rewards"): do nothing until the operator asks for a FULL node —
            // the F key, a persisted "full" mode, or SIGIL_TOP_PRODUCE=1. The gates in
            // `producer::producer_mode_enabled`/`should_produce` still decide "never".
            if !(producer::producer_mode_enabled() && producer::should_produce()) {
                return;
            }
            while !producer::full_node_requested() {
                std::thread::sleep(Duration::from_secs(1));
            }
            // The hybrid (post-quantum) checkpoint verifier in `ChainTip::apply` needs the
            // producer's trusted hybrid id or it reports every checkpoint as FAILED
            // (observe-only, but on the dashboard). A downloaded node trusts the g2
            // producer's ids by default — the retired 09-15 key for older blocks and the
            // rotated one — exactly what happysrv and node3 are configured with.
            if std::env::var("SIGIL_TRUSTED_PRODUCER_ID_HEX").map(|v| v.trim().is_empty()).unwrap_or(true) {
                std::env::set_var("SIGIL_TRUSTED_PRODUCER_ID_HEX", producer::DEFAULT_TRUSTED_PRODUCER_IDS);
            }
            // The coinbase payee. `sigil-node`'s mint pays `SIGIL_PRODUCER_WALLET`, and with
            // nothing set it pays a `c1c1…` placeholder nobody holds — measured 2026-09-17,
            // 437 blocks minted to it. A user's full node pays the wallet the TUI already
            // mines to (`[M]`, `SIGIL_MINE_WALLET`, the chosen-wallet file), unless the
            // operator pinned a producer wallet explicitly.
            if std::env::var("SIGIL_PRODUCER_WALLET").map(|v| v.trim().is_empty()).unwrap_or(true) {
                let w = miner_wallet();
                std::env::set_var("SIGIL_PRODUCER_WALLET", &w);
                tlog!("[producer] coinbase payee = this node's mining wallet {}… (set SIGIL_PRODUCER_WALLET to override)", &w[..8.min(w.len())]);
            }
            boot_trace("producer-mode: full node requested — sync-then-produce bootstrap starting (see [producer]/[producer-sync] log lines)");
            tlog!("[producer] full node requested (F / persisted full / SIGIL_TOP_PRODUCE=1) — sync-then-produce bootstrap starting");
            if let Some(handle) = producer::run::maybe_start(Duration::from_millis(tick_ms)) {
                if let Ok(mut g) = slot_bg.lock() { *g = Some(handle); }
            }
        });
        slot
    };
    // v0.95: the default interactive dashboard runs the pinned updater gate before
    // entering raw-mode TUI so stale channels and bad signatures are visible instead
    // of looking like "already latest". --once, pipes, and --lite stay side-effect
    // free below; --no-update / SIGIL_TOP_NO_AUTOUPDATE=1 still opts out entirely.
    let mut cfg = parse_args();
    cfg.initial_toast = None; // set after one-shot/lite exits; async checks keep banner fresh
    // Non-TTY (piped / redirected / captured), --once, or --lite → emit exactly ONE
    // plain ANSI frame and exit. ratatui needs a real terminal; this path neve
    // spams. The live, interactive dashboard is the TUI below.
    // v0.63.1: a DOUBLE-CLICKED console on Windows can report is_terminal()=false on
    // stdout (tray/subsystem quirk), which dropped the app to the one-frame path and
    // "closed" instead of showing the dashboard. Treat it as interactive if EITHER
    // stdin or stdout is a tty — only a full pipe/redirect (both non-tty) stays plain.
    // v3: the #1 "no TUI on Windows" bug was is_terminal() returning false on a REAL console
    // → headless → dashboard never opened. Run the TUI whenever a console is plausibly attached:
    // is_terminal(stdout|stdin) OR win_has_console() (GetConsoleWindow != null on Windows) OR the
    // SIGIL_TUI=1 escape hatch. A genuine no-console pipe (all false) cleanly goes headless — it
    // CAN'T host an interactive TUI anyway. Headless also opt-in via --once / SIGIL_HEADLESS=1.
    // Terminal setup below is non-fatal so a quirky console can't silent-exit before frame 1.
    // v7.1.10: these two used `.is_ok()` — PRESENCE, not value — so
    // `SIGIL_HEADLESS=0` forced headless ON, the exact opposite of what it reads
    // like, and contradicted the line above documenting `SIGIL_HEADLESS=1`. It
    // cost a whole test cycle: a harness exported `SIGIL_HEADLESS=0` to mean
    // "not headless" and silently measured the one-frame headless path instead of
    // the TUI. Parse the value; treat 0/false/no/off/empty as OFF.
    let env_flag = |k: &str| -> bool {
        match std::env::var(k) {
            Ok(v) => !matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "" | "0" | "false" | "no" | "off"
            ),
            Err(_) => false,
        }
    };
    let force_headless = env_flag("SIGIL_HEADLESS");
    let interactive = !cfg.once && !force_headless && (
        env_flag("SIGIL_TUI")
        || std::io::stdout().is_terminal()
        || std::io::stdin().is_terminal()
        || win_has_console()
    );
    boot_trace(&format!("interactive={} (stdout_tty={} stdin_tty={} win_console={}) once={} lite={}",
        interactive, std::io::stdout().is_terminal(), std::io::stdin().is_terminal(), win_has_console(), cfg.once, cfg.lite));
    // Non-TTY (piped / captured / redirected) or --once → one plain frame, no loop.
    if cfg.once || !interactive {
        // P2a: prewarm the ledger refresher so the single frame carries the real
        // supply + header-verify verdict instead of the feed fallback (the
        // background thread otherwise races this one-and-only render).
        let _ = ledger_verify::latest();
        for _ in 0..40 { if ledger_verify::latest().is_some() { break; } thread::sleep(Duration::from_millis(250)); }
        let (mut st, online, source) = fetch_best(&cfg);
        one_chain_view(&mut st, true); // headless monitor: no spine engine here
        let frame = if cfg.lite { render_lite(&st, online) } else { render_full(&st, online, &cfg.api, source) };
        print!("{frame}");
        let _ = std::io::stdout().flush();
        return;
    }
    // --lite → the compact one-line scorecard, live-looped in place (TTY only).
    if cfg.lite {
        let clear = "\x1b[H\x1b[2J\x1b[3J";
        loop {
            let t = Instant::now();
            let (st, online, _src) = fetch_best(&cfg);
            print!("{clear}{}", render_lite(&st, online));
            let _ = std::io::stdout().flush();
            let nap = Duration::from_secs(cfg.interval).saturating_sub(t.elapsed());
            std::thread::sleep(nap.max(Duration::from_millis(200)));
        }
    }
    cfg.initial_toast = maybe_auto_update(&argv);
    // DEFAULT — the custom ratatui dashboard (Quillon-graph-node styled, multi-panel).
    // ratatui owns all box-drawing + layout, so alignment can never regress.
    let _ = cfg.tui; // --tui kept as an explicit alias; it's the default now
    boot_trace("entering run_tui (interactive dashboard)");
    if let Err(e) = run_tui(cfg) {
        boot_trace(&format!("run_tui returned Err: {e}"));
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
        eprintln!("sigil-top: TUI error: {e}");
    }
}

// ───────────────────────── ratatui TUI (v0.2) ─────────────────────────

// v0.3 redesign — 24-bit TRUECOLOR obsidian palette (replaces 256-indexed for richer rich-text).
const C_VIOLET: Color = Color::Rgb(0x95, 0x80, 0xff);   // panel borders
const C_VBRIGHT: Color = Color::Rgb(0xc8, 0xb6, 0xff);  // brand / emphasis
const C_GOLD: Color = Color::Rgb(0xf5, 0xc8, 0x5a);     // values / accents
const C_GREEN: Color = Color::Rgb(0x66, 0xe6, 0x8c);    // live / verified
const C_RED: Color = Color::Rgb(0xff, 0x6b, 0x6b);      // offline / failed
const C_DIM: Color = Color::Rgb(0x74, 0x74, 0x92);      // labels / subtle
const C_CYAN: Color = Color::Rgb(0x4f, 0xd6, 0xe0);     // titles / links
#[allow(dead_code)]
const C_INK: Color = Color::Rgb(0x4a, 0x4a, 0x66);      // faintest (separators)
// v0.33.2 BOLD NEON redesign — high-contrast neon-on-black accents for banners + bars.
const C_NEON_CYAN: Color = Color::Rgb(0x22, 0xf5, 0xff);   // neon edge / live tip
const C_NEON_GREEN: Color = Color::Rgb(0x4b, 0xff, 0x7a);  // neon synced / healthy
const C_NEON_PINK: Color = Color::Rgb(0xff, 0x4d, 0xa6);   // neon alarm / brand pop
const C_NEON_GOLD: Color = Color::Rgb(0xff, 0xd8, 0x52);   // neon value
const C_BG: Color = Color::Rgb(0x07, 0x07, 0x12);          // obsidian card bg
const C_BG_HEAD: Color = Color::Rgb(0x0c, 0x0a, 0x1f);     // header band bg

// ─────────────────────────────────────────────────────────────────────────────
// Flux-way self-update — read the release channel, BLAKE3-verify, hot-swap in place.
// ─────────────────────────────────────────────────────────────────────────────

// Release channel (manifest model, sig-verify, fetch/anchor) extracted to
// release.rs (god-file split). Re-exported so main.rs + sibling modules' calls
// stay unqualified.
mod release;
pub(crate) use release::{fetch_dns_anchor, fetch_latest, flux_webhook, Release, Target};

#[cfg(test)]
mod version_gt_tests {
    use super::version_gt;
    #[test]
    fn numeric_not_lexical() {
        // The classic bug guard: "7.5.10" must be > "7.5.9" (10 > 9), NOT string-compared
        // where "10" < "9". A regression to lexical compare here silently stops updates.
        assert!(version_gt("7.5.10", "7.5.9"));
        assert!(!version_gt("7.5.9", "7.5.10"));
        assert!(version_gt("7.10.0", "7.9.99"));
    }
    #[test]
    fn ordering_and_equality() {
        assert!(version_gt("8.0.0", "7.9.9"));   // major wins
        assert!(version_gt("7.6.0", "7.5.9"));   // minor wins
        assert!(!version_gt("7.5.5", "7.5.5"));  // equal is NOT greater
        assert!(version_gt("7.5.1", "7.5"));     // longer, trailing component > missing(0)
        assert!(!version_gt("7.5", "7.5.1"));
    }
    #[test]
    fn malformed_is_safe() {
        // Non-numeric components parse to 0 — must never panic.
        assert!(!version_gt("7.x.1", "7.0.5")); // [7,0,1] vs [7,0,5] → false
        assert!(!version_gt("", "0.0.0"));
        assert!(version_gt("1", ""));
    }
}

#[cfg(test)]
mod lane_n_tests {
    use super::resolve_mine_wallet;
    #[test]
    fn seed_derived_beats_everything_else() {
        // 2026-08-25: the whole point of the fix — a wallet mining via SIGIL_MINE_SEED
        // must see [W] resolve to that SAME address, never a stale env var or a
        // previously-chosen wallet from before the seed was set.
        let seed_addr = "d".repeat(64);
        let env = "a".repeat(64);
        let chosen = "b".repeat(64);
        assert_eq!(
            resolve_mine_wallet(Some(&seed_addr), Some(&env), Some(&chosen), "host"),
            seed_addr
        );
    }
    #[test]
    fn env_override_wins_when_no_seed() {
        let env = "a".repeat(64);
        assert_eq!(resolve_mine_wallet(None, Some(&env), Some(&"b".repeat(64)), "host"), env);
    }
    #[test]
    fn chosen_wallet_beats_hostname() {
        let chosen = "c".repeat(64);
        assert_eq!(resolve_mine_wallet(None, None, Some(&chosen), "host"), chosen);
    }
    #[test]
    fn falls_back_to_stable_hostname_hash() {
        let r = resolve_mine_wallet(None, None, None, "host");
        assert_eq!(r.len(), 64);
        assert_eq!(r, resolve_mine_wallet(None, None, None, "host")); // deterministic
        assert_ne!(r, resolve_mine_wallet(None, None, None, "other")); // per-host
    }
    #[test]
    fn invalid_inputs_are_ignored() {
        let host_hash = resolve_mine_wallet(None, None, None, "host");
        // too short + non-hex 64 → both rejected → hostname hash
        assert_eq!(
            resolve_mine_wallet(None, Some("short"), Some(&"z".repeat(64)), "host"),
            host_hash
        );
    }
}

#[cfg(test)]
mod leading_zero_bits_tests {
    use super::leading_zero_bits;

    #[test]
    fn counts_leading_zeros_across_byte_boundaries() {
        assert_eq!(leading_zero_bits(&[]), 0);
        // Whole zero bytes are 8 bits each; no non-zero byte → never breaks.
        assert_eq!(leading_zero_bits(&[0x00, 0x00, 0x00]), 24);
        assert_eq!(leading_zero_bits(&[0xFF, 0x00]), 0); // first byte non-zero
        assert_eq!(leading_zero_bits(&[0x0F]), 4); // 0000_1111
        assert_eq!(leading_zero_bits(&[0x00, 0x80]), 8); // 8 + 0 (1000_0000)
        assert_eq!(leading_zero_bits(&[0x00, 0x01]), 15); // 8 + 7
        assert_eq!(leading_zero_bits(&[0x00, 0x00, 0x20]), 18); // 16 + 2 (0010_0000)
    }

    #[test]
    fn more_leading_zeros_is_a_strictly_higher_count() {
        // The miner accepts a nonce when leading_zero_bits(hash) >= difficulty,
        // so the measure must be monotone: a hash with more leading zeros scores
        // strictly higher, never equal-or-less.
        assert!(leading_zero_bits(&[0x00, 0x00]) > leading_zero_bits(&[0x00, 0x40]));
        assert!(leading_zero_bits(&[0x00, 0x01]) > leading_zero_bits(&[0x01, 0x00]));
    }
}

mod updater;
pub(crate) use updater::{fetch_binary_reqwest, preflight_binary};
mod self_update;
pub(crate) use self_update::{maybe_auto_update, relaunch_new_binary, self_update};

mod kalman;
pub(crate) use kalman::Kalman1D;

mod boot_health;
pub(crate) use boot_health::{crashloop_guard, do_revert, prev_binary_path};

/// LANE-Z ROOT FIX (2026-06-19): a Backend that NEVER reports a degenerate (0×0)
/// terminal size. Windows conhost / Windows Terminal transiently answer the size
/// query with 0×0 at startup; ratatui's `autoresize` then sizes the buffer to 0×0
/// and `term.draw` paints NOTHING — a permanent BLANK SCREEN ("no TUI"), the exact
/// reported symptom. The old guard only *delayed* then force-drew into that same 0×0
/// area, so the first frame never appeared. This wrapper substitutes a sane fallback
/// (120×30) whenever the real size is degenerate and passes the TRUE size straight
/// through the instant the console reports it — a normal terminal is unaffected, and a
/// conhost that's briefly 0×0 still gets a full first frame. It also makes the TUI
/// render in a size-less pty, which is how this fix is validated without a Windows box.
// SafeSizeBackend (ratatui backend clamping a degenerate console size) extracted
// to safe_backend.rs (god-file split). Constructed in run_tui, so pub(crate).
mod safe_backend;
pub(crate) use safe_backend::SafeSizeBackend;

// open_store_with_fallbacks(+_inner) moved to store_lifecycle.rs (god-file split),
// co-located with the reset/heal/size-guard helpers it drives. Re-exported below.

// v7.1.10: probe the embedded wallet server and, if it is not answering, try to
// (re)start it in-process. Returns true when :9800 is accepting connections.
//
// Why this exists: a FAILED initial start (port already held by a previous
// instance — the classic case right after a self-update) left `serve_stop` as
// None, and the watchdog below was gated on `is_some()`. So the port stayed dark
// for the whole life of the process, [W] opened a browser tab at a refused port,
// and the reason was invisible because `serve_status` is rendered nowhere. Both
// the keypress paths and the watchdog now go through here.
use flux_miner::engine::{self, MinerStats};

// Swarm coordination view (data model + loader for the Swarm/Results tabs)
// extracted to swarm_view.rs (god-file split). Re-exported so tabs_ui + main.rs
// stay unqualified.
mod swarm_view;
pub(crate) use swarm_view::{
    load_swarm_view, SwarmActivity, SwarmAgent, SwarmClaim, SwarmMsg, SwarmResult, SwarmTask,
    SwarmView,
};

// ── v0.13 enrichment helpers (DeepSeek-consulted: color-hash, mini-bars, medals, rel-time, heat) ──

pub(crate) use sigil_top_sync::store_lifecycle; // moved to sigil-top-sync (2026-10-07)
pub(crate) use store_lifecycle::{
    build_network_id, dir_size_capped, heal_wedged_store_once, human_bytes,
    open_store_with_fallbacks, oversized_store_for_light_boot, reset_store_on_network_change,
    sigil_top_db_path,
};

#[cfg(test)]
mod pure_helpers_tests {
    //! Coverage for the pure money/format/version helpers (Tier 3 — sigil-top
    //! was the worst-density crate at 581 loc/test). All deterministic, no I/O.
    use super::*;

    #[test]
    fn hash_rows_plain_and_json_and_missing_heights() {
        let rows = vec![(10u64, Some("ab".to_string())), (11, None)];
        assert_eq!(format_hash_rows(99, &rows, false), "synced_to 99\n10 ab\n11 —\n");
        assert_eq!(format_hash_rows(99, &rows, true),
            "{\"synced_to\":99,\"blocks\":[{\"height\":10,\"hash\":\"ab\"},{\"height\":11,\"hash\":null}]}\n");
    }

    fn fb(h: u64) -> FeedBlock { FeedBlock { height: h, hash: format!("{h:x}"), producer: String::new(), txs: 0, tip_ms: 0 } }

    #[test]
    fn block_stream_is_newest_first_one_row_per_height_and_capped() {
        let mut v = vec![fb(5), fb(9), fb(7), fb(9), fb(1), fb(7)];
        merge_block_stream(&mut v, 3);
        assert_eq!(v.iter().map(|b| b.height).collect::<Vec<_>>(), vec![9, 7, 5]);
    }

    #[test]
    fn block_stream_keeps_a_late_arrival_that_is_newer_than_everything() {
        // The bug: gossip blocks were appended at the END, so the panel (first 14) never showed them.
        let mut v: Vec<FeedBlock> = (1..=20).map(fb).collect();
        v.push(fb(500));
        merge_block_stream(&mut v, 500);
        assert_eq!(v[0].height, 500);
    }

    /// How many samples the rate filter needs to cross the bulk→frontier cliff.
    ///
    /// The operator saw "4.6k blk/s" at 98.6% synced while the node was really doing ~10.
    /// That is the moment bulk import ends and the frontier follow begins — a genuine
    /// regime change, not a noisy reading.
    fn samples_to_converge(from: f64, to: f64) -> usize {
        let mut kf = Kalman1D::new();
        for _ in 0..40 {
            kf.update(from);
        }
        assert!((kf.x - from).abs() < from * 0.05, "filter should settle on the bulk rate first");
        for n in 1..=200 {
            kf.update(to);
            // "believable" = within 2x of the truth; at 10 blk/s that is <= 20 on screen.
            if kf.x <= to * 2.0 {
                return n;
            }
        }
        usize::MAX
    }

    #[test]
    fn rate_filter_crosses_a_regime_change_quickly() {
        let n = samples_to_converge(4_600.0, 10.0);
        // The fixed-gain filter (steady-state k = 36/216 = 0.167) needed ~34 samples: with a
        // ~1 s refresh that is over half a minute of showing a number 400x too high, right at
        // the point an operator is watching to see whether sync has finished.
        assert!(
            n <= 8,
            "REGRESSION: rate filter took {n} samples to cross a 4600 -> 10 blk/s collapse; \
             the adaptive gain should cross it in a handful"
        );
    }

    /// The adaptive gain must not turn the filter into a passthrough — smoothing ordinary
    /// jitter is the whole reason it exists, and a twitchy readout was the ORIGINAL
    /// complaint that motivated the filter in the first place.
    #[test]
    fn rate_filter_still_smooths_ordinary_jitter() {
        let mut kf = Kalman1D::new();
        for _ in 0..40 {
            kf.update(100.0);
        }
        let settled = kf.x;
        // A single 20% outlier must barely move the estimate.
        kf.update(120.0);
        assert!(
            (kf.x - settled).abs() < 6.0,
            "a lone 20% blip moved the estimate by {:.2} — too twitchy",
            (kf.x - settled).abs()
        );
    }

    /// `reset_to` is the explicit escape hatch the caller uses once it has DECIDED a
    /// collapse happened; it must land exactly, not slew.
    #[test]
    fn reset_to_lands_exactly_and_stays_usable() {
        let mut kf = Kalman1D::new();
        for _ in 0..40 { kf.update(4_600.0); }
        kf.reset_to(10.0);
        assert_eq!(kf.x, 10.0, "reset must land exactly on the new rate");
        kf.update(11.0);
        assert!(kf.x > 9.0 && kf.x < 12.0, "and the filter must keep working afterwards");
        kf.reset_to(f64::NAN);
        assert!(kf.x.is_finite(), "a non-finite reset must be ignored, not poison the estimate");
    }

    #[test]
    fn hex_to_32_roundtrips_and_rejects_bad_input() {
        // 64 hex chars → 32 bytes, value-correct.
        let h = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
        let b = hex_to_32(h).expect("valid 64-hex");
        assert_eq!(b[0], 0x00);
        assert_eq!(b[1], 0x11);
        assert_eq!(b[31], 0xff);
        // uppercase accepted (from_str_radix is case-insensitive)
        assert_eq!(hex_to_32(&h.to_uppercase()), Some(b));
        // surrounding whitespace trimmed
        assert_eq!(hex_to_32(&format!("  {h}  ")), Some(b));
        // 2026-08-25: an 0x/0X-prefixed seed must decode identically -- mine-rig's
        // seed_bytes() (shield_setup.rs) already stripped this prefix; this function
        // silently didn't, so a prefixed SIGIL_MINE_SEED mined headless correctly but
        // fell through to an unrelated fallback wallet for [M]ine/[W].
        assert_eq!(hex_to_32(&format!("0x{h}")), Some(b));
        assert_eq!(hex_to_32(&format!("0X{h}")), Some(b));
        assert_eq!(hex_to_32(&format!("  0x{h}  ")), Some(b));
        // wrong length / non-hex rejected
        assert_eq!(hex_to_32(&"a".repeat(63)), None);
        assert_eq!(hex_to_32(&"a".repeat(65)), None);
        assert_eq!(hex_to_32(&"g".repeat(64)), None, "non-hex must be rejected, not silently zeroed");
    }

    #[test]
    fn valid_addr_requires_exactly_64_hex() {
        assert!(valid_addr(&"a".repeat(64)));
        assert!(valid_addr(&"A1b2".repeat(16))); // mixed case, 64 chars
        assert!(valid_addr(&format!("  {}  ", "f".repeat(64))), "trims before checking");
        assert!(!valid_addr(&"a".repeat(63)));
        assert!(!valid_addr(&"a".repeat(65)));
        assert!(!valid_addr(&"z".repeat(64)), "non-hex rejected");
        assert!(!valid_addr(""));
    }

    #[test]
    fn fmt_supply_truncates_sub_unit_and_groups_thousands() {
        let unit = 10u128.pow(DECIMALS);
        assert_eq!(fmt_supply(0), "0");
        assert_eq!(fmt_supply(unit - 1), "0", "sub-unit dust truncates to 0 whole");
        assert_eq!(fmt_supply(unit), "1");
        assert_eq!(fmt_supply(999 * unit), "999");
        assert_eq!(fmt_supply(1_000 * unit), "1,000");
        assert_eq!(fmt_supply(1_234_567 * unit), "1,234,567");
    }

    #[test]
    fn fmt_uptime_picks_the_right_granularity() {
        assert_eq!(fmt_uptime(0), "0m 0s");
        assert_eq!(fmt_uptime(59), "0m 59s");
        assert_eq!(fmt_uptime(60), "1m 0s");
        assert_eq!(fmt_uptime(3600), "1h 0m");
        assert_eq!(fmt_uptime(90_061), "1d 1h 1m"); // 1d + 1h + 1m + 1s
    }

    #[test]
    fn one_chain_view_fills_uptime_and_holds_spine_height() {
        // uptime always becomes real (>=1) when the feed gives 0
        let mut st = NodeStatus { uptime_secs: 0, height: 31_780_000, ..Default::default() };
        // spine ON (engine running): height untouched, no target override, uptime filled
        let t = one_chain_view(&mut st, false);
        assert_eq!(t, None, "spine-on must not override target from ledger");
        assert_eq!(st.height, 31_780_000, "spine-on leaves the engine's own height");
        assert!(st.uptime_secs >= 1, "uptime must never render 0m 0s forever");
        // a non-zero feed uptime is respected, not clobbered
        let mut st2 = NodeStatus { uptime_secs: 42, height: 5, ..Default::default() };
        one_chain_view(&mut st2, false);
        assert_eq!(st2.uptime_secs, 42);
    }

    #[test]
    fn fmt_eta_handles_nonfinite_and_ranges() {
        assert_eq!(fmt_eta(-1.0), "—");
        assert_eq!(fmt_eta(0.0), "—");
        assert_eq!(fmt_eta(f64::INFINITY), "—", "non-finite must not panic / format garbage");
        assert_eq!(fmt_eta(f64::NAN), "—");
        assert_eq!(fmt_eta(45.0), "45s");
        assert_eq!(fmt_eta(125.0), "2m");
        assert_eq!(fmt_eta(3_661.0), "1h 1m");
        assert_eq!(fmt_eta(60.0 * 60.0 * 24.0 * 100.0), "∞", "absurd ETA collapses to ∞");
    }

    #[test]
    fn version_gt_orders_semver_numerically() {
        assert!(version_gt("1.0.0", "0.9.9"));
        assert!(version_gt("1.10.0", "1.9.0"), "10 > 9 numerically, not lexically");
        assert!(version_gt("2", "1.9.9"), "shorter-but-larger major wins");
        assert!(version_gt("1.0.1", "1.0.0"));
        assert!(!version_gt("1.2.3", "1.2.3"), "equal is not greater");
        assert!(!version_gt("1.0.0", "1.0.1"));
        assert!(!version_gt("1.0", "1.0.0"), "1.0 == 1.0.0 (missing components are 0)");
    }
}

#[cfg(test)]
mod mesh_join_default_tests {
    use super::mesh_join_default;
    #[test]
    fn joins_by_default_and_respects_opt_out() {
        assert!(mesh_join_default(None, false), "double-click must join the mesh");
        assert!(mesh_join_default(Some("1"), false));
        assert!(!mesh_join_default(Some("0"), false), "explicit opt-out");
        assert!(!mesh_join_default(Some(""), false));
        assert!(!mesh_join_default(None, true), "--no-sync / SIGIL_TOP_NO_SYNC opts out");
        assert!(mesh_join_default(Some("1"), true), "explicit =1 beats the flag");
    }
}
