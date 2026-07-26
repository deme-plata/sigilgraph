// ONE-CHAIN P2 — the block_sync re-point, step 1 (docs/SIGIL_ONE_CHAIN_SCOPE_v0.md).
//
// A persistent, VERIFIED local copy of the LEDGER header chain — the chain the
// money lives on. Complements ledger_verify.rs (budget-capped backward walk,
// no persistence): this module pulls `/headers` batches forward from the header
// floor, verifies `precheck()` + `parent_hash == hash(prev)` on every header,
// and lands them in a dedicated flux-db (`sigil-ledger-headers.db`, keys
// `lh/{height:020}` + `lh/tip`). Once synced it cross-checks the tip against
// the SIGNED public feed (`sigil-ledger-tip.json`) — local chain vs the
// tip-proof oracle, end to end.
//
// DELIBERATELY additive: the spine engine (mod.rs) is untouched except for one
// `ensure_running()` call. The store is separate because ledger heights (~110k)
// live inside the spine's height range (31.5M) — sharing the spine's keyspace
// would collide. The canonical [V]erify flip onto this store is the P2 gate.
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use sigil_header::SigilBlockHeaderV0;

/// Where the ledger lives (same default as ledger_verify.rs / wallet / explorer).
fn ledger_base() -> String {
    std::env::var("SIGIL_LEDGER_URL").ok().filter(|s| !s.is_empty())
        .unwrap_or_else(|| "http://sigilgraph.quillon.xyz:8099".into())
}

/// The signed tip-proof feed published by sigil-ledger-tip.service.
fn tip_feed_url() -> String {
    std::env::var("SIGIL_LEDGER_TIP_URL").ok().filter(|s| !s.is_empty())
        .unwrap_or_else(|| "https://quillon.xyz/sigil-ledger-tip.json".into())
}

/// Dedicated store beside the spine block store (SIGIL_LEDGER_DB overrides).
fn db_path() -> String {
    if let Ok(p) = std::env::var("SIGIL_LEDGER_DB") {
        if !p.trim().is_empty() { return p; }
    }
    crate::sigil_top_db_path().replace("sigil-top-blocks.db", "sigil-ledger-headers.db")
}

fn key(h: u64) -> Vec<u8> { format!("lh/{h:020}").into_bytes() }

/// Pooled TLS-capable GET (the engine's shared client). NOT crate::http_get —
/// that helper is http-only AND returns the raw response with HTTP framing
/// still attached, which silently fails strict JSON parsing.
fn get_text(url: &str) -> Option<String> {
    super::HTTP_BLOCKING.get(url).send().ok()?.error_for_status().ok()?.text().ok()
}

#[derive(Clone, Debug, Default)]
pub struct LedgerSyncInfo {
    /// Oldest header the ledger has (headers exist since the P1 flip, not 0).
    pub floor: u64,
    /// Remote header tip at the last cycle.
    pub tip_remote: u64,
    /// Newest header verified + stored locally.
    pub tip_synced: u64,
    /// Headers verified+stored across the store's lifetime (tip−floor+1 once caught up).
    pub verified: u64,
    /// Signed tip-proof feed cross-check: Some(true)=wallet root matches the
    /// local chain at the feed's height; Some(false) is the load-bearing alarm.
    pub oracle_match: Option<bool>,
    /// Set on precheck/linkage failure — sync halts rather than store a lie.
    pub break_note: Option<String>,
    pub last_cycle_ms: u64,
}

static LATEST: OnceLock<Mutex<Option<LedgerSyncInfo>>> = OnceLock::new();
static SPAWNED: OnceLock<()> = OnceLock::new();

/// Last published sync state, if a cycle has run.
pub fn latest() -> Option<LedgerSyncInfo> {
    LATEST.get_or_init(|| Mutex::new(None)).lock().ok().and_then(|g| g.clone())
}

fn publish(info: LedgerSyncInfo) {
    if let Ok(mut g) = LATEST.get_or_init(|| Mutex::new(None)).lock() { *g = Some(info); }
}

/// Start the background sync once. Cheap to call from anywhere; the engine's
/// launch path calls it so a running node always mirrors the ledger headers.
pub fn ensure_running() {
    SPAWNED.get_or_init(|| {
        std::thread::spawn(|| {
            let db = match flux_db::Database::open(db_path()) {
                Ok(d) => d,
                Err(e) => {
                    publish(LedgerSyncInfo { break_note: Some(format!("ledger db open: {e}")), ..Default::default() });
                    return;
                }
            };
            loop {
                sync_cycle(&db);
                std::thread::sleep(Duration::from_secs(5));
            }
        });
    });
}

fn read_u64(db: &flux_db::Database, k: &[u8]) -> Option<u64> {
    db.get(k).ok().flatten()
        .and_then(|b| String::from_utf8(b).ok())
        .and_then(|s| s.parse().ok())
}

fn load_header(db: &flux_db::Database, h: u64) -> Option<SigilBlockHeaderV0> {
    db.get(&key(h)).ok().flatten().and_then(|b| serde_json::from_slice(&b).ok())
}

/// One cycle: pull forward from the local tip in 512-header batches until caught
/// up (or the cycle budget is spent), then cross-check the signed tip feed.
fn sync_cycle(db: &flux_db::Database) {
    let t0 = Instant::now();
    let mut info = latest().unwrap_or_default();
    info.break_note = None;
    // A verified store never re-walks: resume above the stored tip.
    let mut local_tip = read_u64(db, b"lh/tip");
    let mut prev: Option<SigilBlockHeaderV0> = local_tip.and_then(|t| load_header(db, t));
    // ~16 batches/cycle keeps a cold catch-up (~110k headers) under ~15 cycles
    // without ever hogging the node's HTTP budget.
    for _ in 0..16 {
        let url = match local_tip {
            Some(t) => format!("{}/headers?from={}&count=512", ledger_base(), t + 1),
            None => format!("{}/headers?count=512", ledger_base()), // server starts at its floor
        };
        let Some(body) = get_text(&url) else { break };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) else { break };
        if let Some(f) = v.get("floor").and_then(|x| x.as_u64()) { info.floor = f; }
        if let Some(t) = v.get("tip").and_then(|x| x.as_u64()) { info.tip_remote = t; }
        let Some(arr) = v.get("headers").and_then(|x| x.as_array()) else { break };
        if arr.is_empty() { break } // caught up (or pre-P1 rpcd with no headers)
        let mut batch: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(arr.len());
        for hv in arr {
            let Ok(h) = serde_json::from_value::<SigilBlockHeaderV0>(hv.clone()) else {
                info.break_note = Some("undecodable header in batch".into());
                break;
            };
            if h.precheck().is_err() {
                info.break_note = Some(format!("precheck failed at #{}", h.height));
                break;
            }
            if let Some(p) = &prev {
                if h.parent_hash != p.hash() {
                    info.break_note = Some(format!("linkage break: #{}.parent != hash(#{})", h.height, p.height));
                    break;
                }
            }
            // first header (the floor) anchors the chain — nothing below to link to
            batch.push((key(h.height), hv.to_string().into_bytes()));
            prev = Some(h);
        }
        if !batch.is_empty() {
            let new_tip = prev.as_ref().map(|p| p.height).unwrap_or(0);
            if let Err(e) = db.put_many(&batch) {
                info.break_note = Some(format!("store write: {e}"));
            } else if let Err(e) = db.put(b"lh/tip", new_tip.to_string().as_bytes()) {
                info.break_note = Some(format!("tip write: {e}"));
            } else {
                local_tip = Some(new_tip);
                info.tip_synced = new_tip;
                info.verified += batch.len() as u64;
            }
        }
        if info.break_note.is_some() { break }
    }
    // Caught up → bind the local chain to the SIGNED tip-proof feed: the feed's
    // wallet root must equal the locally-verified header's at the same height.
    if info.break_note.is_none() && info.tip_synced > 0 && info.tip_synced >= info.tip_remote {
        info.oracle_match = check_tip_feed(db);
    }
    info.last_cycle_ms = t0.elapsed().as_millis() as u64;
    publish(info);
}

/// Compare the signed feed's {height, wallet_state_root} against the local store.
/// None = feed unreachable / height not yet local (both benign), Some(false) = alarm.
fn check_tip_feed(db: &flux_db::Database) -> Option<bool> {
    let body = get_text(&tip_feed_url())?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    let h = v.get("height")?.as_u64()?;
    let feed_root: Vec<u8> = v.get("roots")?.get("wallet_state_root")?
        .as_array()?.iter().filter_map(|x| x.as_u64().map(|b| b as u8)).collect();
    let local = load_header(db, h)?;
    Some(local.wallet_state_root.as_slice() == feed_root.as_slice())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ledger_db_path_is_sibling_of_block_store() {
        std::env::remove_var("SIGIL_LEDGER_DB");
        let p = db_path();
        assert!(p.ends_with("sigil-ledger-headers.db"), "{p}");
    }

    #[test]
    fn key_is_fixed_width_sortable() {
        assert_eq!(key(7), b"lh/00000000000000000007".to_vec());
        assert!(key(9) < key(10)); // lexicographic == numeric under fixed width
    }

    /// P2 GATE (run explicitly on the dev box — needs a reachable rpcd):
    ///   SIGIL_LEDGER_URL=http://127.0.0.1:8099 <test-bin> --ignored live_gate
    /// Syncs the ENTIRE ledger header chain floor→tip into a scratch store with
    /// full precheck+linkage verification, then requires the signed tip-proof
    /// feed to match the local chain. This is "[V]erify walks the LEDGER green".
    #[test]
    #[ignore]
    fn live_gate_full_ledger_sync() {
        let scratch = std::env::temp_dir().join(format!("ledger-gate-{}.db", std::process::id()));
        let db = flux_db::Database::open(&scratch).expect("scratch db");
        let deadline = Instant::now() + Duration::from_secs(600);
        loop {
            sync_cycle(&db);
            let info = latest().expect("cycle published");
            assert!(info.break_note.is_none(), "sync broke: {:?}", info.break_note);
            if info.tip_synced > 0 && info.tip_synced >= info.tip_remote { break }
            assert!(Instant::now() < deadline, "gate timeout at {info:?}");
        }
        let info = latest().unwrap();
        // every header from floor to tip is present + the chain linked end to end
        assert!(load_header(&db, info.floor).is_some(), "floor header missing");
        assert!(load_header(&db, info.tip_synced).is_some(), "tip header missing");
        assert_eq!(info.verified, info.tip_synced - info.floor + 1, "verified count != span");
        assert_eq!(info.oracle_match, Some(true), "signed tip feed mismatch");
        let _ = std::fs::remove_dir_all(&scratch);
    }
}
