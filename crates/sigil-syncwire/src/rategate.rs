//! The single shared admission gate + thin call-site seam helpers.
//!
//! Task (T2): build ONE shared `Arc<dyn RateGate>` —
//! `BackpressureSpine::new(RealClock, TARGET_BLK_S, burst, Stage::COUNT)` — and thread it
//! into the live sync loop as a *thin* seam. The lease-holders of `block_sync/*`
//! (fetch/verify/commit) and the serve/ingest lanes do not need to know the concrete clock
//! type or own any rate math; they take an `Arc<dyn RateGate>` and call [`admit`] /
//! [`admit_or_wait_ns`] before processing a batch.

use std::sync::Arc;

use sigil_synctune::{BackpressureSpine, RateGate, RealClock, Stage, TARGET_BLK_S};

/// The concrete shared spine type. Held by the [`crate::TelemetryBus`] (which needs the
/// concrete `set_rate`); handed to stages as `Arc<dyn RateGate>` via [`as_gate`].
pub type SharedSpine = Arc<BackpressureSpine<RealClock>>;

/// Build the one production spine the whole pipeline shares.
///
/// `burst_blocks` is the max instantaneous burst (bucket capacity). A good default is one
/// fetch window-depth's worth of blocks so a full window can be admitted at once without
/// the gate becoming the bottleneck; the autotune controller then sizes the window.
pub fn new_shared_spine(burst_blocks: u32) -> SharedSpine {
    Arc::new(BackpressureSpine::new(
        Arc::new(RealClock::new()),
        TARGET_BLK_S,
        burst_blocks,
        Stage::COUNT,
    ))
}

/// Coerce the concrete spine into the trait object stages code against. Cheap `Arc` clone.
pub fn as_gate<C: sigil_synctune::Clock + 'static>(
    spine: &Arc<BackpressureSpine<C>>,
) -> Arc<dyn RateGate> {
    spine.clone()
}

/// Seam helper for an `async` call-site: try to admit `n` blocks; on success return `None`,
/// otherwise return `Some(wait_ns)` — the caller decides how to wait (e.g.
/// `tokio::time::sleep(Duration::from_nanos(ns))`) so this crate stays runtime-agnostic and
/// dependency-free. Re-call after waiting. This is the *entire* contract a stage adopts.
#[inline]
pub fn admit_or_wait_ns(gate: &Arc<dyn RateGate>, n: u32) -> Option<u64> {
    if gate.admit(n) {
        None
    } else {
        Some(gate.admit_wait_nanos(n))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sigil_synctune::VirtualClock;

    #[test]
    fn shared_spine_admits_burst_then_throttles() {
        // Use a virtual-clock spine directly to assert deterministic admission semantics.
        let clk = Arc::new(VirtualClock::new());
        let spine = Arc::new(BackpressureSpine::new(
            clk.clone(),
            TARGET_BLK_S,
            1000,
            Stage::COUNT,
        ));
        let gate: Arc<dyn RateGate> = spine.clone();

        // burst of 1000 admits, then drained
        assert_eq!(admit_or_wait_ns(&gate, 1000), None);
        let w = admit_or_wait_ns(&gate, 100).expect("should be throttled when drained");
        assert!(w > 0, "drained gate must report a positive wait, got {w}");

        // refill at 100k blk/s: 1ms -> 100 blocks
        clk.advance_ms(1);
        assert_eq!(admit_or_wait_ns(&gate, 100), None);
        assert_eq!(gate.admit_rate(), TARGET_BLK_S);
    }

    #[test]
    fn as_gate_coerces_concrete_spine() {
        let clk = Arc::new(VirtualClock::new());
        let spine = Arc::new(BackpressureSpine::new(clk, TARGET_BLK_S, 8, Stage::COUNT));
        let g = as_gate(&spine);
        assert_eq!(g.admit_rate(), TARGET_BLK_S);
        // both handles point at the same bucket: draining via the gate is visible via spine
        assert!(g.admit(8));
        assert!(!spine.try_acquire(1));
    }
}
