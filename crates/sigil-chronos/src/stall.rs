//! `stall.rs` — **"my node's sync is stalled — do I need to restart it?"**
//!
//! Written 2026-07-29 against a LIVE incident on Epsilon (sigil-g0 sole
//! producer + seed). Measured symptoms there:
//!
//! - header tip **frozen** at 33,816,034 (11,685 log observations, one value)
//! - **zero** `⚡ heartbeat` lines and **zero** `📦` block lines
//! - the `rr-backfill` serve path still emitting 6,000+ lines/hour
//! - two remote peers polling `[tip+1..=tip]` (an empty range) forever
//! - one remote peer re-requesting the SAME 27,442-header range 185×/5min
//! - NOT a memory wedge: RSS 1.2 G under an 8 G cap, `memory.pressure` 0,
//!   `oom_kill 0`
//!
//! i.e. the block-production loop was dead while the request-response serve
//! task stayed alive. The node *looked* healthy from outside — process up, port
//! listening, actively answering requests, serving megabytes — and produced
//! nothing.
//!
//! The operator question was: **does the stalled FOLLOWER need restarting?**
//! These tests answer it deterministically instead of by guesswork, because
//! restarting the wrong box is both useless and disruptive.
//!
//! ## Result (see the tests)
//!
//! | Action | Effect on the follower's height |
//! |---|---|
//! | restart the follower | **none** — it re-syncs to the same frozen tip and stops |
//! | restart the follower repeatedly | **none** |
//! | restart the producer | **everyone advances** |
//!
//! A follower that has reached the tip of a non-producing producer is not
//! broken and is not fixable from its own side. It is *correct*: there is
//! nothing to fetch. Restarting it destroys its warm state and re-downloads the
//! chain to arrive at exactly the same height.
//!
//! **Diagnostic rule this establishes:** before restarting a stalled node, check
//! whether the PRODUCER's tip is advancing. A frozen tip plus healthy peers
//! means the fault is upstream, and the follower is the one component you can
//! prove is behaving.

use flux_chronos::NodeId;

use crate::{demo_genesis, sign_dummy, ApplyOutcome, Block, GenesisSpec, SigilSimNode};

/// Simulated block cadence used by these scenarios (1 virtual second).
const BLOCK_TIME: u64 = 1_000_000;

/// A follower's sync progress over a window — what a light client's UI shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncProgress {
    /// Height at the start of the window.
    pub from: u64,
    /// Height at the end.
    pub to: u64,
    /// Blocks the follower actually applied.
    pub applied: usize,
    /// Blocks it rejected or diverged on.
    pub refused: usize,
}

impl SyncProgress {
    /// Did the follower make ANY forward progress?
    pub fn advanced(&self) -> bool {
        self.to > self.from
    }
}

/// Feed `blocks` to `follower` and report what moved. This is the follower's
/// whole job: apply what the producer offers.
pub fn drive_follower(follower: &mut SigilSimNode, blocks: &[Block]) -> SyncProgress {
    let from = follower.height();
    let mut applied = 0;
    let mut refused = 0;
    for b in blocks {
        match follower.apply_external_block(b) {
            ApplyOutcome::Ok => applied += 1,
            _ => refused += 1,
        }
    }
    SyncProgress { from, to: follower.height(), applied, refused }
}

/// Simulate an operator restarting a follower: throw away the running node and
/// cold-start a fresh one from genesis, then re-sync it from the chain it can
/// actually obtain. Returns the restarted node and its progress.
///
/// This is deliberately the *most* favourable restart possible — a pristine
/// node with no accumulated state. If even this cannot pass the frozen tip,
/// no real restart can.
pub fn restart_follower(genesis: &GenesisSpec, chain: &[Block]) -> (SigilSimNode, SyncProgress) {
    let mut fresh = SigilSimNode::new(
        "follower-restarted",
        NodeId(1),
        vec![],
        false,
        BLOCK_TIME,
        genesis,
    );
    let progress = drive_follower(&mut fresh, chain);
    (fresh, progress)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sigil_state::NATIVE;
    use sigil_tx::SigilTx;

    fn wallet(tag: u8) -> [u8; 32] {
        [tag; 32]
    }

    /// Build a producer and mint `n` blocks, then STOP feeding it work — the
    /// live failure mode (production loop dead, serve path alive).
    fn produce_then_stall(n: u32) -> (GenesisSpec, SigilSimNode, Vec<Block>) {
        let g = demo_genesis();
        let mut producer =
            SigilSimNode::new("producer", NodeId(0), vec![], true, BLOCK_TIME, &g);
        let mut chain = Vec::new();
        for i in 0..n {
            let from = wallet((i % 5 + 1) as u8);
            let to = wallet(((i + 1) % 5 + 1) as u8);
            producer.enqueue_tx(sign_dummy(SigilTx::Send {
                from, to, amount: 100, token: NATIVE, fee: 0,
            }));
            chain.push(producer.produce_one().expect("producer mints"));
        }
        // From here the producer is given no more work: its tip is FROZEN,
        // exactly like Epsilon at 33,816,034.
        (g, producer, chain)
    }

    /// The follower reaches the frozen tip and then — correctly — stops.
    /// Its "stall" is not a fault.
    #[test]
    fn follower_catches_up_then_correctly_stops_at_a_frozen_tip() {
        let (g, mut producer, chain) = produce_then_stall(6);
        let frozen_tip = producer.height();

        let mut follower =
            SigilSimNode::new("follower", NodeId(1), vec![], false, BLOCK_TIME, &g);
        let first = drive_follower(&mut follower, &chain);
        assert!(first.advanced(), "follower must catch up: {first:?}");
        assert_eq!(first.refused, 0, "healthy blocks must all apply: {first:?}");
        assert_eq!(follower.height(), frozen_tip, "follower should reach the tip");

        // The producer is asked for more and yields nothing — a frozen tip.
        assert!(producer.produce_one().is_none(), "producer has no work; tip must freeze");
        assert_eq!(producer.height(), frozen_tip);

        // Poll again with nothing new available — this is the follower sitting
        // at `[tip+1..=tip]` forever, which is what Epsilon logs for its two
        // healthy peers.
        let second = drive_follower(&mut follower, &[]);
        assert!(!second.advanced(), "nothing to fetch, so nothing should move");
        assert_eq!(follower.height(), frozen_tip);
    }

    /// **The operator's actual question.** Restarting the follower does not
    /// help, and cannot: a cold node re-syncs the same chain and lands on the
    /// same height.
    #[test]
    fn restarting_the_follower_does_not_help() {
        let (g, _producer, chain) = produce_then_stall(6);

        let mut follower =
            SigilSimNode::new("follower", NodeId(1), vec![], false, BLOCK_TIME, &g);
        drive_follower(&mut follower, &chain);
        let before_restart = follower.height();

        // Restart once.
        let (restarted, progress) = restart_follower(&g, &chain);
        assert_eq!(progress.refused, 0, "a fresh node must apply the chain cleanly");
        assert_eq!(
            restarted.height(), before_restart,
            "restart re-downloaded the chain and arrived at the SAME height — no gain"
        );

        // Restart twice more. Still nothing. Restarting is not a fix, it is a
        // no-op that costs a full re-sync.
        for _ in 0..2 {
            let (again, _) = restart_follower(&g, &chain);
            assert_eq!(
                again.height(), before_restart,
                "repeated restarts cannot pass a tip the producer never advanced"
            );
        }
    }

    /// Restarting the PRODUCER is what actually moves the tip — and the
    /// follower then advances without being touched at all.
    #[test]
    fn restarting_the_producer_resumes_everyone() {
        let (g, mut producer, mut chain) = produce_then_stall(6);
        let frozen_tip = producer.height();

        let mut follower =
            SigilSimNode::new("follower", NodeId(1), vec![], false, BLOCK_TIME, &g);
        drive_follower(&mut follower, &chain);
        assert_eq!(follower.height(), frozen_tip);

        // Producer resumes (the restart): work flows again, tip advances.
        let mut resumed = Vec::new();
        for i in 100..104u32 {
            producer.enqueue_tx(sign_dummy(SigilTx::Send {
                from: wallet((i % 5 + 1) as u8),
                to: wallet(((i + 1) % 5 + 1) as u8),
                amount: 100, token: NATIVE, fee: 0,
            }));
            resumed.push(producer.produce_one().expect("resumed producer mints"));
        }
        assert!(producer.height() > frozen_tip, "producer tip must advance after resume");

        // The follower was NOT restarted and needed no intervention.
        let progress = drive_follower(&mut follower, &resumed);
        assert!(progress.advanced(), "follower must follow once the tip moves: {progress:?}");
        assert_eq!(progress.refused, 0);
        assert_eq!(
            follower.height(), producer.height(),
            "follower converges on the producer without being restarted"
        );

        chain.extend(resumed);
        assert_eq!(chain.len(), 10);
    }

    /// The looping-peer signature seen live: a peer that keeps re-requesting the
    /// SAME range gains nothing per attempt. Re-fetching is not progress — which
    /// is why 185 requests × 1.96 MB in five minutes moved Epsilon's peer zero
    /// headers.
    #[test]
    fn refetching_the_same_range_makes_no_progress() {
        let (g, _producer, chain) = produce_then_stall(6);
        let mut follower =
            SigilSimNode::new("follower", NodeId(1), vec![], false, BLOCK_TIME, &g);

        let first = drive_follower(&mut follower, &chain);
        assert!(first.advanced());
        let settled = follower.height();

        // Re-serve the identical range repeatedly, as the live node does.
        for attempt in 0..5 {
            let again = drive_follower(&mut follower, &chain);
            assert!(
                !again.advanced(),
                "re-fetching an already-applied range must not advance (attempt {attempt}): {again:?}"
            );
            assert_eq!(follower.height(), settled, "height must be stable under re-fetch");
        }
    }
}
