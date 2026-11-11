//! ◈ NETWORK MAP — tab 6 (2026-11-11, Viktor: "work on sigil graph network map getting it on webpage ui
//! and tui inside sigil graph node ui tui").
//!
//! The terminal twin of the 3D map in the wallet page's ◈ NETWORK STATS window (`gui/sigil-netmap.js`).
//! Same data, same rules, so the two can never tell different stories:
//!
//! * source — `/v1/network/topology` of YOUR node when one runs here, else of the network's node. The header
//!   says which one answered. `peer_views[id].tip_matches_local` + `state_root_matches_local` are that node's
//!   tip hash and state root compared with the ANSWERING node's own block at the same height;
//! * same state — root equal and tip not unequal; different — either unequal, but only after
//!   [`DIFFER_STREAK`] refreshes in a row (one mismatching sample is two reads taken moments apart);
//!   syncing — more than [`SYNC_LAG`] blocks behind; otherwise "no comparable view";
//! * block-makers — `producer` ids in `/v1/dagknight/recent` that made at least 1 in [`MAKER_SHARE`] of the
//!   recent blocks. Not every id is a block-maker: a block WON by a mining rig carries the rig's wallet in that
//!   field (`sigil-node/src/mint.rs`), about one block in a thousand on g3 — counting ids alone read
//!   "2 block-makers" on a one-maker chain (measured 2026-11-11). Those blocks are reported separately;
//! * finality — `/v1/finality/certificate`;
//! * machines — distinct peer IP addresses, COUNTED and never shown (they are people's addresses).
//!
//! Positions on the map are layout only. Every colour, mark and number is one of the readings above.
//! "Decentralized" needs ≥ 2 block-makers AND a finality committee ≥ 4 with BFT AND ≥ 4 machines — the
//! same sentence the page prints.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Context, Line as CLine, Points};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;
use serde_json::Value;

use crate::console_glyphs::ui_ascii;
use crate::heroes::card_block;
use crate::tabs_ui::{dim, group};
use crate::{C_DIM, C_GOLD, C_GREEN, C_NEON_CYAN, C_RED};

const PERIOD: Duration = Duration::from_secs(10);
/// A mismatch is "different" only when seen this many refreshes in a row.
pub(crate) const DIFFER_STREAK: u32 = 3;
/// A node more than this many blocks behind the answering node is syncing, not comparable.
pub(crate) const SYNC_LAG: u64 = 512;
/// A peer view older than this is marked stale.
const STALE_SECS: u64 = 120;
/// A producer id is a block-maker when it made at least one in this many of the recent blocks.
pub(crate) const MAKER_SHARE: usize = 10;
/// BFT finality needs four independent signers.
const BFT_SIGNERS: u64 = 4;

const C_BLUE: Color = Color::Rgb(124, 156, 255);
const C_PURPLE: Color = Color::Rgb(192, 132, 252);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PeerState { Agree, Checking, Differ, Syncing, Unknown }

#[derive(Clone, Debug)]
pub(crate) struct PeerNode {
    pub id: String,
    pub label: String,
    /// Connected over loopback = runs on the answering node's machine.
    pub local: bool,
    pub state: PeerState,
    pub height: Option<u64>,
    /// Share of the chain a syncing node holds (0..1).
    pub frac: f64,
    pub age_s: Option<u64>,
    pub stale: bool,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct NetMap {
    pub self_label: String,
    pub self_height: u64,
    pub is_maker: bool,
    pub peers: Vec<PeerNode>,
    /// Block-makers among the recent blocks (see [`MAKER_SHARE`]), and how many blocks were read (0 = not read).
    pub makers: usize,
    pub blocks: usize,
    /// Recent blocks whose producer field is some other id: won by a mining rig, which is not a block-maker.
    pub rig_blocks: usize,
    pub committee: Option<u64>,
    pub gate: String,
    pub bft: Option<bool>,
    /// This machine + distinct remote addresses.
    pub machines: usize,
    pub on_box: usize,
}

#[derive(Default)]
pub(crate) struct MapState {
    pub map: Option<NetMap>,
    /// "your node" or "network node".
    pub source: &'static str,
    pub fetched: Option<Instant>,
    pub err: Option<String>,
}

/// Short display name for a node's self-reported name; never shows an address.
pub(crate) fn node_label(name: &str, id: &str) -> String {
    if let Some(i) = name.find("epsilon") {
        let rest = &name[i + 7..];
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() && rest[digits.len()..].starts_with("-docker") {
            let n = digits.trim_start_matches('0');
            return format!("node-{}", if n.is_empty() { "0" } else { n });
        }
    }
    if let Some(r) = name.strip_prefix("sigil-g") {
        let d = r.chars().take_while(|c| c.is_ascii_digit()).count();
        if d > 0 && &r[d..] == "-epsilon" { return "Epsilon".into(); }
    }
    if name.contains("cs30067.seedhost.eu") { return "Epsilon sigil-top".into(); }
    if let Some(r) = name.strip_prefix("sigil-top-") {
        let r = r.strip_prefix("top-").unwrap_or(r);
        let hex: String = r.chars().take(6).collect();
        if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) { return format!("sigil-top {hex}"); }
    }
    if !name.is_empty() {
        return name.strip_prefix("sigil-").unwrap_or(name).chars().take(22).collect();
    }
    let tail: String = id.chars().rev().take(6).collect::<Vec<_>>().into_iter().rev().collect();
    format!("peer ..{tail}")
}

fn is_maker_name(name: &str) -> bool {
    name.strip_prefix("sigil-g").map(|r| {
        let d = r.chars().take_while(|c| c.is_ascii_digit()).count();
        d > 0 && &r[d..] == "-epsilon"
    }).unwrap_or(false)
}

fn data(v: &Value) -> &Value { v.get("data").unwrap_or(v) }

/// Build the model from the three route bodies. `streak` carries the per-peer mismatch count between refreshes.
pub(crate) fn build(topology: &Value, cert: Option<&Value>, recent: Option<&Value>, streak: &mut BTreeMap<String, u32>) -> NetMap {
    let t = data(topology);
    let lv = t.get("local_view").cloned().unwrap_or(Value::Null);
    let my_h = lv.get("height").and_then(Value::as_u64).unwrap_or(0);
    let now_ms = lv.get("ts_ms").and_then(Value::as_u64).unwrap_or(0);
    let views = t.get("peer_views").and_then(Value::as_object);
    let mut remote: BTreeSet<String> = BTreeSet::new();
    let mut peers: Vec<PeerNode> = Vec::new();
    let mut list: Vec<&Value> = t.get("peers").and_then(Value::as_array).map(|a| a.iter().collect()).unwrap_or_default();
    list.sort_by_key(|p| p.get("peer_id").and_then(Value::as_str).unwrap_or("").to_string());
    for p in list {
        let id = p.get("peer_id").and_then(Value::as_str).unwrap_or("").to_string();
        let addr = p.get("multiaddr").and_then(Value::as_str).unwrap_or("");
        let mut parts = addr.split('/').skip(1);
        let ip = match (parts.next(), parts.next()) {
            (Some("ip4"), Some(ip)) | (Some("ip6"), Some(ip)) => ip.to_string(),
            _ => "?".into(),
        };
        let local = ip.starts_with("127.") || ip == "::1";
        if !local { remote.insert(ip); }
        let vw = views.and_then(|m| m.get(&id));
        let height = vw.and_then(|v| v.get("height")).and_then(Value::as_u64);
        let (mut state, mut frac) = (PeerState::Unknown, 0.0);
        if let Some(v) = vw {
            let root = v.get("state_root_matches_local").and_then(Value::as_bool);
            let tip = v.get("tip_matches_local").and_then(Value::as_bool);
            if root == Some(true) && tip != Some(false) { state = PeerState::Agree; }
            else if root == Some(false) || tip == Some(false) { state = PeerState::Differ; }
            else if let Some(h) = height {
                if my_h > 0 && h + SYNC_LAG < my_h { state = PeerState::Syncing; frac = h as f64 / my_h as f64; }
            }
        }
        if state == PeerState::Differ {
            let n = streak.entry(id.clone()).or_insert(0);
            *n += 1;
            if *n < DIFFER_STREAK { state = PeerState::Checking; }
        } else {
            streak.insert(id.clone(), 0);
        }
        let age_s = vw.and_then(|v| v.get("seen_ms")).and_then(Value::as_u64)
            .filter(|_| now_ms > 0).map(|s| now_ms.saturating_sub(s) / 1000);
        let name = vw.and_then(|v| v.get("node")).and_then(Value::as_str).unwrap_or("");
        peers.push(PeerNode { label: node_label(name, &id), id, local, state, height, frac, age_s, stale: age_s.map(|a| a > STALE_SECS).unwrap_or(false) });
    }
    let (mut makers, mut blocks, mut rig_blocks) = (0, 0, 0);
    if let Some(r) = recent {
        let d = data(r);
        if let Some(items) = d.as_array().or_else(|| d.get("blocks").and_then(Value::as_array)) {
            let mut by_id: BTreeMap<String, usize> = BTreeMap::new();
            for p in items.iter().filter_map(|b| b.get("producer")) { *by_id.entry(p.to_string()).or_default() += 1; }
            blocks = items.len();
            for n in by_id.values() {
                if n * MAKER_SHARE >= blocks { makers += 1; } else { rig_blocks += n; }
            }
        }
    }
    let c = cert.map(data).map(|d| d.get("certificate").unwrap_or(d));
    let self_name = lv.get("node").and_then(Value::as_str)
        .or_else(|| t.get("self_node_id").and_then(Value::as_str)).unwrap_or("");
    let on_box = peers.iter().filter(|p| p.local).count();
    NetMap {
        self_label: node_label(self_name, ""),
        self_height: my_h,
        // The route does not say which node made a block; the network's block-maker is the node that names
        // itself `sigil-gN-epsilon`, and it is marked only while exactly one producer id made every recent block.
        is_maker: makers == 1 && is_maker_name(self_name),
        makers, blocks, rig_blocks,
        committee: c.and_then(|c| c.get("committee_size")).and_then(Value::as_u64),
        gate: c.and_then(|c| c.get("gate")).and_then(Value::as_str).unwrap_or("?").to_string(),
        bft: c.and_then(|c| c.get("bft")).and_then(Value::as_bool),
        machines: 1 + remote.len(),
        on_box,
        peers,
    }
}

impl NetMap {
    pub(crate) fn count(&self, s: PeerState) -> usize { self.peers.iter().filter(|p| p.state == s).count() }
    pub(crate) fn comparable(&self) -> usize { self.count(PeerState::Agree) + self.count(PeerState::Differ) + self.count(PeerState::Checking) }
    pub(crate) fn decentralized(&self) -> bool {
        self.makers >= 2 && self.committee.unwrap_or(0) >= BFT_SIGNERS && self.bft == Some(true) && self.machines >= 4
    }
    /// Independent finality signers still missing for BFT.
    pub(crate) fn missing_signers(&self) -> u64 { BFT_SIGNERS.saturating_sub(self.committee.unwrap_or(1).max(1)) }

    pub(crate) fn decentral_line(&self) -> String {
        if self.decentralized() {
            return format!("Decentralized: {} block-makers, a finality committee of {} with BFT, on {} machines.",
                self.makers, self.committee.unwrap_or(0), self.machines);
        }
        let makers = match (self.blocks, self.makers) {
            (0, _) => "block-makers not read".to_string(),
            (n, 1) if self.rig_blocks == 0 => format!("1 block-maker made all {n} recent blocks"),
            (n, 1) => format!("1 block-maker made {} of the {n} recent blocks ({} won by mining rigs)", n - self.rig_blocks, self.rig_blocks),
            (_, m) => format!("{m} block-makers"),
        };
        let fin = match self.committee {
            Some(1) => format!("1 key signs finality ({} gate, BFT {})", self.gate, if self.bft == Some(true) { "on" } else { "off" }),
            Some(n) => format!("{n} keys sign finality ({} gate, BFT {})", self.gate, if self.bft == Some(true) { "on" } else { "off" }),
            None => "finality not read".to_string(),
        };
        format!("Not decentralized yet: {makers}, {fin}, and {} of the {} connected nodes run on the answering node's machine.",
            self.on_box, self.peers.len())
    }

    pub(crate) fn integrity_line(&self) -> String {
        let cmp = self.comparable();
        if cmp == 0 { return "Data integrity: no connected node has reported a comparable state yet.".into(); }
        let mut s = format!("Data integrity: {} of {} comparable node{} hold exactly the answering node's state at their own height",
            self.count(PeerState::Agree), cmp, if cmp == 1 { "" } else { "s" });
        let sync = self.count(PeerState::Syncing);
        if sync > 0 { s.push_str(&format!(" · {sync} still syncing")); }
        let none = self.count(PeerState::Unknown);
        if none > 0 { s.push_str(&format!(" · {none} not comparable yet")); }
        s.push('.');
        s
    }
}

fn cell() -> &'static Mutex<MapState> {
    static CELL: OnceLock<Mutex<MapState>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(MapState::default()))
}

/// The newest model (cloned), with where it came from and when.
pub(crate) fn snapshot() -> (Option<NetMap>, &'static str, Option<Instant>, Option<String>) {
    let g = cell().lock().unwrap_or_else(|e| e.into_inner());
    (g.map.clone(), g.source, g.fetched, g.err.clone())
}

fn get_json(base: &str, path: &str) -> Option<Value> {
    let body = crate::HTTP.get(format!("{base}{path}")).timeout(Duration::from_secs(8)).send().ok()?.text().ok()?;
    serde_json::from_str(&body).ok()
}

/// Idempotent: the first call starts the sampler thread.
pub(crate) fn spawn(network_api_base: String) {
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() { return; }
    let _ = std::thread::Builder::new().name("netmap".into()).spawn(move || {
        // one streak table per source: a mismatch against your node says nothing about the network node
        let mut streaks: BTreeMap<&'static str, BTreeMap<String, u32>> = BTreeMap::new();
        loop {
            #[cfg(feature = "producer")]
            let own = match crate::own_node::api() { a if a.is_empty() => None, a => Some(a) };
            #[cfg(not(feature = "producer"))]
            let own: Option<String> = None;
            let mut got: Option<(&'static str, String, Value)> = None;
            if let Some(a) = own {
                if let Some(t) = get_json(&a, "/v1/network/topology") { got = Some(("your node", a, t)); }
            }
            if got.is_none() {
                if let Some(t) = get_json(&network_api_base, "/v1/network/topology") { got = Some(("network node", network_api_base.clone(), t)); }
            }
            match got {
                Some((source, base, t)) => {
                    let cert = get_json(&base, "/v1/finality/certificate");
                    let recent = get_json(&base, "/v1/dagknight/recent");
                    let m = build(&t, cert.as_ref(), recent.as_ref(), streaks.entry(source).or_default());
                    let mut g = cell().lock().unwrap_or_else(|e| e.into_inner());
                    g.map = Some(m); g.source = source; g.fetched = Some(Instant::now()); g.err = None;
                }
                None => {
                    let mut g = cell().lock().unwrap_or_else(|e| e.into_inner());
                    g.err = Some("/v1/network/topology did not answer".into());
                }
            }
            std::thread::sleep(PERIOD);
        }
    });
}

fn state_color(s: PeerState) -> Color {
    match s { PeerState::Agree => C_GREEN, PeerState::Checking => C_GOLD, PeerState::Differ => C_RED, PeerState::Syncing => C_BLUE, PeerState::Unknown => C_DIM }
}

fn state_mark(s: PeerState) -> &'static str {
    if ui_ascii() {
        match s { PeerState::Agree => "ok", PeerState::Checking => "..", PeerState::Differ => "XX", PeerState::Syncing => "~", PeerState::Unknown => "?" }
    } else {
        match s { PeerState::Agree => "✓", PeerState::Checking => "…", PeerState::Differ => "✕", PeerState::Syncing => "◔", PeerState::Unknown => "?" }
    }
}

fn state_text(s: PeerState) -> &'static str {
    match s {
        PeerState::Agree => "same state",
        PeerState::Checking => "mismatch seen, re-checking",
        PeerState::Differ => "DIFFERENT state",
        PeerState::Syncing => "syncing",
        PeerState::Unknown => "no comparable view yet",
    }
}

fn hash01(s: &str) -> f64 {
    let mut h: u32 = 2166136261;
    for b in s.bytes() { h ^= b as u32; h = h.wrapping_mul(16777619); }
    h as f64 / 4294967296.0
}

/// Where each peer sits: nodes on the answering node's machine on the inner ring, others on the outer ring.
/// Returns (x, y) in dot units for each peer, in `peers` order.
pub(crate) fn layout(peers: &[PeerNode], r_in: f64, r_out: f64) -> Vec<(f64, f64)> {
    let n_in = peers.iter().filter(|p| p.local).count().max(1) as f64;
    let n_out = peers.iter().filter(|p| !p.local).count().max(1) as f64;
    let (mut i_in, mut i_out) = (0.0, 0.0);
    peers.iter().map(|p| {
        let (r, a) = if p.local {
            let a = std::f64::consts::TAU * (i_in / n_in) + 0.5; i_in += 1.0; (r_in, a)
        } else {
            // the outer ring runs from the left, under the machine, to the right; the top is kept for the missing signers
            let a = std::f64::consts::PI * (0.89 + 1.22 * ((i_out + 0.5) / n_out)); i_out += 1.0; (r_out, a)
        };
        (r * a.cos(), r * a.sin())
    }).collect()
}

fn ring(ctx: &mut Context, r: f64, sx: f64, color: Color, dashed: bool) {
    let n = 96;
    let pts: Vec<(f64, f64)> = (0..n).filter(|i| !dashed || i % 3 != 2)
        .map(|i| { let a = std::f64::consts::TAU * i as f64 / n as f64; (r * sx * a.cos(), r * a.sin()) }).collect();
    ctx.draw(&Points { coords: &pts, color });
}

fn label_at(ctx: &mut Context, x: f64, y: f64, text: String, color: Color, xmax: f64) {
    let w = text.chars().count() as f64 * 2.0; // one character = two dots wide
    let lx = if x >= 0.0 { (x + 3.0).min(xmax - w) } else { (x - 3.0 - w).max(-xmax) };
    ctx.print(lx, y, Line::from(Span::styled(text, Style::default().fg(color))));
}

fn draw_canvas(f: &mut Frame, m: &NetMap, area: Rect) {
    let block = card_block(if ui_ascii() { "NETWORK MAP" } else { "◈ NETWORK MAP" }, C_NEON_CYAN);
    let inner = block.inner(area);
    // dot units: a braille cell is 2 dots wide and 4 high, and a cell is about twice as tall as wide, so dots are square
    let (xmax, ymax) = (inner.width.max(8) as f64, inner.height.max(4) as f64 * 2.0);
    let roomy = inner.height >= 18 && inner.width >= 70;
    let r_out = (ymax * 0.80).min(xmax * 0.55);
    let r_in = r_out * 0.46;
    // a terminal is wider than it is tall: stretch the picture sideways (layout only) so labels get room
    let sx = (xmax * 0.55 / r_out).clamp(1.0, 1.9);
    let pos: Vec<(f64, f64)> = layout(&m.peers, r_in, r_out).into_iter().map(|(x, y)| (x * sx, y)).collect();
    let phase = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as f64 / 2400.0).unwrap_or(0.0);
    let canvas = Canvas::default()
        .block(block)
        .marker(if ui_ascii() { Marker::Dot } else { Marker::Braille })
        .x_bounds([-xmax, xmax])
        .y_bounds([-ymax, ymax])
        .paint(|ctx| {
            // the machine the answering node runs on: everything connected over loopback lives inside this shell
            if m.on_box > 0 { ring(ctx, r_in * 1.32, sx, C_NEON_CYAN, true); }
            for (p, &(x, y)) in m.peers.iter().zip(&pos) {
                ctx.draw(&CLine { x1: 0.0, y1: 0.0, x2: x, y2: y, color: state_color(p.state) });
            }
            ctx.layer();
            // a travelling dot per link: outward for a node that holds our state, inward for one still fetching it
            for (p, &(x, y)) in m.peers.iter().zip(&pos) {
                if matches!(p.state, PeerState::Unknown) { continue; }
                let mut t = (phase + hash01(&p.id)).fract();
                if p.state == PeerState::Syncing { t = 1.0 - t; }
                ctx.draw(&Points { coords: &[(x * t, y * t)], color: Color::White });
            }
            // missing finality signers: placeholders on the top of the outer ring, clearly not nodes
            let ghosts = m.missing_signers();
            for g in 0..ghosts {
                let a = std::f64::consts::PI * (0.5 + 0.22 * (g as f64 - (ghosts as f64 - 1.0) / 2.0));
                let (x, y) = (r_out * sx * a.cos(), r_out * a.sin());
                ctx.print(x, y, Line::from(Span::styled(if ui_ascii() { "<>" } else { "◇" }, Style::default().fg(C_PURPLE))));
            }
            if ghosts > 0 && roomy {
                label_at(ctx, 0.0, (r_out + 3.0).min(ymax - 1.0), format!("missing: {ghosts} independent finality signer{}", if ghosts == 1 { "" } else { "s" }), C_PURPLE, xmax);
            }
            for (p, &(x, y)) in m.peers.iter().zip(&pos) {
                let c = state_color(p.state);
                ctx.print(x, y, Line::from(Span::styled(if ui_ascii() { "o" } else { "●" }, Style::default().fg(c).add_modifier(Modifier::BOLD))));
                // a small picture cannot carry names without them colliding: the list beside/below it does
                let mut text = if roomy { format!("{} {}", p.label, state_mark(p.state)) } else { state_mark(p.state).to_string() };
                if p.state == PeerState::Syncing { text.push_str(&format!(" {} %", (p.frac * 100.0).floor() as u64)); }
                label_at(ctx, x, y, text, c, xmax);
            }
            let sc = if m.is_maker { C_GOLD } else { C_NEON_CYAN };
            ctx.print(0.0, 0.0, Line::from(Span::styled(if ui_ascii() { "@" } else { "◆" }, Style::default().fg(sc).add_modifier(Modifier::BOLD))));
            label_at(ctx, 0.0, -3.0, format!("{}{}", m.self_label, if m.is_maker { " · block-maker" } else { "" }), sc, xmax);
        });
    f.render_widget(canvas, area);
}

fn rows(m: &NetMap) -> Vec<Line<'static>> {
    let mut out = vec![
        Line::from(vec![
            Span::styled(if ui_ascii() { "@ " } else { "◆ " }, Style::default().fg(if m.is_maker { C_GOLD } else { C_NEON_CYAN })),
            Span::styled(m.self_label.clone(), Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            dim(format!("  answering{}  h {}", if m.is_maker { " · block-maker" } else { "" }, group(m.self_height))),
        ]),
    ];
    let mut order: Vec<&PeerNode> = m.peers.iter().collect();
    order.sort_by(|a, b| b.local.cmp(&a.local).then(a.label.cmp(&b.label)));
    for p in order {
        let c = state_color(p.state);
        let mut tail = String::new();
        if let Some(h) = p.height { tail.push_str(&format!("  h {}", group(h))); }
        if p.state == PeerState::Syncing { tail.push_str(&format!("  {:.1} %", p.frac * 100.0)); }
        if p.stale { if let Some(a) = p.age_s { tail.push_str(&format!("  last report {} ago", if a < 5400 { format!("{} min", a / 60) } else { format!("{} h", a / 3600) })); } }
        out.push(Line::from(vec![
            Span::styled(format!("{:<2} ", state_mark(p.state)), Style::default().fg(c).add_modifier(Modifier::BOLD)),
            Span::styled(format!("{:<18}", p.label), Style::default().fg(Color::White)),
            dim(if p.local { " same machine  " } else { " own machine   " }),
            Span::styled(state_text(p.state), Style::default().fg(c)),
            dim(tail),
        ]));
    }
    out.push(Line::from(""));
    out.push(Line::from(vec![dim("block-makers  "), Span::styled(
        if m.blocks == 0 { "not read".to_string() }
        else if m.rig_blocks > 0 { format!("{} in the last {} blocks · {} won by mining rigs", m.makers, m.blocks, m.rig_blocks) }
        else { format!("{} in the last {} blocks", m.makers, m.blocks) },
        Style::default().fg(if m.makers >= 2 { C_GREEN } else { C_GOLD }))]));
    out.push(Line::from(vec![dim("finality      "), Span::styled(
        match m.committee { Some(n) => format!("{n} signer{} · {} gate · BFT {}", if n == 1 { "" } else { "s" }, m.gate, if m.bft == Some(true) { "on" } else { "off" }), None => "not read".into() },
        Style::default().fg(if m.bft == Some(true) { C_GREEN } else { C_GOLD }))]));
    out.push(Line::from(vec![dim("machines      "), Span::styled(
        format!("{} (this one + {} other) · {} nodes on this one", m.machines, m.machines - 1, 1 + m.on_box),
        Style::default().fg(if m.machines >= 4 { C_GREEN } else { C_GOLD }))]));
    out
}

/// Tab 6.
pub(crate) fn draw_map_tab(f: &mut Frame, _app: &crate::App, area: Rect) {
    let (map, source, fetched, err) = snapshot();
    let Some(m) = map else {
        let msg = err.unwrap_or_else(|| "reading /v1/network/topology …".into());
        f.render_widget(Paragraph::new(Line::from(dim(format!("  network map: {msg}")))).block(card_block("NETWORK MAP", C_NEON_CYAN)), area);
        return;
    };
    let [verdict_area, body_area, legend_area] =
        Layout::vertical([Constraint::Length(5), Constraint::Min(8), Constraint::Length(1)]).areas(area);

    let age = fetched.map(|t| t.elapsed().as_secs()).unwrap_or(0);
    let dc = if m.decentralized() { C_GREEN } else { C_GOLD };
    let ic = if m.count(PeerState::Differ) > 0 { C_RED } else if m.comparable() > 0 && m.count(PeerState::Agree) == m.comparable() { C_GREEN } else { C_GOLD };
    let head = vec![
        Line::from(vec![
            dim("answered by "), Span::styled(source, Style::default().fg(C_NEON_CYAN).add_modifier(Modifier::BOLD)),
            dim(format!(" ({})  ·  {} connected  ·  read {} s ago", m.self_label, m.peers.len(), age)),
            if let Some(e) = err { Span::styled(format!("  ·  last read failed: {e}"), Style::default().fg(C_RED)) } else { Span::raw("") },
        ]),
        Line::from(Span::styled(m.decentral_line(), Style::default().fg(dc))),
        Line::from(Span::styled(m.integrity_line(), Style::default().fg(ic))),
    ];
    f.render_widget(Paragraph::new(head).wrap(Wrap { trim: true }), verdict_area);

    if body_area.width >= 110 {
        let [map_area, rows_area] = Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)]).areas(body_area);
        draw_canvas(f, &m, map_area);
        f.render_widget(Paragraph::new(rows(&m)).block(card_block("NODES & DATA INTEGRITY", C_GREEN)), rows_area);
    } else {
        // narrow window: the list names every node, so it gets the rows it needs and the picture takes the rest
        let want = (m.peers.len() as u16 + 7).min(body_area.height.saturating_sub(10));
        let [map_area, rows_area] = Layout::vertical([Constraint::Min(8), Constraint::Length(want)]).areas(body_area);
        draw_canvas(f, &m, map_area);
        f.render_widget(Paragraph::new(rows(&m)).block(card_block("NODES & DATA INTEGRITY", C_GREEN)), rows_area);
    }

    let key = |mark: &'static str, c: Color, text: &'static str| vec![Span::styled(mark, Style::default().fg(c).add_modifier(Modifier::BOLD)), dim(text)];
    let mut legend = vec![Span::raw(" ")];
    legend.extend(key(state_mark(PeerState::Agree), C_GREEN, " same state   "));
    legend.extend(key(state_mark(PeerState::Checking), C_GOLD, " re-checking   "));
    legend.extend(key(state_mark(PeerState::Differ), C_RED, " differs (3 reads in a row)   "));
    legend.extend(key(state_mark(PeerState::Syncing), C_BLUE, " syncing   "));
    legend.extend(key("?", C_DIM, " no view   "));
    legend.extend(key(if ui_ascii() { "<>" } else { "◇" }, C_PURPLE, " missing signer   "));
    legend.push(dim("dashed ring = one machine"));
    f.render_widget(Paragraph::new(Line::from(legend)), legend_area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn topo(root_c: Option<bool>) -> Value {
        json!({"data": {
            "self_node_id": "sigil-g0-epsilon",
            "local_view": {"node": "sigil-g0-epsilon", "height": 1_000_000u64, "ts_ms": 2_000_000u64},
            "peers": [
                {"peer_id": "A", "multiaddr": "/ip4/127.0.0.1/tcp/1"},
                {"peer_id": "B", "multiaddr": "/ip4/203.0.113.7/tcp/9501"},
                {"peer_id": "C", "multiaddr": "/ip4/127.0.0.1/tcp/2"},
                {"peer_id": "D", "multiaddr": "/ip4/203.0.113.7/tcp/9502"},
                {"peer_id": "Eabcdef", "multiaddr": "/ip6/2001:db8::1/tcp/9501"}
            ],
            "peer_views": {
                "A": {"node": "sigil-g2-epsilon08-docker", "height": 999_999u64, "seen_ms": 1_999_000u64, "tip_matches_local": true, "state_root_matches_local": true},
                "B": {"node": "sigil-top-top-88c8baa811e42551", "height": 470_000u64, "seen_ms": 1_000_000u64, "tip_matches_local": null, "state_root_matches_local": null},
                "C": {"node": "sigil-g3-epsilon04-docker", "height": 999_998u64, "seen_ms": 1_999_500u64, "tip_matches_local": true, "state_root_matches_local": root_c},
                "D": {"node": "", "height": 999_990u64, "seen_ms": 1_999_900u64, "tip_matches_local": null, "state_root_matches_local": null}
            }
        }})
    }

    #[test]
    fn labels_never_show_an_address() {
        assert_eq!(node_label("sigil-g2-epsilon08-docker", "x"), "node-8");
        assert_eq!(node_label("sigil-g0-epsilon", "x"), "Epsilon");
        assert_eq!(node_label("sigil-top-top-88c8baa811e42551", "x"), "sigil-top 88c8ba");
        assert_eq!(node_label("sigil-top-cs30067.seedhost.eu", "x"), "Epsilon sigil-top");
        assert_eq!(node_label("", "12D3KooWabcdef"), "peer ..abcdef");
        assert_eq!(node_label("sigil-se1", "x"), "se1");
    }

    #[test]
    fn states_machines_and_verdicts() {
        let mut streak = BTreeMap::new();
        let cert = json!({"certificate": {"committee_size": 1, "gate": "solo", "bft": false}});
        let recent = json!([{"producer": [1, 2]}, {"producer": [1, 2]}, {"producer": [1, 2]}]);
        let m = build(&topo(Some(true)), Some(&cert), Some(&recent), &mut streak);
        let by = |id: &str| m.peers.iter().find(|p| p.id == id).unwrap().clone();
        assert_eq!(by("A").state, PeerState::Agree);
        assert_eq!(by("A").label, "node-8");
        assert_eq!(by("B").state, PeerState::Syncing);
        assert!((by("B").frac - 0.47).abs() < 1e-9);
        assert!(by("B").stale && !by("A").stale);
        assert_eq!(by("D").state, PeerState::Unknown, "10 blocks behind with no comparison is not syncing");
        assert_eq!(by("Eabcdef").state, PeerState::Unknown, "connected without a view");
        assert!(by("A").local && by("C").local && !by("B").local);
        assert_eq!(m.on_box, 2);
        assert_eq!(m.machines, 3, "this machine + two distinct remote addresses (B and D share one)");
        assert_eq!((m.makers, m.blocks), (1, 3));
        assert!(m.is_maker);
        assert!(!m.decentralized());
        assert_eq!(m.missing_signers(), 3);
        assert_eq!(m.decentral_line(), "Not decentralized yet: 1 block-maker made all 3 recent blocks, 1 key signs finality (solo gate, BFT off), and 2 of the 5 connected nodes run on the answering node's machine.");
        assert_eq!(m.integrity_line(), "Data integrity: 2 of 2 comparable nodes hold exactly the answering node's state at their own height · 1 still syncing · 2 not comparable yet.");
    }

    #[test]
    fn one_mismatch_is_rechecked_three_in_a_row_is_different_and_agreement_resets() {
        let mut streak = BTreeMap::new();
        let c = |m: &NetMap| m.peers.iter().find(|p| p.id == "C").unwrap().state;
        assert_eq!(c(&build(&topo(Some(false)), None, None, &mut streak)), PeerState::Checking);
        assert_eq!(c(&build(&topo(Some(false)), None, None, &mut streak)), PeerState::Checking);
        let third = build(&topo(Some(false)), None, None, &mut streak);
        assert_eq!(c(&third), PeerState::Differ);
        assert_eq!(third.comparable(), 2);
        assert_eq!(c(&build(&topo(Some(true)), None, None, &mut streak)), PeerState::Agree);
        assert_eq!(c(&build(&topo(Some(false)), None, None, &mut streak)), PeerState::Checking, "the count starts over");
    }

    #[test]
    fn a_block_won_by_a_mining_rig_is_not_a_second_block_maker() {
        // measured 2026-11-11: one of 200 recent blocks carried a rig's wallet as producer and the tab said "2 block-makers"
        let mut blocks: Vec<Value> = (0..199).map(|_| json!({"producer": [7, 7]})).collect();
        blocks.push(json!({"producer": [9, 9]}));
        let mut streak = BTreeMap::new();
        let m = build(&topo(Some(true)), None, Some(&json!({"data": blocks})), &mut streak);
        assert_eq!((m.makers, m.blocks, m.rig_blocks), (1, 200, 1));
        assert!(m.is_maker);
        assert!(m.decentral_line().contains("1 block-maker made 199 of the 200 recent blocks (1 won by mining rigs)"));
        // two ids that each make a real share ARE two block-makers
        let two: Vec<Value> = (0..200).map(|i| json!({"producer": [i % 2]})).collect();
        let m2 = build(&topo(Some(true)), None, Some(&json!(two)), &mut streak);
        assert_eq!((m2.makers, m2.rig_blocks), (2, 0));
        assert!(!m2.is_maker, "with two makers the route cannot say which one answered");
    }

    #[test]
    fn decentralized_needs_all_four_conditions() {
        let mut m = NetMap { makers: 2, blocks: 200, committee: Some(4), bft: Some(true), machines: 4, ..Default::default() };
        assert!(m.decentralized());
        assert_eq!(m.missing_signers(), 0);
        for breaker in 0..4 {
            let mut x = m.clone();
            match breaker { 0 => x.makers = 1, 1 => x.committee = Some(3), 2 => x.bft = Some(false), _ => x.machines = 3 }
            assert!(!x.decentralized(), "condition {breaker} alone must break it");
        }
        m.committee = None;
        assert!(m.decentral_line().contains("finality not read"), "an unread route is said, never shown as 0");
    }

    #[test]
    fn layout_keeps_same_machine_nodes_inside_the_shell() {
        let mut streak = BTreeMap::new();
        let m = build(&topo(Some(true)), None, None, &mut streak);
        let pos = layout(&m.peers, 10.0, 30.0);
        for (p, (x, y)) in m.peers.iter().zip(pos) {
            let r = (x * x + y * y).sqrt();
            assert!((r - if p.local { 10.0 } else { 30.0 }).abs() < 1e-6, "{} at r={r}", p.label);
        }
    }
}
