//! dagknight.rs — a read-only, periodically-refreshed snapshot of the
//! braid's recent GHOSTDAG structure, for display purposes (the DagKnight
//! visualization) — NOT a consensus type, never fed back into anything.
//!
//! `Braid` lives as a plain local variable inside sigil-node's single-
//! threaded producer event loop — it is never wrapped in a lock, and it
//! must stay that way (wrapping the live consensus object in a mutex just
//! to let an HTTP handler peek at it would put lock contention on the hot
//! path). Instead: the event loop itself, on a slow timer (a few seconds),
//! calls `Braid::recent_summary()` (a cheap in-memory copy, no I/O) and
//! writes the *result* into this bridge — the only thing that's ever
//! locked is this small, already-copied snapshot, and only ever briefly.
//! See sigil-node's dag-snapshot tick for the write side.

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use sigil_dagknight::BlockSummary;

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// The current snapshot as handed to callers — includes when it was taken
/// so a stale/never-updated snapshot (dag_mode off, or the tick hasn't
/// fired yet) is honestly distinguishable from a fresh one.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct DagSnapshot {
    pub blocks: Vec<BlockSummary>,
    /// GHOSTDAG cluster-bound `k`, or `None` if v2 coloring isn't active
    /// (v1 braid mode — `blue_score`/`is_blue` on every block are then
    /// just `0`/`false`, not "unknown").
    pub k: Option<u32>,
    /// `0` until the first tick fires — the caller can tell "no data yet"
    /// (dag_mode off, or brand new producer) apart from a real snapshot.
    pub updated_at_ms: u64,
}

/// Always constructed (cheap, inert until the producer's dag-snapshot tick
/// starts writing to it) — same "always-on bridge" shape as
/// `MiningBridge`/`SendBridge`.
#[derive(Default)]
pub struct DagSnapshotBridge {
    inner: Mutex<DagSnapshot>,
}

impl DagSnapshotBridge {
    pub fn new() -> Self {
        Self::default()
    }

    /// Called from the producer's event loop only (see module doc) — never
    /// from an HTTP handler.
    pub fn update(&self, blocks: Vec<BlockSummary>, k: Option<u32>) {
        let snap = DagSnapshot { blocks, k, updated_at_ms: now_ms() };
        if let Ok(mut g) = self.inner.lock() {
            *g = snap;
        }
    }

    pub fn get(&self) -> DagSnapshot {
        self.inner.lock().map(|g| g.clone()).unwrap_or_default()
    }
}
