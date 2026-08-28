# Spending more than one note at a time — the design for a 2-in circuit

**Status:** design, not built. Written 2026-08-28 after fixing the dust that made it urgent.
**Depends on:** `TRANSPARENT_COINBASE_HEIGHT` (shipped) and `sigil-shield::payment` (shipped).

---

## The one-sentence version

The spend circuit takes exactly one note as input, so a wallet can never combine two notes
into a bigger one, so a balance spread over many notes is permanently unspendable in bulk.
The fix is a second circuit that takes two — and the trick is to **add** it rather than
replace the one we have, because that removes the only genuinely hard part.

## Where the wall is

`spend_full_v5` is 1-in/2-out. One note goes in; a payment and a change note come out.
That single fact has a consequence people find surprising every time:

> The amount you can send in one transaction is not your balance. It is your largest
> single note.

There is no way around it from the outside. You cannot merge notes by sending to yourself,
because sending to yourself is still one note in and two out — you end up with *more* notes,
never fewer. Consolidation is not unimplemented; with one input it is **arithmetically
impossible**.

Measured on the live chain at height 311,862, before the coinbase fix:

| | |
|---|---|
| Notes in the pool | 836,536 |
| Value locked in them | 18,039 SIGIL |
| Average note | 0.0216 SIGIL |
| Nullifiers ever revealed | 1 |

A pool written to 836,536 times and read from once. Someone reported it from the phone as
*"I can send 0.006 out of 11 SIGIL."* They were reading the instrument correctly.

## What has already been done about it

Two shipped changes take the pressure off without touching the circuit:

1. **Mining rewards are paid transparently** from `TRANSPARENT_COINBASE_HEIGHT`. This is
   what was *creating* the dust — one note per miner per block, ~230,000/day at the
   measured 2.66 blk/s. Transparent balances add, so mined value now accumulates as one
   number. Nothing was lost: chronos measured coinbase notes as 620/620 publicly
   attributable to their miner at mint time, so they never enlarged the anonymity set.
2. **`sigil-shield::payment`** picks the note, and when no note is big enough it mints one
   from the transparent balance via a standard-denomination `Shield`.

So the reported symptom is fixed. What remains is the *general* case: a wallet that
receives many payments accumulates one note per payment, and still cannot spend two at
once. That is what this design is for.

## The design

### Add v6, don't replace v5

The instinct is to generalise: make the circuit K-in, pad unused slots with dummy inputs.
That instinct walks straight into the hardest problem in the space.

A dummy input has no real note, so it has no valid Merkle path, so the membership
constraint must be *conditionally* enforced — something like "check membership only when
`value != 0`". In a STARK there is no branching, so that becomes a witness column `inv`
with `value · (1 − value · inv) = 0`, giving `is_real = value · inv`, and then every
membership constraint gets multiplied by `is_real`. That multiplication **raises the
membership lane from degree 7 to degree 8**, which moves the exemption budget and the
blowup factor — the exact territory where an earlier session lost hours to
`InconsistentOodConstraintEvaluations`. It also means dummy nullifiers get revealed and
recorded forever, growing the spent set with entries that never corresponded to money.

None of that is necessary, because **the verifier already dispatches on circuit version**.
`note_v1::verify_spend_wire` tries v5 and falls back to v4. Adding a third branch costs one
`if`. So:

| Wallet needs | Circuit | Inputs |
|---|---|---|
| Pay from one note | `spend_full_v5` (unchanged) | 1 |
| Merge two notes | `spend_full_v6` (new) | 2 |

Every spend has **exactly** the number of inputs its circuit declares. No dummies, no
conditional membership, no degree change, no new soundness argument beyond "v6 is v5 with a
second input block and a sum". This is the whole point of the design.

### v6's shape

v5's trace is 33 columns wide: 15 for the input (cols 0–8 Merkle lanes, 9–14 note and
owner-key binding) and 9 per output. v6 adds one more 15-column input block:

```
v5:  [ input 0 : 15 ] [ out 0 : 9 ] [ out 1 : 9 ]                = 33 columns
v6:  [ input 0 : 15 ] [ input 1 : 15 ] [ out 0 : 9 ] [ out 1 : 9 ] = 48 columns
```

Trace **length is unchanged** — `(depth+1)·64`, doubled for the zero-knowledge mask. That
is the reason to grow width rather than time: FRI depth follows trace length, so a wider
trace costs more trace-LDE and a bigger commitment, but does not deepen FRI. Expect
roughly `48/33 ≈ 1.45×` v5's proving cost, not `2×`.

Public inputs gain a second nullifier:

```rust
pub struct SpendFullV6PublicInputs {
    pub root: BaseElement,            // one anchor, shared — both notes from the same tree
    pub nf: [BaseElement; 2],         // was a single `nf`
    pub fee: BaseElement,
    pub cm_outs: [BaseElement; N_OUTS],
}
```

Constraints are v5's, instantiated twice at a column offset, plus exactly one new one:

```
value_0 + value_1  ==  out_0 + out_1 + fee
```

The range bound already in v5's module docs must be re-checked for the extra term:
`(N_OUTS + 1) · 2^RANGE_BITS < p` becomes `(N_INS + N_OUTS) · 2^RANGE_BITS < p`. At
`RANGE_BITS = 58` and the Goldilocks prime this still holds with room, but it is a
correctness condition and belongs in a `const` assertion, not a comment.

### What else has to move

- **Wire format.** `SigilTx::ShieldedSend { nullifier: [u8; 32], .. }` becomes a list.
  This is a consensus change and needs a height gate, exactly like the coinbase change.
- **State application.** Check and record *both* nullifiers atomically — either both are
  fresh and both get recorded, or the transaction is refused. A partial application here
  would be a double-spend.
- **Verifier dispatch.** A third branch in `verify_spend_wire`. ⚠️ It must dispatch on a
  declared version, **never on trace length** — 512 rows is v4-at-depth-7 *or* v5-at-depth-3,
  and that ambiguity has already caused one failure ("expected 9408 query value bytes, but
  was 10752").
- **Wallet.** `payment::plan_payment` gains a `ConsolidateThenSpend` arm: merge the two
  smallest notes, repeat until one note covers the payment. `log₂(n)` rounds — 4–5 for a
  wallet with 20 notes, and each round is an ordinary transaction.

## What this does not fix

**The 836,536 notes already in the pool stay stranded.** Two-in halves the count per round,
so clearing them would take ~418,000 transactions in the first round alone. They cannot be
economically swept, and no widening of the circuit changes that — 4-in still needs ~209,000.

g1 is a testnet that was reset on 2026-08-26 with zero premine, so there are two honest
options, and both are operator decisions:

1. **Write off** the 18,039 stranded SIGIL. It is testnet value; the chain is two days old.
2. **Migrate**: credit each wallet's shielded holdings back transparently and reset the
   pool. Cheap, one consensus event, and it makes the pool's real anonymity set — which is
   counted in distinct unlinkable owners, not in notes — start from an honest zero rather
   than from 836,536 pieces of attributable padding.

## Order of work

1. `spend_full_v6.rs` — v5 duplicated at an offset, plus the sum constraint. Prove and
   verify a real 2-note merge in a test before anything else moves.
2. Wire format + height gate + atomic double-nullifier application.
3. Third dispatch branch, with a test that a v5 proof and a v6 proof of the same depth are
   never confused.
4. `ConsolidateThenSpend` in the planner, and the wallet loop that runs it in the
   background so the user never sees the word "consolidate".

Step 1 is self-contained and is where the risk is. Do not start step 2 until a v6 proof
verifies.

## A note on testing this class of change

Every hour lost on the v5 masking work went to the same mistake: proving in release, where
a failed constraint only says `InconsistentOodConstraintEvaluations`. **Prove in debug when
something is wrong** — the debug assertion names the offending constraint. (The shield
prover tests are `#[ignore]`d in debug because of a pre-existing winterfell 0.9 assert, so
use `--profile release-fast`, which inherits `release` and therefore keeps
`debug_assertions` off, for the fast path.)

And the bug that actually cost the most was not in a constraint at all: `get_pub_inputs`
read `trace.length() - 1`, which after padding is a *masked* row. Any anchor taken from the
last row moves when the trace is padded. v6 doubles the number of places that mistake can
be made.
