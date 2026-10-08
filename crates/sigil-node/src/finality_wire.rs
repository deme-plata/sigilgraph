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
    /// Wall clock of the last "certificate at H=" log line (rate-limited to 1/s).
    last_cert_log_ms: u64,
    /// Highest height ever handed to [`FinalityWire::on_block`] — i.e. the
    /// MINT clock.
    ///
    /// This exists because the node runs two different height clocks and the
    /// Phase 2 report was silently mixing them. Votes are cast at mint time,
    /// but `main.rs` had the only convenient tip to hand — `chain.height()`,
    /// the SETTLED height — and settlement is itself gated on
    /// `finalized_height() = tip - final_depth`, so the settled clock trails
    /// the mint clock by roughly `final_depth`.
    ///
    /// Measured on the live producer 2026-08-28 at the same instant:
    /// settled `H=314041`, certified height `314528` — 487 apart. The report
    /// therefore printed `+1010 blocks` ahead of the depth rule when the
    /// genuine saving was ~512: the other ~490 was the clock offset, counted
    /// as if it were a speedup. Roughly a 2x overstatement of the whole
    /// feature's benefit, in the one number the feature exists to produce.
    last_vote_height: u64,
    /// The signer guard (2026-10-08): the last `(height, block)` this node SIGNED,
    /// persisted, so no height is ever signed twice — not even across a restart.
    guard: VoteGuard,
    /// `SIGIL_FINALITY_VOTE_DEPTH`. `0` (default) = the legacy rule, a vote for every
    /// accepted block (unchanged g2 behaviour). `> 0` = spine voting: one vote per
    /// height, for the selected-spine block this many heights below the selected tip,
    /// through [`FinalityWire::on_spine_vote`].
    vote_depth: u64,
}

/// 2026-10-08 (mainnet roadmap step 1): the SIGNER GUARD.
///
/// Measured on the 4-node rehearsal net with two producers: every validator voted for
/// every block it accepted, so at each height the two followers voted for BOTH
/// producers' blocks. r1+r3+r4 then certified producer 1's block and r2+r3+r4
/// certified producer 2's block at the same height — two valid 3-of-4 certificates,
/// and the net split into two settled chains from H=41 (14 FINALITY VIOLATIONs). The
/// equivocation filter in `assemble` cannot see this: each node's vote set may contain
/// only one of a follower's two votes.
///
/// BFT safety assumes an honest validator signs at most ONE block per height. This is
/// that rule, enforced where the key is used, and persisted before the vote is sent:
/// a crash between "signed" and "recorded" must not let the restarted node sign the
/// same height again.
#[derive(Debug, Default)]
pub struct VoteGuard {
    path: Option<std::path::PathBuf>,
    last: Option<(u64, BlockHash)>,
}

impl VoteGuard {
    pub const FILE: &'static str = "finality-vote-guard.json";

    /// Load from `dir`. A missing file is a fresh validator. An unreadable or
    /// malformed one is NOT treated as fresh: we refuse to sign until it is fixed,
    /// because "forgot what I signed" is exactly the state that double-signs.
    pub fn load(dir: &std::path::Path) -> Self {
        let path = dir.join(Self::FILE);
        let last = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<serde_json::Value>(&bytes).ok().and_then(|v| {
                let h = v.get("height")?.as_u64()?;
                let hash: BlockHash = hex::decode(v.get("hash")?.as_str()?).ok()?.try_into().ok()?;
                Some((h, hash))
            }) {
                Some(l) => Some(l),
                None => Some((u64::MAX, [0u8; 32])), // unreadable ⇒ sign nothing
            },
            Err(_) => None,
        };
        Self { path: Some(path), last }
    }

    pub fn last(&self) -> Option<(u64, BlockHash)> {
        self.last
    }

    /// Strictly above everything signed before.
    pub fn may_sign(&self, height: u64) -> bool {
        self.last.map_or(true, |(h, _)| height > h)
    }

    /// Record a signature at `(height, hash)`: write + fsync + rename, then remember.
    /// Without a path (tests, or a node with no data dir) it is remembered in RAM only.
    fn record(&mut self, height: u64, hash: BlockHash) -> std::io::Result<()> {
        if let Some(path) = &self.path {
            use std::io::Write;
            let tmp = path.with_extension("json.tmp");
            let body = serde_json::json!({ "height": height, "hash": hex::encode(hash) }).to_string();
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(body.as_bytes())?;
            f.sync_all()?;
            std::fs::rename(&tmp, path)?;
        }
        self.last = Some((height, hash));
        Ok(())
    }
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
                let vote_depth = std::env::var("SIGIL_FINALITY_VOTE_DEPTH").ok()
                    .and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(0);
                if vote_depth > 0 {
                    eprintln!("🗳 finality: SPINE voting — one vote per height, {vote_depth} below the selected tip (signer guard on)");
                }
                Self { observer, signing_key: key, announced: true, last_cert_log_ms: 0, last_vote_height: 0,
                       guard: VoteGuard::default(), vote_depth }
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
            last_cert_log_ms: 0,
            last_vote_height: 0,
            guard: VoteGuard::default(),
            vote_depth: 0,
        }
    }

    /// Attach the persisted signer guard (`<dir>/finality-vote-guard.json`).
    pub fn with_guard_dir(mut self, dir: &std::path::Path) -> Self {
        self.guard = VoteGuard::load(dir);
        if let Some((h, _)) = self.guard.last() {
            if h == u64::MAX {
                eprintln!("🛑 finality: {} is unreadable — this validator signs NOTHING until it is repaired", VoteGuard::FILE);
            } else {
                eprintln!("🗳 finality: signer guard restored — last signed H={h}; nothing at or below it will be signed again");
            }
        }
        self
    }

    /// See the `vote_depth` field.
    pub fn vote_depth(&self) -> u64 {
        self.vote_depth
    }

    /// The last `(height, block)` this node signed, if any.
    pub fn last_signed(&self) -> Option<(u64, BlockHash)> {
        self.guard.last()
    }

    pub fn is_checkpoint(&self, height: u64) -> bool {
        self.enabled() && self.observer.is_checkpoint(height)
    }

    /// Spine voting (`vote_depth > 0`): sign ONE vote for `(height, spine_block_hash)`,
    /// only above everything signed before, and record it durably BEFORE the vote is
    /// returned for publishing. The caller chooses the block (the selected-spine block
    /// at `tip - vote_depth`) and checks it extends our previous vote; this method
    /// enforces the one rule that must never depend on the caller.
    pub fn on_spine_vote(
        &mut self,
        height: u64,
        spine_block_hash: BlockHash,
        order_hash: Option<[u8; 32]>,
        now_ms: u64,
    ) -> Option<Vec<u8>> {
        if !self.enabled() || !self.observer.is_checkpoint(height) || !self.guard.may_sign(height) {
            return None;
        }
        self.observer.note_checkpoint_seen(height, now_ms);
        self.last_vote_height = self.last_vote_height.max(height);
        let order = order_hash.unwrap_or(spine_block_hash);
        let key = self.signing_key.as_ref()?;
        let vote = self.observer.own_vote(key, height, spine_block_hash, order)?;
        if let Err(e) = self.guard.record(height, spine_block_hash) {
            eprintln!("🛑 finality: could not persist the signer guard ({e}) — vote at H={height} NOT sent");
            return None;
        }
        self.observer.observe(vote.clone(), now_ms);
        encode_vote(&vote)
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
        // Remember the mint clock, so `heartbeat_line` can compare like with
        // like. Recorded before the validator early-return below: a pure
        // observer sees checkpoints too, and its report must not silently
        // fall back to the settled clock just because it holds no key.
        self.last_vote_height = self.last_vote_height.max(height);

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
            ObserveOutcome::Certified { height, latency_ms } => {
                // 2026-09-13 (rocky-bps100-0912): one certificate per block at 100+ blk/s
                // was 7,500 journal lines a minute — half the node's whole log volume
                // (the volatile journal held ~15 minutes). Once a second is a heartbeat;
                // the per-block numbers are on /v1/finality/certificate and /v1/events.
                if now_ms.saturating_sub(self.last_cert_log_ms) < 1000 {
                    return None;
                }
                self.last_cert_log_ms = now_ms;
                Some(format!(
                    "🔒 finality: certificate at H={height} (assembled in {})",
                    latency_ms.map(|m| format!("{m}ms")).unwrap_or_else(|| "unknown".into())
                ))
            }
            _ => None,
        }
    }

    /// The tip to measure the depth rule against.
    ///
    /// The depth rule and the certificate MUST be evaluated on the same
    /// clock or their difference is not a latency saving, it is a unit
    /// error — see [`FinalityWire::last_vote_height`] for the live numbers
    /// that made this concrete. `max` rather than "always the mint clock"
    /// so a node that has not yet minted (a pure observer, or one still
    /// syncing) still reports against the settled tip it does have, instead
    /// of measuring against height 0.
    fn measurement_tip(&self, settled_height: u64) -> u64 {
        self.last_vote_height.max(settled_height)
    }

    /// The Phase 2 log line, for the node's existing 5s heartbeat.
    /// `None` when finality is disabled — an unconfigured node should not
    /// gain a new recurring log line it did not ask for.
    ///
    /// `settled_height` is the caller's `chain.height()`; the comparison is
    /// made on [`FinalityWire::measurement_tip`].
    pub fn heartbeat_line(&self, settled_height: u64) -> Option<String> {
        if !self.enabled() {
            return None;
        }
        Some(self.observer.report(self.measurement_tip(settled_height)).verdict())
    }

    /// Full structured report, for a future `/v1/finality` route. Same
    /// same-clock correction as [`FinalityWire::heartbeat_line`].
    pub fn report(&self, settled_height: u64) -> Option<ObserverReport> {
        self.enabled().then(|| self.observer.report(self.measurement_tip(settled_height)))
    }

    /// Whether the startup banner was printed (test/introspection helper).
    pub fn announced(&self) -> bool {
        self.announced
    }

    /// The newest quorum certificate this node holds: `(height, spine block hash)`.
    /// Phase 3 feeds this to `Braid::set_certified` when the gate is on.
    pub fn latest_certificate(&self) -> Option<(u64, BlockHash)> {
        let h = self.observer.finalized_height()?;
        let c = self.observer.certificate_for(h)?;
        Some((h, c.spine_block_hash))
    }

    /// The newest certificate as the JSON `/v1/finality` publishes — everything a client
    /// needs to re-verify every vote. `gate` is the honesty label the node runs under.
    pub fn certificate_view(&self, gate: GateMode) -> Option<serde_json::Value> {
        let h = self.observer.finalized_height()?;
        let c = self.observer.certificate_for(h)?;
        let report = self.observer.report(0);
        let votes: Vec<serde_json::Value> = c.votes.iter().map(|v| serde_json::json!({
            "validator_id": hex::encode(v.validator_id),
            "signature": hex::encode(&v.signature),
        })).collect();
        Some(serde_json::json!({
            "height": c.height,
            "spine_block_hash": hex::encode(c.spine_block_hash),
            "order_hash": hex::encode(c.order_hash),
            "votes": votes,
            "committee_size": report.committee_size,
            "quorum": report.quorum,
            "bft": report.bft_active,
            "gate": match gate { GateMode::Solo { .. } => "solo", GateMode::Bft { .. } => "bft", _ => "off" },
            "certified_at_ms": now_ms(),
        }))
    }

    /// Committee size, for the gate's safety check.
    pub fn committee_size(&self) -> usize {
        self.observer.report(0).committee_size
    }

    /// Should certificates GATE settlement (Phase 3), and under which honesty label?
    ///
    /// `SIGIL_FINALITY_GATE` =
    ///   * unset / `0`   — off (Phase 2 observe-only, today's behaviour)
    ///   * `1` / `bft`   — on, but ONLY with a committee large enough for Byzantine
    ///                     fault tolerance (n ≥ 4); a smaller committee logs and stays off
    ///   * `solo`        — on regardless of committee size. This is the operator saying
    ///                     out loud that on a chain with one producer, that producer's
    ///                     signature IS the only source of truth there is, and a 512-block
    ///                     wait protects against nobody. It is not Byzantine safety and
    ///                     the log line says so on every boot.
    pub fn gate_mode(&self) -> GateMode {
        let raw = std::env::var("SIGIL_FINALITY_GATE").unwrap_or_default();
        let raw = raw.trim().to_ascii_lowercase();
        if !self.enabled() || raw.is_empty() || raw == "0" || raw == "off" {
            return GateMode::Off;
        }
        let n = self.committee_size();
        match raw.as_str() {
            "solo" => GateMode::Solo { committee: n },
            "1" | "bft" | "on" => {
                if sigil_finality::bft_active_for(n) {
                    GateMode::Bft { committee: n }
                } else {
                    GateMode::RefusedTooSmall { committee: n }
                }
            }
            _ => GateMode::Off,
        }
    }
}

/// See [`FinalityWire::gate_mode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateMode {
    Off,
    Solo { committee: usize },
    Bft { committee: usize },
    RefusedTooSmall { committee: usize },
}

impl GateMode {
    pub fn active(self) -> bool {
        matches!(self, GateMode::Solo { .. } | GateMode::Bft { .. })
    }
    pub fn banner(self) -> String {
        match self {
            GateMode::Off => "finality gate: OFF — certificates are observed, settlement follows the depth rule".into(),
            GateMode::Solo { committee } => format!(
                "🔒 finality gate: SOLO (committee n={committee}) — settlement follows certificates. \
                 NOT Byzantine-fault-tolerant: this trusts the committee's signatures outright."
            ),
            GateMode::Bft { committee } => format!(
                "🔒 finality gate: BFT (committee n={committee}, f={}) — settlement follows quorum certificates",
                sigil_finality::max_byzantine_for(committee)
            ),
            GateMode::RefusedTooSmall { committee } => format!(
                "⚠ finality gate: REFUSED — SIGIL_FINALITY_GATE=1 needs a committee of ≥4 for BFT, have {committee}. \
                 Set SIGIL_FINALITY_GATE=solo to gate on a small committee knowingly. Depth rule stays in force."
            ),
        }
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
            last_vote_height: 0,
            last_cert_log_ms: 0,
            guard: VoteGuard::default(),
            vote_depth: 0,
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
            last_vote_height: 0,
            last_cert_log_ms: 0,
            guard: VoteGuard::default(),
            vote_depth: 0,
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

    /// Regression: the depth rule and the certificate must be measured on
    /// the SAME height clock.
    ///
    /// This is not hypothetical. On the live producer, 2026-08-28, the
    /// heartbeat read `would-be-final=314528 vs depth-rule=313535
    /// (+1010 blocks)` while `chain.height()` — the settled clock the caller
    /// passes in — was `314041`. Settlement is gated on
    /// `finalized_height() = tip - final_depth`, so the settled clock trails
    /// the mint clock the votes are cast on by roughly `final_depth`. The
    /// reported `+1010` was therefore ~512 of genuine saving plus ~490 of
    /// pure clock offset, presented as if the whole thing were a speedup:
    /// a 2x overstatement of the one number Phase 2 exists to produce, in
    /// the direction that flatters the feature.
    ///
    /// The fix compares against `measurement_tip`, so the delta collapses to
    /// the real depth-rule distance regardless of how far the settled clock
    /// happens to trail.
    #[test]
    fn depth_rule_is_measured_on_the_same_clock_as_the_certificate() {
        let k = key(1);
        let committee = sigil_braidpool::committee::Committee::new(vec![(
            k.verifying_key().to_bytes(),
            k.verifying_key().to_bytes(),
        )]);
        let cfg = ObserverConfig { checkpoint_interval: 32, ..Default::default() };
        let mut w = FinalityWire {
            observer: FinalityObserver::new(committee, cfg),
            signing_key: Some(k),
            announced: true,
            last_vote_height: 0,
            last_cert_log_ms: 0,
            guard: VoteGuard::default(),
            vote_depth: 0,
        };

        // The live shape: a certificate at the mint height, while the caller
        // can only offer a settled height ~final_depth behind it.
        const MINT: u64 = 314_528;
        const SETTLED: u64 = 314_041; // 487 behind, as measured live
        w.on_block(MINT, [1u8; 32], Some([2u8; 32]), 0).expect("n=1 committee certifies on its own vote");

        let r = w.report(SETTLED).expect("finality is enabled");
        assert_eq!(r.finalized_height, Some(MINT));
        // Measured on the mint clock, the depth rule sits exactly
        // DEPTH_RULE_BLOCKS below the certificate — no clock offset smuggled in.
        assert_eq!(r.depth_rule_height, MINT - sigil_finality::observer::DEPTH_RULE_BLOCKS);
        assert_eq!(
            r.blocks_ahead_of_depth_rule(),
            sigil_finality::observer::DEPTH_RULE_BLOCKS as i64,
            "the saving is the depth rule itself, not the depth rule plus the settled-clock lag"
        );

        // And the pre-fix behaviour, pinned so the regression is unmistakable:
        // measuring against the settled clock inflates the same saving by the
        // full 487-block offset. 314_528 - (314_041 - 512) = 999, which is the
        // magnitude the live heartbeats were printing (they ranged 993-1023 as
        // the settled clock drifted between prints; the exact figure moves with
        // the offset, the ~2x inflation does not).
        let inflated = MINT as i64 - (SETTLED - sigil_finality::observer::DEPTH_RULE_BLOCKS) as i64;
        assert_eq!(inflated, 999);
        assert_eq!(
            inflated - r.blocks_ahead_of_depth_rule(),
            (MINT - SETTLED) as i64,
            "the entire overstatement is exactly the mint-vs-settled clock offset"
        );
        assert!(
            inflated > r.blocks_ahead_of_depth_rule() * 19 / 10,
            "the old number was nearly double the real saving: {inflated} vs {}",
            r.blocks_ahead_of_depth_rule()
        );
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
            last_vote_height: 0,
            last_cert_log_ms: 0,
            guard: VoteGuard::default(),
            vote_depth: 0,
        };
        let bytes = w.on_block(320, [7u8; 32], None, 0).unwrap();
        let v = decode_vote(&bytes).unwrap();
        assert_eq!(v.order_hash, [7u8; 32], "with no braid commitment the spine hash is the fallback");
    }
    fn committee_of(ks: &[SigningKey]) -> sigil_braidpool::committee::Committee {
        let members: Vec<([u8; 32], [u8; 32])> =
            ks.iter().map(|k| (k.verifying_key().to_bytes(), k.verifying_key().to_bytes())).collect();
        sigil_braidpool::committee::Committee::new(members)
    }

    fn spine_wire(ks: &[SigningKey], guard: VoteGuard) -> FinalityWire {
        FinalityWire {
            observer: FinalityObserver::new(committee_of(ks), ObserverConfig { checkpoint_interval: 1, ..Default::default() }),
            signing_key: Some(ks[0].clone()),
            announced: true,
            last_vote_height: 0,
            last_cert_log_ms: 0,
            guard,
            vote_depth: 8,
        }
    }

    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("sigil-vote-guard-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 2026-10-08: the measured split came from one validator signing two different
    /// blocks at one height. The guard signs a height once, and never below.
    #[test]
    fn the_signer_guard_signs_each_height_at_most_once_and_never_lower() {
        let ks: Vec<SigningKey> = (1..5u8).map(key).collect();
        let mut w = spine_wire(&ks, VoteGuard::default());
        assert!(w.on_spine_vote(40, [0xA1; 32], None, 0).is_some());
        assert!(w.on_spine_vote(40, [0xB2; 32], None, 0).is_none(), "a second block at the same height: never");
        assert!(w.on_spine_vote(40, [0xA1; 32], None, 0).is_none(), "not even the same block twice");
        assert!(w.on_spine_vote(39, [0xC3; 32], None, 0).is_none(), "never below the last signature");
        assert!(w.on_spine_vote(41, [0xD4; 32], None, 0).is_some());
        assert_eq!(w.last_signed(), Some((41, [0xD4; 32])));
    }

    /// The guard survives a restart: the record is on disk before the vote leaves.
    #[test]
    fn the_signer_guard_survives_a_restart() {
        let ks: Vec<SigningKey> = (1..5u8).map(key).collect();
        let dir = scratch_dir("restart");
        let mut w = spine_wire(&ks, VoteGuard::load(&dir));
        assert!(w.on_spine_vote(100, [0xA1; 32], None, 0).is_some());
        drop(w);
        let mut again = spine_wire(&ks, VoteGuard::load(&dir));
        assert_eq!(again.last_signed(), Some((100, [0xA1; 32])));
        assert!(again.on_spine_vote(100, [0xB2; 32], None, 0).is_none(), "restart must not reopen H=100");
        assert!(again.on_spine_vote(101, [0xB2; 32], None, 0).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An unreadable guard file is "I do not know what I signed": sign nothing.
    #[test]
    fn an_unreadable_signer_guard_signs_nothing() {
        let ks: Vec<SigningKey> = (1..5u8).map(key).collect();
        let dir = scratch_dir("corrupt");
        std::fs::write(dir.join(VoteGuard::FILE), b"{not json").unwrap();
        let mut w = spine_wire(&ks, VoteGuard::load(&dir));
        assert!(w.on_spine_vote(1, [0xA1; 32], None, 0).is_none());
        assert!(w.on_spine_vote(1_000_000, [0xA1; 32], None, 0).is_none());
        let _ = std::fs::remove_dir_all(&dir);
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
