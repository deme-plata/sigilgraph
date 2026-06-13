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
    let eta = if synced || kf_rate < 1.0 { f64::INFINITY } else { gap as f64 / kf_rate };

    let (vtext, vcol) = if light { ("◇ LIGHT MONITOR", C_NEON_CYAN) }
        else if s.verify_break.is_some() { ("⚠ SPINE BREAK", C_NEON_PINK) }
        else if synced { ("◆ SYNCED", C_NEON_GREEN) }
        else if connecting { ("… CONNECTING", C_DIM) }
        else if caught { ("≈ TRACKING HEAD", C_NEON_CYAN) }
        else { ("⬇ SYNCING", C_NEON_GOLD) };

    // LANE-P v0.59: never a silent 0 blk/s — when the sync engine reports a PARKED frontier
    // (stall_reason set), the hero headline says STALLED (full reason lives in the state /
    // Sync Log) instead of a quiet "SYNCING" that looks broken.
    let (vtext, vcol) = if !s.stall_reason.is_empty() && !synced && !light {
        ("⚠ STALLED — nudging peer", C_NEON_PINK)
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
    let label = format!(" {:>5.1}%  {}", frac * 100.0, vtext);
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
        format!("[{}…{}]", group(s.sync_cursor), group(s.sync_cursor.saturating_add(2048)))
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
            Line::from(vec![
                Span::styled("⛓ FULL ARCHIVE ", Style::default().fg(C_NEON_GOLD).add_modifier(Modifier::BOLD)),
                dim("tip "), val(group(net_tip)),
                dim("   spine "), Span::styled(format!("⛓{}", group(spine)), Style::default().fg(C_NEON_CYAN).add_modifier(Modifier::BOLD)),
                dim("   gap "), Span::styled(group(gap), Style::default().fg(if caught { C_NEON_GREEN } else { C_GOLD })),
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
        ]),
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
    lines.push(Line::from(vec![
        Span::styled(sa(" ▸ SYNC STATE"), Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD)),
        Span::styled(badge, Style::default().fg(bcol).add_modifier(Modifier::BOLD)),
    ]));
    lines.push(Line::from(vec![
        Span::raw(sa("  height ")), Span::styled(group(s.blocks_synced), Style::default().fg(C_GREEN).add_modifier(Modifier::BOLD)),
        Span::raw(sa("  tip ")), Span::styled(group(tip), Style::default().fg(C_CYAN)),
        Span::raw(sa("  gap ")), Span::styled(group(gap), Style::default().fg(if gap < 8 { C_GREEN } else { C_GOLD })),
        Span::raw(sa("  rate ")), Span::styled(format!("{:.0} blk/s", app.p2p_rate), Style::default().fg(C_CYAN)),
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
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(" ▸ SYNC LOG  (newest at bottom)", Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD))));
    let path = std::env::var("HOME").map(|h| format!("{h}/.sigil-top.log")).unwrap_or_else(|_| "sigil-top.log".into());
    // v0.26: read only the LAST 16 KB (not the whole file) — O(1) per frame, never
    // O(log-size), which would freeze the UI as the log grows over a 24/7 run.
    let body = read_log_tail(&path, 16 * 1024);
    let recent: Vec<String> = body.lines().rev()
        .filter(|l| l.contains("[DBG]") || l.contains("[PANIC]") || l.contains("[sync]")
            || l.contains("[tipfetch]") || l.contains("[D]") || l.contains("[p2p-sync]")
            || l.contains("[tip]") || l.contains("[render]"))
        .take(26)
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
