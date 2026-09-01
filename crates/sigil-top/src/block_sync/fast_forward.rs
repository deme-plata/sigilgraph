//! block_sync/fast_forward.rs — V7-INGEST lane: the client commit→DB fast-forward sink.
//!
//! ## What's already in HEAD (this module ACTIVATES + TUNES it, does NOT re-author)
//! * `d2f8ce4` — authenticated skeleton fast-forward (`verify::fast_forward_from_authenticated_snapshot`):
//!   the snapshot anchor is bound to the trust root and the verified watermark advances over the
//!   trusted prefix; bodies backfill via PASS-2. (the skel→main *watermark* gap, closed.)
//! * `1588da7` — `BlockStore::put_blocks_bulk_trusted_ingest` → `flux_db::Database::ingest_sorted_bodies`
//!   (off-thread sorted-SST build + atomic install, skipping WAL+memtable+compaction), reachable from
//!   `commit_bulk_trusted_durable` under `SIGIL_DB_SST_INGEST`. (the FRONTIER commit SST seam.)
//!
//! ## The gap THIS module closes (not covered by the two above)
//! PASS-2 (`archive.rs`) — the trustless deep-history body backfill that converges every node to a
//! full archive — still committed verified bodies via `BlockStore::put_blocks_bulk_trusted` = the
//! memtable + WAL `batch_put` path (~4k blk/s, the LSM per-key wall). 1588da7 only wired the FRONTIER
//! trusted-commit, NOT PASS-2. At the v7 target of 100k blk/s sustained, that PASS-2 commit primitive
//! is the wall (viktor-v7-coord sweep @48c: DB-ingest is the modeled bottleneck — baseline
//! 1024/128/4 modeled 28k; the tuned config below models 106k).
//!
//! [`Pass2Sink`] routes the same trustless-verified bodies through `commit_bulk_trusted_durable`
//! (the SST-ingest fast path, ~232–238k blk/s when `SIGIL_DB_SST_INGEST=1`), batching to the swept
//! `SIGIL_SST_BATCH` (default [`DEFAULT_SST_BATCH`] = 15552, just UNDER the 16k compaction-stall
//! knee — exceeding it trips flux-db's write-stall guard every batch and *defeats* the optimisation).
//!
//! ## Backpressure (one shared spine with v7-supply)
//! When constructed with a [`sigil_synctune::RateGate`], the sink calls `gate.admit(n)` before
//! committing `n` blocks — the global token bucket caps serve+ingest to the autotuned blk/s so the
//! commit stage can't outrun the DB. The live sync loop wires the *same* spine v7-supply uses.
//!
//! ## Overlap committer ([`CommitPipeline`]) — bounded mpsc, NOT yet live-wired
//! The synchronous [`Pass2Sink`] already turns ~4k → ~230k commit, which is the headline
//! 100k-unblocking win and is what's wired live today. To overlap the serial SST install with the
//! next network fetch (DeepSeek: +2–4× on a network-bound backfill), [`CommitPipeline`] is a bounded
//! `sync_channel` (depth `SIGIL_COMMIT_RING_DEPTH`, default 288) feeding a dedicated commit thread
//! that OWNS the `BlockStore` and accumulates `SIGIL_COMMIT_MPSC_FLUSH` (default 128) -sized messages
//! into `SIGIL_SST_BATCH` SST installs. It is benched/tuned in isolation (the autotune sweep + the
//! `commit_pipeline_bench` gate measure it) but is **not** live-wired: in `block_sync/mod.rs` the
//! frontier loop and PASS-2 share ONE flux-db handle (two `Database` opens on one dir = corruption),
//! so moving the store onto a commit thread is a coordinated mod.rs store-ownership refactor
//! (codex-sigil-100k-v4's file), not a thin call-site patch. Filed so the interface is fixed.
//!
//! ## flux-db knob note (re v7-coord #655 point 3)
//! `O_DIRECT`, `max_background_jobs`, compaction-thread pinning and explicit disjoint-key-range
//! pre-split are RocksDB knobs with NO analog in flux-db's custom LSM: its compaction is
//! synchronous (no background-job pool), IO is buffered, and each `ingest_sorted_bodies` install is
//! one sorted SST that shadows older copies by sequence (disjoint-by-construction within a batch;
//! PASS-2 writes contiguous, disjoint height ranges anyway). The ONE mappable knob is the
//! `level0_slowdown` analog — flux-db's `INGEST_STALL_FILES` (const 64) write-stall high-water —
//! which to raise ~5× must become env-tunable in flux-db (rocky-L1's crate; coordinated via the
//! swarm bus, not faked here).
//!
//! ## Safety
//! * **Trustless** — `header.hash() == skeleton.block_hash[height]` is checked BEFORE a body is
//!   buffered (exactly as `archive.rs`); the SST path is a faster storage layout for already-verified
//!   bytes, adding no trust.
//! * **Crash-safe** — each `commit_bulk_trusted_durable` is atomic (SST ingest-or-nothing) + the
//!   2-phase tip-after-bodies fsync. Bodies buffered-but-unflushed at kill-9 are simply re-requested
//!   next run (`archive::next_body_gap` re-finds the hole via `store.has_height`). No torn commit.
//! * **No best-pointer regression** — `commit_bulk_trusted_durable` only ever RAISES `best_height`,
//!   so backfilling deep history below the tip never rewinds it.
//! * **Zero-regression when off** — the call site (mod.rs) only builds a sink when `SIGIL_DB_SST_INGEST`
//!   is set; with the flag off, PASS-2 runs the byte-identical legacy `archive::ingest_bodies_verified`.

use crate::block_sync::skel_flux::{FluxSkeletonStore, SkelRec};
use crate::block_store::BlockStore;
use sigil_header::SigilBlockHeaderV0;
use std::sync::Arc;
use std::time::Duration;

use sigil_synctune::RateGate;

/// Default SST-install batch (blocks) — viktor-v7-coord sweep @48c sweet spot. Just UNDER the 16384
/// compaction-stall knee: at/above it flux-db's write-stall guard folds the L0 pile on nearly every
/// install (synchronous compaction) and throughput collapses. Tunable via `SIGIL_SST_BATCH`.
pub(super) const DEFAULT_SST_BATCH: usize = 15552;
/// Hard ceiling for `SIGIL_SST_BATCH` — never let an operator cross the 16k stall knee.
pub(super) const SST_BATCH_KNEE: usize = 16_384;
/// Default bounded-mpsc depth for [`CommitPipeline`] (sweep @48c `commit_ring_depth`). Deep enough to
/// keep the commit thread fed across fetch-latency jitter without unbounded memory.
pub(super) const DEFAULT_RING_DEPTH: usize = 288;
/// Default per-message flush granularity feeding [`CommitPipeline`]'s channel (sweep @48c). The
/// committer accumulates these into `SIGIL_SST_BATCH`-sized SST installs.
pub(super) const DEFAULT_MPSC_FLUSH: usize = 128;

/// Read `SIGIL_SST_BATCH`, clamped to `[256, SST_BATCH_KNEE-1]`, else [`DEFAULT_SST_BATCH`].
pub(super) fn env_batch() -> usize {
    std::env::var("SIGIL_SST_BATCH")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .map(|n| n.clamp(256, SST_BATCH_KNEE - 1))
        .unwrap_or(DEFAULT_SST_BATCH)
}

fn env_ring_depth() -> usize {
    std::env::var("SIGIL_COMMIT_RING_DEPTH")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .map(|n| n.clamp(2, 65_536))
        .unwrap_or(DEFAULT_RING_DEPTH)
}

fn env_mpsc_flush() -> usize {
    std::env::var("SIGIL_COMMIT_MPSC_FLUSH")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .map(|n| n.clamp(1, 65_536))
        .unwrap_or(DEFAULT_MPSC_FLUSH)
}

/// Whether the SST-ingest fast path is the active commit primitive (mirrors flux-db's canonical
/// predicate; the call site uses it to decide whether to build a [`Pass2Sink`]).
pub(super) fn sst_ingest_active() -> bool {
    flux_db::ingest::sst_ingest_enabled()
}

/// Block (with bounded sleeps) until `gate` admits `n` blocks, returning nanos waited. Capped at 2 s
/// so a stalled/empty bucket can never wedge the live sync loop — past the cap we proceed (the global
/// rate is best-effort backpressure, not a correctness gate). `None` gate → instant 0.
fn admit_blocking(gate: &Option<Arc<dyn RateGate>>, n: usize) -> u64 {
    let Some(g) = gate else { return 0 };
    let n = n.clamp(1, u32::MAX as usize) as u32;
    let mut waited = 0u64;
    while !g.admit(n) {
        let w = g.admit_wait_nanos(n).clamp(1_000, 5_000_000); // 1µs..5ms steps
        std::thread::sleep(Duration::from_nanos(w));
        waited = waited.saturating_add(w);
        if waited > 2_000_000_000 {
            break;
        }
    }
    waited
}

/// Sort-stable by height + dedup (keep last) so a re-fetched-but-not-yet-committed height can never
/// feed two equal keys into one SST (the builder requires strictly-ascending-unique keys).
fn dedup_by_height(batch: &mut Vec<SigilBlockHeaderV0>) {
    batch.sort_by_key(|h| h.height);
    batch.dedup_by_key(|h| h.height);
}

/// Dedup → gate-admit → atomic SST install of `acc` into `store`, then clear it. Returns bodies
/// committed (0 on commit error — the heights stay absent and the gap walk re-fetches them).
/// Shared by the synchronous sink and the pipeline commit thread so both share one commit primitive.
fn install_batch(
    store: &mut BlockStore,
    acc: &mut Vec<SigilBlockHeaderV0>,
    gate: &Option<Arc<dyn RateGate>>,
    fsync: bool,
) -> u64 {
    if acc.is_empty() {
        return 0;
    }
    dedup_by_height(acc);
    let _ = admit_blocking(gate, acc.len());
    let n = match store.commit_bulk_trusted_durable(acc, fsync) {
        Ok(c) => c as u64,
        Err(e) => {
            crate::tlog!("[v7-ingest] commit failed ({e}) — {} bodies re-queued", acc.len());
            0
        }
    };
    acc.clear();
    n
}

/// Batched, SST-ingest-routed PASS-2 body sink. Construct once when a deep-gap backfill begins; feed
/// each fetched chunk through [`accept`](Pass2Sink::accept); call [`flush`](Pass2Sink::flush) when the
/// gap closes (or at tip) to land the partial tail.
pub(super) struct Pass2Sink {
    buf: Vec<SigilBlockHeaderV0>,
    batch: usize,
    fsync: bool,
    /// Optional global token bucket (shared spine with v7-supply); admit before each commit.
    gate: Option<Arc<dyn RateGate>>,
    committed: u64,
    rejected: u64,
    flushes: u64,
    /// Total nanos spent blocked on `gate.admit` — the commit-stage coordinated-omission signal.
    admit_wait_ns: u64,
}

impl Pass2Sink {
    /// Build from the environment with no rate gate (the live default until mod.rs shares the spine).
    pub(super) fn from_env() -> Self {
        Self::from_env_with_gate(None)
    }

    /// Build from the environment, admitting through `gate` (the shared v7 backpressure spine) before
    /// each commit. `batch` ← `SIGIL_SST_BATCH` (default 15552); `fsync` on unless `SIGIL_SST_FSYNC=0`.
    pub(super) fn from_env_with_gate(gate: Option<Arc<dyn RateGate>>) -> Self {
        let batch = env_batch();
        let fsync = std::env::var("SIGIL_SST_FSYNC").ok().as_deref() != Some("0");
        Self { buf: Vec::with_capacity(batch), batch, fsync, gate, committed: 0, rejected: 0, flushes: 0, admit_wait_ns: 0 }
    }

    /// Verify each body against the skeleton commitment, buffer the matches, and install full
    /// `batch`-sized SSTs as the buffer fills. Returns `(stored_this_call, rejected_this_call)`.
    /// A body with no skeleton entry OR a hash that doesn't match the committed `block_hash` is
    /// dropped and counted as rejected — the archive can never be poisoned.
    pub(super) fn accept(
        &mut self,
        skel: &mut FluxSkeletonStore,
        store: &mut BlockStore,
        bodies: &[SigilBlockHeaderV0],
    ) -> (usize, usize) {
        let mut rejected = 0usize;
        for b in bodies {
            match skel.read_at(b.height) {
                Ok(Some(SkelRec(rec))) if rec.block_hash == b.hash() => self.buf.push(b.clone()),
                _ => rejected += 1,
            }
        }
        self.rejected += rejected as u64;
        let mut stored = 0usize;
        while self.buf.len() >= self.batch {
            stored += self.flush_n(store, self.batch);
        }
        (stored, rejected)
    }

    /// Install up to `n` buffered bodies as one SST (after gate admission + dedup). Returns the
    /// bodies actually committed.
    fn flush_n(&mut self, store: &mut BlockStore, n: usize) -> usize {
        let take = n.min(self.buf.len());
        if take == 0 {
            return 0;
        }
        let mut batch: Vec<SigilBlockHeaderV0> = self.buf.drain(..take).collect();
        dedup_by_height(&mut batch);
        // Backpressure: don't outrun the global rate (shared spine with v7-supply).
        self.admit_wait_ns = self.admit_wait_ns.saturating_add(admit_blocking(&self.gate, batch.len()));
        match store.commit_bulk_trusted_durable(&batch, self.fsync) {
            Ok(c) => {
                self.committed += c as u64;
                self.flushes += 1;
                c
            }
            Err(e) => {
                crate::tlog!(
                    "[pass2-sink] commit failed ({e}) — {} bodies re-queued (gap will re-fetch)",
                    batch.len()
                );
                0
            }
        }
    }

    /// Install whatever remains in the buffer (the partial tail). Call when the gap closes, at tip,
    /// or periodically so a paused backfill doesn't strand verified bodies.
    pub(super) fn flush(&mut self, store: &mut BlockStore) -> usize {
        let mut total = 0usize;
        while !self.buf.is_empty() {
            total += self.flush_n(store, self.batch);
        }
        total
    }

    /// Bodies committed to the DB over this sink's lifetime.
    pub(super) fn committed(&self) -> u64 {
        self.committed
    }

    /// Bodies rejected (no skeleton entry or hash mismatch) over this sink's lifetime.
    #[allow(dead_code)]
    pub(super) fn rejected(&self) -> u64 {
        self.rejected
    }

    /// Verified bodies buffered but not yet flushed to the DB.
    pub(super) fn pending(&self) -> usize {
        self.buf.len()
    }

    /// Coordinated-omission signal: total nanos blocked on the rate gate (0 if ungated).
    #[allow(dead_code)]
    pub(super) fn admit_wait_ns(&self) -> u64 {
        self.admit_wait_ns
    }
}

// ─────────────────────────────────────────────────────────────────────────────────────────────
// CommitPipeline — bounded-mpsc overlap committer (benchable; live-wiring deferred, see header).
// ─────────────────────────────────────────────────────────────────────────────────────────────

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};

/// Config for [`CommitPipeline`], env-overridable so the autotune controller can refine it online.
#[derive(Clone, Copy, Debug)]
pub(super) struct PipelineConfig {
    /// Bounded `sync_channel` capacity (messages in flight) — backpressure: a full channel stalls the
    /// producer, bounding memory. (`SIGIL_COMMIT_RING_DEPTH`, default 288.)
    pub ring_depth: usize,
    /// SST-install accumulation size on the commit thread (`SIGIL_SST_BATCH`, default 15552).
    pub sst_batch: usize,
    /// fsync each install (durable). `SIGIL_SST_FSYNC=0` to skip for a throwaway/bench DB.
    pub fsync: bool,
}

impl PipelineConfig {
    pub(super) fn from_env() -> Self {
        Self {
            ring_depth: env_ring_depth(),
            sst_batch: env_batch(),
            fsync: std::env::var("SIGIL_SST_FSYNC").ok().as_deref() != Some("0"),
        }
    }
}

/// A dedicated commit thread that OWNS a `BlockStore` and drains a bounded channel of verified-body
/// batches, accumulating them into `sst_batch`-sized SST installs so the producer (fetch+verify) can
/// race ahead. The producer reads [`committed_to`](CommitPipeline::committed_to) (an `AtomicU64`
/// highwater) instead of `store.has_height` to walk the gap without touching the commit thread's store.
///
/// Crash-safety is unchanged: each install is atomic + tip-after-bodies fsync; a kill-9 loses only the
/// in-channel/in-accumulator tail, which the gap walk re-requests. Pre-verified input only — admission
/// and the trustless hash check happen on the producer side before `submit`.
pub(super) struct CommitPipeline {
    tx: Option<SyncSender<Vec<SigilBlockHeaderV0>>>,
    handle: Option<std::thread::JoinHandle<u64>>,
    committed: Arc<AtomicU64>,
    submitted: u64,
}

impl CommitPipeline {
    /// Spawn the commit thread, moving `store` onto it. `gate` (if any) admits before each install.
    pub(super) fn spawn(mut store: BlockStore, cfg: PipelineConfig, gate: Option<Arc<dyn RateGate>>) -> Self {
        let (tx, rx): (SyncSender<Vec<SigilBlockHeaderV0>>, Receiver<Vec<SigilBlockHeaderV0>>) =
            sync_channel(cfg.ring_depth);
        let committed = Arc::new(AtomicU64::new(0));
        let committed_thread = Arc::clone(&committed);
        let handle = std::thread::Builder::new()
            .name("v7-ingest-commit".into())
            .spawn(move || {
                let mut acc: Vec<SigilBlockHeaderV0> = Vec::with_capacity(cfg.sst_batch);
                let mut total = 0u64;
                while let Ok(msg) = rx.recv() {
                    acc.extend(msg);
                    if acc.len() >= cfg.sst_batch {
                        total += install_batch(&mut store, &mut acc, &gate, cfg.fsync);
                        committed_thread.store(store.synced_to(), Ordering::Release);
                    }
                }
                // Drain the tail on channel close.
                if !acc.is_empty() {
                    total += install_batch(&mut store, &mut acc, &gate, cfg.fsync);
                    committed_thread.store(store.synced_to(), Ordering::Release);
                }
                total
            })
            .expect("spawn commit thread");
        Self { tx: Some(tx), handle: Some(handle), committed, submitted: 0 }
    }

    /// Submit a verified-body batch (blocks if the channel is full — natural backpressure).
    /// Returns false if the commit thread is gone.
    pub(super) fn submit(&mut self, batch: Vec<SigilBlockHeaderV0>) -> bool {
        self.submitted += batch.len() as u64;
        match &self.tx {
            Some(tx) => tx.send(batch).is_ok(),
            None => false,
        }
    }

    /// Contiguous synced height the commit thread has durably reached — the producer walks the gap
    /// from `committed_to() + 1` instead of touching the store.
    pub(super) fn committed_to(&self) -> u64 {
        self.committed.load(Ordering::Acquire)
    }

    /// Close the channel and join the commit thread; returns total bodies committed.
    pub(super) fn finish(mut self) -> u64 {
        self.tx = None; // drop sender → channel closes → thread drains tail + returns
        self.handle.take().map(|h| h.join().unwrap_or(0)).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes the tests that mutate the process-global SIGIL_SST_BATCH /
    /// SIGIL_SST_FSYNC env vars. cargo runs tests in parallel, so without this two
    /// of them race on the env — one sets+clears while another reads — and the
    /// loser sees the wrong batch size (an intermittent, parallel-only CI failure).
    static SST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    use crate::block_sync::skel_flux::SkelRec;
    use sigil_header::*;

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
            topology_commitment: None,
        }
    }

    fn chain(n: u64) -> Vec<SigilBlockHeaderV0> {
        let mut out = Vec::new();
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
            .join(format!("sigil-ffwd-{}-{}", std::process::id(), tag))
            .to_string_lossy()
            .into_owned()
    }

    fn fresh_skel(path: &str, blocks: &[SigilBlockHeaderV0]) -> FluxSkeletonStore {
        let _ = std::fs::remove_file(path);
        let mut skel: FluxSkeletonStore = flux_db::skeleton::SkeletonStore::open(path, 1).unwrap();
        let recs: Vec<SkelRec> = blocks.iter().map(|h| SkelRec(SkeletonRecord::from_header(h))).collect();
        skel.append(&recs).unwrap();
        skel
    }

    fn fresh_store(path: &str) -> BlockStore {
        let _ = std::fs::remove_dir_all(path);
        let mut store = BlockStore::open_blocking(path).unwrap();
        store.set_base(1);
        store
    }

    /// Honest bodies land; a tampered body (right height, wrong contents → wrong hash) is rejected
    /// and never stored. Batching must not change WHICH bodies land — only WHEN.
    #[test]
    fn batched_sink_stores_matching_rejects_tampered() {
        let _env = SST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // 256 is the clamp FLOOR for SIGIL_SST_BATCH (tiny SST installs are
        // inefficient), so the old value 8 was silently clamped up to 256 — which
        // is what this assertion caught. Use the floor and enough blocks
        // (300 > 256) that the buffer crosses a real install boundary; the point
        // of the test is that batching changes only WHEN a body lands, never
        // WHETHER (matching the sibling batched_sink_matches_legacy_height_set).
        std::env::set_var("SIGIL_SST_BATCH", "256");
        std::env::set_var("SIGIL_SST_FSYNC", "0");
        let sp = tmp("skel");
        let bp = tmp("store");
        let blocks = chain(300);
        let mut skel = fresh_skel(&sp, &blocks);
        let mut store = fresh_store(&bp);

        let mut sink = Pass2Sink::from_env();
        assert_eq!(sink.batch, 256, "clamp floor honoured");
        let mut rejected = 0;
        for chunk in blocks.chunks(37) {
            let (_s, r) = sink.accept(&mut skel, &mut store, chunk);
            rejected += r;
        }
        assert_eq!(rejected, 0, "no honest body rejected");
        sink.flush(&mut store);
        assert_eq!(sink.pending(), 0, "buffer drained");
        assert_eq!(sink.committed(), 300, "every honest body committed exactly once");
        for h in 1..=300u64 {
            assert!(store.has_height(h), "height {h} present after fast-forward");
        }

        let mut store2 = fresh_store(&tmp("store2"));
        let mut liar = mk(5, [0xAB; 32]);
        liar.height = 5;
        let mut sink2 = Pass2Sink::from_env();
        let (_s, r) = sink2.accept(&mut skel, &mut store2, &[liar]);
        sink2.flush(&mut store2);
        assert_eq!(r, 1, "hash != commitment rejected");
        assert_eq!(sink2.committed(), 0, "tampered body never stored");
        assert!(!store2.has_height(5));

        let _ = std::fs::remove_file(&sp);
        let _ = std::fs::remove_dir_all(&bp);
        let _ = std::fs::remove_dir_all(&tmp("store2"));
        std::env::remove_var("SIGIL_SST_BATCH");
        std::env::remove_var("SIGIL_SST_FSYNC");
    }

    /// The batched sink lands EXACTLY the same height set as the legacy per-chunk
    /// `archive::ingest_bodies_verified` — divergence=0 between the slow and fast commit paths.
    #[test]
    fn batched_sink_matches_legacy_height_set() {
        let _env = SST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SIGIL_SST_BATCH", "256"); // floor (clamp), exercises multi-install
        std::env::set_var("SIGIL_SST_FSYNC", "0");
        let blocks = chain(300);

        let sp = tmp("mskel");
        let mut skel = fresh_skel(&sp, &blocks);
        let mut fast = fresh_store(&tmp("mfast"));
        let mut sink = Pass2Sink::from_env();
        assert_eq!(sink.batch, 256, "clamp floor honoured");
        for chunk in blocks.chunks(37) {
            sink.accept(&mut skel, &mut fast, chunk);
        }
        sink.flush(&mut fast);

        let mut slow = fresh_store(&tmp("mslow"));
        let (stored, rejected) =
            crate::block_sync::archive::ingest_bodies_verified(&mut skel, &mut slow, &blocks);
        assert_eq!((stored, rejected), (300, 0));

        for h in 1..=300u64 {
            assert_eq!(fast.has_height(h), slow.has_height(h), "height {h}: divergence=0");
            assert!(fast.has_height(h));
        }
        assert_eq!(sink.committed(), 300);

        let _ = std::fs::remove_file(&sp);
        let _ = std::fs::remove_dir_all(&tmp("mfast"));
        let _ = std::fs::remove_dir_all(&tmp("mslow"));
        std::env::remove_var("SIGIL_SST_BATCH");
        std::env::remove_var("SIGIL_SST_FSYNC");
    }

    /// CommitPipeline: a producer submitting pre-verified batches over the bounded channel lands the
    /// whole contiguous range; `committed_to()` tracks the durable highwater; `finish()` drains.
    #[test]
    fn commit_pipeline_lands_contiguous_range() {
        let _env = SST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SIGIL_SST_BATCH", "256");
        std::env::set_var("SIGIL_COMMIT_RING_DEPTH", "8");
        std::env::set_var("SIGIL_COMMIT_MPSC_FLUSH", "37");
        std::env::set_var("SIGIL_SST_FSYNC", "0");
        let bp = tmp("pstore");
        let blocks = chain(1000);
        // Pre-verify against a skeleton exactly as the producer side would, then pipe to the committer.
        let sp = tmp("pskel");
        let mut skel = fresh_skel(&sp, &blocks);
        let good: Vec<SigilBlockHeaderV0> = blocks
            .iter()
            .filter(|b| matches!(skel.read_at(b.height), Ok(Some(SkelRec(r))) if r.block_hash == b.hash()))
            .cloned()
            .collect();
        assert_eq!(good.len(), 1000);

        let store = fresh_store(&bp);
        let cfg = PipelineConfig::from_env();
        assert_eq!((cfg.ring_depth, cfg.sst_batch), (8, 256));
        let mut pipe = CommitPipeline::spawn(store, cfg, None);
        for chunk in good.chunks(env_mpsc_flush()) {
            assert!(pipe.submit(chunk.to_vec()), "commit thread alive");
        }
        let committed = pipe.finish();
        assert_eq!(committed, 1000, "all submitted bodies committed");

        // Reopen and confirm the whole range is durable.
        let check = BlockStore::open_blocking(&bp).unwrap();
        for h in 1..=1000u64 {
            assert!(check.has_height(h), "height {h} durable after pipeline finish");
        }

        let _ = std::fs::remove_file(&sp);
        let _ = std::fs::remove_dir_all(&bp);
        std::env::remove_var("SIGIL_SST_BATCH");
        std::env::remove_var("SIGIL_COMMIT_RING_DEPTH");
        std::env::remove_var("SIGIL_COMMIT_MPSC_FLUSH");
        std::env::remove_var("SIGIL_SST_FSYNC");
    }

    /// The rate-gated path admits through the shared spine and still lands every block (the gate is
    /// best-effort backpressure, never a correctness gate). Large burst keeps the unit test real-time
    /// independent; the wait-blocking + 2 s cap is covered by `admit_blocking`'s own logic.
    #[test]
    fn gate_path_admits_and_commits() {
        let _env = SST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        use sigil_synctune::{BackpressureSpine, Stage, VirtualClock};
        std::env::set_var("SIGIL_SST_BATCH", "256");
        std::env::set_var("SIGIL_SST_FSYNC", "0");
        let sp = tmp("gskel");
        let bp = tmp("gstore");
        let blocks = chain(300);
        let mut skel = fresh_skel(&sp, &blocks);
        let mut store = fresh_store(&bp);

        // Burst >> total so every admit succeeds immediately — exercises the gate plumbing, not timing.
        let gate: Arc<dyn RateGate> =
            Arc::new(BackpressureSpine::new(Arc::new(VirtualClock::new()), 100_000, 100_000, Stage::COUNT));
        let mut sink = Pass2Sink::from_env_with_gate(Some(gate));
        for chunk in blocks.chunks(37) {
            sink.accept(&mut skel, &mut store, &chunk.to_vec());
        }
        sink.flush(&mut store);
        assert_eq!(sink.committed(), 300, "gate admits; never drops");
        assert_eq!(sink.admit_wait_ns(), 0, "no wait when burst covers the load");
        for h in 1..=300u64 {
            assert!(store.has_height(h));
        }

        let _ = std::fs::remove_file(&sp);
        let _ = std::fs::remove_dir_all(&bp);
        std::env::remove_var("SIGIL_SST_BATCH");
        std::env::remove_var("SIGIL_SST_FSYNC");
    }
}
