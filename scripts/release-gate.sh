#!/usr/bin/env bash
# release-gate.sh — LANE-AA release-safety gate for sigil-top.
#
# Operator directive (0.76): "so it never happens again" — 0.71.1/0.75 shipped
# while --selfcheck was green, because selfcheck only proves the binary PRINTS
# A VERSION, not that the app comes up and STAYS up. This gate BLOCKS publish
# until all three checks are green:
#
#   GATE 1  RUN-TEST           run the fresh binary in a real pty (script -qec),
#                              assert it reaches run_tui and STAYS UP >= 15s.
#                              Parse the startup breadcrumb log: exactly ONE
#                              "main() entry", an "entering run_tui", NO second
#                              entry (self-relaunch loop!), NO "PANIC".
#   GATE 2  CHANNEL CONSISTENCY manifest .version == linux binary --selfcheck;
#                              cache-busted served bytes b3 == manifest b3 for
#                              BOTH targets (linux + windows); win exe contains
#                              the version string; manifest .sig verifies
#                              against the pinned release pubkey.
#   GATE 3  UPDATE-PATH        run the PREVIOUS published version against the
#                              (newly flipped) channel; assert it lands on the
#                              new version AND stays up >= 15s after updating
#                              (no relaunch-loop, no detach). Used by `publish`
#                              mode, which flips the manifest and AUTO-REVERTS
#                              it if this check fails.
#
# Modes:
#   release-gate.sh run-test <binary>                  GATE 1 only (acceptance harness)
#   release-gate.sh check   [<manifest.json>]          GATE 1 + 2 (no channel mutation)
#   release-gate.sh publish [<manifest.json>]          GATE 1 + 2 -> flip channel -> GATE 3
#                                                      (revert flip + RED on failure)
#   release-gate.sh update-path <prev-binary>          GATE 3 standalone (channel as-is)
#
# Wiring:
#   - every RED writes a /sigil-error-log beacon line into the q-flux access
#     log, so `flux_error_tail` (MCP) surfaces gate failures with everything else
#   - every run writes machine-readable status to $GATE_STATUS_JSON; the
#     fluxfood-sentinel hook (scripts/fluxfood-sentinel.sh) alarms on RED/stale
#
# Honest limits:
#   - the windows .exe cannot be executed here: its "selfcheck" is b3 equality
#     (staged == manifest == served) + an embedded-version-string grep
#   - GATE 3 needs the channel flipped to test the real update; `publish` keeps
#     the exposure window short (<~2 min) and auto-reverts manifest+sig on RED.
#     Clients also have the crash-loop guard (auto-revert after 3 strikes).

set -uo pipefail

# ── paths + constants ────────────────────────────────────────────────────────
DOWNLOADS="${DOWNLOADS:-/home/orobit/q-narwhalknight/dist-fluxapp/downloads}"
MANIFEST_NAME="sigil-top-latest.json"
CHANNEL_BASE="${CHANNEL_BASE:-https://sigilgraph.fluxapp.xyz/downloads}"
GATE_STATUS_JSON="${GATE_STATUS_JSON:-/home/orobit/sigil-data/release-gate-status.json}"
QFLUX_ACCESS_LOG="${QFLUX_ACCESS_LOG:-/home/storage/logs/q-flux/access.log}"
SCRATCH_ROOT="${SCRATCH_ROOT:-/home/storage/tmp}"   # NEVER /tmp — Epsilon root disk is tiny
# pinned release-manifest signing pubkey (sigil-top main.rs RELEASE_SIGN_PUBKEY_HEX)
RELEASE_SIGN_PUBKEY_HEX="150fb84d4b2c83e6e81a27f629e60686acf8663be5ce73f46208cce4f5686402"
RUN_SECS="${RUN_SECS:-15}"          # the stays-up bar
UPDATE_WAIT_SECS="${UPDATE_WAIT_SECS:-120}" # max wait for the update path to converge

PASS=()
FAIL=()
MODE="${1:-}"

say()  { printf '%s\n' "$*"; }
green(){ printf '\033[32m%s\033[0m\n' "$*"; }
red()  { printf '\033[31m%s\033[0m\n' "$*"; }

# ── wiring: flux_error_tail beacon + sentinel status file ────────────────────
beacon_fail() {
  # Emulate the wallet error beacon (GET /sigil-error-log?msg=...) in the q-flux
  # access log — flux_error_tail greps exactly this path and URL-decodes msg/at/t.
  local msg; msg=$(printf '%s' "$1" | sed 's/ /%20/g; s/"/%22/g')
  local line
  line="$(date -u '+%d/%b/%Y:%H:%M:%S +0000') 127.0.0.1 \"GET /sigil-error-log?msg=${msg}&at=release-gate&t=$(date +%s%3N) HTTP/1.1\" 404 0"
  [ -w "$(dirname "$QFLUX_ACCESS_LOG")" ] && printf '%s\n' "$line" >> "$QFLUX_ACCESS_LOG" 2>/dev/null
}

write_status() { # $1 = GREEN|RED, $2 = mode, $3 = version-under-test
  mkdir -p "$(dirname "$GATE_STATUS_JSON")" 2>/dev/null
  {
    printf '{\n  "gate": "release-gate",\n  "result": "%s",\n  "mode": "%s",\n  "version": "%s",\n' "$1" "$2" "$3"
    printf '  "ts_unix": %s,\n  "host": "%s",\n' "$(date +%s)" "$(hostname)"
    printf '  "pass": ['; local first=1; for p in "${PASS[@]:-}"; do [ -z "$p" ] && continue; [ $first -eq 0 ] && printf ','; printf '"%s"' "$p"; first=0; done; printf '],\n'
    printf '  "fail": ['; first=1; for f in "${FAIL[@]:-}"; do [ -z "$f" ] && continue; [ $first -eq 0 ] && printf ','; printf '"%s"' "$f"; first=0; done; printf ']\n}\n'
  } > "$GATE_STATUS_JSON".tmp && mv "$GATE_STATUS_JSON".tmp "$GATE_STATUS_JSON"
}

ok_check()  { PASS+=("$1"); green "  ✓ $1"; }
bad_check() { FAIL+=("$1"); red   "  ✗ $1"; beacon_fail "release-gate FAIL: $1"; }

cleanup_pids() { # kill a pty harness tree quietly (by exact pid — never pkill -f)
  for p in "$@"; do
    [ -n "$p" ] && kill -9 "$p" 2>/dev/null
    # children (script's spawned shell + binary)
    for c in $(pgrep -P "$p" 2>/dev/null); do kill -9 "$c" 2>/dev/null; done
  done
}

json_get() { # json_get <file> <python-expr over d>
  python3 -c "import json,sys; d=json.load(open(sys.argv[1])); print(eval(sys.argv[2]))" "$1" "$3" 2>/dev/null
}

# ── GATE 1: RUN-TEST ─────────────────────────────────────────────────────────
# $1 = binary path, $2 = label. Returns 0 green.
run_test() {
  local bin="$1" label="$2"
  say "── GATE 1 · RUN-TEST [$label] ──"
  [ -x "$bin" ] || { bad_check "RUN-TEST[$label]: binary not executable: $bin"; return 1; }

  local iso; iso=$(mktemp -d "$SCRATCH_ROOT/release-gate.XXXXXX")
  mkdir -p "$iso/tmp" "$iso/home"
  cp "$bin" "$iso/sigil-top.test" && chmod +x "$iso/sigil-top.test"

  # selfcheck is necessary-but-not-sufficient — still required to even enter
  local sv; sv=$(timeout 10 "$iso/sigil-top.test" --selfcheck 2>/dev/null | tail -1)
  if [ -z "$sv" ]; then bad_check "RUN-TEST[$label]: --selfcheck printed nothing"; rm -rf "$iso"; return 1; fi
  say "  selfcheck: $sv (necessary, NOT sufficient — now running it for real)"

  # the real test: a pty via script(1), isolated TMPDIR (startup log) + HOME,
  # auto-update OFF so we test THIS binary, not whatever the channel serves.
  TMPDIR="$iso/tmp" HOME="$iso/home" SIGIL_TOP_NO_AUTOUPDATE=1 SIGIL_TOP_TRAY= \
    timeout $((RUN_SECS + 10)) script -qec "'$iso/sigil-top.test'" /dev/null \
    > "$iso/pty.out" 2>&1 &
  local hpid=$!
  sleep "$RUN_SECS"
  local alive=0; kill -0 "$hpid" 2>/dev/null && alive=1
  cleanup_pids "$hpid"; wait "$hpid" 2>/dev/null

  local log="$iso/tmp/sigil-top-startup.log"
  local mains=0 tui=0 panics=0
  if [ -f "$log" ]; then
    mains=$(grep -c '^main() entry' "$log" || true)
    tui=$(grep -c 'entering run_tui' "$log" || true)
    panics=$(grep -c '^PANIC' "$log" || true)
  fi
  say "  trace: main_entries=$mains run_tui=$tui panics=$panics alive_at_${RUN_SECS}s=$alive"

  local g=0
  [ "$alive" -eq 1 ]   || { bad_check "RUN-TEST[$label]: process DIED before ${RUN_SECS}s (exit/relaunch/detach)"; g=1; }
  [ "$mains" -eq 1 ]   || { bad_check "RUN-TEST[$label]: expected exactly 1 main() entry, got $mains (self-relaunch loop?)"; g=1; }
  [ "$tui" -ge 1 ]     || { bad_check "RUN-TEST[$label]: never reached run_tui"; g=1; }
  [ "$panics" -eq 0 ]  || { bad_check "RUN-TEST[$label]: PANIC in startup log: $(grep '^PANIC' "$log" | head -1)"; g=1; }
  [ $g -eq 0 ] && ok_check "RUN-TEST[$label]: v$sv reached run_tui and stayed up ${RUN_SECS}s (1 entry, 0 panics)"
  rm -rf "$iso"
  return $g
}

# ── GATE 2: CHANNEL CONSISTENCY ──────────────────────────────────────────────
# $1 = candidate manifest path. Verifies version/selfcheck/b3/sig for BOTH targets.
channel_check() {
  local mf="$1"
  say "── GATE 2 · CHANNEL CONSISTENCY [$mf] ──"
  [ -f "$mf" ] || { bad_check "CHANNEL: manifest not found: $mf"; return 1; }
  local ver lurl lb3 wurl wb3
  ver=$(json_get "$mf" . "d['version']")
  lurl=$(json_get "$mf" . "d['targets']['linux-x64']['url']")
  lb3=$(json_get "$mf" . "d['targets']['linux-x64']['blake3_hex']")
  wurl=$(json_get "$mf" . "d['targets']['windows-x64']['url']")
  wb3=$(json_get "$mf" . "d['targets']['windows-x64']['blake3_hex']")
  [ -n "$ver" ] || { bad_check "CHANNEL: manifest has no .version"; return 1; }
  say "  manifest: v$ver"
  local g=0 bust; bust=$(date +%s%N)

  # linux: served bytes (cache-busted) → b3 + EXECUTED selfcheck
  local lt; lt=$(mktemp "$SCRATCH_ROOT/rg-linux.XXXXXX")
  if curl -fsS --max-time 60 "${lurl}?t=${bust}" -o "$lt"; then
    local served_b3; served_b3=$(b3sum --no-names "$lt")
    if [ "$served_b3" = "$lb3" ]; then ok_check "CHANNEL: linux served b3 == manifest b3"; else
      bad_check "CHANNEL: linux served b3 ($served_b3) != manifest b3 ($lb3)"; g=1; fi
    chmod +x "$lt"
    local lsv; lsv=$(timeout 10 "$lt" --selfcheck 2>/dev/null | tail -1)
    if [ "$lsv" = "$ver" ]; then ok_check "CHANNEL: linux served --selfcheck ($lsv) == manifest version"; else
      bad_check "CHANNEL: linux served --selfcheck ($lsv) != manifest version ($ver)"; g=1; fi
  else bad_check "CHANNEL: could not fetch linux target $lurl"; g=1; fi

  # windows: served bytes → b3 + embedded version-string grep (cannot execute here)
  local wt; wt=$(mktemp "$SCRATCH_ROOT/rg-win.XXXXXX")
  if curl -fsS --max-time 90 "${wurl}?t=${bust}" -o "$wt"; then
    local wserved_b3; wserved_b3=$(b3sum --no-names "$wt")
    if [ "$wserved_b3" = "$wb3" ]; then ok_check "CHANNEL: windows served b3 == manifest b3"; else
      bad_check "CHANNEL: windows served b3 ($wserved_b3) != manifest b3 ($wb3)"; g=1; fi
    if grep -aqF "$ver" "$wt"; then ok_check "CHANNEL: windows exe embeds version string $ver"; else
      bad_check "CHANNEL: windows exe does NOT embed version string $ver (mislabeled bytes?)"; g=1; fi
  else bad_check "CHANNEL: could not fetch windows target $wurl"; g=1; fi
  rm -f "$lt" "$wt"

  # manifest signature versus the pinned client pubkey (clients hard-require it)
  local sig="$mf.sig"
  [ -f "$sig" ] || sig=$(mktemp "$SCRATCH_ROOT/rg-sig.XXXXXX") && curl -fsS --max-time 30 "${CHANNEL_BASE}/${MANIFEST_NAME}.sig?t=${bust}" -o "$sig" 2>/dev/null
  if [ -s "$sig" ] && python3 - "$mf" "$sig" "$RELEASE_SIGN_PUBKEY_HEX" <<'PYEOF'
import sys
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey
mf, sigf, pk = sys.argv[1], sys.argv[2], sys.argv[3]
sig = bytes.fromhex(open(sigf).read().strip())
Ed25519PublicKey.from_public_bytes(bytes.fromhex(pk)).verify(sig, open(mf,'rb').read())
PYEOF
  then ok_check "CHANNEL: manifest .sig verifies against pinned release pubkey"
  else bad_check "CHANNEL: manifest .sig missing or does NOT verify (clients will reject the channel)"; g=1; fi
  return $g
}

# ── GATE 3: UPDATE-PATH ──────────────────────────────────────────────────────
# $1 = previous-version binary, $2 = expected new version (from channel manifest).
update_path() {
  local prev="$1" newver="$2"
  say "── GATE 3 · UPDATE-PATH [prev=$(basename "$prev") → expect v$newver] ──"
  [ -x "$prev" ] || { bad_check "UPDATE-PATH: previous binary not executable: $prev"; return 1; }

  local iso; iso=$(mktemp -d "$SCRATCH_ROOT/release-gate-up.XXXXXX")
  mkdir -p "$iso/tmp" "$iso/home"
  cp "$prev" "$iso/sigil-top.test" && chmod +x "$iso/sigil-top.test"
  local pv; pv=$(timeout 10 "$iso/sigil-top.test" --selfcheck 2>/dev/null | tail -1)

  # Step 1 — drive the documented update path: the `update` subcommand (same
  # engine as the [U] key + the startup check) must fetch the channel,
  # BLAKE3-verify, self-replace THIS file and relaunch into the new version.
  # Era-independent and deterministic, unlike waiting for a background timer
  # (pre-0.75 binaries have no startup auto-update — only [U]/background).
  say "  previous version: $pv — running '$(basename "$prev") update' against the live channel"
  TMPDIR="$iso/tmp" HOME="$iso/home" \
    timeout "$UPDATE_WAIT_SECS" script -qec "'$iso/sigil-top.test' update" /dev/null \
    > "$iso/update.out" 2>&1
  local fv; fv=$(timeout 10 "$iso/sigil-top.test" --selfcheck 2>/dev/null | tail -1)
  local g=0
  if [ "$fv" = "$newver" ]; then ok_check "UPDATE-PATH: on-disk binary flipped $pv → $newver (swap + relaunch ran)"; else
    bad_check "UPDATE-PATH: update did not converge — binary is '$fv', channel is '$newver' (see update transcript)"
    sed 's/\x1b\[[0-9;]*m//g' "$iso/update.out" | tail -5 | sed 's/^/    │ /'
    g=1
  fi
  local panics=0
  [ -f "$iso/tmp/sigil-top-startup.log" ] && panics=$(grep -c '^PANIC' "$iso/tmp/sigil-top-startup.log" || true)
  [ "$panics" -eq 0 ] || { bad_check "UPDATE-PATH: PANIC during update: $(grep '^PANIC' "$iso/tmp/sigil-top-startup.log" | head -1)"; g=1; }

  # Step 2 — the binary the user is left with must actually BOOT and stay up:
  # full RUN-TEST (fresh pty, fresh startup log) on the post-update file. This
  # is where "updated in place but never restarts / relaunch-loops" shows up.
  if [ $g -eq 0 ]; then
    run_test "$iso/sigil-top.test" "post-update-v$newver" || g=1
  fi
  [ $g -eq 0 ] && ok_check "UPDATE-PATH: $pv → $newver — updated, relaunched, and the result boots + stays up"
  rm -rf "$iso"
  return $g
}

# find the previous published linux binary for GATE 3 (the version the live
# channel served before this candidate): downloads/sigil-top-v<ver> naming.
find_prev_binary() {
  local cur_ver="$1"
  for f in $(ls -t "$DOWNLOADS"/sigil-top-v*[0-9] 2>/dev/null); do
    local bn v; bn=$(basename "$f"); v="${bn#sigil-top-v}"
    [ "$v" = "$cur_ver" ] && continue
    [ -x "$f" ] && file -b "$f" | grep -q 'ELF' && { echo "$f"; return 0; }
  done
  return 1
}

verdict() { # $1 mode, $2 version
  say ""
  if [ ${#FAIL[@]} -eq 0 ]; then
    write_status GREEN "$1" "$2"
    green "✅ RELEASE GATE GREEN (${#PASS[@]} checks) — publish may proceed"
    return 0
  else
    write_status RED "$1" "$2"
    red "⛔ RELEASE GATE RED — publish BLOCKED. Failed checks:"
    for f in "${FAIL[@]}"; do red "   · $f"; done
    return 1
  fi
}

# ── modes ────────────────────────────────────────────────────────────────────
case "$MODE" in
  run-test)
    BIN="${2:?usage: release-gate.sh run-test <binary>}"
    run_test "$BIN" "$(basename "$BIN")"
    verdict run-test "$(timeout 10 "$BIN" --selfcheck 2>/dev/null | tail -1)"
    ;;

  check)
    MF="${2:-$DOWNLOADS/$MANIFEST_NAME}"
    VER=$(json_get "$MF" . "d['version']")
    # GATE 1 on the staged/served linux binary for this manifest
    LBIN=$(mktemp "$SCRATCH_ROOT/rg-bin.XXXXXX")
    LURL=$(json_get "$MF" . "d['targets']['linux-x64']['url']")
    curl -fsS --max-time 60 "${LURL}?t=$(date +%s%N)" -o "$LBIN" && chmod +x "$LBIN" \
      || bad_check "fetch of linux target failed: $LURL"
    [ -s "$LBIN" ] && run_test "$LBIN" "linux-served-v$VER"
    rm -f "$LBIN"
    channel_check "$MF"
    verdict check "$VER"
    ;;

  update-path)
    PREV="${2:?usage: release-gate.sh update-path <prev-binary>}"
    # the channel fetch must be robust — a transient reset must not run the
    # gate with an EMPTY expected version (garbage comparisons).
    NEWVER=""
    for attempt in 1 2 3; do
      NEWVER=$(curl -fsS --max-time 30 "${CHANNEL_BASE}/${MANIFEST_NAME}?t=$(date +%s%N)" 2>/dev/null \
        | python3 -c "import json,sys; print(json.load(sys.stdin)['version'])" 2>/dev/null)
      [ -n "$NEWVER" ] && break
      sleep 2
    done
    [ -n "$NEWVER" ] || { bad_check "UPDATE-PATH: could not fetch channel manifest (3 attempts) — aborting, NOT a verdict on the binary"; verdict update-path unknown; exit 1; }
    update_path "$PREV" "$NEWVER"
    verdict update-path "$NEWVER"
    ;;

  publish)
    # full pipeline: gate 1+2 on the CANDIDATE manifest → flip channel → gate 3
    # → auto-revert flip if gate 3 fails.
    CAND="${2:?usage: release-gate.sh publish <candidate-manifest.json> (with .sig next to it)}"
    VER=$(json_get "$CAND" . "d['version']")
    LIVE_MF="$DOWNLOADS/$MANIFEST_NAME"
    PREV_VER=$(json_get "$LIVE_MF" . "d['version']")
    say "candidate v$VER (live channel currently v$PREV_VER)"
    [ -f "$CAND.sig" ] || { bad_check "publish: $CAND.sig missing — sign-manifest.sh first"; verdict publish "$VER"; exit 1; }

    LBIN=$(mktemp "$SCRATCH_ROOT/rg-bin.XXXXXX")
    LURL=$(json_get "$CAND" . "d['targets']['linux-x64']['url']")
    curl -fsS --max-time 60 "${LURL}?t=$(date +%s%N)" -o "$LBIN" && chmod +x "$LBIN" \
      || bad_check "publish: fetch of staged linux target failed (upload binaries BEFORE the gate): $LURL"
    [ -s "$LBIN" ] && run_test "$LBIN" "candidate-v$VER"
    rm -f "$LBIN"
    channel_check "$CAND"

    if [ ${#FAIL[@]} -gt 0 ]; then verdict publish "$VER"; exit 1; fi

    PREV_BIN=$(find_prev_binary "$VER") || { bad_check "publish: no previous linux binary found in $DOWNLOADS for GATE 3"; verdict publish "$VER"; exit 1; }

    say "── flipping channel v$PREV_VER → v$VER (backup kept; auto-revert on GATE 3 RED) ──"
    cp -a "$LIVE_MF" "$LIVE_MF.gate-backup" 2>/dev/null
    cp -a "$LIVE_MF.sig" "$LIVE_MF.sig.gate-backup" 2>/dev/null
    cp "$CAND" "$LIVE_MF" && cp "$CAND.sig" "$LIVE_MF.sig"

    if update_path "$PREV_BIN" "$VER" && [ ${#FAIL[@]} -eq 0 ]; then
      rm -f "$LIVE_MF.gate-backup" "$LIVE_MF.sig.gate-backup"
      verdict publish "$VER"
    else
      red "── GATE 3 RED → REVERTING channel to v$PREV_VER ──"
      [ -f "$LIVE_MF.gate-backup" ] && mv "$LIVE_MF.gate-backup" "$LIVE_MF"
      [ -f "$LIVE_MF.sig.gate-backup" ] && mv "$LIVE_MF.sig.gate-backup" "$LIVE_MF.sig"
      beacon_fail "release-gate: publish of v$VER REVERTED (update-path failed)"
      verdict publish "$VER"
    fi
    ;;

  *)
    sed -n '2,40p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
    ;;
esac
