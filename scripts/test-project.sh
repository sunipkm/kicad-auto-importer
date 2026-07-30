#!/usr/bin/env bash
# test-project.sh — Guard test-project/ against accidental commits.
#
# test-project/ is a fixture used to manually smoke-test the apps (watcher,
# library import, Populate/Generate BOM). Running the apps against it can
# leave the working tree dirty. This script uses `git update-index
# --skip-worktree` so Git ignores local modifications to it by default;
# unlock before intentionally updating the fixture, then lock again.
#
# Usage:
#   ./scripts/test-project.sh lock     # (default) ignore local edits
#   ./scripts/test-project.sh unlock   # allow editing/staging/committing
#   ./scripts/test-project.sh reset    # discard local edits, restore tracked content, re-lock
#   ./scripts/test-project.sh status   # show current lock state per file

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

mapfile -t FILES < <(git ls-files -- test-project/)
if [[ ${#FILES[@]} -eq 0 ]]; then
  echo "No tracked files under test-project/" >&2
  exit 1
fi

CMD="${1:-lock}"

case "$CMD" in
  lock)
    git update-index --skip-worktree -- "${FILES[@]}"
    echo "Locked ${#FILES[@]} file(s) under test-project/ (skip-worktree set)."
    ;;
  unlock)
    git update-index --no-skip-worktree -- "${FILES[@]}"
    echo "Unlocked ${#FILES[@]} file(s) under test-project/ — edit, then run 'git add' and commit as usual."
    echo "Run '$0 lock' again once you're done to re-protect it."
    ;;
  reset)
    git update-index --no-skip-worktree -- "${FILES[@]}"
    git checkout -- "${FILES[@]}"
    git update-index --skip-worktree -- "${FILES[@]}"
    echo "Restored ${#FILES[@]} file(s) under test-project/ to their committed state and re-locked."
    ;;
  status)
    git ls-files -v -- test-project/ | awk '{print ($1 == "S") ? "locked:   " $2 : "unlocked: " $2}'
    ;;
  *)
    echo "Usage: $0 {lock|unlock|reset|status}" >&2
    exit 1
    ;;
esac
