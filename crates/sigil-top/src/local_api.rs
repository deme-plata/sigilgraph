// sigil-top/src/local_api.rs — embedded explorer API over the LOCAL verified spine (v0.11.0)
//
// The SIGIL Explorer (gui/sigil-explorer.html) used to proxy EVERY `/api/v1/*` call to
// the remote sigil-rpcd. But sigil-top is itself a verifying full-sync light node: it
// holds a flux-db block store (verified genesis→tip), runs a flux-cortex optimization
// loop, and tracks the mesh peer set. This module surfaces all of that to the explorer.
//
// Design = LOCAL-FIRST with REMOTE FALLBACK. `handle()` returns:
//   * `Some(json)` — answered from the local verified store (blocks / status / aether
//     content-verify / cortex / peers). This is the trust-minimised path: the explorer
//     shows THIS node's verified spine, and aether verification re-derives the block
//     hash locally (verify-don't-trust) instead of believing a remote node.
//   * `None`       — not answerable locally (txs, full-text, address lookups, or this
//     process isn't syncing) → serve.rs proxies to the remote sigil-rpcd as before.
//
// Net effect: zero regression for a pure light monitor (no sync → everything proxies,
// exactly as today), and a richer, trustless explorer the moment `--sync` is on.

use std::sync::{Arc, Mutex};

use crate::block_store::{BlockReader, BlockRow};
use crate::block_sync::P2PSyncState;

/// Shared snapshot of the Cortex optimization engine. The TUI thread publishes it after
/// each `[C]` loop; the HTTP server reads it for the explorer's `⚙ Cortex` panel.
#[derive(Clone, Default)]
pub struct CortexSnapshot {
    pub loops: u64,
    pub last_gain_pct: f64,
    pub summary: String,
    pub last_tool: String,
}

/// Handle shared into the embedded HTTP server (serve.rs).
pub struct LocalApi {
    pub reader: BlockReader,
    /// Live mesh state — None when this process isn't syncing (pure light monitor).
    pub sync: Option<Arc<Mutex<P2PSyncState>>>,
    pub cortex: Arc<Mutex<CortexSnapshot>>,
    pub network: String,
}

/// Tiny query-string param lookup (no external dep). Handles `+` and `%XX`.
fn qparam(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if k == key {
            return Some(urldecode(v));
        }
    }
    None
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => { out.push(b' '); i += 1; }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                match (hi, lo) {
                    (Some(h), Some(l)) => { out.push((h * 16 + l) as u8); i += 3; }
                    _ => { out.push(b'%'); i += 1; }
                }
            }
            b => { out.push(b); i += 1; }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl LocalApi {
    /// Snapshot of the sync state (cheap clone under the lock), or None if not syncing.
    fn sync_snapshot(&self) -> Option<P2PSyncState> {
        self.sync.as_ref().and_then(|s| s.lock().ok().map(|g| g.clone()))
    }

    /// Our authoritative chain height: the verified-spine watermark if we have one,
    /// else the raw download tip.
    fn local_top(s: &P2PSyncState) -> u64 {
        // v0.18.5: "height" = the NETWORK TIP we are syncing toward (peer_best from gossip)
        // so the dashboard shows the real target; synced_to/verified report OUR progress.
        s.peer_best_height.max(s.verified).max(s.blocks_synced)
    }

    /// Try to answer `/api/...` locally. Returns `Some(json_body)` or `None` (proxy).
    pub fn handle(&self, path_and_query: &str) -> Option<String> {
        let (path, query) = path_and_query.split_once('?').unwrap_or((path_and_query, ""));
        match path {
            "/api/v1/status" => self.status(),
            "/api/v1/recent" => self.recent(query),
            "/api/v1/search" => self.search(query),
            "/api/v1/aether" => self.aether(query),
            "/api/v1/cortex" => Some(self.cortex_json()), // always local (our engine)
            "/api/v1/peers" => self.peers(),
            _ => None,
        }
    }

    fn status(&self) -> Option<String> {
        // Only authoritative when we're actually syncing; otherwise let the explorer
        // show the remote chain (pure light-monitor parity with the old behaviour).
        let s = self.sync_snapshot()?;
        let top = Self::local_top(&s);
        let cx = self.cortex.lock().ok().map(|g| g.clone()).unwrap_or_default();
        let status = if s.verify_break.is_some() { "spine-break" }
            else if s.running { "syncing" } else { "ready" };
        let v = serde_json::json!({
            "source": "sigil-top-local",
            "network": self.network,
            "height": top,
            "synced_to": s.blocks_synced,
            "tip": s.peer_best_height,
            "base": s.base,
            "downloaded": s.blocks_synced.saturating_sub(s.base),
            "fetched": s.fetched_total,
            "verified": s.verified,
            "peers": s.peer_count,
            "mesh_peers": s.mesh_peer_count,
            "status": status,
            "verify_break": s.verify_break,
            "cortex": { "loops": cx.loops, "gain_pct": cx.last_gain_pct },
        });
        Some(v.to_string())
    }

    fn recent(&self, query: &str) -> Option<String> {
        let kind = qparam(query, "kind").unwrap_or_else(|| "block".into());
        if kind != "block" {
            return None; // txs live on the remote node — proxy
        }
        let limit = qparam(query, "limit").and_then(|l| l.parse::<usize>().ok()).unwrap_or(40).min(200);
        let s = self.sync_snapshot()?;
        let top = Self::local_top(&s);
        let rows = self.reader.recent_from(top, limit);
        if rows.is_empty() {
            return None; // nothing local yet — proxy so the explorer isn't blank
        }
        Some(rows_json(&rows))
    }

    fn search(&self, query: &str) -> Option<String> {
        let q = qparam(query, "q").unwrap_or_default();
        if q.trim().len() < 2 {
            return None;
        }
        let s = self.sync_snapshot()?;
        let top = Self::local_top(&s);
        let rows = self.reader.search(q.trim(), top);
        if rows.is_empty() {
            return None; // let the remote node try tx / address / full-text
        }
        let results: Vec<_> = rows.iter().map(|r| serde_json::json!({
            "title": format!("Block #{}", r.h),
            "id": r.hash,
            "snippet": format!("verified spine · {} tx{} · b3:{}…",
                r.tx_count, if r.tx_count == 1 { "" } else { "s" }, &r.cid[..r.cid.len().min(10)]),
            "kind": "block",
        })).collect();
        Some(serde_json::json!({ "results": results, "source": "sigil-top-local" }).to_string())
    }

    fn aether(&self, query: &str) -> Option<String> {
        let cid = qparam(query, "cid").unwrap_or_default();
        if cid.is_empty() {
            return None;
        }
        // Local content-address verify (verify-don't-trust): re-derive the block hash.
        let row = self.reader.aether_verify(&cid)?;
        Some(serde_json::json!({
            "found": true,
            "verified": row.verified,
            "title": format!("Block #{}", row.h),
            "content": format!("local verified spine · producer {} · {} tx · height {}",
                if row.prod.is_empty() { "—" } else { &row.prod }, row.tx_count, row.h),
            "source": "sigil-top-local",
        }).to_string())
    }

    fn cortex_json(&self) -> String {
        let cx = self.cortex.lock().ok().map(|g| g.clone()).unwrap_or_default();
        // keep the summary small for the panel
        let summary: String = cx.summary.chars().take(400).collect();
        serde_json::json!({
            "active": cx.loops > 0,
            "loops": cx.loops,
            "gain_pct": cx.last_gain_pct,
            "tool": if cx.last_tool.is_empty() { "flux_cortex_loop" } else { cx.last_tool.as_str() },
            "summary": summary,
            "source": "sigil-top-local",
        }).to_string()
    }

    fn peers(&self) -> Option<String> {
        let s = self.sync_snapshot()?;
        let mut peers = Vec::new();
        if s.connected_epsilon { peers.push(serde_json::json!({ "name": "Epsilon", "kind": "bootstrap" })); }
        if s.connected_delta { peers.push(serde_json::json!({ "name": "Delta", "kind": "bootstrap" })); }
        let extra = (s.peer_count as i64) - peers.len() as i64;
        if extra > 0 {
            peers.push(serde_json::json!({ "name": format!("+{extra} mesh"), "kind": "mesh" }));
        }
        Some(serde_json::json!({
            "results": peers,
            "peer_count": s.peer_count,
            "mesh_peers": s.mesh_peer_count,
            "source": "sigil-top-local",
        }).to_string())
    }
}

fn rows_json(rows: &[BlockRow]) -> String {
    let results: Vec<_> = rows.iter().map(|r| serde_json::json!({
        "h": r.h,
        "hash": r.hash,
        "prod": r.prod,
        "cid": r.cid,
        "tx_count": r.tx_count,
        "verified": r.verified,
    })).collect();
    serde_json::json!({ "results": results, "source": "sigil-top-local" }).to_string()
}
