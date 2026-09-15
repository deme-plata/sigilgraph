//! `sigil-earth push` — the feeder arm: commit the latest Kristensen gauge readings on-chain.
//!
//! Viktor, 2026-09-15: "lav sigil-earth push armen". After each publication the feeder wallet
//! signs a `GaugePush` per feed (the `sigil-rpc/v1|gauge_push|…` message the node's
//! `/v1/gauge/push_wallet` verifies) and POSTs it. The readings come from the SAME `latest.json`
//! the attest chain signed, and each push carries that file's BLAKE3 — so `/v1/gauge` and
//! `/v1/earth/attest` name the same bytes.
//!
//! Inert until TWO operator acts, by design (nothing here forces either):
//!   1. the master delegates this wallet as the oracle feeder (`OracleDelegate`), and
//!   2. the master commits `sigil_oracle::GAUGE_LIVE_HEIGHT` (the gate ships DORMANT).
//! Before both, every push is refused at apply and this arm just logs the refusal — a dry run
//! against the live node with no state change. `--dry-run` stops before the POST entirely.
//!
//! The feeder seed (`--seed-file`, a 64-hex line) is read to sign and is NEVER printed —
//! same discipline as `fluxc sigil-attest`.

use crate::fetch::write_atomic;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Node base for the money route (the gate strips accept-encoding, so either works). Default
/// is the local node; the daemon points at `https://sigilgraph.org`.
pub const DEFAULT_NODE: &str = "http://127.0.0.1:18181";

pub struct Opts {
    pub data: PathBuf,
    pub node: String,
    pub seed_file: PathBuf,
    pub fee: u128,
    pub dry_run: bool,
    pub state: PathBuf,
}

/// A feed to push: (canonical name, value as a decimal, the reading's YYYYMMDD).
struct Reading {
    feed: &'static str,
    value: f64,
    date: u32,
}

fn ymd(date_str: &str) -> Option<u32> {
    // "2026-09-14" -> 20260914
    let b: Vec<&str> = date_str.split('-').collect();
    if b.len() != 3 {
        return None;
    }
    let (y, m, d) = (b[0].parse::<u32>().ok()?, b[1].parse::<u32>().ok()?, b[2].parse::<u32>().ok()?);
    Some(y * 10_000 + m * 100 + d)
}

fn f64_at(v: &Value, ptr: &str) -> Option<f64> {
    v.pointer(ptr).and_then(|x| x.as_f64())
}

/// The readings a `latest.json` yields. `earth.*` from the rotation gauge, `bio.*` from the
/// biosphere channel; a channel that is absent (a NOAA outage publishes `bio.error`) is skipped,
/// never pushed as zero.
fn readings(latest: &Value) -> Vec<Reading> {
    let mut out = Vec::new();
    let earth_date = latest.pointer("/today/date").and_then(|v| v.as_str()).and_then(ymd);
    if let (Some(k), Some(d)) = (f64_at(latest, "/today/k_resid"), earth_date) {
        out.push(Reading { feed: "earth.k_resid", value: k, date: d });
    }
    if let (Some(p99), Some(d)) = (f64_at(latest, "/ladder/k_resid/p99"), earth_date) {
        out.push(Reading { feed: "earth.k_p99", value: p99, date: d });
    }
    let bio_date = latest.pointer("/bio/today/date").and_then(|v| v.as_str()).and_then(ymd);
    if let Some(d) = bio_date {
        if let Some(k) = f64_at(latest, "/bio/today/k_bio") {
            out.push(Reading { feed: "bio.k_bio", value: k, date: d });
        }
        if let Some(r) = f64_at(latest, "/bio/breathing/rate_pgc_per_day") {
            out.push(Reading { feed: "bio.breathing_pgc_day", value: r, date: d });
        }
        if let Some(ppm) = f64_at(latest, "/bio/today/ppm") {
            out.push(Reading { feed: "bio.ppm", value: ppm, date: d });
        }
        if let Some(ops) = f64_at(latest, "/bio/lloyd/ops_atp_per_s") {
            if ops > 0.0 {
                // the ops count is ~1e33; a plain ×1e6 would overflow the human read. Commit its
                // base-10 log, so `bio.ops_atp_log10 ≈ 33.2` — the informative, stable figure.
                out.push(Reading { feed: "bio.ops_atp_log10", value: ops.log10(), date: d });
            }
        }
    }
    out
}

/// value × 1e6, rounded, as an i128 decimal string (the wire form GaugePush takes).
fn value_e6(v: f64) -> i128 {
    (v * 1_000_000.0).round() as i128
}

/// Read the feeder's ed25519 signing key from a 64-hex seed file. Never logged.
fn load_key(path: &Path) -> Result<(ed25519_dalek::SigningKey, [u8; 32])> {
    let text = std::fs::read_to_string(path).map_err(|e| anyhow!("read seed {}: {e}", path.display()))?;
    let hexs = text.trim();
    let mut seed = [0u8; 32];
    hex::decode_to_slice(hexs.trim_start_matches("0x"), &mut seed).map_err(|_| anyhow!("seed file must be one 64-hex line"))?;
    let sk = ed25519_dalek::SigningKey::from_bytes(&seed);
    let addr = sk.verifying_key().to_bytes();
    Ok((sk, addr))
}

fn sign(sk: &ed25519_dalek::SigningKey, msg: &str) -> String {
    use ed25519_dalek::Signer;
    hex::encode(sk.sign(msg.as_bytes()).to_bytes())
}

/// Monotone per-feed nonce store (client-chosen strictly-increasing, per the RPC watermark).
fn next_nonce(state: &Path, feed: &str) -> u64 {
    let mut v: Value = std::fs::read_to_string(state).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_else(|| json!({}));
    let base = crate::time::now_ms();
    let n = v.get(feed).and_then(|x| x.as_u64()).map(|prev| prev + 1).unwrap_or(base).max(base);
    v[feed] = json!(n);
    let _ = write_atomic(state, serde_json::to_vec(&v).unwrap_or_default().as_slice());
    n
}

pub fn run(o: &Opts) -> Result<Value> {
    let latest: Value = serde_json::from_slice(&std::fs::read(o.data.join("latest.json")).map_err(|e| anyhow!("read latest.json: {e}"))?)?;
    let attest_blake3 = latest.pointer("/attest_last/blake3").and_then(|v| v.as_str()).map(String::from)
        .or_else(|| {
            // fall back to the newest attest row on disk
            crate::attest::read_chain(&crate::attest::chain_path(&o.data)).iter().rev().find(|r| r.kind == "attest").map(|r| r.blake3.clone())
        })
        .ok_or_else(|| anyhow!("no attest digest to bind the readings to"))?;
    let (sk, authority) = load_key(&o.seed_file)?;
    let authority_hex = hex::encode(authority);
    let rs = readings(&latest);
    if rs.is_empty() {
        return Err(anyhow!("latest.json yielded no readings (both channels absent?)"));
    }
    let agent = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(30)).user_agent(crate::UA).build();
    let mut results = Vec::new();
    let mut ok = 0usize;
    for r in &rs {
        let ve = value_e6(r.value);
        let nonce = if o.dry_run { 0 } else { next_nonce(&o.state, r.feed) };
        let msg = format!("sigil-rpc/v1|gauge_push|{authority_hex}|{}|{ve}|{}|{attest_blake3}|{}|nonce={nonce}", r.feed, r.date, o.fee);
        let sig = sign(&sk, &msg);
        let body = json!({
            "authority": authority_hex, "feed": r.feed, "value_e6": ve.to_string(),
            "reading_date": r.date, "attest_blake3": attest_blake3, "fee": o.fee as u64, "sig": sig, "req_nonce": nonce,
        });
        if o.dry_run {
            results.push(json!({"feed": r.feed, "value": r.value, "value_e6": ve.to_string(), "reading_date": r.date, "dry_run": true}));
            continue;
        }
        let url = format!("{}/v1/gauge/push_wallet", o.node.trim_end_matches('/'));
        let resp = agent.post(&url).set("Content-Type", "application/json").send_string(&body.to_string());
        let entry = match resp {
            Ok(r2) => {
                let body = r2.into_string().unwrap_or_default();
                let j: Value = serde_json::from_str(&body).unwrap_or_else(|_| json!({"ok": false, "error": "non-json response", "raw": body}));
                if j.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                    ok += 1;
                }
                json!({"feed": r.feed, "value": r.value, "value_e6": ve.to_string(), "reading_date": r.date, "resp": j})
            }
            Err(ureq::Error::Status(code, r2)) => json!({"feed": r.feed, "http": code, "error": r2.into_string().unwrap_or_default()}),
            Err(e) => json!({"feed": r.feed, "error": e.to_string()}),
        };
        results.push(entry);
    }
    Ok(json!({
        "ok": o.dry_run || ok == rs.len(),
        "dry_run": o.dry_run, "authority": authority_hex, "node": o.node, "attest_blake3": attest_blake3,
        "pushed": if o.dry_run { 0 } else { ok }, "feeds": rs.len(), "results": results,
        "note": "GaugePush is refused until the master delegates this wallet as the oracle feeder AND commits GAUGE_LIVE_HEIGHT; until then each result carries the node's refusal and nothing changes on chain",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ymd_and_value_e6_round_the_wire_forms() {
        assert_eq!(ymd("2026-09-14"), Some(20260914));
        assert_eq!(ymd("bad"), None);
        assert_eq!(value_e6(2.8089740904860365), 2_808_974);
        assert_eq!(value_e6(-0.04940901337511494), -49_409, "the breathing rate is negative and survives");
        assert_eq!(value_e6(33.20263), 33_202_630);
    }

    #[test]
    fn readings_map_every_present_channel_and_skip_an_absent_one() {
        let latest = json!({
            "today": {"date": "2026-09-13", "k_resid": 2.808974},
            "ladder": {"k_resid": {"p99": 3.127980}},
            "bio": {"today": {"date": "2026-09-14", "k_bio": 1.946316, "ppm": 425.57},
                    "breathing": {"rate_pgc_per_day": -0.049409},
                    "lloyd": {"ops_atp_per_s": 1.5943131678e33}},
        });
        let rs = readings(&latest);
        let names: Vec<&str> = rs.iter().map(|r| r.feed).collect();
        assert_eq!(names, ["earth.k_resid", "earth.k_p99", "bio.k_bio", "bio.breathing_pgc_day", "bio.ppm", "bio.ops_atp_log10"]);
        assert_eq!(rs[0].date, 20260913);
        assert_eq!(rs[2].date, 20260914, "bio carries its own reading date");
        let ops = rs.iter().find(|r| r.feed == "bio.ops_atp_log10").unwrap();
        assert!((ops.value - 33.2026).abs() < 0.01, "ops committed as log10: {}", ops.value);
        // a NOAA outage: no bio block → only the two earth feeds
        let earth_only = json!({"today": {"date": "2026-09-13", "k_resid": 2.8}, "ladder": {"k_resid": {"p99": 3.1}}, "bio": {"error": "unavailable"}});
        assert_eq!(readings(&earth_only).len(), 2);
    }

    #[test]
    fn every_feed_name_is_one_the_chain_accepts() {
        for r in readings(&json!({
            "today": {"date": "2026-09-13", "k_resid": 2.8}, "ladder": {"k_resid": {"p99": 3.1}},
            "bio": {"today": {"date": "2026-09-14", "k_bio": 1.9, "ppm": 425.0}, "breathing": {"rate_pgc_per_day": -0.05}, "lloyd": {"ops_atp_per_s": 1.6e33}},
        })) {
            assert!(sigil_oracle::feed_name(&sigil_oracle::feed_id(r.feed)).is_some(), "chain rejects feed {}", r.feed);
        }
    }
}
