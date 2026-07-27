#!/usr/bin/env bash
# repub-tag.sh — Re-publish a version tag.
#
# Deletes the tag locally and on the remote, then recreates and pushes it,
# which triggers the CI release workflow for a clean re-publish.
#
# Usage:
#   ./scripts/repub-tag.sh [--sign] [-u <keyid>] <tag>
#
# Options:
#   --sign, -s       GPG-sign the new tag
#   -u <keyid>       Use the specified GPG key (implies --sign)
#
# Environment:
#   REMOTE           Git remote name (default: origin)
#   SIGN_TAG         Set to 1 to sign the tag (alternative to --sign)
#   SIGN_KEY         GPG key ID or fingerprint to use for signing

set -euo pipefail

# ── Arguments ─────────────────────────────────────────────────────────────────

SIGN_TAG="${SIGN_TAG:-0}"
SIGN_KEY="${SIGN_KEY:-}"
TAG=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --sign|-s)
      SIGN_TAG=1
      shift
      ;;
    -u)
      SIGN_TAG=1
      SIGN_KEY="${2:?"-u requires a key ID argument"}"
      shift 2
      ;;
    -*)
      echo "Unknown option: $1" >&2
      echo "Usage: $0 [--sign] [-u <keyid>] <tag>" >&2
      exit 1
      ;;
    *)
      TAG="$1"
      shift
      ;;
  esac
done

if [[ -z "$TAG" ]]; then
  echo "Usage: $0 [--sign] [-u <keyid>] <tag>" >&2
  exit 1
fi

# Solid releases (vX.Y.Z) are always signed.
if [[ "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  SIGN_TAG=1
fi

# Pre-flight: verify GPG has a usable key before touching anything.
if [[ "$SIGN_TAG" == "1" ]] && [[ -z "$SIGN_KEY" ]]; then
  _gpg_key="$(git config user.signingkey 2>/dev/null || true)"
  if [[ -z "$_gpg_key" ]]; then
    _gpg_key="$(git config user.email 2>/dev/null || true)"
  fi
  if ! gpg --list-secret-keys "${_gpg_key:-}" &>/dev/null; then
    echo "Error: no usable GPG secret key found for '${_gpg_key:-<unset>}'." >&2
    echo "       Configure user.signingkey, or pass -u <keyid> to specify one." >&2
    exit 1
  fi
fi

REMOTE="${REMOTE:-origin}"

cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"

if ! git diff --quiet || ! git diff --cached --quiet; then
  echo "Error: uncommitted changes present. Commit or stash them first." >&2
  exit 1
fi

# ── Delete existing tag ────────────────────────────────────────────────────────

if git rev-parse "refs/tags/$TAG" &>/dev/null; then
  echo "==> Deleting local tag '$TAG'"
  git tag -d "$TAG"
fi

if git ls-remote --exit-code --tags "$REMOTE" "refs/tags/$TAG" &>/dev/null; then
  echo "==> Deleting remote tag '$TAG' on '$REMOTE'"
  git push "$REMOTE" ":refs/tags/$TAG"
fi

# ── Recreate and push ─────────────────────────────────────────────────────────

if [[ "$SIGN_TAG" == "1" ]]; then
  if [[ -n "$SIGN_KEY" ]]; then
    echo "==> Creating signed tag '$TAG' (key: $SIGN_KEY)"
    git tag -u "$SIGN_KEY" "$TAG"
  else
    echo "==> Creating signed tag '$TAG' (default GPG key)"
    git tag -s "$TAG"
  fi
else
  echo "==> Creating tag '$TAG'"
  git tag "$TAG"
fi

echo "==> Pushing tag '$TAG' to '$REMOTE'"
git push "$REMOTE" "$TAG"

echo "==> Done — CI will now publish '$TAG'"
