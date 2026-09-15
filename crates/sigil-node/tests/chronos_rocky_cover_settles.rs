//! chronos — ROCKY, the first native contract with an execution path, driven through the REAL
//! producer loop and an independent follower: bootstrap → reflection transfer → stake → buy
//! K⊕ cover → the on-chain gauge crosses p99 → claim pays from the pool — every step settled
//! identically on both nodes, every block's typed events committed by its header.
//!
//! Like `chronos_gauge_push_settles.rs`, the activation height is scheduled in-process
//! (`GAUGE_LIVE_HEIGHT` ships dormant). Run under `--profile release-fast`.

mod common;
use common::*;

use sigil_events::{events_root, SigilEvent};
use sigil_header::{PubKeyBytes, SigScheme, SignatureBytes};
use sigil_oracle::feed_id;
use sigil_tx::{SigilTx, SignedTx};
use sigil_vm::contracts::rocky_cover::{self as rc, RockyCall};

fn as_signed(tx: SigilTx) -> SignedTx {
    let payer = tx.fee_payer();
    SignedTx { tx, from_pubkey: payer, nonce: 0, sig_scheme: SigScheme::Ed25519Hot, sig: SignatureBytes(vec![0u8; SigScheme::Ed25519Hot.expected_sig_len()]), pubkey: PubKeyBytes(Vec::new()) }
}
fn call(from: [u8; 32], c: &RockyCall) -> SignedTx {
    as_signed(SigilTx::ContractCall { from, contract: rc::ROCKY_CONTRACT, method: c.selector(), calldata: c.encode(), gas_limit: 0, fee: 0 })
}
fn gauge(authority: [u8; 32], feed: &str, value_e6: i128, date: u32) -> SignedTx {
    as_signed(SigilTx::GaugePush { authority, feed: feed_id(feed), value_e6, reading_date: date, attest_blake3: [0x4A; 32], fee: 0 })
}

/// Settle: tick until `pred` holds on the PRODUCER's settled state.
fn settle(h: &mut Harness, what: &str, pred: impl Fn(&sigil_state::SigilState) -> bool) {
    tick_until(h, 12, what, |h| pred(h.chain.state()));
    assert_eq!(h.follower.state().roots(), h.chain.state().roots(), "{what}: follower and producer roots agree");
}

#[test]
fn chronos_rocky_cover_bootstrap_stake_cover_claim_on_both_nodes() {
    std::env::set_var("SIGIL_DAG_FINAL_DEPTH", FINAL_DEPTH);
    let mut h = Harness::new(OfferPolicy::Fixed);
    let master = h.chain.state().master_wallet().expect("genesis master");
    let rocky = [0x87u8; 32];
    let alice = [0x11u8; 32];
    let bob = [0x22u8; 32];
    let live_at = h.chain.height() + 2;
    sigil_oracle::chronos_schedule_gauge_live_height(live_at);
    tick_until(&mut h, 10, "activation", |h| h.chain.height() >= live_at);

    // ── bootstrap by the master: Rocky owns the token
    h.extra_txs.push(call(master, &RockyCall::Bootstrap { owner: rocky, initial_supply: 1_000_000 * rc::ONE_ROCKY }));
    h.tick();
    settle(&mut h, "bootstrap settles", |s| rc::bootstrapped(s));
    assert_eq!(rc::owner(h.follower.state()), Some(rocky));
    assert_eq!(rc::raw_balance(h.follower.state(), &rocky), 1_000_000 * rc::ONE_ROCKY);

    // ── owner airdrops Alice and Bob; a reflection transfer takes its 2 % fee
    h.extra_txs.push(call(rocky, &RockyCall::Airdrop { recipients: vec![rc::Drop { to: alice, amount: 1_000 * rc::ONE_ROCKY }, rc::Drop { to: bob, amount: 100 * rc::ONE_ROCKY }] }));
    h.tick();
    settle(&mut h, "airdrop settles", |s| rc::raw_balance(s, &bob) == 100 * rc::ONE_ROCKY);
    h.extra_txs.push(call(alice, &RockyCall::Transfer { to: bob, amount: 100 * rc::ONE_ROCKY }));
    h.tick();
    settle(&mut h, "transfer settles", |s| rc::raw_balance(s, &bob) == 198 * rc::ONE_ROCKY);
    assert_eq!(rc::reflect_pool(h.follower.state()), 2 * rc::ONE_ROCKY);

    // ── Alice underwrites: stakes 500 → 500 shares (first staker, NAV 1)
    h.extra_txs.push(call(alice, &RockyCall::Stake { amount: 500 * rc::ONE_ROCKY }));
    h.tick();
    settle(&mut h, "stake settles", |s| rc::pool_balance(s) == 500 * rc::ONE_ROCKY);
    assert_eq!(rc::shares_of(h.follower.state(), &alice), 500 * rc::ONE_ROCKY);

    // ── Bob buys 200 of cover for September, premium 4
    h.extra_txs.push(call(bob, &RockyCall::BuyCover { premium: 4 * rc::ONE_ROCKY, cover: 200 * rc::ONE_ROCKY, from_date: 20260901, until_date: 20260930 }));
    h.tick();
    settle(&mut h, "cover settles", |s| rc::policy_count(s) == 1);
    let p = rc::policy(h.follower.state(), 0).unwrap();
    assert_eq!((p.holder, p.cover, p.claimed), (bob, 200 * rc::ONE_ROCKY, false));
    assert_eq!(rc::pool_balance(h.follower.state()), 504 * rc::ONE_ROCKY);

    // ── a claim before any gauge is on chain is refused by the real minter, nothing written
    let early = call(bob, &RockyCall::Claim { policy: 0 });
    let early_hash = early.tx.hash();
    h.extra_txs.push(early);
    h.tick();
    assert!(h.rejections.iter().any(|r| r.hash == early_hash && r.reason.contains("no fresh gauge")), "{:?}", h.rejections);

    // ── the feeder (master here) pushes p99 and a K⊕ above it, inside Bob's window
    h.extra_txs.push(gauge(master, "earth.k_p99", 3_120_000, 20260918));
    h.extra_txs.push(gauge(master, "earth.k_resid", 3_400_000, 20260918));
    h.tick();
    settle(&mut h, "gauges settle", |s| sigil_oracle::read_gauge(s, &feed_id("earth.k_resid")).is_some());

    // ── anyone triggers the claim; Bob is paid from the pool; both nodes agree; once
    let bob_before = rc::raw_balance(h.follower.state(), &bob);
    let before_blocks = h.applied_blocks.len();
    h.extra_txs.push(call(alice, &RockyCall::Claim { policy: 0 }));
    h.tick();
    settle(&mut h, "claim settles", |s| rc::policy(s, 0).map(|p| p.claimed).unwrap_or(false));
    assert_eq!(rc::raw_balance(h.follower.state(), &bob), bob_before + 200 * rc::ONE_ROCKY, "Bob is paid his cover");
    assert_eq!(rc::pool_balance(h.follower.state()), 304 * rc::ONE_ROCKY, "the underwriters' pool paid it");
    let again = call(bob, &RockyCall::Claim { policy: 0 });
    let again_hash = again.tx.hash();
    h.extra_txs.push(again);
    h.tick();
    assert!(h.rejections.iter().any(|r| r.hash == again_hash && r.reason.contains("already claimed")));

    // ── every block since the claim carries typed events the header committed
    for b in &h.applied_blocks[before_blocks..] {
        assert_eq!(events_root(&b.events), b.header.event_log_root, "h={}", b.header.height);
    }
    let claim_block = h.applied_blocks[before_blocks..].iter().find(|b| b.events.iter().any(|e| matches!(e, SigilEvent::ContractCall { gas_used, .. } if *gas_used > 0))).expect("the claim block carries a ContractCall event");
    assert!(claim_block.events.iter().all(|e| !matches!(e, SigilEvent::ContractCall { result_hash, .. } if *result_hash == [0u8; 32])), "an executed call never carries the zero digest");

    // ── conservation on the follower: every ROCKY is in a wallet, the pool or the reflection pool
    let s = h.follower.state();
    let held: u128 = [rocky, alice, bob].iter().map(|w| rc::raw_balance(s, w)).sum();
    assert_eq!(held + rc::pool_balance(s) + rc::reflect_pool(s), rc::total_supply(s));

    eprintln!(
        "chronos[rocky cover]: {} blocks mirrored · Bob paid {} ROCKY on K⊕ 3.40 > p99 3.12 (2026-09-18) · pool {} · reflection pool {} · producer ≡ follower",
        h.applied_blocks.len(), 200, rc::pool_balance(s) / rc::ONE_ROCKY, rc::reflect_pool(s) as f64 / rc::ONE_ROCKY as f64
    );
    sigil_oracle::chronos_schedule_gauge_live_height(u64::MAX);
    std::env::remove_var("SIGIL_DAG_FINAL_DEPTH");
}
