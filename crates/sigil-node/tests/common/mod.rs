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
use sigil_shield::note_cipher::is_tagged_envelope;
use sigil_shield::wallet::{build_spend, shield_note, NoteStore, ShieldedAccount};
use sigil_shield::wallet_v7::{build_spend_v7, seal_output_v7, shield_note_v7, AccountV7, NoteStoreV7, SpendBundleV7};
use sigil_state::shielded::{PoolGeneration, POOL_CAPACITY, SHIELDED_FEE};
use sigil_state::NATIVE;
use sigil_tx::SignedTx;
use std::sync::atomic::{AtomicU64, Ordering};

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
        // sigil-g3: the door reads each note's generation from the settled pool, as
        // `sigil-node` wires it at startup; without it a v7 anchor is unknown at the door.
        if sigil_header::IS_G3 {
            api.shielded.set_pool_view(Arc::clone(&api.state));
        }
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

/// The epoch a payment made by [`shield_then_send_amount`] spends in: epoch 0 (the v1 tree)
/// on g2/r1, the v7 epoch on the g3 family (v1 closed from genesis, v7 = epoch 1). Exact on
/// purpose, never "spent in any epoch": a spend booked in the wrong epoch must fail a gate.
pub fn spend_epoch(h: &Harness) -> u32 {
    if sigil_header::IS_G3 {
        h.chain.state().shielded().v7().expect("sigil-g3: the v7 epoch is open from genesis").epoch()
    } else {
        0
    }
}

/// Is `nf` spent on the producer's SETTLED chain, in the epoch [`spend_epoch`] names?
pub fn nf_spent(h: &Harness, nf: &[u8; 32]) -> bool {
    h.chain.state().shielded().is_spent_in_epoch(spend_epoch(h), nf)
}

/// Notes in the tree a payment's outputs land in: v1 on g2/r1, v7 on the g3 family.
pub fn pay_tree_len(h: &Harness) -> usize {
    let pool = h.chain.state().shielded();
    if sigil_header::IS_G3 { pool.v7().map_or(0, |e| e.len()) } else { pool.len() }
}

/// Remove the view tag from a sealed envelope (`…,"vt":"ab"}` → `…}`): an honest envelope a
/// pre-g3 wallet would have written (same as `sigil-api/examples/g3_r3_driver.rs`).
pub fn untag(ct: &str) -> String {
    let i = ct.rfind(",\"vt\":\"").expect("tagged envelope");
    format!("{}}}", &ct[..i])
}

/// Deposit nonces for the g3 helpers: strictly increasing across the whole test process, so
/// a test may submit refused and accepted deposits in the same tick.
static G3_NONCE: AtomicU64 = AtomicU64::new(1);
pub fn g3_nonce() -> u64 {
    G3_NONCE.fetch_add(1, Ordering::Relaxed)
}

/// A sigil-g3 deposit, built exactly as the live g3 driver builds it
/// (`sigil-api/examples/g3_r3_driver.rs`): a v7 note, a sealed view-tagged envelope to the
/// depositor, and the opening (blinding + owner key) the door checks, through the door's
/// opened entry (the one `/v1/shield` calls). Returns (tx hash, store slot of the note).
pub fn g3_deposit(h: &mut Harness, sk: &ed25519_dalek::SigningKey, addr: [u8; 32], acct: &AccountV7, store: &mut NoteStoreV7, amount: u128) -> ([u8; 32], usize) {
    let dep = shield_note_v7(acct, store, amount as u64).expect("derive v7 note");
    let ct = seal_output_v7(amount as u64, acct.blinding(dep.index), &acct.pk_enc_hex(), "chronos deposit").expect("seal deposit").0;
    assert!(is_tagged_envelope(&ct), "seal_output_v7 writes a view tag");
    let from_hex = hex::encode(addr);
    let cm_hex = hex::encode(dep.cm);
    let nonce = g3_nonce();
    let sig = sign_shield(sk, &from_hex, amount, &cm_hex, 0, nonce);
    let hash = h
        .api
        .shielded
        .submit_shield_opened(&from_hex, amount, &cm_hex, 0, &sig, nonce, Some(&ct), Some(&hex::encode(dep.blinding)), Some(&hex::encode(dep.pk_shield)))
        .expect("queue the tagged v7 deposit");
    let slot = store.notes.iter().position(|n| n.index == Some(dep.index)).expect("the deposit is booked in the store");
    (hash, slot)
}

/// A real v7 payment of `pay` from the note at `slot` to `to`, change back to `acct`, with a
/// sealed view-tagged envelope for BOTH outputs (g3 refuses a payment with any untagged
/// output). Returns the bundle and the two envelopes; nothing is submitted.
pub fn g3_build_payment(h: &Harness, acct: &AccountV7, to: &AccountV7, store: &mut NoteStoreV7, slot: usize, pay: u64) -> (SpendBundleV7, Vec<Option<String>>) {
    let leaves: Vec<[u8; 32]> = h.chain.state().shielded().v7().expect("sigil-g3: v7 open").notes().to_vec();
    // (scan_owned reports NEWLY found notes; the note's tree position is the check.)
    let _ = store.scan_owned(acct, &leaves);
    assert!(store.notes[slot].position.is_some(), "the note to spend has a v7 tree position");
    let change = store.notes[slot].value - pay - SHIELDED_FEE as u64;
    let b = build_spend_v7(acct, store, &leaves, POOL_CAPACITY, slot, SHIELDED_FEE as u64, &[(pay, to.owner_pk()), (change, acct.owner_pk())])
        .expect("build the v7 payment");
    assert!(
        matches!(h.chain.state().shielded().generation_of_anchor(&b.anchor), Some(PoolGeneration::V7)),
        "the proof is against a v7 anchor the chain holds"
    );
    let cts = b
        .out_preimages
        .iter()
        .zip([to.pk_enc_hex(), acct.pk_enc_hex()])
        .map(|((v, bl), pk)| Some(seal_output_v7(*v, *bl, &pk, "chronos pay").expect("seal output").0))
        .collect();
    (b, cts)
}

/// Submit a v7 payment through the real door.
pub fn g3_submit_payment(h: &Harness, b: &SpendBundleV7, cts: &[Option<String>]) -> Result<[u8; 32], sigil_api::shielded::ShieldedSubmitError> {
    h.api.shielded.submit_shielded_send(
        &hex::encode(b.anchor),
        &hex::encode(b.nullifier),
        &b.extra_nullifiers.iter().map(hex::encode).collect::<Vec<_>>(),
        &b.cm_outs.iter().map(hex::encode).collect::<Vec<_>>(),
        SHIELDED_FEE,
        b.proof.clone(),
        cts,
    )
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
    if sigil_header::IS_G3 {
        // v1 is closed from genesis on g3: the v1 wallet the caller passes is never used.
        let _ = (alice, bob, store);
        return shield_then_send_amount_g3(h, alice_sk, alice_addr, bob_seed, shield_amt, pay_amt);
    }
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

/// [`shield_then_send_amount`] under sigil-g3 rules (2026-10-19): a tagged v7 deposit, then a
/// real v7 payment with both outputs tagged. The v7 accounts derive from the same 32-byte
/// seeds the tests use for Alice's ed25519 key and Bob's v1 account, the way the live g3
/// driver pairs them (`AccountV7::from_seed(producer_seed)`).
fn shield_then_send_amount_g3(
    h: &mut Harness,
    alice_sk: &ed25519_dalek::SigningKey,
    alice_addr: [u8; 32],
    bob_seed: &[u8; 32],
    shield_amt: u128,
    pay_amt: u128,
) -> ([u8; 32], [u8; 32]) {
    let alice = AccountV7::from_seed(alice_sk.to_bytes());
    let bob = AccountV7::from_seed(*bob_seed);
    tick_until(h, 40, "Alice funded by coinbase", |h| h.chain.state().balance_of(&alice_addr, &NATIVE) >= shield_amt);
    let mut store = NoteStoreV7::new();
    let notes_before = pay_tree_len(h);
    let (shield_hash, slot) = g3_deposit(h, alice_sk, alice_addr, &alice, &mut store, shield_amt);
    tick_until(h, 40, "tagged v7 deposit applied", |h| matches!(h.status(&shield_hash), Some(TxOutcome::Applied)));
    assert_eq!(pay_tree_len(h), notes_before + 1, "exactly one new note in the settled v7 tree");
    let (bundle, cts) = g3_build_payment(h, &alice, &bob, &mut store, slot, pay_amt as u64);
    let send_hash = g3_submit_payment(h, &bundle, &cts).expect("queue the v7 shielded send");
    (send_hash, bundle.nullifier)
}

