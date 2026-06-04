#!/usr/bin/env bash
# Bump nemesis8's version. DEFAULT = patch.
#
# NEVER hand-edit `version = ` in Cargo.toml. Always use this script — it
# computes the next number mechanically so the version can't be set to a
# "random" value or reflexively bumped to a minor.
#
#   scripts/bump.sh           -> PATCH  (x.y.Z+1)   <- the default; use this
#   scripts/bump.sh patch     -> PATCH
#   scripts/bump.sh minor     -> MINOR  (x.Y+1.0)   <- ONLY when the user calls
#                                                       it a milestone
#   scripts/bump.sh major     -> MAJOR  (X+1.0.0)
#
# Patch is for everything iterative — fixes, tweaks, columns, modals, polish.
# A modal or a new column is STILL a patch. Minor is a milestone the user names.
set -euo pipefail
cd "$(dirname "$0")/.."

kind="${1:-patch}"
cur=$(grep -m1 '^version = ' Cargo.toml | sed -E 's/version = "([^"]+)".*/\1/')
IFS=. read -r MA MI PA <<<"$cur"

case "$kind" in
  patch) PA=$((PA + 1)) ;;
  minor) MI=$((MI + 1)); PA=0 ;;
  major) MA=$((MA + 1)); MI=0; PA=0 ;;
  *) echo "usage: scripts/bump.sh [patch|minor|major]   (default: patch)" >&2; exit 1 ;;
esac
new="$MA.$MI.$PA"

# Replace only the package version (first `version = ` line).
sed -i.bak -E "0,/^version = \"[^\"]*\"/s//version = \"$new\"/" Cargo.toml && rm -f Cargo.toml.bak
# Refresh Cargo.lock with the new version.
cargo check --quiet >/dev/null 2>&1 || true

echo "bumped: $cur -> $new ($kind)"
echo "now:  git add -A && git commit  &&  git tag v$new  &&  git push origin main v$new"
