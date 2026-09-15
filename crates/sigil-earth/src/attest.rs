//! The attestation chain: one row per published reading, BLAKE3 of the exact bytes written to
//! `latest.json`, linked to the previous row's digest and signed with a dedicated Ed25519 key.
//! The on-chain anchor (a shielded memo self-send carrying the digest) is a separate row written
//! by `sigil-earth attest`, so the chain says honestly which digests reached the chain.

use anyhow::{anyhow, Context, Result};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const DEFAULT_KEY: &str = "/root/.config/sigil/earth-attest.seed";
pub const VERSION: &str = "sigil-earth-attest-v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttestRow {
    pub kind: String, // "attest" | "anchor"
    pub v: u32,
    pub n: u64,
    pub date: String,
    pub ts: String,
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub blake3: String,
    /// v2 rows (2026-09-14 evening onward): SHA-256 of the same bytes, so a browser can verify.
    #[serde(default)]
    pub sha256: String,
    #[serde(default)]
    pub prev: Option<String>,
    #[serde(default)]
    pub msg: String,
    #[serde(default)]
    pub pubkey: String,
    #[serde(default)]
    pub sig: String,
    #[serde(default)]
    pub anchor: serde_json::Value,
}

pub fn load_or_create_key(path: &Path) -> Result<SigningKey> {
    if let Ok(raw) = std::fs::read_to_string(path) {
        let h = raw.trim().strip_prefix("0x").unwrap_or(raw.trim());
        let b = hex::decode(h).context("attest seed is not hex")?;
        let arr: [u8; 32] = b.try_into().map_err(|_| anyhow!("attest seed must be 32 bytes"))?;
        return Ok(SigningKey::from_bytes(&arr));
    }
    let sk = SigningKey::generate(&mut rand::rngs::OsRng);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, hex::encode(sk.to_bytes()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    eprintln!("🔑 generated a NEW attestation key at {} (chmod 600)", path.display());
    Ok(sk)
}

pub fn read_chain(path: &Path) -> Vec<AttestRow> {
    std::fs::read_to_string(path)
        .map(|s| s.lines().filter_map(|l| serde_json::from_str(l).ok()).collect())
        .unwrap_or_default()
}

pub fn message(n: u64, date: &str, digest: &str, prev: Option<&str>) -> String {
    format!("{VERSION}|{n}|{date}|{digest}|{}", prev.unwrap_or("null"))
}

pub const VERSION2: &str = "sigil-earth-attest-v2";
pub fn message_v2(n: u64, date: &str, blake3: &str, sha256: &str, prev: Option<&str>) -> String {
    format!("{VERSION2}|{n}|{date}|{blake3}|{sha256}|{}", prev.unwrap_or("null"))
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}

/// The message a row signed, by its version.
pub fn expected_message(r: &AttestRow) -> String {
    if r.v >= 2 {
        message_v2(r.n, &r.date, &r.blake3, &r.sha256, r.prev.as_deref())
    } else {
        message(r.n, &r.date, &r.blake3, r.prev.as_deref())
    }
}

/// Sign a new row for `latest_bytes` unless the newest attest row already carries this digest.
pub fn sign_row(chain_path: &Path, key: &SigningKey, date: &str, latest_bytes: &[u8]) -> Result<Option<AttestRow>> {
    let digest = blake3::hash(latest_bytes).to_hex().to_string();
    let chain = read_chain(chain_path);
    let last = chain.iter().rev().find(|r| r.kind == "attest");
    if let Some(l) = last {
        if l.blake3 == digest {
            return Ok(None);
        }
    }
    let n = last.map(|l| l.n + 1).unwrap_or(1);
    let prev = last.map(|l| l.blake3.clone());
    let sha = sha256_hex(latest_bytes);
    let msg = message_v2(n, date, &digest, &sha, prev.as_deref());
    let sig = key.sign(msg.as_bytes());
    let row = AttestRow {
        kind: "attest".into(),
        v: 2,
        n,
        date: date.into(),
        ts: crate::time::iso_now(),
        subject: "eop/latest.json".into(),
        blake3: digest.clone(),
        sha256: sha,
        prev,
        msg,
        pubkey: hex::encode(key.verifying_key().to_bytes()),
        sig: hex::encode(sig.to_bytes()),
        anchor: serde_json::json!({"memo": format!("sigil-earth-attest-v1:{n}:{digest}"), "executed": false, "tx_hash": null, "ts": null}),
    };
    append(chain_path, &row)?;
    Ok(Some(row))
}

pub fn append(path: &Path, row: &AttestRow) -> Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "{}", serde_json::to_string(row)?)?;
    f.sync_all()?;
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct Verification {
    pub rows: usize,
    pub attest_rows: usize,
    pub anchor_rows: usize,
    pub chain_intact: bool,
    pub sigs_ok: usize,
    pub sigs_bad: Vec<u64>,
    pub live_match: Option<bool>,
    pub last_digest: Option<String>,
    pub anchored: usize,
}

pub fn verify(chain_path: &Path, live: Option<&Path>) -> Verification {
    let chain = read_chain(chain_path);
    let live_bytes = live.and_then(|p| std::fs::read(p).ok());
    verify_rows(&chain, live_bytes.as_deref())
}

/// Verify a chain of rows against optional live bytes — the same check locally and remotely.
pub fn verify_rows(chain: &[AttestRow], live_bytes: Option<&[u8]>) -> Verification {
    let mut prev: Option<String> = None;
    let mut intact = true;
    let mut ok = 0;
    let mut bad = Vec::new();
    let mut attest_rows = 0;
    let mut anchor_rows = 0;
    let mut anchored = 0;
    let mut last_digest = None;
    for r in chain {
        if r.kind == "anchor" {
            anchor_rows += 1;
            if r.anchor.get("executed").and_then(|v| v.as_bool()) == Some(true) {
                anchored += 1;
            }
            continue;
        }
        attest_rows += 1;
        if r.prev != prev {
            intact = false;
        }
        let expect = expected_message(r);
        let sig_ok = (|| -> Option<bool> {
            let pk: [u8; 32] = hex::decode(&r.pubkey).ok()?.try_into().ok()?;
            let sg: [u8; 64] = hex::decode(&r.sig).ok()?.try_into().ok()?;
            let vk = VerifyingKey::from_bytes(&pk).ok()?;
            Some(r.msg == expect && vk.verify(r.msg.as_bytes(), &Signature::from_bytes(&sg)).is_ok())
        })()
        .unwrap_or(false);
        if sig_ok {
            ok += 1;
        } else {
            bad.push(r.n);
        }
        prev = Some(r.blake3.clone());
        last_digest = Some(r.blake3.clone());
    }
    let live_match = live_bytes.map(|b| Some(blake3::hash(b).to_hex().to_string()) == last_digest);
    Verification { rows: chain.len(), attest_rows, anchor_rows, chain_intact: intact, sigs_ok: ok, sigs_bad: bad, live_match, last_digest, anchored }
}

/// The newest attestation for the live file with its anchor MERGED IN.
///
/// The chain stores the signature (`kind == "attest"`) and the on-chain anchor (`kind == "anchor"`)
/// as two rows, because the anchor lands seconds later and can fail on its own. Reading only the
/// attest row reports its pre-anchor stub (`executed: false, tx_hash: null`) forever — which is
/// exactly what `/v1/earth/latest` told its readers about an attestation that WAS anchored
/// (found 2026-09-15). An anchor row is only borrowed for the same `n`.
pub fn latest_summary(chain: &[AttestRow]) -> Option<serde_json::Value> {
    use serde_json::{json, Value};
    let a = chain.iter().rev().find(|r| r.kind == "attest")?;
    let anchor_row = chain.iter().rev().find(|r| r.kind == "anchor" && r.n == a.n);
    let anchor = anchor_row.map(|r| r.anchor.clone()).unwrap_or_else(|| a.anchor.clone());
    let tx = anchor.get("tx_hash").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
    let anchored = anchor.get("executed").and_then(Value::as_bool).unwrap_or(false) && tx.is_some();
    Some(json!({
        "n": a.n, "v": a.v, "date": a.date, "ts": a.ts, "blake3": a.blake3, "sha256": a.sha256,
        "anchored": anchored,
        "anchor_tx": if anchored { json!(tx) } else { Value::Null },
        "anchor": anchor,
        "note": "the row for the PREVIOUS publication: this file's own row is appended seconds after it is written — /v1/earth/attest has the newest",
    }))
}

pub fn chain_path(out_dir: &Path) -> PathBuf {
    out_dir.join("attest.jsonl")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_rows_chain_and_verify_and_a_repeat_digest_is_skipped() {
        let dir = std::env::temp_dir().join(format!("sigil-earth-attest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let chain = dir.join("attest.jsonl");
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let r1 = sign_row(&chain, &key, "2026-09-13", b"{\"a\":1}").unwrap().unwrap();
        assert_eq!(r1.n, 1);
        assert!(r1.prev.is_none());
        assert!(sign_row(&chain, &key, "2026-09-13", b"{\"a\":1}").unwrap().is_none());
        let r2 = sign_row(&chain, &key, "2026-09-14", b"{\"a\":2}").unwrap().unwrap();
        assert_eq!(r2.n, 2);
        assert_eq!(r2.prev.as_deref(), Some(r1.blake3.as_str()));
        let live = dir.join("latest.json");
        std::fs::write(&live, b"{\"a\":2}").unwrap();
        let v = verify(&chain, Some(&live));
        assert!(v.chain_intact && v.sigs_ok == 2 && v.sigs_bad.is_empty() && v.live_match == Some(true));
        // tamper with the file: the signature check must fail
        let tampered = std::fs::read_to_string(&chain).unwrap().replace("2026-09-14", "2026-09-15");
        std::fs::write(&chain, tampered).unwrap();
        let v2 = verify(&chain, None);
        assert_eq!(v2.sigs_bad, vec![2]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn latest_summary_merges_the_anchor_row_for_the_same_n_only() {
        use serde_json::{json, Value};
        let mk = |kind: &str, n: u64, anchor: Value| AttestRow {
            kind: kind.into(), v: 2, n, date: "2026-09-13".into(), ts: "t".into(), subject: String::new(),
            blake3: format!("b{n}"), sha256: String::new(), prev: None, msg: String::new(), pubkey: String::new(),
            sig: String::new(), anchor,
        };
        // the attest row carries the pre-anchor stub; the anchor row carries the truth
        let chain = vec![mk("attest", 12, json!({"executed": false, "tx_hash": null})),
                         mk("anchor", 12, json!({"executed": true, "tx_hash": "abc"}))];
        let s = latest_summary(&chain).unwrap();
        assert_eq!(s["n"], 12);
        assert_eq!(s["anchored"], true);
        assert_eq!(s["anchor_tx"], "abc");
        assert_eq!(s["anchor"]["executed"], true);
        // no anchor row yet: honest false, null tx
        let s2 = latest_summary(&[mk("attest", 13, json!({"executed": false, "tx_hash": null}))]).unwrap();
        assert_eq!(s2["anchored"], false);
        assert!(s2["anchor_tx"].is_null());
        // an anchor for an OLDER n must not be borrowed by a newer attest
        let chain3 = vec![mk("attest", 12, json!({})), mk("anchor", 12, json!({"executed": true, "tx_hash": "abc"})),
                          mk("attest", 13, json!({"executed": false}))];
        let s3 = latest_summary(&chain3).unwrap();
        assert_eq!(s3["n"], 13);
        assert_eq!(s3["anchored"], false);
        // executed without a tx hash is not an anchor either
        let s4 = latest_summary(&[mk("attest", 1, json!({})), mk("anchor", 1, json!({"executed": true, "tx_hash": ""}))]).unwrap();
        assert_eq!(s4["anchored"], false);
    }
}
