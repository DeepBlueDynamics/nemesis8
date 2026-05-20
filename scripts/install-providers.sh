#!/bin/sh
# Install AI provider CLIs from a comma-separated list of provider names.
# Usage: install-providers.sh <providers>
#   providers: comma-separated names, e.g. "codex,gemini,claude"
set -eu

PROVIDERS="${1:-codex,gemini,claude,openclaw,antigravity}"

pkg_for_provider() {
    case "$1" in
        codex)       echo "@openai/codex@latest" ;;
        gemini)      echo "@google/gemini-cli@latest" ;;
        claude)      echo "@anthropic-ai/claude-code@latest" ;;
        openclaw)    echo "openclaw@latest" ;;
        antigravity) echo "__curl__" ;;  # ships via curl installer, not npm
        ollama)      echo "__host__" ;;  # runs on host, nothing to install in container
        alacode)     echo "__host__" ;;
        *)           echo "" ;;
    esac
}

install_curl_provider() {
    case "$1" in
        antigravity)
            echo "[install-providers] installing Antigravity CLI (agy)..."
            # The official installer drops agy at $HOME/.local/bin/agy.
            # In the Docker build this is /root/.local/bin/agy, which is
            # not on the runtime PATH and is not visible to non-root users.
            # Symlink to /usr/local/bin so every user in the container can
            # find it.
            curl -fsSL https://antigravity.google/cli/install.sh | bash
            agy_path=""
            for candidate in \
                "$HOME/.local/bin/agy" \
                "/root/.local/bin/agy" \
                "/usr/local/bin/agy"; do
                if [ -x "$candidate" ]; then
                    agy_path="$candidate"
                    break
                fi
            done
            if [ -z "$agy_path" ]; then
                echo "install-providers: agy binary not found after install" >&2
                return 1
            fi
            if [ "$agy_path" != "/usr/local/bin/agy" ]; then
                ln -sf "$agy_path" /usr/local/bin/agy
                echo "[install-providers] linked $agy_path -> /usr/local/bin/agy"
            fi
            /usr/local/bin/agy --version || echo "install-providers: agy --version failed but binary is present" >&2
            ;;
        *)
            echo "install-providers: unknown curl provider '$1'" >&2
            return 1
            ;;
    esac
}

pkgs=""
curl_providers=""

OLD_IFS="$IFS"
IFS=','
for p in $PROVIDERS; do
    p="$(echo "$p" | tr -d ' ')"
    [ -z "$p" ] && continue
    pkg="$(pkg_for_provider "$p")"
    if [ "$pkg" = "__host__" ]; then
        : # host-side provider — nothing to install in container
    elif [ "$pkg" = "__curl__" ]; then
        curl_providers="$curl_providers $p"
    elif [ -n "$pkg" ]; then
        pkgs="$pkgs $pkg"
    else
        echo "install-providers: unknown provider '$p', skipping" >&2
    fi
done

IFS="$OLD_IFS"

if [ -n "$pkgs" ]; then
    # shellcheck disable=SC2086
    npm install -g $pkgs
    npm cache clean --force
    # Clean codex cache if installed
    rm -rf /usr/local/share/npm-global/lib/node_modules/codex-cli/node_modules/.cache 2>/dev/null || true
else
    echo "install-providers: no npm providers to install" >&2
fi

# Curl-installer providers (Antigravity, etc.)
if [ -n "$curl_providers" ]; then
    for p in $curl_providers; do
        install_curl_provider "$p" || echo "install-providers: $p install failed, continuing" >&2
    done
fi
