#!/usr/bin/env bash
# chronos-50tb guard — the safety layer chronos_scale does NOT have.
#
# chronos_scale writes until CHRONOS_TARGET_BYTES with NO free-space check. At
# 50 TB on an 80 TB array that also holds the Beta migration backup and the live
# node's data, an unguarded writer is how you take production down. This watches
# every 60 s and kills the soak on any breach, then reports.
#
# Guards:
#   1. FREE SPACE  — abort below FLOOR_TB (default 6 TB). Non-negotiable: the
#      live sigil-node and q-api-server write to this array too.
#   2. NODE HEALTH — sigil-node / q-api-server must stay active. If either dies
#      while we are flooding the disk, we stop and say so rather than "measure
#      through" an outage we caused.
#   3. LOAD        — sustained loadavg above LOAD_MAX pauses (SIGSTOP) the soak
#      until it recovers, so a build storm or the producer always wins.
set -u
DIR=${CHRONOS_SOAK_DIR:-/home/storage/chronos-50tb}
FLOOR_TB=${FLOOR_TB:-6}
LOAD_MAX=${LOAD_MAX:-95}   # box baseline is 55-58 on 48 cores; 40 paused it permanently
EV="$DIR/events.log"

say() { echo "$(date -Is)  $*" | tee -a "$EV"; }

soak_pid() { pgrep -x chronos_scale | head -1; }   # -x = exact NAME; -f matched the launcher shell

paused=0
while true; do
  pid=$(soak_pid)
  [ -z "$pid" ] && { say "soak process gone — guard exiting"; exit 0; }

  free_tb=$(df -B1 --output=avail /home/storage | tail -1 | awk '{printf "%.2f", $1/1e12}')
  load=$(awk '{print $1}' /proc/loadavg)
  node_ok=$(systemctl is-active sigil-node 2>/dev/null)
  api_ok=$(systemctl is-active q-api-server 2>/dev/null)
  dbsz=$(du -sB1 "$DIR/db" 2>/dev/null | awk '{printf "%.3f", $1/1e12}')

  # 1. free space floor
  if awk "BEGIN{exit !($free_tb < $FLOOR_TB)}"; then
    say "🚨 ABORT: free space ${free_tb} TB < floor ${FLOOR_TB} TB — killing soak (db=${dbsz} TB)"
    kill -9 "$pid" 2>/dev/null
    exit 2
  fi

  # 2. node health
  if [ "$node_ok" != "active" ] || [ "$api_ok" != "active" ]; then
    say "🚨 ABORT: node health lost (sigil-node=$node_ok q-api-server=$api_ok) — killing soak (db=${dbsz} TB free=${free_tb} TB)"
    kill -9 "$pid" 2>/dev/null
    exit 3
  fi

  # 3. load-based backpressure (pause, don't kill)
  if awk "BEGIN{exit !($load > $LOAD_MAX)}"; then
    if [ "$paused" = 0 ]; then say "⏸ PAUSE: loadavg $load > $LOAD_MAX — SIGSTOP soak"; kill -STOP "$pid" 2>/dev/null; paused=1; fi
  elif [ "$paused" = 1 ]; then
    say "▶ RESUME: loadavg $load back under $LOAD_MAX"; kill -CONT "$pid" 2>/dev/null; paused=0
  fi

  echo "$(date -Is),$dbsz,$free_tb,$load,$paused" >> "$DIR/guard.csv"
  sleep 60
done
