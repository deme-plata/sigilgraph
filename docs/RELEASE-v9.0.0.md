# sigil-top v9.0.0 — "the chain remembers, and the fee can move"

*2026-09-11 · sigil-g2 · every follower must update: this release changes what a
follower computes, not just what it shows.*

## Why this is a major version

Think of a SIGIL block as two things stapled together: a **header** (four 32-byte
roots that summarise the state after the block) and a **body** (the transactions
and the events they emitted). A follower re-runs the body and checks that it lands
on the header's roots. If it doesn't, the follower stops — *STATE DIVERGENCE* — and
that is the whole security model: nobody can hide a different state from you.

Two things in v9 change what that re-run produces, so an 8.x follower stops the
moment it meets a v9 block that exercises them. Updating is not optional; it is
what keeps your node on the chain.

## 1. Shielded sends are finally recorded (event_log_root is real)

Until 2026-09-11 the only payment a user can make on g2 — a shielded send — was the
one transaction kind that recorded **nothing**. `event_log_root` was all-zero in
every header, so the court, the indexers and wallet history had no input, *by
construction*.

Emitting the event broke the chain on 2026-09-10 (happysrv froze at 5,525,905). The
event was never the bug. The producer builds a block one transaction at a time and
each commit wiped the previous transaction's events, so it published an empty root
while every follower computed a real one. Both builders now rebuild the block's
event log from the mutation list before taking roots — the follower and the
producer agree **by construction**, and four tests fail without the fix
(`a_block_carrying_events_is_accepted_by_a_follower` asserts all four roots through
the real builder).

**What it means for you:** a v9 node computes a non-zero `event_log_root` on any
block with a shielded send. An 8.x node computes zero for the same block and stops.

## 2. The dev-fee wallet can be rotated — by schedule, never by a switch

The master wallet (the 7.5 % coinbase dev fee) is set once in block 0 and the
chokepoint refuses a second install. Moving it used to mean a new genesis.

v9 adds `StateMutation::RotateMasterWallet`, valid **only** when
`(height, wallet)` is an entry of a table compiled into every binary:

```
sigil_state::MASTER_WALLET_ROTATIONS: &[(activation_height, new_master)]
```

- At a scheduled height the block **must open** with that rotation; the same
  block's coinbase already pays the new master. A producer cannot skip it, delay
  it, or point it elsewhere.
- Any rotation not on the table is refused outright. There is no signature to forge
  because there is no authority to exercise: the binary is the authority, exactly
  as it is for genesis.
- The variant is appended last, so every existing block decodes byte-for-byte.

**The table is empty in v9.0.0.** Nothing rotates until the operator names a
wallet and a height in a follow-up release; that release must reach every follower
before the height (≈570k blocks per day at 6.6 blk/s). The tests drive the real
minter and the real follower through a scheduled height — the live producer will
never be the first to emit it.

## 3. A follower that got forked can recover

n=2 finality froze on 2026-09-08 when a producer restart orphaned the single
follower: linear catch-up cannot reorg. v9 carries the durable follower reorg,
a 512-block gap bound, the surfaced apply error, and the backfill fixes that let a
new node actually join (fair per-peer budget, peer rotation and scoring,
concurrency bounded by requests, adaptive chunk + frontier rewind, and the second
backfill loop that had the same two bugs).

## 4. Also in v9

- **Shielded staking** (`sigil-stake`): lock value without disclosing it.
- **SIGIL Nation Supreme Court v3** on the node (`/v1/court/*`) with the founding
  bench seated; share-link UI.
- **Witness endpoint** for per-height roots (`/v1/witness/*`), filled by followers
  too.
- **K-gauge runs on the node** (`GET /v1/kgauge`) with the observables behind it, so
  the MCP, sigil-top and the browser stop computing three different answers.
- **Wallet: Settings → 🔑 Reveal recovery phrase.** Shown only if it derives the
  address you are logged in as; auto-hides in 90 s and the moment the tab goes to
  the back. The same screen offers the **rig seed** (`SIGIL_MINE_SEED`, 64 hex) so
  a mining rig can mine *as* your wallet and open the private notes it earns.
- **[A]I tab sets itself up at boot** — the function existed since 8.0.6 with zero
  callers.
- **linux-arm64 is in the signed manifest** — `[U]` on a Termux install no longer
  fails closed.
- Manifest artifact URLs now point at the canonical home, `sigilgraph.org`.

## How to update

- sigil-top: `[U]` in the TUI, or `sigil-top update`, or fetch
  `https://sigilgraph.org/downloads/sigil-top-linux-x64` (`-windows-x64.exe`,
  `-linux-arm64`). Verify: `fluxc verify-proof <artifact> <artifact>.proof`.
- Followers (`sigil-node`): rebuild from tag `v9.0.0`; the producer on Epsilon
  already runs the event-root fix.

## What is still pretend

- No rotation is scheduled yet; the machinery is proven in tests, not on the live
  chain.
- Finality is still `NO-BFT (n<4)`: two nodes buy cross-check, not Byzantine
  tolerance.
- The court's archive and docket live in memory (a restart empties them).
