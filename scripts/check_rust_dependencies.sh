#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
# Discover actual workspaces and fail before auditing if a source lock is missing
# or ignored, or cannot resolve the current dependencies with --locked.
# Keep command substitution separate so discovery failures propagate.
lockfiles=$(python3 scripts/check_workspace_lockfiles.py --check-resolution)
while IFS= read -r lockfile; do
  cargo audit --file "$lockfile" --deny unsound --deny unmaintained "$@"
done <<< "$lockfiles"
