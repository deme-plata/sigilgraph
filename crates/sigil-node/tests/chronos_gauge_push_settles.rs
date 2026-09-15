//! chronos — a Kristensen gauge reading pushed on-chain must settle ONCE, on BOTH nodes,
//! with the header committing exactly the body it ships with.
//!
//! `GaugePush` (sigil-tx tag 24, 2026-09-15) writes K⊕ / K_bio / the breathing / the ops
//! ladder into contract slots the way `OraclePush` writes the price. It is a consensus
//! change, so per the 2026-09-10 rule it is driven here through the REAL producer loop and
//! an independent follower BEFORE any operator schedules `GAUGE_LIVE_HEIGHT` — the
//! activation constant ships DORMANT and this scenario schedules it in-process.
//!
//! What is proven:
//!   1. A push before the scheduled height is refused by the real minter (rejection recorded),
//!      nothing is written.
//!   2. After the height, the master's pushes for two feeds land in ONE candidate, the
//!      follower mirror applies the block (single commit, `check_roots_match`), and both
//!      nodes read the same `GaugeReading` — including a NEGATIVE value (the breathing rate).
//!   3. The block's typed `GaugePushed` events hash to the header's `event_log_root`
//!      (`sigil_events::events_root`) — the exact seam that froze happysrv on 09-10.
//!   4. A stranger's push and a stale (older-dated) re-push are refused; the committed
//!      reading is untouched; a same-day revision and a newer day advance the nonce.
//!
//! Run under `--profile release-fast` (the harness mints real blocks; no STARK needed here).

mod common;
use common::*;

use sigil_events::{events_root, SigilEvent};
use sigil_header::{PubKeyBytes, SigScheme, SignatureBytes};
use sigil_oracle::{feed_id, read_gauge, GaugeReading};
use sigil_tx::{SigilTx, SignedTx};

/// What `nation_bridge.snapshot_for_mint` hands the minter: the intent, the fee payer, and a
/// zero hot-key signature — authentication happened at the API door; the CONSENSUS rule is
/// `authority == master (or delegated feeder)` inside `apply_tx`.
fn as_signed(tx: SigilTx) -> SignedTx {
    let payer = tx.fee_payer();
    SignedTx {
        tx,
        from_pubkey: payer,
        nonce: 0,
        sig_scheme: SigScheme::Ed25519Hot,
        sig: SignatureBytes(vec![0u8; SigScheme::Ed25519Hot.expected_sig_len()]),
        pubkey: PubKeyBytes(Vec::new()),
    }
}

fn push(authority: [u8; 32], feed_name: &str, value_e6: i128, reading_date: u32, digest: u8) -> SignedTx {
    as_signed(SigilTx::GaugePush { authority, feed: feed_id(feed_name), value_e6, reading_date, attest_blake3: [digest; 32], fee: 0 })
}

#[test]
fn chronos_gauge_push_lands_once_on_both_nodes_and_the_header_commits_it() {
    std::env::set_var("SIGIL_DAG_FINAL_DEPTH", FINAL_DEPTH);
    let mut h = Harness::new(OfferPolicy::Fixed);
    let master = h.chain.state().master_wallet().expect("genesis commits a master wallet");
    let stranger = [0x5Eu8; 32];
    let k_bio = feed_id("bio.k_bio");
    let breathing = feed_id("bio.breathing_pgc_day");

    // ── 1. DORMANT: the constant is u64::MAX; schedule activation a few blocks ahead.
    assert_eq!(sigil_oracle::GAUGE_LIVE_HEIGHT, u64::MAX, "GaugePush must ship dormant");
    let live_at = h.chain.height() + 4;
    sigil_oracle::chronos_schedule_gauge_live_height(live_at);
    let early = push(master, "bio.k_bio", 1_688_887, 20260913, 0xE1);
    let early_hash = early.tx.hash();
    h.extra_txs.push(early);
    h.tick();
    assert!(h.chain.height() < live_at, "still before activation");
    assert!(h.rejections.iter().any(|r| r.hash == early_hash), "the early push must be refused by the real minter: {:?}", h.rejections);
    assert!(read_gauge(h.chain.state(), &k_bio).is_none(), "nothing written before activation");

    // ── 2. After the height: two feeds in one candidate, one of them negative.
    tick_until(&mut h, 10, "activation height reached", |h| h.chain.height() >= live_at);
    let a = push(master, "bio.k_bio", 1_688_887, 20260913, 0xA1);
    let b = push(master, "bio.breathing_pgc_day", -53_320, 20260913, 0xA1);
    let (ha, hb) = (a.tx.hash(), b.tx.hash());
    h.extra_txs.push(a);
    h.extra_txs.push(b);
    let before = h.applied_blocks.len();
    h.tick();
    assert!(!h.rejections.iter().any(|r| r.hash == ha || r.hash == hb), "both pushes accepted: {:?}", h.rejections);
    // the candidate settles once it is FINAL_DEPTH behind the tip — the same wait a wallet does
    tick_until(&mut h, 10, "the push block settles", |h| read_gauge(h.chain.state(), &k_bio).is_some());
    let prod = read_gauge(h.chain.state(), &k_bio).expect("k_bio committed on the producer");
    let foll = read_gauge(h.follower.state(), &k_bio).expect("k_bio committed on the follower");
    assert_eq!(prod, foll, "producer and follower read the same reading");
    assert_eq!((prod.value_e6, prod.reading_date, prod.nonce, prod.attest_blake3), (1_688_887, 20260913, 1, [0xA1; 32]));
    let br = read_gauge(h.follower.state(), &breathing).unwrap();
    assert_eq!(br, GaugeReading { value_e6: -53_320, reading_date: 20260913, height: br.height, nonce: 1, attest_blake3: [0xA1; 32] });
    assert!(br.height >= live_at);
    assert_eq!(h.follower.state().roots(), h.chain.state().roots(), "all four roots agree after the push");

    // ── 3. The body's typed events ARE what the header committed.
    let block = h.applied_blocks[before..].iter().find(|b| !b.events.is_empty()).expect("the push block carries events");
    let pushed: Vec<&SigilEvent> = block.events.iter().filter(|e| matches!(e, SigilEvent::GaugePushed { .. })).collect();
    assert_eq!(pushed.len(), 2, "two GaugePushed events in the block body");
    assert!(pushed.iter().any(|e| matches!(e, SigilEvent::GaugePushed { feed, value_e6: -53_320, .. } if *feed == breathing)));
    assert_eq!(events_root(&block.events), block.header.event_log_root, "header commits exactly the body it ships with");
    assert_ne!(block.header.event_log_root, [0u8; 32]);

    // ── 4. Refusals leave the committed reading untouched; revisions and newer days advance it.
    let s = push(stranger, "bio.k_bio", 9_000_000, 20260914, 0x55);
    let stale = push(master, "bio.k_bio", 9_000_000, 20260912, 0x55);
    let (hs, hst) = (s.tx.hash(), stale.tx.hash());
    h.extra_txs.push(s);
    h.extra_txs.push(stale);
    h.tick();
    assert!(h.rejections.iter().any(|r| r.hash == hs && r.reason.contains("neither the master")), "stranger refused: {:?}", h.rejections);
    assert!(h.rejections.iter().any(|r| r.hash == hst && r.reason.contains("older than the committed")), "stale re-push refused: {:?}", h.rejections);
    assert_eq!(read_gauge(h.follower.state(), &k_bio).unwrap(), prod, "committed reading untouched by refused pushes");
    tick_until(&mut h, 10, "refusal tick settles", |h| h.chain.height() > prod.height + 3);
    assert_eq!(read_gauge(h.follower.state(), &k_bio).unwrap(), prod, "committed reading still untouched once the refusal block settled");
    let rev = push(master, "bio.k_bio", 1_700_000, 20260913, 0xB2); // same day, revised (NOAA revises recent days)
    h.extra_txs.push(rev);
    h.tick();
    tick_until(&mut h, 10, "the revision settles", |h| read_gauge(h.chain.state(), &k_bio).map(|g| g.nonce == 2).unwrap_or(false));
    let next = push(master, "bio.k_bio", 1_200_000, 20260914, 0xC3);
    h.extra_txs.push(next);
    h.tick();
    tick_until(&mut h, 10, "the next day settles", |h| read_gauge(h.chain.state(), &k_bio).map(|g| g.nonce == 3).unwrap_or(false));
    let g = read_gauge(h.follower.state(), &k_bio).unwrap();
    assert_eq!((g.value_e6, g.reading_date, g.nonce, g.attest_blake3), (1_200_000, 20260914, 3, [0xC3; 32]));
    assert_eq!(read_gauge(h.chain.state(), &k_bio).unwrap(), g);
    assert_eq!(read_gauge(h.chain.state(), &breathing).unwrap().nonce, 1, "the other feed did not move");
    assert!(sigil_oracle::gauge_is_fresh(&g, h.chain.height()));

    eprintln!(
        "chronos[gauge push]: activation at h={live_at}, {} blocks mirrored, k_bio nonce {} = {:.6} on {}, breathing {:.6} PgC/day — producer and follower agree",
        h.applied_blocks.len(), g.nonce, g.value_e6 as f64 / 1e6, g.reading_date, br.value_e6 as f64 / 1e6
    );
    sigil_oracle::chronos_schedule_gauge_live_height(u64::MAX);
    std::env::remove_var("SIGIL_DAG_FINAL_DEPTH");
}
