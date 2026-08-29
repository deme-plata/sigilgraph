// ONE-CHAIN P2a (docs/SIGIL_ONE_CHAIN_SCOPE_v0.md) — verify the MONEY chain.
//
// The mining ledger (sigil-rpcd) is THE chain: it emits a real SigilBlockHeaderV0
// per block (P1) with Bitcoin-style self-linkage `header[h].parent_hash ==
// hash(header[h-1])` (P2a). This module walks that header chain over HTTP,
// backward from the tip, verifying `precheck()` + parent linkage — the same
// contract chain_verify.rs applies to the spine, applied to the chain the money
// actually lives on. It also fetches /supply, the ledger's real emission truth
// (the TUI's old 21M/100% figure came from the spine's display path — fiction).
//
// Design: one background refresher thread (spawned lazily on first `latest()`),
// cumulative verify watermark (a once-verified suffix is never re-walked; each
// cycle only links NEW tip → old watermark), budget-capped per cycle so a deep
// header history can't stall the panel. `use super::*` reaches main.rs's
// `http_get` — the heroes.rs/wallet_ui.rs module pattern.
#![allow(clippy::needless_range_loop)]
use super::*;
use std::sync::{Mutex, OnceLock};

/// Where the ledger lives. Override with SIGIL_LEDGER_URL.
/// 2026-08-29: default moved off the retired sigil-rpcd (`:8099`, permanently
/// stopped 2026-08-17). A manually-resurrected zombie of it fed this panel two
/// days of frozen g1 numbers — supply 190,330, height 325,651, a red
/// "precheck failed" — on a chain that had since reset to g2. The braid's
/// sigil-api behind sigilgraph.org is the money truth now.
fn ledger_base() -> String {
    std::env::var("SIGIL_LEDGER_URL").ok().filter(|s| !s.is_empty())
        .unwrap_or_else(|| "https://sigilgraph.org".into())
}

#[derive(Clone, Debug, Default)]
pub struct LedgerInfo {
    /// Real minted supply in base units (from /supply — the chokepoint counter).
    pub supply_base: u128,
    /// Percent of the 21M cap minted.
    pub pct: f64,
    /// Ledger height (mining chain — THE height).
    pub height: u64,
    /// Newest header on the walkable chain.
    pub header_tip: u64,
    /// Oldest header the backward walk verified down to (cumulative watermark).
    pub verified_floor: u64,
    /// Total headers precheck+linkage-verified across all cycles.
    pub checked: u64,
    /// Set when the walk hit a linkage/precheck break (the load-bearing alarm).
    pub break_note: Option<String>,
}

static LATEST: OnceLock<Mutex<Option<LedgerInfo>>> = OnceLock::new();
static SPAWNED: OnceLock<()> = OnceLock::new();

/// Current ledger truth, if a refresh has landed. Spawns the background
/// refresher on first call; never blocks the render loop.
pub fn latest() -> Option<LedgerInfo> {
    SPAWNED.get_or_init(|| {
        thread::spawn(|| {
            let mut watermark: Option<(u64, u64, u64)> = None; // (floor, tip, checked)
            loop {
                if let Some(info) = refresh(&mut watermark) {
                    if let Ok(mut g) = LATEST.get_or_init(|| Mutex::new(None)).lock() { *g = Some(info); }
                }
                thread::sleep(Duration::from_secs(30));
            }
        });
    });
    LATEST.get_or_init(|| Mutex::new(None)).lock().ok().and_then(|g| g.clone())
}

fn get_json(path: &str) -> Option<serde_json::Value> {
    // http_get returns the RAW response incl. status line + headers — extract the
    // outermost JSON object exactly like main.rs's parse_status does.
    let body = http_get(&format!("{}{}", ledger_base(), path), Duration::from_secs(6))?;
    let (start, end) = (body.find('{')?, body.rfind('}')?);
    if end <= start { return None; }
    serde_json::from_str(&body[start..=end]).ok()
}

fn fetch_header(h: u64) -> Option<sigil_header::SigilBlockHeaderV0> {
    let v = get_json(&format!("/header?height={h}"))?;
    if v.get("found").and_then(|f| f.as_bool()) == Some(false) { return None; }
    serde_json::from_value(v).ok()
}

/// One refresh cycle: /supply + /tip, then extend the verified suffix by at most
/// `BUDGET` headers (newest-first). The watermark makes verification CUMULATIVE:
/// verified ground is never re-walked.
fn refresh(watermark: &mut Option<(u64, u64, u64)>) -> Option<LedgerInfo> {
    const BUDGET: u64 = 200;
    // sigil-api wraps in {ok,data:{…}} and names the percent `minted_pct`
    // (already a percent, like rpcd's `pct`); the legacy flat shape is kept so
    // an explicit SIGIL_LEDGER_URL at an old rpcd still parses.
    let sup = get_json("/v1/supply").or_else(|| get_json("/supply"))?;
    let sup = sup.get("data").cloned().unwrap_or(sup);
    let supply_base: u128 = sup.get("native_supply")?.as_str()?.parse().ok()?;
    let pct = sup
        .get("pct")
        .or_else(|| sup.get("minted_pct"))
        .and_then(|p| p.as_f64())
        .unwrap_or(0.0);
    let height = sup
        .get("height")
        .and_then(|h| h.as_u64())
        .or_else(|| get_json("/v1/mining/miners")?.get("data")?.get("height")?.as_u64())
        .unwrap_or(0);

    // sigil-api serves no /tip header walk — and needs none: on g2 the money
    // chain IS the braid spine, whose linkage the TUI's own chain verify
    // already proves. Supply truth stands alone; the walk below only runs
    // against a legacy rpcd that still answers /tip.
    let Some(tipj) = get_json("/tip") else {
        let mut info = LedgerInfo { supply_base, pct, height, ..Default::default() };
        if let Some((f, _t, c)) = *watermark {
            info.verified_floor = f;
            info.checked = c;
        }
        return Some(info);
    };
    let header_tip = tipj.get("header_tip").and_then(|v| v.as_u64()).unwrap_or(0);
    // The node advertises where the self-linked chain STARTS (headers below are
    // the P1-era fold-tip-parent style, or absent) — walking past it would report
    // expected history as a break. Corruption INSIDE [floor, tip] still breaks.
    let advertised_floor = tipj.get("header_floor").and_then(|v| v.as_u64()).unwrap_or(0).max(1);
    let mut info = LedgerInfo { supply_base, pct, height, header_tip, ..Default::default() };
    if header_tip == 0 {
        // pre-P1 rpcd (no headers yet) — supply truth still stands on its own
        if let Some((f, _t, c)) = *watermark { info.verified_floor = f; info.checked = c; }
        return Some(info);
    }

    // Resume state: on a fresh start (or a new tip) walk newest→oldest until we
    // meet the previous watermark, the floor of storage, a break, or the budget.
    let (mut floor, prev_tip, mut checked) = watermark.unwrap_or((header_tip + 1, 0, 0));
    let mut h = header_tip;
    let mut cur = match fetch_header(h) {
        Some(c) => c,
        None => { info.break_note = Some(format!("tip header #{h} unavailable")); return Some(info); }
    };
    if cur.precheck().is_err() { info.break_note = Some(format!("precheck failed at tip #{h}")); return Some(info); }
    let mut walked = 0u64;
    while walked < BUDGET && h > advertised_floor {
        // stitch onto the already-verified suffix: once we link down to the old
        // tip, everything below it is already proven — stop.
        if prev_tip != 0 && h <= prev_tip { break; }
        let parent = match fetch_header(h - 1) {
            Some(p) => p,
            None => break, // storage floor — normal terminator, not corruption
        };
        if parent.precheck().is_err() {
            info.break_note = Some(format!("precheck failed at #{}", h - 1));
            break;
        }
        if cur.parent_hash != parent.hash() {
            info.break_note = Some(format!("linkage break: #{h}.parent != hash(#{})", h - 1));
            break;
        }
        checked += 1; walked += 1; h -= 1; cur = parent;
    }
    floor = floor.min(h);
    *watermark = Some((floor, header_tip, checked));
    info.verified_floor = floor;
    info.checked = checked;
    Some(info)
}
