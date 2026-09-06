//! Per-tick **Kristensen Realization Functional** `K_R`, computed inside the
//! producer loop from the loop's own observables — and answering the one
//! question a running node cannot otherwise answer about itself:
//!
//! > **which single constraint is limiting block realization right now?**
//!
//! ## Why this belongs in the producer and not in a dashboard
//!
//! Every input here is already in memory at the moment a block is minted: the
//! braid knows its own proposers, its finality margin and what it had to drop;
//! the mining bridge knows which shares were accepted and which rigs are alive;
//! the tick knows whether the candidate it just minted actually settled. A
//! dashboard has to go and ask over HTTP, one window late, and cannot see the
//! mint→settle relationship at all. Here it is free — a few adds and one
//! logarithm on a loop that is already doing STARK-adjacent work — and it is
//! *causally attached to the block*.
//!
//! So the producer stops reporting "block production is slow" (a symptom
//! nobody can act on) and starts reporting, e.g.:
//!
//! ```text
//!   K_R=0.412  binding=finality_margin  reordering is within 12 of final_depth
//! ```
//!
//! which is an instruction.
//!
//! ## What is genuinely measured here
//!
//! `Delta_s`, the **proposer entropy**, is real: `braid.recent_summary()` carries
//! `producer` per block, so the Shannon entropy over the window is computed from
//! the same bytes consensus used. That makes `K*` here the SAME quantity
//! `flux_sigil_kgauge` reports over HTTP — one definition, two vantage points —
//! and the two should agree to within their different windows.
//!
//! `Delta_H` is a **lower bound**, exactly as it is in the k-gauge: the local
//! disagreement this node can see (mining rejects + braid drops), with no P2P
//! byte-asymmetry counter to add. Stated in every reading, never rounded up into
//! a claim.
//!
//! The **energy** term is not available at all — a node has no wattmeter — so it
//! is excluded from the geometric mean rather than substituted. A four-term mean
//! with `excluded: ["energy"]` is honest; a five-term mean with a plausible
//! fifth is not.
//!
//! ## Off by default
//!
//! `SIGIL_REALIZATION_EVERY_TICKS` (default `0` = disabled) is the gate. This
//! module cannot affect consensus — it reads, it never writes — but a producer
//! that mints money should do nothing unrequested, and an operator who has not
//! asked for telemetry should not get a webhook.
//!
//! | env | default | meaning |
//! |---|---|---|
//! | `SIGIL_REALIZATION_EVERY_TICKS` | `0` (off) | emit a reading every N ticks |
//! | `SIGIL_REALIZATION_WEBHOOK` | unset | POST each reading to this URL |
//! | `SIGIL_REALIZATION_REF_HPS` | `1e9` | declared reference hash rate for `C` |
//! | `SIGIL_REALIZATION_TARGET_RIGS` | `16` | replication target for `X` and the diversity constraint |
//! | `SIGIL_REALIZATION_MIN_MARGIN` | `32` | finality margin below which reordering is the binder |

use std::collections::HashMap;
use std::time::Instant;

use flux_realization::{evaluate, Capability, Constraint, Design, Peer, Provenance, Realization, Tracked};
use sigil_api::mining::MiningBridge;
use sigil_dagknight::braid::Braid;

use super::run::TickOutcome;

const DEFAULT_REF_HPS: f64 = 1.0e9;
const DEFAULT_TARGET_RIGS: f64 = 16.0;
const DEFAULT_MIN_MARGIN: f64 = 32.0;
/// How many recent braid blocks the proposer-entropy window covers.
const ENTROPY_WINDOW: usize = 64;

fn env_f64(key: &str, default: f64) -> f64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v >= 0.0)
        .unwrap_or(default)
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key).ok().and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(default)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Shannon entropy in bits over a count map — the plurality of who may propose.
/// A lone producer yields exactly `0.0` (and `+0.0`, not `-0.0`, which otherwise
/// leaks a minus sign into every serialized reading on a single-producer chain).
fn shannon_bits(counts: &HashMap<[u8; 32], u64>) -> f64 {
    let total: u64 = counts.values().sum();
    if total == 0 {
        return 0.0;
    }
    let n = total as f64;
    let h: f64 = -counts
        .values()
        .map(|&c| {
            let p = c as f64 / n;
            if p > 0.0 {
                p * p.log2()
            } else {
                0.0
            }
        })
        .sum::<f64>();
    // `-sum` of a single p=1 term is `-0.0`, and `.max(0.0)` does NOT fix that:
    // IEEE 754-2019 maximumNumber leaves the choice between two equal-comparing
    // zeros unspecified, and Rust documents that either input may be returned.
    // Adding `+0.0` is the one operation defined to produce `+0.0` from `-0.0`
    // under round-to-nearest, so a single-producer chain reports `0` and not
    // `-0` in every serialized reading.
    if h == 0.0 {
        0.0
    } else {
        h.max(0.0)
    }
}

/// What the meter carried over from the previous emission, so every rate is a
/// delta over a known interval rather than a lifetime total divided by
/// something. (Dividing a cumulative counter by a windowed one is the exact bug
/// that made the wallet topbar read a fictitious `Delta_H` of 0.7 on a quiet
/// chain — it is worth naming so it is not reinvented here.)
#[derive(Debug, Clone, Copy)]
struct Mark {
    at: Instant,
    settled_height: u64,
    shares: u64,
    rejects: u64,
    dropped: u64,
    applied: u64,
    minted: u64,
}

/// Accumulates producer observables and emits a `K_R` reading every N ticks.
pub struct RealizationMeter {
    every: u64,
    ticks_since: u64,
    mark: Option<Mark>,
    /// Running totals since the last emission.
    applied_acc: u64,
    minted_acc: u64,
    webhook: Option<String>,
    /// Emissions so far — carried in the reading so a reader can tell a fresh
    /// process from a long-running one.
    emitted: u64,
}

impl RealizationMeter {
    /// Build a meter from the environment. Returns `None` when the feature is
    /// not requested, so the caller can skip every observation entirely.
    pub fn from_env() -> Option<Self> {
        let every = env_u64("SIGIL_REALIZATION_EVERY_TICKS", 0);
        if every == 0 {
            return None;
        }
        Some(Self {
            every,
            ticks_since: 0,
            mark: None,
            applied_acc: 0,
            minted_acc: 0,
            webhook: std::env::var("SIGIL_REALIZATION_WEBHOOK").ok().filter(|s| !s.trim().is_empty()),
            emitted: 0,
        })
    }

    /// Feed one completed tick. Returns a reading on the ticks where one is due.
    ///
    /// Never returns an error and never panics on a poisoned lock: telemetry
    /// must not be able to interrupt block production. A lock it cannot read
    /// degrades the affected term's provenance instead.
    pub fn observe(&mut self, out: &TickOutcome, mining: &MiningBridge, braid: &Braid) -> Option<Realization> {
        self.ticks_since += 1;
        self.applied_acc += out.applied;
        self.minted_acc += 1;

        let bs = braid.stats();
        let (net_hps, _live, _blocks, shares, rejects) = mining.stats(now_ms());
        let rejects_total: u64 = rejects.iter().map(|(_, n)| *n).sum();

        let now = Instant::now();
        let prev = match self.mark {
            Some(m) => m,
            None => {
                // First tick: establish the baseline and emit nothing. A reading
                // whose window starts at process start would divide lifetime
                // counters by a few milliseconds.
                self.mark = Some(Mark {
                    at: now,
                    settled_height: out.settled_height,
                    shares,
                    rejects: rejects_total,
                    dropped: bs.below_final_dropped,
                    applied: 0,
                    minted: 0,
                });
                self.ticks_since = 0;
                self.applied_acc = 0;
                self.minted_acc = 0;
                return None;
            }
        };

        if self.ticks_since < self.every {
            return None;
        }

        let tau = now.duration_since(prev.at).as_secs_f64().max(1e-3);
        let d_settled = out.settled_height.saturating_sub(prev.settled_height);
        let d_shares = shares.saturating_sub(prev.shares);
        let d_rejects = rejects_total.saturating_sub(prev.rejects);
        let d_dropped = bs.below_final_dropped.saturating_sub(prev.dropped);
        let applied_win = self.applied_acc.saturating_sub(prev.applied);
        let minted_win = self.minted_acc.saturating_sub(prev.minted);

        // ── Delta_s: proposer entropy over the braid's own recent window ─────
        let mut counts: HashMap<[u8; 32], u64> = HashMap::new();
        for s in braid.recent_summary(ENTROPY_WINDOW) {
            *counts.entry(s.producer).or_insert(0) += 1;
        }
        let delta_s = shannon_bits(&counts);
        let distinct_producers = counts.len() as f64;

        // ── Delta_H: local energetic disagreement, a LOWER BOUND ────────────
        let accept_ratio = if d_shares + d_rejects > 0 { d_shares as f64 / (d_shares + d_rejects) as f64 } else { 1.0 };
        let accept_measured = d_shares + d_rejects > 0;
        let reject_ratio = 1.0 - accept_ratio;
        let emitted_total = bs.emitted_total.max(1) as f64;
        let drop_ratio = (d_dropped as f64 / emitted_total).clamp(0.0, 1.0);
        let delta_h = reject_ratio + drop_ratio;

        // K* = 2*pi*sqrt(Delta_H * tau * Delta_s), hbar := 1. Identical
        // definition to flux_sigil_kgauge — deliberately, so the node and the
        // MCP can be compared rather than merely both quoted.
        let k_star = 2.0 * std::f64::consts::PI * (delta_h * tau * delta_s).sqrt();

        // ── the fleet, as effective peers ───────────────────────────────────
        let ref_hps = env_f64("SIGIL_REALIZATION_REF_HPS", DEFAULT_REF_HPS).max(1.0);
        let target_rigs = env_f64("SIGIL_REALIZATION_TARGET_RIGS", DEFAULT_TARGET_RIGS).max(1.0);
        let min_margin = env_f64("SIGIL_REALIZATION_MIN_MARGIN", DEFAULT_MIN_MARGIN);
        let per_rig_ref = (ref_hps / target_rigs).max(1.0);

        let miners = mining.miners_snapshot(now_ms());
        let total_hps: f64 = miners.iter().map(|m| m.hash_rate).sum();
        let hhi: f64 = if total_hps > 0.0 {
            miners
                .iter()
                .map(|m| {
                    let s = m.hash_rate / total_hps;
                    s * s
                })
                .sum()
        } else {
            1.0
        };
        let expandable = (1.0 - hhi).clamp(0.0, 1.0);
        let distinct_rigs = miners.iter().map(|m| m.rig.as_str()).collect::<std::collections::HashSet<_>>().len();
        let peers: Vec<Peer> = miners
            .iter()
            .map(|m| Peer {
                id: if m.rig.is_empty() { m.wallet.clone() } else { m.rig.clone() },
                useful: (m.hash_rate / per_rig_ref).clamp(0.0, 1.0),
                verified: accept_ratio,
                reliable: (1.0 - (m.last_seen_secs_ago as f64 / 600.0)).clamp(0.0, 1.0),
            })
            .collect();

        // ── the constraint set, from the producer's own vantage point ────────
        // `settle_lag` and `finality_margin` are the two that only exist HERE:
        // they are relationships between minting, mining and finality that no
        // external observer can see.
        let settle_lag = out.minted_height.saturating_sub(out.settled_height) as f64;
        let margin = braid.finality_margin();
        let constraints = vec![
            Constraint::hard(
                "block_liveness",
                1.0,
                if d_settled > 0 { 1.0 } else { 0.0 },
                Provenance::Measured,
                "the settled chain advanced during this window; a producer that mints without settling is stalled, not slow",
            ),
            Constraint::hard(
                "mint_settles",
                1.0,
                if applied_win > 0 { 1.0 } else { 0.0 },
                Provenance::Measured,
                "at least one minted candidate reached the settled chain; zero means the braid is minting into a branch it never selects",
            ),
            Constraint::soft(
                "finality_margin",
                min_margin,
                margin.map(|m| m as f64).unwrap_or(min_margin),
                if margin.is_some() { Provenance::Measured } else { Provenance::Unavailable },
                "observed reordering depth against final_depth — the ONE condition under which the height-offset finality rule is unsound; a shrinking margin is advance notice, not an after-the-fact count",
            ),
            Constraint::soft(
                "braid_pending",
                1.0,
                1.0 / (1.0 + bs.pending as f64),
                Provenance::Measured,
                "views parked waiting for missing parents; a growing pending set means this node is behind the mesh, not that the mesh is slow",
            ),
            Constraint::soft(
                "share_acceptance",
                1.0,
                accept_ratio,
                if accept_measured { Provenance::Measured } else { Provenance::Unavailable },
                "accepted/(accepted+rejected) shares in the window; rejects are miner work thrown away and usually mean a stale tip",
            ),
            Constraint::soft(
                "miner_diversity",
                target_rigs,
                distinct_rigs as f64,
                Provenance::Measured,
                "distinct live rigs — the replication dimension; how many independent boxes would have to leave before this chain did",
            ),
            Constraint::soft(
                "proposer_plurality",
                2.0,
                distinct_producers,
                Provenance::Measured,
                "distinct proposers in the braid window; with one producer K* reads ~0 because there is no plurality to disagree — the gauge is not broken, the chain is centralized",
            ),
            Constraint::soft(
                "solve_backlog",
                1.0,
                1.0 / (1.0 + mining.queued_solves() as f64),
                Provenance::Measured,
                "mining solves queued but not yet credited into a block; a persistent backlog is miner work waiting on the producer",
            ),
        ];

        let capability = Capability {
            compute: Tracked::derived((net_hps / ref_hps).clamp(0.0, 1.0), "net hash rate against the declared reference"),
            energy: Tracked::unavailable(0.0, "a node has no wattmeter — excluded from the mean, never substituted"),
            verifiable: if accept_measured {
                Tracked::measured(accept_ratio, "share acceptance over the window")
            } else {
                Tracked::unavailable(0.0, "no shares submitted in the window — acceptance is unobserved, not perfect")
            },
            availability: Tracked::measured(
                if minted_win > 0 { (applied_win as f64 / minted_win as f64).clamp(0.0, 1.0) } else { 0.0 },
                "fraction of this window's minted candidates that reached the settled chain",
            ),
            expandable: Tracked::measured(expandable, "1 - Herfindahl index of hash-rate concentration across live rigs"),
        };

        let design = Design {
            name: "sigil-producer-local".to_string(),
            capability,
            constraints,
            peers,
            peer_provenance: Provenance::Derived,
            coordination: Tracked::measured(
                k_star,
                "K* = 2*pi*sqrt(Delta_H*tau*Delta_s) from THIS node's braid — same definition as flux_sigil_kgauge; Delta_H is a local lower bound (no P2P byte counters)",
            ),
        };

        let r = evaluate(&design);

        // Roll the window forward.
        self.mark = Some(Mark {
            at: now,
            settled_height: out.settled_height,
            shares,
            rejects: rejects_total,
            dropped: bs.below_final_dropped,
            applied: self.applied_acc,
            minted: self.minted_acc,
        });
        self.ticks_since = 0;
        self.emitted += 1;

        if let Some(url) = self.webhook.clone() {
            let payload = serde_json::json!({
                "event": "sigil_realization_producer",
                "ts_ms": now_ms(),
                "source": "sigil-top producer loop (in-process, no HTTP round trip)",
                "emission": self.emitted,
                "k_r": r.k_r,
                "limiting_factor": r.limiting_factor,
                "binding": r.bottleneck.binding,
                "in_landscape": r.feasibility.in_landscape,
                "capability_core": r.capability.core,
                "excluded_terms": r.capability.excluded,
                "effective_peers": r.amplification.effective_peers,
                "friction": r.friction,
                "K_star": k_star,
                "delta_H_lower_bound": delta_h,
                "delta_s_bits": delta_s,
                "tau_secs": tau,
                "minted_height": out.minted_height,
                "settled_height": out.settled_height,
                "settle_lag": settle_lag,
                "finality_margin": margin,
                "blocks_settled_in_window": d_settled,
                "distinct_rigs": distinct_rigs,
                "provenance": r.provenance,
                "caveats": r.caveats,
            });
            // Fire and forget on a detached thread with a hard timeout: a slow or
            // dead receiver must never add a millisecond to a producer tick.
            std::thread::spawn(move || {
                let _ = reqwest::blocking::Client::builder()
                    .timeout(std::time::Duration::from_secs(5))
                    .build()
                    .and_then(|c| c.post(&url).json(&payload).send());
            });
        }

        Some(r)
    }

    /// One line an operator can read at a glance — the reading reduced to what
    /// it is FOR.
    pub fn summarize(r: &Realization) -> String {
        format!(
            "K_R={:.4} {} binding={} limiting={} core={:.3}{} peers_eff={:.1} friction={:.3} [{}]",
            r.k_r,
            if r.feasibility.in_landscape { "landscape" } else { "SWAMPLAND" },
            r.bottleneck.binding.as_deref().unwrap_or("none"),
            r.limiting_factor,
            r.capability.core,
            if r.capability.excluded.is_empty() {
                String::new()
            } else {
                format!(" (excl {})", r.capability.excluded.join("+"))
            },
            r.amplification.effective_peers,
            r.friction,
            r.provenance,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lone_producer_yields_exactly_zero_entropy_not_negative_zero() {
        let mut c: HashMap<[u8; 32], u64> = HashMap::new();
        c.insert([1u8; 32], 64);
        let h = shannon_bits(&c);
        assert_eq!(h, 0.0);
        assert!(h.is_sign_positive(), "-0.0 leaks a minus sign into every serialized reading");
    }

    #[test]
    fn entropy_rises_with_proposer_plurality() {
        let mk = |n: u8| {
            let mut c: HashMap<[u8; 32], u64> = HashMap::new();
            for i in 0..n {
                c.insert([i; 32], 8);
            }
            shannon_bits(&c)
        };
        assert_eq!(mk(1), 0.0);
        assert!((mk(2) - 1.0).abs() < 1e-12, "two equal proposers = exactly 1 bit");
        assert!((mk(4) - 2.0).abs() < 1e-12, "four equal proposers = exactly 2 bits");
        assert!(mk(8) > mk(4));
    }

    #[test]
    fn an_empty_window_is_zero_not_nan() {
        assert_eq!(shannon_bits(&HashMap::new()), 0.0);
    }

    #[test]
    fn the_meter_is_off_unless_explicitly_asked_for() {
        // The default env has no SIGIL_REALIZATION_EVERY_TICKS, so a producer
        // that nobody asked for telemetry from does no work and sends nothing.
        let saved = std::env::var("SIGIL_REALIZATION_EVERY_TICKS").ok();
        std::env::remove_var("SIGIL_REALIZATION_EVERY_TICKS");
        assert!(RealizationMeter::from_env().is_none());
        std::env::set_var("SIGIL_REALIZATION_EVERY_TICKS", "0");
        assert!(RealizationMeter::from_env().is_none(), "an explicit 0 is also off");
        std::env::set_var("SIGIL_REALIZATION_EVERY_TICKS", "5");
        let m = RealizationMeter::from_env().expect("enabled");
        assert_eq!(m.every, 5);
        match saved {
            Some(v) => std::env::set_var("SIGIL_REALIZATION_EVERY_TICKS", v),
            None => std::env::remove_var("SIGIL_REALIZATION_EVERY_TICKS"),
        }
    }
}
