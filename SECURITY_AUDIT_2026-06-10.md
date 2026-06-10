# SIGIL Graph — Security Audit (2026-06-10)

Auditor: rocky (Claude Opus 4.8). Method: 6 parallel domain audits reading the real
source under `sigil/crates/`, top findings re-verified by hand against the code.

## Threat-model context (READ FIRST)
- **`sigil-rpcd` (`:8099`) is PUBLICLY EXPOSED** (`sigilgraph.quillon.xyz`, firewall ACCEPTs :8099).
  Every finding tagged **[LIVE]** is exploitable *right now* by an anonymous `curl`.
- The `sigil-node`/`sigil-chronos` block-apply path is **Phase-0 by design** ("Phase 0 omits
  crypto verification" — `chain.rs:84`). Findings tagged **[P0-DEFERRED]** are documented
  deferrals, but they mean **sigil-g0 balances are not trust-bearing today**.
- **Good news up front:** the prior red-team precompute/issuance-capture bug is **FIXED**
  (challenge seed is now tip-bound + VDF-chained, `sigil-rpcd.rs:309` + `fold_tip:119`).
  `sigil-dex` math, `sigil-emission` schedule, `sigil-oauth`, ed25519 `verify_strict`, and
  `flux_sqisign::verify` are all correctly built. Sync-down chain-wipe is structurally prevented.

---

## CRITICAL

| # | Title | Where | Status |
|---|---|---|---|
| C1 | **Unauthenticated full-supply mint** — `/mine` takes caller-supplied `difficulty` (default 0 → all PoW passes) and `reward`. One POST mints up to the 21M cap to any wallet. | `sigil-rpcd.rs:777-793` (verified) | **[LIVE]** |
| C2 | **No auth on ANY money route.** mint/swap/add_liquidity/onboard/credit/deploy_token/nation-pay all anonymous. `sigil-oauth` is built but **never wired into the daemon**. | `sigil-rpcd.rs:509-934`; oauth unused | **[LIVE]** |
| C3 | **Spend anyone's wallet** — `/swap` & `/add_liquidity` take `from` as a plain body field, no owner signature. Drains any funded wallet / manipulates pools. | `sigil-rpcd.rs:678-762` | **[LIVE]** |
| C4 | **Unlimited faucet** — `/onboard {}` mints +100 NATIVE +1000 USDS per call, no rate-limit/identity; also a full-state `persist()` per call → DoS amplifier. | `sigil-rpcd.rs:706-739` | **[LIVE]** |
| C5 | **Arbitrary token mint** — `/deploy_token` mints any supply of any non-NATIVE token to any wallet; non-NATIVE has no cap. Mint → LP → drain paired real asset. | `sigil-rpcd.rs:664-676`; cap is NATIVE-only `sigil-state:772` | **[LIVE]** |
| C6 | **Updater RCE — no key pinning.** `verify_announcement` checks the release sig against `a.sqisign_pubkey` — a field of the *attacker-controlled* announcement. No trusted-key allowlist anywhere. Anyone on `/sigil/g0/release` pushes a self-signed malicious binary → fleet-wide RCE. | `sigil-updater/verify.rs:33` (verified) | **[LIVE if release topic subscribed]** |
| C7 | **Self-transfer mints `amount`.** `Send` with `from==to` (native): recipient-credit `SetBalance{from, NATIVE, from_native+amount}` reads pre-state and overwrites the sender debit. Net gain = `amount` per tx. (The same aliasing class was fixed for LP, not for Send.) | `sigil-tx/lib.rs:837-891` (verified) | exploitable via producer/mempool |
| C8 | **`native_supply` uses `wrapping_sub/wrapping_add`** → the once-at-end 21M cap check compares `Σbalances mod 2^128`; crafted writes wrap past the cap while real supply ≫ 21M. | `sigil-state/lib.rs:277` | latent w/ C1/C7 |
| C9 | **Bridge: mint amount/recipient unbound from the proven tx.** `process_deposit(amount, recipient, proof)` — SPV proves *some* tx exists, but `amount`/`recipient` are caller-chosen → unlimited free wBTC/wETH. Plus **no replay/spent-set** (reuse one proof N×) and **withdrawal needs no proof/owner sig at all**. | `sigil-bridge/lib.rs:66-101` | bridge must not custody value |
| C10 | **VDF over a hardcoded known-structure modulus** (`bench_2048`, code comment: "NOT a secure RSA modulus"). Wesolowski needs unknown group order; this N is forgeable → time-lane collapses, instant-mine. | `flux-vdf/lib.rs:179`, used `sigil-rpcd.rs:181/846` | issuance capture |

## HIGH
| # | Title | Where |
|---|---|---|
| H1 | **Block-apply verifies no PoW/VDF/producer-sig.** `precheck()` checks byte-*lengths* only; `producer_sig` is `vec![0u8;292]`. Any peer injects accepted blocks (`sigil-node` + `sigil-chronos`). **[P0-DEFERRED]** but live. | `sigil-header:270-305`, `chain.rs:86`, `chronos lib.rs:333` |
| H2 | **Handshake "verify-before-sync" is a length-only stub** (`identity_sig.len()==32`), and isn't called anywhere → no peer authentication on the sync path. | `sigil-handshake/handshake.rs:142-168` |
| H3 | **Follower trusts peer-declared difficulty (`bits`/`vdf_t`).** Malicious producer serves `bits=4` → follower verifies the cheap share & credits full reward. No independent retarget re-derivation. | `sigil-rpcd.rs:150,172` |
| H4 | **Tip-proofs verified only via keyless `Blake3Fingerprint`** (crate's own doc: "any peer can fabricate one"). `verify_sqisign` exists but is never used on the wire. | `sigil-node/main.rs:1089`, `sigil-tip-proof` |
| H5 | **No nonce/replay protection at state or block layer.** `SignedTx.nonce` is never checked and isn't in the signed digest; mempool dedup is in-RAM only → cross-block replay. | `sigil-tx` (no nonce store) |
| H6 | **`event_log_root` is an order-blind accumulator, but inclusion proofs verify against a binary Merkle root** → proofs don't validate, and event ordering isn't committed. | `sigil-state:201` vs `sigil-events:295-372` |
| H7 | **Bridge SPV difficulty self-declared & un-anchored** + Bitcoin-PoW rules applied to ETH/ZEC/IRON. Mine 6 regtest headers in µs → pass `verify(6)`. | `sigil-bridge/proof.rs:119-152` |
| H8 | **`/credit` redistributes any pool** — caller supplies `operator_pool`+verifiers, no owner auth, no idempotency (doc admits double-credit). | `sigil-rpcd.rs:873-889` |

## MEDIUM / LOW (abridged — full detail in agent notes)
- M: `execute_swap` master-fee credit uses unchecked `+/-` (panic/mis-credit) — `sigil-rpc/lib.rs:147`.
- M: chokepoint `SwapDelta` does NOT check the k-invariant despite its docstring — `sigil-state:485`.
- M: USDS mint/redeem has no collateral ratio / price-staleness guard; single-key oracle — `sigil-usds`.
- M: `/add_liquidity` unchecked `+` on reserves (`sigil-rpcd.rs:694`); `sigil-treasury` unchecked `+=`.
- M: updater `curl` fetch has no https/host allowlist (SSRF-ish, gated by hash) — `transport.rs:82`.
- M: CORS `*` on every response (compounds no-auth) — `sigil-rpcd.rs:502`.
- L: missing release/tip-proof domain-separation tags; `credit_share` drops `operator_share`;
  `sigil-fees` uses `f64` in a consensus-charged value (cross-arch divergence risk).
- L: `sigil-btc-miner` is a local benchmark only (no credit path) — no bug.

## What is SOLID (verified, no action)
- Precompute/issuance bug **fixed** (tip-bound VDF-chained seed). BLAKE4 R<7 properly quarantined
  behind cfg-gates; consensus always re-hashes at R=7. VDF *verifier* is real (weakness is the group).
- `sigil-dex`: checked math, explicit k-invariant, MIN_RESERVE, slippage floor, zero-liq rejected.
- `sigil-emission`: pure fn, compile-time cap assert, no halving off-by-one.
- `sigil-oauth`: real OAuth2/PKCE-S256, Ed25519/SQIsign tokens, epoch revoke, DPoP, fails-closed on
  unknown alg (no `alg:none`). **Just not wired into sigil-rpcd.** ← the fix is integration, not code.
- Crypto primitives: `flux_sqisign::verify` + ed25519 `verify_strict` correct; no return-true stubs,
  no committed secret/backdoor keys. Sync-down chain-wipe structurally impossible (height-monotonic apply).

## FIXES APPLIED (rocky, 2026-06-10) — verified compiling + tested
- **C6** ✅ updater key-pinning — `verify_announcement_pinned` + `handle_release_message(trusted_keys)`; sigil-node loads a pinned const + `SIGIL_TRUSTED_RELEASE_KEYS` (fails closed, empty = auto-update off). updater 6/6 tests.
- **C7** ✅ self-transfer mint — `Send` handler collapses `from==to` to a net mutation; sigil-tx self_send tests 2/2.
- **C8** ✅ supply-cap wrapping — `native_supply` now `saturating_*` (can only pin high → cap check always catches). sigil-state green.
- **C1/C3/C5/H8** ✅ **auth gate wired** on the five theft routes via new `sigil_rpc::auth` (ed25519 wallet-sig over a domain-separated, `req_nonce`-replay-guarded canonical message; per-wallet nonce store persisted in the snapshot):
  - `/mine` — drops caller `difficulty`/`reward` (server-derived from `block_reward`+`n.bits`) + requires miner sig. Kills the one-POST 21M mint.
  - `/swap`, `/add_liquidity` — require `from` sig (no more spending others' wallets).
  - `/deploy_token` — requires creator (`to`) sig.
  - `/credit` — requires `operator_pool` owner sig + nonce (closes the double-credit replay).
  - sigil-rpc lib 42/42, `sigil-rpcd` bin compiles clean. `SIGIL_RPC_NO_AUTH=1` = migration bypass.
- **⚠️ RESIDUAL (not yet done):**
  - **C4 `/onboard`** — still mints +100 NATIVE/+1000 USDS per call. Cold-start can't pre-auth; real fix = finite faucet-debit + per-IP rate-limit (separate change).
  - **`/nation/pay` + `/nation/eboks`** — demo routes on the hardcoded CITIZEN wallet (no key to sign with); left ungated, low value.
  - **C9 bridge** — not started.
  - **Client impact:** the gate BREAKS existing clients (React wallet, sigil-miner `/mine`, scripts) until they sign requests. Signing contract is documented in `sigil-rpc/src/auth.rs`. Use `SIGIL_RPC_NO_AUTH=1` during migration.
  - **✅ DEPLOYED 2026-06-10 (enforcing) on prod sigil-rpcd :8099.** A 5-agent scoping pass (see `docs/AUTH_GATE_MIGRATION_PLAN.md`) verified the real client surface is tiny: the React wallet doesn't POST mutating routes (apiShim stubs them), miners use the **ungated** PoW-bound `/mining/submit` (not `/mine`), only `swarm-money-round.sh` (dev) hits a gated route — so the gate was deployed **enforcing** (no bypass) and broke nothing live. Verified: unsigned `/deploy_token|/mine|/credit|/swap` → `missing 'sig'`; reads + miner unaffected. Client-signing (apiShim.ts:555 + the one script) remains forward-looking, not enforcement-blocking. Rollback: `sigil-rpcd.PREGATE.bak` or `SIGIL_RPC_NO_AUTH=1`.

## Recommended fix order
1. **C1** — delete/neuter caller `difficulty`+`reward` on `/mine` (derive reward from `block_reward`). One-liner, stops the live full-supply mint.
2. **C2/C3** — wire `sigil-oauth::verify_token` + a per-request wallet signature over `from`/`miner` as a mandatory gate on every POST in `route()`. The library already exists.
3. **C6** — pin trusted release keys as a `const` in sigil-node; reject announcements whose pubkey isn't allowlisted *before* verifying.
4. **C7** — collapse `from==to` Send to a single net mutation (mirror the LP fix).
5. **C8** — `checked_add/checked_sub` on `native_supply`, fail commit on overflow.
6. **C4/C5/H8** — auth+cap onboard/deploy_token/credit; finite faucet, rate-limit, decouple `persist()`.
7. **C9/H7** — bridge must bind amount+recipient to the parsed proof, add a spent-set, require signed
   withdrawals, anchor SPV difficulty + per-asset verification — *before* it custodies any value.
8. **C10** — replace `bench_2048` with a class group / ceremony RSA modulus in the production path.
9. **H1/H2/H3/H4/H5** — the P1 consensus-crypto wiring: verify_dual + producer-sig + chain-derived
   difficulty + nonce store in the apply path; real handshake; SQIsign tip-proofs with a pinned key.
