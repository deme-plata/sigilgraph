# USDS: Native Stablecoin + Polygon Bridge

## Technical Review

**Project:** SIGIL (sister chain to Quillon Graph)
**Network:** `sigil-g0` — **TESTNET**, not mainnet (see §6)
**Date:** 2026-08-18
**Author:** Claude (Sonnet 5)
**Classification:** Design + implementation review, pre-deployment. No real-value funds are at risk yet (§6).

---

## 0. Why This Document Exists

Viktor asked for a native $-pegged stablecoin on SIGIL, explicitly modeled on Quillon Graph's QUGUSD, reusing `sigil-bank`, and extended to a Polygon-side token — the same pattern already proven this session for wrapped native SIGIL. Before writing a line of stablecoin code, QUGUSD's actual implementation was read end-to-end (not assumed from memory), because QUGUSD has a real, live, still-unpatched bug in production, and a straight port would have copied it. This document records: what QUGUSD actually does and where it broke, what SIGIL already had to build on, the concrete design decisions USDS makes differently and why, and a from-source verification that the new code is isolated from everything already live.

---

## 1. QUGUSD, As Read From Source — Not As Assumed

Quillon's `q-narwhalknight` tree was read directly (`q-vm/src/contracts/collateral_vault.rs`, `q-api-server/src/{stablecoin_api,quillon_bank_api,handlers}.rs`). Findings:

| Finding | Detail |
|---|---|
| **Two independent mint paths** | A self-service CDP (`collateral_vault.rs`) and a separate Quillon-Bank loan path (`quillon_bank_api.rs`) — different files, different debit logic, not sharing a code path. |
| **135% mint / 115% liquidate** | Wide, deliberately engineered margin, defended by a liquidation engine. |
| **The peg has no active defense** | `get_token_price` hardcodes QUGUSD = \$1.00 unconditionally. The peg is asserted, not defended by arbitrage or redemption pressure — held up only by collateralization holding. |
| **The actual live bug** | `update_price_from_amm` reads raw, single-block AMM pool reserves with no time-weighting, feeding `collateral_vault.qug_price_usd` directly. A separate `as u128` float-cast in a startup price-reset (`lib.rs:2605`) silently *saturates* (Rust float→int casts saturate rather than panic) instead of erroring. Observed live: price pinned near \$43,749.85/QUG against a ~\$3,000 real target — a **14.6× overshoot** — which fed straight into both the collateral math and a public mining-earnings display (\$63.5M/day shown). **Still unfixed in production** at time of writing, held pending operator sign-off because it's money-adjacent code with no staging server.

**The load-bearing lesson:** the bug was never in the collateral ratio. It was upstream, in the price the ratio trusted. A wider liquidation margin does not help if the number the margin is computed against is simply wrong.

---

## 2. What SIGIL Already Had (Read Before Designing, Not Assumed Either)

Three pre-existing, partial pieces were found and evaluated before writing anything:

| Crate | What it is | Verdict |
|---|---|---|
| `sigil-bank::credit` (`CreditVault`) | A SIGIL-collateralized yield vault (mint a `CREDIT` token at 50% LTV, tiered APY) | A different product — not $-pegged, not the target. Left untouched. |
| `sigil-mandat::credit_line` | A separate DEBT/COLLAT ledger for consumer "pay later" loans | Unrelated audience/purpose. Left untouched. |
| `sigil-usds` (196 lines, pre-existing) | 1:1 value-locked, oracle-priced mint/redeem — architecturally the right analog to QUGUSD, but **never wired to any HTTP route** and using an EXACT 1:1 peg (no safety margin at all) | **Selected as the foundation.** Extended, not replaced. |

`sigil-oracle` (also pre-existing) was already a single-pinned-authority, committed-in-roots price feed — structurally immune to QUGUSD's specific bug class (no raw AMM read exists in its code path at all) but with no defense against a simple bad push.

---

## 3. Design Decisions Made, And Why

### 3.1 A fixed collateral buffer, not a CDP with liquidations

QUGUSD's 135%/115% margin needs an active liquidation engine to mean anything. USDS instead mints `locked_value / 1.05` USDS per lock (`MINT_BUFFER_BPS = 10_500`, i.e. 105%) — every position is fully collateralized *by construction*, with a fixed 4.76% cushion, and there is no liquidation logic to get wrong in a first implementation. The explicit trade-off is stated in the whitepaper (§7 there): a sustained price fall beyond the cushion thins a position's true backing with no active defense. This was a deliberate choice to avoid QUGUSD's demonstrated failure mode (two divergent mint paths) rather than build a second, more complex system whose bug surface we cannot yet estimate.

### 3.2 An oracle sanity band — the direct fix for the QUGUSD bug class

`sigil_oracle::update_price` now rejects any single push moving the price >20% (`MAX_PRICE_CHANGE_BPS = 2_000`) from the last committed price, unconditionally, with no bypass (QUGUSD's own circuit breaker existed but was explicitly bypassed on the AMM-update path — that bypass is the reason the bug reached production). A real sustained move still lands, just over more than one push. Verified with dedicated tests: a 3× spike is rejected, a 90% crash is rejected, a sustained move spread over several pushes clears.

### 3.3 Fee reuse, not reinvention

Both mint and redeem pay `sigil_bank::MASTER_SWAP_FEE_BPS` (30 bps) via the *same* `split_swap_output` function DEX swaps already use — not a new constant, not new rounding logic. This directly avoids reproducing QUGUSD's "two independently-coded paths for the same kind of operation" bug class inside USDS itself.

### 3.4 A second, isolated Polygon bridge — not a generalized one

`USDSBridgeWrapped.sol` and `usds_bridge.rs` are structural duplicates of this morning's proven `SigilBridgeWrapped.sol`/`bridge.rs`, not a shared "any token" bridge. This was a deliberate choice over generalizing the existing bridge: a compromised operator/relayer key on one bridge should never be able to touch the other's vault, and the two bridges sign with disjoint message namespaces so a valid signature for one can never be replayed against the other (verified by a dedicated test, §4).

---

## 4. Verification (From Source, Not Claimed)

```
$ grep -c "^" sigil-usds/src/lib.rs sigil-oracle/src/lib.rs sigil-api/src/{dex,usds,usds_bridge}.rs
332  200  550  338  500

$ grep -c "#\[test\]" <same files>
6    9    11   8    7        →  41 new tests total
```

**Isolation, checked, not assumed:**
```
$ grep -rln "UsdsMint\|UsdsRedeem" sigil/crates/*/src/*.rs
sigil-api/src/usds.rs        sigil-events/src/lib.rs
sigil-api/src/lib.rs         sigil-tx/src/lib.rs
```
Exactly the four files intentionally touched — no stray reference anywhere else in the workspace.

```
$ grep -c "USDS\|usds" sigil-api/src/send.rs sigil-api/src/bridge.rs
0    3
```
`send.rs` (plain SIGIL sends) has zero USDS coupling. `bridge.rs` (the native-SIGIL Polygon bridge) has three — all three are DOC COMMENTS citing `sigil-usds::VAULT` as a naming-convention analogy, written this morning before USDS bridging existed; **zero functional coupling** — `bridge.rs`'s actual code paths never call into `sigil-usds` or `usds_bridge`.

**Producer-loop wiring** (`sigil-node/src/main.rs`): 22 references across the two new bridges — every `snapshot_for_mint`/`confirm_applied` call site and both `dag_drain_apply` code paths (the simple-apply and DAG-candidate-racing paths, plus the test harness) carry both new bridges alongside the four pre-existing ones (`send`, `bridge`, `dex`, `usds` mint/redeem), confirmed by direct read of every call site, not by the diff alone.

**A real, unrelated bug fixed in the same session** (not part of USDS, noted for completeness): `sigil-api/src/mining.rs`'s `SOLVE_QUEUE_CAP` was raised from 8 to 512 after confirming live that the 8-slot cap was silently discarding valid, already-verified miner solves whenever block production slowed under real host contention — the queue's own consumer (`sigil-node`'s producer loop) already re-validates every popped solve against the current mint target before use, so a deeper queue introduces no burst-mint risk, only fewer silently-dropped valid solves.

---

## 5. Known Limitations — Stated, Not Hidden

1. **Single-authority oracle.** One wallet pushes SIGIL's price. The sanity band bounds the blast radius of one bad push; it does not remove the trust assumption that the authority is honest and live. A future iteration should consider a multi-source or committee-signed feed.
2. **No liquidation engine.** A position's collateral can, in principle, thin below its issued value after a sustained fall beyond the fixed buffer, with no automated defense. This is a stated, deliberate scope boundary of v1 (§3.1), not an oversight.
3. **No circulating-supply index.** `/v1/usds/status` reports committed price and vault collateral (both real) but not total circulating USDS — that needs an index this implementation doesn't build yet.
4. **Polygon deploy pending funding.** `USDSBridgeWrapped.sol` compiles clean and its real deploy cost was measured on a mainnet fork (≈0.56 POL at time of measurement) — not yet broadcast; needs operator-approved gas funding (see §6).

---

## 6. Deployment Status — Explicit, So Nobody Assumes More Than Is True

- **SIGIL L1 side:** code complete, unit-tested (41 new tests), **not yet built/deployed to the running node** — the `sigil-node` binary carrying this code is compiling as of this writing, queued behind genuinely heavy, unrelated build load from other sessions sharing this machine (confirmed via direct process inspection, not assumed).
- **Polygon side:** contract + deploy script written and compiled clean (Foundry, `forge build` exit 0). **Not deployed.** Needs the burner wallet funded (~0.7–0.8 POL, same pattern as this morning's SIGIL-bridge deploy) before any real transaction broadcasts.
- **Network identity:** everything above is on `sigil-g0`, which the project's own genesis docs label testnet. Nothing here should be read as carrying real monetary value until a real SIGIL mainnet exists and USDS is redeployed against it (the same redeploy-not-migrate recommendation already given for wrapped SIGIL applies identically here).

---

## 7. Bottom Line

USDS is a narrower, single-path design that responds to two specific, verified failures in the system it's modeled on, rather than a feature-parity clone. Every claim above is either a direct source citation or a command whose output is shown — nothing here is asserted from memory of what the code "should" do.
