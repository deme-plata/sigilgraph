#!/usr/bin/env bash
# fluxfood-sentinel.sh — LANE-AA sentinel hook for the release gate.
#
# The lane asked the gate to be wired into "fluxfood-sentinel"; no such daemon
# existed, so this IS it (minimal, cron/systemd-timer friendly): read the gate's
# machine-readable status JSON and ALARM (nonzero exit + flux_error_tail beacon)
# when the latest gate run is RED, or when a publish happened without a gate run
# (manifest newer than the last gate status = someone bypassed the gate).
#
# Wire-up (either):
#   cron:    */10 * * * *  /home/storage/deepseek-codewhale/sigil/scripts/fluxfood-sentinel.sh
#   systemd: a 10-min OnUnitActiveSec timer running this oneshot
#
# Exit codes: 0 = healthy · 1 = gate RED · 2 = gate bypassed/stale

set -uo pipefail
GATE_STATUS_JSON="${GATE_STATUS_JSON:-/home/orobit/sigil-data/release-gate-status.json}"
DOWNLOADS="${DOWNLOADS:-/home/orobit/q-narwhalknight/dist-fluxapp/downloads}"
QFLUX_ACCESS_LOG="${QFLUX_ACCESS_LOG:-/home/storage/logs/q-flux/access.log}"

beacon() {
  local msg; msg=$(printf '%s' "$1" | sed 's/ /%20/g; s/"/%22/g')
  printf '%s 127.0.0.1 "GET /sigil-error-log?msg=%s&at=fluxfood-sentinel&t=%s HTTP/1.1" 404 0\n' \
    "$(date -u '+%d/%b/%Y:%H:%M:%S +0000')" "$msg" "$(date +%s%3N)" >> "$QFLUX_ACCESS_LOG" 2>/dev/null
}

if [ ! -f "$GATE_STATUS_JSON" ]; then
  beacon "fluxfood-sentinel: NO gate status file — release gate has never run on this host"
  echo "⚠ no $GATE_STATUS_JSON"; exit 2
fi

RESULT=$(python3 -c "import json;print(json.load(open('$GATE_STATUS_JSON'))['result'])" 2>/dev/null)
GATE_TS=$(python3 -c "import json;print(json.load(open('$GATE_STATUS_JSON'))['ts_unix'])" 2>/dev/null || echo 0)
MF_TS=$(stat -c %Y "$DOWNLOADS/sigil-top-latest.json" 2>/dev/null || echo 0)

if [ "$RESULT" = "RED" ]; then
  beacon "fluxfood-sentinel: latest release-gate run is RED"
  echo "⛔ gate RED (see $GATE_STATUS_JSON)"; exit 1
fi
# manifest flipped AFTER the last green gate run (+120s slack for the gate's own flip)
if [ "$MF_TS" -gt $((GATE_TS + 120)) ]; then
  beacon "fluxfood-sentinel: sigil-top-latest.json changed WITHOUT a gate run — publish bypassed the release gate"
  echo "⚠ manifest newer than last gate run (gate bypassed?)"; exit 2
fi
echo "✓ sentinel healthy: gate=$RESULT, last run $(date -d @"$GATE_TS" '+%F %T' 2>/dev/null)"
exit 0
