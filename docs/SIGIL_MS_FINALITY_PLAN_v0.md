# SIGIL — the path to millisecond confirmation (measured diagnosis + staged plan)

**Date:** 2026-09-19 · **Author:** Rocky · **Status:** diagnosis MEASURED live; plan staged, nothing flipped on the live producer.

RULE 0 (sigil skill): measure the live path first. Everything in §1 is measured on Epsilon's
`sigil-node` (`:18181`) on 2026-09-19; the Quillon numbers are read-only from
`q-narwhalknight-src` (chain firewall: Quillon is READ-ONLY in a SIGIL session).

## 1. What is actually true right now (measured)

| thing | measured value | source |
|---|---|---|
| Block rate | **8.0 blk/s** (the `SIGIL_RATE_MIN` idle floor) | `🏭 produced … 8.0/s` journal line |
| Finality latency (healthy) | **mean 327 ms**, range 247–372 ms | `🔗 finality: … latency last=Xms mean=327` |
| Finality health, last hour | **1,652 certificate settlements, 0 quorum failures** | journal grep |
| Depth-rule fallback distance | **+507–509 blocks** behind the certificate | `would-be-final vs depth-rule (+507 blocks)` |
| Throughput | **0 TPS** (idle), 6,836 txs ever | `6836 txs (0 TPS verify-once)` |
| Shielded pool | **30,584 / 32,768 notes**, 2,537 nullifiers | `/v1/shielded/anchor` |
| Instant-confirm endpoints | `/v1/transactions/:hash/wait` → 200, `/v1/events` (SSE) → 200 | curl |

## 2. The diagnosis — "minutes" is a TAIL, not the median

A SIGIL transaction, on a healthy chain, confirms in **~0.5 s** (up to 125 ms to be minted at
8 blk/s + ~330 ms certificate finality). That is not minutes. The "one transaction takes
minutes" experience is one (or a stack) of these, none of which is the consensus median:

1. **Bimodal finality with a block-COUNT fallback.** The braid's own finality is `final_depth =
   512` blocks (`sigil-dagknight/src/lib.rs:49`). At 8 blk/s that is 64 s; at the **0.38 blk/s**
   this chain has historically dropped to (`project_sigil_instant_confirm_delivery_2026_09_12`)
   it is **~22 minutes**. The `sigil-finality` certificate hides this — but only while the
   **2-of-2** committee has quorum. The moment it loses quorum (the well-documented n=2
   fragility: one-way gossip froze the certificate for 6 h on 2026-09-15) settlement falls back
   to the 512-depth rule and the tail becomes minutes.
2. **Shielded overhead on every payment.** Transparent sends are retired
   (`SHIELDED_ONLY_HEIGHT == 0`), so every user tx is shielded: client STARK proof (~347 ms) +
   pool scan (~1.6 s near capacity — the pool is 93 % full) + settle. ~2.5 s before any tail.
3. **Wallet polling, not pushing.** `/v1/transactions/:hash/wait` (long-poll) and `/v1/events`
   (SSE) are LIVE, but sigil-top and the Android/browser wallets historically still POLL, so the
   user's "sent" state waits for a full poll cycle instead of the push.

## 3. Why Quillon Graph is blazingly fast (read from its own source)

`q-dag-knight` (10,873 LOC) header, verbatim: *"Bullshark consensus with DAG-Knight ordering /
Zero-message complexity asynchronous BFT with quantum anchor election."* Over `q-narwhal-core`
(10,573 LOC): *"DAG-based mempool."* The two together are the canonical Narwhal + Bullshark win:

- **Narwhal** disseminates tx batches as a DAG with availability certificates — data is never the
  bottleneck; ordering references batches by hash, not by shipping bodies.
- **Bullshark / DAG-Knight** derives the commit from the DAG STRUCTURE — an *anchor* (leader
  vertex) commits once it has enough DAG support, roughly every 2 rounds, with **zero extra
  consensus messages**. Finality is intrinsic; there is no separate vote round and no
  block-count depth rule to degrade to.

SIGIL has the ordering half (the `sigil-dagknight` Braid is GHOSTDAG-style, Quillon-class) but:
its **finality is a block-COUNT depth rule** (`final_depth = 512`) with a **separate, fragile
Ed25519 certificate gadget** (`sigil-finality`, 1,558 LOC) bolted on to make it fast; and its
**mempool is 271 LOC** (`sigil-narwhal-mempool`, a verify-once dedup pool) versus Quillon's
10,573-LOC real Narwhal.

## 4. The levers, in priority order (RULE-0 grounded)

**Do NOT lead with Narwhal.** The chain is at 0 TPS. Porting a 10.5k-LOC Narwhal mempool for
throughput nobody is using is the exact "optimize the idle layer" trap RULE 0 was written
against (the 5-day sync stall). Throughput/BraidPool is the right *long-term* roadmap
(`SIGIL_BRAIDPOOL_v1_1.md` already specs it) — for when there is real load, not first.

The pain is the LATENCY TAIL. Fix the tail:

1. **[UX, safe, do first] Push, don't poll.** Make sigil-top + the wallets show "accepted" on the
   instant txid receipt and flip to "final" off `/v1/transactions/:hash/wait` / `/v1/events` —
   both already return 200. This alone turns the *perceived* latency to milliseconds with **zero
   consensus change**. Touches wallet code only.
2. **[consensus, staged] Replace the 512-block depth rule with a Bullshark ANCHOR-COMMIT rule over
   the existing Braid** — the actual "best from Quillon." Finality becomes intrinsic to the DAG
   (commit in ~2 braid rounds), independent of block rate, with no separate vote round to lose
   quorum on. This kills BOTH the minutes-tail (no block-count fallback) AND the n=2 certificate
   fragility. Port target: `q-dag-knight`'s anchor-election + commit rule → a commit rule on
   `sigil-dagknight`'s `Braid`. **Irreversible on the producer** (a producer cannot be rolled
   back out of its own blocks — CLAUDE.md incident 2026-09-10): stage on a throwaway pair,
   height-gate it, roll out follower-first (node3 → happysrv → Epsilon), keep the 512-depth rule
   as the observe-only fallback until a clean checkpoint on all three, and the rollback plan is a
   HEIGHT + snapshot, never "reinstall the old binary." Add a chronos test that the anchor-commit
   order is prefix-identical across nodes before any live flip.
3. **[stability] Floor the wall-clock, not just the block count.** While (2) is staged, the tail
   is bounded by block rate. The 100 blk/s work (`project_sigil_100bps_produce_loop`) proved
   110 blk/s is reachable; at 110 blk/s even the 512-depth fallback is ~4.6 s, not 64 s. A modest
   raise of the idle floor (with the empty-block-storage cost measured in chronos first) shrinks
   the degraded-mode tail as a stopgap.
4. **[throughput, later] BraidPool / Narwhal maturity** — the 271→real-Narwhal gap, per
   `SIGIL_BRAIDPOOL_v1_1.md`. Only once (1)–(3) land and there is real load to measure against.

## 5. Are the CLAUDE.md + skills effective enough for this? (the operator's question)

Yes, with one gap, now closed by this doc:
- **RULE 0 in the sigil skill worked** — it stopped this session from porting Narwhal into an idle
  chain and forced the live measurement that found the real (tail) problem.
- The skills correctly point to `sigil-dagknight` / `sigil-finality` / `SIGIL_BRAIDPOOL_v1_1.md`.
- **What was missing:** a statement that SIGIL finality is *bimodal* (330 ms vs minutes), that the
  minutes come from the **512-block depth fallback × low block rate**, and that the "best from
  Quillon" for LATENCY is the **Bullshark anchor-commit rule**, not the Narwhal mempool (which is
  throughput, and the chain is idle). That is this document + the companion memory.

## 5b. LANDED 2026-09-19 — lever 2, first slice: the intrinsic quorum commit

`sigil-dagknight` now has the primitive Bullshark-style finality needs, and it is **live-safe by
construction**:

- `BraidConfig.final_quorum: Option<usize>` (env `SIGIL_DAG_FINAL_QUORUM`, default `None` = off).
- `Braid::quorum_final(quorum)` — walks the selected spine from the tip and returns the highest
  block that already has **≥ quorum DISTINCT producers building above it**. Finality derived from
  the DAG structure, with **no finality-vote round** — the gossip round that froze the certificate
  for 6 h on 2026-09-15 cannot stall this one.
- Folded into `computed_final()` as a THIRD `max` term beside the depth rule and the certificate,
  so it can only ever commit SOONER, never lower the line.

**Why this is safe to land on a live chain:** SIGIL has exactly ONE producer today, and one
producer can never form a quorum > 1 — so `quorum_final` returns `None` and `computed_final` is
byte-identical. A test pins that (`quorum_final_is_a_noop_on_a_single_producer_braid` asserts an
armed braid equals an unarmed one). The rule only starts committing once followers co-produce
blocks, which is the staged multi-producer work in §4.2.

**The honest limitation this exposed:** Bullshark's zero-message finality *requires* multiple
validators to co-build the DAG. SIGIL is single-producer with finality-voting followers — which is
exactly WHY it needs the gossip vote at all. So the real unlock is not this rule alone; it is
making happysrv/node3 produce into the braid. This slice is the finality half of that, landed and
tested ahead of it.

Tests (78/78 green, `fluxc test -p sigil-dagknight --profile release-fast`):
`quorum_final_is_a_noop_on_a_single_producer_braid` ·
`two_distinct_producers_finalize_intrinsically_without_a_certificate` (finalizes tip−2 with no
certificate at all) · `quorum_final_never_lowers_the_depth_line`.

## 6. Definition of done (so a future session knows when this is real)

Milliseconds is met when, MEASURED on the live end-to-end (not a bench): the wallet shows
"accepted" in < 50 ms (push receipt), and irreversible finality is < 100 ms **and does not
degrade to minutes when the committee hiccups** — i.e. the anchor-commit rule, deployed
follower-first and height-gated, has replaced the 512-depth fallback as the user-facing
settlement line, verified by a chronos prefix-identity test across all three nodes.
