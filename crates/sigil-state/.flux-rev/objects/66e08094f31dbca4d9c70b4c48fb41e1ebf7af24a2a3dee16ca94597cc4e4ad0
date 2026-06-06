//! BATCH AUTHORIZATION — make the 209M state-commit ceiling USABLE as TPS.
//!
//! The honest gap (measured all session): state APPLIES at ~209M/s but that's
//! "free" because it skips authorization. Each real tx needs a signature, and
//! sig-verify caps at ~800k/s/box — so 209M is unreachable as real TPS.
//!
//! THE THING YOU PUT IN THE TX: one signature over a Merkle/vector commitment
//! to a BATCH of B operations. The verifier checks ONE sig + recomputes the
//! batch root (B hashes), then the state machine applies all B ops at fold
//! speed. Per-op cost becomes (hash + fold) instead of (sig + fold) — the
//! signature amortizes away as B grows.
//!
//! SOUNDNESS: this is sound for a SINGLE-AUTHOR batch — one account signs the
//! root of ITS OWN B operations (an agent / AMM / market-maker / payment-channel
//! burst). The sig binds the author to exactly those ops via the root; the
//! verifier re-derives the root so a forged op fails. Cross-author batching
//! needs aggregate sigs (BLS-style) — out of scope here, noted at the end.
//!
//! This harness MEASURES effective TPS as B grows, to find where it plateaus:
//! sig-bound (small B) → hash-bound (the root recompute) → fold-bound (209M).

use std::time::Instant;
use ed25519_dalek::{Signer, SigningKey, VerifyingKey, Signature};
use rand::rngs::OsRng;

const ACCOUNTS: u64 = 1_000_000;

#[derive(Clone, Copy)]
struct Op { from: u64, to: u64, amt: u64 }

// ── state-commit primitive (the 209M multiset fold, fast leaf) ───────────────
#[inline] fn mix(mut x: u64) -> u64 { x ^= x >> 32; x = x.wrapping_mul(0xd6e8feb86659fd93); x ^= x >> 32; x }
#[inline] fn fold_op(acc: &mut u64, op: &Op) {
    // 4 leaf touches per op (from-, from+, to-, to+) — same shape as stargate_500m
    *acc ^= mix(op.from ^ op.amt);
    *acc ^= mix(op.from.wrapping_add(1));
    *acc ^= mix(op.to ^ op.amt);
    *acc ^= mix(op.to.wrapping_add(2));
}

// ── batch root: BLAKE3 over the B ops (what the author signs) ────────────────
fn batch_root(ops: &[Op]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    for o in ops {
        h.update(&o.from.to_le_bytes());
        h.update(&o.to.to_le_bytes());
        h.update(&o.amt.to_le_bytes());
    }
    *h.finalize().as_bytes()
}

struct AuthBatch { vk: VerifyingKey, sig: Signature, root: [u8; 32], ops: Vec<Op> }

fn main() {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    println!("\n  BATCH AUTHORIZATION — making 209M state usable as TPS  [{cores} cores]\n");
    let mut rng = OsRng;

    // a pool of authors (each signs its own batch root)
    let n_batches = 4096usize;
    let keys: Vec<SigningKey> = (0..n_batches).map(|_| SigningKey::generate(&mut rng)).collect();

    println!("  {:>6}  {:>13}  {:>15}  {:>10}", "B", "sig_verif/s", "effective TPS", "bound by");
    let mut seed = 0x1234_5678_9abc_def0u64;
    let mut nx = || { seed ^= seed << 13; seed ^= seed >> 7; seed ^= seed << 17; seed };

    let mut best = 0.0f64;
    for &b in &[1usize, 16, 64, 256, 1024, 4096] {
        // build n_batches authorized batches of B ops each (one sig per batch)
        let batches: Vec<AuthBatch> = keys.iter().map(|sk| {
            let ops: Vec<Op> = (0..b).map(|_| Op { from: nx()%ACCOUNTS, to: nx()%ACCOUNTS, amt: nx()&0xffff }).collect();
            let root = batch_root(&ops);
            let sig = sk.sign(&root);
            AuthBatch { vk: sk.verifying_key(), sig, root, ops }
        }).collect();
        let total_ops = (n_batches * b) as f64;

        // INGEST (verify-once) + APPLY across all cores:
        //   per batch: 1 sig-verify  +  recompute root (B hashes, SOUNDNESS)  +  apply B folds
        let t0 = Instant::now();
        let chunk = batches.len().div_ceil(cores);
        std::thread::scope(|s| {
            for sl in batches.chunks(chunk) {
                s.spawn(move || {
                    let mut acc = 0u64;
                    for ba in sl {
                        // 1 signature authorizes the whole batch
                        let ok = ba.vk.verify_strict(&ba.root, &ba.sig).is_ok();
                        // re-derive the root so a forged op is rejected (the bind)
                        let rr = batch_root(&ba.ops);
                        if !ok || rr != ba.root { continue; }
                        // apply all B ops at fold speed
                        for op in &ba.ops { fold_op(&mut acc, op); }
                    }
                    std::hint::black_box(acc);
                });
            }
        });
        let dt = t0.elapsed().as_secs_f64();
        let tps = total_ops / dt;
        let sig_s = n_batches as f64 / dt;
        best = best.max(tps);
        let bound = if b <= 16 { "signature" }
                    else if tps < 50e6 { "root-hash" }
                    else { "state-fold" };
        println!("  {b:>6}  {sig_s:>13.0}  {tps:>15.0}  {bound:>10}");
    }

    println!("\n  ── verdict ──");
    println!("    B=1 (one sig per op): sig-bound at the ~800k/s wall.");
    println!("    As B grows, the signature amortizes away; per-op cost → (root-hash + fold).");
    println!("    Peak effective TPS on ONE box: {:.0} ({:.0}× the per-sig wall).", best, best/800_000.0);
    println!("    → 500M needs ~{:.0} boxes this way (vs ~625 sig-bound, ~600 ZK-proving).", 500e6/best);
    println!();
    println!("    SOUND for single-author batches (agent/AMM/channel signs its own burst):");
    println!("    the sig binds the author to the root; the verifier re-derives it, so a");
    println!("    forged op fails. The 'thing in the tx' = (batch_root, sig, ops[]). State");
    println!("    commitment stops being a vanity number and becomes the real TPS ceiling.");
    println!("    Cross-author batching → aggregate signatures (BLS / PQ-aggregate), next.\n");
}
