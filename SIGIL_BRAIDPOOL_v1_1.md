# SIGIL BraidPool v1.1
## Narwhal-style DAG mempool review, literature-aligned protocol, implementation skeleton, benchmark plan, and research extensions

**Date:** 2026-08-15
**Status:** design / implementation specification (external review, not yet built)
**Starting point:** `SIGIL_NARWHAL_MEMPOOL_v0.md`
**Literature-audit companion:** `SIGIL_NARWHAL_ARXIV_INVESTIGATION_v0.pdf`

Provenance note: this document was contributed by Viktor as a review of and extension to
`SIGIL_NARWHAL_MEMPOOL_v0.md`. Three of its corrections (quorum-threshold math, the
single-producer digest-only-block danger, and the erasure-coding bandwidth math) were
independently re-derived and verified before being accepted, and are applied to v0 and
to `sigil-narwhal-mempool`'s code — see v0's dated 2026-08-15 corrections and the
`quorum_threshold_safe_for_non_3f_plus_1_n` test. The remainder (the full BraidPool
architecture, phase plan A-G, benchmark ladder B0-B6, crate layout, and the "Adaptive
Availability Cells" composition) is preserved here in full as the forward design/roadmap
reference — none of it has been implemented yet as of this commit.

> **Novelty discipline:** The companion investigation is the baseline for prior-art claims in this document. Erasure-coded shared-mempool dissemination is **not** claimed as novel. The specific pairing with SIGIL's already-running GHOSTDAG-style braid was **not found in that same-day search**, but the search was explicitly non-exhaustive; this document therefore treats the pairing as an engineering design with a possible research angle, not as a confirmed literature gap.

---

## 1. Executive conclusion

The v0 design has the right architectural instinct: decouple transaction dissemination from ordering, shard ingestion across workers, batch transactions, certify availability, and reuse SIGIL's existing GHOSTDAG braid rather than building a second consensus DAG.

Before Phase 2, however, five items should be corrected:

1. **Quorum math:** `2*f + 1` is safe only when the committee size is exactly `3*f + 1`. For arbitrary `n`, use `f = floor((n-1)/3)` and a quorum of `n-f` — a **deliberately conservative** threshold, not the unique generalization of `2f+1`. The tight minimum satisfying quorum intersection (`2q-n > f`) is `q_min = floor((n+f)/2) + 1`, which is sometimes smaller than `n-f` (e.g. n=6,f=1: q_min=4, n-f=5). BraidPool uses `n-f` anyway because it's simpler and gives more honest data-availability holders, not because it's the only valid choice.
2. **Digest-only blocks must not activate on a one-producer network.** A self-certified digest does not make the batch recoverable if the producer disappears. Keep transaction data inline until real data availability exists.
3. **Erasure-code bandwidth wording:** with an `(f+1,n)` Reed-Solomon code, each shard is approximately `m/(f+1)`, not `m/n`. The EXACT initial-dispersal ratio, corrected 2026-08-15, is `n/k = (3f+1)/(f+1)`, not the earlier "~3m" approximation, which only holds asymptotically for large `f`. Concretely: n=4,f=1,k=2 → **2m**; n=7,f=2,k=3 → **2.33m**; n=10,f=3,k=4 → **2.5m**. This matters specifically for SIGIL's likely first real multi-validator deployment (small n), where "~3m" overstates the cost. This is a design derivation, not a claim attributed to the companion paper.
4. **Availability acknowledgements need stronger domain separation.** A signature must bind chain, epoch, worker, sequence, coding profile, batch digest, and shard commitment—not only the digest. Specifically: sign `shard_index` and `shard_hash` INSIDE `BatchAckMessageV1` (not just `shard_root` at the batch level), so an ACK means precisely "validator V attests it holds shard i, hash H, under batch commitment R" — and independently recompute the expected shard index from the deterministic assignment during verification rather than trusting the transmitted field. Do both: sign it AND recompute it (§3.5, §11).
5. **The system needs bounded queues, deterministic garbage collection, recovery, and anti-explosion logic before high-load claims are meaningful.**

The proposed replacement design is called **BraidPool**: a braid-native availability layer that can switch between replicated and erasure-coded dissemination, commits large sets of batches through a `BatchSetRoot`, uses epoch-salted worker assignment, and keeps data-availability state separate from GHOSTDAG ordering state.

The accompanying literature investigation changes how this should be presented publicly. **Imitater is direct prior art for the erasure-coded shared-mempool mechanism**: `(f+1,n)` Reed-Solomon chunks, Merkle proofs, `2f+1` acknowledgements, an availability certificate, and reconstruct/re-encode/commitment-check recovery. Aptos's Quorum Store is also an important production counterpoint because its team considered erasure coding and rejected it for its symmetric full-broadcast topology. BraidPool therefore treats coding as an experimentally selected engineering option, not an automatic upgrade and not a novelty claim.

---

## 2. What v0 gets right

- Narwhal's key split—**dissemination/storage vs. ordering**—is the correct conceptual model.
- Worker parallelism is the first practical bottleneck to attack.
- Same-wallet affinity is useful for nonce sequencing.
- Batch-level acknowledgement amortizes protocol overhead.
- Reusing the existing GHOSTDAG braid avoids maintaining a redundant Narwhal header DAG.
- The design correctly distinguishes ingestion throughput from committed throughput.
- The correction concerning Imitater is scientifically important: erasure-coded shared-mempool dissemination is direct prior art, not a SIGIL invention. The defensible SIGIL-specific statement is narrower: reusing the in-tree coder is an engineering economy, while the erasure-coded batch layer riding the already-running GHOSTDAG braid was **not found in the companion note's non-exhaustive search**.

---

## 3. Critical architecture corrections

### 3.1 Quorum threshold

The v0 function:

```rust
pub fn quorum_threshold(n: usize) -> usize {
    if n <= 1 { return n.max(1); }
    let f = (n - 1) / 3;
    2 * f + 1
}
```

fails for committee sizes that are not exactly `3f+1`.

Examples:

| n | f=floor((n-1)/3) | v0 `2f+1` | safer `n-f` |
|---:|---:|---:|---:|
| 1 | 0 | 1 | 1 |
| 2 | 0 | 1 | 2 |
| 3 | 0 | 1 | 3 |
| 4 | 1 | 3 | 3 |
| 5 | 1 | 3 | 4 |
| 6 | 1 | 3 | 5 |
| 7 | 2 | 5 | 5 |
| 8 | 2 | 5 | 6 |

For two conflicting certificates to be unable to intersect only in Byzantine signers, the two quorums must intersect in more than `f` members. `q=n-f` gives that property under the usual `n >= 3f+1` assumption.

**`n-f` is a deliberately conservative choice, not the unique correct generalization**
(corrected 2026-08-15). The exact minimum quorum satisfying `2q-n > f` is
`q_min = floor((n+f)/2) + 1`, which is sometimes strictly smaller than `n-f` — e.g. at
`n=6, f=1`: `q_min=4` but `n-f=5`. BraidPool uses `n-f` regardless: it's simpler to
state, and a larger quorum means more independently-honest holders of the data at
certification time, which is a real availability benefit `n-f` buys beyond the bare
safety minimum — but that tradeoff should be stated, not implied to be free.

```rust
pub fn max_byzantine(n: usize) -> usize {
    n.saturating_sub(1) / 3
}

pub fn availability_quorum(n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    let f = max_byzantine(n);
    n - f
}

pub fn bft_active(n: usize) -> bool {
    n >= 4
}
```

For SIGIL, a stronger rollout rule is preferable: **BFT data-availability mode is disabled for `n < 4`**, even though helper functions remain mathematically defined.

---

### 3.2 Do not make one-node blocks digest-only

At `n=1`, a certificate proves only that the producer itself stored the data. If the producer writes:

```text
block -> batch_digest
```

and later disappears before peers obtain the batch, the chain has committed an unrecoverable reference.

Therefore:

```rust
pub enum BodyMode {
    /// Current safe mode: full tx data travels with the block.
    InlineTransactions,

    /// Transitional mode: block commits batch metadata, but full batch bytes
    /// are also included in an authenticated sidecar distributed with block.
    AnchoredSidecar,

    /// Only valid once a real multi-node availability quorum is active.
    CertifiedBatchRefs,
}
```

Activation rule:

```text
n < 4                         => InlineTransactions
n >= 4 but DA not proven      => InlineTransactions / AnchoredSidecar
n >= 4 + DA certification     => CertifiedBatchRefs
```

Do not let an environment variable alone bypass this safety gate.

---

### 3.3 Erasure coding: prior art, engineering trade-off, and corrected bandwidth model

The literature-audit companion establishes the prior-art baseline:

- **Imitater (2024)** already uses an `(f+1,n)` Reed-Solomon code for shared-mempool microblocks, sends one Merkle-proof-carrying chunk to each of `n=3f+1` nodes, forms an availability certificate from `2f+1` signed acknowledgements, and later reconstructs from `f+1` chunks.
- Imitater also performs the important **decode → re-encode → recompute commitment → compare** integrity check before trusting reconstructed data.
- Erasure coding is also established at other layers, including block fanout and blob/data-availability systems.
- **Aptos Quorum Store is a real counter-example in design choice**: its team evaluated erasure coding and rejected it because symmetric full broadcast already balances load across validators and coding adds complexity.

So the correct SIGIL framing is: **known technique, potentially useful here, benchmark before defaulting to it.**

For an `(k,n)` Reed-Solomon code over a batch of `m` bytes:

```text
shard_size ~= m/k
```

If `k=f+1` and `n=3f+1`, a sender transmitting one shard to each validator sends, EXACTLY
(corrected 2026-08-15 — the earlier "~3m" was only the large-`f` asymptote, not the real
ratio at small committee sizes):

```text
n * (m/k) = ((3f+1)/(f+1)) * m
```

which only approaches `3m` for large `f`. For SIGIL's likely first real deployment sizes:

| n | f | k=f+1 | (3f+1)/(f+1) |
|---:|---:|---:|---:|
| 4 | 1 | 2 | **2.00**m |
| 7 | 2 | 3 | **2.33**m |
| 10 | 3 | 4 | **2.50**m |
| large | large | large | → 3.00m (asymptote only) |

before Merkle proofs, signatures, headers, retransmission, and retrieval. This is a protocol-level derivation used for capacity planning; it is not a novelty claim.

The useful property is not "free bandwidth." It is that an individual recipient can initially store only a coded fraction while the certificate proves that enough independently held pieces exist for reconstruction. Whether that beats symmetric full replication in SIGIL's actual network depends on batch size, committee size, CPU cost, packet loss, and retrieval frequency.

**Design rule:** ship replication first in the multi-validator testnet, then enable Reed-Solomon only after side-by-side measurements show a benefit for a defined workload.

---

### 3.4 Batch IDs need canonical structured hashing

Never define the batch identity as only:

```text
BLAKE3(encoded_batch)
```

unless `encoded_batch` has a frozen canonical format.

Recommended:

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BatchHeaderV1 {
    pub version: u16,
    pub chain_id: [u8; 32],
    pub epoch: u64,
    pub worker: WorkerId,
    pub sequence: u64,
    pub previous: Option<[u8; 32]>,
    pub tx_count: u32,
    pub uncompressed_len: u32,
    pub tx_root: [u8; 32],
    pub coding: CodingProfile,
    pub shard_root: [u8; 32],
}

pub fn batch_id(header: &BatchHeaderV1) -> [u8; 32] {
    let bytes = canonical_encode(header);
    blake3_domain_hash(b"SIGIL/BATCH/V1", &bytes)
}
```

Every consensus-relevant serialization must have golden-vector tests.

---

### 3.5 Acknowledgements must bind the entire availability statement

**Corrected 2026-08-15: `shard_index` moved INSIDE the signed message, and `shard_hash`
added.** The original draft left `shard_index` outside `BatchAckMessageV1` (only
`shard_root`, the whole batch's commitment, was signed) — meaning the signature didn't
actually attest to WHICH shard the validator holds, only that it holds *some* shard
under that root. Moving `shard_index` and `shard_hash` inside the signed message makes
an ACK mean something precise: "validator V attests it possesses shard `i`, whose hash
is `H`, under batch commitment `R`." Verification does BOTH: check the signature over
the field as transmitted, AND independently recompute the expected shard index from the
deterministic per-validator assignment — never trust the transmitted index alone.

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BatchAckMessageV1 {
    pub chain_id: [u8; 32],
    pub epoch: u64,
    pub worker: WorkerId,
    pub sequence: u64,
    pub batch_id: [u8; 32],
    pub shard_root: [u8; 32],
    pub coding: CodingProfile,

    // Moved inside the signed statement (was outside, in `BatchAck`, unsigned):
    pub shard_index: u16,
    pub shard_hash: [u8; 32],
}

pub struct BatchAck {
    pub validator: ValidatorId,
    pub signature: ValidatorSignature,
}
```

Validation must check:

- signer is in the epoch committee;
- one acknowledgement per validator;
- the SIGNED `shard_index` matches the deterministic assignment recomputed locally for
  this validator — reject if the validator signed a shard it wasn't assigned;
- `shard_hash` matches the actual bytes of that shard against `shard_root` (Merkle proof);
- signature verifies over a domain-separated canonical message including `shard_index`
  and `shard_hash`, not just `shard_root`;
- acknowledgement belongs to the same epoch and chain;
- quorum weight/count is sufficient.

Avoid hard-coding `[u8; 64]` into the protocol object if validator signatures may later change.

---

## 4. BraidPool architecture

```text
                    CLIENTS
                       |
             +---------+---------+
             |  admission gate   |
             | bytes / CPU / fee |
             +---------+---------+
                       |
              epoch-salted hash
                       |
      +----------------+----------------+
      |                |                |
   Worker 0         Worker 1        Worker N-1
      |                |                |
 bounded queue      bounded queue     bounded queue
 verify outside     verify outside    verify outside
 critical lock      critical lock     critical lock
      |                |                |
      +-------- batch builders ---------+
                       |
                 BatchEnvelope
                       |
             +---------+----------+
             |                    |
       Replicated DA          RS-coded DA
             |                    |
             +---- Availability --+
                  Certificate
                       |
                 BatchSetRoot
                       |
                 GHOSTDAG block
                       |
          blue/red canonical ordering
                       |
               fetch / reconstruct
                       |
                  execution
```

---

## 5. Epoch-salted worker assignment

Plain FNV assignment is fast but allows an attacker who can generate many wallet IDs to search for IDs that concentrate on one lane.

Use a public epoch salt derived from already-finalized history:

```rust
pub fn worker_for(
    wallet: &WalletId,
    worker_count: usize,
    epoch_seed: &[u8; 32],
) -> usize {
    let mut h = blake3::Hasher::new();
    h.update(b"SIGIL/WORKER/V1");
    h.update(epoch_seed);
    h.update(wallet.as_bytes());
    let out = h.finalize();
    let x = u64::from_le_bytes(out.as_bytes()[0..8].try_into().unwrap());
    (x as usize) % worker_count
}
```

Properties:

- same wallet stays on one lane during an epoch;
- assignments change between epochs;
- precomputed lane-targeting wallets become much less useful;
- every node can reproduce the mapping.

Use a frozen worker count for an epoch. Changing `N` mid-epoch would remap wallets and complicate ordering.

---

## 6. Worker implementation

The lock should protect queue mutation only. Signature verification and decoding must happen before the lock.

```rust
pub struct MempoolWorker {
    id: WorkerId,
    ingress: tokio::sync::mpsc::Sender<VerifiedTx>,
    state: parking_lot::Mutex<WorkerState>,
}

struct WorkerState {
    ready: VecDeque<VerifiedTx>,
    seen: hashbrown::HashSet<TxId>,
    bytes: usize,
}

pub struct WorkerLimits {
    pub max_txs: usize,
    pub max_bytes: usize,
    pub per_wallet_max_txs: usize,
}
```

Pipeline:

```text
socket/read
 -> cheap structural checks
 -> global byte budget
 -> signature verification pool
 -> nonce/replay validation
 -> deterministic worker
 -> bounded worker channel
 -> batch builder
```

Do not hold a queue mutex while verifying Ed25519/ML-DSA, hashing a large payload, doing Reed-Solomon encoding, or waiting on network I/O.

---

## 7. Pull scheduler

Round-robin alone can be gamed; global fee sorting can destroy wallet nonce locality.

Recommended two-level scheduler:

1. each worker exposes its next executable transaction or sealed batch;
2. coordinator uses deficit round robin across workers;
3. within a wallet, nonce order is strict;
4. within equal readiness, use fee/age policy;
5. cap consecutive selections from one wallet.

Pseudo-interface:

```rust
pub trait BatchSource {
    fn peek_cost(&self) -> Option<usize>;
    fn pop_ready(&self, budget: &mut PullBudget) -> Option<VerifiedTx>;
}
```

---

## 8. Batch envelope

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BatchEnvelopeV1 {
    pub header: BatchHeaderV1,
    pub txs: Vec<SignedTx>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub enum CodingProfile {
    Replicated,
    ReedSolomon {
        data_shards: u16,
        parity_shards: u16,
    },
}
```

Batch sealing conditions should be deterministic by local policy, not consensus:

```text
seal when:
  bytes >= target_batch_bytes
  OR tx_count >= target_batch_txs
  OR oldest_tx_age >= target_batch_latency
```

The resulting batch identity is consensus-visible; the local moment at which a worker chooses to seal it does not need to be globally identical.

---

## 9. Hybrid dissemination plane

### 9.1 Why hybrid

The companion investigation gives both sides of the decision:

- **For coding:** Imitater demonstrates an erasure-coded shared mempool and reports throughput/latency benefits over its baseline under faults in its own evaluation.
- **Against assuming coding is always better:** Aptos's Quorum Store team explicitly rejected erasure coding for its symmetric shared-mempool topology because full broadcast already distributes sender load evenly and coding would add complexity.

These statements are not contradictory: they optimize different systems and different bottlenecks.

BraidPool therefore makes the dissemination profile an **epoch-level engineering choice** rather than protocol ideology. Replication is the simpler baseline; Reed-Solomon is activated where measured total bandwidth, fault recovery, or storage pressure justifies its additional CPU and protocol complexity.

### 9.2 Deterministic epoch profile

```rust
pub struct DaEpochConfig {
    pub mode: DaMode,
    pub min_batch_bytes_for_rs: usize,
    pub data_shards: u16,
    pub parity_shards: u16,
}

pub enum DaMode {
    ReplicateOnly,
    ReedSolomonOnly,
    SizeHybrid,
}
```

The configuration is committed at an epoch boundary.

In `SizeHybrid`:

```text
batch_bytes < threshold => Replicated
batch_bytes >= threshold => Reed-Solomon
```

The threshold comes from benchmark data, not a hard-coded theoretical assumption.

---

## 10. Progressive repair

For RS-coded batches, precompute all epoch-configured shards and commit all shard hashes under `shard_root`.

Normal path:

1. producer sends each validator its assigned shard;
2. validators verify Merkle proof, persist shard, acknowledge;
3. producer forms an availability certificate;
4. missing recipients fetch shards only if/when reconstruction is needed.

Repair path:

1. request carries `batch_id` and missing shard bitmap;
2. peers are selected deterministically from certificate signers;
3. requester stops after `k` valid distinct shards;
4. decode;
5. re-encode;
6. recompute `shard_root`;
7. reject if the commitment differs.

This is explicitly **adopted prior art**, not an invention: the companion investigation identifies the same decode/re-encode/commitment-compare defense in Imitater.

---

## 11. Certificate format

**Corrected 2026-08-15, structural consequence of §3.5's fix:** once `shard_index` and
`shard_hash` live INSIDE the signed statement, a single `AvailabilityCertificateV1`
covering many validators can no longer share ONE `BatchAckMessageV1` — different
validators hold different shards, so each one signs a DIFFERENT `shard_index`/
`shard_hash` pair. Split the batch-level fields (shared, unsigned-by-themselves) from
the per-validator shard fields (carried per-ack, each independently verified as part of
that validator's signature):

```rust
/// The fields every ack for one batch shares. NOT signed on its own — each
/// validator signs this concatenated with ITS OWN shard_index/shard_hash
/// (see BatchAckMessageV1, §3.5); this struct exists so the certificate
/// doesn't repeat the shared fields once per validator.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BatchStatementV1 {
    pub chain_id: [u8; 32],
    pub epoch: u64,
    pub worker: WorkerId,
    pub sequence: u64,
    pub batch_id: [u8; 32],
    pub shard_root: [u8; 32],
    pub coding: CodingProfile,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AvailabilityCertificateV1 {
    pub statement: BatchStatementV1,
    pub signers: BitVec,
    /// Each ack carries its OWN shard_index/shard_hash — different
    /// validators hold different shards under the same batch commitment.
    pub acks: Vec<BatchAck>,
}

impl AvailabilityCertificateV1 {
    pub fn verify(&self, committee: &Committee) -> Result<(), DaError> {
        let q = availability_quorum(committee.len());

        ensure!(bft_active(committee.len()), DaError::BftInactive);
        ensure!(self.unique_signer_count() >= q, DaError::NoQuorum);
        self.verify_membership(committee)?;
        self.verify_unique_signers()?;
        // Each ack's signature covers `statement` ++ its OWN shard_index/shard_hash —
        // verify_all_signatures reconstructs that per-ack message, it does not check
        // against one shared BatchAckMessageV1.
        self.verify_all_signatures()?;
        // Recompute each ack's expected shard_index from the deterministic
        // per-validator assignment and reject any ack whose SIGNED shard_index
        // doesn't match — never trust the transmitted index alone (§3.5).
        self.verify_shard_assignments()?;
        Ok(())
    }
}
```

Later, signatures can be aggregated, but first ship the unambiguous version and benchmark it.

---

## 12. BatchSetRoot: the real block-size lever

A block cannot reference "thousands of batches with a handful of digests" unless another aggregation layer exists.

Add one:

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BatchRefV1 {
    pub batch_id: [u8; 32],
    pub certificate_hash: [u8; 32],
    pub worker: WorkerId,
    pub tx_count: u32,
}

pub struct BatchSetV1 {
    pub refs: Vec<BatchRefV1>,
}

pub fn batch_set_root(set: &BatchSetV1) -> [u8; 32] {
    merkle_root_domain(b"SIGIL/BATCHSET/V1", &set.refs)
}
```

Block header:

```rust
pub struct BlockHeader {
    // existing fields...
    pub batch_set_root: [u8; 32],
    pub batch_count: u32,
    pub committed_tx_count: u64,
}
```

The sidecar carries the refs and certificates. The header stays essentially constant-size.

---

## 13. GHOSTDAG integration

Keep two state machines conceptually separate:

```text
Availability:
UNKNOWN -> PARTIAL -> CERTIFIED -> RECONSTRUCTED

Ordering:
UNSEEN -> DAG_VISIBLE -> BLUE/RED -> FINAL
```

A blue block does not magically create missing data.

Execution condition:

```text
execute batch iff:
  containing block is canonical/final enough
  AND availability certificate is valid
  AND batch bytes are locally reconstructed/verified
```

A red-only batch can be garbage-collected after the configured finality/retention horizon unless referenced by another still-live object.

---

## 14. Anti-explosion and garbage collection

DAG mempools require explicit resource bounds.

Recommended limits:

```rust
pub struct MempoolLimits {
    pub global_bytes: usize,
    pub per_worker_bytes: usize,
    pub per_wallet_bytes: usize,
    pub max_uncertified_batches: usize,
    pub max_certified_uncommitted_batches: usize,
    pub orphan_retention_rounds: u64,
    pub max_repair_requests_per_peer: u32,
}
```

Garbage-collection keys:

- committed blue batch;
- red-only batch older than finality horizon;
- expired epoch;
- replaced/replayed nonce;
- certificate that can no longer become canonical;
- invalid batch/shard.

Never make the unbounded DAG itself the queue.

---

## 15. Fair-ordering hook

Recent 2026 work shows that high-throughput DAG ordering is not automatically fair. A deterministic validator-index tie-break can create ordering bias.

BraidPool should expose enough authenticated metadata to add a fairness layer later without changing the availability protocol:

```rust
pub struct BatchOrderMetaV1 {
    pub creator: ValidatorId,
    pub epoch: u64,
    pub sequence: u64,
    pub first_seen_round: u64,
    pub tx_root: [u8; 32],
}
```

Do not invent a home-grown fairness rule in Phase 1. Evaluate Tilikum-style or post-consensus visibility approaches separately.

---

## 16. Post-quantum signature path

The protocol layer should not encode Ed25519's 64-byte signature size directly.

```rust
pub enum ValidatorSignature {
    Ed25519([u8; 64]),
    MlDsa(Vec<u8>),
    Hybrid {
        classical: [u8; 64],
        pq: Vec<u8>,
    },
}
```

Even if Phase 1 uses Ed25519, keeping the wire object algorithm-tagged avoids a second schema break later.

Benchmark certificate verification separately for each signature mode.

---

## 17. Phase plan

### Phase A — correctness scaffold — **SHIPPED 2026-08-15**, all in `sigil-narwhal-mempool`
- Replace quorum math. ✅ `quorum_threshold` fixed to `n-f` (commit `466cc93`).
- Canonical batch header + domain-separated IDs. ✅ `canonical.rs`: `BatchHeaderV1`,
  `BATCH_HEADER_VERSION`, `batch_id()` = `BLAKE3("SIGIL/BATCH/V1" || canonical_encode(header))`.
  `WorkerBatch::canonical_header(...)` wires it up; the old bare `digest()` stays for
  `dissemination.rs`'s existing round-trip, explicitly marked superseded.
- Bounded worker queues. ✅ `worker.rs`: `WorkerLimits { max_txs, max_bytes,
  per_wallet_max_txs }`, enforced BEFORE signature verification (§14's DoS-resistance
  point), returns a distinct `BoundedIngestResult.rejected_capacity` rather than folding
  capacity rejections into `invalid`. Per-wallet slots are reserved optimistically during
  the dedup pass and released again if verification then rejects the tx (an invalid
  signature must not permanently burn a wallet's own quota).
- Epoch-salted worker assignment. ✅ `worker.rs`: `epoch_salted_index()` =
  `BLAKE3("SIGIL/WORKER/V1" || epoch_seed || wallet) mod worker_count`, `ShardedMempool`
  holds the seed behind a `RwLock` with `rotate_epoch()`; already-queued txs are NOT
  reshuffled by a rotation (only routing for future `ingest()` calls changes).
- Golden serialization vectors. ✅ `merkle.rs` and `canonical.rs` each pin one hex-encoded
  BLAKE3 value for a fixed input; both were confirmed to actually FAIL against a
  tampered value before being left in their real, passing state (not just written and
  trusted).
- No producer behavior change. ✅ None of this is wired into `sigil-node`'s producer
  loop — the mining-stall fix that shipped hours earlier this same session stays
  untouched. 43/43 crate tests green; SIGIL mainnet-genesis-reset chain mining
  confirmed still healthy immediately before and after this work.

### Phase B — sharded mempool behind flag — **FULLY SHIPPED 2026-08-15**: benchmarked AND wired live
- `SIGIL_BRAIDPOOL=1` — **now a real, deployed producer-loop flag.** The
  correctness hazard noted below was closed in a follow-up same-day commit
  (`7f80f9b`, `sigil_narwhal_mempool::MempoolBackend`) rather than left
  deferred: `sigil_api::AppState` and `sigil-node`'s producer loop now hold
  the exact same `Arc<MempoolBackend>`, so there is structurally no seam left
  where the two could diverge onto different mempools. Deployed live to
  Epsilon with the same care as the mining-stall fix earlier this session:
  both crates' full test suites green pre-deploy (sigil-api 11/11, sigil-node
  53/53, sigil-narwhal-mempool 47/47), binary backed up before swap, verified
  post-deploy that `SIGIL_BRAIDPOOL` is unset in the live unit (legacy
  backend active — this deploy shipped the escape hatch, not a behavior
  change) and that mining, the money API, and the ingest routes all still
  work exactly as before. The hazard investigation itself is preserved below
  unedited, since it's the reasoning that led to `MempoolBackend` existing:
  `sigil_api::AppState` (the money API's `/v1/transactions` handler) and
  `sigil-node/main.rs`'s block-body `pull()` call share ONE `Arc<Mutex<Mempool>>`
  today. If only the `main.rs` pull/TXGEN call site swapped to `ShardedMempool`
  behind the flag, real user transactions submitted via the money API would land
  in the OLD mempool and never get pulled into a block — silently dropped, not
  merely slow, for as long as the flag was set. A safe live swap needs
  `sigil_api` to become backend-aware too, which is a bigger blast radius into a
  second crate, on a node whose producer loop was stabilized (three real bugs
  fixed) earlier THIS SAME SESSION — deliberately deferred rather than rushed.
- same `ingest()`/`pull()` facade as current mempool: ✅ done regardless — this
  was already true after Phase A (`ShardedMempool` matches `sigil_tx::Mempool`'s
  shape for the plain-tx path); no new work needed here.
- **benchmark against old global mutex: ✅ done — real, measured, both a
  positive and a negative result reported honestly.** New
  `crates/sigil-narwhal-mempool/src/bin/mempool_bench.rs`, run capped
  (`systemd-run --scope`) so it never contended with the live mining node,
  verified against the SAME producing chain both before and after (still
  healthy, height climbing normally throughout):

  | Scenario | Legacy (1 mutex) | Sharded | Ratio |
  |---|---:|---:|---:|
  | Sequential, 1 thread, 20k txs, 8 workers | 3,238 tx/s | 3,097 tx/s | **0.96×** (slightly SLOWER) |
  | Parallel, 8 threads, 20k txs, 8 workers | 3,026 tx/s | 19,352 tx/s | **6.39×** |
  | Sequential, 1 thread, 40k txs, 16 workers | 2,789 tx/s | 2,642 tx/s | **0.95×** (slightly SLOWER) |
  | Parallel, 16 threads, 40k txs, 16 workers | 2,748 tx/s | 24,685 tx/s | **8.98×** |

  **Reported honestly, not cherry-picked:** sharding is measurably NOT free —
  under purely sequential ingestion (one thread, no contention to relieve),
  it's consistently ~5% SLOWER than the single mutex, from routing overhead
  with nothing to gain from it. The real, large benefit (6-9×, scaling with
  worker/thread count) only shows up under genuine concurrent load, which is
  the realistic production shape (many senders submitting independently) but
  is worth stating precisely rather than leading with the flattering number
  alone. The legacy mutex caps at ~2,700-3,200 tx/s regardless of thread count
  (one lock serializes everything) — sharding's win is specifically removing
  that ceiling, not raw per-op speed.
  All runs verified zero silently-dropped transactions (the bench asserts
  landed-count == submitted-count in every scenario before reporting a number
  — a fast benchmark that quietly lost transactions would be worse than no
  benchmark).
- no block schema change: ✅ — nothing about block structure touched this pass.

### Phase C — local batches — **SHIPPED 2026-08-15**, all in `sigil-narwhal-mempool`
- Batch construction. ✅ new `sealer.rs`: `BatchSealer` accumulates pulled txs
  per worker and seals a `(BatchHeaderV1, WorkerBatch)` pair once a
  `SealPolicy` threshold fires (`target_bytes` / `target_txs` / `max_latency`,
  OR'd — whichever fires first). Sealing moment is explicitly a LOCAL
  decision (§8: "deterministic by local policy, not consensus") — two nodes
  sealing the same pending txs at different wall-clock moments just produce
  two different (still individually valid) batches; nothing about
  correctness depends on sealing being synchronized. Each sealer keeps its
  own monotonic `sequence` counter and chains each new batch's `previous`
  field to its own last `batch_id`, so a worker's batches form a real local
  chain, not just an unordered set. `force=true` seals whatever's pending
  regardless of policy (e.g. on shutdown), so nothing queued is silently lost.
- Batch store. ✅ new `batch_store.rs`: `BatchStore`, sealed batches keyed by
  `batch_id`, `insert`/`get`/`contains`/`remove`, `&self`-only (RwLock
  inside) matching the rest of the crate's shared-handle style.
- Batch metrics. ✅ `BatchStoreMetrics { sealed_total, bytes_total,
  live_count }` — `sealed_total`/`bytes_total` are lifetime counters (never
  decremented by `remove`, so they answer "how much work has this store
  done" separately from "how much is live right now"). Plain atomics, not
  wired to a real metrics exporter yet — §23's Prometheus-style names are the
  target shape for whenever this is actually deployed, not faked here.
- Still inline transactions in blocks. ✅ none of this touches
  `sigil-node`'s block body — `BlockBatchRef`-referenced bodies stay Phase F,
  gated on real multi-validator availability per the design doc's §3.2
  safety correction (a single producer's own certificate doesn't make a
  batch recoverable if the producer disappears).

59/59 crate tests green (up from 47); SIGIL chain confirmed still mining
continuously immediately before and after this work, unaffected since none
of it touches the producer loop or the deployed binary.

### Phase D — multi-node availability testnet
- require committee `n>=4`;
- replicated availability first;
- certificate verification;
- recovery tests with producer loss.

### Phase E — RS-coded availability — **SHIPPED 2026-08-15**, all in `sigil-narwhal-mempool`
- Use in-tree Reed-Solomon. ✅ unchanged, still `flux-aether::rs_shard`/`rs_reassemble`
  — but a REAL identity bug was fixed first: `dissemination.rs`'s `shard_batch`/
  `reassemble_batch` were still keyed on `WorkerBatch`'s old Phase-0 bare digest,
  not the canonical `BatchHeaderV1::batch_id()` that Phase A/C's `BatchSealer`/
  `BatchStore` actually use. They now shard the `(header, batch)` PAIR together,
  so reconstruction recovers both and can recompute `batch_id()` from what was
  ACTUALLY reconstructed, not trust a separately-carried header.
- Reconstruction + re-encode check (**adopted from established prior art —
  Imitater, not claimed novel**). ✅ now genuinely wired to the real identity:
  `reassemble_batch` checks `batch_id()` match AND independently re-derives
  `tx_root` from the reconstructed transactions and compares against the
  header's claimed root — two separate checks, so a bug breaking only one
  wouldn't silently accept corrupt content. A test (`rejects_when_tx_root_disagrees_with_reconstructed_txs`)
  constructs exactly that failure mode by hand and confirms it's caught.
- Deterministic peer repair. ✅ new `repair.rs`: `repair_priority`/`next_repair_peer`
  rank a batch's certificate signers via `BLAKE3(digest || validator)`, so
  every node computes the SAME ask-order with no coordination, but the order
  varies PER BATCH (repeated repairs spread load across signers instead of
  always hammering whichever validator sorts first in some fixed list).
  **Known gap, stated plainly:** the design doc's §3.5/§11 correction (from
  the earlier Viktor review pass) binds `shard_index` into the signed ack so
  a real deployment knows exactly which shard each signer holds. `types::BatchAck`
  was never updated to that shape — still Phase-0/A's `{digest, validator,
  sig}`. This module ranks "who to ask next," not "who holds shard i
  specifically" — closing that gap is tracked follow-up work (a bigger,
  separately-reviewable change to an already-shipped, already-tested signed
  message), not done in this pass.
- Compare bandwidth/CPU against replication. ✅ new `src/bin/dissemination_bench.rs`,
  measuring the REAL `flux-aether` coder (not just the closed-form math
  already in §3.3) on a 4096-tx batch (matching `SealPolicy::default()`'s
  `target_txs`), across three committee sizes:

  | n (validators) | k / parity | Bandwidth ratio (coded/replicated) | Encode | Decode | Total CPU |
  |---:|---:|---:|---:|---:|---:|
  | 4 | 2/2 | **0.667×** | 31.2 ms | 127.7 ms | 158.9 ms |
  | 7 | 3/4 | **0.389×** | 31.0 ms | 113.4 ms | 144.3 ms |
  | 10 | 4/6 | **0.278×** | 46.0 ms | 155.9 ms | 201.9 ms |

- **Document why coding wins (or doesn't) for the measured SIGIL workload,
  stated honestly rather than picking the flattering half:** bandwidth-wise,
  coding wins clearly and increasingly at larger committee sizes — exactly
  the shape the closed-form math predicted. **But the CPU cost is real and
  large relative to SIGIL's own cadence**: 144-202ms of encode+decode for one
  4096-tx batch, against an adaptive block rate of 8-60 blocks/s (16.7-125ms
  per block) measured earlier this session. Doing this coding SYNCHRONOUSLY
  in the block-production hot path, at the default `SealPolicy` batch size,
  would very plausibly be a genuine THROUGHPUT BOTTLENECK, not a free
  bandwidth win — the opposite of what a "10M TPS" pitch would want to hear,
  and reported here anyway. This does not mean coding is wrong for SIGIL; it
  means: (a) coding needs to run off the production hot path (a background
  dispersal worker, not inline with minting), (b) smaller batches or a lower
  `parity` reduce the CPU cost at the price of some bandwidth benefit, and
  (c) Aptos's own rejection of coding for Quorum Store — cited honestly back
  in the arXiv investigation — starts to look less like "they didn't need it"
  and more like it may ALSO reflect a real CPU-cost tradeoff other teams hit,
  not merely a load-balancing argument. None of this was measured before
  Phase E; it's a genuinely new (if unglamorous) finding from actually running
  the numbers instead of trusting the bandwidth math alone.

65/65 crate tests green (up from 59). SIGIL chain confirmed still mining
continuously immediately before and after this work.

### Phase F — BatchSetRoot sidecars — **SHIPPED 2026-08-15**, all in `sigil-narwhal-mempool`

- BatchSetRoot aggregation. ✅ new `batch_set.rs`: `BatchRefV1 { batch_id,
  certificate_hash, worker, tx_count }` + `BatchSetV1 { refs: Vec<BatchRefV1> }`
  + `batch_set_root()`, one Merkle root (reusing the same RFC6962-style
  `merkle::merkle_root` Phase A already proved) over domain-separated
  (`SIGIL/BATCHREF/V1`) per-ref leaf hashes. §12's actual claim — "a block can
  commit to thousands of batches without its header growing" — is proven at
  scale, not just by type signature: a test builds a 5000-ref set and confirms
  the root stays one deterministic `[u8;32]` value.
- Certificate identity for the ref. ✅ new `BatchCertificate::hash()` in
  `types.rs`, domain-tagged `SIGIL/CERTIFICATE/V1`, distinct from
  `BatchCertificate::digest` (which names the BATCH, not the certificate). A
  `BatchRefV1::certificate_hash` pins a block to the EXACT quorum of acks that
  was verified at production time — a later-arriving ack for the same batch
  changes the certificate's hash, so an old ref stays tied to the evidence
  that actually existed when the block was made, not to "some certificate,
  eventually." Test `certificate_hash_changes_when_ack_set_changes` confirms
  the hash moves when the ack set does and differs from `digest`.
- Schema version/height gate. ✅ new `body_mode.rs`: `BodyMode` enum
  (`InlineTransactions | AnchoredSidecar | CertifiedBatchRefs`) +
  `activation_mode(validator_count, da_certified) -> BodyMode`. Two
  independent conditions gate the real mode, matching this doc's own §3.2
  correction: `validator_count < 4` ALWAYS forces `InlineTransactions`
  regardless of what `da_certified` claims (test:
  `below_four_validators_is_always_inline_regardless_of_da_certified` — even
  a *claimed* true doesn't matter below the BFT floor); `validator_count >= 4`
  without real certification falls back to `AnchoredSidecar` (full bytes still
  travel with the block, just alongside a committed root) rather than jumping
  straight to reference-only. No env var or flag can skip the gate — the two
  booleans are its only inputs, and `validator_count` is a required parameter
  the caller must supply from real state (SIGIL has no formal
  validator-registry concept yet, so this module deliberately does not invent
  one — wiring a real source is separate follow-up).
- Full data-availability gate. ✅ same function — `da_certified` must be
  genuinely true (real per-batch certification having actually happened), not
  inferred from committee size alone.
- Digest/reference block body only after DA tests pass. ✅ **and not
  attempted this pass, by design.** `body_mode::SIGIL_CURRENT_VALIDATOR_COUNT
  = 1` is the honest, current, named answer for this chain — Delta/Gamma/Beta
  are all confirmed permanently gone (2026-08-14), so SIGIL is a genuine
  single-producer network today. A dedicated test,
  `sigil_today_is_inline_transactions_full_stop`, asserts
  `activation_mode(SIGIL_CURRENT_VALIDATOR_COUNT, false) == InlineTransactions`
  — i.e. this module's own conclusion about SIGIL, encoded as a test that
  would fail loudly the moment that stops being true without a deliberate,
  reviewed bump to the constant.

**What this phase explicitly did NOT do, stated plainly (same caution
pattern as Phase B's original deferred-wiring communication):** none of this
touches `sigil-node`'s actual `BlockHeader`/block body. `batch_set_root()`
and `BodyMode` are standalone, tested crate machinery — proven correct and
proven inert (gated to `InlineTransactions`) at SIGIL's real `n=1` — not a
live block-schema change. Wiring a `BatchSetRoot` field into the real,
mining, committed block header is a separate, consensus-critical,
independently-reviewable change that needs its own height-gate per this
doc's own mainnet-safety discipline (borrowed a level early from
`CLAUDE.md`'s "old blocks must always validate the same way" rule) — it
remains explicitly out of scope until real multi-validator infrastructure
exists (Phase D) and someone deliberately decides to do it.

77/77 crate tests green (up from 65: +6 `batch_set`, +5 `body_mode`, +1
`types::certificate_hash_changes_when_ack_set_changes`). SIGIL chain confirmed
still mining continuously immediately before (height 49,481) and after
(height 50,484) this work, unaffected since none of it touches the producer
loop or the deployed binary.

### Phase G — fairness / anti-MEV experiments — **SHIPPED 2026-08-15**, all in `sigil-narwhal-mempool`

- Record visibility metadata. ✅ new `order_meta.rs`: `BatchOrderMetaV1
  { creator, epoch, sequence, first_seen_round, tx_root }`, matching §15's
  field list exactly with one honest substitution — `creator` is `WorkerId`,
  not the spec's `ValidatorId`, because SIGIL's mempool layer has no
  validator-registry concept yet (same gap `body_mode::activation_mode`
  already names). `from_header()` derives everything except
  `first_seen_round` (necessarily local — when THIS node saw the batch)
  straight from the already-committed `BatchHeaderV1`. This module makes no
  ordering decision; it only records data.
- Test Tilikum/MRV-like ordering **separately** (§15's own word, followed
  literally). ✅ new `fair_order_experiment.rs` — deliberately scoped
  narrow, and its own doc comment says so twice over: this is **not** an
  implementation of Tilikum, MRV, Themis, or Aequitas. Those use
  causal-history cliques and cross-validator Byzantine agreement on arrival
  order; this module has neither. What it actually does: measures ONE
  specific, previously-just-asserted bias mechanism — same-round ties broken
  by validator/worker index — against ONE narrow mitigation — same-round
  ties broken by batch content (`tx_root`) instead. `first_seen_round`
  always dominates in both schemes (visibility ordering is preserved); only
  the within-round tie-break differs.
- **Measured, not just asserted** — `fair_order_bias_bench` (8 workers,
  2000 tied cohorts, one batch per worker per cohort, run on this box):

  | Scheme | Tie-break key | Worker win-rate spread | Verdict |
  |---|---|---|---|
  | Naive index tiebreak (the §15-warned-about baseline) | `creator.0` ascending | worker 0: **100.0%** (2000/2000), all others: 0.0% | total identity capture — exactly the bias the doc predicted, now measured rather than assumed |
  | Content tiebreak | `tx_root` ascending | 11.6%–13.7% across 8 workers (uniform = 12.5%) | no worker systematically wins; spread matches random content, not identity |

  Backed by tests, not just the printed bench: `naive_tiebreak_always_favors_the_lowest_creator_index`
  asserts worker 0 wins ALL 50 sampled cohorts (not "usually" — every time, by
  construction of the naive sort key); `content_tiebreak_winner_is_not_determined_by_creator_index`
  swaps which creator holds which `tx_root` between two runs and shows the WIN
  follows the content, not the identity attached to it; `content_tiebreak_distributes_wins_roughly_evenly_across_workers`
  runs 2000 cohorts and bounds every worker's win count to 100-450 (loose, to
  avoid flaking, but tight enough to catch a gross skew like the naive
  scheme's).
- **What this explicitly does NOT claim, stated as plainly as Phase E's CPU-cost
  finding was:** removing identity from a same-round tie-break is not a
  Byzantine-robust fairness guarantee. A real adversary controls their own
  batch's transaction ordering and could, in principle, grind for a favorable
  `tx_root` — this experiment does not measure or defend against that, and
  says so in its own doc comment and bench output. The actual published
  protocols this doc names (Tilikum, MRV, Themis, Aequitas) exist specifically
  to close that gap via richer, cross-validator constructions; evaluating
  those properly remains the real "separately" work §15 calls for, not
  something 84 unit tests in one crate can stand in for.
- **Not wired into anything.** Neither `order_meta` nor `fair_order_experiment`
  is called from `MempoolBackend`, `BatchSealer`, or anywhere in `sigil-node`
  — same standalone-and-inert pattern as every prior phase's out-of-scope
  pieces.

84/84 crate tests green (up from 77: +3 `order_meta`, +4 `fair_order_experiment`).
SIGIL chain confirmed still mining continuously immediately before (height
56,334) and after (height 56,726) this work, unaffected since none of it
touches the producer loop or the deployed binary.

---

## 18. Benchmark ladder

Never report one number as "TPS." The same discipline used by the literature correction applies here: **measure the actual layer first, then name the result precisely.**

### B0 — queue-only
No hashing, no signature verification, no networking.

Measures:
- enqueue/s;
- dequeue/s;
- contention;
- p50/p95/p99.

### B1 — verified ingestion
Real signatures and real transaction bytes.

Measures:
- valid tx/s;
- invalid tx/s;
- CPU/core;
- ns/tx;
- queue pressure.

### B2 — batch construction
Real canonical encoding + BLAKE3 + Merkle roots.

Measures:
- tx/s;
- MB/s;
- batch seal latency;
- allocations/tx.

### B3 — Reed-Solomon
Real `flux-aether` coder.

Measures:
- GB/s encode;
- GB/s decode;
- CPU cost;
- performance by `(k,p)`.

### B4 — loopback dissemination
Multiple local processes.

Measures:
- batches/s;
- certificate latency;
- bytes sent/committed tx.

### B5 — WAN 4-validator
Real hosts/regions.

Fault cases:
- one slow node;
- one offline node;
- packet loss;
- delayed ACK;
- corrupted shard;
- producer disappearance.

### B6 — committed chain throughput
Real state transitions and storage.

Report:
- committed tx/s;
- blocks/s;
- bytes/s;
- state update cost;
- finality latency;
- catch-up behavior.

---

## 19. The 10M TPS reality check

At 10,000,000 transactions/second:

| Mean encoded tx | One-copy payload bandwidth | Raw data/day |
|---:|---:|---:|
| 250 B | 20 Gbit/s | 216 TB/day |
| 500 B | 40 Gbit/s | 432 TB/day |

Those are before protocol overhead, signatures, erasure-code expansion, Merkle proofs, state writes, indexing, retransmission, replication, or archival redundancy.

Therefore:

- **10M queue ops/s** may be an attainable microbenchmark on suitable hardware.
- **10M signature-verified ingress tx/s** is a much harder claim and must be measured.
- **10M sustainably committed state transitions/s** is a separate systems problem involving network, execution, and storage at extraordinary scale.

The public claim should always name the layer.

---

## 20. "Wicked" extension: BraidPool Adaptive Availability Cells

The strongest direction is not to claim a new primitive. It is to make a **SIGIL-specific composition** whose ingredients are individually understood and whose integration can be measured rigorously.

The combination:

1. **epoch-salted transaction lanes**;
2. **batch availability certificates**;
3. **replication/RS hybrid dissemination selected by an epoch policy**;
4. **one GHOSTDAG braid instead of a parallel Narwhal header DAG**;
5. **BatchSetRoot aggregation**;
6. **separate availability and ordering state machines**;
7. **bounded anti-explosion retention**;
8. **fair-ordering metadata hooks**.

Call each certified batch an **Availability Cell**.

Conceptually:

```text
Transaction stream
      |
      v
+------------------+
| Availability Cell|
| tx_root          |
| shard_root       |
| coding profile   |
| quorum proof     |
+------------------+
      |
      +--- Cell
      +--- Cell
      +--- Cell
            |
       BatchSetRoot
            |
       GHOSTDAG braid
```

The braid orders **commitments to recoverable cells**, rather than carrying bulk transaction data itself.

### 20.1 Novelty calibration

| Element | Confidence | Correct claim |
|---|---|---|
| Reusing SIGIL/Flux's existing Reed-Solomon implementation | High | **New for this codebase as an engineering reuse**, not a research contribution |
| Erasure-coded shared-mempool batches + availability certificates | High | **Prior art**; Imitater published essentially this mechanism first |
| Decode → re-encode → commitment comparison | High | **Prior art / adopted**, also present in Imitater |
| Erasure-coded batch availability riding SIGIL's existing GHOSTDAG braid rather than a separate Narwhal header DAG | Medium | **Not found in the companion note's search**; do not say "does not exist" |
| "BraidPool Adaptive Availability Cells" as the complete composition above | Low-to-medium | A project-specific systems design; **no confirmed novelty claim without a systematic review** |

This table is intentionally stricter than the earlier wording. The companion investigation searched roughly a dozen queries and fully read six sources; it explicitly describes itself as a spot-check rather than a systematic review. Therefore **"not found in this search" is the publication ceiling** until a broader prior-art review is completed.

---

## 21. Tests that must exist before activation

### Quorum
- n=1 => no BFT mode
- n=2 => no BFT mode
- n=3 => no BFT mode
- n=4 => q=3
- n=5 => q=4
- n=6 => q=5
- n=7 => q=5
- duplicate signer rejected
- non-member signer rejected

### Hash/encoding
- same batch => same ID across platforms
- changed epoch => different ID
- changed worker => different ID
- changed coding profile => different ID
- changed tx order => different ID
- malformed length rejected

### Availability
- fewer than q ACKs rejected
- q valid ACKs accepted
- corrupt shard rejected
- wrong Merkle proof rejected
- k shards reconstruct
- k-1 shards do not reconstruct
- decode/re-encode root mismatch rejected

### Crash/recovery
- producer dies after certificate
- certificate signer dies
- one Byzantine node serves garbage
- WAL restart
- epoch transition
- old certificate replay

### Memory
- sustained invalid spam
- hot-wallet spam
- lane-targeting wallet generation
- orphan red-block batches
- repair-request amplification

---

## 22. Suggested crate layout

```text
crates/sigil-braidpool/
  Cargo.toml
  src/
    lib.rs
    config.rs
    worker.rs
    ingress.rs
    scheduler.rs
    batch.rs
    canonical.rs
    certificate.rs
    committee.rs
    dissemination/
      mod.rs
      replicated.rs
      reed_solomon.rs
      repair.rs
    store/
      mod.rs
      memory.rs
      wal.rs
    batch_set.rs
    metrics.rs
    errors.rs
  benches/
    ingest.rs
    batch.rs
    erasure.rs
    certificate.rs
  tests/
    quorum.rs
    golden_vectors.rs
    reconstruction.rs
    crash_recovery.rs
    adversarial.rs
```

---

## 23. Metrics

Prometheus-style:

```text
sigil_braidpool_ingest_total
sigil_braidpool_ingest_rejected_total
sigil_braidpool_verified_total
sigil_braidpool_queue_bytes
sigil_braidpool_worker_depth
sigil_braidpool_batches_sealed_total
sigil_braidpool_batch_bytes
sigil_braidpool_cert_latency_seconds
sigil_braidpool_cert_failures_total
sigil_braidpool_shards_sent_total
sigil_braidpool_shards_received_total
sigil_braidpool_reconstruct_total
sigil_braidpool_reconstruct_failures_total
sigil_braidpool_repair_requests_total
sigil_braidpool_gc_batches_total
sigil_braidpool_committed_txs_total
sigil_braidpool_committed_bytes_total
```

Every performance claim should be reproducible from exported counters.

---

## 24. Research basis and provenance

### 24.1 Authoritative companion literature audit

This revision is aligned to **`SIGIL_NARWHAL_ARXIV_INVESTIGATION_v0.pdf`**, whose explicit purpose is to correct the earlier "invented upgrade" wording on the public record.

Its core findings are the publication baseline for this specification:

1. **Narwhal/Tusk** separates transaction dissemination from ordering, with workers batching transactions and primaries forming a DAG of certificates.
2. **Bullshark** is part of the same DAG-BFT family used as conceptual background.
3. **Imitater** is direct prior art for erasure-coded shared-mempool dissemination and availability certificates.
4. **Aptos Quorum Store** supplies a serious production counter-argument to always using erasure coding.
5. Reusing SIGIL's already-built Reed-Solomon implementation is an engineering economy, **not** a research contribution.
6. Pairing coded batch dispersal with SIGIL's already-running GHOSTDAG-style braid was **not found in that search**, at medium confidence only.
7. Reconstruct/re-encode/compare is **adopted prior art**, not an invention.

The companion note's cited references:

- George Danezis, Lefteris Kokoris-Kogias, Alberto Sonnino, Alexander Spiegelman — **Narwhal and Tusk: A DAG-based Mempool and Efficient BFT Consensus**, arXiv:2105.11827 (2021).
- Alexander Spiegelman, Neil Giridharan, Alberto Sonnino, Lefteris Kokoris-Kogias — **Bullshark: DAG BFT Protocols Made Practical**, arXiv:2201.05677 (2022).
- Alexander Spiegelman, Neil Giridharan, Alberto Sonnino, Lefteris Kokoris-Kogias — **Bullshark: The Partially Synchronous Version**, arXiv:2209.05633 (2022).
- Qin Wang, Jiangshan Yu, Shiping Chen, Yang Xiang — **SoK: Diving into DAG-based Blockchain Systems**, arXiv:2012.06128 (2020).
- Qingming Zeng, Mo Li, Ximing Fu, Chuanyi Liu, et al. — **Imitater: An Efficient Shared Mempool Protocol with Application to Byzantine Fault Tolerance**, arXiv:2409.19286 (2024).
- Ulysse Denis — **On the Design of Ethereum's Data Availability Sampling**, arXiv:2407.18085 (2024).
- Yonatan Sompolinsky, Shai Wyborski, Aviv Zohar — **PHANTOM and GHOSTDAG: A Scalable Generalization of Nakamoto Consensus**, IACR ePrint 2018/104.

### 24.2 Additional exploratory material from the broader BraidPool review

The earlier BraidPool review also considered later DAG/fair-ordering/system work such as Mysticeti, Shoal/Shoal++, Lifefin, Beluga, Tilikum, Multi-Round Visibility, Simple-IT, Multimmit, and transaction-ordering-bias work. These are **outside the scope of the companion PDF** and should not be represented as findings of that note. They remain useful leads for a separate systematic follow-up, especially around fairness, synchronization attacks, latency, and bounded DAG resource use.

### 24.3 Process rule

The literature note's most durable lesson becomes an explicit BraidPool release rule:

```text
1. Measure the live path before publishing a performance result.
2. Search prior art before publishing a novelty claim.
3. If the evidence is a spot-check, say "not found in this search."
4. If a claim is wrong, correct it in place and preserve the correction trail.
5. Separate "new for SIGIL" from "new to the field."
```

---

## 25. Recommended public wording

> **SIGIL BraidPool** is an experimental braid-native shared-mempool design that separates high-bandwidth transaction dissemination from GHOSTDAG ordering. It shards ingestion across workers, certifies batch availability, can use replicated or Reed-Solomon-coded dissemination, and aggregates certified batches under compact block commitments. The design builds on Narwhal-style shared mempools and subsequent availability work. **Erasure-coded batch dissemination and reconstruct/re-encode integrity checking are established prior art and are not claimed as SIGIL inventions.** What is specific to SIGIL is the engineering composition: reusing an in-tree Reed-Solomon implementation and attaching availability-certified batches directly to the already-running GHOSTDAG-style braid instead of constructing a second Narwhal header DAG. That exact pairing was **not found in a same-day, non-exhaustive literature search**, so it is presented as an engineering design rather than a confirmed research novelty. Performance is reported separately for ingestion, dissemination, execution, and chain-committed throughput.

### Short version

> **BraidPool: Narwhal-style availability, SIGIL's own braid.** Known data-availability techniques, assembled around the GHOSTDAG machinery SIGIL already runs. No fake novelty claims, and no "TPS" number without naming which layer was actually measured.

That wording preserves the ambition while matching the literature audit's confidence level and correction record.
