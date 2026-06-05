#!/usr/bin/env bash
# cut-release.sh — automate R1 + R2 + R3 + R4 + R7 of the SIGIL release procedure.
#
# Spec source:  sigil/docs/release-procedure.md
# After this:   operator still does R5 (README), R6 (deploy), R8 (announcement), R9 (soak).
#
# Usage:
#   cut-release.sh X.Y.Z [--note "headline"] [--delta-host delta] [--no-deploy] [--dry-run]
#
# Defaults assume execution from Epsilon (where /home/orobit/q-narwhalknight/dist-final/ lives)
# with SSH access to Delta (where SIGIL is compiled). Override via --delta-host or env.

set -euo pipefail

# ────────────────────────────────────────────────────────────────────────────
# Args + defaults
# ────────────────────────────────────────────────────────────────────────────
VERSION=""
NOTE=""
DELTA_HOST="${DELTA_HOST:-delta}"
SIGIL_WORKSPACE="${SIGIL_WORKSPACE:-/home/storage/deepseek-codewhale/sigil}"
DELTA_WORKSPACE="${DELTA_WORKSPACE:-/home/storage/deepseek-codewhale/sigil}"
DIST_FINAL="${DIST_FINAL:-/home/orobit/q-narwhalknight/dist-final}"
DOWNLOADS="${DOWNLOADS:-$DIST_FINAL/downloads}"
SKIP_DEPLOY=0
DRY_RUN=0
BINARIES=("sigil-node" "sigil-chronos-net" "sigil-multiverse-dump")

usage() {
  cat <<EOF
cut-release.sh X.Y.Z [--note "headline"] [--delta-host delta] [--no-deploy] [--dry-run]

Automates R1 + R2 + R3 + R4 + R7 of the SIGIL release procedure:
  R1  workspace version bump
  R2  build binaries on Delta (NEVER Epsilon)
  R3  emit .proof bundles via fluxc compile-native --provenance
  R4  assemble releases/vX.Y.Z/ bundle
  R7  publish download URLs to dist-final/downloads/

After this script: operator runs R5 (README), R6 (deploy), R8 (announcement), R9 (soak).
EOF
  exit "${1:-0}"
}

[[ $# -eq 0 ]] && usage 1

VERSION="$1"; shift || true
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[a-z0-9]+)?$ ]] \
  || { echo "✗ version must be semver, got: $VERSION"; usage 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --note)        NOTE="${2:-}"; shift 2;;
    --delta-host)  DELTA_HOST="${2:-}"; shift 2;;
    --no-deploy)   SKIP_DEPLOY=1; shift;;
    --dry-run)     DRY_RUN=1; shift;;
    -h|--help)     usage 0;;
    *) echo "✗ unknown arg: $1"; usage 1;;
  esac
done

# ────────────────────────────────────────────────────────────────────────────
# Pretty output
# ────────────────────────────────────────────────────────────────────────────
B=$'\e[1m'; R=$'\e[0m'; G=$'\e[32m'; Y=$'\e[33m'; X=$'\e[31m'; C=$'\e[36m'; D=$'\e[2m'

step() { echo; echo "${B}${C}══ $* ══${R}"; }
ok()   { echo "  ${G}✓${R} $*"; }
warn() { echo "  ${Y}⚠${R} $*"; }
fail() { echo "  ${X}✗${R} $*"; exit 1; }
info() { echo "    $*"; }
runc() { if [[ $DRY_RUN -eq 1 ]]; then echo "    ${D}(dry-run) ${*}${R}"; else "$@"; fi; }

# ────────────────────────────────────────────────────────────────────────────
# R1 — Workspace version bump
# ────────────────────────────────────────────────────────────────────────────
step "R1 — workspace version bump → $VERSION"

CARGO_TOML="$SIGIL_WORKSPACE/Cargo.toml"
[[ -f "$CARGO_TOML" ]] || fail "no Cargo.toml at $CARGO_TOML"

current=$(grep -E '^version\s*=' "$CARGO_TOML" | head -1 | sed -E 's/.*"([^"]+)".*/\1/')
ok "current workspace version: $current"

if [[ "$current" == "$VERSION" ]]; then
  warn "already at $VERSION — R1 idempotent skip"
else
  if [[ $DRY_RUN -eq 1 ]]; then
    info "${D}(dry-run) sed -i s/version = \"$current\"/version = \"$VERSION\"/ Cargo.toml${R}"
  else
    sed -i.bak -E "s/^version = \"$current\"/version = \"$VERSION\"/" "$CARGO_TOML"
    rm -f "$CARGO_TOML.bak"
  fi
  ok "bumped to $VERSION"
fi

# Sanity: confirm all crates either inherit workspace or aren't stale
stale=0
for c in "$SIGIL_WORKSPACE"/crates/*/Cargo.toml; do
  if ! grep -q 'version.workspace\|version = "'"$VERSION"'"' "$c" 2>/dev/null; then
    stale=$((stale + 1))
    warn "stale: $(basename $(dirname $c))"
  fi
done
[[ $stale -eq 0 ]] && ok "all crates inherit version.workspace ✓" || warn "$stale crate(s) not inheriting — investigate"

# ────────────────────────────────────────────────────────────────────────────
# R2 — Build binaries on Delta (NEVER on Epsilon/Beta — SIGIL skill rule)
# ────────────────────────────────────────────────────────────────────────────
step "R2 — build ${BINARIES[*]} on Delta"

# Verify we're NOT on Delta — we should SSH into it
THIS_HOST=$(hostname -I 2>/dev/null | tr ' ' '\n' | grep -v "^$\|^172\.\|^10\." | head -1)
if [[ "$THIS_HOST" == "5.79.79.158" ]]; then
  fail "running ON Delta — cut-release.sh must SSH from another host"
fi

# Verify SSH to Delta works
runc ssh -o ConnectTimeout=5 -o BatchMode=yes "$DELTA_HOST" "echo 'reachable'" >/dev/null \
  || fail "cannot SSH to $DELTA_HOST — set --delta-host or check authorized_keys"
ok "SSH to $DELTA_HOST: reachable"

# Verify Delta has the workspace at the expected path
if [[ $DRY_RUN -ne 1 ]]; then
  ssh "$DELTA_HOST" "test -d $DELTA_WORKSPACE" \
    || fail "Delta missing workspace at $DELTA_WORKSPACE — set DELTA_WORKSPACE"
fi
ok "Delta workspace present: $DELTA_WORKSPACE"

# Sync the workspace version bump to Delta
runc ssh "$DELTA_HOST" "cd $DELTA_WORKSPACE && sed -i -E 's/^version = \"[^\"]+\"/version = \"$VERSION\"/' Cargo.toml"
ok "synced Cargo.toml version to Delta"

# Build each binary via fluxc (NEVER raw cargo per [[no-cargo-in-flux]])
for bin in "${BINARIES[@]}"; do
  echo
  info "${C}building $bin on Delta…${R}"
  runc ssh "$DELTA_HOST" "cd $DELTA_WORKSPACE && fluxc build --package $bin --release 2>&1 | tail -20"
  ok "built $bin"
done

# Confirm each binary identifies as the right version
for bin in "${BINARIES[@]}"; do
  if [[ $DRY_RUN -ne 1 ]]; then
    bin_ver=$(ssh "$DELTA_HOST" ".target-shared/release/$bin --version 2>&1 | head -1" || echo "<no --version>")
    if [[ "$bin_ver" == *"$VERSION"* ]]; then
      ok "$bin → $bin_ver"
    else
      warn "$bin reports: $bin_ver (expected $VERSION)"
    fi
  fi
done

# SCP binaries back to this host's working area
STAGING="$SIGIL_WORKSPACE/releases/v$VERSION"
mkdir -p "$STAGING"
for bin in "${BINARIES[@]}"; do
  runc scp "$DELTA_HOST:$DELTA_WORKSPACE/.target-shared/release/$bin" "$STAGING/$bin-v$VERSION"
  ok "staged: $bin-v$VERSION"
done

# ────────────────────────────────────────────────────────────────────────────
# R3 — Emit .proof bundles via fluxc compile-native --provenance
# ────────────────────────────────────────────────────────────────────────────
step "R3 — emit signed .proof bundles"

# Map binary → source file (each binary's main.rs)
declare -A BIN_TO_SRC=(
  ["sigil-node"]="crates/sigil-node/src/main.rs"
  ["sigil-chronos-net"]="crates/sigil-chronos/src/bin/sigil-chronos-net.rs"
  ["sigil-multiverse-dump"]="crates/sigil-chronos/src/bin/sigil-multiverse-dump.rs"
)

for bin in "${BINARIES[@]}"; do
  src="${BIN_TO_SRC[$bin]:-}"
  if [[ -z "$src" ]]; then warn "no source mapping for $bin — skip"; continue; fi
  runc ssh "$DELTA_HOST" "cd $DELTA_WORKSPACE && fluxc compile-native --provenance $src 2>&1 | tail -5"
  # The .proof file lands next to the binary
  runc scp "$DELTA_HOST:$DELTA_WORKSPACE/.target-shared/release/$bin.proof" "$STAGING/$bin-v$VERSION.proof" \
    || warn ".proof for $bin not produced — fluxc agent-keygen first?"
  ok "$bin-v$VERSION.proof"
done

# ────────────────────────────────────────────────────────────────────────────
# R4 — Assemble release bundle
# ────────────────────────────────────────────────────────────────────────────
step "R4 — assemble releases/v$VERSION/"

# Copy demo scripts from prior releases, sed-substitute the version
PRIOR_DEMOS=("v0.0.3/divergence-halt-demo.sh" "v0.0.4/tip-verify-join-demo.sh" "v0.0.5/first-swap-demo.sh")
for prior in "${PRIOR_DEMOS[@]}"; do
  src_path="$SIGIL_WORKSPACE/releases/$prior"
  if [[ -f "$src_path" ]]; then
    dst="$STAGING/$(basename $prior)"
    if [[ $DRY_RUN -eq 1 ]]; then
      info "${D}(dry-run) cp $src_path $dst + sed s/<old_ver>/$VERSION/g${R}"
    else
      cp "$src_path" "$dst"
      old_ver=$(echo "$prior" | sed -E 's|^v([0-9.]+)/.*|\1|')
      sed -i.bak "s/$old_ver/$VERSION/g" "$dst"
      rm -f "$dst.bak"
      chmod +x "$dst"
    fi
    ok "$(basename $prior) → version-substituted"
  fi
done

# Copy any Dockerfile from prior releases
PRIOR_DOCKERFILES=("v0.0.4/Dockerfile.tip-verify-join")
for prior in "${PRIOR_DOCKERFILES[@]}"; do
  src_path="$SIGIL_WORKSPACE/releases/$prior"
  if [[ -f "$src_path" ]]; then
    dst="$STAGING/$(basename $prior)"
    runc cp "$src_path" "$dst"
    ok "$(basename $prior)"
  fi
done

ok "bundle assembled at $STAGING"

# ────────────────────────────────────────────────────────────────────────────
# R7 — Publish download URLs (skip if --no-deploy)
# ────────────────────────────────────────────────────────────────────────────
if [[ $SKIP_DEPLOY -eq 1 ]]; then
  step "R7 — SKIPPED (--no-deploy)"
else
  step "R7 — publish to $DOWNLOADS/"

  mkdir -p "$DOWNLOADS"
  for bin in "${BINARIES[@]}"; do
    [[ -f "$STAGING/$bin-v$VERSION" ]] && runc cp "$STAGING/$bin-v$VERSION" "$DOWNLOADS/$bin-v$VERSION" && ok "→ $bin-v$VERSION"
    [[ -f "$STAGING/$bin-v$VERSION.proof" ]] && runc cp "$STAGING/$bin-v$VERSION.proof" "$DOWNLOADS/$bin-v$VERSION.proof" && ok "→ $bin-v$VERSION.proof"
  done

  # Update sigil-latest.json manifest
  manifest="$DOWNLOADS/sigil-latest.json"
  released_at=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
  released_by="${RELEASED_BY:-cut-release.sh@$(hostname)}"
  if [[ $DRY_RUN -ne 1 ]]; then
    # Compute BLAKE3 of main binary
    main_b3=""
    if command -v b3sum >/dev/null 2>&1; then
      main_b3=$(b3sum "$STAGING/sigil-node-v$VERSION" | awk '{print $1}')
    fi
    cat > "$manifest" <<EOF
{
  "version": "$VERSION",
  "binary_url": "https://quillon.xyz/downloads/sigil-node-v$VERSION",
  "binary_blake3": "$main_b3",
  "proof_url": "https://quillon.xyz/downloads/sigil-node-v$VERSION.proof",
  "release_notes_url": "https://quillon.xyz/releases/v$VERSION/README.md",
  "released_at": "$released_at",
  "released_by": "$released_by",
  "status": "RC",
  "note": "$NOTE"
}
EOF
    ok "manifest updated: sigil-latest.json"
  else
    info "${D}(dry-run) would write sigil-latest.json with version=$VERSION status=RC${R}"
  fi
fi

# ────────────────────────────────────────────────────────────────────────────
# Final summary
# ────────────────────────────────────────────────────────────────────────────
echo
echo "${B}${G}╔══════════════════════════════════════════════════════════════╗${R}"
echo "${B}${G}║   cut-release.sh COMPLETE — v$VERSION (R1+R2+R3+R4+R7 done) $(printf '%*s' $((20 - ${#VERSION})) '')║${R}"
echo "${B}${G}╚══════════════════════════════════════════════════════════════╝${R}"

if [[ -n "$NOTE" ]]; then echo "  Note: ${C}$NOTE${R}"; fi
echo

# Show what's in the staging dir
if [[ -d "$STAGING" ]]; then
  echo "  ${C}Bundle:${R} $STAGING"
  ls -lh "$STAGING" | tail -n +2 | awk '{printf "    %s %s\n", $5, $9}'
fi

# Show the cache-busted URLs for the operator
if [[ $SKIP_DEPLOY -ne 1 && $DRY_RUN -ne 1 ]]; then
  epoch=$(date +%s)
  echo
  echo "  ${C}Cache-busted URLs (hand these to humans):${R}"
  for bin in "${BINARIES[@]}"; do
    echo "    https://quillon.xyz/downloads/$bin-v$VERSION?v=$epoch"
  done
fi

echo
echo "  ${B}NEXT STEPS (operator):${R}"
echo "    R5  write releases/v$VERSION/README.md (start from sigil/docs/honest-release-readme-template.md)"
echo "    R6  scripts/deploy.sh delta $VERSION  +  scripts/deploy.sh epsilon $VERSION"
echo "    R8  sigil-updater publish --binary releases/v$VERSION/sigil-node-v$VERSION --version $VERSION"
echo "    R9  24h soak gate — watch Delta + Epsilon for divergence, OOM, halts. Promote on pass."
echo
