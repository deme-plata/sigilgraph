# sigil-miner on Windows + NVIDIA GPU (RTX 2060)

Two ways to run the SIGIL standalone miner on a Windows box. The CPU build needs
no compiler; the GPU build (RTX 2060, OpenCL) you compile on the box.

The dual lanes map onto the hardware: **Lane A — BLAKE4 (Φ) → GPU** (parallel
nonce search), **Lane B — VDF (Ω) → CPU** (sequential). `--gpu` runs both per
share.

---

## 1. CPU miner — no build, run now

Download + run (the TUI dual-lane miner, BLAKE4 on CPU):

```powershell
# PowerShell
Invoke-WebRequest https://sigilgraph.quillon.xyz/downloads/sigil-miner-windows-x64.exe -OutFile sigil-miner.exe
.\sigil-miner.exe <YOUR_WALLET_64HEX>
#   --headless   plain log, no TUI
#   second arg   custom node URL (default http://sigilgraph.quillon.xyz:8099)
```

You should see Φ/Ω cards, shares ✓ climbing, and your balance rising.

---

## 2. GPU miner — build on the box (RTX 2060, OpenCL)

The RTX 2060 driver already ships the OpenCL **runtime** (`OpenCL.dll`). To
*link* you need the OpenCL **import lib** — the CUDA Toolkit provides it at
`%CUDA_PATH%\lib\x64\OpenCL.lib` (or install the Khronos OpenCL-SDK).

```powershell
# prerequisites: rustup (MSVC toolchain) + CUDA Toolkit (for OpenCL.lib) OR OpenCL-SDK
git clone https://github.com/deme-plata/sigilgraph.git
cd sigilgraph\crates\flux-miner

# make sure the linker can find OpenCL.lib (CUDA example):
$env:LIB += ";$env:CUDA_PATH\lib\x64"

# build with the gpu feature
cargo build --release --features gpu --bin sigil-miner
# → ..\..\target\...\release\sigil-miner.exe   (or .target-shared per workspace)
```

> Server-side we build via `fluxc` (never raw cargo). On a personal Windows
> *test* box cargo is the practical path; the open dogfood item is teaching
> `fluxc cross_compile` to link OpenCL for `x86_64-pc-windows-gnu` so we can ship
> a prebuilt GPU `.exe` too.

### Validate, then mine

```powershell
.\sigil-miner.exe --gpu-list        # should print: GPU: NVIDIA GeForce RTX 2060 · ... MB · ...
.\sigil-miner.exe --gpu-selftest    # on-hardware KAT — MUST print ✓ (GPU kernel == pow.rs)
.\sigil-miner.exe <YOUR_WALLET> --gpu
```

**`--gpu-selftest` is the important one**: it runs the GPU BLAKE4 kernel for 256
nonces at R=7 and R=3 and asserts each word equals the CPU reference
`pow::blake4_word`. ✓ means the OpenCL kernel is byte-correct on your hardware;
only then is `--gpu` mining trustworthy.

---

## Notes

- **`--gpu` mines at R=7** (full rounds) because that's what the node's
  `verify_dual` checks (the live `blake4` == `pow` R=7). The reduced-round speed
  lever (R<7) is real (see `docs/BLAKE4.md`) but needs a node-side promotion +
  preimage-margin analysis before shares at R<7 would be accepted.
- GPU does Lane A only; the CPU still computes the VDF (Lane B) per found share —
  that's by design (the VDF is anti-parallel).
- CUDA / Vulkan backends (the QUG q-miner has both) are follow-ons; OpenCL is the
  portable first backend.
- If `--gpu-selftest` ✗ on your hardware, that's a real kernel/endianness/driver
  bug to fix before mining — report the mismatch line it prints.
