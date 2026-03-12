#!/usr/bin/env bash
set -euo pipefail

bridge_pid=""

cleanup() {
  if [[ -n "${bridge_pid}" ]] && kill -0 "${bridge_pid}" 2>/dev/null; then
    kill "${bridge_pid}" 2>/dev/null || true
    wait "${bridge_pid}" 2>/dev/null || true
  fi
}

trap cleanup EXIT INT TERM

# Bridge container public port 1455 to Codex's loopback callback listener.
socat TCP-LISTEN:1455,bind=0.0.0.0,reuseaddr,fork TCP:127.0.0.1:1455 &
bridge_pid=$!

codex login
