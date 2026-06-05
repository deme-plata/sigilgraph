# /goal — Close the fabric: Tor+WireGuard tested, fold catch-up wired, nodes down clean

> Copy-paste start prompt. Day-1 of SIGIL's 200-day testnet runway. **Verify-don't-trust: every number from a run, not memory. Fail-loud: if the miner or a node is down, YELL to the swarm + Viktor, never proceed silently.**

## The goal (do all three, in order — #1 is time-boxed by billing)

**1. Test Tor + WireGuard in SIGIL on the LIVE Vast fabric — before teardown.**
- ~5 geo-spread CUDA boxes are *running and billing* (~$0.45/hr); the chronos producer is up on Epsilon `:9501` (peer_id `12D3KooWCam5Uj3G2LkxVQcAwma8HfibaYUj6fRu85JddbKJbD1a`).
- Test `sigil-net-wg` (WireGuard mesh: `sigil-node wg-up <iface>` / `wg-down`, keys at `$SIGIL_DB_PATH/wg-keys/`) and `sigil-net-tor` (Arti onion, `--features arti`, stub default). Prove a SIGIL transport carries real traffic over each. `SIGIL_TRANSPORT=direct|wireguard:<iface>|tor|wg+tor:<iface>` (parsed by `sigil_net::parse_transport_str`).
- Drive nodes with the flux-fabric skill: Vast MCP is BROKEN → REST (`~/.config/vastai/vast_api_key`); flux-fleet `up/run --hosts user@host:port` (per-host ports). flux-fleet is hardened (per-host port, dedup#port, ConnectTimeout 15, ServerAlive, `run_timed` wall-clock — a hung node is bounded ≤50s).

**2. Tear the fabric down — clean.** `curl -X DELETE …/instances/<id>/` all boxes, confirm 0 remaining + balance. Coordinate with `rocky-166` (mesh) first. Everything else is already captured: flux-ssh 5-fix hardening, GPU fleet **19.02 GH/s** (CUDA blake3_miner.cu, self-verified), flux-aether **12/12** (mixer/PIR/timelock/deadman), flux-torrent **2/2** (FileBlock↔torrent, ciphertext-only swarm).

**3. Wire flux-fold as the late-join catch-up (no nodes needed).** Today's chronos sync proved blocks now PROPAGATE (the small-mesh fix worked) but a late-joining follower REJECTS them (`rejected=88, divergence=0` — no catch-up, parent-gap). **flux-fold is the fix** (zk-flux v0.3 PNG, measured): a node joining at height N verifies one **2,568-byte fold proof in ~342 ms (100k blocks)** instead of replaying. Wire `flux_miner::light` fold + sigil-node join → fold-verify → then stream-apply from the fold tip. Chronos-sim-first; prove `blocks_applied>0, divergence=0` for a late joiner.

## Reference (the two ChatGPT infographics in sigil/)
- `…09_32_49 PM.png` — Master Equation `δS_SIGIL[g,A,φ,J,K,Σ]=0`. Σ_iso (SQIsign) shipped today; A_μ (divergence) demonstrated (`divergence=0`); transports = the witness/crypto layer.
- `…08_43_28 PM.png` — `zk-flux flux-fold`: the 100k-blocks→2,568 B→342 ms light-client proof = goal #3's mechanism.

## Done = 
Tor + WireGuard each proven carrying SIGIL traffic on the live fabric · fabric destroyed (0 stragglers, balance reported) · a late-joining chronos node fold-syncs (`blocks_applied>0, divergence=0`). Broadcast each result to the swarm; label anything still pretend.

— skills: flux-fabric, flux-dev, sigil · memory: [[project_sigil_200day_testnet_runway]] [[reference_vast_rental_disk_sizing]] [[feedback_miner_node_fail_loud]] [[project_flux_fleet]]
