# Code review request — SIGIL shielded transactions, pre-release

*Context: I am the author and operator of this system. SIGIL is my own project, currently a
testnet with no real value at stake, and this is an internal review of my own code before I
consider it trustworthy. The repository is on this machine. Paste this whole file as the
prompt.*

---

## What I am asking for

Review the shielded transaction implementation in `sigilgraph` and tell me where the
reasoning or the code is wrong. This is a correctness review of privacy-critical code I
wrote, not a penetration test of someone else's service.

Treat every claim below as **something I believe**, not something established. Several of
these statements were true last week and are false today, because the code moved underneath
them. At least one is probably still wrong. Finding that one is the most useful thing you
can do.

Rank findings by consequence. The three outcomes I most need to avoid are:

1. a transaction that increases the total supply,
2. a note that can be spent more than once,
3. an observer learning the amount or the parties of a payment.

A subtle weakening with no path to one of those matters less to me than a dull mistake that
reaches one.

Please skip style notes and "consider adding a comment". I want defects, unsound arguments,
and gaps in reasoning — each with the concrete input or state that would expose it.

## Where the code is

```
/home/storage/deepseek-codewhale/sigil        branch hardening/ws-2026-07-18
```

| Crate | What to read |
|---|---|
| `crates/sigil-shield` | the circuits: `spend_full_v4.rs`, `spend_full_v5.rs`, `spend_full_v6.rs`, plus `zk_mask.rs`, `note_v1.rs`, `membership.rs`, `mimc.rs`, `wallet.rs`, `payment.rs` |
| `crates/sigil-state` | `shielded.rs` (pool, nullifier set, epochs, anchors) and the `Shield` / `ShieldedSpend` / `Unshield` arms of `commit_state_transition` in `lib.rs` |
| `crates/sigil-tx` | wire format, the shielded arms of `apply_tx_inner`, and the height gates |
| `crates/sigil-api` | `shielded.rs` — the submission path, including requests assembled by hand rather than by our wallet |

Build and test (please avoid raw `cargo` in this tree — it invalidates the build cache the
project dogfoods):

```bash
export FLUX_WRAPPER_PATH=$HOME/.flux/bin/fluxc
FLUXC=/home/storage/deepseek-codewhale/flux/target/debug/fluxc
$FLUXC test --package sigil-shield --profile release-fast
$FLUXC test --package sigil-shield --profile release-fast -- --ignored   # the proving tests
```

⚠️ **Prove in `release-fast`, not debug.** winterfell 0.9 has a `#[cfg(debug_assertions)]`
`validate_transition_degrees` that trips on this AIR family's witness-dependent range-bit
columns, so most proving tests are `#[ignore]`d in debug. `release-fast` inherits `release`,
so `debug_assertions` stays off. But when a constraint genuinely fails, a **debug** build
names the offending constraint while release only reports
`InconsistentOodConstraintEvaluations`. That asymmetry cost me hours; use debug to diagnose,
release to run.

## The properties the system is supposed to have

1. **Hiding.** A spend proof reveals no output amount, no recipient key, no spend key.
2. **Conservation.** Inputs equal outputs plus fee; no transaction creates value.
3. **Single spend.** A revealed nullifier marks a note spent, permanently, within its epoch.
4. **Real membership.** A spend proves the note is in a tree the chain genuinely held.
5. **Owner binding.** Only the owning key can spend a note, and the key deriving the owner
   proof is the same key deriving the nullifier.
6. **Ramp coarseness.** `Shield` and `Unshield` amounts must be standard denominations, so
   the transparent side of a ramp is not matched to its shielded side by an unusual amount.

## Defects I already know about — please check my fixes actually hold

These were real. Two of them I missed and a reviewer caught. I would rather you spend time
confirming the repairs are sound under unusual input than rediscovering the originals.

- **v4 publishes the witness.** `spend_full_v4` keeps secrets in constant trace columns, so
  the recipient key and both output amounts appear in the proof bytes roughly 85 times each.
  `spend_full_v5` fixes this by reserving the trace's second half for randomness.
  **v4 is still accepted for one-input spends**, deliberately, so wallets already installed
  keep working — see `note_v1::verify_spend_wire_multi`. *My open question: could a party be
  induced to produce a v4 proof, removing the privacy from an exchange they believed was
  private? I have not analysed this.*
- **The circuit accepts one note used as both inputs of a two-input spend.** Both input
  blocks are independently valid, the conservation lane sums twice the value, every
  constraint holds, and the proof verifies. Only the repeated nullifier reveals it, and the
  chain keeps nullifiers in a set. I now reject it in four places outside the circuit.
  *My open question: is there a fifth route in, and is the equality comparison on the right
  representation — could two encodings of one nullifier differ bytewise?*
- **A crash reachable from the network.** winterfell's `Air::new` **asserts** the trace
  width, so a proof of the wrong shape aborted the process rather than being rejected. Now
  checked via `Proof::trace_info().main_trace_width()` before any AIR sees it. *My open
  question: what else in the verify path panics instead of returning an error?*
- **`get_pub_inputs` read a masked row.** In v5 it read `trace.length() - 1`, which after
  padding holds randomness. *My open question: is every published value anchored to a real
  row now, in both v5 and v6?*

## The areas I trust least

**The masking argument.** `zk_mask.rs` reserves `num_queries + margin` random rows and sets
transition exemptions to match. Is the exemption budget right, and are the reserved rows
genuinely sufficient for the real opening count — including the out-of-domain frame and the
DEEP composition openings, not only the FRI query positions? My own claim is that this gives
*computational* hiding, not information-theoretic. I would like that claim checked too.

**Nullifier scoping.** A nullifier binds to a note's **position in the tree**, and the spent
set is scoped **per epoch**, while sealed epoch roots stay valid anchors indefinitely. That
combination is where I would expect a double-spend to hide: can a note be spent in one epoch
and again against a sealed anchor from another? Could someone else's spend permanently
freeze an honest note?

**Anchors.** `epoch_of_anchor` decides which generation a proof is judged against. What
happens with a root the pool held only briefly, or one appearing in two epochs?

**Ramps.** `Shield` debits transparently and mints a note; `Unshield` reverses it. This is
the correlation surface. Please check denomination enforcement, the `value_locked`
accounting, and whether a replayed or reordered ramp can leave the locked total disagreeing
with the notes actually in the pool.

**Fees.** The shielded fee is fixed deliberately, because a chosen fee is a fingerprint. Is
it enforced on every path, including the two-input one?

**Availability.** The pool has fixed capacity per epoch and rotates when full. Can ordinary
traffic force rotation, exhaust a resource, or wedge a node?

## What I do not need

- Every `.unwrap()` in the codebase, unless you can show it is reachable.
- Confirmation that the tests pass. I know they do. It is the weakest evidence here — tests
  written by the person who wrote the bug tend to agree with the bug.
- Diplomacy. Where a doc comment argues at length for its own correctness and is stale, say
  so plainly. Several are.

## How to report

For each finding: **what is wrong**, **the concrete input or state that shows it**, **which
of the six properties it breaks**, and **your confidence**. Please separate what you verified
by running something from what you concluded by reading — explicitly, every time.

If an area looks sound, say so and say what you actually examined. I will read silence about
an area as "not looked at".
