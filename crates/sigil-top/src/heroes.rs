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
