//! backfill_catchup — chronos timing model for the catch-up RACE.
//!
//! `backfill::run_light_backfill_protocol` proved the *protocol* fetches the gap.
//! This models the **timing**: a producer mints at `production_per_s` while a joining
//! node tries to backfill a `gap`. Catch-up only happens if the node applies faster
//! than the tip grows. The throttles are: how often each side **drains** events
//! (`drain_interval_ms` — the bug was draining only every 5000 ms), the serve
//! **rate-limit** (`serve_interval_ms`, one chunk per interval, GLOBAL on the
//! producer) and the **chunk** size, shared across `followers`.
//!
//! Effective per-node catch-up rate ≈ chunk · min(1/serve_interval, 1/drain_interval)
//! / followers. If that ≤ production_per_s the gap never closes. This lets us TUNE
//! (drain_interval, serve_interval, chunk) to actually sync e.g. 10k at 1200 blk/s,
//! and read off the producer's serve I/O load so we don't re-create a starvation.

/// Result of a catch-up simulation.
#[derive(Debug, Clone)]
pub struct CatchupReport {
    /// Blocks the joining node started behind.
    pub gap: u64,
    /// Producer mint rate (blocks/s).
    pub production_per_s: u64,
    /// Number of nodes sharing the producer's serve budget.
    pub followers: u64,
    /// Serve chunk size (blocks per served range).
    pub chunk: u64,
    /// Producer serve rate-limit (1 chunk per this many ms).
    pub serve_interval_ms: u64,
    /// How often each side drains its event queue (ms).
    pub drain_interval_ms: u64,
    /// True if a node fully caught the tip within the horizon.
    pub caught_up: bool,
    /// Virtual seconds to catch up (or the horizon if never).
    pub seconds_to_sync: u64,
    /// Per-node effective apply rate (blocks/s).
    pub per_node_rate: u64,
    /// Producer's serve serialization load (blocks serialized/s) — keep < a few k.
    pub producer_serve_load: u64,
    /// Final gap (0 if synced; grows if it never catches up).
    pub final_gap: u64,
}

impl CatchupReport {
    /// One-line summary.
    pub fn summary(&self) -> String {
        format!(
            "gap {} @ {} blk/s × {} nodes | chunk {} · serve/{}ms · drain/{}ms → \
             {} (per-node {} blk/s, producer serves {} blk/s); {}",
            self.gap, self.production_per_s, self.followers,
            self.chunk, self.serve_interval_ms, self.drain_interval_ms,
            if self.caught_up { format!("SYNCED in {}s", self.seconds_to_sync) }
            else { format!("NEVER (final gap {})", self.final_gap) },
            self.per_node_rate, self.producer_serve_load,
            if self.caught_up { "ok" } else { "✗ gap grows" },
        )
    }
}

/// Discrete virtual-time simulation (step = `drain_interval_ms`). The producer's tip
/// grows continuously; serving fires at most once per `serve_interval_ms`, each serve
/// re-broadcasts `chunk` blocks, the budget round-robins across `followers`. A node
/// applies what it is served (contiguous), capped at the live tip.
pub fn run_catchup(
    gap: u64,
    production_per_s: u64,
    followers: u64,
    chunk: u64,
    serve_interval_ms: u64,
    drain_interval_ms: u64,
) -> CatchupReport {
    let followers = followers.max(1);
    let drain = drain_interval_ms.max(1);
    let serve = serve_interval_ms.max(1);
    let horizon_s: u64 = 600; // 10 min virtual horizon

    // A serve happens at most once per max(serve_interval, drain_interval): you can't
    // serve more often than the rate-limit allows NOR more often than you drain the
    // request queue. Use float seconds — integer 1000/drain floored to 0 for slow
    // drains and over-counted.
    let serve_period_ms = serve.max(drain) as f64;
    let serves_per_s = 1000.0 / serve_period_ms;
    let total_serve_rate = (serves_per_s * chunk as f64) as u64; // blocks/s serialized
    let per_node_rate = total_serve_rate / followers; // each node's apply rate

    // millisecond-stepped sim.
    let steps = (horizon_s * 1000) / drain;
    let mut tip: u64 = gap; // node starts at 0, tip starts `gap` ahead
    let mut node: u64 = 0;
    let mut synced_ms: i64 = -1;
    let prod_per_step = (production_per_s * drain) as f64 / 1000.0;
    let apply_per_step = (per_node_rate * drain) as f64 / 1000.0;
    let mut tip_acc = 0f64;
    let mut node_acc = 0f64;
    for step in 0..steps {
        tip_acc += prod_per_step;
        node_acc += apply_per_step;
        while tip_acc >= 1.0 {
            tip += 1;
            tip_acc -= 1.0;
        }
        while node_acc >= 1.0 && node < tip {
            node += 1;
            node_acc -= 1.0;
        }
        if node >= tip && synced_ms < 0 {
            synced_ms = (step * drain) as i64;
            break;
        }
    }
    let caught_up = synced_ms >= 0;
    CatchupReport {
        gap,
        production_per_s,
        followers,
        chunk,
        serve_interval_ms: serve,
        drain_interval_ms: drain,
        caught_up,
        seconds_to_sync: if caught_up { (synced_ms as u64) / 1000 } else { horizon_s },
        per_node_rate,
        producer_serve_load: total_serve_rate,
        final_gap: tip.saturating_sub(node),
    }
}

/// Sweep drain/serve/chunk and return the CHEAPEST config (lowest producer serve
/// load) that still syncs `gap` at `production_per_s` for `followers` nodes — the
/// tuning the operator applies. Returns None if nothing in the grid works.
pub fn tune(gap: u64, production_per_s: u64, followers: u64) -> Option<CatchupReport> {
    let drains = [5000u64, 1000, 250, 100, 50, 20, 10];
    let serves = [500u64, 250, 100, 50, 25];
    let chunks = [256u64, 512, 1024, 2048];
    let mut best: Option<CatchupReport> = None;
    for &d in &drains {
        for &s in &serves {
            for &c in &chunks {
                let r = run_catchup(gap, production_per_s, followers, c, s, d);
                if r.caught_up {
                    let take = match &best {
                        None => true,
                        // prefer lower producer load, then faster sync
                        Some(b) => r.producer_serve_load < b.producer_serve_load
                            || (r.producer_serve_load == b.producer_serve_load
                                && r.seconds_to_sync < b.seconds_to_sync),
                    };
                    if take {
                        best = Some(r);
                    }
                }
            }
        }
    }
    best
}

/// The producer's serialization load (blk/s) from SERVING backfill, by transport.
/// GOSSIP re-broadcasts each served block on the shared topic, so EVERY node (incl.
/// the producer's own echo) deserializes it → load ≈ serve_rate × (followers+1).
/// REQUEST-RESPONSE serves point-to-point to the one requester → load ≈ serve_rate.
/// If this exceeds the producer's processing budget, block production starves.
pub fn producer_serve_load(serve_rate: u64, followers: u64, gossip: bool) -> u64 {
    if gossip {
        serve_rate * (followers + 1)
    } else {
        serve_rate
    }
}

/// True if the producer keeps producing while serving backfill under `mode`.
/// Budget = blocks/s the producer can deserialize before the mint loop starves
/// (empirically ~5000/s for small empty blocks on the live box).
pub fn production_survives(serve_rate: u64, followers: u64, gossip: bool, budget: u64) -> bool {
    producer_serve_load(serve_rate, followers, gossip) <= budget
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gossip_floods_the_producer_but_request_response_does_not() {
        // The tuned config serves ~4096 blk/s to close 10k at 1200/s for 3 nodes.
        let serve = 4096;
        let budget = 5000; // producer's deserialize headroom before mint starves
        let gossip = producer_serve_load(serve, 3, true);
        let rr = producer_serve_load(serve, 3, false);
        println!("producer serve load — gossip: {} blk/s, request-response: {} blk/s (budget {})", gossip, rr, budget);
        assert!(!production_survives(serve, 3, true, budget), "gossip MUST flood past budget (= prod collapse we saw live)");
        assert!(production_survives(serve, 3, false, budget), "request-response MUST stay within budget");
    }

    #[test]
    fn the_5s_drain_can_never_catch_up_at_1200() {
        // The shipped bug: drain every 5000ms. Even with a big chunk it serves one
        // chunk per 5s → ~100 blk/s ≪ 1200 → the gap grows forever.
        let r = run_catchup(10_000, 1200, 3, 2048, 250, 5000);
        println!("{}", r.summary());
        assert!(!r.caught_up, "5s drain must NOT catch up at 1200 blk/s");
    }

    #[test]
    fn tuned_config_syncs_10k_at_1200_for_3_nodes() {
        let best = tune(10_000, 1200, 3).expect("a config must sync 10k at 1200×3");
        println!("TUNED → {}", best.summary());
        assert!(best.caught_up);
        // sanity: producer serve load should be bounded (not a starvation cannon)
        assert!(best.producer_serve_load <= 20_000, "serve load too high: {}", best.producer_serve_load);
    }
}
