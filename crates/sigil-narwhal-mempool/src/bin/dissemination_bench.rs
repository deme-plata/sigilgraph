//! dissemination_bench — Phase E (SIGIL_BRAIDPOOL_v1_1.md §17): "compare
//! bandwidth/CPU against replication," measured, for a realistic batch size
//! (matching `SealPolicy::default()`'s target_txs=4096).
//!
//! The closed-form bandwidth math is already worked out exactly in
//! SIGIL_NARWHAL_MEMPOOL_v0.md §3.3 and SIGIL_BRAIDPOOL_v1_1.md §3.3:
//! per-shard size is ~1/k of the batch, total sender-side dispersal is
//! (k+parity)/k of the batch. What that math does NOT cover is the CPU cost
//! erasure coding actually adds — encoding and decoding are real work full
//! replication doesn't have to do at all. This binary measures THAT, on the
//! real `flux-aether` coder, for a batch shaped like what `BatchSealer`
//! would actually produce, not a toy size.
//!
//! Usage: dissemination_bench [tx_count] [validator_count] [k] [parity]

use std::time::Instant;

use sigil_narwhal_mempool::{
    dissemination::{reassemble_batch, shard_batch},
    types::{WorkerBatch, WorkerId},
};
use sigil_tx::{ed25519_keygen, ed25519_sign_tx, SigilTx};

fn make_batch(n: usize) -> WorkerBatch {
    let txs = (0..n)
        .map(|i| {
            let (sk, pk, wallet) = ed25519_keygen();
            let tx = SigilTx::Send { from: wallet, to: [1u8; 32], amount: i as u128, token: [0u8; 32], fee: 0 };
            ed25519_sign_tx(tx, &sk, &pk)
        })
        .collect();
    WorkerBatch::new(WorkerId(0), 1, txs)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let tx_count: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(4_096);
    let validator_count: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(7);
    let k: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(3);
    let parity: usize = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(4);
    let n = k + parity;

    println!("dissemination_bench: tx_count={tx_count} validator_count={validator_count} k={k} parity={parity} (n={n} shards)");
    if n != validator_count {
        println!("  note: k+parity={n} != validator_count={validator_count} — that's fine, this bench measures the coder, not a specific committee-size mapping.\n");
    } else {
        println!();
    }

    let batch = make_batch(tx_count);
    let header = batch.canonical_header([0u8; 32], 0, 1, None);
    let batch_bytes = serde_json::to_vec(&(&header, &batch)).unwrap().len();
    println!("batch: {tx_count} txs, {batch_bytes} bytes (header+batch, canonically encoded)\n");

    // ── bandwidth, exact (not estimated) ──────────────────────────────────
    let replication_total_bytes = batch_bytes * validator_count.saturating_sub(1);
    println!("── BANDWIDTH (exact byte counts) ──");
    println!("  full replication  ({} copies, one per OTHER validator): {replication_total_bytes} bytes total", validator_count.saturating_sub(1));

    // ── CPU: encode ─────────────────────────────────────────────────────
    let t0 = Instant::now();
    let shards = shard_batch(&header, &batch, k, parity);
    let encode_time = t0.elapsed();
    let shard_size = shards[0].bytes.len();
    let coded_total_bytes: usize = shards.iter().map(|s| s.bytes.len()).sum();
    println!("  erasure-coded     ({n} shards, {shard_size} bytes each): {coded_total_bytes} bytes total");
    println!(
        "  bandwidth ratio: {:.3}x ({})\n",
        coded_total_bytes as f64 / replication_total_bytes.max(1) as f64,
        if coded_total_bytes < replication_total_bytes { "coding sends FEWER total bytes" } else { "coding sends MORE total bytes" }
    );

    // ── CPU: decode (reconstruct from exactly k shards — the worst case
    // that still succeeds, since more surviving shards doesn't change the
    // decode algorithm's cost in this coder) ──────────────────────────────
    let orig_len = shards[0].orig_len;
    let mut sparse: Vec<Option<_>> = shards.into_iter().map(Some).collect();
    for s in sparse.iter_mut().skip(k) {
        *s = None; // keep exactly the first k, drop the rest
    }
    let t1 = Instant::now();
    let out = reassemble_batch(header.batch_id(), k, parity, orig_len, sparse);
    let decode_time = t1.elapsed();
    assert!(out.is_some(), "reconstruction from exactly k shards must succeed — a benchmark that silently failed reconstruction would be worse than no benchmark");
    let (_, out_batch) = out.unwrap();
    assert_eq!(out_batch.txs.len(), tx_count, "reconstructed batch must have every original tx");

    println!("── CPU (real flux-aether Reed-Solomon, this box) ──");
    println!("  encode ({n} shards from 1 batch):        {:.3} ms", encode_time.as_secs_f64() * 1000.0);
    println!("  decode (reconstruct from exactly {k} shards): {:.3} ms", decode_time.as_secs_f64() * 1000.0);
    println!(
        "  full replication's equivalent CPU cost: ~0 ms (no coding step — this IS the CPU replication trades away for simplicity)\n"
    );

    println!("── VERDICT (stated at the confidence this single-box, single-run measurement supports) ──");
    let bw_wins = coded_total_bytes < replication_total_bytes;
    println!(
        "  Bandwidth: {} for this (tx_count={tx_count}, validator_count={validator_count}, k={k}, parity={parity}) shape.",
        if bw_wins { "coding wins" } else { "replication wins" }
    );
    println!(
        "  CPU: coding costs {:.3} ms of REAL work per batch (encode+decode) that replication does not pay at all.",
        (encode_time + decode_time).as_secs_f64() * 1000.0
    );
    println!(
        "  This is ONE run, ONE box, ONE batch shape — a single data point, not a general claim. Re-run with different\n  \
         (tx_count, validator_count, k, parity) before treating any specific crossover point as a real threshold."
    );
}
