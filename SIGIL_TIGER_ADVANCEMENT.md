# Full-node advancement: territory, not throttle (the tiger model)

> Viktor: *"when going full node the algorithm should advance like a tiger wanting territory — not by using more resources, but intelligently advance to the next level, with flux://."*

The anti-PoW leveling system. A SIGIL full node does **not** level up by burning more CPU. It levels up by claiming **verified territory** — and it marks that territory with a `flux://` proof, the way a tiger marks its range. Expansion is predatory but cheap; the limit is *intelligence* (what you bother to verify and hold), not power draw.

## The four territories a node claims — all via flux://, all verification-not-watts

| territory | how you claim it | the proof (flux://) | cost |
|---|---|---|---|
| **BREADTH (K)** | cross-check the tip from K independent sources | `flux://tip@…` from N anchors/peers | ~0 — K fetch+verify |
| **DEPTH (fold)** | fold more blocks into your 2.5 KB proof | `flux://fold@H` — succinct whole-chain proof | **O(1)** — constant size |
| **GROUND (aether)** | host more RS shards, answer storage challenges | `flux://shard/<id>` you serve + prove | disk, not CPU |
| **STREAK (time)** | unbroken verified attestations | `flux://attest` signed, climbing | ~0 — a verify + a hash |

**LEVEL = f(K, fold-depth, shards-held, streak).** A tiger's territory is *wide* (K), *deep* (fold), *well-held* (shards), and *long-defended* (streak). None of it is hashrate.

## Why "tiger," not "miner"

A PoW miner is an **ox**: it levels by pulling harder — more watts, more heat, same ground. A SIGIL node is a **tiger**: it levels by claiming the next verifiable frontier *intelligently* — seek a new independent source, fold one more epoch, take on one more shard, extend the streak — and stakes it with a `flux://` proof that says **"this ground is verified mine, and anyone can check it."**

The "advance to the next level" is therefore a **search problem, not a power problem**: the node intelligently picks the highest-value unclaimed territory (the source that most raises K, the fold that most compresses, the shard most at risk of being lost) and takes it. That's the tiger choosing where to expand — not the ox pulling harder on the same furrow.

## flux:// is the medium of the claim

Every inch of territory is a `flux://` resource the node *resolved and verified*. So advancement is **provable and portable**: a node's level isn't a self-reported number, it's a set of `flux://` proofs anyone can re-check. You don't *claim* you verified to H — you hold `flux://fold@H` and it checks in one shot. Territory you can't prove, you don't hold.

## On the dashboard (next rev)

Add a **LEVEL / TERRITORY** readout to `flux://dashboard`: the node's claimed range across the four axes, each a `flux://` link you can resolve to verify the claim. Going full = the tiger stretches — and every inch is provable, not purchased with watts.

— rocky, 2026-05-31 · the anti-watt leveling model. **Territory, not throttle.**
