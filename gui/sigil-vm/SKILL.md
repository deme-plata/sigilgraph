---
name: sigil-vm
description: Build, polish and WIRE SIGIL VM — the OpenSea-shaped marketplace + Quillon-shaped swap module for the SIGIL chain at sigilgraph.org/sigil-vm.html (source sigil gui/sigil-vm, Vite+TS, no framework) — and the Flux-native market engine behind it (sigil-vm-market crate, /v1/vm/snapshot, pools, listings, metadata). Use for any request touching the VM page, its showcase frames, its ship gate (ship.sh / guard.mjs / snapshot.mjs), the one-month wiring plan (engine → liquidity → listings → metadata), or "make it fit the real flux way". Never invent a number: every field is LIVE / DERIVED / PRETEND.
---

# sigil-vm — the marketplace that only shows what the chain can prove

**The one-line contract (Viktor, 2026-09-16, paying for it):** a design people already love — OpenSea's grammar,
Quillon Graph's swap module — filled ONLY with records the SIGIL chain actually keeps, every figure labelled
LIVE / DERIVED / PRETEND, shipped through the Flux Vite engine gate. It made him cry with joy the first time he
saw it. Keep that recipe; the polish loop that followed (150+ ticks) never once added a fake number.

## Where everything is

| what | where |
|---|---|
| page | `https://sigilgraph.org/sigil-vm.html` · framed: `/sigil-vm-showcase.html` (`?frame=multiverse|neon`) |
| source | `/home/storage/deepseek-codewhale/sigil/gui/sigil-vm/` — `src/{api,model,ui,main,art,icons,showcase}.ts`, `style.css`, two entries (`sigil-vm.html` deploy · `index.html` verify/dev → points at `127.0.0.1:8459`) |
| gate | `ship.sh "<msg>"` = tsc → `fluxc build --frontend-only` → flux-vite-engine `examples/verify` → `guard.mjs` (5 widths × 2 themes, live node) → `snapshot.mjs` (no-JS summary baked into the HTML) → copy to `/home/orobit/sigilgraph-org-site/` → HTTPS 200 check → commit + push `hardening/ws-2026-07-18`. **All-or-nothing. Never `cp` the page by hand, never `npm`.** |
| inventory | `gui/sigil-vm/INVENTORY.md` — every element marked live / derived / mock + the 4-week plan |
| frames | sibling skill `sigil-vm-frames` (measure.py, frames.md) |
| memories | `project_sigil_vm_marketplace_page_2026_09_15`, `feedback_sigil_vm_first_sight_joy_2026_09_16` |
| plan | CLAUDE.md § "SIGIL VM — THE PAGE IS DONE, THE MARKET IS NOT" (binding order + done-criteria) |

## How the page is built (so you can change it without breaking the recipe)

- **No framework.** `main.ts` holds state, a 10 s poll (paused when the tab is hidden), delegated click/input/key
  handlers, and per-section render passes. `ui.ts` = pure renderers (snapshot → HTML). `model.ts` = the node's
  raw answers → collections / drops / movers / sales / tokens + the constant-product quote. `api.ts` = fetchers;
  a 200 with `text/html` is the SPA fallback, treated as "no data".
- **Collections are record families the chain keeps** (blocks, rigs, notes, docket seats/honours, attestations,
  treasury, ROCKY, bridge locks, coins) — not NFT contracts. Cover art is deterministic SVG from the name
  (`art.ts`, FNV → palette). "Highest Weekly Sales" are proven settlements with tx hash / docket leaf, not sales.
- **Everything leads somewhere.** Cards, trending rows, ticker items, strip cells, panel tiles and activity rows
  all open the collection modal (`data-coll`); token rows open a drawer; the panel's token rows jump to the table.
  Keep that: a new element that is not clickable is a bug.
- **Money words first** (per `feedback_sigil_ui_money_words_first_2026_09_15`): the For-you block answers "is my
  payment final / what protects it / what is it worth" with MEASURED / DESIGN / EXTERNAL tags, and What's-next
  carries ETAs computed from the measured block rate.
- **Honest empty states.** No wallet → "Connect wallet" (not a 0.0000 balance). No pool → the module says so and
  shows the formula. Node down → skeletons stay and a Retry banner appears; zeros never render as measurements.
- **Heavy routes are throttled:** `/v1/dagknight/recent` and `/v1/mining/hashrate/history` (~90 KB each; the
  site proxy strips gzip on purpose) refresh every 60 s, everything else every poll; the ticker states how far
  the cached block set is behind the tip.

## The polish loop (how to keep improving it)

Every tick: (1) look at a real shot of the LIVE page (playwright, `colorScheme:'dark'` pinned — headless
defaults to light), at one specific width and state; (2) probe behaviour, not just pixels (click it, type in
it, tab to it, reload); (3) fix the one thing you found; (4) `./ship.sh "<msg>"`; (5) re-shoot. Things this loop
has caught that a design review would not: digits typed in reverse (caret restore on a type=number), a MAX
label that was a `<b>` no handler reached, a 200-px topbar overflow that hid the wallet button on phones,
deep links landing 800 px above their section, CSS aimed at a child that never existed. **Measure with
`getComputedStyle` / `getBoundingClientRect` before believing a fix.**

## The one-month wiring plan — details the CLAUDE.md order relies on

**Seams (read from source 2026-09-16):** `SigilTx::ContractCall` is applied ONLY for `ROCKY_CONTRACT` past
`gauge_active(height)` (`sigil-tx/src/lib.rs` ~2401); other contract ids are event-only; `TokenDeploy` /
`ContractDeploy` never apply. DEX math is pure in `sigil-dex` (`swap`, `add_liquidity`, `remove_liquidity`);
`PoolState` lives in `sigil-state` under `dex_state_root`; `/v1/swap` and `/v1/add_liquidity` are wallet-signed
(`sig`, `req_nonce`); `/v1/pools` is `[]`. Oracle: `sigil-oracle-feeder` + `/v1/nation/oracle/*_wallet`.

**Week 1 — `crates/sigil-vm-market` (the Flux way).**
- `MarketSnapshot { at, height, head, collections, drops, movers, sales, tokens, series, quote_params }`, every
  numeric field wrapped as `Measured<T> { value: Option<T>, prov: Live|Derived|Pretend, src: &'static str }`.
- Built from `SigilState` + the same routes the page reads; served by `fluxc serve` (`/v1/vm/snapshot` JSON +
  `/v1/vm/events` SSE); the page's `api.ts` gets one fetch + one EventSource.
- Chain-wide series: sample per block into a ring (1 min for 24 h, hourly to 7 d) inside the crate — replaces the
  browser `localStorage` sampling so every visitor sees the same 24h.
- Tests: `fluxc test -p sigil-vm-market --profile release-fast`; chronos: producer snapshot ≡ follower snapshot at
  the same height (the header↔body discipline from the 09-10 incident applies to snapshots too).
**Week 2 — liquidity.** Delegate a feeder wallet, run `sigil-oracle-feeder` as a systemd unit (scratch and
state under `/home/storage`, never `/tmp`/`/root`), push USDS price; `POST /v1/add_liquidity` SIGIL/USDS from an
operator wallet (operator signs — the agent proposes); wallet parses `#swap=FROM:TO:AMT` and runs its shielded
send; prove one swap with `/v1/transactions/:hash/wait`.
**Week 3 — listings.** `Listing` record family under `contract_state_root` (like the token registry), applied
in the `ContractCall` arm behind `LISTINGS_LIVE_HEIGHT`, routes `/v1/vm/listings`, `/v1/vm/list`, `/v1/vm/buy`.
First item type: a shielded note (a coin is a note). Cart becomes real. Follower-first rollout.
**Week 4 — metadata + art + ownership.** Registry metadata (name, description, BLAKE3 image ref), art via
`flux_aether_*`, `sigil-vm` WASM reachable from the API, royalties to the treasury; honours/seats stay
non-transferable and the page says so.

## Rules
1. Never invent a number. `null` + PRETEND chip beats a plausible 0.
2. Never sign on the page. Money actions defer to the wallet app.
3. Never `cp` / `npm`. `./ship.sh` or nothing.
4. Frames are art, not spec (see `sigil-vm-frames`).
5. This is SIGIL work: the chain firewall binds; deliverables go to `sigilgraph.org`, never `quillon.xyz`.
6. Consensus changes (listings, metadata) are height-gated, follower-first, with a HEIGHT as the rollback plan.
7. End every step with a live shot and a one-line memory note; end every week with fewer PRETEND chips.
