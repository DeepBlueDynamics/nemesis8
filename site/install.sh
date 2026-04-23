#!/bin/sh
# nemesis8 installer for Linux/macOS
# Usage: curl -fsSL https://nemesis8.nuts.services/install.sh | sh
set -e

echo ""
echo "  nemesis8 installer"
echo ""

# Detect platform
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)

case "$OS" in
    linux)  TARGET="x86_64-unknown-linux-gnu" ;;
    darwin)
        case "$ARCH" in
            arm64|aarch64) TARGET="aarch64-apple-darwin" ;;
            *)             TARGET="x86_64-apple-darwin" ;;
        esac
        ;;
    *) echo "Error: unsupported OS: $OS"; exit 1 ;;
esac

# Get latest release
echo "[*] Finding latest release..."
RELEASE_URL="https://api.github.com/repos/DeepBlueDynamics/nemesis8/releases/latest"
TAG=$(curl -fsSL "$RELEASE_URL" | grep '"tag_name"' | head -1 | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')

if [ -z "$TAG" ]; then
    echo "Error: could not find latest release"
    exit 1
fi

echo "[OK] Found $TAG"

DOWNLOAD_URL="https://github.com/DeepBlueDynamics/nemesis8/releases/download/${TAG}/nemisis8-${TARGET}.tar.gz"

# Download and extract
echo "[*] Downloading nemisis8-${TARGET}.tar.gz..."
TMP=$(mktemp -d)
curl -fsSL "$DOWNLOAD_URL" -o "$TMP/nemesis8.tar.gz"
tar xzf "$TMP/nemesis8.tar.gz" -C "$TMP"

# Install
BIN_DIR="$HOME/.local/bin"
mkdir -p "$BIN_DIR"

cp "$TMP/nemisis8" "$BIN_DIR/nemesis8"
chmod +x "$BIN_DIR/nemesis8"
ln -sf "$BIN_DIR/nemesis8" "$BIN_DIR/nemisis8"
ln -sf "$BIN_DIR/nemesis8" "$BIN_DIR/n8"

# Cleanup
rm -rf "$TMP"

# Check PATH
case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *)
        # Detect user's shell to suggest the right rc file
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
                RC_FILE="your shell's rc file"
                EXPORT_LINE='export PATH="$HOME/.local/bin:$PATH"'
                ;;
        esac
        echo "[!] $BIN_DIR is not on your PATH. Add it with:"
        echo "    echo '$EXPORT_LINE' >> $RC_FILE"
        echo "    source $RC_FILE"
        echo ""
        ;;
esac

# Verify
VERSION=$("$BIN_DIR/nemesis8" --version 2>&1 || echo "installed")
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
