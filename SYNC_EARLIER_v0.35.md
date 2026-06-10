# sigil-top v0.35 — "start sync earlier" plan (Claude × DeepSeek-V4 two-mind, 2026-06-10)

Co-analysis: Claude (rocky) + DeepSeek-V4-Pro over the **full** sync subsystem (all of
sigil-top/src + flux-p2p/src/lib.rs, ~101K tokens in DeepSeek's context window). Both
minds independently converge on the #1 fix; DeepSeek's wide pass added the flux-p2p
dial + probe wins.

## The problem
First peer connect (`net.start()`, the ~15s long pole) doesn't begin until three blocking
steps finish first on the boot thread:
`BlockStore::open` → **`sync_aether_to_fluxdb` (minutes)** → inside `launch`,
**`fetch_live_tip_blocking()` (≤5s)** → only THEN the worker spawns and dials.

## Ranked opportunities

| # | What serializes today | Where | Change | Saves | Risk |
|---|---|---|---|---|---|
| **1** | Aether ingest (concat ≤200MB shards + parse one huge JSON array) blocks the UI thread **before** `launch` | `run_tui` main.rs:2207 `sync_aether_to_fluxdb(&mut block_store, …)` | Move it **inside** the sync tokio task, after `net.start().await`, before the sync loop. Store is already owned by the worker → no borrow conflict, no oneshot needed. UI keeps the read-only `reader()`. | **minutes** | Low — only the worker touches the store; reader sees writes as flushed |
| **2** | `NetworkManager::start` calls `kademlia.bootstrap()` but does **no immediate synchronous dial** of known bootstrap peers → first `PeerConnected` waits for a DHT lookup OR the 5s mesh-maintenance timer's first tick | flux-p2p `lib.rs:197 start()` (the dial loop already exists at :313–:348, only fires after 5s) | Right after `kademlia.bootstrap()`, eagerly `swarm.dial()` every configured bootstrap multiaddr once (the bare-addr path already exists in the retry timer). No-op if already connected. | **1–5s** | Very low — reuses existing dial path |
| **3** | Pull-height probe (seeds `peer_best_height`, gates fast-snap) waits up to `PROBE_EVERY` (~500ms) after the first peer connects | `block_sync::launch` inner loop | On the first `PeerConnected` event, set `last_probe` overdue (or shrink `PROBE_EVERY` to ~50ms for the first 10s) so the probe fires immediately | **~500ms** | Low — timer tweak |
| **4** | Fast-snap needs `store.best_height` raised; only real received blocks raise it | `block_sync::launch` start | Seed `store.best_height` from `read_persisted_tip()` on start (already seeded into `peer_best`, not the store) | 0–500ms | None — local max, only raised |

## Before / after (cold restart, live chain)
```
BEFORE:  open ─ aether(minutes) ─ tipfetch(5s) ─ [DHT/5s-timer→connect] ─ first paint ─ sync
AFTER:   open ─ reader ─ launch(worker) ─ terminal ─ first paint           (UI: ~instant)
                              └─ worker: net.start + eager-dial peers ─ aether ingest ─ sync loop
```
"connecting…" shrinks to roughly the time to dial one peer (sub-second on a live mesh).

## Reordered skeleton (DeepSeek-drafted, Claude-reviewed)
- `run_tui`: open store → `reader()` → ctrl-c → `P2PBlockSync::launch_with_aether(block_store, recent_only, aether_dir)` (store MOVED) → terminal/raw-mode → serve → render loop. Aether call **removed** from the main path.
- `launch_with_aether`: spawn worker → build rt → `net.start().await` → `sync_aether_to_fluxdb(&mut store, &aether_dir)` → resume from `store.synced_to()` → sync loop. Tip-poller spawned as today (keeps fast-snap on cycle 1).
- `fetch_live_tip_blocking()` moves into the worker thread (blocks the worker OS thread, not the UI).
- flux-p2p `start()`: add the one-shot eager dial loop after `kademlia.bootstrap()`.

## Invariants to preserve (both minds agree)
1. **Fast-snap on cycle 1** — tip-poller still seeds `peer_best` within one RTT; persisted-tip cache covers oracle-down.
2. **Persisted verified watermark** (`meta 'V'`, loaded in `BlockStore::open`) — aether adds raw blocks below `verified_to`; chain_verify re-verifies in the normal loop. No regression.
3. **Shared read-only `reader()`** — cloned before the store moves; shares the `flux_db::Database` (`Arc<RwLock>`), read-only methods only.

Raw artifacts: `/tmp/sigil-sync-full.md` (brief), `/tmp/ds-resp2.json` (DeepSeek response).
