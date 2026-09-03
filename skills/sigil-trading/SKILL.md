---
name: sigil-trading
version: 1.0.0
audience: public
description: How new SIGIL is created (emission), how it is priced, and how to read a chart honestly — for anyone holding, mining or trading SIGIL.
---

# Trading SIGIL — where the coins come from, and how to read the market

Speak plainly. Picture first, technical name second. Never quote a live number
you were not given — say which screen shows it.

## 1. Emission — where new SIGIL comes from

Think of a barrel that is being filled, and a tap whose flow is being adjusted
so the barrel fills on schedule. Emission is the tap.

- **Total supply is capped at 21,000,000 SIGIL.** That number never changes.
- The base unit is a **glyph**. **1 SIGIL = 10,000,000,000 glyphs (10 decimals).**
  Amounts on the wire are glyphs. Divide by 10^10 before showing a person.
- Emission is split into **64 eras**. Each era lasts **4 Julian years**
  (126,230,400 seconds). Era 0 pays out half the cap; each following era pays
  half of the one before. 64 × 4 years ≈ **256 years** of emission.
- Crucially, the schedule is **time-based, not block-based**. Era k is decided by
  how many seconds have passed since genesis, not by how many blocks exist. Mine
  faster and you do not get more SIGIL — you get the same era budget split
  between more blocks, so each block pays less.
- Within an era the payout is steered by a **controller** (a thermostat, in
  effect). It compares minted-so-far against the time-ideal target and nudges the
  per-block reward up or down. Two hard limits it never crosses: a floor of
  0.00001 SIGIL, and a ceiling of **2 SIGIL per block**.

Consequences worth telling a miner:
- **Your reward per block falls as more people mine**, because the era budget is
  fixed in time. Hashrate competes for a fixed flow, it does not increase it.
- If the network ever slowed below roughly **0.042 blocks/sec**, the 2-SIGIL
  ceiling would start truncating emission and the chain would under-emit. Far
  from that today, but it is the failure mode to name.
- A halving here is gradual and time-driven — there is no single dramatic block
  where the reward halves, unlike Bitcoin.

**Mining is not the only flow.** Blocks pay the miner; the DEX moves existing
coins between pools; the Nation welfare treasury redistributes a slice of the
protocol fee. Only mining creates new SIGIL. Everything else moves what exists.

## 2. What actually sets the price

SIGIL trades in two very different places, and confusing them is the most common
mistake:

- **On SIGIL's own DEX** — a constant-product pool (x·y=k). Price is just the
  ratio of the two reserves. A big buy moves it a lot because the pool is small.
  This is *slippage*, not a market opinion.
- **On Polygon as wSIGIL** — the bridged, wrapped token, paired against USDC.
  This is where an outside dollar price exists at all.

So "the price of SIGIL" needs the venue attached. They can and do differ, and
the gap is an arbitrage, not an error.

## 3. Reading a chart — the honest version

Technical analysis (TA) is pattern-reading on price history. It describes what
traders *did*; it does not know the future. Use it to frame risk, never as
prophecy. The vocabulary people actually use:

- **Support / resistance** — price levels where buying or selling repeatedly
  showed up. A shelf and a ceiling.
- **Trend** — higher highs and higher lows is an uptrend; the reverse is a
  downtrend. Everything else is a range.
- **Moving average (MA)** — the average close over N bars, drawn as a line. It
  smooths noise. A short MA crossing above a long one is read as strength.
- **RSI (Relative Strength Index)** — 0-100, how hard price pushed up versus
  down recently. Above ~70 "overbought", below ~30 "oversold". These are
  descriptions of speed, not signals to act.
- **Volume** — how much actually traded. A move on thin volume is weak evidence.
  On a small DEX pool, most "moves" are thin volume.
- **Pivot points** — yesterday's high, low and close reduced to a mid-point and
  levels around it. Popular with futures traders (George Kleinman's approach)
  because they give one fixed reference for the session instead of a moving one.
- **Fibonacci retracement** — after a move, mark 38.2%, 50%, 61.8% of it as
  places a pullback often pauses. Used with **Elliott Wave** (the idea that
  trends run in five legs and correct in three) by chartists such as Todd Gordon.
  Both are frameworks for organising a chart, and both are contested — say so.

**Where TA breaks on SIGIL specifically.** These tools assume a deep, continuous,
liquid market with many independent participants. SIGIL's own pool is small and
young, so a single wallet can draw a "pattern" by itself. On thin liquidity,
treat every chart signal as weak and size positions accordingly.

## 4. Emission and price together

The one genuinely SIGIL-specific insight worth offering: because emission is
time-based, **new supply arrives at a rate that is indifferent to price and to
hashrate**. Sell pressure from miners is therefore roughly constant per unit of
time, not per unit of enthusiasm. When demand rises, supply does not rise to meet
it — which is the whole point of the fixed schedule.

## 5. Live numbers — never guess these

Do not state any of these from memory. Point at where they are shown:

| Number | Where |
|---|---|
| Balance | Wallet tab, or `/v1/balance` |
| Block height, hashrate, reward | Mining tab (3) |
| Total supply / % minted | Node tab (1), economics panel |
| Pool reserves, DEX quote | `/v1/pools` |
| wSIGIL/USDC price | The bridge card in the web wallet |

If asked "what is SIGIL worth" — say honestly that it depends on the venue,
point at the bridge card for the dollar quote, and never invent a figure.

## 6. Money safety

Never propose a send, swap or bridge as a done action. Describe the step, name
the screen, and let the person press the button. Transfers are irreversible.
SIGIL sends are shielded — they need a client-side proof and take time to settle;
a receipt means "accepted", and settlement is confirmed separately.
