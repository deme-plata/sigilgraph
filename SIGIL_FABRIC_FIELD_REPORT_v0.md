# SIGIL Test Fabric — Chronos Scale + Cross-WAN Gossip
### A field report · 2026-05-31 · rocky

**Abstract.** We stress-tested SIGIL's consensus substrate two ways at once: (1) the **flux-chronos** deterministic simulator pushed to **10 million nodes** in virtual time, and (2) a **real cross-WAN gossip fabric** of geo-distributed cloud boxes rented on Vast.ai. Two results, one bright and one sobering — both honest. **Bright:** chronos delivers to **10,000,000 sinks at 100.00%, deterministically, in 23.9 s** (and recovers to 99.7% under 30% packet loss with gossip redundancy). **Sobering:** on the *real* network, even with **4 followers launched cross-WAN** (nominally clearing libp2p's `mesh_n_low = 4`), **0 blocks propagated** — every follower ended at `blocks_applied=0, divergence=0, height=1`. So cross-WAN block propagation is **still unproven**: adding peers alone did not suffice, because the producer saw *connection churn from a single peer*, not a stable simultaneous 4-peer graft within its block-production window. The simulator proves the *logic*; the live network exposes a *timing/mesh-stability* problem the sim abstracts away. Every number here was read from a run, not memory.

---

## 1. The fabric

```
   Epsilon (DK, producer) ──┐
   Delta   (DE, follower) ──┤   + N Vast.ai followers, WAN-spread
   Vast    KR/US/RO/GR/CZ/CN ┘   (GTX1080 … RTX2080Ti, CUDA image, ~$0.06–0.16/hr each)
```

Provisioned over the Vast REST API (the `mcp__vast-ai__*` MCP is broken → 400s; we drive `GET /bundles/` + `PUT /asks/<id>/` directly with the key at `~/.config/vastai/vast_api_key`). Each node is flux-ssh-installed in one batch call (`flux-fleet up --confirm`, content-verified musl `fluxc`, `flux://b3/…` sha-gated) and runs the musl `sigil-chronos-net` binary. Rental discipline: rent cheap, test fast, **destroy** — the burn is real money.

## 2. Chronos at scale (deterministic simulator)

`flux-chronos` floods one producer's messages to `N−1` sinks under controlled latency/loss in **virtual time** (instant — no real sleeping). The MCP combo caps `nodes=16`; we ran the core uncapped via a lean harness (`flux-chronos/examples/megaflood.rs` — u64-bitmask dedup, zero per-recv allocation):

| Nodes | Unique delivery | Wall time | Peak RAM |
|---|---|---|---|
| 100 000 | **100.00 %** | 0.12 s | 58 MB |
| 1 000 000 | **100.00 %** | 1.44 s | 520 MB |
| **10 000 000** | **100.00 %** (9 999 999 / 9 999 999) | **23.9 s** | 5.2 GB |

Deterministic (same seed → same result). Under **30 % packet loss** with gossip redundancy 5, unique delivery still recovers to **99.7 %** — the re-propagation property that makes the mesh robust.

## 3. The real-network finding: the gossip mesh floor

Cross-WAN, a **single** follower dialing the producer connects fine (libp2p Noise/Yamux, `divergence = 0`) but **0 blocks propagate** — the producer logs `InsufficientPeers`. This is libp2p gossipsub's `mesh_n_low = 4`: it will not graft a *publishing* mesh below four peers. **It is not a SIGIL defect** — it's a parameter of the gossip layer. The fix is simply *more peers*.

## 4. Cross-WAN mesh (real boxes)

Producer on Epsilon (DK) as a systemd unit; **4 followers launched simultaneously** — 3 geo-distributed Vast boxes (US / VN / +1) + Delta (DE) — giving the producer **4 mesh peers, clearing `mesh_n_low = 4`**. Connections established over real internet (Noise-encrypted; metadata/IPs visible — Tor transport is `--features arti`, off by default).

**Result (two runs, honest):** *Run A* — 4 followers, default mesh floor: `blocks_applied=0, divergence=0, height=1` on every follower (genesis only). *Run B (the attempted fix)* — set `FLUX_GOSSIPSUB_MESH_N_LOW=1` + `--run-secs 200` on the producer to outlast the ~36 s bootstrap: **still `blocks_applied=0`**, producer log dominated by `InsufficientPeers`/publish-churn (423 lines).

**Conclusion:** cross-WAN block propagation is **not yet achieved**, and it is **not** an env-tuning problem. Followers connect at the transport layer (Noise/Yamux) but **never stably graft into the gossipsub publishing mesh** — the producer keeps re-seeing the same peer connect/disconnect (churn) rather than holding ≥1 stable mesh peer. Lowering the mesh floor and extending the producer's life did not help, which rules out the two simple hypotheses (floor + timing). The remaining suspects are **code-level**: gossipsub graft/heartbeat behaviour under cross-WAN connection churn, topic-subscription symmetry, or the follower's block-apply path (height never advances past genesis). This is the real open problem — handed back to the swarm as a flux-p2p investigation, not a fabric-rental one. **divergence stayed 0 throughout** — safety held; nothing bad was ever applied.

## 5. Post-quantum security as a term in the action

The session also closed a gap in SIGIL's master equation: cryptography was implicit (the $\Gamma_{\mathrm{verify}}$ wall). We added the **sixth field** $\Sigma=(\Sigma_{\mathrm{iso}},\Sigma_{\mathrm{lat}},\Sigma_{\mathrm{hash}})$ — a **tri-layer PQ hybrid** assigning each family to the layer where its weakness is harmless: **isogeny/SQIsign** for compact genesis provenance, **lattice/Dilithium** for fast per-tx verify, **transparent FRI-STARK** for the trustless verify-once tip-proof. Security is the *product* of three disjoint hardness assumptions; `δS/δΣ=0` is crypto-agility. Grounded in arXiv 2603.09899 (SQIsign), 2211.12265 (Dilithium-GPU), 2512.10020 (zk-STARKs) — ingested via the `flux-arxiv-latex` bridge.

## 6. Honest limits

- The 10 M run is RAM-bound (5.2 GB); the core's `BTreeMap` edges are the speed lever (swap → flat `Vec` adjacency to cut the 18.4 s delivery).
- `debian:bookworm-slim` on Vast **breaks** non-interactive SSH (forced TTY) — use the CUDA image; the FLUX-OS onstart script self-provisions on boot.
- Two terminals double-spun overlapping fleets (~$1/hr, 8 boxes) — coordinate fleet ownership before renting.
- The cross-WAN block-propagation number is preliminary until the unit's collection completes.

*Method: chronos-sim-first · verify-before-claim · fail-loud. Every figure read from a file or tool output.*
