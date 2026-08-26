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

    // Two extra rows for the SHIELDED panel (2026-08-27). A miner whose wallet has
    // published a shield key is paid in notes, so `/v1/balance` — the number every other
    // surface shows them — is frozen at whatever they held before registering, forever.
    // Live at the time this was added: 7.77 SIGIL transparent and unmoving, while the
    // same rig's shielded holdings grew past 100 SIGIL. Those two rows are the difference
    // between "mining is broken" and "mining is working and private".
    let [head, rates, tally, netrow, solverow, acctrow, shrow, poolrow, body, hint] = Layout::vertical([
        Constraint::Length(1), Constraint::Length(1), Constraint::Length(1), Constraint::Length(1),
        Constraint::Length(1), Constraint::Length(1), Constraint::Length(1), Constraint::Length(1),
        Constraint::Min(0), Constraint::Length(1),
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
    // The spendable (shielded) figure when we can compute it, the transparent one
    // otherwise — as (amount, label) so the label can never drift from the number.
    let bal_pair: (String, String) = {
        #[cfg(feature = "shield-register")]
        {
            match crate::shield_setup::latest_shielded() {
                Some(sn) => (
                    format!("{:.8} SIGIL", sn.balance as f64 / 1e8),
                    format!(" shielded  ·  {:.8} transparent", s.balance as f64 / 1e8),
                ),
                None => (format!("{:.8} SIGIL", s.balance as f64 / 1e8), " transparent".to_string()),
            }
        }
        #[cfg(not(feature = "shield-register"))]
        {
            (format!("{:.8} SIGIL", s.balance as f64 / 1e8), " transparent".to_string())
        }
    };
    let bal_span = (
        Span::styled(bal_pair.0, Style::default().fg(C_NEON_GREEN).add_modifier(Modifier::BOLD)),
        Span::styled(bal_pair.1, Style::default().fg(C_DIM)),
    );
    f.render_widget(Paragraph::new(Line::from(vec![
        dim(" shares "), Span::styled(format!("{} ✓", group(s.shares_ok)), Style::default().fg(C_NEON_GREEN).add_modifier(Modifier::BOLD)),
        Span::styled(format!("  {} ✗", group(s.shares_bad)), Style::default().fg(if s.shares_bad > 0 { C_RED } else { C_DIM })),
        dim("   accept "), Span::styled(format!("{accept:.0}%"), Style::default().fg(if accept >= 99.0 { C_NEON_GREEN } else { C_NEON_GOLD })),
        // Was `format!("{} SIGIL", s.balance)` — s.balance is RAW base units, so a
        // 7.77049863 SIGIL balance rendered as "777049863 SIGIL". SIGIL is 8-decimal;
        // printing raw units under a SIGIL label is off by 10^8 in the direction that
        // flatters, which is the worst direction for a money figure.
        //
        // And when a shielded snapshot exists, the SPENDABLE number leads. A registered
        // miner's transparent balance is frozen forever by construction, so showing only
        // it — on the one line an operator reads to answer "is mining paying me?" — says
        // "no" no matter how much they earn. Both numbers appear; neither is disguised as
        // the other.
        dim("   balance "),
        bal_span.0, bal_span.1,
        dim("   mine-chain h "), Span::styled(group(s.last_height), Style::default().fg(C_VBRIGHT)),
        dim(" (egen kaede - ikke produce-tippen)"),
    ])), tally);

    // v7.0.8: NETWORK row — total combined power of all miners + live difficulty + block cadence,
    // and your slice of it. net_hps is the SERVER's live sum of every active miner's
    // self-reported rate (sigil-api::mining::MiningBridge::report_hps/stats — each
    // wallet's LATEST reported rate, pruned after 30s idle), not a difficulty-derived
    // estimate (an earlier version of this comment said otherwise; corrected 2026-08-24).
    //
    // 2026-08-24: "your share" used to be silently clamped to 100% — so right after
    // starting to mine (or any time your LOCALLY-measured `hashrate`, which updates
    // continuously, has ramped ahead of the rate you last PUSHED to the server, which
    // only refreshes on each challenge fetch), the clamp hid a real, informative
    // number behind a falsely-clean "100.0%". Operator-reported live: two freshly-
    // started rigs (uptime <5min) both showed personal hashrate exceeding the
    // "network total" while share still read 100.0%. Now shown uncapped, with an
    // explicit "(ramping)" note above 100% so it reads as "you just started, the
    // server hasn't caught up yet" rather than looking like corrupted data.
    let your_share_raw = if s.net_hps > 1.0 { s.hashrate / s.net_hps * 100.0 } else { 0.0 };
    let ramping = your_share_raw > 100.0;
    let your_share_txt = if ramping {
        format!("{your_share_raw:.0}% (ramping — server hasn't caught up to your latest rate yet)")
    } else {
        format!("{your_share_raw:.1}%")
    };
    f.render_widget(Paragraph::new(Line::from(vec![
        dim(" ◈ network "), Span::styled(engine::format_hps(s.net_hps), Style::default().fg(C_NEON_CYAN).add_modifier(Modifier::BOLD)),
        dim(" total power"),
        dim("   difficulty "), Span::styled(format!("{} bits", s.net_bits), Style::default().fg(C_NEON_GOLD)),
        dim("   block "), Span::styled(format!("{:.1}s", s.net_block_ms / 1000.0), Style::default().fg(C_VBRIGHT)),
        dim("   your share "), Span::styled(your_share_txt, Style::default().fg(if ramping { C_NEON_GOLD } else { C_NEON_GREEN }).add_modifier(Modifier::BOLD)),
    ])), netrow);

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
    // ── SHIELDED: what this seed actually owns, and the pool it lives in ────────────
    {
        #[cfg(feature = "shield-register")]
        let snap = crate::shield_setup::latest_shielded();
        #[cfg(not(feature = "shield-register"))]
        let snap: Option<()> = None;

        #[cfg(feature = "shield-register")]
        match snap {
            Some(sn) => {
                let spendable = sn.balance as f64 / 1e8;
                f.render_widget(Paragraph::new(Line::from(vec![
                    Span::styled(" 🛡 SHIELDED", Style::default().fg(C_NEON_CYAN).add_modifier(Modifier::BOLD)),
                    dim("  yours "),
                    Span::styled(format!("{spendable:.8} SIGIL"), Style::default().fg(C_NEON_GREEN).add_modifier(Modifier::BOLD)),
                    dim("  notes "),
                    Span::styled(format!("{}", sn.owned), Style::default().fg(C_VBRIGHT)),
                    dim(if sn.spent > 0 { "  spent " } else { "" }),
                    Span::styled(
                        if sn.spent > 0 { format!("{}", sn.spent) } else { String::new() },
                        Style::default().fg(C_DIM),
                    ),
                    dim("   (transparent balance stays 0 — this is the real one)"),
                ])), shrow);

                // Pool fill matters to a miner in a way it does not to anyone else: at
                // capacity the pool ROTATES into a fresh generation, and on a build without
                // rotation it instead stops accepting notes and coinbases are dropped whole.
                let pct = sn.fill_pct();
                let fillcol = if pct >= 90.0 { C_NEON_PINK }
                    else if pct >= 70.0 { C_NEON_GOLD }
                    else { C_NEON_CYAN };
                let bars = ((pct / 5.0).round() as usize).min(20);
                let bar: String = "█".repeat(bars) + &"░".repeat(20 - bars);
                f.render_widget(Paragraph::new(Line::from(vec![
                    dim(" pool "),
                    Span::styled(bar, Style::default().fg(fillcol)),
                    Span::styled(format!(" {pct:.1}%"), Style::default().fg(fillcol).add_modifier(Modifier::BOLD)),
                    dim("  "),
                    Span::styled(format!("{}/{}", group(sn.pool_notes as u64), group(sn.pool_capacity as u64)),
                        Style::default().fg(C_VBRIGHT)),
                    dim("  locked "),
                    Span::styled(format!("{:.2}", sn.pool_locked as f64 / 1e8), Style::default().fg(C_GOLD)),
                    dim("  epoch "),
                    Span::styled(format!("{}", sn.epoch), Style::default().fg(C_NEON_CYAN).add_modifier(Modifier::BOLD)),
                    dim(if sn.sealed_epochs > 0 { "  sealed " } else { "" }),
                    Span::styled(
                        if sn.sealed_epochs > 0 { format!("{}", sn.sealed_epochs) } else { String::new() },
                        Style::default().fg(C_NEON_GREEN),
                    ),
                    dim("  registered "),
                    Span::styled(format!("{}", sn.registered), Style::default().fg(C_DIM)),
                ])), poolrow);
            }
            None => {
                f.render_widget(Paragraph::new(Line::from(vec![
                    Span::styled(" 🛡 SHIELDED", Style::default().fg(C_DIM).add_modifier(Modifier::BOLD)),
                    dim("  no seed — set SIGIL_MINE_SEED to see private rewards (rewards stay transparent without it)"),
                ])), shrow);
                f.render_widget(Paragraph::new(Line::from(dim(" pool  —"))), poolrow);
            }
        }
        #[cfg(not(feature = "shield-register"))]
        {
            let _ = snap;
            f.render_widget(Paragraph::new(Line::from(dim(" 🛡 SHIELDED  built without shield-register"))), shrow);
            f.render_widget(Paragraph::new(Line::from(dim(" pool  —"))), poolrow);
        }
    }

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
