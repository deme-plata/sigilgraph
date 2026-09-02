#!/usr/bin/env bash
# release-sigil-top.sh — codifies the FULL sigil-top release ceremony as one command.
#
#   scripts/release-sigil-top.sh 7.0.14 "one-line release note for the manifest"
#
# Steps (each fails LOUD; the sign-manifest step can never be skipped):
#   1. bump crates/sigil-top/Cargo.toml to the new version
#   2. build linux-x64 + windows-x64 (release, via fluxc — never raw cargo)
#   3. sign-artifact both (require-both SQIsign-L5 + Ed25519) + verify-proof
#   4. publish binaries + .proof + source tarball to dist-fluxapp/downloads
#   5. write sigil-top-latest.json manifest, then scripts/sign-manifest.sh it
#   6. verify the LIVE manifest+sig against the pinned RELEASE_SIGN_PUBKEY
#   7. git commit + annotated tag + push (branch + tag)
# The auto-updater rolls the new version to every node from the signed manifest.
set -euo pipefail

VER="${1:?usage: release-sigil-top.sh <version> [note]}"
NOTE="${2:-sigil-top v${VER}}"
PINNED_PUB="150fb84d4b2c83e6e81a27f629e60686acf8663be5ce73f46208cce4f5686402"
REPO="/home/storage/deepseek-codewhale/sigil"
FLUXC="${FLUXC:-/home/storage/deepseek-codewhale/flux/target/debug/fluxc}"
DL="/home/orobit/q-narwhalknight/dist-fluxapp/downloads"
BASE="https://sigilgraph.fluxapp.xyz/downloads"
export PATH="/home/orobit/node/current/bin:$PATH"
cd "$REPO"

# ── SYNC WITH THE BRANCH TIP **BEFORE** BUILDING ──────────────────────────────
#
# Any commit that lands on the branch between the start of a release and its final push
# makes that push non-fast-forward. That happened on BOTH v8.0.1 and v8.0.2: the channel
# went live, signed and verified, while the release commit sat on no branch at all — the
# v7.3.3/v7.3.4 shape, signed binaries that NO REACHABLE COMMIT REPRODUCES. It was caught
# only because someone read the push output, so do it here instead, before compiling.
#
# Merge, never rebase: the branch tip is already published. Cargo.lock is the routine
# conflict (both sides bump sigil-top's version line) and resolves to OURS, the version
# actually being released; anything else stops the release.
_BR=$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo HEAD)
if [ "$_BR" != "HEAD" ]; then
  git fetch -q origin "$_BR" || { echo "✗ REFUSING: cannot fetch origin/$_BR" >&2; exit 1; }
  if [ "$(git rev-list --count "HEAD..origin/$_BR" 2>/dev/null || echo 0)" -gt 0 ]; then
    echo "▸ 0/7 branch tip moved — merging BEFORE the build"
    if ! git merge --no-gpg-sign --no-edit "origin/$_BR" -m "merge: branch tip into the v${VER} release line (pre-build sync)"; then
      _C=$(git diff --name-only --diff-filter=U)
      [ "$_C" = "Cargo.lock" ] || { echo "✗ REFUSING: conflicts beyond Cargo.lock:" >&2; echo "$_C" >&2; exit 1; }
      git checkout --ours Cargo.lock && git add Cargo.lock
      git -c core.editor=true commit --no-gpg-sign -q
      echo "  · Cargo.lock conflict auto-resolved to OURS"
    fi
    echo "  ok synced: now at $(git rev-parse --short HEAD)"
  fi
fi

echo "▸ 1/7 bump version → $VER"
sed -i "s/^version = \"[0-9.]*\"/version = \"$VER\"/" crates/sigil-top/Cargo.toml
grep -q "^version = \"$VER\"" crates/sigil-top/Cargo.toml || { echo "✗ version bump failed"; exit 1; }

# 2026-07-26: the two builds now run IN PARALLEL. They used to be serial for
# ~100 s each (codegen-units=1 => one rustc, no intra-crate parallelism), so step 2
# dominated the ~4 min release. Naive `&` does NOT work: a cross build writes its
# host-side artifacts (proc macros, build scripts) into target/release/, so both
# cargos fight over target/release/.cargo-lock. MEASURED, no-op pair:
#   shared target/    19 s wall — 1x "Blocking waiting for file lock on artifact directory"
#   separate dirs     12 s wall — 0x artifact-directory lock (only the brief package-cache one)
# So the windows build gets its own CARGO_TARGET_DIR. One-time cost to establish it:
# 181 s and 888 MB (measured); warm after that.
# NOTE: two concurrent LTO links — give the wrapping systemd-run scope MemoryMax>=16G.
WIN_TARGET_DIR="${WIN_TARGET_DIR:-/home/storage/sigil-target-win}"
echo "▸ 2/7 build linux + windows (release, PARALLEL — windows in $WIN_TARGET_DIR)"
# NOTE (2026-08-21): sigil-top's Cargo.toml has `default = ["gpu"]` — GPU
# support is baked into EVERY plain build already (the client auto-detects
# hardware and falls back to CPU as needed; SIGIL_MINE_CPU=1 forces CPU).
# There is no separate GPU-featured artifact to build — confirmed by directly
# building `--features gpu` into a completely fresh target dir (bypassing all
# caches) and diffing against the plain build: byte-identical, because `gpu`
# was already on by default in the plain build. Do NOT re-add a third build
# step for this — it was tried and is wasted compute, not a fix.
T_BUILD=$SECONDS
LLOG="/home/orobit/tmp/release-${VER}-build-linux.log"
WLOG="/home/orobit/tmp/release-${VER}-build-windows.log"
"$FLUXC" build --release -p sigil-top > "$LLOG" 2>&1 &
LPID=$!
CARGO_TARGET_DIR="$WIN_TARGET_DIR" "$FLUXC" build --release -p sigil-top --target x86_64-pc-windows-gnu > "$WLOG" 2>&1 &
WPID=$!
wait "$LPID" || { echo "✗ linux build failed:"; tail -25 "$LLOG"; exit 1; }
wait "$WPID" || { echo "✗ windows build failed:"; tail -25 "$WLOG"; exit 1; }
echo "  build wall clock: $((SECONDS-T_BUILD))s (serial baseline v7.1.9/v7.1.10: ~200s)"
LBIN="target/release/sigil-top"; WBIN="$WIN_TARGET_DIR/x86_64-pc-windows-gnu/release/sigil-top.exe"
[ -x "$LBIN" ] || { echo "✗ missing $LBIN"; exit 1; }
[ -s "$WBIN" ] || { echo "✗ missing $WBIN"; exit 1; }
"$LBIN" version 2>/dev/null | grep -q "v$VER" || { echo "✗ built binary is not v$VER"; exit 1; }

echo "▸ 3/7 stage + sign (require-both) + verify"
S="/home/orobit/tmp/sigil-v${VER}-release"; rm -rf "$S"; mkdir -p "$S"
cp "$LBIN" "$S/sigil-top-v${VER}-linux-x64"
cp "$WBIN" "$S/sigil-top-v${VER}-windows-x64.exe"
tar --sort=name --mtime='2026-01-01 00:00:00' --owner=0 --group=0 --numeric-owner \
    -czf "$S/sigil-top-v${VER}-src.tar.gz" \
    -C crates sigil-top/src sigil-top/Cargo.toml \
    -C .. gui/sigil-wallet-tron-embedded.html gui/sigil-wallet-codex.css \
    gui/enter-sigil.html gui/sigil-explorer.html gui/vite-engine-embedded.html
for t in "linux-x64" "windows-x64.exe"; do
  f="$S/sigil-top-v${VER}-${t}"
  "$FLUXC" sign-artifact "$f" --source "$S/sigil-top-v${VER}-src.tar.gz" -o "$f.proof" | grep -q "require-both" || { echo "✗ sign failed $t"; exit 1; }
  "$FLUXC" verify-proof "$f" "$f.proof" | grep -q "✓ require-both" || { echo "✗ verify failed $t"; exit 1; }
done
LB3=$(b3sum "$S/sigil-top-v${VER}-linux-x64" | awk '{print $1}'); LSZ=$(stat -c %s "$S/sigil-top-v${VER}-linux-x64")
WB3=$(b3sum "$S/sigil-top-v${VER}-windows-x64.exe" | awk '{print $1}'); WSZ=$(stat -c %s "$S/sigil-top-v${VER}-windows-x64.exe")

echo "▸ 4/7 publish to $DL (+ legacy channel)"
cp "$S"/sigil-top-v${VER}-* "$DL/"
# LEGACY CHANNEL (2026-07-21): clients from the v7.0.1 era poll quillon.xyz/downloads/ —
# that manifest went stale at v2.0.0 on Jun 17, so old nodes believed they were up to
# date FOREVER. Every release publishes binaries+manifest+sig to BOTH roots now.
LEGACY_DL="/home/orobit/q-narwhalknight/dist-final/downloads"
cp "$S"/sigil-top-v${VER}-* "$LEGACY_DL/"

# ── STABLE-NAME LINKS (2026-08-26, rocky) ────────────────────────────────────
# The two cp's above publish the VERSIONED artifacts, which is what the signed
# manifest points at — so the AUTO-UPDATER has always been correct and every
# check we own (flux_release_check, step 6 below) reads the manifest and says
# "fine". The stable names humans actually `wget` were never refreshed by this
# script at all. Measured on 2026-08-26, moments after cutting v7.1.91:
#
#     sigil-top-linux-x64   15,739,912 B   dated 06-10 09:32
#
# Every `wget .../sigil-top-linux-x64` had served a TWO-AND-A-HALF-MONTH-OLD
# binary since June, and nothing detected it, because publishing to the
# versioned name only is invisible to a manifest-based check. CLAUDE.md's
# downloads rule already required "versioned + stable name … so the stable link
# is always current" — the rule was right, the script just never did it.
#
# .tmp-then-mv so a reader mid-download never sees a half-written file (rename
# is atomic within a filesystem, and an already-open fd keeps the old inode).
# The .proof rides along, or the stable binary would be unverifiable.
# ADDITIVE ONLY — nothing is ever deleted from downloads/ (CLAUDE.md rule 9).
for root in "$DL" "$LEGACY_DL"; do
  for t in "linux-x64" "windows-x64.exe"; do
    src="$root/sigil-top-v${VER}-${t}"; dst="$root/sigil-top-${t}"
    [ -s "$src" ] || { echo "✗ stable-link source missing or empty: $src"; exit 1; }
    cp -f "$src" "$dst.tmp" && mv -f "$dst.tmp" "$dst"
    [ -s "$src.proof" ] && { cp -f "$src.proof" "$dst.proof.tmp" && mv -f "$dst.proof.tmp" "$dst.proof"; }
    # Assert, don't assume: the stable name must now be byte-identical in size
    # to the versioned artifact it is supposed to mirror.
    [ "$(stat -c %s "$dst")" = "$(stat -c %s "$src")" ] \
      || { echo "✗ stable link $dst does not match $src after copy"; exit 1; }
    echo "  ↻ stable link $(basename "$dst") -> v$VER ($(stat -c %s "$dst") B)"
  done
done

echo "▸ 5/7 write + SIGN manifest (mandatory — updater fails closed without a valid .sig)"
REV=$(git rev-parse --short HEAD)
cat > "$DL/sigil-top-latest.json" <<EOF
{
  "product": "sigil-top", "version": "$VER", "channel": "stable",
  "url": "$BASE/sigil-top-v${VER}-linux-x64", "blake3_hex": "$LB3", "size_bytes": $LSZ,
  "flux_rev": "$REV", "source_tag": "v$VER",
  "verify": "fluxc verify-proof <artifact> <artifact>.proof   (require-both SQIsign-L5 + Ed25519)",
  "builder_keys": "$BASE/sigil-builder-keys.json",
  "source_archive": "$BASE/sigil-top-v${VER}-src.tar.gz",
  "notes": "$NOTE",
  "targets": {
    "linux-x64":       { "url": "$BASE/sigil-top-v${VER}-linux-x64",       "proof_url": "$BASE/sigil-top-v${VER}-linux-x64.proof",       "blake3_hex": "$LB3", "size_bytes": $LSZ },
    "linux-x64-gpu":   { "url": "$BASE/sigil-top-v${VER}-linux-x64",       "proof_url": "$BASE/sigil-top-v${VER}-linux-x64.proof",       "blake3_hex": "$LB3", "size_bytes": $LSZ },
    "windows-x64":     { "url": "$BASE/sigil-top-v${VER}-windows-x64.exe", "proof_url": "$BASE/sigil-top-v${VER}-windows-x64.exe.proof", "blake3_hex": "$WB3", "size_bytes": $WSZ },
    "windows-x64-gpu": { "url": "$BASE/sigil-top-v${VER}-windows-x64.exe", "proof_url": "$BASE/sigil-top-v${VER}-windows-x64.exe.proof", "blake3_hex": "$WB3", "size_bytes": $WSZ }
  }
}
EOF
bash scripts/sign-manifest.sh "$DL/sigil-top-latest.json"
# legacy channel gets the identical signed manifest
cp "$DL/sigil-top-latest.json" "$DL/sigil-top-latest.json.sig" "$LEGACY_DL/"

echo "▸ 6/7 verify LIVE manifest+sig against pinned key"
python3 - "$PINNED_PUB" <<'PY'
import urllib.request, time, json, sys
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey
pin=sys.argv[1]; t=int(time.time())
g=lambda u: urllib.request.urlopen(f'https://sigilgraph.fluxapp.xyz/downloads/{u}?t={t}',timeout=25).read()
body=g('sigil-top-latest.json'); sig=bytes.fromhex(g('sigil-top-latest.json.sig').decode().strip()); d=json.loads(body)
Ed25519PublicKey.from_public_bytes(bytes.fromhex(pin)).verify(sig, body)
print('  ✓ live manifest+sig VALID — channel now', d['version'])
PY

# The stable links are what a HUMAN gets from a wget line, and they are NOT
# covered by the manifest check above — which is exactly how they rotted for
# 2.5 months unnoticed. Fetch them over the real network and assert they match
# the versioned artifact the manifest points at. Fail the release if not: a
# release whose published download link serves a stale binary is not released.
echo "  ▸ verifying LIVE stable links match v$VER"
for t in "linux-x64" "windows-x64.exe"; do
  want=$(stat -c %s "$DL/sigil-top-v${VER}-${t}")
  got=$(curl -sfL -o /dev/null -w '%{size_download}' --max-time 120 "$BASE/sigil-top-${t}?t=$(date +%s)" || echo 0)
  [ "$got" = "$want" ] \
    || { echo "✗ LIVE stable link $BASE/sigil-top-${t} served ${got}B, expected ${want}B (v$VER)"; exit 1; }
  echo "    ✓ $BASE/sigil-top-${t} = ${got}B (v$VER)"
done

echo "▸ 7/7 commit + tag + push"
git add crates/sigil-top/Cargo.toml Cargo.lock
git commit --no-gpg-sign -m "release(sigil-top): v${VER}

$NOTE

  linux-x64   blake3 $LB3
  windows-x64 blake3 $WB3

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>" || echo "  (nothing new to commit besides version — continuing)"
git tag -a "v${VER}" -m "sigil-top v${VER}" 2>/dev/null || echo "  (tag v${VER} exists)"
BRANCH=$(git rev-parse --abbrev-ref HEAD)
git push origin "$BRANCH" "v${VER}"

# refresh the code-provenance page the explorer's flux-rev tab iframes (post-tag so
# the new release row appears; non-fatal — the page just lags one release on failure)
bash scripts/gen-flux-versions.sh || echo "  (flux-versions refresh failed — non-fatal)"

echo ""
echo "✅ RELEASED sigil-top v${VER} — channel live, signed, tagged. Nodes pull it on next update check."
echo "   linux   $LB3"
echo "   windows $WB3"
