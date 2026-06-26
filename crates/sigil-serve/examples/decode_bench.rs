//! Transport-decode throughput: sequential (the 50,499 blk/s wall) vs rayon-parallel.
//! Mirrors commit_pipeline_bench's `[transport]` row EXACTLY (N, CHUNK, pad, zstd lvl1)
//! so the number is directly comparable. Run release:
//!   cargo run --release -p sigil-serve --example decode_bench
//! Target: parallel must beat 50,499 → ≥100k.

use sigil_header::*;
use sigil_serve::{decode_zstd_header_pages_parallel, inflate_zstd, DEFAULT_MAX_INFLATE};
use std::time::Instant;

const CHUNK: usize = 4_096; // prod responder per-chunk header cap
const PAD: usize = 1_500; // realistic mature-ish header (matches the gate bench)

fn mk_header(height: u64, parent_hash: BlockHash, pad: usize) -> SigilBlockHeaderV0 {
    let nonce = SqiSignature::from_array([7u8; SQISIGN_L5_LEN]);
    let mut hh = blake3::Hasher::new();
    hh.update(&parent_hash);
    hh.update(nonce.as_bytes());
    let vdf_input: [u8; 32] = *hh.finalize().as_bytes();
    let scheme = SigScheme::SqiSign5;
    SigilBlockHeaderV0 {
        version: HEADER_VERSION,
        network_id: NETWORK_ID,
        height,
        parent_hash,
        merge_parents: Vec::new(),
        timestamp_ms: 1_000 + height,
        nonce_sqisign: nonce,
        vdf_input,
        vdf_proof: WesolowskiProof { y: vec![], pi: vec![0xABu8; pad], t: 100 },
        difficulty: 1,
        wallet_state_root: [0u8; 32],
        dex_state_root: [0u8; 32],
        event_log_root: [0u8; 32],
        contract_state_root: [0u8; 32],
        state_transition_proof: StarkProof { bytes: vec![], public_inputs_hash: [0u8; 32] },
        txs_merkle_root: [0u8; 32],
        tx_count: 0,
        fluxc_artifact_proof: ProofBundle {
            artifact_blake3: [0u8; 32],
            sqisign_sig: vec![],
            sqisign_pubkey: vec![],
            settle_tx: None,
        },
        sig_scheme: scheme,
        producer: [0u8; 32],
        producer_sig: SignatureBytes(vec![0u8; scheme.expected_sig_len()]),
    }
}

fn rate(n: u64, secs: f64) -> f64 {
    if secs <= 0.0 { f64::INFINITY } else { n as f64 / secs }
}

fn main() {
    let n: u64 = std::env::var("BENCH_N").ok().and_then(|v| v.parse().ok()).unwrap_or(20_000);
    // build chain
    let mut chain = Vec::with_capacity(n as usize);
    let mut parent = [0u8; 32];
    for h in 0..n {
        let hdr = mk_header(h, parent, PAD);
        parent = hdr.hash();
        chain.push(hdr);
    }
    // wire blobs (producer side, NOT timed): bincode -> zstd lvl1 ('Z' body)
    let z_blobs: Vec<Vec<u8>> = chain
        .chunks(CHUNK)
        .map(|c| {
            let raw = bincode::serialize(&c.to_vec()).expect("ser");
            zstd::encode_all(&raw[..], 1).expect("zstd")
        })
        .collect();
    eprintln!(
        "decode bench: N={n}, CHUNK={CHUNK} → {} pages, cores={}",
        z_blobs.len(),
        rayon::current_num_threads()
    );

    // (1) SEQUENTIAL — the EXACT commit_pipeline_bench [transport] loop (the wall).
    let best_seq = (0..5)
        .map(|_| {
            let t = Instant::now();
            let mut decoded: Vec<SigilBlockHeaderV0> = Vec::with_capacity(n as usize);
            for z in &z_blobs {
                let inf = inflate_zstd(z, DEFAULT_MAX_INFLATE).expect("inflate");
                let mut v: Vec<SigilBlockHeaderV0> = bincode::deserialize(&inf).expect("de");
                decoded.append(&mut v);
            }
            assert_eq!(decoded.len(), n as usize);
            rate(n, t.elapsed().as_secs_f64())
        })
        .fold(0.0f64, f64::max);

    // (2) PARALLEL — the fix.
    let mut last_len = 0usize;
    let best_par = (0..5)
        .map(|_| {
            let t = Instant::now();
            let decoded = decode_zstd_header_pages_parallel(&z_blobs, DEFAULT_MAX_INFLATE);
            last_len = decoded.len();
            rate(n, t.elapsed().as_secs_f64())
        })
        .fold(0.0f64, f64::max);
    assert_eq!(last_len, n as usize, "parallel decode must produce all blocks");

    eprintln!("(1) SEQUENTIAL inflate+deser : {best_seq:>11.0} blk/s   (baseline wall ~50,499)");
    eprintln!("(2) PARALLEL  (rayon, pages) : {best_par:>11.0} blk/s   ({:.1}x)   target ≥100k → {}",
        best_par / best_seq.max(1.0),
        if best_par >= 100_000.0 { "PASS ✓" } else { "BELOW" });
}
