//! publish.rs — **the K⊕ publication joins the Flux data layer.**
//!
//! Until this module a reading lived in ONE place: a static `latest.json` in the web root, signed
//! (attest row) and anchored (shielded memo on the chain). The digest was replicated; the BYTES
//! were not. `sigil-earth publish` copies the exact attested bytes into the stores that make up
//! the Flux data layer, every one of them addressed by the attested BLAKE3, so a reader verifies
//! by construction rather than by trust:
//!
//! 1. **the SIGIL nodes' own aether store** — `POST /v1/aether/put` on EVERY node in `--nodes`
//!    (`sigil-api/src/aether.rs`, a content-addressed blob store + LWW manifest beside the chain
//!    snapshot shards). The node answers with `content_hash`; it MUST equal the attested digest
//!    or the publication is refused. The node reports `gossip: "not wired"`, so convergence is
//!    reached by writing to each node and CHECKED by every node resolving the same name to the
//!    same `content_hash`.
//! 2. **fluxc-mcp's flux-aether bundle store** — `<store>/<blake3>.json` in the exact shape
//!    `flux_aether_retrieve {content_root}` reads (same `shard_file`, same cluster key).
//! 3. **a local flux-db time series** — `reading/<date>`, `pub/<blake3>`, `attest/<n>`, `latest`.
//! 4. **a flux-search index** — one document per publication whose content is the fortolkning,
//!    so `K⊕ 2026-09-13` finds the reading and its digest.
//!
//! Idempotent: each store is checked for the digest first and skipped when it already holds it;
//! the whole run is a no-op on a digest that is everywhere. `--dry-run` computes the plan and
//! writes nothing. What is NOT claimed: node-to-node gossip (not wired in sigil-node), and any
//! replication of the flux-db / flux-search files (per-host by nature).

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Default node list: this node and the follower over WireGuard.
pub const DEFAULT_NODES: &str = "http://127.0.0.1:18181,http://10.77.0.5:18181";
/// Default fluxc-mcp bundle store (`handlers::aether::store_root()` without `FLUX_AETHER_STORE`).
pub const DEFAULT_AETHER_STORE: &str = "/home/storage/deepseek-codewhale/flux/target/flux-aether-store";
/// Default local flux-db.
pub const DEFAULT_DB: &str = "/home/storage/sigil-earth/db";
/// Default flux-search index (pass it to `flux_search` as `index_path`).
pub const DEFAULT_SEARCH_INDEX: &str = "/home/storage/sigil-earth/search/index.json";
/// Names under which the nodes hold the reading.
pub const NAME_LATEST: &str = "earth/latest";
pub const NAME_ATTEST: &str = "earth/attest/latest";
/// Shard size for the mcp bundle — the same default `flux_aether_ingest` uses.
const MCP_SHARD_SIZE: usize = 65_536;

#[derive(Clone, Debug)]
pub struct Opts {
    pub data: PathBuf,
    pub nodes: Vec<String>,
    pub aether_store: PathBuf,
    pub db: PathBuf,
    pub search_index: PathBuf,
    pub state: PathBuf,
    pub dry_run: bool,
    pub allow_unattested: bool,
}

/// The publication, as one value: what we are publishing and under which digest.
#[derive(Clone, Debug)]
pub struct Publication {
    pub bytes: Vec<u8>,
    pub blake3: String,
    pub sha256: String,
    pub date: String,
    pub mjd: f64,
    pub k_resid: f64,
    pub k_raw: f64,
    pub regime: String,
    pub lod: Option<f64>,
    pub xp: f64,
    pub yp: f64,
    pub attest_n: Option<u64>,
    pub attest_ts: Option<String>,
    pub anchor_tx: Option<String>,
    pub attest_row: Option<Value>,
    pub tips_today_en: Vec<String>,
    pub tips_today_da: Vec<String>,
}

/// Same cluster key `fluxc-mcp` derives, so its `flux_aether_retrieve` can reassemble our bundle.
pub fn cluster_key() -> Vec<u8> {
    std::env::var("FLUX_CLUSTER_SECRET").map(|s| s.into_bytes()).unwrap_or_else(|_| b"flux-aether-v0-cluster".to_vec())
}

fn producer() -> [u8; 32] {
    *blake3::hash(b"sigil-earth").as_bytes()
}

fn hex32(h: &[u8; 32]) -> String {
    h.iter().map(|b| format!("{b:02x}")).collect()
}

/// Read `latest.json` + the attest chain from `data` and build the [`Publication`].
pub fn load(data: &Path, allow_unattested: bool) -> Result<Publication> {
    let path = data.join("latest.json");
    let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let digest = blake3::hash(&bytes).to_hex().to_string();
    let sha256 = crate::attest::sha256_hex(&bytes);
    let v: Value = serde_json::from_slice(&bytes).context("latest.json is not JSON")?;
    let chain = crate::attest::read_chain(&crate::attest::chain_path(data));
    let row = chain.iter().rev().find(|r| r.kind == "attest" && r.blake3 == digest);
    if row.is_none() && !allow_unattested {
        return Err(anyhow!(
            "latest.json ({digest}) has no attest row yet — run fetch (which signs) before publish, or pass --allow-unattested"
        ));
    }
    let anchor = row.and_then(|r| chain.iter().rev().find(|a| a.kind == "anchor" && a.n == r.n && a.anchor["executed"] == true));
    let anchor_tx = anchor.and_then(|a| a.anchor["tx_hash"].as_str().filter(|s| !s.is_empty()).map(str::to_string));
    let t = &v["today"];
    let f = |k: &str| t[k].as_f64().unwrap_or(f64::NAN);
    let mut en = Vec::new();
    let mut da = Vec::new();
    if let Some(tips) = v["tips"].as_object() {
        for (k, tip) in tips {
            if let Some(s) = tip["today"].as_str() { en.push(format!("{k}: {s}")); }
            if let Some(s) = tip["today_da"].as_str() { da.push(format!("{k}: {s}")); }
        }
    }
    Ok(Publication {
        bytes,
        blake3: digest,
        sha256,
        date: t["date"].as_str().unwrap_or("").to_string(),
        mjd: f("mjd"),
        k_resid: f("k_resid"),
        k_raw: f("k_raw"),
        regime: t["regime"].as_str().unwrap_or("unknown").to_string(),
        lod: t["lod"].as_f64(),
        xp: f("xp"),
        yp: f("yp"),
        attest_n: row.map(|r| r.n),
        attest_ts: row.map(|r| r.ts.clone()),
        anchor_tx,
        attest_row: row.map(|r| json!({"row": r, "anchor": anchor.map(|a| a.anchor.clone())})),
        tips_today_en: en,
        tips_today_da: da,
    })
}

/// The slim time-series row stored under `reading/<date>` — the number, the digest, the anchor.
pub fn reading_row(p: &Publication) -> Value {
    json!({
        "date": p.date, "mjd": p.mjd, "k_resid": p.k_resid, "k_raw": p.k_raw, "regime": p.regime,
        "lod_ms": p.lod, "xp": p.xp, "yp": p.yp, "blake3": p.blake3, "sha256": p.sha256,
        "attest_n": p.attest_n, "attest_ts": p.attest_ts, "anchor_tx": p.anchor_tx,
        "source": "sigil-earth publish", "version": crate::VERSION,
    })
}

/// The flux-search document for one publication. `url` is the content address, so the same
/// reading indexed twice is one document.
pub fn search_document(p: &Publication) -> flux_search::Document {
    let mut content = format!(
        "Kristensen Earth Rotation Gauge K⊕ reading of {date}: K⊕ = {k:.2}σ ({regime}), K⊕ raw {kr:.2}, LOD {lod} ms, \
         pole x_p {xp:.6}″ y_p {yp:.6}″, MJD {mjd}. BLAKE3 {b3} SHA-256 {sha}. attest row {n}{anchor}.\n\nFortolkning (today):\n{en}\n\nFortolkning (i dag):\n{da}\n",
        date = p.date, k = p.k_resid, regime = p.regime, kr = p.k_raw,
        lod = p.lod.map(|v| format!("{v:+.3}")).unwrap_or_else(|| "n/a".into()),
        xp = p.xp, yp = p.yp, mjd = p.mjd, b3 = p.blake3, sha = p.sha256,
        n = p.attest_n.map(|n| n.to_string()).unwrap_or_else(|| "none".into()),
        anchor = p.anchor_tx.as_deref().map(|t| format!(", anchored on SIGIL in tx {t}")).unwrap_or_default(),
        en = p.tips_today_en.join("\n"), da = p.tips_today_da.join("\n"),
    );
    content.push_str("K-parameter Earth rotation gauge sigil-earth IERS pole length of day Chandler wobble fortolkning\n");
    // flux-search keeps only tokens that contain a letter and are >= 2 chars, so "2026-09-13" and "K"
    // are not searchable as typed. These compact tokens make the reading findable by date, MJD, attest
    // row and regime: `kearth d20260913`, `kearth mjd61296`, `kearth attest14`, `kearth elevated`.
    content.push_str(&search_tokens(p));
    content.push('\n');
    let word_count = flux_search::tokenize(&content).len();
    flux_search::Document {
        id: p.blake3.clone(),
        url: format!("sigil://earth/pub/{}", p.blake3),
        title: format!("K⊕ {} {:.2}σ {} — Kristensen Earth Rotation Gauge", p.date, p.k_resid, p.regime),
        content,
        meta_description: Some(format!("K⊕ reading {} · attested blake3 {} · https://sigilgraph.org/v1/earth/latest", p.date, &p.blake3[..16])),
        language: Some("en".into()),
        category: Some("sigil-earth".into()),
        page_rank: 0.0,
        readability_score: 0.0,
        word_count,
        last_crawled: Some(crate::time::now_ms() / 1000),
        content_hash: p.blake3[..32].to_string(),
    }
}

/// The compact, letter-bearing tokens that make a publication findable in flux-search.
pub fn search_tokens(p: &Publication) -> String {
    let d = p.date.replace('-', "");
    let mut t = format!("tokens: kearth kristensen-earth earth-gauge d{d} day{d} mjd{} regime-{} k{:.0}sigma", p.mjd as i64, p.regime, p.k_resid.floor());
    if let Some(n) = p.attest_n { t.push_str(&format!(" attest{n} row{n}")); }
    if p.anchor_tx.is_some() { t.push_str(" anchored onchain"); }
    t
}

/// The bundle `fluxc-mcp` stores and `flux_aether_retrieve` reads: `{block, shards, artifact_name}`.
/// Returns `(content_root_hex, json_bytes)`. The content root IS the attested digest by construction
/// (`shard_file` hashes the original bytes), and this function asserts it.
pub fn mcp_bundle(p: &Publication) -> Result<(String, Vec<u8>)> {
    let (block, shards) = flux_aether::shard_file(&p.bytes, MCP_SHARD_SIZE, &cluster_key(), producer());
    let root = hex32(&block.content_root);
    if root != p.blake3 {
        return Err(anyhow!("aether content_root {root} != attested blake3 {} — refusing to store", p.blake3));
    }
    let name = format!("sigil-earth/latest.json@{}#{}", p.date, p.attest_n.map(|n| n.to_string()).unwrap_or_else(|| "unattested".into()));
    let bundle = json!({"block": block, "shards": shards, "artifact_name": name});
    Ok((root, serde_json::to_vec_pretty(&bundle)?))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

// ── the nodes ───────────────────────────────────────────────────────────────

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout(Duration::from_secs(20)).user_agent(crate::UA).build()
}

/// GET a node route and return the parsed body (a 4xx/5xx body is returned too, never swallowed).
fn get_json(ag: &ureq::Agent, url: &str) -> Result<Value> {
    match ag.get(url).call() {
        Ok(r) => { let body = r.into_string()?; Ok(serde_json::from_str(&body).with_context(|| format!("{url}: body is not JSON: {}", body.chars().take(120).collect::<String>()))?) }
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            Ok(serde_json::from_str(&body).unwrap_or_else(|_| json!({"ok": false, "error": format!("HTTP {code}: {body}")})))
        }
        Err(e) => Err(anyhow!("{url}: {e}")),
    }
}

fn post_json(ag: &ureq::Agent, url: &str, body: &Value) -> Result<Value> {
    match ag.post(url).set("content-type", "application/json").send_string(&body.to_string()) {
        Ok(r) => { let body = r.into_string()?; Ok(serde_json::from_str(&body).with_context(|| format!("{url}: body is not JSON: {}", body.chars().take(120).collect::<String>()))?) }
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            Ok(serde_json::from_str(&body).unwrap_or_else(|_| json!({"ok": false, "error": format!("HTTP {code}: {body}")})))
        }
        Err(e) => Err(anyhow!("{url}: {e}")),
    }
}

/// What one node holds for a name: `Some(content_hash)` or `None`.
fn node_stat(ag: &ureq::Agent, base: &str, name: &str) -> Result<Option<String>> {
    let v = get_json(ag, &format!("{base}/v1/aether/stat?name={name}"))?;
    Ok(v.pointer("/data/entry/content_hash").and_then(|c| c.as_str()).map(str::to_string))
}

/// Publish to one node; returns the per-node report. Refuses if the node's `content_hash` for the
/// reading is not the attested digest.
pub fn publish_node(ag: &ureq::Agent, base: &str, p: &Publication, dry_run: bool) -> Result<Value> {
    let text = String::from_utf8(p.bytes.clone()).context("latest.json is not UTF-8")?;
    let day_name = format!("earth/day/{}", p.date);
    let mut puts = Vec::new();
    for name in [NAME_LATEST, day_name.as_str()] {
        let have = node_stat(ag, base, name)?;
        if have.as_deref() == Some(p.blake3.as_str()) {
            puts.push(json!({"name": name, "skipped": "already this digest", "content_hash": p.blake3}));
            continue;
        }
        if dry_run {
            puts.push(json!({"name": name, "dry_run": true, "would_put_bytes": p.bytes.len(), "node_has": have}));
            continue;
        }
        let r = post_json(ag, &format!("{base}/v1/aether/put"), &json!({"name": name, "content": text}))?;
        if r["ok"] != true {
            return Err(anyhow!("{base} put {name}: {}", r["error"].as_str().unwrap_or("unknown error")));
        }
        let got = r.pointer("/data/content_hash").and_then(|c| c.as_str()).unwrap_or("");
        if got != p.blake3 {
            return Err(anyhow!("{base} put {name}: node content_hash {got} != attested {}", p.blake3));
        }
        puts.push(json!({"name": name, "content_hash": got, "version": r["data"]["version"], "deduplicated": r["data"]["deduplicated"], "manifest_root": r["data"]["manifest_root"]}));
    }
    // The attest row beside it (small; LWW under one name — the full history is attest.jsonl + flux-db).
    if let Some(row) = &p.attest_row {
        let body = serde_json::to_string(row)?;
        let want = blake3::hash(body.as_bytes()).to_hex().to_string();
        let have = node_stat(ag, base, NAME_ATTEST)?;
        if have.as_deref() == Some(want.as_str()) {
            puts.push(json!({"name": NAME_ATTEST, "skipped": "already this digest", "content_hash": want}));
        } else if dry_run {
            puts.push(json!({"name": NAME_ATTEST, "dry_run": true, "would_put_bytes": body.len()}));
        } else {
            let r = post_json(ag, &format!("{base}/v1/aether/put"), &json!({"name": NAME_ATTEST, "content": body}))?;
            if r["ok"] != true {
                return Err(anyhow!("{base} put {NAME_ATTEST}: {}", r["error"].as_str().unwrap_or("unknown error")));
            }
            puts.push(json!({"name": NAME_ATTEST, "content_hash": r["data"]["content_hash"], "version": r["data"]["version"]}));
        }
    }
    // Read it back through the node and compare BYTES, not just the hash the node claims.
    let verify = if dry_run {
        json!({"dry_run": true})
    } else {
        let c = get_json(ag, &format!("{base}/v1/aether/cat?name={NAME_LATEST}"))?;
        let back = c.pointer("/data/text").and_then(|t| t.as_str()).unwrap_or("");
        let same = back.as_bytes() == p.bytes.as_slice();
        json!({"cat_bytes_identical": same, "node_verified": c["data"]["verified"], "read_via": c["data"]["read_via"], "content_hash": c["data"]["content_hash"]})
    };
    let root = get_json(ag, &format!("{base}/v1/aether/root"))?;
    Ok(json!({"node": base, "puts": puts, "verify": verify, "manifest_root": root["data"]["manifest_root"], "entries": root["data"]["entries"], "gossip": root["data"]["gossip"]}))
}

// ── the run ─────────────────────────────────────────────────────────────────

fn load_state(path: &Path) -> Value {
    std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_else(|| json!({}))
}

/// Run the whole publication. Returns the report that `main` prints.
pub fn run(o: &Opts) -> Result<Value> {
    let p = load(&o.data, o.allow_unattested)?;
    let mut report = json!({
        "ok": true, "dry_run": o.dry_run, "blake3": p.blake3, "sha256": p.sha256, "bytes": p.bytes.len(),
        "date": p.date, "k_resid": p.k_resid, "regime": p.regime, "attest_n": p.attest_n, "anchor_tx": p.anchor_tx,
    });
    let mut state = load_state(&o.state);
    let prior = state[&p.blake3].clone();

    // 1. the nodes
    let ag = agent();
    let mut nodes = Vec::new();
    let mut node_hashes = Vec::new();
    for base in &o.nodes {
        match publish_node(&ag, base, &p, o.dry_run) {
            Ok(r) => { node_hashes.push(r["verify"]["content_hash"].as_str().map(str::to_string)); nodes.push(r); }
            Err(e) => { report["ok"] = json!(false); nodes.push(json!({"node": base, "error": e.to_string()})); }
        }
    }
    let converged = !o.dry_run && !node_hashes.is_empty() && node_hashes.iter().all(|h| h.as_deref() == Some(p.blake3.as_str()));
    report["nodes"] = json!(nodes);
    report["nodes_converged_on_digest"] = json!(if o.dry_run { Value::Null } else { json!(converged) });

    // 2. the fluxc-mcp bundle store
    let bundle_path = o.aether_store.join(format!("{}.json", p.blake3));
    report["mcp_bundle"] = if bundle_path.exists() {
        json!({"path": bundle_path, "skipped": "exists"})
    } else {
        let (root, bytes) = mcp_bundle(&p)?;
        if o.dry_run {
            json!({"path": bundle_path, "dry_run": true, "content_root": root, "bytes": bytes.len()})
        } else {
            write_atomic(&bundle_path, &bytes)?;
            // read it back through the same reassemble the MCP uses
            let v: Value = serde_json::from_slice(&std::fs::read(&bundle_path)?)?;
            let block: flux_aether::FileBlock = serde_json::from_value(v["block"].clone())?;
            let shards: Vec<flux_aether::Shard> = serde_json::from_value(v["shards"].clone())?;
            let back = flux_aether::reassemble(&block, &shards, &cluster_key()).map_err(|e| anyhow!("reassemble: {e:?}"))?;
            json!({"path": bundle_path, "content_root": root, "shards": shards.len(), "k": block.k, "reassembled_identical": back == p.bytes,
                   "retrieve": format!("flux_aether_retrieve {{content_root: \"{root}\"}}")})
        }
    };

    // 3. flux-db
    let pub_key = format!("pub/{}", p.blake3);
    report["flux_db"] = if o.dry_run {
        json!({"path": o.db, "dry_run": true, "keys": [pub_key, format!("reading/{}", p.date), p.attest_n.map(|n| format!("attest/{n}")), "latest"]})
    } else {
        let db = flux_db::Database::open(&o.db).map_err(|e| anyhow!("flux-db open {}: {e}", o.db.display()))?;
        let already = db.get(pub_key.as_bytes()).map_err(|e| anyhow!("flux-db get: {e}"))?.is_some();
        if !already {
            let mut entries: Vec<(Vec<u8>, Vec<u8>)> = vec![
                (pub_key.clone().into_bytes(), p.bytes.clone()),
                (format!("reading/{}", p.date).into_bytes(), serde_json::to_vec(&reading_row(&p))?),
                (b"latest".to_vec(), p.blake3.clone().into_bytes()),
            ];
            if let (Some(n), Some(row)) = (p.attest_n, &p.attest_row) {
                entries.push((format!("attest/{n}").into_bytes(), serde_json::to_vec(row)?));
            }
            db.put_many(&entries).map_err(|e| anyhow!("flux-db put_many: {e}"))?;
            db.sync_wal().map_err(|e| anyhow!("flux-db sync_wal: {e}"))?;
        }
        let back = db.get(pub_key.as_bytes()).map_err(|e| anyhow!("flux-db get: {e}"))?;
        let readings = db.iter_from(b"reading/").take_while(|(k, _)| k.starts_with(b"reading/")).count();
        json!({"path": o.db, "skipped": if already { Some("pub key present") } else { None }, "readback_identical": back.as_deref() == Some(p.bytes.as_slice()), "readings_in_series": readings})
    };

    // 4. flux-search
    report["flux_search"] = if o.dry_run {
        json!({"index": o.search_index, "dry_run": true, "doc_url": format!("sigil://earth/pub/{}", p.blake3)})
    } else {
        let mut engine = flux_search::SearchEngine::load_or_new(&o.search_index);
        let doc = search_document(&p);
        let url = doc.url.clone();
        engine.index_document(doc);
        engine.save_to_path(&o.search_index).map_err(|e| anyhow!("flux-search save: {e}"))?;
        let q = format!("kearth d{}", p.date.replace('-', ""));
        let resp = engine.search(flux_search::SearchQuery { q: q.clone(), per_page: 5, ..Default::default() });
        let hit = resp.results.iter().any(|r| r.url == url);
        // and the ISO date as typed, via the literal (substring) path
        let lit = engine.literal_search(&p.date, 1, 5, false);
        let lit_hit = lit.results.iter().any(|r| r.url == url);
        json!({"index": o.search_index, "docs": engine.doc_count(), "query": q, "hits": resp.total_results, "found_this_publication": hit,
               "literal_query": p.date, "literal_found": lit_hit,
               "mcp": format!("flux_search {{query: \"{q}\", index_path: \"{}\"}}", o.search_index.display())})
    };

    // 5. state
    if !o.dry_run {
        state[&p.blake3] = json!({"date": p.date, "attest_n": p.attest_n, "ts": crate::time::iso_now(), "nodes_converged": converged, "prior": prior});
        write_atomic(&o.state, serde_json::to_string_pretty(&state)?.as_bytes())?;
    }
    report["already_published_before"] = json!(!prior.is_null());
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_data_dir(tag: &str, attested: bool) -> (PathBuf, Vec<u8>) {
        // one dir per (test, variant): the tests run in parallel threads of ONE process, so a
        // pid-only name made two tests share and delete each other's fixture.
        let dir = std::env::temp_dir().join(format!("sigil-earth-publish-{}-{tag}-{attested}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let latest = json!({
            "today": {"date": "2026-09-13", "mjd": 61296.0, "k_resid": 2.8089, "k_raw": 2.1285, "regime": "elevated", "lod": 1.088, "xp": 0.194492, "yp": 0.331663},
            "tips": {"k_resid": {"name": "K⊕", "today": "2026-09-13: 2.81σ, elevated", "today_da": "2026-09-13: 2.81σ, elevated (da)"}}
        });
        let bytes = serde_json::to_vec(&latest).unwrap();
        std::fs::write(dir.join("latest.json"), &bytes).unwrap();
        if attested {
            let key = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
            crate::attest::sign_row(&crate::attest::chain_path(&dir), &key, "2026-09-13", &bytes).unwrap().unwrap();
        }
        (dir, bytes)
    }

    #[test]
    fn load_requires_an_attest_row_and_reads_the_reading() {
        let (dir, bytes) = fake_data_dir("load", false);
        assert!(load(&dir, false).is_err(), "unattested bytes must be refused by default");
        let p = load(&dir, true).unwrap();
        assert_eq!(p.blake3, blake3::hash(&bytes).to_hex().to_string());
        assert!(p.attest_n.is_none());
        let (dir2, _) = fake_data_dir("load", true);
        let p2 = load(&dir2, false).unwrap();
        assert_eq!(p2.attest_n, Some(1));
        assert_eq!(p2.date, "2026-09-13");
        assert!((p2.k_resid - 2.8089).abs() < 1e-9);
        assert_eq!(p2.tips_today_en.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn mcp_bundle_content_root_is_the_attested_digest_and_reassembles() {
        let (dir, bytes) = fake_data_dir("bundle", true);
        let p = load(&dir, false).unwrap();
        let (root, json_bytes) = mcp_bundle(&p).unwrap();
        assert_eq!(root, p.blake3);
        let v: Value = serde_json::from_slice(&json_bytes).unwrap();
        assert!(v["artifact_name"].as_str().unwrap().starts_with("sigil-earth/latest.json@2026-09-13#1"));
        let block: flux_aether::FileBlock = serde_json::from_value(v["block"].clone()).unwrap();
        let shards: Vec<flux_aether::Shard> = serde_json::from_value(v["shards"].clone()).unwrap();
        assert_eq!(flux_aether::reassemble(&block, &shards, &cluster_key()).unwrap(), bytes);
        // the wrong cluster key must NOT reassemble to the bytes (content root check inside)
        assert!(flux_aether::reassemble(&block, &shards, b"wrong").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn flux_db_and_search_round_trip_and_are_idempotent() {
        let (dir, bytes) = fake_data_dir("roundtrip", true);
        let p = load(&dir, false).unwrap();
        let o = Opts {
            data: dir.clone(), nodes: vec![], aether_store: dir.join("aether-store"), db: dir.join("db"),
            search_index: dir.join("search/index.json"), state: dir.join("publish.json"), dry_run: false, allow_unattested: false,
        };
        let r1 = run(&o).unwrap();
        assert_eq!(r1["ok"], true);
        assert_eq!(r1["mcp_bundle"]["reassembled_identical"], true);
        assert_eq!(r1["flux_db"]["readback_identical"], true);
        assert_eq!(r1["flux_db"]["readings_in_series"], 1);
        assert_eq!(r1["flux_search"]["found_this_publication"], true, "{}", r1["flux_search"]);
        assert_eq!(r1["flux_search"]["literal_found"], true, "{}", r1["flux_search"]);
        assert!(search_document(&p).content.contains("d20260913") && search_document(&p).content.contains("attest1"));
        assert_eq!(r1["already_published_before"], false);
        // second run: every store skips, nothing duplicated
        let r2 = run(&o).unwrap();
        assert_eq!(r2["already_published_before"], true);
        assert_eq!(r2["mcp_bundle"]["skipped"], "exists");
        assert_eq!(r2["flux_db"]["skipped"], "pub key present");
        assert_eq!(r2["flux_db"]["readings_in_series"], 1);
        assert_eq!(r2["flux_search"]["docs"], 1);
        // the bytes in flux-db are the exact bytes
        let db = flux_db::Database::open(&o.db).unwrap();
        assert_eq!(db.get(format!("pub/{}", p.blake3).as_bytes()).unwrap().unwrap(), bytes);
        // dry-run writes nothing new
        let dry = Opts { dry_run: true, state: dir.join("other-state.json"), ..o.clone() };
        let r3 = run(&dry).unwrap();
        assert_eq!(r3["dry_run"], true);
        assert!(!dir.join("other-state.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
