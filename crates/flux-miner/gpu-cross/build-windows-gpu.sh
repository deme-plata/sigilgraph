#!/bin/bash
# Reproducible Linux → Windows GPU (OpenCL) cross-build of sigil-miner.
#
# flux_cross_compile can't pass --features, and opencl3 links `-lOpenCL`, which on
# the windows-gnu target needs an IMPORT LIB for OpenCL.dll (the NVIDIA/AMD driver
# provides the runtime DLL on the user's machine). We synthesize that import lib
# from OpenCL.def (all ~210 cl* exports, extracted from /usr/include/CL/*.h) via
# mingw dlltool, then build with the gpu feature.
#
# Prereqs on the build box:
#   - x86_64-w64-mingw32 toolchain (dlltool, gcc)   apt: gcc-mingw-w64-x86-64
#   - opencl-headers                                 apt: opencl-headers
#   - rustup target add x86_64-pc-windows-gnu
#
# The produced exe REQUIRES OpenCL.dll at runtime → ship it as a *-gpu.exe, NOT as
# the default download (which must run on machines with no OpenCL ICD).
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"

# 1. import lib for OpenCL.dll into the mingw sysroot lib dir (default search path)
x86_64-w64-mingw32-dlltool -d "$HERE/OpenCL.def" \
  -l /usr/x86_64-w64-mingw32/lib/libOpenCL.a -D OpenCL.dll

# 2. clear cargo's target-info cache if poisoned by a stray MOTD line
#    (see memory: feedback_poisoned_rustc_info_breaks_cross)
rm -f "${CARGO_TARGET_DIR:-target}/.rustc_info.json"

# 3. build with the gpu feature (raw cargo --features: the documented cross-link
#    exception, since flux_cross_compile has no --features flag)
cargo build --release --target x86_64-pc-windows-gnu \
  -p flux-miner --features gpu --bin sigil-miner

echo "→ ${CARGO_TARGET_DIR:-target}/x86_64-pc-windows-gnu/release/sigil-miner.exe (GPU)"

# To regenerate OpenCL.def from headers:
#   { echo "LIBRARY OpenCL.dll"; echo EXPORTS; \
#     grep -rhoE '\bcl[A-Z][A-Za-z0-9]+' /usr/include/CL/cl*.h | sort -u; } > OpenCL.def
