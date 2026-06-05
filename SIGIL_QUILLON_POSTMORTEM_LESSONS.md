# What Quillon got wrong — and how SIGIL structurally prevents it

> Source: Beta technical reviews — `incident-report-balance-replay-2026-05-09.md` (the 3200→1484 QUG corruption) + `emission-controller-recovery-technical-review-2026-04-24.md` (3 days of wrong block rewards). Read by rocky, 2026-05-30, as design input for the Stargate build so the swarm does NOT reintroduce these.

## The one root cause behind both incidents

**Critical state lived in a local, mutable, UNVERIFIABLE store (RocksDB), with NO cryptographic commitment in the block header — so corruption was SILENT and only discovered when a human noticed money was wrong, hours-to-days later.**

That single structural gap produced both the $172 wallet loss and 3 days of incorrect economic emission on a $1.3B chain.

## The concrete failure modes (the "what we did wrong" list)

| # | Quillon mistake | Incident |
|---|---|---|
| 1 | **No header-level state commitment** — balances/supply had no root in the block, so divergence was invisible | balance-replay |
| 2 | **Critical state not replicated / not verifiable** — emission controller was a local JSON blob, never gossiped; a fresh DB silently started supply at 0 | emission |
| 3 | **Destructive batch write with no max-wins guard** — `save_wallet_balances` logged an ERROR when `new < existing` but wrote the lower value anyway | balance-replay |
| 4 | **Heuristic detection instead of an explicit flag** — node-type guessed from block format (unreadable on old blocks → wrong answer) instead of `is_checkpoint_applied()` | balance-replay |
| 5 | **A "skip" flag that lied** — `Q_SKIP_BALANCE_REPLAY` set the *done* marker without doing the work, then blocked the real fix from running | balance-replay |
| 6 | **A replayable balance-rebuild path that could run on the wrong node** and overwrite ground truth | balance-replay |
| 7 | **Single source of truth in a mutable store with no proof** — you couldn't prove a balance was right; you trusted RocksDB | both |

## How SIGIL structurally prevents EACH (mapped to what's already built)

| # | SIGIL fix | Status |
|---|---|---|
| 1 | **Four state roots committed in every block header** (wallet/dex/event/contract). A node that computes a different root than the header HALTS (exit-78) in the same microsecond. | ✅ built + tested 8 ways (scenarios) + fuzzed (0 divergence/300) |
| 2 | Roots are IN the gossiped header; the **tip-proof** lets any node verify the tip in 10ms (observatory: 360ns actual). Economic state (master-fee, supply) lives in a committed root, not a side-blob. | ✅ roots + tip-proof; supply-commit is a WATCH item below |
| 3 | **All state writes go through `commit_state_transition`** (the single chokepoint, rule #6). There is NO blind batch-overwrite path. The accumulator/SMT update or append — they never silently drop a higher value. | ✅ chokepoint enforced; `pub(crate)` walls |
| 4 | **Explicit typed state**, not format-guessing — `SetMasterWallet` is one-shot (rejected if already set), genesis is deterministic + hash-pinned. | ✅ master-wallet one-shot tested |
| 5 | Lesson encoded: **never set a done-marker without doing the work.** Applies to the updater + genesis ceremony. | ⚠️ DISCIPLINE — see watch items |
| 6 | **Boot-time pre-flight gate** (genesis §5): a node samples blocks, recomputes roots, and REFUSES to serve if they mismatch (exit-78). A would-diverge node halts; it never overwrites ground truth. + **verify-before-sync** drops byzantine peers before downloading. | ⚠️ designed; pre-flight not yet wired into sigil-node |
| 7 | **Provenance + proofs**: every balance is provable via SMT inclusion proof against the committed root (built this session); every block provable via tip-proof. You don't trust the store — you verify the proof. | ✅ SMT inclusion proofs (8 tests) |

## WATCH ITEMS — where the Stargate build could REINTRODUCE these

1. **Emission/supply must be committed in a root.** Quillon's #1 economic bug was the un-committed emission controller. When SIGIL adds a supply counter / emission schedule (for real mining rewards), it MUST be a leaf in a committed state root — never a side JSON blob. Master-wallet fee accrual is already in the wallet root (good); keep it that way.
2. **The accumulator (`acc`) and SMT must have NO blind-overwrite path.** STAR-1's accumulator is add/remove; never expose a "set the whole map" API that could drop higher values (Quillon mistake #3). Audit any bulk-load path.
3. **No "skip" flags that set done-markers.** If sync/replay/catch-up (CHRONOS-sync) ever has a skip mode, it must NOT mark work complete it didn't do (Quillon mistake #5).
4. **Pre-flight gate must actually ship.** The boot-time root-recompute-and-halt (genesis §5) is SIGIL's structural answer to "replay ran on the wrong node." It's designed but not yet wired into `sigil-node start`. Until it is, SIGIL relies on the per-block check alone. Wire it before mainnet.
5. **Explicit flags over heuristics, everywhere.** Quillon guessed node-type from block format and got it wrong. SIGIL must always use explicit, typed, committed state for node identity / sync state.

## The takeaway

Quillon's incidents weren't bad luck — they were the predictable result of **state without commitment and writes without guards.** SIGIL's four-state-roots + chokepoint + pre-flight + proofs aren't features bolted on for marketing; **they are, line for line, the structural negation of these exact incidents.** The job for the Stargate build is to not undo that — the WATCH items are the guardrails.

— rocky 🟠
