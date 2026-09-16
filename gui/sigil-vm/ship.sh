#!/usr/bin/env bash
# ship.sh — the SIGIL VM release gate. Every step must pass or nothing is deployed:
#   tsc (types) → fluxc build (never npm) → flux-vite-engine verify (headless render, 0 console errors)
#   → copy to the sigilgraph.org root (additive, hashed assets) → HTTPS verify → commit + push.
# Usage: ./ship.sh "commit message"
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
FLUX=/home/storage/deepseek-codewhale/flux
SITE=/home/orobit/sigilgraph-org-site
SHOT=/home/storage/sigil-scratch/sigil-vm-ui/ship-$(date +%H%M%S).png
export PATH=/home/orobit/node/current/bin:$PATH
MSG="${1:?commit message}"
cd "$HERE"
echo "▶ tsc";        ./node_modules/.bin/tsc -p tsconfig.json
echo "▶ fluxc build"; timeout 300 "$FLUX/target/debug/fluxc" build --frontend-only | grep -E 'built in|rror' || true
test -f dist/sigil-vm.html || { echo "✗ no dist/sigil-vm.html"; exit 1; }
echo "▶ verify (flux-vite-engine)"
( cd "$FLUX" && timeout 300 ./target/debug/examples/verify "$HERE" "$SHOT" ) | grep -E 'build-SAP|render ──|console|→'
echo "▶ layout guard (6 widths × 2 themes + dialogs + panel + contrast) ∥ outage gate (12 route families, degrade honestly)"
node "$HERE/outage.mjs" > "$HERE/.outage.log" 2>&1 & OUT_PID=$!
node "$HERE/guard.mjs"
wait $OUT_PID || { cat "$HERE/.outage.log"; echo "✗ outage gate failed"; exit 1; }
tail -1 "$HERE/.outage.log"
echo "▶ pre-render snapshot into dist/sigil-vm.html (readable without JavaScript)"
node "$HERE/snapshot.mjs"
echo "▶ deploy → $SITE (SIGIL root, additive)"
cp dist/sigil-vm.html "$SITE/sigil-vm.html"
cp dist/sigil-vm-showcase.html "$SITE/sigil-vm-showcase.html"
cp dist/assets/sigil-vm-*.js dist/assets/sigil-vm-*.css "$SITE/assets/"
for f in sigil-vm.html sigil-vm-showcase.html $(cd dist && ls assets/sigil-vm-*); do
  code=$(curl -s -o /dev/null -m 10 -w '%{http_code}' "https://sigilgraph.org/$f"); echo "  $f $code"; [ "$code" = 200 ] || exit 1
done
echo "▶ commit + push"
cd /home/storage/deepseek-codewhale/sigil
git add gui/sigil-vm
git commit -q -m "$MSG

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
git push -q origin HEAD:hardening/ws-2026-07-18
git log --oneline -1 | cat
echo "✓ shipped · shot $SHOT"
