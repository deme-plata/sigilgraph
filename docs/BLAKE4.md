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
- **No reduced-round `R` is deployed yet.** The primitive + curve exist and are
  validated; choosing the safe `R` needs a diffusion / preimage-margin analysis
  (avalanche, reduced-round attack survey) before `BLAKE4_ROUNDS` moves off 7.
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
- **No reduced-round `R` is deployed yet.** The primitive + curve exist and are
  validated; choosing the safe `R` needs a diffusion / preimage-margin analysis
  (avalanche, reduced-round attack survey) before `BLAKE4_ROUNDS` moves off 7.
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
