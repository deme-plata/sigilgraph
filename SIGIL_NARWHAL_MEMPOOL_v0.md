# SIGIL Narwhal-style DAG Mempool v0 — design

Status: **design + Phase-0 scaffold**, not yet wired into the live producer loop.
Author: Grogu (Claude Opus 5) with Viktor, 2026-08-15.

## 0. Why this exists

Viktor's ask (paraphrased across the session): SIGIL already has a real GHOSTDAG-style
DAG *consensus* layer (`sigil-dagknight`, shipped this session — k-cluster blue/red
coloring over the braid). What it does NOT have is a real DAG *mempool* underneath it —
today's mempool (`sigil-tx::Mempool`) is a single `Mutex`-guarded `VecDeque` with one
dedup `HashSet`. Quillon Graph has a Narwhal-inspired mempool already; SIGIL was built
explicitly to fix what Quillon got wrong, so it should get a REAL one — and Viktor asked
for an *invented upgrade* beyond stock Narwhal, aimed at an extremely high throughput
ceiling ("like 10m tps").

This doc is honest about what "10M TPS" can and can't mean here (§4), then lays out an
architecture that is a genuine advance on Narwhal for THIS codebase specifically (§3),
and a phased build plan that ships something real now without destabilizing the producer
loop that was just fixed this session (§5).

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
pub fn quorum_threshold(n: usize) -> usize {
    if n <= 1 { return n.max(1); }
    let f = (n - 1) / 3;
    2 * f + 1
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

### 3.3 The invented upgrade: erasure-coded batch dissemination

Stock Narwhal broadcasts each **full batch** to every worker on every validator — at N
validators that's N-1 full copies leaving each worker, every batch. SIGIL already has a
real, tested Reed-Solomon erasure coder in-tree (`flux-aether::rs_shard` /
`rs_reassemble`, used today for chain snapshot durability — "lose up to N-K shards and
still reconstruct byte-identical"). Reusing it for batch dissemination is a genuine,
buildable advance, not a hand-wave:

- Encode each batch into `k` data + `parity` parity shards:
  `let (orig_len, shards) = rs_shard(&batch_bytes, k, parity);`
- Send ONE shard to each of the `k+parity` peers, instead of the full batch to
  everyone. Per-sender bandwidth for a batch drops from `O(N × batch_size)` to
  `O(batch_size)` total (one shard-worth per peer, and shards sum to ~the original
  size across all peers) — the same fan-out economics that make erasure-coded
  storage systems (and later Narwhal variants like "Quorum Store" / EC-augmented
  DAG mempools) beat naive full-replication at scale.
- Any peer holding `≥k` shards (its own + gossiped ones, or fetched on demand)
  reconstructs the full batch locally: `rs_reassemble(orig_len, k, parity, shards)`.
  A validator does **not** need the full batch to ack availability of a *shard* —
  acking "I hold shard i of digest D" is still a valid availability signal, and the
  quorum certificate can require k-reconstructibility rather than N full copies.
- This composes cleanly with the existing GHOSTDAG braid's own pruned-window model
  (`braid base-anchored at H=...`) — old batches age out of the erasure-coded set
  the same way old blocks already age out of the braid's local window.

Net effect: the same `k`-of-`k+parity` durability guarantee Narwhal gets from full
replication, at roughly `1/(k+parity)` the per-node bandwidth cost of sending the whole
batch to everyone. This is the piece that's genuinely SIGIL-specific — it exists because
`flux-aether` already exists in this workspace, not because it's a standard Narwhal
feature.

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
