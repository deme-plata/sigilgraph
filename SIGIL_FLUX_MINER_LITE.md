# flux-miner-lite — the very-low-CPU verifier-miner ("score big, burn nothing")

> Viktor: "a lightweight flux miner, very low — and I mean VERY low — CPU, but still big heights, like scoring big in GTA 6." This is the post-hashrate miner. `flux-miner` (VDF) *burns* CPU for hashrate. **flux-miner-lite burns ~nothing and climbs heights by doing the things SIGIL's security actually rewards: verify, attest, store.**

## The flip (why low-CPU can still score big)

SIGIL's security isn't hashrate — it's **verifiability × verifier-count × durability** (the security model). So don't pay for wasted squaring; pay for the real goods:

```
   flux-miner (VDF)            flux-miner-lite (verifier)
   ─────────────────          ──────────────────────────
   burns CPU squaring         ~0 CPU — verifies a tip in 10µs (free)
   secures ORDERING           secures CORRECTNESS + ECLIPSE + DURABILITY
   reward = win the VDF        reward = 0.1% operator pool + proof-of-storage
   "height" = blocks you mined "height" = blocks you VERIFIED + ATTESTED (free, fast)
```

You don't out-compute anyone — you **out-verify** and **out-store**. The chain produces a block every 40ms; you verify each in 10µs and your height climbs with it, costing nothing. That's the "big height on a potato" — heights scale with the chain, not your CPU.

## How you "mine" (the loop — fits in a 572 KB binary)

```
   loop {
     tip = fetch();                 // from the mesh / DNS anchor
     verify(tip);                   // ~10µs, real sigil-tip-proof — FREE
     attest = sqisign("I saw H=N"); // you become an independent verifier → raises f^K
     broadcast(attest);             // strengthens the network's eclipse-resistance
     hold + serve aether shards;    // proof-of-storage → you're a durability host
   }
   // reward: a slice of the 0.1% operator pool (already wired in sigil-bank)
   //         + proof-of-storage payout for shards you prove you hold
```

CPU cost: a verify + a signature every 40ms. A Raspberry Pi does it idle. **No fans, no heat, no hashrate — and the height counter screams upward.**

## "Score big in GTA 6" — the gamified scoreboard

The miner's headline number isn't hashrate, it's a **SCORE** that climbs like an arcade combo:

```
   SCORE = verified_height
         × attestation_streak       (unbroken blocks you've verified live)
         × shards_held              (durability you provide)
         × source_diversity (K)     (independent tips you cross-check → f^K)
```

The sigil-top **LIGHT-CLIENT SCORECARD** becomes the miner HUD: a big, flashy, climbing number + a leaderboard of verifier-miners — "0 chain bytes, 0 watts wasted, height 4,193,xxx, streak ×9,400." Scoring big = a long honest streak + deep storage + wide source diversity, not a bigger GPU.

## Compatible with our filesystem (flux-aether)

The miner IS an **aether host**: it holds Reed-Solomon shards (16+8), answers proof-of-storage challenges, and serves shards to syncing light clients. Storing the chain = earning. So the miner doubles as the durability layer (the can't-lose mesh) — lose hosts, not data, and the hosts get paid.

## Built from what already exists (FLUXFOOD-composed, almost nothing new)

| existing piece | role in flux-miner-lite |
|---|---|
| `sigil-lite` (572 KB, 10µs verify) | the verify core |
| 0.1% operator coinbase fee (`sigil-bank`) | the reward (already wired — pays light verifiers) |
| `flux-aether` RS 16+8 + proof-of-storage | the storage half + its reward |
| SQIsign-L5 | attestations (signed "I verified H=N") |
| `sigil-updater` / sigil-top hot-update | self-updating miner (no manual re-download) |
| sigil-top SCORECARD + TUI | the HUD / scoreboard |
| `flux_miner_*` MCP combo (from flux-miner) | `flux_miner_lite_start/status/score` — agent-native |

## Ship path
- **ML-1** add the attest+reward loop to `sigil-lite` behind a `--mine` flag (verify → sign → broadcast → claim operator-pool slice). Reuses everything; ~one new module.
- **ML-2** proof-of-storage participation (hold + prove aether shards) → storage reward.
- **ML-3** the SCORE HUD in sigil-top (`[M]` mine mode → the climbing scoreboard).
- **ML-4** `flux_miner_lite_*` MCP combo + hot-update (already have the machinery).

## The one line
**flux-miner-lite mines the way SIGIL is actually secured — by verifying, attesting, and storing — so a fanless 572 KB client on a potato racks up a GTA-sized height-and-streak score while burning, genuinely, almost no CPU.** Mining without the waste; the score is honesty, not heat.

— rocky, 2026-05-31 · the verifier-miner. Big heights, no burn.
