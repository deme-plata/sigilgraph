//! DEPLOYMENT READINESS: the complete production path for a shielded payment.
//!
//! Every other test in this effort exercises one layer. This one drives the exact sequence
//! a real node performs, in order, with no layer stubbed:
//!
//! ```text
//!   ShieldedBridge::submit_*      the API handler's queue (what an HTTP request reaches)
//!        -> snapshot_for_mint()   what the producer embeds in a candidate block
//!        -> apply_tx()            transaction -> state mutations
//!        -> commit_state_transition()   the chokepoint, which VERIFIES the STARK
//!        -> confirm_applied()     retirement once the candidate lands on the spine
//! ```
//!
//! The only thing missing versus production is the HTTP layer and the P2P gossip — both
//! of which are transport, not logic. If this passes, the deploy's remaining risk is
//! operational (does the binary start, does it keep producing), not "does shielded money
//! work".
//!
//! Written before deploying rather than after, because the chokepoint is where a mistake
//! mints or destroys money and "we'll see on mainnet" is not a test strategy.

use sigil_api::shielded::ShieldedBridge;
use sigil_shield::mimc::compress2;
use sigil_shield::note_v1::{from_wire, padding_leaf, to_wire};
use sigil_shield::wallet::{build_spend, shield_note, NoteStore, ShieldedAccount};
use sigil_state::shielded::POOL_CAPACITY;
use sigil_state::{
    commit_state_transition, SigilState, StateMutation, StateTransition, NATIVE,
};
use sigil_tx::apply_tx;

const ALICE: [u8; 32] = [0xA1; 32];
const MASTER: [u8; 32] = [0x11; 32];

/// Run one producer round: drain the bridge, apply each tx, commit the batch.
/// Returns the tx hashes that landed, mirroring what the producer feeds `confirm_applied`.
fn producer_round(
    state: &mut SigilState,
    bridge: &ShieldedBridge,
    height: u64,
) -> Result<Vec<[u8; 32]>, String> {
    let batch = bridge.snapshot_for_mint();
    let mut mutations = Vec::new();
    let mut hashes = Vec::new();
    for signed in &batch {
        let res = apply_tx(state, signed).map_err(|e| format!("apply_tx: {e}"))?;
        mutations.extend(res.mutations);
        hashes.push(signed.tx.hash());
    }
    if mutations.is_empty() {
        return Ok(hashes);
    }
    commit_state_transition(state, &StateTransition { at_height: height, mutations }, height)
        .map_err(|e| format!("chokepoint: {e}"))?;
    Ok(hashes)
}

/// The pool's padded leaf view, as both the chain and a prover must see it.
fn pool_view(state: &SigilState) -> Vec<[u8; 32]> {
    state.shielded().padded_leaves(sigil_shield::note_v1::padding_leaf_wire)
}

#[test]
fn full_production_path_shield_then_private_payment() {
    let seed = [0x5Eu8; 32];
    let alice = ShieldedAccount::from_seed(seed);
    let bridge = ShieldedBridge::new();
    let mut state = SigilState::default();
    let mut store = NoteStore::new();

    // ── genesis: fund Alice transparently ───────────────────────────────────────────
    commit_state_transition(
        &mut state,
        &StateTransition {
            at_height: 1,
            mutations: vec![
                StateMutation::SetMasterWallet { wallet: MASTER },
                StateMutation::SetBalance { wallet: ALICE, token: NATIVE, amount: 100 },
            ],
        },
        1,
    )
    .expect("genesis");
    assert_eq!(state.balance_of(&ALICE, &NATIVE), 100);

    // ── 1. SHIELD: the wallet derives a note, the API queues the deposit ─────────────
    let (note_index, cm) = shield_note(&alice, &mut store, 100).expect("derive note");
    let shield_hash = bridge
        .submit_shield(&hex::encode(ALICE), 100, &hex::encode(cm), 0)
        .expect("the API must accept a well-formed shield");
    assert_eq!(bridge.pending_len(), 1);

    // ── 2. the producer mints it ────────────────────────────────────────────────────
    let landed = producer_round(&mut state, &bridge, 2).expect("shield must land");
    assert!(landed.contains(&shield_hash));
    bridge.confirm_applied(&landed);
    assert_eq!(bridge.pending_len(), 0, "a landed tx must be retired");

    assert_eq!(state.balance_of(&ALICE, &NATIVE), 0, "value left the transparent domain");
    assert_eq!(state.shielded().value_locked(), 100, "and entered the shielded one");
    assert_eq!(state.shielded().len(), 1);

    // ── 3. the wallet finds its note on chain ───────────────────────────────────────
    let pool = pool_view(&state);
    assert_eq!(store.scan_owned(&alice, &pool), 1, "wallet must locate its own note");
    assert_eq!(store.balance(), 100);

    // ── 4. build a private payment: 50 to Bob, 47 change, 3 fee ─────────────────────
    let bob = ShieldedAccount::from_seed([0xB0; 32]);
    let bundle = build_spend(
        &alice,
        &mut store,
        &pool,
        note_index,
        3,
        &[(50, bob.public_key()), (47, alice.public_key())],
    )
    .expect("wallet must build the payment");

    // The anchor the wallet proved against must be one the chain actually published —
    // if these disagree, every honest spend is rejected as unknown-anchor.
    assert!(
        state.shielded().is_known_anchor(&bundle.anchor),
        "the wallet's anchor must be a root the chain published"
    );

    // ── 5. submit through the API and mint it ───────────────────────────────────────
    let send_hash = bridge
        .submit_shielded_send(
            &hex::encode(bundle.anchor),
            &hex::encode(bundle.nullifier),
            &bundle.cm_outs.iter().map(hex::encode).collect::<Vec<_>>(),
            3,
            bundle.proof.clone(),
        )
        .expect("the API must accept a valid shielded send");

    let landed = producer_round(&mut state, &bridge, 3).expect("shielded send must land");
    assert!(landed.contains(&send_hash));
    bridge.confirm_applied(&landed);

    // ── 6. the chain's post-state ───────────────────────────────────────────────────
    assert!(
        state.shielded().is_spent(&bundle.nullifier),
        "the nullifier must be recorded, or the note is spendable twice"
    );
    assert_eq!(state.shielded().len(), 3, "input note + two outputs");
    assert_eq!(state.balance_of(&MASTER, &NATIVE), 3, "the public fee is credited");
    assert_eq!(state.shielded().value_locked(), 97, "fee left the pool, the rest stays");

    // Nothing in the committed state names Bob or reveals an amount.
    let pool_after = pool_view(&state);
    let bob_cm = from_wire(&bundle.cm_outs[0]).unwrap();
    assert!(
        pool_after.iter().any(|c| from_wire(c).ok() == Some(bob_cm)),
        "Bob's note is in the pool"
    );
    assert_ne!(
        bob_cm,
        compress2(compress2(sigil_shield::note_v1::Note::new(50, 0, 0).unwrap().value,
                            bundle.out_preimages[0].1),
                  alice.public_key()),
        "PRIVACY: the note must be bound to Bob, not to the sender"
    );
}

/// A REPLAY submitted twice must be refused, and must not corrupt state.
#[test]
fn replayed_shielded_send_is_refused_on_the_production_path() {
    let seed = [0x11u8; 32];
    let alice = ShieldedAccount::from_seed(seed);
    let bridge = ShieldedBridge::new();
    let mut state = SigilState::default();
    let mut store = NoteStore::new();

    commit_state_transition(
        &mut state,
        &StateTransition {
            at_height: 1,
            mutations: vec![
                StateMutation::SetMasterWallet { wallet: MASTER },
                StateMutation::SetBalance { wallet: ALICE, token: NATIVE, amount: 100 },
            ],
        },
        1,
    )
    .unwrap();

    let (idx, cm) = shield_note(&alice, &mut store, 100).unwrap();
    bridge.submit_shield(&hex::encode(ALICE), 100, &hex::encode(cm), 0).unwrap();
    let h = producer_round(&mut state, &bridge, 2).unwrap();
    bridge.confirm_applied(&h);

    let pool = pool_view(&state);
    store.scan_owned(&alice, &pool);
    let me = alice.public_key();
    let bundle = build_spend(&alice, &mut store, &pool, idx, 3, &[(50, me), (47, me)]).unwrap();

    let args = (
        hex::encode(bundle.anchor),
        hex::encode(bundle.nullifier),
        bundle.cm_outs.iter().map(hex::encode).collect::<Vec<_>>(),
    );
    bridge.submit_shielded_send(&args.0, &args.1, &args.2, 3, bundle.proof.clone()).unwrap();
    let h = producer_round(&mut state, &bridge, 3).unwrap();
    bridge.confirm_applied(&h);
    let locked_after_first = state.shielded().value_locked();

    // Re-submit the identical, still-valid proof.
    let resubmitted =
        bridge.submit_shielded_send(&args.0, &args.1, &args.2, 3, bundle.proof.clone());
    if resubmitted.is_ok() {
        // If the door let it through, the chokepoint MUST refuse it.
        let batch = bridge.snapshot_for_mint();
        let mut refused = false;
        for signed in &batch {
            match apply_tx(&state, signed) {
                Err(_) => refused = true,
                Ok(res) => {
                    let r = commit_state_transition(
                        &mut state,
                        &StateTransition { at_height: 4, mutations: res.mutations },
                        4,
                    );
                    if r.is_err() {
                        refused = true;
                    }
                }
            }
        }
        assert!(refused, "SECURITY: a replayed shielded send must be refused somewhere");
    }
    assert_eq!(
        state.shielded().value_locked(),
        locked_after_first,
        "SECURITY: a refused replay must not move value"
    );
    assert_eq!(state.shielded().nullifier_count(), 1, "still exactly one spend");
}

/// The pool view the chain publishes must be exactly what a prover builds against.
/// If these ever diverge, no honest spend can verify — a silent, total outage.
#[test]
fn chain_pool_view_matches_what_a_prover_builds() {
    let alice = ShieldedAccount::from_seed([7u8; 32]);
    let mut state = SigilState::default();
    let mut store = NoteStore::new();
    let (_i, cm) = shield_note(&alice, &mut store, 42).unwrap();

    commit_state_transition(
        &mut state,
        &StateTransition {
            at_height: 1,
            mutations: vec![
                StateMutation::SetBalance { wallet: ALICE, token: NATIVE, amount: 42 },
                StateMutation::Shield { from: ALICE, amount: 42, cm },
            ],
        },
        1,
    )
    .unwrap();

    let chain_view = pool_view(&state);
    let mut prover_view = vec![cm];
    for i in prover_view.len()..POOL_CAPACITY {
        prover_view.push(to_wire(padding_leaf(i as u64)));
    }
    assert_eq!(chain_view.len(), POOL_CAPACITY);
    assert_eq!(chain_view, prover_view, "chain and prover must pad identically");
    assert!(
        state.shielded().is_known_anchor(&state.shielded().current_root()),
        "the chain must publish the root it currently holds"
    );
}
