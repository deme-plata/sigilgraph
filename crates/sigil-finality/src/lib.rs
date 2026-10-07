//! `sigil-finality` — Phase 1 of **SIGIL True Instant Finality**
//! (`docs/research/SIGIL_INSTANT_FINALITY_v0.tex`, §Message format & wire
//! protocol, §Safety and liveness).
//!
//! Per the design doc's own phased plan, Phase 1 ships exactly three things
//! and nothing else:
//!   1. `FinalityVote` / `FinalityCertificate` wire types (§Message format).
//!   2. Ed25519 sign/verify for votes — the SAME scheme and code style as
//!      the existing `sigil-node::producer_signing` module (reused
//!      deliberately, not reinvented: same `ed25519-dalek` crate, same
//!      "fail-safe to none rather than panic on malformed input" posture).
//!   3. Quorum/certificate assembly, reusing `sigil_braidpool::committee`'s
//!      already-implemented, already-tested `bft_active` / `max_byzantine` /
//!      `availability_quorum` formulas — NOT a second reimplementation of
//!      the `n = 3f+1` math.
//!
//! ## Zero consensus effect
//!
//! This crate is pure library code. Nothing here touches `Braid::insert()`,
//! gossipsub, storage, or any other live path. It is not imported by
//! `sigil-node` or `sigil-top`. Wiring a finality gadget into the actual
//! chain is Phase 3 of the design doc — a much larger, height-gated,
//! opt-in-flag, isolated-testnet-first effort — and is explicitly OUT OF
//! SCOPE here.
//!
//! ## The one-sentence safety proof this crate makes computationally real
//!
//! From the design doc §Safety and liveness: if two *different* tuples both
//! got finalized at the same height, each needs its own quorum of `2f+1`
//! signatures out of `n = 3f+1` total. Two such quorums must overlap in at
//! least `(2f+1)+(2f+1)-(3f+1) = f+1` validators — meaning at least one
//! *honest* validator (since at most `f` are dishonest) would have had to
//! sign both conflicting tuples, which an honest validator never does by
//! construction. [`tests::adversarial_safety_holds_at_or_below_f`] turns
//! this from an assertion into a passing, randomized, adversarial test:
//! thousands of seeded trials with up to `f` Byzantine validators behaving
//! arbitrarily (rogue votes, equivocation, abstention, any mix) never
//! produce two conflicting certificates for one height.
//!
//! **Caveat on "by construction" (2026-10-07).** The premise is a property
//! of the SIGNER, and today's `sigil-node` does not have it: a keyed
//! validator calls `own_vote` for every block it accepts into its braid,
//! with no "already voted at this height" guard. With one producer that
//! is one vote per height. With two producers it is two different tuples
//! at one height from an HONEST validator. That is what froze the
//! certificates on the 4-node rehearsal chain on 2026-10-07 (2,508
//! equivocations), and it also removes the honest validator that the
//! overlap argument above relies on. The fix belongs in the signer: vote
//! at most once per height, and persist that record before publishing.
//!
//! **What equivocator exclusion buys, and where it stops (corrected
//! 2026-10-07 after an external review).** `assemble()` excludes an
//! equivocator's votes from EVERY tally, and `2*quorum(n) > n` for every
//! committee size (checked for `n` up to 64 in
//! [`tests::one_vote_set_never_certifies_two_conflicting_tuples`]). Together
//! these mean that **within one call to `assemble()`**, i.e. within one
//! node's view of the votes, two conflicting certificates cannot form at any
//! dishonest count. That is a property of a single vote set, NOT of the
//! network, and an earlier version of this comment overclaimed it as one.
//!
//! Different nodes assemble from the votes THEY have received, and a node
//! only excludes an equivocator once it has seen both of its votes. With
//! more than `f` Byzantine validators that is enough to break safety across
//! nodes. At `n = 4, quorum = 3`: Byzantine `X` and `Y` sign both A and B,
//! honest `H1` signs A, honest `H2` signs B. A node that receives
//! `{X, Y, H1}` certifies A, a node that receives `{X, Y, H2}` certifies B,
//! and neither has seen an equivocation
//! ([`tests::distributed_views_can_certify_conflicting_tuples_beyond_f`]).
//! The guarantee across nodes is therefore the classical one: safe while at
//! most `f` validators are Byzantine, because then two quorums overlap in an
//! honest validator. The smallest Byzantine count at which two views can
//! conflict is exactly `2*quorum(n) - n`, which is greater than `f` for every
//! committee size tested
//! ([`tests::conflicting_views_need_more_than_f_byzantine`]).
//!
//! A fully dishonest committee that unanimously signs the SAME false claim
//! also certifies that one wrong tuple
//! ([`tests::fully_dishonest_committee_can_certify_a_wrong_tuple`]). No
//! signature scheme can detect coordinated unanimous lying, which is exactly
//! why the design doc's §Validator set insists the operator directly
//! control every key.

use std::collections::{HashMap, HashSet};

use ed25519_dalek::{Signer, Signature, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

/// Phase 2 — the observational finality gadget: gossip-fed vote tallying,
/// per-height certificate assembly, and the latency measurement that says
/// how much faster a certificate arrives than today's 512-block depth rule.
///
/// Declared here rather than left as an untracked file (which is how it sat
/// from 2026-08-28 until now, compiled into nothing): the module is pure
/// state-transition code with no I/O and no consensus reachability, so
/// declaring it changes what is *built*, never what the chain *does*.
pub mod observer;

use sigil_braidpool::committee::{availability_quorum, bft_active, max_byzantine, Committee};
use sigil_header::{BlockHash, ValidatorId};

/// Braid's existing `frozen_acc` order commitment — a 32-byte content
/// address of "the order DagKnight selected up to this height." Phase 1
/// treats it as an opaque, already-computed input; how it gets computed is
/// entirely a `sigil-dagknight` concern this crate does not touch.
pub type OrderHash = [u8; 32];

/// Domain-separation tag for `FinalityVote` signing bytes. Prevents a signed
/// vote from ever being confused with, or replayed as, any other signed
/// message shape already live in this codebase (block headers, hybrid
/// checkpoints, etc.) — same discipline `producer_signing.rs` documents for
/// its own `signing_bytes()` canonicalization.
const VOTE_DOMAIN_TAG: &[u8] = b"SIGIL_FINALITY_VOTE_V0";

/// A single validator's signed vote for "I agree this specific point in
/// DagKnight's order is correct" — the design doc's §Message format struct,
/// verbatim field-for-field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinalityVote {
    /// The checkpoint height being voted on.
    pub height: u64,
    /// DagKnight's own selected spine block hash at that height.
    pub spine_block_hash: BlockHash,
    /// Braid's `frozen_acc` order commitment at that height.
    pub order_hash: OrderHash,
    /// The voting validator's identity — its Ed25519 public key, same
    /// convention as `sigil-header::ValidatorId` for `Ed25519Hot` producers.
    pub validator_id: ValidatorId,
    /// Raw Ed25519 signature (64 bytes) over [`FinalityVote::signing_bytes`].
    /// `Vec<u8>`, not `[u8; 64]` — matches `sigil_header::SignatureBytes`'s
    /// own convention: `serde`'s derive only implements (de)serialize for
    /// fixed arrays up to 32 elements without an extra big-array dependency.
    pub signature: Vec<u8>,
}

impl FinalityVote {
    /// Canonical signing bytes for a vote over `(height, spine_block_hash,
    /// order_hash)` — domain-tagged so it can never collide with any other
    /// signed message shape in this codebase.
    pub fn signing_bytes(height: u64, spine_block_hash: &BlockHash, order_hash: &OrderHash) -> Vec<u8> {
        let mut buf = Vec::with_capacity(VOTE_DOMAIN_TAG.len() + 8 + 32 + 32);
        buf.extend_from_slice(VOTE_DOMAIN_TAG);
        buf.extend_from_slice(&height.to_le_bytes());
        buf.extend_from_slice(spine_block_hash);
        buf.extend_from_slice(order_hash);
        buf
    }

    /// The `(height, spine_block_hash, order_hash)` identity a certificate
    /// is keyed by — two votes with the same key are "the same claim";
    /// different keys at the same height are conflicting claims.
    pub fn key(&self) -> (u64, BlockHash, OrderHash) {
        (self.height, self.spine_block_hash, self.order_hash)
    }

    /// Sign a fresh vote with `key` — real Ed25519 sign, same crate/API as
    /// `producer_signing::maybe_sign`.
    pub fn sign(key: &SigningKey, height: u64, spine_block_hash: BlockHash, order_hash: OrderHash) -> Self {
        let validator_id: ValidatorId = key.verifying_key().to_bytes();
        let msg = Self::signing_bytes(height, &spine_block_hash, &order_hash);
        let sig = key.sign(&msg);
        FinalityVote { height, spine_block_hash, order_hash, validator_id, signature: sig.to_bytes().to_vec() }
    }

    /// Real Ed25519 verify — same "well-formed input required, fails
    /// closed, never panics" posture as
    /// `producer_signing::verify_self_mined_hybrid`. A wrong-length
    /// signature is a graceful `BadSignature`, not an indexing panic.
    pub fn verify(&self) -> Result<(), FinalityError> {
        let vk = VerifyingKey::from_bytes(&self.validator_id).map_err(|_| FinalityError::BadValidatorKey)?;
        let msg = Self::signing_bytes(self.height, &self.spine_block_hash, &self.order_hash);
        let sig_bytes: [u8; 64] = self.signature.as_slice().try_into().map_err(|_| FinalityError::BadSignature)?;
        let sig = Signature::from_bytes(&sig_bytes);
        vk.verify(&msg, &sig).map_err(|_| FinalityError::BadSignature)
    }
}

/// Everything that can go wrong observing a single vote, or the wider
/// assembly process. Never panics on malformed/adversarial input — fails
/// closed with a named reason, same posture as `producer_signing.rs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinalityError {
    /// `validator_id` is not a valid Ed25519 public key encoding.
    BadValidatorKey,
    /// Signature does not verify against `signing_bytes()`.
    BadSignature,
    /// `validator_id` is not a member of the committee this assembly runs
    /// against.
    NotCommitteeMember,
}

/// A set of votes that agree on one `(height, spine_block_hash, order_hash)`
/// and together reach the committee's quorum size — the design doc's
/// definition of a certificate, verbatim: "simply the set of votes agreeing
/// on the same tuple once that set reaches quorum size." Once assembled,
/// this height (and everything before it) is meant to be permanently
/// locked — Phase 1 only assembles the value; actually treating it as
/// irreversible on-chain is Phase 3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalityCertificate {
    /// The finalized checkpoint height.
    pub height: u64,
    /// The finalized spine block hash.
    pub spine_block_hash: BlockHash,
    /// The finalized order commitment.
    pub order_hash: OrderHash,
    /// The agreeing votes — one per distinct validator, size >= the
    /// committee's `availability_quorum`. May exceed quorum size if more
    /// than the minimum number of honest validators agreed.
    pub votes: Vec<FinalityVote>,
}

impl FinalityCertificate {
    /// The `(height, spine_block_hash, order_hash)` identity this
    /// certificate finalizes.
    pub fn key(&self) -> (u64, BlockHash, OrderHash) {
        (self.height, self.spine_block_hash, self.order_hash)
    }
}

/// Real, on-the-wire evidence that `validator_id` signed two *different*
/// tuples at the same height — cheap, strong proof of dishonest behavior a
/// future slashing/eviction mechanism could consume directly (design doc
/// §Safety and liveness: "Both signed votes are visible on the wire").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Equivocation {
    /// The equivocating validator.
    pub validator_id: ValidatorId,
    /// The height at which it equivocated.
    pub height: u64,
    /// First distinct-tuple vote observed.
    pub vote_a: FinalityVote,
    /// Second distinct-tuple vote observed (proves the conflict).
    pub vote_b: FinalityVote,
}

/// The full result of running [`assemble`] over a batch of votes.
#[derive(Debug, Clone, Default)]
pub struct AssemblyReport {
    /// Every certificate that reached quorum. Under the `<=f` Byzantine
    /// assumption this contains AT MOST one entry per height (see
    /// [`AssemblyReport::conflicting_heights`]); if that assumption is
    /// broken, it can legitimately contain more than one — that is
    /// precisely the failure mode the safety property rules out.
    pub certificates: Vec<FinalityCertificate>,
    /// Equivocations detected while assembling (evidence, not itself a
    /// safety failure — the whole point of quorum intersection is that
    /// equivocation alone, bounded by `<=f`, can never certify a conflict).
    pub equivocations: Vec<Equivocation>,
    /// Votes that failed verification or committee-membership before ever
    /// reaching the tally (crypto-invalid input, not equivocation).
    pub rejected: Vec<(FinalityVote, FinalityError)>,
}

impl AssemblyReport {
    /// The certificate finalizing `height`, if any (assumes the `<=f`
    /// invariant holds — returns the first one found if it doesn't).
    pub fn certificate_for_height(&self, height: u64) -> Option<&FinalityCertificate> {
        self.certificates.iter().find(|c| c.height == height)
    }

    /// Heights where MORE THAN ONE distinct `(spine_block_hash, order_hash)`
    /// tuple reached quorum — i.e., two conflicting certificates both
    /// assembled. Empty under the `<=f` Byzantine assumption; this is the
    /// literal, computable form of the safety property this crate exists to
    /// prove.
    pub fn conflicting_heights(&self) -> Vec<u64> {
        let mut by_height: HashMap<u64, HashSet<(BlockHash, OrderHash)>> = HashMap::new();
        for c in &self.certificates {
            by_height.entry(c.height).or_default().insert((c.spine_block_hash, c.order_hash));
        }
        let mut out: Vec<u64> = by_height.into_iter().filter(|(_, tuples)| tuples.len() > 1).map(|(h, _)| h).collect();
        out.sort_unstable();
        out
    }
}

/// Assemble certificates from a batch of votes against `committee`.
///
/// Pure function, no mutable state carried between calls — a caller with a
/// growing vote set simply re-invokes this over the full accumulated batch
/// (Phase 1 does not need incremental/streaming assembly; that is an
/// implementation-efficiency concern for a later phase, not a correctness
/// one — assembling from scratch is deterministic and gives the exact same
/// answer either way).
///
/// Steps, per the design doc:
///  1. Reject any vote with a bad signature or a `validator_id` outside
///     `committee` (recorded in `rejected`, never causes a panic).
///  2. For each `(height, validator_id)`, collect every DISTINCT tuple that
///     validator signed. More than one distinct tuple = equivocation:
///     recorded as evidence, and — conservatively — that validator's votes
///     at that height are excluded from every tally (an equivocator's
///     signature does not count toward ANY certificate, honest or not).
///  3. Tally the remaining (non-equivocating) votes by `(height,
///     spine_block_hash, order_hash)`. Any tuple whose distinct-validator
///     count reaches `committee`'s `availability_quorum` becomes a
///     certificate.
pub fn assemble(committee: &Committee, votes: &[FinalityVote]) -> AssemblyReport {
    assemble_impl(committee, votes, true)
}

/// [`assemble`] for votes the caller has ALREADY signature-checked — the observer's
/// retained set, which only ever holds votes that passed `verify()` in `observe`.
///
/// 2026-09-13 (rocky-bps100-0912): `try_assemble` re-verified every retained vote on
/// every observe — with two validators that was five Ed25519 verifications per block
/// (observe + assemble of one vote, then observe + assemble of two), ~7% of the produce
/// loop at 110 blk/s. The membership check and the equivocation/tally logic are
/// unchanged; only the redundant signature pass is skipped.
pub fn assemble_verified(committee: &Committee, votes: &[FinalityVote]) -> AssemblyReport {
    assemble_impl(committee, votes, false)
}

fn assemble_impl(committee: &Committee, votes: &[FinalityVote], check_sigs: bool) -> AssemblyReport {
    let mut report = AssemblyReport::default();
    if committee.is_empty() {
        // A zero-member committee cannot mean anything as a finality
        // authority; refuse to manufacture certificates from it rather than
        // let `availability_quorum(0) == 0` trivially "certify" nothing
        // against zero votes.
        return report;
    }
    let quorum = availability_quorum(committee.len());

    // (height, validator_id) -> every vote that validator, if a real
    // committee member with a valid signature, cast for that height.
    let mut per_validator: HashMap<(u64, ValidatorId), Vec<FinalityVote>> = HashMap::new();

    for v in votes {
        if check_sigs {
            if let Err(e) = v.verify() {
                report.rejected.push((v.clone(), e));
                continue;
            }
        }
        if !committee.contains(&v.validator_id) {
            report.rejected.push((v.clone(), FinalityError::NotCommitteeMember));
            continue;
        }
        per_validator.entry((v.height, v.validator_id)).or_default().push(v.clone());
    }

    let mut tallies: HashMap<(u64, BlockHash, OrderHash), Vec<FinalityVote>> = HashMap::new();

    for ((height, validator_id), vs) in per_validator.into_iter() {
        let mut distinct: Vec<FinalityVote> = Vec::new();
        for v in &vs {
            if !distinct.iter().any(|d| d.spine_block_hash == v.spine_block_hash && d.order_hash == v.order_hash) {
                distinct.push(v.clone());
            }
        }
        if distinct.len() > 1 {
            report.equivocations.push(Equivocation {
                validator_id,
                height,
                vote_a: distinct[0].clone(),
                vote_b: distinct[1].clone(),
            });
            continue; // excluded from every tally at this height
        }
        let canonical = distinct.into_iter().next().expect("vs is non-empty by construction");
        tallies.entry((height, canonical.spine_block_hash, canonical.order_hash)).or_default().push(canonical);
    }

    for ((height, spine_block_hash, order_hash), agreeing_votes) in tallies.into_iter() {
        if agreeing_votes.len() >= quorum {
            report.certificates.push(FinalityCertificate { height, spine_block_hash, order_hash, votes: agreeing_votes });
        }
    }

    report
}

/// Re-exports of the exact quorum-floor math this crate deliberately does
/// NOT reimplement, for callers that want it without also depending on
/// `sigil-braidpool` directly.
pub fn quorum_for(n: usize) -> usize {
    availability_quorum(n)
}

/// See [`sigil_braidpool::committee::max_byzantine`].
pub fn max_byzantine_for(n: usize) -> usize {
    max_byzantine(n)
}

/// See [`sigil_braidpool::committee::bft_active`].
pub fn bft_active_for(n: usize) -> bool {
    bft_active(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};

    fn gen_key(rng: &mut StdRng) -> SigningKey {
        let mut seed = [0u8; 32];
        rng.fill(&mut seed);
        SigningKey::from_bytes(&seed)
    }

    fn committee_of(keys: &[SigningKey]) -> Committee {
        Committee::new(keys.iter().map(|k| {
            let id = k.verifying_key().to_bytes();
            (id, id) // WalletId == ValidatorId == the pubkey, same convention as sigil-header
        }).collect())
    }

    // ── Basic sign/verify, mirroring producer_signing.rs's own test shapes ──

    #[test]
    fn sign_and_verify_round_trip() {
        let mut rng = StdRng::seed_from_u64(1);
        let key = gen_key(&mut rng);
        let vote = FinalityVote::sign(&key, 100, [7u8; 32], [8u8; 32]);
        assert_eq!(vote.validator_id, key.verifying_key().to_bytes());
        vote.verify().expect("freshly signed vote must verify");
    }

    #[test]
    fn tampering_after_signing_breaks_verification() {
        let mut rng = StdRng::seed_from_u64(2);
        let key = gen_key(&mut rng);
        let mut vote = FinalityVote::sign(&key, 1, [1u8; 32], [2u8; 32]);
        vote.order_hash[0] ^= 0xFF; // tamper AFTER signing
        assert_eq!(vote.verify(), Err(FinalityError::BadSignature));
    }

    #[test]
    fn malformed_validator_key_is_rejected_not_panicking() {
        let mut vote = FinalityVote::sign(&SigningKey::from_bytes(&[3u8; 32]), 1, [1u8; 32], [2u8; 32]);
        // Ed25519's verifying-key check is on curve validity, which almost
        // any 32 bytes fail to violate structurally (compressed points are
        // permissive) — so to exercise the "bad key" path deterministically
        // we corrupt the id to a value ed25519-dalek's decompression rejects.
        vote.validator_id = [0xFFu8; 32];
        // Either BadValidatorKey (decompression fails) or BadSignature
        // (decompresses but doesn't match) — both are graceful `Err`, never
        // a panic, which is the actual property under test.
        assert!(vote.verify().is_err());
    }

    // ── Quorum wiring sanity: matches the design doc's own table exactly ──

    #[test]
    fn quorum_table_matches_design_doc() {
        assert_eq!((quorum_for(4), max_byzantine_for(4), bft_active_for(4)), (3, 1, true));
        assert_eq!((quorum_for(7), max_byzantine_for(7), bft_active_for(7)), (5, 2, true));
        assert_eq!((quorum_for(10), max_byzantine_for(10), bft_active_for(10)), (7, 3, true));
        assert!(!bft_active_for(2), "2-of-2 (today's live producer count) is documented as NOT real BFT");
    }

    // ── assemble(): normal case ──

    #[test]
    fn all_honest_votes_form_exactly_one_certificate() {
        let mut rng = StdRng::seed_from_u64(3);
        let keys: Vec<SigningKey> = (0..5).map(|_| gen_key(&mut rng)).collect();
        let committee = committee_of(&keys);
        let (height, spine, order) = (42u64, [9u8; 32], [10u8; 32]);
        let votes: Vec<FinalityVote> = keys.iter().map(|k| FinalityVote::sign(k, height, spine, order)).collect();

        let report = assemble(&committee, &votes);
        assert!(report.rejected.is_empty());
        assert!(report.equivocations.is_empty());
        assert_eq!(report.certificates.len(), 1);
        let cert = &report.certificates[0];
        assert_eq!(cert.key(), (height, spine, order));
        assert_eq!(cert.votes.len(), 5, "all 5 honest votes should be attached, not just the quorum minimum");
    }

    #[test]
    fn below_quorum_never_certifies_liveness_pause_not_a_safety_break() {
        // n=5 -> f=1, quorum=4. Only 3 vote (an offline majority-of-honest
        // scenario beyond what f tolerates) — must NOT certify.
        let mut rng = StdRng::seed_from_u64(4);
        let keys: Vec<SigningKey> = (0..5).map(|_| gen_key(&mut rng)).collect();
        let committee = committee_of(&keys);
        let (height, spine, order) = (1u64, [1u8; 32], [2u8; 32]);
        let votes: Vec<FinalityVote> = keys[..3].iter().map(|k| FinalityVote::sign(k, height, spine, order)).collect();

        let report = assemble(&committee, &votes);
        assert!(report.certificates.is_empty(), "3-of-5 must not reach the quorum-of-4 floor");
    }

    #[test]
    fn non_member_vote_is_rejected_not_counted() {
        let mut rng = StdRng::seed_from_u64(5);
        let keys: Vec<SigningKey> = (0..4).map(|_| gen_key(&mut rng)).collect();
        let committee = committee_of(&keys[..3]); // only first 3 are members
        let (height, spine, order) = (1u64, [1u8; 32], [2u8; 32]);
        let mut votes: Vec<FinalityVote> = keys[..3].iter().map(|k| FinalityVote::sign(k, height, spine, order)).collect();
        votes.push(FinalityVote::sign(&keys[3], height, spine, order)); // outsider

        let report = assemble(&committee, &votes);
        assert_eq!(report.rejected.len(), 1);
        assert_eq!(report.rejected[0].1, FinalityError::NotCommitteeMember);
        // 3-of-3 real members still reaches quorum(3)=3 on its own.
        assert_eq!(report.certificates.len(), 1);
        assert_eq!(report.certificates[0].votes.len(), 3);
    }

    #[test]
    fn equivocation_is_detected_and_excluded_from_every_tally() {
        // n=4 -> f=1, quorum=3. One validator signs TWO different tuples at
        // the same height (real double-signing, real distinct signatures).
        let mut rng = StdRng::seed_from_u64(6);
        let keys: Vec<SigningKey> = (0..4).map(|_| gen_key(&mut rng)).collect();
        let committee = committee_of(&keys);
        let height = 7u64;
        let (spine_a, order_a) = ([1u8; 32], [2u8; 32]);
        let (spine_b, order_b) = ([3u8; 32], [4u8; 32]);

        let mut votes = vec![
            FinalityVote::sign(&keys[0], height, spine_a, order_a),
            FinalityVote::sign(&keys[1], height, spine_a, order_a),
            FinalityVote::sign(&keys[2], height, spine_a, order_a),
        ];
        // keys[3] equivocates: signs for BOTH tuples.
        votes.push(FinalityVote::sign(&keys[3], height, spine_a, order_a));
        votes.push(FinalityVote::sign(&keys[3], height, spine_b, order_b));

        let report = assemble(&committee, &votes);
        assert_eq!(report.equivocations.len(), 1, "keys[3]'s double-vote must be caught");
        assert_eq!(report.equivocations[0].validator_id, keys[3].verifying_key().to_bytes());
        // keys[3] excluded entirely: only keys[0..3] remain on spine_a/order_a
        // = 3 votes = exactly quorum(4) = 3, so it STILL certifies on the
        // strength of the 3 genuinely-agreeing honest validators alone.
        assert_eq!(report.certificates.len(), 1);
        assert_eq!(report.certificates[0].key(), (height, spine_a, order_a));
        assert_eq!(report.certificates[0].votes.len(), 3);
    }

    #[test]
    fn conflicting_heights_is_empty_in_the_honest_case() {
        let mut rng = StdRng::seed_from_u64(7);
        let keys: Vec<SigningKey> = (0..4).map(|_| gen_key(&mut rng)).collect();
        let committee = committee_of(&keys);
        let votes: Vec<FinalityVote> =
            keys.iter().map(|k| FinalityVote::sign(k, 1, [5u8; 32], [6u8; 32])).collect();
        let report = assemble(&committee, &votes);
        assert!(report.conflicting_heights().is_empty());
    }

    // ── §Safety and liveness, made computational: the adversarial property test ──

    /// Deterministic per-trial adversarial vote generator. `byzantine_count`
    /// validators (the LAST `byzantine_count` keys, by convention) behave
    /// arbitrarily: each independently and randomly chooses to vote for the
    /// honest tuple, an alternate tuple, BOTH (equivocate), or abstain. The
    /// first `n - byzantine_count` keys are honest and ALWAYS vote for the
    /// single canonical tuple — the actual behavioral meaning of "honest"
    /// under the `<=f` assumption this whole design rests on.
    fn adversarial_trial(seed: u64, n: usize, byzantine_count: usize) -> AssemblyReport {
        let mut rng = StdRng::seed_from_u64(seed);
        let keys: Vec<SigningKey> = (0..n).map(|_| gen_key(&mut rng)).collect();
        let committee = committee_of(&keys);
        let height = 1_000_000u64.wrapping_add(seed);
        let (honest_spine, honest_order) = ([0xAAu8; 32], [0xBBu8; 32]);
        let (rogue_spine, rogue_order) = ([0xCCu8; 32], [0xDDu8; 32]);

        let honest_n = n - byzantine_count;
        let mut votes = Vec::new();
        for k in &keys[..honest_n] {
            votes.push(FinalityVote::sign(k, height, honest_spine, honest_order));
        }
        for k in &keys[honest_n..] {
            match rng.gen_range(0..4u8) {
                0 => votes.push(FinalityVote::sign(k, height, honest_spine, honest_order)), // votes honestly anyway
                1 => votes.push(FinalityVote::sign(k, height, rogue_spine, rogue_order)),   // votes for a rogue tuple
                2 => {
                    // equivocates: signs BOTH
                    votes.push(FinalityVote::sign(k, height, honest_spine, honest_order));
                    votes.push(FinalityVote::sign(k, height, rogue_spine, rogue_order));
                }
                _ => {} // abstains / offline
            }
        }
        assemble(&committee, &votes)
    }

    /// For every `n` the design doc's own table names (4, 5, 7, 10) and
    /// every trial where the Byzantine count stays at or below
    /// `max_byzantine(n)`, no two conflicting certificates assemble for the
    /// same height from one vote set, however the Byzantine minority behaves
    /// (rogue votes, equivocation, abstention, any mix). This feeds every
    /// vote to ONE `assemble()` call, so it tests one node's view; the
    /// cross-node property is [`conflicting_views_need_more_than_f_byzantine`].
    /// 8,000 seeded trials total (2,000 per `n`), fully reproducible from
    /// the seed if one ever fails.
    #[test]
    fn adversarial_safety_holds_at_or_below_f() {
        let mut total_certs_formed = 0u64;
        for &n in &[4usize, 5, 7, 10] {
            let f = max_byzantine_for(n);
            for seed in 0..2000u64 {
                let report = adversarial_trial(seed * 31 + n as u64, n, f);
                let conflicts = report.conflicting_heights();
                assert!(
                    conflicts.is_empty(),
                    "SAFETY VIOLATION at n={n} f={f} seed={seed}: conflicting certificates {conflicts:?}"
                );
                total_certs_formed += report.certificates.len() as u64;
            }
        }
        // Coverage sanity: the honest majority should actually have
        // certified something in a healthy fraction of trials (it always
        // has >= quorum honest votes for the honest tuple, so it always
        // should) — if this were 0 the fuzz harness would be vacuously
        // "passing" without exercising the certifying path at all.
        assert!(total_certs_formed > 1000, "fuzz harness produced too few real certificates to be meaningful: {total_certs_formed}");
    }

    /// The converse half of the same property, made explicit: when
    /// `honest_n >= 2f+1` (guaranteed whenever `byzantine_count <= f`,
    /// since `n = honest_n + byzantine_count` and `n - f = quorum <=
    /// honest_n` exactly when `byzantine_count <= f`), the honest tuple
    /// itself DOES always certify — liveness holds, not just safety.
    #[test]
    fn honest_majority_always_certifies_when_byzantine_leq_f() {
        for &n in &[4usize, 5, 7, 10] {
            let f = max_byzantine_for(n);
            for seed in 0..500u64 {
                let report = adversarial_trial(seed * 17 + n as u64, n, f);
                let height = 1_000_000u64.wrapping_add(seed * 17 + n as u64);
                let cert = report.certificate_for_height(height);
                assert!(
                    cert.is_some(),
                    "n={n} f={f} seed={seed}: the honest tuple must certify when byzantine_count<=f"
                );
                assert_eq!(cert.unwrap().spine_block_hash, [0xAAu8; 32], "must be the HONEST tuple, not a rogue one");
            }
        }
    }

    /// A property of ONE vote set, and only that. `assemble()` excludes an
    /// equivocating validator's votes from EVERY tally, not just the
    /// "losing" one. For two conflicting certificates to come out of a
    /// single call, they would need two DISJOINT groups of non-equivocating
    /// validators, each of size >= `quorum(n)`, which needs
    /// `2*quorum(n) <= n`. But `2*quorum(n) > n` for every `n` (checked
    /// below), so it cannot happen within one call, at any dishonest count.
    ///
    /// **Corrected 2026-10-07 (external review).** An earlier version of
    /// this doc called this a network-wide guarantee "stronger than `<=f`".
    /// It is not. Exclusion only works on equivocations a node has SEEN, and
    /// two nodes can each receive a quorum for a different tuple without
    /// either seeing an equivocation; see
    /// [`distributed_views_can_certify_conflicting_tuples_beyond_f`]. Across
    /// nodes the guarantee is the classical `<=f` one, pinned by
    /// [`conflicting_views_need_more_than_f_byzantine`].
    #[test]
    fn one_vote_set_never_certifies_two_conflicting_tuples() {
        // The arithmetic fact itself, for every committee size 1..=64 (well
        // past the design doc's own table of 4/5/7/10).
        for n in 1..=64usize {
            let q = quorum_for(n);
            assert!(2 * q > n, "n={n}: 2*quorum={} must exceed n={n} for the exclusion argument to hold", 2 * q);
        }

        // Sweep byzantine_count from 0 up to n (a fully dishonest committee),
        // all votes into ONE assemble() call: conflicting certificates must
        // never come out of a single vote set.
        for &n in &[4usize, 5, 7, 10] {
            for byzantine_count in 0..=n {
                for seed in 0..200u64 {
                    let report = adversarial_trial(seed * 97 + byzantine_count as u64, n, byzantine_count);
                    assert!(
                        report.conflicting_heights().is_empty(),
                        "n={n} byzantine_count={byzantine_count} seed={seed}: conflicting certificates from ONE \
                         vote set — the exclusion argument above is wrong, or assemble() regressed"
                    );
                }
            }
        }
    }

    /// The external review's counterexample (2026-10-07), as a test. `n = 4`,
    /// `quorum = 3`, `f = 1`. Byzantine `X` and `Y` sign both A and B; honest
    /// `H1` signs A; honest `H2` signs B. Two nodes that each receive one
    /// side certify CONFLICTING tuples, and neither has seen an equivocation.
    /// Only a node that sees all six votes catches `X` and `Y`, and that node
    /// certifies nothing. Two Byzantine validators is more than `f`, so this
    /// does not break the classical guarantee; it shows that equivocator
    /// exclusion does not extend it.
    #[test]
    fn distributed_views_can_certify_conflicting_tuples_beyond_f() {
        let mut rng = StdRng::seed_from_u64(20_261_007);
        let keys: Vec<SigningKey> = (0..4).map(|_| gen_key(&mut rng)).collect();
        let committee = committee_of(&keys);
        assert_eq!((quorum_for(4), max_byzantine_for(4)), (3, 1));
        let height = 1_111_478u64;
        let (spine_a, order_a) = ([0xAAu8; 32], [0x01u8; 32]);
        let (spine_b, order_b) = ([0xBBu8; 32], [0x02u8; 32]);
        let (x, y, h1, h2) = (&keys[0], &keys[1], &keys[2], &keys[3]);

        let view_1 = vec![
            FinalityVote::sign(x, height, spine_a, order_a),
            FinalityVote::sign(y, height, spine_a, order_a),
            FinalityVote::sign(h1, height, spine_a, order_a),
        ];
        let view_2 = vec![
            FinalityVote::sign(x, height, spine_b, order_b),
            FinalityVote::sign(y, height, spine_b, order_b),
            FinalityVote::sign(h2, height, spine_b, order_b),
        ];
        let node_1 = assemble(&committee, &view_1);
        let node_2 = assemble(&committee, &view_2);
        assert!(node_1.equivocations.is_empty() && node_2.equivocations.is_empty(),
            "neither node has seen both votes of X or Y");
        assert_eq!(node_1.certificate_for_height(height).map(|c| c.spine_block_hash), Some(spine_a));
        assert_eq!(node_2.certificate_for_height(height).map(|c| c.spine_block_hash), Some(spine_b));

        let everything: Vec<FinalityVote> = view_1.iter().chain(view_2.iter()).cloned().collect();
        let all_seen = assemble(&committee, &everything);
        assert_eq!(all_seen.equivocations.len(), 2, "a node that sees all six votes catches X and Y");
        assert!(all_seen.certificates.is_empty(), "with X and Y excluded, H1 and H2 alone reach no quorum");
    }

    /// Cross-node safety, exhaustive for the design doc's committee sizes.
    /// Byzantine validators sign BOTH tuples; each honest validator signs
    /// one. The views most favourable to an attacker are one node that sees
    /// every A-vote and no B-vote, and another that sees every B-vote and no
    /// A-vote: neither sees an equivocation. For every Byzantine count and
    /// every split of the honest validators, this builds exactly those two
    /// views, runs the real `assemble()` on each, and records the smallest
    /// Byzantine count at which BOTH certify. It must exceed `f` (the
    /// classical `(2f+1)+(2f+1)-(3f+1) = f+1` overlap), and it is exactly
    /// `2*quorum(n) - n`.
    #[test]
    fn conflicting_views_need_more_than_f_byzantine() {
        for &n in &[4usize, 5, 7, 10] {
            let (f, q) = (max_byzantine_for(n), quorum_for(n));
            let mut rng = StdRng::seed_from_u64(4_242 + n as u64);
            let keys: Vec<SigningKey> = (0..n).map(|_| gen_key(&mut rng)).collect();
            let committee = committee_of(&keys);
            let height = 77u64;
            let (spine_a, order_a) = ([0xA1u8; 32], [0xA2u8; 32]);
            let (spine_b, order_b) = ([0xB1u8; 32], [0xB2u8; 32]);

            let mut smallest_splitting_byzantine = None;
            'byzantine: for byz in 0..=n {
                let honest = n - byz;
                for honest_a in 0..=honest {
                    // keys[..byz] Byzantine, then honest_a honest for A, the rest honest for B.
                    let view_a: Vec<FinalityVote> = keys[..byz].iter().chain(keys[byz..byz + honest_a].iter())
                        .map(|k| FinalityVote::sign(k, height, spine_a, order_a)).collect();
                    let view_b: Vec<FinalityVote> = keys[..byz].iter().chain(keys[byz + honest_a..].iter())
                        .map(|k| FinalityVote::sign(k, height, spine_b, order_b)).collect();
                    let a = assemble(&committee, &view_a).certificate_for_height(height).is_some();
                    let b = assemble(&committee, &view_b).certificate_for_height(height).is_some();
                    if a && b {
                        assert!(byz > f, "n={n} f={f}: two views certified conflicting tuples with only {byz} Byzantine");
                        smallest_splitting_byzantine = Some(byz);
                        break 'byzantine;
                    }
                }
            }
            let smallest = smallest_splitting_byzantine
                .expect("a fully dishonest committee can always split two views");
            assert_eq!(smallest, 2 * q - n, "n={n}: smallest Byzantine count that splits two views");
            assert!(smallest > f, "n={n}: the classical <=f guarantee must hold across nodes");
        }
    }

    /// A FULLY dishonest committee (no equivocation involved; every
    /// validator simply signs the SAME false tuple) certifies that wrong
    /// tuple without any trouble. That is expected, not a bug: a
    /// signature-based finality gadget cannot detect "everyone is lying in
    /// unison" from the signatures alone. Within this one vote set only one
    /// certificate exists; across nodes, a dishonest committee can also
    /// certify different tuples to different nodes (see
    /// [`conflicting_views_need_more_than_f_byzantine`]).
    #[test]
    fn fully_dishonest_committee_can_certify_a_wrong_tuple() {
        let mut rng = StdRng::seed_from_u64(9999);
        let n = 5;
        let keys: Vec<SigningKey> = (0..n).map(|_| gen_key(&mut rng)).collect();
        let committee = committee_of(&keys);
        let (height, false_spine, false_order) = (1u64, [0x77u8; 32], [0x88u8; 32]);

        // ALL n validators dishonestly agree on the SAME false claim.
        let votes: Vec<FinalityVote> = keys.iter().map(|k| FinalityVote::sign(k, height, false_spine, false_order)).collect();
        let report = assemble(&committee, &votes);

        let cert = report.certificate_for_height(height).expect("unanimous agreement certifies, honest or not — inherent to any signature scheme");
        assert_eq!(cert.spine_block_hash, false_spine, "the (false) tuple certifies exactly as any unanimous tuple would");
        assert!(report.conflicting_heights().is_empty(), "one unanimous vote set yields exactly one certificate");
    }
}
