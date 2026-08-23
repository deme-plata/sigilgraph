//! UPGRADE SAFETY: a snapshot written BEFORE the shielded field must still load, with
//! every other field landing in the right place.
//!
//! Snapshots use `rmp_serde::to_vec`, which encodes a struct as a POSITIONAL ARRAY. Field
//! order is load-bearing across versions: a field inserted mid-struct shifts every later
//! field by one when an old snapshot is read, so balances and supply would be silently
//! read from the wrong slots on the first restart after an upgrade — corruption with no
//! error to point at. This test is the guard against that ever being reintroduced.

use sigil_state::{commit_state_transition, SigilState, StateMutation, StateTransition, NATIVE};

const ALICE: [u8; 32] = [0xA1; 32];

/// Build a realistic state, then produce the array encoding a PRE-shielded binary would
/// have written: the same array with the trailing shielded element removed.
fn old_shaped_snapshot(state: &SigilState) -> Vec<u8> {
    let full = rmp_serde::to_vec(state).expect("encode");
    let val: rmp_serde::decode::Error;
    let _ = val;
    // Decode to a generic msgpack Value, drop the last array element, re-encode.
    let mut v: rmpv::Value = rmpv::decode::read_value(&mut &full[..]).expect("decode value");
    match &mut v {
        rmpv::Value::Array(items) => {
            assert!(!items.is_empty());
            items.pop(); // the trailing `shielded` field
        }
        other => panic!("expected a positional array, got {other:?}"),
    }
    let mut out = Vec::new();
    rmpv::encode::write_value(&mut out, &v).expect("re-encode");
    out
}

#[test]
fn a_pre_shielded_snapshot_loads_with_every_field_intact() {
    let mut state = SigilState::default();
    commit_state_transition(
        &mut state,
        &StateTransition {
            at_height: 1,
            mutations: vec![StateMutation::SetBalance {
                wallet: ALICE,
                token: NATIVE,
                amount: 12_345,
            }],
        },
        1,
    )
    .expect("seed");
    let expected_supply = state.native_supply();
    let expected_roots = state.roots();
    assert_eq!(state.balance_of(&ALICE, &NATIVE), 12_345);

    // What an older binary would have on disk.
    let old = old_shaped_snapshot(&state);

    let loaded: SigilState = rmp_serde::from_slice(&old)
        .expect("SECURITY: a pre-shielded snapshot MUST still load, or the node cannot start");

    // The decisive assertions: nothing shifted.
    assert_eq!(
        loaded.balance_of(&ALICE, &NATIVE),
        12_345,
        "a shifted field would silently corrupt balances"
    );
    assert_eq!(loaded.native_supply(), expected_supply, "supply must not shift");
    assert_eq!(loaded.roots(), expected_roots, "state roots must be identical");
    assert_eq!(loaded.shielded().len(), 0, "the pool defaults to empty");
    assert_eq!(loaded.shielded().value_locked(), 0);
}

/// The current shape round-trips exactly.
#[test]
fn current_snapshot_round_trips() {
    let mut state = SigilState::default();
    commit_state_transition(
        &mut state,
        &StateTransition {
            at_height: 1,
            mutations: vec![
                StateMutation::SetBalance { wallet: ALICE, token: NATIVE, amount: 7 },
                StateMutation::Shield { from: ALICE, amount: 7, cm: [9u8; 32] },
            ],
        },
        1,
    )
    .expect("seed");

    let bytes = rmp_serde::to_vec(&state).expect("encode");
    let back: SigilState = rmp_serde::from_slice(&bytes).expect("decode");
    assert_eq!(back.shielded().len(), 1, "the note must survive");
    assert_eq!(back.shielded().value_locked(), 7);
    assert_eq!(back.roots(), state.roots());
}

/// Guard: the shielded field must remain LAST. If someone appends a field after it, an old
/// snapshot starts misaligning again. Encoding a default state and checking the trailing
/// element is the pool is a cheap structural canary.
#[test]
fn shielded_field_is_still_last_in_the_encoding() {
    let s = SigilState::default();
    let bytes = rmp_serde::to_vec(&s).expect("encode");
    let v: rmpv::Value = rmpv::decode::read_value(&mut &bytes[..]).expect("decode");
    let items = match v {
        rmpv::Value::Array(i) => i,
        other => panic!("expected array, got {other:?}"),
    };
    // ShieldedPool encodes as its own 3-element array (notes, nullifiers, value_locked)
    // plus the anchor window and dirty flag are skipped/defaulted.
    let last = items.last().expect("non-empty");
    assert!(
        matches!(last, rmpv::Value::Array(_)),
        "the LAST encoded field must be the shielded pool (a nested array); if this fails, \
         a field was appended after it and pre-shielded snapshots will misalign — see the \
         MUST STAY LAST comment on SigilState::shielded"
    );
}
