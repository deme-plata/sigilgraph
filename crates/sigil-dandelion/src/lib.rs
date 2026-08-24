//! Dandelion++ transaction-propagation privacy (ported from Quillon's `q-dandelion`).
//!
//! # The problem this solves
//!
//! Plain gossipsub broadcasts a new transaction to every mesh peer the instant it is seen.
//! A network-level observer running enough listening peers can therefore watch WHICH peer
//! announces a transaction FIRST and, with high confidence, name that peer's IP as the
//! transaction's origin. For a privacy chain this is a real deanonymization channel that
//! sits entirely outside the shielded-pool cryptography — a perfect shielded-send proof is
//! worthless if the sender's IP leaked at the P2P layer before the proof ever mattered.
//!
//! Dandelion++ (Fanti et al., 2018 — the same design Bitcoin Core and Monero ship) breaks
//! the timing correlation: before a transaction reaches gossip ("fluff"), it first travels
//! a short, private, point-to-point relay path ("stem") through a handful of peers. Each
//! relay hop only knows "the peer before me" and "the peer after me" — never who
//! ORIGINATED it — and the path length is randomized so an observer watching the eventual
//! fluff cannot tell how many stem hops preceded it, let alone reconstruct the path.
//!
//! # What is ported vs. redesigned from `q-dandelion`
//!
//! Quillon's `QuantumDandelion` (`/home/orobit/q-narwhalknight/crates/q-dandelion/src/lib.rs`)
//! re-rolls the stem successor **independently for every message** from a flat peer list.
//! That is simpler but weaker: an adversary who sees many stem-phase messages from this node
//! over time can run a set-intersection/clustering attack on the successor choices and
//! recover the node's peers with higher confidence than a single persistent choice would
//! allow. The Dandelion++ paper's own construction fixes ONE successor per node for an
//! entire EPOCH (all messages during that epoch travel the same first hop), which is what
//! [`StemGraph`] implements here — a deliberate strengthening, not a straight port.
//!
//! Also NOT ported: Quillon's QRNG/L-VRF quantum-randomness layer and its direct Tor-circuit
//! coupling (`tor_bridge.rs`). SIGIL's Tor integration (`sigil-net-tor`, arti-based) is a
//! separate, independently-activated piece — this crate stays transport-agnostic (see
//! [`Action`]) so stem relay can ride over WireGuard, Tor, or plain flux-p2p unicast without
//! this protocol logic caring which. A standard CSPRNG is enough: Dandelion++'s anonymity
//! guarantee does not depend on the randomness being quantum, only on it being unpredictable
//! to an outside observer, which `rand::rngs::OsRng` already provides.
//!
//! # Tunables (defaults match `q-dandelion`'s v8.6.0 values, which cite the paper directly)
//!
//! - [`DEFAULT_STEM_PROBABILITY`] = 0.9 — 90% of locally-originated transactions start in
//!   stem phase (the other 10% fluff immediately; without this floor a network with too few
//!   honest relayers could stall transactions in an all-adversarial stem path forever).
//! - [`DEFAULT_STEM_CONTINUE_PROBABILITY`] = 0.75 — at each hop, roll again; the paper's own
//!   recommendation, chosen so the expected stem path length stays short enough to bound
//!   latency while the geometric tail still makes path length genuinely unpredictable.
//! - [`DEFAULT_MAX_STEM_HOPS`] = 5 — a hard ceiling regardless of the geometric roll; beyond
//!   ~5 hops the marginal anonymity gain is negligible next to the added latency (same
//!   reasoning `q-dandelion`'s v8.6.0 comment gives).
//! - [`DEFAULT_EPOCH`] = 10 minutes — no principled value existed anywhere in this codebase
//!   to inherit, so this picks the same order of magnitude Monero uses (~10 min); short
//!   enough that a successor going offline mid-epoch is quickly self-healed by the next
//!   epoch's reroll, long enough that per-epoch successor churn doesn't itself become a
//!   distinguishing signal. Flagged as an open tuning question in the crate's own docs
//!   rather than asserted as settled.
//! - [`DEFAULT_FAILSAFE_TIMEOUT`] = 30s — if a transaction is still stemming this long after
//!   we first touched it (successor is down, dropped it, or is malicious), force-fluff it.
//!   Privacy must never cost LIVENESS: an attacker who can make a target's transactions
//!   vanish by refusing to relay them has turned an anonymity feature into a censorship
//!   tool, so failure always degrades to "less private, but delivered."
//!
//! # Integration status (read before wiring this into a live node)
//!
//! This crate is a **pure, synchronous protocol state machine** — no libp2p, no tokio, no
//! network I/O. [`DandelionRouter`] consumes a transaction id + bytes + the current peer
//! list and returns an [`Action`] telling the caller what to do; it never sends anything
//! itself. That keeps the privacy-critical logic (phase decisions, hop counting, epoch
//! rollover, failsafe timing) unit-testable without a real network or async runtime.
//!
//! **What is NOT done, checked directly against the live tree before writing this:**
//! SIGIL's transaction layer has no live gossip path to relay at all yet.
//! `sigil-net::TOPIC_TXS` (`/sigil/g0/txs`) is DEFINED but has exactly zero references
//! anywhere else in the workspace (`grep -rl TOPIC_TXS crates/` finds only its own
//! declaration) — every SIGIL node currently mints blocks from its OWN local mempool with
//! no P2P transaction propagation between nodes at all; peers only ever learn about
//! transactions by receiving the BLOCKS that contain them. Dandelion++ protects the P2P
//! gossip of individual pending transactions, so there is currently no live call site for
//! this crate to be wired INTO — that is a real, separate prerequisite (standing up
//! transaction gossip over `TOPIC_TXS` in the first place) that this pass did not attempt,
//! since it is materially larger scope than "port Dandelion++." Once that gossip path
//! exists, the integration point is exactly where a node would otherwise call
//! `flux_p2p::publish(TOPIC_TXS, tx_bytes)` on receiving/creating a transaction: call
//! [`DandelionRouter::originate`] (for a locally-created tx) or
//! [`DandelionRouter::relay_stem_incoming`] / [`relay_fluff_incoming`](DandelionRouter::relay_fluff_incoming)
//! (for one arriving from a peer) instead, and act on the returned [`Action`] using
//! `flux_p2p`'s existing `send_request(peer, bytes)` for stem hops and `publish(topic,
//! bytes)` for fluff — both primitives already exist in `flux-p2p` today, unused for this
//! purpose.

use std::collections::HashMap;
use std::hash::Hash;
use std::time::{Duration, Instant};

use rand::Rng;

/// 90% of locally-originated transactions begin in stem phase.
pub const DEFAULT_STEM_PROBABILITY: f64 = 0.9;
/// Per-hop probability of continuing the stem relay rather than fluffing.
pub const DEFAULT_STEM_CONTINUE_PROBABILITY: f64 = 0.75;
/// Hard ceiling on stem hops regardless of the geometric roll.
pub const DEFAULT_MAX_STEM_HOPS: u8 = 5;
/// How long one stem successor stays fixed before this node rerolls it.
pub const DEFAULT_EPOCH: Duration = Duration::from_secs(600);
/// Force-fluff a transaction that has been stemming longer than this — the liveness floor.
pub const DEFAULT_FAILSAFE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a message id is remembered for dedup before it may be seen "fresh" again.
pub const DEFAULT_SEEN_TTL: Duration = Duration::from_secs(300);

/// Tunable protocol parameters. See the module docs for the reasoning behind each default.
#[derive(Debug, Clone)]
pub struct DandelionConfig {
    pub stem_probability: f64,
    pub stem_continue_probability: f64,
    pub max_stem_hops: u8,
    pub epoch: Duration,
    pub failsafe_timeout: Duration,
    pub seen_ttl: Duration,
}

impl Default for DandelionConfig {
    fn default() -> Self {
        Self {
            stem_probability: DEFAULT_STEM_PROBABILITY,
            stem_continue_probability: DEFAULT_STEM_CONTINUE_PROBABILITY,
            max_stem_hops: DEFAULT_MAX_STEM_HOPS,
            epoch: DEFAULT_EPOCH,
            failsafe_timeout: DEFAULT_FAILSAFE_TIMEOUT,
            seen_ttl: DEFAULT_SEEN_TTL,
        }
    }
}

/// One node's persistent stem successor, re-rolled once per epoch.
///
/// Fixing the successor for the whole epoch (rather than per-message, as `q-dandelion`
/// does) is the paper's actual construction and the reason this crate exists as a separate,
/// deliberate port rather than a copy — see the module docs' "what is ported vs.
/// redesigned" section.
pub struct StemGraph<P> {
    epoch: Duration,
    epoch_start: Instant,
    current: Option<P>,
}

impl<P: Clone> StemGraph<P> {
    pub fn new(epoch: Duration, now: Instant) -> Self {
        Self { epoch, epoch_start: now, current: None }
    }

    /// The successor for the CURRENT epoch. Rerolls only when the epoch boundary has
    /// passed (or nothing has been rolled yet); otherwise returns the same peer every time,
    /// which is the whole point — repeated calls within one epoch must not leak a new
    /// coin-flip's worth of information per call.
    ///
    /// `peers` is consulted only AT THE MOMENT of a reroll. Returns `None` if there is
    /// nothing to route through, which the caller treats as "fall back to fluff" — Dandelion
    /// with zero relay candidates degrades to plain gossip, not to dropping the transaction.
    pub fn successor(&mut self, peers: &[P], now: Instant, rng: &mut impl Rng) -> Option<P> {
        if peers.is_empty() {
            self.current = None;
            return None;
        }
        let epoch_elapsed = now.saturating_duration_since(self.epoch_start);
        if self.current.is_none() || epoch_elapsed >= self.epoch {
            let idx = rng.gen_range(0..peers.len());
            self.current = Some(peers[idx].clone());
            self.epoch_start = now;
        }
        self.current.clone()
    }

    /// Force a reroll on the next [`successor`](Self::successor) call — used when the
    /// current successor is known to be gone (e.g. a stem send failed), so a dead peer
    /// doesn't keep absorbing this node's whole epoch.
    pub fn invalidate(&mut self) {
        self.current = None;
    }
}

/// What the caller must do with a transaction after handing it to the router.
///
/// The router never performs network I/O itself — see the module docs' Integration
/// Status section for exactly what still needs wiring into a live transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action<P> {
    /// Send `bytes` to exactly one peer over a point-to-point channel (flux-p2p
    /// `send_request`, a WireGuard link, or a Tor circuit — this crate does not care which).
    /// The receiving peer must NOT learn this is the transaction's origin; a stem hop looks
    /// identical whether the sender created the transaction or is relaying it.
    ///
    /// `hops` is the count to EMBED IN THE OUTBOUND WIRE MESSAGE, not a value this crate
    /// remembers on the sender's behalf. Each `DandelionRouter` is one node's local state —
    /// it has no memory of how many stem hops a transaction has already taken elsewhere on
    /// the network, since a never-before-seen transaction always looks "fresh" to whichever
    /// node next receives it. The hop count therefore has to ride ON the message itself
    /// (exactly how `q-dandelion`'s `DandelionMessage.hop_count` field works); the receiving
    /// node reads it back off the wire and passes it into
    /// [`relay_stem_incoming`](DandelionRouter::relay_stem_incoming) as `incoming_hops`.
    RelayStem { to: P, hops: u8, bytes: Vec<u8> },
    /// Broadcast `bytes` on the normal gossip topic. Fluffing is terminal for this
    /// transaction id — the router will not stem it again.
    Fluff { bytes: Vec<u8> },
    /// Already seen this transaction id; do nothing. Dandelion++ relies on this dedup to
    /// keep stem paths from cycling and to keep a fluffed message from re-entering stem.
    Drop,
}

#[derive(Clone)]
struct PendingStem {
    hops: u8,
    first_seen: Instant,
}

/// The per-node Dandelion++ state machine: phase decisions, hop counting, dedup, and the
/// failsafe liveness floor. See the module docs for the full design and integration status.
pub struct DandelionRouter<P: Clone + Eq + Hash> {
    config: DandelionConfig,
    stem_graph: StemGraph<P>,
    pending: HashMap<[u8; 32], PendingStem>,
    seen: HashMap<[u8; 32], Instant>,
}

impl<P: Clone + Eq + Hash> DandelionRouter<P> {
    pub fn new(config: DandelionConfig, now: Instant) -> Self {
        let stem_graph = StemGraph::new(config.epoch, now);
        Self { config, stem_graph, pending: HashMap::new(), seen: HashMap::new() }
    }

    fn mark_seen(&mut self, id: [u8; 32], now: Instant) -> bool {
        if self.seen.contains_key(&id) {
            return false;
        }
        self.seen.insert(id, now);
        true
    }

    /// A transaction this node itself created. Rolls the stem-vs-fluff coin at
    /// [`DandelionConfig::stem_probability`]; the fluff branch is what keeps a network of
    /// all-adversarial relays from being able to censor-by-starvation — see module docs.
    pub fn originate(
        &mut self,
        id: [u8; 32],
        bytes: Vec<u8>,
        peers: &[P],
        now: Instant,
        rng: &mut impl Rng,
    ) -> Action<P> {
        if !self.mark_seen(id, now) {
            return Action::Drop;
        }
        let start_stem = rng.gen::<f64>() < self.config.stem_probability;
        if start_stem {
            if let Some(succ) = self.stem_graph.successor(peers, now, rng) {
                self.pending.insert(id, PendingStem { hops: 1, first_seen: now });
                return Action::RelayStem { to: succ, hops: 1, bytes };
            }
        }
        Action::Fluff { bytes }
    }

    /// A transaction arriving from a peer while still in stem phase. `incoming_hops` is the
    /// hop count read off the wire message (see [`Action::RelayStem`]'s docs for why this
    /// crate cannot reconstruct it from local state alone). Continues the stem with
    /// probability [`DandelionConfig::stem_continue_probability`], bounded by
    /// [`DandelionConfig::max_stem_hops`]; otherwise this hop is where it fluffs.
    ///
    /// Deliberately symmetric with [`originate`](Self::originate) in every way an observer
    /// could measure: a relay hop and an origin hop produce structurally identical
    /// `RelayStem`/`Fluff` actions, which is what makes "who started this" unrecoverable
    /// from watching the network alone.
    pub fn relay_stem_incoming(
        &mut self,
        id: [u8; 32],
        incoming_hops: u8,
        bytes: Vec<u8>,
        peers: &[P],
        now: Instant,
        rng: &mut impl Rng,
    ) -> Action<P> {
        if !self.mark_seen(id, now) {
            return Action::Drop;
        }
        let hops = incoming_hops.saturating_add(1);
        let continue_stem =
            hops < self.config.max_stem_hops && rng.gen::<f64>() < self.config.stem_continue_probability;
        if continue_stem {
            if let Some(succ) = self.stem_graph.successor(peers, now, rng) {
                self.pending.insert(id, PendingStem { hops, first_seen: now });
                return Action::RelayStem { to: succ, hops, bytes };
            }
        }
        self.pending.remove(&id);
        Action::Fluff { bytes }
    }

    /// A transaction arriving already in fluff phase (or being fluffed by this node).
    /// Terminal: dedup only, no further stem bookkeeping.
    pub fn relay_fluff_incoming(&mut self, id: [u8; 32], bytes: Vec<u8>, now: Instant) -> Action<P> {
        if !self.mark_seen(id, now) {
            return Action::Drop;
        }
        self.pending.remove(&id);
        Action::Fluff { bytes }
    }

    /// The liveness floor: transaction ids that have been stemming longer than
    /// [`DandelionConfig::failsafe_timeout`] and must be force-fluffed NOW.
    ///
    /// Returns only the ids, not the original bytes — the router does not retain a copy of
    /// pending transaction payloads (the caller's mempool already holds them; duplicating
    /// that storage here would be a second place for it to drift). The caller looks each id
    /// up in its own mempool and re-fluffs it directly; this is why `tick` takes no `rng`
    /// and returns no `Action` — there is no decision left to make, only an instruction.
    pub fn tick(&mut self, now: Instant) -> Vec<[u8; 32]> {
        let timeout = self.config.failsafe_timeout;
        let expired: Vec<[u8; 32]> = self
            .pending
            .iter()
            .filter(|(_, p)| now.saturating_duration_since(p.first_seen) >= timeout)
            .map(|(id, _)| *id)
            .collect();
        for id in &expired {
            self.pending.remove(id);
        }
        expired
    }

    /// Evict dedup entries older than [`DandelionConfig::seen_ttl`], bounding memory on a
    /// long-running node. Call periodically alongside [`tick`](Self::tick).
    pub fn cleanup_seen(&mut self, now: Instant) {
        let ttl = self.config.seen_ttl;
        self.seen.retain(|_, t| now.saturating_duration_since(*t) < ttl);
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
    pub fn seen_count(&self) -> usize {
        self.seen.len()
    }
    /// Tell the router the current stem successor is gone (e.g. a `RelayStem` send failed),
    /// so subsequent `successor()` calls this epoch pick someone else instead of retrying a
    /// dead peer for the rest of the epoch window.
    pub fn invalidate_successor(&mut self) {
        self.stem_graph.invalidate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn id(n: u8) -> [u8; 32] {
        [n; 32]
    }

    fn peers(n: usize) -> Vec<u32> {
        (0..n as u32).collect()
    }

    /// THE EPOCH GATE: within one epoch, the successor never changes no matter how many
    /// times it is queried; once the epoch elapses, it is free to change. If this ever
    /// starts rerolling every call, the crate has regressed to `q-dandelion`'s weaker
    /// per-message design the module docs explicitly say this improves on.
    #[test]
    fn stem_successor_is_fixed_within_an_epoch_and_may_change_across_epochs() {
        let mut rng = rand::thread_rng();
        let epoch = Duration::from_secs(60);
        let t0 = Instant::now();
        let mut graph: StemGraph<u32> = StemGraph::new(epoch, t0);
        let p = peers(50);

        let first = graph.successor(&p, t0, &mut rng).unwrap();
        for step in 1..20 {
            let t = t0 + Duration::from_millis(step * 100);
            assert_eq!(
                graph.successor(&p, t, &mut rng),
                Some(first),
                "successor must not change before the epoch elapses"
            );
        }

        // Advance many independent epochs with a large peer set and confirm the successor
        // is not pinned forever — a statistical check, not a single flaky sample.
        let mut saw_different = false;
        let mut t = t0;
        for _ in 0..200 {
            t += epoch + Duration::from_secs(1);
            let s = graph.successor(&p, t, &mut rng).unwrap();
            if s != first {
                saw_different = true;
                break;
            }
        }
        assert!(saw_different, "successor must be able to change once an epoch elapses");
    }

    #[test]
    fn empty_peer_list_yields_no_successor() {
        let mut rng = rand::thread_rng();
        let t0 = Instant::now();
        let mut graph: StemGraph<u32> = StemGraph::new(Duration::from_secs(60), t0);
        assert_eq!(graph.successor(&[], t0, &mut rng), None);
    }

    #[test]
    fn originate_with_no_peers_falls_back_to_fluff() {
        let mut rng = rand::thread_rng();
        let t0 = Instant::now();
        let mut r: DandelionRouter<u32> = DandelionRouter::new(DandelionConfig::default(), t0);
        let action = r.originate(id(1), vec![1, 2, 3], &[], t0, &mut rng);
        assert_eq!(action, Action::Fluff { bytes: vec![1, 2, 3] });
        assert_eq!(r.pending_count(), 0, "nothing should be tracked as stemming with no peers");
    }

    /// Roughly `stem_probability` of fresh originations should start in stem phase.
    #[test]
    fn originate_respects_stem_probability_statistically() {
        let mut rng = rand::thread_rng();
        let t0 = Instant::now();
        let cfg = DandelionConfig { stem_probability: 0.9, ..Default::default() };
        let p = peers(10);
        let mut stem_count = 0;
        const N: u32 = 2000;
        for i in 0..N {
            let mut r: DandelionRouter<u32> = DandelionRouter::new(cfg.clone(), t0);
            let action = r.originate(id((i % 250) as u8), vec![0u8], &p, t0, &mut rng);
            if matches!(action, Action::RelayStem { .. }) {
                stem_count += 1;
            }
        }
        let ratio = stem_count as f64 / N as f64;
        assert!(
            (ratio - 0.9).abs() < 0.05,
            "expected ~90% stem starts, got {:.1}% over {N} trials",
            ratio * 100.0
        );
    }

    /// THE LIVENESS-VS-PRIVACY GATE: a stemmed transaction, relayed hop after hop across a
    /// CHAIN OF DISTINCT PEERS, must ALWAYS eventually fluff — never stem forever. This is
    /// what stops a privacy feature from silently becoming a censorship / stuck-transaction
    /// bug. One [`DandelionRouter`] per hop, matching real deployment: each stem hop is a
    /// different physical node with its own dedup set, not the same router reprocessing its
    /// own traffic (which the `seen` dedup would — correctly — refuse, since `originate`
    /// and `relay_stem_incoming` sharing one `seen` set is what stops a node from
    /// re-stemming a transaction back to itself).
    #[test]
    fn stem_relay_eventually_fluffs_within_max_hops() {
        let mut rng = rand::thread_rng();
        let t0 = Instant::now();
        // Force max_stem_hops to bound the worst case deterministically even if every
        // continuation roll succeeds.
        let cfg = DandelionConfig {
            stem_probability: 1.0, // deterministic: this test is about the hop bound, not the initial coin flip
            stem_continue_probability: 1.0,
            max_stem_hops: 5,
            ..Default::default()
        };
        let p = peers(20);
        let origin_router: DandelionRouter<u32> = DandelionRouter::new(cfg.clone(), t0);
        let mut hop_routers: HashMap<u32, DandelionRouter<u32>> =
            p.iter().map(|peer| (*peer, DandelionRouter::new(cfg.clone(), t0))).collect();
        let tx = id(7);

        // Exclude every hop already visited from the NEXT hop's candidate pool. A real
        // stem path can legitimately revisit a peer (each node independently reselects a
        // successor from its own full peer list) and a revisited node correctly Drops the
        // duplicate via its dedup set — that is not a liveness bug, it means that copy of
        // the message was already delivered via an earlier path. But this test walks a
        // SINGLE simulated chain and can only see one branch at a time, so a revisit here
        // would be a test-harness artifact indistinguishable from a real stuck-transaction
        // bug. Excluding visited hops isolates the property this test actually checks
        // (bounded, eventually-terminating hop count along one path) from that separate,
        // already-correct dedup behavior. 20 peers vs. <=5 hops leaves ample room, so this
        // never starves the candidate pool.
        let mut visited: std::collections::HashSet<u32> = std::collections::HashSet::new();

        let mut origin_router = origin_router;
        let mut action = origin_router.originate(tx, vec![9], &p, t0, &mut rng);
        let mut hops = 0u32;
        loop {
            match action {
                Action::RelayStem { to, hops: wire_hops, bytes } => {
                    hops += 1;
                    assert!(hops <= cfg.max_stem_hops as u32 + 1, "stem relay exceeded max_stem_hops");
                    visited.insert(to);
                    let candidates: Vec<u32> = p.iter().copied().filter(|x| !visited.contains(x)).collect();
                    let hop = hop_routers.get_mut(&to).expect("stem target must be a known peer");
                    action = hop.relay_stem_incoming(tx, wire_hops, bytes, &candidates, t0, &mut rng);
                }
                Action::Fluff { .. } => break,
                Action::Drop => panic!("must not drop a live in-flight transaction"),
            }
        }
        assert!(hops >= 1, "sanity: this run should have stemmed at least once");
    }

    #[test]
    fn duplicate_message_is_dropped_not_reprocessed() {
        let mut rng = rand::thread_rng();
        let t0 = Instant::now();
        let p = peers(5);
        let mut r: DandelionRouter<u32> = DandelionRouter::new(DandelionConfig::default(), t0);
        let tx = id(3);
        let _ = r.originate(tx, vec![1], &p, t0, &mut rng);
        let second = r.relay_stem_incoming(tx, 1, vec![1], &p, t0, &mut rng);
        assert_eq!(second, Action::Drop, "SECURITY: a message seen twice must not be relayed twice");
    }

    /// THE FAILSAFE GATE: if a stem successor never relays onward (offline, malicious, or
    /// just slow), the transaction must still reach the network once the timeout elapses.
    #[test]
    fn failsafe_force_fluffs_a_stalled_stem_after_timeout() {
        let mut rng = rand::thread_rng();
        let t0 = Instant::now();
        let cfg = DandelionConfig {
            stem_probability: 1.0, // deterministic: this test is about the timeout, not the coin flip
            failsafe_timeout: Duration::from_secs(10),
            ..Default::default()
        };
        let p = peers(5);
        let mut r: DandelionRouter<u32> = DandelionRouter::new(cfg.clone(), t0);
        let tx = id(11);
        let action = r.originate(tx, vec![1], &p, t0, &mut rng);
        assert!(matches!(action, Action::RelayStem { .. }), "expected the tx to enter stem for this test");
        assert_eq!(r.tick(t0 + Duration::from_secs(5)), Vec::<[u8; 32]>::new(), "not expired yet");

        let expired = r.tick(t0 + Duration::from_secs(11));
        assert_eq!(expired, vec![tx], "SECURITY: a stalled stem must be force-fluffed after the timeout");
        assert_eq!(r.pending_count(), 0, "the expired entry must be cleared, not repeatedly re-reported");
    }

    #[test]
    fn cleanup_seen_evicts_old_entries_but_keeps_recent_ones() {
        let mut rng = rand::thread_rng();
        let t0 = Instant::now();
        let cfg = DandelionConfig { seen_ttl: Duration::from_secs(10), ..Default::default() };
        let p = peers(5);
        let mut r: DandelionRouter<u32> = DandelionRouter::new(cfg, t0);
        r.originate(id(1), vec![1], &p, t0, &mut rng);
        r.originate(id(2), vec![1], &p, t0 + Duration::from_secs(15), &mut rng);
        assert_eq!(r.seen_count(), 2);

        r.cleanup_seen(t0 + Duration::from_secs(16));
        assert_eq!(r.seen_count(), 1, "only the entry older than seen_ttl should be evicted");
    }

    /// A statistical check that the continuation probability actually shapes the hop-count
    /// distribution, not just that SOME bound exists (the max-hops test above already pins
    /// the hard ceiling; this pins the geometric shape below it). One router per hop, same
    /// reasoning as [`stem_relay_eventually_fluffs_within_max_hops`].
    ///
    /// The math: the origin always contributes one guaranteed hop (`stem_probability` is
    /// forced to 1.0), then each further hop is a Bernoulli(`stem_continue_probability`)
    /// trial. Total hops = `1 + Geometric(continue_probability)`, whose mean is
    /// `1 + p/(1-p)`; at `p = 0.5` that is `1 + 1 = 2`, not `1`.
    #[test]
    fn stem_continue_probability_shapes_average_hop_count() {
        let mut rng = rand::thread_rng();
        let t0 = Instant::now();
        let cfg = DandelionConfig {
            stem_probability: 1.0, // force stem start every time for a clean sample
            stem_continue_probability: 0.5,
            max_stem_hops: 32, // high enough not to clip the geometric tail
            ..Default::default()
        };
        let p = peers(30);
        let mut total_hops: u64 = 0;
        const TRIALS: u32 = 1000;
        for i in 0..TRIALS {
            let origin_router: DandelionRouter<u32> = DandelionRouter::new(cfg.clone(), t0);
            let mut hop_routers: HashMap<u32, DandelionRouter<u32>> =
                p.iter().map(|peer| (*peer, DandelionRouter::new(cfg.clone(), t0))).collect();
            let mut visited: std::collections::HashSet<u32> = std::collections::HashSet::new();
            let tx = id((i % 250) as u8);

            let mut origin_router = origin_router;
            let mut action = origin_router.originate(tx, vec![0], &p, t0, &mut rng);
            let mut hops = 0u64;
            loop {
                match action {
                    Action::RelayStem { to, hops: wire_hops, bytes } => {
                        hops += 1;
                        visited.insert(to);
                        let candidates: Vec<u32> =
                            p.iter().copied().filter(|x| !visited.contains(x)).collect();
                        let hop = hop_routers.get_mut(&to).expect("stem target must be a known peer");
                        action = hop.relay_stem_incoming(tx, wire_hops, bytes, &candidates, t0, &mut rng);
                    }
                    _ => break,
                }
            }
            total_hops += hops;
        }
        let avg = total_hops as f64 / TRIALS as f64;
        assert!(
            (avg - 2.0).abs() < 0.3,
            "expected ~2.0 average stem hops at p=0.5 continuation (1 guaranteed + geometric mean 1), \
             got {avg:.2} over {TRIALS} trials"
        );
    }
}
