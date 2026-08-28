//! `observer` — Phase 2 of **SIGIL True Instant Finality**
//! (`docs/research/SIGIL_INSTANT_FINALITY_v0.tex`, §Phased implementation
//! plan, row 2).
//!
//! Per the design doc's own phased plan, Phase 2 ships exactly this and
//! nothing more:
//!
//! > P2P gossip topic, local vote tallying, "would-be finalized height"
//! > logged alongside today's estimate; `Braid::insert()` untouched.
//! > *Consensus effect: none — purely observational, same posture as the
//! > existing frontier-debug diagnostic.*
//!
//! ## Zero consensus effect — mechanically, not just by intention
//!
//! Nothing in this module can influence consensus, because nothing in it is
//! reachable from a consensus decision:
//!
//!  - It never imports `sigil_dagknight`, never sees a `Braid`, and has no
//!    way to reach `Braid::insert()`.
//!  - It owns no storage handle and performs no I/O of any kind — every
//!    method is a pure state transition on data the caller hands in.
//!  - Its only outputs are a [`ObserverReport`] (numbers to log) and
//!    [`FinalityVote`]s to gossip. Neither is consulted by block
//!    production, validation, or fork choice in this phase.
//!
//! The *caller* (`sigil-node`) is what decides to log the report and
//! publish the votes. If that wiring were deleted tomorrow, the chain would
//! behave identically.
//!
//! ## What this phase is actually FOR
//!
//! One number. Today SIGIL confirms by depth: `FINAL_DEPTH = 512` blocks at
//! the measured ~6.28 blocks/sec, i.e. ~81.5 seconds of waiting for a
//! *probabilistic* guarantee. A finality gadget replaces that with an
//! *absolute* one — but nobody should promise how much faster it is before
//! measuring it on the real network.
//!
//! [`ObserverReport`] is that measurement: for every checkpoint height it
//! records when the height was first seen and when a quorum certificate
//! actually assembled for it, and reports the difference — plus how far
//! ahead of (or behind) the current 512-deep rule the gadget *would* have
//! been. See [`ObserverReport::verdict`].
//!
//! ## Memory is bounded, deliberately
//!
//! Votes arrive from the network, so an unbounded accumulator is a remote
//! memory-exhaustion vector. This module keeps votes in a `BTreeMap` keyed
//! by height and prunes on two independent axes — height retention and a
//! hard vote cap — so a hostile peer flooding votes cannot grow it without
//! limit. See [`ObserverConfig::retention_heights`] and
//! [`ObserverConfig::max_votes`], and the tests that hold them to it.
//!
//! ## Assembly is per-height, and that is exactly equivalent
//!
//! [`crate::assemble`] is documented as a pure function over a whole batch.
//! This module calls it with one height's votes at a time. That is not an
//! approximation: every rule `assemble` applies is already scoped to a
//! single height — membership and signature checks are per-vote, and
//! equivocation is defined per `(height, validator_id)`. Splitting the
//! batch by height therefore produces byte-identical certificates to
//! assembling the whole set at once, while keeping the work bounded.
//! [`tests::per_height_assembly_matches_whole_batch_assembly`] proves this
//! rather than asserting it.

use std::collections::{BTreeMap, HashMap};

use ed25519_dalek::SigningKey;
use sigil_braidpool::committee::Committee;
use sigil_header::BlockHash;

use crate::{assemble, FinalityCertificate, FinalityError, FinalityVote, OrderHash};

/// The depth rule SIGIL confirms by today, and the measured block rate it
/// was derived from. Both are quoted from the live constants rather than
/// re-guessed here: `sigil_dagknight::BraidConfig::final_depth` defaults to
/// 512 (bumped from 64 on 2026-08-15 after `examples/k_probe.rs` found a
/// real deep reorder), and `sigil-api` uses the same 512 alongside its
/// `SLOWEST_MEASURED_BLOCK_RATE_PER_SEC` when it quotes a settlement floor.
///
/// This module never *enforces* the depth rule — it only reports against it
/// so the two can be compared honestly.
pub const DEPTH_RULE_BLOCKS: u64 = 512;

/// Tuning for [`FinalityObserver`]. Defaults are deliberately conservative:
/// an observer built with [`ObserverConfig::default`] and an empty
/// committee does nothing at all.
#[derive(Debug, Clone)]
pub struct ObserverConfig {
    /// Vote on every Nth height. A checkpoint interval trades vote traffic
    /// against finality granularity: smaller means finality advances in
    /// finer steps and costs more gossip.
    ///
    /// Must be >= 1; [`ObserverConfig::sanitized`] clamps a supplied 0 up
    /// to 1 rather than dividing by zero.
    pub checkpoint_interval: u64,
    /// How many heights below the newest certified height to keep votes
    /// for. Votes older than this are dropped — they can no longer change
    /// any decision, and keeping them is how an observer OOMs.
    pub retention_heights: u64,
    /// Hard ceiling on total retained votes, independent of height. This is
    /// the backstop against a hostile peer flooding many distinct heights:
    /// height retention alone does not bound a flood that spans a wide
    /// height range.
    pub max_votes: usize,
}

impl Default for ObserverConfig {
    fn default() -> Self {
        Self {
            // ~32 blocks at the measured ~6.28 blk/s is roughly 5 seconds —
            // fine enough to be a visible improvement over 81.5s, coarse
            // enough that vote traffic stays negligible next to block
            // traffic. Phase 2 exists partly to find out whether this is
            // the right number, so it is configurable, not baked in.
            checkpoint_interval: 32,
            retention_heights: 4096,
            max_votes: 65_536,
        }
    }
}

impl ObserverConfig {
    /// Return a copy with impossible values corrected rather than trusted.
    /// A zero `checkpoint_interval` would panic on the modulo; a zero
    /// `max_votes` would silently discard everything.
    pub fn sanitized(&self) -> Self {
        Self {
            checkpoint_interval: self.checkpoint_interval.max(1),
            retention_heights: self.retention_heights,
            max_votes: self.max_votes.max(1),
        }
    }
}

/// What happened when a vote was handed to [`FinalityObserver::observe`].
///
/// Note that a *rejected* vote is a completely ordinary event on a public
/// gossip topic — anyone can publish anything to it. Rejection is the
/// system working, not an error to escalate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObserveOutcome {
    /// Accepted and tallied; no certificate formed at that height yet.
    Accepted { height: u64, votes_at_height: usize },
    /// Accepted, and this vote completed a quorum — the height is now
    /// (observationally) final.
    Certified { height: u64, latency_ms: Option<u64> },
    /// Already had this exact vote from this validator; ignored.
    Duplicate { height: u64 },
    /// Refused before tallying: bad signature, or not a committee member.
    Rejected { height: u64, reason: FinalityError },
    /// Dropped because the observer is disabled (empty committee) or the
    /// height is already below the retention window.
    Ignored { height: u64 },
}

/// The Phase 2 deliverable: "would-be finalized height" next to today's
/// estimate, plus the latency measurement that justifies the whole project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObserverReport {
    /// Current chain tip as the caller sees it.
    pub tip_height: u64,
    /// Highest height with an assembled quorum certificate, if any.
    pub finalized_height: Option<u64>,
    /// What today's live rule considers settled: `tip - 512`, saturating.
    pub depth_rule_height: u64,
    /// Certificate assembly latency for the most recently certified
    /// checkpoint, in milliseconds — the number Phase 2 exists to produce.
    /// `None` until at least one checkpoint has both a first-seen timestamp
    /// and a certificate.
    pub last_latency_ms: Option<u64>,
    /// Rolling mean of every latency measured so far, in milliseconds.
    pub mean_latency_ms: Option<u64>,
    /// How many checkpoint certificates have assembled since start.
    pub certificates_seen: u64,
    /// Equivocations detected since start. Non-zero is evidence a validator
    /// is misbehaving — it is NOT by itself a safety failure (that is the
    /// whole point of quorum intersection), but it should be loud.
    pub equivocations_seen: u64,
    /// Votes refused since start (bad signature or non-member).
    pub rejected_seen: u64,
    /// Votes currently retained in memory.
    pub retained_votes: usize,
    /// Is the committee large enough for real Byzantine fault tolerance
    /// (`n >= 4`)? A committee below this still assembles certificates, but
    /// they do not mean "protected against a malicious minority."
    pub bft_active: bool,
    /// Committee size.
    pub committee_size: usize,
    /// Quorum required, `n - f`.
    pub quorum: usize,
}

impl ObserverReport {
    /// How many blocks ahead of today's depth rule the gadget would be.
    /// Positive means finality is running *ahead* of the 512-deep rule —
    /// which is the entire point. Negative means it is lagging, and that is
    /// a finding worth investigating, not a number to hide.
    pub fn blocks_ahead_of_depth_rule(&self) -> i64 {
        match self.finalized_height {
            Some(f) => f as i64 - self.depth_rule_height as i64,
            None => 0,
        }
    }

    /// A one-line, human-readable verdict for the log — the literal Phase 2
    /// deliverable, phrased so a reader who has never seen this code can
    /// tell what it means.
    pub fn verdict(&self) -> String {
        if self.committee_size == 0 {
            return "finality: DISABLED (no committee configured)".to_string();
        }
        let bft = if self.bft_active { "BFT" } else { "NO-BFT (n<4)" };
        match self.finalized_height {
            None => format!(
                "finality: observing, no certificate yet · tip={} depth-rule={} committee={} quorum={} {} · retained_votes={}",
                self.tip_height, self.depth_rule_height, self.committee_size, self.quorum, bft, self.retained_votes
            ),
            Some(f) => format!(
                "finality: would-be-final={} vs depth-rule={} ({:+} blocks) · latency last={} mean={} · certs={} equivocations={} rejected={} · committee={} quorum={} {}",
                f,
                self.depth_rule_height,
                self.blocks_ahead_of_depth_rule(),
                self.last_latency_ms.map(|m| format!("{m}ms")).unwrap_or_else(|| "n/a".into()),
                self.mean_latency_ms.map(|m| format!("{m}ms")).unwrap_or_else(|| "n/a".into()),
                self.certificates_seen,
                self.equivocations_seen,
                self.rejected_seen,
                self.committee_size,
                self.quorum,
                bft
            ),
        }
    }
}

/// Per-height bookkeeping. Kept separate from the votes themselves so
/// pruning votes never destroys a timing measurement that has already been
/// taken.
#[derive(Debug, Clone, Default)]
struct HeightTiming {
    first_seen_ms: Option<u64>,
    certified_ms: Option<u64>,
}

/// Observational finality gadget. Accumulates gossiped votes, assembles
/// certificates, and reports what finality *would* have said — without
/// touching consensus.
#[derive(Debug, Clone)]
pub struct FinalityObserver {
    committee: Committee,
    cfg: ObserverConfig,
    /// height -> votes retained for that height.
    votes: BTreeMap<u64, Vec<FinalityVote>>,
    /// height -> certificate, for heights that reached quorum.
    certificates: BTreeMap<u64, FinalityCertificate>,
    timings: HashMap<u64, HeightTiming>,
    /// Per-height count of equivocations already added to the running
    /// total, so re-assembling the same votes never double-counts.
    equivocation_counts: BTreeMap<u64, u64>,
    latencies: Vec<u64>,
    equivocations_seen: u64,
    rejected_seen: u64,
}

impl FinalityObserver {
    /// Build an observer. An empty `committee` yields a permanently inert
    /// observer — the fail-closed default, so that a node with no finality
    /// configuration behaves exactly as it does today.
    pub fn new(committee: Committee, cfg: ObserverConfig) -> Self {
        Self {
            committee,
            cfg: cfg.sanitized(),
            votes: BTreeMap::new(),
            certificates: BTreeMap::new(),
            timings: HashMap::new(),
            equivocation_counts: BTreeMap::new(),
            latencies: Vec::new(),
            equivocations_seen: 0,
            rejected_seen: 0,
        }
    }

    /// Is this observer doing anything at all? False when no committee is
    /// configured.
    pub fn enabled(&self) -> bool {
        !self.committee.is_empty()
    }

    /// Is `height` a checkpoint this observer votes on and tallies?
    pub fn is_checkpoint(&self, height: u64) -> bool {
        height % self.cfg.checkpoint_interval == 0
    }

    /// Highest height with an assembled certificate.
    pub fn finalized_height(&self) -> Option<u64> {
        self.certificates.keys().next_back().copied()
    }

    /// The certificate for a height, if one assembled.
    pub fn certificate_for(&self, height: u64) -> Option<&FinalityCertificate> {
        self.certificates.get(&height)
    }

    /// Measured assembly latency for a height: the gap between first
    /// observing the checkpoint and a quorum certificate forming for it.
    pub fn latency_ms(&self, height: u64) -> Option<u64> {
        let t = self.timings.get(&height)?;
        match (t.first_seen_ms, t.certified_ms) {
            (Some(seen), Some(done)) => Some(done.saturating_sub(seen)),
            _ => None,
        }
    }

    /// Record that a checkpoint height exists and when it was first seen.
    ///
    /// The caller invokes this when a block reaches a checkpoint height —
    /// this is what starts the latency clock, so it must be called at the
    /// moment the height becomes known locally, not when the first vote
    /// arrives. Calling it repeatedly for the same height is safe; only the
    /// first timestamp is kept.
    pub fn note_checkpoint_seen(&mut self, height: u64, now_ms: u64) {
        if !self.enabled() || !self.is_checkpoint(height) {
            return;
        }
        self.timings.entry(height).or_default().first_seen_ms.get_or_insert(now_ms);
    }

    /// Produce this node's own vote for a checkpoint, if it holds a
    /// validator key that is actually in the committee.
    ///
    /// Returns `None` — rather than an unusable vote — when the observer is
    /// disabled, the height is not a checkpoint, or the key is not a
    /// committee member. A non-member signing votes would only generate
    /// traffic that every honest peer correctly rejects.
    pub fn own_vote(
        &self,
        key: &SigningKey,
        height: u64,
        spine_block_hash: BlockHash,
        order_hash: OrderHash,
    ) -> Option<FinalityVote> {
        if !self.enabled() || !self.is_checkpoint(height) {
            return None;
        }
        let id = key.verifying_key().to_bytes();
        if !self.committee.contains(&id) {
            return None;
        }
        Some(FinalityVote::sign(key, height, spine_block_hash, order_hash))
    }

    /// Observe one gossiped vote.
    ///
    /// Verification, membership and equivocation handling are all delegated
    /// to [`crate::assemble`] — this method deliberately does not
    /// re-implement any of that logic, so there is exactly one place in the
    /// codebase where "is this vote legitimate" is decided.
    pub fn observe(&mut self, vote: FinalityVote, now_ms: u64) -> ObserveOutcome {
        let height = vote.height;
        if !self.enabled() || !self.is_checkpoint(height) {
            return ObserveOutcome::Ignored { height };
        }
        // Below the retention floor this vote can no longer change any
        // decision; accepting it would only be a memory cost.
        if let Some(floor) = self.retention_floor() {
            if height < floor {
                return ObserveOutcome::Ignored { height };
            }
        }
        // Cheap membership/signature pre-check so a flood of junk never
        // reaches the retained set at all. This mirrors `assemble`'s own
        // two checks in the same order; `assemble` still re-checks
        // everything it tallies, so this is a filter, never the authority.
        if let Err(reason) = vote.verify() {
            self.rejected_seen += 1;
            return ObserveOutcome::Rejected { height, reason };
        }
        if !self.committee.contains(&vote.validator_id) {
            self.rejected_seen += 1;
            return ObserveOutcome::Rejected { height, reason: FinalityError::NotCommitteeMember };
        }

        let at_height = self.votes.entry(height).or_default();
        if at_height.iter().any(|v| *v == vote) {
            return ObserveOutcome::Duplicate { height };
        }
        at_height.push(vote);
        let votes_at_height = at_height.len();

        self.timings.entry(height).or_default().first_seen_ms.get_or_insert(now_ms);

        let newly_certified = self.try_assemble(height, now_ms);
        self.prune();

        if newly_certified {
            ObserveOutcome::Certified { height, latency_ms: self.latency_ms(height) }
        } else {
            ObserveOutcome::Accepted { height, votes_at_height }
        }
    }

    /// Run assembly for one height. Returns true if a certificate formed
    /// that had not formed before.
    ///
    /// Equivocation counting is done on the delta, not the total, because
    /// `assemble` is a pure function re-run over the same accumulated votes
    /// — it re-reports the same equivocation every time, and naively adding
    /// its length each call would inflate the counter without bound.
    fn try_assemble(&mut self, height: u64, now_ms: u64) -> bool {
        let Some(votes) = self.votes.get(&height) else { return false };
        let report = assemble(&self.committee, votes);

        let already = self.equivocations_at(height);
        let now_count = report.equivocations.len() as u64;
        if now_count > already {
            self.equivocations_seen += now_count - already;
            self.record_equivocation_count(height, now_count);
        }

        let Some(cert) = report.certificate_for_height(height) else { return false };
        if self.certificates.contains_key(&height) {
            return false;
        }
        self.certificates.insert(height, cert.clone());
        let timing = self.timings.entry(height).or_default();
        timing.certified_ms.get_or_insert(now_ms);
        if let Some(l) = self.latency_ms(height) {
            self.latencies.push(l);
        }
        true
    }

    /// Per-height equivocation counts, so the running total counts each
    /// distinct equivocation exactly once across repeated assemblies.
    fn equivocations_at(&self, height: u64) -> u64 {
        self.equivocation_counts.get(&height).copied().unwrap_or(0)
    }

    fn record_equivocation_count(&mut self, height: u64, count: u64) {
        self.equivocation_counts.insert(height, count);
    }

    /// Lowest height still worth retaining votes for.
    fn retention_floor(&self) -> Option<u64> {
        self.finalized_height().map(|f| f.saturating_sub(self.cfg.retention_heights))
    }

    /// Bound memory on both axes. Height retention handles the normal case;
    /// the hard cap handles a hostile peer spreading votes across a wide
    /// height range, which retention alone does not bound.
    fn prune(&mut self) {
        if let Some(floor) = self.retention_floor() {
            self.votes.retain(|h, _| *h >= floor);
            self.certificates.retain(|h, _| *h >= floor);
            self.timings.retain(|h, _| *h >= floor);
            self.equivocation_counts.retain(|h, _| *h >= floor);
        }
        // Oldest-first eviction until under the cap. `BTreeMap` iterates in
        // key order, so this drops the least relevant heights first.
        while self.retained_votes() > self.cfg.max_votes {
            let Some(oldest) = self.votes.keys().next().copied() else { break };
            self.votes.remove(&oldest);
        }
    }

    /// Total votes currently held in memory.
    pub fn retained_votes(&self) -> usize {
        self.votes.values().map(|v| v.len()).sum()
    }

    /// Build the Phase 2 report for logging.
    pub fn report(&self, tip_height: u64) -> ObserverReport {
        let finalized_height = self.finalized_height();
        let mean_latency_ms = if self.latencies.is_empty() {
            None
        } else {
            let sum: u128 = self.latencies.iter().map(|l| *l as u128).sum();
            Some((sum / self.latencies.len() as u128) as u64)
        };
        ObserverReport {
            tip_height,
            finalized_height,
            depth_rule_height: tip_height.saturating_sub(DEPTH_RULE_BLOCKS),
            last_latency_ms: finalized_height.and_then(|h| self.latency_ms(h)),
            mean_latency_ms,
            certificates_seen: self.certificates.len() as u64,
            equivocations_seen: self.equivocations_seen,
            rejected_seen: self.rejected_seen,
            retained_votes: self.retained_votes(),
            bft_active: self.committee.bft_active(),
            committee_size: self.committee.len(),
            quorum: self.committee.availability_quorum(),
        }
    }
}

/// Build a committee and an optional signing key from environment, the same
/// "every node agrees out-of-band" precedent `SIGIL_TRUSTED_PRODUCER_ID_HEX`
/// already sets in this codebase.
///
/// * `SIGIL_FINALITY_COMMITTEE` — comma-separated 64-char hex Ed25519
///   public keys. Unset or empty means the observer is disabled, which is
///   the correct default: a node with no finality configuration must behave
///   exactly as it does today.
/// * `SIGIL_FINALITY_SIGN_SEED` — 64-char hex Ed25519 seed for THIS node's
///   validator identity. Optional: a node with a committee but no seed is a
///   pure observer, which is the majority case and a useful one.
/// * `SIGIL_FINALITY_CHECKPOINT_INTERVAL` — override the checkpoint stride.
///
/// Every parse failure is fail-closed and *named*: a malformed key disables
/// the gadget rather than silently shrinking the committee, because a
/// committee that is quietly smaller than the operator believes has a
/// quietly smaller quorum, which is a safety-relevant surprise.
pub mod env_config {
    use super::*;

    /// Why the environment did not produce a usable committee.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum ConfigError {
        /// No `SIGIL_FINALITY_COMMITTEE` set — not an error, just off.
        Disabled,
        /// A committee entry was not 64 hex characters.
        BadCommitteeKey(String),
        /// The signing seed was not 64 hex characters.
        BadSeed,
        /// The seed's public key is not in the committee — a non-member
        /// signing votes only generates traffic honest peers reject.
        SeedNotInCommittee,
    }

    fn hex32(s: &str) -> Option<[u8; 32]> {
        let s = s.trim();
        if s.len() != 64 {
            return None;
        }
        let mut out = [0u8; 32];
        for i in 0..32 {
            out[i] = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
        }
        Some(out)
    }

    /// Parse a committee from a comma-separated hex list.
    pub fn parse_committee(raw: &str) -> Result<Committee, ConfigError> {
        let entries: Vec<&str> = raw.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
        if entries.is_empty() {
            return Err(ConfigError::Disabled);
        }
        let mut members = Vec::with_capacity(entries.len());
        for e in entries {
            let pk = hex32(e).ok_or_else(|| ConfigError::BadCommitteeKey(e.to_string()))?;
            // The committee's wallet id and its signing pubkey are the same
            // 32 bytes in this phase — `FinalityVote::validator_id` IS the
            // Ed25519 public key, and `assemble` checks membership against
            // exactly that. Keying them differently here would make every
            // honest vote fail the membership check.
            members.push((pk, pk));
        }
        Ok(Committee::new(members))
    }

    /// Parse this node's validator signing key and confirm it is a member.
    pub fn parse_seed(raw: &str, committee: &Committee) -> Result<SigningKey, ConfigError> {
        let seed = hex32(raw).ok_or(ConfigError::BadSeed)?;
        let key = SigningKey::from_bytes(&seed);
        if !committee.contains(&key.verifying_key().to_bytes()) {
            return Err(ConfigError::SeedNotInCommittee);
        }
        Ok(key)
    }

    /// Read `SIGIL_FINALITY_CHECKPOINT_INTERVAL`, falling back to the
    /// default when unset or unparseable.
    pub fn checkpoint_interval_from_env() -> u64 {
        std::env::var("SIGIL_FINALITY_CHECKPOINT_INTERVAL")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|v| *v >= 1)
            .unwrap_or_else(|| ObserverConfig::default().checkpoint_interval)
    }

    /// The whole thing: committee + optional key + config, straight from
    /// the process environment.
    pub fn from_env() -> Result<(Committee, Option<SigningKey>, ObserverConfig), ConfigError> {
        let raw = std::env::var("SIGIL_FINALITY_COMMITTEE").unwrap_or_default();
        let committee = parse_committee(&raw)?;
        let key = match std::env::var("SIGIL_FINALITY_SIGN_SEED") {
            Ok(s) if !s.trim().is_empty() => Some(parse_seed(&s, &committee)?),
            _ => None,
        };
        let cfg = ObserverConfig { checkpoint_interval: checkpoint_interval_from_env(), ..Default::default() };
        Ok((committee, key, cfg))
    }
}

#[cfg(test)]
mod tests {
    use super::env_config::*;
    use super::*;

    fn keys(n: usize) -> Vec<SigningKey> {
        (0..n)
            .map(|i| {
                let mut seed = [0u8; 32];
                seed[0] = (i as u8) + 1;
                SigningKey::from_bytes(&seed)
            })
            .collect()
    }

    fn committee_of(ks: &[SigningKey]) -> Committee {
        Committee::new(ks.iter().map(|k| (k.verifying_key().to_bytes(), k.verifying_key().to_bytes())).collect())
    }

    fn obs(n: usize, interval: u64) -> (FinalityObserver, Vec<SigningKey>) {
        let ks = keys(n);
        let c = committee_of(&ks);
        let cfg = ObserverConfig { checkpoint_interval: interval, ..Default::default() };
        (FinalityObserver::new(c, cfg), ks)
    }

    const H: u64 = 320;
    const SPINE: BlockHash = [7u8; 32];
    const ORDER: OrderHash = [9u8; 32];

    #[test]
    fn empty_committee_is_inert_so_an_unconfigured_node_behaves_exactly_as_today() {
        let mut o = FinalityObserver::new(Committee::default(), ObserverConfig::default());
        assert!(!o.enabled());
        let k = &keys(1)[0];
        let v = FinalityVote::sign(k, H, SPINE, ORDER);
        assert_eq!(o.observe(v, 0), ObserveOutcome::Ignored { height: H });
        assert_eq!(o.finalized_height(), None);
        assert!(o.report(100_000).verdict().contains("DISABLED"));
    }

    #[test]
    fn quorum_of_four_certifies_at_the_third_vote_not_the_second() {
        let (mut o, ks) = obs(4, 32);
        assert_eq!(o.report(0).quorum, 3, "n=4 must need 3; f=1");
        o.note_checkpoint_seen(H, 1_000);
        for (i, k) in ks.iter().take(2).enumerate() {
            let out = o.observe(FinalityVote::sign(k, H, SPINE, ORDER), 1_100 + i as u64);
            assert!(matches!(out, ObserveOutcome::Accepted { .. }), "vote {i} should not certify yet: {out:?}");
        }
        assert_eq!(o.finalized_height(), None);
        let out = o.observe(FinalityVote::sign(&ks[2], H, SPINE, ORDER), 1_450);
        assert!(matches!(out, ObserveOutcome::Certified { .. }), "third vote must certify, got {out:?}");
        assert_eq!(o.finalized_height(), Some(H));
    }

    #[test]
    fn latency_is_measured_from_first_seen_to_certificate() {
        let (mut o, ks) = obs(4, 32);
        o.note_checkpoint_seen(H, 10_000);
        o.observe(FinalityVote::sign(&ks[0], H, SPINE, ORDER), 10_100);
        o.observe(FinalityVote::sign(&ks[1], H, SPINE, ORDER), 10_200);
        o.observe(FinalityVote::sign(&ks[2], H, SPINE, ORDER), 10_750);
        assert_eq!(o.latency_ms(H), Some(750), "latency must span first-seen -> certificate");
        let r = o.report(H + 600);
        assert_eq!(r.last_latency_ms, Some(750));
        assert_eq!(r.mean_latency_ms, Some(750));
    }

    #[test]
    fn would_be_final_is_reported_against_the_512_depth_rule() {
        let (mut o, ks) = obs(4, 32);
        o.note_checkpoint_seen(H, 0);
        for k in ks.iter().take(3) {
            o.observe(FinalityVote::sign(k, H, SPINE, ORDER), 100);
        }
        // Tip only 8 blocks past the checkpoint: today's rule has settled
        // nothing at all here, the gadget has settled H.
        let r = o.report(H + 8);
        assert_eq!(r.finalized_height, Some(H));
        assert_eq!(r.depth_rule_height, 0, "tip-512 saturates to 0 this early");
        assert_eq!(r.blocks_ahead_of_depth_rule(), H as i64);
        assert!(r.verdict().contains("would-be-final=320"), "verdict: {}", r.verdict());
    }

    #[test]
    fn non_member_and_tampered_votes_are_rejected_not_tallied() {
        let (mut o, ks) = obs(4, 32);
        let outsider = SigningKey::from_bytes(&[99u8; 32]);
        let out = o.observe(FinalityVote::sign(&outsider, H, SPINE, ORDER), 0);
        assert!(matches!(out, ObserveOutcome::Rejected { reason: FinalityError::NotCommitteeMember, .. }));

        let mut tampered = FinalityVote::sign(&ks[0], H, SPINE, ORDER);
        tampered.order_hash = [1u8; 32];
        let out = o.observe(tampered, 0);
        assert!(matches!(out, ObserveOutcome::Rejected { reason: FinalityError::BadSignature, .. }));

        assert_eq!(o.retained_votes(), 0, "rejected votes must never be retained");
        assert_eq!(o.report(0).rejected_seen, 2);
    }

    #[test]
    fn a_duplicate_vote_is_not_counted_twice_toward_quorum() {
        let (mut o, ks) = obs(4, 32);
        let v = FinalityVote::sign(&ks[0], H, SPINE, ORDER);
        o.observe(v.clone(), 0);
        assert_eq!(o.observe(v.clone(), 1), ObserveOutcome::Duplicate { height: H });
        o.observe(FinalityVote::sign(&ks[1], H, SPINE, ORDER), 2);
        // Two distinct validators only; quorum is 3, so still no cert even
        // though three votes were submitted.
        assert_eq!(o.finalized_height(), None);
        assert_eq!(o.retained_votes(), 2);
    }

    #[test]
    fn equivocation_is_counted_once_not_once_per_reassembly() {
        let (mut o, ks) = obs(4, 32);
        o.observe(FinalityVote::sign(&ks[0], H, SPINE, ORDER), 0);
        o.observe(FinalityVote::sign(&ks[0], H, [8u8; 32], ORDER), 1);
        assert_eq!(o.report(0).equivocations_seen, 1);
        // Every further vote re-runs assembly over the same equivocating
        // pair; the counter must not climb.
        o.observe(FinalityVote::sign(&ks[1], H, SPINE, ORDER), 2);
        o.observe(FinalityVote::sign(&ks[2], H, SPINE, ORDER), 3);
        assert_eq!(o.report(0).equivocations_seen, 1, "re-assembly must not inflate the equivocation count");
    }

    #[test]
    fn an_equivocator_cannot_help_form_a_quorum() {
        let (mut o, ks) = obs(4, 32);
        // ks[0] equivocates, ks[1] and ks[2] vote honestly: 2 honest
        // validators, quorum 3 — must NOT certify.
        o.observe(FinalityVote::sign(&ks[0], H, SPINE, ORDER), 0);
        o.observe(FinalityVote::sign(&ks[0], H, [8u8; 32], ORDER), 1);
        o.observe(FinalityVote::sign(&ks[1], H, SPINE, ORDER), 2);
        o.observe(FinalityVote::sign(&ks[2], H, SPINE, ORDER), 3);
        assert_eq!(o.finalized_height(), None, "an equivocator's signature must not count toward any tally");
    }

    #[test]
    fn per_height_assembly_matches_whole_batch_assembly() {
        // The load-bearing claim in this module's docs: splitting the batch
        // by height gives identical certificates to one whole-batch call.
        let ks = keys(4);
        let c = committee_of(&ks);
        let heights = [32u64, 64, 96];
        let mut all: Vec<FinalityVote> = Vec::new();
        for h in heights {
            for k in ks.iter().take(3) {
                all.push(FinalityVote::sign(k, h, SPINE, ORDER));
            }
        }
        let whole = assemble(&c, &all);
        let mut whole_heights: Vec<u64> = whole.certificates.iter().map(|x| x.height).collect();
        whole_heights.sort_unstable();

        let cfg = ObserverConfig { checkpoint_interval: 32, ..Default::default() };
        let mut o = FinalityObserver::new(c, cfg);
        for v in &all {
            o.observe(v.clone(), 0);
        }
        let mut split_heights: Vec<u64> = o.certificates.keys().copied().collect();
        split_heights.sort_unstable();

        assert_eq!(whole_heights, split_heights);
        assert_eq!(split_heights, heights.to_vec());
        for h in heights {
            assert_eq!(
                whole.certificate_for_height(h).map(|x| (x.spine_block_hash, x.order_hash)),
                o.certificate_for(h).map(|x| (x.spine_block_hash, x.order_hash)),
            );
        }
    }

    #[test]
    fn a_vote_flood_across_many_heights_cannot_grow_memory_without_bound() {
        let ks = keys(4);
        let c = committee_of(&ks);
        let cfg = ObserverConfig { checkpoint_interval: 1, retention_heights: 16, max_votes: 40 };
        let mut o = FinalityObserver::new(c, cfg);
        // 2000 heights x 3 validators = 6000 votes offered.
        for h in 1..=2000u64 {
            for k in ks.iter().take(3) {
                o.observe(FinalityVote::sign(k, h, SPINE, ORDER), h);
            }
        }
        assert!(o.retained_votes() <= 40, "hard cap breached: {} retained", o.retained_votes());
        assert!(o.finalized_height().is_some(), "pruning must not stop finality advancing");
    }

    #[test]
    fn non_checkpoint_heights_are_ignored_entirely() {
        let (mut o, ks) = obs(4, 32);
        assert!(!o.is_checkpoint(333));
        assert_eq!(o.observe(FinalityVote::sign(&ks[0], 333, SPINE, ORDER), 0), ObserveOutcome::Ignored { height: 333 });
        assert_eq!(o.retained_votes(), 0);
    }

    #[test]
    fn own_vote_is_only_produced_for_a_real_member_at_a_real_checkpoint() {
        let (o, ks) = obs(4, 32);
        assert!(o.own_vote(&ks[0], H, SPINE, ORDER).is_some());
        assert!(o.own_vote(&ks[0], H + 1, SPINE, ORDER).is_none(), "not a checkpoint height");
        let outsider = SigningKey::from_bytes(&[123u8; 32]);
        assert!(o.own_vote(&outsider, H, SPINE, ORDER).is_none(), "non-members must not sign votes");
    }

    #[test]
    fn zero_checkpoint_interval_is_clamped_instead_of_panicking() {
        let (mut o, ks) = obs(4, 0);
        assert!(o.is_checkpoint(7), "interval 0 must clamp to 1, making every height a checkpoint");
        assert!(matches!(o.observe(FinalityVote::sign(&ks[0], 7, SPINE, ORDER), 0), ObserveOutcome::Accepted { .. }));
    }

    #[test]
    fn env_committee_parses_and_rejects_malformed_keys_loudly() {
        let ks = keys(4);
        let hexes: Vec<String> = ks.iter().map(|k| hex::encode(k.verifying_key().to_bytes())).collect();
        let c = parse_committee(&hexes.join(",")).expect("valid committee");
        assert_eq!(c.len(), 4);
        assert!(c.contains(&ks[0].verifying_key().to_bytes()));

        assert_eq!(parse_committee("").unwrap_err(), ConfigError::Disabled);
        let bad = format!("{},nothex", hexes[0]);
        assert!(matches!(parse_committee(&bad), Err(ConfigError::BadCommitteeKey(_))),
            "a malformed key must disable the gadget, never silently shrink the committee");
    }

    #[test]
    fn env_seed_must_belong_to_the_committee() {
        let ks = keys(4);
        let hexes: Vec<String> = ks.iter().map(|k| hex::encode(k.verifying_key().to_bytes())).collect();
        let c = parse_committee(&hexes.join(",")).unwrap();
        // Use ks[0]'s ACTUAL seed bytes — `keys()` builds [i+1, 0, 0, ...],
        // not a byte-repeated pattern.
        let seed0 = hex::encode(ks[0].to_bytes());
        assert!(parse_seed(&seed0, &c).is_ok(), "ks[0]'s own seed must resolve to a committee member");
        assert_eq!(parse_seed(&hex::encode([200u8; 32]), &c).unwrap_err(), ConfigError::SeedNotInCommittee);
        assert_eq!(parse_seed("short", &c).unwrap_err(), ConfigError::BadSeed);
    }
}
