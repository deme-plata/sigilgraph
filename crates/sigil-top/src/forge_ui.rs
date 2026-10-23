//! ◇ SYNC card fed by YOUR NODE (10.6.1).
//!
//! 10.6.0 (2026-10-20) made sigil-top run the network's own node and replaced the ◇ SYNC card with a smaller
//! ◆ YOUR NODE band while it runs. Viktor, 2026-10-22: "in 10.5.0 the ui looked like this. i liked that better".
//! So the card keeps 10.5.0's shape — one big bar with a fixed label slot, telemetry lines, the starship and its
//! drive word — and every number comes from the node: height / finalized / network tip / peers from
//! `own_node::status` (the host fills it from the node's own local API), the apply rate measured here. One card,
//! one progress bar, the real engine underneath.
use super::*;

#[cfg(feature = "producer")]
use crate::own_node::status as ps;

/// Per-process motion state: the apply rate, measured from the node's height.
struct Motion {
    t0: Instant,
    sample_t: Instant,
    sample_h: u64,
    rate: f64,
}

static MOTION: std::sync::Mutex<Option<Motion>> = std::sync::Mutex::new(None);

/// What one frame needs from the motion state.
struct Beat {
    /// Seconds since the first frame.
    t: f64,
    /// Blocks applied per second (EMA over ≥1 s samples).
    rate: f64,
}

fn observe(height: u64) -> Beat {
    let now = Instant::now();
    let mut g = match MOTION.lock() { Ok(g) => g, Err(p) => p.into_inner() };
    let m = g.get_or_insert_with(|| Motion { t0: now, sample_t: now, sample_h: height, rate: 0.0 });
    let dt = now.duration_since(m.sample_t).as_secs_f64();
    if dt >= 1.0 {
        let jump = height.saturating_sub(m.sample_h);
        // A node resuming from its snapshot hands over a whole chain at once (0 → 42 M) — not an apply RATE, so
        // re-measure from there. 2026-10-23: judged by speed, not by step size. "More than 5,000 blocks in a sample"
        // was every sample once 10.6.7 applied 4,000-16,000 blk/s, and the card said "measuring speed…" all sync long.
        const HANDOVER_BLK_S: f64 = 250_000.0;
        if m.sample_h == 0 || height < m.sample_h || jump as f64 / dt > HANDOVER_BLK_S {
            m.rate = 0.0;
        } else {
            let inst = jump as f64 / dt;
            m.rate = if m.rate == 0.0 { inst } else { 0.7 * m.rate + 0.3 * inst };
        }
        m.sample_t = now;
        m.sample_h = height;
    }
    Beat { t: now.duration_since(m.t0).as_secs_f64(), rate: m.rate }
}

/// Your own node runs (always since 10.6.0, unless SIGIL_TOP_NODE=0) — the SYNC card then shows it.
pub(crate) fn band_visible() -> bool {
    #[cfg(feature = "producer")]
    {
        return ps::phase() != ps::IDLE;
    }
    #[allow(unreachable_code)]
    false
}

fn st(c: Color) -> Style { Style::default().fg(c) }
fn bold(c: Color) -> Style { Style::default().fg(c).add_modifier(Modifier::BOLD) }
fn sp(s: impl Into<String>, style: Style) -> Span<'static> { Span::styled(sa(s.into()), style) }

/// A path squeezed into `max` columns by cutting its MIDDLE — the drive and the file name are what a reader needs
/// to find it (10.6.0 cut the end off: `…\sigil-top-node-sigil-g3\sigil-top-nod` on Viktor's Windows console).
#[cfg_attr(not(feature = "producer"), allow(dead_code))]
fn fit_path(path: &str, max: usize) -> String {
    let n = path.chars().count();
    if n <= max || max < 12 {
        return path.to_string();
    }
    let head = (max - 1) / 3;
    let tail = max - 1 - head;
    let h: String = path.chars().take(head).collect();
    let t: String = path.chars().skip(n - tail).collect();
    format!("{h}…{t}")
}

/// At the tip = within this many blocks of the network (~90 s of chain), the same line the node's own card used.
#[cfg_attr(not(feature = "producer"), allow(dead_code))]
const AT_TIP: u64 = 768;

#[cfg(feature = "producer")]
pub(crate) fn draw_node_sync_card(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    ps::note_net_tip(app.target_height); // the light engine's view of the tip, until the node hears certificates
    let phase = ps::phase();
    let height = ps::height();
    let net = ps::net_tip().max(height);
    let fin = ps::settled();
    let lag = ps::lag();
    let peers = ps::peers();
    let beat = observe(height);
    let stopped = phase == ps::STOPPED;
    let synced = phase == ps::LIVE && lag <= AT_TIP;
    let connecting = !stopped && height == 0;
    // the first samples after a start are hand-over bursts, not a speed (10.6.0's node card: 30 s of measuring)
    let measuring = beat.t < 30.0 || beat.rate < 1.0;
    let frac = if net > 0 { (height as f64 / net as f64).clamp(0.0, 1.0) } else { 0.0 };

    let (vtext, vcol) = if stopped { ("✕ NODE STOPPED", C_NEON_PINK) }
        else if synced { ("◆ SYNCED", C_NEON_GREEN) }
        else if connecting { ("… CONNECTING", C_DIM) }
        else if phase == ps::LIVE { ("≈ CATCHING UP", C_NEON_GOLD) }
        else { ("⬇ SYNCING", C_NEON_GOLD) };

    let title = format!(" ◇ SYNC · {} · your node", crate::build_network_id());
    let block = card_block(&title, C_NEON_CYAN).border_style(st(vcol));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [bar_row, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
    let [tele, ship] = Layout::horizontal([Constraint::Min(0), Constraint::Length(20)]).spacing(1).areas(body);

    // ── BIG progress bar: the label lives in a fixed slot so the bar never jumps (10.4.28) ──
    const LABEL_SLOT: usize = 30;
    let label = pad_to_slot(format!(" {:>5.1}%  {}", frac * 100.0, vtext), LABEL_SLOT);
    let barw = (bar_row.width as usize).saturating_sub(display_width(&label) + 1).max(4);
    let fill = ((frac * barw as f64).round() as usize).min(barw);
    f.render_widget(Paragraph::new(Line::from(vec![
        sp("█".repeat(fill) + &"░".repeat(barw - fill), bold(vcol)),
        sp(label, bold(vcol)),
    ])), bar_row);

    // ── telemetry (left) — every line fits ~95 columns (10.6.0's were cut in a 120-column console) ──
    let rate = if beat.rate >= 10.0 { group(beat.rate.round() as u64) } else { format!("{:.1}", beat.rate) };
    let speed: Vec<Span<'static>> = if stopped {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        vec![sp(match ps::retry_at() {
            0 => "it starts again with sigil-top".to_string(),
            t => format!("restarting automatically in {}s", t.saturating_sub(now)),
        }, bold(C_GOLD)), sp(format!("   · restarts so far {}", ps::restarts()), st(C_DIM))]
    } else if synced {
        vec![sp(format!("rate {rate} blk/s"), bold(C_NEON_GOLD)), sp(" — the chain's own pace   eta —", st(C_DIM))]
    } else if measuring {
        vec![sp("applying — measuring speed…", bold(C_NEON_GOLD))]
    } else {
        vec![
            sp(format!("applying {rate} blk/s"), bold(C_NEON_GOLD)),
            sp("   eta ", st(C_DIM)), sp(fmt_eta(lag as f64 / beat.rate), bold(C_NEON_CYAN)),
            sp(format!("   {} to go", group(lag)), st(C_DIM)),
        ]
    };
    let fleet_total = app.fleet_nodes.len();
    let fleet_on = app.fleet_nodes.iter().filter(|n| n.online).count();
    let pid = ps::pid();
    let log = ps::log();
    let log_room = (tele.width as usize).saturating_sub(5);
    let (itxt, icol) = if stopped {
        let m = ps::message();
        (format!("⛓ node stopped — {}", if m.is_empty() { "the node is not running".to_string() } else { m }), C_NEON_PINK)
    } else if synced {
        (format!("⛓ INTEGRITY VERIFIED — genesis→{} re-executed here, every state root matched", group(height)), C_NEON_GREEN)
    } else {
        (format!("⛓ verifying — block 0 → {} re-executed here so far", group(height)), C_GOLD)
    };
    let (mesh, mcol) = if peers >= 8 { ("WIDE MESH", C_NEON_GREEN) }
        else if peers >= 3 { ("GROWING MESH", C_GOLD) }
        else { ("LOW PEER VISIBILITY", C_NEON_PINK) };
    let live = !stopped && (synced || beat.rate >= 0.5);
    let tlines = vec![
        Line::from(vec![
            sp("⛓ FULL NODE ", bold(C_NEON_GOLD)),
            sp("tip ", st(C_DIM)), sp(group(net), bold(C_VBRIGHT)),
            sp("   height ", st(C_DIM)), sp(format!("⛓{}", group(height)), bold(C_NEON_CYAN)),
            // a catching-up node reports no finalized height yet (0) — a dash, not a zero that reads as broken
            sp("   final ", st(C_DIM)), sp(if fin > 0 { group(fin) } else { "—".to_string() }, bold(if synced { C_NEON_GREEN } else { C_GOLD })),
        ]),
        Line::from(speed),
        Line::from(vec![
            sp("fleet ", st(C_DIM)), sp(format!("{fleet_on}/{fleet_total}"), st(if fleet_total > 0 && fleet_on == fleet_total { C_NEON_GREEN } else { C_GOLD })),
            sp("   peers ", st(C_DIM)), sp(group(peers), st(if peers >= 4 { C_NEON_GREEN } else if peers >= 1 { C_GOLD } else { C_RED })),
            sp(if pid > 0 { format!("   node pid {pid}") } else { String::new() }, st(C_DIM)),
        ]),
        Line::from(vec![
            sp("checks ", st(C_DIM)),
            sp("every block re-executed here; its state roots compared with the header", st(C_NEON_GREEN)),
        ]),
        Line::from(vec![sp(itxt, bold(icol))]),
        Line::from(vec![
            sp("◈ NETWORK HEALTH ", bold(C_NEON_CYAN)), sp(mesh, bold(mcol)),
            sp("   peers ", st(C_DIM)), sp(group(peers), st(if peers >= 1 { C_NEON_GREEN } else { C_RED })),
            sp("   ", st(C_DIM)), sp(if live { "● live" } else { "○ stalled" }, bold(if live { C_NEON_GREEN } else { C_RED })),
        ]),
        Line::from(if synced {
            vec![sp("your rig [M] mines through the network's producer; rewards land in your wallet", st(C_DIM))]
        } else {
            vec![
                sp("wallet [W] and mining [M] work now", st(C_VBRIGHT)),
                sp(" · the first sync happens once; later starts resume", st(C_DIM)),
            ]
        }),
        Line::from(vec![sp("log ", st(C_DIM)), sp(fit_path(&log, log_room), st(C_DIM))]),
    ];
    f.render_widget(Paragraph::new(tlines), tele);

    // ── static starship (right), as in 10.5.0 ──
    let (dtxt, dcol) = if synced { ("DOCKED", C_NEON_GREEN) }
        else if stopped || connecting { ("OFFLINE", C_RED) }
        else { ("ENGAGED", vcol) };
    let ship_lines = vec![
        Line::from(sp("    ╱╲    ", st(C_NEON_CYAN))),
        Line::from(sp("   ╱██╲   ", st(C_NEON_CYAN))),
        Line::from(sp("  ▕████▏  ", bold(C_VBRIGHT))),
        Line::from(sp("  ▕◆◆◆▏  ", st(dcol))),
        Line::from(sp("  ╱╲██╱╲  ", st(C_NEON_PINK))),
        Line::from(vec![sp(" drive ", st(C_DIM)), sp(dtxt, bold(dcol))]),
    ];
    f.render_widget(Paragraph::new(ship_lines), ship);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_log_path_keeps_its_drive_and_file_name() {
        let p = r"C:\Users\Viktor S. Kristensen\AppData\Local\sigil-top-node-sigil-g3\sigil-top-node.log";
        let got = fit_path(p, 60);
        assert_eq!(got.chars().count(), 60);
        assert!(got.starts_with(r"C:\Users\"), "{got}");
        assert!(got.ends_with(r"sigil-top-node.log"), "{got}");
        assert!(got.contains('…'));
        assert_eq!(fit_path(p, 200), p, "a path that fits is untouched");
        assert_eq!(fit_path(p, 5), p, "no room at all: leave it to the terminal");
    }

    #[test]
    fn a_snapshot_handover_is_not_read_as_an_apply_rate() {
        *MOTION.lock().unwrap() = None;
        observe(0);
        if let Some(m) = MOTION.lock().unwrap().as_mut() { m.sample_t -= Duration::from_secs(2); }
        let b = observe(42_466_285);
        assert_eq!(b.rate, 0.0, "0 → 42 M in one sample is a handover, not 21 M blk/s");
        if let Some(m) = MOTION.lock().unwrap().as_mut() { m.sample_t -= Duration::from_secs(2); }
        let b = observe(42_466_301);
        assert!((b.rate - 8.0).abs() < 0.5, "16 blocks in 2 s = 8 blk/s, got {}", b.rate);
        // 2026-10-23: a fast sync IS a rate — 15,000 blocks in one second read 0 under the old 5,000-block line.
        *MOTION.lock().unwrap() = None;
        observe(1_000_000);
        if let Some(m) = MOTION.lock().unwrap().as_mut() { m.sample_t -= Duration::from_secs(1); }
        let b = observe(1_015_000);
        assert!((b.rate - 15_000.0).abs() < 500.0, "15,000 blocks in 1 s = ~15,000 blk/s, got {}", b.rate);
    }
}
