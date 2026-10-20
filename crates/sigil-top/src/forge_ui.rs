//! ◆ YOUR NODE card (10.4.27 as the PRODUCER band; 10.6.0 your own node — Viktor 2026-10-20: "running sigil
//! graph top node should just do it all").
//!
//! What your node does, drawn as it happens: it downloads every block, checks it and applies it — the same
//! program the network's own nodes run — and the seal stamps once per block it applies. Honest by construction:
//! every number is read from `own_node::status`, which the host fills from the node's own local API. The
//! animation never moves faster than the measured apply rate says it should.
use super::*;

#[cfg(feature = "producer")]
use crate::own_node::status as ps;

/// Per-process motion state: settle rate + the moments the seal last struck / a block was minted.
struct Motion {
    t0: Instant,
    sample_t: Instant,
    sample_h: u64,
    rate: f64,
    seen_h: u64,
    last_settle: Option<Instant>,
    seen_minted: u64,
    mint_flash: Option<(Instant, u64)>,
}

static MOTION: std::sync::Mutex<Option<Motion>> = std::sync::Mutex::new(None);

/// What one frame needs from the motion state.
struct Beat {
    /// Seconds since the first frame — drives the belt.
    t: f64,
    /// Blocks settled per second (EMA over ≥1 s samples).
    rate: f64,
    /// The seal struck within the last 260 ms.
    strike: bool,
    /// A block of OUR OWN was minted in the last 8 s: (seconds ago, height).
    mint: Option<(f64, u64)>,
}

fn observe(settled: u64, minted: u64, last_minted_h: u64) -> Beat {
    let now = Instant::now();
    let mut g = match MOTION.lock() { Ok(g) => g, Err(p) => p.into_inner() };
    let m = g.get_or_insert_with(|| Motion {
        t0: now, sample_t: now, sample_h: settled, rate: 0.0, seen_h: settled,
        last_settle: None, seen_minted: minted, mint_flash: None,
    });
    if settled > m.seen_h {
        m.last_settle = Some(now);
        m.seen_h = settled;
    } else if settled < m.seen_h {
        m.seen_h = settled; // a resync reseeded the chain lower — follow it, no strike
    }
    let dt = now.duration_since(m.sample_t).as_secs_f64();
    if dt >= 1.0 {
        let jump = settled.saturating_sub(m.sample_h);
        // The bootstrap hands over a whole chain at once (0 → 42 M); a resync replays a few
        // thousand blocks in seconds. Neither is a settle RATE — re-measure from here.
        if m.sample_h == 0 || settled < m.sample_h || jump > 5_000 {
            m.rate = 0.0;
        } else {
            let inst = jump as f64 / dt;
            m.rate = if m.rate == 0.0 { inst } else { 0.7 * m.rate + 0.3 * inst };
        }
        m.sample_t = now;
        m.sample_h = settled;
    }
    if minted > m.seen_minted {
        m.seen_minted = minted;
        m.mint_flash = Some((now, last_minted_h));
    }
    let mint = m.mint_flash.and_then(|(at, h)| {
        let ago = now.duration_since(at).as_secs_f64();
        (ago < 8.0).then_some((ago, h))
    });
    Beat {
        t: now.duration_since(m.t0).as_secs_f64(),
        rate: m.rate,
        strike: m.last_settle.map(|t| now.duration_since(t) < Duration::from_millis(260)).unwrap_or(false),
        mint,
    }
}

/// Show the band whenever your own node runs (10.6.0: always, unless SIGIL_TOP_NODE=0).
pub(crate) fn band_visible() -> bool {
    #[cfg(feature = "producer")]
    {
        return ps::phase() != ps::IDLE;
    }
    #[allow(unreachable_code)]
    false
}

/// Wants smooth frames (the belt moves) — the TUI loop polls a little faster while this holds.
pub(crate) fn wants_motion() -> bool {
    #[cfg(feature = "producer")]
    {
        let p = ps::phase();
        return p == ps::LIVE || p == ps::SYNCING;
    }
    #[allow(unreachable_code)]
    false
}

fn st(c: Color) -> Style { Style::default().fg(c) }
fn bold(c: Color) -> Style { Style::default().fg(c).add_modifier(Modifier::BOLD) }
fn sp(s: impl Into<String>, style: Style) -> Span<'static> { Span::styled(sa(s.into()), style) }

#[derive(Clone, Copy, PartialEq)]
#[allow(dead_code)] // `Forging` (a block your rig won, in gold) waits for the node to report wins; the tests draw it.
enum Mood { Forging, Armed, CatchingUp, Booting, Refused }

/// The conveyor: `net › ▪  ▪  ▪ ┤◆├ ■ ■ ■ ■ ⛓ h`. Left = blocks seen on the network still inside
/// the 512-deep finality window; the gate = the seal; right = this node's settled chain.
fn belt(width: usize, beat: &Beat, mood: Mood, settled: u64, progress: Option<f64>) -> Line<'static> {
    let head = " net › ";
    let tail = format!(" ⛓ settled {} ", group(settled));
    let gate_w = 3;
    let track = width.saturating_sub(display_width(head) + display_width(&tail) + gate_w);
    if track < 8 {
        return Line::from(sp(format!("⛓ settled {}", group(settled)), bold(C_NEON_GREEN)));
    }
    let left_w = (track * 11) / 20;
    let right_w = track - left_w;
    // Cells per second: follows the measured settle rate, clamped so 0.3 blk/s still moves
    // visibly and a 1,500 blk/s catch-up does not turn into noise.
    let speed = (beat.rate * 0.75).clamp(1.5, 10.0);
    let shift = (beat.t * speed).floor() as i64;
    let mut spans: Vec<Span<'static>> = vec![sp(head, st(C_DIM))];

    match mood {
        Mood::Refused => {
            spans.push(sp("·".repeat(left_w), st(C_DIM)));
            spans.push(sp("┤✕├", bold(C_RED)));
            spans.push(sp("·".repeat(right_w), st(C_DIM)));
        }
        Mood::Booting => {
            // Bootstrapping: the left belt fills as the replay advances, a shimmer runs over it.
            let frac = progress.unwrap_or(0.0).clamp(0.0, 1.0);
            let fill = ((frac * left_w as f64).round() as usize).min(left_w);
            let glint = (shift.rem_euclid(left_w.max(1) as i64)) as usize;
            let mut s = String::with_capacity(left_w * 3);
            for i in 0..left_w {
                s.push(if i == glint { '▓' } else if i < fill { '▒' } else { '░' });
            }
            spans.push(sp(s, st(C_NEON_CYAN)));
            spans.push(sp("┤…├", bold(C_NEON_CYAN)));
            spans.push(sp("·".repeat(right_w), st(C_DIM)));
        }
        _ => {
            // Incoming: a block every 4 cells, moving right, brightening as it nears the seal.
            for i in 0..left_w {
                let on = (i as i64 - shift).rem_euclid(4) == 0;
                let near = i as f64 / left_w.max(1) as f64;
                let c = if near > 0.75 { C_NEON_CYAN } else if near > 0.35 { C_VBRIGHT } else { C_DIM };
                spans.push(sp(if on { "▪" } else { " " }, st(c)));
            }
            let (g, gc) = if beat.strike { ("┤✦├", bold(C_NEON_GOLD)) } else { ("┤◆├", bold(C_NEON_GREEN)) };
            spans.push(sp(g, gc));
            // Settled chain: denser, green, fading toward the far end; our own block rides in gold.
            let mint_pos = beat.mint.map(|(ago, _)| (ago * speed * 1.4) as usize);
            for i in 0..right_w {
                if Some(i) == mint_pos {
                    spans.push(sp("◆", bold(C_NEON_GOLD)));
                    continue;
                }
                let on = (i as i64 - shift).rem_euclid(2) == 0;
                let fade = i as f64 / right_w.max(1) as f64;
                let c = if i == 0 && beat.strike { C_NEON_GOLD } else if fade < 0.45 { C_NEON_GREEN } else if fade < 0.8 { C_GREEN } else { C_DIM };
                spans.push(sp(if on { "■" } else { " " }, st(c)));
            }
        }
    }
    let tail_c = if mood == Mood::Refused { C_DIM } else { C_NEON_GREEN };
    spans.push(sp(tail, bold(tail_c)));
    Line::from(spans)
}

/// The seal (right side): a stamp that presses once per settled block. Gold when OUR block.
fn seal(beat: &Beat, mood: Mood) -> Vec<Line<'static>> {
    let down = beat.strike && matches!(mood, Mood::Armed | Mood::Forging | Mood::CatchingUp);
    let ours = beat.mint.is_some();
    let body_c = match mood { Mood::Refused => C_DIM, Mood::Booting => C_NEON_CYAN, _ => C_VBRIGHT };
    let block_c = if ours { C_NEON_GOLD } else if down { C_NEON_GOLD } else if mood == Mood::Refused { C_DIM } else { C_NEON_GREEN };
    let (r0, r1, r2) = if down {
        ("      │       ", "     ▄█▄      ", "   ✦▐███▌✦    ")
    } else {
        ("     ▄█▄      ", "    ▐███▌     ", "              ")
    };
    let blk = if ours { "◆" } else if down { "◆" } else { "■" };
    let (word, wc) = match mood {
        _ if ours => ("YOUR BLOCK", C_NEON_GOLD),
        Mood::Forging => ("FORGING", C_NEON_GOLD),
        Mood::Armed => (if down { "SEALED" } else { "ARMED" }, if down { C_NEON_GOLD } else { C_NEON_GREEN }),
        Mood::CatchingUp => ("CATCH-UP", C_GOLD),
        Mood::Booting => ("WARMING", C_NEON_CYAN),
        Mood::Refused => ("COLD", C_RED),
    };
    vec![
        Line::from(sp(r0, bold(body_c))),
        Line::from(sp(r1, bold(body_c))),
        Line::from(if down {
            vec![sp("   ✦", bold(C_NEON_GOLD)), sp("▐███▌", bold(body_c)), sp("✦    ", bold(C_NEON_GOLD))]
        } else {
            vec![sp(r2, st(body_c))]
        }),
        Line::from(vec![sp("  ══[", st(C_DIM)), sp(blk, bold(block_c)), sp("]════  ", st(C_DIM))]),
        Line::from(vec![sp(" seal ", st(C_DIM)), sp(word, bold(wc))]),
    ]
}

pub(crate) fn draw_producer_hero(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    #[cfg(feature = "producer")]
    draw_inner(f, area, app.target_height);
    #[cfg(not(feature = "producer"))]
    let _ = (f, app, area);
}

#[cfg(feature = "producer")]
fn draw_inner(f: &mut Frame, area: ratatui::layout::Rect, net_hint: u64) {
    ps::note_net_tip(net_hint); // the light engine's view of the network tip, until the node hears certificates
    let phase = ps::phase();
    let height = ps::height();
    let net = ps::net_tip();
    let lag = ps::lag();
    let beat = observe(height, 0, 0);

    let mood = match phase {
        ps::LIVE if lag > 768 => Mood::CatchingUp,
        ps::LIVE => Mood::Armed,
        ps::SYNCING => Mood::Booting,
        _ => Mood::Refused,
    };
    let (title, col) = match mood {
        Mood::CatchingUp => (" ◆ YOUR NODE · CATCHING UP", C_GOLD),
        Mood::Booting => (" ◆ YOUR NODE · SYNCING", C_NEON_CYAN),
        Mood::Refused => (" ✕ YOUR NODE · STOPPED", C_NEON_PINK),
        _ => (" ◆ YOUR NODE · LIVE", C_NEON_GREEN),
    };
    let block = card_block(&sa(title), col).border_style(st(col));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [belt_row, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
    let [tele, art] = Layout::horizontal([Constraint::Min(0), Constraint::Length(16)]).spacing(1).areas(body);

    // Progress = the share of the chain this node holds, block 0 → the network tip.
    let progress = (mood == Mood::Booting && net > 0).then(|| (height as f64 / net as f64).min(1.0));
    f.render_widget(Paragraph::new(belt(belt_row.width as usize, &beat, mood, height, progress)), belt_row);

    let rate = if beat.rate >= 10.0 { format!("{:.0} blk/s", beat.rate) } else { format!("{:.1} blk/s", beat.rate) };
    let peers = ps::peers();
    let log = ps::log();
    let lines: Vec<Line<'static>> = match mood {
        Mood::Booting => {
            // The first samples after a start are hand-over bursts, not a speed (2026-10-20: the card read 47 blk/s
            // and "≈2.3 h" while the node really did ~850): no speed or ETA until 30 s of measuring.
            let measuring = beat.t < 30.0;
            let rate = if measuring { "measuring speed…".to_string() } else { rate.clone() };
            let eta = if !measuring && beat.rate > 1.0 && net > height {
                let s = (net - height) as f64 / beat.rate;
                if s < 120.0 { format!(" · ≈{s:.0} s to go") } else if s < 7200.0 { format!(" · ≈{:.0} min to go", s / 60.0) } else { format!(" · ≈{:.1} h to go", s / 3600.0) }
            } else { String::new() };
            vec![
                Line::from(vec![
                    sp("◆ SYNCING", bold(C_NEON_CYAN)),
                    sp(" — every block from block 0 is downloaded, checked and applied here, by the same program the network runs", st(C_DIM)),
                ]),
                Line::from(match progress {
                    Some(p) => vec![
                        sp("h ", st(C_DIM)), sp(group(height), bold(C_NEON_CYAN)),
                        sp(" of ~", st(C_DIM)), sp(group(net), bold(C_VBRIGHT)),
                        sp(format!(" ({:.1} %) · {rate}{eta}", p * 100.0), st(C_DIM)),
                    ],
                    None => vec![sp("starting — finding peers and the network's tip", st(C_DIM))],
                }),
                Line::from(vec![
                    sp("peers ", st(C_DIM)), sp(group(peers), bold(if peers > 0 { C_NEON_GREEN } else { C_RED })),
                    sp(format!("   log {log}"), st(C_DIM)),
                ]),
            ]
        }
        Mood::Refused => {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            let retry = match ps::retry_at() {
                0 => "it starts again with sigil-top".to_string(),
                t => format!("restarting automatically in {}s", t.saturating_sub(now)),
            };
            let msg = ps::message();
            vec![
                Line::from(vec![sp("✕ STOPPED", bold(C_NEON_PINK)), sp(format!(" — {}", if msg.is_empty() { "the node is not running" } else { &msg }), st(C_DIM))]),
                Line::from(vec![sp(retry, bold(C_GOLD)), sp(format!("   · restarts so far {}", ps::restarts()), st(C_DIM))]),
                Line::from(vec![sp(format!("the node's own log: {log}"), st(C_DIM))]),
            ]
        }
        _ => {
            let headline = if mood == Mood::CatchingUp {
                vec![
                    sp("◆ CATCHING UP", bold(C_GOLD)),
                    sp(format!(" — {} blocks behind the network's tip", group(lag)), st(C_DIM)),
                ]
            } else {
                vec![
                    sp("◆ LIVE", bold(C_NEON_GREEN)),
                    sp(" — this node checks and applies every block itself: the same program the network's own nodes run", st(C_DIM)),
                ]
            };
            vec![
                Line::from(headline),
                Line::from(vec![
                    sp("applied ", st(C_DIM)), sp(rate, bold(C_NEON_GREEN)),
                    sp("   h ", st(C_DIM)), sp(group(height), bold(C_VBRIGHT)),
                    sp("   finalized ", st(C_DIM)), sp(group(ps::settled()), bold(C_NEON_CYAN)),
                    sp("   network ", st(C_DIM)), sp(group(net), st(C_VBRIGHT)),
                ]),
                Line::from(vec![
                    sp("your rig [M] mines through the network's producer; rewards land in your wallet", st(C_DIM)),
                ]),
                Line::from(vec![
                    sp("peers ", st(C_DIM)), sp(group(peers), bold(if peers > 0 { C_NEON_GREEN } else { C_RED })),
                    sp(format!("   log {log}"), st(C_DIM)),
                ]),
            ]
        }
    };
    f.render_widget(Paragraph::new(lines), tele);
    f.render_widget(Paragraph::new(seal(&beat, mood)), art);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn beat(strike: bool, mint: Option<(f64, u64)>) -> Beat { Beat { t: 3.0, rate: 8.0, strike, mint } }

    fn text(l: &Line<'_>) -> String { l.spans.iter().map(|s| s.content.as_ref()).collect() }

    #[test]
    fn belt_fits_its_row_exactly_in_every_mood() {
        for w in [40usize, 80, 120, 171] {
            for mood in [Mood::Armed, Mood::Forging, Mood::CatchingUp, Mood::Booting, Mood::Refused] {
                let l = belt(w, &beat(false, None), mood, 42_454_075, Some(0.4));
                assert_eq!(display_width(&text(&l)), w, "width {w} mood drifted");
            }
        }
    }

    #[test]
    fn our_minted_block_rides_the_belt_in_gold() {
        let l = belt(120, &beat(false, Some((0.2, 42_454_100))), Mood::Forging, 42_454_075, None);
        assert!(l.spans.iter().any(|s| s.content == "◆" && s.style.fg == Some(C_NEON_GOLD)));
    }

    #[test]
    fn a_bootstrap_handover_is_not_read_as_a_settle_rate() {
        *MOTION.lock().unwrap() = None;
        observe(0, 0, 0);
        if let Some(m) = MOTION.lock().unwrap().as_mut() { m.sample_t -= Duration::from_secs(2); }
        let b = observe(42_466_285, 0, 0);
        assert_eq!(b.rate, 0.0, "0 → 42 M in one sample is a handover, not 21 M blk/s");
        if let Some(m) = MOTION.lock().unwrap().as_mut() { m.sample_t -= Duration::from_secs(2); }
        let b = observe(42_466_301, 0, 0);
        assert!((b.rate - 8.0).abs() < 0.5, "16 blocks in 2 s = 8 blk/s, got {}", b.rate);
    }

    #[test]
    fn seal_rows_have_one_width() {
        for (strike, mint) in [(false, None), (true, None), (false, Some((1.0, 7)))] {
            for mood in [Mood::Armed, Mood::Booting, Mood::Refused] {
                let rows = seal(&beat(strike, mint), mood);
                assert_eq!(rows.len(), 5);
                for r in &rows[..4] { assert!(display_width(&text(r)) <= 15, "{:?}", text(r)); }
            }
        }
    }
}
