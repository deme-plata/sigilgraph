# Flux Miner + BLAKE4 + the FLUX hashpower unit (Φ)

> Viktor (2026-05-31): "invent the flux miner, copied from Quillon's standalone
> miner+node; use BLAKE4 not BLAKE3, parallelized + optimized the Flux way; and
> invent a number-power so 1–5 GH/s reads in Flux numbers, vs an exahash."
> Core invented + MEASURED. — rocky-sigil 🟣

## The FLUX unit (Φ) — hashpower in Flux numbers

**Anchor: `1 Φ ≡ 1 EH/s` (one exahash).** SI prefixes on Φ then line up with the
hash-magnitude prefix shifted by 18 — so the conversion is memorable:

| FLUX | hashrate | who |
|---|---|---|
| 1 Φ | 1 EH/s (10¹⁸) | the network at exascale (the ceiling) |
| 1 mΦ | 1 PH/s (10¹⁵) | a large pool |
| 1 µΦ | 1 TH/s (10¹²) | a GPU farm |
| **1 nΦ** | **1 GH/s (10⁹)** | **a miner "starting around 1–5 GH/s" = 1–5 nΦ** |
| 1 pΦ | 1 MH/s (10⁶) | a CPU core or two |

> **nanoflux IS gigahash.** You start at a few **billionths of a Flux** and the
> network climbs toward **1 Φ**. (This box, sound PoW, sits at ~155 pΦ — about
> 1 / 6.4-billionth of an exahash.)

## BLAKE4 — the miner hash (the Flux way)

"BLAKE4" was judged **dead for state roots** (roots_throughput.rs: once roots are
O(1), a faster leaf hash buys ~nothing). But **mining is the opposite regime — the
hash IS the product.** Measured side-by-side on 48 cores (`flux_blake4.rs`):

| core | hashrate | FLUX | note |
|---|---|---|---|
| **BLAKE4-sound** (BLAKE3 core, all-cores) | **155 MH/s** | 155.5 pΦ | deployable PoW (preimage-hard) |
| **BLAKE4-turbo** (fast mix, ceiling) | **12.92 GH/s** | 12.9 nΦ | NOT sound (invertible) — the speed ceiling |

**The faster hash is an 83× lever for mining** (12.9 GH/s ÷ 155 MH/s) — exactly
where it was ~0× for state roots. That's the whole case for BLAKE4. The deployable
BLAKE4 today is BLAKE3-core + Flux all-cores parallelism (1 core 5 MH/s → 48 cores
155 MH/s). The 83× headroom is the **BLAKE4 research lane**: a hash that stays
preimage-hard (so PoW can't be shortcut) but is faster than full 7-round BLAKE3 —
i.e. a **reduced-round / SIMD-batched** construction behind the crypto-agility flag.
GPU mining (the design's `prover.rs` CPU+GPU) pushes the sound rate into the GH/s
(nΦ) band directly.

## How far are we — flux-miner status

Design: `sigil/docs/flux-miner-design.md` (chain-agnostic `flux/crates/flux-miner/`,
copies Quillon's standalone miner+node, fixes its 4 gaps). Status:

| piece | state |
|---|---|
| design (architecture + the 4 fixes) | ✅ done |
| **BLAKE4 PoW hash + parallel nonce search + difficulty** | ✅ **measured** (`flux_blake4.rs`: real `mine()` finds nonces below a leading-zero target across all cores) |
| **FLUX hashpower unit** | ✅ done + measured |
| client.rs (challenge/submit, chain-agnostic HTTP) | ⬜ next |
| updater.rs (gossipsub self-update) | ⬜ reuse `sigil-updater` (shipped) |
| mcp.rs (`flux_miner_start/_status/_stop/_tune/_combo`) | ⬜ first-class agent mining |
| provenance.rs (`.proof`-gated miners, anti-botnet) | ⬜ reuse the `.proof` path |
| VDF prover (CPU+GPU) | ⬜ port `flux-vdf` (q-vdf) |

So: the **hash + the number-power are real and measured**; the surrounding miner
(submit/update/MCP/VDF/provenance) is scaffolded in the design + reuses shipped
parts. Crate not yet cut at `flux/crates/flux-miner/`.

## Quillon → Flux (what we copy, what we fix)
Copy: the standalone miner+node split, challenge/submit loop, difficulty retarget,
downloadable binary. Fix (per design doc): MCP-first (agent-native mining), fluxc
quick-compile, gossipsub auto-update (kills the "wget the new link" ritual),
`.proof` provenance on the miner binary.

## Honest caveats
- BLAKE4-turbo is a **ceiling, not deployable** — a fast invertible mix; real PoW
  needs preimage-hardness. The 83× is the *headroom*, the deployable number is 155 MH/s (CPU).
- 155 MH/s is CPU/48-core on a loaded box; GPU + a reduced-round sound BLAKE4 are
  the two levers that move the deployable rate into the nanoflux (GH/s) band.
- Harness: `crates/sigil-state/examples/flux_blake4.rs`.
