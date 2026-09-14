//! `sigil-earth verify --remote <base>` — the proof you can run yourself, from any machine:
//!   1. fetch the published attestation rows, the live `latest.json` and the provenance;
//!   2. recompute BLAKE3 and SHA-256 of the live bytes and match the newest row;
//!   3. re-check every chain link and every Ed25519 signature against the PUBLISHED key;
//!   4. ask the node whether every anchor transaction is applied;
//!   5. with the anchor wallet's PUBLISHED viewing key, open the shielded pool's ciphertexts and
//!      read the memos — the digest must be there, sealed in a block nobody can rewrite.
//! Nothing here trusts this box: every input is fetched over HTTPS from the public surface.

use crate::attest::{verify_rows, AttestRow};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::io::Read;
use std::time::Duration;

fn get(base: &str, path: &str) -> Result<Vec<u8>> {
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(120)).user_agent(crate::UA).build();
    let resp = agent.get(&format!("{base}{path}")).call().map_err(|e| anyhow!("GET {path}: {e}"))?;
    let mut buf = Vec::new();
    resp.into_reader().take(64 * 1024 * 1024).read_to_end(&mut buf)?;
    Ok(buf)
}

fn get_json(base: &str, path: &str) -> Result<Value> {
    Ok(serde_json::from_slice(&get(base, path)?)?)
}

pub fn verify_remote(base: &str, viewing_key_override: Option<String>) -> Result<Value> {
    let base = base.trim_end_matches('/');
    let mut report = json!({"base": base, "checks": {}, "ok": true});
    let mut fail = |report: &mut Value, name: &str, detail: Value| {
        report["checks"][name] = detail;
        report["ok"] = json!(false);
    };

    // 1 · the published inputs
    let latest = get(base, "/v1/earth/latest")?;
    let prov = get_json(base, "/v1/earth/provenance")?;
    let att = get_json(base, "/v1/earth/attest?last=500")?;
    let rows: Vec<AttestRow> = att["rows"].as_array().cloned().unwrap_or_default().into_iter().filter_map(|v| serde_json::from_value(v).ok()).collect();
    let pubkey = prov["attest_pubkey"].as_str().unwrap_or("").to_string();
    report["published_pubkey"] = json!(pubkey);
    report["rows_fetched"] = json!(rows.len());
    report["latest_bytes"] = json!(latest.len());

    // 2 · digests of the live bytes
    let b3 = blake3::hash(&latest).to_hex().to_string();
    let sha = crate::attest::sha256_hex(&latest);
    let newest = rows.iter().rev().find(|r| r.kind == "attest");
    report["live_digests"] = json!({"blake3": b3, "sha256": sha});
    match newest {
        Some(r) => {
            let ok_b3 = r.blake3 == b3;
            let ok_sha = r.sha256.is_empty() || r.sha256 == sha;
            report["checks"]["live_file_matches_newest_row"] = json!({"ok": ok_b3 && ok_sha, "row": r.n, "row_blake3": r.blake3, "row_sha256": r.sha256, "row_version": r.v});
            if !(ok_b3 && ok_sha) { report["ok"] = json!(false); }
        }
        None => fail(&mut report, "live_file_matches_newest_row", json!({"ok": false, "error": "no attest rows published"})),
    }

    // 3 · chain links + signatures, and every signature must be under the PUBLISHED key
    let v = verify_rows(&rows, Some(&latest));
    let foreign: Vec<u64> = rows.iter().filter(|r| r.kind == "attest" && r.pubkey != pubkey).map(|r| r.n).collect();
    let chain_ok = v.chain_intact && v.sigs_bad.is_empty() && foreign.is_empty() && v.attest_rows > 0;
    report["checks"]["chain_and_signatures"] = json!({"ok": chain_ok, "attest_rows": v.attest_rows, "chain_intact": v.chain_intact, "sigs_ok": v.sigs_ok, "sigs_bad": v.sigs_bad, "rows_not_under_published_key": foreign});
    if !chain_ok { report["ok"] = json!(false); }

    // 4 · every anchor transaction must be applied on the node
    let anchors: Vec<&AttestRow> = rows.iter().filter(|r| r.kind == "anchor" && r.anchor["executed"] == true).collect();
    let mut tx_checks = Vec::new();
    let mut tx_ok = true;
    for a in &anchors {
        let tx = a.anchor["tx_hash"].as_str().unwrap_or("").to_string();
        let st = get_json(base, &format!("/v1/transactions/{tx}")).ok();
        let status = st.as_ref().and_then(|s| s.pointer("/data/status")).and_then(|s| s.as_str()).unwrap_or("unknown").to_string();
        let applied = status == "applied";
        if !applied { tx_ok = false; }
        tx_checks.push(json!({"n": a.n, "tx_hash": tx, "status": status, "ok": applied}));
    }
    report["checks"]["anchor_transactions_applied"] = json!({"ok": tx_ok && !anchors.is_empty(), "anchors": anchors.len(), "txs": tx_checks});
    if !(tx_ok && !anchors.is_empty()) { report["ok"] = json!(false); }

    // 5 · open the anchor wallet's memos with the PUBLISHED viewing key
    let vk_hex = viewing_key_override.or_else(|| prov.pointer("/anchor/viewing_key").and_then(|v| v.as_str()).map(|s| s.to_string()));
    match vk_hex {
        None => { report["checks"]["memos_readable_with_published_viewing_key"] = json!({"ok": false, "skipped": "no viewing key published yet"}); }
        Some(vkh) => {
            let vk: [u8; 32] = hex::decode(vkh.trim()).ok().and_then(|b| b.try_into().ok()).ok_or_else(|| anyhow!("viewing key must be 32-byte hex"))?;
            let id = flux_swarm_secret::SecretIdentity::from_sk_bytes(vk);
            let pool = get_json(base, "/v1/shielded/leaves")?;
            let cts: Vec<String> = pool["ciphertexts"].as_array().map(|a| a.iter().filter_map(|c| c.as_str().map(|s| s.to_string())).filter(|s| !s.is_empty()).collect()).unwrap_or_default();
            let mut memos = Vec::new();
            for c in &cts {
                if let Ok(pt) = sigil_shield::note_cipher::try_open_note(&sigil_shield::note_cipher::NoteCiphertext(c.clone()), &id) {
                    let m = pt.memo.text();
                    if m.starts_with("sigil-earth-attest-v") { memos.push(m); }
                }
            }
            let mut per = Vec::new();
            let mut all = true;
            for a in &anchors {
                let memo = a.anchor["memo"].as_str().unwrap_or("").to_string();
                let found = memos.iter().any(|m| *m == memo);
                // anchors made before the dedicated wallet existed cannot be opened with its key: say so
                let wallet = a.anchor["wallet"].as_str().unwrap_or("").to_string();
                let expected_here = prov.pointer("/anchor/address").and_then(|v| v.as_str()).map(|addr| wallet.is_empty() || wallet.starts_with(&addr[..addr.len().min(24)])).unwrap_or(true);
                if expected_here && !found { all = false; }
                per.push(json!({"n": a.n, "memo": memo, "found_on_chain": found, "from_published_wallet": expected_here}));
            }
            report["checks"]["memos_readable_with_published_viewing_key"] = json!({"ok": all && !per.is_empty(), "ciphertexts_scanned": cts.len(), "earth_memos_found": memos.len(), "anchors": per});
            if !(all && !per.is_empty()) { report["ok"] = json!(false); }
        }
    }
    report["verdict"] = json!(if report["ok"] == true { "VERIFIED — the published Earth reading is the one that was signed, the chain is intact under the published key, and every anchor is applied on the SIGIL chain with its digest readable in the memo" } else { "NOT VERIFIED — see checks" });
    Ok(report)
}
