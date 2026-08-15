# SIGIL Narwhal-style DAG Mempool v0 — design

Status: **design + Phase-0 scaffold**, not yet wired into the live producer loop.
Author: Grogu (Claude Opus 5) with Viktor, 2026-08-15.

**Forward roadmap: `SIGIL_BRAIDPOOL_v1_1.md`.** Viktor reviewed this doc and caught two
real, independently-verified bugs — the quorum-threshold math (`2f+1` is unsafe for
committee sizes not exactly `3f+1`; the crate's own tests didn't catch it because they
only exercised the sizes where the bug is invisible) and the erasure-coding bandwidth
math (conflated shard count with shard size). Both are fixed below and in
`sigil-narwhal-mempool`'s code, with a new test that would have failed against the old
version. BraidPool v1.1 also proposes a much larger architecture (canonical batch
headers, epoch-salted worker assignment, a hybrid replication/erasure-coding
dissemination plane, `BatchSetRoot` block aggregation, a benchmark ladder, and a phased
A-G build plan) that is preserved in full there as the roadmap — none of it is built yet;
this v0 doc and its Phase-0 crate remain the actually-shipped starting point.

## 0. Why this exists

Viktor's ask (paraphrased across the session): SIGIL already has a real GHOSTDAG-style
DAG *consensus* layer (`sigil-dagknight`, shipped this session — k-cluster blue/red
coloring over the braid). What it does NOT have is a real DAG *mempool* underneath it —
today's mempool (`sigil-tx::Mempool`) is a single `Mutex`-guarded `VecDeque` with one
dedup `HashSet`. Quillon Graph has a Narwhal-inspired mempool already; SIGIL was built
explicitly to fix what Quillon got wrong, so it should get a REAL one — and Viktor asked
for an *invented upgrade* beyond stock Narwhal, aimed at an extremely high throughput
ceiling ("like 10m tps").

**Update, same day:** §3.3's original draft claimed an "invented" erasure-coded batch
dissemination scheme. A literature check (requested by Viktor, written up in
`SIGIL_NARWHAL_ARXIV_INVESTIGATION_v0.md`) found that claim was wrong — a 2024 paper,
Imitater (arXiv:2409.19286), already does almost exactly this. §3.3 below is corrected
in place rather than rewritten from scratch, so the correction is visible, not memory-holed.

This doc is honest about what "10M TPS" can and can't mean here (§4), lays out an
architecture assembled for THIS codebase specifically (§3) — some of it genuinely new for
SIGIL's own mempool even where the underlying technique is not new to the field, per §3.3's
correction — and a phased build plan that ships something real now without destabilizing
the producer loop that was just fixed this session (§5).

## 1. What Narwhal actually is (so the "upgrade" claim is checkable)

Narwhal (Danezis, Kokoris-Kogias, Sonnino, Spiegelman — *"Narwhal and Tusk: A DAG-based
Mempool and Efficient BFT Consensus"*, 2021) separates two jobs that most chains bundle
together:

- **Data dissemination** (the mempool's real job): get transactions from clients to
  every validator, reliably, in parallel, at high bandwidth.
- **Ordering** (consensus's real job): decide a total order over what's been disseminated.

Narwhal's mempool: each validator runs several **worker** processes (not just one
mempool). Clients submit transactions to any worker. A worker batches transactions and
broadcasts the **batch** (not individual txs) to the same-numbered worker on every other
validator. Each receiving worker acks with a signature over the batch digest. Once a
batch has **2f+1** signatures (a Byzantine-fault-tolerant quorum, tolerating up to `f`
malicious/offline validators out of `3f+1` total), that's a **certificate of
availability** — proof the batch is durably stored across enough of the network to
survive `f` failures.

The validator's **primary** process doesn't handle transactions at all — it only builds
a small **header** each round: a round number, a reference to its own previous header,
and the **digests** (not the data) of the batches its workers certified this round, plus
2f+1 certificates from *other* primaries' headers from the previous round. Headers
chain into a DAG. That DAG *is* the mempool's output — it's already a proof that
everything referenced is available, before consensus ever looks at it. Consensus (Tusk,
or the later Bullshark) then just walks this DAG deterministically to get a total order
— it's nearly free, because the hard part (proving availability) already happened.

The throughput trick: workers run in parallel (N workers × validator), batches amortize
one signature check over many transactions, and headers carry tiny digests instead of
full data — so header/consensus traffic doesn't grow with transaction volume at all.

## 2. What SIGIL has today (the honest starting point)

- `sigil-tx::Mempool` (crates/sigil-tx/src/lib.rs:531) — one `VecDeque<SignedTx>` +
  one dedup `HashSet`, wrapped in a single `Mutex` shared across the whole node
  (`sigil-node/src/main.rs`: `mempool.lock().unwrap()...` at 4 call sites). No workers,
  no batching-for-dissemination, no availability certificates, no DAG. It DOES already
  have an `AuthorizedBatch` type (line 619+) — but that's a *user-level* convenience (one
  wallet signs N of its own ops in one envelope, to amortize verification), not a
  Narwhal-style validator-dissemination batch. Different concept, same word.
- Block inclusion pulls straight from this one mempool: `mempool.lock().unwrap().pull(txgen)`
  (main.rs:908), capped at `SIGIL_TXGEN` or a default **256 tx per block** (main.rs:642).
- The GHOSTDAG braid (`sigil-dagknight`, shipped this session) already gives SIGIL the
  *consensus*-side DAG Narwhal pairs with (Tusk/Bullshark's job). What's missing is
  everything on the *mempool* side of that pairing.
- SIGIL is currently a **single-producer testnet** (Epsilon only — Delta/Gamma/Beta
  confirmed permanently gone, 2026-08-14). This matters a lot for §3.2: Narwhal's core
  security property (2f+1 BFT quorum) needs `≥4` independent validators to mean
  anything. At n=1, "2f+1 of 1" is a degenerate case — self-certification, not BFT. The
  design below is built so the REAL protocol shape is correct and ready NOW, and the BFT
  guarantee activates automatically the moment more validators exist, but it does not
  pretend today's single-producer deployment has Byzantine fault tolerance it doesn't.

## 3. The design

### 3.1 Sharded workers, in-process (Phase 0 — ships now)

Even with one validator, splitting ingestion across N independent workers removes the
CURRENT bottleneck: one global `Mutex<Mempool>` serializing every tx submission, mempool
pull, and dedup check behind a single lock. Shard by `wallet_id` hash into N lanes
(default N = number of CPU cores, configurable):

```rust
pub struct WorkerId(pub u16);

pub struct MempoolWorker {
    id: WorkerId,
    inner: parking_lot::Mutex<WorkerInner>,   // one lock, but only 1/N of the traffic
}

struct WorkerInner {
    verified: VecDeque<SignedTx>,
    seen: HashSet<[u8; 32]>,
}

pub struct ShardedMempool {
    workers: Vec<MempoolWorker>,               // N independent lock domains
}

impl ShardedMempool {
    pub fn worker_for(&self, wallet: &WalletId) -> &MempoolWorker {
        let n = self.workers.len() as u64;
        &self.workers[(fnv1a64(wallet) % n) as usize]
    }
}
```

Same-wallet transactions land in the same worker (preserves per-wallet nonce ordering
without cross-worker coordination); different wallets' transactions ingest and verify
**fully in parallel**, with zero lock contention across workers. This alone is a large,
measurable, real improvement over today's single mutex — and it's a self-contained
change: nothing about block production has to know workers exist yet, they just feed the
existing pull path.

### 3.2 Batches as the unit of dissemination + availability (Phase 1)

A **worker batch** (Narwhal's real unit) is a bundle of transactions from *many*
different senders that one worker gossips as one message, gets acked, and later
certifies:

```rust
pub struct WorkerBatch {
    pub worker: WorkerId,
    pub round: u64,
    pub txs: Vec<SignedTx>,               // many senders, unlike AuthorizedBatch
    pub digest: [u8; 32],                 // BLAKE3 over the encoded batch
}

/// One peer's signed acknowledgement that it received + stored `digest`.
pub struct BatchAck {
    pub digest: [u8; 32],
    pub validator: WalletId,
    pub sig: [u8; 64],
}

/// A batch is CERTIFIED once it has a quorum of acks. `threshold()` is the ONLY
/// place BFT math happens — see below.
pub struct BatchCertificate {
    pub digest: [u8; 32],
    pub acks: Vec<BatchAck>,
}

/// n = live validator set size. Standard BFT quorum is 2f+1 out of n=3f+1.
/// At n=1 this correctly reduces to "1 of 1" — self-certification, no invented
/// security — rather than silently requiring more acks than exist.
///
/// CORRECTED 2026-08-15 (caught in external review, independently re-derived
/// before accepting): `2f+1` is only safe when n is EXACTLY 3f+1. Two quorums
/// of size q intersect in an honest node only if `2q > n+f`; for n NOT of that
/// exact form (e.g. n=5,6,8,9,11,12) `2f+1` falls short and quorum
/// intersection is not guaranteed. `n-f` is the general-n-safe threshold.
/// Verified by brute-force check, n=1..64: `2f+1` fails at 7 of the first 12
/// values tested. The crate's OWN test suite didn't catch this originally —
/// its 3 quorum examples (n=4,7,10) are all exactly 3f+1, where n-f and 2f+1
/// coincide; see sigil-narwhal-mempool's `quorum_threshold_safe_for_non_3f_plus_1_n`
/// test, added specifically because the existing tests couldn't have failed
/// against the buggy version.
pub fn quorum_threshold(n: usize) -> usize {
    if n <= 1 { return n.max(1); }
    let f = (n - 1) / 3;
    n - f
}
```

Blocks then reference **batch digests**, not raw transactions:

```rust
pub struct BlockBatchRef {
    pub digest: [u8; 32],
    pub worker: WorkerId,
}
// header.body: Vec<BlockBatchRef>   — replaces (or augments) the current
//                                      Vec<SignedTx> block body
```

This is the throughput lever that actually matters (see §4): a block can commit
*thousands* of batches' worth of transactions by referencing a handful of digests,
instead of being limited by how many raw tx bytes fit in one block. Block cadence and
raw transaction volume become decoupled, which is the whole point of the Narwhal split.

**Safety correction, 2026-08-15 (external review):** digest-only block bodies are UNSAFE
on a single-producer network and must not activate there, even though the code above is
correct as a general-`n` data structure. A `BatchCertificate` at `n=1` proves only that
the ONE producer stored the batch — if that producer disappears (crash, restart onto a
fresh DB, disk loss) before any peer has fetched the full batch, the digest in the block
is a reference to data that no longer exists anywhere. This is a real, concrete failure
mode, not a theoretical one: SIGIL's `sigil-node` genesis-guard already treats an
unrecoverable local chain state as fatal (`crates/sigil-node/src/main.rs`'s "GENESIS
GUARD L1" comment) for exactly this class of reason. The rule this design adopts:
`BlockBatchRef`-only bodies require `n >= 4` (the point `bft_active(n)` becomes true, per
the corrected `quorum_threshold` above) AND a demonstrated availability certificate from
that quorum — never merely an env flag. Below `n=4`, or before availability is actually
certified, transactions travel inline in the block body exactly as they do today; the
`BlockBatchRef` structure exists in the schema but stays dormant. This mirrors the
project's own mainnet-safety height-gate pattern (CLAUDE.md "mainnet-safe code changes")
applied one level earlier, to testnet, on the theory that it's a good habit to have
already before it's a mainnet requirement.

### 3.3 Erasure-coded batch dissemination — NOT an invention, corrected 2026-08-15

**This section originally claimed erasure-coded batch dissemination as an "invented
upgrade." That claim was wrong and has been corrected after actually checking the
literature** (see `SIGIL_NARWHAL_ARXIV_INVESTIGATION_v0.md` for the full research
writeup). The honest picture:

- **Imitater** (Zeng, Li, Fu, Liu, Jiang — arXiv:2409.19286, Sep 2024, rev. Apr 2025)
  already does almost exactly this: encode each microblock with `(f+1, n)` Reed-Solomon
  into `n = 3f+1` chunks, disperse one Merkle-proof-carrying chunk per node, collect
  `2f+1` signed acks into an **Availability Certificate** — the same shape as the
  `BatchCertificate` below, published a year before this doc. Imitater targets
  HotStuff-family leader-based BFT rather than a Narwhal/Bullshark DAG-certificate
  base, and adds a re-encode-and-compare integrity check this design did not originally
  include (§ below now does).
- Erasure-coded propagation is separately well-proven at the **block** layer: Solana's
  **Turbine** and Monad's **RaptorCast** both erasure-code block data for leader→validator
  fanout, and Ethereum's **danksharding / Data Availability Sampling** (Ethereum
  Foundation roadmap; see arXiv:2407.18085 for a DAS design analysis) uses Reed-Solomon
  for the same "prove availability without needing the whole blob" property, at yet
  another layer (blob data, not mempool batches).
- **Aptos's own team explicitly evaluated and rejected erasure coding for Quorum
  Store** (their Narwhal-derived shared mempool) — their stated reasoning: it "would
  only add complexity and yield no benefits in terms of load balancing," because
  Quorum Store's symmetric full-broadcast already balances load evenly across
  validators. That is a real, considered counter-argument from a production team, not
  an oversight to route around quietly.

**What's actually left, once the honest baseline is Imitater rather than nothing:**
Aptos's rejection is about *load-balancing symmetry* (full replication is already
fair, since every validator does the same work) — a different axis from *total
bandwidth*, where `k`-of-`(k+parity)` sharding is still a real reduction versus full
replication regardless of fairness. Imitater already proves that axis works. What
this design adds on top, honestly scoped:
1. Reuses `flux-aether::rs_shard`/`rs_reassemble` — an already-built, already-tested
   Reed-Solomon coder in THIS workspace (built for chain-snapshot durability) — instead
   of writing a new coder, which is a real engineering economy specific to this
   codebase, not a research contribution.
2. Pairs erasure-coded dispersal with a **Narwhal/Bullshark-family DAG certificate
   layer** (§3.4, riding the GHOSTDAG braid already shipped this session) rather than
   Imitater's HotStuff-style leader-based BFT — a combination I could not find already
   published, though the search was not exhaustive enough to call that a confirmed gap
   rather than a search miss.
3. Imitater's re-encode-and-compare integrity check (decode, then re-encode, then
   compare the Merkle root) is adopted here too — `dissemination.rs`'s
   `reassemble_batch` already re-derives the digest from reconstructed bytes and
   rejects on mismatch, which is the same defense, arrived at independently but not
   claimed as novel now that Imitater's prior publication is known.

Net effect, stated at the correct confidence level, and with the bandwidth math corrected
2026-08-15 (a second external-review catch, same class of error as the quorum bug above:
conflating shard COUNT with shard SIZE). Each of a batch's `k+parity` shards is
approximately `m/k` bytes (a `(k, parity)` Reed-Solomon split, matching Imitater's own
`(f+1, n)` parameterization where shard size is `m/(f+1)`, NOT `m/n`). So:
- **Per-shard reduction** (what one recipient stores/verifies before reconstruction):
  `~1/k`× the full batch — this is the number that matters for a validator's own storage
  and initial download.
- **Total sender-side bytes for one batch** (dispersing all `k+parity` shards, one to
  each peer): `~(k+parity)/k`× the batch size — e.g. Imitater's own `k=f+1, n=3f+1`
  parameterization gives `~3`× the batch size for large `f`, MORE than the raw batch, not
  less; the win is spreading that cost across `k+parity` separate outbound sends instead
  of `n-1` full-size ones, and in the shard SIZE each recipient handles, not in total
  bytes leaving the network. Full replication's comparable total is `(n-1)`× the batch
  size (one full copy per other validator) — so coding still reduces total network bytes
  moved whenever `(k+parity)/k < n-1`, which holds for any real validator count, but the
  reduction is `~k`×, not the `~(k+parity)`× this doc originally and then still incorrectly
  implied. Assembled cheaply from an in-tree primitive, applied to a DAG-certificate
  consensus pairing rather than a leader-based one — not a new cryptographic idea, and
  this doc no longer claims it is one, nor overstates its bandwidth math.

### 3.4 Certification rides the GHOSTDAG braid, not a new DAG

Stock Narwhal builds its own header-DAG (round → round, 2f+1 parent certs) as a
structure *separate from* the consensus DAG that reads it. SIGIL doesn't need a second
DAG: the GHOSTDAG braid already IS a DAG of blocks with blue/red k-cluster coloring
(shipped this session, `sigil-dagknight::ghostdag`). A block's `BlockBatchRef`s are
canonical exactly when the block itself is blue; a batch referenced only by a red
(non-canonical) block is simply not committed, same as any other red-block content.
This reuses machinery that's already built, tested, and — as of this session — actually
producing blocks, instead of standing up parallel round/certificate bookkeeping.

## 4. Honest throughput ceiling — what "10M TPS" can mean here

Three different numbers get called "TPS" in this space and conflating them is how
benchmark claims go wrong. Being explicit:

| Layer | What limits it | SIGIL today | With this design |
|---|---|---|---|
| **Raw ingestion** (workers accepting + verifying signatures) | CPU cores × per-sig verify cost, fully parallel across workers | ~1 mutex, single-threaded | scales ~linearly with worker count + cores; a modern many-core box doing batched ed25519 verify can plausibly reach into the low millions of sigs/s in a synthetic benchmark — this is the number a "10M" headline usually means |
| **Dissemination** (batches reliably reaching quorum) | network bandwidth, erasure-coding overhead | N/A (no batching) | erasure coding cuts required bandwidth per node by ~`(k+parity)/k`× vs. full replication; still fundamentally bounded by real network bandwidth, not free |
| **Chain-committed TPS** (txs actually finalized in blocks peers agree on) | block cadence × ops committed per block | `adaptive rate 8–60 blk/s × 256 tx/blk cap ≈ 15,360 tx/s ceiling, TODAY` | referencing batch digests instead of raw tx lists per block lets ONE block commit many batches (thousands of txs each) — this is what actually raises the *committed* number, not raw ingestion speed |

**The honest claim**: a synthetic, worker-parallel, batched-verification ingestion
benchmark reaching into the millions/s is a real and buildable target — it's measuring
the mempool layer alone, decoupled from consensus and execution, which is exactly what
Narwhal's own paper does too (their reported numbers are mempool throughput, not
execution throughput). **End-to-end chain-committed TPS is a different, lower number**
bounded additionally by block cadence, state-transition execution cost per op, and real
network bandwidth — and no production chain anywhere sustains single-digit millions of
*committed* TPS today. This design pushes SIGIL's *committed* ceiling from ~15k/s toward
whatever `(batches per block) × (txs per batch)` supports once §3.2 lands — a large,
real improvement — while keeping the "10M" framing honestly scoped to the ingestion
layer where it's actually achievable and independently benchmarkable (chronos-style, per
the sigil skill's Rule 0: measure the real number, don't celebrate a microbench next to
a claim it doesn't support).

## 5. Phased build plan

- **Phase 0 (this pass)**: `sigil-narwhal-mempool` crate — sharded worker ingestion
  (§3.1), `WorkerBatch`/`BatchCertificate`/`quorum_threshold` types (§3.2) with unit
  tests including the n=1 degenerate case, erasure-coded batch round-trip using
  `flux-aether` (§3.3) with a test proving k-of-(k+parity) reconstruction. Standalone,
  tested, **not yet wired into `sigil-node`'s producer loop** — Rule 0 discipline: the
  producer loop was just stabilized this session (three real bugs fixed), and it does
  not get destabilized again by cramming in new wiring under time pressure.
- **Phase 1**: wire `ShardedMempool` in as a drop-in replacement for `sigil-tx::Mempool`
  behind the SAME `pull()`/`ingest()` call sites in `main.rs`, gated by an env flag, with
  a chronos-style side-by-side throughput comparison against the current mempool before
  it becomes default.
  - Additive-only: `SIGIL_NARWHAL_MEMPOOL=1` opt-in, mempool.lock() path stays as the
    fallback. No behavior change for anyone who doesn't set the flag.
- **Phase 2**: `BlockBatchRef`-based block bodies (§3.2) — the actual committed-TPS
  lever. Needs a header-schema bump (mainnet-safety height-gate pattern per CLAUDE.md
  §"mainnet-safe code changes", even though this is testnet — good habit to build now).
- **Phase 3**: real multi-validator quorum certification — inert until SIGIL has ≥4
  independent validators again (tracked separately; Delta/Gamma/Beta are gone per
  2026-08-14). `quorum_threshold(n)` already does the right thing the day that's true.

## 6. What this doc is NOT claiming

- Not claiming SIGIL has BFT today — it has one producer.
- Not claiming 10M *committed* TPS — see §4's table.
- Not claiming this replaces GHOSTDAG — it feeds it (§3.4).
- Not shipping wired-in yet — Phase 0 is a tested, standalone crate; integration is
  Phase 1+, deliberately sequenced after the crate proves itself in isolation.
- Not claiming `BlockBatchRef` (digest-only block bodies) is safe below `n=4` validators
  — a single producer's certificate only proves it holds the data itself; if it
  disappears before any peer fetched the batch, the reference is to data that no
  longer exists anywhere. Gated on real quorum, not an env flag (§3.2).
- Not claiming erasure-coded batch dissemination is a novel idea — Imitater
  (arXiv:2409.19286) published the same core mechanism in 2024. See §3.3's
  correction and `SIGIL_NARWHAL_ARXIV_INVESTIGATION_v0.md` for what is and isn't
  actually new here once that's accounted for.
