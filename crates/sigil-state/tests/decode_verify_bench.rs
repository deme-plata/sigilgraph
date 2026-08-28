//! LANE-B (rocky-sync-B) — per-stage decode+verify microbench for the SIGIL v3 SYNC SPRINT.
//!
//! MASTER GOAL: sustained >=20,000 blk/s, 0->tip, divergence=0, over WireGuard.
//! LANE-B owns the CPU half of the pipeline: zstd-inflate -> bincode-deserialize ->
//! verify (precheck + parent linkage). Before optimizing anything, MEASURE which of
//! those stages is actually the wall at the 20k bar — the swarm lead asked each lane
//! for a measured blk/s, not a compile.
//!
//! WHY THIS IS FAITHFUL (not a toy):
//!   * The wire codec is the EXACT prod path — the responder bincode-serializes a
//!     `Vec<SigilBlockHeaderV0>` chunk (capped at 4096 headers) and compresses it with
//!     the C `zstd` crate at level 1 (`'Z'` codec=1). The follower decodes with pure-Rust
//!     `ruzstd` (so the Windows cross-build stays mingw-clean), then
//!     `bincode::deserialize::<Vec<SigilBlockHeaderV0>>`. We reproduce both ends here.
//!   * `precheck()` is the real per-header verify cost (schema/network/sig-len/nonce +
//!     the `vdf_input == BLAKE3(parent_hash || nonce_sqisign)` binding — a BLAKE3 over
//!     ~324 B). Headers are constructed to PASS precheck, so we time the success path.
//!   * Parent linkage (since v0.33) is a 32-byte memcmp against the parent's STORED
//!     ingest hash — NOT a re-hash. We measure both that and the OLD `header.hash()`
//!     JSON-serialize ceiling, to show why v0.33's change mattered (it was the
//!     measured 26-52k blk/s cap).
//!
//! HONEST scope (VARFLOW checklist — what's still modeled):
//!   * Synthetic headers carry empty STARK/VDF/proof-bundle vecs by default, so a minimal
//!     header is ~0.7 KB. Real mature headers are larger (~2-8 KB) once VDF/STARK proofs
//!     are populated. INFLATE + DESERIALIZE are size-dependent, so we run a `pad` sweep
//!     (0 / ~1.5 KB / ~7.5 KB padded into `vdf_proof.pi`) to BRACKET the real decode cost.
//!     precheck/linkage are size-INDEPENDENT (they don't touch the padded fields), so
//!     those numbers are directly comparable across pad sizes.
//!   * This is a single-thread-per-stage microbench. It attributes per-stage cost; it does
//!     NOT model the staged-pipeline overlap (bounded channels) — that's the next lane
//!     deliverable, and this bench is the baseline it must beat.
//!
//! RUN (do NOT use raw cargo per the dogfood rule — but this prints via the test harness):
//!   flux_combo --package sigil-state    # compile + run (green/red)
//!   then run the test binary directly with --nocapture to SEE the numbers:
//!   ./target/debug/deps/decode_verify_bench-<hash> --nocapture --exact stages

use rayon::prelude::*;
use sigil_header::*;
use std::hint::black_box;
use std::io::Read;
use std::time::Instant;

/// Build a well-formed, precheck-passing header at `height` linking to `parent_hash`.
/// `pad` bytes are stuffed into `vdf_proof.pi` to model a heavier mature header without
/// perturbing the precheck path (precheck never reads `vdf_proof`).
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
        // Dormant on this chain (TOPOLOGY_COMMITMENT_ACTIVATION_HEIGHT is u64::MAX),
        // so `None` is what every real header carries — and what `hash()` strips.
        topology_commitment: None,
    }
}

/// A correctly-linked chain of `n` headers (heights 0..n), each parent_hash = hash of the
/// previous. Returns (chain, stored_hashes) where stored_hashes[i] = chain[i].hash() — the
/// 32-byte value ingest persists and linkage compares against (NOT recomputed in the walk).
fn mk_chain(n: u64, pad: usize) -> (Vec<SigilBlockHeaderV0>, Vec<BlockHash>) {
    let mut chain = Vec::with_capacity(n as usize);
    let mut hashes = Vec::with_capacity(n as usize);
    let mut parent = [0u8; 32];
    for h in 0..n {
        let hdr = mk_header(h, parent, pad);
        let hh = hdr.hash();
        parent = hh;
        hashes.push(hh);
        chain.push(hdr);
    }
    (chain, hashes)
}

/// pure-Rust ruzstd inflate — the EXACT decoder the follower runs in prod.
fn ruzstd_inflate(comp: &[u8]) -> Vec<u8> {
    let mut dec = ruzstd::StreamingDecoder::new(comp).expect("ruzstd new");
    let mut out = Vec::new();
    dec.read_to_end(&mut out).expect("ruzstd read_to_end");
    out
}

fn rate(n: u64, secs: f64) -> f64 {
    if secs <= 0.0 { f64::INFINITY } else { n as f64 / secs }
}

#[test]
fn stages() {
    const N: u64 = 20_000; // one VERIFY_BUDGET-ish slice (prod budget is 240k/1.5s)
    const CHUNK: usize = 4_096; // the responder's per-chunk header cap

    eprintln!("\n================ LANE-B decode+verify per-stage microbench ================");
    eprintln!("N={N} headers, chunk={CHUNK} (prod responder cap), codec='Z'(zstd lvl1)->ruzstd->bincode");
    eprintln!("bar = 20,000 blk/s sustained (MASTER GOAL). cores = {}", rayon::current_num_threads());
    eprintln!("---------------------------------------------------------------------------");

    for &pad in &[0usize, 1_500, 7_500] {
        // ---- setup (NOT timed): build the chain + the prod wire chunks ----
        let (chain, stored_hashes) = mk_chain(N, pad);
        let chunks: Vec<Vec<SigilBlockHeaderV0>> =
            chain.chunks(CHUNK).map(|c| c.to_vec()).collect();

        // producer side (reported FYI, not the follower's cost): serialize + compress.
        let mut raw_blobs: Vec<Vec<u8>> = Vec::with_capacity(chunks.len());
        let t = Instant::now();
        for c in &chunks {
            raw_blobs.push(bincode::serialize(c).expect("bincode ser"));
        }
        let ser_s = t.elapsed().as_secs_f64();

        let mut z_blobs: Vec<Vec<u8>> = Vec::with_capacity(chunks.len());
        let t = Instant::now();
        for b in &raw_blobs {
            z_blobs.push(zstd::encode_all(&b[..], 1).expect("zstd encode"));
        }
        let zenc_s = t.elapsed().as_secs_f64();

        let raw_bytes: usize = raw_blobs.iter().map(|b| b.len()).sum();
        let z_bytes: usize = z_blobs.iter().map(|b| b.len()).sum();
        let ratio = raw_bytes as f64 / z_bytes.max(1) as f64;
        let wire_bpb = z_bytes as f64 / N as f64;

        // ---- STAGE 1: INFLATE (follower, ruzstd) — REAL prod cost ----
        let t = Instant::now();
        let mut inflated: Vec<Vec<u8>> = Vec::with_capacity(z_blobs.len());
        for z in &z_blobs {
            inflated.push(ruzstd_inflate(z));
        }
        let inflate_s = t.elapsed().as_secs_f64();
        // soundness spot-check: inflate must reproduce the exact pre-compression bytes.
        assert_eq!(inflated[0], raw_blobs[0], "ruzstd must byte-match the zstd input");

        // ---- STAGE 2: DESERIALIZE (follower, bincode) — REAL prod cost ----
        let t = Instant::now();
        let mut decoded_total = 0usize;
        let mut decoded_chunks: Vec<Vec<SigilBlockHeaderV0>> = Vec::with_capacity(inflated.len());
        for inf in &inflated {
            let v: Vec<SigilBlockHeaderV0> = bincode::deserialize(inf).expect("bincode de");
            decoded_total += v.len();
            decoded_chunks.push(v);
        }
        let deser_s = t.elapsed().as_secs_f64();
        assert_eq!(decoded_total as u64, N, "all headers must round-trip");

        let all: Vec<SigilBlockHeaderV0> = decoded_chunks.into_iter().flatten().collect();

        // ---- STAGE 3a: PRECHECK serial ----
        let t = Instant::now();
        let mut ok = 0usize;
        for h in &all {
            if black_box(h.precheck()).is_ok() {
                ok += 1;
            }
        }
        let pc_ser_s = t.elapsed().as_secs_f64();
        assert_eq!(ok as u64, N, "every synthetic header must precheck OK");

        // ---- STAGE 3b: PRECHECK rayon (this is what verify_to_parallel does) ----
        let t = Instant::now();
        let ok_par = all
            .par_iter()
            .filter(|h| black_box(h.precheck()).is_ok())
            .count();
        let pc_par_s = t.elapsed().as_secs_f64();
        assert_eq!(ok_par as u64, N);

        // ---- STAGE 4: LINKAGE walk — 32-byte memcmp vs STORED hash (v0.33 path) ----
        let t = Instant::now();
        let mut linked = 0u64;
        let mut parent: Option<BlockHash> = None;
        for (i, h) in all.iter().enumerate() {
            if let Some(p) = parent.as_ref() {
                if black_box(&h.parent_hash) == black_box(p) {
                    linked += 1;
                }
            }
            parent = Some(stored_hashes[i]); // stored, not re-hashed
        }
        let link_s = t.elapsed().as_secs_f64();
        assert_eq!(linked, N - 1, "the spine links 1..N");

        // ---- CONTRAST: OLD verify ceiling — re-hash each header (JSON serialize) ----
        // This is the pre-v0.33 path. Shown to justify why linkage moved to stored-hash.
        let t = Instant::now();
        let mut acc = 0u8;
        for h in &all {
            acc ^= black_box(h.hash())[0];
        }
        let rehash_s = t.elapsed().as_secs_f64();
        black_box(acc);

        // ---- END-TO-END decode+verify (inflate+deser+precheck-par+linkage) ----
        let e2e_s = inflate_s + deser_s + pc_par_s + link_s;

        eprintln!(
            "\n[pad={pad}B]  wire={:.0} B/header  zstd ratio={:.1}x  (raw {:.1} MB -> z {:.1} MB)",
            wire_bpb, ratio, raw_bytes as f64 / 1e6, z_bytes as f64 / 1e6
        );
        eprintln!("  producer  bincode-ser : {:>10.0} blk/s   ({:.3}s)  [FYI, not follower cost]", rate(N, ser_s), ser_s);
        eprintln!("  producer  zstd-encode : {:>10.0} blk/s   ({:.3}s)  [FYI, not follower cost]", rate(N, zenc_s), zenc_s);
        eprintln!("  FOLLOWER inflate(ruzstd): {:>10.0} blk/s   ({:.3}s)", rate(N, inflate_s), inflate_s);
        eprintln!("  FOLLOWER deserialize    : {:>10.0} blk/s   ({:.3}s)", rate(N, deser_s), deser_s);
        eprintln!("  verify precheck SERIAL  : {:>10.0} blk/s   ({:.3}s)", rate(N, pc_ser_s), pc_ser_s);
        eprintln!("  verify precheck RAYON   : {:>10.0} blk/s   ({:.3}s)  <- verify_to_parallel", rate(N, pc_par_s), pc_par_s);
        eprintln!("  linkage memcmp (v0.33)  : {:>10.0} blk/s   ({:.3}s)", rate(N, link_s), link_s);
        eprintln!("  [old] re-hash ceiling   : {:>10.0} blk/s   ({:.3}s)  <- pre-v0.33 26-52k cap", rate(N, rehash_s), rehash_s);
        eprintln!("  ===> E2E decode+verify  : {:>10.0} blk/s   ({:.3}s)  vs 20k bar = {:.0}x headroom",
            rate(N, e2e_s), e2e_s, rate(N, e2e_s) / 20_000.0);
    }

    eprintln!("\n=> Read the SLOWEST 'FOLLOWER'/'verify' row per pad: THAT is LANE-B's wall.");
    eprintln!("   If E2E >> 20k at the realistic pad, decode+verify is NOT the binding constraint");
    eprintln!("   (the wire / commit / lock-contention is) — report the number to rocky-sync-lead.\n");
}
