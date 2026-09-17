# SIGIL VM — handoff (2026-09-17)

*Written so you can put this down and pick it up again. Everything here was measured on the live page or read from the code — nothing is assumed.*

## Where it is

| thing | where |
|---|---|
| Live page | `https://sigilgraph.org/sigil-vm.html` (framed showcase: `/sigil-vm-showcase.html`) |
| Source | `sigil/gui/sigil-vm/` — `src/{main,ui,model,api,art,icons}.ts`, `src/style.css`, Vite + TypeScript, no framework |
| Ship | `./ship.sh "<message>"` — the ONLY way the page reaches the site (tsc → `fluxc build --frontend-only` → flux-vite-engine verify → `guard.mjs` ∥ `outage.mjs` → `snapshot.mjs` → additive copy to `/home/orobit/sigilgraph-org-site/` → HTTPS check → source tarball → commit + push `hardening/ws-2026-07-18`) |
| Gate | `guard.mjs`: 9 viewports × 2 themes — overflow, dialogs, wallet panel, 3:1 contrast, keyboard ring ≥3:1 (24 stops), 8 by-hand states scanned, search dropdown, `[hidden]`, phone tap targets ≥24 px. `outage.mjs`: 14 route families failed one at a time — no invented zero, no NaN, an "unread / did not answer / —" admission; storage-blocked and print passes |
| Probes | `/home/storage/sigil-scratch/sigil-vm-ui/probe*.mjs` — `node probe.mjs <w> <dark|light> <png> '<pre js>' '<post js>'` (env `QS`, `H`, `MOUSE`, `RM`, `FC`, `NOFONTS`) |
| Inventory | `INVENTORY.md` → `https://sigilgraph.org/downloads/sigil-vm-inventory.md` (what is real, what is mock, per surface) |
| Memory | `~/.claude/projects/-home-storage-claude-code/memory/project_sigil_vm_marketplace_page_2026_09_15.md` (every tick 191–396); skill `~/.claude/skills/sigil-vm/SKILL.md` (recipe + the traps) |
| Last commit | see `git log -1 -- gui/sigil-vm` on `hardening/ws-2026-07-18` |

## State in one paragraph

The design is finished and has been through ~200 shipped polish ticks; the gate refuses bad builds. About 70 % of what the page shows is live from `sigil-api` (`127.0.0.1:18181` via the site proxy, 10 s poll); ~30 % is OpenSea market vocabulary the chain does not have yet, and the page says so with LIVE / DERIVED / PRETEND chips (hover or tap any chip for its meaning). A **WIP** tag sits in the top bar and a **WIP** button in the bottom-left corner opens this explanation.

## What is real / what is not (short form — the inventory has the long form)

**Live:** hero, chain strip (height, finality, supply = open wallets + sealed pool, hashrate + 24 h history, shielded pool, treasury), 10 of 11 collections and their sheets (items, activity, info), drops with measured countdowns, top movers (rigs), latest settlements (attestations, docket), the wallet panel (balances, the wallet's own records marked "yours"), search, watchlist, deep links (`#coll=<id>`, `#tok=<symbol>`), Back closes any sheet on phones.

**Pretend (no chain source yet):** prices, pools and quotes (no oracle, `/v1/pools` = `[]`), listings and sales (none exist — "Highest Weekly Sales" are settlements in a sales costume), the cart, limit orders, the wSIGIL3 / SSHARE token rows, the Soneium drop, the novel collection. The Swap button hands off to the wallet with `#swap=FROM:TO:AMT`, which the wallet does not parse yet. The 1h/6h/24h/7d change column is sampled in the visitor's browser, so a fresh visitor sees "—".

## Two things learned this week that matter beyond the page

1. **`/v1/supply` `native_supply` is the sum of TRANSPARENT balances** (`sigil-state/src/lib.rs:444`). It *fell* 179K → 150K on 09-17 while the shielded pool's `value_locked` rose 29K — coins were shielded. Circulating = `native_supply + value_locked`. Never quote either alone as "supply".
2. **A route that answers with a 200 is not the same as a route that answers the question.** `/api.html#usds` was a 200 with no USDS on the page; the wallet's `#court` was a 200 whose hash nobody read (court.js now honours it). Follow links to what they show.

## The scoping suggestion (my recommendation, awaiting your word)

Stop building the marketplace; finish the overview.
1. **Cut** (≈1 day): cart, Limit tab, the six dash columns (Price·1h·24h·7d·Vol·Liq → show Supply·Holders·Age·Status), the browser-sampled change column, the two static token rows, the Soneium drop, the novel. Rename "Highest Weekly Sales" → "Latest settlements". Result: zero PRETEND chips, every figure a chain fact.
2. **Keep** the swap module (wired correctly; says "no pool yet").
3. **One real addition:** the first SIGIL/USDS pool — feed the USDS oracle (delegate a feeder wallet, run `sigil-oracle-feeder`), then one `add_liquidity` from an operator wallet. That makes the swap module quote for real and gives the page its first price.
Then stop. Engine (`sigil-vm-market`), listings, metadata and art are the next phase *when there is a listing to sell*.

## Open operator decisions (not mine to make)

- "cut" — go on the subtraction above.
- Which wallet delegates the USDS oracle feeder; which wallet funds the first pool and with how much.
- ROCKY bootstrap (`downloads/activate-gauge-rocky.sh delegate/bootstrap`) — still not run; the drop card says so.
- The wallet page's hash router: `#swap=…` (and a `#usds`) are still unparsed by `sigil-wallet-tron-embedded.html` (5 copies — surgical patches only, see CLAUDE.md).

## Next work streams (as agreed 2026-09-17)

1. **Data integrity among nodes** — true decentralization: prove that Epsilon (producer), happysrv (follower) and node3 (docker) hold the same chain and state at the same height, continuously, and alarm when they do not. Start from `three-node-compare.sh`, the chronos header↔body gate and the `order_hash` / `spine_block_hash` / state-root comparison at equal height (the 09-15 freeze and the 09-08 follower fork are the failure shapes).
2. **Fix send** — the send feature still does not work for you. Diagnose against the LIVE node with the actual wallet you use (memory: `project_sigil_shielded_send_self_refusal_rootcause_2026_09_08`, `project_sigil_android_v185…`, `project_sigil_mcp_shielded_notes_blind_to_chain_nullifiers…` — the previous three root causes were all outside the Rust send path).

## Rules that bind on this page

- Never invent a number: `null` over `0`, NaN prints "—", every figure LIVE / DERIVED / PRETEND.
- Never sign on the page.
- Ship only through `ship.sh`; never `cp` the page by hand; never raw `npm` for deploys.
- Scratch under `/home/storage/sigil-scratch/`; never `git stash` in the shared sigil tree.
- Quillon Graph is read-only during SIGIL work; SIGIL deliverables live on `sigilgraph.org`.
