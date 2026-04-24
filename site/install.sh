#!/bin/sh
# nemesis8 installer for Linux/macOS
# Usage: curl -fsSL https://nemesis8.nuts.services/install.sh | sh
#        curl -fsSL https://nemesis8.nuts.services/install.sh | sh -s -- --no-modify-path
set -e

NO_MODIFY_PATH=0
for arg in "$@"; do
  case "$arg" in
    --no-modify-path) NO_MODIFY_PATH=1 ;;
  esac
done

echo ""
echo "  nemesis8 installer"
echo ""

# Check dependencies
for cmd in curl tar; do
  if ! command -v "$cmd" >/dev/null 2>&1; then
    echo "Error: '$cmd' is required but not installed." >&2
    exit 1
  fi
done

# Detect platform
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)

case "$OS" in
  linux)
    case "$ARCH" in
      x86_64)          TARGET="x86_64-unknown-linux-gnu" ;;
      aarch64|arm64)   TARGET="aarch64-unknown-linux-gnu" ;;
      *) echo "Error: unsupported Linux architecture: $ARCH" >&2; exit 1 ;;
    esac
    ;;
  darwin)
    case "$ARCH" in
      arm64|aarch64)   TARGET="aarch64-apple-darwin" ;;
      *)               TARGET="x86_64-apple-darwin" ;;
    esac
    ;;
  *) echo "Error: unsupported OS: $OS" >&2; exit 1 ;;
esac

# Get latest release tag
echo "[*] Finding latest release..."
RELEASE_URL="https://api.github.com/repos/DeepBlueDynamics/nemesis8/releases/latest"
RELEASE_JSON=$(curl -fsSL --max-time 30 "$RELEASE_URL")

if command -v jq >/dev/null 2>&1; then
  TAG=$(echo "$RELEASE_JSON" | jq -r '.tag_name // empty')
else
  TAG=$(echo "$RELEASE_JSON" | grep '"tag_name"' | head -1 | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')
fi

if [ -z "$TAG" ]; then
  echo "Error: could not find latest release" >&2
  exit 1
fi

echo "[OK] Found $TAG"

DOWNLOAD_URL="https://github.com/DeepBlueDynamics/nemesis8/releases/download/${TAG}/nemisis8-${TARGET}.tar.gz"

# Download and extract
echo "[*] Downloading nemisis8-${TARGET}.tar.gz..."
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

curl -fsSL --max-time 120 "$DOWNLOAD_URL" -o "$TMP/nemesis8.tar.gz"
tar xzf "$TMP/nemesis8.tar.gz" -C "$TMP"

# Locate binary
BINARY=$(find "$TMP" -name "nemisis8" -not -path "*.tar.gz" | head -1)
if [ -z "$BINARY" ]; then
  echo "Error: nemisis8 binary not found in archive" >&2
  exit 1
fi

# Install
BIN_DIR="$HOME/.local/bin"
mkdir -p "$BIN_DIR"

# Stop any running instances before overwriting
for name in nemesis8 nemisis8 n8; do
  pkill -x "$name" 2>/dev/null || true
done
sleep 1

cp "$BINARY" "$BIN_DIR/nemesis8"
chmod +x "$BIN_DIR/nemesis8"
ln -sf "$BIN_DIR/nemesis8" "$BIN_DIR/nemisis8"
ln -sf "$BIN_DIR/nemesis8" "$BIN_DIR/n8"

# PATH setup
if [ "$NO_MODIFY_PATH" = "0" ]; then
  case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *)
      SHELL_NAME=$(basename "${SHELL:-/bin/sh}")
      case "$SHELL_NAME" in
        zsh)
          RC_FILE="$HOME/.zshrc"
          EXPORT_LINE='export PATH="$HOME/.local/bin:$PATH"'
          ;;
        bash)
          if [ "$OS" = "darwin" ]; then
            RC_FILE="$HOME/.bash_profile"
          else
            RC_FILE="$HOME/.bashrc"
          fi
          EXPORT_LINE='export PATH="$HOME/.local/bin:$PATH"'
          ;;
        fish)
          RC_FILE="$HOME/.config/fish/config.fish"
          EXPORT_LINE='set -gx PATH $HOME/.local/bin $PATH'
          ;;
        *)
          RC_FILE=""
          EXPORT_LINE='export PATH="$HOME/.local/bin:$PATH"'
          ;;
      esac
      if [ -n "$RC_FILE" ]; then
        echo "$EXPORT_LINE" >> "$RC_FILE"
        echo "[!] Added $BIN_DIR to PATH in $RC_FILE (restart terminal to take effect)"
      else
        echo "[!] Add this to your shell config: $EXPORT_LINE"
      fi
      ;;
  esac
fi

# Verify
if ! VERSION=$("$BIN_DIR/nemesis8" --version 2>&1); then
  echo "Error: installed binary failed to run" >&2
  exit 1
fi

echo ""
echo "nemesis8 installed: $VERSION"
echo ""
echo "Prerequisites: Docker (https://docs.docker.com/engine/install/)"
echo ""
echo "Get started:"
echo "  nemesis8 interactive          # start a session"
echo "  nemesis8 doctor               # check prerequisites"
echo "  nemesis8 --help               # see all commands"
echo ""
