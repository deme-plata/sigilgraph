//! The alert rule. Evaluated on the newest observed row with LOD, every run:
//!   k_p99         K_resid above the empirical p99 of the trailing three years — fires once, then
//!                 stays disarmed until K_resid falls back below p90 (hysteresis).
//!   regime_change the K-family ladder step (stable / elevated / critical) moved.
//! One row per (kind, date), never twice. Delivery: the SSE stream and alerts.jsonl always;
//! the fluxc webhook rail (`fluxc webhook trigger sigil_earth_gauge …`) and a Buzz post when asked.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct State {
    pub last_regime: Option<String>,
    #[serde(default = "armed_default")]
    pub armed: bool,
    pub last_date: Option<String>,
    pub last_k: Option<f64>,
}
fn armed_default() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertRow {
    pub ts: String,
    pub unix_ms: u64,
    pub kind: String,
    pub date: String,
    pub k_resid: f64,
    pub p99: f64,
    pub p90: f64,
    pub regime: String,
    pub prev_regime: Option<String>,
    pub msg: String,
    pub source: String,
    /// The interpretation travels with the event (SSE, webhook, file): what to look at first, and a Danish line.
    #[serde(default)]
    pub fortolkning: String,
    #[serde(default)]
    pub fortolkning_da: String,
}

pub fn load_state(path: &Path) -> State {
    std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

pub fn save_state(path: &Path, st: &State) -> Result<()> {
    crate::fetch::write_atomic(path, serde_json::to_string_pretty(st)?.as_bytes())
}

pub fn read_rows(path: &Path) -> Vec<AlertRow> {
    std::fs::read_to_string(path)
        .map(|s| s.lines().filter_map(|l| serde_json::from_str(l).ok()).collect())
        .unwrap_or_default()
}

/// Decide what fires for today's reading. Pure: returns the rows and the next state.
pub fn evaluate(k: f64, date: &str, p99: f64, p90: f64, st: &State, existing: &[AlertRow], injected: bool) -> (Vec<AlertRow>, State) {
    let regime = crate::regime(k).to_string();
    let mut next = st.clone();
    let mut rows = Vec::new();
    let source = if injected { "injected (dry run)" } else { "IERS finals2000A, observed row" }.to_string();
    let seen = |kind: &str| existing.iter().any(|r| r.kind == kind && r.date == date);
    let mk = |kind: &str, msg: String, prev: Option<String>| AlertRow {
        ts: crate::time::iso_now(),
        unix_ms: crate::time::now_ms(),
        kind: kind.into(),
        date: date.into(),
        k_resid: k,
        p99,
        p90,
        regime: regime.clone(),
        prev_regime: prev,
        msg,
        source: source.clone(),
        fortolkning: match kind {
            "k_p99" => "K⊕ crossed the empirical p99 of its own three years. Look at the fluids panel first: if the atmosphere or ocean estimate also jumped, it is weather; if not, it is the core or an IERS revision. IERS rapid values are revised for weeks, so this can be revised away.".into(),
            "k_p99_cleared" => "K⊕ is back below p90; the alert re-arms. Nothing to do.".into(),
            _ => "The K-family ladder moved a step (stable / elevated / critical). Elevated usually means the day length after a storm season or an ENSO shift; critical means the model does not know what happened.".into(),
        },
        fortolkning_da: match kind {
            "k_p99" => "K⊕ krydsede sin egen empiriske p99 over tre år. Se på væskepanelet først: hvis atmosfære- eller hav-estimatet også sprang, er det vejr; hvis ikke, er det kernen eller en IERS-revision. IERS' hurtige værdier revideres i uger, så alarmen kan blive revideret væk.".into(),
            "k_p99_cleared" => "K⊕ er tilbage under p90; alarmen genaktiveres. Intet at gøre.".into(),
            _ => "K-familiens stige rykkede et trin (stabil / forhøjet / kritisk). Forhøjet betyder som regel døgnlængden efter en stormsæson eller et ENSO-skift; kritisk betyder, at modellen ikke ved, hvad der skete.".into(),
        },
    };
    if k > p99 && next.armed {
        next.armed = false;
        if !seen("k_p99") {
            rows.push(mk("k_p99", format!("K⊕ {k:.2} above the p99 line {p99:.2} on {date} — something unexplained happened to Earth's clock"), st.last_regime.clone()));
        }
    } else if k < p90 && !next.armed {
        next.armed = true;
        if !seen("k_p99_cleared") {
            rows.push(mk("k_p99_cleared", format!("K⊕ {k:.2} back below p90 {p90:.2} on {date}"), st.last_regime.clone()));
        }
    }
    if let Some(prev) = &st.last_regime {
        if *prev != regime && !seen("regime_change") {
            rows.push(mk("regime_change", format!("K⊕ ladder moved {prev} → {regime} ({k:.2}) on {date}"), Some(prev.clone())));
        }
    }
    next.last_regime = Some(regime);
    next.last_date = Some(date.into());
    next.last_k = Some(k);
    (rows, next)
}

pub fn append(path: &Path, row: &AlertRow) -> Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "{}", serde_json::to_string(row)?)?;
    f.sync_all()?;
    Ok(())
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Dispatch {
    pub webhook: bool,
    pub buzz: bool,
}

/// Deliver one row: the fluxc webhook rail (one HMAC implementation on this box) and/or Buzz.
pub fn dispatch(row: &AlertRow, how: Dispatch) -> Value {
    let mut out = json!({});
    if how.webhook {
        let payload = serde_json::to_string(row).unwrap_or_default();
        let fluxc = std::env::var("FLUXC_BIN").unwrap_or_else(|_| "fluxc".into());
        let res = std::process::Command::new(&fluxc).args(["webhook", "trigger", "sigil_earth_gauge", &payload]).output();
        out["webhook"] = match res {
            Ok(o) => json!({"ok": o.status.success(), "stdout": String::from_utf8_lossy(&o.stdout).trim(), "stderr": String::from_utf8_lossy(&o.stderr).trim()}),
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        };
    }
    if how.buzz {
        let text = format!("🌍 Kristensen Earth gauge — {} · K⊕ {:.2} ({}) · https://sigilgraph.org/datacenter.html", row.msg, row.k_resid, row.regime);
        out["buzz"] = match crate::buzz::post(&text, "sigil") {
            Ok(v) => v,
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        };
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn p99_fires_once_and_rearms_below_p90_and_regime_changes_are_reported() {
        let st = State { last_regime: Some("elevated".into()), armed: true, last_date: None, last_k: None };
        let (rows, s1) = evaluate(3.5, "2026-09-09", 3.1, 2.5, &st, &[], false);
        assert_eq!(rows.iter().map(|r| r.kind.as_str()).collect::<Vec<_>>(), vec!["k_p99", "regime_change"]);
        assert!(!s1.armed);
        // still high next day: nothing new
        let (rows2, s2) = evaluate(3.4, "2026-09-10", 3.1, 2.5, &s1, &rows, false);
        assert!(rows2.is_empty() && !s2.armed);
        // falls between p90 and p99: still disarmed, regime moves back
        let (rows3, s3) = evaluate(2.8, "2026-09-11", 3.1, 2.5, &s2, &rows, false);
        assert_eq!(rows3.len(), 1);
        assert_eq!(rows3[0].kind, "regime_change");
        assert!(!s3.armed);
        // below p90: cleared and re-armed
        let (rows4, s4) = evaluate(2.0, "2026-09-12", 3.1, 2.5, &s3, &rows, false);
        assert_eq!(rows4[0].kind, "k_p99_cleared");
        assert!(s4.armed);
        // same (kind, date) never twice: both kinds already exist for 2026-09-09
        let (rows5, _) = evaluate(3.5, "2026-09-09", 3.1, 2.5, &s4, &rows, false);
        assert!(rows5.is_empty());
        // a new date fires both again
        let (rows6, _) = evaluate(3.5, "2026-09-13", 3.1, 2.5, &s4, &rows, false);
        assert_eq!(rows6.iter().map(|r| r.kind.as_str()).collect::<Vec<_>>(), vec!["k_p99", "regime_change"]);
    }
}
