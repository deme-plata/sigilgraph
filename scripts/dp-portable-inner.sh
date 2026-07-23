#!/bin/bash
# Runs INSIDE rust:1-bullseye. Builds delivery-probe with the proven zig-cc
# glibc-2.27 linker (HiveOS recipe): portable to any x86-64 Linux with glibc>=2.27.
set -euo pipefail
set -o pipefail
cat > /usr/local/bin/zcc <<'EOF'
#!/bin/bash
for a in "$@"; do shift; case "$a" in --target=*) ;; *) set -- "$@" "$a";; esac; done
exec /ztools/zig cc -target x86_64-linux-gnu.2.27 "$@"
EOF
chmod +x /usr/local/bin/zcc
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=/usr/local/bin/zcc
export CC_x86_64_unknown_linux_gnu=/usr/local/bin/zcc
export ZIG_GLOBAL_CACHE_DIR=/tmp/zigcache
cd /src/sigil
cargo build --release -p delivery-probe
echo "BUILT: $(ls -la "$CARGO_TARGET_DIR/release/delivery-probe")"
