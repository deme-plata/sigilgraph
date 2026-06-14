// sigil-top/src/gap_sync.rs — testable contiguous-backfill engine (the SPINE-BREAK fix)
//
// WHY THIS EXISTS
// ───────────────
// The live `P2PBlockSync` loop (block_sync.rs) is a ~900-line tokio/libp2p engine with
// dozens of hard-won edge-case fixes (zstd codec, peer benching, recent-window snap,
// chain-reset self-heal). It worked for the light monitor but full-archive (genesis→tip)
// sync STALLED at ~499k blocks — "SPINE BREAK": a dropped chunk / hole left the contiguous
// download frontier (`synced_to`) frozen below a higher RECEIVED block, the verifier
// (chain_verify::verify_to) walked genesis-up and stopped at the first `Missing` height,
// and because a `Missing` break is NOT a corruption it was mapped to `None` and never
// surfaced — a SILENT rate-0 stall. Worse, the full-archive base-skip could silently
// abandon the hole, breaking the "hold every block" guarantee.
//
// Rather than rewrite the live loop (high risk on a shared production tree), this module
// is the **canonical, deterministically-tested contiguity policy** the live loop now also
// follows. Two pieces of pure logic are SHARED with the live loop so the behaviour can't
// drift: [`classify_break`] and [`watchdog_verdict`]. The rest is a self-contained
// `GapSyncEngine<T: Transport>` that runs the WHOLE genesis-up backfill against an
// abstract [`Transport`], so the failure modes (out-of-order / dropped / reordered chunks,
// a lying-tip peer, a permanent hole) are testable in milliseconds with no network and no
// sleeps. The engine owns a REAL `BlockStore` and calls the REAL `chain_verify::verify_to`,
// so the test exercises the same store + verify code the production path does.
//
// THE THREE GUARANTEES (asserted by the test module below)
//   1. genesis-up CONTIGUOUS backfill: the engine reaches a verified spine 0..N, in order,
//      anchored at the base — never a bag of unconnected headers it calls "synced".
//   2. NO SILENT STALL: a permanent hole at height h makes the engine FAIL LOUD naming the
//      EXACT height h (a real ParentMismatch/Precheck break is surfaced immediately).
//   3. a lying-tip peer (claims a tip 10× reality) can NOT wedge it into a false failure —
//      reality is what peers actually SERVE, not what they CLAIM.

use crate::block_store::BlockStore;
use crate::chain_verify::{self, BreakReason, VerifyReport};
use sigil_header::SigilBlockHeaderV0;
use std::time::Duration;

/// Synchronous, blocking block-source abstraction. The contiguity logic only needs three
/// things from the network, and keeping them blocking + synchronous is what makes the
/// engine testable without faking the tokio/libp2p stack. The live adapter bridges these
/// onto `flux_p2p::NetworkManager` (peers ↔ `connected_peers`, `fetch_range` ↔ a
/// `send_request` of a `BackfillReq`, `tip_hint` ↔ the oracle/gossip `peer_best`).
#[allow(dead_code)] // consumed by the engine + the test harness; the live path uses the shared fns
pub trait Transport {
    /// Currently reachable peers, as opaque ids.
    fn peers(&self) -> Vec<String>;
    /// Fetch the headers a peer has for the INCLUSIVE range `[from, to]`. `None` = the
    /// request dropped/timed out; `Some(vec)` = what the peer holds in range (MAY be
    /// empty, MAY have internal gaps — a peer that pruned `h` simply omits it). The
    /// returned headers may arrive in ANY order; the store's height index reorders them.
    fn fetch_range(&self, peer: &str, from: u64, to: u64) -> Option<Vec<SigilBlockHeaderV0>>;
    /// An optimistic best tip. This is a HINT and MAY LIE (gossip/eclipse). The engine
    /// uses it only as a soft upper bound for look-ahead — never as proof a height exists.
    fn tip_hint(&self) -> u64;
}

/// How the verifier's `first_break` should be treated by the scheduler + watchdog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BreakClass {
    /// The contiguous spine is verified all the way to the download frontier — clean.
    Clean,
    /// Hit the download frontier at this height: the block isn't here YET. Normal during
    /// catch-up; only a PROBLEM if it never fills (→ watchdog). Carries the exact height to
    /// re-request at high priority (genesis-up exact repair).
    NeedHeight(u64),
    /// A real corruption: parent-linkage broken, precheck rejected, or a corrupt stored
    /// hash. FATAL — the downloaded chain does not form one connected spine. Surface
    /// loudly and forever (retrying can't fix forged/inconsistent headers).
    Fatal(u64, String),
}

/// PURE classification of a verify report — SHARED with the live loop so the two paths
/// can't diverge on what counts as "fatal" vs "just the frontier".
pub fn classify_break(report: &VerifyReport) -> BreakClass {
    match &report.first_break {
        None => BreakClass::Clean,
        Some((_h, BreakReason::Missing)) => BreakClass::NeedHeight(report.verified_to),
        Some((h, reason)) => BreakClass::Fatal(*h, reason.to_string()),
    }
}

/// A confirmed, operator-visible sync failure: the exact stuck height + a human message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncFailure {
    pub height: u64,
    pub reason: String,
}

/// PURE no-progress watchdog decision — SHARED with the live loop (block_sync.rs). This is
/// the heart of "never a silent rate-0 stall". It deliberately does NOT fire merely because
/// a peer CLAIMS a higher tip (`peer_best`) — that would false-fire whenever we've simply
/// caught up to what the mesh actually serves while a peer/oracle over-reports (the
/// lying-tip / eclipse case). It fires only when we hold PROOF of a hole: a block strictly
/// ABOVE the contiguous frontier has actually been RECEIVED (`best_height > frontier`) yet
/// the frontier hasn't advanced for `threshold`. The stuck height is the frontier itself
/// (= the exact next-needed / first `Missing`).
///
/// `frontier`     = store.synced_to() (contiguous downloaded tip; next-needed height).
/// `best_height`  = store.best_height() (max height actually RECEIVED, incl. out-of-order).
/// `stalled`      = how long the frontier has been parked.
/// `threshold`    = how long to tolerate a parked frontier before declaring failure.
pub fn watchdog_verdict(
    frontier: u64,
    best_height: u64,
    stalled: Duration,
    threshold: Duration,
) -> Option<SyncFailure> {
    if best_height > frontier && stalled >= threshold {
        Some(SyncFailure {
            height: frontier,
            reason: format!(
                "no-progress watchdog: contiguous frontier stuck at height {frontier} for {}s while a \
                 higher block ({best_height}) is already held — an unfillable hole at {frontier} (no \
                 reachable peer serves it). NOT a silent stall.",
                stalled.as_secs()
            ),
        })
    } else {
        None
    }
}

/// Per-request span AND look-ahead stride. Matches the live loop's proven 4096.
const CHUNK: u64 = 4096;
/// Verify budget per step. verify_to resumes from `verified_to`, so cumulative cost is
/// O(chain), not O(chain)·steps — a big number is fine.
const VERIFY_BUDGET: u64 = 1_000_000;

/// The deterministic, network-free backfill engine. Owns a real `BlockStore` and drives a
/// genesis-up contiguous fill against any [`Transport`], with exact-height gap repair and a
/// tick-based no-progress watchdog. The live `P2PBlockSync` mirrors this policy (and shares
/// its [`classify_break`] + [`watchdog_verdict`] decisions), so the engine itself is exercised
/// by the test harness rather than constructed on the live path — hence `allow(dead_code)`.
#[allow(dead_code)]
pub struct GapSyncEngine<T: Transport> {
    pub store: BlockStore,
    transport: T,
    /// Soft upper bound for look-ahead; only ever raised; a lying hint never proves a height.
    peer_best: u64,
    /// Round-robin peer cursor (so a dead peer doesn't get every request).
    rr: usize,
    /// Watchdog: contiguous frontier at the last advance, and consecutive no-advance ticks.
    last_frontier: u64,
    no_progress_ticks: u32,
    /// Fire the watchdog after this many consecutive no-progress `step`s (tick == one step;
    /// deterministic, so the test needs no real time). The live loop uses a wall-clock
    /// threshold via the shared [`watchdog_verdict`].
    watchdog_ticks: u32,
    /// Set when a fatal break is hit or the watchdog fires. STICKY: once a real failure is
    /// named, the engine stops doing work and keeps reporting it (the operator must see it).
    pub failure: Option<SyncFailure>,
}

#[allow(dead_code)]
impl<T: Transport> GapSyncEngine<T> {
    pub fn new(store: BlockStore, transport: T) -> Self {
        let peer_best = transport.tip_hint();
        let last_frontier = store.synced_to();
        Self {
            store,
            transport,
            peer_best,
            rr: 0,
            last_frontier,
            no_progress_ticks: 0,
            watchdog_ticks: 20,
            failure: None,
        }
    }

    /// Tune how many no-progress steps trigger the loud watchdog (test/ops knob).
    pub fn with_watchdog_ticks(mut self, ticks: u32) -> Self {
        self.watchdog_ticks = ticks.max(1);
        self
    }

    /// One deterministic tick: raise the soft tip, issue genesis-up + repair fetches,
    /// advance the contiguous frontier, verify, classify, and run the watchdog. Idempotent
    /// once a failure is latched.
    pub fn step(&mut self, max_inflight: usize) {
        if self.failure.is_some() {
            return; // sticky: a named failure is surfaced until the operator acts
        }
        // Soft upper bound only — a lying hint can RAISE this but never proves a height exists.
        self.peer_best = self.peer_best.max(self.transport.tip_hint());

        self.fetch_round(max_inflight.max(1));
        self.store.advance();

        // Verify the newly-contiguous prefix and classify the stopping reason.
        let report = chain_verify::verify_to(&mut self.store, VERIFY_BUDGET);
        match classify_break(&report) {
            BreakClass::Fatal(h, reason) => {
                // A forged/inconsistent header can never be repaired — surface immediately.
                self.failure = Some(SyncFailure { height: h, reason });
                return;
            }
            BreakClass::Clean | BreakClass::NeedHeight(_) => {}
        }

        // Watchdog: tick-based here (deterministic); the SAME decision the live loop makes
        // via the shared `watchdog_verdict` with a wall-clock duration.
        let frontier = self.store.synced_to();
        if frontier > self.last_frontier {
            self.last_frontier = frontier;
            self.no_progress_ticks = 0;
        } else {
            self.no_progress_ticks = self.no_progress_ticks.saturating_add(1);
        }
        if let Some(f) = watchdog_verdict(
            frontier,
            self.store.best_height(),
            Duration::from_secs(self.no_progress_ticks as u64),
            Duration::from_secs(self.watchdog_ticks as u64),
        ) {
            self.failure = Some(f);
        }
    }

    /// Issue this tick's fetches, genesis-up and gap-first:
    ///   1. the EXACT contiguous frontier (next-needed) — highest priority, fanned to peers;
    ///   2. internal holes in `(frontier, best_height]` — the repair scan (out-of-order /
    ///      dropped middle chunks);
    ///   3. look-ahead chunks ABOVE the frontier, bounded by the soft tip — so prefetch can
    ///      help but can NEVER race so far past an unfillable hole that the lead chunk starves
    ///      (the v0.10.0 / 499k frontier-stall root cause).
    fn fetch_round(&mut self, max_inflight: usize) {
        let peers = self.transport.peers();
        if peers.is_empty() {
            return;
        }
        let frontier = self.store.synced_to();
        let mut targets: Vec<u64> = Vec::with_capacity(max_inflight);
        targets.push(frontier); // (1) exact next-needed, always
        self.collect_holes(&mut targets, max_inflight); // (2) repair internal holes
        // (3) bounded look-ahead — never past peer_best (soft) + one chunk of slack.
        let mut h = (frontier / CHUNK) * CHUNK + CHUNK;
        while targets.len() < max_inflight && h <= self.peer_best.saturating_add(CHUNK) {
            if !self.store.has_height(h) {
                targets.push(h);
            }
            h += CHUNK;
        }

        for (i, &start) in targets.iter().enumerate() {
            if self.store.has_height(start) {
                continue;
            }
            let to = start.saturating_add(CHUNK - 1);
            // The frontier (i==0) is the chunk that actually advances `synced_to`; on a lossy
            // mesh fan it to ALL peers so it lands as soon as ANY responds (duplicates are
            // idempotent — the store dedups by height). Look-ahead/repair use one rotating peer.
            let fanout = if i == 0 { peers.len() } else { 1 };
            let mut got_any: Vec<SigilBlockHeaderV0> = Vec::new();
            for k in 0..fanout {
                let peer = &peers[(self.rr + k) % peers.len()];
                if let Some(mut hdrs) = self.transport.fetch_range(peer, start, to) {
                    got_any.append(&mut hdrs);
                    if i == 0 && !got_any.is_empty() {
                        break; // frontier landed from one peer; no need to ask the rest
                    }
                }
            }
            self.rr = self.rr.wrapping_add(fanout.max(1));
            if !got_any.is_empty() {
                self.store.put_blocks_batch(&got_any);
            }
        }
    }

    /// Find up to `budget` internal holes in `(synced_to, best_height]` and queue them for
    /// repair. Holes can ONLY exist above the contiguous frontier (below it is contiguous by
    /// definition), so the scan window is naturally bounded by how far out-of-order arrivals
    /// have run — small on a healthy sync, and capped here so a wildly-reordered stream can't
    /// make one tick O(chain).
    fn collect_holes(&self, targets: &mut Vec<u64>, budget: usize) {
        let lo = self.store.synced_to();
        let hi = self.store.best_height();
        if hi <= lo {
            return;
        }
        const MAX_SCAN: u64 = 100_000; // bound the per-tick scan
        let mut h = lo;
        let end = hi.min(lo.saturating_add(MAX_SCAN));
        while h <= end && targets.len() < budget {
            if !self.store.has_height(h) && !targets.contains(&h) {
                targets.push(h);
            }
            h += 1;
        }
    }

    /// Run until the verified spine reaches `target`, a failure latches, or `max_ticks`
    /// elapse. Returns the final verified watermark. (Test/headless helper.)
    pub fn run(&mut self, target: u64, max_ticks: u32, max_inflight: usize) -> u64 {
        for _ in 0..max_ticks {
            if self.failure.is_some() || self.store.verified_to() >= target {
                break;
            }
            self.step(max_inflight);
        }
        self.store.verified_to()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sigil_header::*;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn tmp(tag: &str) -> String {
        std::env::temp_dir()
            .join(format!("sigil-gapsync-{}-{}", std::process::id(), tag))
            .to_string_lossy()
            .into_owned()
    }

    /// A precheck-valid, correctly-linked header (mirrors chain_verify.rs's test builder).
    fn mk_header(height: u64, parent_hash: BlockHash) -> SigilBlockHeaderV0 {
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
            vdf_proof: WesolowskiProof { y: vec![], pi: vec![], t: 100 },
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

    /// A correctly-linked chain of heights `0..=n`, each parent_hash = hash of the previous.
    fn mk_chain(n: u64) -> Vec<SigilBlockHeaderV0> {
        let mut chain = Vec::with_capacity(n as usize + 1);
        let mut parent = [0u8; 32];
        for h in 0..=n {
            let hdr = mk_header(h, parent);
            parent = hdr.hash();
            chain.push(hdr);
        }
        chain
    }

    /// A fake peer holding a configurable subset of the chain, with optional drops/reorder.
    struct FakePeer {
        headers: BTreeMap<u64, SigilBlockHeaderV0>,
        /// 0 = never drop; else drop every Nth fetch (simulated timeout).
        drop_every: u64,
        /// reverse the returned slice (simulate out-of-order delivery).
        reorder: bool,
        calls: AtomicU64,
    }
    impl FakePeer {
        fn full(chain: &[SigilBlockHeaderV0]) -> Self {
            FakePeer {
                headers: chain.iter().map(|h| (h.height, h.clone())).collect(),
                drop_every: 0,
                reorder: false,
                calls: AtomicU64::new(0),
            }
        }
        fn fetch(&self, from: u64, to: u64) -> Option<Vec<SigilBlockHeaderV0>> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if self.drop_every > 0 && (n + 1) % self.drop_every == 0 {
                return None; // simulated drop/timeout
            }
            let mut v: Vec<_> = (from..=to)
                .filter_map(|h| self.headers.get(&h).cloned())
                .collect();
            if self.reorder {
                v.reverse();
            }
            if v.is_empty() { None } else { Some(v) }
        }
    }

    struct FakeNet {
        peers: Vec<FakePeer>,
        tip_hint: u64,
        lie_factor: u64, // 1 = honest; >1 = claim tip_hint*lie_factor
    }
    impl Transport for FakeNet {
        fn peers(&self) -> Vec<String> {
            (0..self.peers.len()).map(|i| format!("peer{i}")).collect()
        }
        fn fetch_range(&self, peer: &str, from: u64, to: u64) -> Option<Vec<SigilBlockHeaderV0>> {
            let idx = peer.strip_prefix("peer").and_then(|s| s.parse::<usize>().ok())?;
            self.peers.get(idx)?.fetch(from, to)
        }
        fn tip_hint(&self) -> u64 {
            self.tip_hint * self.lie_factor
        }
    }

    fn fresh_store(tag: &str) -> BlockStore {
        let p = tmp(tag);
        let _ = std::fs::remove_dir_all(&p);
        BlockStore::open_blocking(&p).unwrap()
    }

    // ─────────────────────────────────────────────────────────────────────────────
    // Pure-logic gates (the shared decision functions the live loop also calls).
    // ─────────────────────────────────────────────────────────────────────────────

    #[test]
    fn watchdog_fires_only_on_a_real_hole_not_a_lying_tip() {
        // Caught up to what's served (best == frontier-1, i.e. best_height < frontier):
        // a lying peer_best is irrelevant — NO failure.
        assert_eq!(watchdog_verdict(100, 99, Duration::from_secs(999), Duration::from_secs(20)), None);
        // A higher block is HELD above the frontier (proof of a hole) + parked long enough → fire,
        // naming the exact frontier height.
        let v = watchdog_verdict(499_000, 503_000, Duration::from_secs(30), Duration::from_secs(20));
        assert_eq!(v.as_ref().map(|f| f.height), Some(499_000));
        // Hole present but not parked long enough yet → hold (keep retrying, don't cry wolf).
        assert_eq!(watchdog_verdict(499_000, 503_000, Duration::from_secs(5), Duration::from_secs(20)), None);
    }

    #[test]
    fn classify_break_separates_frontier_from_corruption() {
        assert_eq!(classify_break(&VerifyReport { verified_to: 10, checked: 10, first_break: None }), BreakClass::Clean);
        assert_eq!(
            classify_break(&VerifyReport { verified_to: 7, checked: 7, first_break: Some((7, BreakReason::Missing)) }),
            BreakClass::NeedHeight(7)
        );
        match classify_break(&VerifyReport {
            verified_to: 3, checked: 3,
            first_break: Some((3, BreakReason::ParentMismatch { height: 3, expected: "a".into(), found: "b".into() })),
        }) {
            BreakClass::Fatal(3, _) => {}
            other => panic!("expected Fatal(3,..), got {other:?}"),
        }
    }

    // ─────────────────────────────────────────────────────────────────────────────
    // End-to-end engine gates (real BlockStore + real chain_verify).
    // ─────────────────────────────────────────────────────────────────────────────

    /// (i) Happy path: one full peer → contiguous verified spine all the way to the tip.
    #[test]
    fn happy_path_reaches_contiguous_genesis_anchored_tip() {
        const N: u64 = 20_000;
        let chain = mk_chain(N);
        let net = FakeNet { peers: vec![FakePeer::full(&chain)], tip_hint: N, lie_factor: 1 };
        let mut eng = GapSyncEngine::new(fresh_store("happy"), net);
        let verified = eng.run(N + 1, 5_000, 8);
        assert!(eng.failure.is_none(), "clean chain must not fail: {:?}", eng.failure);
        assert_eq!(verified, N + 1, "verified spine reaches the tip (next-needed = N+1)");
        assert_eq!(eng.store.verified_to(), eng.store.synced_to(), "verified == downloaded — one connected spine");
    }

    /// (ii) A permanent hole → FAIL LOUD naming the EXACT missing height, never silent stall.
    #[test]
    fn permanent_hole_fails_loud_with_exact_height() {
        const N: u64 = 20_000;
        const HOLE: u64 = 12_345;
        let chain = mk_chain(N);
        // Peer holds everything EXCEPT the hole; a 2nd peer is empty (also can't serve it).
        let mut headers: BTreeMap<u64, _> = chain.iter().map(|h| (h.height, h.clone())).collect();
        headers.remove(&HOLE);
        let net = FakeNet {
            peers: vec![
                FakePeer { headers, drop_every: 0, reorder: false, calls: AtomicU64::new(0) },
                FakePeer { headers: BTreeMap::new(), drop_every: 0, reorder: false, calls: AtomicU64::new(0) },
            ],
            tip_hint: N,
            lie_factor: 1,
        };
        let mut eng = GapSyncEngine::new(fresh_store("hole"), net).with_watchdog_ticks(5);
        eng.run(N + 1, 5_000, 8);
        let f = eng.failure.as_ref().expect("a permanent hole MUST surface a loud failure");
        assert_eq!(f.height, HOLE, "the failure names the EXACT missing height");
        assert!(eng.store.verified_to() <= HOLE, "the spine never claims past the hole");
    }

    /// (ii-b) A genuinely corrupt header (parent linkage broken) is FATAL and named.
    #[test]
    fn parent_mismatch_is_fatal_and_named() {
        const N: u64 = 2_000;
        let mut chain = mk_chain(N);
        // Re-forge block 1000 onto a bogus-but-internally-consistent parent (precheck passes,
        // linkage breaks) — isolates the ParentMismatch path.
        chain[1000] = mk_header(1000, [0xAB; 32]);
        let net = FakeNet { peers: vec![FakePeer::full(&chain)], tip_hint: N, lie_factor: 1 };
        let mut eng = GapSyncEngine::new(fresh_store("forge"), net);
        eng.run(N + 1, 5_000, 8);
        let f = eng.failure.as_ref().expect("a forged parent link MUST fail loud");
        assert_eq!(f.height, 1000, "names the exact break height");
        assert!(f.reason.contains("parent"), "reason identifies the linkage break: {}", f.reason);
    }

    /// (iii) Out-of-order + reordered + dropping peers still CONVERGE to a contiguous spine.
    #[test]
    fn out_of_order_and_drops_still_converge() {
        const N: u64 = 16_000;
        let chain = mk_chain(N);
        // Three peers, each holds the WHOLE chain but: peer0 reorders every reply, peer1 drops
        // every 3rd request, peer2 is clean. No single peer is reliable, but together they cover.
        let net = FakeNet {
            peers: vec![
                FakePeer { headers: chain.iter().map(|h| (h.height, h.clone())).collect(), drop_every: 0, reorder: true, calls: AtomicU64::new(0) },
                FakePeer { headers: chain.iter().map(|h| (h.height, h.clone())).collect(), drop_every: 3, reorder: false, calls: AtomicU64::new(0) },
                FakePeer::full(&chain),
            ],
            tip_hint: N,
            lie_factor: 1,
        };
        let mut eng = GapSyncEngine::new(fresh_store("reorder"), net);
        let verified = eng.run(N + 1, 20_000, 8);
        assert!(eng.failure.is_none(), "a recoverable mesh must not fail: {:?}", eng.failure);
        assert_eq!(verified, N + 1, "reorder/drops reorder in the store and still reach the tip");
    }

    /// (iv) A lying-tip peer (claims 10× the real tip) cannot wedge the engine into a false
    /// failure — it syncs the REAL chain the peer actually serves and stops cleanly.
    #[test]
    fn lying_tip_cannot_wedge_into_false_failure() {
        const N: u64 = 3_000;
        let chain = mk_chain(N);
        // One honest peer that holds 0..=N, but the network CLAIMS the tip is 10×N.
        let net = FakeNet { peers: vec![FakePeer::full(&chain)], tip_hint: N, lie_factor: 10 };
        let mut eng = GapSyncEngine::new(fresh_store("liar"), net).with_watchdog_ticks(5);
        // Run well past the real tip's worth of ticks; the lie must not provoke a watchdog fire.
        let verified = eng.run(N + 1, 5_000, 8);
        assert!(eng.failure.is_none(), "a lying tip must NOT cause a false failure: {:?}", eng.failure);
        assert_eq!(verified, N + 1, "syncs the real served chain, ignores the inflated claim");
        assert!(eng.peer_best >= N * 10, "the soft hint was raised by the lie (proof it WAS exposed to the lie)");
    }
}
