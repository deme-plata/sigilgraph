# BLAKE4 — the flux-miner proof-of-work hash

> *What it is, why it has a "4" in the name, why it is the miner's hash and not
> the chain's hash, and how the dual-lane miner uses it.*
> Companion to `sigil/docs/flux-miner-design.md` and `SIGIL_FLUX_MINER_v0.md`.

---

## 1. The one-paragraph version

**BLAKE4 is not a new cryptographic hash.** It is flux-miner's name for **the
BLAKE3 hashing core, repurposed and parallelized specifically as a
proof-of-work function**. The "4" doesn't mean "BLAKE3 plus one round" — it
means "the mining-tuned member of the BLAKE family": same preimage-hard core,
but driven flat-out across every CPU core to produce *hashpower* (the **Φ**
lane), instead of being used once per block to fingerprint state. Same trusted
core, different job.

If you've used BLAKE3 to checksum a file, you've already used the engine inside
BLAKE4. BLAKE4 is just "BLAKE3, but we run it a billion times a second and the
*rate* is the product."

---

## 2. The kitchen analogy

Think of BLAKE3 as a **very fast, very honest stamping press**. You feed it any
document and it stamps a unique, unforgeable seal. You cannot work backwards
from a seal to a document (preimage-hard), and you cannot find two documents
with the same seal (collision-resistant).

The chain uses that press **once per block** — to seal the four state roots.
There, speed barely matters: you stamp four things and you're done. Making the
press 80× faster saves you nothing, because you weren't stamping very much.
(We measured this — see §5 "dead for roots".)

**Mining is the opposite job.** Mining says: *"keep stamping the same document
with a different serial number until a seal comes out that starts with enough
zeros."* Now the press runs *continuously*, millions of times a second, and the
**number of stamps per second IS your mining power.** Here a faster press is
worth everything. That continuous-stamping mode of the press is what we call
**BLAKE4**.

---

## 3. What the code actually does

`flux-miner/src/lib.rs`:

```rust
// One BLAKE4 evaluation over header || nonce; the first 8 bytes are the
// target word a miner drives below the difficulty target.
pub fn blake4(header: &[u8], nonce: u64) -> u64 {
    let mut h = blake3::Hasher::new();
    h.update(header);
    h.update(&nonce.to_le_bytes());
    let b = h.finalize();
    u64::from_le_bytes(b.as_bytes()[0..8].try_into().unwrap())
}
```

So one "BLAKE4 hash" = `BLAKE3(header ‖ nonce)`, read back as a 64-bit number.
A miner increments `nonce` until that number is `<= target`. Lower target =
fewer winning nonces = more stamps needed = higher difficulty. **The work is
preimage-hard because BLAKE3 is** — there is no shortcut; you must try nonces.

The node verifies a found share by re-hashing the claimed nonce *once*
(`verify_dual` in the same file): O(1) to check, O(huge) to find. That
asymmetry is the whole point of proof-of-work.

---

## 4. The FLUX unit (Φ) — measuring BLAKE4 power

Because the hash-rate is the product, flux-miner gives it a unit:

> **1 Φ (flux) ≡ 1 EH/s = 10¹⁸ hashes/second.**

The SI prefix on Φ is the hash-rate prefix shifted by 18, which makes it easy to
hold in your head:

| You see | Means | i.e. |
|---|---|---|
| `1 nΦ` (nanoflux) | 1 GH/s | a strong single rig |
| `1 µΦ` | 1 TH/s | |
| `1 mΦ` | 1 PH/s | |
| `1 Φ` | 1 EH/s | the network at exascale |
| `1 pΦ` | 1 MH/s | a laptop core or two |

`format_flux()` in flux-miner renders this. When you run `sigil-miner` on a
laptop you'll see numbers in the **pΦ** band — that's correct, a few MH/s.

---

## 5. Why BLAKE4 is the *miner's* hash, not the *chain's* hash

This is the verdict that justifies BLAKE4 existing at all (measured 2026-05-31,
48-core box — `flux_blake4.rs` / `roots_throughput.rs`; do not relitigate):

- **For state roots: a faster hash is worth ~0×.** SIGIL commits roots with an
  O(1) incremental multiset accumulator (see `project_sigil_supply_cap`), so the
  root computation doesn't depend on raw hash speed. Making the hash 80× faster
  changed roots throughput by nothing. **BLAKE4 is dead for roots.**
- **For mining: a faster hash is worth ~83×.** The hash *is* the product, so
  every bit of speed is mining power.

Measured rates:

| Variant | Rate | Φ | Status |
|---|---|---|---|
| **BLAKE4-sound** (full BLAKE3 core, all cores) | **155 MH/s** | 155 pΦ | ✅ deployable PoW (preimage-hard) |
| BLAKE4-turbo (fast invertible mix) | 12.9 GH/s | 12.9 nΦ | ⚠️ ceiling only — **NOT** deployable (invertible ≠ secure) |

The 83× gap between sound and turbo is the **research headroom**: a
preimage-hard hash *faster* than full 7-round BLAKE3 (reduced-round / SIMD-batched,
behind a crypto-agility flag) would capture real mining throughput without
weakening the security. GPU pushes the *sound* rate straight into the GH/s (nΦ)
band. That is the open BLAKE4 lane.

---

## 6. BLAKE4 is only half the miner — the dual lane

A valid flux-miner / sigil-miner block needs **two** independent proofs:

```
        Lane A — BLAKE4  (Φ, POWER)            Lane B — VDF  (Ω, TIME)
   ┌──────────────────────────────┐    ┌──────────────────────────────────┐
   │ parallel hashes/sec           │    │ t sequential squarings y=x^(2^t)  │
   │ hardware-buyable, ~scales     │    │ CANNOT be parallelized            │
   │ with cores → throughput       │    │ one fast core ≈ one vote          │
   │ unit: Φ (flux)                │    │ unit: Ω (1 Ω = 1 Mega-turn/s)     │
   └──────────────────────────────┘    └──────────────────────────────────┘
        "power can't fake time   ·   time can't fake power"
```

- **Lane A (BLAKE4, Φ)** — this document. Buyable throughput; provides liveness.
- **Lane B (VDF, Ω)** — a Wesolowski verifiable delay function (`flux-vdf`):
  `t` sequential squarings mod N. It cannot be sped up by adding cores (measured:
  48 parallel VDF chains each ran at the *same* ~29 mΩ as one — the anti-parallel
  proof), so it is grind-proof and ASIC-resistant, and provides fair, egalitarian
  proof of *elapsed time*.

An attacker must win **both** lanes: lots of power doesn't manufacture elapsed
time, and a fast clock doesn't manufacture hashes. `verify_dual()` checks both;
either one failing rejects the share.

---

## 7. How `sigil-miner` shows it

`sigil-miner <wallet> [node-url]` runs the dual-lane loop against a SIGIL node's
`/api/v1/mining/{challenge,submit}` endpoints and renders a TUI with both lanes:

- **Φ — POWER (BLAKE4)**: live hash-rate of the nonce search (this hash).
- **Ω — TIME (VDF)**: the VDF turn-rate for the time lane.
- shares ✓ / ✗, balance, recent-shares log, solve-time sparkline.

`--headless` prints a plain log instead (CI / no TTY).

---

## 7½. BLAKE4 is now a real primitive — `flux_miner::pow`

As of 2026-06-08 BLAKE4 is no longer "an alias for `blake3::hash`." The module
`flux-miner/src/pow.rs` implements the BLAKE3 compression **from scratch with the
round count as a parameter** (`blake4_rounds(input, R)`, `blake4_word(header,
nonce, R)`):

- **R = 7 (`FULL_ROUNDS`) is byte-identical to BLAKE3** — proven by a
  known-answer test against the `blake3` crate (`pow::tests::
  r7_is_byte_identical_to_blake3`, plus a check that the word extractor matches
  the legacy `blake4()`). This is the soundness anchor: reduced-round variants are
  *the same function with fewer rounds*, not a different hash.
- **R < 7 is the real speed lever.** Measured (scalar reference impl, 48 cores,
  `examples/blake4_rounds.rs`):

  | R | hashrate | ×R=7 | |
  |---|---|---|---|
  | 7 | 4.97 MH/s | 0.95× | BLAKE3, KAT-verified (sound anchor) |
  | 5 | 6.47 MH/s | 1.24× | reduced |
  | 3 | 9.44 MH/s | 1.81× | reduced |
  | 1 | 15.22 MH/s | 2.92× | reduced |

  Roughly linear in rounds, as expected (each round is ~equal work).

- **Crypto-agility, no consensus break.** `BLAKE4_ROUNDS` stays at `FULL_ROUNDS`
  (= BLAKE3) so the live PoW path is unchanged. Promoting a reduced `R` is a
  deliberate, gated consensus decision once a round count is shown to keep enough
  preimage margin.

Two **independent** speed levers compose: **fewer rounds** (this module) **×
SIMD batching** (the blake3 crate already gets ~31× via AVX-512; the scalar
numbers above are the per-round curve, not the deployable ceiling). The deployed
hash would be SIMD × a validated R.

## 7⅗. Which reduced R is safe? — the diffusion measurement (2026-06-21)

§7½ measured the *speed* of each round count but left the *security margin*
unmeasured — "which `R` is sound enough is an empirical question the bench loop
answers." This is that answer. `pow::blake4_avalanche(R, samples)` runs a
strict-avalanche-criterion (SAC) probe: flip every input bit of many random
`header‖nonce` inputs and measure (a) the **mean** fraction of the 256 output bits
that change (ideal **0.5**) and (b) the **worst single-output-bit bias**
`|P(flip)−0.5|` (ideal **0** — a biased output bit is a grind handle even when the
mean looks fine). Gated `#[cfg(any(test, feature="bench"))]`, deterministic, no deps.
Regression-locked by `pow::tests::avalanche_curve_quantifies_round_security_margin`.

Measured (48 samples = 15,360 single-bit trials/round; R=7 also at 192 samples):

| R | mean (→0.5) | max_bias (→0) | speed (§7½) | verdict |
|---|---|---|---|---|
| 1 | **0.366** | **0.200** | 2.92× | **BROKEN** — 37% diffusion, one output bit 0.20 off ideal |
| 2 | 0.500 | 0.012 | ~1.5× | SAC **saturated** (zero margin above the floor) |
| 3 | 0.500 | 0.012 | **1.81×** | ideal SAC + a 1-round cushion |
| 4–6 | ≈0.500 | 0.011–0.014 | — | ideal (noise floor) |
| 7 | 0.500 | 0.011 (0.007 @192) | 0.95× | BLAKE3, KAT-anchored |

**Finding:** BLAKE3's `G` + message permutation reach **full strict-avalanche in
just 2 rounds** — the diffusion *floor* is R=2. R=1 is provably unusable; the
tempting 2.92× lever is exactly the broken one.

**SAC is necessary, not sufficient.** It does not rule out the higher-order
differential / algebraic-degree weaknesses that reduced-round BLAKE/ChaCha-family
permutations are known to have (those attacks bite well above where avalanche
saturates). So SAC sets a hard *floor*, not a green light.

### The PoW grind-bias survey (2026-06-22)

SAC probes the full 256-bit digest under arbitrary input flips — but a miner only
ever reads the **64-bit target word** (`blake4_word`, compared to difficulty) and only
moves by **nonce increment**. `pow::blake4_grind_bias(R, nonces)` measures soundness on
exactly those two surfaces: the worst **target-word monobit bias** (is the difficulty
surface uniform?) and the **consecutive-nonce avalanche** (are grind attempts
independent, or do near-target words cluster?). Regression-locked by
`pow::tests::grind_bias_survey_targets_the_pow_word`.

Measured (16,384-nonce sweep/round; R=7 also at 65,536):

| R | word_monobit_bias (→0) | nonce_step_avalanche (→0.5) | verdict |
|---|---|---|---|
| 1 | **0.500** | **0.075** | **DEGENERATE** — a target-word bit is *constant*; consecutive nonces change 7.5% of the word → near-wins cluster → trivial grind |
| 2 | 0.010 | 0.500 | clean on both PoW surfaces |
| 3–6 | 0.006–0.016 | 0.500–0.501 | clean |
| 7 | 0.010 (0.006 @65k) | 0.500 | BLAKE3 |

This is a **sharper kill of R=1** than SAC: not "weaker diffusion" (mean 0.366) but
*degenerate on the exact surfaces a miner exploits* — a constant target-word bit and
near-identical consecutive words. Both instruments converge: **the floor is R=2.**

### The differential survey (2026-06-22)

SAC measures *single-bit* input differences (averaged); grind-bias measures the
*nonce-increment* difference. Neither reports a **low-weight multi-bit input difference
that propagates quietly** — a differential *trail*, the classic ARX/BLAKE attack class.
`pow::blake4_diff_bias(R, samples)` sweeps a Δ set (nonce single-bit + pseudo-random and
rotation-aligned 2-bit) and reports the **worst-case (minimum) output-differential
avalanche** over that set — ideal ≈ 0.5 for *every* Δ; a Δ whose avalanche collapses is a
trail. Regression-locked by `pow::tests::differential_survey_finds_no_trail_at_full_rounds`.

Measured (128 samples/Δ over ~160 Δ; R=7 also at 256):

| R | min Δ-avalanche (→0.5) | worst Δ weight | verdict |
|---|---|---|---|
| 1 | **0.023** | 1 | **dominant trail** — a Δ produces ~2% output change |
| 2–6 | 0.492–0.494 | 1–2 | no trail in the Δ set |
| 7 | 0.495 (0.495 @256) | 1 | BLAKE3 |

R=1's worst-case differential (0.023) is even starker than its SAC *mean* (0.366) — the
*min* exposes the worst Δ, not the average. R≥2 is clean across single-bit AND multi-bit Δ.

**⚠️ Honest scope:** this is a **sampled screen**, not an exhaustive differential trail
search. It catches a *dominant* trail (and did, at R=1), but absence of a trail in ~160
sampled Δ is **not a proof** of differential security — a real promotion still needs an
automated trail search (SAT/MILP, the tooling that broke reduced ChaCha/BLAKE) over the
full difference space.

### The automated trail search — rigorous core (`diff_search`, 2026-06-22)

The screen above is empirical (random Δ, measured diffusion). The *analytic* gate is a
**differential trail search**, and `crates/flux-miner/src/diff_search.rs` builds its exact
foundation. BLAKE4 is Add-Rotate-XOR: rotation and XOR pass an XOR-difference through
deterministically, so the only nonlinear gate is modular addition, whose XOR-differential
probability is given EXACTLY by **Lipmaa-Moriai (2001)**.

- `xdp_add_weight(α,β,γ,n)` — exact weight (−log₂ prob) of `(α,β→γ)` through `+`.
  **Brute-force-verified** against the exhaustive truth over *all* (α,β,γ) at n=6, and
  **Monte-Carlo-verified at the real n=32** (modelled 2⁻ʷ == measured probability).
- `best_xdp_add(α,β)` — the optimal (min-weight) output difference, proven equal to an
  exhaustive γ-scan at n=8.
- `g_best_trail(...)` + `min_active_g_weight()` — greedily compose the exact `xdp+` through
  one BLAKE4 `G` (two adds + the rotate/XOR layer).

**Result:** the cheapest a *single active* `G` can be is **weight 7** (an MSB message-bit
difference). Monte-Carlo on the real `G` measured that trail at 2⁻⁶ vs the modelled 2⁻⁷ — ~1
bit more probable than the greedy trail, the expected **differential-clustering** signature
(several trails reach the same output diff). So: the per-add core is *exact*; composing it
through `G` is *approximate* (~1 bit optimistic), the well-known trail-model caveat.

**Why this corroborates R≥2.** One active `G` costs ~6–7 bits, and the §7⅗ screen shows the
difference has activated the *whole state* (many G's) by R=2 — so the multi-round trail
weight blows past the 64-bit PoW window within a couple of rounds. Consistent with the
empirical "no trail at R≥2."

#### Multi-round trail engine (`message_trail_weight` / `best_single_bit_trail`)

The single-G core is now propagated through the **real** BLAKE4 round (the 8-G column+diagonal
pattern + message permutation). With the chaining value identical (state difference 0), a
message difference flows through every `G`, accumulating exact `xdp+` weight. The engine's
wiring + probability are validated end-to-end (`round_engine_matches_real_round_mc`: the
predicted output difference occurs on the *real* round at ~the modelled rate). Best greedy
single-bit-message attack trail, by round count:

| rounds | best greedy trail weight | vs 64-bit PoW window |
|---|---|---|
| 1 | 1 | trivially below (R=1 is broken anyway) |
| 2 | **199** | **3.1× past** |
| 3 | 803 | 12.5× past |

(A column-start difference is far more expensive even at R=1 — e.g. weight 88 — because it
feeds all four diagonal G's in the same round.)

**⚠️ Read the semantics correctly.** Greedy takes the locally cheapest output difference at
each add, so its weight is an **upper bound** on the optimal trail: "a trail no worse than this
exists." It finds *attacks*; it does **not** prove their absence — a cleverer trail could be
cheaper, so the weight-199 at R=2 is *suggestive corroboration* of the screens, **not** a
security proof. The matching **lower bound** (proving no cheap trail exists) is what the
Matsui/SAT enumeration provides. What the engine shows *soundly* is the steep weight growth as
the difference saturates the state (1 → 199 → 803).

#### Matsui branch-and-bound — the lower-bound search (`matsui_toy` / `matsui_g_min`)

The lower-bound direction is now built. **Matsui's algorithm** finds the EXACT minimum-weight
trail (= a proof "nothing is cheaper") by depth-first branch-and-bound: enumerate each addition's
output differences *cheapest-first* (`enum_gamma`, the Lipmaa-Moriai "country roads"), and prune a
partial trail when its spent weight plus the best achievable for the remaining rounds (the
inductive bound `B[r]`) can't beat the incumbent.

- `enum_gamma` — the increasing-weight transition enumerator. **Verified exact** against a brute
  γ-scan over all (α,β) at n=6, every cap.
- `matsui_toy` — the full Matsui search on a 1-word ARX toy (round `δ ↦ (δ⋙rot)+dk`). **Verified
  `== brute force`** across rotations, round-differences, widths and round counts — so the
  branch-and-bound provably computes the true minimum (the lower bound), not just a good trail.
- `matsui_g_min` — the exact search applied to one BLAKE4 `G`. For the cheapest active-G input it
  returns a **proven minimum of 7** — equal to the greedy upper bound, so greedy happened to be
  tight there (now *proven*, not assumed). An inactive `G` is proven to cost 0.

**⚠️ Honest scope.** This is a *correct, verified* lower-bound engine, but `matsui_g_min` is
tractable only for low-weight (small-seed) inputs — an expensive input explodes the first
enumeration. Scaling it to the **full 16-word, multi-round BLAKE4 state** (proving the R-round
minimum exceeds 64 for *every* message difference) is the genuine research step: it needs the
Matsui `B[r]` bounds threaded through the whole compression, and in practice that is where one
reaches for a **SAT/SMT solver** (encode the Lipmaa-Moriai validity + a weight-≤-W cardinality
constraint; UNSAT at W=63 proves the bound) — at the cost of an external-solver dependency. The
verified per-add enumerator + branch-and-bound + the toy proof are the foundation that search
would be built on.

#### SMT lower bound — z3 run (`tools/blake4_diff_z3.py`, 2026-06-22)

The SAT/SMT path was built and run. `tools/blake4_diff_z3.py` encodes the trail in z3's
bit-vector theory — the SAME round structure as the verified Rust engine, each modular addition a
free output-difference variable under the Lipmaa-Moriai validity constraint, weight = popcount of
the non-equal bits — and minimizes (or threshold-checks) the total. The encoding is **validated
before use** (`tools/blake4_z3_validate.py`): z3's `valid`/`weight` == the brute-verified
closed-form `xdp+` (exhaustive n=6 + 4000 random 32-bit triples).

Results (z3 4.16, capped on Epsilon):

| query | result |
|---|---|
| **R=1** minimize | **exact minimum = 1** (0.4 s) — matches the Rust engine; R=1 provably broken |
| **R=2** minimize | did not converge in 20 min — **proven bounds [9, 2389]** (min ≥ 9 is sound) |
| **R=2** "trail ≤ 64?" | **unknown** (15-min timeout) — neither a cheap trail found nor its absence proved |

So R=1 is a *complete* proof; R=2 is partial — z3 proves the minimum is ≥ 9 and ≤ 199 (the greedy
trail), but the *security-grade* claim (min > 64) is **not reached in tractable time**. This is the
honest frontier: published BLAKE-class multi-round differential bounds are computed with
cluster-scale dedicated SAT solvers (CryptoMiniSat + optimized cardinality) or MILP, not a 20-min
z3 Optimize. The prover + validator are in-tree and reproducible; closing R=2 needs either a much
larger solve, a dedicated SAT toolchain, or the convex-hull MILP model of `xdp+`.

**Bottom line for promotion.** The evidence — SAC, PoW grind-bias, the empirical differential
screen, the exact `xdp+` trail core, the greedy multi-round engine, AND a *verified Matsui
lower-bound search* — all agree: floor **R=2**, R=1 unusable, **R=3** the candidate (~1.81×). The
upper-bound side (find an attack) and the lower-bound machinery (prove none cheaper) both exist
and are brute-verified — and the SAT/SMT path was built and **run** (validated z3 encoding; R=1
minimum proved =1, R=2 proved ∈[9,199]). The ONE thing still between here and moving
`BLAKE4_ROUNDS` off 7: pushing that lower bound to the **security grade** — proving the R=2 (or
R=3) minimum > 64 over *every* message difference — which did **not** converge in a 20-min z3
solve and needs a larger/dedicated SAT-MILP run, plus a **real-GPU grind validation**. Until that
proof lands, `BLAKE4_ROUNDS` stays at 7 — zero consensus change.

## 7¾. BLAKE4 on the GPU — Lane A → GPU (scaffold, 2026-06-08)

The dual lanes map cleanly onto the two kinds of hardware, which is the third
speed lever:

```
   Lane A — BLAKE4 (Φ, POWER)  → GPU   (millions of independent nonces in parallel)
   Lane B — VDF    (Ω, TIME)   → CPU   (inherently sequential — a GPU cannot help)
```

So the GPU does exactly what it is good at (the embarrassingly-parallel nonce
search) and the CPU does the one thing that *must* be sequential (the VDF).

- **Kernel — `flux-miner/src/gpu/blake4.cl`.** One work-item per nonce; a
  **byte-for-byte port of `pow::compress8`** (same IV, message schedule, G mix,
  flags, single ≤64-byte block). It carries the same `rounds` parameter, so the
  GPU has the identical round-count dial as the CPU.
- **On-hardware KAT — `sigil-miner --gpu-selftest`.** Runs the kernel for 256
  nonces at R=7 and R=3 and asserts every word equals the CPU `pow::blake4_word`.
  ✓ means the OpenCL kernel is byte-correct on *that* GPU; only then is GPU mining
  trustworthy. This is how a port is proven, not assumed.
- **Hybrid mining — `sigil-miner --gpu`.** GPU `search()` finds a Lane-A nonce →
  `flux_miner::block_for_nonce` runs the CPU VDF (Lane B) over it → submit. Uses
  `FULL_ROUNDS` so shares pass the node's `verify_dual` (the live `blake4` ==
  `pow` R=7); a reduced R needs a node-side promotion first.
- **Gated.** Behind the `gpu` Cargo feature (default OFF → the normal build needs
  no OpenCL). OpenCL is the first backend (most portable); CUDA / Vulkan (the QUG
  q-miner has both) are follow-ons.
- **Validate on:** any OpenCL GPU. First target = a Windows RTX 2060 — recipe in
  [`SIGIL_MINER_GPU.md`](SIGIL_MINER_GPU.md).

## 7⅞. Measured live — CPU miner on Epsilon (2026-06-08)

`sigil-miner` (CPU, headless) against the live sigil-rpcd `:8099` node at the
production difficulty (16 leading-zero bits, vdf_t 600): **48 dual-lane shares
accepted, 0 rejected**, ~130 ms/share on one scalar thread, balance climbing
50/share. End-to-end proof that the full loop — challenge → BLAKE4 nonce search →
VDF → submit → cap-enforced credit — works on real binaries.

## 8. Honest checklist — what's still pretend

- **BLAKE4-turbo is a ceiling, not a product.** The 12.9 GH/s number is an
  *invertible* mix used only to measure the headroom. Don't quote turbo as a
  real rate.
- **No reduced-round `R` is deployed yet.** The primitive, speed curve, three empirical
  soundness instruments, the exact Lipmaa-Moriai `xdp+` core, the greedy multi-round engine AND
  a brute-verified Matsui lower-bound search now exist (§7⅗: all agree floor=R=2, R=1 degenerate,
  R=3 candidate). The remaining gate before `BLAKE4_ROUNDS` moves off 7 is running that
  lower-bound search at **full 16-word/multi-round scale** (proving the R-round minimum > 64 for
  every message Δ — in practice a SAT/SMT-assisted job on this in-tree foundation) + a real-GPU
  grind validation. Everything built so far is a floor, not yet the green light.
- **`pow.rs` is scalar.** The per-round curve is honest but un-SIMD'd; the
  deployable rate is SIMD (blake3-crate-class) × the chosen R. SIMD is the
  flux-cortex/flux-optimize lever.
- **GPU is scaffolded, NOT yet validated.** The OpenCL kernel + `--gpu`/`--gpu-list`/
  `--gpu-selftest` exist and the default build is green, but no `gpu`-feature code
  has been compiled or run — Epsilon has no GPU. `--gpu-selftest` on a real GPU
  (RTX 2060 next) is the gate before any GPU rate is quoted.
- **VDF absolute rate is num-bigint-limited.** `flux-vdf` uses pure-Rust bigints,
  slower than a GMP / genus-2 Jacobian implementation. The *sequential character*
  (no speedup from cores) is the real result; the absolute mΩ is not the ceiling.

- **BLAKE4-turbo is a ceiling, not a product.** The 12.9 GH/s number is an
  *invertible* mix used only to measure the headroom. Don't quote turbo as a
  real rate.
- **No reduced-round `R` is deployed yet.** The primitive, speed curve, three empirical
  soundness instruments, the exact Lipmaa-Moriai `xdp+` core, the greedy multi-round engine AND
  a brute-verified Matsui lower-bound search now exist (§7⅗: all agree floor=R=2, R=1 degenerate,
  R=3 candidate). The remaining gate before `BLAKE4_ROUNDS` moves off 7 is running that
  lower-bound search at **full 16-word/multi-round scale** (proving the R-round minimum > 64 for
  every message Δ — in practice a SAT/SMT-assisted job on this in-tree foundation) + a real-GPU
  grind validation. Everything built so far is a floor, not yet the green light.
- **`pow.rs` is scalar.** The per-round curve is honest but un-SIMD'd; the
  deployable rate is SIMD (blake3-crate-class) × the chosen R. SIMD is the
  flux-cortex/flux-optimize lever.
- **GPU is not wired.** All rates are CPU. GPU is the next lever for the Φ lane.
- **VDF absolute rate is num-bigint-limited.** `flux-vdf` uses pure-Rust bigints,
  slower than a GMP / genus-2 Jacobian implementation. The *sequential character*
  (no speedup from cores) is the real result; the absolute mΩ is not the ceiling.

---

*Baseline measured numbers from `project_flux_miner_blake4` (2026-05-31); the real
`pow` primitive, GPU scaffold, and live CPU run added 2026-06-08
(`project_sigil_dual_lane_mining_wired`). Engine: `flux-miner` crate (sigil
workspace) — `pow.rs` (CPU, parameterized rounds), `gpu/blake4.cl` (OpenCL),
`sigil-miner` (TUI). Unit definitions are LOCKED — don't re-derive Φ or Ω. See
also [`SIGIL_MINER_GPU.md`](SIGIL_MINER_GPU.md).*
