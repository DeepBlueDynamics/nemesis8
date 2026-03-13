#!/usr/bin/env bash
set -euo pipefail

# Ensure npm global bin and cargo are in PATH
export PATH="/usr/local/share/npm-global/bin:/opt/codex-home/.cargo/bin:${PATH}"

if [ -d "/workspace" ]; then
  cd /workspace
fi

# Bridge: Docker maps host:1455 → container_ip:1455.
# Codex binds 127.0.0.1:1455. Socat bridges container_ip:1455 → 127.0.0.1:1455.
container_ip=$(hostname -I | awk '{print $1}')
socat TCP-LISTEN:1455,fork,reuseaddr,bind="$container_ip" TCP:127.0.0.1:1455 >/tmp/codex-login-bridge.log 2>&1 &
bridge_pid=$!

cleanup() {
  if [ -n "$bridge_pid" ]; then
    kill "$bridge_pid" 2>/dev/null || true
  fi
}
trap cleanup EXIT

codex login
