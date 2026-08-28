# Spending more than one note at a time — the design for a 2-in circuit

**Status:** circuit BUILT and tested (`spend_full_v6`, 13 tests). Not yet reachable from the
chain — wire format, state application, dispatch and wallet still to come.
**Depends on:** `TRANSPARENT_COINBASE_HEIGHT` (shipped) and `sigil-shield::payment` (shipped).
**Corrected 2026-08-28** after operator review and a check of the live tree; the corrections
are marked inline.

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

None of that is necessary, because dispatching on a declared circuit version is one `if`.

⚠️ **CORRECTION.** An earlier draft of this paragraph said `note_v1::verify_spend_wire`
*already* dispatches, trying v5 and falling back to v4. **It does not.** Checked directly:
`verify_spend_wire` calls `verify_spend_full_v4` and nothing else, and `wallet.rs` proves
with `build_spend_full_v4_trace` + `mimc_options()`. `spend_full_v5` is committed but is
**dead code** — referenced only from `examples/`. The wiring exists solely in an orphaned
commit (`033bb594`, "wip: preserve orphaned in-flight work") which is *not* an ancestor of
this branch.

That matters far beyond this document: **the zero-knowledge fix is not in the production
spend path.** Live spends still prove and verify with v4, the circuit that publishes the
recipient key and both output amounts ~85x each in the clear. v5 fixed it, v5 was committed,
and nothing calls v5. So the dispatch work below is not an optional third branch — the
*first* branch is missing too.

| Wallet needs | Circuit | Inputs | Wired today? |
|---|---|---|---|
| Pay from one note | `spend_full_v4` | 1 | ✅ — and it leaks the witness |
| Pay from one note, privately | `spend_full_v5` | 1 | ❌ dead code |
| Merge two notes | `spend_full_v6` | 2 | ❌ built, not wired |

Every spend has **exactly** the number of inputs its circuit declares. No dummies, no
conditional membership, no degree change, no new soundness argument beyond "v6 is v5 with a
second input block and a sum". This is the whole point of the design.

### v6's shape

v5's trace is 33 columns wide: 15 for the input (cols 0–8 Merkle lanes, 9–14 note and
owner-key binding) and 9 per output. v6 adds one more 15-column input block:

```
v5:  [0 bal][1 sub][ input 0 : 13 ] [ out 0 : 9 ] [ out 1 : 9 ]                = 33 columns
v6:  [0 bal][1 sub][ input 0 : 13 ] [ out 0 : 9 ] [ out 1 : 9 ] [ input 1 : 13 ] = 46 columns
```

⚠️ **CORRECTION:** an earlier draft said 48 columns and `48/33 ≈ 1.45×`. The real figure is
**46**, because columns 0 and 1 — the conservation lane and the subtrahend lane — are
*shared*, not duplicated per input. Input 1 adds 13 columns, not 15. As built, input 1 is
appended at columns 33..=45 so v5's layout stays byte-identical and no existing constraint
index moves.

Trace **length is unchanged** — `(depth+1)·64`, doubled for the zero-knowledge mask. That is
the reason to grow width rather than time: FRI depth follows trace length, so a wider trace
costs more trace-LDE and a bigger commitment but does not deepen FRI.

`46/33 ≈ 1.4×` is a first-order expectation for trace-proportional work only, and should be
read as such (noted in review): hashing, composition-polynomial evaluation, memory bandwidth
and commitment overhead do not all scale linearly with width. **Not yet measured** — the
benchmark is on the checklist below and the ratio here is a prediction, not a result.

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

The field bound needs re-deriving, and the first draft got the reason wrong. Conservation is
checked **in the field**, so for field equality to imply integer equality neither *side* may
wrap. Every amount is separately range-constrained to `< 2^RANGE_BITS` by the circuit, so the
bound is set by the side with more terms — not by their total:

```
inputs        : N_INS terms          = 2
outputs + fee : N_OUTS + 1 terms     = 3      ← the binding side
⇒ max(N_INS, N_OUTS + 1) · 2^RANGE_BITS  <  p
```

The earlier draft used `(N_INS + N_OUTS) = 4`, which is *stricter* and therefore still sound,
but for the wrong reason — and a future arity change would have inherited the wrong rule.
Corrected in review. At `RANGE_BITS = 58` and Goldilocks there is abundant room either way.
It is a `const` assertion in the code, not a comment.

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
- **Wallet.** `payment::plan_payment` gains a `ConsolidateThenSpend` arm. See the section
  below — the first draft of this line got the cost badly wrong.

## Planning a payment, and what consolidation actually costs

**Correction (review, 2026-08-28).** An earlier draft said consolidation takes `log₂(n)`
rounds and implied 4–5 transactions for a 20-note wallet. That confuses *depth* with
*count*. Each merge is its own transaction, and each one reduces the note count by exactly
one, so **reducing `n` notes to one costs `n − 1` transactions**, no matter how they are
scheduled. A balanced schedule only shortens the critical path:

```
round 1:  20 → 10     10 transactions
round 2:  10 →  5      5 transactions
round 3:   5 →  3      2 transactions + 1 carried
round 4:   3 →  2      1 transaction  + 1 carried
round 5:   2 →  1      1 transaction
                      ─────────────────
                      19 transactions, depth 5
```

19, not 5. That difference decides whether background consolidation is a quiet convenience
or a fee event the user must consent to, so it belongs in the design, not in the code
review.

**And consolidation is usually the wrong move anyway.** Merging the two smallest notes is
the obvious rule and it is bad. A wallet holding

```
0.1   0.2   0.3   4.9   5.0
```

that wants to send `9.5` needs no consolidation at all — `4.9 + 5.0` is a single v6 spend.
Repeatedly merging the smallest notes would burn several transactions to arrive at the
same place. So the planner asks, in order:

```
1. Does ONE note cover amount + fee?            → v5 spend, done.
2. Do TWO notes cover it?                        → v6 spend, done.
3. Otherwise: pick the consolidation pair that
   minimises expected remaining cost.
4. Repeat only while step 2 still fails.
```

Steps 1 and 2 cover almost every real wallet, and neither costs an extra transaction. Step 3
is the rare path. For small note sets an exhaustive pair search is trivial; for large ones,
sort and use a two-pointer scan.

Note the asymmetry with the single-note rule already shipped in `sigil-shield::payment`,
which picks the *smallest* covering note deliberately — spending the largest would destroy
the only note capable of a big payment. For a pair, the same logic gives "smallest pair that
covers", not "two largest".

## What this does not fix

**The 836,536 notes already in the pool are not cheaply recoverable in bulk.** Precision
matters here (review, 2026-08-28): they are *not* cryptographically stranded. Once v6 exists
any individual holder can recover their own balance through repeated 2-in merges, and that
works. What cannot be done cheaply is compressing the pool as a whole — one merge removes
one note, so clearing 836,536 of them is ~836,535 transactions however they are scheduled,
and a 4-in circuit only changes the constant. The obstacle is economic, not cryptographic.

g1 is a testnet that was reset on 2026-08-26 with zero premine, so there are two honest
options, and both are operator decisions:

1. **Write off** the 18,039 stranded SIGIL. It is testnet value; the chain is two days old.
2. **Migrate**: credit each wallet's shielded holdings back transparently and reset the
   pool. Cheap, one consensus event, and it makes the pool's real anonymity set — which is
   counted in distinct unlinkable owners, not in notes — start from an honest zero rather
   than from 836,536 pieces of attributable padding.

## The refactor this points at

As built, v6's input-1 constraint block is a transcription of input 0's at different column
offsets. That was deliberate — the frame indices differ, a shared helper would take a dozen
of them, and this is consensus code where an index slip is expensive. But it does not scale:
a hypothetical 4-in circuit would be four transcriptions.

The suggestion from review is the right shape, and it keeps the property that makes this
design safe:

```rust
SpendFullAir<const N_INS: usize>
```

with **separate protocol versions at fixed arity**, so the family reads:

```
v5 = SpendFull<1>
v6 = SpendFull<2>
v7 = SpendFull<4>    // only if ever justified
```

That is not "a variable-input circuit". Each shipped version still has a compile-time fixed
input count, so there is still no runtime padding, no dummy notes, no conditional
membership and no degree increase — the whole reason this design works. It only removes the
copy-paste.

Two things make this cheaper than it looks. The column layout is already generated by
`const fn` helpers on the output side (`col_hv(i)`, `col_iox(i)`, …), so the same treatment
on the input side is mechanical. And v5 is currently dead code (see the correction above),
so refactoring it carries none of the risk of touching a live circuit.

Worth doing before v7 is ever contemplated, and not urgent before then.

## Order of work


0. ✅ **DONE** — `spend_full_v6.rs`, 13 tests. A real 2-note merge proves and verifies.
1. **Wire v5 into the production path.** This jumped the queue: the discovery that
   `verify_spend_wire` still calls v4 means live spends publish the witness. Privacy before
   convenience.
2. Wire format + height gate + atomic double-nullifier application — with
   `reject_duplicate_nullifiers` called at state application, not only in the verifier.
3. Dispatch, keyed on a declared version. ⚠️ **never** on trace length.
4. Planner: steps 1–2 of the payment algorithm above (one note, then two) before any
   consolidation logic, since they cover almost every real wallet at no extra transaction.
5. Benchmark the width increase — proving time, peak memory, proof size — and replace the
   predicted `1.4x` with a measured number.

Validation checklist for step 2 onward, from review:

  - two real notes at the same anchor ✅
  - a bad path on either input ✅
  - either ownership key wrong ✅
  - each published nullifier mutated independently ✅
  - `v0 + v1 != o0 + o1 + fee` ✅
  - duplicate nullifiers refused ✅
  - randomised masking leaves public inputs invariant ✅
  - v5/v6 same-depth dispatch — **pending**, needs step 3
  - width/memory/proof-size benchmark — **pending**, step 5

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
