#!/bin/sh
# Install AI provider CLIs from a comma-separated list of provider names.
# Usage: install-providers.sh <providers> [extras]
#   providers: comma-separated names, e.g. "codex,gemini,claude"
#   extras:    comma-separated optional extras, e.g. "baml"
set -eu

PROVIDERS="${1:-codex,gemini,claude,openclaw,ollama}"
EXTRAS="${2:-}"

pkg_for_provider() {
    case "$1" in
        codex)    echo "@openai/codex@latest" ;;
        gemini)   echo "@google/gemini-cli@latest" ;;
        claude)   echo "@anthropic-ai/claude-code@latest" ;;
        openclaw) echo "openclaw@latest" ;;
        ollama)   echo "@qwen-code/qwen-code@latest" ;;
        *)        echo "" ;;
    esac
}

pkg_for_extra() {
    case "$1" in
        baml) echo "@boundaryml/baml@latest" ;;
        *)    echo "" ;;
    esac
}

pkgs=""

OLD_IFS="$IFS"
IFS=','
for p in $PROVIDERS; do
    p="$(echo "$p" | tr -d ' ')"
    [ -z "$p" ] && continue
    pkg="$(pkg_for_provider "$p")"
    if [ -n "$pkg" ]; then
        pkgs="$pkgs $pkg"
    else
        echo "install-providers: unknown provider '$p', skipping" >&2
    fi
done

if [ -n "$EXTRAS" ]; then
    for e in $EXTRAS; do
        e="$(echo "$e" | tr -d ' ')"
        [ -z "$e" ] && continue
        pkg="$(pkg_for_extra "$e")"
        if [ -n "$pkg" ]; then
            pkgs="$pkgs $pkg"
        else
            echo "install-providers: unknown extra '$e', skipping" >&2
        fi
    done
fi
IFS="$OLD_IFS"

if [ -n "$pkgs" ]; then
    # shellcheck disable=SC2086
    npm install -g $pkgs
    npm cache clean --force
    # Clean codex cache if installed
    rm -rf /usr/local/share/npm-global/lib/node_modules/codex-cli/node_modules/.cache 2>/dev/null || true
else
    echo "install-providers: no providers to install" >&2
fi
