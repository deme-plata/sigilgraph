#!/usr/bin/env bash
# milestone.sh — stop the soak AT 5 TB for the re-decision, without losing it.
#
# chronos_scale's target is 50 TB and it has no intermediate stop. Killing at 5 TB
# would forfeit the run: the writer replays deterministic heights from 0, so a
# restart would rewrite the same keys rather than continue growing. So we SIGSTOP
# instead — the process stays alive holding all its state, and the decision is:
#     continue → kill -CONT <pid>   (resumes toward 50 TB, nothing lost)
#     finish   → kill <pid> then run chronos_verify for the presence audit
#
# The guard is stopped at the same time: its load-backpressure logic issues its own
# SIGCONT, which would silently un-pause this milestone. Two things sending signals
# to one process is how you get a "paused" run that isn't paused — the same class of
# bug as the pgrep -f mismatch this morning.
set -u
DIR=${CHRONOS_SOAK_DIR:-/home/storage/chronos-50tb}
TARGET_TB=${TARGET_TB:-5}
EV="$DIR/events.log"

say() { echo "$(date -Is)  $*" | tee -a "$EV"; }

# Find the guard by its EXACT argv (bash <path>/guard.sh, exactly 2 args) via
# /proc. `pgrep -f guard.sh` matches any shell whose command line merely CONTAINS
# that string — including the launcher that started it — and killing that instead
# would leave the real guard alive to SIGCONT the writer we just paused. This is
# the third time today the -f form has matched a launcher; do it exactly or not
# at all.
guard_pid() {
  local d a
  for d in /proc/[0-9]*; do
    [ -r "$d/cmdline" ] || continue
    mapfile -d '' -t a < "$d/cmdline" 2>/dev/null || continue
    [ "${#a[@]}" -eq 2 ] || continue
    [ "$(basename "${a[0]}")" = "bash" ] || continue
    [ "$(basename "${a[1]}")" = "guard.sh" ] || continue
    echo "${d#/proc/}"; return 0
  done
}


say "milestone watcher armed: will SIGSTOP the soak at ${TARGET_TB} TB"
while true; do
  pid=$(pgrep -x chronos_scale | head -1)
  [ -z "$pid" ] && { say "milestone: writer gone before ${TARGET_TB} TB — nothing to pause"; exit 0; }

  bytes=$(awk -F, 'END{print $5+0}' "$DIR/metrics.csv" 2>/dev/null)
  tb=$(awk -v b="${bytes:-0}" 'BEGIN{printf "%.3f", b/1e12}')

  if awk "BEGIN{exit !(${tb} >= ${TARGET_TB})}"; then
    kill -STOP "$pid" 2>/dev/null
    # stand the guard down so it cannot SIGCONT us back to life
    gpid=$(guard_pid)
    [ -n "$gpid" ] && kill -9 "$gpid" 2>/dev/null
    state=$(ps -o stat= -p "$pid" | tr -d ' ')
    say "🎯 MILESTONE ${TARGET_TB} TB REACHED — soak PAUSED (pid $pid state=$state), guard stood down"
    say "   resume:  kill -CONT $pid   |   finish: kill $pid && chronos_verify"
    exit 0
  fi
  sleep 120
done
