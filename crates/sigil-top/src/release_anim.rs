//! The release ceremony: a full-screen animated sequence that plays once after a
//! successful self-update, with the changelog underneath it.
//!
//! # Why this exists
//!
//! An auto-updater that prints `updated to v7.3.3` teaches the user nothing and feels like
//! nothing. A release is the one moment they are already looking at the screen, so it is
//! the one moment worth spending frames on — and the changelog is worth more than the
//! animation, so the animation is over in ten seconds and the changelog stays.
//!
//! # Structure — built to be extended
//!
//! The sequence is a list of [`Act`]s with explicit time windows. Adding a new act for a
//! future release means appending one entry to [`SEQUENCE`] and one match arm to
//! [`draw_act`]; nothing else needs to know. Acts overlap deliberately — the seal is still
//! settling while the wordmark starts assembling — because hard cuts read as a slideshow
//! and crossfades read as a film.
//!
//! Every act is a function of ONE number, `t` ∈ [0,1] through its own window, so the whole
//! thing is deterministic, frame-rate independent, and resizes cleanly. No frame counters.
//!
//! The acts are not decoration; each is something SIGIL actually does:
//!   VOID  — entropy before ordering
//!   BRAID — the DagKnight braid weaving a DAG into one order
//!   LANES — the dual-lane proof of work: hash lane × verifiable-delay lane, converging
//!   SEAL  — the shielded pool closing over the ledger; plaintext becomes redaction
//!   MARK  — the sigil itself igniting out of the particles
//!   STAMP — version, tagline, and the provenance fingerprint that signs the binary

use std::time::{Duration, Instant};

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};
use ratatui::Frame;

// ── the changelog shown under the animation ────────────────────────────────────────────

/// One line of "what changed". `kind` drives the colour and the glyph.
pub struct Change {
    pub kind: ChangeKind,
    pub text: &'static str,
}

#[derive(Clone, Copy, PartialEq)]
pub enum ChangeKind {
    /// A new capability.
    Added,
    /// Behaviour that changed, including bug fixes.
    Fixed,
    /// Something the user must know — security, breaking changes, honest limitations.
    Notice,
}

impl ChangeKind {
    fn glyph(self) -> &'static str {
        match self {
            ChangeKind::Added => "✦",
            ChangeKind::Fixed => "✔",
            ChangeKind::Notice => "⚠",
        }
    }
    fn color(self) -> Color {
        match self {
            ChangeKind::Added => Color::Rgb(120, 230, 200),
            ChangeKind::Fixed => Color::Rgb(120, 190, 255),
            ChangeKind::Notice => Color::Rgb(255, 190, 90),
        }
    }
    fn label(self) -> &'static str {
        match self {
            ChangeKind::Added => "ADDED",
            ChangeKind::Fixed => "FIXED",
            ChangeKind::Notice => "NOTICE",
        }
    }
}

/// What this release is. Edit this — and only this — each release.
pub struct ReleaseNotes {
    pub version: &'static str,
    pub codename: &'static str,
    pub tagline: &'static str,
    pub changes: &'static [Change],
}

/// ─────────────────────────────────────────────────────────────────────────────────────
/// THE CURRENT RELEASE. This is the one block a release bump edits.
/// ─────────────────────────────────────────────────────────────────────────────────────
pub const NOTES: ReleaseNotes = ReleaseNotes {
    version: env!("CARGO_PKG_VERSION"),
    codename: "VEIL",
    tagline: "the proof stops telling on you",
    changes: &[
        Change {
            kind: ChangeKind::Fixed,
            text: "Shielded spends no longer publish their own witness. The proof was sound \
                   but not hiding: secrets sat in trace columns held constant down every row, \
                   and a constant column's low-degree extension is that constant everywhere — \
                   so all 84 query openings printed it. Measured on a real proof: recipient \
                   key and both amounts, 85x each, verbatim.",
        },
        Change {
            kind: ChangeKind::Added,
            text: "Reserved-random-row masking (spend_full_v5): the trace's second half is \
                   uniform randomness released from the constraint system, so the openings \
                   are simulatable. Leak measured at 0; two proofs of one spend now differ \
                   in 99.6% of their bytes.",
        },
        Change {
            kind: ChangeKind::Added,
            text: "HidingProver — a prover that must NAME its secrets and refuses to return \
                   a proof containing any of them. This bug cannot ship again silently.",
        },
        Change {
            kind: ChangeKind::Notice,
            text: "Consensus accepts BOTH proof versions during rollout, so wallets still on \
                   v7.3.2 keep working — but a v4 proof still leaks. Update every wallet you \
                   own. v4 acceptance is removed at a later height gate.",
        },
        Change {
            kind: ChangeKind::Notice,
            text: "Masking makes the trace openings simulatable — verified three ways \
                   (0 deterministic channels, 5,493 functionals vs 8,448 random values, and \
                   a 3,000-proof recovery game at chance). A simulator proof is NOT claimed.",
        },
    ],
};

// ── the animation ──────────────────────────────────────────────────────────────────────

/// An act and the second at which it begins. The last act runs until the end.
struct Act {
    at: f32,
    name: &'static str,
    /// Acts that stay at full intensity once they have played. The wordmark is the payoff
    /// of the whole sequence; letting it decay like the transitional acts left the final
    /// frame with a version number floating in an empty box.
    persist: bool,
}

/// Append here to add an act to a future release ceremony.
const SEQUENCE: &[Act] = &[
    Act { at: 0.0, name: "VOID",  persist: false },
    Act { at: 1.1, name: "BRAID", persist: false },
    Act { at: 3.2, name: "LANES", persist: false },
    Act { at: 5.2, name: "SEAL",  persist: false },
    Act { at: 7.0, name: "MARK",  persist: true  },
    Act { at: 9.6, name: "STAMP", persist: true  },
];

/// Total run time before the animation settles into its final frame.
const RUNTIME: f32 = 11.4;

/// Drives the ceremony. Cheap to hold; does nothing until `start()`.
pub struct ReleaseCeremony {
    started: Option<Instant>,
    /// Set when the user presses a key to skip the animation. The changelog stays.
    skipped: bool,
}

impl Default for ReleaseCeremony {
    fn default() -> Self {
        Self { started: None, skipped: false }
    }
}

impl ReleaseCeremony {
    pub fn new() -> Self {
        Self::default()
    }

    /// Begin the ceremony. Call once, after a successful self-update.
    pub fn start(&mut self) {
        self.started = Some(Instant::now());
        self.skipped = false;
    }

    pub fn is_running(&self) -> bool {
        self.started.is_some()
    }

    /// Any keypress skips the animation to its final frame; a second one dismisses.
    pub fn on_key(&mut self) {
        if self.skipped {
            self.started = None;
        } else {
            self.skipped = true;
        }
    }

    pub fn dismiss(&mut self) {
        self.started = None;
    }

    /// Seconds since start, clamped to the end when skipped.
    fn elapsed(&self) -> f32 {
        if self.skipped {
            return RUNTIME;
        }
        self.started
            .map(|s| s.elapsed().as_secs_f32())
            .unwrap_or(0.0)
            .min(RUNTIME)
    }

    /// The frame budget the caller should use while this is running, so the animation is
    /// smooth without spinning the CPU when it is not.
    pub fn tick(&self) -> Duration {
        if self.is_running() && !self.skipped {
            Duration::from_millis(33) // ~30 fps
        } else {
            Duration::from_millis(250)
        }
    }

    pub fn render(&self, f: &mut Frame, area: Rect) {
        if !self.is_running() || area.height < 12 || area.width < 40 {
            return;
        }
        let t = self.elapsed();

        // Full-screen dim backdrop so the ceremony reads as a takeover, not an overlay.
        let backdrop = Block::default().style(Style::default().bg(Color::Rgb(6, 8, 12)));
        f.render_widget(backdrop, area);

        // Stage on top, changelog below. The stage shrinks on short terminals so the
        // changelog — the part with actual information — is never the thing that is lost.
        let stage_h = (area.height as f32 * 0.52) as u16;
        let stage_h = stage_h.clamp(9, 20).min(area.height.saturating_sub(8));
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(stage_h), Constraint::Min(6)])
            .split(area);

        self.draw_stage(f, chunks[0], t);
        draw_changelog(f, chunks[1], t);
    }

    fn draw_stage(&self, f: &mut Frame, area: Rect, t: f32) {
        // Which act is current, and how far through it are we?
        let mut idx = 0usize;
        for (i, a) in SEQUENCE.iter().enumerate() {
            if t >= a.at {
                idx = i;
            }
        }
        let start = SEQUENCE[idx].at;
        let end = SEQUENCE.get(idx + 1).map(|a| a.at).unwrap_or(RUNTIME);
        let local = if end > start { ((t - start) / (end - start)).clamp(0.0, 1.0) } else { 1.0 };

        let buf = f.buffer_mut();

        // Earlier acts keep painting underneath at reduced intensity, so the picture
        // accumulates instead of cutting. This is what makes it feel composed.
        for (i, act) in SEQUENCE.iter().enumerate().take(idx + 1) {
            let age = t - act.at;
            // Transitional acts decay FAST. An earlier version let them linger for 6 s at
            // up to 0.85 intensity and by the MARK act the stage was unreadable static —
            // six things drawn at once is not composition, it is noise. Persistent acts
            // (the wordmark) stay lit.
            let fade = if i == idx {
                1.0
            } else if act.persist {
                1.0
            } else {
                (1.0 - age / 1.6).clamp(0.0, 0.45)
            };
            if fade <= 0.0 { continue; }
            let l = if i == idx { local } else { 1.0 }; // finished acts render settled
            draw_act(buf, area, act.name, l, t, fade);
        }

        // Act label, bottom-left of the stage — tiny, uppercase, tracked.
        if area.height > 4 {
            let label = format!(" {} ", SEQUENCE[idx].name);
            let y = area.y + area.height - 1;
            let style = Style::default()
                .fg(Color::Rgb(70, 90, 110))
                .add_modifier(Modifier::ITALIC);
            put_str(buf, area, area.x + 1, y, &label, style);
        }
    }
}

// ── act renderers ──────────────────────────────────────────────────────────────────────

/// Deterministic value noise — no rng dependency, stable across frames for a given cell.
fn hash01(x: i32, y: i32, s: u32) -> f32 {
    let mut h = (x as u32)
        .wrapping_mul(0x9E37_79B9)
        ^ (y as u32).wrapping_mul(0x85EB_CA6B)
        ^ s.wrapping_mul(0xC2B2_AE35);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2545_F491);
    h ^= h >> 13;
    (h % 100_000) as f32 / 100_000.0
}

fn put(buf: &mut ratatui::buffer::Buffer, area: Rect, x: u16, y: u16, ch: char, style: Style) {
    if x >= area.x && y >= area.y && x < area.x + area.width && y < area.y + area.height {
        buf[(x, y)].set_char(ch).set_style(style);
    }
}

fn put_str(buf: &mut ratatui::buffer::Buffer, area: Rect, x: u16, y: u16, s: &str, style: Style) {
    for (i, ch) in s.chars().enumerate() {
        put(buf, area, x + i as u16, y, ch, style);
    }
}

fn lerp_rgb(a: (u8, u8, u8), b: (u8, u8, u8), t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    Color::Rgb(
        (a.0 as f32 + (b.0 as f32 - a.0 as f32) * t) as u8,
        (a.1 as f32 + (b.1 as f32 - a.1 as f32) * t) as u8,
        (a.2 as f32 + (b.2 as f32 - a.2 as f32) * t) as u8,
    )
}

fn dim(c: (u8, u8, u8), k: f32) -> Color {
    Color::Rgb((c.0 as f32 * k) as u8, (c.1 as f32 * k) as u8, (c.2 as f32 * k) as u8)
}

fn draw_act(
    buf: &mut ratatui::buffer::Buffer,
    area: Rect,
    name: &str,
    local: f32,
    t: f32,
    fade: f32,
) {
    match name {
        "VOID" => act_void(buf, area, local, t, fade),
        "BRAID" => act_braid(buf, area, local, t, fade),
        "LANES" => act_lanes(buf, area, local, t, fade),
        "SEAL" => act_seal(buf, area, local, t, fade),
        "MARK" => act_mark(buf, area, local, t, fade),
        "STAMP" => act_stamp(buf, area, local, t, fade),
        _ => {}
    }
}

/// VOID — entropy before there is any order to it.
fn act_void(buf: &mut ratatui::buffer::Buffer, area: Rect, local: f32, t: f32, fade: f32) {
    let glyphs = ['·', '˙', '⋅', '∙'];
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            let n = hash01(x as i32, y as i32, 1);
            if n > 0.972 {
                // slow twinkle, each cell on its own phase
                let ph = hash01(x as i32, y as i32, 7) * 6.283;
                let tw = ((t * 1.7 + ph).sin() * 0.5 + 0.5) * local * fade;
                let g = glyphs[((n * 1000.0) as usize) % glyphs.len()];
                put(buf, area, x, y, g, Style::default().fg(dim((90, 120, 160), 0.25 + tw * 0.6)));
            }
        }
    }
}

/// BRAID — strands weave across the field and merge: a DAG being put into one order.
fn act_braid(buf: &mut ratatui::buffer::Buffer, area: Rect, local: f32, t: f32, fade: f32) {
    let mid = area.y as f32 + area.height as f32 / 2.0;
    // 5 strands with distinct amplitudes and phases. 7 identical-amplitude strands
    // collapsed onto the same rows and drew as solid horizontal bars.
    let strands = 5usize;
    let reach = (area.width as f32 * local).ceil() as u16;
    for s in 0..strands {
        let phase = s as f32 * 1.37; // irrational-ish spacing so strands never sync up
        let amp = (area.height as f32 / 2.2) * (0.35 + 0.65 * (s as f32 / (strands - 1).max(1) as f32));
        for dx in 0..reach {
            let x = area.x + dx;
            let fx = dx as f32 / area.width.max(1) as f32;
            // strands converge toward the centre as they travel right — the braid resolving
            let converge = 1.0 - fx * 0.85;
            let y = mid + (fx * 7.0 + phase + t * 1.1).sin() * amp * converge;
            let yy = y.round();
            if yy < area.y as f32 || yy >= (area.y + area.height) as f32 {
                continue;
            }
            let head = (dx + 1) as f32 / reach.max(1) as f32;
            let bright = (0.25 + head * 0.75) * fade;
            let col = lerp_rgb((90, 70, 190), (60, 210, 230), fx);
            let ch = if head > 0.97 { '◆' } else if converge < 0.35 { '━' } else { '─' };
            put(buf, area, x, yy as u16, ch, Style::default().fg(dim(rgb_of(col), bright)));
        }
    }
}

fn rgb_of(c: Color) -> (u8, u8, u8) {
    match c {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => (200, 200, 200),
    }
}

/// LANES — the two proof lanes march inward and collide: hash power × verifiable delay.
fn act_lanes(buf: &mut ratatui::buffer::Buffer, area: Rect, local: f32, t: f32, fade: f32) {
    let cx = area.x + area.width / 2;
    let mid = area.y + area.height / 2;
    let travel = (area.width as f32 / 2.0 * local) as u16;

    // hash lane, from the left, warm
    for d in 0..travel {
        let x = area.x + d;
        let y = mid.saturating_sub(1);
        let lead = d + 1 == travel;
        let ch = if lead { '▶' } else if d % 3 == 0 { '▰' } else { '▱' };
        let b = (0.35 + 0.65 * (d as f32 / travel.max(1) as f32)) * fade;
        put(buf, area, x, y, ch, Style::default().fg(dim((255, 150, 60), b)));
    }
    // delay lane, from the right, cool — deliberately slower: a VDF cannot be parallelised
    let travel2 = (area.width as f32 / 2.0 * local.powf(1.45)) as u16;
    for d in 0..travel2 {
        let x = area.x + area.width - 1 - d;
        let y = mid + 1;
        let lead = d + 1 == travel2;
        let ch = if lead { '◀' } else if d % 4 == 0 { '◧' } else { '◫' };
        let b = (0.35 + 0.65 * (d as f32 / travel2.max(1) as f32)) * fade;
        put(buf, area, x, y, ch, Style::default().fg(dim((70, 210, 240), b)));
    }
    // convergence flash
    if local > 0.82 {
        let k = ((local - 0.82) / 0.18).clamp(0.0, 1.0);
        let r = (k * 6.0) as i32;
        for dy in -r..=r {
            for dx in -(r * 2)..=(r * 2) {
                if (dx * dx) / 4 + dy * dy > r * r {
                    continue;
                }
                let x = cx as i32 + dx;
                let y = mid as i32 + dy;
                if x < 0 || y < 0 {
                    continue;
                }
                let f = (1.0 - k) * fade;
                put(buf, area, x as u16, y as u16, '·',
                    Style::default().fg(dim((255, 240, 200), f)));
            }
        }
        put_str(buf, area, cx.saturating_sub(3), mid, "◈◈◈",
            Style::default().fg(dim((255, 255, 220), fade)).add_modifier(Modifier::BOLD));
    }
    let _ = t;
}

/// SEAL — the veil closes. Plaintext turns to redaction; this is the shielded pool.
fn act_seal(buf: &mut ratatui::buffer::Buffer, area: Rect, local: f32, t: f32, fade: f32) {
    let cx = area.x as f32 + area.width as f32 / 2.0;
    let cy = area.y as f32 + area.height as f32 / 2.0;
    let max_r = (area.width as f32 / 2.0).max(area.height as f32);
    let r = max_r * local;

    // the ward expanding outward
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            // diamond metric — a seal, not a circle
            let d = ((x as f32 - cx).abs() / 2.0) + (y as f32 - cy).abs();
            let edge = (d - r).abs();
            if edge < 0.9 {
                let col = lerp_rgb((150, 110, 255), (80, 240, 210), (local * 1.4).min(1.0));
                put(buf, area, x, y, '◇', Style::default().fg(dim(rgb_of(col), fade)));
            } else if d < r {
                // inside the ward, the ledger is redacted
                let n = hash01(x as i32, y as i32, 3);
                if n > 0.86 {
                    let blocks = ['█', '▓', '▒'];
                    let g = blocks[((n * 977.0) as usize) % blocks.len()];
                    let b = 0.10 + 0.22 * hash01(x as i32, y as i32, 11);
                    put(buf, area, x, y, g, Style::default().fg(dim((110, 90, 180), b * fade)));
                }
            }
        }
    }
    let _ = t;
}

/// A 5-row block font, just enough for the wordmark.
fn glyph(c: char) -> [&'static str; 5] {
    match c {
        'S' => ["█████", "█    ", "█████", "    █", "█████"],
        'I' => ["█████", "  █  ", "  █  ", "  █  ", "█████"],
        'G' => ["█████", "█    ", "█  ██", "█   █", "█████"],
        'L' => ["█    ", "█    ", "█    ", "█    ", "█████"],
        _ => ["     ", "     ", "     ", "     ", "     "],
    }
}

/// MARK — the sigil ignites: glyphs fall into place, then a shimmer sweeps across.
fn act_mark(buf: &mut ratatui::buffer::Buffer, area: Rect, local: f32, t: f32, fade: f32) {
    let word = ['S', 'I', 'G', 'I', 'L'];
    let gw = 5u16;
    let gap = 2u16;
    let total = word.len() as u16 * gw + (word.len() as u16 - 1) * gap;
    if area.width < total + 2 || area.height < 7 {
        // too narrow for the wordmark — fall back to plain text, still centred
        let s = "S I G I L";
        let x = area.x + area.width.saturating_sub(s.len() as u16) / 2;
        let y = area.y + area.height / 2;
        put_str(buf, area, x, y, s,
            Style::default().fg(dim((90, 240, 220), fade)).add_modifier(Modifier::BOLD));
        return;
    }
    let x0 = area.x + (area.width - total) / 2;
    let y0 = area.y + (area.height.saturating_sub(5)) / 2;

    // Clear a field behind the wordmark. Whatever the earlier acts left there is texture,
    // and texture behind block letters reads as damage. The clear widens as the letters
    // land so it feels like the mark is burning the field away rather than sitting on it.
    let pad = 3u16;
    let grow = (local * 1.6).clamp(0.0, 1.0);
    let cw = ((total + pad * 2) as f32 * grow) as u16;
    let cx0 = x0 + total / 2;
    for y in y0.saturating_sub(1)..(y0 + 6).min(area.y + area.height) {
        for dx in 0..cw {
            let x = cx0.saturating_sub(cw / 2) + dx;
            put(buf, area, x, y, ' ', Style::default());
        }
    }

    for (i, c) in word.iter().enumerate() {
        // each letter drops in on its own beat
        let beat = i as f32 * 0.11;
        let p = ((local - beat) / 0.42).clamp(0.0, 1.0);
        if p <= 0.0 {
            continue;
        }
        let rows = glyph(*c);
        let drop = ((1.0 - p) * 6.0) as u16; // falls from above into place
        for (ry, row) in rows.iter().enumerate() {
            for (rx, ch) in row.chars().enumerate() {
                if ch == ' ' {
                    continue;
                }
                let x = x0 + i as u16 * (gw + gap) + rx as u16;
                let y = y0 + ry as u16;
                let y = y.saturating_sub(drop);
                // shimmer: a bright band sweeping left→right once the letters have landed
                let sweep = ((t * 0.9).fract() * (total as f32 + 20.0)) - 10.0;
                let dxs = (x as f32 - x0 as f32 - sweep).abs();
                let hot = (1.0 - dxs / 5.0).clamp(0.0, 1.0) * if local > 0.55 { 1.0 } else { 0.0 };
                let base = lerp_rgb((70, 200, 220), (170, 140, 255), i as f32 / 4.0);
                let col = lerp_rgb(rgb_of(base), (255, 255, 240), hot);
                let b = (0.45 + 0.55 * p) * fade;
                put(buf, area, x, y, ch,
                    Style::default().fg(dim(rgb_of(col), b)).add_modifier(Modifier::BOLD));
            }
        }
    }
}

/// STAMP — version, codename, tagline. The part that is actually information.
fn act_stamp(buf: &mut ratatui::buffer::Buffer, area: Rect, local: f32, _t: f32, fade: f32) {
    let y = area.y + area.height.saturating_sub(3);
    let line = format!("v{}  ·  {}", NOTES.version, NOTES.codename);
    let x = area.x + area.width.saturating_sub(line.len() as u16) / 2;
    let b = local.clamp(0.0, 1.0) * fade;
    put_str(buf, area, x, y, &line,
        Style::default().fg(dim((235, 245, 255), b)).add_modifier(Modifier::BOLD));

    let tl = NOTES.tagline;
    let x2 = area.x + area.width.saturating_sub(tl.len() as u16) / 2;
    put_str(buf, area, x2, y + 1, tl,
        Style::default().fg(dim((120, 150, 180), b)).add_modifier(Modifier::ITALIC));
}

// ── changelog ──────────────────────────────────────────────────────────────────────────

fn draw_changelog(f: &mut Frame, area: Rect, t: f32) {
    // Reveal one entry at a time, so the eye is led down the list instead of hit with it.
    let reveal_at = 1.6f32;
    let per = 0.5f32;

    let title = format!(" what changed in v{} ", NOTES.version);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Rgb(60, 78, 96)))
        .title(Span::styled(
            title,
            Style::default().fg(Color::Rgb(150, 200, 235)).add_modifier(Modifier::BOLD),
        ))
        // On the BOTTOM border: a long changelog pushes trailing lines out of the
        // paragraph, and the one line the user must not miss is how to get rid of this.
        .title_bottom(Span::styled(
            if t >= RUNTIME { " any key to dismiss " } else { " any key to skip " },
            Style::default().fg(Color::Rgb(95, 120, 145)).add_modifier(Modifier::ITALIC),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    for (i, c) in NOTES.changes.iter().enumerate() {
        if t < reveal_at + i as f32 * per {
            break;
        }
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {} ", c.kind.glyph()),
                Style::default().fg(c.kind.color()).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:<7}", c.kind.label()),
                Style::default().fg(c.kind.color()).add_modifier(Modifier::BOLD),
            ),
            Span::styled(c.text, Style::default().fg(Color::Rgb(198, 210, 222))),
        ]));
        lines.push(Line::from(""));
    }
    if lines.is_empty() {
        lines.push(Line::from(""));
    }

    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: true }).alignment(Alignment::Left),
        inner,
    );
}

/// Render one frame at an explicit time. Exists so the ceremony can be inspected and
/// regression-tested without a real terminal or a real clock — an animation nobody has
/// looked at is just code that compiles.
pub fn render_frame_at(f: &mut Frame, area: Rect, t: f32) {
    let backdrop = Block::default().style(Style::default().bg(Color::Rgb(6, 8, 12)));
    f.render_widget(backdrop, area);
    let stage_h = (area.height as f32 * 0.52) as u16;
    let stage_h = stage_h.clamp(9, 20).min(area.height.saturating_sub(8));
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(stage_h), Constraint::Min(6)])
        .split(area);
    let c = ReleaseCeremony { started: None, skipped: false };
    let _ = &c;
    // reuse the same act pipeline the live path uses
    let mut idx = 0usize;
    for (i, a) in SEQUENCE.iter().enumerate() {
        if t >= a.at { idx = i; }
    }
    let start = SEQUENCE[idx].at;
    let end = SEQUENCE.get(idx + 1).map(|a| a.at).unwrap_or(RUNTIME);
    let local = if end > start { ((t - start) / (end - start)).clamp(0.0, 1.0) } else { 1.0 };
    {
        let buf = f.buffer_mut();
        for (i, act) in SEQUENCE.iter().enumerate().take(idx + 1) {
            let age = t - act.at;
            // Transitional acts decay FAST. An earlier version let them linger for 6 s at
            // up to 0.85 intensity and by the MARK act the stage was unreadable static —
            // six things drawn at once is not composition, it is noise. Persistent acts
            // (the wordmark) stay lit.
            let fade = if i == idx {
                1.0
            } else if act.persist {
                1.0
            } else {
                (1.0 - age / 1.6).clamp(0.0, 0.45)
            };
            if fade <= 0.0 { continue; }
            let l = if i == idx { local } else { 1.0 }; // finished acts render settled
            draw_act(buf, chunks[0], act.name, l, t, fade);
        }
    }
    draw_changelog(f, chunks[1], t);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sequence must be monotonic and fit inside the runtime, or acts silently never
    /// play — the kind of bug that is invisible until a release day.
    #[test]
    fn sequence_is_ordered_and_within_runtime() {
        let mut last = -1.0f32;
        for a in SEQUENCE {
            assert!(a.at > last, "act {} is out of order", a.name);
            assert!(a.at < RUNTIME, "act {} starts after the runtime ends", a.name);
            last = a.at;
        }
    }

    /// Every act named in the sequence must have a renderer, or it draws nothing.
    #[test]
    fn every_act_has_a_renderer() {
        let known = ["VOID", "BRAID", "LANES", "SEAL", "MARK", "STAMP"];
        for a in SEQUENCE {
            assert!(known.contains(&a.name), "act {} has no renderer arm", a.name);
        }
    }

    #[test]
    fn release_notes_are_populated() {
        assert!(!NOTES.changes.is_empty(), "a release with no changelog is a bug");
        assert!(!NOTES.tagline.is_empty());
        for c in NOTES.changes {
            assert!(c.text.len() > 20, "changelog entries should say something");
        }
    }

    /// Skipping must jump to the settled frame, and a second key dismisses.
    #[test]
    fn skip_then_dismiss() {
        let mut c = ReleaseCeremony::new();
        c.start();
        assert!(c.is_running());
        c.on_key();
        assert!(c.is_running(), "first key skips the animation, it does not close it");
        assert_eq!(c.elapsed(), RUNTIME);
        c.on_key();
        assert!(!c.is_running(), "second key dismisses");
    }

    /// Print the ceremony at several instants so a human can actually see it:
    ///   cargo test -p sigil-top ceremony_preview -- --nocapture
    #[test]
    fn ceremony_preview() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        for t in [0.6f32, 2.2, 4.3, 6.1, 8.4, 11.0] {
            let mut term = Terminal::new(TestBackend::new(96, 26)).unwrap();
            term.draw(|f| render_frame_at(f, f.area(), t)).unwrap();
            let buf = term.backend().buffer().clone();
            let act = SEQUENCE.iter().rev().find(|a| t >= a.at).map(|a| a.name).unwrap_or("?");
            println!("\n╔══ t = {t:>5.1}s   act = {act} {}", "═".repeat(60));
            for y in 0..buf.area.height {
                let mut line = String::new();
                for x in 0..buf.area.width {
                    line.push_str(buf[(x, y)].symbol());
                }
                println!("║{}", line.trim_end());
            }
        }
    }

    #[test]
    fn noise_is_stable_per_cell_and_varies_across_cells() {
        assert_eq!(hash01(3, 4, 1), hash01(3, 4, 1));
        assert_ne!(hash01(3, 4, 1), hash01(4, 3, 1));
        for s in 0..50 {
            let v = hash01(s, s * 7, 2);
            assert!((0.0..1.0).contains(&v));
        }
    }
}
