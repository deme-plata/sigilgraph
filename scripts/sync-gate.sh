#!/usr/bin/env bash
# sync-gate.sh — LANE-P CI GATE for the sigil-top backfill sync.
#
# FAILS (exit 1) if a FRESH-DB sigil-top sync does not reach +THRESHOLD contiguous
# blocks within WINDOW seconds against the live sigil-g0 testnet. This is the
# regression gate for the frontier-stall class of bugs (the old freeze at
# synced_to=57345 = unaligned base 20481 + 9*CHUNK). Run it before every sync release.
#
# Usage:   scripts/sync-gate.sh [full|recent]
#   full   (default) the genesis-anchored contiguous crawl (recent_only=false)
#   recent the MONITOR recent-window-snap path (recent_only=true) — the exact branch
#          the 57345 freeze lived in.
#
# Env:  SIGIL_TOP_BIN, SYNC_GATE_THRESHOLD (default 50000), SYNC_GATE_WINDOW (default 60)
#
# Discipline: ionice -c3 nice -19 (never starve the live node); fresh temp DB on
# /home/storage (epsilon root is tiny); cleans up after itself.
set -uo pipefail

BIN="${SIGIL_TOP_BIN:-/home/storage/deepseek-codewhale/.target-shared/release/sigil-top}"
THRESHOLD="${SYNC_GATE_THRESHOLD:-50000}"
WINDOW="${SYNC_GATE_WINDOW:-60}"
MODE="${1:-full}"
DB="/home/storage/tmp/sync-gate-$$.db"; rm -rf "$DB"
LOG="$(mktemp /tmp/sync-gate-XXXX.log)"
EXTRA=""; [ "$MODE" = "recent" ] && EXTRA="--recent"

[ -x "$BIN" ] || { echo "[sync-gate] FAIL — binary not found/executable: $BIN"; exit 2; }
echo "[sync-gate] mode=$MODE threshold=+$THRESHOLD window=${WINDOW}s bin=$(basename "$BIN") db=$DB"

SIGIL_TLOG=1 SIGIL_TOP_DB="$DB" ionice -c3 nice -19 "$BIN" full-sync $EXTRA > "$LOG" 2>&1 &
PID=$!

HIGH=0; PASS=0
for _ in $(seq 1 "$WINDOW"); do
  sleep 1
  kill -0 "$PID" 2>/dev/null || break   # process exited (e.g. reached tip) — last HIGH stands
  s=$(grep -oE 'synced=[0-9]+' "$LOG" 2>/dev/null | sed 's/synced=//' | sort -n | tail -1)
  [ -n "$s" ] && HIGH="$s"
  if [ "${HIGH:-0}" -ge "$THRESHOLD" ]; then PASS=1; break; fi
done
kill -9 "$PID" 2>/dev/null

DBG=$(grep '\[DBG\]' "$LOG" 2>/dev/null | tail -1 | sed 's/\x1b\[[0-9;]*m//g')
if [ "$PASS" = 1 ]; then
  echo "[sync-gate] PASS — reached synced=$HIGH (>= $THRESHOLD) within ${WINDOW}s [$MODE]"
  rm -rf "$DB" "$LOG"; exit 0
else
  echo "[sync-gate] FAIL — only reached synced=$HIGH (< $THRESHOLD) in ${WINDOW}s [$MODE]"
  echo "[sync-gate] state: ${DBG:-<no DBG line — mesh never connected?>}"
  rm -rf "$DB"; exit 1
fi
