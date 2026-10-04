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
    let synced = is_synced(caught, settled_gap, fold_ok, s.verify_break.is_some());
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
    // An unservable frontier outranks every other verdict: it is the one state that looks
    // completely healthy (ranges in flight, bytes moving, a plausible rate and ETA) while
    // being permanently unable to finish. It must never again read as "SYNCING".
    let (vtext, vcol) = match s.unservable_frontier {
        Some(_) if !light => ("🚧 UNSERVABLE HISTORY", C_NEON_PINK),
        _ => (vtext, vcol),
    };

    // SPINE-BREAK fix: a CONFIRMED watchdog/fatal failure outranks every other headline —
    // the operator must never miss it (this is what replaces the old silent ~499k rate-0).
    let (vtext, vcol) = if s.sync_failure.is_some() {
        ("✗ SPINE BREAK — STUCK", C_NEON_PINK)
    } else { (vtext, vcol) };

    // state-themed border; title chip stays neon-cyan
    // The chain this build follows — not a hard-coded "sigil-g0": the header bar says
    // sigil-g2 and this card said sigil-g0 on the same screen (operator screenshot 2026-09-06).
    let sync_title = format!(" ◇ SYNC · {}", crate::build_network_id());
    let block = card_block(&sync_title, C_NEON_CYAN)
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
    let mesh_started = app.p2p_sync.is_some();
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
            dim("   mesh "), if mesh_started {
                Span::styled(format!("{} peers", mesh), Style::default().fg(if mesh >= 4 { C_NEON_GREEN } else if mesh >= 1 { C_GOLD } else { C_RED }))
            } else {
                // Not joined on purpose: the light monitor follows the tip over HTTPS and only dials
                // the peer network after [F] (or SIGIL_SPINE_SYNC=1). "0 peers" in red said "broken".
                Span::styled("off — light monitor (F joins)".to_string(), Style::default().fg(C_GOLD))
            },
            // v10.4.17: a 0-peer mesh must say WHY — the dial error was only in the logfile, so
            // "mesh 0 peers" gave an operator nothing to act on (blocked port? dead seed?).
            if mesh_started && mesh == 0 && s.dial_failures > 0 {
                Span::styled(
                    format!("  ✗{} dial fails: {}", s.dial_failures,
                        if s.last_dial_err.is_empty() { s.stall_reason.trim_start_matches("✗ dial failed: ").chars().take(70).collect::<String>() } else { s.last_dial_err.clone() }),
                    Style::default().fg(C_RED))
            } else { Span::raw("") },
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
                (integrity_verified_text(snap_mode, &group(spine)), C_NEON_GREEN)
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
            let (phase, pcol) = if !mesh_started && obs == 0 { ("MESH OFF · LIGHT MONITOR", C_GOLD) }
                else if obs >= 8 { ("WIDE MESH", C_NEON_GREEN) }
                else if obs >= 3 { ("GROWING MESH", C_GOLD) }
                else { ("LOW PEER VISIBILITY", C_NEON_PINK) };
            Line::from(vec![
                Span::styled("◈ NETWORK HEALTH ", Style::default().fg(C_NEON_CYAN).add_modifier(Modifier::BOLD)),
                Span::styled(phase, Style::default().fg(pcol).add_modifier(Modifier::BOLD)),
                dim("   peers "), if !mesh_started && obs == 0 { Span::styled("— (not joined)".to_string(), Style::default().fg(C_GOLD)) } else { Span::styled(format!("{}", obs), Style::default().fg(if obs >= 1 { C_NEON_GREEN } else { C_RED })) },
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

/// "SYNCED" needs the SETTLED gap small, not just `gap < 16_384`: at ~8 blk/s that is 34 minutes of
/// chain, and a node that far behind used to read "◆ SYNCED" with no ETA. `settled_gap` already
/// discounts the ~512-block probation window, so caught-up reads near zero.
const SYNCED_MAX_SETTLED_GAP: u64 = 1024;

/// The verified-integrity line. In the recent-window monitor (`snap_mode`) the spine is anchored at a
/// CHECKPOINT, not genesis — claiming "genesis→N" there overstated what this node had checked.
fn integrity_verified_text(snap_mode: bool, spine: &str) -> String {
    if snap_mode {
        format!("⛓ TIP VERIFIED — recent window from a checkpoint (not genesis) · {spine} · fold ✓ · tip-fp ✓")
    } else {
        format!("⛓ INTEGRITY VERIFIED — genesis→{spine} spine · fold ✓ · tip-fp ✓ (verified here, no trusted peer)")
    }
}

fn is_synced(caught: bool, settled_gap: u64, fold_ok: bool, broken: bool) -> bool {
    caught && settled_gap < SYNCED_MAX_SETTLED_GAP && fold_ok && !broken
}

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
    /// Network tip when the range completed. Blocks above it did not exist yet, so the
    /// reply cannot have carried them.
    tip: u64,
}

/// How many blocks of `[start, end)` could exist at network height `tip`. A sync range near
/// the live tip asks for 10,000 blocks and receives only the few hundred that exist; counting
/// the request instead of the reply is what made the Queues tab read 7,680–22,936 blk/s on a
/// chain that makes 8 a second (Viktor's photo, 2026-09-25: every row was exactly 10,000/WAIT).
/// `tip == 0` means unknown: fall back to the requested size rather than invent a number.
pub(crate) fn existing_blocks(start: u64, end: u64, tip: u64) -> u64 {
    let cap = if tip == 0 { end } else { end.min(tip.saturating_add(1)) };
    cap.saturating_sub(start)
}

impl DoneRange {
    fn blocks(&self) -> u64 {
        existing_blocks(self.start, self.end, self.tip)
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
fn reap_completed(rows: &[sigil_sync::RangeRow], tip: u64) {
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
                tip,
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
    let net_tip = s.peer_best_height.max(app.target_height);
    // Move anything that left the live queue since the last frame into the retained log,
    // so the table keeps showing completed work instead of blanking it instantly.
    reap_completed(&t.rows, net_tip);

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
    // One extra header row when the unservable-frontier banner is present.
    let head_rows = if app.p2p_state.unservable_frontier.is_some() { 5 } else { 4 };
    let [head, mid] = Layout::vertical([Constraint::Length(head_rows), Constraint::Min(0)]).areas(inner);
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
        // "peers 0" on its own is mute: it cannot distinguish "the dials are being
        // REFUSED by this network" from "we simply have not dialled yet". Both look
        // identical and both stall the queue below at "nothing in flight", which is
        // exactly how an operator ends up staring at a frozen sync with no lead
        // (2026-09-06). flux-p2p already retries the bootstrap set at 1/2/4s while the
        // pool is empty and block_sync already records the failure count + newest error
        // — this just says it out loud on the panel the operator is actually looking at.
        Span::styled(zero_peer_reason(s), Style::default().fg(C_NEON_PINK)),
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
    if let Some(h) = s.unservable_frontier {
        // Name the height, say why it cannot resolve itself, and give the one action that
        // helps. A verdict with no remedy is how an operator ends up staring at a parked
        // sync for an hour.
        hl.push(Line::from(vec![
            Span::styled(" 🚧 ", Style::default().fg(C_NEON_PINK)),
            Span::styled(
                format!("no reachable peer serves h={} ", group(h)),
                Style::default().fg(C_NEON_PINK).add_modifier(Modifier::BOLD),
            ),
            dim("— re-anchored above it; this is NOT a genesis archive. [F] = light monitor."),
        ]));
    }
    f.render_widget(Paragraph::new(hl), head);

    // ── body: the queue table (left) + peer/session panes (right) ───────
    let [tbl, side] = Layout::horizontal([Constraint::Min(40), Constraint::Length(38)]).spacing(1).areas(mid);

    // ---- the queue table ----
    let mut lines: Vec<Line> = Vec::new();
    // Wide windows get five DERIVED columns (each computed from counters already on this
    // screen, nothing invented): where the range sits in the chain, how far it is ahead of
    // the verified frontier, its estimated size, an ETA for in-flight rows, and a plain-words
    // note on why it is in the state it is in.
    let wide = tbl.width >= 150;
    let bytes_per_block = if s.fetched_total > 0 { s.sync_total as f64 / s.fetched_total as f64 } else { 0.0 };
    let avg_wait_ms: Option<f64> = done_log().lock().ok().and_then(|d| {
        let now = Instant::now();
        let w: Vec<f64> = d.iter().filter(|x| now.duration_since(x.at) <= DONE_RETAIN && x.wait_ms > 0)
            .map(|x| x.wait_ms as f64).collect();
        (!w.is_empty()).then(|| w.iter().sum::<f64>() / w.len() as f64)
    });
    let mut hdr = format!(" {:<3} {:<26} {:>7} {:<11} {:<12} {:<18} {:>9} {:>8}",
        "#", "RANGE", "BLOCKS", "WAIT", "STATE", "PEER", "blk/s", "AGE");
    if wide { hdr.push_str(&format!("  {:>6} {:>11} {:>7} {:>6}  {}", "POS", "AHEAD", "~MB", "ETA", "NOTE")); }
    lines.push(Line::from(Span::styled(hdr, Style::default().fg(C_DIM).add_modifier(Modifier::BOLD))));
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
            let blocks = existing_blocks(r.start, r.end, net_tip);
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
            if wide {
                let ex = queue_extra(r, tip, verified, bytes_per_block, avg_wait_ms);
                if let Some(last) = lines.last_mut() {
                    last.spans.push(Span::styled(format!("  {:>6} ", ex.pos), Style::default().fg(C_DIM)));
                    last.spans.push(Span::styled(format!("{:>11} ", ex.ahead), Style::default().fg(C_VBRIGHT)));
                    last.spans.push(Span::styled(format!("{:>7} ", ex.mb), Style::default().fg(C_DIM)));
                    last.spans.push(Span::styled(format!("{:>6}  ", ex.eta), Style::default().fg(C_NEON_CYAN)));
                    last.spans.push(Span::styled(ex.note, Style::default().fg(ex.note_col)));
                }
            }
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
    let mut sl = sl;
    sl.extend(disk_lines());
    f.render_widget(Paragraph::new(sl), pane_sess);
}

/// The derived, wide-window columns of one queue row.
pub(crate) struct QueueExtra { pub pos: String, pub ahead: String, pub mb: String, pub eta: String, pub note: String, pub note_col: Color }

/// Pure: compute the derived columns for a range row from counters already on screen.
pub(crate) fn queue_extra(r: &sigil_sync::RangeRow, tip: u64, verified: u64, bytes_per_block: f64, avg_wait_ms: Option<f64>) -> QueueExtra {
    let blocks = existing_blocks(r.start, r.end, tip);
    let pos = if tip > 0 { format!("{:.1}%", 100.0 * r.start as f64 / tip as f64) } else { "—".into() };
    let ahead_n = r.start.saturating_sub(verified);
    let ahead = if ahead_n == 0 { "at frontier".into() } else { format!("+{}", group(ahead_n)) };
    let mb = if bytes_per_block > 0.0 { format!("{:.1}", blocks as f64 * bytes_per_block / 1_048_576.0) } else { "—".into() };
    let (eta, note, note_col) = match &r.state {
        sigil_sync::RangeState::InFlight { fanout, .. } => {
            let eta = match avg_wait_ms {
                Some(a) if *fanout > 0 => { let left = (a - r.age_ms as f64).max(0.0); if left < 1.0 { "now".into() } else { fmt_age(left as u64) } }
                _ => "—".into(),
            };
            let (n, c) = if *fanout == 0 { ("claimed, not sent yet".to_string(), C_DIM) }
                else if r.age_ms > WAIT_FULL_MS { ("timed out — will be re-asked".to_string(), C_RED) }
                else if r.age_ms as f64 > WAIT_FULL_MS as f64 * 0.5 { ("slow peer".to_string(), C_GOLD) }
                else if *fanout > 1 { (format!("redundant ×{fanout}, first reply wins"), C_NEON_GREEN) }
                else { ("downloading".to_string(), C_NEON_GREEN) };
            (eta, n, c)
        }
        sigil_sync::RangeState::Fetched { .. } => {
            let (n, c) = if r.start <= verified { ("next to verify".to_string(), C_NEON_CYAN) }
                else { (format!("blocked by gap @ {}", group(verified)), C_GOLD) };
            ("—".into(), n, c)
        }
        sigil_sync::RangeState::Verified => ("—".into(), "verified, leaving the queue".into(), C_NEON_CYAN),
    };
    QueueExtra { pos, ahead, mb, eta, note, note_col }
}

#[cfg(test)]
mod queue_extra_tests {
    use super::queue_extra;
    use sigil_sync::{RangeRow, RangeState};
    #[test]
    fn staged_row_behind_a_gap_says_so_and_frontier_row_is_next() {
        let r = RangeRow { start: 9_940_000, end: 9_950_000, state: RangeState::Fetched { at_ms: 0 }, age_ms: 1000 };
        let e = queue_extra(&r, 30_000_000, 9_928_193, 150.0, Some(2000.0));
        assert!(e.note.contains("gap @ 9,928,193"), "{}", e.note);
        assert_eq!(e.ahead, "+11,807");
        assert_eq!(e.pos, "33.1%");
        assert_eq!(e.mb, "1.4");
        let f = RangeRow { start: 9_928_193, end: 9_938_193, state: RangeState::Fetched { at_ms: 0 }, age_ms: 10 };
        assert_eq!(queue_extra(&f, 30_000_000, 9_928_193, 0.0, None).note, "next to verify");
    }
    #[test]
    fn in_flight_eta_comes_from_recent_completions_only() {
        let r = RangeRow { start: 1, end: 10_001, state: RangeState::InFlight { peer: "p".into(), since_ms: 0, fanout: 1 }, age_ms: 500 };
        assert_eq!(queue_extra(&r, 100_000, 0, 0.0, None).eta, "—");
        assert_ne!(queue_extra(&r, 100_000, 0, 0.0, Some(2_000.0)).eta, "—");
        let slow = RangeRow { age_ms: 9_000, ..r.clone() };
        assert!(queue_extra(&slow, 100_000, 0, 0.0, Some(2_000.0)).note.contains("timed out"));
    }
}

/// Disk usage of the braid store + free space on its device, sampled OFF the draw thread
/// every 15 s (walking a multi-GB store per frame would stall the UI).
#[derive(Clone, Copy, Default)]
struct DiskSample { braid: Option<u64>, free: Option<u64>, total: Option<u64> }

fn disk_sample() -> DiskSample {
    use std::sync::{Mutex, OnceLock};
    static SLOT: OnceLock<Mutex<DiskSample>> = OnceLock::new();
    let slot = SLOT.get_or_init(|| {
        std::thread::Builder::new().name("disk-sampler".into()).spawn(|| loop {
            let db = crate::store_lifecycle::sigil_top_db_path();
            let braid = crate::store_lifecycle::dir_size_capped(&db, u64::MAX).ok().filter(|&b| b > 0);
            let (free, total) = df_free_total(std::path::Path::new(&db));
            if let Some(m) = SLOT.get() { if let Ok(mut g) = m.lock() { *g = DiskSample { braid, free, total }; } }
            std::thread::sleep(std::time::Duration::from_secs(15));
        }).ok();
        Mutex::new(DiskSample::default())
    });
    slot.lock().map(|g| *g).unwrap_or_default()
}

/// (free, total) bytes on the filesystem holding `path` via POSIX `df -Pk`; `None` where
/// there is no `df` (Windows) — shown as "—", never guessed.
fn df_free_total(path: &std::path::Path) -> (Option<u64>, Option<u64>) {
    let mut p = path;
    while !p.exists() { match p.parent() { Some(q) => p = q, None => return (None, None) } }
    let Ok(out) = std::process::Command::new("df").arg("-Pk").arg(p).output() else { return (None, None) };
    parse_df_pk(&String::from_utf8_lossy(&out.stdout))
}

/// Pure: `df -Pk` output → (available, total) bytes.
pub(crate) fn parse_df_pk(text: &str) -> (Option<u64>, Option<u64>) {
    let Some(line) = text.lines().nth(1) else { return (None, None) };
    let f: Vec<&str> = line.split_whitespace().collect();
    let kb = |i: usize| f.get(i).and_then(|x| x.parse::<u64>().ok()).map(|k| k * 1024);
    (kb(3), kb(1))
}

fn fmt_bytes(b: u64) -> String {
    let g = b as f64 / (1024.0 * 1024.0 * 1024.0);
    if g >= 1.0 { format!("{g:.1} GB") } else { format!("{:.0} MB", b as f64 / (1024.0 * 1024.0)) }
}

fn disk_lines() -> Vec<Line<'static>> {
    let d = disk_sample();
    let braid = d.braid.map(fmt_bytes).unwrap_or_else(|| "—".into());
    let free_col = match d.free { Some(f) if f < 5 << 30 => C_NEON_PINK, Some(f) if f < 20 << 30 => C_GOLD, _ => C_NEON_GREEN };
    let free = match (d.free, d.total) {
        (Some(f), Some(t)) if t > 0 => format!("{} of {} ({}% used)", fmt_bytes(f), fmt_bytes(t), 100 - (100 * f / t)),
        (Some(f), _) => fmt_bytes(f),
        _ => "—".into(),
    };
    vec![
        Line::from(vec![dim("  braid on disk   "), Span::styled(braid, Style::default().fg(C_VBRIGHT))]),
        Line::from(vec![dim("  disk free       "), Span::styled(free, Style::default().fg(free_col))]),
    ]
}

#[cfg(test)]
mod disk_tests {
    #[test]
    fn df_pk_parses_available_and_total() {
        let t = "Filesystem 1024-blocks Used Available Capacity Mounted on\noverlay 83886080 5452595 78433485 7% /\n";
        assert_eq!(super::parse_df_pk(t), (Some(78433485 * 1024), Some(83886080 * 1024)));
        assert_eq!(super::parse_df_pk(""), (None, None));
    }
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

/// The one-line explanation that turns a mute `peers 0` into a diagnosis.
///
/// Empty string whenever we have peers — the caller renders it unconditionally, so the
/// healthy case must cost nothing visually.
///
/// With no peers there are exactly two situations worth telling apart:
///   * `dial_failures > 0` — we ARE dialling and the network is REFUSING us. This is the
///     firewalled-client case (outbound 9501/853 blocked); the newest error says why.
///   * `dial_failures == 0` — no dial has failed yet, so the mesh is simply still coming
///     up. Nothing is wrong; wait.
fn zero_peer_reason(s: &crate::block_sync::P2PSyncState) -> String {
    if s.peer_count > 0 {
        return String::new();
    }
    if s.dial_failures > 0 {
        let why = s.stall_reason.trim();
        if why.is_empty() {
            format!("   ⚠ {} dial(s) refused — mesh unreachable", s.dial_failures)
        } else {
            format!("   ⚠ {} dial(s) refused — {why}", s.dial_failures)
        }
    } else {
        "   … dialling bootstrap (retry 1/2/4s)".to_string()
    }
}

#[cfg(test)]
mod synced_tests {
    use super::*;

    #[test]
    fn a_node_thousands_of_blocks_behind_is_not_synced() {
        // Old rule: caught = gap < 16_384 → "SYNCED". 10,000 settled blocks behind is ~21 minutes.
        assert!(!is_synced(true, 10_000, true, false));
        assert!(!is_synced(true, SYNCED_MAX_SETTLED_GAP, true, false), "the bound itself is not synced");
    }

    #[test]
    fn a_checkpoint_anchored_monitor_does_not_claim_genesis() {
        assert!(!integrity_verified_text(true, "28,000,000").contains("genesis→"), "must not claim a genesis-anchored spine");
        assert!(integrity_verified_text(false, "28,000,000").contains("genesis→28,000,000"));
    }

    #[test]
    fn caught_up_needs_every_condition() {
        assert!(is_synced(true, 0, true, false));
        assert!(is_synced(true, SYNCED_MAX_SETTLED_GAP - 1, true, false));
        assert!(!is_synced(false, 0, true, false), "not following the network");
        assert!(!is_synced(true, 0, false, false), "no valid tip-proof");
        assert!(!is_synced(true, 0, true, true), "a verify break outranks everything");
    }
}

#[cfg(test)]
mod existing_blocks_tests {
    use super::existing_blocks;

    /// Viktor's photo, 2026-09-25: verified 25,979,107, tip 25,979,638, every range 10,000 wide.
    #[test]
    fn a_range_past_the_tip_counts_only_blocks_that_exist() {
        assert_eq!(existing_blocks(25_979_107, 25_989_107, 25_979_638), 532);
        // 532 blocks in 0.436 s is ~1,220 blk/s, not the 22,936 the old formula printed.
        assert!(532.0 / 0.436 < 1_300.0);
    }

    #[test]
    fn a_range_below_the_tip_is_unchanged_and_unknown_tip_falls_back() {
        assert_eq!(existing_blocks(1_000, 11_000, 50_000), 10_000);
        assert_eq!(existing_blocks(1_000, 11_000, 0), 10_000);
        assert_eq!(existing_blocks(60_000, 70_000, 50_000), 0, "entirely above the tip: nothing existed");
    }
}
