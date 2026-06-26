//! sigil-syncwire — V7-INTEGRATE lane (agent: v7-integrate)
//!
//! The integration spine that turns the four independently-built v7 sync stages
//! (serve / fetch+verify / commit / db-ingest) plus the [`sigil_synctune`] control plane
//! into **one** working pipeline, and closes the online-refinement loop.
//!
//! It owns nothing the other lanes own. It only:
//!   * [`rategate`] — constructs the **single** shared [`RateGate`] (a
//!     [`BackpressureSpine`]) every stage admits from, and exposes thin call-site
//!     helpers so the block_sync lease-holders need only a one-line seam patch, not
//!     an internals rewrite.
//!   * [`telemetry`] — the [`TelemetryBus`]: stages push cheap, lock-free, cache-line
//!     padded counters; each control tick the bus folds them into [`RawStage`] samples,
//!     drives sigil-synctune's [`OnlineTuner::tick`] (consumed, not re-implemented),
//!     applies the returned [`KnobSet`], and adjusts the spine's admission rate via a
//!     small AIMD admission governor (the `spine.set_rate` actuator `OnlineTuner` leaves
//!     to the integration layer). This is the live wiring sigil-synctune had no node for.
//!
//! Everything is generic over the injectable [`sigil_synctune::Clock`], so the whole
//! crate — including the closed control loop — is deterministic and testable in virtual
//! time, exactly like sigil-synctune and flux-chronos.
//!
//! ## Two AIMD loops, one telemetry source — why they don't fight
//! The synctune `AutoTuneController` does AIMD on the *structural* knobs (window,
//! substreams, rayon, sst-batch, ring). The admission governor here does AIMD on the
//! *rate ceiling* the spine hands out. They share the same congestion signal but act on
//! orthogonal actuators (shape vs. pace), and the governor's ceiling is clamped to
//! [`TARGET_BLK_S`] so it can only ever *relieve* overrun, never push the pipeline past
//! its tuned target. See `telemetry.rs` for the anti-oscillation discipline (single
//! shared `congested` definition, multiplicative-decrease-only on the rate).

pub mod rategate;
pub mod telemetry;

pub use rategate::{as_gate, new_shared_spine, SharedSpine};
pub use telemetry::{StageStat, TelemetryBus};

// Re-export the control-plane surface the call-sites and lanes code against, so a stage only
// needs `use sigil_syncwire::*` to get the shared vocabulary. The control loop itself is
// sigil-synctune's `OnlineTuner` — we consume it, not re-implement it.
pub use sigil_synctune::{
    BackpressureSpine, Clock, KnobSet, OnlineTuner, RateGate, RawStage, RealClock, Stage,
    VirtualClock, TARGET_BLK_S,
};
