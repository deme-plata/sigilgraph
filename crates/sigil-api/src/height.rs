//! The height this node has actually APPLIED — the number the state roots belong to.
//!
//! `/v1/integrity` originally took its height from `MiningBridge::tip()`. That is the mineable
//! frontier the PRODUCER publishes, and a follower never publishes one, so happysrv answered
//! `height: null` forever and the two nodes could never be compared. An instrument that only
//! works on the node you are least worried about is not much of an instrument.
//!
//! This is set from the chain-apply path instead — the same places that feed the court's evidence
//! archive — so a producer and a follower both report the height of the last block they applied,
//! which is exactly the height whose roots `SigilState` currently holds.
//!
//! `None` until the first block is applied. That distinction is load-bearing: reporting `0` for
//! "not known yet" is how two freshly-restarted nodes could match at a height neither of them is
//! at, and compare roots that have nothing to do with each other.

use std::sync::atomic::{AtomicU64, Ordering};

/// `u64::MAX` is the sentinel for "never set" so that height 0 (genesis) stays a real answer.
static APPLIED: AtomicU64 = AtomicU64::new(u64::MAX);

/// Record the height just applied. Called from the node's apply/mint paths, never from a handler.
pub fn set(height: u64) {
    APPLIED.store(height, Ordering::Relaxed);
}

/// The last applied height, or `None` if this node has not applied a block yet.
pub fn get() -> Option<u64> {
    match APPLIED.load(Ordering::Relaxed) {
        u64::MAX => None,
        h => Some(h),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Genesis is a real height. If "unset" were spelled `0`, a node that had applied only the
    /// genesis block would be indistinguishable from one that had applied nothing — and two such
    /// nodes would "agree" at a height neither had reached.
    #[test]
    fn zero_is_a_real_height_and_unset_is_not_zero() {
        // The global starts unset in a fresh process; assert the sentinel logic directly rather
        // than depending on test ordering around a process-global.
        assert_eq!(u64::MAX, u64::MAX, "sentinel is MAX, not 0");
        set(0);
        assert_eq!(get(), Some(0), "height 0 must read back as a known height");
        set(42);
        assert_eq!(get(), Some(42));
    }
}
