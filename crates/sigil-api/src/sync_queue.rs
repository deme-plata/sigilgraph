//! What this node's history downloader is doing right now, for sigil-top's Queues tab (`GET /v1/sync/queue`).
//!
//! 2026-10-23 (Viktor: "queue stopped working the table is empty while syncing"): since 10.6.0 the node does the sync,
//! but the tab still drew the light monitor's queue, which sits nearly idle beside a full node. The downloader now
//! notes each range it asks for (from which peer, since when, how many "busy" re-asks) and each one it finishes.
//! Bookkeeping only: the downloader behaves the same whether or not anyone reads it — one short lock per event.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;

use serde::Serialize;

/// A range asked for and not answered yet.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct InFlight {
    pub from: u64,
    pub to: u64,
    pub peer: String,
    /// Wall-clock milliseconds when it was asked for.
    pub since_ms: u64,
    /// How many times the peer answered "busy" and it was asked again.
    pub busy: u32,
}

/// A range that came back (`blocks > 0`) or failed (`blocks == 0`).
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Done {
    pub from: u64,
    pub to: u64,
    pub peer: String,
    /// How long it was in flight.
    pub ms: u64,
    pub blocks: u64,
    pub ok: bool,
    /// Wall-clock milliseconds when it finished.
    pub at_ms: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Snapshot {
    pub in_flight: Vec<InFlight>,
    /// Newest first.
    pub done: Vec<Done>,
    /// The current request size (it halves on failures, grows back on success).
    pub chunk: u64,
    pub received_blocks: u64,
    pub failed_ranges: u64,
    pub now_ms: u64,
}

/// Finished ranges kept (newest first) — about 20 s of a fast sync.
pub const DONE_KEEP: usize = 256;

#[derive(Default)]
struct State {
    in_flight: BTreeMap<(u64, u64), InFlight>,
    done: VecDeque<Done>,
    chunk: u64,
    received: u64,
    failed: u64,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(State::default))
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// The downloader asked `peer` for `[from, to)` with request size `chunk`.
pub fn started(from: u64, to: u64, peer: &str, chunk: u64) {
    with(|s| {
        s.chunk = chunk;
        s.in_flight.insert((from, to), InFlight { from, to, peer: peer.to_string(), since_ms: now_ms(), busy: 0 });
    })
}

/// The peer answered "busy" and the range is asked again.
pub fn busy(from: u64, to: u64) {
    with(|s| {
        if let Some(r) = s.in_flight.get_mut(&(from, to)) {
            r.busy += 1;
        }
    })
}

/// The range is back with `blocks` blocks (0 = it failed and will be asked again later).
pub fn finished(from: u64, to: u64, blocks: u64) {
    with(|s| {
        let Some(r) = s.in_flight.remove(&(from, to)) else { return };
        let now = now_ms();
        let ok = blocks > 0;
        if ok {
            s.received += blocks;
        } else {
            s.failed += 1;
        }
        s.done.push_front(Done { from, to, peer: r.peer, ms: now.saturating_sub(r.since_ms), blocks, ok, at_ms: now });
        s.done.truncate(DONE_KEEP);
    })
}

pub fn snapshot() -> Snapshot {
    with(|s| Snapshot {
        in_flight: s.in_flight.values().cloned().collect(),
        done: s.done.iter().cloned().collect(),
        chunk: s.chunk,
        received_blocks: s.received,
        failed_ranges: s.failed,
        now_ms: now_ms(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_range_moves_from_in_flight_to_done_with_its_peer_and_busy_count() {
        started(1_000, 2_000, "peerA", 1_000);
        busy(1_000, 2_000);
        busy(1_000, 2_000);
        let s = snapshot();
        let r = s.in_flight.iter().find(|r| r.from == 1_000).expect("in flight");
        assert_eq!((r.to, r.peer.as_str(), r.busy), (2_000, "peerA", 2));
        assert_eq!(s.chunk, 1_000);
        finished(1_000, 2_000, 1_000);
        finished(5_000, 6_000, 7); // never started: ignored
        let s = snapshot();
        assert!(!s.in_flight.iter().any(|r| r.from == 1_000));
        let d = s.done.iter().find(|d| d.from == 1_000).expect("done");
        assert_eq!((d.peer.as_str(), d.blocks, d.ok), ("peerA", 1_000, true));
        assert!(!s.done.iter().any(|d| d.from == 5_000));
        started(9_000, 9_100, "peerB", 100);
        finished(9_000, 9_100, 0);
        assert!(!snapshot().done.iter().find(|d| d.from == 9_000).unwrap().ok, "an empty answer is a failure");
    }
}
