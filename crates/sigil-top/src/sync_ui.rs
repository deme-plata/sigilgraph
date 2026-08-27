// LANE-U: extracted from heroes.rs + main.rs (pure move, no behavior change).
// `use super::*` reaches main.rs's private helpers/consts/App — the heroes.rs pattern.
#![allow(clippy::too_many_lines)]
use super::*;

/// v0.33.3: the SYNC HERO — a full-width band with a BIG progress bar, Kalman-smoothed rate
/// + ETA, in-flight chunk / fleet / mesh / PID telemetry, and a static starship motif. The
/// whole frame is themed by the sync verdict color. Progress = s.verified (the honest spine),
/// NOT s.blocks_synced (faked to the tip in light-monitor mode — see memory/render note).
pub(crate) fn draw_sync_hero(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let s = &app.p2p_state;
    // v0.40.2: with the sync engine off (the Windows light-monitor default) the
    // hero must SAY so instead of rendering a 0% backfill that looks broken.
    let light = app.p2p_sync.is_none();
    let fold_ok = app.verify.as_ref().map(|v| v.ok).unwrap_or(false);
    let net_tip = s.peer_best_height.max(app.target_height);
    // v0.59: a checkpoint/spine can NEVER be above the real network tip. A phantom gossip
    // claim (or a stale high-water mark from a chain that was reset) used to make the hero
    // read "✓ checkpoint 5M" while the tip was only 0.33M — clamp the displayed value to the
    // live tip so it's always honest (the chain-reset detector clamps the state too).
    let spine = if net_tip > 0 { s.verified.min(net_tip) } else { s.verified };
    // v0.57 (LANE-M): RECENT-WINDOW monitor (base snapped forward, sync engine ON). `verified` is
    // anchored at the CHECKPOINT base, not genesis — so a verified/tip bar is dishonest: it implies
    // a full-genesis spine and FREEZES when the base-anchored watermark can't reach genesis (the
    // "froze at 49,153, looks broken" repro). A light monitor's real job is TRACKING THE HEAD via
    // the 10ms tip-proof, so drive the bar off that (caught + fold_ok ⇒ 100%) and show `verified`
    // as a separate checkpoint badge below. Full-sync (--sync genesis, !light_mode) keeps the
    // spine bar, which legitimately advances from genesis.
    let snap_mode = !light && s.light_mode;
    let gap = net_tip.saturating_sub(spine);
    // 2026-08-23 (grogu-finality-gap-clarity): `gap` above compares the verified spine
    // against the RAW frontier tip — but sigil-g0's DagKnight/GHOSTDAG consensus holds
    // the newest ~512 blocks in a probation window before they're finalized
    // (`BraidConfig::final_depth`, sigil-node/src/main.rs — bumped 64→512 on 2026-08-15).
    // The server itself won't serve past its own finalized height. So a perfectly
    // healthy, fully-caught-up client STILL shows a ~512 gap forever against the raw
    // tip — not stuck, just honestly reflecting blocks nobody on the network has
    // settled yet. Root-caused live 2026-08-23 against Epsilon's own frontier
    // telemetry (`tip_h` vs `fin_h`): the delta was exactly 512, an exact match, and
    // an operator flagged the persistent ~500 gap as confusing before this was found.
    // `settled_gap` estimates the gap against the FINALIZED tip instead — this is
    // what should read near-zero once truly caught up. It's an ESTIMATE (the client
    // has no live wire-protocol field for the real finalized height yet, only this
    // documented default), so it can drift if the operator changes final_depth, but
    // it's far more honest than comparing against a tip that's permanently ~512 ahead.
    const FINAL_DEPTH_ESTIMATE: u64 = 512;
    let settled_tip = net_tip.saturating_sub(FINAL_DEPTH_ESTIMATE);
    let settled_gap = settled_tip.saturating_sub(spine);
    let following = net_tip > 0;
    let caught = following && gap < 16_384;
    let frac = if net_tip > 0 { (spine as f64 / net_tip as f64).clamp(0.0, 1.0) } else { 0.0 };
    // light (engine off): bar = the 10ms tip-proof verdict, not the disabled backfill.
    // snap (recent-window): caught + valid tip-proof ⇒ fully doing its job (100%), never a frozen %.
    let frac = if light { if fold_ok { 1.0 } else { 0.0 } }
        else if snap_mode && fold_ok && caught { 1.0 }
        else { frac };
    let synced = caught && fold_ok && s.verify_break.is_none();
    let connecting = s.fetched_total == 0 && spine == 0 && !following;
    let kf_rate = app.sync_kf.x.max(0.0);                       // Kalman-smoothed blk/s
    // v0.58: the kalman feed (app.p2p_rate) can read 0 mid-sync while the REAL contiguous
    // commit rate (s.commit_rate = the shown ⚡/s commit) is live -> fall back to it so the
    // panel never shows a false rate 0 blk/s during an ACTIVE sync (it looked dead).
    let kf_rate = if kf_rate >= 1.0 { kf_rate } else { s.commit_rate.max(0.0) };
    let eta = if synced || kf_rate < 1.0 { f64::INFINITY } else { gap as f64 / kf_rate };
    // Turbo continuity score (invented for continuous high download bandwidth)
    let cont = s.turbo_continuity.continuity_score * 100.0;

    let (vtext, vcol) = if light { ("◇ LIGHT MONITOR", C_NEON_CYAN) }
        else if s.verify_break.is_some() { ("⚠ SPINE BREAK", C_NEON_PINK) }
        else if synced { ("◆ SYNCED", C_NEON_GREEN) }
        else if connecting { ("… CONNECTING", C_DIM) }
        else if caught { ("≈ TRACKING HEAD", C_NEON_CYAN) }
        else { ("⬇ SYNCING", C_NEON_GOLD) };

    // LANE-P v0.59: never a silent 0 blk/s — when the sync engine reports a PARKED frontier
    // (stall_reason set), the hero headline says STALLED (full reason lives in the state /
    // Sync Log) instead of a quiet "SYNCING" that looks broken.
    //
    // 2026-08-23 (grogu-stall-color-honesty): this used C_NEON_PINK — the SAME bright
    // alarm color as the genuine "SPINE BREAK — STUCK" state below. A routine, usually
    // self-recovering nudge (fires every time the connection hiccups, which is often —
    // real, ongoing network churn between clients and the server) was visually
    // indistinguishable from an actual stuck/broken sync. Flagged live by an operator
    // watching a client cycle STALLED→resume→STALLED 5-10 times in 4 minutes while
    // genuinely still making progress: "looks very unstable" even though it wasn't.
    // Gold (the same color SYNCING already uses) reserves the alarm color for the one
    // state that actually needs it.
    let (vtext, vcol) = if !s.stall_reason.is_empty() && !synced && !light {
        ("⚠ STALLED — nudging peer", C_NEON_GOLD)
    } else { (vtext, vcol) };

    // SPINE-BREAK fix: a CONFIRMED watchdog/fatal failure outranks every other headline —
    // the operator must never miss it (this is what replaces the old silent ~499k rate-0).
    let (vtext, vcol) = if s.sync_failure.is_some() {
        ("✗ SPINE BREAK — STUCK", C_NEON_PINK)
    } else { (vtext, vcol) };

    // state-themed border; title chip stays neon-cyan
    let block = card_block(" ◇ SYNC · sigil-g0", C_NEON_CYAN)
        .border_style(Style::default().fg(vcol));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // [ big bar (full width) ] over [ telemetry | starship ]
    let [bar_row, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
    let [tele, ship] = Layout::horizontal([Constraint::Min(0), Constraint::Length(20)]).spacing(1).areas(body);

    // ── BIG progress bar ─────────────────────────────────────────────────
    let label = format!(" {:>5.1}%  {}  BW:{:.0}%{}", frac * 100.0, vtext, cont,
        if s.commit_rate > 1.0 { format!("  ⚡{}/s commit", group(s.commit_rate.round() as u64)) } else { String::new() });
    let total = bar_row.width as usize;
    let barw = total.saturating_sub(label.chars().count() + 1).max(4);
    let fill = (frac * barw as f64).round() as usize;
    let bar_str = "█".repeat(fill.min(barw)) + &"░".repeat(barw.saturating_sub(fill));
    f.render_widget(Paragraph::new(Line::from(vec![
        Span::styled(bar_str, Style::default().fg(vcol).add_modifier(Modifier::BOLD)),
        Span::styled(label, Style::default().fg(vcol).add_modifier(Modifier::BOLD)),
    ])), bar_row);

    // ── telemetry (left) ─────────────────────────────────────────────────
    let chunk = if s.sync_cursor > 0 {
        // Plain ASCII ".." instead of the Unicode ellipsis "…": on a terminal
        // without full glyph coverage (see this screen's own "glyph test /
        // install a Nerd Font" hint below), "…" can render as a bare "." —
        // which then reads as part of the adjacent number, e.g.
        // "36,113,830.36,115,878" (looks like one garbled figure instead of
        // a range). ".." never depends on font coverage. Reported live by
        // Viktor 2026-08-15.
        format!("[{}..{}]", group(s.sync_cursor), group(s.sync_cursor.saturating_add(2048)))
    } else { "—".into() };
    let fleet_total = app.fleet_nodes.len();
    let fleet_on = app.fleet_nodes.iter().filter(|n| n.online).count();
    let mesh = s.mesh_peer_count;
    let pid = std::process::id();
    let proof = if s.verify_break.is_some() { Span::styled("fold ✗ break".to_string(), Style::default().fg(C_RED).add_modifier(Modifier::BOLD)) }
        else if fold_ok { Span::styled("fold ✓ attests rest".to_string(), Style::default().fg(C_NEON_GREEN)) }
        else { Span::styled("fold … verifying".to_string(), Style::default().fg(C_GOLD)) };
    let pos = if s.pos_rate > 0.0 {
        Span::styled(format!("   ⛏{} blk/s verify", group(s.pos_rate.round() as u64)), Style::default().fg(C_GOLD))
    } else { Span::raw("") };

    let tlines = vec![
        if light {
            // 0.77 (#156): name the mode + what it holds, verbatim with the [F] toasts.
            Line::from(vec![
                dim("tip "), val(group(net_tip)),
                dim("   mode "), Span::styled("◇ LIGHT MONITOR — verifies tip (~10ms), holds nothing", Style::default().fg(C_NEON_CYAN)),
            ])
        } else if snap_mode {
            // recent-window monitor: `verified` is checkpoint-anchored (NOT a genesis spine), so
            // label it honestly as a tip-proof checkpoint badge — never "spine" (implies genesis).
            Line::from(vec![
                Span::styled("◇ LIGHT MONITOR ", Style::default().fg(C_NEON_CYAN).add_modifier(Modifier::BOLD)),
                dim("tip "), val(group(net_tip)),
                dim("   ✓ checkpoint "), Span::styled(group(spine), Style::default().fg(C_NEON_GREEN).add_modifier(Modifier::BOLD)),
                dim(" (tip-proof, not genesis)"),
                dim("   gap "), Span::styled(group(gap), Style::default().fg(if caught { C_NEON_GREEN } else { C_GOLD })),
            ])
        } else {
            // 0.77 (#156): the explicit [F] archive — genesis→tip, holding everything.
            // 2026-08-23 (grogu-sync-card-visibility): also show the raw FETCH frontier
            // (s.blocks_synced — downloaded, not yet verified) whenever it has run ahead
            // of the verified spine by a visible margin. Without this the operator only
            // sees the slow cryptographic-verify number and reads a large lag as "data
            // lost", when in fact the bytes are already on disk and verification is just
            // behind (see the SESSION HANDOFF memory — this exact confusion cost hours).
            let fetch_frontier = s.blocks_synced;
            let fetch_ahead = fetch_frontier.saturating_sub(spine);
            Line::from(vec![
                Span::styled("⛓ FULL ARCHIVE ", Style::default().fg(C_NEON_GOLD).add_modifier(Modifier::BOLD)),
                dim("tip "), val(group(net_tip)),
                dim("   spine "), Span::styled(format!("⛓{}", group(spine)), Style::default().fg(C_NEON_CYAN).add_modifier(Modifier::BOLD)),
                // settled_gap (vs the finalized tip) is the primary number — it's the
                // one that should read near-zero once truly caught up (raw tip minus
                // spine, vs the still-settling raw frontier, permanently sits ~512 by
                // design — see the const comment above). No secondary "(N unsettled)"
                // annotation here: this line is already at the ~57-column budget of an
                // 80-col terminal (the tmux test pane, and plausibly many real
                // terminals — Windows cmd.exe defaults to 80 too) and ratatui clips
                // rather than wraps, so a tacked-on annotation silently vanished in
                // testing. `tip` and `spine` are both still shown on this same line for
                // anyone who wants to compute the raw delta themselves.
                //
                // 2026-08-24: label changed from "gap" to "settled" — an operator read
                // literal tip-minus-spine off this same line (e.g. 2,068,882 - 2,068,371
                // = 511) and asked why the displayed number said 0. It wasn't wrong, just
                // unlabeled: this has always been settled_gap, not the raw subtraction.
                // The bare word "gap" invites exactly that mental math; "settled" points
                // at what's actually being measured without adding a column.
                dim("   settled "), Span::styled(group(settled_gap), Style::default().fg(if settled_gap < 16 { C_NEON_GREEN } else { C_GOLD })),
                if fetch_ahead > 1_000 {
                    Span::styled(format!("   fetched-to {} (+{} unverified)", group(fetch_frontier), group(fetch_ahead)), Style::default().fg(C_GOLD))
                } else { Span::raw("") },
            ])
        },
        if light {
            Line::from(vec![
                dim("backfill "), Span::styled("off — light monitor", Style::default().fg(C_DIM)),
                dim("   F "), Span::styled("= FULL ARCHIVE", Style::default().fg(C_NEON_CYAN).add_modifier(Modifier::BOLD)),
                dim(" (genesis→tip, holder hele kæden ~1GB)"),
            ])
        } else {
            Line::from(vec![
                dim("rate "), Span::styled(format!("{} blk/s", group(kf_rate.round() as u64)), Style::default().fg(C_NEON_GOLD).add_modifier(Modifier::BOLD)),
                dim(" ~kalman   eta "), Span::styled(if synced { "—".to_string() } else { fmt_eta(eta) }, Style::default().fg(C_NEON_CYAN).add_modifier(Modifier::BOLD)),
                dim("   chunk "), Span::styled(chunk, Style::default().fg(C_VBRIGHT)),
            ])
        },
        Line::from(vec![
            dim("fleet "), Span::styled(format!("{}/{}", fleet_on, fleet_total), Style::default().fg(if fleet_total > 0 && fleet_on == fleet_total { C_NEON_GREEN } else { C_GOLD })),
            dim("   mesh "), Span::styled(format!("{} peers", mesh), Style::default().fg(if mesh >= 4 { C_NEON_GREEN } else if mesh >= 1 { C_GOLD } else { C_RED })),
            dim("   "),
            Span::styled("Δ", Style::default().fg(if s.connected_delta { C_NEON_GREEN } else { C_DIM }).add_modifier(Modifier::BOLD)),
            Span::styled("Ε", Style::default().fg(if s.connected_epsilon { C_NEON_GREEN } else { C_DIM }).add_modifier(Modifier::BOLD)),
            dim(format!("   pid {}", pid)),
        ]),
        Line::from(vec![
            dim("proof "), proof,
            dim("   fetched "), Span::styled(group(s.fetched_total), Style::default().fg(C_GREEN)),
            pos,
            // 2026-08-23 (grogu-sync-card-visibility): self-heal budget — a bounded,
            // silent counter (MAX_HEALS=3) that only ever surfaced as an UNEXPLAINED
            // "SPINE BREAK — STUCK" once exhausted. Showing "heal N/3" while it's in
            // use turns that into a countdown instead of a surprise.
            if s.heal_attempts > 0 {
                Span::styled(format!("   🔧 heal {}/3", s.heal_attempts), Style::default().fg(if s.heal_attempts >= 3 { C_NEON_PINK } else { C_GOLD }).add_modifier(Modifier::BOLD))
            } else { Span::raw("") },
            // v7.1.57: adaptive frontier width — narrows the in-flight request to a
            // single block once the connection-churn threshold trips, to fit inside a
            // short-lived connection window. Surfaced so a slower-looking frontier
            // reads as "compensating", not "broken".
            if s.frontier_narrow {
                Span::styled("   ⚡ narrow-mode (compensating for connection instability)", Style::default().fg(C_NEON_CYAN))
            } else { Span::raw("") },
        ]),
        // v7.0.4: explicit DATA-INTEGRITY verdict — this node's OWN independent
        // verification of everything it holds. Honest scope: header spine parent-linked
        // back to genesis + the fold checkpoint + the tip fingerprint — NOT a full state
        // re-execution (a light/verifying client does not recompute every balance).
        {
            let (itxt, icol) = if s.verify_break.is_some() {
                (format!("⛓ INTEGRITY BROKEN — {}", s.verify_break.as_deref().unwrap_or("spine break")), C_RED)
            } else if synced {
                (format!("⛓ INTEGRITY VERIFIED — genesis→{} spine · fold ✓ · tip-fp ✓ (verified here, no trusted peer)", group(spine)), C_NEON_GREEN)
            } else if fold_ok {
                (format!("⛓ verifying integrity — spine {} / tip {} · fold ✓", group(spine), group(net_tip)), C_GOLD)
            } else if light {
                (format!("⛓ tip integrity — 10ms tip-proof {}", if fold_ok { "✓" } else { "…" }), C_NEON_CYAN)
            } else {
                (format!("⛓ verifying integrity — spine {} / tip {}", group(spine), group(net_tip)), C_DIM)
            };
            Line::from(vec![Span::styled(itxt, Style::default().fg(icol).add_modifier(Modifier::BOLD))])
        },
        // v7.0.4: NETWORK HEALTH gauge (Quillon k-parameter style, honest). SIGIL's
        // DagKnight is parameterless, so rather than fake a DagKnight-k the client can't
        // compute, this is the decentralization/health THIS node observes: connected
        // peers + liveness. Not a real network-wide DECENTRALIZED reading.
        //
        // 2026-08-23 (grogu-honest-network-health): `obs` is this node's OWN local peer
        // count, NOT a count of distinct block producers — the header schema carries no
        // producer identity the light client can durably tally (the "prod" tag exists
        // only in sigil-node's own live gossip JSON, not in SigilBlockHeaderV0). The old
        // label asserted "single-producer" outright whenever obs<3 — flagged live by an
        // operator whose node showed exactly that label while they knew the network
        // actually had two producers: this node simply wasn't well-connected, which says
        // nothing about how many producers exist. Labels now describe local peer
        // visibility only, never a specific producer-count claim the client can't back.
        {
            let live = kf_rate >= 1.0 || s.commit_rate >= 1.0 || (following && s.verify_break.is_none());
            let obs = s.peer_count.max(mesh); // THIS node's own connected peers, not network-wide producer count
            let (phase, pcol) = if obs >= 8 { ("WIDE MESH", C_NEON_GREEN) }
                else if obs >= 3 { ("GROWING MESH", C_GOLD) }
                else { ("LOW PEER VISIBILITY", C_NEON_PINK) };
            Line::from(vec![
                Span::styled("◈ NETWORK HEALTH ", Style::default().fg(C_NEON_CYAN).add_modifier(Modifier::BOLD)),
                Span::styled(phase, Style::default().fg(pcol).add_modifier(Modifier::BOLD)),
                dim("   peers "), Span::styled(format!("{}", obs), Style::default().fg(if obs >= 1 { C_NEON_GREEN } else { C_RED })),
                dim("   "), Span::styled(if live { "● live" } else { "○ stalled" }, Style::default().fg(if live { C_NEON_GREEN } else { C_RED }).add_modifier(Modifier::BOLD)),
            ])
        },
    ];
    f.render_widget(Paragraph::new(tlines), tele);

    // ── static starship (right) ──────────────────────────────────────────
    let (dtxt, dcol) = if light { ("MONITOR", C_NEON_CYAN) }
        else if synced { ("DOCKED", C_NEON_GREEN) }
        else if connecting { ("OFFLINE", C_RED) }
        else { ("ENGAGED", vcol) };
    let ship_lines = vec![
        Line::from(Span::styled("    ╱╲    ", Style::default().fg(C_NEON_CYAN))),
        Line::from(Span::styled("   ╱██╲   ", Style::default().fg(C_NEON_CYAN))),
        Line::from(Span::styled("  ▕████▏  ", Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD))),
        Line::from(Span::styled("  ▕◆◆◆▏  ", Style::default().fg(dcol))),
        Line::from(Span::styled("  ╱╲██╱╲  ", Style::default().fg(C_NEON_PINK))),
        Line::from(vec![dim(" drive "), Span::styled(dtxt, Style::default().fg(dcol).add_modifier(Modifier::BOLD))]),
    ];
    f.render_widget(Paragraph::new(ship_lines), ship);
}

/// How long an in-flight range may wait before the WAIT bar reads full/red.
/// Display-only: it does NOT cause a timeout, it just makes a range that is
/// aging toward one visible before it fails.
const WAIT_FULL_MS: u64 = 8_000;

fn fmt_eta_secs(secs: u64) -> String {
    if secs >= 3600 { format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60) }
    else if secs >= 60 { format!("{}m {:02}s", secs / 60, secs % 60) }
    else { format!("{secs}s") }
}

fn fmt_age(ms: u64) -> String {
    if ms >= 60_000 { format!("{}m{:02}s", ms / 60_000, (ms % 60_000) / 1000) }
    else if ms >= 1000 { format!("{:.1}s", ms as f64 / 1000.0) }
    else { format!("{ms}ms") }
}

/// [4] QUEUES — the torrent-client view of sync, full screen (operator request,
/// 2026-08-26: "get rid of this in the node tab, put it in a separate tab …
/// expand the table to give a nice overview over all the queues").
///
/// A torrent client's main screen lists every file in the transfer with its own
/// progress, peer and rate. Sync's equivalent "files" are fetch RANGES: fixed-
/// stride chunks of chain, each claimed from one peer, each independently in
/// flight. Before this, the card showed only aggregates plus a single cursor, so
/// a stalled sync and a slow one looked identical.
///
/// HONESTY NOTE — why the bar is WAIT, not PROGRESS: the sync store tracks a
/// range's lifecycle (in-flight → fetched → verified), not how many of its bytes
/// have landed, because a range is committed as one unit. There is no per-range
/// byte counter, so a "% downloaded" bar would be an invented number. The bar
/// instead shows how long the range has WAITED against [`WAIT_FULL_MS`] — real
/// data, and exactly what separates a healthy range from a wedging one. Ages are
/// recomputed on every read (see `sigil_sync::live_telemetry`); a snapshot-time
/// age froze at 0 and made every row look fresh, which defeated the point.
/// A range that has LEFT the live queue, kept so the table still shows it.
///
/// `sigil_sync::live_telemetry()` tracks only what is currently in flight or staged, so a
/// range that completes vanishes from the table on the very next frame. On a healthy fast
/// sync that means rows flash past too quickly to read anything from — the table shows
/// that work is happening but never what happened. Keeping completions for a couple of
/// minutes turns it into a record you can actually follow, and it costs one small
/// bounded deque.
#[derive(Clone)]
struct DoneRange {
    start: u64,
    end: u64,
    peer: String,
    /// How long it sat in flight before completing — the honest per-range duration.
    wait_ms: u64,
    at: Instant,
    /// Reached `Verified` before leaving, as opposed to merely being staged/dropped.
    verified: bool,
}

impl DoneRange {
    fn blocks(&self) -> u64 {
        self.end.saturating_sub(self.start)
    }
    /// Blocks per second for this range alone. The number that makes a row interesting:
    /// it says which peer is actually fast, not just which one answered.
    ///
    /// Returns `None` below [`RATE_FLOOR_MS`]. A range that "completed" in 22 ms did not
    /// move 10,000 blocks over the wire in 22 ms — it was a duplicate or cache-served
    /// reply, and dividing by that interval produces a headline like "peak 454,545 blk/s"
    /// that is pure arithmetic artifact. Reporting nothing is honest; reporting half a
    /// million blk/s teaches the operator to distrust the whole panel.
    fn rate(&self) -> Option<f64> {
        (self.wait_ms >= RATE_FLOOR_MS)
            .then(|| self.blocks() as f64 / (self.wait_ms as f64 / 1000.0))
    }
}

/// Below this, a completion interval is too short to be a real transfer measurement.
///
/// 10,000 blocks in 36 ms would be 277,000 blk/s — three orders of magnitude past anything
/// this wire does. Such rows are duplicate/cached replies, so their "rate" is meaningless.
const RATE_FLOOR_MS: u64 = 250;

/// How long a completed range stays on screen.
const DONE_RETAIN: Duration = Duration::from_secs(150);
/// Hard cap, so a very fast sync cannot grow this without bound.
const DONE_CAP: usize = 256;

type SeenMap = std::collections::HashMap<(u64, u64), (String, u64, bool)>;

fn done_log() -> &'static std::sync::Mutex<std::collections::VecDeque<DoneRange>> {
    static D: std::sync::OnceLock<std::sync::Mutex<std::collections::VecDeque<DoneRange>>> =
        std::sync::OnceLock::new();
    D.get_or_init(|| std::sync::Mutex::new(std::collections::VecDeque::new()))
}

fn seen_map() -> &'static std::sync::Mutex<SeenMap> {
    static S: std::sync::OnceLock<std::sync::Mutex<SeenMap>> = std::sync::OnceLock::new();
    S.get_or_init(|| std::sync::Mutex::new(SeenMap::new()))
}

/// Diff this frame's live rows against the previous frame; anything that disappeared has
/// completed, so move it into the retained log.
fn reap_completed(rows: &[sigil_sync::RangeRow]) {
    let now = Instant::now();
    let mut current: SeenMap = SeenMap::new();
    for r in rows {
        let (peer, verified) = match &r.state {
            sigil_sync::RangeState::InFlight { peer, .. } => (peer.clone(), false),
            sigil_sync::RangeState::Fetched { .. } => (String::new(), false),
            sigil_sync::RangeState::Verified => (String::new(), true),
        };
        current.insert((r.start, r.end), (peer, r.age_ms, verified));
    }
    let Ok(mut seen) = seen_map().lock() else { return };
    let Ok(mut done) = done_log().lock() else { return };
    for (k, (peer, age_ms, verified)) in seen.iter() {
        if !current.contains_key(k) {
            done.push_front(DoneRange {
                start: k.0,
                end: k.1,
                peer: peer.clone(),
                wait_ms: *age_ms,
                at: now,
                verified: *verified,
            });
        }
    }
    *seen = current;
    while done.len() > DONE_CAP {
        done.pop_back();
    }
    while done.back().is_some_and(|d| now.duration_since(d.at) > DONE_RETAIN) {
        done.pop_back();
    }
}

pub(crate) fn draw_queues_tab(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let s = &app.p2p_state;
    let t = sigil_sync::live_telemetry();
    // Move anything that left the live queue since the last frame into the retained log,
    // so the table keeps showing completed work instead of blanking it instantly.
    reap_completed(&t.rows);

    // `card_block` wants a &'static str. Build the title once and keep it — leaking on
    // every frame would be a slow memory leak in a program that redraws continuously.
    static QUEUES_TITLE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let block = card_block(
        QUEUES_TITLE.get_or_init(|| format!(" ◈ FETCH QUEUES · {}", build_network_id())).as_str(),
        C_NEON_CYAN,
    );
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 4 {
        return;
    }

    let inflight: Vec<&sigil_sync::RangeRow> =
        t.rows.iter().filter(|r| matches!(r.state, sigil_sync::RangeState::InFlight { .. })).collect();
    let staged: Vec<&sigil_sync::RangeRow> =
        t.rows.iter().filter(|r| matches!(r.state, sigil_sync::RangeState::Fetched { .. })).collect();

    let tip = s.peer_best_height.max(app.target_height);
    let verified = t.verified_to.max(s.verified);
    let gap = tip.saturating_sub(verified);
    let eta = if t.rate >= 1.0 && gap > 0 { Some((gap as f64 / t.rate) as u64) } else { None };

    // ── header: the whole pipeline in two lines ─────────────────────────
    let [head, mid] = Layout::vertical([Constraint::Length(4), Constraint::Min(0)]).areas(inner);
    let mut hl: Vec<Line> = Vec::new();
    hl.push(Line::from(vec![
        Span::styled("⇣ ", Style::default().fg(C_NEON_GREEN)),
        Span::styled(format!("{}", inflight.len()), Style::default().fg(if inflight.is_empty() { C_DIM } else { C_NEON_GREEN }).add_modifier(Modifier::BOLD)),
        dim(" fetching   "),
        Span::styled("✓ ", Style::default().fg(C_GOLD)),
        Span::styled(format!("{}", staged.len()), Style::default().fg(C_GOLD).add_modifier(Modifier::BOLD)),
        dim(" staged   chunk "),
        Span::styled(group(t.chunk), Style::default().fg(C_VBRIGHT)),
        dim("   rate "),
        Span::styled(
            if t.rate >= 1.0 { format!("{} blk/s", group(t.rate.round() as u64)) } else { "—".into() },
            Style::default().fg(if t.rate >= 1.0 { C_NEON_GREEN } else { C_DIM }).add_modifier(Modifier::BOLD),
        ),
        dim("   eta "),
        Span::styled(eta.map(fmt_eta_secs).unwrap_or_else(|| "—".into()), Style::default().fg(C_NEON_CYAN)),
    ]));
    hl.push(Line::from(vec![
        dim("verified "), Span::styled(group(verified), Style::default().fg(C_NEON_CYAN)),
        dim("   fetched→ "), Span::styled(group(t.fetched_to), Style::default().fg(C_GOLD)),
        dim("   tip "), Span::styled(group(tip), Style::default().fg(C_VBRIGHT)),
        dim("   gap "), Span::styled(group(gap), Style::default().fg(if gap > 100_000 { C_NEON_PINK } else { C_DIM })),
        dim("   peers "), Span::styled(format!("{}", s.peer_count), Style::default().fg(if s.peer_count > 0 { C_NEON_GREEN } else { C_RED })),
    ]));
    // frontier bar: verified → fetched → tip, so the two watermarks are visible as one picture
    {
        let w = (head.width as usize).saturating_sub(2).max(10);
        let fv = if tip > 0 { (verified as f64 / tip as f64).clamp(0.0, 1.0) } else { 0.0 };
        let ff = if tip > 0 { (t.fetched_to as f64 / tip as f64).clamp(0.0, 1.0) } else { 0.0 };
        let nv = (fv * w as f64).round() as usize;
        let nf = ((ff * w as f64).round() as usize).saturating_sub(nv);
        hl.push(Line::from(vec![
            Span::styled("█".repeat(nv.min(w)), Style::default().fg(C_NEON_CYAN)),
            Span::styled("▒".repeat(nf.min(w.saturating_sub(nv))), Style::default().fg(C_GOLD)),
            Span::styled("░".repeat(w.saturating_sub(nv + nf)), Style::default().fg(C_DIM)),
        ]));
        hl.push(Line::from(vec![
            Span::styled("█", Style::default().fg(C_NEON_CYAN)), dim(" verified   "),
            Span::styled("▒", Style::default().fg(C_GOLD)), dim(" fetched, awaiting verify   "),
            Span::styled("░", Style::default().fg(C_DIM)), dim(" not downloaded"),
        ]));
    }
    f.render_widget(Paragraph::new(hl), head);

    // ── body: the queue table (left) + peer/session panes (right) ───────
    let [tbl, side] = Layout::horizontal([Constraint::Min(40), Constraint::Length(38)]).spacing(1).areas(mid);

    // ---- the queue table ----
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        format!(" {:<3} {:<26} {:>7} {:<11} {:<12} {:<18} {:>9} {:>8}",
            "#", "RANGE", "BLOCKS", "WAIT", "STATE", "PEER", "blk/s", "AGE"),
        Style::default().fg(C_DIM).add_modifier(Modifier::BOLD),
    )));
    if t.rows.is_empty() {
        lines.push(Line::from(Span::styled(
            if t.chunk == 0 { "  idle — the sync engine is not running (light monitor; press F for full sync)" }
            else { "  empty — nothing in flight (at tip, or the wire is idle)" },
            Style::default().fg(C_DIM),
        )));
    } else {
        let mut rows: Vec<&sigil_sync::RangeRow> = t.rows.iter().collect();
        // in-flight first (that is where trouble shows), then by height
        rows.sort_by_key(|r| (!matches!(r.state, sigil_sync::RangeState::InFlight { .. }), r.start));
        let room = (tbl.height as usize).saturating_sub(2);
        for (i, r) in rows.iter().take(room).enumerate() {
            let (stxt, scol, peer) = match &r.state {
                sigil_sync::RangeState::InFlight { peer, fanout, .. } => {
                    // fanout 0 = claimed but not yet dispatched; >1 = frontier
                    // redundancy (same range sent to several peers, first good
                    // reply wins), so name the first and show the count.
                    let p = if *fanout == 0 {
                        "— not sent yet".to_string()
                    } else {
                        let short = if peer.len() > 13 { format!("{}…", &peer[..12]) } else { peer.clone() };
                        if *fanout > 1 { format!("{short} ×{fanout}") } else { short }
                    };
                    ("⇣ fetching", C_NEON_GREEN, p)
                }
                sigil_sync::RangeState::Fetched { .. } => ("✓ staged", C_GOLD, "—".into()),
                sigil_sync::RangeState::Verified => ("⛓ verified", C_NEON_CYAN, "—".into()),
            };
            let (bar, bcol) = match &r.state {
                sigil_sync::RangeState::InFlight { .. } => {
                    let frac = (r.age_ms as f64 / WAIT_FULL_MS as f64).clamp(0.0, 1.0);
                    let w = 9usize;
                    let fill = (frac * w as f64).round() as usize;
                    let c = if frac >= 0.85 { C_RED } else if frac >= 0.5 { C_GOLD } else { C_NEON_GREEN };
                    ("█".repeat(fill.min(w)) + &"░".repeat(w.saturating_sub(fill)), c)
                }
                _ => ("─────────".to_string(), C_DIM),
            };
            // BLOCKS and blk/s per range: the table used to say a range was in flight
            // and how long it had waited, but not how BIG it was — so a slow row and a
            // huge row looked identical. Per-range throughput is what identifies a slow
            // peer rather than merely a slow moment.
            let blocks = r.end.saturating_sub(r.start);
            let live_rate = if r.age_ms > 0 { blocks as f64 / (r.age_ms as f64 / 1000.0) } else { 0.0 };
            let rate_txt = if matches!(r.state, sigil_sync::RangeState::InFlight { .. }) && live_rate > 0.0 {
                format!("{:.0}", live_rate)
            } else { "—".to_string() };
            let rate_col = if live_rate >= 1000.0 { C_NEON_GREEN }
                else if live_rate >= 100.0 { C_GOLD }
                else { C_DIM };
            lines.push(Line::from(vec![
                Span::styled(format!(" {:<3} ", i + 1), Style::default().fg(C_DIM)),
                Span::styled(format!("{:<26} ", format!("{}..{}", group(r.start), group(r.end))), Style::default().fg(C_VBRIGHT)),
                Span::styled(format!("{:>7} ", group(blocks)), Style::default().fg(C_NEON_CYAN)),
                Span::styled(format!("{bar:<11}"), Style::default().fg(bcol)),
                Span::styled(format!("{stxt:<12}"), Style::default().fg(scol)),
                Span::styled(format!("{peer:<18}"), Style::default().fg(C_DIM)),
                Span::styled(format!("{rate_txt:>9} "), Style::default().fg(rate_col)),
                Span::styled(format!("{:>8}", fmt_age(r.age_ms)), Style::default().fg(if r.age_ms > WAIT_FULL_MS { C_RED } else { C_DIM })),
            ]));
        }
        if rows.len() > room {
            lines.push(Line::from(Span::styled(
                format!("  … {} more — enlarge the window", rows.len() - room),
                Style::default().fg(C_DIM),
            )));
        }
    }

    // ── COMPLETED: ranges that have left the live queue ─────────────────────────────
    //
    // Kept for DONE_RETAIN so the table reads as a record rather than a strobe. Each row
    // carries what the live view could never show, because the range was gone before it
    // could: how many blocks it actually carried, how long it took start to finish, and
    // the throughput that implies — which is the number that tells a fast peer from a
    // slow one.
    if let Ok(done) = done_log().lock() {
        let used = lines.len();
        let left = (tbl.height as usize).saturating_sub(used + 1);
        if left > 1 && !done.is_empty() {
            let now = Instant::now();
            let fresh: Vec<&DoneRange> =
                done.iter().filter(|d| now.duration_since(d.at) <= DONE_RETAIN).collect();
            if !fresh.is_empty() {
                let total_blocks: u64 = fresh.iter().map(|d| d.blocks()).sum();
                let best = fresh.iter().filter_map(|d| d.rate()).fold(0.0_f64, f64::max);
                // How many rows were too fast to measure — usually the duplicate-reply
                // signature, which is worth surfacing rather than hiding.
                let instant = fresh.iter().filter(|d| d.rate().is_none()).count();
                // SAY WHEN THE COUNT IS THE CAP. `fresh.len()` maxes out at DONE_CAP, so a
                // saturated log reads "256 ranges" whatever the truth is — a number that
                // looks measured and is not. An operator reading 256 ranges / 2,560,000
                // blocks against 10,707 actually fetched deserves to know which of those
                // figures is a ceiling.
                let capped = fresh.len() >= DONE_CAP;
                lines.push(Line::from(vec![
                    Span::styled(" ✔ COMPLETED", Style::default().fg(C_NEON_GREEN).add_modifier(Modifier::BOLD)),
                    dim(&format!(" — last {}s: ", DONE_RETAIN.as_secs())),
                    Span::styled(
                        if capped { format!("{}+", fresh.len()) } else { format!("{}", fresh.len()) },
                        Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD),
                    ),
                    dim(if capped { " ranges (log full) " } else { " ranges  " }),
                    Span::styled(group(total_blocks), Style::default().fg(C_NEON_CYAN).add_modifier(Modifier::BOLD)),
                    dim(" blocks  peak "),
                    Span::styled(
                        if best > 0.0 { format!("{best:.0} blk/s") } else { "—".into() },
                        Style::default().fg(C_GOLD),
                    ),
                    dim(if instant > 0 { "  instant " } else { "" }),
                    Span::styled(
                        if instant > 0 { format!("{instant}") } else { String::new() },
                        Style::default().fg(C_NEON_PINK),
                    ),
                    dim(if instant > 0 { " (cached/dup — not measurable)" } else { "" }),
                ]));
                for d in fresh.iter().take(left.saturating_sub(1)) {
                    let age = now.duration_since(d.at).as_secs();
                    let rate = d.rate();
                    let rcol = match rate {
                        Some(r) if r >= 1000.0 => C_NEON_GREEN,
                        Some(r) if r >= 100.0 => C_GOLD,
                        Some(_) => C_DIM,
                        None => C_NEON_PINK,
                    };
                    let rate_txt = match rate {
                        Some(r) => format!("{r:.0}"),
                        None => "inst".to_string(),
                    };
                    // Fade the marker with age so the eye lands on what just happened.
                    let mark_col = if age < 10 { C_NEON_GREEN } else if age < 45 { C_GOLD } else { C_DIM };
                    let peer = if d.peer.is_empty() { "—".to_string() }
                        else if d.peer.len() > 17 { format!("{}…", &d.peer[..16]) }
                        else { d.peer.clone() };
                    lines.push(Line::from(vec![
                        Span::styled(if d.verified { "  ⛓ " } else { "  ✔ " }, Style::default().fg(mark_col)),
                        Span::styled(format!("{:<26} ", format!("{}..{}", group(d.start), group(d.end))), Style::default().fg(C_DIM)),
                        Span::styled(format!("{:>7} ", group(d.blocks())), Style::default().fg(C_NEON_CYAN)),
                        Span::styled(format!("{:<11}", fmt_age(d.wait_ms)), Style::default().fg(C_DIM)),
                        Span::styled(format!("{:<12}", if d.verified { "⛓ verified" } else { "✔ done" }), Style::default().fg(mark_col)),
                        Span::styled(format!("{peer:<18}"), Style::default().fg(C_DIM)),
                        Span::styled(format!("{rate_txt:>9} "), Style::default().fg(rcol)),
                        Span::styled(format!("{:>8}", format!("{age}s ago")), Style::default().fg(C_DIM)),
                    ]));
                }
            }
        }
    }
    f.render_widget(Paragraph::new(lines), tbl);

    // ---- side: peers holding ranges + session totals ----
    let [pane_peers, pane_sess] = Layout::vertical([Constraint::Percentage(55), Constraint::Min(0)]).areas(side);

    let mut pl: Vec<Line> = vec![Line::from(Span::styled(
        "◆ PEERS HOLDING RANGES", Style::default().fg(C_NEON_CYAN).add_modifier(Modifier::BOLD)))];
    {
        use std::collections::BTreeMap;
        let mut by_peer: BTreeMap<String, (usize, u64)> = BTreeMap::new();
        let mut undispatched = 0usize;
        for r in &inflight {
            if let sigil_sync::RangeState::InFlight { peer, fanout, .. } = &r.state {
                // fanout 0 = claimed but not yet sent to anyone. Its `peer` is
                // still the claim placeholder, so grouping it here would invent
                // a peer that does not exist. Count it separately instead.
                if *fanout == 0 {
                    undispatched += 1;
                    continue;
                }
                let e = by_peer.entry(peer.clone()).or_insert((0, 0));
                e.0 += 1;
                e.1 = e.1.max(r.age_ms);
            }
        }
        if by_peer.is_empty() {
            pl.push(Line::from(Span::styled("  none dispatched", Style::default().fg(C_DIM))));
        }
        for (peer, (n, oldest)) in by_peer.iter().take(pane_peers.height.saturating_sub(2) as usize) {
            let p = if peer.len() > 16 { format!("{}…", &peer[..15]) } else { peer.clone() };
            pl.push(Line::from(vec![
                Span::styled(format!("  {p:<17}"), Style::default().fg(C_VBRIGHT)),
                Span::styled(format!("{n:>3} "), Style::default().fg(C_NEON_GREEN).add_modifier(Modifier::BOLD)),
                dim("rng  oldest "),
                Span::styled(fmt_age(*oldest), Style::default().fg(if *oldest > WAIT_FULL_MS { C_RED } else { C_DIM })),
            ]));
        }
        if undispatched > 0 {
            pl.push(Line::from(vec![
                dim("  "),
                Span::styled(format!("{undispatched}"), Style::default().fg(C_GOLD).add_modifier(Modifier::BOLD)),
                dim(" claimed, awaiting dispatch"),
            ]));
        }
    }
    f.render_widget(Paragraph::new(pl), pane_peers);

    let mb = s.sync_total as f64 / (1024.0 * 1024.0);
    let sl: Vec<Line> = vec![
        Line::from(Span::styled("◆ SESSION", Style::default().fg(C_NEON_CYAN).add_modifier(Modifier::BOLD))),
        Line::from(vec![dim("  blocks fetched  "), Span::styled(group(s.fetched_total), Style::default().fg(C_VBRIGHT))]),
        Line::from(vec![dim("  range blocks    "), Span::styled(group(t.fetched_blocks), Style::default().fg(C_VBRIGHT))]),
        Line::from(vec![dim("  ranges tracked  "), Span::styled(format!("{}", t.rows.len()), Style::default().fg(C_VBRIGHT))]),
        Line::from(vec![dim("  commit rate     "), Span::styled(
            if s.commit_rate >= 1.0 { format!("{} blk/s", group(s.commit_rate.round() as u64)) } else { "—".into() },
            Style::default().fg(C_NEON_GREEN))]),
        Line::from(vec![dim("  data            "), Span::styled(if mb >= 1.0 { format!("{mb:.0} MB") } else { "—".into() }, Style::default().fg(C_DIM))]),
    ];
    f.render_widget(Paragraph::new(sl), pane_sess);
}

/// v0.26: read at most the last `max_bytes` of a (possibly huge) log file — seek to the
/// tail instead of slurping the whole thing, so the Sync Log tab stays O(1) per frame.
pub(crate) fn read_log_tail(path: &str, max_bytes: u64) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = match std::fs::File::open(path) { Ok(f) => f, Err(_) => return String::new() };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(max_bytes);
    if f.seek(SeekFrom::Start(start)).is_err() { return String::new(); }
    let mut buf = Vec::with_capacity(max_bytes as usize);
    let _ = f.take(max_bytes).read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

/// [3] Results — settled work + QUG payouts from the swarm.
/// v0.25.5: the Sync Log tab — a live sync-state header + a tail of the sync events
/// (peer connects, fast-snap/track-tip, tip-fetch, backfill chunks, timeouts) read from
/// ~/.sigil-top.log, so the operator can SEE what sync is doing, not just a bar.
pub(crate) fn render_sync_log(app: &App) -> Paragraph<'static> {
    let s = &app.p2p_state;
    let tip = s.peer_best_height.max(app.target_height);
    let gap = tip.saturating_sub(s.blocks_synced);
    let mut lines: Vec<Line> = Vec::new();
    // v0.26: LIVE/STALE badge — if the tip-poller hasn't gotten a fresh tip in >12s
    // (oracle down / partition), say so instead of a falsely confident "AT TIP".
    let stale = s.last_tip_at.map(|t| t.elapsed().as_secs() > 12).unwrap_or(true);
    let (badge, bcol) = if stale {
        (sa(format!(" (STALE){}", s.last_tip_at.map(|t| format!(" ({}s)", t.elapsed().as_secs())).unwrap_or_default())), C_RED)
    } else { (sa(" ● LIVE"), C_GREEN) };
    let pct = if tip > 0 { (s.blocks_synced as f64 / tip as f64 * 100.0).clamp(0.0, 100.0) } else { 0.0 };
    lines.push(Line::from(vec![
        Span::styled(sa(" ▸ SYNC STATE"), Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD)),
        Span::styled(badge, Style::default().fg(bcol).add_modifier(Modifier::BOLD)),
        Span::raw(sa("   ")), Span::styled(format!("{pct:5.1}%"), Style::default().fg(if pct >= 99.9 { C_GREEN } else { C_GOLD }).add_modifier(Modifier::BOLD)),
    ]));
    // v7.0.13: a real progress bar so you can see position at a glance, not just the number.
    let barw = 46usize;
    let filled = ((pct / 100.0) * barw as f64).round() as usize;
    let bar: String = std::iter::repeat('█').take(filled).chain(std::iter::repeat('░').take(barw.saturating_sub(filled))).collect();
    lines.push(Line::from(vec![
        Span::raw(sa("  ")), Span::styled(bar, Style::default().fg(if pct >= 99.9 { C_GREEN } else { C_CYAN })),
    ]));
    let eta = if app.p2p_rate >= 1.0 && gap > 0 {
        let secs = gap as f64 / app.p2p_rate; let h = (secs / 3600.0) as u64; let m = ((secs % 3600.0) / 60.0) as u64;
        if h > 0 { format!("{h}h {m}m") } else { format!("{m}m") }
    } else if gap == 0 { "—".into() } else { "…".into() };
    lines.push(Line::from(vec![
        Span::raw(sa("  height ")), Span::styled(group(s.blocks_synced), Style::default().fg(C_GREEN).add_modifier(Modifier::BOLD)),
        Span::raw(sa("  tip ")), Span::styled(group(tip), Style::default().fg(C_CYAN)),
        Span::raw(sa("  gap ")), Span::styled(group(gap), Style::default().fg(if gap < 8 { C_GREEN } else { C_GOLD })),
        Span::raw(sa("  rate ")), Span::styled(format!("{:.0} blk/s", app.p2p_rate), Style::default().fg(C_CYAN)),
        Span::raw(sa("  eta ")), Span::styled(sa(eta), Style::default().fg(C_DIM)),
    ]));
    // v0.57 (LANE-M): label honestly. In recent-window (light) mode `verified` is anchored at the
    // checkpoint base — a tip-proof, NOT a spine linked to genesis — so don't call it "verified
    // spine" (which implies genesis linkage and reads as a stuck full-spine when it can't reach 0).
    let (vlabel, vcol) = if s.light_mode {
        (sa("  ✓ checkpoint-verified "), C_CYAN) // tip-proof from the snap base, not genesis
    } else {
        (sa("  ⛓ verified spine "), C_GREEN)     // full-sync: genuine genesis-linked spine
    };
    lines.push(Line::from(vec![
        Span::styled(vlabel, Style::default().fg(vcol)), Span::styled(group(s.verified), Style::default().fg(vcol)),
        Span::raw(sa("   peers ")), Span::styled(format!("{}", s.peer_count), Style::default().fg(C_CYAN)),
        Span::raw(sa("   ")), Span::styled(sa(if s.connected_delta { "Δ" } else { "·" }), Style::default().fg(C_GOLD)),
        Span::styled(sa(if s.connected_epsilon { "Ε" } else { "·" }), Style::default().fg(C_GOLD)),
    ]));
    // 0.77 (#156): name the live mode + exactly what it holds ([F] flips it live).
    let (mlabel, mcol) = if s.light_mode {
        (sa("  mode ◇ LIGHT MONITOR — verifies tip, holds nothing ([F] = full archive)"), C_CYAN)
    } else {
        (sa("  mode ⛓ FULL ARCHIVE — genesis→tip, holds everything ([F] = light monitor)"), C_GOLD)
    };
    lines.push(Line::from(Span::styled(mlabel, Style::default().fg(mcol))));
    // v7.0.13: say WHY it's stuck (or that it's healthy) up front, instead of burying it in the tail.
    if let Some((h, reason)) = &s.sync_failure {
        lines.push(Line::from(Span::styled(sa(format!("  ✗ SPINE BREAK — STUCK at h={h}: {}", trunc(reason, 88))), Style::default().fg(C_RED).add_modifier(Modifier::BOLD))));
    } else if let Some(b) = &s.verify_break {
        lines.push(Line::from(Span::styled(sa(format!("  ⚠ spine break: {}", trunc(b, 98))), Style::default().fg(C_RED))));
    } else if !s.stall_reason.is_empty() {
        lines.push(Line::from(Span::styled(sa(format!("  ⚠ stalled: {}", trunc(&s.stall_reason, 98))), Style::default().fg(C_GOLD))));
    } else if gap < 8 && !stale {
        lines.push(Line::from(Span::styled(sa("  ✓ healthy — at tip, spine advancing, no breaks"), Style::default().fg(C_GREEN))));
    } else {
        lines.push(Line::from(Span::styled(sa("  ⬇ syncing — spine advancing, no breaks detected"), Style::default().fg(C_CYAN))));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(" ▸ SYNC LOG  (newest at bottom)", Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD))));
    // 2026-08-23 (grogu-windows-synclog-path): native Windows (cmd.exe/PowerShell,
    // not Git Bash) has no `HOME` env var — this used to fall straight through to a
    // bare relative "sigil-top.log" (cwd), while the actual writer (main.rs's tlog!)
    // already falls back to `%TEMP%\sigil-top.log`. Result: the Sync Log tab read a
    // file that was never written, so it showed "no sync activity logged yet" FOREVER
    // even while sync was genuinely running (confirmed live: a Windows client at 7.8%,
    // 1094 blk/s, still showed the empty placeholder). Mirror main.rs's HOME→TEMP
    // fallback exactly so both sides agree on the same path.
    let path = std::env::var("HOME")
        .map(|h| format!("{h}/.sigil-top.log"))
        .or_else(|_| std::env::var("TEMP").map(|t| format!("{t}\\sigil-top.log")))
        .unwrap_or_else(|_| "sigil-top.log".into());
    // v0.26: read only the LAST 16 KB (not the whole file) — O(1) per frame, never
    // O(log-size), which would freeze the UI as the log grows over a 24/7 run.
    let body = read_log_tail(&path, 16 * 1024);
    let recent: Vec<String> = body.lines().rev()
        .filter(|l| l.contains("[DBG]") || l.contains("[PANIC]") || l.contains("[sync]")
            || l.contains("[tipfetch]") || l.contains("[D]") || l.contains("[p2p-sync]")
            || l.contains("[tip]") || l.contains("[render]"))
        .take(44)
        .map(|l| l.to_string())
        .collect();
    if recent.is_empty() {
        lines.push(Line::from(Span::styled("  (no sync activity logged yet — connecting to the mesh…)", Style::default().fg(C_DIM))));
    }
    for l in recent.iter().rev() {
        let t = l.trim();
        let col = if t.contains("[PANIC]") { C_RED }
            else if t.contains("[DBG]") { C_VBRIGHT }
            else if t.contains("track tip") || t.contains("fast-snap") || t.contains("[sync]") { C_GOLD }
            else if t.contains("[tipfetch]") { C_CYAN }
            else if t.contains("TIMEOUT") || t.contains("err") { C_RED }
            else if t.contains("peer +") { C_GREEN }
            else { C_DIM };
        lines.push(Line::from(Span::styled(format!("  {}", trunc(t, 116)), Style::default().fg(col))));
    }
    Paragraph::new(lines)
}
