//! ◆ PRODUCER hero band (10.4.27, Viktor 2026-10-15: "make it visible that node is producing
//! blocks and make a wonderful animation for this").
//!
//! What a full node does, drawn as it happens: blocks arrive from the network, wait out the
//! 512-deep finality window, and are SEALED onto this node's own settled chain — the seal
//! stamps once per block it settles. A block this node mints itself rides the belt in gold.
//!
//! Honest by construction: every number is read from `producer::status`, the same counters
//! the `[producer] heartbeat` log line prints. "Settling" (verifying + applying every g2 block
//! here) and "minting" (this node forging a block of its own) are shown apart, because on g2
//! the first happens every second and the second only when work reaches this node — the mint
//! policy is WORK, so a block is forged when the rig's [M] solve or a transaction arrives.
//! The animation never moves faster than the measured settle rate says it should.
use super::*;

#[cfg(feature = "producer")]
use crate::producer::status as ps;

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

/// Show the band? Only once a full node was asked for (F / persisted full / SIGIL_TOP_PRODUCE=1);
/// a light monitor keeps its screen as it was.
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
        return p == ps::PRODUCING || p == ps::SYNCING;
    }
    #[allow(unreachable_code)]
    false
}

fn st(c: Color) -> Style { Style::default().fg(c) }
fn bold(c: Color) -> Style { Style::default().fg(c).add_modifier(Modifier::BOLD) }
fn sp(s: impl Into<String>, style: Style) -> Span<'static> { Span::styled(sa(s.into()), style) }

#[derive(Clone, Copy, PartialEq)]
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

/// First replay height seen in this bootstrap (the snapshot's height) — the 0 % mark of the
/// bootstrap belt. Cleared whenever the producer is not bootstrapping.
static BOOT_FROM: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(feature = "producer")]
fn draw_inner(f: &mut Frame, area: ratatui::layout::Rect, net_hint: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    let phase = ps::phase();
    let settled = ps::settled();
    let net = ps::gossip_tip();
    let minted = ps::minted();
    let won = ps::won();
    // "Ours" = blocks this node forged + blocks the rig WON (minted by the network's producer
    // with this wallet as producer). Either one rides the belt in gold.
    let ours = minted + won;
    let last_h = ps::last_minted_height().max(ps::last_won_height());
    let lag = ps::lag();
    let beat = observe(settled, ours, last_h);

    let mood = match phase {
        ps::PRODUCING if lag > 768 => Mood::CatchingUp,
        ps::PRODUCING if ours > 0 => Mood::Forging,
        ps::PRODUCING => Mood::Armed,
        ps::SYNCING => Mood::Booting,
        _ => Mood::Refused,
    };
    let (title, col) = match mood {
        Mood::Forging => (if minted > 0 { " ⛏ PRODUCER · FORGING BLOCKS" } else { " ⛏ PRODUCER · YOUR RIG WINS BLOCKS" }, C_NEON_GOLD),
        Mood::Armed => (" ◆ PRODUCER · FULL NODE LIVE", C_NEON_GREEN),
        Mood::CatchingUp => (" ◆ PRODUCER · CATCHING UP", C_GOLD),
        Mood::Booting => (" ◆ PRODUCER · BOOTSTRAPPING", C_NEON_CYAN),
        Mood::Refused => (" ✕ PRODUCER · NOT STARTED", C_NEON_PINK),
    };
    let block = card_block(&sa(title), col).border_style(st(col));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [belt_row, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
    let [tele, art] = Layout::horizontal([Constraint::Min(0), Constraint::Length(16)]).spacing(1).areas(body);

    // Bootstrap progress: from the snapshot's height to the network tip (the light client's
    // view of it when the producer has none yet).
    let replayed = ps::replayed();
    let target = ps::replay_target().max(net).max(net_hint);
    if mood != Mood::Booting {
        BOOT_FROM.store(0, Relaxed);
    } else if replayed > 0 && BOOT_FROM.load(Relaxed) == 0 {
        BOOT_FROM.store(replayed, Relaxed);
    }
    let from = BOOT_FROM.load(Relaxed);
    let progress = (from > 0 && target > from && replayed >= from)
        .then(|| (replayed - from) as f64 / (target - from) as f64);
    f.render_widget(Paragraph::new(belt(belt_row.width as usize, &beat, mood, settled, progress)), belt_row);

    let rate = if beat.rate >= 10.0 { format!("{:.0} blk/s", beat.rate) } else { format!("{:.1} blk/s", beat.rate) };
    let lines: Vec<Line<'static>> = match mood {
        Mood::Armed | Mood::Forging | Mood::CatchingUp => {
            let headline = match mood {
                Mood::Forging => vec![
                    sp(if minted > 0 { "⛏ FORGING" } else { "⛏ WINNING" }, bold(C_NEON_GOLD)),
                    sp(" — blocks with your wallet as producer are in the chain, and this node seals every block", st(C_DIM)),
                ],
                Mood::CatchingUp => vec![
                    sp("◆ CATCHING UP", bold(C_GOLD)),
                    sp(format!(" — {} blocks behind the network — the resync lane catches up after 20 s without progress", group(lag)), st(C_DIM)),
                ],
                _ => vec![
                    sp("◆ ARMED", bold(C_NEON_GREEN)),
                    sp(" — this node verifies and seals every g2 block itself, with no trusted peer", st(C_DIM)),
                ],
            };
            let secs = if beat.rate > 0.5 { format!(" ≈{:.0} s", lag as f64 / beat.rate) } else { String::new() };
            let how = if ps::minting() {
                "  · this node also forges its own when your rig solves against it"
            } else {
                "  · your rig [M] mines through the network's producer; a block it wins carries your wallet"
            };
            let mine = if ours == 0 {
                vec![
                    sp("your blocks ", st(C_DIM)), sp("0 yet", bold(C_GOLD)),
                    sp(how, st(C_DIM)),
                ]
            } else {
                let mut v = vec![
                    sp("your blocks ", st(C_DIM)), sp(group(ours), bold(C_NEON_GOLD)),
                    sp("  last ", st(C_DIM)), sp(format!("h={}", group(last_h)), bold(C_NEON_GOLD)),
                ];
                if minted > 0 {
                    v.push(sp(format!("  ({} won by your rig, {} forged here)", group(won), group(minted)), st(C_DIM)));
                } else {
                    v.push(sp("  · won by your rig, settled and checked here", st(C_DIM)));
                }
                v
            };
            vec![
                Line::from(headline),
                Line::from(vec![
                    sp("sealed ", st(C_DIM)), sp(rate, bold(C_NEON_GREEN)),
                    sp("   net tip ", st(C_DIM)), sp(group(net), bold(C_VBRIGHT)),
                    sp("   finality lag ", st(C_DIM)), sp(format!("{}{secs}", group(lag)), bold(if lag > 768 { C_GOLD } else { C_NEON_CYAN })),
                    sp(" (the 512-deep rule)", st(C_DIM)),
                ]),
                Line::from(mine),
                Line::from(vec![
                    sp("peers ", st(C_DIM)), sp(group(ps::peers()), bold(if ps::peers() > 0 { C_NEON_GREEN } else { C_RED })),
                    sp("   gossip in ", st(C_DIM)), sp(group(ps::ingested()), st(C_VBRIGHT)),
                    sp("   resyncs ", st(C_DIM)), sp(group(ps::resyncs()), st(C_VBRIGHT)),
                ]),
            ]
        }
        Mood::Booting => {
            let what = ps::line().unwrap_or_default().trim_start_matches("full node: ").to_string();
            vec![
                Line::from(vec![sp("◆ BOOTSTRAPPING", bold(C_NEON_CYAN)), sp(format!(" — {what}"), st(C_DIM))]),
                Line::from(vec![
                    sp("then this node seals every block itself and counts every block your rig [M] wins", st(C_DIM)),
                ]),
                Line::from(match progress {
                    Some(p) => vec![
                        sp("replayed ", st(C_DIM)), sp(group(replayed.saturating_sub(from)), bold(C_NEON_CYAN)),
                        sp(" of ", st(C_DIM)), sp(group(target.saturating_sub(from)), bold(C_VBRIGHT)),
                        sp(format!(" blocks since the snapshot ({:.0} %) — each one applied and checked here", p * 100.0), st(C_DIM)),
                    ],
                    None => vec![sp("the snapshot is signed by the g2 producer and checked against a key built into sigil-top", st(C_DIM))],
                }),
            ]
        }
        Mood::Refused => {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            let retry = match ps::retry_at() {
                0 => "will try again on the next start".to_string(),
                t => format!("trying again automatically in {}s", t.saturating_sub(now)),
            };
            let msg = ps::message();
            vec![
                Line::from(vec![sp("✕ NOT STARTED", bold(C_NEON_PINK)), sp(format!(" — {}", if msg.is_empty() { "bootstrap failed" } else { &msg }), st(C_DIM))]),
                Line::from(vec![sp(retry, bold(C_GOLD))]),
                Line::from(vec![sp("the history download above is separate and keeps going; details in ~/.sigil-top.log", st(C_DIM))]),
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
