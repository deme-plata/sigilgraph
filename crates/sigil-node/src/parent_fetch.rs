//! Fetch missing DAG parents BY HASH (2026-10-07, mainnet roadmap step 1).
//!
//! ## Why this exists
//!
//! Catch-up has always been by HEIGHT: "send me blocks `[from..=to]`", answered
//! from the server's own settled chain. With ONE producer that chain IS the
//! DAG, so every parent a block names can be fetched by its height.
//!
//! With TWO producers it is not. Producer B's blocks are merge parents in A's
//! blocks, but they are side blocks on A's spine, so they never appear in A's
//! settled chain and no height-range request can return them. Measured on the
//! 4-node rehearsal net on 2026-10-07 with a second producer: the followers
//! froze at height 1,111,487 (`parent_hash mismatch`), and the two producers
//! each gave up on the other's blocks (`evict_stale_pending`), finalized their
//! own strand, and ended up with two different finalized histories (heights
//! ~20,000 apart, different supply).
//!
//! The braid already knows exactly which hashes it is waiting for
//! (`InsertOutcome::MissingParents`, `Braid::missing_parents`). This module
//! adds the missing verb: ask a peer for those blocks by hash, and the peer
//! answers from its DAG body store, which holds side blocks as well as spine
//! blocks.
//!
//! ## Wire
//!
//! `BackfillReq.by_hash: Option<Vec<String>>` (lower-case hex). An old server
//! ignores the unknown field and serves its `[from..=to]` range instead, so the
//! by-hash request sets an empty range (`from = 1, to = 0`) and an old server
//! answers with nothing rather than with unrelated blocks. The response is the
//! ordinary `BackfillResp`.
//!
//! This is a sync change, not a consensus change: no block, header or state
//! rule is touched.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use sigil_header::BlockHash;

/// Most hashes served or requested in one by-hash round trip. A full block is
/// ~1.6 KB raw, so 64 is ~100 KB: far inside the request-response timeout.
pub const BY_HASH_MAX: usize = 64;

/// How long a requested hash is considered "in flight" before it may be asked
/// for again. Long enough to absorb a round trip, short enough that a lost
/// reply is retried quickly.
pub const BY_HASH_RETRY: Duration = Duration::from_secs(2);

/// Lower-case hex of a block hash, the wire form.
pub fn to_hex(h: &BlockHash) -> String {
    hex::encode(h)
}

/// Parse a requester's hex list: malformed entries are skipped, duplicates are
/// dropped, order is kept, and at most [`BY_HASH_MAX`] hashes are returned.
pub fn parse_hashes(hexes: &[String]) -> Vec<BlockHash> {
    let mut out: Vec<BlockHash> = Vec::new();
    for s in hexes {
        if out.len() >= BY_HASH_MAX {
            break;
        }
        let Ok(bytes) = hex::decode(s.trim()) else { continue };
        let Ok(h) = <BlockHash>::try_from(bytes.as_slice()) else { continue };
        if !out.contains(&h) {
            out.push(h);
        }
    }
    out
}

/// Look the requested hashes up with `get` — and walk back through their parents
/// (`parent_hash` and `merge_parents`) and returns every ancestor the server holds, up to
/// `limit` blocks in total, OLDEST FIRST so the requester can insert them in order.
///
/// 2026-10-08, measured after a 40 s split on the GRANDPA rehearsal: each half had built
/// ~300 blocks the other never received. One-block answers walk that back one parent per
/// round trip — ~6 blocks/s against a branch growing 8 blocks/s — so the halves never
/// re-merged and finality could not resume. With ancestors, one answer carries up to
/// `limit` blocks. An old requester simply receives more of what it was missing.
pub fn collect_with_ancestors<B: Clone>(
    hashes: &[BlockHash],
    get: impl Fn(&BlockHash) -> Option<B>,
    parents_of: impl Fn(&B) -> Vec<BlockHash>,
    height_of: impl Fn(&B) -> u64,
    limit: usize,
) -> Vec<B> {
    let mut seen: std::collections::HashSet<BlockHash> = std::collections::HashSet::new();
    let mut queue: std::collections::VecDeque<BlockHash> = hashes.iter().copied().collect();
    let mut out: Vec<B> = Vec::new();
    while let Some(h) = queue.pop_front() {
        if out.len() >= limit {
            break;
        }
        if !seen.insert(h) {
            continue;
        }
        if let Some(b) = get(&h) {
            queue.extend(parents_of(&b));
            out.push(b);
        }
    }
    out.sort_by_key(|b| height_of(b));
    out
}

/// Requester-side dedupe: the same missing parent is reported by every child
/// that names it, many times a second. Without this, one gap becomes a request
/// storm.
#[derive(Debug, Default)]
pub struct InFlight {
    asked_at: HashMap<BlockHash, Instant>,
}

impl InFlight {
    pub fn new() -> Self {
        Self::default()
    }

    /// Of `wanted`, the hashes not already asked for within [`BY_HASH_RETRY`],
    /// capped at [`BY_HASH_MAX`]. Every hash returned is marked as asked at `now`.
    pub fn take_new(&mut self, wanted: &[BlockHash], now: Instant) -> Vec<BlockHash> {
        let mut out = Vec::new();
        for h in wanted {
            if out.len() >= BY_HASH_MAX {
                break;
            }
            let fresh = match self.asked_at.get(h) {
                Some(t) => now.saturating_duration_since(*t) >= BY_HASH_RETRY,
                None => true,
            };
            if fresh && !out.contains(h) {
                self.asked_at.insert(*h, now);
                out.push(*h);
            }
        }
        // Bound the map: anything older than a few retry periods is forgotten.
        if self.asked_at.len() > 8 * BY_HASH_MAX {
            let horizon = BY_HASH_RETRY * 4;
            self.asked_at
                .retain(|_, t| now.saturating_duration_since(*t) < horizon);
        }
        out
    }

    /// A hash arrived: it is no longer in flight.
    pub fn arrived(&mut self, h: &BlockHash) {
        self.asked_at.remove(h);
    }

    pub fn len(&self) -> usize {
        self.asked_at.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(b: u8) -> BlockHash {
        [b; 32]
    }

    #[test]
    fn parse_skips_malformed_dedups_and_caps() {
        let mut input = vec![
            to_hex(&h(1)),
            "not hex".to_string(),
            "abcd".to_string(), // valid hex, wrong length
            to_hex(&h(1)),      // duplicate
            to_hex(&h(2)),
        ];
        assert_eq!(parse_hashes(&input), vec![h(1), h(2)]);

        input = (0..200u32)
            .map(|i| {
                let mut x = [0u8; 32];
                x[..4].copy_from_slice(&i.to_le_bytes());
                hex::encode(x)
            })
            .collect();
        assert_eq!(parse_hashes(&input).len(), BY_HASH_MAX);
    }

    #[test]
    fn ancestors_come_back_oldest_first_and_stop_at_the_limit() {
        // A chain 1 <- 2 <- 3 <- 4 plus a side block 9 merged into 4.
        let store: HashMap<BlockHash, (u64, Vec<BlockHash>)> = [
            (h(1), (1, vec![])),
            (h(2), (2, vec![h(1)])),
            (h(3), (3, vec![h(2)])),
            (h(9), (3, vec![h(2)])),
            (h(4), (4, vec![h(3), h(9)])),
        ]
        .into_iter()
        .collect();
        let get = |k: &BlockHash| store.get(k).map(|v| (*k, v.clone()));
        let parents = |b: &(BlockHash, (u64, Vec<BlockHash>))| b.1 .1.clone();
        let height = |b: &(BlockHash, (u64, Vec<BlockHash>))| b.1 .0;
        let all = collect_with_ancestors(&[h(4)], get, parents, height, 64);
        let hs: Vec<u64> = all.iter().map(|b| b.1 .0).collect();
        assert_eq!(hs, vec![1, 2, 3, 3, 4], "oldest first, side block included, no duplicates");
        let capped = collect_with_ancestors(&[h(4)], get, parents, height, 2);
        assert_eq!(capped.len(), 2);
        assert_eq!(capped.last().map(|b| b.0), Some(h(4)), "the asked-for block is always in, the cap trims ancestry");
    }

    #[test]
    fn in_flight_suppresses_repeats_until_the_retry_period_passes() {
        let mut f = InFlight::new();
        let t0 = Instant::now();
        assert_eq!(f.take_new(&[h(1), h(2), h(1)], t0), vec![h(1), h(2)]);
        // The same gap reported again a moment later: nothing new to ask.
        assert!(f.take_new(&[h(1), h(2)], t0 + Duration::from_millis(100)).is_empty());
        // After the retry period a lost reply is asked for again.
        assert_eq!(f.take_new(&[h(1)], t0 + BY_HASH_RETRY), vec![h(1)]);
        // An arrived block is no longer in flight.
        f.arrived(&h(2));
        assert_eq!(f.take_new(&[h(2)], t0 + Duration::from_millis(200)), vec![h(2)]);
    }

    #[test]
    fn in_flight_caps_one_request_at_the_wire_limit() {
        let mut f = InFlight::new();
        let wanted: Vec<BlockHash> = (0..200u32)
            .map(|i| {
                let mut x = [0u8; 32];
                x[..4].copy_from_slice(&i.to_le_bytes());
                x
            })
            .collect();
        assert_eq!(f.take_new(&wanted, Instant::now()).len(), BY_HASH_MAX);
    }
}
