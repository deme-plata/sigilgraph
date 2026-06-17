#!/bin/bash
# Reproducible Linux → Windows GPU (OpenCL) cross-build of sigil-top (and sigil-miner).
#
# v0.90: opencl3 was bumped to 0.10 with the `dynamic` feature, so the OpenCL ICD
# (OpenCL.dll) is dlopen'd at RUNTIME via libloading instead of linked with
# `-lOpenCL`. That removes the whole import-lib dance: the windows-gnu link needs NO
# OpenCL SDK / import lib in the mingw sysroot. (The old dlltool-from-OpenCL.def path
# is kept below, commented, for reference only — it is no longer used.)
#
# Build box prereqs (present on epsilon):
#   - x86_64-w64-mingw32 toolchain (gcc)   apt: gcc-mingw-w64-x86-64
#   - rustup target add x86_64-pc-windows-gnu
#   - the fluxc binary (dogfooded build path)
#
# The produced exe needs OpenCL.dll at RUNTIME (the GPU driver ships one in
# System32; we also ship one beside the exe as a fallback). It is a *-gpu.exe, NOT
# the default download — the default must run on machines with no OpenCL at all.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
FLUXC="${FLUXC:-/home/storage/deepseek-codewhale/flux/target/debug/fluxc}"
PKG="${PKG:-sigil-top}"          # sigil-top (the TUI miner) by default; PKG=flux-miner for sigil-miner
BIN="${BIN:-sigil-top}"

# clear cargo's target-info cache if poisoned by a stray MOTD line
# (see memory: feedback_poisoned_rustc_info_breaks_cross)
rm -f "${CARGO_TARGET_DIR:-$REPO/target}/.rustc_info.json" 2>/dev/null || true

# Build with the gpu feature, dogfooded through fluxc (which forwards --features /
# --target to the build). Niced because this runs on the production host (epsilon).
cd "$REPO"
ionice -c3 nice -n19 "$FLUXC" build --release \
  --target x86_64-pc-windows-gnu \
  -p "$PKG" --features gpu --bin "$BIN"

OUT="$REPO/target/x86_64-pc-windows-gnu/release/$BIN.exe"
echo "→ $OUT (GPU, opencl3 dynamic — ship OpenCL.dll beside it)"

# ── OBSOLETE (pre-dynamic) import-lib path — kept for reference ────────────────
# x86_64-w64-mingw32-dlltool -d "$HERE/OpenCL.def" \
#   -l /usr/x86_64-w64-mingw32/lib/libOpenCL.a -D OpenCL.dll
# Regenerate OpenCL.def from headers:
#   { echo "LIBRARY OpenCL.dll"; echo EXPORTS; \
#     grep -rhoE '\bcl[A-Z][A-Za-z0-9]+' /usr/include/CL/cl*.h | sort -u; } > OpenCL.def
