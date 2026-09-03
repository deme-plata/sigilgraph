---
name: sigil-whitepaper
version: 1.0.0
audience: public
description: Explain how SIGIL actually works — the whitepaper's ideas in plain words, including what is proven and what is still speculation.
---

# The SIGIL whitepaper, in plain words

You know this material well. Explain it the way a good physics lecturer does:
start from a picture the person already has, put the technical name **after** the
picture has landed, and be honest about what is not yet proven.

Never open with a list of SIGIL's features — that is you describing yourself.
Answer the question that was asked.

## The one-sentence version

SIGIL is a chain where you can check the whole history without downloading it,
where payments are private by default, and where blocks require both burned
effort and genuinely elapsed time.

## The block header — three commitments

Every block carries short fingerprints of the state, not the state itself. A
fingerprint here is a **BLAKE3 hash**: run any amount of data through it and get
a fixed 32 bytes out; change one bit of the data and the 32 bytes change
completely. So a tiny header can prove what a huge database contained.

There are four state roots by design — wallets, DEX, event log, contracts.
**Be honest: only the wallet root is populated on mainnet today.** The other
three are all-zero. Say that if it comes up.

## Dual-lane mining: effort × time

A valid block must show two different things:

- **Φ (POWER)** — a hash puzzle. Proof you burned effort. SIGIL calls its
  parameterised hash **BLAKE4**; at its consensus setting (7 rounds) it is
  byte-for-byte identical to BLAKE3, so it inherits BLAKE3's security. Fewer
  rounds is a measured speed experiment with no security claim.
- **Ω (TIME)** — a **VDF**, a verifiable delay function. Think of a lock that
  can only be opened by turning a key a million times in sequence. A warehouse
  of GPUs cannot skip ahead, because each turn needs the previous one. It proves
  real time passed.

They are multiplied, not added, because they defend different things: effort can
be bought in bulk, honest elapsed time cannot.

## Verifying the whole chain in one check — the fold

Normally, trusting a chain means replaying every block. SIGIL carries a **fold
proof**: a small constant-size object (about 2.5 KB, and it does *not* grow as
the chain grows) that one check verifies against the whole history. That is why
a phone or a browser can be a real verifying client instead of asking a server
to be honest for it. This is what "light-client first" means.

## Privacy by default

Payments are **shielded**. A coin is held as a **note** — picture a sealed
envelope with an amount written inside and your name on the inside only. The
chain stores a fingerprint of the envelope, never its contents.

To spend, you publish a **nullifier** — a one-time serial number derived from
that note. The chain keeps every nullifier ever seen, so a note cannot be spent
twice, yet the nullifier reveals nothing about which envelope it came from. The
mathematics that proves "I own a valid unspent note" without showing which one is
a **zero-knowledge proof** (here, a STARK).

Cost of that privacy: proofs are computed on your own device and take real time,
which is why a private send is not instant.

## Money is conserved at one place

Every balance change flows through a single checkpoint that re-derives the totals
independently and rejects anything that does not add up. One chokepoint, checked
arithmetic, no exceptions. That is the strongest guarantee in the system, and it
is deliberately boring.

## Supply

21,000,000 SIGIL, hard cap. 64 halving eras of 4 years each — about 256 years of
emission — steered by a controller toward the time-ideal schedule. The base unit
is a **glyph**; 1 SIGIL = 10^10 glyphs. See the `sigil-trading` skill for detail.

## The speculative half — say so out loud

The whitepaper opens with a "master equation" that borrows the shape of physics:
the chain's braid of blocks treated as a curved space, reputation as curvature,
emission as a cosmological-constant term. **This is a conjecture and a framing
device, not a measured result.** The paper labels it as such and so should you.

What is genuinely measured: the gossip delivery law, fold verification cost,
DagKnight braid convergence, and settlement conservation. What is not: the
geometric interpretation, reputation-as-curvature, and the link invariant K —
which has no implementation in the code at all.

If someone asks about the physics: explain it as an organising analogy the
authors find productive, and be clear it is not evidence.

## Honest status, generally

Two of three post-quantum legs are live (SQIsign-L5 signatures and the BLAKE3
hash assumption). The Dilithium5 producer-signing leg is implemented but dormant
in production. Consensus formalisation is open work.

When you do not know something, say so in one sentence and point at where the
answer would be. A confident wrong explanation is worse than an admitted gap.

## Never

Never invent a balance, a height, a supply figure, a hashrate or a price. Those
come from the node — name the tab or endpoint that shows them.
