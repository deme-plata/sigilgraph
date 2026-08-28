//! `finality_wire` — the node-side half of **SIGIL True Instant Finality**
//! Phase 2 (`docs/research/SIGIL_INSTANT_FINALITY_v0.tex`, phased plan row 2).
//!
//! Phase 2's whole deliverable is a *measurement*, not a behaviour change:
//!
//! > P2P gossip topic, local vote tallying, "would-be finalized height"
//! > logged alongside today's estimate; `Braid::insert()` untouched.
//! > *Consensus effect: none.*
//!
//! `sigil_finality::observer` holds the pure state machine. This module is
//! only the plumbing between that state machine and the node's event loop:
//! env configuration, wire encode/decode, and the four call sites.
//!
//! ## Why this is a separate file
//!
//! `main.rs` is contended — several agents hold overlapping work in it at
//! any time. Keeping every decision here means the finality feature costs
//! `main.rs` exactly four one-line calls ([`FinalityWire::on_block`],
//! [`FinalityWire::on_gossip`], [`FinalityWire::heartbeat_line`], and the
//! constructor), which is both easier to review and easier to revert. It is
//! the same shape `sigil-api/src/attribution.rs` uses for the same reason.
//!
//! ## The safety posture, stated plainly
//!
//! Nothing in this file can change what the chain orders or accepts:
//!
//!  - It never calls `Braid::insert`, never writes storage, and never
//!    returns a value that block production or validation consumes.
//!  - [`FinalityWire::on_block`] takes already-produced block data and
//!    returns an *optional payload to gossip*. If the caller drops that
//!    payload on the floor, the chain is unaffected.
//!  - [`FinalityWire::on_gossip`] consumes bytes and updates a counter.
//!    Its return value is a log line.
//!  - A node with `SIGIL_FINALITY_COMMITTEE` unset builds a
//!    [`FinalityWire::disabled`] instance where every method is a no-op, so
//!    the default configuration is byte-for-byte today's behaviour.
//!
//! Gating consensus on these certificates is Phase 3 — opt-in flag,
//! isolated testnet first, and explicitly out of scope here.

use sigil_finality::observer::{
    env_config, FinalityObserver, ObserveOutcome, ObserverConfig, ObserverReport,
};
use sigil_finality::FinalityVote;
use sigil_header::BlockHash;

use ed25519_dalek::SigningKey;

/// Node-side Phase 2 plumbing. Cheap to construct, inert when unconfigured.
pub struct FinalityWire {
    observer: FinalityObserver,
    /// This node's validator key, if it holds one that is actually in the
    /// committee. `None` = pure observer, which is the common case and a
    /// useful one: an observer still measures latency from votes it sees.
    signing_key: Option<SigningKey>,
    /// Set once so the operator gets one clear line about what finality is
    /// doing, rather than silence they have to infer meaning from.
    announced: bool,
}

impl FinalityWire {
    /// Build from `SIGIL_FINALITY_*` environment. Never fails the node: a
    /// misconfiguration disables finality and says so loudly, because a
    /// node refusing to boot over an observational diagnostic would be a
    /// far worse failure than not having the diagnostic.
    pub fn from_env() -> Self {
        match env_config::from_env() {
            Ok((committee, key, cfg)) => {
                let n = committee.len();
                let quorum = committee.availability_quorum();
                let bft = committee.bft_active();
                let observer = FinalityObserver::new(committee, cfg.clone());
                if !bft {
                    eprintln!(
                        "⚠ finality: committee n={n} is below the n>=4 BFT floor — certificates \
                         will assemble but do NOT mean 'protected against a malicious minority'"
                    );
                }
                eprintln!(
                    "🔗 finality Phase 2 (OBSERVATIONAL, zero consensus effect): committee n={n} \
                     quorum={quorum} checkpoint_interval={} role={}",
                    cfg.checkpoint_interval,
                    if key.is_some() { "VALIDATOR" } else { "observer" }
                );
                Self { observer, signing_key: key, announced: true }
            }
            Err(env_config::ConfigError::Disabled) => Self::disabled(),
            Err(e) => {
                // Fail CLOSED and loud. A malformed committee entry must
                // never silently shrink the committee — a smaller committee
                // has a smaller quorum, which is a safety-relevant surprise
                // an operator would have no way to notice.
                eprintln!(
                    "⚠ finality: DISABLED — SIGIL_FINALITY_* config rejected ({e:?}). \
                     Node continues normally on today's {}-block depth rule.",
                    sigil_finality::observer::DEPTH_RULE_BLOCKS
                );
                Self::disabled()
            }
        }
    }

    /// A permanently inert wire — what an unconfigured node gets.
    pub fn disabled() -> Self {
        Self {
            observer: FinalityObserver::new(Default::default(), ObserverConfig::default()),
            signing_key: None,
            announced: false,
        }
    }

    /// Is finality doing anything on this node?
    pub fn enabled(&self) -> bool {
        self.observer.enabled()
    }

    /// Call when a block has been applied at `height`.
    ///
    /// `order_hash` should be the braid's own topology commitment for this
    /// height (`crate::dag::compute_topology_commitment`) — the real
    /// DagKnight-derived order commitment that already rides in the header.
    /// When the braid is not active (`SIGIL_DAG=0`) there is no such
    /// commitment, and passing `None` degrades the vote to committing over
    /// the spine hash alone. That is honest for Phase 2's purpose
    /// (measuring latency) but is NOT sufficient for Phase 3, which must
    /// commit to a real order or it is certifying less than it claims.
    ///
    /// Returns the bytes to publish on
    /// [`sigil_net::TOPIC_FINALITY_VOTES`], or `None` when this node has
    /// nothing to say (disabled, not a checkpoint, or not a validator).
    /// **The caller may discard the return value with no consequence to the
    /// chain.**
    pub fn on_block(
        &mut self,
        height: u64,
        spine_block_hash: BlockHash,
        order_hash: Option<[u8; 32]>,
        now_ms: u64,
    ) -> Option<Vec<u8>> {
        if !self.enabled() || !self.observer.is_checkpoint(height) {
            return None;
        }
        // Start the latency clock the moment the height is known locally —
        // not when the first vote arrives, or the measurement would quietly
        // exclude the network time it is supposed to be measuring.
        self.observer.note_checkpoint_seen(height, now_ms);

        let order = order_hash.unwrap_or(spine_block_hash);
        let key = self.signing_key.as_ref()?;
        let vote = self.observer.own_vote(key, height, spine_block_hash, order)?;

        // Count our own vote locally too. Gossipsub does not loop a
        // publisher's own message back to it, so without this the node
        // would be one vote short of what every peer sees — and on a 4-node
        // committee with quorum 3 that is the difference between measuring
        // finality and never observing it at all.
        self.observer.observe(vote.clone(), now_ms);

        encode_vote(&vote)
    }

    /// Call for every message on [`sigil_net::TOPIC_FINALITY_VOTES`].
    ///
    /// Returns `Some(line)` only for events worth an operator's attention —
    /// a certificate forming, or an equivocation. Ordinary accepted votes
    /// and ordinary rejections return `None`: on a public gossip topic
    /// anyone can publish anything, so a rejected vote is the system
    /// working, not an incident, and logging each one would be a
    /// remote-triggerable log flood.
    pub fn on_gossip(&mut self, data: &[u8], now_ms: u64) -> Option<String> {
        if !self.enabled() {
            return None;
        }
        let vote = decode_vote(data)?;
        match self.observer.observe(vote, now_ms) {
            ObserveOutcome::Certified { height, latency_ms } => Some(format!(
                "🔒 finality: certificate at H={height} (assembled in {})",
                latency_ms.map(|m| format!("{m}ms")).unwrap_or_else(|| "unknown".into())
            )),
            _ => None,
        }
    }

    /// The Phase 2 log line, for the node's existing 5s heartbeat.
    /// `None` when finality is disabled — an unconfigured node should not
    /// gain a new recurring log line it did not ask for.
    pub fn heartbeat_line(&self, tip_height: u64) -> Option<String> {
        if !self.enabled() {
            return None;
        }
        Some(self.observer.report(tip_height).verdict())
    }

    /// Full structured report, for a future `/v1/finality` route.
    pub fn report(&self, tip_height: u64) -> Option<ObserverReport> {
        self.enabled().then(|| self.observer.report(tip_height))
    }

    /// Whether the startup banner was printed (test/introspection helper).
    pub fn announced(&self) -> bool {
        self.announced
    }
}

/// Wire encoding for a vote: `serde_json`, matching the precedent already
/// set by the peer-heights heartbeat in `main.rs` rather than introducing a
/// second control-plane codec. Votes are small and infrequent (one per
/// validator per checkpoint), so the compactness argument for `bincode`
/// does not apply, and being human-readable on the wire is worth more while
/// this is a diagnostic.
pub fn encode_vote(vote: &FinalityVote) -> Option<Vec<u8>> {
    serde_json::to_vec(vote).ok()
}

/// Decode a gossiped vote. Returns `None` on anything malformed — this is
/// attacker-controlled input arriving on a public topic, so it must never
/// panic and never propagate an error that could unwind the event loop.
/// Signature and committee-membership checks are NOT done here; they belong
/// to `sigil_finality::assemble`, which is the single authority on vote
/// legitimacy.
pub fn decode_vote(data: &[u8]) -> Option<FinalityVote> {
    serde_json::from_slice::<FinalityVote>(data).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(i: u8) -> SigningKey {
        let mut seed = [0u8; 32];
        seed[0] = i;
        SigningKey::from_bytes(&seed)
    }

    #[test]
    fn an_unconfigured_node_is_completely_inert() {
        let mut w = FinalityWire::disabled();
        assert!(!w.enabled());
        assert_eq!(w.on_block(320, [1u8; 32], None, 0), None);
        assert_eq!(w.on_gossip(b"anything", 0), None);
        assert_eq!(w.heartbeat_line(100_000), None, "a disabled node must not gain a log line");
        assert!(w.report(1).is_none());
    }

    #[test]
    fn vote_round_trips_over_the_wire() {
        let v = FinalityVote::sign(&key(1), 320, [7u8; 32], [9u8; 32]);
        let bytes = encode_vote(&v).expect("encode");
        assert_eq!(decode_vote(&bytes).as_ref(), Some(&v));
    }

    #[test]
    fn malformed_gossip_is_dropped_never_panics() {
        assert!(decode_vote(b"").is_none());
        assert!(decode_vote(b"not json").is_none());
        assert!(decode_vote(br#"{"height":1}"#).is_none(), "partial struct must not decode");
        // A 1 MiB junk payload is the shape a hostile peer would actually send.
        assert!(decode_vote(&vec![0xffu8; 1 << 20]).is_none());
    }

    #[test]
    fn on_block_returns_none_off_checkpoints_and_for_non_validators() {
        // Committee of 4 that does NOT include our key: we must not sign.
        let members: Vec<([u8; 32], [u8; 32])> =
            (10..14u8).map(|i| (key(i).verifying_key().to_bytes(), key(i).verifying_key().to_bytes())).collect();
        let committee = sigil_braidpool::committee::Committee::new(members);
        let cfg = ObserverConfig { checkpoint_interval: 32, ..Default::default() };
        let mut w = FinalityWire {
            observer: FinalityObserver::new(committee, cfg),
            signing_key: Some(key(99)),
            announced: true,
        };
        assert!(w.enabled());
        assert_eq!(w.on_block(321, [1u8; 32], None, 0), None, "not a checkpoint height");
        assert_eq!(w.on_block(320, [1u8; 32], None, 0), None, "non-member must not publish a vote");
        // But it still measures: the checkpoint clock started.
        assert!(w.heartbeat_line(320).is_some());
    }

    #[test]
    fn a_validator_publishes_and_self_counts_its_own_vote() {
        let ks: Vec<SigningKey> = (1..5u8).map(key).collect();
        let members: Vec<([u8; 32], [u8; 32])> =
            ks.iter().map(|k| (k.verifying_key().to_bytes(), k.verifying_key().to_bytes())).collect();
        let committee = sigil_braidpool::committee::Committee::new(members);
        let cfg = ObserverConfig { checkpoint_interval: 32, ..Default::default() };
        let mut w = FinalityWire {
            observer: FinalityObserver::new(committee, cfg),
            signing_key: Some(ks[0].clone()),
            announced: true,
        };
        let bytes = w.on_block(320, [7u8; 32], Some([9u8; 32]), 1_000).expect("validator must publish");
        assert!(decode_vote(&bytes).is_some());

        // Gossipsub never echoes a publisher's own message back, so the
        // node must have counted itself — two more peers then complete the
        // quorum of 3.
        for k in ks.iter().skip(1).take(1) {
            let v = FinalityVote::sign(k, 320, [7u8; 32], [9u8; 32]);
            assert!(w.on_gossip(&encode_vote(&v).unwrap(), 1_100).is_none());
        }
        let v = FinalityVote::sign(&ks[2], 320, [7u8; 32], [9u8; 32]);
        let line = w.on_gossip(&encode_vote(&v).unwrap(), 1_400).expect("third distinct vote must certify");
        assert!(line.contains("H=320"), "line: {line}");
        assert!(line.contains("400ms"), "latency must be measured from on_block: {line}");
    }

    #[test]
    fn order_hash_falls_back_to_the_spine_hash_when_the_braid_is_off() {
        let ks: Vec<SigningKey> = (1..5u8).map(key).collect();
        let members: Vec<([u8; 32], [u8; 32])> =
            ks.iter().map(|k| (k.verifying_key().to_bytes(), k.verifying_key().to_bytes())).collect();
        let committee = sigil_braidpool::committee::Committee::new(members);
        let cfg = ObserverConfig { checkpoint_interval: 32, ..Default::default() };
        let mut w = FinalityWire {
            observer: FinalityObserver::new(committee, cfg),
            signing_key: Some(ks[0].clone()),
            announced: true,
        };
        let bytes = w.on_block(320, [7u8; 32], None, 0).unwrap();
        let v = decode_vote(&bytes).unwrap();
        assert_eq!(v.order_hash, [7u8; 32], "with no braid commitment the spine hash is the fallback");
    }
}

/// Wall-clock milliseconds, for latency measurement only.
///
/// Deliberately NOT used for any ordering or eviction decision: braid state
/// has to converge identically on every node and a clock does not (the same
/// reasoning `BraidConfig::pending_max_tip_lag` documents for measuring its
/// bound in tip height rather than wall-clock). Here it only timestamps a
/// diagnostic.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
