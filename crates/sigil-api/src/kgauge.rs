//! The K-gauge, computed BY THE NODE and published with its inputs.
//!
//! # Why this module exists
//!
//! `flux-kgauge` has been a complete, careful crate for a while: `KGauge::observe` takes an
//! [`Observables`] and returns a [`GaugeReport`] carrying `k_base`, `k_enhanced`, a phase, and —
//! the part that matters most — [`Caveat`]s naming every channel it could NOT answer. What it
//! never had was a caller on the node.
//!
//! Instead each consumer rebuilt the measurement for itself: the fluxc MCP handler fetched
//! `/v1/dagknight/recent` and the counters and assembled its own `Observables`; sigil-top's TUI
//! did its own; the browser dropdown did a third. Three clients, three windows, three answers, and
//! no way to say which one the chain actually had — because K was never a property of the node,
//! only an opinion a client formed about it.
//!
//! That is the difference between a quantity that is *measured* and a quantity that is a *term*.
//! A term is computed once, at the source, from stated inputs, and published so anyone can check
//! the arithmetic. This module makes K that.
//!
//! # The honesty rule, restated in code
//!
//! The response publishes the INPUTS next to the value — both counter samples, the wall-clock
//! window that separated them, the block window's size and producer diversity, and the assumed
//! network size. A reader can recompute `k` from them and disagree with this node in public. A
//! bare number cannot be argued with, which is exactly what makes a bare number untrustworthy.
//!
//! Two inputs this node genuinely does not have are reported as ZERO rather than filled with a
//! plausible-looking substitute: `p2p_bytes_in` / `p2p_bytes_out`. The mesh summary counts
//! gossip MESSAGES, not bytes, and messages are not bytes. Feeding a proxy would produce a number
//! that moves, looks alive, and measures something other than what it claims — the failure the
//! K-parameter work already hit once on the Quillon gauge, where a block-rate deviation stood in
//! for proposer entropy and left the instrument blind to the very churn it was meant to see. The
//! gauge's own machinery answers "unavailable" for a channel with no data, and `caveats` in the
//! response says so out loud. Fixing it means counting bytes in `flux-p2p`, not guessing here.
//!
//! # Windowing
//!
//! K is defined over a window, so the first call after start establishes a baseline and publishes
//! no value — `ready: false` — rather than inventing a zero-length window. Every later call
//! measures from the previous call. That makes the cadence the caller's choice and keeps the node
//! from carrying a timer it does not need.

use std::sync::{Mutex, OnceLock};

use flux_kgauge::observables::{BlockFact, BlockWindow, CounterSample, NetworkSize, Observables};
use flux_kgauge::{GaugeConfig, GaugeReport, KGauge};

/// DAG-Knight `kappa`. Zero on SIGIL today: the anticone channel it scales is reported as
/// unavailable rather than asserted, matching what every existing client already passes. When the
/// braid publishes a real cluster-bound k, this is where it gets read from.
const SIGIL_KAPPA: f64 = 0.0;

struct Inner {
    gauge: KGauge,
    /// The previous sample and the millisecond it was taken — the other end of the window.
    last: Option<(CounterSample, u64)>,
}

fn inner() -> &'static Mutex<Inner> {
    static I: OnceLock<Mutex<Inner>> = OnceLock::new();
    I.get_or_init(|| {
        Mutex::new(Inner { gauge: KGauge::new(GaugeConfig::default()), last: None })
    })
}

/// A gauge reading together with the observables it was computed from.
pub struct Reading {
    pub report: GaugeReport,
    pub observables: Observables,
}

/// Pure windowing: given the previous sample and its timestamp, build the observables for the
/// current one. Returns `None` when there is no window yet, or when it would be zero-length.
///
/// Split out of [`observe`] deliberately so the windowing rules are testable WITHOUT the
/// process-global gauge. Two tests sharing one `OnceLock` order-depend on each other: whichever
/// runs second finds a baseline the first one left behind, and "the first sample publishes
/// nothing" then passes or fails depending on test-harness scheduling. This codebase has already
/// paid for that lesson once, in the mining bridge's shared-env lock.
pub fn observables_for(
    previous: Option<(CounterSample, u64)>,
    current: CounterSample,
    window: BlockWindow,
    network_size: NetworkSize,
    now_ms: u64,
) -> Option<Observables> {
    let (previous, prev_ms) = previous?;
    let window_secs = now_ms.saturating_sub(prev_ms) as f64 / 1000.0;
    if window_secs <= 0.0 {
        return None;
    }
    Some(Observables { previous, current, window_secs, window, network_size, kappa: SIGIL_KAPPA })
}

/// Take a sample against this process's gauge. Returns `None` on the first call (baseline only)
/// and whenever the window would be zero-length — a refusal to publish a number, not a failure.
pub fn observe(
    current: CounterSample,
    window: BlockWindow,
    network_size: NetworkSize,
    now_ms: u64,
) -> Option<Reading> {
    let mut g = inner().lock().ok()?;
    let previous = g.last.replace((current.clone(), now_ms));
    let observables = observables_for(previous, current, window, network_size, now_ms)?;
    let report = g.gauge.observe(&observables);
    Some(Reading { report, observables })
}

/// Map the node's own recent-block snapshot into the gauge's minimal block facts.
pub fn window_from_blocks(blocks: &[sigil_dagknight::braid::BlockSummary]) -> BlockWindow {
    BlockWindow::new(
        blocks
            .iter()
            .map(|b| BlockFact {
                height: b.height,
                producer: b.producer,
                merge_parent_count: b.merge_parents.len() as u32,
                blue_score: Some(b.blue_score),
                is_blue: Some(b.is_blue),
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(h: u64, peers: u64) -> CounterSample {
        CounterSample {
            mining_submitted: h * 2,
            mining_accepted: h,
            p2p_bytes_in: 0,
            p2p_bytes_out: 0,
            peer_count: peers,
            local_height: h,
            network_height: h,
        }
    }

    /// A window needs two ends. The first sample must publish NOTHING — a gauge that answers
    /// before it has a window is reporting the shape of its own initialisation, which is the
    /// most convincing kind of wrong number.
    #[test]
    fn first_sample_establishes_a_baseline_and_publishes_no_value() {
        let first = observables_for(
            None, sample(100, 2), BlockWindow::default(), NetworkSize::Estimated(3), 1_000,
        );
        assert!(first.is_none(), "the first sample has no window to be measured over");

        let second = observables_for(
            Some((sample(100, 2), 1_000)), sample(160, 2),
            BlockWindow::default(), NetworkSize::Estimated(3), 61_000,
        )
        .expect("the second sample closes a 60 s window");
        assert!((second.window_secs - 60.0).abs() < 1e-9, "window is wall-clock, not assumed");
        assert_eq!(second.previous.local_height, 100, "inputs are published, not just the value");
        assert_eq!(second.current.local_height, 160);
    }

    /// A zero-length window is a division by zero wearing a suit. Refuse it.
    #[test]
    fn a_zero_length_window_publishes_nothing() {
        let same_ms = observables_for(
            Some((sample(10, 1), 5_000)), sample(20, 1),
            BlockWindow::default(), NetworkSize::Estimated(2), 5_000,
        );
        assert!(same_ms.is_none(), "two samples at the same millisecond define no window");
    }

    /// The unmeasured channels must stay unmeasured. If someone later wires `messages_processed`
    /// into `p2p_bytes_*` because it "looks close enough", this fails and says why.
    #[test]
    fn p2p_byte_counters_are_reported_as_absent_not_approximated() {
        let s = sample(100, 2);
        assert_eq!(s.p2p_bytes_in, 0);
        assert_eq!(s.p2p_bytes_out, 0);
    }
}
