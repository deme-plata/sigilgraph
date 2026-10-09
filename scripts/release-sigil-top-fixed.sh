#!/usr/bin/env bash
# release-sigil-top-fixed.sh <version> <commit> "<note>"   — cut a sigil-top release from <commit>
# release-sigil-top-fixed.sh --warm <commit> [version]     — build only: warm the caches, publish nothing
#
# ONE FIXED RELEASE FOLDER, reused for every release (Viktor, 2026-10-09).
#
# Why: cargo identifies a path dependency by its PATH, not its content. Every release used to get a
# worktree of its own (rel-10.4.0 … rel-10.4.20: 21 folders), so every release recompiled all of our
# own ~40 crates, for all three targets, from scratch — step 2 of v10.4.20 took 558 s. Reusing one
# path means only what actually changed between two releases is rebuilt.
#
# Still a DETACHED worktree pinned at the tested commit (2026-09-25), so nobody's uncommitted work in
# the shared tree ships under the release signature. Layout matters (measured 2026-09-25): the
# worktree must be named `sigil` beside a `flux` symlink, or cargo sees flux-topology under two path
# spellings and refuses ("package collision in the lockfile").
set -euo pipefail
MAIN=/home/storage/deepseek-codewhale/sigil
REL=/home/storage/sigil-scratch/sigil-release
WT=$REL/sigil

# bash reads a script WHILE it runs, and this one checks out another commit inside the folder it may
# live in — so it always runs from a private copy.
if [ "${_SIGIL_REL_COPY:-}" != 1 ]; then
  mkdir -p "$REL"; C="$REL/.wrapper-running.sh"; cp "$0" "$C"
  _SIGIL_REL_COPY=1 exec bash "$C" "$@"
fi

if [ "${1:-}" = "--warm" ]; then
  MODE=warm; COMMIT="${2:?usage: --warm <commit> [version]}"; VER="${3:-}"; NOTE="warm"
else
  MODE=release; VER="${1:?usage: <version> <commit> \"<note>\"}"; COMMIT="${2:?commit}"; NOTE="${3:-sigil-top v$1}"
fi

mkdir -p "$REL"; ln -sfn /home/storage/deepseek-codewhale/flux "$REL/flux"
if git -C "$WT" rev-parse --git-dir >/dev/null 2>&1; then
  # A failed or build-only run leaves its version bump behind. Keep it as a patch, never just drop it.
  if [ -n "$(git -C "$WT" status --porcelain --untracked-files=no)" ]; then
    P="$REL/leftover-$(date +%Y%m%d-%H%M%S).patch"
    git -C "$WT" diff HEAD > "$P"
    echo "  ! leftover changes in the release folder saved to $P"
  fi
  git -C "$WT" checkout -q -f --detach "$COMMIT"
else
  git -C "$MAIN" worktree add -q --detach "$WT" "$COMMIT"
fi
cd "$WT"
[ -e target ] || ln -s "$MAIN/target" target
echo "▸ release folder $WT @ $(git rev-parse --short HEAD) (detached, fixed path — warm cache)"

# every fix the release line has promised users must be in what we build
for fix in 4eed359e dfd1120c 6376cc85 53cd3c9c 6fa3aa28 ${EXTRA_FIXES:-}; do
  git merge-base --is-ancestor "$fix" HEAD && echo "  ✓ $fix is in this build" \
    || { echo "✗ $fix is NOT an ancestor of $COMMIT"; exit 1; }
done

export SIGIL_RELEASE_REPO="$WT"
# Capped so the live nodes are never starved (CLAUDE.md compile rule); three LTO links run in parallel.
RUN=(systemd-run --scope -q -p MemoryMax=20G -p CPUQuota=1000% nice -n10 ionice -c2 -n7)

if [ "$MODE" = warm ]; then
  # An older commit's release script has no build-only mode and would PUBLISH. Refuse instead.
  grep -q 'SIGIL_RELEASE_BUILD_ONLY' scripts/release-sigil-top.sh \
    || { echo "✗ $COMMIT's release script has no build-only mode — refusing to warm (it would publish)"; exit 1; }
  [ -n "$VER" ] || VER=$(grep -m1 -oE '^version = "[0-9.]+"' crates/sigil-top/Cargo.toml | grep -oE '[0-9.]+')
  rc=0
  SIGIL_RELEASE_BUILD_ONLY=1 "${RUN[@]}" bash scripts/release-sigil-top.sh "$VER" "$NOTE" || rc=$?
  git checkout -q -f --detach "$COMMIT"   # undo the build-only version bump
  exit $rc
fi
exec "${RUN[@]}" bash scripts/release-sigil-top.sh "$VER" "$NOTE"
