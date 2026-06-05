# sigil-wallet

> Browser wallet for the **SIGIL** experimental DagKnight-on-Flux chain. Forked from Quillon Graph quantum-wallet (`v1.0.0-beta`, 2026-05-29) and re-targeted at `sigil-node` (`:8181`, `network_id = sigil-g0`).

- React 19 + Vite 7 + TypeScript 5 + Tailwind + libp2p 3 (browser P2P) + three.js
- Visual identity (Phase B): obsidian + violet, gold reserved for `.proof` accents
- Network (Phase C): JSON-RPC + SSE against `sigil-node`, `sgl1…` addresses, Dilithium5/SQIsign-L5
- Provenance (Phase D): every release ships a SQIsign-signed `fluxc compile-native --provenance` `.proof`

## Status
- **Current release**: `0.1.0` — Phase A (Bootstrap & Inventory)
- **Plan**: 100 releases across 10 phases — see `../../RELEASE_PLAN_100_WALLET.md`
- **Changelog**: see `CHANGELOG.md`
- **Source tree**: see `INDEX.md` (auto-generated)

## Quick start

```bash
npm install
npm run dev          # vite dev server with HMR (observed via flux-vite-engine)
npm run build        # tsc -b && vite build → dist/
```

Then deploy via `flux_ui_deploy file=sigil-wallet/dist` (never raw `cp` — the cache-busted URL is the whole point of the substrate).

## Heritage

Substrate is Quillon Graph quantum-wallet, the production wallet at `quillon.xyz`. Where parts of this codebase are unchanged from that origin, attribution is preserved in comments. The fork is permitted by virtue of SIGIL being a sister experimental chain — every block SIGIL produces commits `BLAKE3(SIGIL_GENESIS_v0.md)` which itself acknowledges Quillon.

---

(original Vite template README below — retained for reference, will be removed in 0.1.9 lock-in)

This template provides a minimal setup to get React working in Vite with HMR and some ESLint rules.

Currently, two official plugins are available:

- [@vitejs/plugin-react](https://github.com/vitejs/vite-plugin-react/blob/main/packages/plugin-react) uses [Babel](https://babeljs.io/) for Fast Refresh
- [@vitejs/plugin-react-swc](https://github.com/vitejs/vite-plugin-react/blob/main/packages/plugin-react-swc) uses [SWC](https://swc.rs/) for Fast Refresh

## Expanding the ESLint configuration

If you are developing a production application, we recommend updating the configuration to enable type-aware lint rules:

```js
export default tseslint.config([
  globalIgnores(['dist']),
  {
    files: ['**/*.{ts,tsx}'],
    extends: [
      // Other configs...

      // Remove tseslint.configs.recommended and replace with this
      ...tseslint.configs.recommendedTypeChecked,
      // Alternatively, use this for stricter rules
      ...tseslint.configs.strictTypeChecked,
      // Optionally, add this for stylistic rules
      ...tseslint.configs.stylisticTypeChecked,

      // Other configs...
    ],
    languageOptions: {
      parserOptions: {
        project: ['./tsconfig.node.json', './tsconfig.app.json'],
        tsconfigRootDir: import.meta.dirname,
      },
      // other options...
    },
  },
])
```

You can also install [eslint-plugin-react-x](https://github.com/Rel1cx/eslint-react/tree/main/packages/plugins/eslint-plugin-react-x) and [eslint-plugin-react-dom](https://github.com/Rel1cx/eslint-react/tree/main/packages/plugins/eslint-plugin-react-dom) for React-specific lint rules:

```js
// eslint.config.js
import reactX from 'eslint-plugin-react-x'
import reactDom from 'eslint-plugin-react-dom'

export default tseslint.config([
  globalIgnores(['dist']),
  {
    files: ['**/*.{ts,tsx}'],
    extends: [
      // Other configs...
      // Enable lint rules for React
      reactX.configs['recommended-typescript'],
      // Enable lint rules for React DOM
      reactDom.configs.recommended,
    ],
    languageOptions: {
      parserOptions: {
        project: ['./tsconfig.node.json', './tsconfig.app.json'],
        tsconfigRootDir: import.meta.dirname,
      },
      // other options...
    },
  },
])
```
