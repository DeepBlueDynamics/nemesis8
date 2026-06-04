#!/usr/bin/env bash
# nemesis8 macOS sign + notarize script  (MANUAL FALLBACK)
#
# As of the signing step in .github/workflows/release.yml, CI now signs +
# notarizes the macOS release binaries automatically (same Developer ID cert
# via the CSC_LINK / APPLE_* secrets). Use this script only to re-sign an
# existing release asset by hand (e.g. if CI signing was skipped).
#
# Run this on a Mac that has the Developer ID Application cert in its Keychain
# (set up per docs/signing-apple.md in the hyperia repo — same cert).
#
# Downloads the unsigned CI tarball for the given version, signs the binary
# with hardened runtime + timestamp, notarizes it with Apple, repackages, and
# re-uploads the now-signed tarball as the GitHub release asset (clobbering
# the unsigned one).
#
# Notarization tickets can't be stapled to bare CLI binaries (only .app /
# .dmg / .pkg bundles). Gatekeeper fetches notarization status online when
# the binary first runs — which is fine for CLI distribution. The binary
# is verifiable offline as "signed by DeepBlue Dynamics LLC".
#
# Usage:
#   APPLE_ID=you@example.com \
#   APPLE_APP_SPECIFIC_PASSWORD=xxxx-xxxx-xxxx-xxxx \
#   APPLE_TEAM_ID=XXXXXXXXXX \
#   ./scripts/sign-mac.sh v0.7.4
#
# Override arch with TARGET=x86_64-apple-darwin to sign the Intel tarball
# from an Apple Silicon Mac (or vice versa) — codesign and notarytool work
# cross-arch fine.

set -euo pipefail

VERSION="${1:-}"
if [ -z "$VERSION" ]; then
    echo "Usage: $0 <version>"
    echo "Example: $0 v0.7.4"
    exit 1
fi

: "${APPLE_ID:?APPLE_ID must be set (your Apple ID email)}"
: "${APPLE_APP_SPECIFIC_PASSWORD:?APPLE_APP_SPECIFIC_PASSWORD must be set (xxxx-xxxx-xxxx-xxxx)}"
: "${APPLE_TEAM_ID:?APPLE_TEAM_ID must be set (10-char Team ID)}"

REPO="${REPO:-DeepBlueDynamics/nemesis8}"
IDENTITY="${IDENTITY:-Developer ID Application: DeepBlue Dynamics LLC ($APPLE_TEAM_ID)}"

if [ -z "${TARGET:-}" ]; then
    ARCH=$(uname -m)
    case "$ARCH" in
        arm64)  TARGET="aarch64-apple-darwin" ;;
        x86_64) TARGET="x86_64-apple-darwin" ;;
        *) echo "Unsupported architecture: $ARCH" >&2; exit 1 ;;
    esac
fi

ARTIFACT="nemesis8-${VERSION}-${TARGET}.tar.gz"
DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${VERSION}/${ARTIFACT}"

WORKDIR=$(mktemp -d -t nemesis8-sign)
trap 'rm -rf "$WORKDIR"' EXIT

echo "==> [1/7] Verifying signing identity is in Keychain"
if ! security find-identity -v -p codesigning | grep -q "$IDENTITY"; then
    echo "Error: identity not found in Keychain:" >&2
    echo "  $IDENTITY" >&2
    echo "Available codesigning identities:" >&2
    security find-identity -v -p codesigning >&2
    exit 1
fi

echo "==> [2/7] Downloading unsigned $ARTIFACT"
curl -fsSL --max-time 120 "$DOWNLOAD_URL" -o "$WORKDIR/$ARTIFACT"

echo "==> [3/7] Extracting"
( cd "$WORKDIR" && tar xzf "$ARTIFACT" )
BIN="$WORKDIR/nemesis8"
if [ ! -f "$BIN" ]; then
    echo "Error: nemesis8 binary not found in tarball" >&2
    exit 1
fi

echo "==> [4/7] Signing with hardened runtime + timestamp"
codesign --sign "$IDENTITY" \
    --options runtime \
    --timestamp \
    --force \
    --verbose \
    "$BIN"

echo "==> [5/7] Verifying signature"
codesign --verify --verbose=2 "$BIN"
codesign --display --verbose=4 "$BIN" 2>&1 | head -20

echo "==> [6/7] Submitting to Apple notary service (this can take 1-5 min)"
ZIP="$WORKDIR/nemesis8-signed.zip"
( cd "$WORKDIR" && zip -j "$ZIP" nemesis8 )

xcrun notarytool submit "$ZIP" \
    --apple-id "$APPLE_ID" \
    --password "$APPLE_APP_SPECIFIC_PASSWORD" \
    --team-id "$APPLE_TEAM_ID" \
    --wait

echo "==> [7/7] Repackaging signed binary"
SIGNED_TARBALL="$WORKDIR/$ARTIFACT"
( cd "$WORKDIR" && tar czf "$SIGNED_TARBALL.new" nemesis8 && mv "$SIGNED_TARBALL.new" "$SIGNED_TARBALL" )

echo ""
echo "Signed + notarized tarball ready:"
echo "  $SIGNED_TARBALL"
echo ""
echo "Upload to GitHub release (replaces the unsigned asset):"
echo "  gh release upload $VERSION $SIGNED_TARBALL --repo $REPO --clobber"
echo ""
read -p "Upload now? [y/N] " yn
case "$yn" in
    [Yy]*)
        gh release upload "$VERSION" "$SIGNED_TARBALL" --repo "$REPO" --clobber
        echo "Done. The $ARTIFACT asset on release $VERSION is now signed."
        ;;
    *)
        echo "Skipped upload. Run the gh command above when ready."
        ;;
esac
