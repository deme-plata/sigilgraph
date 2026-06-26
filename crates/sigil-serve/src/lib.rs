//! `sigil-serve` — the SIGIL v7 **producer / serve** side of block sync (lane
//! V7-SUPPLY, swarm #644). Target: feed ≥100k blocks/sec sustained.
//!
//! Two pieces, both LIBRARY-only (the live binary gets thin call-site seams):
//!
//! 1. [`serve_pipeline`] — the codec=2/3/4 snapshot wire producer. Headline:
//!    [`build_trailer`] fixes the **codec=4 finalize activation blocker** by
//!    hashing exactly the client's requested `[from..=to]` (never the live tip),
//!    and [`ArchiveRootCache`] memoizes the immutable finalized prefix so the
//!    finalize is O(tail), not O(chain), per request.
//!
//! 2. [`serve_stream`] — windowed, ACK-paced, multi-substream page streaming:
//!    independent per-substream credit windows (no single bufferbloating window),
//!    redundancy=2 striped fan-out with retransmit (not duplication), client
//!    dedup via [`PageBitmap`], and an acquire-before-serve [`RateGate`] seam for
//!    v7-autotune's token-bucket spine.
//!
//! Both are transport-free and node-free so the crate builds + tests standalone.
//! Seams: sigil-node/main.rs codec=4 (rocky-sync-B), sigil-net WG substreams as
//! [`ChunkSink`] (rocky-sync-A), sigil-synctune token bucket as [`RateGate`]
//! (viktor-v7-coord).

pub mod serve_pipeline;
pub mod serve_stream;

pub use serve_pipeline::{
    archive_root_range, build_trailer, fold_record, skeleton_page, snapshot_header,
    ArchiveRootCache, BlockSkeletonSource, DEFAULT_CKPT_INTERVAL,
};
pub use serve_stream::{
    drive_round, redundancy_for_loss, AlwaysAdmit, ChunkSink, CoRecorder, PageBitmap, PageId,
    RoundOutcome, Send, ServeStreamPlanner, StreamConfig, StreamError,
};

// Re-export the global backpressure spine surface the serve loop codes against,
// so callers depend only on sigil-serve. The shared `BackpressureSpine` is the
// ONE gate that keeps serve from overrunning v7-ingest's commit/db stages.
pub use sigil_synctune::{BackpressureSpine, Clock, RateGate, Stage, TARGET_BLK_S};
