//! chronos — a shielded payment on the braid must land ONCE, be offered to the builder
//! ONCE, and `/v1/transactions/:hash` must never say "rejected" for money that moved.
//!
//! Drives the REAL producer loop, in the order `sigil-node` runs it every tick, with no
//! layer stubbed: `dag_build_frontier` → `ShieldedBridge::snapshot_for_mint*` →
//! `mint_next_block` (the real candidate builder, real STARK re-verification at the
//! chokepoint) → `coinbase::take_rejections` → braid insert → `dag_drain_apply` (the ONE
//! settlement path). Funding is the real coinbase: `SIGIL_PRODUCER_WALLET` is Alice, so
//! her transparent balance comes from minted blocks, not a fixture mutation.
//!
//! ## What was measured live before this test existed (2026-09-05)
//!
//! Two shielded sends from one wallet: both answered `rejected: nullifier already spent`
//! within seconds, both had APPLIED (nullifiers 7→9, the payee received twice). The
//! mechanism: candidate N carries the send, its nullifier enters the frontier, candidate
//! N+1 is handed the same still-pending tx, refuses it against its own effects, and three
//! refusals evict it as rejected before N is anywhere near finality. `THE_OLD_LOOP` below
//! reproduced that until 2026-09-28; since the builder's landed-spend skip (0bf72c1c) it
//! checks that the old offer policy can no longer produce it. Run under `--profile release-fast` (the prover needs
//! `debug_assertions` off — same reason the sigil-shield prover tests are ignored in
//! debug).

mod common;
use common::*;
use sigil_api::shielded::TxOutcome;
use sigil_shield::wallet::{NoteStore, ShieldedAccount};

#[test]
#[cfg_attr(debug_assertions, ignore = "STARK prover needs debug_assertions off — run with --profile release-fast")]
fn chronos_shielded_send_lands_once_and_the_status_never_lies() {
    std::env::set_var("SIGIL_DAG_FINAL_DEPTH", FINAL_DEPTH);
    let alice_sk = ed25519_dalek::SigningKey::from_bytes(&[0x5E; 32]);
    let alice_addr = alice_sk.verifying_key().to_bytes();
    std::env::set_var("SIGIL_PRODUCER_WALLET", hex::encode(alice_addr));
    let alice = ShieldedAccount::from_seed([0x5E; 32]);
    let bob_seed = [0xB0u8; 32];
    let bob = ShieldedAccount::from_seed(bob_seed);

    // ═══════════════════ THE_OLD_LOOP — the incident cannot recur ═══════════════════
    // Until 2026-09-28 this half REPRODUCED the incident (three self-refusals → evicted as
    // rejected while the nullifier settled). Since 0bf72c1c the candidate builder skips a
    // spend whose outputs are all already held — "this exact transaction already landed" —
    // so even the old unconditional offer policy cannot produce the lie. The incident is
    // closed at two layers (offer policy 09-08, builder 09-28); this half pins the second
    // one, with the old policy still re-offering the send so the builder's skip is exercised.
    {
        let mut h = Harness::new(OfferPolicy::OldUnconditional);
        let mut store = NoteStore::new();
        let (send_hash, nf) = shield_then_send(&mut h, &alice_sk, alice_addr, &alice, &bob, &bob_seed, &mut store);
        let mut applied = false;
        for _ in 0..40 {
            h.tick();
            match h.status(&send_hash) {
                Some(TxOutcome::Rejected(r)) => panic!("OLD LOOP: a landed send was reported rejected — the 09-05 lie is back: {r}"),
                Some(TxOutcome::Applied) => { applied = true; break; }
                _ => {}
            }
        }
        assert!(applied, "the send must reach Applied under the old loop too");
        assert!(nf_spent(&h, &nf), "the nullifier is spent on the SETTLED chain");
        assert!(h.permanent_refusals(&send_hash).is_empty(), "never refused against itself: {:?}", h.permanent_refusals(&send_hash));
        assert!(
            h.offered_count(&send_hash) >= 2,
            "the old loop must still RE-OFFER the in-flight send, or the builder's skip was never exercised (offered {}x)",
            h.offered_count(&send_hash)
        );
        eprintln!(
            "chronos[old loop]: offered {}x, 0 self-refusals, applied — the 09-05 lie cannot recur even under the old offer policy (builder skip, 0bf72c1c)",
            h.offered_count(&send_hash)
        );
    }

    // ═══════════════════════════ THE FIXED LOOP ═══════════════════════════════════
    let mut h = Harness::new(OfferPolicy::Fixed);
    let mut store = NoteStore::new();
    let (send_hash, nf) = shield_then_send(&mut h, &alice_sk, alice_addr, &alice, &bob, &bob_seed, &mut store);
    let pool_before = pay_tree_len(&h);

    let mut saw_in_flight = false;
    let mut in_flight_ticks = 0u64;
    let mut settled_at: Option<u64> = None;
    for _ in 0..60 {
        h.tick();
        match h.status(&send_hash) {
            Some(TxOutcome::Pending { in_flight, permanent_fails, .. }) => {
                assert_eq!(permanent_fails, 0, "a send in flight must never be refused against itself");
                saw_in_flight |= in_flight;
                in_flight_ticks += u64::from(in_flight);
            }
            Some(TxOutcome::Applied) => {
                settled_at = Some(h.ticks);
                break;
            }
            Some(TxOutcome::Rejected(r)) => panic!("FIXED LOOP: a landed send was reported rejected: {r}"),
            None => panic!("the bridge forgot a pending send"),
        }
    }
    let settled_at = settled_at.expect("the send must reach Applied within 60 ticks");

    // What the fix must guarantee — measured, not reasoned:
    assert_eq!(h.offered_count(&send_hash), 1, "offered to the builder exactly ONCE (one STARK verification, not one per candidate)");
    assert!(h.permanent_refusals(&send_hash).is_empty(), "never refused: {:?}", h.permanent_refusals(&send_hash));
    assert!(saw_in_flight, "status reported 'in flight' while the carrying block awaited finality");
    assert!(nf_spent(&h, &nf), "the nullifier is spent on the SETTLED chain");
    assert_eq!(pay_tree_len(&h), pool_before + 2, "exactly two output notes landed, once");
    assert_eq!(h.api.shielded.pending_len(), 0, "nothing left pending after finality");
    eprintln!(
        "chronos[fixed loop]: offered 1x, in flight for {in_flight_ticks} candidate(s), applied at tick {settled_at} (final_depth={FINAL_DEPTH})"
    );

    // ═══════════ PHASE 3 FINALITY GATE — production depth, solo certificate ═══════════
    //
    // At the real final_depth (512) the depth rule alone would settle NOTHING inside
    // this test. With the gate, the producer's own certificate for each minted block
    // raises the braid's line to that block, so the shielded send settles in the SAME
    // tick it is carried: instant finality, measured rather than promised.
    std::env::set_var("SIGIL_DAG_FINAL_DEPTH", "512");
    let mut g = Harness::new(OfferPolicy::Fixed);
    g.gate = true;
    let mut store = NoteStore::new();
    let (send_hash, nf) = shield_then_send(&mut g, &alice_sk, alice_addr, &alice, &bob, &bob_seed, &mut store);
    let submitted_at = g.ticks;
    let mut applied_at: Option<u64> = None;
    for _ in 0..8 {
        g.tick();
        match g.status(&send_hash) {
            Some(TxOutcome::Applied) => { applied_at = Some(g.ticks); break; }
            Some(TxOutcome::Rejected(r)) => panic!("GATE: a landed send was reported rejected: {r}"),
            _ => {}
        }
    }
    let applied_at = applied_at.expect("with the finality gate the send must settle within a few ticks at depth 512");
    assert!(applied_at - submitted_at <= 2, "settled {} tick(s) after submission — expected ≤2 at final_depth=512", applied_at - submitted_at);
    assert!(nf_spent(&g, &nf));
    assert_eq!(g.offered_count(&send_hash), 1);
    // At final_depth=512 the depth rule alone would have settled NOTHING (h=0): every
    // settled block here is the certificate's doing.
    assert!(g.chain.height() >= 2, "the gate settled blocks the depth rule never would (h={})", g.chain.height());
    assert!(g.braid.certified_line().is_some(), "the braid's line is held by a certificate");
    eprintln!(
        "chronos[finality gate]: final_depth=512, send applied {} tick(s) after submission (settled h={}, certified line={:?})",
        applied_at - submitted_at, g.chain.height(), g.braid.certified_line().map(|c| c.0)
    );
    std::env::remove_var("SIGIL_DAG_FINAL_DEPTH");
    std::env::remove_var("SIGIL_PRODUCER_WALLET");
}
