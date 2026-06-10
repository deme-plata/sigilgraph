# SIGIL Release Plan — v0.36.1 "Snapshot Boot"

> **Dansk TL;DR (til Viktor):** 0.36.1's hovedfeature er **snapshot-boot af produceren**: i dag
> replayer en node hele sin 52 GB chain.log ved hver genstart (~35 min, compute-bundet — ikke
> disk). Efter 0.36.1: indlæs et periodisk state-snapshot + replay kun halen → **sekunder**.
> Tre Claude-agenter implementerer parallelt (A: snapshot-kernen, B: range-index i chain-loggen,
> C: WAL-auto-flush i flux-db) på kollisionsfrie filer med en aftalt API-kontrakt imellem.
> DeepSeek har red-teamet planen (14 fejlmodes — alle adresseret nedenfor, afsnit 7).
> Udgivelses-gaten er én falsificerbar test: **snapshot+hale-replay skal give PRÆCIS samme
> tip-hash og 4 state-roots som fuld replay.** Rollout: Epsilon sidst (producer), feature-flag
> default-on først efter hele flåden kører 0.36.1-binæren. Rollback = slet snapshot-filen →
> noden falder automatisk tilbage til fuld replay (dagens adfærd, urørt).

---

## 1. Goal & non-goals

| | |
|---|---|
| **Headline** | Producer boot: 35 min full `chain.log` replay → **load snapshot + tail replay = seconds** |
| **Supporting** | B: `ChainLog::replay_from(height)` via sparse height→offset index · C: flux-db WAL auto-flush (`max_wal_bytes`, default 64 MB) |
| **Non-goals (explicitly out)** | in-memory write window for the monitor · BDP tuning of CHUNK/MAX_INFLIGHT · incremental/delta snapshots · snapshot compression · monitor 1M blk/s full-sync |

Scope discipline is deliberate: every non-goal is a real lane, parked — not forgotten
(§10). 0.36.1 ships ONE behavioral change to the producer plus two enablers.

## 2. Why now — the measured baseline (2026-06-10)

```
Producer boot (Epsilon, 52 GB chain.log, ~21.5M blocks):
  ChainLog::replay(0..tip)  ≈ 35 min      ← COMPUTE-bound: serde_json deserialize
                                              + chain.apply() per block. Disk I/O
                                              is ~1-2 s of it (NVMe sequential).
Monitor store open (2.7 GB):
  127 s → 78 s → 20.7 s                    ← fixed in 0.35 ('B' meta + flux-db
                                              lazy-SST + reader cache + WAL
                                              truncation-after-flush)
Wire: zstd codec=1 deployed fleet-wide     ← 14.0× measured, E2E live
```

The 35-minute producer boot is now the single largest operational pain: every deploy,
crash, or host reboot silences the producer for half an hour (verified live during the
zstd-wire deploy). `snapshot.rs` already exists (Reed-Solomon shard save/load) but is
**never called at boot**, and `ChainLog::replay` has no range parameter — confirmed by
code exploration (Explore agent, 2026-06-10).

## 3. Architecture

### Boot — before / after

```
BEFORE (every restart):                         AFTER (0.36.1):
┌─────────────────────────────┐                 ┌─────────────────────────────┐
│ ChainLog::open (offset scan)│                 │ load state-snapshot.bin     │
│ replay(0 .. 21.5M)          │                 │   verify BLAKE3 + version   │
│   deserialize + apply ALL   │  ≈ 35 min       │   restore Chain @ H_snap    │  ≈ s
│                             │                 │ replay_from(H_snap+1 .. tip)│
│                             │                 │   (sparse-index seek + tail)│
└─────────────────────────────┘                 └──────────┬──────────────────┘
                                                            │ snapshot missing/corrupt/
                                                            │ stale/version-mismatch
                                                            ▼
                                                 FULL REPLAY (unchanged path)
```

### The periodic capture (producer loop)

```
every SIGIL_SNAPSHOT_EVERY blocks (default 100_000 ≈ 7-11 min at 156-250 blk/s):
  snapshot = { version: u8, height: H, window blocks (window_base..H, height-
               ascending), accumulated state (wallet/dex/event/contract + 4 roots),
               DAG root set (see risk #1), blake3 checksum }
  write → state-snapshot.tmp → fsync → rename → state-snapshot.bin   (atomic)
  OFF the producer hot path (background write / double-buffered state copy, risk #10)
```

## 4. Work breakdown — 3 parallel agents (running now)

| Agent | Files (exclusive) | Deliverable | Status at plan time |
|---|---|---|---|
| **A** | `snapshot.rs`, `main.rs`, (`chain.rs` accessor) | StateSnapshot + atomic write + boot sequence + `snapshot-create`/`snapshot-info` CLI + **the equivalence test** | 🟡 analyzing chain.rs state model |
| **B** | `chain_log.rs` only | `replay_from(dir, from_height, f) -> Result<u64,String>` + sparse height→offset index (`chain.idx`, ~16 B/entry, every 4096 blocks) + missing-index fallback | 🟢 implementation in, building |
| **C** | `flux-db/src/lib.rs` only | `max_wal_bytes` (default 64 MB) auto-flush, lock-safe (trigger AFTER the put guard drops), failure never fails the put | 🟢 implementation in |

**The A↔B contract** (defined verbatim in both prompts so parallel work cannot diverge):

```rust
pub fn replay_from(dir: &std::path::Path, from_height: u64,
                   f: impl FnMut(Block)) -> Result<u64, String>;
// semantics: blocks with height >= from_height, in order; returns count;
// self-heals on missing/corrupt index (scan-filter fallback) — never errors on it.
```

Integration (parent, after all three report): combined build → full test suites
(sigil-node + flux-db 85+ + sigil-top 8) → equivalence gate → review diff → commit.
Agents do **not** commit.

## 5. Release gates — all falsifiable, no gate = no ship

| # | Gate | How measured |
|---|---|---|
| G1 | **State equivalence**: fresh Chain from snapshot@400 + `replay_from(401..)` ≡ full-replay Chain — tip hash AND all 4 state roots byte-equal | Agent A's integration test (the release gate) |
| G2 | Boot on the real 21.5M chain < 30 s (target: seconds) | timed `systemctl restart sigil-node` on Epsilon, journal `⚡ snapshot boot` line |
| G3 | `replay_from` seek locates start < 10 ms on a 10k-block log; `replay_from(9_900)` yields exactly 100 | Agent B unit tests (timed) |
| G4 | Index-less fallback: delete `chain.idx` → `replay_from` still correct | Agent B test (c) |
| G5 | WAL auto-flush: threshold 4 KB test → WAL truncated + SST exists + all keys readable; 85/85 existing flux-db tests stay green | Agent C tests |
| G6 | Full replay path UNCHANGED byte-for-byte when no snapshot exists | code review + existing boot test |
| G7 | Producer pause from snapshot write < 1 tick budget (no missed-block window) | journal block-rate around `📸` lines during soak |

## 6. Test matrix

| Test | Layer | Owner |
|---|---|---|
| snapshot → kill → restore → equivalence (G1) | integration | A |
| corrupt checksum → fallback + clear log line | unit | A |
| snapshot height > chain.log tail → refuse + fallback (risk #3) | unit | A |
| version byte mismatch → refuse + fallback (risk #4) | unit | A |
| replay_from exact-count / order / fallback / timing | unit | B |
| append hot path unchanged with index writes (timing) | unit | B |
| auto-flush threshold + no-spurious-flush + 85 legacy | unit | C |
| 24 h Epsilon soak: snapshots every 100k, then one timed restart | operational | parent |

## 7. Risk register — DeepSeek red-team (14), with parent verdicts

Consulted via DeepSeek API (deepseek-chat) 2026-06-10; each item audited by the parent
before adoption. Severity: 🔴 must-fix in 0.36.1 · 🟡 mitigate/document · ⚪ noted, low.

| # | Risk | Verdict & mitigation |
|---|---|---|
| 1 | 🔴 **merge_parents below snapshot height** → incomplete DAG tip set after boot | Snapshot includes a **DAG root set** (all sub-window blocks still referenced as merge_parents by window blocks). G1 catches violations by construction. |
| 2 | 🔴 Crash during snapshot write | Already designed: tmp + fsync + atomic rename; checksum verified BEFORE load; corrupt → delete + full replay. |
| 3 | 🔴 Snapshot newer than chain.log tail (operator truncated log) | Boot compares snapshot height vs log tail; if snap > tail → refuse, loud log line, full replay. Test in matrix. |
| 4 | 🔴 Format evolution / downgrade | Strict version byte; mismatch → refuse + fallback. Documented: deleting `state-snapshot.bin` is always safe. |
| 5 | 🔴 Mixed-version fleet during rollout | Feature flag `SIGIL_SNAPSHOT_BOOT` (default **off** in the binary rollout, flipped per-host after fleet is on 0.36.1). Snapshots are LOCAL per node — never exchanged — so the only cross-version risk is operational, handled by rollout order (§8). |
| 6 | 🟡 rr-backfill serving during tail-replay | Nuance vs DeepSeek: disk range-serves (`chain_log.get_range`) stay correct during replay (log is intact on disk); only RAM-window serves are incomplete. Mitigation: don't start the P2P listener until tail-replay completes (current boot already orders it this way — verify in review). |
| 7 | 🟡 Disk-full during snapshot write | ENOSPC → fsync/rename fails → tmp deleted, loud log, producer continues, old snapshot stays valid. Pre-write space check optional; the atomic rename already guarantees no torn `state-snapshot.bin`. |
| 8 | 🟡 Auto-flush latency spike on producer hot path | C's design: trigger after the put guard drops; flush is synchronous but bounded by memtable size (≤64 MB). G7 measures it. If soak shows spikes: lower threshold or move to background thread (follow-up, not blocker — producer writes go through chain.log, not flux-db, on the hot path). |
| 9 | 🟡 Monitor shows height=0 during boot | sigil-top reads the node via tip-feed/backfill, not internal state; the tip oracle keeps the last height. Add `boot_status` to the node's status JSON (nice-to-have; not a 0.36.1 blocker). Documented for operators. |
| 10 | 🔴 Snapshot write pausing the producer (~every 7 min) | Background write of a double-buffered state copy, or accept a bounded pause if measured < 1 tick (G7 decides with data, not opinion). |
| 11 | 🔴 Window reconstruction ordering | Snapshot stores window blocks height-ascending; restore validates every merge_parent resolves within window ∪ root set, else snapshot = corrupt → fallback. Folded into G1. |
| 12 | ⚪ Clock skew (downgraded by parent) | `timestamp_ms` is explicitly non-consensus in the header spec; difficulty uses VDF params. Store last block height, not wall clock. No further action. |
| 13 | ⚪ Checksum bitrot paranoia (double checksum) | Single BLAKE3 in-file is sufficient: a corrupted checksum fails verification exactly the same as corrupted data → fallback. Sidecar adds complexity for no new guarantee. Rejected. |
| 14 | 🟡 Gap at chain.log tail (torn write at H_snap+1) | `replay_from` validates the first record parses; torn/missing → loud log + full replay. B's index makes the probe cheap. |

## 8. Rollout plan (fleet)

```
Phase 0  Land code (3 agents → parent integration → commit). All gates green.
Phase 1  Build: linux native + musl static-pie (fleet is glibc 2.36, builder 2.39 —
         the musl lesson from the zstd deploy). Preflight gate = grep usage output,
         NOT exit code (sigil-node --help exits non-zero — banked lesson).
Phase 2  Deploy binaries with SIGIL_SNAPSHOT_BOOT unset (=off): serial,
         Delta → Gamma → Beta → (soak) → Epsilon. Behavior identical to 0.35 fleet.
         .prev rollback binary beside every target (established pattern).
Phase 3  Enable on ONE follower (Delta): set env, restart, confirm 📸 lines +
         timed restart boots from snapshot. 24 h soak.
Phase 4  Enable Gamma + Beta. Soak.
Phase 5  Enable Epsilon (the producer — the node the feature exists for).
         One scheduled restart, measure G2 on the real 52 GB log.
ROLLBACK (any phase): unset env OR delete state-snapshot.bin → next boot is
         today's full replay. Binary rollback: mv .prev back + restart.
```

## 9. Version & compatibility matrix

| Component | 0.35 peer | 0.36.1 peer | Notes |
|---|---|---|---|
| chain.log format | ✓ unchanged | ✓ unchanged | `chain.idx` is additive; absent = fallback |
| snapshot file | n/a (never read) | local-only | never exchanged over the mesh |
| backfill wire | 'H'/'Z' both | 'H'/'Z' both | untouched by this release |
| flux-db on-disk | ✓ | ✓ | auto-flush changes WAL *lifecycle*, not format |
| monitor (sigil-top) | ✓ no change required | ✓ | explicitly out of scope this release |

## 10. After 0.36.1 — honest open lanes (parked, not forgotten)

1. **flux-db background flush/compaction thread** (if G7 shows spikes).
2. **Monitor in-memory window** — the remaining DB-write bound on the 1M blk/s path.
3. **BDP scaling** of CHUNK/MAX_INFLIGHT for high-RTT peers.
4. **bincode-canonical `hash()`** — 3-5× on ingest hash, but it is a **height-gated
   chain upgrade** (changes block identity). Producer-side, with activation height.
5. **1 Gbit followers** honestly cap ~300-500k blk/s with zstd; 10 Gbit class is where
   1M blk/s lives.

## 11. Decision log (how this plan was made)

- **Explore agent** (Claude subagent, 16 tool calls): verified WAL truncation already
  landed in flux-db v0.35; found `snapshot.rs` unused at boot; established replay is
  compute-bound; mapped flush/compact triggers.
- **DeepSeek API** (deepseek-chat): release scoping (headline + 3 supporting items +
  not-do list) and the 14-item red team (§7). Parent corrections applied: index size
  estimate fixed (16 B/entry ≈ 32 MB, not 400 MB), cadence math fixed (producer runs
  156-250 blk/s, not 84k), risk #12 downgraded, risk #13 rejected with reasoning.
- **Three Claude Code agents** (running): implementation per §4 with an explicit API
  contract to make parallel work safe.

*Plan author: rocky (Claude) · 2026-06-10 · the flux way: measured-first, gates over
opinions, every number in this document is either measured or marked as a target.*
