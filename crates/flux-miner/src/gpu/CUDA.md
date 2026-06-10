# BLAKE4 CUDA backend (Lane A / Φ-POWER) — native NVIDIA

`blake4_cuda.cu` is the advanced CUDA-core port of `pow::compress8` / `blake4_word`,
a faster sibling to the portable OpenCL kernel (`blake4.cl`). Tuned for NVIDIA SM:
`__funnelshift_r` rotate, ROUNDS as a **template** param (full unroll + folded
permutation), `__constant__` IV/header, `__launch_bounds__(256)`, grid-stride search,
`atomicCAS` winner claim.

## Soundness (self-contained KAT — no Rust toolchain needed on the GPU box)
1. embedded CPU reference `h_compress8` == real BLAKE3 KAT vectors for "" and "abc" @ R=7
2. GPU kernel word == CPU reference word over 4096 nonces × {hlen 32,40,48,56,37} × {R=7,R=3}
   ⇒ GPU ≡ BLAKE3 @ R=7, exactly what `pow::blake4_word` requires.

## Build & run (devel CUDA image, e.g. a Vast RTX 4090)
    nvcc -O3 -arch=sm_89 --use_fast_math -o blake4_cuda blake4_cuda.cu   # sm_89 = Ada/4090
    ./blake4_cuda selftest
    ./blake4_cuda bench 5 7                 # sound (consensus) rounds
    ./blake4_cuda mine <header_hex> <target_hex_u64> [rounds] [base]

## Measured — RTX 4090 (128 SM, sm_89), CUDA 12.4, 2026-06-09
    selftest : ALL PASS (GPU ≡ CPU ≡ BLAKE3 @ R=7; R=3 reduced also GPU==CPU)
    R=7 sound : 25.65 GH/s     (~165× the CPU AVX2-x8 sound path @ 155 MH/s)
    R=3 lever : 41.04 GH/s
    mine R=7  : nonce 6311661, word 0x00000c8726e1663c <= 0x00000fffffffffff, CPU-verified, <2ms

## Consensus note
R=7 is the ONLY consensus path (≡ BLAKE3). Reduced R<7 is a bench/research speed lever,
never deployed without a crypto-agility-gated consensus change — same rule as `pow.rs`.
The node verify path is unchanged (scalar, re-hashes at FULL_ROUNDS).
