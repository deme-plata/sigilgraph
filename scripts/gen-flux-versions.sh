#!/usr/bin/env bash
# gen-flux-versions.sh — generate the self-contained code-provenance page the explorer's
# flux-rev tab iframes (https://sigilgraph.fluxapp.xyz/flux-versions.html).
# Data is INLINED at generation time (no fetch, no CORS, works from any node's iframe).
# NO author names — public authorship is pseudonymous (bitknight), commits carry only
# hash / date / subject / tags. Called by release-sigil-top.sh on every release.
set -euo pipefail
REPO="/home/storage/deepseek-codewhale/sigil"
cd "$REPO"

# dist-fluxapp is an OVERLAYFS mount (upper=.fluxapp-upper): direct `cat >` into the
# mount can land as a 0-byte file. Always render to a temp file, then cp to both paths.
OUT_FINAL="${1:-/home/orobit/q-narwhalknight/dist-fluxapp/flux-versions.html}"
OUT=$(mktemp /tmp/flux-versions.XXXXXX.html)
N_COMMITS=200

TOTAL=$(git rev-list --count HEAD)
LATEST_TAG=$(git describe --tags --abbrev=0 2>/dev/null || echo "—")
GEN_DATE=$(date -u +"%Y-%m-%d %H:%M UTC")
HEAD_SHORT=$(git rev-parse --short HEAD)

# rows: short|date|refs|subject  (refs carries "tag: vX.Y.Z" when present)
ROWS_JSON=$(git log -n $N_COMMITS --date=short --format='%h%x00%ad%x00%D%x00%s' | python3 -c '
import json,sys
rows=[]
for line in sys.stdin:
    p=line.rstrip("\n").split("\x00")
    if len(p)!=4: continue
    h,d,refs,s=p
    tags=[r.strip()[5:] for r in refs.split(",") if r.strip().startswith("tag: ")]
    rows.append({"h":h,"d":d,"t":tags,"s":s})
print(json.dumps(rows,separators=(",",":")))')

cat > "$OUT" <<HTML
<!doctype html>
<html lang="en"><head>
<meta charset="UTF-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>SIGIL — Code Provenance</title>
<style>
:root{--bg:#04080c;--cyan:#21d4fd;--cyan-bright:#6df3ff;--teal:#00e0c6;--gold:#fbbf24;
  --line:rgba(33,212,253,.2);--text:#d7eef5;--dim:#5f8390;--panel:rgba(8,16,24,.7)}
*{box-sizing:border-box;margin:0;padding:0}
body{background:var(--bg);color:var(--text);font-family:'Rajdhani',system-ui,sans-serif;padding:22px 26px}
.hd{display:flex;align-items:baseline;gap:14px;flex-wrap:wrap;margin-bottom:6px}
.hd h1{font-size:19px;letter-spacing:2px;color:var(--cyan-bright);text-transform:uppercase;font-family:monospace}
.hd .sub{font-family:monospace;font-size:11px;color:var(--dim)}
.stats{display:flex;gap:10px;flex-wrap:wrap;margin:14px 0 18px}
.stat{border:1px solid var(--line);border-radius:12px;padding:10px 16px;background:var(--panel)}
.stat .v{font-family:monospace;font-size:17px;color:var(--cyan)}
.stat .l{font-family:monospace;font-size:9px;color:var(--dim);text-transform:uppercase;letter-spacing:1px;margin-top:2px}
.row{display:flex;align-items:center;gap:12px;padding:9px 12px;border-bottom:1px solid rgba(33,212,253,.08);border-radius:8px}
.row:hover{background:rgba(33,212,253,.05)}
.row .h{font-family:monospace;font-size:11px;color:var(--cyan);flex:none;width:70px}
.row .d{font-family:monospace;font-size:10.5px;color:var(--dim);flex:none;width:88px}
.row .s{font-size:14px;flex:1;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.tag{font-family:monospace;font-size:9.5px;color:var(--gold);border:1px solid rgba(251,191,36,.4);border-radius:999px;padding:1px 9px;flex:none;background:rgba(251,191,36,.07)}
.row.rel{border-left:2px solid var(--gold);background:rgba(251,191,36,.03)}
.ft{margin-top:16px;font-family:monospace;font-size:10px;color:var(--dim);text-align:center}
</style></head><body>
<div class="hd"><h1>⬡ SIGIL · Code Provenance</h1><span class="sub">flux-rev · signed releases · pseudonymous authorship</span></div>
<div class="stats">
  <div class="stat"><div class="v">$TOTAL</div><div class="l">commits</div></div>
  <div class="stat"><div class="v">$LATEST_TAG</div><div class="l">latest release</div></div>
  <div class="stat"><div class="v">$HEAD_SHORT</div><div class="l">head</div></div>
</div>
<div id="list"></div>
<div class="ft">last $N_COMMITS commits · every release binary ships a require-both SQIsign-L5 + Ed25519 .proof · generated $GEN_DATE</div>
<script>
var ROWS=$ROWS_JSON;
document.getElementById('list').innerHTML=ROWS.map(function(r){
  var tags=(r.t||[]).map(function(t){return '<span class="tag">⬡ '+t+'</span>';}).join('');
  return '<div class="row'+(r.t&&r.t.length?' rel':'')+'"><span class="h">'+r.h+'</span><span class="d">'+r.d+'</span><span class="s">'+r.s.replace(/</g,'&lt;')+'</span>'+tags+'</div>';
}).join('');
</script>
</body></html>
HTML

[ -s "$OUT" ] || { echo "✗ generated page is empty"; exit 1; }
cp "$OUT" "$OUT_FINAL"
cp "$OUT" /home/orobit/q-narwhalknight/.fluxapp-upper/flux-versions.html 2>/dev/null || true
rm -f "$OUT"
echo "✓ flux-versions.html generated ($TOTAL commits, latest $LATEST_TAG) → $OUT_FINAL"
