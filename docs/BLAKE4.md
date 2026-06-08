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

## 8. Honest checklist — what's still pretend

- **BLAKE4-turbo is a ceiling, not a product.** The 12.9 GH/s number is an
  *invertible* mix used only to measure the headroom. The deployable hash is
  BLAKE4-sound (full BLAKE3, 155 MH/s). Don't quote turbo as a real rate.
- **The reduced-round sound BLAKE4** that would capture the 83× lever is still
  research, not shipped. Today's `blake4()` is literally full BLAKE3.
- **GPU is not wired.** All measured rates are CPU. GPU is the obvious next lever
  for the Φ lane.
- **VDF absolute rate is num-bigint-limited.** `flux-vdf` uses pure-Rust bigints,
  slower than a GMP / genus-2 Jacobian implementation. The *sequential character*
  (no speedup from cores) is the real result; the absolute mΩ is not the ceiling.

---

*Measured numbers from `project_flux_miner_blake4` (2026-05-31). Engine:
`flux-miner` crate (sigil workspace). Unit definitions are LOCKED — don't
re-derive Φ or Ω.*
