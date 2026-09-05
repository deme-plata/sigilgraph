#!/usr/bin/env bash
# publish-ai-manifests.sh — write + SIGN + publish the two manifests the [A]I tab
# trusts, to BOTH download roots, then re-fetch them live and verify.
#
#   sigil-ai-latest.json      which ollama (url + Ollama's OWN sha256 + size) and which model
#   sigil-skills-latest.json  flux-signed skill packs (SKILL.md inlined + blake3 each)
#
# Signing = scripts/sign-manifest.sh (pinned release ed25519 seed, root-only). The client
# verifies both manifests with RELEASE_SIGN_PUBKEY_HEX — the same key the auto-updater uses.
#
# Usage: publish-ai-manifests.sh [OLLAMA_VER]      (default 0.33.2)
set -euo pipefail
OLLAMA_VER="${1:-0.33.2}"
DEFAULT_MODEL="${DEFAULT_MODEL:-qwen3:8b}"
FALLBACK_MODEL="${FALLBACK_MODEL:-qwen3:4b}"
# One rung below the fallback: what a phone (Termux) or a < 6 GB box pulls. sigil-top ≥ 8.0.4
# reads it (`small_model`, optional); older clients ignore the field.
SMALL_MODEL="${SMALL_MODEL:-qwen3:1.7b}"
REPO=/home/storage/deepseek-codewhale/sigil
DL=/home/orobit/q-narwhalknight/dist-fluxapp/downloads
LEGACY_DL=/home/orobit/q-narwhalknight/dist-final/downloads
# sigilgraph.org is the CANONICAL home (operator ruling 2026-08-26) and is CHANNEL_BASES[0]
# as of sigil-top v8.0.0 — publishing here is mandatory, not a mirror. Before 2026-09-02 this
# root held only APKs, so .org answered every manifest GET with its SPA fallback page: HTTP 200,
# content-type text/html, 3701 bytes. A 200-with-HTML is worse than a 404 — the client only
# discovers the problem at signature verification, and reports it as a bad signature.
ORG_DL=/home/orobit/sigilgraph-org-site/downloads
BASE=https://sigilgraph.org/downloads
GH="https://github.com/ollama/ollama/releases/download/v${OLLAMA_VER}"
PINNED_PUB="150fb84d4b2c83e6e81a27f629e60686acf8663be5ce73f46208cce4f5686402"
SKILL_TGZ="${SKILL_TGZ:-$DL/slagteren-skill-latest.tar.gz}"
WORK=$(mktemp -d); trap 'rm -rf "$WORK"' EXIT
cd "$REPO"

b3() { # blake3 hex of a file — b3sum, else python blake3, else fail loud
  if command -v b3sum >/dev/null 2>&1; then b3sum --no-names "$1"
  elif python3 -c 'import blake3' 2>/dev/null; then python3 -c 'import sys,blake3;print(blake3.blake3(open(sys.argv[1],"rb").read()).hexdigest())' "$1"
  else echo "✗ need b3sum or python3 blake3 to hash skills" >&2; exit 1; fi
}
clen() { curl -sIL -m 30 "$1" | tr -d '\r' | awk 'tolower($1)=="content-length:"{v=$2} END{print v}'; }

echo "▸ 1/5 Ollama v${OLLAMA_VER}: fetch its published sha256sum.txt"
curl -sSL -m 60 "$GH/sha256sum.txt" -o "$WORK/sha256sum.txt"
sha_of() { awk -v n="./$1" '$2==n{print $1}' "$WORK/sha256sum.txt"; }
WIN_SHA=$(sha_of OllamaSetup.exe);           [ -n "$WIN_SHA" ] || { echo "✗ no sha for OllamaSetup.exe"; exit 1; }
LIN_SHA=$(sha_of ollama-linux-amd64.tar.zst); [ -n "$LIN_SHA" ] || { echo "✗ no sha for ollama-linux-amd64.tar.zst"; exit 1; }
MAC_SHA=$(sha_of ollama-darwin.tgz);          [ -n "$MAC_SHA" ] || { echo "✗ no sha for ollama-darwin.tgz"; exit 1; }
ARM_SHA=$(sha_of ollama-linux-arm64.tar.zst); [ -n "$ARM_SHA" ] || { echo "✗ no sha for ollama-linux-arm64.tar.zst"; exit 1; }
WIN_SZ=$(clen "$GH/OllamaSetup.exe");           [ "${WIN_SZ:-0}" -gt 0 ] || { echo "✗ no size for OllamaSetup.exe"; exit 1; }
LIN_SZ=$(clen "$GH/ollama-linux-amd64.tar.zst"); [ "${LIN_SZ:-0}" -gt 0 ] || { echo "✗ no size for linux tar"; exit 1; }
MAC_SZ=$(clen "$GH/ollama-darwin.tgz");          [ "${MAC_SZ:-0}" -gt 0 ] || { echo "✗ no size for darwin tgz"; exit 1; }
ARM_SZ=$(clen "$GH/ollama-linux-arm64.tar.zst"); [ "${ARM_SZ:-0}" -gt 0 ] || { echo "✗ no size for linux arm64 tar"; exit 1; }
echo "  windows $WIN_SHA $WIN_SZ"; echo "  linux   $LIN_SHA $LIN_SZ"; echo "  macos   $MAC_SHA $MAC_SZ"; echo "  arm64   $ARM_SHA $ARM_SZ"

echo "▸ 2/5 write sigil-ai-latest.json"
cat > "$WORK/sigil-ai-latest.json" <<EOF
{
  "product": "sigil-ai", "ollama_version": "${OLLAMA_VER}",
  "default_model": "${DEFAULT_MODEL}", "fallback_model": "${FALLBACK_MODEL}", "small_model": "${SMALL_MODEL}",
  "verify": "sha256 below are Ollama's OWN published checksums (${GH}/sha256sum.txt); the client refuses any size/hash mismatch",
  "installers": {
    "windows-x64": { "url": "${GH}/OllamaSetup.exe",            "sha256": "${WIN_SHA}", "size_bytes": ${WIN_SZ}, "args": ["/VERYSILENT", "/NORESTART"] },
    "linux-x64":   { "url": "${GH}/ollama-linux-amd64.tar.zst", "sha256": "${LIN_SHA}", "size_bytes": ${LIN_SZ}, "args": [] },
    "macos":       { "url": "${GH}/ollama-darwin.tgz",          "sha256": "${MAC_SHA}", "size_bytes": ${MAC_SZ}, "args": [] },
    "linux-arm64": { "url": "${GH}/ollama-linux-arm64.tar.zst", "sha256": "${ARM_SHA}", "size_bytes": ${ARM_SZ}, "args": [] }
  },
  "updated": $(date +%s)
}
EOF
python3 -c 'import json,sys;json.load(open(sys.argv[1]))' "$WORK/sigil-ai-latest.json"

echo "▸ 3/5 write sigil-skills-latest.json (first signed skill: slagteren-suensonsvej)"
[ -s "$SKILL_TGZ" ] || { echo "✗ skill tarball missing: $SKILL_TGZ"; exit 1; }
mkdir -p "$WORK/skill" && tar xzf "$SKILL_TGZ" -C "$WORK/skill"
SKILL_MD=$(find "$WORK/skill" -name SKILL.md | head -1); [ -s "$SKILL_MD" ] || { echo "✗ no SKILL.md in tarball"; exit 1; }
SKILL_NAME=$(awk -F': *' '/^name:/{print $2; exit}' "$SKILL_MD"); [ -n "$SKILL_NAME" ] || { echo "✗ SKILL.md has no name:"; exit 1; }
SKILL_DESC=$(awk -F': *' '/^description:/{print $2; exit}' "$SKILL_MD")
SKILL_VER=$(basename "$(readlink -f "$SKILL_TGZ")" | grep -oE 'v[0-9]+\.[0-9]+\.[0-9]+' | tr -d v || true); SKILL_VER="${SKILL_VER:-1.0.0}"
SKILL_B3=$(b3 "$SKILL_MD")
python3 - "$WORK/sigil-skills-latest.json" "$SKILL_NAME" "$SKILL_VER" "$SKILL_DESC" "$SKILL_B3" "$SKILL_MD" <<'PY'
import json,sys,time
out,name,ver,desc,b3,path=sys.argv[1:7]
md=open(path,encoding='utf-8').read()
json.dump({"product":"sigil-skills","skills":[{"name":name,"version":ver,"description":desc,"blake3_hex":b3,"skill_md":md}],"updated":int(time.time())},open(out,'w'),ensure_ascii=False,indent=1)
print(f"  {name} v{ver} blake3 {b3[:16]}… ({len(md)} chars)")
PY

echo "▸ 4/5 sign both + publish to BOTH roots (additive)"
for m in sigil-ai-latest.json sigil-skills-latest.json; do
  bash scripts/sign-manifest.sh "$WORK/$m"
  for d in "$DL" "$LEGACY_DL" "$ORG_DL"; do
    [ -d "$d" ] || { echo "✗ download root missing: $d"; exit 1; }
    cp -f "$WORK/$m" "$d/$m.tmp" && mv -f "$d/$m.tmp" "$d/$m"; cp -f "$WORK/$m.sig" "$d/$m.sig"
  done
done

echo "▸ 5/5 re-fetch LIVE from EVERY CHANNEL_BASE and verify against the pinned key"
# Every base in sigil-top's CHANNEL_BASES is checked. A base that serves the SPA fallback
# (HTTP 200 + text/html) fails here loudly instead of silently at the client.
python3 - "$PINNED_PUB" <<'PY'
import sys,json,urllib.request,time
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey
pub=bytes.fromhex(sys.argv[1])
pk=Ed25519PublicKey.from_public_bytes(pub)
BASES=["https://sigilgraph.org/downloads",
       "https://sigilgraph.fluxapp.xyz/downloads",
       "https://quillon.xyz/downloads"]
bad=0
for base in BASES:
    g=lambda n: urllib.request.urlopen(f"{base}/{n}?t={int(time.time())}",timeout=30).read()
    for n in ("sigil-ai-latest.json","sigil-skills-latest.json"):
        try:
            body=g(n); sig=bytes.fromhex(g(n+".sig").decode().strip())
            pk.verify(sig,body); d=json.loads(body)
            what=d.get("default_model") or [s["name"] for s in d.get("skills",[])]
            print(f"  ✓ {base.split('//')[1].split('/')[0]:28s} {n:26s} sig OK ({len(body)} B) {what}")
        except Exception as e:
            bad+=1; print(f"  ✗ {base} {n}: {type(e).__name__}: {e}")
if bad: raise SystemExit(f"✗ {bad} manifest fetch/verify failure(s) — NOT publishable")
PY
echo "✓ published: $BASE/sigil-ai-latest.json  $BASE/sigil-skills-latest.json"
