# SIGIL Auth-Gate Client-Signing Migration — Plan (2026-06-10)

Synthesized from 5 read-only Claude Code scoping agents (contract / wallet / miner / scripts / rollout), all verified against the `security/audit-hardening-2026-06-10` branch. Goal: close the live anonymous-mint hole on `sigil-rpcd :8099` (audit C1/C3/C5/H8) by enforcing the per-request ed25519 signature gate **without a broken window**.

## Headline finding — the surface is FAR smaller than the audit warned
The audit note said the gate "breaks every existing client (React wallet, sigil-miner /mine, scripts)." Verified against current code, that's an over-estimate:
- **React wallet** does NOT POST any mutating call to sigil-rpcd today — `gui/sigil-wallet/src/sigil/apiShim.ts` intercepts and **stubs every `/api/v1/*`**; Send/Swap/Mint write to `localStorage`; mining is in-tab WASM (no network). **Enforcement breaks nothing live in the wallet.** The signing work is *forward-looking* (so future real POSTs are signed).
- **sigil-miner / flux-miner** use the **ungated** `/api/v1/mining/submit` (dual-lane, wallet is bound into the BLAKE4+VDF header and re-checked by `check_submission` — cryptographically authenticated, no signature needed). The gated `/mine` (single-lane) has **no Rust client**. **The miner fleet needs NO change and does NOT break.** → Risk flag "miner needs a secret key" **dissolves**.
- **Scripts/services:** exactly **one** caller hits a gated route — `scripts/swarm-money-round.sh` (`/swap`, dev/test placeholder wallets). No systemd/cron unit hits a gated route (status-writer is read-only; the sigil-node producer's block-apply is separate from the RPC gate).

**Net: enforcing the gate breaks (a) the React wallet only if/when it's wired to POST for real, and (b) one dev script.** That makes the rollout low-risk.

## The signing contract (authoritative, from auth.rs + sigil-rpcd.rs)
- Wallet = 32-byte ed25519 pubkey (64-hex on the wire). Sign UTF-8 `sigil-rpc/v1|<action>|<field0>|...|nonce=<req_nonce>` with the wallet secret → 64-byte sig → **128-hex in body field `sig`**; replay nonce in body field **`req_nonce`** (u64, ms timestamp recommended; strictly-increasing per wallet).
- 5 gated routes (action → ordered fields; numbers as **decimal strings**, wallets/pools as **lowercase 64-hex**):
  - `/deploy_token` → `deploy_token` : `[symbol, supply, to]`
  - `/add_liquidity` → `add_liquidity` : `[from, pool, amount_a, amount_b]`
  - `/swap` → `swap` : `[from, pool, dir("AtoB"|"BtoA"), amount_in, min_out]`
  - `/mine` → `mine` : `[miner, header, nonce]`
  - `/credit` → `credit` : `[operator_pool, pool_amount, verifiers(comma-join 64-hex, operator excluded)]`
- Actor wallet is read from each route's own body field (`to`/`from`/`miner`/`operator_pool`).
- **Bypass:** `SIGIL_RPC_NO_AUTH` disables on **presence of the var (any value, even `=0`/empty)** — only fully unsetting it enforces (`.is_ok()`, sigil-rpcd.rs:536).
- **Nonce store IS persistent** (`Snapshot.auth_nonces`, restored on boot) → a clean restart neither opens a replay window nor locks clients out.

## The two changes to write (small)
### 1. React wallet — `gui/sigil-wallet/src/sigil/apiShim.ts:555` (the `window.fetch` interceptor)
- Single chokepoint for all app network writes. Add `signMutation(action, fields[])` next to `jsonRes`.
- Keypair + sign ALREADY exist: `src/services/walletAuth.ts` (`keypairFromMnemonic` = SHA3-256→Ed25519, `@noble/ed25519.sign`, key held in `walletSession.getSession().privateKey`). **No new lib.**
- For mutating method+path: build the canonical string, `ed25519.sign` it, attach `sig`(hex)+`req_nonce`(=`max(Date.now(), last+1)`), and `origFetch` to `${LIVE_NODE}<route>`.
- ⚠️ Whitelist `LIVE_NODE`'s `:8843` so the shim's host:port-stripping (apiShim.ts:544-549) doesn't kill the signed write path.
- Build: **vite on Delta** (node gone from Epsilon PATH), deploy to `dist-final/sigil-wallet/` + `dist-fluxapp/sigil-wallet/` via `flux_ui_deploy`; ship `/sigil_rpc.wasm` + `/sigil_tip.wasm` alongside; visual-verify via playwright `render-check.mjs`.

### 2. `scripts/swarm-money-round.sh` — either bypass or a tiny signer
- It's a dev/test round with placeholder wallets. Simplest: run against a `SIGIL_RPC_NO_AUTH`-set daemon during transition. If gate-clean is wanted: a small Rust/node signer using `sigil_oauth::Keypair::sign(auth_message("swap", &fields, req_nonce))`.

### (No change) sigil-miner / flux-miner — uses ungated PoW-bound `/mining/submit`. Leave as-is.

## Rollout (no broken window)
- **Phase 0 — pre-flight (MANDATORY):** confirm the production `sigil-rpcd` persists state — boot log says `flux-db: RESTORED state @ height …`, NOT `seeding fresh genesis`, and `$SIGIL_STATE_PATH` (`/home/orobit/sigil-data/state`) is a populated dir. If it's in the skill's historical in-memory mode, nonces reset on every restart (replay window) — fix persistence FIRST. Locate the systemd unit (`systemctl list-units '*sigil*'`; the rpcd unit isn't in the tree) and see where/if `SIGIL_RPC_NO_AUTH` is set.
- **Phase 1 — deploy gate WITH bypass:** build the audit branch on Delta (`fluxc build --package sigil-rpc --bin sigil-rpcd`, never raw cargo), hot-swap `.target-shared/debug/sigil-rpcd` (kill by **PID**, never `pkill -f` → self-match exit 144), with `SIGIL_RPC_NO_AUTH=1` in the unit. Ships the code, closes nothing, de-risks the binary swap.
- **Phase 2 — migrate clients (server still bypassed):** ship the wallet apiShim signing; bypass/sign the one script. Server tolerates signed traffic, so roll out gradually with zero coupling to the flip.
- **Phase 3 — ENFORCE:** **unset** `SIGIL_RPC_NO_AUTH` (remove the `Environment=` line — do NOT set `=0`, `.is_ok()` still bypasses) + restart. Watch the daemon's 400 rate for a forgotten client.
- **Rollback (seconds):** re-add `Environment=SIGIL_RPC_NO_AUTH=1` + restart (~2-5s, no rebuild; binary unchanged between modes).
- Per-route gradual enforcement is NOT in the code (one global env var) — don't add it; the bypass window already gives the safe window.

## Verification probes (against :8099 with a wallet you hold the key for)
- Unsigned `/swap` → **HTTP 400** `{"ok":false,"error":"missing 'sig' …"}` (note: 400, not 401/403).
- Signed (`sigil-rpc/v1|swap|<from>|<pool>|AtoB|<amt_in>|<min_out>|nonce=<n>`, ed25519, `sig`+`req_nonce` in body) → **200** `{"ok":true,"amount_out":…}`.
- Replay same `req_nonce` → **400** `{"…","error":"stale/replayed req_nonce …"}`.

## Residual gaps to file (not blockers for this migration)
- `/onboard` still an unlimited faucet (C4); `/nation/pay` + `/nation/eboks` unauthenticated demo mutators — flag for the audit.
- Snapshot persist is a best-effort full-overwrite with no shown fsync → a crash mid-write can roll a nonce watermark back (narrow replay window). Consider WAL/fsync on the nonce store.
- `SIGIL_RPC_NO_AUTH` should arguably enforce on `!= "1"` rather than presence — the current `.is_ok()` is a footgun (`=0` bypasses).
