# sigil-top 0.26 sync-engine production hardening (DeepSeek-reviewed, 2026-06-09)

Goal: run EXCELLENT for a multi-billion-dollar graph. Ranked, most-impactful first.

1. MUTEX POISON PANIC freezes all threads -> replace `.lock().unwrap()` with `.lock().unwrap_or_else(|poisoned| poisoned.into_inner())` in all 20+ hot-path locks  
2. SYNC STALL causes 0 blk/s with no recovery -> spawn a 30‑second watchdog that resets peer pool and sync state if `synced_to` unchanged  
3. PEER POOL BENCHED TO ZERO stops all sync -> enforce minimum active peers (e.g., 3) in pool update logic before applying bench scores  
4. UNBOUNDED MEMORY from old blocks -> cap `recv_window` to 2048 using `VecDeque::with_capacity(2048)` and `pop_front()` when full  
5. AUTO‑UPDATER RAN STALE CODE for 9h -> embed binary hash via `include_str!(concat!(env!("OUT_DIR"), "/commit.txt"))` and verify against metadata endpoint before applying update  
6. MANIFEST VERSION DRIFT breaks determinism -> compile‑time `env!("CARGO_PKG_VERSION")` and git SHA into binary; reject network if mismatch with expected network version  
7. NO OBSERVABILITY for root cause -> export prometheus counters (`blocks_per_sec`, `peer_pool_size`, `synced_to`, `mutex_poison_count`) and log on every state change  
8. GRACEFUL RESTART missing leads to orphan state -> register `SIGTERM` handler that flushes `P2PSyncState` and exits with code 0 (supervisor restarts)

## Status
- [x] #1 mutex-poison resilience — applied to block_sync.rs hot path (rocky-explorer)
- [ ] #2-#8 — sync-lane to incorporate into the 0.26.x line

## Cross-check vs 0.26.0 (rocky-explorer + DeepSeek, 2026-06-09)
The sync-lane already covered most items in 0.26.0:
- [x] #1 mutex-poison — 17 locks use `.lock().unwrap_or_else(|e| e.into_inner())` (no raw unwrap on the hot path)
- [x] #2 sync-stall watchdog — `last_advance_t` + continuous re-snap chases the tip
- [x] #3 peer-pool floor — `if healthy.is_empty() { healthy = net.connected_peers() }`
- [x] #6 version coherence — `VERSION = env!("CARGO_PKG_VERSION")` (compile-time; no hardcoded drift)
- [~] #5 updater — has 300s poll + `relaunch_new_binary` (re-exec, not exit). BUT a node ran STALE 9h
      with no re-exec → needs a reliability proof: confirm the poll fires in `--interval`/TUI mode AND
      that a self-update actually re-execs (the 0.22.25-era "exits instead of restarts" class). Suggest a
      heartbeat log on each update check + a metric `last_update_check_age`.
- [~] #7 observability — `/api/v1/status` already exposes synced/tip/peers/behind/verify_break (the key
      sync signals). Nice-to-have: a `/metrics` prometheus surface for fleet scraping.
- [ ] #8 graceful restart — add a SIGTERM handler that flushes the flux-db store + exits 0 so a supervisor
      restart never orphans the watermark.
- [x] #4 bounded memory — recent_only keeps a 2048-block window (RECENT_WINDOW); base creeps, store prunes.

VERDICT: 0.26.0 is production-grade on the core sync path (no-panic-freeze, self-healing snap, peer floor).
The remaining hardening for a multi-billion-$ graph is OPERATIONAL: prove updater reliability (#5), add
SIGTERM flush (#8), and expose /metrics (#7) for fleet observability.
