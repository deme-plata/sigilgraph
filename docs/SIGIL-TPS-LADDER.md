# SIGIL Throughput Ladder — from ~0 real TPS to 1.9M TPS

**Status:** design, adversarially reviewed (18 agents across 2 workflows, 2026-07-04). Architecture = **PRISM v1** (unanimous 3/3 judge winner). Every number ≥108k is **measured on epsilon (48-core)**, not projected. Three skeptic-surfaced fixes are folded into the ladder below (marked ⚠︎FIX).

---

## 1. Measured foundations (real pipeline, this session)

| Path | TPS | Bench |
|---|---:|---|
| Per-tx signed (1 sig/tx) | 15,954 | `sigil-throughput` / `sigil-batchsweep` @1 op/sig |
| **AuthorizedBatch @64 ops/sig** | **102,630** | `sigil-batchsweep` — peak; sigs amortize to *free* at ≥64; degrades past 512 |
| Sequential single-shard (full apply+commit+4 roots) | 108,696 | `sigil-scaling` archive-shard @N=1 |
| **Sharded apply @N=16 / @N=64** | **1,006,289 / 1,921,922** | `sigil-scaling` archive-shard, core-saturated |
| Replica (N validators, same chain) | 84k→2.9k (anti-scales ~1/N) | `sigil-scaling` — validators ≠ throughput |
| zk-flux verify-once | verifier 0.9ms O(1) **but produce ceiling ~59k** | `sigil-zkflux-vs-current` — **rejected for now** |

**The wall at 100k is `apply_tx` (~4µs/op), not signatures and not block rate.** State commits themselves run at 13M sound / 209M unsafe/s (`zkflux.rs:5`). Block rate is irrelevant to TPS (92 blk/s happened naturally in the 100k runs; the deployed adaptive governor floor-8/ceiling-60 is orthogonal). **Sharding the apply is the only lever that multiplies past 100k, and it's measured: 17.3× on one box.**

## 2. Architecture — PRISM (Phase-split Lane-Sharded Apply)

One producer, one block stream, N execution lanes **inside** the node. `SIGIL_LANES` is an *execution* parameter, **not consensus** — N=1 serial replay must produce byte-identical roots to N=64 (CI-gated). This is what lets a Raspberry Pi verify the same chain a 64-lane epsilon produces.

Three structural pillars (all verified against real code):

1. **Order-free roots by construction.** `wallet/dex/contract` roots are 256-bit *additive* multiset accumulators (`acc_add`, `sigil-state/src/lib.rs:851`, verified wrapping-commutative-associative). Per-slot sequences telescope → the root is a pure function of the *final* key→value set, so thread schedule / fold order / inbox drain order are provably irrelevant. `event_log_root` is the one ordered root → pinned to canonical body-index order.
2. **Single-writer Phase A.** `AuthorizedBatch::verify()` enforces `fee_payer == author` for every op (`sigil-tx/src/lib.rs:630`), so **every debit is home-shard-local**. `shard(w) = w[0] & (N-1)` (WalletId = BLAKE3(pubkey), uniform). An author's slots are written only by that author's lane, in body order.
3. **Commutative Phase B.** The *only* cross-shard effect is credits. **Uniform rule (consensus, N-agnostic):** all recipient credits — same- and cross-shard alike — are emitted as `StateMutation::Credit{wallet,token,delta}` and applied at end-of-block via parallel per-shard inbox drains. Commutative additive deltas → order-free.

Cross-shard `Send A→B`: debit A in Phase A on A's lane (precheck vs parent state + A's own earlier body ops); emit `Credit{B, delta}` to B's shard inbox; drain in Phase B. No cross-shard *reads* during apply. Determinism is structural, CI-gated by a 10⁷-op differential fuzz (serial N=1 vs parallel) at **transition/event BYTE granularity** + N∈{1,4,16,64} replay equality.

## 3. The ladder (each rung independently shippable + measurable)

### R0 — Consensus replay nonce ⚠︎FIX (soundness prerequisite — do first)
**Both workflows' #1 finding.** `AuthorizedBatch` carries no nonce/expiry; `batch_root = BLAKE3(ops)` is pure; `SigilState` tracks no per-account nonce (`sigil-tx/src/lib.rs:18-20` admits it). Today's *only* replay defense is the mempool's in-memory `seen` HashSet — non-persistent, cleared on every restart. **A public batch can be rebroadcast by anyone after the producer's ~10-min restart and re-executes: author debited again, recipient credited again.** This is a live-chain soundness gap *independent of TPS*.
**Fix:** add a per-author monotonic `nonce` (or nonce-window/expiry-height) to `AuthorizedBatch`, **fold it into `batch_root`** so the signature binds it; track a per-author high-water nonce in state. It's home-shard-local under `shard(·)` → checked in Phase-A precheck on the author's own serial lane at **zero cross-shard cost**.
**Verify:** replayed batch bytes rejected at apply (not just mempool); nonce gap/reorder rejected; CI test.

### R1 — Real user ingest bridge (greenfield)
Mapping proved `sigil-rpcd` (:8099) and `sigil-node` (:9501) are **two disconnected ledgers** — no user-tx path exists; `/send` mutates rpcd-local state and never forwards; `/sigil/g0/txs` is subscribed but dead; the node's `:8181` `api_port` is printed but never bound. So we *build* the first bridge (design batch shape in from day 1). Node `:8181` HTTP thread (std OS thread, mirroring the TXGEN task) → mempool batch lane. rpcd `/api/v1/batch` verifies once, forwards verbatim over loopback.
**Verify:** a user-signed batch via rpcd lands in a block; recipient balance reflects it.

### R2 — Batch-carrying blocks (~100k TPS)
Block body carries `Vec<AuthorizedBatch>` **intact** (version-gated header, body_root binding). Verifiers re-execute through the single `ChainTip::apply` chokepoint (`chain.rs:106`) → **1 sig-check per batch reaches every verifier** (the "does-it-deliver" skeptic confirmed no path re-verifies per-op). Batches ride the 64–512 ops/sig sweet spot. Old nodes precheck-reject v1 → deploy binaries (incl. rebuilt `sigil-top`) to all nodes+monitors *before* the first batched block.
**Verify:** serial replay of a batch corpus reproduces identical 4 roots on a second node; `check_roots_match` silent.

### R3 — Credit{delta} semantics flip + author-pinning ⚠︎FIX (the fork event; zero perf change)
`StateMutation::Credit{wallet,token,delta}` added; the `Send` arm's recipient emission flips from **absolute `SetBalance`** (`sigil-tx/src/lib.rs:~906`) to `Credit` — the minimal fix that makes parallel apply sound (today's absolute write is last-writer-wins under parallelism = money destruction). All credits deferred to Phase B; `checked_add`→halt on overflow (never silent).
**⚠︎FIX (skeptic 2):** **author-pinning rule** — if an author has ≥1 non-`Send` op (Swap/VM/etc.) *anywhere* in the block, **all** that author's batches execute on the serial global lane in body order (the Swap arm writes the author's slot with absolute `SetBalance`, so mixing a Send-lane batch + Swap-global batch for the same author = concurrent slot write = divergence). Consensus-defined, N-agnostic, one pre-pass over the body. Also flip the master-wallet swap skim + `MintReward`/coinbase to `Credit` and pin coinbase to a canonical body slot.
**Verify:** full historical replay pre-gate byte-identical; post-gate corpus replays equal on two nodes; activation-height gate (or g0 reset).

### R4 — ShardedState + parallel engine, `SIGIL_LANES` default 1 (off-prod; ~1.5M in the wind tunnel)
`SigilState → N× ShardState{wallets partition, wallet_acc[4], native_supply, author_nonce_hwm}`. Phase A: N lanes apply debits+prechecks in body order → barrier → Phase B: parallel inbox credit drains → fold = `acc_add` over N partials. ~500–600 LOC (largest change). `SIGIL_LANES=1` is bit-identical to today.
**Verify:** chronos CI gate — 10⁷ random ops through serial + parallel assert equal roots AND equal transition/event *bytes*; 10k-block corpus equal at N∈{1,4,16,64}; wind tunnel ≥1.5M.

### R5 — Snapshot/restart safety (hard prod prerequisite)
Restart today = ~10 min / 61 GB log replay on the sole producer; at rate it becomes *hours* → snapshot-boot mandatory. Dual-format snapshot (wire stays flattened — byte-stable for rpcd's bincode consumers; shadow-write new format one release early); boot-time per-shard acc-drift audit.
**Verify:** kill+restart a shadow N=64 node mid-load → boots from snapshot, audit clean, roots match a never-restarted replica; old snapshots still load.

### R6 — Persist slimming FIRST ⚠︎FIX, then burst → sustained
**⚠︎FIX (skeptic 3): resequenced.** Original R6 gated *sustained 1M TPS with JSON dual-carry ON* — but persist is synchronous `serde_json` on RAID0 HDD (~0.7–1 GB/s); JSON dual-carry = 1.7–3.5 GB/s = **2–4× over the device ceiling**. So:
- Move persist rework up: **bincode block bodies + buffered/async `chain_log` append off the producer thread**; active segment on the 1.8 TB NVMe, sealed segments migrated to md0 (RAID0 HDD).
- **Bound the mempool `seen` set** — it has *no eviction* today → ~43 GB/day at 1M TPS → OOM in ~1 day on the 62 GB box. Add a persistent-nonce-backed bound (R0's nonce makes the in-mem set unnecessary for correctness).
- Split the gate: **R6a burst demo** (minutes, page-cache-survivable) → **R6b sustained 1h** gated on the persist rework. Dual-carry at sustained rate must be bincode, byte-cost re-verified against a measured `fio` number on md0 before the flip.
**Verify:** R6a burst ≥1M; R6b sustained ≥1.0M/1h with bounded persist-queue + disk rate ≤ device ceiling, zero follower divergence.

### R7 — Full slimming to target (1.5–1.9M sustained)
Drop dual-carry (compact batch-only blocks, ~140–200 B/op vs ~1.7–2.3 KB/op JSON); parallelize `event_log` Merkle by subtree (`hash_event_log:977` is single-threaded O(n) — eats a core at rate); segment pruning + snapshot cadence + **multi-node serving** (else epsilon is the sole-server supply-starvation wall — cf. the sync fix this session). Storage reality: **12–30 TB/day** at full rate → flux-db (proven at TB scale) + pruning are mandatory.
**Verify:** ≥1.5M/1h, bounded persist, a cold follower syncs the high-rate segment (via snapshot+segments) *faster* than production.

## 4. Honest ceilings & risks

- **Committed numbers:** ~100k (batches), ~1.0–1.5M sustained, **1.9M peak** (@N=64, one 48-core box). Per-*author* ceiling = one lane = **108,696 ops/s** (a single exchange-like hot author serializes).
- **Storage:** 12–30 TB/day at full rate. Non-negotiable: R7 pruning + snapshot cadence + multi-node serving.
- **Hot-shard skew:** uniform over authors, not over *load* → work-stealing + per-author op caps + occupancy metrics.
- **Global-lane Amdahl:** DEX/VM serialize at ~100–250k ops/s; if non-Send traffic > ~5–15% it's the wall. v1 pins non-Send authors to the global lane (R3 fix) rather than mis-order.
- **Fork discipline:** R3 is a height-gated semantics fork — replaying across the gate with the wrong engine *halts* at `check_roots_match` (safe failure, but an ops footgun). Deploy-order discipline mandatory.
- **zk-flux:** revisit only if proving gets ~10× faster or verifier-count dominates.

## 5. Recommended sequencing

**Ship R0 first** — it's a soundness fix the chain needs regardless of TPS (a replayable-transfer bug exists *today*), and it's self-contained (~1 file, testable locally). Then R1→R2 give the first real user TPS (~100k) with the batch shape correct from day one. R3→R4 unlock sharding behind `SIGIL_LANES` entirely off-prod (bit-identical at N=1). R5→R7 are the prod-hardening path to sustained 1.9M. **Nothing touches the live producer until R6, and every rung is measurable in the chronos wind tunnel first.**

_Source: `sigil-batchsweep` / `sigil-scaling` / `sigil-zkflux-vs-current` benches (this session, working tree); PRISM design + 3-skeptic review (workflow wf_987da84a); ingest map + skeptic (workflow wqgpuq7uk). All changes land in the codewhale working tree — GPG-signed commit is the operator's._
