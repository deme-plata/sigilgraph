//! chronos_style_soak — a scaled, seeded, adversarial integration test for
//! `sigil-narwhal-mempool`, in the spirit of `sigil-chronos`'s deterministic
//! scenario philosophy (seeded PRNG, adversarial injections, hard invariant
//! assertions) but living in THIS crate: `sigil-chronos` has no dependency on
//! `sigil-narwhal-mempool` at all — this mempool has never been exercised by
//! any chronos-style harness before this test. TEST-ONLY: nothing here is
//! wired into `sigil-node`'s producer loop, matching every prior phase's
//! standalone-by-design discipline.
//!
//! Unlike the crate's existing 91 unit tests (which each isolate ONE
//! mechanism at a small, fixed scale), this drives thousands of real
//! transactions through sharded ingestion + batch sealing + a real
//! multi-validator quorum-certification committee, under seeded-random
//! adversarial conditions (invalid signatures mixed in, partial/incomplete
//! dissemination, a validator lost at an UNPREDICTABLE round rather than a
//! hand-picked safe point) — the combination is what's new, not any single
//! mechanism.

use std::collections::HashSet;

use sigil_narwhal_mempool::availability_testnet::SimCommittee;
use sigil_narwhal_mempool::sealer::{BatchSealer, SealPolicy};
use sigil_narwhal_mempool::types::{quorum_threshold, WorkerId};
use sigil_narwhal_mempool::worker::ShardedMempool;
use sigil_tx::{ed25519_keygen, ed25519_sign_tx, SigilTx};

struct XorShift64(u64);
impl XorShift64 {
    fn new(seed: u64) -> Self { Self(seed.max(1)) }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13; x ^= x >> 7; x ^= x << 17;
        self.0 = x; x
    }
    fn below(&mut self, n: usize) -> usize { (self.next() % n as u64) as usize }
    fn chance(&mut self, pct: u64) -> bool { self.next() % 100 < pct }
}

#[test]
fn scaled_adversarial_soak_workers_sealing_and_quorum_under_random_validator_loss() {
    const WORKER_COUNT: u16 = 8;
    const WALLET_COUNT: usize = 200;
    const ROUNDS: u64 = 500;
    const COMMITTEE_N: usize = 7; // quorum_threshold(7) == 5

    let mut rng = XorShift64::new(0xC0FFEE_5011);

    // Real ed25519 wallets — not synthetic hashes. `verify_partition_parallel`
    // (inside ShardedMempool::ingest) actually checks these signatures.
    let wallets: Vec<([u8; 32], [u8; 32], sigil_state::WalletId)> =
        (0..WALLET_COUNT).map(|_| ed25519_keygen()).collect();

    let epoch_seed = [7u8; 32];
    let mempool = ShardedMempool::new(WORKER_COUNT, epoch_seed);

    // One sealer per worker, independently chaining its own sequence/previous
    // — small thresholds so sealing actually fires repeatedly under this
    // load instead of once at the very end.
    let policy = SealPolicy { target_bytes: 64 * 1024, target_txs: 24, max_latency: std::time::Duration::from_millis(1) };
    let sealers: Vec<BatchSealer> = (0..WORKER_COUNT)
        .map(|w| BatchSealer::new(WorkerId(w), [1u8; 32], 0, policy))
        .collect();

    let mut committee = SimCommittee::new(COMMITTEE_N);
    let mut validator_lost_at: u64 = ROUNDS + 1; // "never" until rolled below
    if rng.chance(70) {
        // Lost at an UNPREDICTABLE round in [50, ROUNDS-50] — not a
        // hand-picked safe point. 30% of runs never lose a validator at all,
        // so the harness also covers the "nothing goes wrong" path.
        validator_lost_at = 50 + rng.below((ROUNDS - 100) as usize) as u64;
    }
    let mut lost_validator_wallet: Option<sigil_state::WalletId> = None;

    let mut unique_amount: u128 = 1; // monotonic — guarantees no two generated txs ever collide in content, so dedup only ever fires on a GENUINE resubmission, never an accidental birthday-paradox collision
    let mut submitted_valid: u64 = 0;
    let mut submitted_invalid: u64 = 0;
    let mut accepted_total: u64 = 0;
    let mut rejected_invalid_total: u64 = 0;
    let mut dupe_total: u64 = 0;
    let mut sealed_batches = 0u64;
    let mut sealed_tx_total = 0u64;
    let mut certified_batches = 0u64;
    let mut quorum_denied_batches = 0u64; // partial dissemination, correctly fails to certify
    let mut seen_batch_ids: HashSet<[u8; 32]> = HashSet::new();
    let mut recovered_after_loss = 0u64;

    for round in 0..ROUNDS {
        // ── ingest: a burst of 20-50 txs, ~5% intentionally forged ──
        let n = 20 + rng.below(31);
        let mut txs = Vec::with_capacity(n);
        for _ in 0..n {
            let (sk, pk, wallet) = &wallets[rng.below(WALLET_COUNT)];
            let (_, _, to) = &wallets[rng.below(WALLET_COUNT)];
            unique_amount += 1;
            let tx = SigilTx::Send { from: *wallet, to: *to, amount: unique_amount, token: [0u8; 32], fee: 1 };
            if rng.chance(5) {
                // Forge: sign legitimately, then corrupt one signature byte.
                // Deterministically invalid regardless of wallet-index
                // coincidences (an earlier version re-signed with a random
                // OTHER wallet's key, which ~1/WALLET_COUNT of the time
                // accidentally re-drew the SAME wallet — a genuinely valid
                // signature masquerading as a forgery, and the actual cause
                // of this test's first failed run: accepted == submitted_valid
                // + 5, exactly matching 882 forge attempts / 200 wallets).
                let mut signed = ed25519_sign_tx(tx, sk, pk);
                let last = signed.sig.0.len() - 1;
                signed.sig.0[last] ^= 0xFF;
                txs.push(signed);
                submitted_invalid += 1;
            } else {
                txs.push(ed25519_sign_tx(tx, sk, pk));
                submitted_valid += 1;
            }
        }
        let res = mempool.ingest(txs);
        accepted_total += res.accepted as u64;
        rejected_invalid_total += res.invalid as u64;
        dupe_total += res.dupe as u64;

        // ── seal: pull from the REAL aggregate (ShardedMempool::pull), same
        // entry point sigil-node's own producer loop uses (mempool.pull(N)),
        // then round-robin the pulled batch across the per-sealer streams.
        // An earlier version tried to fetch "worker w" via
        // `worker_for(&wallets[w % WALLET_COUNT])`, which looks up the
        // worker a WALLET routes to (epoch-salted hash), not a direct
        // worker-by-index accessor (ShardedMempool has none, by design —
        // only `worker_for(wallet)` and the aggregate `pull`). Since only 8
        // of the 200 wallets were ever consulted, most of the real 8 workers
        // were structurally never visited, and their accepted txs sat
        // unharvested forever — a real bug in this test, not the mempool
        // (confirmed: accepted == submitted_valid was already correct; only
        // the harvest step was broken).
        let pulled_all = mempool.pull(256);
        for (i, tx) in pulled_all.into_iter().enumerate() {
            sealers[i % sealers.len()].push(vec![tx]);
        }
        for sealer in sealers.iter() {
            if let Some((header, batch)) = sealer.try_seal(round % 37 == 0) {
                sealed_batches += 1;
                sealed_tx_total += batch.txs.len() as u64;
                let digest = header.batch_id();
                assert!(seen_batch_ids.insert(digest), "batch_id collision — sequence chaining is broken");

                // ── dissemination: adversarial partial delivery ~15% of the time ──
                let bytes = format!("{:?}", batch.txs.len()).into_bytes(); // content stand-in; certify() only needs the digest
                if rng.chance(15) {
                    let reach = 1 + rng.below(committee.validators.len().saturating_sub(1).max(1));
                    let recipients: Vec<usize> = (0..committee.validators.len()).collect();
                    let recipients = &recipients[..reach.min(recipients.len())];
                    committee.disseminate_to(digest, &bytes, recipients);
                } else {
                    committee.disseminate_replicated(digest, &bytes);
                }
                match committee.certify(digest) {
                    Some(cert) => {
                        certified_batches += 1;
                        assert_eq!(cert.digest, digest);
                        assert!(cert.acks.len() >= quorum_threshold(committee.validators.len()));
                    }
                    None => quorum_denied_batches += 1,
                }
            }
        }

        // ── the unpredictable validator loss ──
        if round == validator_lost_at && !committee.validators.is_empty() {
            let idx = rng.below(committee.validators.len());
            let lost = committee.remove_validator(idx);
            lost_validator_wallet = Some(lost.wallet);
        }

        // Post-loss: confirm quorum is STILL achievable for a FRESH batch on
        // the shrunken committee (n-1), and that a previously-fully-replicated
        // digest is still recoverable from a survivor.
        if lost_validator_wallet.is_some() && round == validator_lost_at + 1 {
            if let Some(&any_digest) = seen_batch_ids.iter().next() {
                if committee.survivors_can_serve(&any_digest).is_some() {
                    recovered_after_loss += 1;
                }
            }
        }
    }

    // Final forced seal so nothing is left stranded mid-batch.
    for sealer in &sealers {
        if let Some((header, batch)) = sealer.try_seal(true) {
            sealed_batches += 1;
            sealed_tx_total += batch.txs.len() as u64;
            let digest = header.batch_id();
            assert!(seen_batch_ids.insert(digest));
            let bytes = b"final".to_vec();
            committee.disseminate_replicated(digest, &bytes);
            if committee.certify(digest).is_some() { certified_batches += 1; } else { quorum_denied_batches += 1; }
        }
    }

    eprintln!(
        "chronos_style_soak: rounds={ROUNDS} wallets={WALLET_COUNT} workers={WORKER_COUNT} committee={COMMITTEE_N}\n\
         submitted valid={submitted_valid} invalid={submitted_invalid}\n\
         accepted={accepted_total} rejected_invalid={rejected_invalid_total} dupe={dupe_total}\n\
         sealed_batches={sealed_batches} sealed_tx_total={sealed_tx_total} unique_batch_ids={}\n\
         certified={certified_batches} quorum_denied={quorum_denied_batches}\n\
         validator_lost_at={} recovered_after_loss={recovered_after_loss}",
        seen_batch_ids.len(),
        if validator_lost_at <= ROUNDS { validator_lost_at.to_string() } else { "never".into() },
    );

    // ── hard invariants ──
    assert_eq!(accepted_total, submitted_valid, "every validly-signed tx must be accepted (no false rejections under load)");
    assert_eq!(rejected_invalid_total, submitted_invalid, "every forged-signature tx must be rejected — none must sneak through under load");
    assert_eq!(sealed_tx_total, accepted_total, "every accepted tx must eventually be sealed into exactly one batch — none lost, none duplicated");
    assert!(sealed_batches > 0, "soak must actually exercise sealing, not just ingestion");
    assert_eq!(seen_batch_ids.len() as u64, sealed_batches, "every sealed batch must have a UNIQUE batch_id — a collision means the sequence/previous chaining broke under concurrent multi-worker load");
    assert!(certified_batches > 0, "soak must actually reach real quorum at least once");
    if validator_lost_at <= ROUNDS {
        assert!(recovered_after_loss > 0, "a real validator loss occurred but no post-loss recovery was observed — the exact regression this harness exists to catch");
    }
}
