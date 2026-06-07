# ⬡ SigilGraph — Release Ledger

Commercial-grade releases. Every release is a **GPG-signed (Verified)** commit, and the released
code carries a **content-addressed flux-rev provenance hash** — re-snapshot the crate with
`flux-rev snapshot <crate>` and verify the hash matches. *Probatione, non fide.*

| Version | Date | Type | git (signed) | flux-rev provenance |
|---|---|---|---|---|
| **v0.0.13** | 2026-06-07 | 🚀 sigil-top v0.3.5 + Chronos Swarm | `HEAD` | sigil-top `—` (musl static-pie) |
| **v0.0.12** | 2026-06-07 | 🚀 sigil-top v0.3.0 P1 Foundation | `4c98eb4` | sigil-top `0fb29075428d61e6…` (musl static-pie) |
| **v0.0.11** | 2026-06-07 | ✨ sigil-top v0.2.35 | `e083d57` | sigil-top `c468aedd58c37ecf…` (musl static-pie) |
| **v0.0.10** | 2026-06-05 | 🐛 fix | `ec873b7` | — |
| **v0.0.9** | 2026-06-05 | ✨ feature | `d03672d` | sigil-state `1f5c55a01cfe3aa9…` |
| **v0.0.8** | 2026-06-05 | 🚀 initial public release (67 crates) | `b219791` | — |

## v0.0.13 — sigil-top v0.3.5 "Chronos Swarm + TPS Breakthrough"

### sigil-top v0.3.5
- **Chronos benchmark card** — displays latest Warp Drive TPS results from local JSON. Shows max TPS, block/tpb/wallets config, apply-vs-commit split, sweep duration.
- **P2P Gossip health card** — peer count, mesh quality (healthy/warming/empty), estimated drop rate, verify latency. Fan-out derived from sqrt(peers).
- **v0.3.5 version bump** — `LATEST = "0.3.5"`, auto-updater fetches from `sigilgraph.quillon.xyz/downloads/sigil-top-latest.json`.

### Chronos Research (5 new binaries in sigil-chronos)
- **`sigil-warp-drive`** — TPS parameter sweeper. Sweeps blocks×tpb×wallets via Chronos throughput harness, finds max TPS config. Parallel Rayon execution. Outputs JSON + Delta deploy env vars.
- **`sigil-hurricane`** — Gossip parameter optimizer. Simulates multi-node mesh in Chronos, sweeps fan-out×latency×drop%, finds min block propagation latency. Outputs optimal flux-p2p NetworkConfig.
- **`sigil-delta-pipeline`** — Pipeline block production benchmark. Models CPU/network overlap, sweeps depth×build-time×propagation-time, quantifies TPS speedup vs sequential.
- **`delta-archive-oracle`** — 20TB block-range server. Sharded block store (1M/shard), zstd compression, BLAKE3 integrity, HTTP API (`GET /blocks?from=N&to=M`), binary index O(1) seeks. Deploy on Delta :9800.
- **`sigil-swarm`** — Multi-Epsilon swarm orchestrator with 4 MCP combo verbs: `sigil_swarm_launch` (spawn N + health), `sigil_swarm_bench` (coordinated benchmark), `sigil_swarm_health` (aggregate), `sigil_swarm_diverge` (divergence detection). 70-85% token savings.

### MCP Combos (new phrasal verbs)
| Verb | Composes | Token savings |
|------|----------|---------------|
| `sigil_swarm_launch` | spawn N epsilon nodes + health check + mesh verify | 70% |
| `sigil_swarm_bench` | launch + warp-drive TPS sweep + hurricane gossip sweep | 80% |
| `sigil_swarm_health` | node_status × N + divergence check + aggregate | 75% |
| `sigil_swarm_diverge` | diff all nodes + find divergence points + report | 85% |

### flux-chronos (CHRONOS-S snapshot serde)
- Full/delta snapshots with zstd/lz4 compression
- BLAKE3 integrity checksums per snapshot
- Atomic catalog writes (tmp→rename), genealogy tracking (parent/child)
- SnapshotDiff engine (nodes changed/added/removed, event growth, seed/tick comparison)
- Query/predicate/prune/annotate API
- Enables 50K-100K checkpoints on 20TB
- `Universe::serialize_to_vec()` / `deserialize_from_slice()` with node factory
- `SimNode::type_tag()` for snapshot reconstruction

### Dependencies added
- `bincode`, `hex`, `blake3`, `serde_json`, `zstd`, `lz4` → flux-chronos + sigil-chronos
- `rayon` → sigil-chronos (parallel benchmark sweeps)

## v0.0.11 — sigil-top v0.2.35 "visible wallet + SQIsign readiness"
- **feat(sigil-top):** wallet balance display — fetches `wallet_balance` from feed + renders in MINING card (whole.fractional SIGIL, 8 decimals). Non-breaking: 0 when feed doesn't carry it.
- **feat(sigil-top):** live mining hashrate — `⛏ 12.34 MH/s · 5M hashes` reported every ~2s from the miner thread, rendered in MINING card with auto-scaling to GH/s.
- **feat(sigil-top):** SQIsign tip-proof flavor scaffolding (L4-B readiness) — `TipVerify.sqisign_available` field + `cfg!(feature = "sqisign")` gate. SECURITY card shows "SQIsign · gated (L4-B)" until the feature lands.
- **fix(sigil-top):** ratatui TUI is now the default — `--tui` kept as explicit alias, no opt-in needed.
- **fix(sigil-top):** startup auto-update uses TUI toast instead of `eprintln!` — no more alt-screen corruption on launch.
- **chore:** stale workspace-root binaries (`sigil-top.1`–`.3`) moved to `sigil/releases/`.

## v0.0.10 — fix
- **fix(acc-scale-bench):** faithful O(1) update measurement — the loop passed a stale `old_value`, drifting the accumulator; it now flips one key between two values (each old = previous new), so the sum stays consistent and the timing is a true single-key O(1) update.

## v0.0.9 — feature
- **feat(sigil-state):** O(1) state-root scaling benchmark (`acc-scale-bench`) — scales the multiset accumulator 1M→100M accounts and shows `root()`/`update()` stay **flat (~200 ns)** while from-scratch grows O(N). Measured proof that a tiny light node verifies a multi-TB chain at constant cost.

## v0.0.8 — initial public release
- An experimental DagKnight chain on Flux: provenance-signed blocks (BLAKE3 × SQIsign), a 572 KB light client that verifies the whole chain in ~10 µs, 21M capped by construction.
