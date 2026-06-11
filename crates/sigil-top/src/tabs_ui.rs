// LANE-U: extracted from main.rs (pure move, no behavior change).
// `use super::*` reaches main.rs's private helpers/consts/App — the heroes.rs pattern.
#![allow(clippy::too_many_lines)]
use super::*;

pub(crate) fn dim(s: impl Into<String>) -> Span<'static> { Span::styled(s.into(), Style::default().fg(C_DIM)) }

// ── v0.33.2 BOLD NEON helpers ──────────────────────────────────────────
/// A filled "banner" pill: bright text on a saturated neon background — used for the
/// loud status verdicts (LIGHT-SYNCED, SPINE BREAK, LIVE). ASCII-safe (text only).
pub(crate) fn banner(text: impl Into<String>, bg: Color) -> Span<'static> {
    Span::styled(format!(" {} ", text.into()),
        Style::default().bg(bg).fg(Color::Rgb(0x05, 0x05, 0x0d)).add_modifier(Modifier::BOLD))
}

/// A neon block-bar `█████░░░` sized to `frac` over `width` cells. Rich (uses █/░ — both
/// width-1, conhost-safe). Returns a styled Span in the given neon color.
pub(crate) fn neon_bar(frac: f64, width: usize, color: Color) -> Span<'static> {
    let f = (frac.clamp(0.0, 1.0) * width as f64).round() as usize;
    let s = "█".repeat(f.min(width)) + &"░".repeat(width.saturating_sub(f));
    Span::styled(s, Style::default().fg(color))
}

/// Bright value span (neon gold, bold) — the headline numbers.
pub(crate) fn val(s: impl Into<String>) -> Span<'static> {
    Span::styled(s.into(), Style::default().fg(C_NEON_GOLD).add_modifier(Modifier::BOLD))
}

/// thousands-grouped integer (1135287 → "1,135,287")
pub(crate) fn group(n: u64) -> String {
    let s = n.to_string(); let b = s.as_bytes(); let mut o = String::new();
    for (i, c) in b.iter().enumerate() { if i > 0 && (b.len() - i) % 3 == 0 { o.push(','); } o.push(*c as char); }
    o
}

pub(crate) fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() }
    else { format!("{}…", s.chars().take(n.saturating_sub(1)).collect::<String>()) }
}

/// Stable per-agent color from an id hash — premium control-panel feel, same agent always same hue.
pub(crate) fn agent_color(id: &str) -> Color {
    let pal = [C_CYAN, C_VBRIGHT, C_GREEN, C_GOLD, Color::Magenta, Color::LightBlue];
    let mut h: u32 = 2166136261;
    for b in id.bytes() { h = (h ^ b as u32).wrapping_mul(16777619); }
    pal[(h as usize) % pal.len()]
}

/// Inline unicode mini-bar: `value` normalized to `max` across `width` cells.
pub(crate) fn qug_bar(value: f64, max: f64, width: usize) -> String {
    if max <= 0.0 || width == 0 { return " ".repeat(width); }
    let filled = (((value / max) * width as f64).round() as usize).min(width);
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}

/// Medal glyph for a 0-based rank.
pub(crate) fn medal(rank: usize) -> &'static str {
    match rank { 0 => "🥇", 1 => "🥈", 2 => "🥉", _ => "  " }
}

/// Relative "Nm ago" from a unix-secs timestamp.
pub(crate) fn rel_time(at: u64) -> String {
    if at == 0 { return "—".into(); }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(at);
    let d = now.saturating_sub(at);
    if d < 60 { format!("{}s", d) }
    else if d < 3600 { format!("{}m", d / 60) }
    else if d < 86400 { format!("{}h", d / 3600) }
    else { format!("{}d", d / 86400) }
}

/// Status dot + color for an agent status string.
pub(crate) fn status_glyph(status: &str) -> Span<'static> {
    let (g, c) = match status.to_lowercase().as_str() {
        "working" | "busy" | "active" | "claimed" => ("●", C_GREEN),
        "idle" => ("○", C_DIM),
        "error" | "failed" => ("●", C_RED),
        _ => ("◦", C_DIM),
    };
    Span::styled(g, Style::default().fg(c))
}

pub(crate) fn render_tab_bar(app: &App) -> Paragraph<'static> {
    let tab = |label: &'static str, key: &'static str, t: Tab| -> Vec<Span<'static>> {
        let active = app.tab == t;
        let style = if active {
            Style::default().fg(C_BG).bg(C_NEON_CYAN).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(C_DIM)
        };
        vec![Span::styled(format!(" {key} {label} "), style), Span::raw(" ")]
    };
    let mut spans = vec![Span::raw(" ")];
    spans.extend(tab("Node", "1", Tab::Node));
    spans.extend(tab("Swarm AI", "2", Tab::SwarmAi));
    spans.extend(tab("Results", "3", Tab::Results));
    spans.extend(tab("Sync Log", "4", Tab::SyncLog));
    spans.extend(tab("Mining", "5", Tab::Mining));
    spans.push(Span::styled(" · Tab cycles", Style::default().fg(C_DIM)));
    Paragraph::new(Line::from(spans))
}

/// [2] MCP Swarm AI — the live job-index board from the Claude Code sessions.
pub(crate) fn render_swarm_ai(app: &App) -> Paragraph<'static> {
    let sw = &app.swarm;
    let mut lines: Vec<Line> = Vec::new();
    if let Some(e) = &sw.err {
        lines.push(Line::from(Span::styled(format!(" ⚠ {e}"), Style::default().fg(C_GOLD))));
        lines.push(Line::from(""));
    }
    let real: Vec<&SwarmAgent> = sw.agents.iter().filter(|a| !a.id.starts_with("test_")).collect();
    lines.push(Line::from(vec![
        Span::styled("  AGENTS ", Style::default().fg(C_CYAN).add_modifier(Modifier::BOLD)),
        Span::styled(format!("{}", real.len()), Style::default().fg(C_VBRIGHT)),
        Span::styled("   TASKS ", Style::default().fg(C_CYAN).add_modifier(Modifier::BOLD)),
        Span::styled(format!("{}", sw.tasks.len()), Style::default().fg(C_VBRIGHT)),
        Span::styled("   FILES ", Style::default().fg(C_CYAN).add_modifier(Modifier::BOLD)),
        Span::styled(format!("{}", sw.claims.len()), Style::default().fg(C_VBRIGHT)),
        Span::styled("   DONE ", Style::default().fg(C_CYAN).add_modifier(Modifier::BOLD)),
        Span::styled(format!("{}", sw.completed_count), Style::default().fg(C_GREEN)),
        Span::styled("   QUG PAID ", Style::default().fg(C_CYAN).add_modifier(Modifier::BOLD)),
        Span::styled(format!("{:.1}", sw.qug_paid), Style::default().fg(C_GOLD)),
    ]));
    lines.push(Line::from(""));
    // ── TASK BOARD: claimed jobs with priority + QUG bounty (the index board) ──
    let max_b = sw.tasks.iter().map(|t| t.est_qug).fold(0.0f64, f64::max);
    lines.push(Line::from(Span::styled(" ▸ TASK BOARD — claimed jobs · priority · bounty", Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD))));
    for t in sw.tasks.iter().take(7) {
        let (ptxt, pcol) = match t.priority {
            0 | 1 => (format!("P{}", t.priority), C_GOLD),
            2 => ("P2".to_string(), C_CYAN),
            _ => (format!("P{}", t.priority), C_DIM),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("  {:<3}", ptxt), Style::default().fg(pcol).add_modifier(Modifier::BOLD)),
            Span::styled(format!("{:<15}", trunc(&t.agent, 15)), Style::default().fg(agent_color(&t.agent)).add_modifier(Modifier::BOLD)),
            Span::styled(format!("{:<18}", trunc(&t.crates, 18)), Style::default().fg(C_CYAN)),
            Span::styled(qug_bar(t.est_qug, max_b, 8), Style::default().fg(C_GOLD)),
            Span::styled(format!(" {:>5.1} QUG", t.est_qug), Style::default().fg(C_GOLD)),
        ]));
    }
    lines.push(Line::from(""));
    // ── AGENTS leaderboard ──
    let max_q = real.iter().map(|a| a.qug).fold(0.0f64, f64::max);
    lines.push(Line::from(Span::styled(" ▸ AGENTS — leaderboard by QUG earned", Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD))));
    for (i, a) in real.iter().take(5).enumerate() {
        lines.push(Line::from(vec![
            Span::raw(format!("  {} ", medal(i))),
            status_glyph(&a.status),
            Span::styled(format!(" {:<20}", trunc(&a.id, 20)), Style::default().fg(agent_color(&a.id)).add_modifier(Modifier::BOLD)),
            Span::styled(qug_bar(a.qug, max_q, 10), Style::default().fg(C_GOLD)),
            Span::styled(format!(" {:>8.1} QUG", a.qug), Style::default().fg(C_GOLD)),
        ]));
    }
    lines.push(Line::from(""));
    // ── BROADCAST FEED: the human-readable coordination board ──
    lines.push(Line::from(Span::styled(" ▸ 📢 BROADCAST FEED", Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD))));
    for m in sw.feed.iter().take(5) {
        lines.push(Line::from(vec![
            Span::styled(format!("  {:>4} ", rel_time(m.at)), Style::default().fg(C_DIM)),
            Span::styled(format!("{:<16}", trunc(&m.from, 16)), Style::default().fg(agent_color(&m.from)).add_modifier(Modifier::BOLD)),
            Span::styled(trunc(&m.text, 56), Style::default().fg(C_DIM)),
        ]));
    }
    lines.push(Line::from(""));
    // ── LIVE ACTIVITY (compact) ──
    lines.push(Line::from(Span::styled(" ▸ LIVE ACTIVITY", Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD))));
    for ev in sw.activity.iter().filter(|e| !e.agent.starts_with("test_")).take(6) {
        lines.push(Line::from(vec![
            Span::styled(format!("  {:>4} ", rel_time(ev.at)), Style::default().fg(C_DIM)),
            Span::styled(format!("{:<16}", trunc(&ev.agent, 16)), Style::default().fg(agent_color(&ev.agent))),
            Span::styled(format!("{:<14}", trunc(&ev.kind, 14)), Style::default().fg(C_GOLD)),
            Span::styled(trunc(&ev.detail, 44), Style::default().fg(C_DIM)),
        ]));
    }
    Paragraph::new(lines).block(card_block(" ✦ MCP SWARM AI — JOB INDEX BOARD", C_NEON_PINK))
}

pub(crate) fn render_results(app: &App) -> Paragraph<'static> {
    let sw = &app.swarm;
    let mut lines: Vec<Line> = Vec::new();
    let mut totals: std::collections::HashMap<String, (f64, u32)> = std::collections::HashMap::new();
    for r in sw.results.iter().filter(|r| !r.agent.starts_with("test_")) {
        let e = totals.entry(r.agent.clone()).or_insert((0.0, 0));
        e.0 += r.qug; e.1 += 1;
    }
    let mut tv: Vec<(String, (f64, u32))> = totals.into_iter().collect();
    tv.sort_by(|a, b| b.1 .0.partial_cmp(&a.1 .0).unwrap_or(std::cmp::Ordering::Equal));
    let max_e = tv.iter().map(|(_, (q, _))| *q).fold(0.0f64, f64::max);
    lines.push(Line::from(Span::styled(" ▸ EARNINGS — leaderboard", Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD))));
    for (i, (ag, (qug, n))) in tv.iter().take(7).enumerate() {
        lines.push(Line::from(vec![
            Span::raw(format!("  {} ", medal(i))),
            Span::styled(format!("{:<22}", trunc(ag, 22)), Style::default().fg(agent_color(ag)).add_modifier(Modifier::BOLD)),
            Span::styled(qug_bar(*qug, max_e, 12), Style::default().fg(C_GOLD)),
            Span::styled(format!(" {:>3}t", n), Style::default().fg(C_DIM)),
            Span::styled(format!("{:>10.2} QUG", qug), Style::default().fg(C_GOLD)),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(" ▸ COMPLETED TASKS (newest first)", Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD))));
    for r in sw.results.iter().filter(|r| !r.agent.starts_with("test_")).take(13) {
        let mark = if r.success { Span::styled("✓", Style::default().fg(C_GREEN)) } else { Span::styled("✗", Style::default().fg(C_RED)) };
        lines.push(Line::from(vec![
            Span::raw(sa("  ")), mark, Span::raw(sa(" ")),
            Span::styled(format!("{:>4} ", rel_time(r.at)), Style::default().fg(C_DIM)),
            Span::styled(format!("{:<14}", trunc(&r.agent, 14)), Style::default().fg(agent_color(&r.agent))),
            Span::styled(format!("{:<18}", trunc(&r.crates, 18)), Style::default().fg(C_CYAN)),
            Span::styled(format!("{:>7.2} QUG", r.qug), Style::default().fg(C_GOLD)),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  TOTAL SETTLED: ", Style::default().fg(C_CYAN).add_modifier(Modifier::BOLD)),
        Span::styled(format!("{:.1} QUG", sw.qug_paid), Style::default().fg(C_GOLD).add_modifier(Modifier::BOLD)),
        Span::styled(format!("  ·  {} tasks completed", sw.completed_count), Style::default().fg(C_DIM)),
    ]));
    Paragraph::new(lines).block(card_block("🏆 RESULTS — SETTLED WORK", C_GOLD))
}

pub(crate) fn draw_ui(f: &mut Frame, app: &App) {
    let area = f.area();
    if let Some(until) = app.splash_until {
        if Instant::now() < until {
            f.render_widget(render_update_splash(app.splash_frame), area);
            return;
        }
    }
    // v0.13: tab bar between header and body — [1] Node · [2] Swarm AI · [3] Results.
    let [header_area, tab_area, body_area, footer_area] =
        Layout::vertical([Constraint::Length(2), Constraint::Length(1), Constraint::Min(0), Constraint::Length(2)]).areas(area);

    f.render_widget(render_header(app), header_area);
    f.render_widget(render_tab_bar(app), tab_area);

    match app.tab {
        Tab::Node => {
            // v0.33.3: SYNC full-width HERO band on top; v0.36.1: an equally-large MINING hero
            // (network power + personal mining) right below it; the other cards flow beneath.
            let [hero_sync, hero_mining, cards_area] =
                Layout::vertical([Constraint::Length(9), Constraint::Length(9), Constraint::Min(0)]).areas(body_area);
            draw_sync_hero(f, app, hero_sync);
            draw_mining_hero(f, app, hero_mining);
            draw_node_body(f, app, cards_area);
        }
        Tab::SwarmAi => f.render_widget(render_swarm_ai(app), body_area),
        Tab::Results => f.render_widget(render_results(app), body_area),
        Tab::SyncLog => f.render_widget(render_sync_log(app), body_area),
        Tab::Mining => draw_mining_tab(f, app, body_area),
    }

    f.render_widget(render_footer(app), footer_area);

    // LANE-B v0.50: float the SIGIL rune band ABOVE the live cockpit — drawn last
    // so it overlays every tab, fades on its own clock, and steals no layout slot.
    if let (Some(start), Some(until)) = (app.rune_started, app.rune_until) {
        if Instant::now() < until {
            draw_rune_band(f, app, area, start, until);
        }
    }

    // LANE-U v0.67: first-launch WELCOME — centered card with the SIGIL emblem + a giant F
    // call-to-action. Drawn LAST so it floats above everything; any key or 14s clears it.
    if app.welcome_until.map(|u| Instant::now() < u).unwrap_or(false) {
        draw_welcome_modal(f, area);
    }
}

pub(crate) fn render_fleet_card(app: &App) -> Paragraph<'static> {
    // Clone all fleet data to avoid borrow-from-app lifetime issues
    let nodes: Vec<(String, bool, u64, String)> = app.fleet_nodes.iter().map(|n| {
        let ver = if n.version.is_empty() { "?".to_string() }
            else if version_gt(VERSION, &n.version) { format!("!{}", n.version) }
            else { n.version.clone() };
        (n.name.clone(), n.online, n.height, ver)
    }).collect();
    let checking = app.fleet_last_check.elapsed() < Duration::from_secs(30);
    let total = nodes.len();
    let online = nodes.iter().filter(|n| n.1).count();
    let outdated = nodes.iter().filter(|n| n.1 && n.3.starts_with('!')).count();

    // v0.8: Mesh health from flux-p2p (if P2P sync is running)
    let mesh = app.p2p_state.mesh_peer_count;
    let mesh_quality = if mesh >= 4 { "healthy" } else if mesh >= 1 { "warming" } else { "empty" };
    let mesh_blk = app.p2p_blocks_synced;

    // Status line with fleet + mesh summary
    let status_color = if online == total && outdated == 0 { C_GREEN }
        else if online > 0 { C_GOLD }
        else { C_RED };
    let status_line = Line::from(vec![
        dim("fleet   "),
        Span::styled(format!("{}/{}", online, total),
            Style::default().fg(status_color).add_modifier(Modifier::BOLD)),
        dim(if checking { "  checking…" } else { "" }),
        if outdated > 0 {
            Span::styled(format!("  {} behind", outdated),
                Style::default().fg(C_RED).add_modifier(Modifier::BOLD))
        } else { Span::raw(sa("")) },
    ]);
    // v0.8: Mesh health line
    let mesh_line = Line::from(vec![
        dim("mesh    "),
        Span::styled(format!("{} peers", mesh), Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD)),
        dim("  "),
        Span::styled(mesh_quality, Style::default().fg(
            if mesh_quality == "healthy" { C_GREEN } else if mesh_quality == "warming" { C_GOLD } else { C_RED }
        )),
        dim(format!("  {} blk synced", group(mesh_blk))),
    ]);

    // Per-node lines with owned data
    let node_lines: Vec<Line> = nodes.iter().map(|(name, online, height, ver)| {
        let dot = if *online { Span::styled("●", Style::default().fg(C_GREEN)) }
                  else { Span::styled("○", Style::default().fg(C_RED)) };
        let ver_color = if ver == "?" { C_DIM }
            else if ver.starts_with('!') { C_RED }
            else { C_GREEN };
        Line::from(vec![
            dot,
            Span::raw(sa(" ")),
            Span::styled(name.clone(), Style::default().fg(C_CYAN)),
            dim(format!("  h{}", group(*height))),
            Span::raw(sa("  ")),
            Span::styled(ver.clone(), Style::default().fg(ver_color).add_modifier(Modifier::BOLD)),
        ])
    }).collect();

    let mut lines = vec![status_line, mesh_line];
    lines.extend(node_lines);

    Paragraph::new(lines).block(card_block(" ● FLEET · MESH", C_NEON_CYAN))
}

pub(crate) fn render_block_stream(app: &App) -> Paragraph<'static> {
    let tip_ok = app.verify.as_ref().map_or(false, |v| v.ok);
    let title: &'static str = if app.st.blocks_per_sec >= 5000.0 {
        " BLOCK STREAM · 5k+ blk/s"
    } else if app.st.blocks_per_sec >= 1000.0 {
        " BLOCK STREAM · 1k+ blk/s"
    } else if app.st.blocks_per_sec > 0.0 {
        " BLOCK STREAM · turbo"
    } else {
        " BLOCK STREAM · live"
    };
    let lines: Vec<Line> = if app.blocks.is_empty() {
        vec![Line::from(dim("streaming…"))]
    } else {
        app.blocks.iter().take(14).enumerate().map(|(i, b)| {
            let hash_pref: String = b.hash.chars().take_while(|c| c.is_ascii_hexdigit()).take(8).collect();
            let producer = if b.producer.is_empty() { "—".to_string() } else { b.producer.chars().take(12).collect() };
            // colored block marker (no dingbat): green tip, violet history
            let mark = if i == 0 && tip_ok { Span::styled("█ ", Style::default().fg(C_GREEN)) }
                       else { Span::styled("▌ ", Style::default().fg(C_VIOLET)) };
            Line::from(vec![
                mark,
                Span::styled(format!("{:>10} ", group(b.height)), Style::default().fg(C_GOLD)),
                Span::styled(format!("{}… ", hash_pref), Style::default().fg(C_DIM)),
                Span::styled(producer, Style::default().fg(C_CYAN)),
                dim(format!("  {}ms", b.tip_ms)),
            ])
        }).collect()
    };
    Paragraph::new(lines).block(card_block(&title, C_VBRIGHT))
}

pub(crate) fn render_cortex_card(app: &App) -> Paragraph<'static> {
    let agents_available = app.agents.iter().filter(|a| a.available).count();
    let agent_count = app.agents.len();
    let top_name: String = app.agents.iter()
        .max_by(|a, b| a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal))
        .map(|a| a.name.clone())
        .unwrap_or_else(|| "—".to_string());
    let mcp_combo_tool = app.mcp_combo_tool.clone();
    let mcp_combo_result = app.mcp_combo_result.clone();
    let last_cortex_gain = app.last_cortex_gain;
    let cortex_loops = app.cortex_loops;

    let agent_line = if agent_count == 0 {
        Line::from(dim("agents  — no registry loaded"))
    } else {
        Line::from(vec![
            dim("agents  "),
            Span::styled(format!("{}/{}", agents_available, agent_count),
                Style::default().fg(if agents_available > 0 { C_GREEN } else { C_RED }).add_modifier(Modifier::BOLD)),
            dim("  top "),
            Span::styled(top_name, Style::default().fg(C_CYAN)),
        ])
    };
    let combo_line = if mcp_combo_tool.is_empty() {
        Line::from(vec![
            dim("combo   "),
            Span::styled("idle", Style::default().fg(C_DIM)),
            dim("  [C] execute cortex loop"),
        ])
    } else {
        let running = mcp_combo_result.is_empty();
        Line::from(vec![
            dim("combo   "),
            Span::styled(mcp_combo_tool, Style::default().fg(C_GOLD).add_modifier(Modifier::BOLD)),
            dim(if running { "  running…" } else { "  ✓ done" }),
        ])
    };
    let cortex_line = if last_cortex_gain > 0.0 {
        Line::from(vec![
            dim("cortex  "),
            Span::styled(format!("+{:.1}%", last_cortex_gain),
                Style::default().fg(C_GREEN).add_modifier(Modifier::BOLD)),
            dim(format!("  loops {}", cortex_loops)),
        ])
    } else if cortex_loops > 0 {
        Line::from(vec![
            dim("cortex  "),
            Span::styled(format!("no gain  loops {}", cortex_loops),
                Style::default().fg(C_DIM)),
        ])
    } else {
        Line::from(vec![
            dim("cortex  "),
            Span::styled("idle  [C] run optimization loop",
                Style::default().fg(C_DIM)),
        ])
    };
    let mcp_line = if !mcp_combo_result.is_empty() {
        let preview: String = mcp_combo_result.chars().take(60).collect();
        Line::from(vec![
            dim("result  "),
            Span::styled(preview, Style::default().fg(C_VBRIGHT)),
        ])
    } else {
        Line::from(dim("result  —"))
    };
    let lines = vec![
        agent_line,
        combo_line,
        cortex_line,
        mcp_line,
    ];
    Paragraph::new(lines).block(card_block(" ◆ CORTEX MCP", C_NEON_GOLD))
}

pub(crate) fn render_footer(app: &App) -> Paragraph<'static> {
    let toast = if app.toast.is_empty() { String::new() } else { format!(" › {}", app.toast) };
    let keys = |c: &'static str, rest: &'static str, col: Color| -> Vec<Span<'static>> {
        vec![Span::styled(c, Style::default().fg(col).add_modifier(Modifier::BOLD)), dim(rest), Span::raw(sa("  "))]
    };
    let mut kb = vec![Span::raw(sa(" "))];
    kb.extend(keys("[M]", "ine", C_GOLD));
    kb.extend(keys("[F]", "ull", C_GREEN));
    kb.extend(keys("[V]", "erify", C_GREEN));
    kb.extend(keys("[Y]", "esync", C_CYAN));
    kb.extend(keys("[W]", "allet", C_CYAN));
    kb.extend(keys("[B]", "locks", C_CYAN));
    kb.extend(keys("[U]", "pdate", C_VBRIGHT));
    kb.extend(keys("[C]", "ortex", C_GOLD));
    kb.extend(keys("[H]", "eal", C_GOLD));
    kb.extend(keys("[N]", "odes", C_CYAN));
    kb.extend(keys("[L]", "ogin", C_VBRIGHT));
    kb.extend(keys("[Q]", "uit", C_RED));
    // v0.6.0: show serve status line
    let serve_line = if !app.serve_status.is_empty() {
        let short: String = app.serve_status.chars().take(72).collect();
        Line::from(Span::styled(format!(" ⚡ {}", short), Style::default().fg(C_GREEN)))
    } else {
        Line::from(Span::styled(" ⚡ fluxc serve :9800 · local wallet [W]", Style::default().fg(C_DIM)))
    };
    // v0.10.5.1: when offline, the top line becomes a calm status banner with a
    // live "offline for X · retry in Ns" countdown instead of a stale gold toast —
    // the operator always knows the cockpit is reconnecting, not frozen.
    let top_line = if !app.online {
        let dur = app.offline_since.map(|t| fmt_uptime(t.elapsed().as_secs())).unwrap_or_else(|| "0s".into());
        let txt = if app.refresh_inflight {
            format!(" ⚠ offline {} · reconnecting…", dur)
        } else {
            let next = app.refresh_delay().as_secs().saturating_sub(app.last_fetch.elapsed().as_secs());
            format!(" ⚠ offline {} · retry in {}s", dur, next)
        };
        Line::from(Span::styled(txt, Style::default().fg(C_RED).add_modifier(Modifier::BOLD)))
    } else {
        Line::from(Span::styled(toast, Style::default().fg(C_GOLD)))
    };
    Paragraph::new(vec![
        top_line,
        Line::from(kb),
        serve_line,
    ])
}
