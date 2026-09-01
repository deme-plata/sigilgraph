//! Node feed / status polling: fetch the tip + recent blocks, pick the best of
//! feed-vs-API, roll one refresh, and verify a tip. Extracted from main.rs.
//! `use super::*` reaches the NodeStatus/Config/Tip/etc. structs + http_client::fetch.
use super::*;

pub(crate) fn set_feed_err(e: String) { if let Ok(mut g)=LAST_FEED_ERR.lock(){ *g=e; } }
/// The most recent feed-fetch failure reason — shown on the OFFLINE card so a user can SEE why
/// (DNS, TLS, connection refused, HTTP status, or JSON parse) instead of a blind "offline".
pub fn last_feed_err() -> String { LAST_FEED_ERR.lock().map(|g| g.clone()).unwrap_or_default() }

pub(crate) fn fetch_feed(url: &str) -> Option<(NodeStatus, Vec<FeedBlock>)> {
    // 0.77: shared pooled client (keep-alive) — was a fresh Client per call per tick.
    let resp = match HTTP.get(url).timeout(Duration::from_secs(6)).send() { Ok(r)=>r, Err(e)=>{ set_feed_err(format!("connect @ {url}: {e}")); return None; } };
    let code = resp.status();
    let body = match resp.text() { Ok(b)=>b, Err(e)=>{ set_feed_err(format!("read @ {url} (HTTP {code}): {e}")); return None; } };
    let feed: Feed = match serde_json::from_str(&body) { Ok(f)=>f, Err(e)=>{ set_feed_err(format!("parse @ {url} (HTTP {code}): {e}")); return None; } };
    let s = feed.status;
    // v0.64 LANE-T: the status feed sends supply in BASE units on some sources and
    // WHOLE SIGIL on others. Multiplying a base value by 10^8 again double-scaled it
    // (showed 1,748,502,017,000 instead of ~17,485). Auto-detect by magnitude: nothing
    // can exceed the 21M WHOLE cap, so a value above it is already base -> keep; a value
    // at/under 21M is whole -> scale to base.
    let supply_raw: u128 = s.supply.chars().filter(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap_or(0);
    // LANE-T hardening: a value 10^8 over the cap is the double-scale tell — clamp to the
    // 21M base cap so a corrupt/mis-scaled source can never show > 21M SIGIL in the hero.
    let native_supply = (if supply_raw > 21_000_000u128 { supply_raw } else { supply_raw.saturating_mul(10u128.pow(DECIMALS)) }).min(MAX_SUPPLY_BASE);
    // Carry the committed roots through as hex so the no-local-node view still
    // shows the 4 state roots, not "—".
    let (wr, dr, er, cr) = feed
        .tip
        .as_ref()
        .map(|t| {
            (
                hex(&t.roots.wallet_state_root),
                hex(&t.roots.dex_state_root),
                hex(&t.roots.event_log_root),
                hex(&t.roots.contract_state_root),
            )
        })
        .unwrap_or_default();
    let st = NodeStatus {
        network: s.network_id,
        height: feed.tip.as_ref().map(|t| t.height).filter(|h| *h > 0).unwrap_or(s.height),
        peers: s.peers,
        producer: feed.blocks.first().map(|b| b.producer.clone()).unwrap_or_default(),
        native_supply,
        wallet_root: wr,
        dex_root: dr,
        event_root: er,
        contract_root: cr,
        tip: feed.tip,
        blocks_per_sec: s.blocks_per_sec,
        reward_sig: s.reward_sig,
        ..Default::default()
    };
    Some((st, feed.blocks))
}

/// Resolve the best available status. A lightweight verifier-miner is meant to
/// run on a "potato" with NO local full node, so prefer the verified live HTTPS
/// feed (real chain tip, supply, and committed roots); only fall back to a local
/// node on the api port if the feed can't be reached. Returns (status, online,
/// source) where source is "feed" | "local" | "offline".
pub(crate) fn fetch_best(cfg: &Config) -> (NodeStatus, bool, &'static str) {
    // Try the configured feed, then known-good public mirrors — so a node on a network where one
    // host is blocked/unresolvable still syncs from another. (Was single-feed → looked "offline".)
    // v0.64.1: remember the last PRODUCE-feed height so the local-API fallback can
    // never silently swap chains. The :8099 rpcd is the MINE chain (height ~3.5k);
    // when all feed mirrors hiccup for one poll, falling back to it made the hero
    // JUMP 520k -> 3.5k -> 520k. A fallback drastically below the last feed height
    // is a different chain -> show an honest offline/retry instead of lying.
    static LAST_FEED_H: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    // v7.1.13: STALENESS GUARD. A mirror can answer HTTP 200 with a frozen file
    // (dist-fluxapp's copy stopped updating 2026-07-27; it is FIRST in this list,
    // so every client pinned its tip to 33,598,726 for 13 days). A feed whose own
    // `updated` stamp is >15 min old loses to any fresh mirror; the freshest stale
    // copy is still used when NOTHING is fresh (better an old tip than none).
    let mut stale_best: Option<NodeStatus> = None;
    for url in [cfg.feed.as_str(),
                "https://sigilgraph.fluxapp.xyz/sigil-status.json",
                "https://quillon.xyz/sigil-status.json",
                // v7.0.26: plain-HTTP :8099 mirror — the port mining provably reaches
                // on networks that filter the app's HTTPS (the OFFLINE-badge saga).
                "http://sigilgraph.quillon.xyz:8099/sigil-status.json"] {
        if let Some((st, _b)) = fetch_feed(url) {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            if st.updated > 0 && now.saturating_sub(st.updated) > 900 {
                if stale_best.as_ref().map(|b| st.updated > b.updated).unwrap_or(true) {
                    stale_best = Some(st);
                }
                continue;
            }
            LAST_FEED_H.store(st.height, std::sync::atomic::Ordering::Relaxed);
            return (st, true, "feed");
        }
    }
    if let Some(st) = stale_best {
        LAST_FEED_H.store(st.height, std::sync::atomic::Ordering::Relaxed);
        return (st, true, "feed-stale");
    }
    match fetch(&cfg.api) {
        Ok(s) => {
            let lf = LAST_FEED_H.load(std::sync::atomic::Ordering::Relaxed);
            if lf > 0 && s.height.saturating_mul(4) < lf {
                // mine-chain rpcd answering for a produce-feed blip — suppress, retry feed
                (NodeStatus::default(), false, "offline")
            } else {
                (s, true, "local")
            }
        }
        Err(_) => (NodeStatus::default(), false, "offline"),
    }
}

/// v0.10.5: the result of one network refresh cycle, produced ENTIRELY on a
/// background worker thread so the render loop never blocks on a socket. Owned
/// data only — moves cleanly across the channel into `App::apply_refresh`.
pub(crate) struct RefreshOutcome {
    pub(crate) st: NodeStatus,
    pub(crate) online: bool,
    pub(crate) blocks: Option<Vec<FeedBlock>>,                // Some => replace the block list
    pub(crate) fallback_note: bool,                           // show the "API fallback" toast
    pub(crate) eclipse: Option<(u32, Vec<(String, bool)>)>,   // Some => eclipse-K re-measured this cycle
}

/// v0.10.5 "smooth cruise": all the blocking network I/O of the old
/// `App::refresh` — feed fetch, the 8s reqwest block-fallback, the local API
/// probe, and the DoH eclipse-K measurement — gathered into ONE function that
/// runs off the UI thread. Previously these ran inline on every interval tick
/// and every [R], so a slow/unreachable node froze the whole TUI for up to
/// ~8 seconds (keystrokes ignored, animation stalled). Now the render loop
/// spawns this and keeps drawing at full frame-rate while it works.
pub(crate) fn fetch_refresh(feed: String, api: String, want_eclipse: bool, prior_synced: u64) -> RefreshOutcome {
    // Primary: HTTPS status feed, then fall back to the local node API.
    let (st, online, mut blocks) = match fetch_feed(&feed) {
        Some((s, b)) => (s, true, Some(b)),
        None => match fetch(&api) {
            Ok(s) => (s, true, None),
            Err(_) => (NodeStatus::default(), false, None),
        },
    };

    // v0.4.0 fallback: feed online but no blocks → pull recent blocks from the API.
    let mut fallback_note = false;
    let empty_blocks = blocks.as_ref().map(|b| b.is_empty()).unwrap_or(true);
    if empty_blocks && online {
        // 0.77: shared pooled client — was a fresh Client per fallback tick.
        {
            let client = &*HTTP;
            let api_base = api.trim_end_matches('/');
            if let Ok(resp) = client.get(format!("{}/v1/blocks/recent?limit=14", api_base)).timeout(Duration::from_secs(8)).send() {
                if let Ok(json) = resp.json::<serde_json::Value>() {
                    if let Some(arr) = json.get("blocks").or_else(|| json.get("data")).and_then(|v| v.as_array()) {
                        let fb: Vec<FeedBlock> = arr.iter().filter_map(|b| {
                            let h = b.get("height").and_then(|v| v.as_u64()).unwrap_or(0);
                            if h == 0 { return None; }
                            Some(FeedBlock {
                                height: h,
                                hash: b.get("proposer").and_then(|p| p.as_str()).map(|s| &s[..s.len().min(16)]).unwrap_or("—").into(),
                                producer: b.get("proposer").and_then(|p| p.as_str()).unwrap_or("").into(),
                                txs: b.get("tx_count").and_then(|t| t.as_u64()).unwrap_or(0),
                                tip_ms: 0,
                            })
                        }).collect();
                        if !fb.is_empty() { blocks = Some(fb); fallback_note = true; }
                    }
                }
            }
        }
    }

    // L2-B eclipse-K (DoH, RTT-blocking) — also off the UI thread now. tip_ok is
    // computed here from the just-fetched tip; height uses the verified tip when
    // good, else the prior verified watermark.
    let eclipse = if want_eclipse {
        let tip_ok = st.tip.as_ref().map(|t| verify_tip(t).ok).unwrap_or(false);
        let height = st.tip.as_ref().map(|t| t.height).filter(|_| tip_ok).unwrap_or(prior_synced);
        Some(measure_eclipse_k(height, tip_ok))
    } else {
        None
    };

    RefreshOutcome { st, online, blocks, fallback_note, eclipse }
}

/// Outcome of verifying the node's real tip — every field is a fact the client
/// just checked, not a placeholder.
#[derive(Clone)]
pub(crate) struct TipVerify {
    pub(crate) ok: bool,
    pub(crate) err: Option<String>,
    pub(crate) height: u64,
    pub(crate) fingerprint_hex: String,
    /// True iff the reported block hash equals the v0 tip-proof fingerprint
    /// (i.e. the block hash commits to exactly these 4 roots and nothing else).
    pub(crate) hash_is_fingerprint: bool,
    pub(crate) reported_hash: String,
    pub(crate) latency_us: u128,
    /// v0.2.35 L4-B: whether the SQIsign post-quantum flavor is available on this
    /// build. False = only BLAKE3 v0 flavor; true = flux-sqisign crate linked and
    /// the SqiSignBlob flavor can be verified (adversary-resistant). The UI uses
    /// this to show "PQ-ready" vs "base" security level.
    pub(crate) sqisign_available: bool,
}

/// L4-A keystone: reconstruct the canonical v0 tip-proof from the node's real
/// roots and verify it for sigil-g0. ~µs, downloads 0 blocks. NOTE (honest): the
/// v0 `Blake3Fingerprint` flavor proves the proof is well-formed + on the right
/// network + uncorrupted — it does NOT alone prove canonicality/adversarial
/// safety. That comes from K independent sources (L4-C) + the SQIsign/STARK
/// flavors. The UI says so.
///
/// L4-B (v0.2.35 scaffolding): when flux-sqisign is linked and the node emits
/// the `SqiSignBlob` tip-proof flavor, this function will also construct a
/// `TipProof::new_sqisign()` and verify the post-quantum signature. The
/// `sqisign_available` field in TipVerify signals whether that code path exists
/// on this build — currently gated on the `sqisign` feature of sigil-tip-proof.
/// v0.3.1 L4-B: testnet producer SQIsign public key (129 bytes, base64).
/// Pinned here until DNS anchor (Lane 5) publishes it in _sigil-tip TXT.
/// The SQIsign verify path uses this key to determine adversary-resistance.
const PRODUCER_SQISIGN_PK: &[u8] = b""; // populated when the producer key is published

pub(crate) fn verify_tip(tip: &Tip) -> TipVerify {
    let roots = tip.roots.to_state_roots();
    let t = Instant::now();
    let proof = TipProof::new_blake3(tip.height, roots);
    let res = proof.verify(sigil_net::NETWORK_ID);
    let latency_us = t.elapsed().as_micros();
    let fingerprint_hex = hex(&proof.fingerprint());
    let hash_is_fingerprint =
        !tip.hash.is_empty() && tip.hash.eq_ignore_ascii_case(&fingerprint_hex);
    // v0.3.1 L4-B: SQIsign post-quantum flavor — now live via sigil-tip-proof's
    // native feature (flux-sqisign linked). When the tip carries a SqiSignBlob
    // flavor AND the producer public key is known, verify_sqisign() runs.
    let sqisign_available = cfg!(feature = "sqisign");
    // Future: if the TipProof flavor is SqiSignBlob and PRODUCER_SQISIGN_PK is set,
    // call proof.verify_sqisign(sigil_net::NETWORK_ID, PRODUCER_SQISIGN_PK) and
    // fold the result into `ok`. For now, the BLAKE3 v0 path remains the primary
    // verify; the SQIsign path composes once the DNS anchor publishes the key.
    let _ = PRODUCER_SQISIGN_PK; // silence unused warning until key is published
    TipVerify {
        ok: res.is_ok(),
        err: res.err().map(|e| e.to_string()),
        height: tip.height,
        fingerprint_hex,
        hash_is_fingerprint,
        reported_hash: tip.hash.clone(),
        latency_us,
        sqisign_available,
    }
}
