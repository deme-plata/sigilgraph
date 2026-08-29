//! `sigil-memwedge` — the deterministic chronos harness for the sigil-node
//! memory wedge.
//!
//! ## The incident this reproduces
//!
//! sigil-node has now wedged THREE times in the same way (2026-07-23 at
//! `MemoryHigh=3G`, again after the bump, and 2026-08-01 at `MemoryHigh=6G`).
//! Each time it was diagnosed as "the main thread spins producing nothing" and
//! each time the fix was a cap bump. Measured on the live process 2026-08-01,
//! that diagnosis was WRONG — the threads are in **D-state** on
//! `mem_cgroup_handle_over_high`:
//!
//! ```text
//!   memory.current 6.7G  vs  memory.high 6.0G      -> over the throttle line
//!   memory.events high    : 1,927,560              -> throttle events
//!   PSI memory full avg300: 97.17                  -> 97% of time ALL threads stalled
//!   pgscan_direct         : 171,377,370
//!   pgscan_kswapd         : 0                      -> ALL reclaim is synchronous
//!   workingset_refault_anon: 7,659,124             -> swap thrash
//!   memory.swap.current   : 8.0G  (memory.swap.max = max, UNLIMITED)
//! ```
//!
//! It is not spinning. It is being throttled into the ground by cgroup memory
//! pressure, and because swap is unlimited it never reaches `memory.max` and so
//! never OOMs, never restarts — the very escape hatch the 2026-07-23 fix was
//! designed to provide ("hard-OOM at 8G → Restart=always resumes in minutes
//! instead of a 4h zombie") is defeated by uncapped swap.
//!
//! ## The question this harness answers
//!
//! A cap bump only helps if the node's memory is a BOUNDED working set. If it
//! grows without bound in the block count, every bump just buys time and the
//! wedge recurs — which is exactly the observed history.
//!
//! Prime suspect, from reading `sigil-dagknight/src/braid.rs`: `Braid.frozen`
//! is a `Vec<BlockHash>` documented as "append-only". It is only ever `push`ed
//! (`emit`, :538) and sliced from `drained` (`drain_ordered`, :528). There is no
//! `truncate`/`drain`/`clear` anywhere. At the live tip (33,598,726 blocks) that
//! is 1.00 GiB that can never be released.
//!
//! This harness drives a real `Braid` exactly the way the live node does —
//! `insert()` then `drain_ordered()`, never `linearize()` (which is tests-only)
//! — and samples real RSS from `/proc/self/statm` alongside `BraidStats`, so the
//! growth law is measured rather than argued.
//!
//! Deterministic: seeded xorshift, no clocks in the data path, no rand crate.
//! Same seed → same DAG → same curve.
//!
//! Env:
//!   MEMWEDGE_BLOCKS    total blocks to feed        (default 2_000_000)
//!   MEMWEDGE_PRODUCERS concurrent producers        (default 2)
//!   MEMWEDGE_SAMPLE    sample every N blocks       (default 50_000)
//!   MEMWEDGE_SEED      rng seed                    (default 42)
//!   MEMWEDGE_FINAL_DEPTH / MEMWEDGE_MAX_WINDOW     (default 64 / 16384 = live)

use sigil_dagknight::{BlockView, Braid, BraidConfig, InsertOutcome};
use sigil_header::BlockHash;

const GENESIS: BlockHash = [0u8; 32];

fn env<T: std::str::FromStr>(k: &str, d: T) -> T {
    std::env::var(k).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(d)
}

/// Resident set size of THIS process, in bytes. Field 2 of /proc/self/statm is
/// resident pages. This is the number the cgroup throttles on.
fn rss_bytes() -> u64 {
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1).and_then(|v| v.parse::<u64>().ok()))
        .map(|pages| pages * 4096)
        .unwrap_or(0)
}

fn block_hash(producer: usize, height: u64) -> BlockHash {
    let mut h = blake3::Hasher::new();
    h.update(b"memwedge/blk");
    h.update(&(producer as u64).to_le_bytes());
    h.update(&height.to_le_bytes());
    *h.finalize().as_bytes()
}

fn producer_id(p: usize) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"memwedge/prod");
    h.update(&(p as u64).to_le_bytes());
    *h.finalize().as_bytes()
}

/// Build a realistic full `Block`-equivalent payload so the pending-buffer
/// measurement uses TRUE in-memory cost, not a guess. Mirrors what the node
/// actually holds: a populated header (292-B SQIsign nonce, 292-B producer sig,
/// VDF proof, STARK proof, provenance bundle, 4 roots) plus an empty transition.
/// High-entropy fill — real blocks carry random hashes.
fn realistic_header(height: u64) -> sigil_header::SigilBlockHeaderV0 {
    use sigil_header::*;
    let seed = *blake3::hash(&height.to_le_bytes()).as_bytes();
    let parent = *blake3::hash(&(height.wrapping_sub(1)).to_le_bytes()).as_bytes();
    let nonce = SqiSignature::from_array([0x5A; SQISIGN_L5_LEN]);
    let mut h = blake3::Hasher::new();
    h.update(&parent);
    h.update(nonce.as_bytes());
    let vdf_input = *h.finalize().as_bytes();
    SigilBlockHeaderV0 {
        version: HEADER_VERSION,
        network_id: NETWORK_ID,
        height,
        parent_hash: parent,
        merge_parents: vec![seed, seed],
        timestamp_ms: 1_780_000_000_000,
        nonce_sqisign: nonce,
        vdf_input,
        vdf_proof: WesolowskiProof { y: vec![0x11; 256], pi: vec![0x22; 256], t: 64 },
        difficulty: 24,
        wallet_state_root: seed,
        dex_state_root: seed,
        event_log_root: seed,
        contract_state_root: seed,
        state_transition_proof: StarkProof { bytes: vec![], public_inputs_hash: seed },
        txs_merkle_root: seed,
        tx_count: 0,
        fluxc_artifact_proof: ProofBundle {
            artifact_blake3: seed,
            sqisign_sig: vec![0x33; 292],
            sqisign_pubkey: vec![0x44; 129],
            settle_tx: None,
        },
        sig_scheme: SigScheme::SqiSign5,
        producer: seed,
        producer_sig: SignatureBytes(vec![0x55; 292]),
        topology_commitment: Some(seed),
    }
}

/// MODE 2 — the UNBOUNDED `pending` buffer.
///
/// `sigil-node/src/main.rs` buffers gossiped blocks by height in
/// `pending: BTreeMap<u64, Block>`. FOUR sites insert into it; only ONE is
/// capped:
///
/// ```text
///   main.rs:1360  if pending.len() < 200_000 { ... }   legacy linear path  CAPPED
///   main.rs:1272  pending.entry(bheight).or_insert(..) BRAID gossip path   UNCAPPED
///   main.rs:1566  pending.entry(h).or_insert(block)    backfill response   UNCAPPED
///   main.rs:1610  pending.entry(h).or_insert(block)    backfill response   UNCAPPED
/// ```
///
/// The live node runs `SIGIL_DAG=1`, i.e. the UNCAPPED braid path. The guard is
/// only `bheight >= chain.height()`, so every distinct height at or above the
/// local tip allocates and retains a full block. A node that falls behind
/// therefore buffers the entire gap — and the buffering is what makes it fall
/// further behind. That is the death spiral.
///
/// This measures the true per-entry cost and projects the gap the live node saw.
fn run_pending_mode() {
    let gap: u64 = env("MEMWEDGE_GAP", 200_000u64);
    let sample: u64 = env("MEMWEDGE_SAMPLE", 10_000u64);

    eprintln!("sigil-memwedge[pending]: modelling main.rs:1272 — the UNCAPPED braid pending buffer");
    eprintln!("sigil-memwedge[pending]: buffering {gap} heights ahead of a stalled tip\n");

    let rss0 = rss_bytes();
    let mut pending: std::collections::BTreeMap<u64, sigil_header::SigilBlockHeaderV0> =
        std::collections::BTreeMap::new();

    println!("heights_buffered\trss_mib\trss_delta_mib\tbytes_per_height");
    for h in 1..=gap {
        // The live guard is only `bheight >= chain.height()` — with a stalled
        // tip every gossiped height passes it. No length cap on this path.
        pending.entry(h).or_insert_with(|| realistic_header(h));
        if h % sample == 0 {
            let rss = rss_bytes();
            let d = rss.saturating_sub(rss0);
            println!("{}\t{:.1}\t{:.1}\t{:.0}", h, rss as f64 / 1048576.0,
                d as f64 / 1048576.0, d as f64 / h as f64);
        }
    }

    let rss = rss_bytes();
    let grew = rss.saturating_sub(rss0);
    let per = grew as f64 / gap as f64;
    eprintln!("\n── PENDING-BUFFER VERDICT ──");
    eprintln!("  heights buffered : {gap}");
    eprintln!("  RSS grew         : {:.1} MiB", grew as f64 / 1048576.0);
    eprintln!("  cost per height  : {per:.0} bytes  (HEADER ONLY — a full Block with its");
    eprintln!("                     transition + events is larger still)");
    eprintln!();
    eprintln!("  Projected buffer if the node falls behind by:");
    for g in [100_000u64, 1_000_000, 10_000_000, 33_598_726] {
        eprintln!("    {:>10} heights -> {:>7.2} GiB", g, per * g as f64 / 1073741824.0);
    }
    eprintln!();
    eprintln!("  cgroup memory.high on the live unit = 6.0 GiB.");
    eprintln!("  The buffering is what makes the node fall further behind -> death spiral.");
}

fn main() {
    if std::env::var("MEMWEDGE_MODE").as_deref() == Ok("pending") {
        run_pending_mode();
        return;
    }
    let total: u64 = env("MEMWEDGE_BLOCKS", 2_000_000u64);
    let producers: usize = env("MEMWEDGE_PRODUCERS", 2usize);
    let sample_every: u64 = env("MEMWEDGE_SAMPLE", 50_000u64);
    let final_depth: u64 = env("MEMWEDGE_FINAL_DEPTH", 64u64);
    let max_window: usize = env("MEMWEDGE_MAX_WINDOW", 16_384usize);

    eprintln!(
        "sigil-memwedge: blocks={total} producers={producers} final_depth={final_depth} max_window={max_window}"
    );
    eprintln!("sigil-memwedge: driving a real Braid the way sigil-node does (insert -> drain_ordered)");

    // Only the knobs this tool actually sweeps are named; everything else
    // takes the production default via `..Default::default()`.
    //
    // Spelling out every field here is what broke this bin: `BraidConfig`
    // gained `ghostdag_k`, `final_blue_depth`, `saturated_self_heal_window`
    // and `pending_max_tip_lag`, and an exhaustive initializer turns each
    // such addition into a compile error in a diagnostic tool that has no
    // opinion about those fields. Inheriting the defaults is also the more
    // honest measurement: a memory probe should model the braid production
    // actually runs, not a frozen subset of it.
    let cfg = BraidConfig {
        final_depth,
        max_window,
        max_pending: 4_096,
        max_merge_parents: 4,
        ..Default::default()
    };
    let mut braid = Braid::new_with_base(cfg, GENESIS, 0);

    let rss0 = rss_bytes();

    println!(
        "blocks\trss_mib\trss_delta_mib\tbytes_per_block\tfrozen_len\tfrozen_mib\twindow\tpending\ttips\tdag_mem_mib\tdrained_total"
    );

    let mut rounds: u64 = 0;
    let mut fed: u64 = 0;
    let mut drained_total: u64 = 0;

    // Each round every producer mints one block on its own spine, weaving in the
    // other producers' previous tips as merge parents — the live topology in
    // miniature. Then we drain, exactly as the node's produce tick does.
    while fed < total {
        rounds += 1;
        let height = rounds;

        for p in 0..producers {
            let hash = block_hash(p, height);
            let parent = if height == 1 { GENESIS } else { block_hash(p, height - 1) };
            // weave: reference the other producers' previous-round tips
            let mut merge_parents = Vec::new();
            if height > 1 {
                for q in 0..producers {
                    if q != p && merge_parents.len() < 4 {
                        merge_parents.push(block_hash(q, height - 1));
                    }
                }
            }
            let view = BlockView { hash, parent, merge_parents, height, producer: producer_id(p),
                    // 0 = a producer free-run mint, which is what 99.83% of real
                    // blocks are (7 of 4096 measured 2026-08-28 carried a real
                    // solve). Matching that here keeps the simulation honest:
                    // WorkPolicy defaults to UniformCount precisely because
                    // weighting by this field would give almost every block zero.
                    difficulty: 0 };
            match braid.insert(view) {
                InsertOutcome::Inserted { .. } | InsertOutcome::Duplicate => {}
                InsertOutcome::MissingParents(_) => {}
                InsertOutcome::BelowFinal { .. } => {}
                InsertOutcome::Rejected(r) => {
                    if fed % 1_000_000 == 0 {
                        eprintln!("  reject at h={height}: {r}");
                    }
                }
            }
            fed += 1;
        }

        // The live node drains every tick — this is what advances `frozen`.
        drained_total += braid.drain_ordered().len() as u64;

        if fed % sample_every < producers as u64 {
            let st = braid.stats();
            let rss = rss_bytes();
            let delta = rss.saturating_sub(rss0);
            let per_block = if fed > 0 { delta as f64 / fed as f64 } else { 0.0 };
            let frozen_len = st.emitted_total as u64;
            println!(
                "{}\t{:.1}\t{:.1}\t{:.1}\t{}\t{:.1}\t{}\t{}\t{}\t{:.1}\t{}",
                fed,
                rss as f64 / 1048576.0,
                delta as f64 / 1048576.0,
                per_block,
                frozen_len,
                (frozen_len * 32) as f64 / 1048576.0,
                st.window,
                st.pending,
                st.tips,
                st.dag_memory_bytes as f64 / 1048576.0,
                drained_total,
            );
        }
    }

    let st = braid.stats();
    let rss = rss_bytes();
    let grew = rss.saturating_sub(rss0);
    eprintln!("\n── VERDICT ──");
    eprintln!("  blocks fed          : {fed}");
    eprintln!("  RSS start -> end    : {:.1} MiB -> {:.1} MiB (grew {:.1} MiB)",
        rss0 as f64 / 1048576.0, rss as f64 / 1048576.0, grew as f64 / 1048576.0);
    eprintln!("  marginal cost       : {:.1} bytes per block", grew as f64 / fed.max(1) as f64);
    eprintln!("  frozen (append-only): {} entries = {:.1} MiB",
        st.emitted_total, (st.emitted_total * 32) as f64 / 1048576.0);
    eprintln!("  window / pending    : {} / {}  (caps {} / 4096)", st.window, st.pending, max_window);
    eprintln!("  bitfield dag        : {:.1} MiB", st.dag_memory_bytes as f64 / 1048576.0);
    eprintln!();
    eprintln!("  If bytes-per-block is FLAT and non-zero, memory is UNBOUNDED in chain length");
    eprintln!("  and no cgroup cap bump can fix it — only trimming the append-only structures can.");
    eprintln!("  Projection at the live tip (33,598,726 blocks): {:.2} GiB",
        (grew as f64 / fed.max(1) as f64) * 33_598_726.0 / 1073741824.0);
}
