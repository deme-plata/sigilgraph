#!/usr/bin/env bash
# prewarm-sigil.sh — keep the crates we actually iterate on compile-warm, and MEASURE
# whether that is working instead of asserting it.
#
# ── What is actually known (measured on Epsilon, 2026-09-02) ─────────────────
#
#  * The flux content-hash cache is REAL and populated: 28 GB, 10,457 blob dirs,
#    ~33.7 % unit hit rate (36,582 / 108,409, from `flux_stats`). It fills only
#    through the RUSTC_WRAPPER path, which is why raw cargo is banned here — raw
#    cargo does not just break the dogfood proof, it bypasses the cache entirely.
#    (If you ever "discover" this cache is empty: /root/.flux/cache is a SYMLINK to
#    /home/storage/flux-cache, and neither `du -sh` nor `find` follows symlinks.
#    Use `du -shL`. That mistake cost a session once already.)
#
#  * Warm beats cold by a lot on the crates that matter:
#        sigil-top   cold check   49.9 s
#        sigil-api   warm check   13.2 s
#
# ── SETTLED by measurement (2026-09-02, idle box, `--measure sigil-fees`) ────
#
#   1 lib          3s
#   2 lib again    9s
#   3 tests        8s
#   4 lib again    1s     <- would be ~3s+ if the shapes evicted each other
#   5 tests again  0s
#
# It was HYPOTHESISED that `check -p X` and `check -p X --tests` are disjoint
# fingerprint universes, so alternating them costs a full rebuild each way.
# **That is FALSE.** Step 4 returned in 1 s and step 5 in 0 s — after both shapes had
# been touched once, every subsequent check of either is essentially free. The shapes
# share their work; alternating is not a cost.
#
# The hypothesis looked true earlier only because the first attempt was measured while
# a release build saturated eight cores (48.8s / 131s / 138s). Those numbers were
# contention, not fingerprints. Hence `--measure` now reads /proc/loadavg and warns:
# a busy box will manufacture whatever conclusion you were hoping for.
#
# What survives is the STRATEGY, for a better reason than the original one: warming is
# worth doing not because the shapes fight, but because a genuine fixed point exists —
# once the matrix is warm it stays warm at 0-1 s per cell. This script drives to that
# fixed point and then proves it reached one, rather than assuming a pass was enough.
#
# ── Discipline ───────────────────────────────────────────────────────────────
#
#  * fluxc only, never raw cargo.
#  * FLUX_WRAPPER_PATH pins ONE wrapper identity. Cargo hashes the wrapper into every
#    unit fingerprint, so two agents with different wrapper paths silently invalidate
#    each other's work — the single most expensive coordination failure here.
#  * nice/ionice + a capped systemd scope: a live production sigil-node runs on this
#    box and must never lose CPU or IO to a warm-up.
#
# Usage:
#   scripts/prewarm-sigil.sh [crate ...]     warm the hot set (default) or named crates
#   scripts/prewarm-sigil.sh --measure       controlled shape experiment on one small crate
#   SHAPES="lib" scripts/prewarm-sigil.sh    warm only the library shape
set -uo pipefail

FLUXC="${FLUXC:-/home/storage/deepseek-codewhale/flux/target/debug/fluxc}"
REPO="${REPO:-/home/storage/deepseek-codewhale/sigil}"
export FLUX_WRAPPER_PATH="${FLUX_WRAPPER_PATH:-/root/.flux/bin/fluxc}"

# Ranked by REAL commit churn over the last 200 commits — these are the crates a
# session actually rebuilds, not a guess. Recompute with:
#   git log --name-only --pretty=format: -200 | grep '^crates/' | cut -d/ -f2 | sort | uniq -c | sort -rn
DEFAULT_CRATES="sigil-top sigil-node sigil-api sigil-shield sigil-dagknight sigil-tx sigil-state"

[ -x "$FLUXC" ] || { echo "✗ fluxc not found at $FLUXC" >&2; exit 1; }
cd "$REPO" || exit 1

# One capped, polite check. Echoes elapsed seconds, or FAIL.
timed_check() {  # crate, shape, [profile]
  local crate="$1" shape="$2" profile="${3:-dev}" extra="" prof="" t0 rc
  [ "$shape" = tests ] && extra="--tests"
  # `dev` is cargo's default and takes no flag; anything else is a named profile.
  [ "$profile" != dev ] && prof="--profile $profile"
  t0=$SECONDS
  systemd-run --scope -q -p MemoryMax=16G -p CPUQuota=600% \
    bash -c "cd '$REPO' && ionice -c3 nice -n19 '$FLUXC' check -p '$crate' $extra $prof" >/dev/null 2>&1
  rc=$?
  [ $rc -eq 0 ] && echo $((SECONDS-t0)) || echo FAIL
}

# ── Selecting WHAT to warm ───────────────────────────────────────────────────
#
# A fixed hot-list is a decent prior, but the crates that matter *right now* are the
# ones you just touched plus everything that depends on them — those are exactly the
# units the next build must redo. `--since <ref>` derives that set from git instead of
# guessing, so the warm-up matches the work actually queued up.
#
# Reverse dependencies are resolved from the workspace's own Cargo.toml files (a crate
# that path-depends on a changed crate must itself recompile). One level is enough in
# practice here: sigil's graph is wide and shallow, and each extra level costs real
# minutes on a box shared with a live node.
changed_crates() {  # ref -> crate names, one per line
  local ref="$1"
  git diff --name-only "$ref" -- crates/ 2>/dev/null | cut -d/ -f2 | sort -u
}

reverse_deps() {  # crate names on stdin -> those crates PLUS their direct dependents
  local seeds; seeds=$(cat)
  [ -z "$seeds" ] && return 0
  {
    echo "$seeds"
    for m in crates/*/Cargo.toml; do
      local me; me=$(basename "$(dirname "$m")")
      while read -r seed; do
        [ -z "$seed" ] && continue
        # a path dependency on the changed crate == this crate rebuilds too
        grep -qE "^[[:space:]]*${seed}[[:space:]]*=.*path[[:space:]]*=" "$m" && { echo "$me"; break; }
      done <<< "$seeds"
    done
  } | sort -u
}

if [ "${1:-}" = "--since" ]; then
  REF="${2:-HEAD~1}"
  mapfile -t SEEDS < <(changed_crates "$REF")
  if [ "${#SEEDS[@]}" -eq 0 ] || [ -z "${SEEDS[0]}" ]; then
    echo "▸ no crate changed since $REF — nothing to warm."
    exit 0
  fi
  mapfile -t SELECTED < <(printf '%s\n' "${SEEDS[@]}" | reverse_deps)
  echo "▸ changed since $REF: ${SEEDS[*]}"
  echo "  + dependents        -> ${SELECTED[*]}"
  echo
  set -- "${SELECTED[@]}"
fi

if [ "${1:-}" = "--measure" ]; then
  C="${2:-sigil-fees}"
  load=$(cut -d' ' -f1 /proc/loadavg)
  echo "▸ shape experiment on '$C'   (1-min load average: $load)"
  case "$load" in [0-9].*|[0-3]) ;; *) echo "  ⚠ box is BUSY — these numbers will be noise. Re-run when idle." ;; esac
  echo "  1 lib            : $(timed_check "$C" lib)s"
  echo "  2 lib again      : $(timed_check "$C" lib)s   <- same shape; near-zero means warm"
  echo "  3 tests          : $(timed_check "$C" tests)s"
  echo "  4 lib again      : $(timed_check "$C" lib)s   <- if >> step 2, shapes DO evict each other"
  echo "  5 tests again    : $(timed_check "$C" tests)s"
  echo
  echo "  Read it this way: step 4 is the whole question. Close to step 2 => the shapes"
  echo "  coexist and alternating is free. Close to step 1 => they evict each other and"
  echo "  you should pick one shape per loop. Anything else => the box was not idle."
  exit 0
fi

read -ra CRATES <<< "${*:-$DEFAULT_CRATES}"
read -ra SHAPES <<< "${SHAPES:-lib tests}"
# PROFILES is the third dimension, and it is not decoration — `release-fast` is the GATE
# profile. It inherits `release`, so `debug_assertions` stays OFF, which is the property the
# sigil-shield prover tests actually need: in `dev` they are `#[cfg_attr(debug_assertions,
# ignore)]`d because winterfell 0.9 trips `validate_transition_degrees`, so a green dev run
# reports them as IGNORED and proves nothing about the prover. Measured 2026-09-02 on
# sigil-api: dev said "182 passed, 7 ignored"; release-fast said "189 passed, 0 IGNORED".
# Those seven are the difference between a suite that ran and a suite that looked like it did.
#
# It is cheap enough to keep warm: release-fast is opt-level 1, lto off, codegen-units 256, so
# the sigil-api test build took 1m04s — against the tens of minutes a real `release` relink of
# sigil-top costs under codegen-units=1 + thin LTO. That is the operator ruling working as
# intended: release-fast for gates, release for shipping. `release` is deliberately NOT warmed
# here — it is a shipping profile, it is enormous, and warming it would starve the live node.
read -ra PROFILES <<< "${PROFILES:-dev release-fast}"

# ── The hybrid loop: `while` converges, `for` enumerates ─────────────────────
#
# A single pass is not enough to claim "warm". Warming crate B can invalidate work
# for crate A (shared dependencies get rebuilt with different features; another agent
# may touch the same target dir mid-pass). One pass therefore leaves you *probably*
# warm, which is exactly the uncertainty this script exists to remove.
#
# So the structure is:
#
#   while   not converged and passes remain      <- "does it MATCH every time?"
#     for   each crate                           <- bounded, deterministic enumeration
#       for each shape                           <-   (crate x shape matrix)
#
# Convergence = every cell in the matrix completed at or under WARM_S seconds in the
# SAME pass. That is a real fixed point, not a fixed number of repetitions: the loop
# stops as soon as an entire pass is warm, and keeps going (up to MAX_PASSES) while
# anything is still cold. A pass that fails to improve on the previous one twice in a
# row aborts — that means something outside this script keeps flipping fingerprints,
# and more passes would just burn the CPU a live node needs.
WARM_S="${WARM_S:-8}"           # a cell at/below this is considered warm
MAX_PASSES="${MAX_PASSES:-3}"   # hard ceiling; a live node shares this box

echo "▸ prewarm  repo=$REPO"
echo "  crates: ${CRATES[*]}"
echo "  shapes: ${SHAPES[*]}"
echo "  profiles: ${PROFILES[*]}   (release-fast is the GATE profile — prover tests only run there)"
echo "  converge: every cell <= ${WARM_S}s in one pass (max ${MAX_PASSES} passes)"
echo

TOTAL=$SECONDS
pass=0
converged=0
prev_cold=999999
stagnant=0

while [ "$pass" -lt "$MAX_PASSES" ]; do
  pass=$((pass + 1))
  cold=0
  failed=0
  echo "── pass $pass ─────────────────────────────────────────"
  for crate in "${CRATES[@]}"; do
    if [ ! -d "crates/$crate" ]; then
      printf "  %-18s (absent — skipped)\n" "$crate"
      continue
    fi
    line="  $(printf '%-18s' "$crate")"
    for profile in "${PROFILES[@]}"; do
      for shape in "${SHAPES[@]}"; do
        r=$(timed_check "$crate" "$shape" "$profile")
        if [ "$r" = FAIL ]; then
          failed=$((failed + 1)); mark="✗"
        elif [ "$r" -le "$WARM_S" ]; then
          mark="✓"
        else
          cold=$((cold + 1)); mark="·"
        fi
        tag="$shape"; [ "$profile" != dev ] && tag="$shape/${profile%%-*}f"
        line="$line $(printf '%-15s' "$mark $tag=${r}s")"
      done
    done
    echo "$line"
  done
  echo "  pass $pass: $cold cold cell(s), $failed failure(s)"

  if [ "$failed" -gt 0 ]; then
    echo "  ✗ a check FAILED — that is a compile error, not a cold cache. Stopping."
    exit 1
  fi
  if [ "$cold" -eq 0 ]; then
    converged=1
    echo "  ✓ converged: every cell warm in a single pass"
    break
  fi
  # No improvement twice running => an external actor owns the fingerprints, not us.
  if [ "$cold" -ge "$prev_cold" ]; then
    stagnant=$((stagnant + 1))
    if [ "$stagnant" -ge 2 ]; then
      echo "  ⚠ no improvement across two passes — something OUTSIDE this script is"
      echo "    invalidating fingerprints (different FLUX_WRAPPER_PATH, changed rustflags,"
      echo "    or another agent building this target dir). More passes will not help."
      break
    fi
  else
    stagnant=0
  fi
  prev_cold=$cold
done

echo
if [ "$converged" -eq 1 ]; then
  echo "▸ WARM in $((SECONDS-TOTAL))s over $pass pass(es) — the next real build is incremental."
else
  echo "▸ stopped after $pass pass(es), $((SECONDS-TOTAL))s — still $prev_cold cold cell(s)."
  echo "  Not a failure: it means the matrix does not hold warm on this box right now."
fi
