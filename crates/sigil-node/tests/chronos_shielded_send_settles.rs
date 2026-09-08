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
//! mechanism is reproduced below in `THE_OLD_LOOP`: candidate N carries the send, its
//! nullifier enters the frontier, candidate N+1 is handed the same still-pending tx,
//! refuses it against its own effects, and three refusals evict it as rejected before
//! N is anywhere near finality. Run under `--profile release-fast` (the prover needs
//! `debug_assertions` off — same reason the sigil-shield prover tests are ignored in
//! debug).

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use sigil_api::shielded::{already_in_pool, TxOutcome};
use sigil_dagknight::{BlockView, Braid};
use sigil_header::BlockHash;
use sigil_narwhal_mempool::MempoolBackend;
use sigil_node::block::Block;
use sigil_node::chain::ChainTip;
use sigil_node::coinbase::{self, Rejection};
use sigil_node::dag::{
    compute_topology_commitment, dag_build_frontier, dag_drain_apply, dag_seed_braid,
    dag_store_body,
};
use sigil_node::mint::mint_next_block;
use sigil_shield::wallet::{build_spend, shield_note, NoteStore, ShieldedAccount};
use sigil_state::shielded::SHIELDED_FEE;
use sigil_state::NATIVE;
use sigil_tx::SignedTx;

/// Production is 512 (≈80 s of candidates). The finalization rule is the same code at any
/// depth; 3 lets one test cross it dozens of times.
const FINAL_DEPTH: &str = "3";
const DAG_BODIES_CAP: usize = 4096;

/// Which offer policy the loop runs. `Fixed` is what `sigil-node` and the sigil-top
/// producer now do; `OldUnconditional` is what they did until 2026-09-08, kept here so
/// the incident stays reproducible and the fix stays measured rather than asserted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum OfferPolicy {
    Fixed,
    OldUnconditional,
}

struct Harness {
    chain: ChainTip,
    braid: Braid,
    dag_bodies: HashMap<BlockHash, Block>,
    mint_map: HashMap<BlockHash, Vec<[u8; 32]>>,
    api: sigil_api::AppState,
    policy: OfferPolicy,
    /// Phase 3 finality gate (2026-09-08): after each mint, adopt a solo certificate for
    /// the block just minted — what `apply_finality_gate` does with a one-node committee.
    gate: bool,
    /// Every shielded tx handed to the builder, per tick — the "offered ONCE" gauge.
    offered: Vec<[u8; 32]>,
    /// Every verdict the builder recorded — the "never refused against itself" gauge.
    rejections: Vec<Rejection>,
    ticks: u64,
}

impl Harness {
    fn new(policy: OfferPolicy) -> Self {
        let mut chain = ChainTip::new();
        chain.apply(sigil_node::genesis::build_genesis().expect("genesis")).expect("apply genesis");
        let braid = dag_seed_braid(&chain);
        let state = Arc::new(RwLock::new(chain.state_snapshot()));
        let api = sigil_api::AppState::new(Arc::new(MempoolBackend::legacy()), state);
        Self {
            chain,
            braid,
            dag_bodies: HashMap::new(),
            mint_map: HashMap::new(),
            api,
            policy,
            gate: false,
            offered: Vec::new(),
            rejections: Vec::new(),
            ticks: 0,
        }
    }

    /// One producer tick — the same sequence as `sigil-node`'s mint loop.
    fn tick(&mut self) {
        self.ticks += 1;
        let frontier = dag_build_frontier(&self.chain, &self.braid, &self.dag_bodies);
        let merge_parents = self.braid.merge_tips(&frontier.parent_hash(), 4);
        let topo = compute_topology_commitment(Some(&self.braid), frontier.height());

        let txs: Vec<SignedTx> = match self.policy {
            OfferPolicy::Fixed => {
                let settled = self.chain.state().shielded();
                let _ = self.api.shielded.retire_settled(|tx| already_in_pool(settled, tx));
                let fp = frontier.state().shielded();
                self.api.shielded.snapshot_for_mint_excluding(|tx| already_in_pool(fp, tx))
            }
            OfferPolicy::OldUnconditional => self.api.shielded.snapshot_for_mint(),
        };
        self.offered.extend(txs.iter().map(|t| t.tx.hash()));

        let (block, minted) =
            mint_next_block(&frontier, merge_parents, &txs, None, None, topo, None).expect("mint");
        for r in coinbase::take_rejections() {
            if r.permanent {
                self.api.shielded.note_rejection(r.hash, &r.reason);
            }
            self.rejections.push(r);
        }
        let view = BlockView::from(&block.header);
        let vh = view.hash;
        let _ = self.braid.insert(view);
        let minted_height = self.dag_bodies.get(&vh).map(|b| b.header.height).unwrap_or(0);
        dag_store_body(&mut self.dag_bodies, DAG_BODIES_CAP, vh, block);
        let minted_height = self.dag_bodies.get(&vh).map(|b| b.header.height).unwrap_or(minted_height);
        if !minted.is_empty() {
            self.mint_map.insert(vh, minted);
        }
        if self.gate {
            let _ = self.braid.set_certified(minted_height, vh);
        }
        let (_applied, _skipped, failed) = dag_drain_apply(
            &mut self.braid,
            &mut self.dag_bodies,
            &mut self.chain,
            &mut |_| {},
            &self.api.send,
            &self.api.bridge,
            &self.api.dex,
            &self.api.usds,
            &self.api.usds_bridge,
            &self.api.shielded,
            &mut self.mint_map,
        );
        assert_eq!(failed, 0, "spine apply must never fail against our own candidates");
        if let Ok(mut w) = self.api.state.write() {
            *w = self.chain.state_snapshot();
        }
    }

    fn status(&self, h: &[u8; 32]) -> Option<TxOutcome> {
        self.api.shielded.status(h)
    }

    fn offered_count(&self, h: &[u8; 32]) -> usize {
        self.offered.iter().filter(|x| *x == h).count()
    }

    fn permanent_refusals(&self, h: &[u8; 32]) -> Vec<String> {
        self.rejections.iter().filter(|r| r.permanent && r.hash == *h).map(|r| r.reason.clone()).collect()
    }

    fn pool_view(&self) -> Vec<[u8; 32]> {
        self.chain.state().shielded().padded_leaves(sigil_shield::note_v1::padding_leaf_wire)
    }
}

fn sign_shield(sk: &ed25519_dalek::SigningKey, from: &str, amount: u128, cm: &str, fee: u128, nonce: u64) -> String {
    use ed25519_dalek::Signer;
    let msg = format!("sigil-rpc/v1|shield|{from}|{amount}|{cm}|{fee}|nonce={nonce}");
    hex::encode(sk.sign(msg.as_bytes()).to_bytes())
}

/// Tick until `pred` holds, or fail after `max` ticks with a readable reason.
fn tick_until(h: &mut Harness, max: u64, what: &str, mut pred: impl FnMut(&Harness) -> bool) {
    for _ in 0..max {
        if pred(h) {
            return;
        }
        h.tick();
    }
    assert!(pred(h), "gave up waiting for: {what} (after {max} ticks, settled h={})", h.chain.height());
}

/// Shield 1e6 from Alice, then privately pay Bob 500_000 (change 400_000, fee 100_000).
/// Returns (send_hash, nullifier). Everything real: signature, note derivation, STARK.
fn shield_then_send(
    h: &mut Harness,
    alice_sk: &ed25519_dalek::SigningKey,
    alice_addr: [u8; 32],
    alice: &ShieldedAccount,
    bob: &ShieldedAccount,
    bob_seed: &[u8; 32],
    store: &mut NoteStore,
) -> ([u8; 32], [u8; 32]) {
    // ── funding by the REAL coinbase ────────────────────────────────────────────
    tick_until(h, 40, "Alice funded by coinbase", |h| h.chain.state().balance_of(&alice_addr, &NATIVE) >= 1_000_000);

    // ── SHIELD ───────────────────────────────────────────────────────────────────
    let (note_index, cm) = shield_note(alice, store, 1_000_000).expect("derive note");
    let from_hex = hex::encode(alice_addr);
    let cm_hex = hex::encode(cm);
    let nonce = h.ticks; // strictly increasing per wallet within one harness
    let sig = sign_shield(alice_sk, &from_hex, 1_000_000, &cm_hex, 0, nonce);
    let shield_hash = h.api.shielded.submit_shield(&from_hex, 1_000_000, &cm_hex, 0, &sig, nonce).expect("queue shield");

    tick_until(h, 40, "shield applied", |h| matches!(h.status(&shield_hash), Some(TxOutcome::Applied)));
    assert_eq!(h.chain.state().shielded().len(), 1, "one note in the settled pool");

    // ── the wallet finds its note, builds a REAL proof ───────────────────────────
    let pool = h.pool_view();
    assert_eq!(store.scan_owned(alice, &pool), 1, "wallet must locate its own note");
    let bundle = build_spend(
        alice,
        store,
        &pool,
        note_index as usize,
        SHIELDED_FEE as u64,
        &[(500_000, bob.public_key()), (400_000, alice.public_key())],
    )
    .expect("build the payment");
    assert!(h.chain.state().shielded().is_known_anchor(&bundle.anchor));

    let bob_addr = bob.address(bob_seed);
    let (bv, bb) = bundle.out_preimages[0];
    let bob_ct = sigil_shield::note_cipher::seal_note(
        &sigil_shield::note_cipher::NotePlaintext::new(bv, bb),
        &bob_addr,
    )
    .expect("seal to bob");

    let send_hash = h
        .api
        .shielded
        .submit_shielded_send(
            &hex::encode(bundle.anchor),
            &hex::encode(bundle.nullifier),
            &[],
            &bundle.cm_outs.iter().map(hex::encode).collect::<Vec<_>>(),
            SHIELDED_FEE,
            bundle.proof.clone(),
            &[Some(bob_ct.0.clone()), None],
        )
        .expect("queue the shielded send");
    (send_hash, bundle.nullifier)
}

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

    // ═══════════════════ THE_OLD_LOOP — reproduce the live incident ═══════════════════
    {
        let mut h = Harness::new(OfferPolicy::OldUnconditional);
        let mut store = NoteStore::new();
        let (send_hash, nf) = shield_then_send(&mut h, &alice_sk, alice_addr, &alice, &bob, &bob_seed, &mut store);

        // Three candidates later the bridge has evicted it as rejected — while the
        // nullifier is on its way to being settled. This is the lie, reproduced.
        tick_until(&mut h, 8, "old loop evicts the send", |h| matches!(h.status(&send_hash), Some(TxOutcome::Rejected(_))));
        let refusals = h.permanent_refusals(&send_hash);
        assert!(
            refusals.iter().all(|r| r.contains("nullifier already spent")),
            "the old loop's refusals must be self-collisions, got {refusals:?}"
        );
        assert!(refusals.len() >= 3, "REJECT_AFTER=3 refusals evict, got {}", refusals.len());
        // ...and yet the money moved: finality proves it.
        tick_until(&mut h, 40, "old loop settles the nullifier anyway", |h| h.chain.state().shielded().is_spent(&nf));
        assert!(
            matches!(h.status(&send_hash), Some(TxOutcome::Rejected(_))),
            "OLD LOOP: status stays 'rejected' after the send settled — the incident of 2026-09-05"
        );
        assert!(h.offered_count(&send_hash) >= 4, "old loop re-offered the landed send every candidate");
        eprintln!(
            "chronos[old loop]: offered {}x, {} self-refusals, status=rejected while nullifier settled — incident reproduced",
            h.offered_count(&send_hash), refusals.len()
        );
    }

    // ═══════════════════════════ THE FIXED LOOP ═══════════════════════════════════
    let mut h = Harness::new(OfferPolicy::Fixed);
    let mut store = NoteStore::new();
    let (send_hash, nf) = shield_then_send(&mut h, &alice_sk, alice_addr, &alice, &bob, &bob_seed, &mut store);
    let pool_before = h.chain.state().shielded().len();

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
    assert!(h.chain.state().shielded().is_spent(&nf), "the nullifier is spent on the SETTLED chain");
    assert_eq!(h.chain.state().shielded().len(), pool_before + 2, "exactly two output notes landed, once");
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
    assert!(g.chain.state().shielded().is_spent(&nf));
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
