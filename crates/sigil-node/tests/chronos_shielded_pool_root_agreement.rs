//! chronos gate — THE FIFTH ROOT: the header commits the shielded pool from its activation
//! height, and producer ≡ follower on it under real shielded traffic.
//!
//! 2026-09-25 (`project_sigil_docker_follower_epsilon_four_roots_2026_09_25`): a docker
//! follower re-derived the whole chain and agreed with the producer on all four header
//! roots — and it was found that the SHIELDED POOL, where every coin has lived since
//! transparent sends were retired, had no root in the header at all. Two nodes could hold
//! different notes and nullifiers and sign identical headers. `ShieldedPool::digest` had
//! been written for exactly this and never got a caller.
//!
//! This gate lowers `sigil_header::SHIELDED_POOL_ROOT_ACTIVATION_HEIGHT` to a height the
//! REAL loop (`dag_build_frontier` → `mint_next_block` → `dag_drain_apply`) crosses while
//! carrying REAL shielded payments (real STARKs), then asks:
//!   1. GATE: every applied header carries `shielded_pool_root` exactly when the gate says
//!      so — `None` below, `Some` from the activation height.
//!   2. FOLLOWER: an independent `ChainTip` re-applies every body (`ChainTip::apply`, which
//!      now refuses a block whose pool root differs from its own) and ends on the
//!      producer's tip with the identical pool root.
//!   3. NOT VACUOUS: the committed root takes at least three distinct values across the
//!      gated blocks — a deposit (note +1) and a private payment (nullifier +1) each move it.
//!   4. TEETH: a header whose pool root is flipped is REFUSED by a fresh follower at the
//!      pre-block state, naming the shielded pool; a body missing its spend no longer
//!      re-derives the header's root.
//!
//! Run under `--profile release-fast` (the STARK prover needs `debug_assertions` off).
mod common;
use common::*;
use sigil_api::shielded::TxOutcome;
use sigil_header::{override_shielded_pool_root_activation_for_simulation, shielded_pool_root_required};
use sigil_shield::wallet::{NoteStore, ShieldedAccount};
use sigil_state::StateMutation;
use std::collections::BTreeSet;

/// Genesis is height 0; the harness funds Alice from the coinbase over the first few blocks,
/// so a gate at 4 puts the deposits AND the payments on the gated side.
const GATE: u64 = 4;

#[test]
#[cfg_attr(debug_assertions, ignore = "STARK prover needs debug_assertions off — run with --profile release-fast")]
fn chronos_header_commits_the_shielded_pool_from_the_gate() {
    // sigil-g3 commits the pool root from genesis: lowering the gate on a g3 build would
    // simulate a configuration no g3 node runs, so g3 keeps its compiled-in height 0.
    let gate = if sigil_header::IS_G3 {
        assert_eq!(sigil_header::SHIELDED_POOL_ROOT_ACTIVATION_HEIGHT, 0, "sigil-g3: the pool root is committed from genesis");
        0
    } else {
        override_shielded_pool_root_activation_for_simulation(GATE);
        GATE
    };
    std::env::set_var("SIGIL_DAG_FINAL_DEPTH", FINAL_DEPTH);
    let alice_sk = ed25519_dalek::SigningKey::from_bytes(&[0x5E; 32]);
    let alice_addr = alice_sk.verifying_key().to_bytes();
    std::env::set_var("SIGIL_PRODUCER_WALLET", hex::encode(alice_addr));
    let alice = ShieldedAccount::from_seed([0x5E; 32]);
    let bob_seed = [0xB0u8; 32];
    let bob = ShieldedAccount::from_seed(bob_seed);

    let mut h = Harness::new(OfferPolicy::Fixed);

    // Two real private payments (each preceded by its real deposit), each settled before
    // the next — every one of them moves the pool root under the gate.
    for (round, (shield_amt, pay_amt)) in [(1_000_000u128, 500_000u128), (2_000_000, 1_500_000)].into_iter().enumerate() {
        let mut store = NoteStore::new();
        let (send_hash, nf) = shield_then_send_amount(
            &mut h, &alice_sk, alice_addr, &alice, &bob, &bob_seed, &mut store, shield_amt, pay_amt,
        );
        tick_until(&mut h, 60, "the private payment settles", |h| {
            matches!(h.status(&send_hash), Some(TxOutcome::Applied)) && nf_spent(h, &nf)
        });
        assert_eq!(
            h.applied_blocks.len() as u64, h.chain.height() - h.genesis_height,
            "round {round}: every applied block was mirrored (the follower would have PANICKED on a pool-root refusal)"
        );
    }
    for _ in 0..6 { h.tick(); }

    // ── 2. FOLLOWER ≡ PRODUCER, fifth root included ──────────────────────────────
    assert_eq!(h.follower.height(), h.chain.height());
    assert_eq!(h.follower.parent_hash(), h.chain.parent_hash(), "follower sits on the producer's tip");
    assert_eq!(h.follower.state().roots(), h.chain.state().roots(), "four roots agree");
    assert_eq!(
        h.follower.state().shielded_pool_root(), h.chain.state().shielded_pool_root(),
        "the fifth root agrees"
    );
    assert!(h.chain.height() > gate + 2, "the chain must have crossed the gate: height {}", h.chain.height());

    // ── 1. GATE + 3. NOT VACUOUS: replay every block through a third ChainTip ─────
    let mut replay = sigil_node::chain::ChainTip::new();
    replay.apply(sigil_node::genesis::build_genesis().expect("genesis")).expect("apply genesis");
    let mut roots_seen: BTreeSet<[u8; 32]> = BTreeSet::new();
    let (mut below, mut gated) = (0usize, 0usize);
    for b in &h.applied_blocks {
        let hgt = b.header.height;
        assert_eq!(
            b.header.shielded_pool_root.is_some(), shielded_pool_root_required(hgt),
            "h={hgt}: shielded_pool_root present ⇔ height ≥ gate {gate}"
        );
        replay.apply(b.clone()).unwrap_or_else(|e| panic!("replay refused the producer's block at {hgt}: {e}"));
        match b.header.shielded_pool_root {
            Some(r) => {
                assert_eq!(r, replay.state().shielded_pool_root(), "h={hgt}: header root == re-derived pool root");
                roots_seen.insert(r);
                gated += 1;
            }
            None => below += 1,
        }
    }
    if sigil_header::IS_G3 {
        assert_eq!(below, 0, "sigil-g3: every block commits the pool root");
    } else {
        assert!(below >= 1, "no block below the gate — the pre-gate `None` path was never exercised");
    }
    assert!(gated >= 2, "fewer than two gated blocks");
    assert!(
        roots_seen.len() >= 3,
        "gate would pass VACUOUSLY: the pool root took only {} distinct value(s) over {} gated blocks — \
         a deposit and a private payment must each move it",
        roots_seen.len(), gated
    );
    eprintln!(
        "chronos[fifth root]: {} blocks below the gate, {} gated, pool root took {} distinct values, follower ≡ producer",
        below, gated, roots_seen.len()
    );

    // ── 4. TEETH ───────────────────────────────────────────────────────────────
    let target = h.applied_blocks.iter()
        .find(|b| b.header.shielded_pool_root.is_some()
            && b.transition.mutations.iter().any(|m| matches!(m, StateMutation::ShieldedSpend { .. })))
        .expect("a gated block carrying a real ShieldedSpend");
    let mut pre = sigil_node::chain::ChainTip::new();
    pre.apply(sigil_node::genesis::build_genesis().expect("genesis")).expect("apply genesis");
    for b in &h.applied_blocks {
        if b.header.height >= target.header.height { break; }
        pre.apply(b.clone()).expect("replay prefix");
    }
    assert_eq!(pre.height(), target.header.height);

    // (a) flipped header root → refused, and the refusal names the shielded pool
    let mut flipped = target.clone();
    let mut r = flipped.header.shielded_pool_root.expect("gated");
    r[0] ^= 0x01;
    flipped.header.shielded_pool_root = Some(r);
    let err = pre.clone().apply(flipped).expect_err("a follower must refuse a header whose pool root is not its own");
    assert!(err.to_string().contains("STATE DIVERGENCE"), "{err}");
    assert!(err.to_string().contains("shielded pool root"), "the refusal names the fifth root: {err}");

    // (b) the body without its spend does not re-derive the header's pool root
    let mut dropped = target.clone();
    let pos = dropped.transition.mutations.iter()
        .position(|m| matches!(m, StateMutation::ShieldedSpend { .. })).expect("the spend");
    dropped.transition.mutations.remove(pos);
    let mut scratch = pre.state().clone();
    // whatever else the body does, the pool root of "body minus its spend" ≠ the header's
    let _ = sigil_state::commit_state_transition(&mut scratch, &dropped.transition, dropped.header.height);
    assert_ne!(
        Some(scratch.shielded_pool_root()), dropped.header.shielded_pool_root,
        "a body missing its spend must not re-derive the header's pool root"
    );
    let err = pre.clone().apply(dropped).expect_err("a follower must refuse a body that does not hash to its header");
    assert!(err.to_string().contains("STATE DIVERGENCE"), "{err}");

    // (c) the genuine block is accepted
    pre.apply(target.clone()).expect("the genuine block is accepted");

    override_shielded_pool_root_activation_for_simulation(u64::MAX);
    std::env::remove_var("SIGIL_DAG_FINAL_DEPTH");
}
