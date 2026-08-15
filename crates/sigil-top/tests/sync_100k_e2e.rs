//! V7-GATE (agent v7-gate) — SIGIL fast-sync **HONEST END-TO-END 100k ACCEPTANCE GATE**
//! for the sigil-top **v7.0.0** cut (live is v6.0.x). This is the file `@rocky` (release
//! cutter) waits on: the v7.0.0 tag is BLOCKED until this gate is GREEN.
//!
//! ## What this adds over LANE-4's `commit_pipeline_bench.rs` (rocky-lane4, the COMPONENT gate)
//!   `commit_pipeline_bench.rs` measures each prod primitive **in isolation** and a **30k
//!   burst** staged pipeline whose overlap is "reported, not gated". Those numbers (commit
//!   92.6k–485k, full-verify 6.6M) are real but they are NOT an end-to-end sustained claim:
//!   a 30k burst drains a buffer; a stage measured alone never pays the pipeline's
//!   back-pressure + the SLOWEST stage's tax. This file composes ALL the CPU stages of the
//!   real client path (inflate → decode → verify → commit) and asserts the things a 100k
//!   headline REQUIRES and a burst bench cannot:
//!     1. **Per-stage breakdown at scale** so the bottleneck is *visible* (it is the
//!        pure-Rust ruzstd inflate wall — measured ~70–80k blk/s, BELOW the 100k bar).
//!     2. **Sustained throughput** over a long window (≥1M blocks ≈ ≥10s @100k), with a
//!        fixed warm-up discard and a **rolling-window** check so a burst that drains a
//!        bounded buffer cannot masquerade as sustained.
//!     3. **Coordinated-omission-aware latency-under-load**: pages are scheduled at the
//!        *intended* 100k arrival cadence and service latency is measured from the INTENDED
//!        time — so a pipeline that merely *survives* bursts by buffering is exposed as
//!        latency blows up, instead of being hidden behind goodput.
//!     4. **No silent drops**: `finished == scheduled` (the single most common benchmark lie
//!        — dropping/skipping late work — is asserted away).
//!
//! ## Methodology (consulted DeepSeek-reasoner 2026-06-26 on coordinated omission +
//! ## sustained-vs-burst + warmup; rules baked in below):
//!   * **Coordinated omission**: intended[i] = start + interval*i (interval = CHUNK/TARGET);
//!     latency = finish - intended, NEVER finish - pickup. A stalled pipeline → later pages
//!     finish far past intended → latency climbs linearly → caught by the p99/max bound.
//!   * **Sustained**: window ≥1M blocks after warmup; the headline rate is
//!     total_blocks / (last_finish - first_intended); a rolling 1s sub-window dipping below
//!     target invalidates the claim. Bounded channels (cap) ⇒ a stage that can't keep up
//!     back-pressures the feeder (no unbounded buffer to hide behind — same OOM-safety the
//!     live launch() needs; cf. the 20→55GB pending_solutions spiral in CLAUDE.md).
//!   * **Warmup**: discard a FIXED first WARMUP_BLOCKS (alloc, file create, zstd/blake3 ctx,
//!     rayon pool spin-up). The window is `[WARMUP, WARMUP+SUSTAIN)` — chosen by constant,
//!     never hand-picked post-hoc.
//!   * **Composed number**: we REPORT min(stage rates) only as a *diagnosis* and we MEASURE
//!     the actual pipelined sustained rate as the *verdict*. They differ under contention /
//!     back-pressure, so the verdict is always the measured pipeline rate (DeepSeek §4).
//!
//! ## Honest scope (VARFLOW — what is modeled vs real)
//!   * **REAL prod primitives**: `zstd` lvl-1 page encoding (producer side, untimed) → pure-
//!     Rust `ruzstd` inflate (the EXACT follower decoder) → `bincode` deserialize → `rayon`
//!     per-header `precheck()` + parent-linkage memcmp → `flux_db::SkeletonStore` durable
//!     flat append. This is the sigil-top `block_sync` CPU pipeline, byte-for-byte.
//!   * **MODELED (documented, not hidden)**:
//!     - FETCH (network) is NOT measured here — a virtual-time bench has no socket. Fetch +
//!       producer serve-rate is the job of the **2-node real demo** (`~/sigil-release-gate/
//!       sync-100k-2node.sh`), which the gate runs separately. This file is the CLIENT-CPU
//!       ceiling: if the client CPU pipeline can't hit 100k, no network can rescue it.
//!     - To run 1M+ blocks without 1M live headers in RAM, ONE representative compressed page
//!       (CHUNK headers, realistic pad) is built once and **replayed** through inflate+verify
//!       (per-page CPU cost is content-independent — this is a throughput ceiling, stated
//!       plainly), while commit gets FRESH contiguous skeleton records each page so the
//!       flat-store append + contiguity cost is fully real.
//!   * Because of the modeled fetch, a GREEN here means "client CPU sustains 100k"; the cut
//!     also requires the 2-node demo GREEN. The gate script ANDs them. Neither alone ships v7.
//!
//! ## Run (dogfood — NOT raw cargo; build the test binary, run it directly)
//!   ionice -c3 nice -n19 /home/storage/deepseek-codewhale/flux/target/debug/fluxc \
//!       build --release -p sigil-top --no-default-features --test sync_100k_e2e
//!   ./target/release/deps/sync_100k_e2e-<hash> --nocapture --test-threads=1
//!     # --nocapture to SEE the per-stage + sustained + latency report
//!     # --test-threads=1 so the timed sections don't contend for cores
//!   # tunables (defaults are the real spec; smaller only for a smoke run):
//!   #   SIGIL_GATE_TARGET_BLKS=100000  SIGIL_GATE_SUSTAIN_BLOCKS=1000000
//!   #   SIGIL_GATE_WARMUP_BLOCKS=200000  SIGIL_GATE_ENFORCE=1 (hard-panic on RED in-test)
//!
//! The machine-parseable verdict line (the gate script greps this):
//!   `GATE_RESULT stage=<name> sustained_blk_s=<N> target=<N> verdict=<GREEN|RED>`

use flux_db::skeleton::{SkeletonRecord as DbSkeletonRecord, SkeletonStore};
use rayon::prelude::*;
use sigil_header::*;
use std::hint::black_box;
use std::io::Read;
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ───────────────────────────── tunables / bars ─────────────────────────────

/// The v7.0.0 headline: sustained client-CPU sync throughput.
fn target_blk_s() -> f64 {
    env_f64("SIGIL_GATE_TARGET_BLKS", 100_000.0)
}
/// Steady-state window length (≈10s @100k). The number the verdict is computed over.
fn sustain_blocks() -> u64 {
    env_u64("SIGIL_GATE_SUSTAIN_BLOCKS", 1_000_000)
}
/// Fixed warm-up discard (alloc / file-create / ctx / rayon spin-up).
fn warmup_blocks() -> u64 {
    env_u64("SIGIL_GATE_WARMUP_BLOCKS", 200_000)
}
/// If set, the test itself hard-fails on RED (CI mode). Default: report-only — the gate
/// SCRIPT parses GATE_RESULT and owns the exit code, so the harness stays runnable on RED.
fn enforce_in_test() -> bool {
    std::env::var("SIGIL_GATE_ENFORCE").map(|v| v == "1").unwrap_or(false)
}

const CHUNK: usize = 4_096; // prod responder per-chunk header cap
/// Bounded channel depth between stages. Small ⇒ real back-pressure, bounded in-flight RAM.
const CAP: usize = 8;
/// CO max-latency ceiling: a page must complete within this of its INTENDED arrival or the
/// pipeline is buffering, not sustaining. 5s is generous (DeepSeek §5); steady state is ms.
const CO_MAX_LATENCY: Duration = Duration::from_secs(5);
/// CO p99 ceiling for steady state.
const CO_P99_LATENCY: Duration = Duration::from_millis(750);

fn env_u64(k: &str, d: u64) -> u64 {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}
fn env_f64(k: &str, d: f64) -> f64 {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

// ───────────────────────────── shared helpers ──────────────────────────────

fn rate(n: u64, secs: f64) -> f64 {
    if secs <= 0.0 {
        f64::INFINITY
    } else {
        n as f64 / secs
    }
}

/// Machine-parseable verdict line for the gate script + a human line.
fn report(stage: &str, sustained: f64, target: f64) -> bool {
    let green = sustained >= target;
    eprintln!(
        "  {stage:<22} {sustained:>12.0} blk/s  vs {target:.0} bar → {}",
        if green { "GREEN ✓" } else { "RED ✗" }
    );
    println!(
        "GATE_RESULT stage={stage} sustained_blk_s={sustained:.0} target={target:.0} verdict={}",
        if green { "GREEN" } else { "RED" }
    );
    green
}

fn scratch(tag: &str) -> std::path::PathBuf {
    let base = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::path::Path::new(&base).join(format!("sigil-v7gate-{tag}-{pid}-{nanos}"));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Well-formed, precheck-passing header (mirrors commit_pipeline_bench::mk_header so both
/// rigs measure the SAME header shape). `pad` models a heavier mature header.
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
        topology_commitment: None,
    }
}

/// One correctly-linked page of CHUNK headers (the producer's per-chunk unit).
fn mk_page(start_height: u64, pad: usize) -> Vec<SigilBlockHeaderV0> {
    let mut page = Vec::with_capacity(CHUNK);
    let mut parent = *blake3::hash(&start_height.to_le_bytes()).as_bytes();
    for i in 0..CHUNK as u64 {
        let hdr = mk_header(start_height + i, parent, pad);
        parent = hdr.hash();
        page.push(hdr);
    }
    page
}

/// pure-Rust ruzstd inflate — the EXACT decoder the follower runs in prod.
fn ruzstd_inflate(comp: &[u8]) -> Vec<u8> {
    let mut dec = ruzstd::StreamingDecoder::new(comp).expect("ruzstd new");
    let mut out = Vec::new();
    dec.read_to_end(&mut out).expect("ruzstd read_to_end");
    out
}

/// 72-B skeleton record — byte-identical to `sigil_header::SkeletonRecord` layout, and
/// implements flux-db's trait so commit runs against the EXACT prod flat-store path.
#[derive(Debug, PartialEq, Eq, Clone)]
struct Skel {
    height: u64,
    block_hash: [u8; 32],
    parent_hash: [u8; 32],
}
impl DbSkeletonRecord for Skel {
    const WIDTH: usize = 72;
    fn seq(&self) -> u64 {
        self.height
    }
    fn encode(&self, out: &mut [u8]) {
        out[0..8].copy_from_slice(&self.height.to_le_bytes());
        out[8..40].copy_from_slice(&self.block_hash);
        out[40..72].copy_from_slice(&self.parent_hash);
    }
    fn decode(buf: &[u8]) -> Result<Self, String> {
        if buf.len() != 72 {
            return Err(format!("Skel: need 72 bytes, got {}", buf.len()));
        }
        let mut h = [0u8; 8];
        h.copy_from_slice(&buf[0..8]);
        let mut b = [0u8; 32];
        b.copy_from_slice(&buf[8..40]);
        let mut p = [0u8; 32];
        p.copy_from_slice(&buf[40..72]);
        Ok(Skel { height: u64::from_le_bytes(h), block_hash: b, parent_hash: p })
    }
}

/// FRESH contiguous skeleton page for the commit stage at a given global base height —
/// keeps the flat-store contiguity invariant real while the inflate/verify replay a fixed
/// compressed page (the per-page CPU cost is content-independent; commit cost is fully real).
fn fresh_skel_page(base: u64) -> Vec<Skel> {
    let mut out = Vec::with_capacity(CHUNK);
    let mut parent = if base == 0 { [0u8; 32] } else { *blake3::hash(&(base - 1).to_le_bytes()).as_bytes() };
    for i in 0..CHUNK as u64 {
        let h = base + i;
        let bh: [u8; 32] = *blake3::hash(&h.to_le_bytes()).as_bytes();
        out.push(Skel { height: h, block_hash: bh, parent_hash: parent });
        parent = bh;
    }
    out
}

/// Percentile (nearest-rank) over a sorted slice of micros.
fn pct(sorted_us: &[u64], p: f64) -> Duration {
    if sorted_us.is_empty() {
        return Duration::ZERO;
    }
    let idx = (((sorted_us.len() as f64) * p).ceil() as usize).saturating_sub(1).min(sorted_us.len() - 1);
    Duration::from_micros(sorted_us[idx])
}

// ════════════════════════ GATE A: PER-STAGE BREAKDOWN AT SCALE ════════════════════════
//
// Each CPU stage measured alone, on the SAME page, so the bottleneck is visible. This is
// DIAGNOSIS (DeepSeek §4): min(stage) is the upper bound, not the verdict. The verdict is
// GATE B (measured pipeline). The headline truth this exposes: inflate is the wall.

#[test]
fn stage_breakdown_at_scale() {
    let target = target_blk_s();
    let n = (sustain_blocks() / CHUNK as u64) * CHUNK as u64; // page-aligned
    let pages = n / CHUNK as u64;

    // producer side (UNTIMED): one representative compressed page, realistic mature pad.
    let page = mk_page(0, 1_500);
    let raw = bincode::serialize(&page).expect("ser");
    let zblob = zstd::encode_all(&raw[..], 1).expect("zstd");
    let stored_hashes: Vec<BlockHash> = page.iter().map(|h| h.hash()).collect();

    let cores = rayon::current_num_threads();
    let wire = zblob.len();
    let per_hdr = wire as f64 / CHUNK as f64;
    eprintln!("\n================ V7-GATE A: per-stage breakdown @ {n} blocks ({pages} pages) ================");
    eprintln!("cores = {cores}  page = {CHUNK} hdr  wire = {wire} B/page ({per_hdr:.1} B/hdr)  bar = {target:.0} blk/s");
    eprintln!("--------------------------------------------------------------------------------");

    // INFLATE (the suspected wall) — replay the page `pages` times.
    let t = Instant::now();
    let mut inflated_total = 0u64;
    for _ in 0..pages {
        let inf = ruzstd_inflate(black_box(&zblob));
        inflated_total += black_box(inf.len()) as u64;
    }
    let inflate_rate = rate(n, t.elapsed().as_secs_f64());
    black_box(inflated_total);

    // DECODE (bincode deserialize).
    let inf = ruzstd_inflate(&zblob);
    let t = Instant::now();
    let mut decoded_count = 0u64;
    for _ in 0..pages {
        let v: Vec<SigilBlockHeaderV0> = bincode::deserialize(black_box(&inf)).expect("de");
        decoded_count += v.len() as u64;
    }
    let decode_rate = rate(n, t.elapsed().as_secs_f64());
    assert_eq!(decoded_count, n, "decode produced every header");

    // VERIFY (rayon precheck + parent linkage memcmp).
    let decoded: Vec<SigilBlockHeaderV0> = bincode::deserialize(&inf).expect("de");
    let t = Instant::now();
    let mut ok_total = 0u64;
    for _ in 0..pages {
        ok_total += decoded.par_iter().filter(|h| black_box(h.precheck()).is_ok()).count() as u64;
    }
    let verify_rate = rate(n, t.elapsed().as_secs_f64());
    assert_eq!(ok_total, n, "every header must precheck OK across all pages");
    // linkage
    let t = Instant::now();
    let mut linked = 0u64;
    for _ in 0..pages {
        let mut parent: Option<BlockHash> = None;
        for (i, h) in decoded.iter().enumerate() {
            if let Some(p) = parent.as_ref() {
                if black_box(&h.parent_hash) == black_box(p) {
                    linked += 1;
                }
            }
            parent = Some(stored_hashes[i]);
        }
    }
    let link_rate = rate(n, t.elapsed().as_secs_f64());
    assert_eq!(linked, (CHUNK as u64 - 1) * pages, "intra-page linkage holds every page");

    // COMMIT (flux-db SkeletonStore durable flat append, FRESH contiguous records).
    let dir = scratch("stageA-commit");
    let path = dir.join("skel.flat");
    let mut store: SkeletonStore<Skel> = SkeletonStore::open(&path, 0).expect("open");
    let t = Instant::now();
    for p in 0..pages {
        let sk = fresh_skel_page(p * CHUNK as u64);
        store.append_unsynced(&sk).expect("append");
    }
    store.sync().expect("sync");
    let commit_rate = rate(n, t.elapsed().as_secs_f64());
    assert_eq!(store.count(), n, "commit persisted every record");
    let _ = std::fs::remove_dir_all(&dir);

    let verify_total = rate(n, n as f64 / verify_rate + n as f64 / link_rate);
    eprintln!("  inflate (ruzstd)       : {inflate_rate:>12.0} blk/s   <-- the codec wall (LANE-3)");
    eprintln!("  decode  (bincode)      : {decode_rate:>12.0} blk/s");
    eprintln!("  verify  (precheck+link): {verify_total:>12.0} blk/s");
    eprintln!("  commit  (flat append)  : {commit_rate:>12.0} blk/s");

    let bottleneck_name;
    let bottleneck_rate;
    {
        let stages = [
            ("inflate", inflate_rate),
            ("decode", decode_rate),
            ("verify", verify_total),
            ("commit", commit_rate),
        ];
        let (bn, br) = stages.iter().cloned().fold(("", f64::INFINITY), |acc, s| if s.1 < acc.1 { s } else { acc });
        bottleneck_name = bn.to_string();
        bottleneck_rate = br;
    }
    eprintln!("  ── diagnosis: min(stage) = {bottleneck_name} @ {bottleneck_rate:.0} blk/s (upper bound on the composed pipeline)");
    report("stage:inflate", inflate_rate, target);
    report("stage:decode", decode_rate, target);
    report("stage:verify", verify_total, target);
    report("stage:commit", commit_rate, target);
    report("stage:min(bottleneck)", bottleneck_rate, target);
    eprintln!("================ V7-GATE A done (diagnosis only — verdict is GATE B) ================\n");
}

// ════════════════════════ GATE B: COMPOSED SUSTAINED PIPELINE ════════════════════════
//
// The VERDICT. The real client path composed (inflate→decode→verify→commit) over a long
// window with warm-up discarded. Bounded channels ⇒ a slow stage back-pressures the feeder
// (no unbounded buffer to hide behind). finished == scheduled (no drops). The sustained
// rate over [warmup, warmup+sustain) is the number the v7.0.0 tag is gated on.

#[test]
fn composed_sustained_pipeline() {
    let target = target_blk_s();
    let warmup = (warmup_blocks() / CHUNK as u64).max(1) * CHUNK as u64;
    let sustain = (sustain_blocks() / CHUNK as u64).max(1) * CHUNK as u64;
    let total = warmup + sustain;
    let warm_pages = warmup / CHUNK as u64;
    let total_pages = total / CHUNK as u64;

    // producer side (UNTIMED): one compressed page, replayed through the CPU stages.
    let page = mk_page(0, 1_500);
    let zblob = Arc::new(zstd::encode_all(&bincode::serialize(&page).expect("ser")[..], 1).expect("zstd"));

    eprintln!("\n================ V7-GATE B: composed sustained pipeline ================");
    eprintln!("warmup = {warmup} (discard)   sustain = {sustain} (measure)   total = {total}");
    eprintln!("bounded channels cap={CAP} (back-pressure)   cores = {}   bar = {target:.0} blk/s",
        rayon::current_num_threads());
    eprintln!("------------------------------------------------------------------------");

    let dir = scratch("gateB");
    let path = dir.join("skel.flat");

    // raw(zblob, page_idx) -> decode -> verify(headers,page_idx) -> commit(skels,page_idx)
    let (raw_tx, raw_rx) = sync_channel::<(Arc<Vec<u8>>, u64)>(CAP);
    let (dec_tx, dec_rx) = sync_channel::<(Vec<SigilBlockHeaderV0>, u64)>(CAP);
    let (ver_tx, ver_rx) = sync_channel::<(Vec<Skel>, u64)>(CAP);

    // STAGE 1 — DECODE: ruzstd inflate + bincode deserialize.
    let decode = std::thread::spawn(move || {
        while let Ok((z, idx)) = raw_rx.recv() {
            let inf = ruzstd_inflate(&z);
            let v: Vec<SigilBlockHeaderV0> = bincode::deserialize(&inf).expect("de");
            if dec_tx.send((v, idx)).is_err() {
                break;
            }
        }
    });
    // STAGE 2 — VERIFY: rayon precheck across the page; restamp to FRESH contiguous skels.
    let verify = std::thread::spawn(move || {
        while let Ok((hdrs, idx)) = dec_rx.recv() {
            let all_ok = hdrs.par_iter().all(|h| black_box(h.precheck()).is_ok());
            assert!(all_ok, "verify: a header failed precheck");
            let skels = fresh_skel_page(idx * CHUNK as u64);
            if ver_tx.send((skels, idx)).is_err() {
                break;
            }
        }
    });
    // STAGE 3 — COMMIT (IO): durable flat append; record per-page finish Instant.
    let commit_path = path.clone();
    let finish_at: Arc<Mutex<Vec<(u64, Instant)>>> = Arc::new(Mutex::new(Vec::with_capacity(total_pages as usize)));
    let finish_at_c = finish_at.clone();
    let commit = std::thread::spawn(move || -> u64 {
        let mut store: SkeletonStore<Skel> = SkeletonStore::open(&commit_path, 0).expect("open");
        while let Ok((skels, idx)) = ver_rx.recv() {
            store.append_unsynced(&skels).expect("append");
            finish_at_c.lock().unwrap().push((idx, Instant::now()));
        }
        store.sync().expect("final sync");
        store.count()
    });

    // FEEDER (coordinated-omission-aware): schedule each page at its INTENDED arrival cadence
    // for the TARGET rate. If the pipeline can't keep up, send() blocks on the bounded channel
    // (back-pressure) and the intended schedule slips — which IS the honest signal. We record
    // each page's intended time so latency = finish - intended (never finish - pickup).
    let interval = Duration::from_secs_f64(CHUNK as f64 / target);
    let mut intended: Vec<Instant> = Vec::with_capacity(total_pages as usize);
    let start = Instant::now();
    for idx in 0..total_pages {
        let want = start + interval * (idx as u32);
        let now = Instant::now();
        if want > now {
            std::thread::sleep(want - now); // ahead of schedule: pace down to target
        }
        intended.push(want);
        raw_tx.send((zblob.clone(), idx)).expect("feed"); // behind schedule: blocks (back-pressure)
    }
    drop(raw_tx);
    decode.join().expect("decode join");
    verify.join().expect("verify join");
    let committed = commit.join().expect("commit join");

    // ---- correctness (always hard) ----
    assert_eq!(committed, total, "pipeline committed every scheduled block (no drops)");
    let mut finishes = finish_at.lock().unwrap().clone();
    assert_eq!(finishes.len() as u64, total_pages, "every page produced a finish (finished == scheduled)");
    finishes.sort_by_key(|(idx, _)| *idx);

    // durable soundness: reopen, verify exact contiguous prefix.
    {
        let mut store: SkeletonStore<Skel> = SkeletonStore::open(&path, 0).expect("reopen");
        assert_eq!(store.count(), total, "durable count after pipeline");
        let exp0 = fresh_skel_page(0);
        let expn = fresh_skel_page((total_pages - 1) * CHUNK as u64);
        assert_eq!(store.read_at(0).unwrap().as_ref(), Some(&exp0[0]), "first record byte-exact");
        assert_eq!(
            store.read_at(total - 1).unwrap().as_ref(),
            Some(&expn[CHUNK - 1]),
            "last record byte-exact"
        );
    }

    // ---- sustained rate over [warmup, total) — the VERDICT (DeepSeek §2/§4) ----
    // REAL steady-state wall: t0 = real finish of the LAST warmup page (the instant the
    // measured window begins), t1 = last real finish. This excludes warmup AND is pure
    // real elapsed (NOT the intended clock) — the honest drain rate of the pipeline.
    let win_start = if warm_pages >= 1 {
        finishes[(warm_pages - 1) as usize].1
    } else {
        finishes[0].1
    };
    let last_finish = finishes.last().unwrap().1;
    let sustained = rate(sustain, (last_finish - win_start).as_secs_f64());

    // ---- rolling 1s sub-window check: no second inside the window may dip below target ----
    // Bucket per-page completion times (relative to the real window start) into 1s bins and
    // require each FULL bin to clear the bar. A burst that drains a buffer fails this.
    let mut worst_bin = f64::INFINITY;
    {
        // (page_idx in window) -> finish offset secs
        let mut offs: Vec<f64> = finishes
            .iter()
            .filter(|(idx, _)| *idx >= warm_pages)
            .map(|(_, fin)| (*fin - win_start).as_secs_f64())
            .collect();
        offs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let full_secs = offs.last().map(|&l| l.floor() as u64).unwrap_or(0);
        for s in 0..full_secs {
            let (lo, hi) = (s as f64, s as f64 + 1.0);
            let cnt = offs.iter().filter(|&&o| o >= lo && o < hi).count() as u64;
            let bin_rate = (cnt * CHUNK as u64) as f64; // blocks completed in this 1s
            if bin_rate < worst_bin {
                worst_bin = bin_rate;
            }
        }
        if full_secs == 0 {
            worst_bin = sustained; // window shorter than 1s (smoke run) — fall back to overall
        }
    }

    // ---- coordinated-omission latency over the steady-state window ----
    let mut lat_us: Vec<u64> = finishes
        .iter()
        .filter(|(idx, _)| *idx >= warm_pages)
        .map(|(idx, fin)| (*fin - intended[*idx as usize]).as_micros() as u64)
        .collect();
    lat_us.sort_unstable();
    let p50 = pct(&lat_us, 0.50);
    let p99 = pct(&lat_us, 0.99);
    let p999 = pct(&lat_us, 0.999);
    let lmax = Duration::from_micros(*lat_us.last().unwrap_or(&0));

    eprintln!("  SUSTAINED (measured pipeline) : {sustained:>12.0} blk/s   <-- VERDICT");
    eprintln!("  worst 1s rolling sub-window   : {worst_bin:>12.0} blk/s");
    eprintln!("  coordinated-omission latency  : p50={:?} p99={:?} p99.9={:?} max={:?}", p50, p99, p999, lmax);
    eprintln!("  (latency from INTENDED arrival @ {target:.0} blk/s, not from pickup — buffering can't hide)");

    let _ = std::fs::remove_dir_all(&dir);

    let green_rate = report("e2e:sustained", sustained, target);
    let green_roll = report("e2e:worst-1s", worst_bin, target);
    let lat_ok = lmax <= CO_MAX_LATENCY && (lat_us.is_empty() || p99 <= CO_P99_LATENCY);
    println!(
        "GATE_RESULT stage=e2e:latency p99_ms={} max_ms={} target_p99_ms={} verdict={}",
        p99.as_millis(),
        lmax.as_millis(),
        CO_P99_LATENCY.as_millis(),
        if lat_ok { "GREEN" } else { "RED" }
    );
    eprintln!(
        "  latency-under-load            : p99 {:?} ≤ {:?}? max {:?} ≤ {:?}? → {}",
        p99, CO_P99_LATENCY, lmax, CO_MAX_LATENCY, if lat_ok { "GREEN ✓" } else { "RED ✗" }
    );

    let overall_green = green_rate && green_roll && lat_ok;
    println!(
        "GATE_RESULT stage=e2e:OVERALL sustained_blk_s={sustained:.0} target={target:.0} verdict={}",
        if overall_green { "GREEN" } else { "RED" }
    );
    eprintln!("================ V7-GATE B verdict: {} ================\n", if overall_green { "GREEN" } else { "RED" });

    if enforce_in_test() {
        assert!(
            overall_green,
            "V7-GATE RED: sustained {sustained:.0} blk/s / worst-1s {worst_bin:.0} / p99 {:?} — v7.0.0 BLOCKED",
            p99
        );
    }
}
