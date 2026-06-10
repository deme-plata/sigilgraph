# SIGIL 0.36.1 — "Iron Gate" release plan

Planned 2026-06-10 by Claude Code agents (2 parallel codebase inventories, every item
verified at file:line) + DeepSeek-V4 (independent release plan). sigil-top is at 0.35.0 →
**0.36.1**. Theme: *make the network safe to actually deploy* — nothing is live on the
public `:8099` yet, deliberately.

## Key correction from the agent inventory
The feared "auth gate breaks all clients" is **far smaller than the audit assumed**:
- **flux-miner** uses `/mining/submit` (PoW-gated, NOT auth-gated) → unaffected.
- **React wallet** posts to `/v1/dex/swap` etc. → after the `/api/v1` strip becomes `/dex/swap`,
  which is *not* a gated flat route → unaffected today.
- Legacy `/mine` (now gated) has **no source caller**.
- **Only real breakage:** `scripts/swarm-money-round.sh:73` (curls `/swap` unsigned).

So the deploy-gate is just: (a) sign that one script, (b) close `/onboard`.

## MUST (deploy is blocked until these ship)
| # | Item | Where | Effort |
|---|---|---|---|
| 1 | **C4 — `/onboard` finite faucet + per-IP rate-limit** (only remaining [LIVE] anon mint). Thread peer-IP through `route()` (handle() has `stream.peer_addr()`; honor `X-Forwarded-For` behind q-flux). Debit a fixed faucet wallet, not unconditional mint. Decouple `persist()` from the request. | `sigil-rpc/src/bin/sigil-rpcd.rs:768-802`, `handle()` :1062 | M |
| 2 | **Client signing for `swarm-money-round.sh`** + document the signing contract. Tiny: derive key from trader seed → sign `auth_message` → attach `sig`+`req_nonce`. | `scripts/swarm-money-round.sh:73`, `crates/sigil-rpc/src/auth.rs` (contract) | S |

## SHOULD (high value, not deploy-blocking — independent lanes)
| # | Item | Where | Effort |
|---|---|---|---|
| 3 | **v0.35 sync #1 — move `sync_aether_to_fluxdb` INTO the sync worker** (the last + biggest sync-earlier win; ingest is still blocking the boot thread). New `launch_with_aether(store, recent_only, aether_dir)`: run ingest after `net.start().await`, before the loop (store already owned by the worker → no oneshot). Convert its `eprintln!`→`tlog!` (TUI safety). ⚠️ sigil-top files owned by parallel `rocky-janitor` — coordinate. | `sigil-top/src/{main.rs:2184, block_sync.rs:381}` | M |
| 4 | **Bridge BOLT11 binding** — LN deposit `amount_msat` is still self-declared; parse + verify the signed BOLT11 to bind amount+payee before any custody. | `sigil-bridge/src/ln.rs:42-55`, `lib.rs process_ln_deposit` | M |
| 5 | **Cleanup sweep** — `checked_add` on `/add_liquidity` reserves (rpcd:756-757); execute_swap master-fee unchecked `+/-` (sigil-rpc/lib.rs:147); SwapDelta k-invariant at the chokepoint (sigil-state:485); treasury unchecked `+=`. | several | S |
| 6 | **Foundation: wire `sigil-node/src/store.rs` (flux-db) into the runtime** (it exists but `mod store` is absent from main.rs — node still uses the RS-aether snapshot; SigilState in-memory) + add `sigil-node get-block <height>` CLI. | `sigil-node/src/main.rs:13,106-123` | M/L |

## CUT-LINE — explicitly NOT in 0.36.1
| Item | Why deferred (→ 0.37 / consensus track) |
|---|---|
| **H1 — consensus apply-path crypto** (verify PoW/VDF/producer-sig; `producer_sig` is zeros) | Touches sigil-header + sigil-node/chain.rs + sigil-chronos + difficulty re-derivation + nonce store + height-gated activation. Too large/risky for a point release. |
| **C10 — VDF `bench_2048` known-modulus** (forgeable time-lane; live in `/mining/submit` via `check_submission`, flux-vdf:179) | Needs a class group (no setup) or a real RSA ceremony + a height-gated wire-format change every miner adopts together. Pairs with H1. *(Practically: factoring a 2048-bit modulus is hard, but the security claim is void.)* |
| **/nation/pay + /nation/eboks** ungated (hardcoded CITIZEN, no key) | Demo, ~1000 NATIVE blast radius. Quick win if wanted: feature-gate behind `SIGIL_NATION_DEMO`. |

## Parallel agent lanes (disjoint files — no merge collisions)
- **Lane A** (deploy-gate): #1 onboard faucet + #2 script signing → `sigil-rpcd.rs` + `scripts/`. Owner: fresh agent.
- **Lane B** (sync): #3 aether-into-worker → `sigil-top/*`. **Coordinate with `rocky-janitor`** (holds those claims).
- **Lane C** (bridge): #4 BOLT11 → `sigil-bridge/ln.rs`. Independent.
- **Lane D** (hygiene): #5 cleanup sweep → scattered handlers. Independent.
- #6 (foundation) is its own lane, lower priority.

Deploy gate = Lane A done + smoke-tested (signed onboard → mine → swap end-to-end), then bump
sigil-top → 0.36.1, tag rc1, smoke-test, tag 0.36.1, deploy, remove `SIGIL_RPC_NO_AUTH`.

Inputs: SECURITY_AUDIT_2026-06-10.md, SYNC_EARLIER_v0.35.md. Two-mind: Claude agents × DeepSeek-V4.
