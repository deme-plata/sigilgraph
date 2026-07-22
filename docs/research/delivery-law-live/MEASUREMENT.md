# Delivery Law — Live-Network Measurement Record (2026-07-22)

**Purpose.** Promotion experiment for the gossip delivery law `D(p,r) = 1 − p^r`
(sigil-top-delivery-law.pdf, 2026-06-16), per the promotion condition stated in
whitepaper v1.2: *"promotion to measured requires live-network delivery data or an
independently controlled experiment on a real lossy network."* This record is the
second: a controlled experiment on a real lossy network — real binaries, real
kernel-induced loss, no simulator anywhere in the loop.

## What was run

- **Primitive under test:** the exact shipped redundancy primitive — one
  "transmission" = one flux-p2p request/response (`/sigil/backfill/1`,
  `NetworkManager::send_request`) to one peer; redundancy r = parallel requests to r
  distinct peers, delivery = ≥1 reply within the shipped `REQ_TIMEOUT = 10 s`
  (`sigil-top/src/block_sync/mod.rs`, `FRONTIER_REDUNDANCY = 3`).
- **Harness:** `crates/delivery-probe` (client + minimal responder speaking the real
  protocol; responses 70,700 B ≈ one 1000-header frontier chunk at the live
  70.7 B/header compressed size). Orchestration: `scripts/dp-lab.sh`,
  `scripts/dp-sweep.sh`; analysis: `scripts/dp-analyze.py`.
- **Network:** 8 peer processes in Linux network namespaces on a private bridge
  (10.99.99.0/24), each peer's veth degraded by **kernel netem in both directions**.
  Real TCP, real retransmission, real timeouts. Production interfaces untouched.
- **Conditions** (each: r ∈ {1,2,3} × 600 trials, concurrency 16):
  `c0` = delay 20 ms; `c10` = +10% i.i.d. loss; `c25` = +25% i.i.d. loss;
  `c40` = +40% i.i.d. loss; `ge30` = Gilbert–Elliott burst loss
  (`gemodel 15% 35%`, ≈30% average, mean burst ≈2.9 packets) — the correlated-loss
  condition the delivery-law paper pre-registered as its falsification test.
- **Totals:** 9,000 trials, 18,000 request attempts. Raw per-attempt records:
  `*.jsonl.gz` in this directory; per-run summary: `summary.txt`.

## Method note (what tests what)

p̂ is **measured, never assumed**: it is the observed per-attempt failure fraction of
each run. r=1 runs are p̂ calibration (D̂ ≡ 1−p̂ by construction — not a law test).
The law's testable content is the **composition across peers**: whether r parallel
attempts to distinct peers fail independently, i.e. whether the r=2 and r=3 runs
match `1 − p̂^r` using their own measured p̂. p̂ was **non-stationary between runs at
high loss** (the mesh degrades over time), so within-run comparison is the valid
test; the six informative cells are (c25, c40, ge30) × (r=2, r=3).

## Results (per-run, RESULT lines from summary.txt)

| cond | netem | p̂ (r1 run) | r=2 residual | r=3 residual |
|---|---|---|---|---|
| c0 | 20 ms, 0% loss | 0.0000 | +0.00 pp | +0.00 pp |
| c10 | 20 ms, 10% loss | **0.0000** | +0.00 pp | +0.00 pp |
| c25 | 20 ms, 25% loss | 0.0550 | +0.13 pp | +0.02 pp |
| c40 | 20 ms, 40% loss | 0.8733 (→0.94 by r3 run) | −0.76 pp | **−3.02 pp** |
| ge30 | 20 ms, GE burst ≈30% | 0.5117 (0.59–0.62 later runs) | +1.27 pp | +1.47 pp |

**RMS residual over the six informative cells: 1.50 pp. Max |residual|: 3.02 pp**
(collapsed-transport regime only). Binomial 1σ at n=600 is ≈0.9–2.0 pp depending on
D, so every cell except c40-r3 (≈2.2σ) is statistically consistent with the law.

## Findings

1. **The law's `p` lives at the request level, not the packet level.** Kernel packet
   loss ≤10% (both directions) produced p̂ = 0.0000 over 3,600 attempts — TCP
   retransmission absorbs it entirely within the 10 s timeout. At 25% packet loss,
   p̂ ≈ 5%; at 40% both-directions loss the transport collapses (p̂ ≈ 0.87→0.94).
   Operators applying `r* = ⌈ln(1−SLA)/ln p⌉` must measure p as request failures,
   not packet loss.
2. **Composition confirmed in every functioning regime.** With request failures
   actually occurring (p̂ ≈ 5%, ≈50–60%), r=2/r=3 delivery matched `1 − p̂^r` within
   +0.02…+1.5 pp — i.i.d. and burst conditions alike.
3. **Burst loss does not break cross-peer composition.** Per-link Gilbert–Elliott
   bursts raise p̂ but leave attempts on *distinct peers* independent (+1.3/+1.5 pp,
   within noise + peer heterogeneity). The pre-registered correlated-loss
   falsification attacked per-transmission correlation; redundancy across distinct
   links is robust to it by construction, and the measurement bears that out.
4. **At transport collapse the law degrades in the predicted direction.** At
   p̂ ≈ 0.94, delivery fell **below** independence (−0.8, −3.0 pp): failures cluster
   on dead connections shared across a trial's r-window (per-peer failure rates
   0.90–0.96, and p̂ drifting upward run-over-run). Positive within-trial
   correlation → sub-law delivery, exactly the failure mode the law's limitations
   section anticipated. The shipped r=3 default is not a safety mechanism at 40%
   both-directions packet loss; nothing at the gossip layer is.
5. **Peer heterogeneity is real but second-order** (per-peer p̂ spread e.g.
   0.52–0.72 under burst loss); the per-peer product refinement does not materially
   beat the pooled within-run law.

## Scope (stated, per house rules)

Single 48-core host, network namespaces + veth + netem (real kernel network stack;
not WAN); peers are minimal responders speaking the real wire protocol (not full
sigil-nodes); frontier-scale payloads (70.7 kB); i.i.d. and GE loss models. A
measurement on the WAN-scale live mesh remains open as the next extension — it
would sample real Internet loss processes but cannot sweep p.

## Verdict

The delivery law's composition claim is **measured** on a real lossy network at the
exact granularity it is deployed at, within the scope above: six informative cells,
RMS 1.50 pp, breakdown only at transport collapse and in the predicted direction.
Grade coordinates (two-axis taxonomy): evidence **measured (controlled real-network
experiment)** · deployment **live** (r=3 default). Reproduce: build
`delivery-probe`, then `scripts/dp-lab.sh setup` → start 8 servers →
`OUT=<dir> scripts/dp-sweep.sh`.
