//! producer/sync.rs — the "sync-then-produce" bridge (2026-08-24, operator-directed:
//! "work on unifying the sigil top node so that i can produce blocks and actually is
//! a real node ... every user downloading sigil top will be full node operator").
//!
//! `producer::run::maybe_start()` used to start every producer from a fresh, empty
//! genesis-only chain — safe in isolation, but wrong for a REAL node: it can never
//! see any of the state that already exists on the live network, so every block it
//! mints is built on top of a chain the rest of sigil-g0 doesn't recognize (a silent
//! fork, not a real participant). This module is the fix: before minting anything,
//! pull the real chain — genesis through the current tip, WITH FULL STATE — from a
//! running node, the same shape as how a Quillon node bootstraps.
//!
//! Two phases:
//!  1. **Snapshot bootstrap**: HTTP GET a running node's `/snapshot` endpoint (added
//!     to `ingest.rs` this session) — a BLAKE3+SQIsign-signed `StateSnapshot`. Never
//!     trusted until `sigil_node::snapshot::verify_snapshot_bytes` passes.
//!  2. **Tail replay**: whatever blocks were minted after the snapshot was taken,
//!     fetched FULL (not headers-only) via the same point-to-point `BackfillReq` wire
//!     sigil-top's light-client engine already speaks for header sync — every
//!     existing call site hardcodes `headers_only: true`, so `headers_only: false` is
//!     new wiring here, not a duplicate of existing logic.
//!
//! Safety rule (deliberate, non-negotiable): if EITHER phase fails, this returns
//! `None` and the caller MUST refuse to start producing. A producer that silently
//! fell back to a fresh local genesis on a sync failure would mint blocks for a
//! chain nobody else on the network recognizes — never let that happen automatically.

use std::time::Duration;

use sigil_node::block::Block;
use sigil_node::chain::ChainTip;

use crate::block_sync::BackfillReq;

/// A locally-typed mirror of sigil-node's `BackfillResp` (full-block reply). We
/// deliberately do NOT reuse `crate::block_sync::BackfillResp` here: that struct's
/// `blocks` field is `Vec<serde_json::Value>` (sigil-top's existing header-only
/// sync engine has never needed to deserialize a real block body), which would
/// throw away exactly the typed data this bridge needs. The wire shape — one
/// `blocks` field, bincode-encoded, no leading tag byte — is confirmed against
/// sigil-node's own `main.rs` serve handler for `headers_only: false`.
#[derive(serde::Deserialize)]
struct FullBackfillResp {
    blocks: Vec<Block>,
}

const SNAPSHOT_URL_ENV: &str = "SIGIL_TOP_SNAPSHOT_URL";
const SIGNER_PK_ENV: &str = "SIGIL_TOP_SNAPSHOT_SIGNER_PK_HEX";
const DEFAULT_SNAPSHOT_URL: &str = "http://89.149.241.126:18183/snapshot";

/// Kept under the server's 8192-full-block serve cap (`ingest`/backfill handler in
/// sigil-node's main.rs) with headroom for full block bodies being much larger than
/// headers.
const TAIL_CHUNK: u64 = 2048;
const REQ_TIMEOUT: Duration = Duration::from_secs(30);
const PEER_WAIT_TRIES: u32 = 20;
const PEER_WAIT_STEP: Duration = Duration::from_millis(500);

/// 2026-08-25 (root-caused live): sigil-node's own backfill handler runs a
/// per-peer + global throttle on EXPENSIVE (full-block) serves —
/// `SIGIL_SERVE_EXPENSIVE_THROTTLE_MS`, 120ms default — and a throttled
/// request is silently DROPPED, never queued (see main.rs's comment at that
/// throttle: "safe, because every caller here already retries on its own
/// cadence"). This module was not retrying at all, so any drop — whether
/// from this peer's own throttle window or from cross-peer contention on the
/// shared global floor, both real and observed live on a busy producer —
/// looked identical to a genuine network failure and aborted sync entirely.
/// `RETRY_BACKOFF` is comfortably above the 120ms default throttle window so
/// a retry lands outside it even under load.
const CHUNK_RETRIES: u32 = 6;
const RETRY_BACKOFF: Duration = Duration::from_millis(400);

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.is_empty() || s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// Fetch + verify + restore a signed state snapshot from `url`. Returns `None` on
/// ANY failure (network, verification, or decode) — never hands back a partially
/// trusted chain.
async fn fetch_snapshot(url: &str) -> Option<ChainTip> {
    let expected_pk = std::env::var(SIGNER_PK_ENV).ok().and_then(|h| hex_decode(&h));
    if expected_pk.is_none() {
        crate::tlog!(
            "[producer-sync] ⚠ {SIGNER_PK_ENV} unset — snapshot signer identity is NOT pinned, \
             only transit corruption is checked, not source authenticity. Set it to the \
             producer's SIGIL_PRODUCER_SQISIGN_PK_HEX to pin it."
        );
    }

    let bytes = match reqwest::Client::new().get(url).timeout(Duration::from_secs(120)).send().await {
        Ok(r) if r.status().is_success() => match r.bytes().await {
            Ok(b) => b.to_vec(),
            Err(e) => {
                crate::tlog!("[producer-sync] ⚠ snapshot body read failed: {e}");
                return None;
            }
        },
        Ok(r) => {
            crate::tlog!("[producer-sync] ⚠ snapshot fetch {url} → HTTP {}", r.status());
            return None;
        }
        Err(e) => {
            crate::tlog!("[producer-sync] ⚠ snapshot fetch {url} failed: {e}");
            return None;
        }
    };

    let snap = match sigil_node::snapshot::verify_snapshot_bytes(&bytes, expected_pk.as_deref()) {
        Some(s) => s,
        None => {
            crate::tlog!("[producer-sync] ⚠ snapshot from {url} failed verification — refusing it");
            return None;
        }
    };
    crate::tlog!(
        "[producer-sync] snapshot verified: height={} base={} ({} bytes)",
        snap.snapshot_height,
        snap.base_height,
        bytes.len()
    );
    Some(snap.restore())
}

/// Fetch one `[from..=to]` chunk, retrying up to `CHUNK_RETRIES` times with
/// `RETRY_BACKOFF` between attempts. A retry re-reads the peer list each time
/// (not just the request) since the set of connected peers can change between
/// attempts. Returns `None` only once every retry has failed — at that point
/// this really does look like a broken peer or a dead network, not a throttle
/// drop the caller was supposed to shrug off.
async fn fetch_chunk_with_retry(
    net: &flux_p2p::NetworkManager,
    from: u64,
    to: u64,
    applied_so_far: u64,
) -> Option<Vec<Block>> {
    let payload = match serde_json::to_vec(&BackfillReq { from, to, headers_only: false, codec: 0 }) {
        Ok(p) => p,
        Err(e) => {
            // A local serialization failure is not a network hiccup — retrying
            // it would just fail the same way every time.
            crate::tlog!("[producer-sync] ⚠ encode BackfillReq [{from}..{to}] failed: {e} — refusing to start ({applied_so_far} blocks were replayed before this)");
            return None;
        }
    };

    for attempt in 1..=CHUNK_RETRIES {
        let peer = match net.connected_peers().first().cloned() {
            Some(p) => p,
            None => {
                crate::tlog!("[producer-sync] ⚠ tail replay: no connected peers (attempt {attempt}/{CHUNK_RETRIES}) at height={from}");
                tokio::time::sleep(RETRY_BACKOFF).await;
                continue;
            }
        };

        let outcome = tokio::time::timeout(REQ_TIMEOUT, net.send_request(peer, payload.clone())).await;
        match outcome {
            Ok(Ok(bytes)) => match bincode::deserialize::<FullBackfillResp>(&bytes) {
                Ok(r) => return Some(r.blocks),
                Err(e) => {
                    crate::tlog!(
                        "[producer-sync] ⚠ decode BackfillResp [{from}..{to}] failed (attempt {attempt}/{CHUNK_RETRIES}): {e} — \
                         likely the server's per-peer/global expensive-serve throttle dropped this request; retrying"
                    );
                }
            },
            Ok(Err(e)) => {
                crate::tlog!("[producer-sync] ⚠ request [{from}..{to}] failed (attempt {attempt}/{CHUNK_RETRIES}): {e} — retrying");
            }
            Err(_) => {
                crate::tlog!(
                    "[producer-sync] ⚠ request [{from}..{to}] timed out (attempt {attempt}/{CHUNK_RETRIES}) — \
                     likely the server's per-peer/global expensive-serve throttle dropped this request; retrying"
                );
            }
        }
        if attempt < CHUNK_RETRIES {
            tokio::time::sleep(RETRY_BACKOFF).await;
        }
    }

    crate::tlog!(
        "[producer-sync] ⚠ request [{from}..{to}] failed after {CHUNK_RETRIES} attempts — refusing to start \
         ({applied_so_far} blocks were replayed before this)"
    );
    None
}

/// Tail-replay from `chain.height()` up to whatever the connected mesh has, fetching
/// FULL blocks (not headers) and applying each one in order. `net` must already be
/// started. Returns the count of blocks applied (0 is a valid, successful "already
/// at tip" outcome) ONLY when the server told us it has nothing more to serve — i.e.
/// we are genuinely caught up. `None` on EVERY other exit — no peers, a lost
/// connection mid-sync, a malformed/undecodable response, a request timeout, a block
/// that fails to apply, or three consecutive rounds with no progress.
///
/// 2026-08-25 (live-test finding): the first version of this function returned
/// `Some(applied)` — "success" — on every one of those error paths too, just with
/// whatever partial height replay happened to reach. Caught live: a real snapshot +
/// tail-replay run hit a mid-stream decode error after only 4,096 of the real ~85,000
/// missing blocks and the caller went on to start producing anyway, from a height
/// tens of thousands of blocks behind the actual tip — exactly the "silent fork by
/// construction" this module's own top-level doc warns against. A stall/error partway
/// through is not a lesser form of success; it is indistinguishable from "the peer or
/// the network broke," and `sync_chain`'s caller must refuse to start on it exactly as
/// it would refuse on a snapshot fetch failure.
async fn tail_replay(net: &flux_p2p::NetworkManager, chain: &mut ChainTip) -> Option<u64> {
    let mut waited = 0u32;
    while net.connected_peers().is_empty() && waited < PEER_WAIT_TRIES {
        tokio::time::sleep(PEER_WAIT_STEP).await;
        waited += 1;
    }
    if net.connected_peers().is_empty() {
        crate::tlog!("[producer-sync] ⚠ tail replay: no peers connected after {waited} tries — cannot sync");
        return None;
    }

    let mut applied = 0u64;
    let mut stall_rounds = 0u32;
    loop {
        let from = chain.height();
        let to = from + TAIL_CHUNK - 1;

        let blocks = match fetch_chunk_with_retry(net, from, to, applied).await {
            Some(b) => b,
            None => return None,
        };

        if blocks.is_empty() {
            crate::tlog!(
                "[producer-sync] tail replay caught up at height={} ({applied} blocks replayed)",
                chain.height()
            );
            return Some(applied);
        }

        let mut made_progress = false;
        for b in blocks {
            let h = b.header.height;
            match chain.apply(b) {
                Ok(()) => {
                    applied += 1;
                    made_progress = true;
                }
                Err(e) => {
                    crate::tlog!(
                        "[producer-sync] ⚠ apply block h={h} failed: {e} — refusing to start (stopped at height={}, {applied} blocks replayed before this)",
                        chain.height()
                    );
                    return None;
                }
            }
        }
        stall_rounds = if made_progress { 0 } else { stall_rounds + 1 };
        if stall_rounds >= 3 {
            crate::tlog!("[producer-sync] ⚠ no progress for 3 consecutive rounds — refusing to start ({applied} blocks were replayed before this)");
            return None;
        }
    }
}

/// The full sync-then-produce bootstrap: snapshot bootstrap → tail replay via `net`
/// up to the live mesh tip. `net` must already be started (`.start().await` called).
/// On ANY failure returns `None` — the caller must refuse to start producing rather
/// than fall back to a fresh, network-incompatible genesis.
pub async fn sync_chain(net: &flux_p2p::NetworkManager) -> Option<ChainTip> {
    let url = std::env::var(SNAPSHOT_URL_ENV).unwrap_or_else(|_| DEFAULT_SNAPSHOT_URL.to_string());
    let mut chain = fetch_snapshot(&url).await?;
    crate::tlog!("[producer-sync] snapshot restored at height={} — starting tail replay", chain.height());
    let applied = tail_replay(net, &mut chain).await?;
    crate::tlog!(
        "[producer-sync] sync-then-produce bootstrap complete: height={} ({applied} blocks replayed after snapshot)",
        chain.height()
    );
    Some(chain)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_decode_roundtrips_and_rejects_garbage() {
        assert_eq!(hex_decode("00ff"), Some(vec![0x00, 0xff]));
        assert_eq!(hex_decode(""), None);
        assert_eq!(hex_decode("f"), None); // odd length
        assert_eq!(hex_decode("zz"), None); // not hex
    }
}
