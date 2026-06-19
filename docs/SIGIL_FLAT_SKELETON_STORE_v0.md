# SIGIL flat append-only skeleton store — v0 (LANE-A format × LANE-C store)

> Author: rocky-sync-A. **Status: VALIDATED — THE path to 100k (lead #491 confirms).** The
> read_dir / skip-gets fix (#486) only reached 3,863 blk/s: even `trusted=true` (zero gets)
> is capped by flux-db's LSM per-key cost (~130 µs/KV entry × 2 entries/block). The lead's
> #491 architectural call IS this design — skeleton prefix → flat append-only store, NOT
> flux-db KV. EMPIRICALLY MEASURED (100k × 72 B): buffered append 19.4M blk/s; **durable
> (append + one fsync) 2.2M blk/s** — ~570× the flux-db 3,863 and ~22× the 100k goal. The
> commit wall vanishes; the constraint flips back to transport/verify (already fast).
> Companion to docs/SIGIL_SKELETON_CODEC2_v0.md.

## Why

MEASURED (lead #481, C's commit_sink_throughput, 100k blocks, real flux-db):
commit = **3,268 blk/s durable** / 3,548 non-durable. The wall is the per-block flux-db
`put_block_raw` (**~285 µs/block**) — NOT fsync (8%), NOT batch size. That caps the whole
sync at ~3.5k blk/s, **26× short of 92.6k**. Transport (~200k) and verify (~millions) are
already far past 100k. **Commit is the sole binding constraint.**

The fix the 72 B fixed-size skeleton enables: don't pay the general per-block DB put for
the prefix. Use a flat, height-indexed, append-only store.

## The key property

A `SkeletonRecord` is **fixed 72 B** under bincode (`height: u64` + `block_hash: [u8;32]`
+ `parent_hash: [u8;32]`). So:

- **height IS the index.** Record for height `H` lives at byte offset `(H - base) * 72`.
  Zero index update, zero key encoding, zero hash-keyed CF write, zero per-key WAL.
- **commit = memcpy** the raw 72 B (already bincode-framed on the wire — no re-encode) into
  an `mmap`'d region, or a buffered `pwrite`. 100k recs = 7.2 MB; appends at GB/s →
  millions of blk/s. The 285 µs/block becomes ~nanoseconds.

## On-disk layout

Two files under the data dir (`<db>/skel/`):

```
meta            (fixed, tiny)        data             (flat array of 72 B records)
  magic   [4]   = b"SKL0"              rec[0]  @ 0          = SkeletonRecord(base+0)
  version u16                          rec[1]  @ 72         = SkeletonRecord(base+1)
  base_height  u64                     ...
  committed_height u64  (durable)      rec[k]  @ k*72       = SkeletonRecord(base+k)
  anchor_hash  [32]                    ...
```

`committed_height` is the ONE durable watermark — the highest contiguous height whose 72 B
record is flushed. Readers trust `data` only up to `committed_height`.

## Commit path (the hot loop)

`pull_snapshot`'s commit hook changes from `FnMut(u64, &str)` (the `put_block_raw` seam) to a
flat-append seam. Cleanest: hand the raw record bytes straight through — pull_snapshot already
has the 72 B per record:

```
commit: FnMut(u64 /*height*/, &[u8; 72] /*raw bincode record*/)
  // launch() wires: |h, rec| flat.append_at((h - base), rec)
```

`flat.append_at(slot, rec)` = `memcpy(mmap + slot*72, rec, 72)` (or pwrite). No per-block
fsync. Flush + advance `committed_height` once per page (e.g. every 50k recs / 3.6 MB) with a
single `fdatasync` of `data` then a `fdatasync` of `meta` (2-phase: data durable before the
watermark moves — power-loss-safe, mirrors C's existing ring discipline).

## Crash safety

- Append-only + 2-phase watermark: on restart, trust `data[0..committed_height]`; anything
  past it is a torn tail → **re-pull** (the snapshot's `archive_root` + parent-linkage walk
  catch any corruption; the flat store is a cache of verified records, not the source of
  truth for trust).
- `committed_height` only advances after the records below it are flushed AND the snapshot's
  fold/anchor verification (LANE-B) accepted the range — so an unverified prefix never
  becomes a trusted watermark.

## Read path

- `get(H)` = read 72 B at `(H - base) * 72`, `bincode::deserialize::<SkeletonRecord>` — O(1)
  random access, no DB get.
- `fast_forward_to_anchored_checkpoint` reads the anchor record at `(anchor_height - base)*72`,
  compares `block_hash` to the trusted `anchor_hash`. Works unchanged in spirit.

## Split with flux-db

The general flux-db block store stays for the **FRONTIER** (full ~8 KB blocks, real txs,
state). Only the **skeleton PREFIX** goes to the flat store. The two never overlap: prefix =
flat (cheap, cache-of-verified), frontier = flux-db (full, authoritative).

## Expected result

Prefix commit 3.5k → millions blk/s → the constraint flips back to transport/verify (already
fast) = the 92.6k regime. This is the highest-leverage change off the 3.5k floor.

## Ownership / next

- LANE-C (rocky-sync-C): owns the flat-store impl + crash-safety + the 2-phase watermark.
- LANE-A (rocky-sync-A): owns the commit-hook signature change in `pull_snapshot` + the
  wire `SkeletonRecord` (72 B fixed — the enabler). I'll change the hook to pass raw bytes
  the moment C confirms the append seam.
- Open: confirm bincode `SkeletonRecord` is byte-stable at exactly 72 B (the
  `skeleton_record_is_72_bytes_on_the_wire` test pins it) so offset arithmetic is exact.
