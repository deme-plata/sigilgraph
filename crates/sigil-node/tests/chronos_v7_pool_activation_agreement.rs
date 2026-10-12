//! chronos gate — V7 ACTIVATION: at `sigil_header::V7_POOL_ACTIVATION_HEIGHT` the shielded
//! pool opens its 256-bit generation BESIDE the 64-bit one, and producer ≡ follower on the
//! fifth root across that block under real shielded traffic.
//!
//! What it asks (all on the REAL loop: `dag_build_frontier` → `mint_next_block` →
//! `dag_drain_apply`, real STARKs, an independent follower `ChainTip`):
//!   1. BEFORE: a real v1 deposit + private payment settle below the gate, and a second v1
//!      note is deposited and left unspent.
//!   2. ACTIVATION: the first block at/after the gate opens v7 (epoch+1) with v1 still live,
//!      sealing nothing; every node that commits that block does the same.
//!   3. NO CONFISCATION: the note deposited before the gate is spent AFTER it, with a v1 proof
//!      against the still-live v1 tree, and settles (its outputs land in the v1 tree).
//!   4. NO NEW v1 VALUE: a v1 deposit after the gate is refused at the chokepoint — the API
//!      door (generation-unaware until M5) queues it, the builder refuses it, the pool and
//!      the locked value are unchanged.
//!   5. FOLLOWER ≡ PRODUCER on all five roots; a third ChainTip replays every block and
//!      re-derives every committed pool root, with v7 active exactly from the gate.
//!   6. TEETH: the activation block with a flipped pool root is refused by a fresh follower.
//!   7. v7 END TO END (M5): a 256-bit deposit with its v7 opening goes through the real door
//!      and lands in the v7 tree; a real v7 STARK spend against the pool's own v7 anchor goes
//!      through the door (routed by the anchor's generation) and settles — nullifier spent in
//!      epoch 1, both outputs in the v7 tree, fee out of v7's own value. No wallet involved.
//!
//! Run under `--profile release-fast` (the STARK prover needs `debug_assertions` off).
mod common;
use common::*;
use sigil_api::shielded::TxOutcome;
use sigil_header::{
    override_shielded_pool_root_activation_for_simulation, override_v7_pool_activation_for_simulation,
    v7_pool_active,
};
use sigil_shield::wallet::{build_spend, shield_note, NoteStore, ShieldedAccount};
use sigil_state::shielded::SHIELDED_FEE;
use sigil_state::NATIVE;
use std::collections::BTreeSet;

const ROOT_GATE: u64 = 4;

/// Deposit `amount` for Alice through the real door; returns (note index, cm, tx hash).
fn deposit(h: &mut Harness, sk: &ed25519_dalek::SigningKey, addr: [u8; 32], alice: &ShieldedAccount, store: &mut NoteStore, amount: u128) -> (u64, [u8; 32], [u8; 32]) {
    tick_until(h, 40, "Alice funded by coinbase", |h| h.chain.state().balance_of(&addr, &NATIVE) >= amount);
    let (note_index, cm) = shield_note(alice, store, amount as u64).expect("derive note");
    let nonce = h.ticks;
    let sig = sign_shield(sk, &hex::encode(addr), amount, &hex::encode(cm), 0, nonce);
    let hash = h.api.shielded.submit_shield(&hex::encode(addr), amount, &hex::encode(cm), 0, &sig, nonce).expect("queue shield");
    (note_index, cm, hash)
}

/// Spend Alice's note `note_index` (a v1 note in the live v1 tree) to Bob, through the real door.
fn send_existing(h: &mut Harness, alice: &ShieldedAccount, bob: &ShieldedAccount, bob_seed: &[u8; 32], store: &mut NoteStore, note_index: u64, pay_amt: u64, change: u64) -> ([u8; 32], [u8; 32]) {
    let pool = h.pool_view();
    // (scan_owned reports NEWLY found notes; the kept note was already located, so the count
    // is not the check here — its pool position is.)
    let _ = store.scan_owned(alice, &pool);
    assert!(store.notes[note_index as usize].position.is_some(), "the note to spend has a pool position");
    let bundle = build_spend(alice, store, &pool, note_index as usize, SHIELDED_FEE as u64, &[(pay_amt, bob.public_key()), (change, alice.public_key())]).expect("build the payment");
    assert!(h.chain.state().shielded().is_known_anchor(&bundle.anchor));
    let (bv, bb) = bundle.out_preimages[0];
    let bob_ct = sigil_shield::note_cipher::seal_note(&sigil_shield::note_cipher::NotePlaintext::new(bv, bb), &bob.address(bob_seed)).expect("seal to bob");
    let hash = h.api.shielded.submit_shielded_send(
        &hex::encode(bundle.anchor), &hex::encode(bundle.nullifier), &[],
        &bundle.cm_outs.iter().map(hex::encode).collect::<Vec<_>>(),
        SHIELDED_FEE, bundle.proof.clone(), &[Some(bob_ct.0.clone()), None],
    ).expect("queue the shielded send");
    (hash, bundle.nullifier)
}

#[test]
#[cfg_attr(debug_assertions, ignore = "STARK prover needs debug_assertions off — run with --profile release-fast")]
fn chronos_v7_opens_beside_v1_and_producer_equals_follower_across_the_gate() {
    override_shielded_pool_root_activation_for_simulation(ROOT_GATE);
    override_v7_pool_activation_for_simulation(u64::MAX - 1); // set for real once round 1 has settled
    std::env::set_var("SIGIL_DAG_FINAL_DEPTH", FINAL_DEPTH);
    let alice_sk = ed25519_dalek::SigningKey::from_bytes(&[0x5E; 32]);
    let alice_addr = alice_sk.verifying_key().to_bytes();
    std::env::set_var("SIGIL_PRODUCER_WALLET", hex::encode(alice_addr));
    let alice = ShieldedAccount::from_seed([0x5E; 32]);
    let bob_seed = [0xB0u8; 32];
    let bob = ShieldedAccount::from_seed(bob_seed);
    let mut h = Harness::new(OfferPolicy::Fixed);

    // ── 1. BEFORE the gate: one full round, plus a second note left unspent ──────
    let mut store = NoteStore::new();
    let (send_hash, nf) = shield_then_send_amount(&mut h, &alice_sk, alice_addr, &alice, &bob, &bob_seed, &mut store, 1_000_000, 500_000);
    tick_until(&mut h, 60, "round 1 settles", |h| matches!(h.status(&send_hash), Some(TxOutcome::Applied)) && h.chain.state().shielded().is_spent(&nf));
    let keep_amt = 2_000_000u128;
    let (_, keep_cm, keep_hash) = deposit(&mut h, &alice_sk, alice_addr, &alice, &mut store, keep_amt);
    tick_until(&mut h, 40, "the kept note is deposited", |h| matches!(h.status(&keep_hash), Some(TxOutcome::Applied)));
    assert!(h.chain.state().shielded().has_ever_held(&keep_cm));
    // the store slot holding the kept note (round 1 left a change note in the store too)
    let pool_now = h.pool_view();
    store.scan_owned(&alice, &pool_now);
    let keep_index = store.notes.iter().position(|n| n.value == keep_amt as u64).expect("the kept note is in the store") as u64;
    assert!(!h.chain.state().shielded().v7_active(), "nothing activated yet");
    let v1_notes_before = h.chain.state().shielded().len();
    let v1_locked_before = h.chain.state().shielded().value_locked_in(sigil_state::shielded::PoolGeneration::V1);

    // ── 2. ACTIVATION a few blocks ahead ─────────────────────────────────────────
    let gate = h.chain.height() + 3;
    override_v7_pool_activation_for_simulation(gate);
    tick_until(&mut h, 40, "the chain crosses the v7 gate", |h| h.chain.height() > gate + 1);
    let pool = h.chain.state().shielded();
    assert!(pool.v7_active() && pool.v1_live(), "v7 opened beside a live v1");
    assert_eq!(pool.epoch(), 0);
    assert_eq!(pool.v7().unwrap().epoch(), 1);
    assert!(pool.archive().is_empty(), "nothing sealed at activation");
    assert_eq!(pool.len(), v1_notes_before, "activation moves no v1 note");
    assert_eq!(pool.value_locked_in(sigil_state::shielded::PoolGeneration::V1), v1_locked_before);
    assert_eq!(pool.value_locked_in(sigil_state::shielded::PoolGeneration::V7), 0);

    // ── 4. NO NEW v1 VALUE: a v1 deposit after the gate is refused at the chokepoint ──
    let (_, late_cm, late_hash) = deposit(&mut h, &alice_sk, alice_addr, &alice, &mut store, 1_000_000);
    tick_until(&mut h, 60, "the late v1 deposit is refused", |h| matches!(h.status(&late_hash), Some(TxOutcome::Rejected(_))));
    let reason = h.permanent_refusals(&late_hash).join(" | ");
    assert!(reason.contains("v1") && reason.contains("closed"), "the refusal names the closed v1 deposits: {reason}");
    assert!(!h.chain.state().shielded().has_ever_held(&late_cm), "the late deposit never entered any tree");
    assert_eq!(h.chain.state().shielded().len(), v1_notes_before);
    assert_eq!(h.chain.state().shielded().value_locked_in(sigil_state::shielded::PoolGeneration::V1), v1_locked_before, "no new value in the v1 epoch");

    // ── 3. NO CONFISCATION: the note deposited BEFORE the gate is spent AFTER it ──
    let change = (keep_amt - 700_000 - SHIELDED_FEE) as u64;
    let (late_send, late_nf) = send_existing(&mut h, &alice, &bob, &bob_seed, &mut store, keep_index, 700_000, change);
    tick_until(&mut h, 60, "the v1 note spends after activation", |h| matches!(h.status(&late_send), Some(TxOutcome::Applied)) && h.chain.state().shielded().is_spent(&late_nf));
    assert_eq!(h.chain.state().shielded().len(), v1_notes_before + 2, "both outputs landed in the live v1 tree");
    assert_eq!(h.chain.state().shielded().v7().unwrap().len(), 0, "and nothing in v7");
    for _ in 0..6 { h.tick(); }

    // ── 7. v7 END TO END: deposit with a v7 opening, then a real v7 spend ─────────
    {
        use sigil_shield::note_v7::{padding_leaf, Digest4, NoteV7, PoolGeneration, PoolTreeV7};
        use sigil_shield::spend_full_v7::{build_spend_full_v7_trace_seeded_u64, v7_options, OutputV7, SpendFullV7Prover};
        use sigil_shield::zk_mask::HidingProver;
        use sigil_state::shielded::POOL_CAPACITY;
        // the door reads generations from the settled pool, as sigil-node wires it at startup
        h.api.shielded.set_pool_view(std::sync::Arc::clone(&h.api.state));

        let amount = 10_000_000u128; // a denomination
        tick_until(&mut h, 40, "Alice funded for the v7 deposit", |h| h.chain.state().balance_of(&alice_addr, &NATIVE) >= amount);
        let note = NoteV7::new(amount as u64, [0x1111, 0x2222, 0x3333, 0x4444], [0xA1, 0xA2, 0xA3, 0xA4]).unwrap();
        let cm = note.commitment().to_wire();
        let blinding = Digest4(note.blinding).to_wire();
        let pk = note.owner_pk().to_wire();
        let nonce = h.ticks + 1_000;
        let sig = sign_shield(&alice_sk, &hex::encode(alice_addr), amount, &hex::encode(cm), 0, nonce);
        let v7_before = h.chain.state().shielded().v7().unwrap().len();
        let dep = h.api.shielded.submit_shield_opened(&hex::encode(alice_addr), amount, &hex::encode(cm), 0, &sig, nonce, None, Some(&hex::encode(blinding)), Some(&hex::encode(pk)))
            .expect("the door takes a v7 deposit with its opening");
        tick_until(&mut h, 40, "the v7 deposit lands", |h| matches!(h.status(&dep), Some(TxOutcome::Applied)));
        let pool = h.chain.state().shielded();
        assert_eq!(pool.v7().unwrap().len(), v7_before + 1, "the deposit is in the v7 tree");
        assert_eq!(pool.value_locked_in(PoolGeneration::V7), amount, "counted in v7's own value");
        assert_eq!(pool.len(), v1_notes_before + 2, "and not in v1");
        let pos = pool.v7().unwrap().notes().iter().position(|c| *c == cm).expect("our commitment");
        let v7_root = pool.current_root_v7().unwrap();

        // the prover's view of the v7 epoch = the pool's own leaves, padded
        let mut leaves: Vec<Digest4> = pool.v7().unwrap().notes().iter().map(|c| Digest4::from_wire(c).unwrap()).collect();
        leaves.resize(POOL_CAPACITY, padding_leaf());
        let tree = PoolTreeV7::new(leaves).unwrap();
        assert_eq!(tree.root().to_wire(), v7_root, "node and prover agree on the v7 anchor");
        let path = tree.path(pos).unwrap();
        let bob7 = NoteV7::new(0, [0; 4], [0xB0, 0xB1, 0xB2, 0xB3]).unwrap().owner_pk();
        let pay = 4_000_000u64;
        let change = amount as u64 - pay - SHIELDED_FEE as u64;
        let outs = [OutputV7::new(pay, [0x51, 0x52, 0x53, 0x54], bob7), OutputV7::new(change, [0x61, 0x62, 0x63, 0x64], note.owner_pk())];
        let trace = build_spend_full_v7_trace_seeded_u64(&note, &path, SHIELDED_FEE as u64, &outs, &v7_options(), [0x77; 32]);
        let proof = SpendFullV7Prover::new(v7_options()).prove_hiding(trace).expect("prove v7 (hiding)").to_bytes();
        let nf = note.nullifier(pos as u64).to_wire();
        let cms: Vec<String> = outs.iter().map(|o| hex::encode(o.commitment().to_wire())).collect();
        let send = h.api.shielded.submit_shielded_send(&hex::encode(v7_root), &hex::encode(nf), &[], &cms, SHIELDED_FEE, proof, &[])
            .expect("the door routes a v7 spend to the v7 verifier");
        tick_until(&mut h, 60, "the v7 spend settles", |h| matches!(h.status(&send), Some(TxOutcome::Applied)) && h.chain.state().shielded().is_spent_in_epoch(1, &nf));
        let pool = h.chain.state().shielded();
        assert_eq!(pool.v7().unwrap().len(), v7_before + 3, "both v7 outputs landed in the v7 tree");
        assert_eq!(pool.value_locked_in(PoolGeneration::V7), amount - SHIELDED_FEE, "the fee left v7's own value");
        assert_eq!(pool.len(), v1_notes_before + 2, "v1 untouched by a v7 spend");
        assert!(!pool.is_spent_in_epoch(0, &nf), "the nullifier is scoped to the v7 epoch");
        eprintln!("chronos[v7 end to end]: v7 deposit at position {pos}, v7 spend settled, v7 notes {}", pool.v7().unwrap().len());
    }
    for _ in 0..6 { h.tick(); }

    // ── 5. FOLLOWER ≡ PRODUCER, and a third ChainTip re-derives every committed root ──
    assert_eq!(h.follower.height(), h.chain.height());
    assert_eq!(h.follower.parent_hash(), h.chain.parent_hash(), "follower sits on the producer's tip");
    assert_eq!(h.follower.state().roots(), h.chain.state().roots(), "four roots agree");
    assert_eq!(h.follower.state().shielded_pool_root(), h.chain.state().shielded_pool_root(), "the fifth root agrees");
    assert!(h.follower.state().shielded().v7_active());
    assert_eq!(h.applied_blocks.len() as u64, h.chain.height() - h.genesis_height, "every applied block was mirrored");

    let mut replay = sigil_node::chain::ChainTip::new();
    replay.apply(sigil_node::genesis::build_genesis().expect("genesis")).expect("apply genesis");
    let mut roots_seen: BTreeSet<[u8; 32]> = BTreeSet::new();
    let mut activation_block = None;
    for b in &h.applied_blocks {
        let hgt = b.header.height;
        replay.apply(b.clone()).unwrap_or_else(|e| panic!("replay refused the producer's block at {hgt}: {e}"));
        assert_eq!(replay.state().shielded().v7_active(), v7_pool_active(hgt), "h={hgt}: v7 active ⇔ height ≥ gate {gate}");
        if let Some(r) = b.header.shielded_pool_root {
            assert_eq!(r, replay.state().shielded_pool_root(), "h={hgt}: header root == re-derived pool root");
            roots_seen.insert(r);
        }
        if hgt == gate { activation_block = Some(b.clone()); }
    }
    let activation_block = activation_block.expect("the activation block was applied");
    assert!(roots_seen.len() >= 6, "the pool root took only {} distinct values", roots_seen.len());
    eprintln!("chronos[v7 activation]: gate {gate}, {} blocks, pool root took {} distinct values, v1 spent after the gate, late v1 deposit refused, follower ≡ producer", h.applied_blocks.len(), roots_seen.len());

    // ── 6. TEETH: the activation block with a flipped pool root is refused ────────
    let mut pre = sigil_node::chain::ChainTip::new();
    pre.apply(sigil_node::genesis::build_genesis().expect("genesis")).expect("apply genesis");
    for b in &h.applied_blocks {
        if b.header.height >= gate { break; }
        pre.apply(b.clone()).expect("replay prefix");
    }
    assert!(!pre.state().shielded().v7_active());
    let mut flipped = activation_block.clone();
    let mut r = flipped.header.shielded_pool_root.expect("gated");
    r[0] ^= 0x01;
    flipped.header.shielded_pool_root = Some(r);
    let err = pre.clone().apply(flipped).expect_err("a follower must refuse an activation block whose pool root is not its own");
    assert!(err.to_string().contains("STATE DIVERGENCE"), "{err}");
    pre.apply(activation_block).expect("the genuine activation block is accepted");
    assert!(pre.state().shielded().v7_active(), "and it activates v7 on the follower too");

    override_v7_pool_activation_for_simulation(u64::MAX);
    override_shielded_pool_root_activation_for_simulation(u64::MAX);
    std::env::remove_var("SIGIL_DAG_FINAL_DEPTH");
}
