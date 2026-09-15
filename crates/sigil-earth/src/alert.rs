//! The alert rule, one instance per channel. Evaluated on the newest reading, every run:
//!   k_p99         K above the empirical p99 of the trailing three years — fires once, then
//!                 stays disarmed until K falls back below p90 (hysteresis).
//!   regime_change the K-family ladder step (stable / elevated / critical) moved.
//! Earth (K⊕, kinds `k_p99` / `k_p99_cleared` / `regime_change`) and, since 2026-09-15, the
//! biosphere (K_bio, kinds `k_bio_p99` / `k_bio_p99_cleared` / `bio_regime_change`) share the
//! rule, the file and the rails; each keeps its own hysteresis state. One row per (kind, date),
//! never twice. Delivery: the SSE stream and alerts.jsonl always; the fluxc webhook rail
//! (`fluxc webhook trigger sigil_earth_gauge …`) and a Buzz post when asked.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
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
/// A channel with no state file yet is ARMED. (2026-09-15: the derived `Default` gave
/// `armed: false`, so a brand-new channel — the biosphere's first day — could never fire its
/// p99 alert until K had first dipped below p90. The serde default already said `true`; the
/// two now agree.)
impl Default for State {
    fn default() -> Self {
        State { last_regime: None, armed: true, last_date: None, last_k: None }
    }
}
fn earth_default() -> Channel {
    Channel::Earth
}

/// The per-channel hysteresis state file under the state dir.
pub fn state_path(state_dir: &Path, ch: Channel) -> std::path::PathBuf {
    state_dir.join(ch.state_file())
}

/// Which gauge an alert row belongs to. Old rows (before 2026-09-15) carry no field → Earth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    Earth,
    Bio,
}

impl Channel {
    pub fn symbol(self) -> &'static str {
        match self { Channel::Earth => "K⊕", Channel::Bio => "K_bio" }
    }
    fn kinds(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Channel::Earth => ("k_p99", "k_p99_cleared", "regime_change"),
            Channel::Bio => ("k_bio_p99", "k_bio_p99_cleared", "bio_regime_change"),
        }
    }
    fn source(self, injected: bool) -> &'static str {
        match (self, injected) {
            (_, true) => "injected (dry run)",
            (Channel::Earth, false) => "IERS finals2000A, observed row",
            (Channel::Bio, false) => "NOAA GML Mauna Loa daily CO₂, 7-day residual",
        }
    }
    fn state_file(self) -> &'static str {
        match self { Channel::Earth => "alerts-state.json", Channel::Bio => "alerts-bio-state.json" }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertRow {
    pub ts: String,
    pub unix_ms: u64,
    #[serde(default = "earth_default")]
    pub channel: Channel,
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

/// Decide what fires for today's Earth reading. Pure: returns the rows and the next state.
pub fn evaluate(k: f64, date: &str, p99: f64, p90: f64, st: &State, existing: &[AlertRow], injected: bool) -> (Vec<AlertRow>, State) {
    evaluate_channel(Channel::Earth, k, date, p99, p90, st, existing, injected)
}

/// The rule for any channel. Pure: returns the rows and the next state.
#[allow(clippy::too_many_arguments)]
pub fn evaluate_channel(ch: Channel, k: f64, date: &str, p99: f64, p90: f64, st: &State, existing: &[AlertRow], injected: bool) -> (Vec<AlertRow>, State) {
    let regime = crate::regime(k).to_string();
    let mut next = st.clone();
    let mut rows = Vec::new();
    let source = ch.source(injected).to_string();
    let (kind_p99, kind_cleared, kind_regime) = ch.kinds();
    let sym = ch.symbol();
    let seen = |kind: &str| existing.iter().any(|r| r.kind == kind && r.date == date);
    let mk = |kind: &str, msg: String, prev: Option<String>| AlertRow {
        ts: crate::time::iso_now(),
        unix_ms: crate::time::now_ms(),
        channel: ch,
        kind: kind.into(),
        date: date.into(),
        k_resid: k,
        p99,
        p90,
        regime: regime.clone(),
        prev_regime: prev,
        msg,
        source: source.clone(),
        fortolkning: match (ch, kind) {
            (Channel::Earth, "k_p99") => "K⊕ crossed the empirical p99 of its own three years. Look at the fluids panel first: if the atmosphere or ocean estimate also jumped, it is weather; if not, it is the core or an IERS revision. IERS rapid values are revised for weeks, so this can be revised away.".into(),
            (Channel::Earth, "k_p99_cleared") => "K⊕ is back below p90; the alert re-arms. Nothing to do.".into(),
            (Channel::Earth, _) => "The K-family ladder moved a step (stable / elevated / critical). Elevated usually means the day length after a storm season or an ENSO shift; critical means the model does not know what happened.".into(),
            (Channel::Bio, "k_bio_p99") => "The biosphere's breathing crossed the empirical p99 of its own three years: Mauna Loa's last week sits further from the fitted seasonal cycle than 99 % of weeks. Check first whether the residual is one week or one season — a single week is usually the instrument or an air-mass switch at the station; a season is a drought, a fire year or an El Niño. NOAA revises recent days, so this can be revised away.".into(),
            (Channel::Bio, "k_bio_p99_cleared") => "K_bio is back below p90; the alert re-arms. Nothing to do.".into(),
            (Channel::Bio, _) => "The biosphere's K-family ladder moved a step (stable / elevated / critical). Elevated: an early spring, a warm autumn, a fire season, or a Mauna Loa instrument week. Critical: the cycles do not know what is happening — write 'the biosphere is off its rhythm', not 'CO₂ is high'.".into(),
        },
        fortolkning_da: match (ch, kind) {
            (Channel::Earth, "k_p99") => "K⊕ krydsede sin egen empiriske p99 over tre år. Se på væskepanelet først: hvis atmosfære- eller hav-estimatet også sprang, er det vejr; hvis ikke, er det kernen eller en IERS-revision. IERS' hurtige værdier revideres i uger, så alarmen kan blive revideret væk.".into(),
            (Channel::Earth, "k_p99_cleared") => "K⊕ er tilbage under p90; alarmen genaktiveres. Intet at gøre.".into(),
            (Channel::Earth, _) => "K-familiens stige rykkede et trin (stabil / forhøjet / kritisk). Forhøjet betyder som regel døgnlængden efter en stormsæson eller et ENSO-skift; kritisk betyder, at modellen ikke ved, hvad der skete.".into(),
            (Channel::Bio, "k_bio_p99") => "Biosfærens vejrtrækning krydsede sin egen empiriske p99 over tre år: Mauna Loas seneste uge ligger længere fra den tilpassede sæsoncyklus end 99 % af ugerne. Tjek først om residualet er én uge eller én sæson — én uge er som regel instrumentet eller et luftmasseskift ved stationen; en sæson er tørke, et brandår eller El Niño. NOAA reviderer de seneste dage, så alarmen kan blive revideret væk.".into(),
            (Channel::Bio, "k_bio_p99_cleared") => "K_bio er tilbage under p90; alarmen genaktiveres. Intet at gøre.".into(),
            (Channel::Bio, _) => "Biosfærens K-stige rykkede et trin (stabil / forhøjet / kritisk). Forhøjet: et tidligt forår, et varmt efterår, en brandsæson eller en instrumentuge på Mauna Loa. Kritisk: cyklusserne ved ikke, hvad der sker — skriv \"biosfæren er ude af rytme\", ikke \"CO₂ er høj\".".into(),
        },
    };
    let what = match ch { Channel::Earth => "something unexplained happened to Earth's clock", Channel::Bio => "the biosphere is off its rhythm" };
    if k > p99 && next.armed {
        next.armed = false;
        if !seen(kind_p99) {
            rows.push(mk(kind_p99, format!("{sym} {k:.2} above the p99 line {p99:.2} on {date} — {what}"), st.last_regime.clone()));
        }
    } else if k < p90 && !next.armed {
        next.armed = true;
        if !seen(kind_cleared) {
            rows.push(mk(kind_cleared, format!("{sym} {k:.2} back below p90 {p90:.2} on {date}"), st.last_regime.clone()));
        }
    }
    if let Some(prev) = &st.last_regime {
        if *prev != regime && !seen(kind_regime) {
            rows.push(mk(kind_regime, format!("{sym} ladder moved {prev} → {regime} ({k:.2}) on {date}"), Some(prev.clone())));
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
        let text = match row.channel {
            Channel::Earth => format!("🌍 Kristensen Earth gauge — {} · K⊕ {:.2} ({}) · https://sigilgraph.org/datacenter.html", row.msg, row.k_resid, row.regime),
            Channel::Bio => format!("🌱 Kristensen biosphere gauge — {} · K_bio {:.2} ({}) · https://sigilgraph.org/kristensen-board.html", row.msg, row.k_resid, row.regime),
        };
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

    #[test]
    fn the_bio_channel_has_its_own_kinds_state_and_words_and_never_collides_with_earth() {
        let st = State { last_regime: Some("stable".into()), armed: true, last_date: None, last_k: None };
        let (rows, s1) = evaluate_channel(Channel::Bio, 3.0, "2026-09-13", 2.68, 1.64, &st, &[], false);
        assert_eq!(rows.iter().map(|r| r.kind.as_str()).collect::<Vec<_>>(), vec!["k_bio_p99", "bio_regime_change"]);
        assert!(rows.iter().all(|r| r.channel == Channel::Bio));
        assert!(rows[0].msg.starts_with("K_bio 3.00 above the p99 line 2.68"));
        assert!(rows[0].msg.contains("off its rhythm"));
        assert!(rows[0].fortolkning.contains("one week or one season"));
        assert!(rows[0].fortolkning_da.contains("vejrtrækning"));
        assert!(rows[1].fortolkning_da.contains("ude af rytme"));
        assert_eq!(rows[0].source, "NOAA GML Mauna Loa daily CO₂, 7-day residual");
        assert!(!s1.armed);
        // an Earth row on the same date does not count as "seen" for the bio kinds, and vice versa
        let (again, _) = evaluate_channel(Channel::Earth, 3.5, "2026-09-13", 3.1, 2.5, &State::default(), &rows, false);
        assert_eq!(again.iter().map(|r| r.kind.as_str()).collect::<Vec<_>>(), vec!["k_p99"]);
        // old rows without a channel field deserialize as Earth
        let legacy: AlertRow = serde_json::from_str(r#"{"ts":"t","unix_ms":1,"kind":"k_p99","date":"2026-09-09","k_resid":3.5,"p99":3.1,"p90":2.5,"regime":"critical","prev_regime":null,"msg":"m","source":"s"}"#).unwrap();
        assert_eq!(legacy.channel, Channel::Earth);
        assert_ne!(state_path(Path::new("/s"), Channel::Bio), state_path(Path::new("/s"), Channel::Earth));
    }
}
