#!/usr/bin/env bash
# commit-record.sh — anchor a SIGIL Commitment Record: content-address it, sign it,
# Bitcoin-timestamp it, publish the full bundle, and append it to the public ledger.
#
# This is the ritual behind "The Word and the Ledger" (SIGIL_COMMITMENT_PROVENANCE
# note). Run it once per promise, disclosure, release-announcement, direction-change,
# OR fulfillment — so the project's word is as verifiable as its binaries, witnessed
# by Bitcoin (which we don't control), not only by our own resettable chain.
#
#   scripts/commit-record.sh <id> <kind> <status> <body.txt> [supersedes_id]
#     id        stable commitment id, e.g. mainnet.reset-before-launch
#     kind      roadmap|disclosure|release|direction-change|retraction|fulfillment
#     status    active|satisfied|superseded|retracted
#     body.txt  the EXACT words of the commitment (or, for a fulfillment, the
#               evidence statement: what was done + evidence_refs)
#     supersedes optional id this record revises (never deletes)
#
# HOW YOU SHOW YOU KEPT A PROMISE:
#   commit the promise  → kind=roadmap  status=active
#   later, when done    → kind=fulfillment status=satisfied, SAME id, body cites the
#                         evidence (commit hash / release tag / block height). Both
#                         records are Bitcoin-timestamped, so "promised on date A,
#                         kept on date B, evidence X" is mechanically checkable by
#                         anyone, forever.
set -euo pipefail

ID="${1:?usage: commit-record.sh <id> <kind> <status> <body.txt> [supersedes]}"
KIND="${2:?kind required}"; STATUS="${3:?status required}"; BODY="${4:?body.txt required}"
SUPERSEDES="${5:-}"
[ -f "$BODY" ] || { echo "✗ body file not found: $BODY"; exit 1; }

OTS="${OTS_BIN:-/home/orobit/otsvenv/bin/ots}"
B3="$(command -v b3sum || true)"
DL="/home/orobit/q-narwhalknight/dist-fluxapp/downloads"
LEDGER_DIR="/home/orobit/q-narwhalknight/dist-fluxapp/downloads/commitments"
LEGACY_DL="/home/orobit/q-narwhalknight/dist-final/downloads/commitments"
UPPER_DL="/home/orobit/q-narwhalknight/.fluxapp-upper/downloads/commitments"
KEYSET_ID="${SIGIL_GOV_KEYSET_ID:-governance-v0-UNPINNED}"  # TODO: separate threshold governance key (reviewer point #4)
mkdir -p "$LEDGER_DIR"

# 1. content-address the exact words (BLAKE3)
BODY_HASH="$([ -n "$B3" ] && b3sum "$BODY" | awk '{print $1}' || python3 -c "import blake3,sys;print(blake3.blake3(open(sys.argv[1],'rb').read()).hexdigest())" "$BODY")"
SHA256="$(sha256sum "$BODY" | awk '{print $1}')"
BYTES="$(stat -c %s "$BODY")"

# 2. previous ledger head (the prev-chain — tamper-evidence)
HEAD_FILE="$LEDGER_DIR/HEAD"
PREV="$([ -f "$HEAD_FILE" ] && cat "$HEAD_FILE" || echo "0000000000000000000000000000000000000000000000000000000000000000")"
TS="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

# 3. canonical record JSON (sorted keys → stable bytes)
REC="$LEDGER_DIR/${ID}.$(date -u +%Y%m%dT%H%M%SZ).record.json"
python3 - "$ID" "$KIND" "$STATUS" "$BODY_HASH" "$SHA256" "$BYTES" "$PREV" "$SUPERSEDES" "$TS" "$KEYSET_ID" > "$REC" <<'PY'
import json,sys
i,kind,status,b3,sha,byt,prev,sup,ts,keyset=sys.argv[1:11]
rec={"commitment_id":i,"kind":kind,"status":status,"body_blake3":b3,"body_sha256":sha,
     "body_bytes":int(byt),"media_type":"text/plain; charset=utf-8","prev":prev,
     "supersedes":(sup or None),"timestamp":ts,"keyset_id":keyset,"schema_version":1}
print(json.dumps(rec,sort_keys=True,separators=(",",":"),ensure_ascii=False))
PY
REC_HASH="$([ -n "$B3" ] && b3sum "$REC" | awk '{print $1}' || python3 -c "import blake3,sys;print(blake3.blake3(open(sys.argv[1],'rb').read()).hexdigest())" "$REC")"

# 4. SIGN — governance keyset (separate from the builder key, reviewer point #4).
#    Until the threshold governance key is pinned+published, this step is a documented
#    TODO; the OTS timestamp + prev-chain already give tamper-evidence + independent time.
if [ -n "${SIGIL_GOV_SIGN_CMD:-}" ]; then
  eval "$SIGIL_GOV_SIGN_CMD \"$REC\" > \"$REC.sig\"" && echo "  ✓ governance-signed"
else
  echo "  ⚠ SIGIL_GOV_SIGN_CMD unset — record NOT governance-signed yet (OTS+prev-chain still bind it)"
fi

# 5. BITCOIN-TIMESTAMP the record (independent witness — the whole point)
cp "$BODY" "$LEDGER_DIR/${ID}.body.txt"
"$OTS" stamp "$REC" >/dev/null 2>&1 && echo "  ✓ Bitcoin-timestamped → $(basename "$REC").ots (pending confirmation, upgrades in hours)"

# 6. advance the prev-chain head + append to the public ledger index
echo "$REC_HASH" > "$HEAD_FILE"
LEDGER_INDEX="$LEDGER_DIR/commitment-log.jsonl"
python3 -c "import json,sys;print(json.dumps({'id':sys.argv[1],'kind':sys.argv[2],'status':sys.argv[3],'record':sys.argv[4],'record_blake3':sys.argv[5],'ts':sys.argv[6]},separators=(',',':')))" \
  "$ID" "$KIND" "$STATUS" "$(basename "$REC")" "$REC_HASH" "$TS" >> "$LEDGER_INDEX"

# 7. mirror the whole ledger dir to the other served roots (overlay-safe cp)
for D in "$LEGACY_DL" "$UPPER_DL"; do mkdir -p "$D"; cp -r "$LEDGER_DIR/." "$D/" 2>/dev/null || true; done

echo "✅ Commitment Record anchored:"
echo "   id:      $ID  ($KIND / $STATUS)"
echo "   words:   $BYTES B · blake3 $BODY_HASH"
echo "   record:  $REC_HASH   prev $PREV"
echo "   verify:  $OTS verify $(basename "$REC").ots  (+ recompute body blake3, walk prev-chain)"
echo "   public:  https://sigilgraph.fluxapp.xyz/downloads/commitments/"
