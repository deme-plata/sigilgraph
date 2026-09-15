//! Shared chronos harness for the sigil-node integration tests: the REAL producer loop, in
//! the order `sigil-node` runs it every tick, with no layer stubbed — plus an independent
//! FOLLOWER `ChainTip` that re-applies every body the spine applied (the single-commit shape
//! happysrv runs). Extracted from `chronos_shielded_send_settles.rs` on 2026-09-14 so the
//! header-vs-body gate could drive the same loop.
#![allow(dead_code)]

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
pub const FINAL_DEPTH: &str = "3";
pub const DAG_BODIES_CAP: usize = 4096;

/// Which offer policy the loop runs. `Fixed` is what `sigil-node` and the sigil-top
/// producer now do; `OldUnconditional` is what they did until 2026-09-08, kept here so
/// the incident stays reproducible and the fix stays measured rather than asserted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OfferPolicy {
    Fixed,
    OldUnconditional,
}

pub struct Harness {
    pub chain: ChainTip,
    pub braid: Braid,
    pub dag_bodies: HashMap<BlockHash, Block>,
    pub mint_map: HashMap<BlockHash, Vec<[u8; 32]>>,
    pub api: sigil_api::AppState,
    pub policy: OfferPolicy,
    /// Phase 3 finality gate (2026-09-08): after each mint, adopt a solo certificate for
    /// the block just minted — what `apply_finality_gate` does with a one-node committee.
    pub gate: bool,
    /// Every shielded tx handed to the builder, per tick — the "offered ONCE" gauge.
    pub offered: Vec<[u8; 32]>,
    /// Every verdict the builder recorded — the "never refused against itself" gauge.
    pub rejections: Vec<Rejection>,
    pub ticks: u64,
    /// A second, independent `ChainTip` fed ONLY the bodies the producer's spine applied —
    /// the follower's single-commit shape (`ChainTip::apply` → `check_roots_match`). This is
    /// the seam that broke on 2026-09-10: producer per-tx commits vs follower single commit.
    /// Every block the producer applies is re-applied here and must be accepted.
    pub follower: ChainTip,
    /// Every body the spine applied, in order, as the follower saw it.
    pub applied_blocks: Vec<Block>,
    /// Height after genesis — `applied_blocks.len() == chain.height() - genesis_height`.
    pub genesis_height: u64,
    /// Non-shielded txs handed to the NEXT candidate (what `nation_bridge.snapshot_for_mint`
    /// contributes on the live node): GaugePush, OraclePush… Drained on the tick they are
    /// offered; a refused one shows up in `rejections`.
    pub extra_txs: Vec<SignedTx>,
}

impl Harness {
    pub fn new(policy: OfferPolicy) -> Self {
        let mut chain = ChainTip::new();
        chain.apply(sigil_node::genesis::build_genesis().expect("genesis")).expect("apply genesis");
        let follower = chain.clone();
        let genesis_height = chain.height();
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
            follower,
            applied_blocks: Vec::new(),
            genesis_height,
            extra_txs: Vec::new(),
        }
    }

    /// Replay onto the follower every body the producer's spine applied since the last
    /// tick, oldest first, by walking the parent links back from the new tip to the
    /// follower's tip — exactly what a follower does with the gossiped bodies. A refusal
    /// here IS the 2026-09-10 incident (`STATE DIVERGENCE … cannot extend tip`).
    fn mirror_follower(&mut self) {
        let want = self.follower.parent_hash();
        let mut cursor = self.chain.parent_hash();
        let mut path: Vec<Block> = Vec::new();
        while cursor != want {
            let Some(b) = self.dag_bodies.get(&cursor) else {
                panic!("follower mirror: body {} missing from dag_bodies (cap {})", hex::encode(&cursor[..4]), DAG_BODIES_CAP);
            };
            cursor = b.header.parent_hash;
            path.push(b.clone());
        }
        path.reverse();
        for b in path {
            let h = b.header.height;
            self.follower
                .apply(b.clone())
                .unwrap_or_else(|e| panic!("FOLLOWER REFUSED the producer's block at height {h}: {e}"));
            self.applied_blocks.push(b);
        }
        assert_eq!(self.follower.height(), self.chain.height(), "follower and producer heights agree");
    }

    /// One producer tick — the same sequence as `sigil-node`'s mint loop.
    pub fn tick(&mut self) {
        self.ticks += 1;
        let frontier = dag_build_frontier(&self.chain, &self.braid, &self.dag_bodies);
        let merge_parents = self.braid.merge_tips(&frontier.parent_hash(), 4);
        let topo = compute_topology_commitment(Some(&self.braid), frontier.height());

        let mut txs: Vec<SignedTx> = match self.policy {
            OfferPolicy::Fixed => {
                let settled = self.chain.state().shielded();
                let _ = self.api.shielded.retire_settled(|tx| already_in_pool(settled, tx));
                let fp = frontier.state().shielded();
                self.api.shielded.snapshot_for_mint_excluding(|tx| already_in_pool(fp, tx))
            }
            OfferPolicy::OldUnconditional => self.api.shielded.snapshot_for_mint(),
        };
        txs.append(&mut self.extra_txs);
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
        self.mirror_follower();
        if let Ok(mut w) = self.api.state.write() {
            *w = self.chain.state_snapshot();
        }
    }

    pub fn status(&self, h: &[u8; 32]) -> Option<TxOutcome> {
        self.api.shielded.status(h)
    }

    pub fn offered_count(&self, h: &[u8; 32]) -> usize {
        self.offered.iter().filter(|x| *x == h).count()
    }

    pub fn permanent_refusals(&self, h: &[u8; 32]) -> Vec<String> {
        self.rejections.iter().filter(|r| r.permanent && r.hash == *h).map(|r| r.reason.clone()).collect()
    }

    pub fn pool_view(&self) -> Vec<[u8; 32]> {
        self.chain.state().shielded().padded_leaves(sigil_shield::note_v1::padding_leaf_wire)
    }
}

pub fn sign_shield(sk: &ed25519_dalek::SigningKey, from: &str, amount: u128, cm: &str, fee: u128, nonce: u64) -> String {
    use ed25519_dalek::Signer;
    let msg = format!("sigil-rpc/v1|shield|{from}|{amount}|{cm}|{fee}|nonce={nonce}");
    hex::encode(sk.sign(msg.as_bytes()).to_bytes())
}

/// Tick until `pred` holds, or fail after `max` ticks with a readable reason.
pub fn tick_until(h: &mut Harness, max: u64, what: &str, mut pred: impl FnMut(&Harness) -> bool) {
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
pub fn shield_then_send(
    h: &mut Harness,
    alice_sk: &ed25519_dalek::SigningKey,
    alice_addr: [u8; 32],
    alice: &ShieldedAccount,
    bob: &ShieldedAccount,
    bob_seed: &[u8; 32],
    store: &mut NoteStore,
) -> ([u8; 32], [u8; 32]) {
    shield_then_send_amount(h, alice_sk, alice_addr, alice, bob, bob_seed, store, 1_000_000, 500_000)
}

/// [`shield_then_send`] with explicit amounts: shield `shield_amt`, pay Bob `pay_amt`, the
/// rest minus `SHIELDED_FEE` returns to Alice as change. Use a FRESH `NoteStore` per call
/// (the store's positional note index is what `build_spend` selects by), and a different
/// `shield_amt` per call under one seed, or the deposit is a commitment replay.
#[allow(clippy::too_many_arguments)]
pub fn shield_then_send_amount(
    h: &mut Harness,
    alice_sk: &ed25519_dalek::SigningKey,
    alice_addr: [u8; 32],
    alice: &ShieldedAccount,
    bob: &ShieldedAccount,
    bob_seed: &[u8; 32],
    store: &mut NoteStore,
    shield_amt: u128,
    pay_amt: u128,
) -> ([u8; 32], [u8; 32]) {
    let change = shield_amt - pay_amt - SHIELDED_FEE;
    // ── funding by the REAL coinbase ────────────────────────────────────────────
    tick_until(h, 40, "Alice funded by coinbase", |h| h.chain.state().balance_of(&alice_addr, &NATIVE) >= shield_amt);

    // ── SHIELD ───────────────────────────────────────────────────────────────────
    let (note_index, cm) = shield_note(alice, store, shield_amt as u64).expect("derive note");
    let from_hex = hex::encode(alice_addr);
    let cm_hex = hex::encode(cm);
    let nonce = h.ticks; // strictly increasing per wallet within one harness
    let sig = sign_shield(alice_sk, &from_hex, shield_amt, &cm_hex, 0, nonce);
    let notes_before = h.chain.state().shielded().len();
    let shield_hash = h.api.shielded.submit_shield(&from_hex, shield_amt, &cm_hex, 0, &sig, nonce).expect("queue shield");

    tick_until(h, 40, "shield applied", |h| matches!(h.status(&shield_hash), Some(TxOutcome::Applied)));
    assert_eq!(h.chain.state().shielded().len(), notes_before + 1, "exactly one new note in the settled pool");

    // ── the wallet finds its note, builds a REAL proof ───────────────────────────
    // (`scan_owned` counts every note of Alice's in the pool — 1 on a fresh harness, more
    // once earlier rounds left her change notes behind.)
    let pool = h.pool_view();
    assert!(store.scan_owned(alice, &pool) >= 1, "wallet must locate its own note");
    let bundle = build_spend(
        alice,
        store,
        &pool,
        note_index as usize,
        SHIELDED_FEE as u64,
        &[(pay_amt as u64, bob.public_key()), (change as u64, alice.public_key())],
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

