//! block_sync/commit.rs — LANE-C (storage / durability)
//!
//! Owner: rocky-sync-C. Tip-cache persistence today; this is where the batched
//! write-back commit path + content-addressed BlockStore bulk-import land. Split out
//! of block_sync.rs 2026-06-19 (v3 sync sprint). Verbatim.

// ── v0.32.5: persisted tip — OFFLINE-RESILIENT COLD START ───────────────────────────────────
// The fast-snap needs a known network tip in peer_best. The eager-seed + poller fetch it from the
// CDN oracles, but if BOTH are unreachable at boot (laptop offline, CDN outage, captive portal),
// peer_best stays 0 and the monitor sits at "connecting…". Cache the last-known tip on disk each
// time it advances; on the next cold start, seed peer_best from it so the snap can STILL fire to a
// recent window. The live poller corrects it upward the instant an oracle answers. Only ever RAISES.
pub(super) fn tip_cache_path() -> std::path::PathBuf {
    let dir = std::env::var("SIGIL_TOP_HOME").ok().map(std::path::PathBuf::from)
        .or_else(|| std::env::var("HOME").ok().map(|h| std::path::Path::new(&h).join(".sigil-top")))
        .unwrap_or_else(std::env::temp_dir);
    dir.join("last-tip")
}
pub(super) fn read_persisted_tip() -> Option<u64> {
    std::fs::read_to_string(tip_cache_path()).ok()?.trim().parse::<u64>().ok().filter(|&h| h > 0)
}
pub(super) fn persist_tip(h: u64) {
    let p = tip_cache_path();
    if let Some(dir) = p.parent() { let _ = std::fs::create_dir_all(dir); }
    let _ = std::fs::write(p, h.to_string());
}
/// v0.36.1: drop the persisted tip so a restart doesn't re-seed a stale (pre-reset)
/// height. Called when chain-reset detection fires in the tip-poller.
pub(super) fn clear_persisted_tip() { let _ = std::fs::remove_file(tip_cache_path()); }

/// v5.0.3 LANE-C: dir for the flat skeleton stores (one file per snapshot base).
pub(super) fn skeleton_dir() -> std::path::PathBuf {
    let dir = std::env::var("SIGIL_TOP_HOME").ok().map(std::path::PathBuf::from)
        .or_else(|| std::env::var("HOME").ok().map(|h| std::path::Path::new(&h).join(".sigil-top")))
        .unwrap_or_else(std::env::temp_dir);
    let d = dir.join("skeleton");
    let _ = std::fs::create_dir_all(&d);
    d
}

// ════════════════════════════════════════════════════════════════════════════════════
//  v3 (LANE-C): write-back commit ring — batched durable commits for the 20k/92.6k sprint
// ════════════════════════════════════════════════════════════════════════════════════
//
// THE WALL (charter): committing each ~4096-block chunk through flux-db one chunk at a time
// is its own ceiling. flux-db's implicit durability fires a SYNCHRONOUS leveled compaction
// at 64 MB WAL (full-level rewrite, ~10-20× write-amp at 160 MB/s of ~8 KB headers) — that
// compaction storm, NOT the fsync, is the real >20k wall (confirmed via source read + a
// DeepSeek design review fed the raw flux-db facts).
//
// THE FIX (all engine primitives live in `crate::block_store`, additive + default-off):
//   • `CommitBuffer` — an explicit WRITE-BACK RING: verified headers accumulate, then flush
//     to flux-db in ONE large `batch_put` per drain (`BlockStore::commit_batch_durable`),
//     with a 2-phase atomic-tip fsync (blocks fsync'd, THEN tip fsync'd — power-loss safe).
//   • BULK-LOAD mode (`arm`) DEFERS flux-db compaction during the sync (a forward sync is
//     append-mostly → mid-sync compaction is near-pure write-amp) and grows the memtable to
//     256 MB so flushes are ~4× rarer; `finish` folds the deferred SST pile down ONCE at tip.
//   • Content-addressed `bulk_import_prefix` stays OFF the hot path (per-block file-create
//     caps ~10k/s) — an offline prefix-archival pass, never the live sink.
//
// SEAM: `launch()` owns `store` and reads `store.synced_to()` to drive the fetch cursor, so
// the integrated commit must stay WRITE-THROUGH (the buffer flushes before the loop re-reads
// the cursor). The ring is also driveable STANDALONE (see `tests::commit_sink_throughput`) —
// that standalone sink rate is the ceiling LANE-B's fold fast-path will expose at 92.6k.

use sigil_header::SigilBlockHeaderV0;
use crate::block_store::BlockStore;

/// Commit-ring tunables, env-overridable so an operator can trade the durability window
/// against throughput with no rebuild.
// `allow(dead_code)`: this API is consumed by `mod.rs::launch()` (rocky-sync-lead's seam) and
// by the standalone bench — transitional until the launch() wiring lands.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(super) struct CommitConfig {
    /// Flush the ring to flux-db once it holds this many blocks (the "large multi-CF batch").
    /// SIGIL_COMMIT_BATCH (default 16384, clamped 256..=1048576).
    pub batch_size: usize,
    /// fdatasync each batch (power-loss durable). false = kill-9-safe via the OS page cache
    /// only — faster, for throughput benches / ephemeral runs. SIGIL_COMMIT_FSYNC (default on).
    pub fsync: bool,
    /// Defer flux-db compaction + grow the memtable for the sync; folded down once at `finish`.
    /// SIGIL_COMMIT_BULK (default on).
    pub bulk_load: bool,
    /// Use the TRUSTED bulk commit (`commit_bulk_trusted_durable`, ZERO per-block gets) instead of
    /// the checked `commit_batch_durable`. ONLY for a verified, contiguous, fold-proven prefix —
    /// the caller must guarantee no fork/dup risk. SIGIL_COMMIT_TRUSTED (default OFF — the live
    /// gossip/frontier path must keep the fork+dup checks). The 100k snapshot/skeleton path sets it.
    pub trusted: bool,
    /// THROUGHPUT_MASTER LANE 2: per-batch TRUSTED routing on the verified watermark. When on, a
    /// flush whose MAX height ≤ `store.fold_anchor_height()` (the SQIsign-fold-anchored prefix) goes
    /// through the trusted zero-get bulk commit; the frontier (> anchor) stays on the checked path.
    /// Unlike `trusted` (which forces the WHOLE ring trusted), this is the DEFAULT-safe form — it
    /// only trusts what the fold anchor cryptographically authorizes (invariant #2), so the live
    /// frontier keeps its fork+dup checks. SIGIL_COMMIT_TRUSTED_PREFIX (default OFF — DARK).
    pub trusted_prefix: bool,
}

#[allow(dead_code)]
impl CommitConfig {
    pub(super) fn from_env() -> Self {
        let usize_env = |k: &str, d: usize| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
        let flag_env = |k: &str, d: bool| std::env::var(k).ok()
            .map(|v| v != "0" && !v.eq_ignore_ascii_case("false")).unwrap_or(d);
        CommitConfig {
            batch_size: usize_env("SIGIL_COMMIT_BATCH", 16_384).clamp(256, 1_048_576),
            fsync: flag_env("SIGIL_COMMIT_FSYNC", true),
            bulk_load: flag_env("SIGIL_COMMIT_BULK", true),
            trusted: flag_env("SIGIL_COMMIT_TRUSTED", false),
            trusted_prefix: flag_env("SIGIL_COMMIT_TRUSTED_PREFIX", false),
        }
    }
}

/// Explicit write-back commit ring. Accumulates verified headers and flushes them to flux-db
/// in large durable batches. Synchronous + write-through-on-flush, so it drops into `launch()`
/// without inverting `store` ownership; it is also driven directly by the standalone bench.
#[allow(dead_code)]
pub(super) struct CommitBuffer {
    buf: Vec<SigilBlockHeaderV0>,
    cfg: CommitConfig,
    committed: u64,
    batches: u64,
}

#[allow(dead_code)]
impl CommitBuffer {
    pub(super) fn new(cfg: CommitConfig) -> Self {
        let cap = cfg.batch_size;
        CommitBuffer { buf: Vec::with_capacity(cap), cfg, committed: 0, batches: 0 }
    }
    pub(super) fn from_env() -> Self { Self::new(CommitConfig::from_env()) }

    /// Arm the store for the sync: enter bulk-load (defer compaction + grow memtable). Call
    /// ONCE at loop start, before the first push.
    pub(super) fn arm(&self, store: &BlockStore) {
        if self.cfg.bulk_load { store.set_bulk_load(true); }
    }

    /// Buffer verified headers; flush a durable batch when the ring reaches `batch_size`.
    /// Returns the number of blocks made durable by THIS call (0 if only buffered).
    pub(super) fn push(&mut self, store: &mut BlockStore, headers: Vec<SigilBlockHeaderV0>) -> usize {
        if headers.is_empty() { return 0; }
        self.buf.extend(headers);
        if self.buf.len() >= self.cfg.batch_size { self.flush(store) } else { 0 }
    }

    /// Buffer from a borrowed slice (avoids a move when the caller still needs `headers`).
    pub(super) fn push_slice(&mut self, store: &mut BlockStore, headers: &[SigilBlockHeaderV0]) -> usize {
        if headers.is_empty() { return 0; }
        self.buf.extend_from_slice(headers);
        if self.buf.len() >= self.cfg.batch_size { self.flush(store) } else { 0 }
    }

    /// Flush whatever is buffered as ONE large durable batch now (the "one fsync per batch"
    /// point). Returns blocks made durable. Call before the loop re-reads `store.synced_to()`
    /// so the fetch cursor never re-requests buffered-but-uncommitted heights.
    pub(super) fn flush(&mut self, store: &mut BlockStore) -> usize {
        if self.buf.is_empty() { return 0; }
        // Route this batch trusted-vs-checked:
        //   • `trusted` forces the WHOLE ring trusted (the explicit snapshot/skeleton path).
        //   • `trusted_prefix` (THROUGHPUT_MASTER LANE 2, DEFAULT-safe) trusts ONLY a batch whose max
        //     height is within the SQIsign-fold-anchored prefix (≤ fold_anchor_height); a batch that
        //     reaches the frontier — or any batch when no fold anchor is set — stays on the CHECKED
        //     path. A straddling batch routes checked (conservative; the snapshot path sends pure
        //     prefix batches, so it doesn't straddle in practice).
        // Trusted → skip the per-block fork/dup gets (the 100k fast path); checked = live/frontier.
        // v0.58 (10k-sync): CONTIGUITY-AWARE flush. Strict downward-linkage only accepts a run
        // whose parent is already stored, so a parallel look-ahead chunk landing ABOVE the frontier
        // was previously rejected + DROPPED here and re-fetched (measured ~74% waste). Instead:
        // sort+dedup, commit ONLY the contiguous run from the frontier (`synced_to`), and RETAIN the
        // out-of-order tail so it commits for free (one big jump) the moment the gap below it fills.
        self.buf.sort_by_key(|h| h.height);
        self.buf.dedup_by_key(|h| h.height);
        let synced = store.synced_to();                               // next-needed height
        let base_idx = self.buf.partition_point(|h| h.height < synced); // below frontier = already stored
        let mut cut = base_idx;
        let mut want = synced;
        while cut < self.buf.len() && self.buf[cut].height == want { cut += 1; want += 1; }
        let tail = self.buf.split_off(cut);                           // out-of-order remainder -> RETAIN
        let run = self.buf.split_off(base_idx);                       // [synced..want) contiguous -> commit
        self.buf = tail;
        // Bound retained memory: if a gap never fills, drop the tail (it will be re-fetched).
        const RETAIN_CAP: usize = 786_432;                            // ~8 chunks of 32768 headers
        if self.buf.len() > RETAIN_CAP { self.buf.clear(); }
        if run.is_empty() { return 0; }                              // frontier chunk not here yet
        let anchor = store.fold_anchor_height();
        let max_h = run.last().map(|h| h.height).unwrap_or(0);
        let use_trusted = self.cfg.trusted
            || (self.cfg.trusted_prefix && anchor > 0 && max_h <= anchor);
        let result = if use_trusted {
            store.commit_bulk_trusted_durable(&run, self.cfg.fsync)
        } else {
            store.commit_batch_durable(&run, self.cfg.fsync)
        };
        match result {
            Ok(n) => {
                self.committed += n as u64;
                self.batches += 1;
                n
            }
            Err(e) => {
                // Run NOT stored (tip advances only after a successful batch_put); the retained
                // tail stays buffered and the source re-fetches the run. Bounded above by RETAIN_CAP.
                crate::tlog!("[commit] durable batch failed ({} blocks): {e}", run.len());
                0
            }
        }
    }

    /// Final drain: flush the tail, then fold the deferred SST pile down ONCE at tip (re-enables
    /// eager compaction for steady-state live operation). Call at full-sync completion / shutdown.
    pub(super) fn finish(&mut self, store: &mut BlockStore) {
        self.flush(store);
        if self.cfg.bulk_load {
            if let Err(e) = store.compact_to_tip() {
                crate::tlog!("[commit] compact_to_tip at finish failed: {e}");
            }
        }
        let _ = store.sync_wal();
    }

    pub(super) fn committed(&self) -> u64 { self.committed }
    pub(super) fn batches(&self) -> u64 { self.batches }
    pub(super) fn pending(&self) -> usize { self.buf.len() }
}

/// v3 (LANE-C): content-addressed bulk import of a height PREFIX `[from..=to]` from the live
/// flux-db store into a flux-sync (flux-aether) content-addressed BlockStore. OFF the hot path
/// by design — flux-sync writes ONE verify-don't-trust file per block, so per-block file-create
/// caps throughput around ~10k/s (a fraction of the ≥20k live sink). Use it for an offline /
/// background prefix-archival pass that yields a verifiable `SyncManifest` (height→BLAKE3 root),
/// NOT for the live commit path. Returns (blocks imported, manifest). Logs the bound it covered
/// so a partial pass is never mistaken for full coverage.
#[allow(dead_code)]
pub(super) fn bulk_import_prefix(
    store: &BlockStore,
    aether_dir: &str,
    from: u64,
    to: u64,
) -> Result<(usize, flux_sync::SyncManifest), String> {
    let cas = flux_sync::BlockStore::open(aether_dir).map_err(|e| format!("aether open: {e}"))?;
    let mut manifest = flux_sync::SyncManifest::new("sigil-g0");
    let mut imported = 0usize;
    for h in from..=to {
        let Some(header) = store.get_header_at_height(h) else { continue };
        let bytes = bincode::serialize(&header).map_err(|e| format!("serialize h={h}: {e}"))?;
        let root = cas.ingest(&bytes).map_err(|e| format!("ingest h={h}: {e}"))?;
        manifest.add(h, root);
        imported += 1;
    }
    crate::tlog!("[commit] bulk_import_prefix: {imported} blocks [{from}..={to}] → content-addressed @ {aether_dir}");
    Ok((imported, manifest))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    use crate::block_store::BlockStore;
    use sigil_header::*;

    /// Build a genesis-linked header at `height` whose `parent_hash == parent`.
    fn mk(height: u64, parent: BlockHash) -> SigilBlockHeaderV0 {
        let nonce = SqiSignature::from_array([7u8; SQISIGN_L5_LEN]);
        let mut hh = blake3::Hasher::new();
        hh.update(&parent);
        hh.update(nonce.as_bytes());
        let vdf_input: [u8; 32] = *hh.finalize().as_bytes();
        let scheme = SigScheme::SqiSign5;
        SigilBlockHeaderV0 {
            version: HEADER_VERSION, network_id: NETWORK_ID, height, parent_hash: parent,
            merge_parents: Vec::new(), timestamp_ms: 1000 + height, nonce_sqisign: nonce,
            vdf_input, vdf_proof: WesolowskiProof { y: vec![], pi: vec![], t: 100 }, difficulty: 1,
            wallet_state_root: [0u8; 32], dex_state_root: [0u8; 32], event_log_root: [0u8; 32],
            contract_state_root: [0u8; 32],
            state_transition_proof: StarkProof { bytes: vec![], public_inputs_hash: [0u8; 32] },
            txs_merkle_root: [0u8; 32], tx_count: 0,
            fluxc_artifact_proof: ProofBundle { artifact_blake3: [0u8; 32], sqisign_sig: vec![], sqisign_pubkey: vec![], settle_tx: None },
            sig_scheme: scheme, producer: [0u8; 32],
            producer_sig: SignatureBytes(vec![0u8; scheme.expected_sig_len()]),
        }
    }

    /// A base=1-anchored linked chain over heights [1..=n] (h=0 genesis — the anchor's parent —
    /// is not stored; base=1 is the exempt anchor).
    fn chain(n: u64) -> Vec<SigilBlockHeaderV0> {
        let mut out = Vec::with_capacity(n as usize);
        let mut parent = [0u8; 32];
        for h in 0..=n {
            let hdr = mk(h, parent);
            parent = hdr.hash();
            if h >= 1 { out.push(hdr); }
        }
        out
    }

    fn tmp(tag: &str) -> String {
        std::env::temp_dir()
            .join(format!("sigil-commit-{}-{}", std::process::id(), tag))
            .to_string_lossy().into_owned()
    }

    fn test_cfg() -> CommitConfig {
        CommitConfig { batch_size: 2048, fsync: true, bulk_load: true, trusted: false, trusted_prefix: false }
    }

    /// The ring buffers across pushes and flushes durable batches; every block survives a reopen
    /// and the contiguous tip advances exactly to the chain head.
    #[test]
    fn ring_commits_pushed_blocks_and_advances_tip() {
        let p = tmp("ring");
        let _ = std::fs::remove_dir_all(&p);
        {
            let mut store = BlockStore::open_blocking(&p).unwrap();
            store.set_base(1);
            let mut ring = CommitBuffer::new(test_cfg());
            ring.arm(&store);
            // Push in 1000-block slices so several coalesce into 2048-batch flushes.
            for slice in chain(5000).chunks(1000) { ring.push_slice(&mut store, slice); }
            ring.finish(&mut store);
            assert_eq!(ring.committed(), 5000, "all 5000 accepted");
            assert!(ring.batches() >= 2, "multiple durable batches flushed, got {}", ring.batches());
            assert_eq!(ring.pending(), 0, "ring drained at finish");
            assert_eq!(store.synced_to(), 5001, "tip = 5001 (blocks 1..=5000 + base anchor)");
        }
        // Reopen: every block durable, tip resumes from disk.
        let store = BlockStore::open_blocking(&p).unwrap();
        assert_eq!(store.synced_to(), 5001, "resumes from the persisted store");
        for h in 1..=5000 { assert!(store.has_height(h), "h={h} present after reopen"); }
        let _ = std::fs::remove_dir_all(&p);
    }

    /// LANE-C crash-safety gate (same discipline as sigil-bank's credit_persist_test): a child
    /// process durably commits a batch, then is KILLED WITH SIGKILL — no Drop, no clean flush.
    /// The parent reopens and asserts every fsync'd block + the tip survived, proving the 2-phase
    /// atomic-tip fsync (not graceful shutdown) is what makes the commit durable. kill -9 can
    /// NOT corrupt the tip pointer.
    #[test]
    fn kill9_mid_sync_survives_durable_commit() {
        const N: u64 = 4000;
        // ── child mode ──
        if let Ok(db) = std::env::var("SIGIL_KILL9_DB") {
            let mut store = BlockStore::open_blocking(&db).unwrap();
            store.set_base(1);
            // Synchronous fsync'd commit on the owning thread — nothing is buffered when we die,
            // so durability is provably 100% on disk at the moment of the kill.
            let accepted = store.commit_batch_durable(&chain(N), true).unwrap();
            assert_eq!(accepted, N as usize);
            std::fs::write(std::path::Path::new(&db).join(".kill9-ready"), b"ready").unwrap();
            loop { std::thread::sleep(Duration::from_millis(50)); } // hang for the SIGKILL
        }

        // ── parent mode ──
        let dir = tmp("kill9");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = std::env::current_exe().expect("test exe");
        let mut child = std::process::Command::new(&exe)
            .args(["--exact", "block_sync::commit::tests::kill9_mid_sync_survives_durable_commit", "--nocapture"])
            .env("SIGIL_KILL9_DB", &dir)
            .spawn().expect("spawn kill9 child");

        let marker = std::path::Path::new(&dir).join(".kill9-ready");
        let mut waited = 0u64;
        while !marker.exists() {
            std::thread::sleep(Duration::from_millis(100));
            waited += 100;
            if waited > 30_000 { let _ = child.kill(); panic!("child never reached the durable-commit marker"); }
        }
        // kill -9: SIGKILL, uncatchable, no destructors, no flush — exactly a hard crash.
        let _ = std::process::Command::new("kill").args(["-9", &child.id().to_string()]).status();
        let _ = child.wait();

        // Reopen the post-crash store: WAL replay (CRC-guarded, torn-write-safe) + the open-time
        // advance_synced rescan restore every durably-committed block and the contiguous tip.
        let store = BlockStore::open_blocking(&dir).unwrap();
        assert_eq!(store.synced_to(), N + 1, "tip durable after kill -9 (blocks 1..={N} + anchor)");
        for h in 1..=N { assert!(store.has_height(h), "h={h} survived kill -9"); }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The TRUSTED bulk path (100k fast path) stores a contiguous prefix with ZERO per-block gets,
    /// is durable across reopen, and advances the tip — same observable result as the checked path
    /// for a clean contiguous chain (the caller guarantees no fork/dup).
    #[test]
    fn trusted_bulk_commit_stores_and_advances() {
        let p = tmp("trusted");
        let _ = std::fs::remove_dir_all(&p);
        {
            let mut store = BlockStore::open_blocking(&p).unwrap();
            store.set_base(1);
            let mut ring = CommitBuffer::new(CommitConfig { batch_size: 2048, fsync: true, bulk_load: true, trusted: true, trusted_prefix: false });
            ring.arm(&store);
            for slice in chain(5000).chunks(1000) { ring.push_slice(&mut store, slice); }
            ring.finish(&mut store);
            assert_eq!(ring.committed(), 5000, "trusted path committed all 5000");
            assert_eq!(store.synced_to(), 5001, "tip advanced to 5001");
        }
        let store = BlockStore::open_blocking(&p).unwrap();
        assert_eq!(store.synced_to(), 5001, "trusted commits durable across reopen");
        for h in 1..=5000 { assert!(store.has_height(h), "h={h} present"); }
        let _ = std::fs::remove_dir_all(&p);
    }

    /// LANE-B fold seam: the durable verified watermark survives a reopen and is never reported
    /// ahead of downloaded state (clamped to synced_to) — the divergence=0 persistence guarantee.
    #[test]
    fn verified_watermark_durable_and_clamped() {
        let p = tmp("vwm");
        let _ = std::fs::remove_dir_all(&p);
        {
            let mut store = BlockStore::open_blocking(&p).unwrap();
            store.set_base(1);
            store.commit_batch_durable(&chain(100), true).unwrap();
            assert_eq!(store.synced_to(), 101);
            // Anchor the verified watermark durably at 100.
            store.commit_verified_to_durable(100, true).unwrap();
            assert_eq!(store.verified_to(), 100);
            // A watermark ABOVE downloaded state is clamped to synced_to — never phantom-ahead.
            store.commit_verified_to_durable(10_000, true).unwrap();
            assert_eq!(store.verified_to(), 101, "verified_to clamped to synced_to, not 10000");
        }
        // Survives reopen.
        let store = BlockStore::open_blocking(&p).unwrap();
        assert_eq!(store.verified_to(), 101, "durable watermark resumes from disk");
        let _ = std::fs::remove_dir_all(&p);
    }

    /// LANE-B fold Option (b): a durable fold anchor relaxes the verified_to clamp to the anchor
    /// height (prefix trusted-not-downloaded), re-clamps at/above it, and survives reopen — so a
    /// kill-9 can't leave a verified watermark without its authorizing anchor.
    #[test]
    fn fold_anchor_relaxes_clamp_and_is_durable() {
        let p = tmp("fold");
        let _ = std::fs::remove_dir_all(&p);
        {
            let mut store = BlockStore::open_blocking(&p).unwrap();
            store.set_base(1);
            store.commit_batch_durable(&chain(10), true).unwrap(); // download only frontier [1..=10]
            assert_eq!(store.synced_to(), 11);
            // No anchor yet → verified_to clamps to synced_to.
            store.commit_verified_to_durable(1000, true).unwrap();
            assert_eq!(store.verified_to(), 11, "no anchor → clamped to synced_to");
            // Durable fold anchor at 1000 (hash = stand-in for the DNS SQIsign tip hash).
            store.commit_fold_anchor_durable(1000, [0xABu8; 32], true).unwrap();
            store.commit_verified_to_durable(1000, true).unwrap();
            assert_eq!(store.verified_to(), 1000, "fold anchor relaxes clamp to anchor height");
            assert_eq!(store.fold_anchor_height(), 1000);
            // Above the anchor, re-clamp to max(anchor, synced) — the frontier must be downloaded.
            store.commit_verified_to_durable(5000, true).unwrap();
            assert_eq!(store.verified_to(), 1000, "above anchor re-clamps to 1000");
        }
        // Reopen: anchor + relaxed verified_to survive — no phantom downgrade to synced_to=11.
        let store = BlockStore::open_blocking(&p).unwrap();
        assert_eq!(store.fold_anchor_height(), 1000, "fold anchor durable across reopen");
        assert_eq!(store.verified_to(), 1000, "verified_to resumes at the anchor, not clamped to 11");
        let _ = std::fs::remove_dir_all(&p);
    }

    /// Content-addressed prefix import yields one verifiable root per stored height.
    #[test]
    fn bulk_import_prefix_yields_manifest() {
        let p = tmp("casdb");
        let a = tmp("cas-aether");
        let _ = std::fs::remove_dir_all(&p);
        let _ = std::fs::remove_dir_all(&a);
        {
            let mut store = BlockStore::open_blocking(&p).unwrap();
            store.set_base(1);
            store.commit_batch_durable(&chain(50), true).unwrap();
            let (imported, man) = bulk_import_prefix(&store, &a, 1, 50).unwrap();
            assert_eq!(imported, 50, "all 50 prefix blocks imported");
            assert_eq!(man.refs.len(), 50, "one content-root per height");
            assert_eq!(man.tip, 50);
        }
        let _ = std::fs::remove_dir_all(&p);
        let _ = std::fs::remove_dir_all(&a);
    }

    /// STANDALONE commit-sink throughput bench (env-gated — `SIGIL_COMMIT_BENCH=<n_blocks>`),
    /// the ceiling LANE-B's fold fast-path will expose for the 92.6k hunt. Drives the ring
    /// directly with synthetic linked headers (no net/verify cost) and reports durable
    /// commit blk/s, batches, and fsync mode. Skipped by default so CI stays fast.
    #[test]
    fn commit_sink_throughput() {
        let n: u64 = match std::env::var("SIGIL_COMMIT_BENCH") {
            Ok(v) => v.parse().unwrap_or(0),
            Err(_) => return, // not requested → skip
        };
        if n == 0 { return; }
        let p = tmp("bench");
        let _ = std::fs::remove_dir_all(&p);
        let headers = chain(n);
        let mut store = BlockStore::open_blocking(&p).unwrap();
        store.set_base(1);
        let mut ring = CommitBuffer::from_env();
        ring.arm(&store);
        // THROUGHPUT_MASTER LANE 2: to bench the TRUSTED-PREFIX route, anchor the whole chain under a
        // fold anchor at the tip so every batch (max height ≤ n ≤ anchor) routes through
        // put_blocks_bulk_trusted. Without SIGIL_COMMIT_TRUSTED_PREFIX this is a no-op and the bench
        // measures the checked path. (Hash is a stand-in — routing reads height only. One meta put,
        // done before the timer.)
        if ring.cfg.trusted_prefix { store.set_fold_anchor(n, [0u8; 32]); }
        let t0 = Instant::now();
        // Feed in 4096-block slices, mirroring the live backfill chunk size.
        for slice in headers.chunks(4096) { ring.push_slice(&mut store, slice); }
        ring.finish(&mut store);
        let secs = t0.elapsed().as_secs_f64().max(1e-9);
        let rate = ring.committed() as f64 / secs;
        eprintln!("[commit-bench] {} blocks committed in {:.3}s = {:.0} blk/s | batches={} fsync={} bulk={} trusted={} trusted_prefix={} batch_size={}",
            ring.committed(), secs, rate, ring.batches(), ring.cfg.fsync, ring.cfg.bulk_load, ring.cfg.trusted, ring.cfg.trusted_prefix, ring.cfg.batch_size);
        assert_eq!(store.synced_to(), n + 1);
        let _ = std::fs::remove_dir_all(&p);
    }

    /// THROUGHPUT_MASTER LANE 2 — watermark-gated routing (`SIGIL_COMMIT_TRUSTED_PREFIX`): with a fold
    /// anchor at C, the ring sends batches whose max height ≤ C through the TRUSTED zero-get bulk
    /// commit and batches reaching the frontier (> C) through the CHECKED path — and the resulting
    /// store is BYTE-IDENTICAL (divergence=0) to the all-checked reference. This is the §5 invariant
    /// gate that must pass before the flag can ever be flipped default-on.
    #[test]
    fn trusted_prefix_routes_on_fold_anchor_matches_checked() {
        let p_ref = tmp("tp-ref");
        let p_tp = tmp("tp-on");
        let _ = std::fs::remove_dir_all(&p_ref);
        let _ = std::fs::remove_dir_all(&p_tp);
        let headers = chain(5000);
        // Reference: the all-checked path (trusted_prefix off).
        {
            let mut store = BlockStore::open_blocking(&p_ref).unwrap();
            store.set_base(1);
            let mut ring = CommitBuffer::new(test_cfg());
            ring.arm(&store);
            for slice in headers.chunks(1000) { ring.push_slice(&mut store, slice); }
            ring.finish(&mut store);
            assert_eq!(store.synced_to(), 5001);
        }
        // Trusted-prefix on, fold anchor at 3000 → [1..3000] routes trusted, [3001..5000] checked.
        {
            let mut store = BlockStore::open_blocking(&p_tp).unwrap();
            store.set_base(1);
            store.set_fold_anchor(3000, [0u8; 32]); // hash is a stand-in; routing reads height only
            let cfg = CommitConfig { batch_size: 2048, fsync: true, bulk_load: true, trusted: false, trusted_prefix: true };
            let mut ring = CommitBuffer::new(cfg);
            ring.arm(&store);
            for slice in headers.chunks(1000) { ring.push_slice(&mut store, slice); }
            ring.finish(&mut store);
            assert_eq!(store.synced_to(), 5001, "trusted-prefix routing stores the whole chain");
        }
        // Divergence=0: identical hash_hex at every height across the two routes, survives reopen.
        let a = BlockStore::open_blocking(&p_ref).unwrap();
        let b = BlockStore::open_blocking(&p_tp).unwrap();
        assert_eq!(a.synced_to(), b.synced_to());
        for h in 1..=5000 {
            assert_eq!(
                a.get_stored_at_height(h).map(|s| s.hash_hex),
                b.get_stored_at_height(h).map(|s| s.hash_hex),
                "height {h} diverged between checked and trusted-prefix routes"
            );
        }
        let _ = std::fs::remove_dir_all(&p_ref);
        let _ = std::fs::remove_dir_all(&p_tp);
    }

    /// THROUGHPUT_MASTER LANE 2 — the in-memory parent-hash cache (`SIGIL_COMMIT_PARENT_CACHE`) is a
    /// pure read-elision: with it ON, a forward batch sync stores the IDENTICAL chain as the disk-only
    /// path (divergence=0), and a conflicting-hash fork at an already-mapped height is STILL rejected
    /// (warm-cache hit returns the canonical hash; it can never overwrite the spine).
    #[test]
    fn link_cache_outcome_identical_and_fork_safe() {
        let p_off = tmp("lc-off");
        let p_on = tmp("lc-on");
        let _ = std::fs::remove_dir_all(&p_off);
        let _ = std::fs::remove_dir_all(&p_on);
        let headers = chain(3000);
        let sync = |path: &str, cache: bool| -> u64 {
            let mut s = BlockStore::open_blocking(path).unwrap();
            s.set_base(1);
            s.set_link_cache(cache);
            let mut acc = 0usize;
            for slice in headers.chunks(1000) { acc += s.put_blocks_batch(slice); s.advance(); }
            assert_eq!(acc, 3000, "all 3000 accepted (cache={cache})");
            s.synced_to()
        };
        assert_eq!(sync(&p_off, false), 3001);
        assert_eq!(sync(&p_on, true), 3001);
        // Divergence=0 between disk-only and cache-accelerated routes.
        let a = BlockStore::open_blocking(&p_off).unwrap();
        let b = BlockStore::open_blocking(&p_on).unwrap();
        for h in 1..=3000 {
            assert_eq!(
                a.get_stored_at_height(h).map(|s| s.hash_hex),
                b.get_stored_at_height(h).map(|s| s.hash_hex),
                "height {h} diverged with the parent-hash cache on"
            );
        }
        // Warm-cache fork safety: commit [1..200] into a fresh store (cache now holds h=100's
        // canonical hash), then a DIFFERENT header at h=100 must be rejected via the cache hit.
        let p_fork = tmp("lc-fork");
        let _ = std::fs::remove_dir_all(&p_fork);
        {
            let mut s = BlockStore::open_blocking(&p_fork).unwrap();
            s.set_base(1);
            s.set_link_cache(true);
            assert_eq!(s.put_blocks_batch(&headers[..200]), 200);
            s.advance();
            let forky = mk(100, [0x42u8; 32]); // different parent → different hash at h=100
            assert_eq!(
                s.put_blocks_batch(std::slice::from_ref(&forky)), 0,
                "warm-cache fork at an already-mapped height must be rejected"
            );
        }
        let _ = std::fs::remove_dir_all(&p_off);
        let _ = std::fs::remove_dir_all(&p_on);
        let _ = std::fs::remove_dir_all(&p_fork);
    }

    /// THROUGHPUT_MASTER LANE 2 ⇄ LANE 1 — the SST-ingest gate (`SIGIL_DB_SST_INGEST`) is DARK and
    /// FALLS BACK to the batch_put bulk path until flux-db's `ingest_sorted_bodies` lands: with the
    /// gate ON, `commit_bulk_trusted_durable` still stores the verified prefix durably and advances
    /// the tip (no behavior change — the seam is wired, kill-9 durability via the 2-phase fsync
    /// unchanged). Activating the real ingest call is a one-line swap in `put_blocks_bulk_trusted_ingest`.
    #[test]
    fn sst_ingest_gate_falls_back_and_stays_durable() {
        let p = tmp("sst-ingest");
        let _ = std::fs::remove_dir_all(&p);
        {
            let mut store = BlockStore::open_blocking(&p).unwrap();
            store.set_base(1);
            store.set_sst_ingest(true); // gate ON → put_blocks_bulk_trusted_ingest (fallback today)
            let n = store.commit_bulk_trusted_durable(&chain(2000), true).unwrap();
            assert_eq!(n, 2000, "SST-ingest gate (fallback) committed all 2000");
            assert_eq!(store.synced_to(), 2001, "tip advanced under the gate");
        }
        let store = BlockStore::open_blocking(&p).unwrap();
        assert_eq!(store.synced_to(), 2001, "durable across reopen with the gate on");
        for h in 1..=2000 { assert!(store.has_height(h), "h={h} present"); }
        let _ = std::fs::remove_dir_all(&p);
    }
}
