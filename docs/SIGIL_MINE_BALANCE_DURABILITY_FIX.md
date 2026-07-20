# SIGIL — Crash-Safe Mine Balance (durability fix)

**Status:** design + incident post-mortem · **Date:** 2026-07-20 · **Component:** `crates/sigil-rpc/src/bin/sigil-rpcd.rs`

## TL;DR

A `sigil-rpcd` restart on 2026-07-20 collapsed the operator's mine balance from a live
in-memory **3.11 T** to a durable **569 B**, and reset the mine-tip from **74,563 → 0**.
Root cause: **mine rewards are credited to in-memory state and made durable only through a
monolithic `snapshot` blob that had gone stale at height 2, while the boot path trusts that
blob as the sole source of truth** — even though the per-block reward records were durably on
disk the whole time. The fix makes the **durable block store authoritative** and turns the
snapshot into a fast-boot checkpoint, with fsync'd frontier + graceful-shutdown flush.

## What actually happened (evidence)

| Fact | Value | Source |
|---|---|---|
| Live in-memory balance before restart | 3,111,133,294,133 | operator TUI |
| Durable balance after restart | 569,165,075,827 | `GET /api/v1/balance` post-boot |
| Restore log | `RESTORED state @ height 2 (60 wallets, native supply 3017000)` | stdout |
| Mine-tip before / after | 74,563 → ~0 (climbing) | `/mining/challenge` |
| **Durable `block/` keys in the 77 MB SST** | **24,428 records, heights 2..73,283** | binary scan of `state/flux_L01_…sst` |
| `snapshot` key writes materialized | 2 | binary scan |

The decisive line: **73k blocks of rewards were durably on disk, but the balance snapshot was
stuck at height 2.** The blocks (append-only, one key each) persisted fine; the monolithic
balance blob did not.

## Root cause — the two durability tiers are mismatched

The mine-accept path (`("POST","/mining/submit")`, ~line 1348) does three things on an
accepted share:

1. `credit_share(&mut n.state, eh, miner, reward)` — credits the miner **in memory** (`n.state`).
2. `store_block(&n, bh, &sub, reward, …)` — writes `block/{bh:020}` to flux-db (line ~205–212). **Durable, append-only, one key per block.**
3. `n.block_height += 1; n.height += 1; retarget(&mut n)` — advances counters **in memory**.

Balance durability then rides entirely on `persist(&node)` (line ~147), which serializes the
**entire** `Node` state into one `snapshot` key:

```rust
fn persist(node: &Node) {
    let snap = Snapshot { state: node.state.clone(), height: …, block_height: …, /* all pools, nonces, … */ };
    if let Ok(bytes) = bincode::serialize(&snap) { let _ = db.put(b"snapshot", &bytes); } // O(state), best-effort
    …
}
```

`persist()` is invoked by a generic hook after any 200 POST (line ~2014). Three structural
problems make the balance blob unreliable where the block store is reliable:

1. **O(state) full clone + bincode on every write.** As the self-mining chain grew, the
   `Snapshot` blob grew with it (the mine-accept path even has an OOM patch clearing a
   per-block event buffer that "accumulated unbounded in SigilState as a self-mining
   mine-chain grew"). A whole-state serialize per block is too heavy to run every ~5 s, so in
   practice the `snapshot` key lagged far behind the block store (materialized at height 2).
2. **Single-blob overwrite, no incremental durability.** One `db.put(b"snapshot", …)` replaces
   the whole thing. Its durability depends on flux-db memtable→SST flush + WAL fsync timing;
   an unclean kill (systemd restart = SIGTERM) loses everything written since the last SST
   flush. The block store survived precisely because it's many small append-only keys, several
   of which had already flushed to L01.
3. **Boot trusts the fragile tier as the sole source of truth.** Restore reads `block_height`
   and balances **only** from `snapshot` (line ~612: `block_height: snap.block_height`), and
   never reconciles against `max(block/…)`. So a stale snapshot silently wins over 73k durable
   blocks — the node boots at height 2, re-mines from 3, and the balance is whatever the last
   blob capture held.

**In one sentence:** the rewards are durable (block store), the balance is not (monolithic
snapshot), and the boot path believes the wrong one.

## The fix — make the durable block store authoritative

Layered, smallest-blast-radius first. (A)+(D)+(E) alone make balance crash-safe; (B)+(C) make
it cheap and bound the recovery window.

### (A) Boot reconciliation: replay the block store over the checkpoint
On boot, after loading the `snapshot` checkpoint, compute `store_tip = max height among
block/… keys`. If `store_tip > snap.block_height`, **replay** `block/{snap.block_height+1 …
store_tip}`, crediting each block's `reward` to its `submission` miner. Set
`n.block_height = n.height = store_tip`.

- Replay credits are **max-wins** (Balance Integrity Rule 1): never write a balance lower than
  the current one. A checkpoint that is *ahead* of a given block simply skips it.
- This is the same shape as Quillon's `replay_post_checkpoint_balances`, gated the same way
  (only run when the store is ahead of the checkpoint). It is the crash-recovery safety net:
  even a totally stale snapshot reconstructs the true balance from durable blocks.

### (B) Checkpoint on a cadence, not on every write
Replace "persist after every 200 POST" for the mine path with a **checkpoint every
`SIGIL_CHECKPOINT_EVERY` blocks** (default 512) *and* on graceful shutdown. Cost becomes
O(state)/512 instead of O(state)/block; replay-on-boot is bounded to ≤512 blocks. Keep the
per-request persist for **non-mine** mutations (sends/DEX/governance) — those are low-rate and
want immediate durability.

### (C) fsync the durable frontier
The block store is what we now trust, so its writes must be crash-durable:
- Call flux-db `sync_wal` (already exists, LANE-C) after each `store_block`, or batch-fsync
  every `SIGIL_SYNC_EVERY` blocks (default 32) to amortize.
- At each checkpoint (B), `sync_wal` after the `snapshot` put so the checkpoint itself is torn-
  write-safe. Stamp the checkpoint with `block_height` + a `balances_root` (BLAKE3 over sorted
  wallet→balance) so a torn/partial checkpoint is detectable and ignored in favour of replay.

### (D) Preflight assertion (fail loud, Rule: no silent lower balance)
Before binding ports, assert `snap.block_height ≤ store_tip`. If replay-derived balances differ
from the checkpoint beyond tolerance, log **loudly** and prefer the higher (max-wins), never the
lower. Exit non-zero on an unrecoverable inconsistency rather than serving a wrong balance.

### (E) Graceful-shutdown flush
Install a SIGTERM/SIGINT handler that does a final checkpoint + `sync_wal` before exit. A
planned restart (deploy, config change — exactly this incident) then loses **nothing**: the
in-memory frontier is flushed first. (`sigil-top` already has this pattern for its verified-
spine watermark; mirror it here.)

## Test plan (Alpha/isolated only — never hot-patch the live producer)

1. **kill -9 mid-mine** → reboot → assert `block_height` and every wallet balance equal the
   pre-kill values (replay reconstructs them from the block store). This is the regression that
   would have caught the incident.
2. **Stale-checkpoint** → hand-write a height-2 `snapshot` with a live block store at 70k →
   boot → assert replay lifts balance to the block-store truth, max-wins (no lower write).
3. **Torn checkpoint** → truncate the `snapshot` blob → boot → assert it's rejected and replay
   is used, not a garbage decode.
4. **Cadence bound** → `SIGIL_CHECKPOINT_EVERY=512`, crash at +511 → replay ≤512 blocks, tip
   exact.
5. **Graceful restart** → SIGTERM under load → zero divergence (the deploy path).

## Recovering THIS incident's balance

Because the block store is durable (24,428 records, heights 2..73,283, each with `reward` +
miner), the pre-restart balance is **largely reconstructable** by replaying those blocks —
this is mechanism (A) run once as a repair tool, not a fresh mint (Balance Integrity Rule 3:
never write a *lower* balance; replay only raises). Caveats: this SST level holds 24,428 of
~73k heights, so the remaining heights must be gathered from other flux-db levels / the WAL
before the sum is complete; and the reconstructed figure is the sum of durably-recorded
rewards, which is the honest durable truth (it may land below the 3.11 T in-memory display if
that display had run ahead of what was ever block-committed). Run read-only, in a copy of the
data dir, and diff against the live balance before applying anything to the producer.

## Non-goals / notes

- This does **not** change emission, reward math, or consensus — only *where balance durability
  comes from* and *when it flushes*.
- Applies to the mine-chain balance specifically; the same reconciliation naturally protects
  DEX/pool/nonce state carried in the same `snapshot` blob.
- Ordering: land (A)+(D)+(E) first (crash-safe + graceful), then (B)+(C) (cheap + bounded).
  Verify with the kill-9 regression on Alpha before the producer ever restarts on the new
  binary.
