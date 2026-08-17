//! chronos_send_throughput.rs — REAL sustained send throughput (TPS) through
//! the actual money chokepoint (`coinbase::build_block_body_for_shares` +
//! `sigil_state::commit_state_transition`), under the SAME multi-candidate-
//! per-height churn that caused the send-orphan bug: a live capture showed
//! the braid minting THREE competing candidate blocks at one height before
//! settlement picked a winner. `SendBridge`'s confirm-on-settle pending pool
//! (v7.1.29) exists to survive that; this harness measures what it costs.
//!
//! Every send is a REAL signed submission through `SendBridge::submit` (real
//! ed25519 keypairs, real signature verification, the exact wire message
//! `window.sigilSign` produces) — this isn't a shortcut benchmark that skips
//! the auth path, it's the whole pipeline minus the HTTP layer and the real
//! network/DAG timing.
//!
//! Per simulated height: submit N new signed sends, mint `CANDIDATES`
//! independent competing block bodies against the SAME pre-height state
//! (mirrors `mint_next_block` cloning `state` fresh per candidate), commit
//! only the LAST one (arbitrary deterministic "winner"), confirm only ITS
//! included tx hashes. Everything embedded in a losing candidate stays
//! pending and rides the next height's attempts — exactly the live retry
//! path. A healthy run ends with pending_len()==0 and zero give-ups.
//!
//! Env vars:
//!   CHRONOS_SEND_HEIGHTS     settled heights to mint       (default 50_000)
//!   CHRONOS_SEND_PER_HEIGHT  new signed sends per height   (default 20)
//!   CHRONOS_SEND_CANDIDATES  competing candidates/height   (default 3 — the
//!                            measured live count)
//!   CHRONOS_SEND_SENDERS     distinct funded sender wallets (default 64)
//!   CHRONOS_SEND_SWEEP=1     ignore CANDIDATES; run {1,2,3,5,8} and print a
//!                            comparison table instead of one detailed run

use ed25519_dalek::{Signer, SigningKey};
use sigil_api::send::SendBridge;
use sigil_node::coinbase;
use sigil_state::{SigilState, StateMutation, StateTransition, WalletId, NATIVE};
use std::time::Instant;

/// Fund `n` fresh ed25519 sender wallets with a large NATIVE balance via a
/// single real `commit_state_transition` at height 0 — the same chokepoint
/// production funding (coinbase) goes through, just batched for setup speed.
fn fund_senders(state: &mut SigilState, n: usize) -> Vec<(SigningKey, WalletId)> {
    let senders: Vec<(SigningKey, WalletId)> = (0..n)
        .map(|i| {
            let mut seed = [0u8; 32];
            seed[..8].copy_from_slice(&(i as u64).to_le_bytes());
            let sk = SigningKey::from_bytes(&seed);
            let addr = sk.verifying_key().to_bytes();
            (sk, addr)
        })
        .collect();
    // Each test send moves only 1..1000 raw units (fractions of a SIGIL — see
    // `run()`'s amount calc), so 10,000 SIGIL/sender is enormous headroom —
    // but cap the TOTAL at 10% of the real 21M-SIGIL MAX_SUPPLY regardless of
    // how many senders the caller asks for, so a large CHRONOS_SEND_SENDERS
    // fails loud via the real commit_state_transition cap check rather than
    // silently funding past what production would ever allow.
    let want_each: u128 = 10_000 * 100_000_000; // 10,000 SIGIL, 8 decimals
    let budget = sigil_state::MAX_SUPPLY / 10;
    let fund_each = want_each.min(budget / (n as u128).max(1));
    let mutations: Vec<StateMutation> = senders
        .iter()
        .map(|(_, addr)| StateMutation::SetBalance { wallet: *addr, token: NATIVE, amount: fund_each })
        .collect();
    sigil_state::commit_state_transition(state, &StateTransition { at_height: 0, mutations }, 0)
        .expect("fund senders");
    senders
}

fn sign_send(sk: &SigningKey, from_hex: &str, to_hex: &str, amount: u128, nonce: u64) -> String {
    let msg = format!("sigil-rpc/v1|send|{from_hex}|{to_hex}|SIGIL|{amount}|nonce={nonce}");
    hex::encode(sk.sign(msg.as_bytes()).to_bytes())
}

struct RunResult {
    candidates: usize,
    heights: u64,
    submitted: u64,
    confirmed: u64,
    pending_at_end: usize,
    peak_pending: usize,
    elapsed_secs: f64,
}

fn run(heights: u64, per_height: u64, candidates: usize, n_senders: usize) -> RunResult {
    let mut state = SigilState::new();
    let senders = fund_senders(&mut state, n_senders);
    let sender_hex: Vec<String> = senders.iter().map(|(_, a)| hex::encode(a)).collect();
    let recipients: Vec<WalletId> = (0..256u32)
        .map(|i| {
            let mut w = [0u8; 32];
            w[..4].copy_from_slice(&i.to_le_bytes());
            w
        })
        .collect();
    let recipient_hex: Vec<String> = recipients.iter().map(hex::encode).collect();

    let bridge = SendBridge::new();
    let producer = coinbase::producer_wallet();
    let shares = std::collections::HashMap::from([(producer, 1u64)]);

    let mut submitted = 0u64;
    let mut nonce_ctr = 1u64;
    let mut peak_pending = 0usize;
    let start = Instant::now();

    for h in 1..=heights {
        for k in 0..per_height {
            let idx = ((h * per_height + k) % n_senders as u64) as usize;
            let ridx = ((h * per_height + k) % recipients.len() as u64) as usize;
            let amount: u128 = 1 + ((h + k) % 1000) as u128; // varied, never zero
            nonce_ctr += 1;
            let sig = sign_send(&senders[idx].0, &sender_hex[idx], &recipient_hex[ridx], amount, nonce_ctr);
            bridge
                .submit(&sender_hex[idx], &recipient_hex[ridx], amount, "SIGIL", &sig, nonce_ctr)
                .expect("submit must succeed — this harness never sends malformed requests");
            submitted += 1;
        }
        peak_pending = peak_pending.max(bridge.pending_len());

        // Mint `candidates` independent competing bodies against the SAME
        // pre-height `state` — exactly what `mint_next_block` does per
        // candidate (fresh `state.clone()` inside `build_block_body_for_shares`).
        let mut winner: Option<(StateTransition, Vec<[u8; 32]>)> = None;
        for c in 0..candidates {
            let block_txs = bridge.snapshot_for_mint();
            let (transition, _roots, _events, included) =
                coinbase::build_block_body_for_shares(&state, h, None, &block_txs, producer, &shares);
            if c == candidates - 1 {
                let hashes: Vec<[u8; 32]> = included.iter().map(|t| t.tx.hash()).collect();
                winner = Some((transition, hashes));
            }
            // every other candidate is discarded here — the orphan, modeled directly.
        }
        let (transition, included_hashes) = winner.expect("candidates >= 1");
        sigil_state::commit_state_transition(&mut state, &transition, h).expect("commit winner");
        bridge.confirm_applied(&included_hashes);
    }

    let elapsed = start.elapsed();
    let pending_at_end = bridge.pending_len();
    let confirmed = submitted - pending_at_end as u64;
    RunResult {
        candidates,
        heights,
        submitted,
        confirmed,
        pending_at_end,
        peak_pending,
        elapsed_secs: elapsed.as_secs_f64(),
    }
}

fn print_row(r: &RunResult) {
    let blk_s = r.heights as f64 / r.elapsed_secs;
    let tps = r.confirmed as f64 / r.elapsed_secs;
    println!(
        "candidates={:<3} heights={:<8} submitted={:<9} confirmed={:<9} pending_end={:<6} peak_pending={:<6} elapsed={:>7.2}s  →  {:>8.1} blk/s  {:>9.1} TPS",
        r.candidates, r.heights, r.submitted, r.confirmed, r.pending_at_end, r.peak_pending, r.elapsed_secs, blk_s, tps
    );
}

fn main() {
    let heights: u64 = std::env::var("CHRONOS_SEND_HEIGHTS").ok().and_then(|s| s.parse().ok()).unwrap_or(50_000);
    let per_height: u64 = std::env::var("CHRONOS_SEND_PER_HEIGHT").ok().and_then(|s| s.parse().ok()).unwrap_or(20);
    let n_senders: usize = std::env::var("CHRONOS_SEND_SENDERS").ok().and_then(|s| s.parse().ok()).unwrap_or(64);
    let sweep = std::env::var("CHRONOS_SEND_SWEEP").ok().as_deref() == Some("1");

    println!("=== chronos send-throughput — real SendBridge + real apply_tx/commit_state_transition ===");
    println!(
        "heights={heights}  per_height={per_height}  total_sends={}  senders={n_senders}",
        heights * per_height
    );

    if sweep {
        println!("\nsweeping CHRONOS_SEND_CANDIDATES over {{1,2,3,5,8}} (1 = no orphan churn, baseline)\n");
        let mut rows = Vec::new();
        for &c in &[1usize, 2, 3, 5, 8] {
            let r = run(heights, per_height, c, n_senders);
            print_row(&r);
            rows.push(r);
        }
        let baseline_tps = rows[0].confirmed as f64 / rows[0].elapsed_secs;
        println!("\n=== overhead vs candidates=1 (no orphan churn) ===");
        for r in &rows {
            let tps = r.confirmed as f64 / r.elapsed_secs;
            println!(
                "candidates={:<3} TPS={:>9.1}  ({:+.1}% vs baseline)",
                r.candidates, tps, (tps / baseline_tps - 1.0) * 100.0
            );
        }
        let unhealthy: Vec<&RunResult> = rows.iter().filter(|r| r.pending_at_end != 0).collect();
        if unhealthy.is_empty() {
            println!("\n✓ every sweep point drained to pending_len()==0 — no send was lost or stuck at any churn level tested.");
        } else {
            println!("\n✗ {} sweep point(s) ended with a nonzero pending backlog — see rows above.", unhealthy.len());
        }
    } else {
        let candidates: usize = std::env::var("CHRONOS_SEND_CANDIDATES").ok().and_then(|s| s.parse().ok()).unwrap_or(3);
        println!("candidates_per_height={candidates}\n");
        let r = run(heights, per_height, candidates, n_senders);
        print_row(&r);
        if r.pending_at_end == 0 && r.confirmed == r.submitted {
            println!("\n✓ every submitted send was confirmed — none lost to orphaned candidates, none abandoned.");
        } else {
            println!(
                "\n✗ {} of {} sends never confirmed (pending_end={}) — investigate before trusting this parameter set.",
                r.submitted - r.confirmed, r.submitted, r.pending_at_end
            );
        }
    }
}
