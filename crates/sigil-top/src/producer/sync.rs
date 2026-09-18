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

/// The full-block request. A local mirror of sigil-node's `BackfillReq` rather than
/// `crate::block_sync::BackfillReq`, because the light engine's struct has no `zstd`
/// field and every one of its call sites would have to grow one. Old servers ignore
/// unknown JSON keys, so `zstd: true` against a pre-09-13 node is simply not honoured.
#[derive(serde::Serialize)]
struct FullBackfillReq {
    from: u64,
    to: u64,
    headers_only: bool,
    codec: u8,
    /// 2026-09-13 node wire: ask for the reply as ONE zstd frame (`SIGILZ1`). Measured
    /// on Epsilon: 1,024 full blocks = 3.5 MB raw MessagePack / ~0.7 MB zstd, and the
    /// 8 s request-response timeout bounds the SEND of the reply, so the uncompressed
    /// shape was being cut off mid-body on the WireGuard link.
    zstd: bool,
}

/// A locally-typed mirror of sigil-node's `BackfillResp` (full-block reply). We
/// deliberately do NOT reuse `crate::block_sync::BackfillResp` here: that struct's
/// `blocks` field is `Vec<serde_json::Value>` (sigil-top's existing header-only
/// sync engine has never needed to deserialize a real block body), which would
/// throw away exactly the typed data this bridge needs.
#[derive(serde::Deserialize)]
struct FullBackfillResp {
    blocks: Vec<Block>,
}

// The node's full-block reply framing (`sigil-node/src/serve_read.rs`, 2026-09-11/13):
// an 8-byte magic, then the body. `SIGILM1` = MessagePack `BackfillResp` (bincode cannot
// decode a block that carries an internally-tagged event — the 09-11 incident);
// `SIGILZ1` = one zstd frame over the same MessagePack body; `SIGILB1` alone = the
// server is throttled, ask again later at the SAME size. Anything else is the legacy
// raw-bincode shape a pre-09-11 node still serves.
//
// 2026-09-17 (unified binary): the shipped v10.0.3 producer decoded EVERY reply as raw
// bincode, so the magic was read as a u64 block count of ~1.4e16 and the decoder ran off
// the end of the buffer — "unexpected end of file", six retries, refusing to start. No
// downloaded sigil-top had produced a block since the node's wire moved.
const BACKFILL_BODY_MAGIC: &[u8; 8] = b"SIGILM1\0";
const BACKFILL_ZSTD_MAGIC: &[u8; 8] = b"SIGILZ1\0";
const BACKFILL_BUSY_MAGIC: &[u8; 8] = b"SIGILB1\0";

/// What one reply decoded to.
enum Decoded {
    Blocks(Vec<Block>),
    /// `SIGILB1`: the server is throttled right now. Not an error, not a smaller ask.
    Busy,
}

/// Decode a full-block backfill reply in any of the node's shapes. Pure-Rust zstd
/// (ruzstd) so the Windows cross-build stays mingw-clean, same as the light engine.
fn decode_full_backfill(bytes: &[u8]) -> Result<Decoded, String> {
    if bytes == BACKFILL_BUSY_MAGIC {
        return Ok(Decoded::Busy);
    }
    if let Some(body) = bytes.strip_prefix(BACKFILL_BODY_MAGIC) {
        return rmp_serde::from_slice::<FullBackfillResp>(body)
            .map(|r| Decoded::Blocks(r.blocks))
            .map_err(|e| format!("msgpack: {e}"));
    }
    if let Some(body) = bytes.strip_prefix(BACKFILL_ZSTD_MAGIC) {
        let raw = crate::block_sync::verify::zstd_decompress_body(body)
            .ok_or_else(|| "zstd: frame did not inflate (malformed or over the 64 MiB guard)".to_string())?;
        return rmp_serde::from_slice::<FullBackfillResp>(&raw)
            .map(|r| Decoded::Blocks(r.blocks))
            .map_err(|e| format!("msgpack (zstd): {e}"));
    }
    bincode::deserialize::<FullBackfillResp>(bytes)
        .map(|r| Decoded::Blocks(r.blocks))
        .map_err(|e| format!("legacy bincode: {e}"))
}

const SNAPSHOT_URL_ENV: &str = "SIGIL_TOP_SNAPSHOT_URL";
const SIGNER_PK_ENV: &str = "SIGIL_TOP_SNAPSHOT_SIGNER_PK_HEX";
const DEFAULT_SNAPSHOT_URL: &str = "http://89.149.241.126:18183/snapshot";
/// The g2 node's snapshot signing key (SQIsign L5, `snapshot-sqisign.pk` generated
/// 2026-08-29 with the g2 genesis; read from the node's aether dir and checked against the
/// key embedded in the served snapshot on 2026-09-17). Pinned here so a fresh install
/// verifies the snapshot's SOURCE by default, not just its transit. `SIGIL_TOP_SNAPSHOT_
/// SIGNER_PK_HEX` overrides it (a chain reset rotates this key).
const DEFAULT_SNAPSHOT_SIGNER_PK_HEX: &str = "89e240db908474bf18a8f36efc815fde946017d22379481a5fffca6869cfa31cbe8380e148a4b8e53cc93582e2ccc4731fd2ddcc49614dddbdd20e7618366e007151e62a7bbcb204ed8a74d570db4a14125e0b61fae35972dc0e6b4bd957220c37f9c6bde7297c689f5638b63a58f127dd39e67f85e540c4b317a7dff182870102";

/// The server's full-block serve cap is 1,024 since 2026-09-13 (`SIGIL_SERVE_FULL_CAP`,
/// sigil-node main.rs) — a bigger ask is silently clamped, so ask for exactly what one
/// reply can carry. The replay loop advances by `chain.height()`, never by this number.
const TAIL_CHUNK: u64 = 1024;
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
    let expected_pk = match std::env::var(SIGNER_PK_ENV) {
        Ok(h) => {
            let pk = hex_decode(&h);
            if pk.is_none() {
                crate::tlog!("[producer-sync] ⚠ {SIGNER_PK_ENV} is not valid hex — refusing to fetch an unpinned snapshot");
                return None;
            }
            pk
        }
        Err(_) => hex_decode(DEFAULT_SNAPSHOT_SIGNER_PK_HEX),
    };
    // Never fetch unpinned: a snapshot is 48 MB of state this node will PRODUCE on top of.
    let expected_pk = expected_pk?;

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

    let snap = match sigil_node::snapshot::verify_snapshot_bytes(&bytes, Some(&expected_pk)) {
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
    let payload = match serde_json::to_vec(&FullBackfillReq { from, to, headers_only: false, codec: 0, zstd: true }) {
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
            Ok(Ok(bytes)) => match decode_full_backfill(&bytes) {
                Ok(Decoded::Blocks(blocks)) => return Some(blocks),
                Ok(Decoded::Busy) => {
                    crate::tlog!(
                        "[producer-sync] peer busy (throttled) for [{from}..{to}] (attempt {attempt}/{CHUNK_RETRIES}) — asking again, same size"
                    );
                }
                Err(e) => {
                    crate::tlog!(
                        "[producer-sync] ⚠ decode BackfillResp [{from}..{to}] failed (attempt {attempt}/{CHUNK_RETRIES}): {e} ({} bytes, head {:02x?}) — retrying",
                        bytes.len(),
                        &bytes[..bytes.len().min(8)]
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
    super::status::set_message(format!("backfill [{from}..{to}] failed {CHUNK_RETRIES}× — peer or network down"));
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
///
/// 2026-09-18: also the mid-run REPAIR the networked loop calls when its settled chain
/// has frozen behind the gossip tip (`run.rs`, the resync lane) — it is the same
/// height-addressed catch-up the bootstrap trusts, so a frozen node recovers along the
/// network's spine instead of waiting for parents that will never be gossiped again.
pub(super) async fn tail_replay(net: &flux_p2p::NetworkManager, chain: &mut ChainTip) -> Option<u64> {
    let mut waited = 0u32;
    while net.connected_peers().is_empty() && waited < PEER_WAIT_TRIES {
        tokio::time::sleep(PEER_WAIT_STEP).await;
        waited += 1;
    }
    if net.connected_peers().is_empty() {
        crate::tlog!("[producer-sync] ⚠ tail replay: no peers connected after {waited} tries — cannot sync");
        super::status::set_message("no peers connected — cannot replay the chain");
        return None;
    }

    let mut applied = 0u64;
    let mut stall_rounds = 0u32;
    super::status::set_phase(super::status::SYNCING);
    loop {
        let from = chain.height();
        let to = from + TAIL_CHUNK - 1;
        super::status::set_replayed(from);

        let blocks = match fetch_chunk_with_retry(net, from, to, applied).await {
            Some(b) => b,
            None => return None,
        };

        if blocks.is_empty() {
            crate::tlog!(
                "[producer-sync] tail replay caught up at height={} ({applied} blocks replayed)",
                chain.height()
            );
            super::status::set_replayed(chain.height());
            return Some(applied);
        }

        // The server clamps every reply to what it has: fewer blocks than asked means
        // this reply reached ITS tip. That is the only honest "caught up" on a chain
        // that never stops growing — waiting for an EMPTY reply (the pre-09-17 rule)
        // chased the tip forever at 8+ blk/s, and the node answers a range that starts
        // above its tip with silence, not an empty list.
        let short = (blocks.len() as u64) < TAIL_CHUNK;
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
                    super::status::set_message(format!("block h={h} failed to apply: {e}"));
                    return None;
                }
            }
        }
        if short && made_progress {
            crate::tlog!(
                "[producer-sync] tail replay caught up at height={} ({applied} blocks replayed; last reply was short)",
                chain.height()
            );
            super::status::set_replayed(chain.height());
            return Some(applied);
        }
        stall_rounds = if made_progress { 0 } else { stall_rounds + 1 };
        if stall_rounds >= 3 {
            crate::tlog!("[producer-sync] ⚠ no progress for 3 consecutive rounds — refusing to start ({applied} blocks were replayed before this)");
            super::status::set_message(format!("no progress for 3 rounds at h={}", chain.height()));
            return None;
        }
    }
}

/// The full sync-then-produce bootstrap: snapshot bootstrap → tail replay via `net`
/// up to the live mesh tip. `net` must already be started (`.start().await` called).
/// On ANY failure returns `None` — the caller must refuse to start producing rather
/// than fall back to a fresh, network-incompatible genesis.
pub async fn sync_chain(net: &flux_p2p::NetworkManager) -> Option<ChainTip> {
    // GENESIS-UP IS THE DEFAULT (2026-08-28, operator: "i dont want snapshots. just sync
    // from scratch. it syncs 5-10kblks so its fine").
    //
    // Two reasons, and the second is the important one:
    //
    // 1. It is fast enough. Backfill runs at 5-10k blk/s, so ~264k blocks is well under a
    //    minute — while the snapshot path downloads 94 MB and THEN still tail-replays tens
    //    of thousands of blocks, which in practice timed out repeatedly against the live
    //    node's expensive-serve throttle. Measured 2026-08-28: a four-minute snapshot boot
    //    never reached the tip and never mined a single share.
    //
    // 2. THE SNAPSHOT IS NOT AUTHENTICATED unless the operator happens to have set
    //    SIGIL_TOP_SNAPSHOT_SIGNER_PK_HEX — otherwise `fetch_snapshot` logs "signer
    //    identity is NOT pinned, only transit corruption is checked" and restores 94 MB of
    //    state from an unverified source anyway. For a project whose whole claim is
    //    verify-don't-trust, bootstrapping every fresh node from an unpinned blob is the
    //    wrong default. Replaying from genesis over the p2p mesh verifies every block.
    //
    // Snapshot boot remains available for anyone who wants it — set SIGIL_TOP_SNAPSHOT=1 —
    // and is still the right tool once the chain is long enough that genesis replay hurts.
    // If you do use it, PIN THE SIGNER.
    // 2026-09-17 (unified binary): SNAPSHOT + TAIL IS THE DEFAULT AGAIN. The 08-28 ruling
    // above was made at 264k blocks; the chain is past 20,000,000 and the replay MEASURED
    // 1,900 blk/s against Epsilon (1,024 full blocks per request, the node's cap) — 2.9 h
    // per start, and the producer's state lives only in memory, so every restart paid it
    // again. Reason 2 is answered by pinning the signer (`DEFAULT_SNAPSHOT_SIGNER_PK_HEX`):
    // the snapshot is now authenticated by default, and the tail from its height to the
    // tip is still replayed block by block through `chain.apply`. `SIGIL_TOP_SNAPSHOT=0`
    // keeps the genesis-up path for anyone who wants to pay for it.
    let want_snapshot = !matches!(std::env::var("SIGIL_TOP_SNAPSHOT").as_deref(), Ok("0"));
    let mut chain = if want_snapshot {
        let url = std::env::var(SNAPSHOT_URL_ENV).unwrap_or_else(|_| DEFAULT_SNAPSHOT_URL.to_string());
        let c = fetch_snapshot(&url).await?;
        crate::tlog!("[producer-sync] snapshot restored at height={} — starting tail replay", c.height());
        c
    } else {
        // Genesis is applied locally from `mint::build_genesis`, never fetched, so the
        // starting point is the one compiled into this binary rather than one a peer
        // asserted. Everything above it arrives through the same verified replay path the
        // snapshot route uses for its tail.
        let mut c = ChainTip::new();
        let g = match crate::producer::mint::build_genesis() {
            Ok(g) => g,
            Err(e) => {
                crate::tlog!("[producer-sync] ✗ could not build genesis locally: {e}");
                return None;
            }
        };
        if let Err(e) = c.apply(g) {
            crate::tlog!("[producer-sync] ✗ genesis failed to apply: {e:?}");
            return None;
        }
        crate::tlog!("[producer-sync] genesis-up sync (no snapshot) — replaying from height 0");
        c
    };
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

    /// The node's three reply shapes (sigil-node `serve_read.rs`), byte-for-byte as it
    /// frames them, plus the legacy one — every shape must hand back the same genesis
    /// block. Encoded here with the SAME libraries the node uses (rmp_serde
    /// `to_vec_named`, C zstd level 3) and decoded with the shipped pure-Rust path.
    #[test]
    fn decodes_every_node_backfill_shape() {
        #[derive(serde::Serialize)]
        struct Resp { blocks: Vec<Block> }
        let genesis = super::super::mint::build_genesis().expect("genesis");
        let want = genesis.header.height;
        let resp = Resp { blocks: vec![genesis.clone()] };
        let body = rmp_serde::to_vec_named(&resp).unwrap();

        let mut msgpack = BACKFILL_BODY_MAGIC.to_vec();
        msgpack.extend_from_slice(&body);
        let mut zst = BACKFILL_ZSTD_MAGIC.to_vec();
        zst.extend_from_slice(&zstd::encode_all(&body[..], 3).unwrap());
        let legacy = bincode::serialize(&resp).unwrap();

        for (name, bytes) in [("SIGILM1", msgpack), ("SIGILZ1", zst), ("legacy", legacy)] {
            match decode_full_backfill(&bytes) {
                Ok(Decoded::Blocks(b)) => {
                    assert_eq!(b.len(), 1, "{name}");
                    assert_eq!(b[0].header.height, want, "{name}");
                    assert_eq!(b[0].hash(), genesis.hash(), "{name}: block must survive the trip");
                }
                other => panic!("{name}: expected one block, got {}", match other { Ok(Decoded::Busy) => "BUSY".to_string(), Err(e) => e, _ => unreachable!() }),
            }
        }
        assert!(matches!(decode_full_backfill(BACKFILL_BUSY_MAGIC), Ok(Decoded::Busy)));
        // Garbage is an error, not a panic and not "zero blocks".
        assert!(decode_full_backfill(b"SIGILM1\0not-msgpack").is_err());
        assert!(decode_full_backfill(&[]).is_err());
    }

    /// The pinned default signer must be the 129-byte SQIsign key the node embeds in its
    /// snapshot — a typo here would make every fresh install refuse to boot.
    #[test]
    fn default_snapshot_signer_is_a_sqisign_pubkey() {
        let pk = hex_decode(DEFAULT_SNAPSHOT_SIGNER_PK_HEX).expect("valid hex");
        assert_eq!(pk.len(), 129, "SQIsign L5 public key length");
    }

    /// The request must carry `zstd: true` under that exact key — the node reads it by
    /// name (`#[serde(default)] zstd: bool`); a misspelling silently means "raw".
    #[test]
    fn request_asks_for_zstd_by_name() {
        let v: serde_json::Value = serde_json::from_slice(
            &serde_json::to_vec(&FullBackfillReq { from: 1, to: 2, headers_only: false, codec: 0, zstd: true }).unwrap(),
        ).unwrap();
        assert_eq!(v["zstd"], serde_json::Value::Bool(true));
        assert_eq!(v["headers_only"], serde_json::Value::Bool(false));
        assert_eq!(v["from"], 1);
    }
}
