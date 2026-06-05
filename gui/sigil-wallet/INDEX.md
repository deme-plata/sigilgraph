# sigil-wallet — Source Tree Index

> Auto-generated 2026-05-30 for release 0.1.1 (Phase A, Bootstrap & Inventory).
> Re-generate with: `bash scripts/gen-index.sh` (planned in 0.1.9 lock-in).

## Top-level

- `build-preserve-downloads.sh`
- `CHANGELOG.md`
- `deploy.sh`
- `.env.example`
- `eslint.config.js`
- `index.html`
- `INDEX.md`
- `nginx.conf`
- `nginx-local.conf`
- `package.json`
- `postcss.config.js`
- `public/` (5 files)
- `README.md`
- `src/` (172 files)
- `tailwind.config.js`
- `tsconfig.app.json`
- `tsconfig.json`
- `tsconfig.node.json`
- `vite.config.ts`

## src/ layout

### `src/assets/` — 0 files, 0 LOC

- react.svg

### `src/components/` — 117 files, 109430 LOC

- ActiveLoansCard.tsx
- AddressBook.tsx
- AIChatScreen.tsx
- AIWheelButton.tsx
- AIWorkerDemo.tsx
- AIWorkerPanel.tsx
- AnalyticsScreen.tsx
- AnimatedBorder.css
- ... 109 more

### `src/constants/` — 1 files, 86 LOC

- ticker.ts

### `src/contexts/` — 3 files, 683 LOC

- LibP2PContext.tsx
- PasswordModalContext.tsx
- SessionTimeoutContext.tsx

### `src/hooks/` — 7 files, 2054 LOC

- useAIWorker.ts
- useInfiniteBlockScroll.ts
- useMinerLink.ts
- useP2PData.ts
- usePasswordPrompt.ts
- useRealtimeBlocks.ts
- useTransactionBroadcast.ts

### `src/libp2p/` — 28 files, 14853 LOC

- ai-worker-node.ts
- blockCache.ts
- blockPropagationQueue.ts
- blockRequest.ts
- blockServer.ts
- browserPeerDiscovery.ts
- bulletproofsPP.ts
- config.ts
- ... 20 more

### `src/services/` — 7 files, 6566 LOC

- aegisQL.ts
- api.ts
- gmailAuth.ts
- nodeDiscovery.ts
- SignalingService.ts
- sseManager.ts
- walletAuth.ts

### `src/utils/` — 1 files, 123 LOC

- transactionFix.ts

### `src/webrtc/` — 1 files, 535 LOC

- WebRTCManager.ts

## Top-level src files

- App.css
- App.tsx
- global.d.ts
- index.css
- main.tsx
- vite-env.d.ts

## Totals

- Components: 117
- Services: 7
- Contexts: 3
- libp2p modules: 28
- webrtc modules: 1
- TS/TSX/CSS LOC: 139742
