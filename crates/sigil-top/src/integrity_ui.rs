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
            {
                let mut g = cell().lock().unwrap_or_else(|e| e.into_inner());
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
                g.last_run = Some(Instant::now());
                g.last_err = if any { None } else { Some("no node answered (feed + monitor unreachable)".into()) };
            }
            // Once a minute, the same verdict in the logfile — evidence that outlives the screen.
            if last_log.elapsed() >= Duration::from_secs(60) {
                last_log = Instant::now();
                let s = snapshot();
                if !s.nodes.is_empty() {
                    let per: Vec<String> = s.nodes.iter().map(|(n, c)| format!("{n}{}@{}", if c.agree { "✓" } else { "✗" }, c.height)).collect();
                    crate::tlog!(
                        "[integrity] {} · {} · checked {} · mismatches {} · src {} · certified {}",
                        if s.nodes.values().all(|c| c.agree) { "TRUE" } else { "FALSE" },
                        per.join(" "), s.checked, s.mismatches, s.local_source, s.certified
                    );
                }
            }
            std::thread::sleep(PERIOD);
            }
        })
        .ok();
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
    } else if s.nodes.values().any(|n| !n.agree) {
        badge("FALSE", C_RED)
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

    let l2 = Line::from(vec![
        dim("you  "),
        Span::styled(if s.local_top > 0 { format!("h {}", group(s.local_top)) } else { "no local chain yet".into() }, Style::default().fg(C_NEON_CYAN)),
        dim(format!("  src {}   certified ≤ {}", if s.local_source.is_empty() { "—" } else { s.local_source }, if s.certified > 0 { group(s.certified) } else { "—".into() })),
    ]);

    let mut l3: Vec<Span<'static>> = Vec::new();
    for (name, n) in &s.nodes {
        let (mark, col) = if n.agree { ("✓", C_GREEN) } else { ("✗", C_RED) };
        l3.push(Span::styled(format!("{name} {mark} "), Style::default().fg(col)));
        l3.push(dim(format!("{}  ", group(n.height))));
    }
    if l3.is_empty() { l3.push(dim("comparing our hash against Epsilon, happysrv, node3 at equal height…")); }

    let l4 = match s.last_mismatch.as_ref() {
        Some((node, h, ours, theirs)) => Line::from(vec![
            Span::styled("last mismatch ", Style::default().fg(C_RED)),
            dim(format!("{node} @ {}: ours {} theirs {}", group(*h), short(ours), short(theirs))),
        ]),
        None => Line::from(vec![dim("equal hash at equal height ⇒ equal header ⇒ equal state roots (exact, no race)")]),
    };

    Paragraph::new(vec![Line::from(l1), l2, Line::from(l3), l4]).block(card_block(" ◈ DATA INTEGRITY", C_NEON_CYAN))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reader_none() -> Arc<OnceLock<sigil_block_store::BlockReader>> { Arc::new(OnceLock::new()) }

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
