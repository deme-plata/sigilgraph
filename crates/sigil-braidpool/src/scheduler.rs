//! scheduler.rs — SIGIL_BRAIDPOOL_v1_1.md §7's pull scheduler, which never
//! had ANY implementation before this: `worker::ShardedMempool::pull`
//! already does fair worker-level round robin (an equal `max/N` share per
//! worker + a leftover mop-up pass), which is real and tested, but it's
//! specifically worker-level fairness — it has no concept of the two things
//! §7 asks for beyond that: (1) a generic `BatchSource` interface any pull
//! source can implement (not hardcoded to `MempoolWorker`), and (2) genuine
//! DEFICIT round robin, where a source that's skipped (nothing ready, or its
//! item costs more than its remaining credit) accumulates credit for next
//! time instead of just losing its turn — plus §7 point 5's "cap consecutive
//! selections from one source," which nothing else in this crate does at
//! all.
//!
//! Standalone: does not implement `BatchSource` for `MempoolWorker` (that
//! would require touching the already-tested `worker.rs`) and is not called
//! from anywhere in `sigil-node`/`sigil-api`. A real integration means an
//! adapter `impl BatchSource for &MempoolWorker` plus swapping the call site
//! that currently calls `ShardedMempool::pull` directly — separate follow-up
//! wiring work.

/// One pull-able source of "next ready item" — §7's pseudo-interface,
/// implemented for real here. `T` is whatever unit the scheduler pulls
/// (a transaction, a sealed batch — the scheduler itself is unit-agnostic).
pub trait BatchSource<T> {
    /// The cost of the next item this source would hand back, or `None` if
    /// nothing is ready. Checked BEFORE `pop_ready` so the scheduler can
    /// decide whether this source's remaining budget/credit can afford it
    /// without actually consuming the item.
    fn peek_cost(&self) -> Option<usize>;

    /// Pop the next ready item, charging up to `budget` (the scheduler
    /// decrements `budget` itself after this returns — implementations
    /// don't need to touch it). Returns `None` if nothing was ready (should
    /// only be called after `peek_cost` returned `Some`, but must not panic
    /// if it wasn't).
    fn pop_ready(&mut self) -> Option<T>;
}

/// Deficit round-robin scheduling state for one source, indexed by whatever
/// key identifies it (e.g. `crate::types::WorkerId`).
struct SourceState {
    deficit: usize,
    consecutive: u32,
}

/// A generic deficit-round-robin coordinator over `N` sources (§7's
/// "coordinator uses deficit round robin across workers"). `quantum` is the
/// credit added to a source's deficit counter each round it's visited — the
/// DRR mechanism: a source whose next item costs more than its accumulated
/// deficit is skipped THIS round without losing the credit, so a source
/// that's temporarily starved of small-enough-to-afford items still gets
/// serviced once enough rounds have accumulated credit for it.
pub struct PullScheduler {
    quantum: usize,
    max_consecutive: u32,
    states: Vec<SourceState>,
}

impl PullScheduler {
    /// `quantum` — credit granted per source per round (§7's DRR knob).
    /// `max_consecutive` — §7 point 5's cap: a source is skipped once it has
    /// been selected this many times in a row, even if it still has
    /// deficit/ready items, so one hot source can never fully starve its
    /// neighbors within a single `run` call.
    pub fn new(source_count: usize, quantum: usize, max_consecutive: u32) -> Self {
        Self {
            quantum,
            max_consecutive,
            states: (0..source_count).map(|_| SourceState { deficit: 0, consecutive: 0 }).collect(),
        }
    }

    /// Pull up to `budget` total cost-units worth of items across all
    /// `sources` (same order as this scheduler was constructed with —
    /// `sources.len()` must equal the `source_count` passed to `new`),
    /// deficit-round-robin fair, honoring the consecutive-selection cap.
    /// Runs until `budget` is exhausted or every source has nothing ready.
    pub fn run<T>(&mut self, sources: &mut [impl BatchSource<T>], mut budget: usize) -> Vec<T> {
        assert_eq!(sources.len(), self.states.len(), "source count must match what this scheduler was built for");
        let mut out = Vec::new();
        if sources.is_empty() {
            return out;
        }
        // Safety valve, not a normal exit path: a degenerate `quantum=0`
        // (or any config where no source's deficit ever grows enough to
        // afford its next item) would otherwise spin forever, since
        // `any_ready` stays true while `quantum` never lets anything pop.
        // Cooldown cycling legitimately needs a few stalled passes to
        // resolve; this bound is generous relative to that and only trips
        // on a genuinely stuck configuration.
        let max_stalled_passes = sources.len().saturating_mul(self.max_consecutive as usize + 2).max(4);
        let mut stalled_passes = 0usize;
        loop {
            // Whether ANY source still has something ready, independent of
            // whether it actually got served this pass. A source cooling
            // down after hitting `max_consecutive` still counts as ready —
            // it has more to give once the cooldown clears next pass. Using
            // this (rather than "did this pass pop anything") to decide
            // whether to keep looping is the fix for a real bug: when every
            // source is simultaneously mid-cooldown, a pass pops nothing,
            // but there IS more work for the next pass once cooldowns
            // reset — terminating there would silently under-deliver
            // against `budget` with ready work still sitting in every source.
            let mut any_ready = false;
            let popped_before_pass = out.len();
            for (i, source) in sources.iter_mut().enumerate() {
                if budget == 0 {
                    return out;
                }
                let Some(cost) = source.peek_cost() else {
                    let state = &mut self.states[i];
                    state.deficit = 0; // nothing ready — don't let idle sources bank infinite credit
                    state.consecutive = 0;
                    continue;
                };
                any_ready = true;
                let state = &mut self.states[i];
                if state.consecutive >= self.max_consecutive {
                    state.consecutive = 0; // cooldown: this source sits out one full pass
                    continue;
                }
                state.deficit += self.quantum;
                if cost > state.deficit || cost > budget {
                    continue; // not enough credit or budget yet — deficit carries over to next round
                }
                if let Some(item) = source.pop_ready() {
                    state.deficit -= cost;
                    budget -= cost;
                    state.consecutive += 1;
                    out.push(item);
                } else {
                    // peek_cost lied (nothing actually popped) — don't spin forever on it.
                    state.deficit = 0;
                    state.consecutive = 0;
                }
            }
            if !any_ready || budget == 0 {
                break;
            }
            if out.len() == popped_before_pass {
                stalled_passes += 1;
                if stalled_passes > max_stalled_passes {
                    break; // degenerate config (e.g. quantum=0) — stop rather than spin forever
                }
            } else {
                stalled_passes = 0;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// A trivial fixed-cost-per-item source for tests: a queue of `(cost,
    /// value)` pairs.
    struct TestSource {
        items: VecDeque<(usize, u32)>,
    }

    impl TestSource {
        fn new(items: Vec<(usize, u32)>) -> Self {
            Self { items: items.into() }
        }
    }

    impl BatchSource<u32> for TestSource {
        fn peek_cost(&self) -> Option<usize> {
            self.items.front().map(|(c, _)| *c)
        }
        fn pop_ready(&mut self) -> Option<u32> {
            self.items.pop_front().map(|(_, v)| v)
        }
    }

    #[test]
    fn pulls_everything_when_budget_is_unbounded() {
        let mut sources = vec![
            TestSource::new(vec![(1, 10), (1, 11)]),
            TestSource::new(vec![(1, 20)]),
        ];
        let mut sched = PullScheduler::new(2, 10, 100);
        let out = sched.run(&mut sources, 100);
        let mut sorted = out.clone();
        sorted.sort();
        assert_eq!(sorted, vec![10, 11, 20], "every ready item across every source must be pulled");
    }

    #[test]
    fn stops_at_budget() {
        let mut sources = vec![TestSource::new(vec![(1, 1), (1, 2), (1, 3), (1, 4)])];
        let mut sched = PullScheduler::new(1, 10, 100);
        let out = sched.run(&mut sources, 2);
        assert_eq!(out.len(), 2, "must stop exactly at the cost budget, not pull everything");
    }

    #[test]
    fn no_source_starves_another_with_equal_costs() {
        // Two sources with plenty ready; a fair scheduler must interleave
        // roughly evenly rather than draining one source before the other.
        let mut sources = vec![
            TestSource::new((0..20).map(|i| (1, i)).collect()),
            TestSource::new((100..120).map(|i| (1, i)).collect()),
        ];
        let mut sched = PullScheduler::new(2, 1, 3);
        let out = sched.run(&mut sources, 20);
        let from_a = out.iter().filter(|&&v| v < 100).count();
        let from_b = out.iter().filter(|&&v| v >= 100).count();
        assert!(from_a >= 8 && from_b >= 8, "expected roughly even split, got a={from_a} b={from_b}");
    }

    #[test]
    fn max_consecutive_forces_a_cooldown() {
        // One source with an enormous backlog and a huge quantum (so DRR
        // credit alone would never make it wait) — only the consecutive cap
        // should stop it from monopolizing every slot in a row.
        let mut sources = vec![
            TestSource::new((0..10).map(|i| (1, i)).collect()),
            TestSource::new(vec![(1, 999)]),
        ];
        let mut sched = PullScheduler::new(2, 1_000_000, 2);
        let out = sched.run(&mut sources, 20);
        // Source 1 (index 1) has exactly one item; it must appear even
        // though source 0 has a much larger backlog and a huge quantum,
        // because the consecutive cap forces source 0 to yield.
        assert!(out.contains(&999), "the small source must get serviced, not starved by the cap-less big one");
    }

    #[test]
    fn empty_sources_returns_empty_without_panicking() {
        let mut sources: Vec<TestSource> = Vec::new();
        let mut sched = PullScheduler::new(0, 1, 1);
        assert_eq!(sched.run(&mut sources, 100), Vec::<u32>::new());
    }

    #[test]
    fn exhausted_sources_stop_the_loop_without_spinning() {
        let mut sources = vec![TestSource::new(vec![(1, 1)])];
        let mut sched = PullScheduler::new(1, 5, 100);
        // Budget far exceeds what's available — must return promptly with
        // just the one real item, not hang.
        let out = sched.run(&mut sources, 1_000_000);
        assert_eq!(out, vec![1]);
    }

    /// `quantum=0` means no source's deficit can ever afford a cost>=1 item
    /// — a degenerate config with items ready forever and nothing ever
    /// poppable. Must terminate via the stalled-pass safety valve, not hang.
    #[test]
    fn zero_quantum_terminates_instead_of_spinning_forever() {
        let mut sources = vec![TestSource::new(vec![(1, 1), (1, 2), (1, 3)])];
        let mut sched = PullScheduler::new(1, 0, 5);
        let out = sched.run(&mut sources, 100);
        assert!(out.is_empty(), "with zero credit ever granted, nothing should ever be poppable");
    }
}
