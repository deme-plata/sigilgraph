# SIGIL Wallet — Changelog

> Per-release log for the 100-release plan (`sigil/RELEASE_PLAN_100_WALLET.md`).
> Each `0.X.Y` tag is one focused diff, one `fluxc compile-native --provenance`, one settled Flux-swarm task.

## Phase B — Visual identity

### 0.2.0 — Hyprland / Arch reskin — *2026-05-31*

- New layer `src/sigil/hyprland.css` (imported last in `main.tsx`, after `palette-override.css`, so it wins the cascade over the runtime-injected SIGIL chrome `<style>` blocks).
- Catppuccin-Mocha palette (`#1e1e2e` base + surfaces + mauve/pink/sky/lavender accents), reusing the exact tokens from `dist/theme-hyprland.css` for cross-surface parity.
- Hyprland signatures: the animated gradient **focused-window border ring** (8s sweep, mask-composite ::after) on ribbon / frame / tweaker / home-hero / HUD balance+panels / stat cards; **Waybar-style floating pill ribbon** with surface0 chip modules; generous window rounding + heavier backdrop blur.
- Gold (`--sigil-gold`) demoted to provenance marks only per genesis §12a — mint/primary buttons now mauve→pink gradients.
- Driven by the **Flux Eye** bridge feedback loop (snapshot comment: *"make this much prettier with hyperland archlinux inspired theme"*).

## Phase A — Bootstrap & Inventory

### 0.1.0 — Fork import — *2026-05-30*

- Sparse-checked-out `gui/quantum-wallet/` from `github.com/deme-plata/q-narwhalknight` (139 742 LOC TS/TSX/CSS, 117 components, React 19.1.1 + Vite 7.1.2 + Tailwind + libp2p 3.1.2 + three.js).
- Landed at `/home/storage/deepseek-codewhale/sigil/gui/sigil-wallet/`. Excluded `dist-final/`, `crates/` (embedded q-api-server), `node_modules/`, `package-lock.json`, Quillon-internal historical fix-log `*.md`.
- Renamed `package.json` `name`: `quantum-wallet` → `sigil-wallet`. Version reset: `1.0.0-beta` → `0.1.0` (start of independent SIGIL semver).

### 0.1.1 — Tree taxonomy — *2026-05-30*

- Auto-generated `INDEX.md` walking `src/` — 14 top-level dirs, 117 components, 8 services, 3 contexts, libp2p + webrtc modules. Index is the canonical handover doc for future Phase B-J editors.

### 0.1.3 — API base URL swap — *2026-05-30*

- `quillon.xyz` → `sigilgraph.com` everywhere in `src/` (was 145 hits across 10+ files). Centralized origin in `.env.example` (`VITE_API_URL` default).

### 0.1.4 — Brand string sweep — *2026-05-30*

- `Quillon Graph` / `Quillon` → `SIGIL` (124 word-bounded hits). Historical references in comments preserved with explicit "originally from Quillon Graph" attribution.

### 0.1.5 — Coin symbol QUG → SGL — *2026-05-30*

- 730 `QUG` references → `SGL`. TypeScript identifiers using `QUG` prefix (`QUGAmount`, etc.) renamed by same pass. `QUGUSD` left alone (handled in Phase H `USDS`/`usdSIGIL` migration).

### 0.1.6 — Logo + favicon swap — *2026-05-30*

- `public/quillon-logo.svg` + `public/quantum-ai-logo.svg` replaced with placeholder violet sigil glyph. Final logo lands in Phase B (`0.2.0 — visual identity`).

### 0.1.2 — Build green — *pending*

- `npm install` + `npm run build` — produces `dist/`.

### 0.1.7 — First deploy — *pending*

- `flux_ui_deploy` → cache-busted URL.

### 0.1.8 — Smoke test live — *pending*

- Wallet loads, connects to (`sigil-node :8181` when live, otherwise `quillon.xyz/api` as Phase 0 fallback).

### 0.1.9 — Lock-in tag — *pending*

- `0.1.9-bootstrap`, `fluxc compile-native --provenance`, `.proof` archived.
