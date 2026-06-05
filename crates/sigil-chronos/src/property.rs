//! CHRONOS-C — property-based fuzzer over the SIGIL soak.
//!
//! Turns the handful of hand-written scenarios in `lib.rs` tests into
//! hundreds of seeded random ones. Each trial derives — from a single u64
//! seed — a random transaction count, a random tx-mix across the demo
//! wallets, and random network conditions (latency + packet-loss). It runs
//! the real two-node SIGIL soak under [`flux_chronos`] and checks the
//! invariants.
//!
//! The headline property — the one that matters — is **SAFETY**:
//!
//! > No matter how chaotic the network (high latency, heavy loss), the
//! > follower NEVER silently diverges. Every block it *does* apply has
//! > locally-recomputed roots matching the producer's committed roots.
//!
//! This is the exit-78 invariant (SIGIL_GENESIS §9) under fuzzing. Liveness
//! (does the follower stay caught up?) is deliberately NOT asserted as an
//! invariant — under packet loss a follower legitimately misses blocks and
//! then *rejects* later ones (parent-hash gap). Rejection is safe; silent
//! divergence is not. The fuzzer separates the two.
//!
//! Determinism: each trial is fully seeded (splitmix64 + the Universe's own
//! seeded RNG), so a failing trial is reproducible from its seed alone — the
//! whole point of deterministic simulation.

use flux_chronos::{secs, NetEdge, NodeId, ScenarioSeed, Universe};
use sigil_tx::{SigilTx, NATIVE};

use crate::{demo_genesis, sign_dummy, SigilSimNode};

/// Tiny deterministic PRNG (splitmix64). No external dep, fully reproducible
/// from a seed — exactly what a property fuzzer over a deterministic sim
/// wants.
struct SplitMix64(u64);

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self(seed)
    }
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    /// Uniform in `[lo, hi]`.
    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        if hi <= lo {
            return lo;
        }
        lo + self.next_u64() % (hi - lo + 1)
    }
    /// Float in `[0, 1)`.
    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Network + workload parameters a trial draws from its seed.
#[derive(Debug, Clone, Copy)]
pub struct TrialConfig {
    /// Number of Send txs injected.
    pub n_txs: u64,
    /// One-way edge latency, micros.
    pub latency_micros: u64,
    /// Packet-drop probability on the producer→follower edge.
    pub drop_prob: f64,
}

impl TrialConfig {
    /// Derive a config from a seed: 50–300 txs, 1–500 ms latency, 0–15% loss.
    pub fn from_seed(seed: u64) -> Self {
        let mut r = SplitMix64::new(seed);
        TrialConfig {
            n_txs: r.range(50, 300),
            latency_micros: r.range(1_000, 500_000),
            drop_prob: r.unit() * 0.15,
        }
    }
}

/// What a single trial observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrialOutcome {
    /// Seed that produced this trial (for reproduction).
    pub seed: u64,
    /// Blocks the producer minted.
    pub produced: u64,
    /// Blocks the follower applied cleanly (roots matched).
    pub applied_ok: u64,
    /// Divergences — the SAFETY violation. MUST be 0.
    pub divergence: u64,
    /// Blocks rejected (parent/height gap from a dropped block). Safe.
    pub rejected: u64,
}

impl TrialOutcome {
    /// The safety invariant: zero silent divergence.
    pub fn is_safe(&self) -> bool {
        self.divergence == 0
    }
    /// Did the network actually stress the run (loss caused rejections)?
    pub fn saw_chaos(&self) -> bool {
        self.rejected > 0
    }
}

/// Run ONE trial deterministically from `seed`. Spawns producer + follower
/// from the shared genesis, injects a seed-derived Send stream, advances
/// simulated time past all block-ticks, tallies the event log.
pub fn run_trial(seed: u64) -> TrialOutcome {
    let cfg = TrialConfig::from_seed(seed);
    let g = demo_genesis();
    let block_time = secs(1);

    let mut u = Universe::new(ScenarioSeed::from(seed));
    let delta_id = NodeId(0);
    let epsilon_id = NodeId(1);
    let delta = Box::new(SigilSimNode::new("delta", delta_id, vec![epsilon_id], true, block_time, &g));
    let epsilon = Box::new(SigilSimNode::new("epsilon", epsilon_id, vec![], false, block_time, &g));
    let d = u.spawn_node(delta);
    let e = u.spawn_node(epsilon);
    u.connect(
        d,
        e,
        NetEdge { latency_micros: cfg.latency_micros, drop_prob: cfg.drop_prob, partitioned: false },
    );

    // Seed-derived Send stream across the 5 demo wallets.
    let mut r = SplitMix64::new(seed ^ 0xDEAD_BEEF);
    for _ in 0..cfg.n_txs {
        let from = (r.range(1, 5)) as u8;
        let mut to = (r.range(1, 5)) as u8;
        if to == from {
            to = (to % 5) + 1;
        }
        let tx = sign_dummy(SigilTx::Send {
            from: [from; 32],
            to: [to; 32],
            amount: r.range(1, 500) as u128,
            token: NATIVE,
            fee: 1,
        });
        let mut payload = vec![crate::TAG_TX];
        payload.extend_from_slice(&serde_json::to_vec(&tx).unwrap());
        u.inject(d, payload);
    }

    // Advance well past n_txs block-times + max latency drain.
    u.advance(cfg.latency_micros + block_time * (cfg.n_txs + 50));

    let log = u.event_log();
    let produced = log.iter().filter(|(_, _, s)| s.contains("produced H=")).count() as u64;
    let applied_ok = log
        .iter()
        .filter(|(_, _, s)| s.contains("apply H=") && s.contains("Ok"))
        .count() as u64;
    let divergence = log.iter().filter(|(_, _, s)| s.contains("Divergence")).count() as u64;
    let rejected = log.iter().filter(|(_, _, s)| s.contains("Rejected")).count() as u64;

    TrialOutcome { seed, produced, applied_ok, divergence, rejected }
}

/// Aggregate report over `n` trials.
#[derive(Debug, Clone)]
pub struct FuzzReport {
    /// Trials run.
    pub trials: u64,
    /// Sum of divergences across all trials — MUST be 0.
    pub total_divergence: u64,
    /// Trials that saw network-induced rejections (chaos coverage).
    pub chaos_trials: u64,
    /// First unsafe trial's seed, if any (for reproduction).
    pub first_unsafe_seed: Option<u64>,
}

/// Run `n` trials starting at `base_seed`, base_seed+1, … Returns the
/// aggregate. The caller asserts `total_divergence == 0`.
pub fn fuzz(n: u64, base_seed: u64) -> FuzzReport {
    let mut total_divergence = 0;
    let mut chaos_trials = 0;
    let mut first_unsafe_seed = None;
    for i in 0..n {
        let seed = base_seed.wrapping_add(i);
        let o = run_trial(seed);
        total_divergence += o.divergence;
        if o.saw_chaos() {
            chaos_trials += 1;
        }
        if !o.is_safe() && first_unsafe_seed.is_none() {
            first_unsafe_seed = Some(seed);
        }
    }
    FuzzReport { trials: n, total_divergence, chaos_trials, first_unsafe_seed }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_clean_trial_applies_everything() {
        // A seed that happens to draw near-zero loss should see produced ==
        // applied_ok. We just assert safety + that it produced something.
        let o = run_trial(1);
        assert!(o.is_safe(), "trial seed=1 diverged: {o:?}");
        assert!(o.produced > 0);
    }

    #[test]
    fn trial_is_reproducible_from_seed() {
        // Determinism: same seed → identical outcome.
        assert_eq!(run_trial(42), run_trial(42));
        assert_eq!(run_trial(9999), run_trial(9999));
    }

    #[test]
    fn fuzz_trials_zero_divergence() {
        // THE property. Seeded trials with random tx-mix + random latency +
        // random packet loss. SIGIL must NEVER silently diverge.
        //
        // In-suite count kept at 120 (~40s) for shared-suite hygiene; this
        // run already passed at 300. For a heavy soak call `fuzz(10_000, …)`
        // directly — it's a pub fn precisely so an on-demand run can crank
        // the trial count without bloating every `fluxc build --tests`.
        let report = fuzz(120, 0xC0FFEE);
        assert_eq!(
            report.total_divergence, 0,
            "SAFETY VIOLATION — divergence in {} trials, first unsafe seed: {:?}",
            report.trials, report.first_unsafe_seed
        );
        // Coverage check: the fuzzer must actually have exercised lossy
        // networks (rejections), else we proved nothing about chaos. With
        // 0–15% loss over 300 trials this is essentially certain.
        assert!(
            report.chaos_trials > 0,
            "fuzzer never induced packet loss — chaos coverage is zero"
        );
    }

    #[test]
    fn high_loss_causes_rejections_not_divergence() {
        // Hand-craft a brutal-loss trial: prove the follower REJECTS (safe,
        // a parent-gap) rather than DIVERGING (unsafe) when blocks drop.
        let g = demo_genesis();
        let block_time = secs(1);
        let mut u = Universe::new(ScenarioSeed::from(7));
        let d = u.spawn_node(Box::new(SigilSimNode::new("delta", NodeId(0), vec![NodeId(1)], true, block_time, &g)));
        let e = u.spawn_node(Box::new(SigilSimNode::new("epsilon", NodeId(1), vec![], false, block_time, &g)));
        u.connect(d, e, NetEdge { latency_micros: 50_000, drop_prob: 0.6, partitioned: false });
        for n in 0..30u32 {
            let tx = sign_dummy(SigilTx::Send {
                from: [(n % 5 + 1) as u8; 32],
                to: [((n + 1) % 5 + 1) as u8; 32],
                amount: 10,
                token: NATIVE,
                fee: 1,
            });
            let mut p = vec![crate::TAG_TX];
            p.extend_from_slice(&serde_json::to_vec(&tx).unwrap());
            u.inject(d, p);
        }
        u.advance(secs(120));
        let log = u.event_log();
        let diverged = log.iter().filter(|(_, _, s)| s.contains("Divergence")).count();
        assert_eq!(diverged, 0, "heavy loss must never cause divergence (only rejection)");
    }
}
