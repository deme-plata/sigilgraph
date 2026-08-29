# SIGIL testnet reset — `sigil-g1` → `sigil-g2`

**Your balance is zero. That is the reset, not a bug — and not your wallet.**

We restarted the SIGIL testnet from block 0 today. Every balance, every note and every block
of history from `sigil-g1` is gone. Nothing was lost to an attack or a failure; we did this
on purpose, and here is why.

---

## The bug we were fixing

SIGIL money is not one balance. It's a bundle of individual notes, each with a fixed value —
and until this week, **one payment could spend exactly one note.**

Mining minted a fresh note every single block. At the live block rate that's roughly 230,000
new notes per day, each worth one block reward. So a miner's balance was real, and almost
none of it was spendable: to pay, you pick one note, and one note is one block's reward.

Someone reported it from the Android wallet as *"I can send 0.006 out of 11 SIGIL."* They
were reading it correctly. The number was true and meant something other than it looked like.

It got worse the more you cared about privacy. A miner who **hadn't** registered for private
payouts got an ordinary balance that adds up, and could spend all of it. A miner who **had**
registered got dust. Choosing privacy is what broke spending.

When we measured the pool this morning: **836,536 notes** holding 18,039 SIGIL, and exactly
**one** of them had ever been spent. By tonight it was 1,208,134. A write-only pool, growing
by roughly 370,000 notes a day.

## What's fixed

**Mining pays into an ordinary balance now.** Balances add up, so you can spend everything
you've mined, and the dust never starts.

We checked whether this costs privacy before changing it. It doesn't: a mining note is minted
in the block you just mined, and that block names you. Our simulation measured 620 out of 620
mining notes as publicly traceable to their miner the moment they were created. They inflated
the note count without hiding anyone. Real privacy now comes from choosing to shield an
amount, at a time you pick — which is better cover on every count.

**Payments can now spend two notes at once.** New circuit, so notes can finally be combined.
On `sigil-g1` this would have activated at block 600,000 — days away. On a fresh chain it
works from block 0.

**And the privacy fix is actually running.** Awkward one: the code that stops a payment proof
from revealing the amount and the recipient was written, tested and merged — and then nothing
called it. It sat unused for a day while real payments kept publishing their details. It's
wired in now, with a test that inspects the actual bytes a wallet is about to broadcast, so
it can't quietly fall out again.

## Two things we changed while we had the chance

Genesis is the only moment some numbers can be set at all, so we used it.

**SIGIL now has 10 decimals instead of 8**, and the base unit has a name: the **glyph**.

```
1 SIGIL = 10,000,000,000 glyphs
```

The name isn't decoration. A *glyph* is a sign with a fixed, shared meaning that anyone can
read. A *sigil* — from `sigillum`, "a small seal" — is a unique mark whose meaning is private
to whoever made it, and which can't be read as ordinary text. That's this chain in two words:
the glyph is the public unit everyone counts in, and the SIGIL it composes into is the sealed
thing only its owner opens.

Ten was close to the ceiling, not a round number we liked. Shielded note amounts are
range-constrained inside the proof, over a 64-bit field, so decimals are capped by the
mathematics: at 10, one note holds up to 28,823,037 SIGIL — just above the 21M supply. At 12
a note couldn't hold the supply. At 18, the Ethereum convention, a note couldn't represent
**one** SIGIL. That's also why we can't simply adopt Quillon Graph's 24 decimals: Quillon has
no range-constrained notes, so no ceiling. Privacy and fine decimals pull against each other,
and 10 is near the top of what the circuit allows.

**Nothing changed in value.** Fees, rewards and the 21M cap are the same amounts of SIGIL;
only the smallest expressible slice got 100× finer.

## Why reset instead of migrating

We could have kept the old chain and let the fixes activate at a future block. We chose not
to, for three reasons:

- The 1.2 million existing dust notes can't be cleaned up in bulk. Combining notes takes one
  transaction per note — around 1.2 million transactions. Individually recoverable, but not
  as a whole.
- A fresh chain gets the fixes at **block 0** instead of block 500,000, so nothing has to be
  coordinated across a deadline.
- We can test note-merging today rather than in two days.

SIGIL is a testnet. It's where we're allowed to be wrong cheaply — that's what it's for, and
it's why the mainnet work happens elsewhere.

## What you need to do

**Nothing, except start again.** Balances are zero for everyone, including us. There was no
premine on `sigil-g1` and there is none on `sigil-g2` — supply at block 0 is exactly zero.

- **Update your node or `sigil-top` client.** The old version can't join the new chain; it's
  a different network and they won't talk to each other.
- **Keep your recovery phrase.** Your wallet still works — it's the chain that restarted, not
  your keys.
- **Mining starts from scratch.** Rewards now land in a spendable balance.

**Bridge users:** the 1 wSIGIL on Polygon is no longer backed by SIGIL on this chain. It's a
test token in a pool worth roughly $0.001, and the bridge relayer has been stopped since
27 August, so nothing further can be minted. We're flagging it rather than letting you find
it later.

## Honest status

- SIGIL is a **testnet**, and there is currently **one block producer**. It is not
  decentralised yet, and we won't claim otherwise.
- Note-merging is built and tested, but it's new today. Expect us to find things.
- We've asked for an independent review of the private-payment code. When it comes back,
  we'll post what it finds — including anything unflattering.

Questions welcome. If your wallet behaves oddly after updating, tell us — post-reset is
exactly when we want to hear about it.
