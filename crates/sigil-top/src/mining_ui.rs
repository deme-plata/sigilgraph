// LANE-U: extracted from heroes.rs (pure move, no behavior change).
// `use super::*` reaches main.rs's private helpers/consts/App — the heroes.rs pattern.
#![allow(clippy::too_many_lines)]
use super::*;

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
