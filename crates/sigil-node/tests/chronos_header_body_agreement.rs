//! chronos gate — HEADER ↔ BODY agreement on every block the real producer mints.
//!
//! The 2026-09-10 incident (`project_sigil_shielded_event_consensus_break`): a consensus
//! change made blocks carry `SigilEvent::ShieldedSend`, the producer signed headers whose
//! `event_log_root` did NOT commit the body's events, the follower logged
//! `🔴 STATE DIVERGENCE at height 5525905 — cannot extend tip` and froze. The test suite was
//! green: it checked that the event was well-formed and never that the header committed it.
//! Rolling the binary back did not help — the bad blocks were already in the chain.
//!
//! This gate asks the three questions that would have failed that day, against blocks
//! minted by the REAL loop (`dag_build_frontier` → `mint_next_block` → `dag_drain_apply`)
//! carrying REAL shielded payments (real STARKs, real events):
//!   1. FOLLOWER: an independent `ChainTip` re-applies every body the spine applied
//!      (single-commit shape, `check_roots_match`) — harness `mirror_follower`, asserted
//!      on every tick.
//!   2. BODY → HEADER: `sigil_events::events_root(&block.events)` equals
//!      `block.header.event_log_root`, and the transition's `PushEventHash` leaves are
//!      exactly the typed events' leaf hashes, in order.
//!   3. TEETH: a corrupted body (an event dropped; a leaf altered) is REFUSED by a fresh
//!      follower at the pre-block state and disagrees with the header root — so the check
//!      is a check and not a tautology.
//! And it refuses to pass vacuously: at least two event-bearing blocks must exist, because
//! the 09-10 shape "hid for months because nothing emitted events".
//!
//! Run under `--profile release-fast` (the STARK prover needs `debug_assertions` off).

mod common;
use common::*;

use sigil_api::shielded::TxOutcome;
use sigil_events::events_root;
use sigil_node::block::Block;
use sigil_shield::wallet::{NoteStore, ShieldedAccount};
use sigil_state::StateMutation;

/// The leaves the follower's single commit will fold into `event_log_root`.
fn push_event_leaves(b: &Block) -> Vec<[u8; 32]> {
    b.transition
        .mutations
        .iter()
        .filter_map(|m| match m { StateMutation::PushEventHash(h) => Some(*h), _ => None })
        .collect()
}

#[test]
#[cfg_attr(debug_assertions, ignore = "STARK prover needs debug_assertions off — run with --profile release-fast")]
fn chronos_header_commits_exactly_the_body_it_ships_with() {
    std::env::set_var("SIGIL_DAG_FINAL_DEPTH", FINAL_DEPTH);
    let alice_sk = ed25519_dalek::SigningKey::from_bytes(&[0x5E; 32]);
    let alice_addr = alice_sk.verifying_key().to_bytes();
    std::env::set_var("SIGIL_PRODUCER_WALLET", hex::encode(alice_addr));
    let alice = ShieldedAccount::from_seed([0x5E; 32]);
    let bob_seed = [0xB0u8; 32];
    let bob = ShieldedAccount::from_seed(bob_seed);

    let mut h = Harness::new(OfferPolicy::Fixed);

    // Two real private payments, each settled before the next: two blocks with a
    // ShieldedSend event (plus the Shield deposits that funded them). Fresh store and a
    // different deposit size per round — see `shield_then_send_amount`.
    let mut settled_sends = 0;
    for (round, (shield_amt, pay_amt)) in [(1_000_000u128, 500_000u128), (2_000_000, 1_500_000)].into_iter().enumerate() {
        let mut store = NoteStore::new();
        let (send_hash, nf) = shield_then_send_amount(
            &mut h, &alice_sk, alice_addr, &alice, &bob, &bob_seed, &mut store, shield_amt, pay_amt,
        );
        tick_until(&mut h, 60, "the private payment settles", |h| {
            matches!(h.status(&send_hash), Some(TxOutcome::Applied)) && h.chain.state().shielded().is_spent(&nf)
        });
        settled_sends += 1;
        assert_eq!(
            h.applied_blocks.len() as u64, h.chain.height() - h.genesis_height,
            "round {round}: every applied block was mirrored"
        );
    }
    assert_eq!(settled_sends, 2);
    // Let the sends finalize and a few empty blocks follow, so the gate also sees blocks
    // with NO events (their root must be all-zero on both sides).
    for _ in 0..6 { h.tick(); }

    // ── 1. FOLLOWER: already asserted by `mirror_follower` on every tick. Restate the receipt.
    assert_eq!(h.follower.height(), h.chain.height());
    assert_eq!(h.follower.parent_hash(), h.chain.parent_hash(), "follower sits on the producer's tip");
    assert_eq!(h.follower.state().roots(), h.chain.state().roots(), "follower and producer roots agree");

    // ── 2. BODY → HEADER on every applied block.
    let mut with_events: Vec<Block> = Vec::new();
    for b in &h.applied_blocks {
        let leaves = push_event_leaves(b);
        let typed: Vec<[u8; 32]> = b.events.iter().map(|e| e.leaf_hash()).collect();
        assert_eq!(leaves, typed, "h={}: transition leaves must be the typed events' hashes, in order", b.header.height);
        assert_eq!(
            events_root(&b.events), b.header.event_log_root,
            "h={}: the body's {} event(s) must hash to the header's event_log_root", b.header.height, b.events.len()
        );
        if b.events.is_empty() {
            assert_eq!(b.header.event_log_root, [0u8; 32], "h={}: no events ⇒ all-zero root", b.header.height);
        } else {
            with_events.push(b.clone());
        }
    }
    assert!(
        with_events.len() >= 2,
        "gate would pass VACUOUSLY: only {} event-bearing block(s) out of {} — the 09-10 shape hid because nothing emitted events",
        with_events.len(), h.applied_blocks.len()
    );
    let kinds: std::collections::BTreeSet<String> =
        with_events.iter().flat_map(|b| b.events.iter().map(|e| format!("{:?}", e.tag()))).collect();
    eprintln!(
        "chronos[header↔body]: {} blocks mirrored to the follower, {} carry events ({:?}), all roots agree",
        h.applied_blocks.len(), with_events.len(), kinds
    );

    // ── 3. TEETH: the same checks must FAIL on a corrupted body.
    // Rebuild a follower at the state just before the first event-bearing block.
    let target = &with_events[0];
    let mut pre = sigil_node::chain::ChainTip::new();
    pre.apply(sigil_node::genesis::build_genesis().expect("genesis")).expect("apply genesis");
    for b in &h.applied_blocks {
        if b.header.height >= target.header.height { break; }
        pre.apply(b.clone()).expect("replay prefix");
    }
    assert_eq!(pre.height(), target.header.height, "`height()` is the next height to apply (genesis is header height 0)");

    // (a) an event dropped from the body: a light client comparing body to header catches it.
    let mut dropped = target.clone();
    dropped.events.pop();
    assert_ne!(events_root(&dropped.events), dropped.header.event_log_root, "a body missing an event must not match the header");

    // (b) a leaf altered in the transition: the follower's own commit must refuse the block.
    let mut altered = target.clone();
    let pos = altered.transition.mutations.iter().position(|m| matches!(m, StateMutation::PushEventHash(_))).expect("an event leaf");
    if let StateMutation::PushEventHash(leaf) = &mut altered.transition.mutations[pos] { leaf[0] ^= 0x01; }
    let err = pre.clone().apply(altered).expect_err("a follower must refuse a block whose transition does not hash to its header");
    assert!(err.to_string().contains("STATE DIVERGENCE"), "refusal names the divergence: {err}");

    // (c) and the untouched block is still accepted by that same pre-state.
    pre.apply(target.clone()).expect("the genuine block is accepted");

    std::env::remove_var("SIGIL_DAG_FINAL_DEPTH");
}
