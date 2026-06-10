// LANE-U item 3: UI heroes/cards extracted from main.rs (pure move, no behavior change).
// A child module so `use super::*` reaches main.rs's private helpers/consts/App.
#![allow(clippy::too_many_lines)]
use super::*;

/// LANE-B v0.50: the SIGIL rune drawing itself line-by-line then fading — a floating
/// overlay band, NOT a fullscreen splash. The envelope (draw-on → hold → fade) is
/// driven by ELAPSED TIME, not raw frame count, so it looks identical at the 33 ms
/// (*nix) and 66 ms (legacy-conhost Windows) render cadences. CP437-safe by
/// construction: only light box-drawing (┌─┐│└┘├┤┬┴) + block elements (█▓▒░) +
/// ASCII, every string also run through `sa()` and the global ascii pass as a
/// belt-and-suspenders. Never reads input; the event loop is untouched.
pub(crate) fn draw_rune_band(f: &mut Frame, app: &App, area: Rect, start: Instant, until: Instant) {
    // The sigil itself — pure CP437-safe glyphs.
    const RUNE: [&str; 7] = [
        "    ┌───────┴───────┐    ",
        "    │  ░▒▓█████▓▒░  │    ",
        "    ├──┐  ▓███▓  ┌──┤    ",
        "    │  └──┐███┌──┘  │    ",
        "    ├──┘  ▒███▒  └──┤    ",
        "    │  ░▒▓█████▓▒░  │    ",
        "    └───────┬───────┘    ",
    ];
    let n = RUNE.len();

    // ── time envelope ────────────────────────────────────────────────────────
    let total = until.saturating_duration_since(start).as_secs_f64().max(0.001);
    let progress = (Instant::now().saturating_duration_since(start).as_secs_f64() / total)
        .clamp(0.0, 1.0);
    // draw-on over the first 40% (top→bottom), full hold to 72%, then fade out.
    let revealed = if progress >= 0.40 { n }
        else { (((progress / 0.40) * n as f64).ceil() as usize).clamp(1, n) };
    let glyph_col = if progress < 0.72 {
        C_VBRIGHT
    } else {
        // lerp brand-violet → obsidian so the rune dissolves into the background.
        let fde = ((progress - 0.72) / 0.28).clamp(0.0, 1.0);
        let lerp = |a: u32, b: u32| (a as f64 * (1.0 - fde) + b as f64 * fde) as u8;
        Color::Rgb(lerp(0xc8, 0x10), lerp(0xb6, 0x10), lerp(0xff, 0x1e))
    };

    // ── content: a version/alive banner line + the revealed rune lines ─────────
    let update_avail = version_gt(&app.latest, VERSION);
    let pulse = ["░", "▒", "▓", "█", "▓", "▒"][(app.rune_frame as usize) % 6];
    let mut lines: Vec<Line> = Vec::with_capacity(n + 1);
    if update_avail {
        lines.push(Line::from(vec![
            Span::styled(sa(format!("{pulse} ")), Style::default().fg(C_GOLD)),
            Span::styled("SIGIL ", Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD)),
            Span::styled(sa(format!("⬆ v{} ", app.latest)),
                Style::default().fg(C_NEON_GOLD).add_modifier(Modifier::BOLD)),
            Span::styled(sa("— [U]"), Style::default().fg(C_GOLD)),
        ]));
    } else {
        lines.push(Line::from(vec![
            Span::styled(sa(format!("{pulse} ")), Style::default().fg(C_DIM)),
            Span::styled("SIGIL", Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD)),
            Span::styled("  still alive", Style::default().fg(C_DIM)),
        ]));
    }
    for (i, art) in RUNE.iter().enumerate() {
        if i < revealed {
            lines.push(Line::from(Span::styled(sa(*art),
                Style::default().fg(glyph_col).add_modifier(Modifier::BOLD))));
        } else {
            lines.push(Line::from(""));
        }
    }

    // ── floating band geometry — centered, upper portion, never full screen ────
    let bw = 40u16.min(area.width.max(1));
    let bh = ((n as u16) + 3).min(area.height.max(1)); // banner + rune + 2 border
    let bx = area.x + area.width.saturating_sub(bw) / 2;
    let by = area.y + area.height.saturating_sub(bh) / 4;
    let band = Rect { x: bx, y: by, width: bw, height: bh };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(if ui_ascii() { BorderType::Plain } else { BorderType::Rounded })
        .border_style(Style::default().fg(glyph_col))
        .style(Style::default().bg(C_BG));
    f.render_widget(Clear, band); // float above whatever tab is live
    f.render_widget(
        Paragraph::new(lines)
            .alignment(ratatui::layout::Alignment::Center)
            .block(block),
        band,
    );
}

/// LANE-U v0.67: the first-launch welcome modal — a centered card with a bold SIGIL emblem and a
/// GIANT F prompt to start. CP437-safe by construction: only full-block (█) + light-box glyphs
/// (the operator console renders nothing fancier), every string also run through `sa()`.
pub(crate) fn draw_welcome_modal(f: &mut Frame, area: Rect) {
    let w: u16 = 60.min(area.width.saturating_sub(2));
    let h: u16 = 22.min(area.height.saturating_sub(2));
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let modal = Rect { x, y, width: w, height: h };
    f.render_widget(Clear, modal); // punch a hole over the dashboard

    let g = |t: &str, c: Color| Line::from(Span::styled(sa(t), Style::default().fg(c).add_modifier(Modifier::BOLD)));
    let mut lines: Vec<Line> = vec![
        Line::from(""),
        // SIGIL emblem — a full-block diamond (CP437-safe)
        g("        ███        ", C_NEON_CYAN),
        g("      ███████      ", C_NEON_CYAN),
        g("    ███████████    ", C_VBRIGHT),
        g("      ███████      ", C_NEON_PINK),
        g("        ███        ", C_NEON_PINK),
        g("      S I G I L      ", C_NEON_CYAN),
        Line::from(""),
        // GIANT F (full-block) beside the call-to-action label
        g("   ███████   start", C_NEON_GOLD),
        g("   ███        the", C_NEON_GOLD),
        g("   █████      live", C_NEON_GOLD),
        g("   ███        node", C_NEON_GOLD),
        g("   ███        + mining", C_NEON_GOLD),
        Line::from(""),
        Line::from(vec![
            Span::styled(sa("  press "), Style::default().fg(C_DIM)),
            Span::styled(" F ", Style::default().bg(C_NEON_GOLD).fg(C_BG).add_modifier(Modifier::BOLD)),
            Span::styled(sa(" to START — "), Style::default().fg(C_NEON_GREEN).add_modifier(Modifier::BOLD)),
            Span::styled(sa("[5] Mining  [M] mine"), Style::default().fg(C_DIM)),
        ]),
        Line::from(Span::styled(sa("        any other key to skip"), Style::default().fg(C_DIM))),
    ];
    lines.truncate(h.saturating_sub(2) as usize);
    let card = card_block(" ◇ WELCOME TO SIGIL", C_NEON_CYAN)
        .border_style(Style::default().fg(C_NEON_CYAN))
        .style(Style::default().bg(C_BG));
    f.render_widget(
        Paragraph::new(lines).alignment(ratatui::layout::Alignment::Center).block(card),
        modal,
    );
}

/// The original node dashboard, now the [1] Node tab body.
pub(crate) fn draw_node_body(f: &mut Frame, app: &App, body_area: ratatui::layout::Rect) {
    let body_h = Layout::horizontal([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)])
        .spacing(1)  // v0.33.2: breathing room so the two columns don't fuse at the border
        .split(body_area);
    let (left_area, right_area) = (body_h[0], body_h[1]);

    let left_v = Layout::vertical([
        Constraint::Length(6), // Node
        Constraint::Length(6), // StateRoots
        Constraint::Length(4), // Supply
        Constraint::Min(0),    // spacer (v0.36.1: MINING promoted to a top hero band)
    ])
    .split(left_area);

    f.render_widget(render_node_card(app), left_v[0]);
    f.render_widget(render_state_roots(app), left_v[1]);
    f.render_widget(render_supply(app), left_v[2]);

    let right_v = Layout::vertical([Constraint::Length(5), Constraint::Length(5), Constraint::Length(7), Constraint::Min(0)])
        .spacing(1)
        .split(right_area);
    f.render_widget(render_security(app), right_v[0]);
    f.render_widget(render_fleet_card(app), right_v[1]);
    f.render_widget(render_cortex_card(app), right_v[2]);
    f.render_widget(render_block_stream(app), right_v[3]);
}

/// v0.33.2 BOLD NEON card: rounded obsidian card with a bright neon title chip glowing in
/// the accent color, and a border tinted toward the accent instead of flat grey. The title
/// is a filled chip (` ◆ NODE `) so it reads as a label, not glued to the corner.
pub(crate) fn card_block(title: &'static str, color: Color) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        // v0.33.5: light box-drawing (┌─┐│└┘) IS in CP437 → renders on classic raster conhost;
        // heavy/rounded corners are NOT and showed as `?`. Use Plain on ascii consoles, Rounded
        // on rich terminals (Windows Terminal / VS Code / *nix) where it's prettier.
        .border_type(if ui_ascii() { BorderType::Plain } else { BorderType::Rounded })
        .padding(Padding::horizontal(1))
        .title(Line::from(vec![
            Span::styled(format!("{} ", title.trim_start()),
                Style::default().bg(color).fg(C_BG).add_modifier(Modifier::BOLD)),
        ]))
        .border_style(Style::default().fg(color))
        .style(Style::default().bg(C_BG))
}

pub(crate) fn render_header(app: &App) -> Paragraph<'static> {
    let live = app.online;
    // v0.33.2: loud neon brand block + a filled status banner pill.
    let status = if live { banner("◆ LIVE", C_NEON_GREEN) } else { banner("✕ OFFLINE", C_NEON_PINK) };
    let update = if version_gt(&app.latest, VERSION) {
        Span::styled(format!("   ⬆ UPDATE v{} [U]", app.latest),
            Style::default().fg(C_NEON_GOLD).add_modifier(Modifier::BOLD))
    } else {
        Span::raw("")
    };
    let line = Line::from(vec![
        Span::styled(" ◇ SIGIL ", Style::default().bg(C_NEON_CYAN).fg(C_BG_HEAD).add_modifier(Modifier::BOLD)),
        Span::styled(format!(" v{}", VERSION), Style::default().fg(C_NEON_CYAN).add_modifier(Modifier::BOLD)),
        Span::styled(format!(" {}", short_rev()), Style::default().fg(C_INK)),
        Span::styled(format!(" · {} ", app.st.network), Style::default().fg(C_DIM)),
        status,
        Span::styled("  uptime ", Style::default().fg(C_DIM)),
        Span::styled(fmt_uptime(app.st.uptime_secs), Style::default().fg(C_CYAN)),
        Span::styled("  ·  net height ", Style::default().fg(C_DIM)),
        val(group(app.target_height)),
        update,
    ]);
    Paragraph::new(line).style(Style::default().bg(C_BG_HEAD))
}

pub(crate) fn render_node_card(app: &App) -> Paragraph<'static> {
    let st = &app.st;
    let producer = if st.producer.is_empty() { "—".to_string() } else { st.producer.clone() };
    let lines = vec![
        Line::from(vec![
            dim("height  "), Span::styled(group(st.height), Style::default().fg(C_GOLD).add_modifier(Modifier::BOLD)),
            dim("   peers "), Span::styled(group(st.peers), Style::default().fg(C_GREEN)),
        ]),
        Line::from(vec![ dim("producer "), Span::styled(producer, Style::default().fg(C_CYAN)) ]),
        Line::from(vec![
            dim("reward  "), Span::styled("5", Style::default().fg(C_GREEN)), dim(" SIGIL/blk"),
            dim("   uptime "), Span::raw(fmt_uptime(st.uptime_secs)),
        ]),
    ];
    Paragraph::new(lines).block(card_block(" ◆ NODE", C_NEON_GREEN))
}

pub(crate) fn render_state_roots(app: &App) -> Paragraph<'static> {
    let (badge, lat_str) = match &app.verify {
        Some(v) if v.ok => (
            Span::styled(" VERIFIED ", Style::default().bg(C_GREEN).fg(Color::Rgb(0x0a,0x0a,0x14)).add_modifier(Modifier::BOLD)),
            format!(" BLAKE3 · {}µs", v.latency_us),
        ),
        Some(_) => (Span::styled(" FAILED ", Style::default().bg(C_RED).fg(Color::Rgb(0x0a,0x0a,0x14)).add_modifier(Modifier::BOLD)), String::new()),
        None => (Span::styled(" WAITING ", Style::default().bg(C_DIM).fg(Color::Rgb(0x0a,0x0a,0x14))), String::new()),
    };
    let (wallet, dex, event, contract) = if let Some(t) = app.st.tip.as_ref() {
        (short_hex(&t.roots.wallet_state_root), short_hex(&t.roots.dex_state_root),
         short_hex(&t.roots.event_log_root), short_hex(&t.roots.contract_state_root))
    } else { ("—".into(), "—".into(), "—".into(), "—".into()) };
    let lines = vec![
        Line::from(vec![badge, Span::styled(lat_str, Style::default().fg(C_DIM))]),
        Line::from(vec![ dim("wallet "), Span::raw(wallet), dim("  dex "), Span::raw(dex) ]),
        Line::from(vec![ dim("events "), Span::raw(event), dim("  contract "), Span::raw(contract) ]),
    ];
    Paragraph::new(lines).block(card_block(" ◈ STATE ROOTS", C_GOLD))
}

pub(crate) fn render_supply(app: &App) -> Paragraph<'static> {
    let supply = app.st.native_supply;
    let frac = if MAX_SUPPLY_BASE > 0 { (supply as f64 / MAX_SUPPLY_BASE as f64).clamp(0.0, 1.0) } else { 0.0 };
    let lines = vec![
        Line::from(vec![
            val(fmt_supply(supply)),
            dim(" / 21,000,000   "),
            Span::styled(format!("{:.2}%", frac * 100.0), Style::default().fg(C_NEON_CYAN).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(neon_bar(frac, 34, C_NEON_GOLD)),
    ];
    Paragraph::new(lines).block(card_block(" ⬣ SUPPLY", C_NEON_GOLD))
}

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
            Line::from(vec![
                dim("tip "), val(group(net_tip)),
                dim("   mode "), Span::styled("light tip-proof verify (~10ms)", Style::default().fg(C_NEON_CYAN)),
            ])
        } else if snap_mode {
            // recent-window monitor: `verified` is checkpoint-anchored (NOT a genesis spine), so
            // label it honestly as a tip-proof checkpoint badge — never "spine" (implies genesis).
            Line::from(vec![
                dim("tip "), val(group(net_tip)),
                dim("   ✓ checkpoint "), Span::styled(group(spine), Style::default().fg(C_NEON_GREEN).add_modifier(Modifier::BOLD)),
                dim(" (tip-proof, not genesis)"),
                dim("   gap "), Span::styled(group(gap), Style::default().fg(if caught { C_NEON_GREEN } else { C_GOLD })),
            ])
        } else {
            Line::from(vec![
                dim("tip "), val(group(net_tip)),
                dim("   spine "), Span::styled(format!("⛓{}", group(spine)), Style::default().fg(C_NEON_CYAN).add_modifier(Modifier::BOLD)),
                dim("   gap "), Span::styled(group(gap), Style::default().fg(if caught { C_NEON_GREEN } else { C_GOLD })),
            ])
        },
        if light {
            Line::from(vec![
                dim("backfill "), Span::styled("off — light monitor", Style::default().fg(C_DIM)),
                dim("   enable: "), Span::styled("--sync", Style::default().fg(C_NEON_CYAN).add_modifier(Modifier::BOLD)),
                dim(" (tungt: henter hele kæden)"),
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

/// [5] Mining — the REAL in-process dual-lane miner. Reads the SAME engine state
/// (flux_miner::engine::MinerStats) the standalone sigil-miner exe shows, so
/// sigil-top is node + miner in ONE binary. [m] start/stop · [g] GPU/CPU.
pub(crate) fn draw_mining_tab(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let s = app.mine_stats.lock().unwrap().clone();
    let mining = app.mining;
    let mode = if s.mode.is_empty() { "CPU".to_string() } else { s.mode.clone() };
    let mcol = if mode == "GPU" { C_NEON_GREEN } else { C_NEON_CYAN };
    let (conn_txt, conn_col) = if !mining { ("○ stopped", C_DIM) }
        else if s.connected { ("● LIVE", C_NEON_GREEN) }
        else { ("◌ connecting", C_NEON_GOLD) };

    let block = card_block(" ⛏ MINING · DUAL-LANE ENGINE", C_NEON_PINK)
        .border_style(Style::default().fg(if mining { mcol } else { C_DIM }));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let [head, rates, tally, solverow, acctrow, body, hint] = Layout::vertical([
        Constraint::Length(1), Constraint::Length(1), Constraint::Length(1),
        Constraint::Length(1), Constraint::Length(1), Constraint::Min(0), Constraint::Length(1),
    ]).areas(inner);

    let wallet = miner_wallet();
    let wshort = if wallet.len() >= 14 {
        format!("{}…{}", &wallet[..8], &wallet[wallet.len() - 6..])
    } else { wallet.clone() };

    f.render_widget(Paragraph::new(Line::from(vec![
        Span::styled(format!(" [{mode}] "), Style::default().fg(mcol).add_modifier(Modifier::BOLD)),
        Span::styled("BLAKE4 Φ", Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD)),
        dim(" + "), Span::styled("VDF Ω", Style::default().fg(C_NEON_CYAN).add_modifier(Modifier::BOLD)),
        dim("   "), Span::styled(conn_txt, Style::default().fg(conn_col).add_modifier(Modifier::BOLD)),
        dim("   node "), Span::styled(engine_node_url(), Style::default().fg(C_DIM)),
        dim("   "), Span::styled(wshort, Style::default().fg(C_DIM)),
    ])), head);

    f.render_widget(Paragraph::new(Line::from(vec![
        dim(" hashrate "), Span::styled(engine::format_hps(s.hashrate), Style::default().fg(C_NEON_GOLD).add_modifier(Modifier::BOLD)),
        dim("   "), Span::styled(flux_miner::format_flux(s.hashrate), Style::default().fg(C_VBRIGHT)),
        dim("   vdf "), Span::styled(flux_miner::format_omega(s.vdf_rate), Style::default().fg(C_NEON_CYAN)),
        dim("   last solve "), Span::styled(format!("{:.0} ms", s.last_solve_ms), Style::default().fg(C_GOLD)),
        dim("   vdf_t "), Span::styled(group(s.vdf_t), Style::default().fg(C_DIM)),
    ])), rates);

    let total = s.shares_ok + s.shares_bad;
    let accept = if total > 0 { s.shares_ok as f64 / total as f64 * 100.0 } else { 100.0 };
    f.render_widget(Paragraph::new(Line::from(vec![
        dim(" shares "), Span::styled(format!("{} ✓", group(s.shares_ok)), Style::default().fg(C_NEON_GREEN).add_modifier(Modifier::BOLD)),
        Span::styled(format!("  {} ✗", group(s.shares_bad)), Style::default().fg(if s.shares_bad > 0 { C_RED } else { C_DIM })),
        dim("   accept "), Span::styled(format!("{accept:.0}%"), Style::default().fg(if accept >= 99.0 { C_NEON_GREEN } else { C_NEON_GOLD })),
        dim("   balance "), Span::styled(format!("{} SIGIL", s.balance), Style::default().fg(C_NEON_GREEN).add_modifier(Modifier::BOLD)),
        dim("   mine-chain h "), Span::styled(group(s.last_height), Style::default().fg(C_VBRIGHT)),
        dim(" (egen kaede - ikke produce-tippen)"),
    ])), tally);

    // ── solve-time sparkline (last solve relative to the session max) ─────────
    let maxv = s.solve_hist.iter().copied().max().unwrap_or(1).max(1);
    let last = s.solve_hist.back().copied().unwrap_or(0);
    let barw = (solverow.width as usize).saturating_sub(20).max(4);
    let fill = ((last as f64 / maxv as f64) * barw as f64).round() as usize;
    let bar = "█".repeat(fill.min(barw)) + &"░".repeat(barw.saturating_sub(fill));
    f.render_widget(Paragraph::new(Line::from(vec![
        dim(" solve  "), Span::styled(bar, Style::default().fg(mcol)),
        Span::styled(format!(" {last} ms"), Style::default().fg(C_DIM)),
    ])), solverow);

    // ── LANE-B v0.50: accept-rate sparkline from App-side history. CP437-safe:
    // each sample maps to a block shade by band (<90 ░ · <97 ▒ · <100 ▓ · 100 █).
    let mut acc_line: Vec<Span> = vec![dim(" accept ")];
    if app.accept_hist.is_empty() {
        acc_line.push(dim("—"));
    } else {
        let aw = (acctrow.width as usize).saturating_sub(20).max(4);
        let tail: Vec<u8> = app.accept_hist.iter().rev().take(aw).rev().copied().collect();
        for a in tail {
            let (ch, col) = if a < 90 { ("░", C_RED) }
                else if a < 97 { ("▒", C_NEON_GOLD) }
                else if a < 100 { ("▓", C_NEON_GREEN) }
                else { ("█", C_NEON_GREEN) };
            acc_line.push(Span::styled(ch, Style::default().fg(col)));
        }
    }
    acc_line.push(Span::styled(format!(" {accept:.0}%"),
        Style::default().fg(if accept >= 99.0 { C_NEON_GREEN } else { C_NEON_GOLD })));
    f.render_widget(Paragraph::new(Line::from(acc_line)), acctrow);

    // ── LANE-B v0.50: split the lower area — recent MINE-CHAIN blocks (left) vs
    // the live share log (right). The mine-chain is THIS miner's own chain; the
    // produce-tip is what the node syncs/serves — both shown so they never blur.
    let [mined_col, log_col] = Layout::horizontal([
        Constraint::Percentage(48), Constraint::Percentage(52),
    ]).spacing(1).areas(body);

    let produce_tip = app.st.tip.as_ref().map(|t| t.height).filter(|h| *h > 0).unwrap_or(app.st.height);
    let now = Instant::now();
    let mut mlines: Vec<Line> = Vec::new();
    mlines.push(Line::from(vec![
        Span::styled(" MINE-CHAIN", Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD)),
        dim(" — your shares, not the produce-tip"),
    ]));
    mlines.push(Line::from(vec![
        dim("  produce-tip "), Span::styled(group(produce_tip), Style::default().fg(C_NEON_CYAN)),
        dim("   mine-tip "), Span::styled(group(s.last_height), Style::default().fg(C_NEON_GREEN).add_modifier(Modifier::BOLD)),
    ]));
    if app.mined_recent.is_empty() {
        mlines.push(Line::from(dim(if mining { "  warming up — no accepted blocks yet" } else { "  press [m] to mine" })));
    } else {
        let rows = (mined_col.height as usize).saturating_sub(2);
        for (h, ms, when) in app.mined_recent.iter().take(rows) {
            let age = now.saturating_duration_since(*when).as_secs();
            let agestr = if age < 60 { format!("{age}s ago") }
                else if age < 3600 { format!("{}m ago", age / 60) }
                else { format!("{}h ago", age / 3600) };
            mlines.push(Line::from(vec![
                Span::styled(format!("  #{}", group(*h)), Style::default().fg(C_NEON_GREEN).add_modifier(Modifier::BOLD)),
                dim("  "), Span::styled(format!("{ms:.0}ms"), Style::default().fg(C_GOLD)),
                dim("  "), Span::styled(agestr, Style::default().fg(C_DIM)),
            ]));
        }
    }
    f.render_widget(Paragraph::new(mlines), mined_col);

    let mut llines: Vec<Line> = Vec::new();
    llines.push(Line::from(Span::styled(" SHARE LOG", Style::default().fg(C_NEON_PINK).add_modifier(Modifier::BOLD))));
    let maxlines = (log_col.height as usize).saturating_sub(1);
    for l in s.log.iter().take(maxlines) {
        let c = if l.starts_with('✓') { C_NEON_GREEN } else if l.starts_with('✗') { C_NEON_GOLD } else { C_DIM };
        llines.push(Line::from(Span::styled(format!("  {l}"), Style::default().fg(c))));
    }
    f.render_widget(Paragraph::new(llines), log_col);

    let err = s.last_err.as_ref().map(|e| format!("   ⚠ {e}")).unwrap_or_default();
    f.render_widget(Paragraph::new(Line::from(vec![
        Span::styled(" m ", Style::default().fg(C_NEON_PINK).add_modifier(Modifier::BOLD)),
        dim(if mining { "stop  " } else { "start  " }),
        Span::styled("g ", Style::default().fg(C_NEON_PINK).add_modifier(Modifier::BOLD)),
        dim("GPU/CPU  "),
        Span::styled(err, Style::default().fg(C_RED)),
    ])), hint);
}

pub(crate) fn draw_mining_hero(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let st = &app.st;
    let bps = st.blocks_per_sec.max(0.0);
    const TARGET_BPS: f64 = 250.0;                    // SIGIL_PRODUCE_US=4000 → 250 blk/s
    let frac = (bps / TARGET_BPS).clamp(0.0, 1.0);
    let emit = bps * 5.0;                             // reward 5 SIGIL/blk
    let mining = app.mining;
    let (hv, hu) = if app.mine_hashrate >= 1000.0 { (app.mine_hashrate / 1000.0, "GH/s") }
        else { (app.mine_hashrate, "MH/s") };
    // v0.64.2 ONE GRAPH: this hero is about YOUR RIG. The network graph (supply,
    // height, blk/s bar) lives ONLY on the Node tab — two competing "network"
    // displays kept reading as a bug (mine-chain vs produce-chain confusion).
    let _ = (bps, TARGET_BPS, emit, &st);
    let pcol = if mining { C_NEON_GREEN } else { C_DIM };
    let ptext = if mining { "⚒ MINING" } else { "✕ idle — press M" };
    let rig_frac = if app.mine_hashrate >= 1000.0 { 1.0 } else { (app.mine_hashrate / 20.0).clamp(0.0, 1.0) };

    let block = card_block(" ✦ MINING · YOUR RIG", C_NEON_PINK)
        .border_style(Style::default().fg(pcol));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let [bar_row, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
    let [tele, art] = Layout::horizontal([Constraint::Min(0), Constraint::Length(20)]).spacing(1).areas(body);

    // ── BIG network-power bar ────────────────────────────────────────────
    let label = format!(" {:.2} {}  {}", hv, hu, ptext);
    let total = bar_row.width as usize;
    let barw = total.saturating_sub(label.chars().count() + 1).max(4);
    let fill = (rig_frac * barw as f64).round() as usize;
    let bar_str = "█".repeat(fill.min(barw)) + &"░".repeat(barw.saturating_sub(fill));
    f.render_widget(Paragraph::new(Line::from(vec![
        Span::styled(bar_str, Style::default().fg(pcol).add_modifier(Modifier::BOLD)),
        Span::styled(label, Style::default().fg(pcol).add_modifier(Modifier::BOLD)),
    ])), bar_row);

    // ── telemetry (left): network economics + YOUR mining ────────────────
    let est_earn = format!("~{:.4}", app.verified_count as f64 * 0.0005);
    let (whole, cents) = (app.wallet_balance / 100_000_000, (app.wallet_balance % 100_000_000) / 1_000_000);
    let tlines = vec![
        Line::from(vec![
            dim("reward "), Span::styled("5 SIGIL/blk".to_string(), Style::default().fg(C_GREEN)),
            dim("   network graph: Node tab [1]"),
        ]),
        Line::from(vec![
            dim("you  "), Span::styled(if mining { "◆ MINING".to_string() } else { "○ off".to_string() },
                Style::default().fg(if mining { C_NEON_GREEN } else { C_DIM }).add_modifier(Modifier::BOLD)),
            dim("   "), Span::styled(format!("{:.2} {}", hv, hu), Style::default().fg(C_NEON_GOLD).add_modifier(Modifier::BOLD)),
            dim("   "), Span::styled(format!("{} ✓ shares", group(app.mine_accepted)), Style::default().fg(C_NEON_GREEN)),
            dim("   streak ×"), Span::styled(group(app.streak), Style::default().fg(C_GOLD)),
        ]),
        Line::from(vec![
            dim("wallet "), Span::styled(format!("{whole}.{cents:02} SIGIL"), Style::default().fg(C_NEON_GREEN).add_modifier(Modifier::BOLD)),
            dim("   hashes "), Span::styled(format!("{}M", app.mine_hashes / 1_000_000), Style::default().fg(C_DIM)),
            dim("   est earn "), Span::styled(est_earn, Style::default().fg(C_GOLD)),
            dim("   [M] mine"),
        ]),
    ];
    f.render_widget(Paragraph::new(tlines), tele);

    // ── forge motif (right): glows HOT while you mine ────────────────────
    let (ftxt, fcol) = if mining { ("HOT", C_NEON_PINK) } else { ("cold", C_DIM) };
    let art_lines = vec![
        Line::from(Span::styled("   ╱██╲   ", Style::default().fg(C_NEON_GOLD))),
        Line::from(Span::styled("  ▕◆◆◆▏  ", Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD))),
        Line::from(Span::styled("   ╲██╱   ", Style::default().fg(C_NEON_GOLD))),
        Line::from(Span::styled("  ═════  ", Style::default().fg(C_DIM))),
        Line::from(Span::styled(if mining { "  ✦ · ✦  ".to_string() } else { "         ".to_string() },
            Style::default().fg(C_NEON_PINK))),
        Line::from(vec![dim(" forge "), Span::styled(ftxt, Style::default().fg(fcol).add_modifier(Modifier::BOLD))]),
    ];
    f.render_widget(Paragraph::new(art_lines), art);
}

pub(crate) fn render_mining(app: &App) -> Paragraph<'static> {
    let (state, scol) = if app.mining { ("ON", C_GREEN) } else { ("off", C_RED) };
    let earn = format!("~{:.4}", app.verified_count as f64 * 0.0005);
    // v0.2.35: live hashrate from the miner thread
    let rate_line = if app.mine_hashrate > 0.0 {
        let (val, unit) = if app.mine_hashrate >= 1000.0 {
            (app.mine_hashrate / 1000.0, "GH/s")
        } else {
            (app.mine_hashrate, "MH/s")
        };
        Line::from(vec![
            dim("rate "), Span::styled(format!("{:.2} {unit}", val), Style::default().fg(C_GOLD).add_modifier(Modifier::BOLD)),
            dim("   hashes "), Span::styled(format!("{}M", app.mine_hashes / 1_000_000), Style::default().fg(C_DIM)),
        ])
    } else {
        Line::from(dim("rate —   hashes —"))
    };
    // v0.2.35: wallet balance line
    let bal_line = if app.wallet_balance > 0 {
        let whole = app.wallet_balance / 100_000_000;
        let frac = (app.wallet_balance % 100_000_000) / 1_000_000;
        Line::from(vec![
            dim("balance "), Span::styled(format!("{whole}.{frac:02} SIGIL"), Style::default().fg(C_GREEN).add_modifier(Modifier::BOLD)),
            dim("   shares "), Span::styled(group(app.mine_accepted), Style::default().fg(C_DIM)),
        ])
    } else {
        Line::from(vec![
            dim("balance —   shares "), Span::styled(group(app.mine_accepted), Style::default().fg(C_DIM)),
        ])
    };
    let lines = vec![
        Line::from(vec![
            dim("mining "), Span::styled(state, Style::default().fg(scol).add_modifier(Modifier::BOLD)),
            dim("   score "), Span::styled(group(app.score), Style::default().fg(C_GOLD)),
            dim("   verified "), Span::styled(group(app.verified_count), Style::default().fg(C_GREEN)),
        ]),
        Line::from(vec![
            dim("streak "), Span::styled(format!("×{}", app.streak), Style::default().fg(C_GOLD)),
            dim("   est earn "), Span::styled(earn, Style::default().fg(C_GOLD)),
        ]),
        rate_line,
        bal_line,
    ];
    Paragraph::new(lines).block(card_block(" ✦ MINING", C_NEON_PINK))
}

pub(crate) fn render_security(app: &App) -> Paragraph<'static> {
    let k = app.eclipse_k;
    let agreed = app.eclipse_sources.iter().filter(|(_, b)| *b).count();
    let total = app.eclipse_sources.len().max(1);
    // v0.7.5: Real SQIsign status from tip verification
    let pq = app.verify.as_ref().map(|v| v.sqisign_available).unwrap_or(false);
    let sig_verified = app.verify.as_ref().map(|v| v.ok).unwrap_or(false);
    let sig_line = if sig_verified && pq {
        Line::from(vec![
            dim("sig "), Span::styled("SQIsign ✓", Style::default().fg(C_GREEN).add_modifier(Modifier::BOLD)),
            dim("  "), Span::styled("PQ-verified · 177B", Style::default().fg(C_VBRIGHT)),
        ])
    } else if sig_verified {
        Line::from(vec![
            dim("sig "), Span::styled("BLAKE3 ✓", Style::default().fg(C_GREEN).add_modifier(Modifier::BOLD)),
            dim("  "), Span::styled(if pq { "SQIsign ready" } else { "SQIsign gated" }, Style::default().fg(C_DIM)),
        ])
    } else if app.verify.is_some() {
        Line::from(vec![
            dim("sig "), Span::styled("FAILED", Style::default().fg(C_RED).add_modifier(Modifier::BOLD)),
            dim("  "), Span::styled("tip verification failed", Style::default().fg(C_RED)),
        ])
    } else {
        Line::from(vec![
            dim("sig "), Span::styled("waiting", Style::default().fg(C_DIM)),
            dim("  "), Span::styled("no tip received yet", Style::default().fg(C_DIM)),
        ])
    };
    // v0.7.5: Real eclipse probability — computed from actual K, not hardcoded 0.30
    let p_eclipse = if k > 0 { 0.30_f64.powi(k as i32) } else { 1.0 };
    let eclipse_line = if k > 0 {
        Line::from(vec![
            dim("eclipse "), Span::styled(format!("K={}", k), Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD)),
            dim("  agree "), Span::styled(format!("{}/{}", agreed, total),
                Style::default().fg(if agreed >= k as usize { C_GREEN } else if agreed > 0 { C_GOLD } else { C_RED })),
            dim(format!("  P={:.1e}", p_eclipse)),
        ])
    } else {
        Line::from(vec![
            dim("eclipse "), Span::styled("K=0", Style::default().fg(C_RED).add_modifier(Modifier::BOLD)),
            dim("  "), Span::styled("no independent sources — measuring…", Style::default().fg(C_DIM)),
        ])
    };
    let lines = vec![
        eclipse_line,
        sig_line,
        Line::from(vec![
            dim("verify "), Span::styled(format!("{}µs", app.sync_us), Style::default().fg(C_CYAN)),
            dim(if sig_verified { "  ✓ tip proven" } else { "  awaiting proof" }),
        ]),
    ];
    Paragraph::new(lines).block(card_block(" ✶ SECURITY", C_VBRIGHT))
}
