# SIGIL Wallet — 100-Release Plan

> Forked from **Quillon Graph quantum-wallet** (`gui/quantum-wallet/` on Beta `185.182.185.227`), reskinned + retargeted for the **SIGIL** experimental chain (`sigilgraph.com`, `sigil-node` on `:8181`, `network_id = sigil-g0`).
>
> 10 phases × 10 releases = 100 releases. Each release is one focused diff, one tagged build (via `fluxc compile-native --provenance`), one settled Flux-swarm task. Estimated total settlement: **~50 QUG** at 0.5 QUG/release.
>
> Planned with `flux_architect_predict` (66 crates, 75 348 LOC, 62% architecture score) + `flux_predict_batch` (15-crate batch) + the existing flux-vite-engine / CHIRON / hot-swap substrate already shipped under [[project_session_checkpoint_2026_05_30]].

---

## Substrate basis

| What | Source | Target |
|---|---|---|
| Code starting point | `git://185.182.185.227:9418/q-narwhalknight` → `gui/quantum-wallet/` | `/home/storage/deepseek-codewhale/sigil/gui/sigil-wallet/` |
| Build system | Vite + TypeScript + React (as-shipped by Quillon) | unchanged — observed live via `flux-vite-engine` v0.2 |
| Backend RPC | `quillon.xyz` `/api/v1/*` REST + SSE | `sigil-node` `:8181` JSON-RPC + `/api/v1/feed` SSE |
| Coin symbol | QUG (Quillon Graph) | SGL (SIGIL) |
| Address prefix | `qnk1…` | `sgl1…` |
| Network ID | `mainnet-genesis` | `sigil-g0` |
| Visual identity | Quillon obsidian + emerald + gold | SIGIL obsidian + violet + gold-accents-only-on-`.proof` |
| Provenance | none (Quillon ships unsigned binaries) | every binary carries `fluxc .proof` (SQIsign-L5, BLAKE3) |

---

## The 10 phases

| # | Phase | Versions | Theme | Lane |
|---|---|---|---|---|
| A | Bootstrap & Inventory | `0.1.0 — 0.1.9` | Fork, build green, deploy as-is | open |
| B | Visual Identity | `0.2.0 — 0.2.9` | Obsidian + violet palette, fonts, sigil glow | blocked by A |
| C | Network Substitution | `0.3.0 — 0.3.9` | Repoint RPC, address formats, signing | blocked by A |
| D | SIGIL Primitives | `0.4.0 — 0.4.9` | 4-state-root view, `.proof`, 10ms tip-verify | blocked by C |
| E | Dev Surface | `0.5.0 — 0.5.9` | vite-garden + hot-swap + CHIRON embedded | blocked by B |
| F | Multi-Agent / agentic-money | `0.6.0 — 0.6.9` | CLAI drops, sister-agent book, swarm tab | blocked by C |
| G | Privacy + ZK | `0.7.0 — 0.7.9` | Tor egress, ring sigs, bulletproofs | blocked by D |
| H | DEX + DeFi | `0.8.0 — 0.8.9` | Pools, swaps, USDS, oracle, emission | blocked by F |
| I | Polish + Perf | `0.9.0 — 0.9.9` | a11y, bundle audit, i18n, error boundaries | blocked by E,F,G,H |
| J | Launch + Post-launch | `1.0.0 — 1.0.9` | v1.0 ship + hotfix waves + 24h soak | blocked by I |

---

## All 100 releases

### A — Bootstrap & Inventory (`0.1.0 → 0.1.9`)

| Ver | Release | Acceptance |
|---|---|---|
| 0.1.0 | **Fork import** | `sigil/gui/sigil-wallet/` exists with verbatim Quillon source; `git log` shows single import commit |
| 0.1.1 | **Tree taxonomy** | auto-generated `INDEX.md` via `flux_xray` lists every file's purpose |
| 0.1.2 | **Build green** | `npm i && npm run build` produces `dist/`; emitted under `flux-vite-engine` observation |
| 0.1.3 | **API base URL swap** | every `quillon.xyz` REST origin → `sigilgraph.com`; centralized in `src/config/api.ts` |
| 0.1.4 | **Brand string sweep** | "Quillon" → "SIGIL" across 50+ literals; no remaining "QUG" outside historical docs |
| 0.1.5 | **Coin symbol QUG→SGL** | parser/formatter centralized in `src/lib/units.ts` |
| 0.1.6 | **Logo + favicon** | placeholder violet sigil glyph in `public/` |
| 0.1.7 | **First deploy** | `flux_ui_deploy → sigilgraph.com/wallet.html`; cache-busted URL returned |
| 0.1.8 | **Smoke test live** | wallet loads, connects to `sigil-node :8181`, displays "0 SGL" for fresh address |
| 0.1.9 | **Lock-in tag** | `0.1.9-bootstrap`, `fluxc compile-native --provenance`, `.proof` archived |

### B — Visual Identity (`0.2.0 → 0.2.9`)

| Ver | Release | Acceptance |
|---|---|---|
| 0.2.0 | **CSS variable migration** | emerald→violet, gold-everywhere→amethyst; tokens in `:root` |
| 0.2.1 | **Font swap** | JetBrains Mono headings, IBM Plex Sans body |
| 0.2.2 | **Obsidian-only** | drop light theme; single canonical dark obsidian palette |
| 0.2.3 | **Sigil glow** | keyframe + drop-shadow on primary buttons + balance |
| 0.2.4 | **Gold = provenance** | gold reserved for `.proof` badges + verified-author marks (Quillon-thread continuity) |
| 0.2.5 | **Animated background** | slow violet/amethyst gradient, GPU-friendly |
| 0.2.6 | **Icon set redo** | lucide + custom sigil sub-icons; SVG sprite |
| 0.2.7 | **Toast redesign** | violet pulse on success → gold halo on verified-proof, ember on error |
| 0.2.8 | **Loading skeletons** | sigil-shaped placeholders |
| 0.2.9 | **Lock-in tag** | `0.2.9-brand`, brand alignment review w/ `flux_swot` |

### C — Network Substitution (`0.3.0 → 0.3.9`)

| Ver | Release | Acceptance |
|---|---|---|
| 0.3.0 | **RPC client rewrite** | `q-api-server` REST → `sigil-node` JSON-RPC, codegen via `flux-api` |
| 0.3.1 | **Address format** | `qnk1…` → `sgl1…` everywhere (display, parsing, QR) |
| 0.3.2 | **Signing** | Quillon Dilithium → SIGIL Dilithium5 / SQIsign-L5 via `flux-eternal-cypher` dispatch |
| 0.3.3 | **Mempool view** | reads `flux-mempool` over WS |
| 0.3.4 | **Peer list** | `GET /api/v1/peers` from sigil-node |
| 0.3.5 | **Block explorer panel** | 4 state roots displayed per block |
| 0.3.6 | **SSE feed** | subscribe to `/api/v1/feed`, render in feed component |
| 0.3.7 | **Multisig flow** | `q-multisig` → `flux-multisig` |
| 0.3.8 | **Strip Quillon bridges** | remove cross-chain bridge UI not in SIGIL scope |
| 0.3.9 | **Lock-in tag** | `0.3.9-sigil-substrate` |

### D — SIGIL Primitives (`0.4.0 → 0.4.9`)

| Ver | Release | Acceptance |
|---|---|---|
| 0.4.0 | **State-root widget** | 4 roots (wallet/dex/event/contract) shown per block, clickable to inspect |
| 0.4.1 | **`.proof` viewer** | parse + verify SQIsign sig client-side via `flux-ivc-verifier-wasm` |
| 0.4.2 | **10ms tip-verify** | `flux-zk-stark` gate; render OK/FAIL in ≤10ms |
| 0.4.3 | **BLAKE3 fingerprint** | shown on every binary download; one-click verify |
| 0.4.4 | **Genesis BLAKE3** | `SIGIL_GENESIS_v0.md` hash pinned + verified on every cold boot |
| 0.4.5 | **Master equation card** | links to Whitepaper Appendix A, interactive rerender |
| 0.4.6 | **Settled-task receipt** | per-agent `.proof` viewer (BLAKE3 + SQIsign + optional `settle_tx`) |
| 0.4.7 | **Honest-pretend banner** | VarFlow Axiom 6 — explicit "what is not yet real" panel |
| 0.4.8 | **Reorg detector** | fork-detection feed surfaced as toast + log |
| 0.4.9 | **Lock-in tag** | `0.4.9-sigil-native` |

### E — Dev Surface (`0.5.0 → 0.5.9`)

| Ver | Release | Acceptance |
|---|---|---|
| 0.5.0 | **vite-garden embed** | iframe `/dev` route renders live garden + ribbon |
| 0.5.1 | **Hot-swap panel** | SSE from `/api/hotswap/events` ([[project task #128]]) rendered live |
| 0.5.2 | **CHIRON pipeline panel** | sister surgical engine wired into `/dev` |
| 0.5.3 | **Workspace x-ray** | `/api/xray` snapshot rendered as ArchMap |
| 0.5.4 | **⌘K search** | `flux-search` v2 system-wide bar (mcp_tap + secret_scrape live) |
| 0.5.5 | **Build event ribbon** | live HMR + transform + hot-swap + CHIRON events, color-coded |
| 0.5.6 | **SAP score dial** | composite of 4 axes (HMR/types/transform/hot-swap) |
| 0.5.7 | **X-Algo predict card** | 8-dim forecast |
| 0.5.8 | **Architect snapshot** | 66-crate panel (LOC, batches, deps) |
| 0.5.9 | **Lock-in tag** | `0.5.9-dev-surface` |

### F — Multi-Agent / Agentic-Money (`0.6.0 → 0.6.9`)

| Ver | Release | Acceptance |
|---|---|---|
| 0.6.0 | **Agent wallet UI** | nickname + icon header for known agents (rocky/adrian/codex/grok) |
| 0.6.1 | **CLAI welcome-drop** | paste sister-agent qnk → one-click 100 CLAI send |
| 0.6.2 | **Sister-agent address book** | persisted, synced via `flux_swarm_register` |
| 0.6.3 | **Swarm dashboard tab** | active claims + recent completions + settlement log |
| 0.6.4 | **LP positions widget** | per-agent pool list, fee accrual marked at QUGUSD-anchor (USDS-anchor for SIGIL) |
| 0.6.5 | **Earned vs mined toggle** | balance card splits sources |
| 0.6.6 | **`.proof` artifact gallery** | agent's signed work browsable + verifiable |
| 0.6.7 | **swarm chat panel** | `flux_swarm_inbox` rendered as thread view |
| 0.6.8 | **Quorum-sign multisig** | M-of-N agent approval flow ([[#114 flux_quorum_sign]]) |
| 0.6.9 | **Lock-in tag** | `0.6.9-agent-network` |

### G — Privacy + ZK (`0.7.0 → 0.7.9`)

| Ver | Release | Acceptance |
|---|---|---|
| 0.7.0 | **Tor egress toggle** | `q-tor-client` SOCKS5 routing for outbound RPC |
| 0.7.1 | **Dandelion stem viz** | stem-vs-fluff propagation visualised |
| 0.7.2 | **Ring-sig sender** | 10-of-anonymous-set send flow |
| 0.7.3 | **Bulletproof amount-hide** | toggle that wraps tx amount in BP |
| 0.7.4 | **Stealth-address QR** | one-shot receive QR |
| 0.7.5 | **ZK-anchor receipt** | block-inclusion proof + STARK rendered post-send |
| 0.7.6 | **PQ key import** | Dilithium5 / SQIsign roundtrip |
| 0.7.7 | **Recursive-proof bundle viewer** | inspect bundled N-sig proofs |
| 0.7.8 | **Privacy dashboard** | "which flows leak what" honest summary |
| 0.7.9 | **Lock-in tag** | `0.7.9-private` |

### H — DEX + DeFi (`0.8.0 → 0.8.9`)

| Ver | Release | Acceptance |
|---|---|---|
| 0.8.0 | **SIGIL DEX pool list** | full pool browser |
| 0.8.1 | **Swap flow** | route + slippage + `.proof` of route |
| 0.8.2 | **LP add/remove** | with min-out protection |
| 0.8.3 | **LP earnings tracker** | per-pool fee accrual, mark-to-USDS |
| 0.8.4 | **USDS panel** | native stablecoin overview |
| 0.8.5 | **Oracle viewer** | feed sources + median + freshness |
| 0.8.6 | **Emission controller** | rate, watermark, total supply, parameters (state-root committed) |
| 0.8.7 | **Token deploy wizard** | ERC-style flat token |
| 0.8.8 | **Custom-token book** | persisted contract-address ↔ symbol |
| 0.8.9 | **Lock-in tag** | `0.8.9-defi` |

### I — Polish + Perf (`0.9.0 → 0.9.9`)

| Ver | Release | Acceptance |
|---|---|---|
| 0.9.0 | **Bundle audit** | tree-shake, `vite-plugin-visualizer` report attached as `.proof` |
| 0.9.1 | **Lighthouse 95+** | scores attached as proof artifact |
| 0.9.2 | **a11y AA contrast** | every text/bg pair ≥4.5:1 |
| 0.9.3 | **Keyboard nav** | full pass with focus-visible rings |
| 0.9.4 | **Error boundary + offline** | retry strategy, offline banner |
| 0.9.5 | **i18n scaffold** | `en` + `nb` (Norwegian — Viktor's locale) |
| 0.9.6 | **Animation polish** | reduced-motion respected; CSS-first |
| 0.9.7 | **Onboarding rewrite** | first-time user runs into 100 CLAI drop |
| 0.9.8 | **Settings consolidation** | one settings page, sub-tabs |
| 0.9.9 | **Lock-in tag** | `0.9.9-rc1` |

### J — Launch + Post-launch (`1.0.0 → 1.0.9`)

| Ver | Release | Acceptance |
|---|---|---|
| 1.0.0 | **SIGIL Wallet v1.0** | announcement post, .proof archived on-chain, infographic deployed |
| 1.0.1 | **Hotfix wave #1** | first 48h soak feedback addressed |
| 1.0.2 | **Hotfix wave #2** | week-2 feedback |
| 1.0.3 | **Soak feedback #1** | tracked via `/dev` ribbon + telemetry |
| 1.0.4 | **Mobile-responsive** | 360px → 1920px |
| 1.0.5 | **Browser-extension build** | wxt or plasmo, dist as `.crx` + `.xpi` |
| 1.0.6 | **Hardware-wallet integration** | Ledger / Trezor (PQ profile when available) |
| 1.0.7 | **QuillonOS userspace embed** | in-tab CPU miner (`fluxc → wasm32-wasip1`) |
| 1.0.8 | **Cross-chain bridge re-added** | SIGIL ↔ Quillon Graph, audited path |
| 1.0.9 | **Soak gate** | 24h continuous run on Delta + Epsilon w/o crash; ⇒ `1.0.9-soaked` lock-in |

---

## Tooling per release

Every release runs the same loop:

```
flux_file_claim     → lock files
flux_predict        → forecast diff cost
edit                → minimal diff for one acceptance criterion
flux_combo          → compile + test + predict
flux_ui_preview     → cache-busted URL
fluxc compile-native --provenance → .proof
flux_swarm_complete → settle 0.5 QUG
flux_swarm_message  → broadcast tag + verify URL
```

For phase-final lock-in releases (`0.X.9`):

```
flux_release_check   → versioning + changelog gate
flux_zk_combo        → 10ms tip-verify gate
flux_ai_audit        → policy + scope audit on state-touching code
flux_release_publish → push tag + announce
```

---

## What's deliberately NOT in this plan

- **In-house Slint native wallet** — separate roadmap, not browser-wallet's 100-release plan.
- **Mobile-first rewrite** — release 1.0.4 is mobile-responsive, not a native app rewrite.
- **Multi-account UI from day one** — single active account through Phase H; multi-account is a post-1.0 lane.
- **Server-side anything** — wallet is pure client; backend changes belong in `sigil-node` lanes.

---

## Honest-pretend section (VarFlow Axiom 6)

What is real today (`2026-05-30`):

- ✅ Quillon Graph quantum-wallet source — exists, builds, ships on `quillon.xyz`
- ✅ Local sparse clone landed at `/tmp/qnk-snap/gui/quantum-wallet/`
- ✅ flux-vite-engine v0.2 (events for HMR / hot-swap / CHIRON / search-tap; 26/26 tests)
- ✅ vite-garden surface live (CHIRON panel + hot-swap panel + 7 vite panels)
- ✅ flux-search v2 substrate (mcp_tap + secret_scrape + facets; 27/27 tests)
- ✅ flux-hotswap crate (HotFn AtomicPtr trampoline; 248 LOC)

What is still pretend:

- ⚠ `sigil-node` `:8181` exists in code (`sigil-node` crate), not yet running 24h on Delta+Epsilon (Phase 0 deploy in progress)
- ⚠ `sigilgraph.com` DNS not yet pointed to live boxes
- ⚠ `flux-multisig`, `flux-eternal-cypher`, `flux-mempool` are crates referenced in this plan that may or may not yet exist by the time their phase begins (recheck via `flux_architect_predict` at phase start)
- ⚠ `flux_release_check`, `flux_release_publish` MCP tools (SWARM S1) — pending
- ⚠ The wallet's existing Tor / privacy code may need substantial rewrite vs. simple repointing
- ⚠ The 0.5 QUG/release rate is a planning estimate; live rate is `flux_swarm_settlement_preview` ([[#115]]) when it ships

This plan is the *map*, not the *territory*. Each phase begins with a re-survey via `flux_architect_predict` + `flux_xray` before releases are scoped.
