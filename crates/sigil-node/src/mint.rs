//! mint.rs — block minting. Shared by the `sigil-node` binary and any external
//! crate that needs to mint the SAME way (e.g. `sigil-top`'s `producer` feature —
//! see `producer/mint.rs` there).
//!
//! 2026-08-23 (grogu-producer-unification Phase 2): moved out of `main.rs` so
//! sigil-top can call the REAL `mint_next_block()` instead of maintaining a
//! hand-ported duplicate that could silently drift from what the live producer
//! actually mints. Dual-declared in both `main.rs` (`mod mint;`) and `lib.rs`
//! (`pub mod mint;`) — same pattern as `genesis`/`dag`/`coinbase`. Takes
//! everything as explicit parameters (no captured main.rs-local state), so the
//! move is a pure relocation — zero behavior change, verified by running
//! sigil-node's existing mint test group after the move.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use sigil_header::{
    ProofBundle, SigScheme, SigilBlockHeaderV0, SignatureBytes, SqiSignature, StarkProof,
    WesolowskiProof, HEADER_VERSION, NETWORK_ID, SQISIGN_L5_LEN,
};
use sigil_header::BlockHash;
use sigil_state::WalletId;
use sigil_tx::SignedTx;

use crate::block::Block;
use crate::chain::ChainTip;

/// Mint the next block on top of `chain`, embedding `txs` (already-authenticated —
/// see each bridge's own `submit`) and, if a verified mining solve is available,
/// crediting it real dual-lane PoW. `merge_parents`/`topology_commitment` come from
/// the caller's DAG braid (see `crate::dag`) — a block can't be minted without
/// knowing its DAG parents, which is why this function and `dag::dag_build_frontier`
/// always get ported/called together.
pub fn mint_next_block(
    chain: &ChainTip,
    merge_parents: Vec<BlockHash>,
    txs: &[SignedTx],
    reward_override: Option<u128>,
    solve: Option<&sigil_api::mining::AcceptedSolve>,
    topology_commitment: Option<[u8; 32]>,
    // Partial-share pool drained for THIS block, used only when `solve` is `None`.
    // See the `None` arm below and `MiningBridge::take_share_pool`.
    share_pool: Option<std::collections::HashMap<WalletId, u64>>,
) -> Result<(Block, Vec<[u8; 32]>)> {
    let height = chain.height();
    let parent = chain.parent_hash();
    // P1 mining-onto-the-braid: when this block is minted for a verified
    // dual-lane solve, the 292-byte nonce field carries the winning
    // (nonce ‖ blake4_hash) — the same carrier layout the ledger header uses —
    // and the real Wesolowski proof rides in `vdf_proof`. A follower rebuilds
    // the challenge from (parent_hash, height, producer) and re-verifies BOTH
    // lanes: `sigil_api::mining::verify_header_pow`. Without a solve the fields
    // stay zeroed (the free-running dyno, unchanged).
    let nonce = match solve {
        Some(s) => SqiSignature::from_array(sigil_api::mining::pack_nonce_carrier(s.nonce, s.blake4_hash)),
        None => SqiSignature::from_array([0u8; SQISIGN_L5_LEN]),
    };
    // vdf_input MUST satisfy header.precheck — derived through the ONE shared
    // function precheck itself uses, so the two can never drift.
    let vdf_input = sigil_header::vdf_input_from(&parent, nonce.as_bytes());

    // ONE-CHAIN step 1: the braid mints a REAL coinbase — the producer credits
    // itself block_reward(height) through the shared money chokepoint, so money
    // now lives IN the graph (not a separate rpcd chain). The reward mutation is
    // in the block body, so every follower re-applies it and the root-match check
    // in ChainTip::apply passes. SIGIL_COINBASE=0 restores the empty-block dyno.
    // ONE-CHAIN: the full block body = coinbase + user sends, applied in order
    // against the evolving state. reward: SIGIL_COINBASE=0 → no coinbase; the
    // adaptive controller (if live) supplies the exact amount; else the height
    // schedule. Sends flow through apply_tx → real balance moves on the braid.
    let coinbase_on = std::env::var("SIGIL_COINBASE").map(|v| v != "0").unwrap_or(true);
    let state = chain.state_snapshot();
    let reward = if coinbase_on { reward_override } else { Some(0u128) };
    // Mined block → the reward is split over the verified solves for this
    // height (pool-share economics: dev-fee + commons + proportional payout,
    // winner absorbs the remainder); otherwise the node's configured producer
    // wallet takes the whole thing (a one-entry "shares" map, which is exactly
    // what the split degenerates to — no behavior change from before this was
    // wired unless a master wallet is genesis-committed on this chain).
    let (winner, shares): (WalletId, std::collections::HashMap<WalletId, u64>) = match solve {
        Some(s) => (s.wallet, s.shares.clone()),
        None => {
            // 2026-08-26 — OPTION C (operator-directed). This arm used to pay the
            // producer's own wallet 100% of the miner slice on every self-minted
            // block. Measured live that day: the producer free-runs at ~5 blk/s
            // while difficulty targets one miner block-win per 120 s, so real
            // miners took 0.1% of emissions for 37% of network hashrate and the
            // producer wallet took 93.8%. No single function was buggy — the mint
            // cadence and the target-win cadence were never reconciled.
            //
            // Now: if real work arrived since the last block, this block pays THAT
            // work, split proportionally by proven hashes (see `share_work_weight`).
            // The producer stays `winner`, so it absorbs only the rounding
            // remainder — it did no proof-of-work for this block and should not be
            // paid as though it had.
            //
            // Behaviour is UNCHANGED unless the operator sets SIGIL_PAY_SHARE_POOL=1,
            // so any other node building this code is byte-for-byte unaffected.
            let w = crate::coinbase::producer_wallet();
            let pay_pool = std::env::var("SIGIL_PAY_SHARE_POOL")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false);
            match share_pool {
                Some(pool) if pay_pool && !pool.is_empty() => (w, pool),
                _ => (w, std::collections::HashMap::from([(w, 1u64)])),
            }
        }
    };
    let (transition, roots, block_events, included_txs) =
        crate::coinbase::build_block_body_for_shares(&state, height, reward, txs, winner, &shares);
    // Commit the verify-once txs: a sequential BLAKE3 root over their intent
    // hashes + the count. The signatures were verified ONCE at mempool ingest;
    // the producer-sig over this header binds the producer to this exact set.
    let txs_root = {
        let mut th = blake3::Hasher::new();
        for t in &included_txs { th.update(&t.tx.hash()); }
        *th.finalize().as_bytes()
    };

    let mut header = SigilBlockHeaderV0 {
        version: HEADER_VERSION,
        network_id: NETWORK_ID,
        height,
        parent_hash: parent,
        merge_parents,
        timestamp_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        nonce_sqisign: nonce,
        vdf_input,
        vdf_proof: match solve {
            Some(s) => WesolowskiProof { y: s.vdf.y.clone(), pi: s.vdf.pi.clone(), t: s.vdf.t },
            None => WesolowskiProof { y: vec![], pi: vec![], t: 0 },
        },
        difficulty: solve.map(|s| s.bits as u64).unwrap_or(0),
        wallet_state_root: roots.wallet_state_root,
        dex_state_root: roots.dex_state_root,
        event_log_root: roots.event_log_root,
        contract_state_root: roots.contract_state_root,
        state_transition_proof: StarkProof { bytes: vec![], public_inputs_hash: [0u8; 32] },
        txs_merkle_root: txs_root,
        tx_count: included_txs.len() as u32,
        fluxc_artifact_proof: ProofBundle {
            artifact_blake3: [0u8; 32],
            sqisign_sig: vec![],
            sqisign_pubkey: vec![],
            settle_tx: None,
        },
        sig_scheme: SigScheme::SqiSign5,
        producer: match solve {
            // The miner's wallet — the work is bound to it (the header the
            // miner hashed commits to this exact wallet), so it can't be
            // re-pointed. Untouched by producer-signing: repointing this
            // would invalidate the PoW solution.
            Some(s) => s.wallet,
            // No solve (the empty-block dyno / self-mined path) — this field
            // was always an unused `[0u8;32]` placeholder here (nothing
            // hashed against it). Real post-quantum SQIsign5 signing costs
            // ~1.16s (measured) — far too slow for every block — so it only
            // runs on periodic checkpoint heights (see `producer_signing::
            // HYBRID_CHECKPOINT_INTERVAL`); every other self-mined block uses
            // the fast Ed25519-only identity. Deciding this HERE (not just at
            // the signing call below) matters: if `producer` pointed at the
            // hybrid identity on a non-checkpoint block, the Ed25519-only
            // signer below would see a mismatch and skip too, leaving the
            // block silently unsigned. Falls to `[0u8;32]` — unchanged legacy
            // behavior — when nothing's opted in.
            None => {
                if crate::producer_signing::is_hybrid_checkpoint(height) {
                    crate::producer_signing::configured_hybrid_producer_wallet()
                        .or_else(crate::producer_signing::configured_signing_wallet)
                        .unwrap_or([0u8; 32])
                } else {
                    crate::producer_signing::configured_signing_wallet().unwrap_or([0u8; 32])
                }
            }
        },
        producer_sig: SignatureBytes(vec![0u8; SQISIGN_L5_LEN]),
        topology_commitment,
    };
    // Real signature if (and only if) the operator opted in AND this is the
    // self-mined path with a matching producer wallet — see
    // `producer_signing`'s module doc. Hybrid (SQIsign5+Ed25519, real
    // post-quantum protection) only attempted on checkpoint heights — see the
    // `producer:` field comment above for why the gate has to be consistent
    // between the two. Each function is independently a no-op (byte-for-byte
    // unchanged header) unless its own env vars AND the producer match are
    // both right, so calling both in sequence is safe either way.
    if crate::producer_signing::is_hybrid_checkpoint(height) {
        crate::producer_signing::maybe_sign_hybrid(&mut header);
    }
    crate::producer_signing::maybe_sign(&mut header);
    // Hashes of the txs that actually made it into `transition` (a STRICT
    // subset of the `txs` argument — build_block_body_for_shares silently
    // skips anything that failed apply_tx/commit_state_transition, e.g.
    // insufficient balance). The caller needs this list to know which
    // SendBridge-pending sends to retire ONLY once THIS candidate is
    // confirmed on the settled spine — not at mint time, when it's still
    // just one of possibly several competing candidates at this height.
    let included_tx_hashes: Vec<[u8; 32]> = included_txs.iter().map(|t| t.tx.hash()).collect();
    Ok((Block { header, transition, events: block_events }, included_tx_hashes))
}
