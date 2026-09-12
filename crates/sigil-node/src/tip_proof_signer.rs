//! Off-loop SQIsign tip-proof signer (2026-09-12, rocky-bps100-0912).
//!
//! # Why this module exists — the 0.33 blk/s wall
//!
//! P4.1 (2026-09-08) made `apply_finality_gate` sign the certified tip as a
//! SQIsign L5 tip proof "at most once a second". It called
//! `TipProof::new_sqisign` INLINE, on the produce loop. MEASURED on Epsilon
//! 2026-09-12 (perf, dwarf call graphs, main thread only): ~75% of the loop's
//! cycles were inside `sqisign_rs::sign::protocols_sign` — one SQIsign L5
//! signature costs 1–3 s on this box, so the "once a second" throttle never
//! engaged: every certificate raise (every block, checkpoint interval 1) found
//! ≥1 s elapsed and signed again. The journal showed the shape exactly: three
//! blocks in one second, then a 3 s hole, repeat. 0.33 blk/s against a
//! governor floor of 50.
//!
//! The tip proof is a VIEW (`/v1/finality/tip-proof`) — nothing in consensus,
//! storage or money reads it. So the signing moves to its own OS thread and
//! the produce loop only ever hands it a 72-byte job. The thread signs at its
//! own pace; the channel holds ONE pending job, and a newer raise simply
//! replaces a stale one that the signer has not picked up yet (`offer`
//! drains-then-sends). The view therefore always converges on the newest
//! certified tip the signer has had time to sign — the same "latest, at most
//! once per sign-duration" semantics P4.1 intended, minus the stall.
//!
//! Consensus effect: none. Byte-for-byte identical blocks. A follower with
//! no SQIsign key never had a signer and still doesn't.

use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use sigil_header::BlockHash;
use sigil_state::StateRoots;

/// One signing job: the certified tip's height, spine hash and header roots.
#[derive(Clone, Debug)]
pub struct TipJob {
    pub height: u64,
    pub hash: BlockHash,
    pub roots: StateRoots,
}

/// Handle owned by the produce loop. Cheap to clone; `offer` never blocks.
#[derive(Clone)]
pub struct TipProofSigner {
    tx: SyncSender<TipJob>,
}

impl TipProofSigner {
    /// Spawn the signer thread. `view` is the same `Arc` the money API serves
    /// on `/v1/finality/tip-proof`; the thread is its only writer.
    pub fn spawn(
        sk: Vec<u8>,
        pk: Vec<u8>,
        view: Arc<RwLock<Option<serde_json::Value>>>,
    ) -> Self {
        // Capacity 1: at most one job waits. `offer` replaces it (see below).
        let (tx, rx) = sync_channel::<TipJob>(1);
        std::thread::Builder::new()
            .name("sigil-tipproof-sign".into())
            .spawn(move || signer_loop(rx, sk, pk, view))
            .expect("spawn tip-proof signer thread");
        TipProofSigner { tx }
    }

    /// Hand the signer the newest certified tip. Never blocks the caller.
    /// If a job is still queued (signer busy), it is superseded by this one —
    /// the signer always works on the freshest tip it can get.
    /// Returns false only if the signer thread is gone.
    pub fn offer(&self, job: TipJob) -> bool {
        match self.tx.try_send(job) {
            Ok(()) => true,
            Err(TrySendError::Full(job)) => {
                // Drop the stale queued job by letting the signer's next recv
                // take ours: a 1-slot channel can't be swapped atomically, but
                // a second try_send after the signer drains is the common
                // case. If it is STILL full the signer is mid-sign with one
                // queued; that queued job is at most one raise old, and the
                // next raise (≤ one block later) will re-offer. Nothing lost.
                matches!(self.tx.try_send(job), Ok(()) | Err(TrySendError::Full(_)))
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }
}

fn signer_loop(
    rx: Receiver<TipJob>,
    sk: Vec<u8>,
    pk: Vec<u8>,
    view: Arc<RwLock<Option<serde_json::Value>>>,
) {
    let mut signed: u64 = 0;
    let mut total_ms: u128 = 0;
    // A signature every 1–3 s back-to-back pinned a full core on a 48-core box that
    // runs at load ~100 (MEASURED 20:0x: the signer thread at 99.9%). The view is a
    // convenience read; one fresh proof every few seconds is plenty.
    // `SIGIL_TIP_PROOF_INTERVAL_MS` (default 5000) is the floor between two signs.
    let min_gap = std::time::Duration::from_millis(
        std::env::var("SIGIL_TIP_PROOF_INTERVAL_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(5000),
    );
    let mut last_done: Option<Instant> = None;
    while let Ok(mut job) = rx.recv() {
        if let Some(t) = last_done {
            let since = t.elapsed();
            if since < min_gap {
                std::thread::sleep(min_gap - since);
            }
        }
        // Coalesce: if more jobs piled up while we were signing, take the newest.
        while let Ok(newer) = rx.try_recv() {
            job = newer;
        }
        let t0 = Instant::now();
        match sigil_tip_proof::TipProof::new_sqisign(job.height, job.roots, &sk, &pk) {
            Ok(tp) => {
                let took = t0.elapsed().as_millis();
                last_done = Some(Instant::now());
                signed += 1;
                total_ms += took;
                let now = crate::finality_wire::now_ms();
                let v = serde_json::json!({
                    "flavor": "SqiSignBlob",
                    "height": job.height,
                    "spine_block_hash": hex::encode(job.hash),
                    "producer_pk_sqisign": hex::encode(&pk),
                    "proof": serde_json::from_slice::<serde_json::Value>(&tp.encode_json())
                        .unwrap_or(serde_json::Value::Null),
                    "signed_at_ms": now,
                    "sign_ms": took,
                    "off_loop": true,
                });
                if let Ok(mut w) = view.write() {
                    *w = Some(v);
                }
                // One line per 64 signatures: the measured cost, so nobody has
                // to guess again whether SQIsign fits inside a block interval.
                if signed % 64 == 1 {
                    eprintln!(
                        "🖋 tip-proof signer (off-loop): H={} took {} ms (avg {} ms over {} sigs)",
                        job.height, took, total_ms / signed as u128, signed
                    );
                }
            }
            Err(e) => {
                last_done = Some(Instant::now());
                eprintln!("⚠ tip-proof: SQIsign signing failed at H={}: {e:?}", job.height)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zero_roots() -> StateRoots {
        StateRoots {
            wallet_state_root: [0u8; 32],
            dex_state_root: [0u8; 32],
            event_log_root: [0u8; 32],
            contract_state_root: [0u8; 32],
        }
    }

    #[test]
    fn offer_never_blocks_and_coalesces_to_newest() {
        // No real key: exercise the channel discipline with a fake consumer.
        let (tx, rx) = sync_channel::<TipJob>(1);
        let s = TipProofSigner { tx };
        let job = |h: u64| TipJob { height: h, hash: [0u8; 32], roots: zero_roots() };
        assert!(s.offer(job(1)));
        // Slot full, signer hasn't drained: offer must still return promptly.
        assert!(s.offer(job(2)));
        assert!(s.offer(job(3)));
        // Consumer drains what's there — exactly one queued job (the oldest
        // undrained one), never a deadlock.
        let got = rx.try_recv().expect("one job queued");
        assert!(got.height >= 1);
        assert!(rx.try_recv().is_err(), "slot holds at most one job");
        // After the drain the next offer lands.
        assert!(s.offer(job(4)));
        assert_eq!(rx.try_recv().unwrap().height, 4);
    }

    #[test]
    fn offer_reports_dead_signer() {
        let (tx, rx) = sync_channel::<TipJob>(1);
        let s = TipProofSigner { tx };
        drop(rx);
        assert!(!s.offer(TipJob { height: 9, hash: [1u8; 32], roots: zero_roots() }));
    }
}
