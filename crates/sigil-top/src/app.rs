//! Split out of main.rs (2026-10-08) so an edit in main() no longer re-optimizes this
//! code: rustc gives each module its own incremental codegen unit.
use super::*;

pub(crate) struct App {
    pub(crate) cfg: Config,
    pub(crate) st: NodeStatus,
    pub(crate) online: bool,
    pub(crate) last_fetch: Instant,
    pub(crate) verify: Option<TipVerify>,
    pub(crate) toast: String,
    pub(crate) toast_sticky: bool,        // v0.2.35: user-action toasts survive mining noise
    pub(crate) latest: String,            // live version from the flux release channel (auto-refreshed)
    pub(crate) last_update_check: Instant,
    pub(crate) update_rx: Option<mpsc::Receiver<String>>, // [U] runs on a bg thread; result lands here
    pub(crate) blocks: Vec<FeedBlock>,
    pub(crate) target_height: u64,  // the network tip we're syncing to
    pub(crate) synced_height: u64,  // last height we cryptographically verified
    pub(crate) verified_count: u64, // tips verified this session
    pub(crate) streak: u64,
    pub(crate) score: u64,
    pub(crate) mining: bool,
    pub(crate) mine_rx: Option<mpsc::Receiver<String>>,           // accepted-share messages from the miner thread
    pub(crate) mine_stop: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>, // signals the miner thread to stop
    pub(crate) mine_accepted: u64,                                 // shares the node accepted this session
    pub(crate) mine_hashrate: f64,                                 // v0.2.35: live GH/s from the miner thread
    pub(crate) mine_hashes: u64,                                   // v0.2.35: cumulative hashes computed
    pub(crate) wallet_balance: u128,                               // v0.2.35: miner wallet balance from feed
    pub(crate) mine_stats: std::sync::Arc<std::sync::Mutex<MinerStats>>, // v0.37: REAL dual-lane engine stats (shared w/ in-process miner thread)
    pub(crate) mine_desired_gpu: std::sync::Arc<std::sync::atomic::AtomicBool>, // v0.37: GPU/CPU toggle for the engine
    pub(crate) mine_gpu_failed: std::sync::Arc<std::sync::atomic::AtomicBool>,  // v0.37: engine signals GPU init failure -> CPU fallback
    pub(crate) mine_gpu_warned: bool,  // v0.90: one-time thermal/opt-in warning shown on first [G]-enable
    pub(crate) bps_ema: f64,            // v0.37: smoothed network block-rate (raw bps blinks 0<->250)
    pub(crate) bps_zero_streak: u32,    // consecutive zero-bps polls before showing honest idle
    pub(crate) welcome_until: Option<Instant>, // LANE-U v0.67: first-launch welcome modal (SIGIL emblem + giant F); any key or 14s clears it
    pub(crate) idle_store: Option<block_store::BlockStore>, // v0.40.3: parked store so [F] can launch sync ON DEMAND
    /// v7.0.21: the background store-opener's channel when no engine auto-started —
    /// [F]/[Y] poll it via `take_idle_store` (the store may still be opening/compacting).
    pub(crate) idle_store_rx: Option<std::sync::mpsc::Receiver<block_sync::OpenedStore>>,
    /// Live WAL-replay progress for the background store open — set unconditionally at
    /// startup (before the opener thread spawns) so the render loop can show a real
    /// gauge instead of the old static "still opening (compaction) — press again" toast.
    /// `finished` flips true the instant the open resolves either way (success or the
    /// fallback chain exhausting), independent of whether `[F]`/`[Y]` has been pressed.
    pub(crate) open_progress: std::sync::Arc<block_store::OpenProgress>,
    pub(crate) full_sync: bool,                                    // [F] opt-in heavy full sync (default = 10ms lightweight verify)
    pub(crate) full_sync_height: u64,                              // blocks downloaded so far in full sync
    pub(crate) full_sync_target: u64,                              // target height for full sync
    pub(crate) full_sync_active: bool,                             // true while downloading
    pub(crate) sync_us: u128,
    // L2-B: real eclipse-K — measured independent sources agreeing on the tip (replaces hardcoded k).
    pub(crate) eclipse_k: u32,
    pub(crate) eclipse_sources: Vec<(String, bool)>,
    pub(crate) last_eclipse: Instant,
    /// Post-update logo splash (flux updater return UX).
    pub(crate) splash_until: Option<Instant>,

    // === [A]I tab (flux_moe — on-device chat via the user's local ollama) ===
    pub(crate) ai_detected: bool,                          // have we probed ollama for models yet
    pub(crate) ai_models: Vec<String>,                     // models the local ollama has pulled
    pub(crate) ai_model: Option<String>,                   // the chosen model (the effort dial)
    pub(crate) ai_input: String,                           // the line being typed
    pub(crate) ai_msgs: Vec<(String, String)>,             // (role, content) chat history
    pub(crate) ai_thinking: bool,                          // a reply is in flight
    pub(crate) ai_rx: Option<mpsc::Receiver<flux_moe::StreamEv>>, // streamed reply events (off-UI-thread)
    pub(crate) ai_started: Option<Instant>,                // when the in-flight reply was sent
    pub(crate) ai_first_token: Option<Instant>,            // first visible answer token (latency to text)
    pub(crate) ai_think_chars: usize,                      // hidden reasoning received so far
    pub(crate) ai_tokens: u64,                             // answer chunks received (≈ tokens on ollama)
    pub(crate) ai_avg_reply_tokens: f64,                   // running average reply length → the ETA estimate
    pub(crate) ai_load: std::sync::Arc<std::sync::Mutex<flux_moe::Load>>, // GPU/CPU while generating
    pub(crate) ai_load_stop: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    pub(crate) ai_last_stats: String,                      // "120 tok · 19.8 tok/s · 6.1 s" of the last reply
    pub(crate) ai_pending: Option<(String, serde_json::Value)>, // an ACTION the model asked for, awaiting the user's `yes`
    pub(crate) ai_queued: std::collections::VecDeque<String>, // typed while a reply was in flight — sent next, never dropped
    pub(crate) ai_reply_idx: Option<usize>,                // index of the in-flight reply in ai_msgs (others may push meanwhile)
    pub(crate) ai_setup_rx: Option<mpsc::Receiver<ai_setup::SetupEvent>>, // auto-setup progress (install/pull), off-UI-thread
    pub(crate) ai_setup_running: bool,                     // an auto-setup is in flight (F5 is a no-op meanwhile)
    pub(crate) ai_skills: Vec<skills::LoadedSkill>,        // flux-signed skills loaded this session (text only)
    pub(crate) ai_skills_rx: Option<mpsc::Receiver<Result<skills::SkillLoad, String>>>,
    pub(crate) ai_skills_note: String,                     // status-line summary of the last skills load

    // === CATHEDRAL DAGKNIGHT (wired 2026-06-17) ===
    /// Live vaulted DagKnight view. Ingested from tips/blocks after spine checks.
    /// Surface: health_summary, last certified vault roots, divergence=0, flux proofs.
    pub(crate) cathedral: Cathedral,
    pub(crate) splash_frame: u8,
    // ── LANE-B v0.50: SIGIL rune animation tied to update availability ──────────
    // A floating overlay band — a sigil drawing itself line-by-line then fading.
    // Frame-counter + elapsed-envelope, driven entirely off the existing rende
    // tick (never blocks input/render). Plays every 10 min when an update is
    // available, every 2 h otherwise. Timestamps live in-process only (no disk).
    pub(crate) rune_until: Option<Instant>,    // Some(end) while the band is on screen
    pub(crate) rune_started: Option<Instant>,  // when the current play began (draw-on/fade envelope)
    pub(crate) rune_frame: u16,                // shimmer phase, bumped each tick like splash_frame
    pub(crate) rune_last_played: Instant,      // last time a play started (resets per process)
    // ── LANE-B v0.50: Mining-tab depth, derived App-side from MinerStats deltas ──
    // engine.rs / MinerStats are LANE-C's; we only OBSERVE them here, never mutate.
    pub(crate) accept_hist: std::collections::VecDeque<u8>,                   // accept-rate % samples (sparkline)
    pub(crate) last_accept_sample: Instant,                                   // throttle accept sampling
    pub(crate) mined_recent: std::collections::VecDeque<(u64, f64, Instant)>, // (mine-chain height, solve ms, when), newest first
    pub(crate) mine_shares_seen: u64,          // last shares_ok observed → detect freshly mined blocks
    // v0.6.0: Cortex MCP combo integration — AI agent registry + optimization engine
    pub(crate) cortex_loops: u64,
    pub(crate) last_cortex_gain: f64,
    pub(crate) cortex_summary: String,
    /// v0.11.0: cortex snapshot shared with the embedded HTTP server so the explorer's
    /// `/api/v1/cortex` panel reflects the live optimization-engine state.
    pub(crate) cortex_shared: std::sync::Arc<std::sync::Mutex<local_api::CortexSnapshot>>,
    pub(crate) mcp_combo_tool: String,     // active MCP combo verb being executed
    pub(crate) mcp_combo_result: String,   // last MCP combo result
    // v0.6.5: Real P2P block sync via flux-p2p mesh (Delta + Epsilon)
    pub(crate) p2p_sync: Option<block_sync::P2PBlockSync>,
    pub(crate) p2p_state: block_sync::P2PSyncState,
    /// The light archive's reader, once the store has opened (shared with the explorer's
    /// local API). The DATA INTEGRITY card reads our hash at a height from it.
    pub(crate) block_reader: std::sync::Arc<std::sync::OnceLock<block_store::BlockReader>>,
    pub(crate) p2p_blocks_synced: u64,
    pub(crate) p2p_rate: f64,                            // backfill blocks/sec (10s trailing window = current speed)
    pub(crate) p2p_rate_samples: std::collections::VecDeque<(std::time::Instant, u64)>, // (t, blocks_synced)
    /// 10.4.34: (t, verified spine height) — the rate at which the VERIFIED height climbs.
    /// The head rate above follows the network tip (~8 blk/s on g2) and divided the gap to
    /// the tip by it: "8 blk/s · eta 16 h" while the spine was really climbing ~650 blk/s
    /// (Viktor's screenshots, 2026-10-16). See [`spine_rate`].
    pub(crate) spine_samples: std::collections::VecDeque<(std::time::Instant, u64)>,
    pub(crate) sync_kf: Kalman1D,                         // v0.33.3: Kalman-smoothed sync rate (→ stable ETA)
    // v0.7.0: AI fleet monitoring — AIs worry about their nodes' uptime and version compliance
    pub(crate) fleet_nodes: Vec<FleetNode>,
    pub(crate) fleet_last_check: Instant,
    // v0.6.0: fluxc serve status for local wallet + cockpit
    pub(crate) serve_status: String,
    // v0.7.0: embedded HTTP serve shutdown signal (no external process)
    pub(crate) serve_stop: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    pub(crate) render_panic_streak: u8,
    pub(crate) bad_size_streak: u16,
    pub(crate) first_frame_logged: bool,
    // v0.10.5 "smooth cruise": refresh runs off the render thread. The worker's
    // result lands on refresh_rx; refresh_inflight stops duplicate workers piling up.
    pub(crate) refresh_rx: Option<mpsc::Receiver<RefreshOutcome>>,
    pub(crate) refresh_inflight: bool,
    // v0.10.5.1: graceful offline handling + embedded-serve watchdog.
    pub(crate) offline_since: Option<Instant>,  // when the node first stopped answering
    pub(crate) offline_streak: u32,             // consecutive offline refreshes → backoff
    pub(crate) last_serve_check: Instant,       // throttle the :9800 liveness probe
    // v0.13: tabbed cockpit
    pub(crate) tab: Tab,
    pub(crate) swarm: SwarmView,
    pub(crate) last_swarm_load: Instant,
}

impl App {
    pub(crate) fn new(cfg: Config) -> Self {
        let toast = cfg.initial_toast.clone().unwrap_or_default();
        App { cfg, st: NodeStatus::default(), online: false, last_fetch: Instant::now(),
              verify: None, toast, toast_sticky: false,
              latest: LATEST.to_string(),
              // v0.7.5: Trigger first check immediately (now - 300s = overdue)
              last_update_check: instant_ago(301),
              update_rx: None,
              blocks: Vec::new(),
              target_height: 0, synced_height: 0, verified_count: 0, streak: 0, score: 0,
              mining: false, mine_rx: None, mine_stop: None, mine_accepted: 0,
              mine_hashrate: 0.0, mine_hashes: 0, wallet_balance: 0,
              full_sync: false, full_sync_height: 0, full_sync_target: 0, full_sync_active: false, sync_us: 0,
              eclipse_k: 0, eclipse_sources: Vec::new(),
              last_eclipse: instant_ago(60),
              splash_until: if std::env::var("SIGIL_TOP_JUST_UPDATED").ok().as_deref() == Some("1") {
                  let _ = std::env::remove_var("SIGIL_TOP_JUST_UPDATED");
                  Some(Instant::now() + Duration::from_millis(1800))
              } else { None },
              ai_detected: false, ai_models: Vec::new(), ai_model: None,
              ai_input: String::new(), ai_msgs: Vec::new(), ai_thinking: false, ai_rx: None,
              ai_started: None, ai_first_token: None, ai_think_chars: 0, ai_tokens: 0, ai_avg_reply_tokens: 0.0,
              ai_load: Default::default(), ai_load_stop: None, ai_last_stats: String::new(), ai_pending: None, ai_queued: Default::default(), ai_reply_idx: None,
              ai_setup_rx: None, ai_setup_running: false, ai_skills: Vec::new(), ai_skills_rx: None,
              ai_skills_note: String::new(),
              splash_frame: 0,
              // LANE-B v0.50: rune animation — first play deferred a full interval
              // (no flash on launch; the post-update splash already covers just-updated).
              rune_until: None,
              rune_started: None,
              rune_frame: 0,
              // SIGIL_RUNE_DEMO=1 forces the first play immediately (QA/demo); off by
              // default so production launches don't flash (interval governs replays).
              rune_last_played: if std::env::var("SIGIL_RUNE_DEMO").ok().as_deref() == Some("1") {
                  instant_ago(86_400)
              } else {
                  Instant::now()
              },
              // LANE-B v0.50: mining-depth history (App-side observers of MinerStats)
              accept_hist: std::collections::VecDeque::new(),
              last_accept_sample: instant_ago(10),
              mined_recent: std::collections::VecDeque::new(),
              mine_shares_seen: 0,
              // v0.6.0: Cortex MCP combo integration
              cortex_loops: 0,
              last_cortex_gain: 0.0,
              cortex_summary: String::new(),
              cortex_shared: std::sync::Arc::new(std::sync::Mutex::new(local_api::CortexSnapshot::default())),
              mcp_combo_tool: String::new(),
              mcp_combo_result: String::new(),
              // v0.6.5: P2P block sync starts lazy — launched in run_tui after terminal is ready
              p2p_sync: None,
              p2p_state: block_sync::P2PSyncState::default(),
              block_reader: std::sync::Arc::new(std::sync::OnceLock::new()),
              p2p_blocks_synced: 0,
              p2p_rate: 0.0,
              p2p_rate_samples: std::collections::VecDeque::new(),
              spine_samples: std::collections::VecDeque::new(),
              sync_kf: Kalman1D::new(),
              serve_status: String::new(),
              serve_stop: None,
              render_panic_streak: 0,
              bad_size_streak: 0,
              first_frame_logged: false,
              // v0.7.0: Fleet starts with known bootstrap peers.
              // Delta dropped 2026-08-14 -- confirmed permanently gone (same
              // sweep that trimmed sigil-net::DEFAULT_BOOTSTRAP_PEERS down to
              // just Epsilon). Showing a dead node here as "checking..."
              // forever was its own small dishonesty on top of the real
              // sync-stall bug this was found alongside.
              fleet_nodes: vec![
                  FleetNode { name: "Epsilon".into(), addr: "89.149.241.126".into(), port: 9501, online: false, height: 0, version: String::new(), uptime_secs: 0 },
              ],
              fleet_last_check: instant_ago(3600),
              refresh_rx: None,
              refresh_inflight: false,
              offline_since: None,
              offline_streak: 0,
              last_serve_check: Instant::now(),
              mine_stats: std::sync::Arc::new(std::sync::Mutex::new(MinerStats::default())),
              mine_desired_gpu: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
              // Cathedral DagKnight live view (first wire)
              cathedral: Cathedral::new(),
              mine_gpu_failed: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
              mine_gpu_warned: false,
              bps_ema: 0.0,
              bps_zero_streak: 0,
              welcome_until: Some(Instant::now() + Duration::from_secs(14)),
              idle_store: None,
              idle_store_rx: None,
              open_progress: block_store::OpenProgress::new(),
              tab: Tab::Node,
              swarm: SwarmView::default(),
              last_swarm_load: instant_ago(10),
        }
    }

    /// LANE-B v0.50: advance + schedule the SIGIL rune animation. Pure frame-state,
    /// called once per render tick — never blocks input. Starts a ~4 s play every
    /// 10 min when an update is available, every 2 h otherwise (subtle alive-pulse),
    /// and never overlaps the post-update splash.
    pub(crate) fn tick_rune(&mut self) {
        let now = Instant::now();
        // currently playing → just bump the shimmer phase (like splash_frame).
        if self.rune_until.map(|u| now < u).unwrap_or(false) {
            self.rune_frame = self.rune_frame.wrapping_add(1);
            return;
        }
        // a play just ended → drop the handles so we stop counting as "animating".
        if self.rune_until.is_some() {
            self.rune_until = None;
            self.rune_started = None;
        }
        // never overlap the post-update logo splash.
        if self.splash_until.map(|u| now < u).unwrap_or(false) { return; }
        let interval = if version_gt(&self.latest, VERSION) {
            RUNE_INTERVAL_UPDATE
        } else {
            RUNE_INTERVAL_IDLE
        };
        if now.saturating_duration_since(self.rune_last_played) >= interval {
            self.rune_until = Some(now + RUNE_PLAY);
            self.rune_started = Some(now);
            self.rune_frame = 0;
            self.rune_last_played = now;
        }
    }

    /// LANE-B v0.50: true while the rune band is on screen (drives the 33/66 ms cadence).
    pub(crate) fn rune_active(&self) -> bool {
        self.rune_until.map(|u| Instant::now() < u).unwrap_or(false)
    }

    /// LANE-B v0.50: observe MinerStats and accumulate render-side history for the
    /// Mining tab — accept-rate sparkline + a ring of recently-mined MINE-CHAIN
    /// blocks. Read-only on the shared MinerStats (engine.rs is LANE-C's); all the
    /// derived state lives in App fields we own. Cheap; called each tick.
    pub(crate) fn tick_mining_history(&mut self) {
        let now = Instant::now();
        let (ok, bad, height, solve_ms) = match self.mine_stats.lock() {
            Ok(s) => (s.shares_ok, s.shares_bad, s.last_height, s.last_solve_ms),
            Err(_) => return,
        };
        // a freshly accepted share == a new block on our own mine-chain.
        if ok > self.mine_shares_seen {
            self.mined_recent.push_front((height, solve_ms, now));
            while self.mined_recent.len() > 12 { self.mined_recent.pop_back(); }
            self.mine_shares_seen = ok;
        } else if ok < self.mine_shares_seen {
            // engine restarted (counters reset) → re-baseline, don't log phantom blocks.
            self.mine_shares_seen = ok;
        }
        // sample the accept-rate every ~3 s while mining (CP437-safe shade sparkline).
        if self.mining && now.saturating_duration_since(self.last_accept_sample) >= Duration::from_secs(3) {
            let total = ok + bad;
            let acc = if total > 0 { ((ok as f64 / total as f64) * 100.0).round() as u8 } else { 100 };
            self.accept_hist.push_back(acc.min(100));
            while self.accept_hist.len() > 80 { self.accept_hist.pop_front(); }
            self.last_accept_sample = now;
        }
    }

    /// v0.37: start/stop the REAL in-process dual-lane miner — the SAME engine
    /// (flux_miner::engine::supervisor) as the standalone `sigil-miner` exe. One
    /// binary is now node + miner: no separate exe to run alongside. [g] flips GPU/CPU.
    pub(crate) fn toggle_engine_mining(&mut self) {
        self.mining = !self.mining;
        if self.mining {
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (url, wallet) = (engine_node_url(), miner_wallet());
            let (st, dg, gf) = (self.mine_stats.clone(), self.mine_desired_gpu.clone(), self.mine_gpu_failed.clone());
            let stopc = stop.clone();
            std::thread::spawn(move || engine::supervisor(url, wallet, st, stopc, dg, gf));
            self.mine_stop = Some(stop);
            self.tab = Tab::Mining;
            self.toast = "▲ MINING STARTED — dual-lane BLAKE4 Φ + VDF Ω, in-process".into();
            flux_webhook("mining_start", &format!("engine supervisor up (gpu_wanted={})", self.mine_desired_gpu.load(std::sync::atomic::Ordering::Relaxed)));
            self.toast_sticky = true;
        } else {
            if let Some(s) = self.mine_stop.take() { s.store(true, std::sync::atomic::Ordering::Relaxed); }
            self.toast = "■ mining stopped".into();
            flux_webhook("mining_stop", "engine stopped");
            self.toast_sticky = true;
        }
    }

    /// v0.10.5.1: adaptive refresh cadence. Base interval while the node answers;
    /// a gentle backoff (cap 15s) while it's offline so we don't hammer a dead
    /// endpoint; instant snap-back the moment it reconnects. Cruise control: ease
    /// off when the road's empty, accelerate the instant traffic returns.
    pub(crate) fn refresh_delay(&self) -> Duration {
        let base = self.cfg.interval.max(1);
        if self.offline_streak == 0 {
            Duration::from_secs(base)
        } else {
            let mult = 1u64 << self.offline_streak.saturating_sub(1).min(4); // 1,2,4,8,16
            Duration::from_secs((base * mult).min(15))
        }
    }
    /// v7.0.21: get the parked store for an on-demand engine launch ([F]/[Y]). The store
    /// now opens on a background thread, so it may STILL be opening (compaction) — in
    /// that case say so and return None; the operator retries in a moment. A dropped
    /// channel (all open fallbacks failed) reports the store as unavailable.
    pub(crate) fn take_idle_store(&mut self) -> Option<block_store::BlockStore> {
        if let Some(s) = self.idle_store.take() { return Some(s); }
        if let Some(rx) = &self.idle_store_rx {
            match rx.try_recv() {
                Ok(opened) => {
                    self.idle_store_rx = None;
                    if let Some(n) = opened.note {
                        self.toast = n;
                        self.toast_sticky = true;
                    }
                    return Some(opened.store);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    // v7.1.28: same live numbers the floating gauge shows (draw_store_open_overlay),
                    // so a keypress-driven toast and the passive overlay never disagree.
                    let (consumed, total, _finished) = self.open_progress.snapshot();
                    self.toast = if total > 0 {
                        let pct = ((consumed as f64 / total as f64) * 100.0).clamp(0.0, 100.0);
                        format!(
                            "⏳ block store still opening ({pct:.0}% — {} / {}) — press again in a moment",
                            human_bytes(consumed), human_bytes(total)
                        )
                    } else {
                        "⏳ block store still opening — press again in a moment".into()
                    };
                    self.toast_sticky = false;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.idle_store_rx = None;
                    self.toast = "✗ block store unavailable — see ~/.sigil-top.log".into();
                    self.toast_sticky = true;
                }
            }
        }
        None
    }

    pub(crate) fn resync(&mut self) {
        // v0.37: a resync that ACTUALLY restarts the sync surface. The old one only
        // cleared the feed list (which the SYNC hero ignores), so [y] looked like a
        // no-op. Now we reset every sync-progress signal the hero reads — counters,
        // the trailing-rate window, and the Kalman ETA filter — re-assert the tip
        // target to the engine, and kick a fresh tip/status fetch.
        self.blocks.clear();
        self.synced_height = 0;
        self.verify = None;
        self.p2p_blocks_synced = 0;
        self.p2p_rate = 0.0;
        self.p2p_rate_samples.clear();
        self.spine_samples.clear();
        self.sync_kf = Kalman1D::new();
        self.full_sync_height = 0;
        self.full_sync_target = 0;
        self.full_sync_active = false;
        if let Some(ref p2p) = self.p2p_sync {
            // v7.1.92 (rocky) — THE ACTUAL FIX. Everything above this line is
            // TUI-local: counters, the rate window, the Kalman ETA, the feed list.
            // Resetting them made the hero LOOK restarted while the engine carried on
            // from exactly where it was, because the persisted store watermarks
            // (synced_to / verified_to / base) were never touched. Operator report,
            // 2026-08-26: "[Y] doesn't work" — correct, it never did the one thing
            // its name promises. `request_full_resync` zeroes those watermarks on the
            // engine thread (the store lives there and reset needs &mut), so sync
            // genuinely restarts from the base.
            p2p.request_full_resync();
            // re-arm the target so the hero shows progress-to-tip again and the
            // engine re-checks it is tracking the latest tip.
            p2p.set_known_tip(self.st.height.max(self.target_height));
            self.toast = "⟳ RESYNC — starting from scratch: store watermarks zeroed, re-walking from base".into();
        } else if let Some(mut store) = self.take_idle_store() {
            // Same reset, applied directly — there is no engine thread to defer to
            // yet, and this is the one moment we own the store by value.
            store.reset_watermarks();
            // v0.71.1: in light monitor [Y] was a silent no-op with a lying toast —
            // there was no engine to resync. Now it STARTS the engine (same path
            // as F) so resync always resyncs.
            let p2p = block_sync::P2PBlockSync::launch(store, true);
            p2p.set_known_tip(self.st.height.max(self.target_height));
            self.p2p_sync = Some(p2p);
            // 0.77: the mode file now means full-ARCHIVE ([F]) specifically — [Y] starts
            // the light engine, so persist "light" (was "full", which would have made the
            // next restart launch a genesis archive the operator never asked for).
            persist_sync_mode("light");
            self.toast = "⟳ RESYNC — from scratch: watermarks zeroed, engine STARTED (was light monitor)".into();
        } else {
            self.toast = "✗ resync: sync store unavailable — restart the app".into();
        }
        self.toast_sticky = true; // confirmation survives mining/refresh noise (was vanishing instantly)
        self.refresh();
    }
    /// Back-compat shim: callers that used to block now kick off an async refresh.
    pub(crate) fn refresh(&mut self) { self.request_refresh(); }

    /// v0.13: reload the swarm coordination snapshot for the [2]/[3] tabs.
    pub(crate) fn load_swarm(&mut self) {
        self.swarm = load_swarm_view();
        self.last_swarm_load = Instant::now();
    }

    /// v0.10.5: spawn the network refresh on a worker thread (if none in flight).
    /// Returns immediately — the render loop keeps drawing while the socket work
    /// happens elsewhere. Result is drained by `poll_refresh`.
    pub(crate) fn request_refresh(&mut self) {
        if self.refresh_inflight { return; }
        self.refresh_inflight = true;
        // Decide here (UI thread) whether this cycle re-measures eclipse-K, so the
        // 30s throttle stays honest even though the DoH probe runs off-thread.
        let want_eclipse = self.last_eclipse.elapsed() >= Duration::from_secs(30);
        if want_eclipse { self.last_eclipse = Instant::now(); }
        let feed = self.cfg.feed.clone();
        let api = self.cfg.api.clone();
        // Start the K-gauge sampler on the first refresh, once `cfg.api` is settled (flags
        // and env have been applied by then). `spawn` is idempotent, so calling it every
        // cycle costs one atomic and cannot produce a second sampling thread.
        crate::kgauge_ui::spawn(crate::kgauge_ui::api_base_of(&api));
        crate::integrity_ui::spawn(crate::kgauge_ui::api_base_of(&api), self.block_reader.clone());
        let prior_synced = self.synced_height;
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(fetch_refresh(feed, api, want_eclipse, prior_synced));
        });
        self.refresh_rx = Some(rx);
    }

    /// v0.10.5: drain a completed refresh without ever blocking. Called once pe
    /// render-loop iteration.
    pub(crate) fn poll_refresh(&mut self) {
        let Some(rx) = self.refresh_rx.as_ref() else { return };
        match rx.try_recv() {
            Ok(out) => { self.refresh_rx = None; self.apply_refresh(out); }
            Err(mpsc::TryRecvError::Disconnected) => {
                // Worker died (panic / drop) — clear in-flight so the next interval retries.
                self.refresh_rx = None;
                self.refresh_inflight = false;
                self.last_fetch = Instant::now();
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
    }

    /// v0.10.5: merge a worker result into App state. Cheap (no I/O) → safe on the
    /// UI thread. This is the non-network tail of the old `refresh`.
    pub(crate) fn apply_refresh(&mut self, out: RefreshOutcome) {
        self.st = out.st;
        // v0.37: smooth the node jumpy blocks_per_sec (it blinks between 0 and
        // ~250-300 between its own measurement windows) so the MINING / NETWORK
        // POWER card always shows steady production. EMA on real samples; HOLD the
        // last rate through brief zero-gaps; fall to honest idle only after a
        // sustained run of zero polls (a genuine stop).
        let raw_bps = self.st.blocks_per_sec;
        if raw_bps > 0.0 {
            self.bps_ema = if self.bps_ema <= 0.0 { raw_bps } else { self.bps_ema * 0.7 + raw_bps * 0.3 };
            self.bps_zero_streak = 0;
        } else {
            self.bps_zero_streak += 1;
            if self.bps_zero_streak > 12 { self.bps_ema = 0.0; }
        }
        self.st.blocks_per_sec = self.bps_ema;
        // v7.0.24 MESH-ONLINE FIX: the online badge watched ONLY the HTTPS status feed —
        // an AUXILIARY web endpoint. A node whose libp2p mesh is connected, syncing and
        // mining is NOT offline (operator-reported: badge said "OFFLINE · net height 0"
        // for hours while the same node synced 8M+ blocks and mined at ~900 MH/s — the
        // feed's HTTPS path was blocked on the operator's side; the mesh was not). If the
        // feed fails but the mesh is live, stay ONLINE: tip from gossip (peer_best), and
        // say honestly that the feed is unreachable + why (the previously TUI-invisible
        // last_feed_err self-diagnosis).
        let mut online = out.online;
        if !online {
            if let Some(sync) = &self.p2p_sync {
                let sh = sync.state_handle();
                let (peers, peer_best) = {
                    let s = sh.lock().unwrap_or_else(|e| e.into_inner());
                    (s.peer_count, s.peer_best_height)
                };
                if peers > 0 && peer_best > 0 {
                    online = true;
                    self.st.height = self.st.height.max(peer_best);
                    if !self.toast_sticky {
                        let e = last_feed_err();
                        self.toast = format!("◆ online via p2p mesh — status feed unreachable{}",
                            if e.is_empty() { String::new() }
                            else { format!(": {}", e.chars().take(90).collect::<String>()) });
                    }
                }
            }
        }
        self.online = online;
        // v0.10.5.1: track offline → online transitions for backoff + banner.
        if online {
            if self.offline_streak > 0 && !self.toast_sticky {
                let was = self.offline_since.map(|t| fmt_uptime(t.elapsed().as_secs())).unwrap_or_default();
                self.toast = format!("✓ reconnected after {} offline", was);
            }
            self.offline_streak = 0;
            self.offline_since = None;
        } else {
            self.offline_streak = self.offline_streak.saturating_add(1);
            if self.offline_since.is_none() { self.offline_since = Some(Instant::now()); }
        }
        if let Some(b) = out.blocks { self.blocks = b; }
        if out.fallback_note && !self.toast_sticky {
            self.toast = "📡 Blocks fetched from API fallback".into();
        }
        if let Some((k, srcs)) = out.eclipse {
            self.eclipse_k = k;
            self.eclipse_sources = srcs;
        }
        self.refresh_inflight = false;
        // v0.7.8 (HONEST full sync): the old [F] equated full_sync_height with the
        // tip height and printed "complete: N verified" while storing ZERO blocks —
        // a false claim (the DB stayed empty). Full sync now reports the REAL numbe
        // of blocks actually received + stored via the chain mesh (p2p block store).
        // It only says "complete" when stored blocks actually reach the target, and
        // shows the true count (0 until the node mesh serves history) otherwise.
        if self.full_sync {
            let stored = self.p2p_state.blocks_synced;          // blocks really stored
            let target = self.p2p_state.peer_best_height.max(self.st.height);
            self.full_sync_height = stored;
            self.full_sync_target = target;
            self.full_sync_active = self.p2p_state.running && (target == 0 || stored < target);
            self.toast = if target > 0 && stored >= target {
                format!("✓ Full sync: {} blocks stored + verified", group(stored))
            } else if self.p2p_state.running {
                format!("⬇ Full sync: {} / {} blocks stored via chain mesh", group(stored), group(target))
            } else {
                "⬇ Full sync: connecting to chain mesh…".into()
            };
            #[cfg(feature = "producer")]
            {
                // The producer's own replay (full blocks + state, the thing that makes
                // this a REAL node) runs beside the archive; show it on the same line.
                if target > 0 { producer::status::set_replay_target(target); }
                if let Some(line) = producer::status::line() {
                    self.toast = format!("{} · ⛏ {}", self.toast, line);
                }
            }
        }
        // v0.2.35: carry wallet balance from feed into local state (non-breaking — 0 when absent).
        if self.st.wallet_balance > 0 { self.wallet_balance = self.st.wallet_balance; }
        // Auto-update signal: poll the flux release channel every 5 min so the update
        // v0.7.5: Non-blocking update check — runs on background thread
        if self.last_update_check.elapsed() > Duration::from_secs(300) {
            self.last_update_check = Instant::now();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let msg = match fetch_latest() {
                    Ok(r) => format!("AUTO-CHECK:{}", r.version),
                    Err(e) if e.contains("MANIFEST SIGNATURE INVALID") => format!("AUTO-CHECK-ERR:{e}"),
                    Err(_) => "AUTO-CHECK:".into(),
                };
                let _ = tx.send(msg);
            });
            self.update_rx = Some(rx);
        }
        // v0.40.2: the API fallback is the rpcd's MINING chain (a few thousand
        // blocks) and carries no `tip` object — never let it clobber the
        // produce-chain tip the feed gave us (the "tip 1,495" phantom). A tip
        // DROP is only believed when the verified feed tip itself reports it.
        let fresh_tip = self.st.tip.as_ref().map(|t| t.height).filter(|h| *h > 0).unwrap_or(self.st.height);
        self.target_height = if self.st.tip.is_some() { fresh_tip } else { fresh_tip.max(self.target_height) };
        // ONE-CHAIN (v7.1.6): show THE chain (ledger) height + a real uptime.
        if let Some(h) = one_chain_view(&mut self.st, self.p2p_sync.is_none()) {
            self.target_height = h;
        }
        // verify the tip (verify-don't-trust) and advance the synced height
        self.verify = self.st.tip.as_ref().map(verify_tip);
        if let Some(v) = self.verify.as_ref() {
            if v.ok {
                let advanced = v.height > self.synced_height;
                self.synced_height = v.height;
                self.sync_us = v.latency_us;
                if self.mining && advanced {
                    self.verified_count += 1;
                    self.streak += 1;
                    self.score = self.verified_count * self.streak.max(1);
                }
            } else if self.mining {
                self.streak = 0; // a bad tip breaks the streak
            }
        }
        // eclipse-K is now measured off-thread in fetch_refresh and applied above
        // via out.eclipse — no blocking DoH probe on the render thread anymore.
        self.last_fetch = Instant::now();
    }
}

/// L2-B: REAL eclipse-K — count INDEPENDENT verification paths that agree on the chain tip,
/// replacing the old hardcoded `k=2`. Path 0 = the node/feed tip we just cryptographically verified.
/// Paths 1..N = independent public DoH resolvers resolving the `_sigil-tip` anchor TXT; one counts
/// only if its answer carries the current tip height (so a single lying resolver can't fake the tip —
/// DNS-level eclipse resistance). HONEST: until the anchor is published (L2-C), the DoH paths return
/// nothing → K reflects only what was really verified, never a simulated climb.
// ── v0.27.5: self-healing crash-loop rollback (the updater's third layer) ────────────────
// `--selfcheck` pre-flight (v0.25) catches binaries that can't START; the fail-safe relaunch
// (v0.25) keeps the app alive if a handoff fails. THIS catches the last case: a new version
// that passes pre-flight, starts, but then CRASHES in real operation. Every dashboard boot
// records an attempt for the running VERSION; a detached timer clears it once the process has
// survived HEAL_SECS ("healthy"). If a boot instead finds the SAME version already failed to
// heal CRASH_STRIKES times in a row, it reverts to the binary the last update backed up and
// relaunches — no operator intervention. A high-value 24/7 node self-heals from a bad update.
pub(crate) const HEAL_SECS: u64 = 12;
pub(crate) const CRASH_STRIKES: u32 = 3;
