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

# Refresh Cargo.lock so it records the new version.
#
# This used to be `cargo check ... || true`. That was wrong three times over:
# it ran a full typecheck (minutes on a cold tree) just to rewrite one line;
# the `|| true` swallowed every failure; and on Windows `cargo` is frequently
# NOT on PATH inside git-bash, where this script runs — so it was a silent
# no-op, and every release shipped a Cargo.lock still pinned to the previous
# version. A `--locked` CI build then fails after the tag is already pushed.
pkg=$(grep -m1 '^name = ' Cargo.toml | sed -E 's/name = "([^"]+)".*/\1/')

# Preferred: let cargo do it (only rewrites the lock entry, no compilation).
if command -v cargo >/dev/null 2>&1; then
  cargo update -p "$pkg" --offline >/dev/null 2>&1 || cargo update -p "$pkg" >/dev/null 2>&1 || true
fi

# Fallback for when cargo isn't reachable from this shell: rewrite the version
# line under our own [[package]] entry. Bumping the workspace package's version
# involves no dependency resolution, so this is exactly what cargo would write.
# The \r? keeps it working on CRLF checkouts.
if ! tr -d '\r' < Cargo.lock | grep -A1 "^name = \"$pkg\"\$" | grep -q "^version = \"$new\""; then
  awk -v pkg="$pkg" -v ver="$new" '
    $0 ~ "^name = \"" pkg "\"\r?$" { print; hit=1; next }
    hit && /^version = / { sub(/"[^"]*"/, "\"" ver "\""); hit=0 }
    { print }
  ' Cargo.lock > Cargo.lock.bump.tmp && mv Cargo.lock.bump.tmp Cargo.lock
fi

# Verify it actually took. A stale Cargo.lock breaks `--locked` release builds,
# and that failure only surfaces in CI AFTER the tag is pushed — by which point
# the fix means re-tagging. Fail here, where it's cheap.
locked=$(tr -d '\r' < Cargo.lock | grep -A1 "^name = \"$pkg\"\$" | grep -m1 '^version = ' | sed -E 's/version = "([^"]+)".*/\1/')
if [ "$locked" != "$new" ]; then
  echo "ERROR: bumped Cargo.toml to $new but Cargo.lock still records $pkg $locked." >&2
  echo "       Cargo.toml has been modified; Cargo.lock has NOT. Fix before tagging:" >&2
  echo "         cargo update -p $pkg" >&2
  exit 1
fi

echo "bumped: $cur -> $new ($kind)  [Cargo.lock refreshed]"
echo "now:  git add -A && git commit  &&  git tag v$new  &&  git push origin main v$new"
