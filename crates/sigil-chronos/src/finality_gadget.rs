//! `finality_gadget` — chronos scenarios for Phase 1 of **SIGIL True Instant
//! Finality** (`docs/research/SIGIL_INSTANT_FINALITY_v0.tex`, §Phased
//! implementation plan, §Honest risk assessment).
//!
//! Drives the REAL `sigil_finality::assemble` (Phase 1's own quorum/
//! certificate assembly, not a reimplementation here) with
//! `spine_block_hash`/`order_hash` inputs taken from REAL production code:
//! [`crate::SigilSimNode`] for the honest/liveness/equivocation cases, and
//! the REAL `sigil_dagknight::Braid` ordering engine for the deep-reorder
//! regression case — the exact DagKnight machinery the finality gadget is
//! designed to sit on top of, not a mocked stand-in.
//!
//! Covers every adversarial scenario the design doc's §Honest risk
//! assessment names as required "before Phase 3 ever touches the live
//! chain":
//!   - normal case: quorum forms at every checkpoint
//!   - `<=f` validators silent: a liveness hiccup only, catches up
//!   - `>f` validators silent: finality correctly HALTS, never proceeds
//!     on insufficient data
//!   - equivocation: safety holds, evidence captured, honest tuple still
//!     certifies
//!   - the exact deep-reorder mechanism that forced `BraidConfig::
//!     final_depth` from 64 to 512 (`examples/k_probe.rs`'s own finding),
//!     replayed against the finality gadget directly
//!
//! Order-hash simplification, stated honestly: Phase 1 has no accessor from
//! a [`crate::Block`] to `Braid`'s own `frozen_acc` (they are independent
//! crates in this phase — wiring them together is Phase 2/3 territory, see
//! `sigil-finality`'s own crate doc). The honest/liveness/equivocation
//! scenarios below therefore chain a `BLAKE3(acc || block_hash)` accumulator
//! over the real produced block-hash sequence — the identical chaining rule
//! `Braid::order_hash()` itself uses (`crates/sigil-dagknight/src/braid.rs`,
//! `chain_hash`), applied to the sim's real blocks instead of a Braid
//! instance. The deep-reorder scenario, by contrast, drives a REAL `Braid`
//! and reads its REAL `order_hash()` directly — no simplification there.

use ed25519_dalek::SigningKey;

use sigil_braidpool::committee::Committee;
use sigil_dagknight::{BlockView, Braid, BraidConfig};
use sigil_finality::{assemble, FinalityVote};
use sigil_header::BlockHash;

use crate::{demo_genesis, sign_dummy, SigilSimNode};
use flux_chronos::NodeId;

/// Deterministic n-validator committee: fixed, reproducible Ed25519 seeds
/// (real keys, real signatures — not placeholders), one per validator
/// index. Shared by every scenario in this module so results are
/// reproducible run to run.
fn validator_keys(n: usize) -> Vec<SigningKey> {
    (0..n as u8)
        .map(|i| {
            let mut h = blake3::Hasher::new();
            h.update(b"finality_gadget/validator");
            h.update(&[i]);
            SigningKey::from_bytes(h.finalize().as_bytes())
        })
        .collect()
}

/// A committee where each validator's `WalletId` IS its own Ed25519 pubkey
/// — the same "id == pubkey" convention `sigil-header::ValidatorId` already
/// uses for `Ed25519Hot` producers.
fn committee_of(keys: &[SigningKey]) -> Committee {
    Committee::new(
        keys.iter()
            .map(|k| {
                let id = k.verifying_key().to_bytes();
                (id, id)
            })
            .collect(),
    )
}

/// `demo_genesis()` only funds wallets `[1;32]..=[5;32]` (same constraint
/// `wedge_writer_gap.rs::funded_wallet` documents) — stepping `i` by 1 mod 5
/// always yields a DIFFERENT wallet for `to` than `from`, since the modulus
/// (5) exceeds the step (1).
fn funded_wallet(i: u64) -> [u8; 32] {
    [((i % 5) + 1) as u8; 32]
}

/// Produce `n_checkpoints` real blocks from one producer [`SigilSimNode`],
/// returning `(height, spine_block_hash, order_hash)` per checkpoint — the
/// exact tuple a real validator would vote on. `order_hash` chains the SAME
/// way `Braid::order_hash()` does (see module doc's "simplification, stated
/// honestly" paragraph).
fn produce_checkpoints(n_checkpoints: u64) -> Vec<(u64, BlockHash, [u8; 32])> {
    let g = demo_genesis();
    let mut producer = SigilSimNode::new("producer", NodeId(0), vec![], true, 1, &g);
    let mut acc = [0u8; 32];
    let mut out = Vec::with_capacity(n_checkpoints as usize);
    for i in 0..n_checkpoints {
        producer.enqueue_tx(sign_dummy(sigil_tx::SigilTx::Send {
            from: funded_wallet(i),
            to: funded_wallet(i + 1),
            amount: 1,
            token: sigil_tx::NATIVE,
            fee: 0,
        }));
        let block = producer.produce_one().expect("producer mints a block from a non-empty mempool");
        let h = block.hash();
        let mut hasher = blake3::Hasher::new();
        hasher.update(&acc);
        hasher.update(&h);
        acc = *hasher.finalize().as_bytes();
        out.push((block.header.height, h, acc));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Scenario 1: normal case — quorum every checkpoint ──

    #[test]
    fn all_honest_validators_finalize_every_checkpoint() {
        let n = 5;
        let keys = validator_keys(n);
        let committee = committee_of(&keys);
        let checkpoints = produce_checkpoints(20);

        for (height, spine, order) in &checkpoints {
            let votes: Vec<FinalityVote> =
                keys.iter().map(|k| FinalityVote::sign(k, *height, *spine, *order)).collect();
            let report = assemble(&committee, &votes);
            let cert = report
                .certificate_for_height(*height)
                .unwrap_or_else(|| panic!("checkpoint {height} must certify when all {n} validators agree"));
            assert_eq!(cert.spine_block_hash, *spine);
            assert_eq!(cert.votes.len(), n);
            assert!(report.equivocations.is_empty());
            assert!(report.conflicting_heights().is_empty());
        }
    }

    // ── Scenario 2: <=f offline — liveness hiccup, never a safety break ──

    #[test]
    fn up_to_f_offline_with_slack_never_misses_a_checkpoint() {
        // n=7 -> f=2, quorum=5. ONE validator (< f) permanently silent —
        // real slack remains, so every single checkpoint still finalizes.
        let n = 7;
        let keys = validator_keys(n);
        let committee = committee_of(&keys);
        let checkpoints = produce_checkpoints(15);
        let online = &keys[..6]; // keys[6] silent for the whole run

        for (height, spine, order) in &checkpoints {
            let votes: Vec<FinalityVote> =
                online.iter().map(|k| FinalityVote::sign(k, *height, *spine, *order)).collect();
            let report = assemble(&committee, &votes);
            assert!(
                report.certificate_for_height(*height).is_some(),
                "6-of-7 with 1 (< f=2) offline must still finalize height {height}"
            );
        }
    }

    #[test]
    fn exactly_f_offline_is_zero_margin_and_one_more_hiccup_pauses_then_catches_up() {
        // n=7 -> f=2, quorum=5. keys[5],keys[6] permanently silent (offline
        // == f exactly — the design doc's own "zero margin" case). The
        // remaining 5 == quorum(7) exactly, so it still finalizes with NO
        // slack left. If, on top of that, ONE more of the remaining 5 is
        // merely delayed for a single checkpoint (a realistic network
        // hiccup, not dishonesty), that round does not finalize — but the
        // checkpoint is not lost: once the delayed vote arrives, the SAME
        // pure `assemble()` call over the accumulated votes finalizes it.
        // This is exactly the design doc's claim: "a liveness hiccup, never
        // a safety break... catches up once reconnected."
        let n = 7;
        let keys = validator_keys(n);
        let committee = committee_of(&keys);
        let checkpoints = produce_checkpoints(3);
        let (h0, s0, o0) = checkpoints[0];
        let (h1, s1, o1) = checkpoints[1];

        // Zero margin still finalizes cleanly with exactly quorum(7)=5.
        let votes_ok: Vec<FinalityVote> = keys[..5].iter().map(|k| FinalityVote::sign(k, h0, s0, o0)).collect();
        let report_ok = assemble(&committee, &votes_ok);
        assert!(report_ok.certificate_for_height(h0).is_some(), "exactly quorum(7)=5 must still certify");

        // A SECOND, independent round: keys[4] (one of the online 5) is
        // ALSO delayed this time on top of the 2 permanently silent ones —
        // only 4-of-7 vote, below quorum(7)=5.
        let votes_short: Vec<FinalityVote> = keys[..4].iter().map(|k| FinalityVote::sign(k, h1, s1, o1)).collect();
        let report_short = assemble(&committee, &votes_short);
        assert!(
            report_short.certificate_for_height(h1).is_none(),
            "4-of-7 (below quorum(7)=5) must NOT finalize — this is the liveness pause"
        );

        // keys[4]'s vote finally arrives (late). Re-running assemble over
        // the accumulated votes — the ONLY thing a real system would need
        // to do once the vote lands — now finalizes h1 too. The checkpoint
        // was delayed, never lost.
        let mut votes_caught_up = votes_short;
        votes_caught_up.push(FinalityVote::sign(&keys[4], h1, s1, o1));
        let report_caught_up = assemble(&committee, &votes_caught_up);
        assert!(
            report_caught_up.certificate_for_height(h1).is_some(),
            "the late-arriving vote must let h1 catch up — nothing about the pause makes it permanent"
        );
    }

    // ── Scenario 3: >f offline — finality correctly HALTS ──

    #[test]
    fn more_than_f_offline_halts_finality_entirely_then_resumes() {
        // n=7 -> f=2, quorum=5. THREE offline (f+1=3) for many consecutive
        // checkpoints: no certificate must EVER assemble, for as long as it
        // lasts — nothing silently proceeds on insufficient agreement.
        let n = 7;
        let keys = validator_keys(n);
        let committee = committee_of(&keys);
        let checkpoints = produce_checkpoints(10);
        let online = &keys[..4]; // 4 < quorum(7)=5

        for (height, spine, order) in &checkpoints {
            let votes: Vec<FinalityVote> =
                online.iter().map(|k| FinalityVote::sign(k, *height, *spine, *order)).collect();
            let report = assemble(&committee, &votes);
            assert!(
                report.certificate_for_height(*height).is_none(),
                "4-of-7 (f+1=3 offline, beyond tolerance) must NEVER finalize height {height}"
            );
        }

        // Resumes the instant enough validators return (back to <= f offline).
        let (height, spine, order) = checkpoints[9];
        let recovered: Vec<FinalityVote> = keys[..5].iter().map(|k| FinalityVote::sign(k, height, spine, order)).collect();
        let report = assemble(&committee, &recovered);
        assert!(
            report.certificate_for_height(height).is_some(),
            "finality must resume the instant the offline count drops back to <= f"
        );
    }

    // ── Scenario 4: equivocation — safety holds, evidence captured ──

    #[test]
    fn equivocating_validator_is_caught_and_cannot_break_safety() {
        // n=5 -> f=1, quorum=4. keys[4] is the SOLE dishonest validator: it
        // signs BOTH the real tuple and a rogue alternate at the same
        // height (real double-signing with two real, valid signatures —
        // not a malformed vote). keys[0..4] (4 honest, == quorum(5)) always
        // vote the real tuple.
        let n = 5;
        let keys = validator_keys(n);
        let committee = committee_of(&keys);
        let checkpoints = produce_checkpoints(1);
        let (height, spine, order) = checkpoints[0];
        let mut rogue_spine = spine;
        rogue_spine[0] ^= 0xFF; // a plausible-looking but different tuple
        let rogue_order = order;

        let mut votes: Vec<FinalityVote> = keys[..4].iter().map(|k| FinalityVote::sign(k, height, spine, order)).collect();
        votes.push(FinalityVote::sign(&keys[4], height, spine, order)); // equivocator's FIRST vote
        votes.push(FinalityVote::sign(&keys[4], height, rogue_spine, rogue_order)); // its SECOND, conflicting vote

        let report = assemble(&committee, &votes);

        assert_eq!(report.equivocations.len(), 1, "the double-sign must be caught");
        let ev = &report.equivocations[0];
        assert_eq!(ev.validator_id, keys[4].verifying_key().to_bytes());
        assert_eq!(ev.height, height);
        // Both signed votes are real evidence, visible on the wire — per
        // the design doc: "cheap, strong evidence for a future
        // slashing/eviction mechanism."
        assert_ne!(ev.vote_a.spine_block_hash, ev.vote_b.spine_block_hash);
        ev.vote_a.verify().expect("evidence vote A must be a real, valid signature");
        ev.vote_b.verify().expect("evidence vote B must be a real, valid signature");

        // Safety: the honest tuple STILL certifies, on the strength of the
        // 4 genuinely-agreeing honest validators alone (== quorum(5)=4).
        let cert = report.certificate_for_height(height).expect("4 honest votes (quorum) must still certify");
        assert_eq!(cert.spine_block_hash, spine);
        assert_eq!(cert.votes.len(), 4, "the equivocator's votes must be excluded from the tally entirely");

        // And the rogue tuple never certifies — no conflicting certificate
        // ever forms.
        assert!(report.conflicting_heights().is_empty());
    }

    // ── Scenario 5: the deep-reorder regression (final_depth 64 -> 512) ──

    /// Tiny deterministic PRNG (SplitMix64) — same shape as
    /// `sigil-chronos::property`'s own generator, no external dep.
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
    }

    fn dr_block_hash(producer: usize, height: u64) -> BlockHash {
        let mut h = blake3::Hasher::new();
        h.update(b"finality_gadget/deep_reorder/blk");
        h.update(&(producer as u64).to_le_bytes());
        h.update(&height.to_le_bytes());
        *h.finalize().as_bytes()
    }
    fn dr_producer_id(producer: usize) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        h.update(b"finality_gadget/deep_reorder/prod");
        h.update(&(producer as u64).to_le_bytes());
        *h.finalize().as_bytes()
    }

    /// Reproduces the EXACT structural mechanism `crates/sigil-dagknight/
    /// examples/k_probe.rs` used to find the divergence that forced
    /// `BraidConfig::final_depth` from 64 to 512 — P=6 concurrent
    /// producers, k=1 merge parent each, generated in creation order, then
    /// fed into two independent `Braid` instances: node A in that same
    /// creation order, node B under a BOUNDED FORWARD reorder window (k_probe's
    /// own doc, verbatim: "KPROBE_REORDER=N is a bounded forward reorder of
    /// window N, which is the realistic model" — as opposed to its
    /// `KPROBE_REORDER=0` full-random-permutation mode, which that SAME doc
    /// calls out as "maximally adversarial, and NOT how gossip behaves").
    ///
    /// **First attempt used the full-shuffle mode and got a genuinely
    /// different, less useful result, worth recording honestly:** node B's
    /// tip never advanced far enough to finalize ANYTHING even at
    /// `final_depth=64` (not "converges vs. diverges" — it just stalled),
    /// because k=1 admission is extremely sensitive to full random
    /// reordering (matching k_probe's own doc: a shuffled feed "ends up
    /// ordering far fewer blocks... 421 vs 2400"). That is a real, separate
    /// finding (full adversarial shuffle can starve a k=1 braid entirely),
    /// but it is not the specific historical scenario that set
    /// `final_depth=512` — that used the BOUNDED window model, replicated
    /// here — and node B also needs k_probe's own exhaustive backfill loop
    /// (parked blocks are retried, not abandoned) to make fair progress.
    fn build_two_arrival_order_braids(final_depth: u64, reorder_window: usize, rounds: usize) -> (Braid, Braid) {
        const PRODUCERS: usize = 6;
        const K: usize = 1;
        const GENESIS: BlockHash = [0u8; 32];

        let mut backlog: Vec<std::collections::VecDeque<(usize, BlockHash)>> =
            vec![std::collections::VecDeque::new(); PRODUCERS];
        let mut views: Vec<BlockView> = Vec::new();
        for r in 1..=rounds {
            let mut minted = Vec::with_capacity(PRODUCERS);
            for p in 0..PRODUCERS {
                let height = r as u64;
                let hash = dr_block_hash(p, height);
                let parent = if r == 1 { GENESIS } else { dr_block_hash(p, height - 1) };
                let mut merge_parents = Vec::with_capacity(K);
                while merge_parents.len() < K {
                    match backlog[p].front() {
                        Some(&(vis, h)) if vis <= r => {
                            backlog[p].pop_front();
                            if h != parent && !merge_parents.contains(&h) {
                                merge_parents.push(h);
                            }
                        }
                        _ => break,
                    }
                }
                views.push(BlockView { hash, parent, merge_parents, height, producer: dr_producer_id(p),
                    // 0 = a producer free-run mint, which is what 99.83% of real
                    // blocks are (7 of 4096 measured 2026-08-28 carried a real
                    // solve). Matching that here keeps the simulation honest:
                    // WorkPolicy defaults to UniformCount precisely because
                    // weighting by this field would give almost every block zero.
                    difficulty: 0 });
                minted.push((p, hash));
            }
            for (origin, hash) in &minted {
                for p in 0..PRODUCERS {
                    if p != *origin {
                        backlog[p].push_back((r, *hash));
                    }
                }
            }
        }

        let cfg = |fd: u64| BraidConfig {
            final_depth: fd,
            max_window: 1 << 20,
            max_pending: 1 << 18,
            max_merge_parents: K.max(1),
            ghostdag_k: None,
            final_blue_depth: None,
            saturated_self_heal_window: 1 << 20,
            // 0 disables pending eviction entirely — the same value
            // `sigil_dagknight::sim` uses in both its simulation configs,
            // and for the same reason. Every other cap here is already set
            // enormous (1<<20) so that eviction can never confound the
            // measurement; inheriting the production default (512) would
            // quietly switch tip-lag eviction ON inside the one scenario
            // that exists to replay DEEP reordering, which is exactly where
            // it could mask the behaviour under test.
            pending_max_tip_lag: 0,
        };

        // Node A: creation order (k_probe.rs: `Braid::new_with_base(cfg, GENESIS, 0)`).
        let mut creation = Braid::new_with_base(cfg(final_depth), GENESIS, 0);
        for v in &views {
            creation.insert(v.clone());
        }

        // Node B: bounded forward reorder window (k_probe.rs's `KPROBE_REORDER=N` mode).
        let mut order: Vec<usize> = (0..views.len()).collect();
        let mut rng = SplitMix64::new(0xDEAD_BEEF_C0FFEE);
        for i in 0..order.len() {
            let w = (order.len() - i).min(reorder_window) as u64;
            if w > 1 {
                let j = i + (rng.next_u64() % w) as usize;
                order.swap(i, j);
            }
        }
        let mut reordered = Braid::new_with_base(cfg(final_depth), GENESIS, 0);
        for &i in &order {
            reordered.insert(views[i].clone());
        }

        // Exhaustive backfill (k_probe.rs: up to 64 passes, re-offering the
        // missing-parents worklist AND the full view set each pass) — a
        // parked block whose parent has since arrived must actually get a
        // chance to be re-tried, or node B unfairly "loses" blocks it
        // legitimately could absorb.
        let index: std::collections::HashMap<BlockHash, usize> =
            views.iter().enumerate().map(|(i, v)| (v.hash, i)).collect();
        for _pass in 0..64 {
            let mut progressed = false;
            for h in reordered.missing_parents() {
                if let Some(&i) = index.get(&h) {
                    if matches!(reordered.insert(views[i].clone()), sigil_dagknight::InsertOutcome::Inserted { .. }) {
                        progressed = true;
                    }
                }
            }
            for v in &views {
                if matches!(reordered.insert(v.clone()), sigil_dagknight::InsertOutcome::Inserted { .. }) {
                    progressed = true;
                }
            }
            if !progressed {
                break;
            }
        }

        (creation, reordered)
    }

    #[test]
    fn deep_reorder_replay_against_both_final_depths() {
        // ONE test, honestly observing BOTH configurations and asserting
        // exactly what was measured (see the report this session produced
        // for the real numbers) — rather than assuming in advance which
        // way it breaks. The invariant that must ALWAYS hold, regardless of
        // whether the two arrival orders converge or diverge: the finality
        // gadget must NEVER certify two different tuples for one height,
        // and must NEVER certify a tuple that isn't backed by real quorum
        // agreement.
        let n = 4;
        let keys = validator_keys(n);
        let committee = committee_of(&keys);
        // Window=64 is the exact value `BraidConfig::final_depth`'s own doc
        // names as where v1 divergence starts ("the exact same test starts
        // diverging at a reorder window of 64, i.e. right at the OLD
        // final_depth"). rounds=1000: must comfortably clear BOTH tested
        // final_depths (a braid only finalizes anything once its tip height
        // exceeds final_depth) — 400 (k_probe.rs's own default) was plenty
        // for 64 but left the final_depth=512 case with zero margin (tip
        // never reached 512), caught by this test's own setup assertion
        // rather than silently measuring nothing.
        let (reorder_window, rounds) = (64usize, 1000usize);

        for &final_depth in &[64u64, 512u64] {
            let (creation_order_braid, reordered_braid) = build_two_arrival_order_braids(final_depth, reorder_window, rounds);
            let h = creation_order_braid.finalized_height().min(reordered_braid.finalized_height());
            assert!(h > 0, "final_depth={final_depth}: test setup produced nothing finalized — widen ROUNDS");

            let order_a = creation_order_braid.order_hash();
            let order_b = reordered_braid.order_hash();
            let spine_a = creation_order_braid.selected_tip().expect("creation-order braid has a tip");
            let spine_b = reordered_braid.selected_tip().expect("reordered braid has a tip");

            if order_a == order_b {
                // The two arrival orders converged: every honestly-running
                // validator computes the SAME tuple regardless of which
                // network path it saw, so all n vote identically and a
                // clean, unanimous certificate must form.
                assert_eq!(spine_a, spine_b, "order_hash agreeing but selected_tip disagreeing would itself be a bug");
                let votes: Vec<FinalityVote> = keys.iter().map(|k| FinalityVote::sign(k, h, spine_a, order_a)).collect();
                let report = assemble(&committee, &votes);
                let cert = report
                    .certificate_for_height(h)
                    .unwrap_or_else(|| panic!("final_depth={final_depth}: converged validators must cleanly finalize height {h}"));
                assert_eq!(cert.votes.len(), n);
                assert!(report.conflicting_heights().is_empty());
            } else {
                // The two arrival orders genuinely diverged at this height
                // (exactly the historical bug this final_depth setting
                // guards against). Split the committee accordingly — half
                // "saw" order A, half "saw" order B. THE INVARIANT: neither
                // side may manufacture a certificate on a genuine 2-vs-2
                // split (both below quorum(4)=3), and no conflicting
                // certificate may ever register.
                let half = n / 2;
                let mut votes: Vec<FinalityVote> =
                    keys[..half].iter().map(|k| FinalityVote::sign(k, h, spine_a, order_a)).collect();
                votes.extend(keys[half..].iter().map(|k| FinalityVote::sign(k, h, spine_b, order_b)));
                let report = assemble(&committee, &votes);
                assert!(
                    report.certificate_for_height(h).is_none(),
                    "final_depth={final_depth}: a genuine split below quorum on both sides must NOT manufacture a certificate"
                );
                assert!(
                    report.conflicting_heights().is_empty(),
                    "final_depth={final_depth}: a below-quorum split must not even register as a conflicting-certificate case"
                );
            }
        }
    }
}
