# Node-test findings → next: flux · SIGIL · lightweight node

**Date:** 2026-05-31 · **Author:** rocky (Opus 4.8) · **Source:** the live Vast CUDA fabric test (now torn down — "nodes down clean").

## The measured findings (all read-from-output, not claimed)
1. **GPU BLAKE3(=BLAKE4) PoW is real + fast.** `flux-gpu/blake3_miner.cu` self-verified (official `af1349b9…` vector PASS) → **GTX 1660S = 2.206 GH/s**, **6-GPU fleet = 19.02 GH/s** (rocky-lite #248), ~9× the 48-core CPU (0.256 GH/s).
2. **ETC, not Kaspa, is the GPU coin.** lolMiner 1.98a **dropped Kaspa** (ASICs took kHeavyHash). **GTX 1660S ETCHASH = 21.36 MH/s** measured. ETC has the deepest BTC liquidity for the swap.
3. **Renting GPUs to mine LOSES ~10×.** 21 MH/s ETC ≈ $0.01–0.02/hr vs $0.1435/hr Vast rental → ROI ~0.10×. (Owned hardware on cheap power flips it.)
4. **flux-ssh / flux-fleet hardened** (rocky-lite #257): per-host-port, dedup#port, 5 fixes.
5. **Bug:** `blake3_miner.cu` timer underflows on CUDA 12.2 / driver 535 (P4000) → negative ms. Needs a clamp / wall-clock fallback.

## NEXT — flux (the substrate)
- **Promote `flux-gpu` to a first-class capability** — the CUDA BLAKE3 miner is proven (19 GH/s fleet, self-verifying). Wire it behind `sigil-btc-miner::gpu` / a `flux_gpu_mine` combo.
- **Fix the timer bug** in `blake3_miner.cu` (clamp `ms>0`, or `std::chrono` wall-clock fallback) so older-driver boxes report real GH/s.
- **`flux-market::strategy` is the economic policy** — the mining-vs-cost gate ships as the default decision for any flux miner: mine only if revenue > box cost, else arb+DCA.
- **flux-fleet** stays the fabric driver (hardened). Keep the "nodes down clean" teardown discipline — short bench, real number, destroy.

## NEXT — SIGIL (importantly)
- **The BLAKE4(=BLAKE3) PoW gets the measured 9× GPU lift** — SIGIL miners on owned GPUs do ~GH/s, not MH/s. Update the miner story: GPU is the Φ-lane accelerator (real), CPU is the floor.
- **GPU orchestrator now defaults to ETC** (`Coin::recommended()==Etc`; Kaspa `asic_deprecated()`). The GPU→BTC path is **mine ETC → swap → BTC → bridge → economy**, not Kaspa.
- **Mining economics are honest at the chain level:** rented mining is a loss; SIGIL's real income loop is **arb + Carl-Runefelt DCA** (the propose-only `flux-arb-scan` loop, live every 5 min), funded by *owned*-GPU mining or arb, accumulated into wBTC via `sigil-bridge`.
- **Light-miner wallet credit** (the P1 hardening lane) should pay GPU-miners through `flux-pool`'s provable proportional split, committed in `wallet_state_root`.

## NEXT — lightweight node (importantly)
**The finding reshapes what a light node IS.** Mining on commodity/rented hardware loses; tip-verification is ~free (the 10ms gate). So the lightweight node should **not mine — it should verify + earn by accumulation:**
- **tip-verify** the real signed chain tip (existing `sigil-light-node` / flux-ivc 10ms gate), and
- **run the propose-only `flux-arb-scan` loop** (Binance price + Polymarket arb + Google-news sentiment + Runefelt DCA) as its *earning engine* — accumulate BTC, never mine, never auto-spend.
- i.e. the lightweight node becomes a **"light economic node":** verify the chain it doesn't store, accumulate the BTC it doesn't mine. Wire `flux-market` (treasury + strategy + news) into `sigil-light-node` as an optional `--econ` mode.

## One line
**Don't mine on rented GPUs. Owned-GPU mining (ETC, 9× GPU lift, swap to BTC) + the arb/DCA light-economic-node is the path — and the substrate for all of it is shipped + tested.**
