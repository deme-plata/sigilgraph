# SIGIL 0.90 — Chronos & Storage Test Findings (2026-06-13)

Driven via flux MCP + sigil skill. Build on Delta (release), run/store on epsilon /home/storage. Live sigil-g0 testnet (:9501) untouched throughout.

## 1. Turbosync (verify-every-block throughput) — Delta release
- Peak **59,823 blk/s @ batch 8192** (256=59,067; 1024 dips to 52,198; 2048-16384 ~flat ~59-60k). 0 divergence all batches.
- Cached "~25k blk/s" estimate understated reality by **2.4x** — update it.
- ACTION: default batch -> 4096-8192; AVOID 1024. Re-confirm after any consensus change.

## 2. Emission x chronos storage engine — release, FULL PASS
- 3,628,000 blocks / 8 nodes: 0 divergence, identical roots, **byte-identical cold recovery from disk** (ok=1 bad=0), reject path OK, emission conservation holds (94.9/5/0.1), light-sync 3.6M tip-proofs @ ~683 B/tip ~0us.
- RAM: process RSS **flat ~16-31 MB at any height** (verified VmRSS + /usr/bin/time across 2/4/8/16 nodes & to 1.5M+ blocks). NOTE: cgroup memory.current LOOKS like a leak (~388 B/block) but that is reclaimable PAGE CACHE from archive writes, NOT heap. Measure RSS, not cgroup.
- Throughput ~1/nodes: 11,998 blk/s @2 -> 3,018 @16.

## 3. Storage footprint / 10TB sizing
- Archive JSON ~5.5 KB/block; release write rate ~25.8 MB/s (~93 GB/h). Literal 10TB = ~4.5 days / ~900k files.
- Pruned fluxdb format ~378 B/block = **14x denser** than JSON archives.
- ACTIONS (task #8): (a) shard archives into subdirs (~900k files in one dir at 10TB), update recover_from_disk to match; (b) offer pruned/binary archive encoding for scale.
- 1 TB soak in progress (blocks-bounded, niced): see SOAK_1TB_RESULT.txt for write-rate/file-count/recovery-time verdict.

## 4. Virtual-time gossip scenario sweep (flux_chronos_run)
- Delivery = 1 - drop^redundancy (exact). Latency-free for delivery. Deterministic. Scales to 16 nodes.
- Redundancy is expensive: 99.9% under 50% loss needs r~=10 (10x bandwidth).

## 5. Pull-repair (net-layer) — quantified
- Pull-repair (inv/getdata want-have) matches blind-redundancy delivery at ~1/(1-p) bandwidth vs (k+1)x. 50% loss for 99.9%: 10x -> 2x (~5x cheaper). Prototype + sim-validate for 0.90.

## 6. Real flux-p2p multi-node (sigil-chronos-net) — Delta<->Epsilon
- 60 blocks produced (epsilon) -> applied (delta) over real libp2p gossipsub, 0 divergence. Sustained cross-host propagation **~79 ms**.
- Isolation: separate Kademlia islands despite shared TOPIC_BLOCKS (/sigil/g0/blocks) — never cross-bootstrap a live addr.
- CHURN BUG (task #10): 0.0.0.0 bind advertises private addrs (172.17/10.x) -> dial thrash 69/66 per 60s. Public-bind fix -> 26/24 (~60% better), 0 divergence. Residual ~24 = bidirectional dial dedup (NOT idle-timeout; flux-p2p idle=60s, traffic every 1s). FIX: advertise public addr only + coalesce simultaneous dials at flux-p2p identify layer.

## 7. Release manifest (correction)
- sigil-top-latest.json is well-formed (blake3_hex, v0.77.6, targets+provenance). flux_release_check fails only on a sha256_hex schema mismatch — TOOLING, not a client bug.

## Prioritized 0.90 actions
1. [net] flux-p2p churn: public-addr advertise + simultaneous-dial coalesce (#10). Config mitigation deployable now.
2. [storage] shard archive subdirs + pruned format (#8).
3. [net] pull-repair prototype + sim-validate (#6).
4. [perf] default batch 4096-8192, avoid 1024 (#5).
5. [release] bump 0.77.6 -> 0.90, re-hash all targets, run smoke-gate + safety-gate (#7).
6. [tooling] flux_release_check accept blake3_hex; flux_sigil_benchmark/flux_bench_p2p are stubs (need SIGIL_BENCH_BIN / live peers).
