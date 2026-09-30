//! ◈ DATA INTEGRITY — "is what I hold the same chain as the network?" (2026-09-17)
//!
//! Viktor: "i want a metric for data integrity so i can validate that its true." The
//! metric is one comparison, made exactly and repeatedly: OUR block hash at height H
//! against each network node's block hash at the SAME height H. A block hash commits to
//! its header, the header carries the four state roots the node computed after applying
//! that block, and a follower refuses to apply a block whose roots disagree with its own
//! recomputation ("STATE DIVERGENCE", 2026-09-10). So equal hash at equal height means
//! equal committed state at that height — no race, no "a block or two apart" caveat, the
//! caveat `/v1/integrity` has to carry because it reads its height and its live roots
//! separately.
//!
//! Sources, all public:
//! * ours — the embedded full node's settled ring (`producer::status::settled_hash_at`)
//!   when the operator pressed F, else the light archive (`BlockReader::hash_at_height`);
//! * Epsilon — `{api}/v1/dagknight/recent` (its finalized spine, 200 heights) and
//!   `{api}/v1/finality/certificate` (the certified height; we only compare at or below it);
//! * happysrv / node3 — `sigilgraph.org/downloads/sigil-integrity.json` `spine_hashes`,
//!   published every 5 s by the `sigil-integrity` monitor that follows their certificates.
//!
//! Every height is counted once per node. `checked` and `mismatches` are cumulative for
//! the session; the verdict is FALSE the moment one hash differs and stays visible.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::heroes::card_block;
use crate::tabs_ui::{dim, group};
use crate::{C_BG, C_DIM, C_GOLD, C_GREEN, C_NEON_CYAN, C_RED};

pub(crate) const MONITOR_URL: &str = "https://sigilgraph.org/downloads/sigil-integrity.json";
const PERIOD: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
pub(crate) struct NodeCheck {
    /// Highest height compared so far for this node.
    pub height: u64,
    /// Did our hash equal theirs at `height`?
    pub agree: bool,
    /// Their newest published height (how far ahead of our comparison they are).
    pub remote_top: u64,
}

#[derive(Default)]
pub(crate) struct IntegrityState {
    pub nodes: BTreeMap<String, NodeCheck>,
    pub checked: u64,
    pub mismatches: u64,
    pub last_run: Option<Instant>,
    pub last_err: Option<String>,
    /// (node, height, ours, theirs) of the newest disagreement.
    pub last_mismatch: Option<(String, u64, String, String)>,
    pub local_source: &'static str,
    pub local_top: u64,
    pub certified: u64,
    /// Per node: every height at or below this has been counted.
    counted_to: BTreeMap<String, u64>,
    // ── THE FIFTH ROOT (2026-09-25) ──
    /// Epsilon's newest `/v1/integrity` reading of the shielded pool root.
    pub pool_epsilon: Option<PoolObs>,
    /// Our embedded node's reading (only when the operator runs one — F).
    pub pool_ours: Option<PoolObs>,
    pub pool_checked: u64,
    pub pool_mismatches: u64,
    /// Consecutive EQUAL-HEIGHT disagreements; ≥ [`POOL_PERSISTENT`] is a fork, less is a race.
    pub pool_consecutive_mismatch: u32,
    /// (height, ours, theirs) of the newest pool-root disagreement.
    pub pool_last_mismatch: Option<(u64, String, String)>,
    /// The height already compared once (a node answers the same height for ~1 s at 8 blk/s).
    pool_compared_height: Option<u64>,
    /// Epsilon's measured block rate (blocks per second), smoothed; 0 = unknown.
    pub bps: f64,
    bps_prev: Option<(Instant, u64)>,
}

// ── THE FIFTH ROOT (2026-09-25) ────────────────────────────────────────────────
// `shielded_pool_root` is committed in the header from
// `sigil_header::SHIELDED_POOL_ROOT_ACTIVATION_HEIGHT` (27,000,000). Below the gate the
// header does not carry it, so equal HASH at equal height says nothing about the pool:
// two nodes could hold different note trees / nullifier sets, agree on every hash until
// the gate, and fork AT it. `/v1/integrity` publishes the LIVE pool root on every node
// today, so the client compares it at equal `applied_height` BEFORE the gate, while a
// divergence is still cheap.
//
// 🪤 `/v1/integrity` reads its height and its roots separately (its own `caveat`), and for
// ~2 min after a node boots `applied_height` runs ahead of the state it reports — so ONE
// equal-height mismatch is a race, not a fork. Only a PERSISTENT one (`POOL_PERSISTENT`
// consecutive equal-height disagreements) flips the verdict.
pub(crate) const POOL_PERSISTENT: u32 = 3;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PoolObs {
    /// `applied_height` — the height the reported roots belong to (modulo the caveat).
    pub height: u64,
    /// `roots.shielded_pool_root`, lowercase hex (64 chars).
    pub root: String,
    /// `shielded_pool_root_in_header` — is this tip past the gate?
    pub in_header: bool,
    /// `shielded_pool_root_activation_height`.
    pub gate: u64,
    pub notes: u64,
    pub nullifiers: u64,
}

/// Parse one `/v1/integrity` body. `None` if it carries no pool root (a pre-fifth-root node).
pub(crate) fn parse_integrity(body: &str) -> Option<PoolObs> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let d = v.get("data").unwrap_or(&v);
    let root = d.get("roots")?.get("shielded_pool_root")?.as_str()?.to_ascii_lowercase();
    if root.len() != 64 || !root.bytes().all(|b| b.is_ascii_hexdigit()) { return None; }
    let height = d.get("applied_height").and_then(|x| x.as_u64())
        .or_else(|| d.get("height").and_then(|x| x.as_u64()))?;
    let sh = d.get("shielded");
    Some(PoolObs {
        height,
        root,
        in_header: d.get("shielded_pool_root_in_header").and_then(|x| x.as_bool()).unwrap_or(false),
        gate: d.get("shielded_pool_root_activation_height").and_then(|x| x.as_u64())
            .unwrap_or(sigil_header::SHIELDED_POOL_ROOT_ACTIVATION_HEIGHT),
        notes: sh.and_then(|s| s.get("notes")).and_then(|x| x.as_u64()).unwrap_or(0),
        nullifiers: sh.and_then(|s| s.get("nullifiers")).and_then(|x| x.as_u64()).unwrap_or(0),
    })
}

fn fetch_integrity(base: &str) -> Option<PoolObs> {
    let body = crate::HTTP.get(format!("{base}/v1/integrity")).timeout(Duration::from_secs(8)).send().ok()?.text().ok()?;
    parse_integrity(&body)
}

/// Equal-height comparison. `None` = different heights (or height 0): no verdict, ever.
pub(crate) fn pool_agree(a: &PoolObs, b: &PoolObs) -> Option<bool> {
    (a.height == b.height && a.height > 0).then(|| a.root.eq_ignore_ascii_case(&b.root))
}

/// Blocks left to the gate at the measured block rate: "~41h" / "~12m" / "reached" / "—".
pub(crate) fn gate_eta(blocks_left: u64, bps: f64) -> String {
    if blocks_left == 0 { return "reached".into(); }
    if !(bps > 0.0) || !bps.is_finite() { return "—".into(); }
    let secs = blocks_left as f64 / bps;
    if secs < 90.0 { format!("~{}s", secs.round() as u64) }
    else if secs < 5_400.0 { format!("~{}m", (secs / 60.0).round() as u64) }
    else if secs < 172_800.0 { format!("~{:.0}h", secs / 3600.0) }
    else { format!("~{:.1}d", secs / 86_400.0) }
}

/// Fold one (ours, epsilon) pair into the state; a height is counted once. Returns this
/// pass's verdict (`None` = not comparable).
pub(crate) fn note_pool_compare(st: &mut IntegrityState, ours: &PoolObs, theirs: &PoolObs) -> Option<bool> {
    let v = pool_agree(ours, theirs)?;
    if st.pool_compared_height == Some(ours.height) { return Some(v); }
    st.pool_compared_height = Some(ours.height);
    st.pool_checked += 1;
    if v {
        st.pool_consecutive_mismatch = 0;
    } else {
        st.pool_mismatches += 1;
        st.pool_consecutive_mismatch += 1;
        st.pool_last_mismatch = Some((ours.height, ours.root.clone(), theirs.root.clone()));
    }
    Some(v)
}

pub(crate) fn pool_persistent_mismatch(st: &IntegrityState) -> bool {
    st.pool_consecutive_mismatch >= POOL_PERSISTENT
}

/// Blend a new block-rate sample into the smoothed estimate (0 = no estimate yet).
pub(crate) fn blend_bps(prev: f64, sample: f64) -> f64 {
    if !(sample >= 0.0) || !sample.is_finite() { return prev; }
    if prev > 0.0 { 0.7 * prev + 0.3 * sample } else { sample }
}

fn cell() -> &'static Mutex<IntegrityState> {
    static CELL: OnceLock<Mutex<IntegrityState>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(IntegrityState::default()))
}

pub(crate) fn snapshot() -> IntegrityState {
    let g = cell().lock().unwrap_or_else(|e| e.into_inner());
    IntegrityState {
        nodes: g.nodes.clone(),
        checked: g.checked,
        mismatches: g.mismatches,
        last_run: g.last_run,
        last_err: g.last_err.clone(),
        last_mismatch: g.last_mismatch.clone(),
        local_source: g.local_source,
        local_top: g.local_top,
        certified: g.certified,
        counted_to: BTreeMap::new(),
        pool_epsilon: g.pool_epsilon.clone(),
        pool_ours: g.pool_ours.clone(),
        pool_checked: g.pool_checked,
        pool_mismatches: g.pool_mismatches,
        pool_consecutive_mismatch: g.pool_consecutive_mismatch,
        pool_last_mismatch: g.pool_last_mismatch.clone(),
        pool_compared_height: g.pool_compared_height,
        bps: g.bps,
        bps_prev: None,
    }
}

/// Our hash at `h`, with where it came from. The embedded full node wins when it holds
/// the height (its hashes are of blocks it APPLIED, roots recomputed); the archive holds
/// verified headers only.
fn local_hash(reader: &Arc<OnceLock<sigil_block_store::BlockReader>>, h: u64) -> Option<(String, &'static str)> {
    #[cfg(feature = "producer")]
    if let Some(x) = crate::producer::status::settled_hash_at(h) {
        return Some((hex::encode(x), "full node"));
    }
    let r = reader.get()?;
    r.hash_at_height(h).map(|s| (s.to_ascii_lowercase(), "archive"))
}

fn local_top(reader: &Arc<OnceLock<sigil_block_store::BlockReader>>) -> (u64, &'static str) {
    #[cfg(feature = "producer")]
    {
        let t = crate::producer::status::settled_top();
        if t > 0 {
            return (t, "full node");
        }
    }
    (reader.get().map(|r| r.best_height()).unwrap_or(0), "archive")
}

/// Epsilon's finalized spine from `/v1/dagknight/recent`: height → hash hex.
fn fetch_recent(api_base: &str) -> Option<BTreeMap<u64, String>> {
    let body = crate::HTTP.get(format!("{api_base}/v1/dagknight/recent")).timeout(Duration::from_secs(8)).send().ok()?.text().ok()?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    let d = v.get("data").unwrap_or(&v);
    let items = d.as_array().or_else(|| d.get("blocks").and_then(|b| b.as_array()))?;
    let mut out = BTreeMap::new();
    for b in items {
        let h = b.get("height")?.as_u64()?;
        let hash = match b.get("hash")? {
            serde_json::Value::Array(bytes) => {
                let v: Vec<u8> = bytes.iter().filter_map(|x| x.as_u64().map(|n| n as u8)).collect();
                hex::encode(v)
            }
            serde_json::Value::String(s) => s.to_ascii_lowercase(),
            _ => continue,
        };
        out.insert(h, hash);
    }
    Some(out)
}

fn fetch_certified(api_base: &str) -> Option<u64> {
    let body = crate::HTTP.get(format!("{api_base}/v1/finality/certificate")).timeout(Duration::from_secs(8)).send().ok()?.text().ok()?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    let d = v.get("data").unwrap_or(&v);
    d.get("height").and_then(|x| x.as_u64())
        .or_else(|| d.get("certificate").and_then(|c| c.get("height")).and_then(|x| x.as_u64()))
}

/// The monitor's per-node `(height, hash)` lists.
fn fetch_monitor() -> Option<BTreeMap<String, BTreeMap<u64, String>>> {
    let body = crate::HTTP.get(MONITOR_URL).timeout(Duration::from_secs(8)).send().ok()?.text().ok()?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    let sh = v.get("spine_hashes")?.as_object()?;
    let mut out = BTreeMap::new();
    for (node, list) in sh {
        let mut m = BTreeMap::new();
        for e in list.as_array().into_iter().flatten() {
            if let (Some(h), Some(x)) = (e.get(0).and_then(|x| x.as_u64()), e.get(1).and_then(|x| x.as_str())) {
                m.insert(h, x.to_ascii_lowercase());
            }
        }
        out.insert(node.clone(), m);
    }
    Some(out)
}

/// One comparison pass over `remote` for `node`: every height not yet counted, at or
/// below `ceiling`, that we hold locally. Returns (checked, mismatched, newest NodeCheck).
fn compare(
    st: &mut IntegrityState,
    reader: &Arc<OnceLock<sigil_block_store::BlockReader>>,
    node: &str,
    remote: &BTreeMap<u64, String>,
    ceiling: u64,
) {
    let from = st.counted_to.get(node).copied().unwrap_or(0);
    let remote_top = remote.keys().next_back().copied().unwrap_or(0);
    let mut newest: Option<NodeCheck> = None;
    let mut counted_to = from;
    if from >= ceiling {
        if let Some(n) = st.nodes.get_mut(node) { n.remote_top = remote_top; }
        return;
    }
    for (h, theirs) in remote.range((from + 1)..=ceiling) {
        let Some((ours, src)) = local_hash(reader, *h) else { continue };
        st.local_source = src;
        let agree = ours.eq_ignore_ascii_case(theirs);
        st.checked += 1;
        if !agree {
            st.mismatches += 1;
            st.last_mismatch = Some((node.to_string(), *h, ours.clone(), theirs.clone()));
        }
        counted_to = counted_to.max(*h);
        newest = Some(NodeCheck { height: *h, agree, remote_top });
    }
    st.counted_to.insert(node.to_string(), counted_to);
    match newest {
        Some(n) => { st.nodes.insert(node.to_string(), n); }
        None => {
            // Nothing new to compare — keep the last verdict, refresh their top.
            if let Some(n) = st.nodes.get_mut(node) { n.remote_top = remote_top; }
        }
    }
}

/// Idempotent: the first call starts the sampler thread, later calls are one atomic.
pub(crate) fn spawn(api_base: String, reader: Arc<OnceLock<sigil_block_store::BlockReader>>) {
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    std::thread::Builder::new()
        .name("integrity".into())
        .spawn(move || {
            let mut last_log = Instant::now() - Duration::from_secs(60);
            loop {
            let certified = fetch_certified(&api_base);
            let recent = fetch_recent(&api_base);
            let monitor = fetch_monitor();
            // The fifth root: Epsilon's live pool root, and ours when an embedded node runs.
            let pool_eps = fetch_integrity(&api_base);
            #[cfg(feature = "producer")]
            let pool_ours = if crate::producer::mining_api::local_mining_api_is_up() {
                fetch_integrity(&crate::producer::mining_api::local_mining_api_url())
            } else { None };
            #[cfg(not(feature = "producer"))]
            let pool_ours: Option<PoolObs> = None;
            {
                let mut g = cell().lock().unwrap_or_else(|e| e.into_inner());
                if let Some(e) = pool_eps.as_ref() {
                    let now = Instant::now();
                    if let Some((t0, h0)) = g.bps_prev {
                        let dt = now.duration_since(t0).as_secs_f64();
                        if dt >= 5.0 && e.height >= h0 { g.bps = blend_bps(g.bps, (e.height - h0) as f64 / dt); }
                    }
                    g.bps_prev = Some((now, e.height));
                    g.pool_epsilon = Some(e.clone());
                }
                if let Some(o) = pool_ours.as_ref() { g.pool_ours = Some(o.clone()); }
                if let (Some(o), Some(e)) = (pool_ours.as_ref(), pool_eps.as_ref()) { note_pool_compare(&mut g, o, e); }
                let (top, src) = local_top(&reader);
                g.local_top = top;
                if top > 0 { g.local_source = src; }
                if let Some(c) = certified { g.certified = c; }
                let ceiling = if g.certified > 0 { g.certified } else { u64::MAX };
                let mut any = false;
                // Epsilon: the monitor's 512-height list (reaches back to where a depth-rule
                // node like us has settled) merged with the live route (newest ~200, exact
                // from the node itself; wins on overlap). Measured 2026-09-17: the live
                // window alone sits ~30 blocks under the tip while our settled ring is 512
                // behind it — no overlap, nothing compared, the card froze at one height.
                let mut epsilon: BTreeMap<u64, String> = monitor.as_ref().and_then(|m| m.get("epsilon").cloned()).unwrap_or_default();
                if let Some(r) = recent.as_ref() {
                    epsilon.extend(r.iter().map(|(h, x)| (*h, x.clone())));
                }
                if !epsilon.is_empty() {
                    any = true;
                    compare(&mut g, &reader, "epsilon", &epsilon, ceiling);
                }
                if let Some(m) = monitor.as_ref() {
                    for (node, map) in m {
                        if node == "epsilon" { continue; }
                        any = true;
                        compare(&mut g, &reader, node, map, ceiling);
                    }
                }
                // `last_run` means "the last time a LIVE source answered" — not "the last time this loop
                // ran". Stamping every pass made a dead monitor / unreachable Epsilon read "3s ago" and
                // keep a green TRUE on data that had stopped moving.
                if recent.is_some() || pool_eps.is_some() { g.last_run = Some(Instant::now()); }
                g.last_err = if any { None }
                    else if pool_eps.is_some() { Some("hash sources unreachable (recent + monitor); /v1/integrity answered".into()) }
                    else { Some("no node answered (feed + monitor unreachable)".into()) };
            }
            // Once a minute, the same verdict in the logfile — evidence that outlives the screen.
            if last_log.elapsed() >= Duration::from_secs(60) {
                last_log = Instant::now();
                let s = snapshot();
                if !s.nodes.is_empty() {
                    let per: Vec<String> = s.nodes.iter().map(|(n, c)| format!("{n}{}@{}", if c.agree { "✓" } else { "✗" }, c.height)).collect();
                    let pool = match (s.pool_ours.as_ref(), s.pool_epsilon.as_ref()) {
                        (Some(o), Some(e)) => format!("pool {}@{} ours {}@{} cmp {} mism {}{}",
                            short(&e.root), e.height, short(&o.root), o.height, s.pool_checked, s.pool_mismatches,
                            if pool_persistent_mismatch(&s) { " PERSISTENT" } else { "" }),
                        (None, Some(e)) => format!("pool {}@{} ({})", short(&e.root), e.height, if e.in_header { "in header" } else { "pre-gate" }),
                        _ => "pool —".into(),
                    };
                    crate::tlog!(
                        "[integrity] {} · {} · checked {} · mismatches {} · src {} · certified {} · {}",
                        if s.nodes.values().all(|c| c.agree) && !pool_persistent_mismatch(&s) { "TRUE" } else { "FALSE" },
                        per.join(" "), s.checked, s.mismatches, s.local_source, s.certified, pool
                    );
                }
            }
            std::thread::sleep(PERIOD);
            }
        })
        .ok();
}

/// No live source has answered for this long: the verdict is a memory, not a reading.
const STALE_SECS: u64 = 60;

/// `None` (never answered) is not stale — the card says WAITING / has no nodes yet.
fn is_stale(age_secs: Option<u64>) -> bool {
    age_secs.map_or(false, |a| a > STALE_SECS)
}

fn short(h: &str) -> String {
    if h.len() >= 8 { format!("{}…", &h[..8]) } else { h.to_string() }
}

pub(crate) fn render_card(_app: &crate::App) -> Paragraph<'static> {
    let s = snapshot();
    let badge = |t: &str, bg| Span::styled(format!(" {t} "), Style::default().bg(bg).fg(C_BG).add_modifier(Modifier::BOLD));
    let agreeing = s.nodes.values().filter(|n| n.agree).count();
    let total = s.nodes.len();
    let verdict = if total == 0 {
        badge("WAITING", C_DIM)
    } else if s.nodes.values().any(|n| !n.agree) || pool_persistent_mismatch(&s) {
        badge("FALSE", C_RED)
    } else if is_stale(s.last_run.map(|t| t.elapsed().as_secs())) {
        badge("STALE", C_GOLD)
    } else if s.mismatches > 0 {
        badge("TRUE NOW", C_GOLD)
    } else {
        badge("TRUE", C_GREEN)
    };
    let age = s.last_run.map(|t| format!("{}s ago", t.elapsed().as_secs())).unwrap_or_else(|| "—".into());
    let mut l1 = vec![
        verdict,
        dim(format!("  {agreeing}/{total} nodes agree · checked {} · mismatch {} · {age}", group(s.checked), group(s.mismatches))),
    ];
    if let Some(e) = s.last_err.as_ref() { l1.push(Span::styled(format!("  {e}"), Style::default().fg(C_RED))); }

    let mut l2v = vec![
        dim("you  "),
        Span::styled(if s.local_top > 0 { format!("h {}", group(s.local_top)) } else { "no local chain yet".into() }, Style::default().fg(C_NEON_CYAN)),
        dim(format!("  src {}   certified ≤ {}", if s.local_source.is_empty() { "—" } else { s.local_source }, if s.certified > 0 { group(s.certified) } else { "—".into() })),
    ];
    // The gate countdown: every client must decode the 5-root header before the tip gets there.
    if let Some(e) = s.pool_epsilon.as_ref() {
        if e.in_header || e.height >= e.gate {
            l2v.push(Span::styled("  5th root in header ✓", Style::default().fg(C_GREEN)));
        } else {
            let left = e.gate.saturating_sub(e.height);
            l2v.push(dim(format!("  gate {} in {} blk {}", group(e.gate), group(left), gate_eta(left, s.bps))));
        }
    }
    let l2 = Line::from(l2v);

    let mut l3: Vec<Span<'static>> = Vec::new();
    for (name, n) in &s.nodes {
        let (mark, col) = if n.agree { ("✓", C_GREEN) } else { ("✗", C_RED) };
        l3.push(Span::styled(format!("{name} {mark} "), Style::default().fg(col)));
        l3.push(dim(format!("{}  ", group(n.height))));
    }
    if l3.is_empty() { l3.push(dim("comparing our hash against Epsilon, happysrv, node3 at equal height…")); }

    let l4 = if let Some((node, h, ours, theirs)) = s.last_mismatch.as_ref() {
        Line::from(vec![
            Span::styled("last mismatch ", Style::default().fg(C_RED)),
            dim(format!("{node} @ {}: ours {} theirs {}", group(*h), short(ours), short(theirs))),
        ])
    } else if let (true, Some((h, ours, theirs))) = (pool_persistent_mismatch(&s), s.pool_last_mismatch.as_ref()) {
        Line::from(vec![
            Span::styled("5th root DIFFERS ", Style::default().fg(C_RED)),
            dim(format!("@ {} ×{}: ours {} epsilon {} — the pool forks at the gate", group(*h), s.pool_consecutive_mismatch, short(ours), short(theirs))),
        ])
    } else if let Some(e) = s.pool_epsilon.as_ref() {
        // The fifth root, compared at equal height when we run a node; shown live otherwise.
        let mut v = vec![
            Span::styled("5th root ", Style::default().fg(C_GOLD)),
            dim(format!("pool {} @ {} {}", short(&e.root), group(e.height), if e.in_header { "committed" } else { "live·pre-gate" })),
        ];
        match s.pool_ours.as_ref() {
            Some(o) => {
                let (mark, col) = match pool_agree(o, e) {
                    Some(true) => ("✓ ours equal", C_GREEN),
                    Some(false) => ("✗ ours differ", C_RED),
                    None => ("· ours at another height", C_DIM),
                };
                v.push(Span::styled(format!("  {mark}"), Style::default().fg(col)));
                if s.pool_checked > 0 { v.push(dim(format!(" ({} cmp · {} mism)", group(s.pool_checked), group(s.pool_mismatches)))); }
            }
            None => v.push(dim("  ours — (F runs a node and compares)")),
        }
        Line::from(v)
    } else {
        Line::from(vec![dim("equal hash at equal height ⇒ equal header ⇒ equal state roots (exact, no race)")])
    };

    Paragraph::new(vec![Line::from(l1), l2, Line::from(l3), l4]).block(card_block(" ◈ DATA INTEGRITY", C_NEON_CYAN))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staleness_is_a_measured_silence_not_a_default() {
        assert!(!is_stale(None), "never answered is WAITING, not STALE");
        assert!(!is_stale(Some(0)));
        assert!(!is_stale(Some(STALE_SECS)), "the boundary itself is still fresh");
        assert!(is_stale(Some(STALE_SECS + 1)));
    }

    fn reader_none() -> Arc<OnceLock<sigil_block_store::BlockReader>> { Arc::new(OnceLock::new()) }

    /// Fixture = the live producer's `/v1/integrity` on 2026-09-25 (h 25,818,771), trimmed.
    const LIVE_INTEGRITY: &str = r#"{"ok":true,"applied_height":25818771,"height":25818771,"finalized_height":null,
      "roots":{"contract_state_root":"0000000000000000000000000000000000000000000000000000000000000000",
      "dex_state_root":"0000000000000000000000000000000000000000000000000000000000000000",
      "event_log_root":"0000000000000000000000000000000000000000000000000000000000000000",
      "shielded_pool_root":"1110A57B6F649C09BC84354E4531A8BE112949F9711F1C72BEA3041A7C5F5AC6",
      "wallet_state_root":"b29db47b88cfc1c43bace40901cf6b70d168c40035426cb174a77a3606a2c7a6"},
      "shielded":{"anchor":"1752afe868dc265e000000000000000000000000000000000000000000000000","notes":2364,"nullifiers":2575},
      "shielded_pool_root_activation_height":27000000,"shielded_pool_root_in_header":false,"ts_ms":1790342591598}"#;

    fn obs(h: u64, root: &str) -> PoolObs {
        PoolObs { height: h, root: root.repeat(32), in_header: false, gate: 27_000_000, notes: 0, nullifiers: 0 }
    }

    #[test]
    fn parses_the_live_integrity_body_lowercasing_the_root() {
        let p = parse_integrity(LIVE_INTEGRITY).expect("parse");
        assert_eq!(p.height, 25_818_771);
        assert_eq!(p.root, "1110a57b6f649c09bc84354e4531a8be112949f9711f1c72bea3041a7c5f5ac6");
        assert!(!p.in_header);
        assert_eq!(p.gate, 27_000_000);
        assert_eq!((p.notes, p.nullifiers), (2364, 2575));
        // A pre-fifth-root node (no `shielded_pool_root` key) parses to None — nothing to compare.
        assert!(parse_integrity(r#"{"ok":true,"applied_height":5,"roots":{"wallet_state_root":"00"}}"#).is_none());
        // A malformed root is refused, not compared as a string.
        assert!(parse_integrity(r#"{"applied_height":5,"roots":{"shielded_pool_root":"zz"}}"#).is_none());
    }

    /// The comparison is exact and only ever at EQUAL height — two readings a block apart
    /// legitimately differ and must produce no verdict at all.
    #[test]
    fn pool_agree_is_equal_height_only() {
        assert_eq!(pool_agree(&obs(10, "aa"), &obs(10, "aa")), Some(true));
        assert_eq!(pool_agree(&obs(10, "aa"), &obs(10, "AA")), Some(true));
        assert_eq!(pool_agree(&obs(10, "aa"), &obs(10, "bb")), Some(false));
        assert_eq!(pool_agree(&obs(10, "aa"), &obs(11, "aa")), None);
        assert_eq!(pool_agree(&obs(0, "aa"), &obs(0, "aa")), None);
    }

    /// One equal-height disagreement is a race (`/v1/integrity` reads height and roots
    /// separately); the verdict flips only after POOL_PERSISTENT consecutive ones, and a
    /// single agreement resets the streak. A height is counted once.
    #[test]
    fn persistent_rule_and_count_once() {
        let mut st = IntegrityState::default();
        assert_eq!(note_pool_compare(&mut st, &obs(100, "aa"), &obs(100, "bb")), Some(false));
        assert_eq!(note_pool_compare(&mut st, &obs(100, "aa"), &obs(100, "bb")), Some(false)); // same height again
        assert_eq!((st.pool_checked, st.pool_mismatches, st.pool_consecutive_mismatch), (1, 1, 1));
        assert!(!pool_persistent_mismatch(&st));
        note_pool_compare(&mut st, &obs(101, "aa"), &obs(101, "bb"));
        assert!(!pool_persistent_mismatch(&st));
        note_pool_compare(&mut st, &obs(102, "aa"), &obs(102, "bb"));
        assert!(pool_persistent_mismatch(&st));
        assert_eq!(st.pool_last_mismatch.as_ref().map(|m| m.0), Some(102));
        // One agreement at a new height ends the streak; the cumulative count stays.
        assert_eq!(note_pool_compare(&mut st, &obs(103, "cc"), &obs(103, "cc")), Some(true));
        assert!(!pool_persistent_mismatch(&st));
        assert_eq!((st.pool_checked, st.pool_mismatches), (4, 3));
        // Different heights: nothing counted.
        assert_eq!(note_pool_compare(&mut st, &obs(104, "cc"), &obs(105, "cc")), None);
        assert_eq!(st.pool_checked, 4);
    }

    /// 1,181,148 blocks at the 8 blk/s measured 2026-09-25 14:03 CEST ≈ 41 h.
    #[test]
    fn gate_eta_is_derived_from_the_measured_rate() {
        assert_eq!(gate_eta(1_181_148, 8.0), "~41h");
        assert_eq!(gate_eta(0, 8.0), "reached");
        assert_eq!(gate_eta(1_000, 0.0), "—");
        assert_eq!(gate_eta(1_000, f64::NAN), "—");
        assert_eq!(gate_eta(80, 8.0), "~10s");
        assert_eq!(gate_eta(4_800, 8.0), "~10m");
        assert_eq!(gate_eta(2_000_000, 8.0), "~2.9d");
        assert_eq!(blend_bps(0.0, 8.0), 8.0);
        assert!((blend_bps(8.0, 4.0) - 6.8).abs() < 1e-9);
        assert_eq!(blend_bps(8.0, f64::NAN), 8.0);
    }

    /// With no local chain there is nothing to compare: nothing counted, no verdict.
    #[test]
    fn no_local_chain_counts_nothing() {
        let mut st = IntegrityState::default();
        let mut remote = BTreeMap::new();
        remote.insert(100u64, "aa".repeat(32));
        compare(&mut st, &reader_none(), "epsilon", &remote, u64::MAX);
        assert_eq!(st.checked, 0);
        assert!(st.nodes.is_empty());
    }

    /// The ceiling is respected: a height above the certified line is not compared.
    #[cfg(feature = "producer")]
    #[test]
    fn compares_only_at_or_below_the_certified_height_and_counts_once() {
        crate::producer::status::push_settled(1_000_000, [0x11; 32]);
        crate::producer::status::push_settled(1_000_001, [0x22; 32]);
        crate::producer::status::push_settled(1_000_002, [0x33; 32]);
        let mut st = IntegrityState::default();
        let mut remote = BTreeMap::new();
        remote.insert(1_000_000u64, "11".repeat(32));
        remote.insert(1_000_001u64, "22".repeat(32));
        remote.insert(1_000_002u64, "ff".repeat(32)); // above the ceiling AND wrong — must be ignored
        compare(&mut st, &reader_none(), "happysrv", &remote, 1_000_001);
        assert_eq!(st.checked, 2);
        assert_eq!(st.mismatches, 0);
        assert!(st.nodes["happysrv"].agree);
        assert_eq!(st.nodes["happysrv"].height, 1_000_001);
        // Same input again: nothing is double-counted.
        compare(&mut st, &reader_none(), "happysrv", &remote, 1_000_001);
        assert_eq!(st.checked, 2);
        // Raise the ceiling: the wrong hash is now compared and flagged.
        compare(&mut st, &reader_none(), "happysrv", &remote, 1_000_002);
        assert_eq!(st.checked, 3);
        assert_eq!(st.mismatches, 1);
        assert!(!st.nodes["happysrv"].agree);
        assert_eq!(st.last_mismatch.as_ref().map(|m| m.1), Some(1_000_002));
    }
}
