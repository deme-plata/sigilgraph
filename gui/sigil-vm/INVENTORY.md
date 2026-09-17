# SIGIL VM — what is real, what is mock, and what it takes to finish

*Inventory of `https://sigilgraph.org/sigil-vm.html` (+ the framed showcase), 2026-09-16, height ≈ 19.24 M.
Every line was checked against the running page and the node routes it reads, not against the design.*

Three words are used throughout, the same ones the page shows as chips:

| word | meaning |
|---|---|
| **LIVE** | read from `sigil-api` (`127.0.0.1:18181` via the site proxy) on every 10-second poll |
| **DERIVED** | a stated calculation over live numbers (block rate, window changes, hashrate share) |
| **MOCK** | illustrative — no chain source exists yet; the page says so with a chip or a "—" |

Honest headline: **the page is about 70 % live data and about 30 % marketplace mechanics that do not exist on the chain yet.** Everything it *shows* is either measured or labelled; what is missing is the *market itself* (listings, sales, prices, liquidity) and the Flux-native engine behind the page.

---

## 1. Section by section

### Top bar
| element | status | source / note |
|---|---|---|
| Search (collections, tokens, rigs, wallets), `/` shortcut, Enter opens first hit | LIVE | searches the in-memory snapshot; wallets by 64-hex |
| Chain chip + dropdown (SIGIL g2 / Polygon / Soneium) | LIVE / MOCK | height is live; Polygon row is an external link; Soneium row is a placeholder ("blocked") |
| Bell (new-block badge) | LIVE | lights when height advanced between polls |
| Cart | MOCK | no listings exist; toast says so |
| Connect wallet | LIVE (read-only) | paste a 64-hex id or prefilled from the gate; balances via `/v1/balance`; **no signing on this page** |
| Theme toggle, panel toggle, keyboard `?` sheet | LIVE | client-side |

### Ticker
| element | status |
|---|---|
| network line (hashrate, rigs, blk/s), last 6 blocks (height, blue/red, score, producer), rigs, K⊕ attest, docket, finality | LIVE — `/v1/mining/miners`, `/v1/dagknight/recent`, `/v1/earth/latest`, `/v1/court/docket`, `/v1/finality/certificate` |

### Hero banner
| element | status | note |
|---|---|---|
| Featured collection rotation (6 covers, thumbs, progress dots, pause on hover, Ken Burns) | LIVE | rotates over the collections ranked by items |
| Floor / Items / Owners / Volume chips | LIVE | but "floor" is a **re-purposed** figure per collection (blue score, free leaves, glyphs…) — there is no floor *price* anywhere |
| 24h chip | DERIVED | from browser-sampled item series; shows block rate / hashrate window until samples exist |
| Cover art | DERIVED | deterministic SVG from the collection name — **no real artwork exists for any collection** |

### Chain strip
| cell | status |
|---|---|
| Height, blk/s | LIVE / DERIVED (rate measured between polls) |
| Finality gate, certified height, validators | LIVE `/v1/finality/certificate` |
| Supply, % of 21 M | LIVE `/v1/supply` |
| Hashrate + sparkline, miners, window change | LIVE `/v1/mining/hashrate/history` |
| Shielded pool notes, value locked, spent | LIVE `/v1/shielded/anchor` |
| Treasury | LIVE `/v1/nation/status` |

### For you / What's next
| element | status |
|---|---|
| finality lag → seconds, protection summary, minted % | LIVE / DERIVED |
| "no on-chain price" statement | LIVE (true: USDS oracle not fed, `/v1/pools` empty) |
| ROCKY activation state, gauge feed freshness, USDS oracle, first pool, coins, wSIGIL3 | LIVE for the chain facts; the *ETAs* are DERIVED from measured block rate |

### Featured Collections (11 collections)
| collection | items / owners / volume | status |
|---|---|---|
| DagKnight Blocks | height, producers, blue score | LIVE |
| Shielded Notes | notes, registered wallets, free leaves, value locked, nullifiers | LIVE |
| Kristensen Earth K⊕ | attest row, tx hash, amount | LIVE (`/v1/earth/latest`) |
| Miners' Rigs | live rigs, hashrate, blocks accepted | LIVE |
| Justices of the Bench | docket JusticeAppointed entries | LIVE |
| Elefantordenen | HonourConferred entries, approvals, co-sign | LIVE |
| Nation Treasury | treasury glyphs, carve bps | LIVE |
| Rocky K⊕ Cover | live/bootstrapped, supply, pool, policies | LIVE (`/v1/rocky`) |
| Bridge Locks | lock count, vault balance, paused | LIVE (`/v1/bridge/status`) |
| SIGIL Coins (NFC) | 0 batches | LIVE (honestly empty) |
| Shadows in the Chain | — | **MOCK** (the novel; no on-chain record) |

What a *collection* is here: a **record family the chain already keeps**, presented in OpenSea's grammar. None of them is an NFT contract. Nothing can be listed, bought or transferred from this page.

### Featured Drops
| drop | status |
|---|---|
| ROCKY + GaugePush (countdown, then live/minting) | LIVE + DERIVED countdown |
| USDS | LIVE |
| SIGIL Coins first batch | LIVE facts, no date (product decision) |
| wSIGIL3 Polygon leg | **MOCK-ish** — token and pool exist on Polygon, not read from anywhere; static text |
| Soneium leg | **MOCK** — static text ("blocked") |

### Trending Collections
| element | status |
|---|---|
| ranking, floor, volume, items, owners | LIVE (same fields as above) |
| 1h / 6h / 24h / 7d change | DERIVED — sampled **in the visitor's browser**, once a minute, compacted to 7 days. A fresh visitor sees "—" until enough samples exist. **Not a chain-wide series.** |
| Trending vs Top | DERIVED (sorted by that window change / by items) |
| hover preview card | LIVE |

### Top Movers Today
| element | status |
|---|---|
| rig hashrate, kind, shielded, last seen | LIVE |
| sparkline per rig | DERIVED (browser-kept series, last 48 samples) |
| share of network | DERIVED |
| "today" change | DERIVED against the browser's first sample today |

### Highest Weekly Sales
| element | status |
|---|---|
| K⊕ attestation (amount, fee, tx hash) | LIVE |
| docket entries (rank, wallet, leaf, approvals) | LIVE |
| USDS activation, latest block, top rig | LIVE |
| **"sale", "settled", a price** | **MOCK by definition** — these are proven settlements, not marketplace sales; there is no sale on SIGIL to show |

### Swap & Tokens (the Quillon module)
| element | status | note |
|---|---|---|
| Market / Limit toggle, From/To, MAX, slider, 25/50/75/100, flip, token picker | LIVE UI | fully working controls |
| balances (when a wallet is connected) | LIVE `/v1/balance` | |
| quote (constant product, 0.3 %) | LIVE *code*, **no data** — `/v1/pools` is `[]` so the module shows "no pool yet" | |
| **Swap button → hands off to the wallet** via `#swap=FROM:TO:AMT` | **MOCK** — the wallet page does not parse that hash yet; nothing is signed or sent | |
| Limit orders | **MOCK** — no order book, no keeper; button disabled | |
| Route & pool card (formula, fee, settle time, free notes) | LIVE facts | |
| Token table: SIGIL, USDS, ROCKY | LIVE (supply, holders, age, status) | |
| Token table: wSIGIL3, SSHARE rows | **MOCK** (static, "external"/"dormant") | |
| Price, 1h/24h/7d, Volume, Liquidity columns | **MOCK / empty** — no oracle, no pool; shown as "—" | |
| Gainers / Losers filters | LIVE code, **always empty** (no price change exists) | |
| token detail drawer | LIVE | |

### Wallet panel
| tab | status |
|---|---|
| Tokens (SIGIL, USDS, ROCKY balances) | LIVE, read-only |
| NFTs | LIVE — the wallet's own records (rig, seat, honour, attester) |
| Activity | LIVE (blocks, rigs, docket, attest, finality) |
| Send / Receive | **MOCK** — open the wallet page in a new tab |
| Swap | jumps to the module (see above) |
| Portfolio USD value | **MOCK / "unavailable"** — no oracle |

### Collection modal (Items / Activity / Info)
LIVE for every collection except the novel; Info tab carries provenance, category, source page, window changes.

### Footer, OG tags, no-JS snapshot
LIVE (links checked by content type; wallet button reads the signed manifest; snapshot pre-rendered at deploy).

### Showcase (`sigil-vm-showcase.html`)
LIVE — the real page runs inside the frame; poster mode on phones. Frames are art, not spec.

---

## 2. What is NOT on the page at all (the marketplace itself)

These are the things OpenSea *is* and SIGIL VM *shows the shape of*:

1. **Listings** — nobody can put a record up for sale. No listing tx type, no order storage, no cart.
2. **Sales / offers / bids** — no settlement of a sale, no offer book, no royalties.
3. **NFT-like ownership transfer** — collections are chain record families (blocks, rigs, docket…), not transferable tokens. `sigil-vm` (the WASM VM) and the token registry exist as crates but have **no execution path from the API** (`TokenDeploy`/`ContractDeploy`/`ContractCall` are never applied).
4. **Prices** — no oracle feeds USDS or SIGIL; the only market is wSIGIL3 on Polygon, off-chain to this page.
5. **Liquidity** — `/v1/pools` is empty; `add_liquidity`/`swap` routes exist but no pool has ever been created on g2.
6. **Signing from the page** — every money action defers to the wallet app.
7. **Artwork / metadata** — no image, name or description is stored on chain for any item; covers are procedural.
8. **Chain-wide time series** — window changes and sparklines live in the visitor's browser, so two visitors see different "24h" figures.
9. **Notifications** — the bell only marks "new block since you looked".
10. **The Flux-native engine** — the page is TypeScript polling 14 JSON routes; the memo's "real flux way" is not built.

---

## 3. What it takes to finish — one month, if the days are real

Your estimate of one month is right *for the surface*; the marketplace underneath is where the month goes. Ordered so each week ships something a user can touch, and each step keeps the "nothing invented" rule.

**Week 1 — the engine (Flux way).** A Rust `sigil-vm-market` crate: one committed `MarketSnapshot` (collections, drops, movers, sales, window series, quote) built from the same routes, with provenance per field, served by `fluxc serve` at `/v1/vm/snapshot` and pushed over SSE. The page drops its 14 fetches for one. Chain-wide series replace the browser sampling, so every visitor sees the same 24h. Chronos test: snapshot(producer) ≡ snapshot(follower).

**Week 2 — liquidity and a real quote.** Create the first SIGIL/USDS pool on g2 (`/v1/add_liquidity`, needs the USDS oracle fed first — the `sigil-oracle-feeder` and a feeder wallet). The swap module then quotes for real; wire the Swap button to the wallet's actual send flow (the `#swap=` hash → wallet parses it → shielded send). Gainers/Losers get their first non-empty day.

**Week 3 — listings.** A `Listing` record family in `sigil-state` (seller, item ref, ask in glyphs, expiry) committed under the contract state root like the token registry, `/v1/vm/listings` + `/v1/vm/list` (signed), the cart becomes real, "Highest Weekly Sales" becomes actual sales. Start with the one thing that is already ownable: **shielded notes as coins** (the NFC coin is a note; a listing sells a note).

**Week 4 — metadata, art, ownership.** On-chain item metadata (name, description, image hash) via the token registry, real cover art upload to the aether store with BLAKE3 refs, `sigil-vm` WASM contracts reachable from the API so a collection can be a contract, royalties/fees to the treasury. Honours and seats stay non-transferable by design; the page says which collections are.

Every week ends the way this one did: through the Flux Vite engine gate, shot at five widths in two themes, labelled live / derived / pretend — with fewer "pretend" chips each Friday.

**Not in the month (and should not be):** price oracles beyond USDS, cross-chain listings, limit-order books. Those are chains of their own.

---

## 4. Addendum — 2026-09-17 (ticks 306–355)

New since the inventory above. None of it invents a chain figure; the first three are the visitor's own, kept in the browser.

| addition | status | note |
|---|---|---|
| Watchlist — ☆ on Trending rows and in the collection sheet, a **Watching** view | BROWSER-ONLY | `localStorage sigilvm-watch`; nothing on chain |
| Empty search start — **Recent** (last 4 collections opened here) + **Trending now** (top 4 by items) + native tokens | BROWSER-ONLY / LIVE | Recent is local; Trending now and tokens are the live snapshot |
| **yours** marks — sheet items, Weekly Sales cards, Top Movers cards, panel NFTs and Activity ("Yours" group) for the connected wallet | LIVE | matched on the on-chain wallet id (rig wallet, justice, honour recipient, attester) |
| Drop status **external** (wSIGIL3 Polygon leg) | PRETEND | live on Polygon, read from nowhere on this page — the old card wore PRETEND and LIVE together |
| Links: Rocky K⊕ Cover → `/v1/rocky`, USDS → the wallet's USDS desk + `/v1/usds/status`, bench/honours → wallet `#court` (court.js now opens on that hash) | LIVE | every "source page" was followed to what it actually shows |
| Gate: keyboard-ring contrast (24 Tab stops/theme), 8 by-hand states scanned, search dropdown at every viewport, `[hidden]` honoured, one starred row before the scan | — | `guard.mjs` |
| Reduced motion keeps the ticker (hand-scrollable); forced colours get Highlight markers; landscape phones get a text-height hero and a compact drawer | — | `style.css` media blocks |

Still exactly as pretend as before: listings, sales, prices, liquidity, signing from the page, item metadata, chain-wide series.
